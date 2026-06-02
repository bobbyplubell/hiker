//! The `hiker` CLI. Thin adapters over `hiker-core` / `hiker-extract`: parse
//! args, call the library, print a result. Phase 4 wires the web-scrape verbs
//! (`scrape-cmd`): `hiker scrape <url>` writes a visible clip note, and
//! `hiker refresh` re-fetches every scraped source in the vault.
//
// status: scrape-cmd

use std::path::PathBuf;
use std::process::ExitCode;

use hiker_core::config::Config;
use hiker_core::extract as artifact;
use hiker_core::ops::op_writes;
use hiker_core::oplog::OpLog;
use hiker_core::vault::Vault;
use hiker_extract::capture::{CrawlMode, CrawlParams, FeedParams};
use hiker_extract::crawl::{self, Hooks};
use hiker_extract::feed::{self, HttpFetcher};
use hiker_extract::scrape;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("hiker: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch one CLI invocation. Returns a human-readable error string on
/// failure (printed to stderr by `main`).
fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("scrape") => cmd_scrape(&args[1..]),
        Some("refresh") => cmd_refresh(&args[1..]),
        Some("crawl") => cmd_crawl(&args[1..]),
        Some("feed") => cmd_feed(&args[1..]),
        Some("help" | "--help" | "-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}` (try `hiker help`)")),
    }
}

/// `hiker scrape <url> [--into <folder>] [--vault <path>]` — fetch a URL, write
/// a visible clip note into the clip folder (`--into` overrides
/// `[extract].clip_folder`), and drop the offline HTML archive beside it.
///
/// status: scrape-cmd
fn cmd_scrape(args: &[String]) -> Result<(), String> {
    let mut url: Option<String> = None;
    let mut into: Option<String> = None;
    let mut vault: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--into" => into = Some(next_value(&mut it, "--into")?),
            "--vault" => vault = Some(PathBuf::from(next_value(&mut it, "--vault")?)),
            other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
            other => url = Some(other.to_string()),
        }
    }
    let url = url.ok_or("scrape needs a <url>")?;
    let vault_root = vault_root(vault)?;
    let clip_folder = into.unwrap_or_else(|| clip_folder(&vault_root));

    let outcome = scrape::scrape(&vault_root, &clip_folder, &url).map_err(|e| e.to_string())?;
    println!("clipped {} -> {}", url, outcome.clip_path.display());
    if let Some(archive) = outcome.archive_path {
        println!("archived original -> {}", archive.display());
    }
    Ok(())
}

/// `hiker refresh [--vault <path>]` — re-fetch every scraped clip in the vault's
/// clip folder.
///
/// status: scrape-cmd
fn cmd_refresh(args: &[String]) -> Result<(), String> {
    let mut vault: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--vault" => vault = Some(PathBuf::from(next_value(&mut it, "--vault")?)),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let vault_root = vault_root(vault)?;
    let folder = clip_folder(&vault_root);
    let clips = scrape::refreshable_clips(&vault_root, &folder);
    if clips.is_empty() {
        println!("no scraped clips found in {folder}");
        return Ok(());
    }
    // Open the op-log + vault so a re-fetch lands as an `extractor` op on the
    // EXISTING clip (`extract-version-oplog`) instead of a fresh collision-
    // suffixed clip. Bootstrap first so pre-existing clips have op-log doc_ids.
    let vault = Vault::open(&vault_root).map_err(|e| e.to_string())?;
    let log = OpLog::open(&vault_root).map_err(|e| e.to_string())?;
    op_writes::bootstrap(&vault, &log).map_err(|e| e.to_string())?;
    let vault_default = Config::load(&vault_root)
        .map(|c| c.extract.artifact_retention)
        .unwrap_or_else(|_| "latest".to_string());

    for (clip_path, url) in clips {
        let rel = clip_rel(&vault_root, &clip_path);
        match refresh_one(&vault, &log, &vault_default, &rel, &clip_path, &url) {
            Ok(out) => println!("refreshed {url} -> {rel}: {out:?}"),
            Err(e) => eprintln!("failed {url}: {e}"),
        }
    }
    Ok(())
}

