//! RSS / Atom / JSON-Feed parsing — the pure-Rust normalizer behind
//! [`crate::builtin::FeedExtractor`] and the feed-poll core (`crate::feed`).
//!
//! All dialect handling (RSS 0.9x / 1.0 / 2.0, Atom, JSON Feed; date formats;
//! namespaces) is delegated to `feed-rs`, the unified parser built on
//! `quick-xml` — so the long tail of feed formats isn't ours to maintain
//! (`rss-feed-extractor`). The bytes are **parsed, never executed**: this is
//! pure string/XML processing with no script engine, mirroring the
//! no-JavaScript stance of the website extractor.
//!
//! Two surfaces sit on top of [`parse`]:
//!
//! - the [`crate::builtin::FeedExtractor`] trait impl, which the registry can
//!   route a feed URL to and which emits each entry's link as a `next_url`
//!   (so a feed participates in the frontier loop exactly like any other
//!   crawl-capable extractor, and serves the thin-page RSS fallback);
//! - [`parse`], which the feed-poll core calls directly for the full per-entry
//!   `{ guid, link, title, published, content? }` it needs for cross-run guid
//!   dedup.
//
// status: rss-feed-extractor

use crate::ExtractError;

/// Fetch raw feed bytes over the static-fetch HTTP path (no JavaScript), for the
/// production feed-poll fetcher (`crate::feed::HttpFetcher`). Re-exposes the
/// web module's byte fetch so the feed-poll core (a crate-root module) can reach
/// it without the web module being public.
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, ExtractError> {
    super::web::fetch_bytes(url)
}

/// One normalized feed entry: the fields the poll core needs for dedup and
/// child-note writing. Dialect-specific shapes (`<guid>` vs Atom `<id>`,
/// `<content:encoded>` vs `<summary>`, `pubDate` vs `<updated>`) are all
/// collapsed here by `feed-rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// The stable cross-run identity (`<guid>` / Atom `<id>`). Falls back to
    /// the entry link, then the title, when the feed omits an explicit id —
    /// so dedup still has *something* stable to key on.
    pub guid: String,
    /// The entry's canonical link (the article URL). `None` for a feed that
    /// only carries inline content with no outbound link.
    pub link: Option<String>,
    /// The entry title, used as the child note's title + filename slug.
    pub title: Option<String>,
    /// The publication / update timestamp as an RFC-3339 string, when present.
    /// Used for retention ordering (oldest-first pruning).
    pub published: Option<String>,
    /// The feed-provided body (`<content:encoded>` preferred, else
    /// `<summary>` / Atom `<summary>`), as raw HTML/markup. `None` for a
    /// link-only feed. The content-source toggle decides whether this is used
    /// or the linked article is fetched instead (`rss-content-source`).
    pub content: Option<String>,
}

/// A parsed feed: its own title (used to name the subscription) plus the
/// normalized entries in feed order (newest first, as feeds conventionally
/// emit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    /// The feed's title (channel `<title>` / Atom feed `<title>`).
    pub title: Option<String>,
    /// The entries in the order the feed presented them.
    pub entries: Vec<FeedEntry>,
}

/// Parse raw feed bytes (RSS / Atom / JSON Feed) into a normalized [`Feed`].
/// Pure: no network, no execution — just `feed-rs` over the bytes. A malformed
/// feed is an [`ExtractError::Extractor`].
///
/// status: rss-feed-extractor
pub fn parse(bytes: &[u8]) -> Result<Feed, ExtractError> {
    let parsed = feed_rs::parser::parse(bytes)
        .map_err(|e| ExtractError::Extractor("rss".into(), e.to_string()))?;
    let title = parsed.title.map(|t| t.content);
    let entries = parsed.entries.iter().map(normalize_entry).collect();
    Ok(Feed { title, entries })
}

/// Collapse one `feed-rs` entry into our normalized [`FeedEntry`], applying the
/// guid fallback chain and the `content:encoded` > `summary` body preference.
fn normalize_entry(e: &feed_rs::model::Entry) -> FeedEntry {
    let link = e.links.first().map(|l| l.href.clone());
    let title = e.title.as_ref().map(|t| t.content.clone());
    // `<content:encoded>` (the full body) wins over `<summary>` (the teaser).
    let content = e
        .content
        .as_ref()
        .and_then(|c| c.body.clone())
        .or_else(|| e.summary.as_ref().map(|s| s.content.clone()));
    // guid fallback: explicit id -> link -> title -> empty (the caller treats
    // an empty guid as "no stable identity" and skips dedup for it).
    let guid = if e.id.trim().is_empty() {
        link.clone().or_else(|| title.clone()).unwrap_or_default()
    } else {
        e.id.clone()
    };
    let published = e
        .published
        .or(e.updated)
        .and_then(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true).into());
    FeedEntry { guid, link, title, published, content }
}

#[cfg(test)]
mod tests {
    use super::parse;

    const RSS_2_0: &str = r#"<?xml version="1.0"?>
<rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
  <channel>
    <title>Example Feed</title>
    <item>
      <title>First Post</title>
      <link>https://example.com/first</link>
      <guid>tag:example.com,2026:first</guid>
      <pubDate>Tue, 01 Jan 2026 10:00:00 GMT</pubDate>
      <content:encoded><![CDATA[<p>The full body of the first post.</p>]]></content:encoded>
    </item>
    <item>
      <title>Second Post</title>
      <link>https://example.com/second</link>
      <guid>tag:example.com,2026:second</guid>
      <pubDate>Wed, 02 Jan 2026 10:00:00 GMT</pubDate>
      <description>Just a summary of the second post.</description>
    </item>
  </channel>
</rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Example</title>
  <entry>
    <title>Atom Entry</title>
    <link href="https://atom.example/post"/>
    <id>urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a</id>
    <updated>2026-01-03T18:30:02Z</updated>
    <summary>An atom summary.</summary>
  </entry>
</feed>"#;

    #[test]
    fn parses_rss_2_0_entries() {
        let feed = parse(RSS_2_0.as_bytes()).expect("parse rss");
        assert_eq!(feed.title.as_deref(), Some("Example Feed"));
        assert_eq!(feed.entries.len(), 2);
        let first = &feed.entries[0];
        assert_eq!(first.guid, "tag:example.com,2026:first");
        assert_eq!(first.link.as_deref(), Some("https://example.com/first"));
        assert_eq!(first.title.as_deref(), Some("First Post"));
        // `<content:encoded>` is preferred as the body.
        assert!(first.content.as_deref().unwrap().contains("full body"));
        // the second item has only a <description>, used as the body.
        assert!(feed.entries[1].content.as_deref().unwrap().contains("summary of the second"));
    }

    #[test]
    fn parses_atom_with_id_and_updated() {
        let feed = parse(ATOM.as_bytes()).expect("parse atom");
        assert_eq!(feed.title.as_deref(), Some("Atom Example"));
        assert_eq!(feed.entries.len(), 1);
        let e = &feed.entries[0];
        assert_eq!(e.guid, "urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a");
        assert_eq!(e.link.as_deref(), Some("https://atom.example/post"));
        assert!(e.published.as_deref().unwrap().starts_with("2026-01-03"));
        assert!(e.content.as_deref().unwrap().contains("atom summary"));
    }

    #[test]
    fn malformed_feed_errors() {
        assert!(parse(b"not a feed at all").is_err());
    }
}
