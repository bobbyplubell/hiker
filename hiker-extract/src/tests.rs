//! Unit tests for the Phase-2 extractor foundation: registry routing, the
//! `Ok(None)` fallback chain, the per-source override pin, the version-aware
//! cache key, the sidecar write path (file + URL clip + collision), the
//! capture-spec frontmatter model, and the auto-glob matcher.

use std::path::PathBuf;

use crate::builtin::{PassthroughExtractor, PdfExtractor};
use crate::capture::{self, Mode, ParseError, Spec};
use crate::contract::{CacheKey, Extracted, SidecarMeta};
use crate::sidecar::{slugify, Producer, Writer};
use crate::trigger::{self, glob_matches, Decision};
use crate::{Ctx, ExtractError, Extractor, Registry, Source};

// --- test extractors -------------------------------------------------------

/// Always-matches, always-declines (`Ok(None)`) — the fallback-chain probe.
struct AlwaysDeclines;
impl Extractor for AlwaysDeclines {
    fn name(&self) -> &str { "declines" }
    fn version(&self) -> &str { "1" }
    fn matches(&self, _: &Source) -> bool { true }
    fn extract(&self, _: &Source, _: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        Ok(None)
    }
}

/// Always-matches, always-succeeds with a fixed body.
struct AlwaysWins(&'static str);
impl Extractor for AlwaysWins {
    fn name(&self) -> &str { self.0 }
    fn version(&self) -> &str { "1" }
    fn matches(&self, _: &Source) -> bool { true }
    fn extract(&self, _: &Source, _: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        Ok(Some(Extracted::from_markdown("won")))
    }
}

// --- registry routing + fallback ------------------------------------------

#[test]
fn route_picks_first_matching() {
    let mut reg = Registry::empty();
    reg.register(Box::new(AlwaysWins("first")));
    reg.register(Box::new(AlwaysWins("second")));
    let src = Source::Url("https://x".into());
    let routed = reg.route(&src, &Ctx::default()).unwrap();
    assert_eq!(routed.name(), "first");
}

#[test]
fn fallback_chain_skips_decliners() {
    // declines (Ok(None)) → declines → wins.
    let mut reg = Registry::empty();
    reg.register(Box::new(AlwaysDeclines));
    reg.register(Box::new(AlwaysDeclines));
    reg.register(Box::new(AlwaysWins("third")));
    let src = Source::Url("https://x".into());
    let out = reg.extract(&src, &Ctx::default()).unwrap().unwrap();
    assert_eq!(out.extractor_name, "third");
    assert_eq!(out.extracted.markdown, "won");
}

#[test]
fn all_decline_yields_none() {
    let mut reg = Registry::empty();
    reg.register(Box::new(AlwaysDeclines));
    let src = Source::Url("https://x".into());
    assert!(reg.extract(&src, &Ctx::default()).unwrap().is_none());
}

#[test]
fn no_match_yields_none() {
    let reg = Registry::with_builtins();
    // The passthrough extractor only claims text extensions; a .bin doesn't
    // match anything.
    let src = Source::File(PathBuf::from("/tmp/x.bin"));
    assert!(reg.route(&src, &Ctx::default()).is_none());
}

#[test]
fn per_source_override_pins_extractor() {
    // status: extract-per-source-override
    let mut reg = Registry::empty();
    reg.register(Box::new(AlwaysWins("first")));
    reg.register(Box::new(AlwaysWins("second")));
    let src = Source::Url("https://x".into());
    let ctx = Ctx { pinned_extractor: Some("second".into()) };
    let routed = reg.route(&src, &ctx).unwrap();
    assert_eq!(routed.name(), "second", "pin bypasses match order");
}

#[test]
fn per_source_override_rejects_nonmatching_pin() {
    let mut reg = Registry::empty();
    reg.register(Box::new(PassthroughExtractor));
    // Pin to a name that doesn't exist / doesn't match → no candidate.
    let src = Source::File(PathBuf::from("/tmp/a.txt"));
    let ctx = Ctx { pinned_extractor: Some("nonexistent".into()) };
    assert!(reg.route(&src, &ctx).is_none());
}

// --- passthrough extractor -------------------------------------------------

#[test]
fn passthrough_extracts_text_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.txt");
    std::fs::write(&path, "hello\nworld\n").unwrap();
    let reg = Registry::with_builtins();
    let src = Source::File(path);
    let out = reg.extract(&src, &Ctx::default()).unwrap().unwrap();
    assert_eq!(out.extractor_name, "passthrough");
    assert_eq!(out.extracted.markdown, "hello\nworld\n");
}