/// Re-fetch one clip and land the result on its existing sidecar via the op-log
/// re-extract path. Routes the new body through `core::ops::op_writes::reextract`
/// (replace-if-linked / skip-if-unlinked, no-op on identical) and, when a new
/// version lands, stores the captured HTML archive under
/// `.hiker/refs/<doc_id>/<op_id>/` and prunes to the resolved retention cascade.
///
/// status: extract-web-versioned
/// status: extract-artifact-retention
fn refresh_one(
    vault: &Vault,
    log: &OpLog,
    vault_default: &str,
    rel: &str,
    clip_path: &std::path::Path,
    url: &str,
) -> Result<op_writes::ReextractOutcome, String> {
    let routed = scrape::re_extract_url(url).map_err(|e| e.to_string())?;
    let outcome = op_writes::reextract(log, vault, rel, &routed.extracted.markdown, &routed.extractor_name)
        .map_err(|e| e.to_string())?;
    if outcome == op_writes::ReextractOutcome::Replaced
        && let Some(archive) = &routed.extracted.archive
    {
        // The artifact is keyed by the op that just produced this version.
        let doc_id = log
            .doc_id_for_path(rel)
            .map_err(|e| e.to_string())?
            .ok_or("clip has no op-log doc")?;
        let op_id = log
            .doc_history(&doc_id, 1)
            .map_err(|e| e.to_string())?
            .first()
            .map(|m| m.op_id.clone())
            .ok_or("no producing op for the new version")?;
        let filename = format!("original.{}", archive.extension);
        artifact::store_artifact(vault.root(), &doc_id, &op_id, &filename, &archive.bytes)
            .map_err(|e| e.to_string())?;
        // Per-source `hiker.artifact_retention` overrides the vault default.
        let per_source = clip_artifact_retention(clip_path);
        let retention =
            artifact::resolve_retention(vault_default, None, per_source.as_deref());
        artifact::prune_refs(vault.root(), &doc_id, retention).map_err(|e| e.to_string())?;
    }
    Ok(outcome)
}

/// Vault-relative, forward-slashed path for a clip under the vault root.
fn clip_rel(vault_root: &std::path::Path, clip_path: &std::path::Path) -> String {
    clip_path
        .strip_prefix(vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| clip_path.to_string_lossy().into_owned())
}

/// Read a clip's per-source `hiker.artifact_retention` frontmatter override, if
/// present — the lowest (most specific) level of the retention cascade. Uses
/// `core::frontmatter` so the cli adds no YAML dependency of its own.
fn clip_artifact_retention(clip_path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(clip_path).ok()?;
    let fm = hiker_core::frontmatter::split(&content).frontmatter?;
    fm.get("hiker")?
        .get("artifact_retention")?
        .as_str()
        .map(str::to_string)
}

/// `hiker crawl <seed> [--mode list|hub|deep] [--depth N] [--follow <glob>]
/// [--extract <glob>] [--max-pages N] [--rate-ms N] [--extract-seed]
/// [--name <job-name>] [--vault <path>]` — run a governed crawl. Writes a
/// `mode: crawl` job note in the vault and captures each page into its
/// companion folder, stamping `hiker.parent`. The CLI driver exercises the
/// whole frontier loop end-to-end without the egui form.
///
/// status: crawl-job-form
fn cmd_crawl(args: &[String]) -> Result<(), String> {
    let opts = CrawlOpts::parse(args)?;
    let vault_root = vault_root(opts.vault.clone())?;

    let mode = opts.mode;
    let params = CrawlParams {
        seeds: opts.seeds.clone(),
        mode,
        depth: opts.depth.unwrap_or_else(|| mode.default_depth()),
        follow_pattern: opts.follow.clone(),
        extract_pattern: opts.extract.clone(),
        extract_seed: opts.extract_seed.unwrap_or_else(|| mode.default_extract_seed()),
        max_pages: opts.max_pages,
        rate_limit_ms: opts.rate_ms,
        artifact_retention: None,
    };

    // Open the op-log + bootstrap BEFORE the crawl, so on a RE-crawl each
    // already-captured page's `accepted` state is its pre-crawl body — a
    // changed page then re-extracts onto its existing sidecar as an `extractor`
    // op (`extract-version-oplog`) instead of a blind overwrite.
    let vault = Vault::open(&vault_root).map_err(|e| e.to_string())?;
    let log = OpLog::open(&vault_root).map_err(|e| e.to_string())?;
    op_writes::bootstrap(&vault, &log).map_err(|e| e.to_string())?;

    // The job note + its ULID; captured pages nest under it via `hiker.parent`.
    let job_ulid = ulid::Ulid::new().to_string();
    let job_note = vault_root.join(format!("{}.md", opts.name));
    crawl::write_job_note(&job_note, &params, &job_ulid).map_err(|e| e.to_string())?;

    let mut on_page = |r: &crawl::PageRecord| match &r.path {
        Some(p) => println!("  captured {} -> {}", r.url, p.display()),
        None => println!("  {} ({})", r.url, r.note),
    };
    let mut hooks = Hooks { cancel: None, on_page: Some(&mut on_page) };

    println!("crawling {} (mode {}, depth {})", opts.seeds.join(", "), mode.as_str(), params.depth);
    let report = crawl::run_default(&params, &job_note, &vault_root, &job_ulid, None, &mut hooks)
        .map_err(|e| e.to_string())?;

    // Re-crawl versioning: route every captured page whose sidecar already had
    // an op-log doc (i.e. a re-extract, not a first capture) through the op-log
    // re-extract path. First-capture pages (no doc yet) report `Skipped` and
    // keep their just-written body; the `URL → sidecar` map stays stable so
    // wikilinks don't break.
    for page in &report.pages {
        let Some(path) = &page.path else { continue };
        let rel = clip_rel(&vault_root, path);
        if let Err(e) = op_writes::reextract(&log, &vault, &rel, &child_body(path), "web") {
            eprintln!("version {rel}: {e}");
        }
    }
    println!(
        "done: {} captured, {} pages touched -> {}",
        report.captured_count(),
        report.pages.len(),
        job_note.display()
    );
    Ok(())
}

