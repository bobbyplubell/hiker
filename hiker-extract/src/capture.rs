//! The capture-spec-note frontmatter model. One note primitive drives every
//! capture: a note carrying `hiker.kind: capture` (with `capture.mode:
//! clip | crawl | feed`) declares a source + extraction params in its
//! frontmatter; a Run action (a later phase) executes it. Single clip, crawl
//! job, and RSS feed are three instances of the same note. This module is
//! just the data model + parse/serialize; the Run UI is not in Phase 2. See
//! `docs/extract.md` `capture-spec-note` / `capture-fill-body-toggle`.
//
// status: capture-spec-note
// status: capture-fill-body-toggle

use serde_yml::Value as Yaml;

/// The three capture instances, the `capture.mode` discriminator. They differ
/// only in source kind and fan-out, not in mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A single URL or local path: one-shot, re-runnable. Output is the
    /// note's own body (`fill_body: true`) or one child.
    Clip,
    /// Seed(s) + scope/depth: a one-shot snapshot, re-runnable. Output is
    /// child notes in a companion folder.
    Crawl,
    /// A feed URL + poll interval: a living subscription whose new entries
    /// accrue as child notes on each poll.
    Feed,
}

impl Mode {
    /// The wire string (`clip` / `crawl` / `feed`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Clip => "clip",
            Mode::Crawl => "crawl",
            Mode::Feed => "feed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "clip" => Some(Mode::Clip),
            "crawl" => Some(Mode::Crawl),
            "feed" => Some(Mode::Feed),
            _ => None,
        }
    }
}

/// The `hiker.kind` discriminator. Only `capture` is modeled here; any other
/// note (or a missing `kind`) is not a capture-spec note. A non-`.md`
/// extension with the discriminator is treated as a regular note by the
/// caller (mirrors `trail-doc-shape`); this parser doesn't see the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Capture,
}

/// A parsed capture-spec note's frontmatter fields. Produced by
/// [`Spec::from_frontmatter`]; serialized back by
/// [`Spec::to_yaml`]. Phase 5 adds the `mode: crawl` parameters
/// ([`CrawlParams`]); feed poll/retention fields layer on later without
/// changing the discriminator shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Always `Capture` once parsed (the discriminator gate passed).
    pub kind: Kind,
    /// `capture.mode` — the instance discriminator.
    pub mode: Mode,
    /// The declared source: a URL or a vault-relative/local path. Empty when
    /// the frontmatter omits it (a freshly-created spec the user hasn't
    /// filled in yet). For a multi-seed `mode: crawl` list, the first seed
    /// lives here and the rest in [`CrawlParams::seeds`].
    pub source: Option<String>,
    /// `hiker.fill_body` (default `false`). `false` = the body is user-owned
    /// prose about the capture, never touched by Run, re-runs are safe.
    /// `true` = the body is a linked extracted sidecar, read-only in the
    /// editor, overwritten in place on re-run. `fill_body` IS the body's
    /// link-state switch — there is no separate `link_state` field on a
    /// capture-spec note.
    ///
    /// status: capture-fill-body-toggle
    pub fill_body: bool,
    /// `hiker.extractor` — a per-source override pinning a specific extractor
    /// and bypassing match order (`extract-per-source-override`).
    pub extractor: Option<String>,
    /// The `capture.crawl` parameter block, present only for `mode: crawl`
    /// notes. The frontier loop reads its scope/depth/pattern fields; a
    /// `clip`/`feed` note leaves it `None`.
    ///
    /// status: crawl-job-note
    pub crawl: Option<CrawlParams>,
    /// The `capture.feed` parameter block, present only for `mode: feed`
    /// notes. The poll core reads its `poll_interval` / `full_text` /
    /// `item_retention` / `last_checked` fields; a `clip`/`crawl` note leaves
    /// it `None`.
    ///
    /// status: rss-subscription-lifecycle
    pub feed: Option<FeedParams>,
}

