//! Offline tests for the feed-poll core. A fake [`Fetcher`] returns canned
//! RSS/Atom fixtures (and canned full-text articles), so the whole poll path —
//! guid dedup across two polls, content-source off vs on, item-retention
//! pruning, and the "is due?" schedule decision — runs fully offline against a
//! temp vault. NO live network.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::capture::FeedParams;
use crate::ExtractError;

use super::{poll, poll_note, write_feed_note, Fetcher};

// --- a fake, offline feed fetcher ------------------------------------------

/// A fake fetcher: a fixed feed document, plus a `link → article` map for the
/// full-text path. Records which article URLs were fetched so a test can assert
/// the content-source toggle actually followed (or didn't follow) links.
struct FakeFetch {
    feed: RefCell<Vec<u8>>,
    articles: HashMap<String, String>,
    article_hits: RefCell<Vec<String>>,
}

impl FakeFetch {
    fn new(feed: &str) -> Self {
        Self {
            feed: RefCell::new(feed.as_bytes().to_vec()),
            articles: HashMap::new(),
            article_hits: RefCell::new(Vec::new()),
        }
    }

    fn with_article(mut self, url: &str, body: &str) -> Self {
        self.articles.insert(url.to_string(), body.to_string());
        self
    }

    fn set_feed(&self, feed: &str) {
        *self.feed.borrow_mut() = feed.as_bytes().to_vec();
    }
}

impl Fetcher for FakeFetch {
    fn fetch_feed(&self, _url: &str) -> Result<Vec<u8>, ExtractError> {
        Ok(self.feed.borrow().clone())
    }

    fn fetch_article(&self, url: &str) -> Result<Option<String>, ExtractError> {
        self.article_hits.borrow_mut().push(url.to_string());
        Ok(self.articles.get(url).cloned())
    }
}

/// A temp vault root + a feed note path inside it.
fn vault() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let note = root.join("my-feed.md");
    (dir, root, note)
}

fn companion(note: &Path) -> PathBuf {
    crate::crawl::job_companion_dir(note)
}

const TWO_ITEMS: &str = r#"<rss version="2.0">
<channel>
  <title>Test Feed</title>
  <item><title>Alpha</title><link>https://feed.test/alpha</link><guid>g-alpha</guid>
    <pubDate>Tue, 01 Jan 2026 10:00:00 GMT</pubDate>
    <description>Alpha summary.</description></item>
  <item><title>Beta</title><link>https://feed.test/beta</link><guid>g-beta</guid>
    <pubDate>Wed, 02 Jan 2026 10:00:00 GMT</pubDate>
    <description>Beta summary.</description></item>
</channel></rss>"#;

// --- guid dedup across polls -----------------------------------------------

#[test]
fn first_poll_writes_all_new_children() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS);
    let params = FeedParams::new("https://feed.test/rss");
    let report = poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    assert_eq!(report.new_children.len(), 2);
    assert!(companion(&note).join("alpha.md").exists());
    assert!(companion(&note).join("beta.md").exists());
}

#[test]
fn second_poll_dedups_by_guid_only_new_entry_writes() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS);
    let params = FeedParams::new("https://feed.test/rss");

    // First poll: both alpha + beta land.
    let first = poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    assert_eq!(first.new_children.len(), 2);

    // Second feed state: beta unchanged + a NEW gamma. alpha dropped from the
    // feed window but its child stays (dedup is by guid, not feed presence).
    let next = r#"<rss version="2.0"><channel><title>Test Feed</title>
      <item><title>Beta</title><link>https://feed.test/beta</link><guid>g-beta</guid>
        <pubDate>Wed, 02 Jan 2026 10:00:00 GMT</pubDate><description>Beta summary.</description></item>
      <item><title>Gamma</title><link>https://feed.test/gamma</link><guid>g-gamma</guid>
        <pubDate>Thu, 03 Jan 2026 10:00:00 GMT</pubDate><description>Gamma summary.</description></item>
    </channel></rss>"#;
    fetch.set_feed(next);
    let second = poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    assert_eq!(second.new_children.len(), 1, "only gamma is new");
    assert_eq!(second.unchanged, 1, "beta unchanged is a no-op");
    assert!(companion(&note).join("gamma.md").exists());
}

#[test]
fn changed_entry_reextracts_onto_existing_child() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS);
    let params = FeedParams::new("https://feed.test/rss");
    poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();

    // alpha's content changes; same guid.
    let changed = TWO_ITEMS.replace("Alpha summary.", "Alpha summary, now revised with more detail.");
    fetch.set_feed(&changed);
    let report = poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    assert_eq!(report.new_children.len(), 0, "no new guids");
    assert_eq!(report.updated_children.len(), 1, "alpha re-extracted");
    assert_eq!(report.unchanged, 1, "beta still unchanged");
    let body = std::fs::read_to_string(companion(&note).join("alpha.md")).unwrap();
    assert!(body.contains("now revised"), "child body overwritten; got: {body}");
}

// --- content-source toggle -------------------------------------------------

#[test]
fn full_text_off_uses_feed_summary() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS).with_article("https://feed.test/alpha", "FULL ARTICLE");
    let params = FeedParams::new("https://feed.test/rss"); // full_text defaults off
    poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    let body = std::fs::read_to_string(companion(&note).join("alpha.md")).unwrap();
    assert!(body.contains("Alpha summary"), "summary used; got: {body}");
    assert!(fetch.article_hits.borrow().is_empty(), "no article fetched when full_text off");
}