/// Parsed `hiker crawl` flags.
struct CrawlOpts {
    seeds: Vec<String>,
    mode: CrawlMode,
    depth: Option<u32>,
    follow: Option<String>,
    extract: Option<String>,
    extract_seed: Option<bool>,
    max_pages: u32,
    rate_ms: u64,
    name: String,
    vault: Option<PathBuf>,
}

impl CrawlOpts {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut seeds = Vec::new();
        let mut mode = CrawlMode::Deep;
        let (mut depth, mut follow, mut extract, mut extract_seed) = (None, None, None, None);
        let (mut max_pages, mut rate_ms) = (500_u32, 500_u64);
        let mut name = "crawl".to_string();
        let mut vault = None;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--mode" => mode = parse_mode(&next_value(&mut it, "--mode")?)?,
                "--depth" => depth = Some(parse_u32(&next_value(&mut it, "--depth")?, "--depth")?),
                "--follow" => follow = Some(next_value(&mut it, "--follow")?),
                "--extract" => extract = Some(next_value(&mut it, "--extract")?),
                "--extract-seed" => extract_seed = Some(true),
                "--max-pages" => max_pages = parse_u32(&next_value(&mut it, "--max-pages")?, "--max-pages")?,
                "--rate-ms" => rate_ms = next_value(&mut it, "--rate-ms")?.parse().map_err(|_| "--rate-ms wants a number".to_string())?,
                "--name" => name = next_value(&mut it, "--name")?,
                "--vault" => vault = Some(PathBuf::from(next_value(&mut it, "--vault")?)),
                other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
                other => seeds.push(other.to_string()),
            }
        }
        if seeds.is_empty() {
            return Err("crawl needs at least one <seed> URL".to_string());
        }
        Ok(Self { seeds, mode, depth, follow, extract, extract_seed, max_pages, rate_ms, name, vault })
    }
}

/// `hiker feed <add|poll>` — the feed-subscription driver. A feed is a living
/// `mode: feed` capture note; `add` creates one and runs a first poll, `poll`
/// re-polls existing feed notes. Exercises the whole feed-poll core
/// (fetch → guid-dedup → write new children → prune to retention) end-to-end
/// without the egui form. The background TIMER that fires polls on a cadence is
/// deferred (the task-queue IO lane isn't built); `poll` is the manual driver.
///
/// status: rss-subscription-lifecycle
fn cmd_feed(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("add") => cmd_feed_add(&args[1..]),
        Some("poll") => cmd_feed_poll(&args[1..]),
        Some(other) => Err(format!("unknown feed subcommand `{other}` (try add|poll)")),
        None => Err("feed needs a subcommand (add|poll)".to_string()),
    }
}

