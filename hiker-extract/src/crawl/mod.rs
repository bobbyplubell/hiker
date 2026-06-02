//! The governed crawl frontier loop. Crawling is not a separate engine — it is
//! the extractor contract (`extract-contract-next-urls`) wrapped in one small
//! governed loop: a queue ("frontier") of URLs is drained by a worker that
//! pops a URL, extracts it, writes its sidecar (+ archive) into the job's
//! companion folder, takes its `next_urls`, admits the survivors, and repeats
//! until the queue empties or a limit trips. The dangerous parts — scope,
//! dedup, depth, page-count, rate, robots — all live in [`governance`], written
//! once, so no extractor can runaway-crawl. List / hub / deep are just loop
//! *parameters*. See `docs/extract.md` "Crawling".
//!
//! The seam that keeps the whole loop offline-testable is the [`PageSource`]
//! trait: the production source ([`RegistryPageSource`]) wraps the built-in
//! [`Registry`] and does real HTTP, but a test injects a fake that returns
//! canned [`Extracted`] with controlled `next_urls`, so the frontier, all of
//! governance, every mode, and the child-parent writes run fully offline
//! against a temp vault.
//
// status: crawl-frontier-loop

pub mod governance;
pub mod manifest;
pub mod robots;
pub mod scope;
mod wikilink;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::capture::{CrawlMode, CrawlParams};
use crate::contract::Extracted;
use crate::{Ctx, ExtractError, Registry, Source};

use crate::companion::{dir_for, write_child, ChildWrite};
use governance::{Governor, Verdict};
use robots::Cache;
use wikilink::LinkMap;

/// The companion folder path for a job note at `note_path`
/// (`<dir>/<name>.md` → `<dir>/<name>/`) — re-exposed for the manifest-import
/// path and external callers that need to address the folder before a crawl
/// has written into it.
pub fn job_companion_dir(note_path: &Path) -> PathBuf {
    dir_for(note_path)
}

/// The user-agent the crawl announces (robots matching + HTTP).
const USER_AGENT: &str = concat!("hiker-extract/", env!("CARGO_PKG_VERSION"));

/// A source of pages for the crawl loop: given a URL, produce its [`Extracted`]
/// content (markdown + archive + `next_urls`) or `Ok(None)` if nothing could be
/// extracted. The production impl ([`RegistryPageSource`]) fetches over the
/// network via the registry; tests inject a fake. This is the injection seam
/// that keeps the loop offline-testable.
pub trait PageSource {
    /// Extract one page.
    fn fetch(&self, url: &str) -> Result<Option<Extracted>, ExtractError>;
}

/// The production page source: runs the built-in [`Registry`] (the web
/// extractor claims the URL and does the real fetch + transform).
pub struct RegistryPageSource {
    registry: Registry,
    ctx: Ctx,
}

impl RegistryPageSource {
    /// New source over the built-in registry. `extractor` pins a specific
    /// extractor when set (`extract-per-source-override`).
    pub fn new(extractor: Option<String>) -> Self {
        Self {
            registry: Registry::with_builtins(),
            ctx: Ctx { pinned_extractor: extractor },
        }
    }
}

impl PageSource for RegistryPageSource {
    fn fetch(&self, url: &str) -> Result<Option<Extracted>, ExtractError> {
        let source = Source::Url(url.to_string());
        Ok(self.registry.extract(&source, &self.ctx)?.map(|r| r.extracted))
    }
}

/// The production crawl entry point: run a crawl over the real built-in
/// registry, fetching `robots.txt` over HTTP. Keeps the whole network surface
/// (page fetch + robots fetch) confined to this crate so the CLI/app stay thin
/// adapters and `core` stays network-free. The `extractor` pin and `hooks`
/// (cancel + progress) are passed through to [`run`].
///
/// status: crawl-frontier-loop
pub fn run_default(
    params: &CrawlParams,
    job_note_path: &Path,
    vault_root: &Path,
    parent_ulid: &str,
    extractor: Option<String>,
    hooks: &mut Hooks<'_>,
) -> Result<Report, Error> {
    let source = RegistryPageSource::new(extractor);
    run(params, job_note_path, vault_root, parent_ulid, &source, &http_robots, hooks)
}

/// Fetch a `robots.txt` URL over HTTP for the governor. Best-effort — any
/// failure is `None` (allow all). A blocking GET with a short timeout keeps a
/// hung host from wedging the crawl. The network seam stays in this crate.
fn http_robots(url: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    client.get(url).send().ok()?.error_for_status().ok()?.text().ok()
}

