//! The readability + HTML→markdown transform behind the web extractor. Takes
//! the raw fetched HTML, isolates the main article content with a pure-Rust
//! readability pass (`readable-readability`, a markup5ever DOM transform with
//! no network of its own), serializes the isolated subtree, and emits a clean
//! markdown body with `htmd`. The page title parsed out of the readability
//! metadata rides along so the sidecar write path can slug a filename from it.
//! See `docs/extract.md` `extract-web-readability`.
//
// status: extract-web-readability

use readable_readability::Readability;

/// One readability transform result: the markdown body plus the best page
/// title we could recover (readability's article title, falling back to the
/// raw `<title>`). The caller decides whether the body is "real content"
/// (the fallback chain in `super::fallback` gates on its length).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Article {
    /// The cleaned markdown body.
    pub markdown: String,
    /// The recovered page title, if any.
    pub title: Option<String>,
}

/// Run the readability pass over `html` (anchored at `base_url` so relative
/// links resolve), serialize the isolated content node, and convert it to
/// markdown. `base_url` is the page's own URL — readability uses it to
/// absolutize `href`/`src` attributes before we serialize.
pub(super) fn to_article(html: &str, base_url: &str) -> Article {
    let mut readability = Readability::new();
    if let Ok(parsed) = url::Url::parse(base_url) {
        readability.base_url(parsed);
    }
    let (node, meta) = readability.parse(html);

    // The readability node serializes back to HTML; htmd turns that into a
    // clean markdown body. A serialize/convert failure degrades to an empty
    // body so the fallback chain can take over rather than hard-failing.
    let inner_html = node.to_string();
    let markdown = htmd::convert(&inner_html).unwrap_or_default();

    let title = meta
        .article_title
        .or(meta.page_title)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    Article { markdown: collapse_blank_runs(&markdown), title }
}

/// Collapse the runs of 3+ newlines htmd can leave between blocks down to a
/// single blank line, and trim trailing whitespace, so the sidecar body reads
/// cleanly. Mirrors the PDF extractor's `normalize` posture: tidy whitespace
/// only, never drop content.
fn collapse_blank_runs(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut blank_run = 0u32;
    for line in markdown.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run >= 2 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out.trim_start().to_string()
}

/// The count of non-whitespace characters in `markdown` — the "is this real
/// content?" signal the fallback chain (`extract-web-fallbacks`) gates on. A
/// thin readability result (nav-only chrome, a cookie wall) sits well below a
/// real article.
pub(super) fn content_len(markdown: &str) -> usize {
    markdown.chars().filter(|c| !c.is_whitespace()).count()
}

#[cfg(test)]
mod tests {
    use super::{content_len, to_article};

    #[test]
    fn isolates_article_and_emits_markdown() {
        let html = r#"<!DOCTYPE html><html><head><title>My Page</title></head>
            <body>
              <nav><a href="/">home</a><a href="/about">about</a></nav>
              <article>
                <h1>The Real Heading</h1>
                <p>This is the first substantial paragraph of the article body, long
                   enough that readability keeps it over the surrounding chrome.</p>
                <p>And a second paragraph with even more genuine prose content so the
                   main-content scorer prefers this subtree to the navigation.</p>
              </article>
              <footer>copyright boilerplate</footer>
            </body></html>"#;
        let art = to_article(html, "https://example.com/post");
        // Readability promotes the lone <h1> to the article title and keeps the
        // paragraphs as the body; the surrounding chrome is dropped.
        assert_eq!(art.title.as_deref(), Some("The Real Heading"));
        assert!(art.markdown.contains("first substantial paragraph"));
        assert!(!art.markdown.contains("copyright boilerplate"), "dropped footer chrome");
    }

    #[test]
    fn recovers_title() {
        let html = "<html><head><title>Recovered Title</title></head><body>\
            <article><p>Some article prose that is long enough to be kept by the \
            readability content scorer as the main body of this document.</p>\
            </article></body></html>";
        let art = to_article(html, "https://example.com/");
        assert_eq!(art.title.as_deref(), Some("Recovered Title"));
    }

    #[test]
    fn content_len_counts_non_whitespace() {
        assert_eq!(content_len("a b\nc"), 3);
        assert_eq!(content_len("   \n  "), 0);
    }
}
