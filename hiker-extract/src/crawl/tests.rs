//! Offline tests for the governed crawl frontier loop. A fake [`PageSource`]
//! returns canned [`Extracted`] with controlled `next_urls`, so the whole
//! frontier, all of governance (scope / dedup / depth / page-cap / robots),
//! every mode, the child-parent writes, the `URL → child-path` map, and the
//! wikilink rewrite run fully offline against a temp vault — NO live network.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::capture::{CrawlMode, CrawlParams};
use crate::contract::{Extracted, SidecarMeta};
use crate::ExtractError;

use super::governance::{scope_for, Governor, Verdict};
use super::robots::{Rules, Cache};
use super::scope::Pattern;
use super::wikilink::LinkMap;
use super::{run, Hooks, PageSource};

// --- a fake, offline page source -------------------------------------------

/// A canned page: its markdown body, optional title, and the links it
/// "discovers" (the loop's `next_urls`).
struct Page {
    markdown: String,
    title: Option<String>,
    links: Vec<String>,
}

/// A fake [`PageSource`] driven by a `url → Page` map — no network at all.
struct FakeSource {
    pages: HashMap<String, Page>,
}

impl FakeSource {
    fn new() -> Self {
        Self { pages: HashMap::new() }
    }

    fn add(&mut self, url: &str, markdown: &str, title: Option<&str>, links: &[&str]) -> &mut Self {
        self.pages.insert(
            url.to_string(),
            Page {
                markdown: markdown.to_string(),
                title: title.map(str::to_string),
                links: links.iter().map(|s| (*s).to_string()).collect(),
            },
        );
        self
    }
}

impl PageSource for FakeSource {
    fn fetch(&self, url: &str) -> Result<Option<Extracted>, ExtractError> {
        match self.pages.get(url) {
            Some(p) => Ok(Some(Extracted {
                markdown: p.markdown.clone(),
                frontmatter: Some(SidecarMeta { title: p.title.clone(), source_url: Some(url.to_string()) }),
                archive: None,
                next_urls: p.links.clone(),
            })),
            None => Ok(None),
        }
    }
}

/// No robots.txt anywhere (allow all).
fn no_robots(_: &str) -> Option<String> {
    None
}

/// A temp vault root + a job note path inside it.
fn vault() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let note = root.join("crawl-job.md");
    (dir, root, note)
}

fn crawl(params: &CrawlParams, src: &FakeSource, root: &Path, note: &Path) -> super::Report {
    run(params, note, root, "01JOBULID", src, &no_robots, &mut Hooks::none()).unwrap()
}

// --- frontier loop + modes -------------------------------------------------