#[test]
fn passthrough_declines_non_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.txt");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
    let reg = Registry::with_builtins();
    let src = Source::File(path);
    // Declines via Ok(None); with no fallback registered the chain ends None.
    assert!(reg.extract(&src, &Ctx::default()).unwrap().is_none());
}

// --- pdf extractor ---------------------------------------------------------

/// Absolute path to a checked-in PDF fixture.
fn pdf_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

#[test]
fn pdf_matches_pdf_extension_only() {
    // status: extract-pdf-fast-path
    let ext = PdfExtractor;
    assert!(ext.matches(&Source::File(PathBuf::from("/x/a.pdf"))));
    assert!(ext.matches(&Source::File(PathBuf::from("/x/A.PDF"))), "case-insensitive ext");
    assert!(!ext.matches(&Source::File(PathBuf::from("/x/a.txt"))));
    assert!(!ext.matches(&Source::Url("https://x/a.pdf".into())));
}

#[test]
fn pdf_fast_path_extracts_text() {
    // A real text-bearing PDF → the fast path produces a markdown body.
    let reg = Registry::with_builtins();
    let src = Source::File(pdf_fixture("text.pdf"));
    let out = reg.extract(&src, &Ctx::default()).unwrap().unwrap();
    assert_eq!(out.extractor_name, "pdf", "PDF extractor wins the .pdf match");
    assert!(out.extracted.markdown.contains("Hello hiker extraction test document"));
    assert!(out.extracted.markdown.contains("real embedded text layer"));
}

#[test]
fn pdf_scanned_declines_to_fallback() {
    // status: extract-pdf-scanned-detect
    // An image-only (scanned-like) PDF yields no text layer → Ok(None) so the
    // fallback chain can take over. With only built-ins registered (no
    // fallback for PDF), the chain ends in None.
    let reg = Registry::with_builtins();
    let src = Source::File(pdf_fixture("scanned.pdf"));
    assert!(
        reg.extract(&src, &Ctx::default()).unwrap().is_none(),
        "scanned PDF declines and no built-in fallback claims it"
    );
}

#[test]
fn pdf_scanned_falls_through_to_next_extractor() {
    // The scanned PDF's Ok(None) must advance the chain: register a catch-all
    // after the PDF extractor and confirm it claims the declined source.
    let mut reg = Registry::empty();
    reg.register(Box::new(PdfExtractor));
    reg.register(Box::new(AlwaysWins("fallback")));
    let src = Source::File(pdf_fixture("scanned.pdf"));
    let out = reg.extract(&src, &Ctx::default()).unwrap().unwrap();
    assert_eq!(out.extractor_name, "fallback", "scanned PDF fell through to the fallback");
}

#[test]
fn registry_routes_pdf_before_passthrough() {
    // status: extract-registry
    // The PDF extractor must be registered ahead of passthrough so a .pdf
    // routes to it, not the (non-matching) text extractor.
    let reg = Registry::with_builtins();
    let routed = reg.route(&Source::File(PathBuf::from("/x/a.pdf")), &Ctx::default()).unwrap();
    assert_eq!(routed.name(), "pdf");
}

// --- cache key -------------------------------------------------------------

#[test]
fn cache_key_changes_on_version_bump() {
    // status: extract-version-cache-key
    let bytes = b"same source bytes";
    let v1 = CacheKey::from_bytes(bytes, "pdf", "1");
    let v2 = CacheKey::from_bytes(bytes, "pdf", "2");
    assert_ne!(v1, v2, "bumping version must change the key");
    assert_eq!(v1.source_hash, v2.source_hash, "same bytes → same hash");
    assert_ne!(v1.tag(), v2.tag());
}

