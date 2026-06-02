//! The feed-subscription poll core: the per-poll work that turns a `mode: feed`
//! capture note into accruing child notes. A feed is *a scheduled,
//! dedup-by-guid, list-mode capture* — so this module reuses the shared
//! companion-folder child-write path (`crate::companion`) the crawl loop also
//! uses, and adds only what recurrence demands: cross-run guid dedup, a
//! content-source toggle, and a retention bound. See `docs/extract.md`
//! "RSS / Atom feeds".
//!
//! One poll ([`poll`]):
//! 1. fetch the feed bytes (the injected [`Fetcher`] seam) and parse them via
//!    `feed-rs` ([`crate::builtin::syndication::parse`]);
//! 2. load the companion-folder **manifest** (`guid → child-path`,
//!    `.feed-manifest.json`) — the persistent cross-run dedup map
//!    (`rss-guid-dedup`);
//! 3. for each entry: a NEW guid writes a new child; a KNOWN guid whose content
//!    changed re-extracts onto its existing child; an unchanged guid is a no-op;
//! 4. prune children beyond the resolved retention bound to the vault trash dir
//!    (`rss-item-retention`);
//! 5. stamp `last_checked` on the feed note so the schedule advances
//!    (`rss-poll-schedule`).
//!
//! The TIMER that decides *when* to call [`poll`] is app-side and deferred (the
//! task-queue IO lane doesn't exist yet) — the "is this feed due?" decision
//! lives in [`crate::capture::FeedParams::is_due`] and the CLI `hiker feed poll`
//! driver exercises the whole core offline. GAP: background scheduling wiring.
//
// status: rss-subscription-lifecycle
// status: rss-guid-dedup

mod manifest;
mod retention;

use std::path::{Path, PathBuf};

use crate::builtin::syndication::{self, FeedEntry};
use crate::capture::{FeedParams, Kind, Mode, Spec};
use crate::companion::{write_child, ChildWrite};
use crate::ExtractError;

use manifest::Manifest;

/// A source of feed bytes: given the feed URL, return its raw bytes. The
/// production impl does a static HTTP GET (the same network seam the web
/// extractor owns); tests inject a fake returning canned RSS/Atom fixtures, so
/// the whole poll core runs offline. Mirrors the crawl loop's [`PageSource`]
/// seam (`crate::crawl::PageSource`).
pub trait Fetcher {
    /// Fetch the feed document at `url`.
    fn fetch_feed(&self, url: &str) -> Result<Vec<u8>, ExtractError>;
    /// Fetch + extract a full article at `url` for the `full_text` toggle
    /// (`rss-content-source`). Returns the article markdown, or `None` when the
    /// link yields nothing. The production impl runs the website extractor; a
    /// summary-only feed (`full_text: false`) never calls this.
    fn fetch_article(&self, url: &str) -> Result<Option<String>, ExtractError>;
}

/// The production feed fetcher: feed bytes + full-article extraction both go
/// through the built-in registry / web path, keeping the whole network surface
/// inside this crate.
pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn fetch_feed(&self, url: &str) -> Result<Vec<u8>, ExtractError> {
        syndication::fetch_bytes(url)
    }

    fn fetch_article(&self, url: &str) -> Result<Option<String>, ExtractError> {
        let source = crate::Source::Url(url.to_string());
        let registry = crate::Registry::with_builtins();
        Ok(registry
            .extract(&source, &crate::Ctx::default())?
            .map(|r| r.extracted.markdown))
    }
}

/// What one poll did: the entries touched, classified. Mirrors the crawl
/// [`Report`](crate::crawl::Report) shape so callers report both uniformly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollReport {
    /// Children created for genuinely new guids this poll.
    pub new_children: Vec<PathBuf>,
    /// Children re-extracted because a known guid's content changed.
    pub updated_children: Vec<PathBuf>,
    /// Children pruned to honor the retention bound (moved to trash).
    pub pruned_children: Vec<PathBuf>,
    /// Count of entries that were already current (no-op).
    pub unchanged: usize,
}