/// `hiker feed add <url> [--every <interval>] [--full-text] [--keep N|forever]
/// [--name <note>] [--vault <path>]` — subscribe to a feed: write a `mode: feed`
/// capture note and run a first poll so its current entries land as children.
///
/// status: rss-subscription-lifecycle
/// status: rss-poll-schedule
fn cmd_feed_add(args: &[String]) -> Result<(), String> {
    let opts = FeedAddOpts::parse(args)?;
    let vault_root = vault_root(opts.vault.clone())?;
    let default_poll = feed_default_poll(&vault_root);

    let mut params = FeedParams::new(&opts.url);
    params.poll_interval = Some(opts.every.unwrap_or(default_poll));
    params.full_text = opts.full_text;
    params.item_retention = opts.retention.clone();

    let feed_ulid = ulid::Ulid::new().to_string();
    let note = vault_root.join(format!("{}.md", opts.name));
    feed::write_feed_note(&note, &params, &feed_ulid).map_err(|e| e.to_string())?;
    println!("subscribed {} -> {}", opts.url, note.display());

    let default_retention = feed_item_retention(&vault_root);
    let fetch = HttpFetcher;
    let report = feed::poll_note(&note, &vault_root, &default_retention, &fetch).map_err(|e| e.to_string())?;
    print_poll_report(&note, &report);
    Ok(())
}

/// `hiker feed poll [--vault <path>] [<note>...]` — re-poll feed notes. With no
/// note arguments, every top-level `*.md` that parses as a `mode: feed` note is
/// polled; named notes are polled directly. Per-feed retention resolves against
/// the vault `[extract].feed_item_retention` default.
///
/// status: rss-subscription-lifecycle
fn cmd_feed_poll(args: &[String]) -> Result<(), String> {
    let mut vault: Option<PathBuf> = None;
    let mut notes: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--vault" => vault = Some(PathBuf::from(next_value(&mut it, "--vault")?)),
            other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
            other => notes.push(other.to_string()),
        }
    }
    let vault_root = vault_root(vault)?;
    let default_retention = feed_item_retention(&vault_root);
    let fetch = HttpFetcher;

    let note_paths: Vec<PathBuf> = if notes.is_empty() {
        discover_feed_notes(&vault_root)
    } else {
        notes.iter().map(|n| resolve_note_path(&vault_root, n)).collect()
    };
    if note_paths.is_empty() {
        println!("no feed notes found in {}", vault_root.display());
        return Ok(());
    }
    // Open the op-log + bootstrap BEFORE polling, so each existing child's
    // `accepted` state is its pre-poll body. A changed entry's re-extract then
    // routes onto its existing child as an `extractor` op (`extract-version-oplog`)
    // rather than a blind overwrite — its prior body stays in op-log history.
    let vault = Vault::open(&vault_root).map_err(|e| e.to_string())?;
    let log = OpLog::open(&vault_root).map_err(|e| e.to_string())?;
    op_writes::bootstrap(&vault, &log).map_err(|e| e.to_string())?;

    for note in &note_paths {
        match feed::poll_note(note, &vault_root, &default_retention, &fetch) {
            Ok(report) => {
                // Convert each in-place child overwrite into an `extractor` op:
                // the file on disk already carries the new body, so reextract
                // diffs it against the prior accepted state and lands one
                // version (no-op if identical, skip if the child was unlinked).
                for child in &report.updated_children {
                    let rel = clip_rel(&vault_root, child);
                    if let Err(e) = op_writes::reextract(&log, &vault, &rel, &child_body(child), "rss") {
                        eprintln!("version {rel}: {e}");
                    }
                }
                print_poll_report(note, &report);
            }
            Err(e) => eprintln!("failed {}: {e}", note.display()),
        }
    }
    Ok(())
}

/// Read the body (everything after the frontmatter fence) of a child note on
/// disk — the new extracted body the feed poll just wrote, handed to
/// `core::ops::reextract` to land as an `extractor` op. Empty string when the
/// file can't be read.
fn child_body(path: &std::path::Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    hiker_core::frontmatter::split(&content).body.to_string()
}

/// Print a one-feed poll summary.
fn print_poll_report(note: &std::path::Path, report: &feed::PollReport) {
    println!(
        "polled {} -> {} new, {} updated, {} pruned, {} unchanged",
        note.display(),
        report.new_children.len(),
        report.updated_children.len(),
        report.pruned_children.len(),
        report.unchanged,
    );
}