#[test]
fn cache_key_changes_on_source_change() {
    let a = CacheKey::from_bytes(b"a", "pdf", "1");
    let b = CacheKey::from_bytes(b"b", "pdf", "1");
    assert_ne!(a, b);
}

#[test]
fn cache_key_stable_for_same_inputs() {
    let a = CacheKey::from_bytes(b"x", "web", "3");
    let b = CacheKey::from_bytes(b"x", "web", "3");
    assert_eq!(a, b);
    assert_eq!(a.tag(), "web@3#".to_string() + &a.source_hash);
}

// --- sidecar write: file beside source ------------------------------------

#[test]
fn file_sidecar_written_beside_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("rm0090.pdf");
    std::fs::write(&source, b"%PDF fake").unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let extracted = Extracted::from_markdown("# Extracted\n\ntext\n");
    let producer = Producer { extractor_name: "pdf", extractor_version: "1", provenance: "pdf" };
    let written = writer
        .write_file_sidecar(&source, b"%PDF fake", "2026-05-30T00:00:00Z", &extracted, &producer, "pdf")
        .unwrap();
    // `<full-source-filename>.md` beside the source.
    assert_eq!(written.path, dir.path().join("rm0090.pdf.md"));
    let content = std::fs::read_to_string(&written.path).unwrap();
    assert!(content.starts_with("---\n"));
    assert!(content.contains("author: imported"), "provenance author stamp");
    assert!(content.contains("provenance: pdf"));
    assert!(content.contains("source_sha256:"));
    assert!(content.contains("source_mtime:"));
    assert!(content.contains("2026-05-30T00:00:00Z"));
    assert!(content.contains("type: pdf"));
    assert!(content.contains("storage: sidecar"));
    assert!(content.contains("link_state: linked"));
    assert!(content.ends_with("# Extracted\n\ntext\n"));
    // The original is untouched.
    assert_eq!(std::fs::read(&source).unwrap(), b"%PDF fake");
}

#[test]
fn file_sidecar_overwrites_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a.bin");
    std::fs::write(&source, b"v1").unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let producer = Producer { extractor_name: "x", extractor_version: "1", provenance: "x" };
    let e1 = Extracted::from_markdown("first");
    let w1 = writer
        .write_file_sidecar(&source, b"v1", "t", &e1, &producer, "bin")
        .unwrap();
    let e2 = Extracted::from_markdown("second");
    let w2 = writer
        .write_file_sidecar(&source, b"v2", "t", &e2, &producer, "bin")
        .unwrap();
    assert_eq!(w1.path, w2.path, "same sidecar path");
    let content = std::fs::read_to_string(&w2.path).unwrap();
    assert!(content.ends_with("second"));
    assert_ne!(w1.cache_tag, w2.cache_tag, "changed source → new cache tag");
}

// --- sidecar write: URL clip ----------------------------------------------

#[test]
fn url_clip_written_to_clip_folder_with_title_slug() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let extracted = Extracted {
        markdown: "article body".into(),
        frontmatter: Some(SidecarMeta {
            title: Some("Hello, World! Part 2".into()),
            source_url: Some("https://example.com/a".into()),
        }),
        ..Extracted::default()
    };
    let written = writer
        .write_url_clip("https://example.com/a", &extracted, &web_producer())
        .unwrap();
    assert_eq!(written.path, dir.path().join("clips/hello-world-part-2.md"));
    let content = std::fs::read_to_string(&written.path).unwrap();
    assert!(content.contains("kind: capture"));
    assert!(content.contains("mode: clip"));
    assert!(content.contains("author: imported"));
    assert!(content.contains("provenance: web-scrape"));
    assert!(content.contains("source_url:"));
    assert!(content.contains("https://example.com/a"));
    assert!(content.contains("captured_at:"), "scrape stamps captured_at");
    assert!(content.contains("title:"));
    assert!(content.contains("Hello, World! Part 2"));
    assert!(content.ends_with("article body"));
}