/// A feed-poll failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fetch feed {0}: {1}")]
    Fetch(String, ExtractError),
    #[error("parse feed {0}: {1}")]
    Parse(String, ExtractError),
    #[error("write child: {0}")]
    Write(String),
    #[error("feed manifest: {0}")]
    Manifest(String),
    #[error("the note at {0} is not a `mode: feed` capture note")]
    NotFeed(PathBuf),
    #[error("read/write feed note {0}: {1}")]
    Note(PathBuf, String),
}

/// Poll one feed described by `params` for the feed note at `feed_note_path`
/// (`<dir>/<name>.md`), writing new entries as children into its `<name>/`
/// companion folder stamped with `parent_ulid`. `vault_root` anchors the trash
/// destination. Feed bytes + full-article fetches come from `fetch` (the
/// injected seam). Returns a [`PollReport`]; does NOT stamp `last_checked`
/// (that is [`mark_checked`], called by the note-driven [`poll_note`] wrapper so
/// a raw `poll` stays free of note I/O for the unit tests).
///
/// status: rss-guid-dedup
/// status: rss-content-source
/// status: rss-item-retention
pub fn poll(
    params: &FeedParams,
    feed_note_path: &Path,
    vault_root: &Path,
    parent_ulid: &str,
    item_retention: &str,
    fetch: &dyn Fetcher,
) -> Result<PollReport, Error> {
    let bytes = fetch
        .fetch_feed(&params.url)
        .map_err(|e| Error::Fetch(params.url.clone(), e))?;
    let parsed = syndication::parse(&bytes).map_err(|e| Error::Parse(params.url.clone(), e))?;

    let companion = crate::crawl::job_companion_dir(feed_note_path);
    let mut manifest = Manifest::load(&companion).map_err(|e| Error::Manifest(e.to_string()))?;

    let mut report = PollReport::default();
    for entry in &parsed.entries {
        if entry.guid.trim().is_empty() {
            continue; // no stable identity → can't dedup; skip rather than dupe
        }
        let body = entry_body(entry, params, fetch)?;
        let hash = blake3::hash(body.as_bytes()).to_hex().to_string();

        match manifest.lookup(&entry.guid) {
            Some(record) if record.content_hash == hash => {
                report.unchanged += 1;
            }
            Some(record) => {
                // Known guid, changed content → re-extract onto its child.
                let path = companion.join(&record.child_file);
                rewrite_child(&path, entry, &body, params, parent_ulid)?;
                manifest.update_hash(&entry.guid, &hash);
                report.updated_children.push(path);
            }
            None => {
                // New guid → a new child note.
                let stem = entry_stem(entry);
                let path = write_entry_child(&companion, &stem, entry, &body, params, parent_ulid)?;
                let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default().to_string();
                manifest.insert(&entry.guid, &file, &hash, entry.published.as_deref());
                report.new_children.push(path);
            }
        }
    }

    // Prune to the retention bound, oldest first, moving losers to trash.
    let pruned = retention::prune(&mut manifest, &companion, vault_root, item_retention)
        .map_err(|e| Error::Manifest(e.to_string()))?;
    report.pruned_children = pruned;

    manifest.save(&companion).map_err(|e| Error::Manifest(e.to_string()))?;
    Ok(report)
}

/// Resolve the effective entry body for the content-source toggle
/// (`rss-content-source`): `full_text: true` follows the entry link and runs
/// the website extractor; otherwise the feed-provided `<content:encoded>` /
/// `<summary>` is used. Falls back to the feed content (then the title) when a
/// full-text fetch yields nothing, so a child always has *some* body.
fn entry_body(entry: &FeedEntry, params: &FeedParams, fetch: &dyn Fetcher) -> Result<String, Error> {
    if let Some(article) = full_text_body(entry, params, fetch)? {
        return Ok(article);
    }
    Ok(entry
        .content
        .clone()
        .or_else(|| entry.title.clone())
        .unwrap_or_default())
}