#[test]
fn full_text_on_follows_link_and_uses_article() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS)
        .with_article("https://feed.test/alpha", "The full alpha article body.")
        .with_article("https://feed.test/beta", "The full beta article body.");
    let mut params = FeedParams::new("https://feed.test/rss");
    params.full_text = true;
    poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    let body = std::fs::read_to_string(companion(&note).join("alpha.md")).unwrap();
    assert!(body.contains("full alpha article body"), "article used; got: {body}");
    assert_eq!(fetch.article_hits.borrow().len(), 2, "both links followed");
}

// --- item retention --------------------------------------------------------

#[test]
fn retention_keep_n_prunes_oldest() {
    let (_d, root, note) = vault();
    // Three dated items oldest→newest: alpha(Jan1), beta(Jan2), gamma(Jan3).
    let three = r#"<rss version="2.0"><channel><title>F</title>
      <item><title>Alpha</title><link>https://f.test/a</link><guid>g-a</guid>
        <pubDate>Tue, 01 Jan 2026 10:00:00 GMT</pubDate><description>a</description></item>
      <item><title>Beta</title><link>https://f.test/b</link><guid>g-b</guid>
        <pubDate>Wed, 02 Jan 2026 10:00:00 GMT</pubDate><description>b</description></item>
      <item><title>Gamma</title><link>https://f.test/c</link><guid>g-c</guid>
        <pubDate>Thu, 03 Jan 2026 10:00:00 GMT</pubDate><description>c</description></item>
    </channel></rss>"#;
    let fetch = FakeFetch::new(three);
    let params = FeedParams::new("https://f.test/rss");
    let report = poll(&params, &note, &root, "FEEDULID", "keep:2", &fetch).unwrap();
    assert_eq!(report.new_children.len(), 3, "all three written before pruning");
    assert_eq!(report.pruned_children.len(), 1, "one pruned to honor keep:2");
    // alpha is the oldest → pruned to trash, not deleted.
    assert!(!companion(&note).join("alpha.md").exists(), "oldest pruned from companion");
    assert!(companion(&note).join("beta.md").exists());
    assert!(companion(&note).join("gamma.md").exists());
    let trash = root.join(".hiker").join("trash").join("alpha.md");
    assert!(trash.exists(), "pruned child moved to trash, not deleted");
}

#[test]
fn retention_forever_prunes_nothing() {
    let (_d, root, note) = vault();
    let fetch = FakeFetch::new(TWO_ITEMS);
    let params = FeedParams::new("https://f.test/rss");
    let report = poll(&params, &note, &root, "FEEDULID", "forever", &fetch).unwrap();
    assert!(report.pruned_children.is_empty());
}

// --- note-driven poll + last_checked stamp ---------------------------------

#[test]
fn poll_note_stamps_last_checked() {
    let (_d, root, note) = vault();
    let mut params = FeedParams::new("https://feed.test/rss");
    params.poll_interval = Some("6h".into());
    write_feed_note(&note, &params, "FEEDULID").unwrap();

    let fetch = FakeFetch::new(TWO_ITEMS);
    let report = poll_note(&note, &root, "forever", &fetch).unwrap();
    assert_eq!(report.new_children.len(), 2);

    // The note now carries last_checked, and re-parses as the same feed.
    let content = std::fs::read_to_string(&note).unwrap();
    assert!(content.contains("last_checked"), "last_checked stamped; got: {content}");
    assert!(content.contains("FEEDULID"), "feed ulid preserved");
}

#[test]
fn poll_note_uses_vault_default_retention_when_unset() {
    let (_d, root, note) = vault();
    // No per-feed retention → vault default keep:1 should prune to 1.
    let three = r#"<rss version="2.0"><channel><title>F</title>
      <item><title>A</title><link>https://f.test/a</link><guid>g-a</guid>
        <pubDate>Tue, 01 Jan 2026 10:00:00 GMT</pubDate><description>a</description></item>
      <item><title>B</title><link>https://f.test/b</link><guid>g-b</guid>
        <pubDate>Wed, 02 Jan 2026 10:00:00 GMT</pubDate><description>b</description></item>
    </channel></rss>"#;
    let params = FeedParams::new("https://f.test/rss");
    write_feed_note(&note, &params, "FEEDULID").unwrap();
    let fetch = FakeFetch::new(three);
    let report = poll_note(&note, &root, "keep:1", &fetch).unwrap();
    assert_eq!(report.pruned_children.len(), 1, "vault default keep:1 applied");
}

// --- is_due scheduling -----------------------------------------------------

fn secs(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339).unwrap().timestamp()
}

#[test]
fn never_checked_feed_is_due() {
    let mut p = FeedParams::new("https://f.test/rss");
    p.poll_interval = Some("6h".into());
    assert!(p.is_due(secs("2026-01-01T00:00:00Z")));
}

#[test]
fn feed_within_interval_is_not_due() {
    let mut p = FeedParams::new("https://f.test/rss");
    p.poll_interval = Some("6h".into());
    p.last_checked = Some("2026-01-01T00:00:00Z".into());
    // one hour later, interval is 6h → not due.
    assert!(!p.is_due(secs("2026-01-01T01:00:00Z")));
    // seven hours later → due.
    assert!(p.is_due(secs("2026-01-01T07:00:00Z")));
}

#[test]
fn manual_only_feed_never_auto_due() {
    let mut p = FeedParams::new("https://f.test/rss");
    p.poll_interval = None; // manual-only
    p.last_checked = Some("2020-01-01T00:00:00Z".into());
    assert!(!p.is_due(secs("2026-01-01T00:00:00Z")));
}

#[test]
fn paused_feed_is_never_due() {
    let mut p = FeedParams::new("https://f.test/rss");
    p.poll_interval = Some("1m".into());
    p.paused = true;
    assert!(!p.is_due(secs("2026-01-01T00:00:00Z")));
}