#[test]
fn clip_archive_written_to_companion_folder() {
    // status: extract-web-archive-singlefile
    use crate::contract::Archive;
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let extracted = Extracted {
        markdown: "body".into(),
        frontmatter: Some(SidecarMeta { title: Some("Archived Post".into()), source_url: None }),
        ..Extracted::default()
    };
    let clip = writer.write_url_clip("https://x/p", &extracted, &web_producer()).unwrap();
    let archive = Archive { extension: "html".into(), bytes: b"<html>offline</html>".to_vec() };
    let archive_path = writer.write_clip_archive(&clip.path, &archive).unwrap();
    // `<clip-stem>/original.html` beside the clip note.
    assert_eq!(archive_path, dir.path().join("clips/archived-post/original.html"));
    assert_eq!(std::fs::read(&archive_path).unwrap(), b"<html>offline</html>");
}

#[test]
fn url_clip_collision_suffixes() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let make = || Extracted {
        markdown: "b".into(),
        frontmatter: Some(SidecarMeta { title: Some("Same Title".into()), source_url: None }),
        ..Extracted::default()
    };
    let w1 = writer.write_url_clip("https://x/1", &make(), &web_producer()).unwrap();
    let w2 = writer.write_url_clip("https://x/2", &make(), &web_producer()).unwrap();
    assert_eq!(w1.path, dir.path().join("clips/same-title.md"));
    assert_eq!(w2.path, dir.path().join("clips/same-title-2.md"));
}

#[test]
fn url_clip_falls_back_to_url_path_slug() {
    let dir = tempfile::tempdir().unwrap();
    let writer = Writer::new(dir.path(), "clips/");
    let extracted = Extracted::from_markdown("body"); // no title
    let written = writer
        .write_url_clip("https://example.com/blog/my-post?ref=x", &extracted, &web_producer())
        .unwrap();
    assert_eq!(written.path, dir.path().join("clips/blog-my-post.md"));
}

// --- capture-spec frontmatter model ---------------------------------------

#[test]
fn capture_spec_parses_clip() {
    // status: capture-spec-note
    let yaml = "---\nhiker:\n  kind: capture\n  fill_body: true\ncapture:\n  mode: clip\n  source: https://x\n---\nbody\n";
    let fm = parse_fm(yaml);
    let spec = Spec::from_frontmatter(&fm).unwrap();
    assert_eq!(spec.mode, Mode::Clip);
    assert_eq!(spec.source.as_deref(), Some("https://x"));
    assert!(spec.fill_body);
}

#[test]
fn capture_spec_fill_body_defaults_false() {
    // status: capture-fill-body-toggle
    let yaml = "---\nhiker:\n  kind: capture\ncapture:\n  mode: crawl\n---\nbody\n";
    let fm = parse_fm(yaml);
    let spec = Spec::from_frontmatter(&fm).unwrap();
    assert!(!spec.fill_body, "fill_body defaults false (body user-owned)");
    assert_eq!(spec.mode, Mode::Crawl);
}

#[test]
fn non_capture_note_is_rejected() {
    let yaml = "---\ntitle: just a note\n---\nbody\n";
    let fm = parse_fm(yaml);
    assert_eq!(
        Spec::from_frontmatter(&fm).unwrap_err(),
        ParseError::NotCapture
    );
}

#[test]
fn capture_note_missing_mode_is_bad() {
    let yaml = "---\nhiker:\n  kind: capture\n---\nbody\n";
    let fm = parse_fm(yaml);
    assert_eq!(
        Spec::from_frontmatter(&fm).unwrap_err(),
        ParseError::BadMode
    );
}

#[test]
fn capture_spec_roundtrips() {
    let spec = Spec {
        kind: capture::Kind::Capture,
        mode: Mode::Feed,
        source: Some("https://feed".into()),
        fill_body: false,
        extractor: Some("rss".into()),
        crawl: None,
        feed: Some(capture::FeedParams {
            url: "https://feed".into(),
            poll_interval: Some("6h".into()),
            last_checked: None,
            full_text: false,
            item_retention: Some("keep:50".into()),
            paused: false,
        }),
    };
    let yaml = spec.to_yaml();
    let back = Spec::from_frontmatter(&yaml).unwrap();
    assert_eq!(spec, back);
}