#[test]
fn deep_crawl_walks_links_into_companion_folder() {
    let mut src = FakeSource::new();
    src.add("https://site.test/a", "A body [link](https://site.test/b)", Some("Page A"), &["https://site.test/b"])
        .add("https://site.test/b", "B body", Some("Page B"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 2,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://site.test/a".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    assert_eq!(report.captured_count(), 2, "seed + one followed page");
    // children landed in the companion folder beside the job note.
    let companion = root.join("crawl-job");
    assert!(companion.join("page-a.md").exists());
    assert!(companion.join("page-b.md").exists());
}

#[test]
fn hub_mode_depth_one_extracts_links_not_seed() {
    let mut src = FakeSource::new();
    src.add("https://hub.test/index", "hub", Some("Index"), &["https://hub.test/x", "https://hub.test/y"])
        .add("https://hub.test/x", "x body", Some("X"), &[])
        .add("https://hub.test/y", "y body", Some("Y"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Hub,
        depth: 1,
        extract_seed: false,
        ..CrawlParams::list(vec!["https://hub.test/index".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    // seed not kept (extract_seed off), the two linked pages are.
    assert_eq!(report.captured_count(), 2);
    let companion = root.join("crawl-job");
    assert!(!companion.join("index.md").exists());
    assert!(companion.join("x.md").exists());
    assert!(companion.join("y.md").exists());
}

#[test]
fn list_mode_depth_zero_follows_nothing() {
    let mut src = FakeSource::new();
    src.add("https://a.test/1", "one", Some("One"), &["https://a.test/should-not-follow"])
        .add("https://b.test/2", "two", Some("Two"), &[])
        .add("https://a.test/should-not-follow", "nope", Some("Nope"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams::list(vec!["https://a.test/1".into(), "https://b.test/2".into()]);
    let report = crawl(&params, &src, &root, &note);
    assert_eq!(report.captured_count(), 2, "both seeds, no follows at depth 0");
    assert!(report.pages.iter().all(|p| p.url != "https://a.test/should-not-follow"));
}

// --- governance: dedup, depth cap, page cap, scope -------------------------

#[test]
fn visited_set_dedups_diamond() {
    // a -> b, a -> c, b -> d, c -> d : d must be captured once.
    let mut src = FakeSource::new();
    src.add("https://s.test/a", "a", Some("A"), &["https://s.test/b", "https://s.test/c"])
        .add("https://s.test/b", "b", Some("B"), &["https://s.test/d"])
        .add("https://s.test/c", "c", Some("C"), &["https://s.test/d"])
        .add("https://s.test/d", "d", Some("D"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 5,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://s.test/a".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    let d_count = report.captured().filter(|p| p.url == "https://s.test/d").count();
    assert_eq!(d_count, 1, "diamond target captured exactly once");
    assert_eq!(report.captured_count(), 4);
}

#[test]
fn depth_cap_stops_following() {
    let mut src = FakeSource::new();
    src.add("https://d.test/0", "0", Some("L0"), &["https://d.test/1"])
        .add("https://d.test/1", "1", Some("L1"), &["https://d.test/2"])
        .add("https://d.test/2", "2", Some("L2"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 1,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://d.test/0".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    // depth 0 (seed) + depth 1 only; the depth-2 page is never reached.
    assert_eq!(report.captured_count(), 2);
    assert!(report.captured().all(|p| p.url != "https://d.test/2"));
}

#[test]
fn page_cap_halts_crawl() {
    let mut src = FakeSource::new();
    src.add("https://p.test/0", "0", Some("P0"), &["https://p.test/1", "https://p.test/2", "https://p.test/3"])
        .add("https://p.test/1", "1", Some("P1"), &[])
        .add("https://p.test/2", "2", Some("P2"), &[])
        .add("https://p.test/3", "3", Some("P3"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        max_pages: 2,
        ..CrawlParams::list(vec!["https://p.test/0".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    assert_eq!(report.captured_count(), 2, "page cap of 2 enforced");
}

#[test]
fn deep_crawl_stays_same_site() {
    let mut src = FakeSource::new();
    src.add("https://in.test/a", "a", Some("A"), &["https://out.test/x", "https://in.test/b"])
        .add("https://in.test/b", "b", Some("B"), &[])
        .add("https://out.test/x", "x", Some("X"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://in.test/a".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    assert!(report.captured().all(|p| p.url.contains("in.test")), "off-site link not followed");
    assert_eq!(report.captured_count(), 2);
}

// --- scope patterns --------------------------------------------------------

#[test]
fn follow_pattern_restricts_followed_links() {
    let mut src = FakeSource::new();
    src.add("https://s.test/docs/a", "a", Some("A"), &["https://s.test/docs/b", "https://s.test/blog/c"])
        .add("https://s.test/docs/b", "b", Some("B"), &[])
        .add("https://s.test/blog/c", "c", Some("C"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        follow_pattern: Some("s.test/docs/**".into()),
        ..CrawlParams::list(vec!["https://s.test/docs/a".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    assert!(report.captured().all(|p| p.url.contains("/docs/")), "only /docs/ followed");
}

#[test]
fn extract_pattern_keeps_only_matching_pages() {
    let mut src = FakeSource::new();
    src.add("https://s.test/keep/a", "a", Some("A"), &["https://s.test/drop/b"])
        .add("https://s.test/drop/b", "b", Some("B"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        extract_pattern: Some("s.test/keep/**".into()),
        ..CrawlParams::list(vec!["https://s.test/keep/a".into()])
    };
    let report = crawl(&params, &src, &root, &note);
    // b is fetched (for its links) but not kept.
    assert_eq!(report.captured_count(), 1);
    assert!(report.captured().all(|p| p.url.contains("/keep/")));
}

#[test]
fn regex_escape_hatch_matches() {
    let p = Pattern::parse(r"re:https://s\.test/\d+");
    assert!(p.matches("https://s.test/42"));
    assert!(!p.matches("https://s.test/abc"));
}

#[test]
fn glob_pattern_matches_host_path() {
    let p = Pattern::parse("example.com/docs/**");
    assert!(p.matches("https://example.com/docs/a/b"));
    assert!(!p.matches("https://example.com/blog/x"));
}

// --- governor unit verdicts ------------------------------------------------

#[test]
fn governor_verdicts() {
    let scope = scope_for("https://g.test/a", true, None, None);
    let mut gov = Governor::new(scope, 1, 10, std::time::Duration::ZERO);
    gov.mark_seen("https://g.test/a");
    assert_eq!(gov.admit("https://g.test/a", 1), Verdict::Duplicate);
    assert_eq!(gov.admit("https://other.test/x", 1), Verdict::OutOfScope);
    assert_eq!(gov.admit("https://g.test/b", 2), Verdict::TooDeep);
    assert_eq!(gov.admit("https://g.test/b", 1), Verdict::Admitted);
    assert_eq!(gov.admit("https://g.test/b", 1), Verdict::Duplicate);
}

// --- robots ----------------------------------------------------------------

#[test]
fn robots_disallow_blocks_path() {
    let body = "User-agent: *\nDisallow: /private/\nAllow: /private/ok\n";
    let rules = Rules::parse(body, "hiker-extract/0.1");
    assert!(!rules.allows("/private/secret"));
    assert!(rules.allows("/private/ok"));
    assert!(rules.allows("/public/x"));
}

#[test]
fn robots_blocks_crawl_fetch() {
    let mut src = FakeSource::new();
    src.add("https://r.test/a", "a", Some("A"), &["https://r.test/private/b"])
        .add("https://r.test/private/b", "b", Some("B"), &[]);
    let robots = |u: &str| {
        if u.ends_with("/robots.txt") {
            Some("User-agent: *\nDisallow: /private/\n".to_string())
        } else {
            None
        }
    };
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://r.test/a".into()])
    };
    let report = run(&params, &note, &root, "JOB", &src, &robots, &mut Hooks::none()).unwrap();
    // /private/b is admitted to the frontier but robots blocks the fetch.
    assert_eq!(report.captured_count(), 1);
    assert!(report.pages.iter().any(|p| p.url.contains("/private/b") && p.note.contains("robots")));
}

#[test]
fn robots_cache_fetches_once_per_host() {
    use std::cell::Cell;
    let count = Cell::new(0);
    let fetch = |_: &str| {
        count.set(count.get() + 1);
        Some("User-agent: *\nDisallow: /no\n".to_string())
    };
    let mut cache = Cache::new("hiker", &fetch);
    assert!(cache.allows("https://h.test/yes"));
    assert!(!cache.allows("https://h.test/no"));
    assert!(cache.allows("https://h.test/other"));
    assert_eq!(count.get(), 1, "robots.txt fetched once per host");
}

// --- url -> child-path map + wikilink rewrite ------------------------------

#[test]
fn wikilink_rewrite_links_crawled_pages() {
    let entries = vec![
        ("https://s.test/a".to_string(), "job/page-a.md".to_string()),
        ("https://s.test/b".to_string(), "job/page-b.md".to_string()),
    ];
    let map = LinkMap::new(&entries);
    // inline link to a crawled page -> wikilink with label
    let out = map.rewrite("See [Page B](https://s.test/b) and https://external.test/x");
    assert!(out.contains("[[page-b|Page B]]"), "got: {out}");
    // external link untouched
    assert!(out.contains("https://external.test/x"));
}

#[test]
fn wikilink_uses_full_path_when_basename_collides() {
    let entries = vec![
        ("https://s.test/a".to_string(), "job/x/page.md".to_string()),
        ("https://s.test/b".to_string(), "job/y/page.md".to_string()),
    ];
    let map = LinkMap::new(&entries);
    let out = map.rewrite("[go](https://s.test/a)");
    assert!(out.contains("[[job/x/page|go]]"), "ambiguous basename -> full path; got: {out}");
}

#[test]
fn crawl_rewrites_internal_links_in_written_children() {
    let mut src = FakeSource::new();
    src.add("https://w.test/a", "Body links to [B](https://w.test/b).", Some("A"), &["https://w.test/b"])
        .add("https://w.test/b", "B body", Some("B"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 2,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://w.test/a".into()])
    };
    crawl(&params, &src, &root, &note);
    let a = std::fs::read_to_string(root.join("crawl-job/a.md")).unwrap();
    assert!(a.contains("[[b|B]]"), "internal link rewritten to wikilink; got: {a}");
    assert!(a.contains("hiker:"), "parent frontmatter stamped");
    assert!(a.contains("01JOBULID"), "parent stamp present; got: {a}");
}

// --- child-parent stamp ----------------------------------------------------

#[test]
fn children_stamp_parent_ulid() {
    let mut src = FakeSource::new();
    src.add("https://c.test/a", "a", Some("Alpha"), &[]);
    let (_d, root, note) = vault();
    let params = CrawlParams {
        mode: CrawlMode::List,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://c.test/a".into()])
    };
    crawl(&params, &src, &root, &note);
    let body = std::fs::read_to_string(root.join("crawl-job/alpha.md")).unwrap();
    assert!(body.contains("01JOBULID"), "parent stamp; got: {body}");
    assert!(body.contains("provenance: web-crawl"));
    assert!(body.contains("source_url: https://c.test/a"));
}

// --- list-from-note --------------------------------------------------------

#[test]
fn list_from_note_harvests_links() {
    let body = "Some notes.\n\n- [Rust](https://rust-lang.org)\n- bare https://example.com/x\n\nNo dup: https://rust-lang.org";
    let params = super::list_from_note(body).expect("links found");
    assert_eq!(params.mode, CrawlMode::List);
    assert_eq!(params.depth, 0);
    assert_eq!(params.seeds, vec!["https://rust-lang.org".to_string(), "https://example.com/x".to_string()]);
}

#[test]
fn list_from_note_empty_when_no_links() {
    assert!(super::list_from_note("just prose, no urls here").is_none());
}

// --- cancel ----------------------------------------------------------------

#[test]
fn cancel_flag_stops_loop() {
    let mut src = FakeSource::new();
    src.add("https://x.test/a", "a", Some("A"), &["https://x.test/b"])
        .add("https://x.test/b", "b", Some("B"), &[]);
    let (_d, root, note) = vault();
    let cancel = AtomicBool::new(true); // already cancelled
    let mut hooks = Hooks { cancel: Some(&cancel), on_page: None };
    let params = CrawlParams {
        mode: CrawlMode::Deep,
        depth: 3,
        extract_seed: true,
        ..CrawlParams::list(vec!["https://x.test/a".into()])
    };
    let report = run(&params, &note, &root, "JOB", &src, &no_robots, &mut hooks).unwrap();
    assert_eq!(report.captured_count(), 0, "pre-cancelled crawl captures nothing");
}

#[test]
fn progress_hook_fires_per_page() {
    let mut src = FakeSource::new();
    src.add("https://prog.test/a", "a", Some("A"), &["https://prog.test/b"])
        .add("https://prog.test/b", "b", Some("B"), &[]);
    let (_d, root, note) = vault();
    let mut seen = Vec::new();
    {
        let mut on_page = |r: &super::PageRecord| seen.push(r.url.clone());
        let mut hooks = Hooks { cancel: None, on_page: Some(&mut on_page) };
        let params = CrawlParams {
            mode: CrawlMode::Deep,
            depth: 3,
            extract_seed: true,
            ..CrawlParams::list(vec!["https://prog.test/a".into()])
        };
        run(&params, &note, &root, "JOB", &src, &no_robots, &mut hooks).unwrap();
    }
    assert_eq!(seen.len(), 2, "progress fired for both pages");
}

// --- manifest import -------------------------------------------------------

#[test]
fn manifest_import_places_children_and_rewrites() {
    let import = tempfile::tempdir().unwrap();
    let idir = import.path();
    std::fs::write(idir.join("a.md"), "Links to [B](https://m.test/b).").unwrap();
    std::fs::write(idir.join("b.md"), "B body").unwrap();
    std::fs::write(
        idir.join("manifest.json"),
        r#"{"pages":[
            {"url":"https://m.test/a","output_file":"a.md","title":"Page A","links":["https://m.test/b"]},
            {"url":"https://m.test/b","output_file":"b.md","title":"Page B","links":[]}
        ]}"#,
    )
    .unwrap();

    let (_d, root, note) = vault();
    let companion = super::job_companion_dir(&note);
    let report =
        super::manifest::import_dir(idir, &companion, &root, "01MANIFEST").unwrap();
    assert_eq!(report.pages.len(), 2);
    let a = std::fs::read_to_string(root.join("crawl-job/page-a.md")).unwrap();
    assert!(a.contains("[[page-b|B]]"), "manifest rewrite into wikilink; got: {a}");
    assert!(a.contains("01MANIFEST"), "manifest parent stamp; got: {a}");
}