/// A progress + cancel handle the caller passes in so a crawl is cancellable
/// and reports per-page progress without the loop knowing about the task queue
/// (the loop stays `core`-free). `cancel` is polled before each fetch;
/// `on_page` fires after each captured/skipped page. Both are optional — the
/// CLI driver and tests use [`Hooks::none`]; the task-queue lane (when it
/// lands) passes a real cancel flag + progress sink.
pub struct Hooks<'a> {
    /// Set by the caller (e.g. on a task-queue cancel) to stop the loop at the
    /// next page boundary. `None` never cancels.
    pub cancel: Option<&'a AtomicBool>,
    /// Called after each page is processed with the just-processed record —
    /// drives a live progress widget. `None` reports nothing.
    pub on_page: Option<&'a mut dyn FnMut(&PageRecord)>,
}

impl Hooks<'_> {
    /// A no-cancel, no-progress handle — the CLI/test default.
    pub fn none() -> Self {
        Self { cancel: None, on_page: None }
    }

    /// Whether the caller has signalled cancel.
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// Fire the progress hook for one processed page, if a sink is set.
    fn report_page(&mut self, record: &PageRecord) {
        if let Some(sink) = self.on_page.as_mut() {
            sink(record);
        }
    }
}

/// One page the loop processed: its URL, where it landed (if captured), the
/// depth it was found at, and a short note (captured / skipped reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRecord {
    pub url: String,
    /// The written child note path, or `None` when the page was fetched but
    /// not kept (out of extract scope, empty, or the seed with extract-seed
    /// off).
    pub path: Option<PathBuf>,
    /// Link-depth from the seed (0 = seed).
    pub depth: u32,
    /// A short human-readable status (`captured`, `skipped: out of scope`, …).
    pub note: String,
}

/// The outcome of a crawl run: every page touched, in processing order.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub pages: Vec<PageRecord>,
}

impl Report {
    /// The pages that were actually captured (a sidecar written).
    pub fn captured(&self) -> impl Iterator<Item = &PageRecord> {
        self.pages.iter().filter(|p| p.path.is_some())
    }

    /// Count of captured pages.
    pub fn captured_count(&self) -> usize {
        self.captured().count()
    }
}

/// A crawl failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("crawl has no seed URL")]
    NoSeed,
    #[error("write child: {0}")]
    Write(String),
    #[error("manifest import: {0}")]
    Manifest(String),
}

/// One queued frontier entry: a URL plus the depth it was discovered at.
struct Frontier {
    url: String,
    depth: u32,
}

/// Run a crawl described by `params` for the job note at `job_note_path`
/// (`<dir>/<name>.md`), writing captured pages into its `<name>/` companion
/// folder stamped with `parent_ulid`. `vault_root` anchors the vault-relative
/// child paths used for the wikilink rewrite. Pages come from `source` (the
/// injected seam — production passes [`RegistryPageSource`], tests a fake), and
/// `robots_fetch` fetches `robots.txt` (production: HTTP; tests: a canned map).
///
/// The loop:
/// 1. seeds the frontier (optionally extracting the seed per
///    `crawl-extract-seed-flag`),
/// 2. drains it: fetch → (maybe) write child → admit `next_urls` survivors,
/// 3. runs the wikilink rewrite over every captured page once the map is
///    complete (`crawl-link-rewrite-wikilinks`).
///
/// status: crawl-frontier-loop
/// status: crawl-modes
/// status: crawl-extract-seed-flag
/// status: crawl-child-parent
pub fn run(
    params: &CrawlParams,
    job_note_path: &Path,
    vault_root: &Path,
    parent_ulid: &str,
    source: &dyn PageSource,
    robots_fetch: &dyn Fn(&str) -> Option<String>,
    hooks: &mut Hooks<'_>,
) -> Result<Report, Error> {
    let seeds = effective_seeds(params)?;
    let dir = dir_for(job_note_path);

    // Same-site anchoring: a deep/hub crawl stays on the seed host unless a
    // follow-pattern opts another in; a list crawl is host-unrestricted.
    let same_site = !matches!(params.mode, CrawlMode::List);
    let scope = governance::scope_for(
        &seeds[0],
        same_site,
        params.follow_pattern.as_deref(),
        params.extract_pattern.as_deref(),
    );
    let mut gov = Governor::new(
        scope,
        params.depth,
        params.max_pages,
        Duration::from_millis(params.rate_limit_ms),
    );
    let mut robots = Cache::new(USER_AGENT, robots_fetch);

    let mut frontier: VecDeque<Frontier> = VecDeque::new();
    for seed in &seeds {
        gov.mark_seen(seed);
        frontier.push_back(Frontier { url: seed.clone(), depth: 0 });
    }
    let seed_set: std::collections::HashSet<&String> = seeds.iter().collect();

    let mut report = Report::default();
    // (url, vault_rel_path, raw_markdown) for captured pages, pending rewrite.
    let mut captured: Vec<CapturedPage> = Vec::new();

    while let Some(entry) = frontier.pop_front() {
        if hooks.cancelled() || gov.page_cap_reached() {
            break;
        }
        let is_seed = seed_set.contains(&entry.url);

        // Robots check before every fetch — the governance seatbelt.
        if !robots.allows(&entry.url) {
            push_record(&mut report, hooks, entry.url.clone(), None, entry.depth, "skipped: robots");
            continue;
        }

        std::thread::sleep(gov.rate_limit());
        let extracted = match source.fetch(&entry.url) {
            Ok(Some(e)) => e,
            Ok(None) => {
                push_record(&mut report, hooks, entry.url.clone(), None, entry.depth, "skipped: empty");
                continue;
            }
            Err(e) => {
                push_record(&mut report, hooks, entry.url.clone(), None, entry.depth, &format!("error: {e}"));
                continue;
            }
        };

        // Admit the page's links to the frontier (one hop deeper).
        admit_links(&extracted, entry.depth, &mut gov, &mut frontier);

        // Decide whether to keep (write) this page.
        let keep_seed = !is_seed || params.extract_seed;
        if keep_seed && gov.may_extract(&entry.url) {
            captured.push(CapturedPage {
                url: entry.url.clone(),
                stem: page_stem(&extracted, &entry.url),
                markdown: extracted.markdown.clone(),
                title: extracted.frontmatter.as_ref().and_then(|m| m.title.clone()),
                archive: extracted.archive.as_ref().map(|a| a.bytes.clone()),
                depth: entry.depth,
            });
            gov.record_capture();
        } else {
            let why = if is_seed { "skipped: seed not extracted" } else { "skipped: out of extract scope" };
            push_record(&mut report, hooks, entry.url.clone(), None, entry.depth, why);
        }
    }

    finalize(&captured, &dir, vault_root, parent_ulid, &mut report, hooks)?;
    Ok(report)
}

