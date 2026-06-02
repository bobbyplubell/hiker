//! The self-contained single-file HTML archiver — the canonical artifact
//! "view original" opens, offline. Hand-rolled (there is no mature pure-Rust
//! single-file-archiver *library*): walk the parsed DOM, strip every
//! `<script>`, inline `<link rel="stylesheet">` and `<img>`/`<source>`
//! subresources as `data:` URIs, and re-serialize to one `.html` blob with no
//! outbound references. Subresource fetching is an injected closure so the
//! archiver itself stays network-free and unit-testable; the orchestrator in
//! `super` passes the real HTTP fetcher. See `docs/extract.md`
//! `extract-web-archive-singlefile`.
//
// status: extract-web-archive-singlefile

use base64::Engine;
use scraper::{Html, Selector};
use url::Url;

/// One fetched subresource: its bytes + the MIME type to stamp into the
/// `data:` URI. The orchestrator's HTTP fetcher returns this; tests inject a
/// canned map.
#[derive(Debug, Clone)]
pub(super) struct SubResource {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// A subresource fetcher: absolute URL → bytes + MIME, or `None` when the
/// fetch failed (the reference is then left as the absolute URL rather than
/// inlined — a partial archive beats a failed one).
pub(super) type Fetcher<'a> = dyn Fn(&str) -> Option<SubResource> + 'a;

/// Build a self-contained single-file HTML archive from the page `html`
/// (anchored at `base_url` for relative-URL resolution), inlining subresources
/// via `fetch`. Scripts are always stripped; stylesheets and images are
/// inlined as `data:` URIs when the fetch succeeds, else rewritten to absolute
/// URLs so the archive degrades to "needs network for that one asset" rather
/// than breaking.
pub(super) fn build(html: &str, base_url: &str, fetch: &Fetcher<'_>) -> String {
    let base = Url::parse(base_url).ok();
    let resolve = |href: &str| -> Option<String> {
        match &base {
            Some(b) => b.join(href).ok().map(|u| u.to_string()),
            None => Url::parse(href).ok().map(|u| u.to_string()),
        }
    };

    let doc = Html::parse_document(html);
    let mut output = doc.html();

    // Strip every <script> ... </script> (and self-closing/loader script tags):
    // the archive is a no-JS rendering by construction.
    output = strip_scripts(&output);

    // Inline stylesheet links as <style> blocks (fetched) or absolute hrefs.
    for href in stylesheet_hrefs(&doc) {
        let Some(abs) = resolve(&href) else { continue };
        if let Some(res) = fetch(&abs) {
            let css = String::from_utf8_lossy(&res.bytes);
            let replacement = format!("<style>{css}</style>");
            output = replace_link_tag(&output, &href, &replacement);
        } else {
            output = output.replace(&href, &abs);
        }
    }

    // Inline image/source subresources as data: URIs, else absolutize.
    for src in subresource_srcs(&doc) {
        let Some(abs) = resolve(&src) else { continue };
        match fetch(&abs) {
            Some(res) => {
                let data_uri = to_data_uri(&res);
                output = output.replace(&src, &data_uri);
            }
            None => {
                output = output.replace(&src, &abs);
            }
        }
    }

    output
}

/// Strip `<script>…</script>` blocks from serialized HTML. String-level (the
/// DOM is already parsed; we operate on the serialized form so the surrounding
/// inline-replacement passes share one representation).
fn strip_scripts(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = find_ci(rest, "<script") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(end) = find_ci(after, "</script>") {
            rest = &after[end + "</script>".len()..];
        } else {
            // Unterminated; drop the remainder to be safe.
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive substring search returning the byte offset of `needle` in
/// `haystack`.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.to_ascii_lowercase();
    h.find(&needle.to_ascii_lowercase())
}

/// The `href`s of `<link rel="stylesheet">` elements.
fn stylesheet_hrefs(doc: &Html) -> Vec<String> {
    let Ok(sel) = Selector::parse(r#"link[rel="stylesheet"][href]"#) else {
        return Vec::new();
    };
    doc.select(&sel)
        .filter_map(|el| el.value().attr("href").map(str::to_string))
        .collect()
}

/// The `src`s of `<img>` / `<source>` elements (the inlinable media set).
fn subresource_srcs(doc: &Html) -> Vec<String> {
    let Ok(sel) = Selector::parse("img[src], source[src]") else {
        return Vec::new();
    };
    doc.select(&sel)
        .filter_map(|el| el.value().attr("src").map(str::to_string))
        .filter(|s| !s.starts_with("data:"))
        .collect()
}

/// Replace the whole `<link ... href="HREF" ...>` tag carrying `href` with
/// `replacement`. Falls back to a plain href substitution if the tag bounds
/// can't be found.
fn replace_link_tag(html: &str, href: &str, replacement: &str) -> String {
    let Some(href_pos) = html.find(href) else {
        return html.to_string();
    };
    let before = &html[..href_pos];
    let Some(tag_start) = before.rfind("<link") else {
        return html.replace(href, replacement);
    };
    let after = &html[href_pos..];
    let Some(rel_end) = after.find('>') else {
        return html.replace(href, replacement);
    };
    let tag_end = href_pos + rel_end + 1;
    format!("{}{replacement}{}", &html[..tag_start], &html[tag_end..])
}

/// Encode a fetched subresource as a `data:<mime>;base64,<…>` URI.
fn to_data_uri(res: &SubResource) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(&res.bytes);
    format!("data:{};base64,{}", res.mime, b64)
}

#[cfg(test)]
mod tests {
    use super::{build, SubResource};
    use std::collections::HashMap;

    fn fetcher(map: HashMap<String, SubResource>) -> impl Fn(&str) -> Option<SubResource> {
        move |url: &str| map.get(url).cloned()
    }

    #[test]
    fn strips_scripts() {
        let html = r#"<html><head><script src="app.js"></script></head>
            <body><p>kept</p><script>alert(1)</script></body></html>"#;
        let out = build(html, "https://x.test/", &fetcher(HashMap::new()));
        assert!(!out.to_ascii_lowercase().contains("<script"), "all scripts stripped");
        assert!(out.contains("kept"));
    }

    #[test]
    fn inlines_image_as_data_uri() {
        let mut map = HashMap::new();
        map.insert(
            "https://x.test/pic.png".to_string(),
            SubResource { bytes: vec![1, 2, 3], mime: "image/png".to_string() },
        );
        let html = r#"<html><body><img src="pic.png"></body></html>"#;
        let out = build(html, "https://x.test/", &fetcher(map));
        assert!(out.contains("data:image/png;base64,"), "image inlined as data uri");
        assert!(!out.contains("src=\"pic.png\""));
    }

    #[test]
    fn inlines_stylesheet() {
        let mut map = HashMap::new();
        map.insert(
            "https://x.test/style.css".to_string(),
            SubResource { bytes: b"body{color:red}".to_vec(), mime: "text/css".to_string() },
        );
        let html = r#"<html><head><link rel="stylesheet" href="style.css"></head><body></body></html>"#;
        let out = build(html, "https://x.test/", &fetcher(map));
        assert!(out.contains("<style>body{color:red}</style>"), "css inlined");
        assert!(!out.contains("stylesheet"), "link tag replaced");
    }

    #[test]
    fn unfetched_subresource_absolutized() {
        // No fetcher hit → the relative src becomes absolute rather than breaking.
        let html = r#"<html><body><img src="missing.png"></body></html>"#;
        let out = build(html, "https://x.test/blog/", &fetcher(HashMap::new()));
        assert!(out.contains("https://x.test/blog/missing.png"));
    }
}