/// The full-article body for the `full_text` toggle, or `None` when the toggle
/// is off, the entry has no link, or the fetched article is empty (so the
/// caller falls back to the feed-provided content).
fn full_text_body(entry: &FeedEntry, params: &FeedParams, fetch: &dyn Fetcher) -> Result<Option<String>, Error> {
    if !params.full_text {
        return Ok(None);
    }
    let Some(link) = entry.link.as_deref() else {
        return Ok(None);
    };
    let article = fetch
        .fetch_article(link)
        .map_err(|e| Error::Fetch(link.to_string(), e))?;
    Ok(article.filter(|a| !a.trim().is_empty()))
}

/// Write a new child note for a feed entry into the companion folder.
fn write_entry_child(
    companion: &Path,
    stem: &str,
    entry: &FeedEntry,
    body: &str,
    params: &FeedParams,
    parent_ulid: &str,
) -> Result<PathBuf, Error> {
    let child = ChildWrite {
        companion_dir: companion,
        stem,
        markdown: body,
        title: entry.title.as_deref(),
        source_url: entry.link.as_deref().unwrap_or(&params.url),
        parent_ulid,
        provenance: "rss",
        archive: None,
    };
    write_child(&child).map_err(|e| Error::Write(e.to_string()))
}

/// Re-extract a known entry whose content changed onto its existing child,
/// overwriting the file at `path` in place (the `extract-version-oplog`
/// re-extract-replace shape — op-log history is the app-layer concern; here we
/// overwrite the body). Preserves the child's filename so wikilinks / the
/// dedup map stay stable.
fn rewrite_child(
    path: &Path,
    entry: &FeedEntry,
    body: &str,
    params: &FeedParams,
    parent_ulid: &str,
) -> Result<(), Error> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("entry")
        .to_string();
    let companion = path.parent().unwrap_or_else(|| Path::new("."));
    // write_child collision-suffixes, so write to a temp companion would dupe;
    // instead remove the old file first, then write with the same stem.
    let _ = std::fs::remove_file(path);
    write_entry_child(companion, &stem, entry, body, params, parent_ulid)?;
    Ok(())
}

/// The filename stem for an entry child: a slug of its title, else of its link,
/// else its guid.
fn entry_stem(entry: &FeedEntry) -> String {
    for candidate in [entry.title.as_deref(), entry.link.as_deref(), Some(entry.guid.as_str())].into_iter().flatten() {
        let s = crate::sidecar::slugify(candidate);
        if !s.is_empty() {
            return s;
        }
    }
    "entry".to_string()
}

/// Poll a feed by reading its `mode: feed` capture note: parse the note's
/// frontmatter, run [`poll`] with the per-feed retention resolved against the
/// `vault_default` (the cascade `rss-item-retention` reuses from
/// `extract-artifact-retention` — per-feed frontmatter wins, else the vault
/// default), and stamp `last_checked` on success. This is the note-driven entry
/// point the CLI `hiker feed poll` driver and the (deferred) timer call.
///
/// status: rss-subscription-lifecycle
pub fn poll_note(
    feed_note_path: &Path,
    vault_root: &Path,
    vault_default_retention: &str,
    fetch: &dyn Fetcher,
) -> Result<PollReport, Error> {
    let (spec, body) = read_feed_note(feed_note_path)?;
    let params = spec
        .feed
        .clone()
        .ok_or_else(|| Error::NotFeed(feed_note_path.to_path_buf()))?;
    let parent_ulid = note_ulid(&body).unwrap_or_else(|| ulid::Ulid::new().to_string());
    let retention = params
        .item_retention
        .clone()
        .unwrap_or_else(|| vault_default_retention.to_string());

    let report = poll(&params, feed_note_path, vault_root, &parent_ulid, &retention, fetch)?;
    mark_checked(feed_note_path, &spec, &body)?;
    Ok(report)
}