/// A page held in memory until the full `URL → child-path` map is known, then
/// wikilink-rewritten and written.
struct CapturedPage {
    url: String,
    stem: String,
    markdown: String,
    title: Option<String>,
    archive: Option<Vec<u8>>,
    depth: u32,
}

/// Build the `URL → child-path` map, rewrite each captured page's links into
/// wikilinks, and write the children with the parent stamp. The map is built
/// *before* any write so a link's target path is stable.
fn finalize(
    captured: &[CapturedPage],
    dir: &Path,
    vault_root: &Path,
    parent_ulid: &str,
    report: &mut Report,
    hooks: &mut Hooks<'_>,
) -> Result<(), Error> {
    // Resolve each captured page's eventual filename up-front (collision-aware)
    // so the wikilink map matches what actually lands on disk.
    let mut url_to_rel: Vec<(String, String)> = Vec::new();
    let mut planned: Vec<(usize, String)> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, page) in captured.iter().enumerate() {
        let stem = reserve_stem(&page.stem, &mut used);
        let rel = rel_path(dir, vault_root, &stem);
        url_to_rel.push((page.url.clone(), rel));
        planned.push((i, stem));
    }
    let link_map = LinkMap::new(&url_to_rel);

    for (i, stem) in planned {
        let page = &captured[i];
        let rewritten = link_map.rewrite(&page.markdown);
        let child = ChildWrite {
            companion_dir: dir,
            stem: &stem,
            markdown: &rewritten,
            title: page.title.as_deref(),
            source_url: &page.url,
            parent_ulid,
            provenance: "web-crawl",
            archive: page.archive.as_deref(),
        };
        let path = write_child(&child).map_err(|e| Error::Write(e.to_string()))?;
        push_record(report, hooks, page.url.clone(), Some(path), page.depth, "captured");
    }
    Ok(())
}

/// Admit a page's discovered links (one hop deeper) to the frontier, running
/// each through governance. Out-of-scope / duplicate / too-deep / cap-reached
/// links are silently dropped (their verdict is governance's business, not the
/// report's — only *processed pages* are recorded).
fn admit_links(extracted: &Extracted, depth: u32, gov: &mut Governor, frontier: &mut VecDeque<Frontier>) {
    for link in &extracted.next_urls {
        if gov.admit(link, depth + 1) == Verdict::Admitted {
            frontier.push_back(Frontier { url: link.clone(), depth: depth + 1 });
        }
    }
}