/// The feed-subscription parameter block, parsed from `capture.feed` in a
/// `mode: feed` note's frontmatter. A feed is a *living subscription*, not a
/// one-shot: these fields drive recurrence (`poll_interval` / `last_checked`),
/// cross-run dedup behavior, the content-source toggle (`full_text`), and the
/// item-retention bound. The `guid → child-path` dedup map itself lives in the
/// companion-folder manifest, not here, so this block stays small and the
/// user-facing note doesn't bloat with hundreds of guid rows. See
/// `docs/extract.md` "RSS / Atom feeds".
///
/// status: rss-subscription-lifecycle
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedParams {
    /// The feed URL to poll.
    pub url: String,
    /// `poll_interval` — how often the timer should re-fetch (e.g. `30m`,
    /// `6h`). `None` means manual-Run only (no recurring poll). Parsed into a
    /// `Duration` by [`parse_interval`] for the "is due?" decision
    /// (`rss-poll-schedule`).
    pub poll_interval: Option<String>,
    /// `last_checked` — the RFC-3339 timestamp of the last successful poll,
    /// stamped by the poll core. `None` on a never-polled feed (always due).
    /// Compared against `last_checked + poll_interval` to decide due-ness.
    ///
    /// status: rss-poll-schedule
    pub last_checked: Option<String>,
    /// `full_text` — the content-source toggle (`rss-content-source`). `false`
    /// (default) stores the feed-provided `<content:encoded>` / `<summary>` as
    /// the child body; `true` follows each entry link and runs the website
    /// extractor for the full article.
    ///
    /// status: rss-content-source
    pub full_text: bool,
    /// `item_retention` — the per-feed child-count bound (`rss-item-retention`):
    /// `keep:N` / `forever`. `None` falls back to the vault
    /// `[extract].feed_item_retention` default in the resolution cascade.
    ///
    /// status: rss-item-retention
    pub item_retention: Option<String>,
    /// `paused` — a subscription that is paused does not poll even when due
    /// (the pause/resume lifecycle affordance, `rss-subscription-lifecycle`).
    pub paused: bool,
}

impl FeedParams {
    /// A bare feed subscription over `url` with no interval (manual-Run only),
    /// summary content, and default retention — the no-config CLI entry point.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            poll_interval: None,
            last_checked: None,
            full_text: false,
            item_retention: None,
            paused: false,
        }
    }

    /// Parse the `capture.feed` block from a frontmatter mapping, falling back
    /// to `fallback_source` for the URL when the block omits it.
    fn from_capture(capture: Option<&Yaml>, fallback_source: Option<&str>) -> Self {
        let feed = capture.and_then(|c| c.get("feed"));
        let url = feed
            .and_then(|f| f.get("url"))
            .and_then(Yaml::as_str)
            .or(fallback_source)
            .unwrap_or_default()
            .to_string();
        Self {
            url,
            poll_interval: feed.and_then(|f| f.get("poll_interval")).and_then(Yaml::as_str).map(str::to_string),
            last_checked: feed.and_then(|f| f.get("last_checked")).and_then(Yaml::as_str).map(str::to_string),
            full_text: feed.and_then(|f| f.get("full_text")).and_then(Yaml::as_bool).unwrap_or(false),
            item_retention: feed.and_then(|f| f.get("item_retention")).and_then(Yaml::as_str).map(str::to_string),
            paused: feed.and_then(|f| f.get("paused")).and_then(Yaml::as_bool).unwrap_or(false),
        }
    }

    /// Serialize this block back into a `capture.feed` YAML mapping. Only the
    /// fields that carry information are written (a `None` interval / empty
    /// retention is omitted) so the user-facing note stays terse.
    fn to_yaml(&self) -> Yaml {
        let mut f = serde_yml::Mapping::new();
        f.insert(Yaml::from("url"), Yaml::from(self.url.clone()));
        if let Some(p) = &self.poll_interval {
            f.insert(Yaml::from("poll_interval"), Yaml::from(p.clone()));
        }
        if let Some(lc) = &self.last_checked {
            f.insert(Yaml::from("last_checked"), Yaml::from(lc.clone()));
        }
        f.insert(Yaml::from("full_text"), Yaml::from(self.full_text));
        if let Some(r) = &self.item_retention {
            f.insert(Yaml::from("item_retention"), Yaml::from(r.clone()));
        }
        if self.paused {
            f.insert(Yaml::from("paused"), Yaml::from(true));
        }
        Yaml::Mapping(f)
    }

    /// Whether this feed is due for a poll *now* (`now` is an RFC-3339 UTC
    /// instant in seconds-since-epoch). The decision logic for
    /// `rss-poll-schedule`: a paused feed is never due; a feed with no
    /// `poll_interval` is manual-only and never auto-due; a never-checked feed
    /// (no `last_checked`) is always due; otherwise due once
    /// `last_checked + poll_interval <= now`. A `last_checked` that can't be
    /// parsed is treated as "never checked" (due) rather than silently stuck.
    ///
    /// status: rss-poll-schedule
    pub fn is_due(&self, now_epoch_secs: i64) -> bool {
        if self.paused {
            return false;
        }
        let Some(interval) = self.poll_interval.as_deref().and_then(parse_interval) else {
            return false; // manual-only
        };
        let Some(last) = self.last_checked.as_deref().and_then(parse_rfc3339_secs) else {
            return true; // never checked
        };
        last.saturating_add(interval) <= now_epoch_secs
    }
}

