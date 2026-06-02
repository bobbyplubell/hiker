//! Thin-page fallback selection. When the main fetch yields little usable
//! content (readability + data-blob both came up short), the extractor looks
//! for an alternate representation of the same page declared in its `<head>`,
//! and tries them in order: the page's declared RSS/Atom full-text feed, an AMP
//! variant (`<link rel="amphtml">`), then a print view. The first that produces
//! real content wins. This module only *selects* the candidate URLs from the
//! HTML; the orchestrator in `super` fetches + re-extracts each in order (so
//! the network stays one injectable seam). See `docs/extract.md`
//! `extract-web-fallbacks`.
//
// status: extract-web-fallbacks

use scraper::{Html, Selector};
use url::Url;

/// An ordered list of alternate-representation URLs to try when the primary
/// extraction is thin, most-likely-full-text first.
pub(super) fn candidates(html: &str, base_url: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    let resolve = |href: &str| -> Option<String> {
        match &base {
            Some(b) => b.join(href).ok().map(|u| u.to_string()),
            None => Some(href.to_string()),
        }
    };

    let mut out = Vec::new();
    // 1. Declared full-text RSS/Atom feed.
    out.extend(feed_links(&doc).into_iter().filter_map(|h| resolve(&h)));
    // 2. AMP variant — usually a stripped, content-first rendering.
    if let Some(amp) = link_href(&doc, r#"link[rel="amphtml"]"#)
        && let Some(u) = resolve(&amp)
    {
        out.push(u);
    }
    // 3. Print view — a `?print` / print-stylesheet alternate, when declared.
    if let Some(print) = link_href(&doc, r#"link[rel="alternate"][media="print"]"#)
        && let Some(u) = resolve(&print)
    {
        out.push(u);
    }
    dedup_preserve(out)
}

/// The `href`s of any declared RSS/Atom feed `<link>`s, in document order.
fn feed_links(doc: &Html) -> Vec<String> {
    const FEED_TYPES: &[&str] = &["application/rss+xml", "application/atom+xml", "application/feed+json"];
    let Ok(sel) = Selector::parse(r#"link[rel="alternate"][type]"#) else {
        return Vec::new();
    };
    doc.select(&sel)
        .filter(|el| el.value().attr("type").is_some_and(|t| FEED_TYPES.contains(&t)))
        .filter_map(|el| el.value().attr("href").map(str::to_string))
        .collect()
}

/// The `href` of the first element matching `selector`, if any.
fn link_href(doc: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    doc.select(&sel)
        .next()
        .and_then(|el| el.value().attr("href"))
        .map(str::to_string)
}

/// Drop duplicate URLs while preserving first-seen order.
fn dedup_preserve(urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    urls.into_iter().filter(|u| seen.insert(u.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::candidates;

    #[test]
    fn orders_feed_amp_print() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            <link rel="amphtml" href="/amp/post">
            <link rel="alternate" media="print" href="/post?print=1">
            </head><body></body></html>"#;
        let c = candidates(html, "https://example.com/post");
        assert_eq!(
            c,
            vec![
                "https://example.com/feed.xml".to_string(),
                "https://example.com/amp/post".to_string(),
                "https://example.com/post?print=1".to_string(),
            ]
        );
    }

    #[test]
    fn resolves_relative_against_base() {
        let html = r#"<html><head><link rel="amphtml" href="amp.html"></head></html>"#;
        let c = candidates(html, "https://site.test/blog/post/");
        assert_eq!(c, vec!["https://site.test/blog/post/amp.html".to_string()]);
    }

    #[test]
    fn no_alternates_is_empty() {
        let html = "<html><head><title>plain</title></head><body></body></html>";
        assert!(candidates(html, "https://example.com/").is_empty());
    }

    #[test]
    fn dedups_repeated_links() {
        let html = r#"<html><head>
            <link rel="alternate" type="application/atom+xml" href="/feed">
            <link rel="alternate" type="application/rss+xml" href="/feed">
            </head></html>"#;
        let c = candidates(html, "https://example.com/");
        assert_eq!(c, vec!["https://example.com/feed".to_string()]);
    }
}