#[test]
fn crawl_spec_roundtrips_params() {
    let crawl = capture::CrawlParams {
        seeds: vec!["https://s.test/seed".into()],
        mode: capture::CrawlMode::Deep,
        depth: 4,
        follow_pattern: Some("s.test/docs/**".into()),
        extract_pattern: None,
        extract_seed: true,
        max_pages: 200,
        rate_limit_ms: 750,
        artifact_retention: Some("keep:3".into()),
    };
    let spec = Spec {
        kind: capture::Kind::Capture,
        mode: Mode::Crawl,
        source: Some("https://s.test/seed".into()),
        fill_body: false,
        extractor: None,
        crawl: Some(crawl),
        feed: None,
    };
    let yaml = spec.to_yaml();
    let back = Spec::from_frontmatter(&yaml).unwrap();
    let back_crawl = back.crawl.expect("crawl params parsed");
    assert_eq!(back_crawl.mode, capture::CrawlMode::Deep);
    assert_eq!(back_crawl.depth, 4);
    assert_eq!(back_crawl.follow_pattern.as_deref(), Some("s.test/docs/**"));
    assert!(back_crawl.extract_seed);
    assert_eq!(back_crawl.max_pages, 200);
    assert_eq!(back_crawl.artifact_retention.as_deref(), Some("keep:3"));
}

// --- slugify ---------------------------------------------------------------

#[test]
fn slugify_basics() {
    assert_eq!(slugify("Hello, World!"), "hello-world");
    assert_eq!(slugify("  leading & trailing  "), "leading-trailing");
    assert_eq!(slugify("already-ok"), "already-ok");
    assert_eq!(slugify("!!!"), "");
}

// --- auto-glob trigger -----------------------------------------------------

#[test]
fn glob_directory_prefix_matches() {
    // status: extract-trigger-auto-glob
    assert!(glob_matches("inbox/", "inbox/a.pdf"));
    assert!(glob_matches("inbox/", "inbox/sub/a.pdf"));
    assert!(!glob_matches("inbox/", "other/a.pdf"));
    // No trailing slash, no wildcard → still a directory prefix.
    assert!(glob_matches("papers", "papers/x.pdf"));
}

#[test]
fn glob_star_within_segment() {
    assert!(glob_matches("docs/*.pdf", "docs/a.pdf"));
    assert!(!glob_matches("docs/*.pdf", "docs/sub/a.pdf"), "* doesn't cross /");
}

#[test]
fn glob_doublestar_crosses_segments() {
    assert!(glob_matches("docs/**/*.pdf", "docs/a.pdf"));
    assert!(glob_matches("docs/**/*.pdf", "docs/sub/deep/a.pdf"));
    assert!(glob_matches("**/*.pdf", "any/where/a.pdf"));
}

#[test]
fn trigger_decision_auto_vs_ignore() {
    let globs = vec!["inbox/".to_string(), "**/*.pdf".to_string()];
    assert_eq!(trigger::decide("inbox/a.bin", &globs), Decision::AutoExtract);
    assert_eq!(trigger::decide("deep/x.pdf", &globs), Decision::AutoExtract);
    assert_eq!(trigger::decide("elsewhere/a.bin", &globs), Decision::Ignore);
    // Empty globs → nothing auto-extracts.
    assert_eq!(trigger::decide("inbox/a.bin", &[]), Decision::Ignore);
}

// --- helpers ---------------------------------------------------------------

/// A stock web-scrape producer for the URL-clip tests.
fn web_producer() -> Producer<'static> {
    Producer { extractor_name: "web-scrape", extractor_version: "1", provenance: "web-scrape" }
}

/// Parse a `---`-delimited frontmatter block into a YAML mapping for the
/// capture-spec tests. A minimal local split (the crate doesn't depend on
/// core's frontmatter splitter).
fn parse_fm(src: &str) -> serde_yml::Value {
    let body = src.strip_prefix("---\n").unwrap();
    let end = body.find("\n---").unwrap();
    serde_yml::from_str(&body[..end + 1]).unwrap()
}