/// Parse a human poll interval (`30m`, `6h`, `2d`, `90s`, or a bare number of
/// seconds) into a count of seconds. Returns `None` for an unparseable string
/// so the caller can fall back (treating it as manual-only). Supports the
/// `s` / `m` / `h` / `d` suffixes the spec's examples use.
///
/// status: rss-poll-schedule
pub fn parse_interval(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some(c) if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: i64 = num.trim().parse().ok()?;
    if n < 0 {
        None
    } else {
        Some(n.saturating_mul(mult))
    }
}

/// Parse an RFC-3339 timestamp into whole seconds since the Unix epoch, for
/// the `is_due` comparison. Best-effort: a malformed stamp is `None`. Uses
/// `chrono` (already in the dependency tree via `feed-rs`) for parsing — the
/// workspace `time` build is formatting-only, so it can't parse.
fn parse_rfc3339_secs(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.timestamp())
}

/// The crawl-job parameter block, parsed from `capture.crawl` in a
/// `mode: crawl` note's frontmatter. Every governed-loop knob lives here so a
/// crawl is a saved, re-runnable, synced note by construction. The body stays
/// user-owned (`fill_body: false`); the run log + page index render in the
/// form, not the body. See `docs/extract.md` `crawl-job-note`.
///
/// status: crawl-job-note
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlParams {
    /// Seed URL(s). For a list crawl this is the whole set; for hub/deep it
    /// is the single entry point (extra seeds still allowed). When empty the
    /// loop falls back to [`Spec::source`].
    pub seeds: Vec<String>,
    /// list / hub / deep — selects the depth + follow defaults
    /// (`crawl-modes`).
    pub mode: CrawlMode,
    /// Maximum link-depth from the seed. `0` follows nothing (list); `1` is
    /// one hop (hub); `N` is a deep crawl. Defaults from `mode` when the
    /// frontmatter omits it.
    pub depth: u32,
    /// gitignore-style glob (or `re:<regex>`) restricting which links the
    /// loop continues *into* — "only follow links matching X"
    /// (`crawl-scope-patterns`). `None` follows everything in scope.
    pub follow_pattern: Option<String>,
    /// gitignore-style glob (or `re:<regex>`) restricting which reached pages
    /// are actually *kept* as sidecars — "only extract pages matching Y".
    /// `None` keeps every reached page.
    pub extract_pattern: Option<String>,
    /// Whether the seed/hub page itself becomes a sidecar. Defaults off for
    /// list/hub (launcher page), on for deep (`crawl-extract-seed-flag`).
    pub extract_seed: bool,
    /// Hard cap on the number of pages captured before the loop stops — the
    /// page-count seatbelt (`crawl-governance`).
    pub max_pages: u32,
    /// Politeness delay between fetches, in milliseconds (`crawl-governance`).
    pub rate_limit_ms: u64,
    /// `latest` / `keep:N` / `forever` — the per-crawl artifact-retention
    /// override stamped onto captured pages (`extract-artifact-retention`).
    pub artifact_retention: Option<String>,
}

/// The three crawl modes — loop parameters, not separate code paths
/// (`crawl-modes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlMode {
    /// Multi-seed, depth 0: extract a known set of URLs, follow nothing.
    List,
    /// Single seed, depth 1: harvest one index/hub page's links.
    Hub,
    /// Depth N + scope patterns: archive a section of a site.
    Deep,
}

