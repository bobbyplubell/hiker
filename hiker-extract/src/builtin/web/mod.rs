//! The website-to-markdown extractor: a URL → clean markdown clip note + a
//! self-contained HTML archive, with **no JavaScript execution and no embedded
//! browser engine** (`docs/extract.md` "Website-to-markdown extractor"). One
//! blocking HTTP GET (`reqwest` + `rustls`, pure-Rust TLS) fetches the page;
//! the transform pipeline then runs entirely over the static HTML string:
//!
//! 1. a server-rendered data-blob probe (`datablob`) — `__NEXT_DATA__` /
//!    `__NUXT__` / JSON-LD, parsed never executed;
//! 2. a readability + html→markdown pass (`content`);
//! 3. a thin-page fallback chain (`fallback`) — declared full-text feed → AMP
//!    → print view, re-fetched and re-extracted in order;
//!
//! plus a self-contained single-file HTML archive (`archive`) ridden on
//! [`Extracted::archive`]. The whole transform is a pure string→string
//! function ([`extract_html`]) the unit tests drive directly with local HTML
//! fixtures; the network is confined to the thin [`fetch`] seam.
//
// status: extract-web-static-fetch

mod archive;
mod content;
mod datablob;
mod fallback;

use crate::contract::{Archive, Extracted, SidecarMeta};
use crate::ExtractError;

/// Fetch `url` and run the full transform pipeline, returning the extracted
/// clip (markdown + frontmatter + HTML archive) or `Ok(None)` for a genuinely
/// empty page. The entry point [`super::WebExtractor`] calls — it owns the one
/// live network seam ([`fetch`] + [`http_subresource`]); everything downstream
/// is a pure string transform.
///
/// status: extract-web-static-fetch
pub(super) fn scrape_url(url: &str) -> Result<Option<Extracted>, ExtractError> {
    let html = fetch(url)?;
    Ok(extract_with_fetcher(&html, url, &|u| http_subresource(u)))
}

/// Minimum non-whitespace body length below which the primary extraction is
/// judged "thin" and the fallback chain is tried. A real article clears this
/// comfortably; a cookie-wall / nav-only readability result does not.
const THIN_CONTENT: usize = 400;

/// Run the same readability + data-blob + htmd transform hiker's web ingest
/// uses, on an already-fetched HTML string (no network). Lets an external
/// renderer (hiker-crawler's CEF engine) produce byte-identical output to
/// ingest: the body is the best-of (data-blob > readability) representation,
/// the parsed title rides on the [`SidecarMeta`], and any discovered follow-up
/// links land on `next_urls`. `archive` is always `None` — the single-file
/// archive needs a subresource fetcher (the in-process [`extract_with_fetcher`]
/// owns that seam; the crawler builds WARC separately).
///
/// Returns a default [`Extracted`] (empty markdown) for a genuinely empty page
/// so the function is total over arbitrary HTML; callers that need to tell an
/// empty result apart can check `markdown.is_empty()`.
//
// status: crawler-preview-fidelity
pub fn extract_from_html(html: &str, base_url: &str) -> Extracted {
    let Some((markdown, title)) = best_body(html, base_url) else {
        return Extracted::default();
    };
    Extracted {
        markdown,
        frontmatter: Some(SidecarMeta {
            title,
            source_url: Some(base_url.to_string()),
        }),
        archive: None,
        next_urls: Vec::new(),
    }
}

/// The pure transform pipeline over already-fetched HTML, with an injectable
/// subresource fetcher for the archive (so tests run fully offline). This is
/// the seam the unit tests drive with local HTML fixtures: it never touches
/// the network itself — the page HTML is an argument and subresource fetches
/// go through `archive_fetch`.
///
/// Reuses [`extract_from_html`] for the body/title/links transform and only
/// adds the single-file HTML archive on top, so the in-process and crawler
/// paths share one transform definition.
///
/// Returns `None` only when no representation produced any content at all
/// (a genuinely empty page); otherwise the best-of (data-blob > readability)
/// body with the archive attached.
///
/// status: extract-web-readability
fn extract_with_fetcher(
    html: &str,
    url: &str,
    archive_fetch: &archive::Fetcher<'_>,
) -> Option<Extracted> {
    let mut extracted = extract_from_html(html, url);
    if extracted.markdown.is_empty() {
        return None;
    }
    let archive_html = archive::build(html, url, archive_fetch);
    extracted.archive =
        Some(Archive { extension: "html".to_string(), bytes: archive_html.into_bytes() });
    Some(extracted)
}

/// Choose the best article body for `html`: the server-rendered data blob when
/// it has real content (`extract-web-data-blob`), else the readability pass
/// (`extract-web-readability`), else — when that is thin — a fallback
/// representation (`extract-web-fallbacks`). Returns the body + recovered
/// title, or `None` when nothing yielded content.
fn best_body(html: &str, url: &str) -> Option<(String, Option<String>)> {
    // 1. Data blob first — it's the cleanest source on framework-rendered
    //    sites and sidesteps readability heuristics entirely.
    if let Some(blob) = datablob::probe(html) {
        let md = htmd::convert(&blob.body).unwrap_or(blob.body);
        if content::content_len(&md) > 0 {
            return Some((md, blob.title));
        }
    }

    // 2. Readability + html→markdown over the fetched page.
    let article = content::to_article(html, url);
    if content::content_len(&article.markdown) >= THIN_CONTENT {
        return Some((article.markdown, article.title));
    }

    // 3. Thin: try declared alternates (feed / AMP / print) in order. Each is
    //    fetched and run through the same readability pass; first real wins.
    if let Some(better) = try_fallbacks(html, url) {
        return Some(better);
    }

    // Nothing better — keep whatever readability produced, even if thin, so a
    // sparse-but-real page still clips. Only a truly empty body declines.
    if content::content_len(&article.markdown) > 0 {
        Some((article.markdown, article.title))
    } else {
        None
    }
}