/// Write a fresh `mode: feed` capture note for `params` at `path`, stamping the
/// subscription ULID as `hiker.id` so accruing children's `hiker.parent`
/// matches. The note's body is user-owned (`fill_body: false`); the
/// subscribed/last-checked status renders in the form, not the body. The
/// `hiker feed add` CLI driver calls this. Returns the subscription ULID.
///
/// status: rss-subscription-lifecycle
pub fn write_feed_note(path: &Path, params: &FeedParams, feed_ulid: &str) -> Result<(), Error> {
    let spec = Spec {
        kind: Kind::Capture,
        mode: Mode::Feed,
        source: Some(params.url.clone()),
        fill_body: false,
        extractor: None,
        crawl: None,
        feed: Some(params.clone()),
    };
    let content = assemble_note(&spec, feed_ulid, &format!("# Feed: {}\n\nNotes about this subscription.\n", params.url));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Note(path.to_path_buf(), e.to_string()))?;
    }
    std::fs::write(path, content).map_err(|e| Error::Note(path.to_path_buf(), e.to_string()))
}

/// Stamp `capture.feed.last_checked` (RFC-3339, now) on the feed note so the
/// schedule advances, preserving its `hiker.id` and body.
///
/// status: rss-poll-schedule
fn mark_checked(path: &Path, spec: &Spec, body: &str) -> Result<(), Error> {
    let mut params = spec.feed.clone().ok_or_else(|| Error::NotFeed(path.to_path_buf()))?;
    params.last_checked = Some(now_rfc3339());
    let updated = Spec { feed: Some(params), ..spec.clone() };
    let ulid = note_ulid(body).unwrap_or_default();
    let content = assemble_note(&updated, &ulid, &note_body(body));
    std::fs::write(path, content).map_err(|e| Error::Note(path.to_path_buf(), e.to_string()))
}

/// Whether the note at `path` is a `mode: feed` capture note. The CLI `feed
/// poll` driver uses this to discover which vault notes are feeds without
/// re-implementing frontmatter parsing in the adapter layer.
///
/// status: rss-subscription-lifecycle
pub fn is_feed_note(path: &Path) -> bool {
    matches!(read_feed_note(path), Ok((spec, _)) if spec.mode == Mode::Feed)
}

/// Read a feed note into its parsed [`Spec`] + raw file content (so callers can
/// recover the ULID stamp and the user-owned body).
fn read_feed_note(path: &Path) -> Result<(Spec, String), Error> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::Note(path.to_path_buf(), e.to_string()))?;
    let fm = frontmatter_value(&content).ok_or_else(|| Error::NotFeed(path.to_path_buf()))?;
    let spec = Spec::from_frontmatter(&fm).map_err(|_| Error::NotFeed(path.to_path_buf()))?;
    if spec.mode != Mode::Feed {
        return Err(Error::NotFeed(path.to_path_buf()));
    }
    Ok((spec, content))
}

/// Assemble a feed note: the spec frontmatter (with `hiker.id` stamped) + body.
fn assemble_note(spec: &Spec, feed_ulid: &str, body: &str) -> String {
    let mut root = match spec.to_yaml() {
        serde_yml::Value::Mapping(m) => m,
        _ => serde_yml::Mapping::new(),
    };
    if let (false, Some(serde_yml::Value::Mapping(hiker))) = (feed_ulid.is_empty(), root.get_mut("hiker")) {
        hiker.insert(serde_yml::Value::from("id"), serde_yml::Value::from(feed_ulid.to_string()));
    }
    let yaml = serde_yml::to_string(&serde_yml::Value::Mapping(root)).unwrap_or_default();
    let yaml = yaml.trim_end_matches('\n');
    format!("---\n{yaml}\n---\n{body}")
}

/// Parse the YAML frontmatter block out of a note's content.
fn frontmatter_value(content: &str) -> Option<serde_yml::Value> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    serde_yml::from_str(&rest[..end + 1]).ok()
}

/// The note body (everything after the frontmatter block), or the whole content
/// when there is no frontmatter.
fn note_body(content: &str) -> String {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content.to_string();
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches('\n').to_string(),
        None => content.to_string(),
    }
}

/// Read the `hiker.id` ULID stamp out of a feed note's content.
fn note_ulid(content: &str) -> Option<String> {
    frontmatter_value(content)?
        .get("hiker")?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

/// Now as an RFC-3339 UTC string for the `last_checked` stamp.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