impl CrawlMode {
    /// The wire string (`list` / `hub` / `deep`).
    pub const fn as_str(self) -> &'static str {
        match self {
            CrawlMode::List => "list",
            CrawlMode::Hub => "hub",
            CrawlMode::Deep => "deep",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "list" => Some(CrawlMode::List),
            "hub" => Some(CrawlMode::Hub),
            "deep" => Some(CrawlMode::Deep),
            _ => None,
        }
    }

    /// The default link-depth for this mode when the frontmatter omits an
    /// explicit `depth` (`crawl-modes`).
    pub const fn default_depth(self) -> u32 {
        match self {
            CrawlMode::List => 0,
            CrawlMode::Hub => 1,
            CrawlMode::Deep => 3,
        }
    }

    /// Whether the seed page is extracted by default for this mode
    /// (`crawl-extract-seed-flag`): off for list/hub, on for deep.
    pub const fn default_extract_seed(self) -> bool {
        matches!(self, CrawlMode::Deep)
    }
}

/// The default page-count seatbelt when the frontmatter omits `max_pages`.
const DEFAULT_MAX_PAGES: u32 = 500;
/// The default politeness delay between fetches, in milliseconds.
const DEFAULT_RATE_LIMIT_MS: u64 = 500;

impl CrawlParams {
    /// Parse the `capture.crawl` block from a frontmatter mapping, applying
    /// `mode`-driven defaults for any omitted field. Returns sensible
    /// defaults (a single-seed list with no patterns) when the block is
    /// entirely absent.
    fn from_capture(capture: Option<&Yaml>, fallback_source: Option<&str>) -> Self {
        let crawl = capture.and_then(|c| c.get("crawl"));
        let mode = crawl
            .and_then(|c| c.get("mode"))
            .and_then(Yaml::as_str)
            .and_then(CrawlMode::parse)
            .unwrap_or(CrawlMode::List);

        let mut seeds: Vec<String> = crawl
            .and_then(|c| c.get("seeds"))
            .and_then(Yaml::as_sequence)
            .map(|seq| seq.iter().filter_map(Yaml::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if let (true, Some(s)) = (seeds.is_empty(), fallback_source) {
            seeds.push(s.to_string());
        }

        let depth = crawl
            .and_then(|c| c.get("depth"))
            .and_then(Yaml::as_u64)
            .map_or_else(|| mode.default_depth(), |d| d as u32);
        let extract_seed = crawl
            .and_then(|c| c.get("extract_seed"))
            .and_then(Yaml::as_bool)
            .unwrap_or_else(|| mode.default_extract_seed());

        Self {
            seeds,
            mode,
            depth,
            follow_pattern: str_field(crawl, "follow_pattern"),
            extract_pattern: str_field(crawl, "extract_pattern"),
            extract_seed,
            max_pages: crawl
                .and_then(|c| c.get("max_pages"))
                .and_then(Yaml::as_u64)
                .map_or(DEFAULT_MAX_PAGES, |n| n as u32),
            rate_limit_ms: crawl
                .and_then(|c| c.get("rate_limit_ms"))
                .and_then(Yaml::as_u64)
                .unwrap_or(DEFAULT_RATE_LIMIT_MS),
            artifact_retention: str_field(crawl, "artifact_retention"),
        }
    }

    /// A bare multi-seed list crawl with default governance — the no-config
    /// entry point for `crawl-list-from-note` and ad-hoc CLI list crawls.
    pub const fn list(seeds: Vec<String>) -> Self {
        Self {
            seeds,
            mode: CrawlMode::List,
            depth: 0,
            follow_pattern: None,
            extract_pattern: None,
            extract_seed: true,
            max_pages: DEFAULT_MAX_PAGES,
            rate_limit_ms: DEFAULT_RATE_LIMIT_MS,
            artifact_retention: None,
        }
    }

    /// Serialize this block back into a `capture.crawl` YAML mapping.
    fn to_yaml(&self) -> Yaml {
        let mut c = serde_yml::Mapping::new();
        c.insert(Yaml::from("mode"), Yaml::from(self.mode.as_str()));
        let seeds: Vec<Yaml> = self.seeds.iter().map(|s| Yaml::from(s.clone())).collect();
        c.insert(Yaml::from("seeds"), Yaml::Sequence(seeds));
        c.insert(Yaml::from("depth"), Yaml::from(self.depth as u64));
        c.insert(Yaml::from("extract_seed"), Yaml::from(self.extract_seed));
        c.insert(Yaml::from("max_pages"), Yaml::from(self.max_pages as u64));
        c.insert(Yaml::from("rate_limit_ms"), Yaml::from(self.rate_limit_ms));
        if let Some(p) = &self.follow_pattern {
            c.insert(Yaml::from("follow_pattern"), Yaml::from(p.clone()));
        }
        if let Some(p) = &self.extract_pattern {
            c.insert(Yaml::from("extract_pattern"), Yaml::from(p.clone()));
        }
        if let Some(r) = &self.artifact_retention {
            c.insert(Yaml::from("artifact_retention"), Yaml::from(r.clone()));
        }
        Yaml::Mapping(c)
    }
}

/// Read a string field from the `crawl` mapping, `None` when absent/non-string.
fn str_field(crawl: Option<&Yaml>, key: &str) -> Option<String> {
    crawl.and_then(|c| c.get(key)).and_then(Yaml::as_str).map(str::to_string)
}

/// Why a frontmatter mapping is not a capture-spec note.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// No `hiker.kind: capture` discriminator — not a capture note at all.
    #[error("not a capture note (missing `hiker.kind: capture`)")]
    NotCapture,
    /// Discriminator present but `capture.mode` is missing or not one of
    /// `clip | crawl | feed`.
    #[error("capture note has missing or invalid `capture.mode` (want clip|crawl|feed)")]
    BadMode,
}

impl Spec {
    /// Parse a capture-spec from a frontmatter YAML mapping (the value
    /// `core::frontmatter::split` produces). Returns `Err(NotCapture)` when
    /// the note isn't a capture note, so a caller can cheaply discriminate.
    pub fn from_frontmatter(frontmatter: &Yaml) -> Result<Self, ParseError> {
        let hiker = frontmatter.get("hiker");
        let kind = hiker
            .and_then(|h| h.get("kind"))
            .and_then(Yaml::as_str);
        if kind != Some("capture") {
            return Err(ParseError::NotCapture);
        }

        let mode = frontmatter
            .get("capture")
            .and_then(|c| c.get("mode"))
            .and_then(Yaml::as_str)
            .and_then(Mode::parse)
            .ok_or(ParseError::BadMode)?;

        // `source` may live under `capture.source` (the form-authored home)
        // or `hiker.source` (the sidecar provenance home for a local path).
        let source = frontmatter
            .get("capture")
            .and_then(|c| c.get("source"))
            .and_then(Yaml::as_str)
            .or_else(|| hiker.and_then(|h| h.get("source")).and_then(Yaml::as_str))
            .map(str::to_string);

        let fill_body = hiker
            .and_then(|h| h.get("fill_body"))
            .and_then(Yaml::as_bool)
            .unwrap_or(false);

        let extractor = hiker
            .and_then(|h| h.get("extractor"))
            .and_then(Yaml::as_str)
            .map(str::to_string);

        let crawl = (mode == Mode::Crawl)
            .then(|| CrawlParams::from_capture(frontmatter.get("capture"), source.as_deref()));
        let feed = (mode == Mode::Feed)
            .then(|| FeedParams::from_capture(frontmatter.get("capture"), source.as_deref()));

        Ok(Self { kind: Kind::Capture, mode, source, fill_body, extractor, crawl, feed })
    }