/// Try each declared fallback URL in order, fetching + re-running readability;
/// return the first with real content. Network failures on a fallback are
/// skipped (a fallback that won't fetch is just not a fallback).
fn try_fallbacks(html: &str, url: &str) -> Option<(String, Option<String>)> {
    for candidate in fallback::candidates(html, url) {
        let Ok(alt_html) = fetch(&candidate) else { continue };
        let article = content::to_article(&alt_html, &candidate);
        if content::content_len(&article.markdown) >= THIN_CONTENT {
            return Some((article.markdown, article.title));
        }
    }
    None
}

/// One blocking HTTP GET over a pure-Rust `rustls` TLS stack — no JavaScript,
/// no headless browser (`extract-web-static-fetch`). A shared blocking client
/// (connection reuse, gzip, a sane UA + timeout) is built once. The
/// `Extractor::extract` trait is synchronous, so blocking reqwest fits without
/// making the whole trait async.
fn fetch(url: &str) -> Result<String, ExtractError> {
    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| ExtractError::Extractor("web-scrape".into(), e.to_string()))?
        .error_for_status()
        .map_err(|e| ExtractError::Extractor("web-scrape".into(), e.to_string()))?;
    resp.text()
        .map_err(|e| ExtractError::Extractor("web-scrape".into(), e.to_string()))
}

/// One blocking HTTP GET returning the raw response bytes — the feed extractor
/// path (`rss-feed-extractor`), which hands the bytes straight to `feed-rs`
/// rather than decoding them as a UTF-8 HTML string. Same client / TLS / no-JS
/// posture as [`fetch`]; shares the one network seam this module owns.
pub(super) fn fetch_bytes(url: &str) -> Result<Vec<u8>, ExtractError> {
    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| ExtractError::Extractor("rss".into(), e.to_string()))?
        .error_for_status()
        .map_err(|e| ExtractError::Extractor("rss".into(), e.to_string()))?;
    resp.bytes()
        .map(|b| b.to_vec())
        .map_err(|e| ExtractError::Extractor("rss".into(), e.to_string()))
}

/// Fetch one subresource (CSS, image, font) for the archiver: returns its bytes
/// + MIME, or `None` on any failure (the archiver then leaves an absolute URL).
fn http_subresource(url: &str) -> Option<archive::SubResource> {
    let client = http_client().ok()?;
    let resp = client.get(url).send().ok()?.error_for_status().ok()?;
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let bytes = resp.bytes().ok()?.to_vec();
    Some(archive::SubResource { bytes, mime })
}

/// The shared blocking HTTP client: rustls TLS, gzip, a desktop UA, and a
/// bounded timeout so a hung server can't wedge the extractor.
fn http_client() -> Result<reqwest::blocking::Client, ExtractError> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!("hiker-extract/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| ExtractError::Extractor("web-scrape".into(), e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{archive, extract_with_fetcher};
    use std::collections::HashMap;

    /// An offline archive fetcher that inlines nothing (every subresource
    /// fetch misses), so the transform tests never touch the network.
    fn no_fetch() -> impl Fn(&str) -> Option<archive::SubResource> {
        let map: HashMap<String, archive::SubResource> = HashMap::new();
        move |u: &str| map.get(u).cloned()
    }

    #[test]
    fn extracts_readable_article_with_archive() {
        let html = r#"<html><head><title>Static Article</title></head><body>
            <nav>nav links here</nav>
            <article><h1>Static Article</h1>
            <p>This is a substantial article body with plenty of real prose so that the
               readability content scorer keeps it as the main content and the resulting
               markdown clears the thin-content threshold comfortably for the test.</p>
            <p>A second paragraph keeps the body well above the minimum length the pipeline
               requires before it would start looking at fallback representations instead.</p>
            </article></body></html>"#;
        let out = extract_with_fetcher(html, "https://example.com/post", &no_fetch())
            .expect("article extracted");
        assert!(out.markdown.contains("substantial article body"));
        assert_eq!(
            out.frontmatter.as_ref().unwrap().title.as_deref(),
            Some("Static Article")
        );
        assert_eq!(
            out.frontmatter.as_ref().unwrap().source_url.as_deref(),
            Some("https://example.com/post")
        );
        let archive = out.archive.expect("archive attached");
        assert_eq!(archive.extension, "html");
        assert!(!String::from_utf8_lossy(&archive.bytes).to_ascii_lowercase().contains("<script"));
    }

    #[test]
    fn data_blob_wins_over_thin_readability() {
        // A framework page whose visible HTML is empty chrome, but a JSON-LD
        // blob carries the real article. The blob must win.
        let html = r#"<html><head>
            <script type="application/ld+json">
            {"@type":"Article","headline":"Blob Title",
             "articleBody":"The genuine article content lives only in the JSON-LD data blob here, recovered without executing any JavaScript whatsoever."}
            </script></head>
            <body><div id="app"></div></body></html>"#;
        let out = extract_with_fetcher(html, "https://spa.example/x", &no_fetch())
            .expect("blob extracted");
        assert!(out.markdown.contains("genuine article content lives only in the JSON-LD"));
        assert_eq!(out.frontmatter.unwrap().title.as_deref(), Some("Blob Title"));
    }

    #[test]
    fn empty_page_declines() {
        let html = "<html><head></head><body></body></html>";
        assert!(extract_with_fetcher(html, "https://example.com/", &no_fetch()).is_none());
    }
}