/// Reserve a non-colliding stem within `used`, suffixing `-2`, `-3`, … (mirrors
/// the disk-collision rule in [`write`] so the planned path matches reality).
fn reserve_stem(stem: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = if stem.is_empty() { "page" } else { stem };
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for n in 2..100_000 {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    base.to_string()
}

/// The vault-relative path a child at `<dir>/<stem>.md` will have.
fn rel_path(dir: &Path, vault_root: &Path, stem: &str) -> String {
    let abs = dir.join(format!("{stem}.md"));
    abs.strip_prefix(vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| format!("{stem}.md"))
}

/// Append a record to the report and fire the progress hook.
fn push_record(
    report: &mut Report,
    hooks: &mut Hooks<'_>,
    url: String,
    path: Option<PathBuf>,
    depth: u32,
    note: &str,
) {
    let record = PageRecord { url, path, depth, note: note.to_string() };
    hooks.report_page(&record);
    report.pages.push(record);
}

/// The effective seed list: `params.seeds`, erroring if empty.
fn effective_seeds(params: &CrawlParams) -> Result<Vec<String>, Error> {
    let seeds: Vec<String> = params.seeds.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
    if seeds.is_empty() {
        Err(Error::NoSeed)
    } else {
        Ok(seeds)
    }
}

/// The filename stem for a captured page: a slug of its title, else of the URL
/// path.
fn page_stem(extracted: &Extracted, url: &str) -> String {
    let title = extracted.frontmatter.as_ref().and_then(|m| m.title.as_deref());
    if let Some(t) = title {
        let s = crate::sidecar::slugify(t);
        if !s.is_empty() {
            return s;
        }
    }
    let s = crate::sidecar::slugify(url_path(url));
    if s.is_empty() { "page".to_string() } else { s }
}

/// The path portion of a URL (for slug fallback).
fn url_path(url: &str) -> &str {
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_host = no_scheme.split_once('/').map(|(_, rest)| rest).unwrap_or("");
    after_host.split(['?', '#']).next().unwrap_or(after_host)
}

/// Write the `mode: crawl` capture-spec job note for a crawl: serialize the
/// params into frontmatter (body stays user-owned, `fill_body: false`) and
/// stamp the job's ULID as `hiker.id` so captured children's `hiker.parent`
/// matches. The job note is a normal synced/versioned note, so a crawl is
/// saved + re-runnable by construction (`crawl-job-note`). Returns the written
/// path.
///
/// status: crawl-job-note
pub fn write_job_note(
    path: &Path,
    params: &CrawlParams,
    job_ulid: &str,
) -> Result<(), std::io::Error> {
    use crate::capture::{Kind, Mode, Spec};
    let spec = Spec {
        kind: Kind::Capture,
        mode: Mode::Crawl,
        source: params.seeds.first().cloned(),
        fill_body: false,
        extractor: None,
        crawl: Some(params.clone()),
        feed: None,
    };
    let mut root = match spec.to_yaml() {
        serde_yml::Value::Mapping(m) => m,
        _ => serde_yml::Mapping::new(),
    };
    if let Some(serde_yml::Value::Mapping(hiker)) = root.get_mut("hiker") {
        hiker.insert(serde_yml::Value::from("id"), serde_yml::Value::from(job_ulid.to_string()));
    }
    let yaml = serde_yml::to_string(&serde_yml::Value::Mapping(root))
        .unwrap_or_default();
    let yaml = yaml.trim_end_matches('\n');
    let heading = params.seeds.join(", ");
    let content = format!("---\n{yaml}\n---\n# Crawl: {heading}\n\nNotes about this crawl.\n");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// Harvest every http(s) link in a note's markdown `body` into a depth-0 list
/// crawl — the "extract all links in this note" entry point
/// (`crawl-list-from-note`). Collects both `[text](url)` inline-link targets
/// and bare URLs, de-duplicated in first-seen order. Returns `None` when the
/// body has no links (nothing to crawl).
///
/// status: crawl-list-from-note
pub fn list_from_note(body: &str) -> Option<CrawlParams> {
    let mut seen = std::collections::HashSet::new();
    let mut seeds = Vec::new();
    for url in harvest_links(body) {
        if seen.insert(url.clone()) {
            seeds.push(url);
        }
    }
    if seeds.is_empty() {
        None
    } else {
        Some(CrawlParams::list(seeds))
    }
}

/// Pull every http(s) URL out of a markdown string: inline-link targets first,
/// then bare URLs. Best-effort string scanning (no full markdown parse needed
/// for harvesting).
fn harvest_links(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Inline links `[text](url)`.
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let inline = (bytes[i] == b']' && bytes[i + 1] == b'(')
            .then(|| body[i + 2..].find(')'))
            .flatten();
        if let Some(rel) = inline {
            let url = &body[i + 2..i + 2 + rel];
            if is_http_url(url) {
                out.push(url.to_string());
            }
            i += 2 + rel + 1;
            continue;
        }
        i += 1;
    }
    // Bare URLs anywhere in the text.
    for token in body.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | '`')) {
        let trimmed = token.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if is_http_url(trimmed) && !out.contains(&trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Whether `s` is an http(s) URL.
fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

#[cfg(test)]
mod tests;