    /// Serialize this spec back into a frontmatter YAML mapping with the
    /// `hiker:` and `capture:` blocks populated. Round-trips
    /// [`Spec::from_frontmatter`]. `fill_body` is always written
    /// (it is load-bearing for body ownership); optional fields are omitted
    /// when `None`.
    pub fn to_yaml(&self) -> Yaml {
        let mut hiker = serde_yml::Mapping::new();
        hiker.insert(Yaml::from("kind"), Yaml::from("capture"));
        hiker.insert(Yaml::from("fill_body"), Yaml::from(self.fill_body));
        if let Some(extractor) = &self.extractor {
            hiker.insert(Yaml::from("extractor"), Yaml::from(extractor.clone()));
        }

        let mut capture = serde_yml::Mapping::new();
        capture.insert(Yaml::from("mode"), Yaml::from(self.mode.as_str()));
        if let Some(source) = &self.source {
            capture.insert(Yaml::from("source"), Yaml::from(source.clone()));
        }
        if let Some(crawl) = &self.crawl {
            capture.insert(Yaml::from("crawl"), crawl.to_yaml());
        }
        if let Some(feed) = &self.feed {
            capture.insert(Yaml::from("feed"), feed.to_yaml());
        }

        let mut root = serde_yml::Mapping::new();
        root.insert(Yaml::from("hiker"), Yaml::Mapping(hiker));
        root.insert(Yaml::from("capture"), Yaml::Mapping(capture));
        Yaml::Mapping(root)
    }
}