/// Find every top-level `*.md` in the vault that parses as a `mode: feed` note.
fn discover_feed_notes(vault_root: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(vault_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if feed::is_feed_note(&path) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Resolve a feed-note argument to a path: an absolute/relative `.md` path, or a
/// bare name joined to the vault root with `.md` appended.
fn resolve_note_path(vault_root: &std::path::Path, name: &str) -> PathBuf {
    let p = std::path::Path::new(name);
    if p.is_absolute() || name.ends_with(".md") {
        vault_root.join(p)
    } else {
        vault_root.join(format!("{name}.md"))
    }
}

/// Parsed `hiker feed add` flags.
struct FeedAddOpts {
    url: String,
    every: Option<String>,
    full_text: bool,
    retention: Option<String>,
    name: String,
    vault: Option<PathBuf>,
}

impl FeedAddOpts {
    fn parse(args: &[String]) -> Result<Self, String> {
        let (mut url, mut every, mut retention, mut name, mut vault) = (None, None, None, None, None);
        let mut full_text = false;
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--every" => every = Some(next_value(&mut it, "--every")?),
                "--full-text" => full_text = true,
                "--keep" => retention = Some(parse_retention(&next_value(&mut it, "--keep")?)),
                "--name" => name = Some(next_value(&mut it, "--name")?),
                "--vault" => vault = Some(PathBuf::from(next_value(&mut it, "--vault")?)),
                other if other.starts_with("--") => return Err(format!("unknown flag `{other}`")),
                other => url = Some(other.to_string()),
            }
        }
        let url = url.ok_or("feed add needs a <url>")?;
        let name = name.unwrap_or_else(|| "feed".to_string());
        Ok(Self { url, every, full_text, retention, name, vault })
    }
}

/// Normalize a `--keep` value into a retention string: a bare number becomes
/// `keep:N`; `forever` and `keep:N` pass through.
fn parse_retention(s: &str) -> String {
    let s = s.trim();
    if s.eq_ignore_ascii_case("forever") || s.starts_with("keep:") {
        s.to_string()
    } else {
        format!("keep:{s}")
    }
}

/// The configured default `poll_interval` (`[extract].feed_default_poll`).
fn feed_default_poll(vault_root: &std::path::Path) -> String {
    Config::load(vault_root)
        .map(|c| c.extract.feed_default_poll)
        .unwrap_or_else(|_| "6h".to_string())
}

/// The configured default feed item-retention (`[extract].feed_item_retention`).
fn feed_item_retention(vault_root: &std::path::Path) -> String {
    Config::load(vault_root)
        .map(|c| c.extract.feed_item_retention)
        .unwrap_or_else(|_| "keep:200".to_string())
}

/// Parse a crawl mode string.
fn parse_mode(s: &str) -> Result<CrawlMode, String> {
    match s {
        "list" => Ok(CrawlMode::List),
        "hub" => Ok(CrawlMode::Hub),
        "deep" => Ok(CrawlMode::Deep),
        other => Err(format!("unknown crawl mode `{other}` (want list|hub|deep)")),
    }
}

/// Parse a non-negative integer flag value.
fn parse_u32(s: &str, flag: &str) -> Result<u32, String> {
    s.parse().map_err(|_| format!("{flag} wants a number"))
}

/// Pull the next argument as a flag value, erroring if it's missing.
fn next_value(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next().cloned().ok_or_else(|| format!("{flag} needs a value"))
}

/// Resolve the vault root: the `--vault` path, else the current directory.
fn vault_root(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    match explicit {
        Some(p) => Ok(p),
        None => std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}")),
    }
}

/// The configured clip folder (`[extract].clip_folder`), defaulting to `clips/`
/// when the config can't be loaded.
fn clip_folder(vault_root: &std::path::Path) -> String {
    Config::load(vault_root)
        .map(|c| c.extract.clip_folder)
        .unwrap_or_else(|_| "clips/".to_string())
}

fn print_usage() {
    println!(
        "hiker — extraction CLI\n\n\
         USAGE:\n\
         \x20 hiker scrape <url> [--into <folder>] [--vault <path>]\n\
         \x20 hiker refresh [--vault <path>]\n\
         \x20 hiker crawl <seed>... [--mode list|hub|deep] [--depth N]\n\
         \x20             [--follow <glob>] [--extract <glob>] [--extract-seed]\n\
         \x20             [--max-pages N] [--rate-ms N] [--name <job>] [--vault <path>]\n\
         \x20 hiker feed add <url> [--every <30m|6h|2d>] [--full-text]\n\
         \x20             [--keep N|forever] [--name <note>] [--vault <path>]\n\
         \x20 hiker feed poll [<note>...] [--vault <path>]\n\n\
         scrape  fetch a URL into a visible clip note (+ offline HTML archive)\n\
         refresh re-fetch every scraped clip in the vault\n\
         crawl   run a governed crawl, capturing pages into a job note's folder\n\
         feed    subscribe to (add) or re-poll an RSS/Atom feed; new entries\n\
         \x20       accrue as child notes, deduped by guid across polls"
    );
}
