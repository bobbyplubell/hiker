//! The built-in extractor set. Each extractor is registered with one line in
//! `Registry::with_builtins`; its format-specific logic lives in a sibling
//! module (`pdf`, `command`) while the small `Extractor`-trait struct that the
//! registry holds lives here, so no type repeats its module name. Phase 2
//! shipped the trivial [`PassthroughExtractor`] (plain-text → markdown); Phase
//! 3 adds the [`PdfExtractor`] pure-Rust fast path (registered ahead of
//! passthrough so it wins the `.pdf` match) and the format-agnostic
//! [`CommandExtractor`] escape hatch. Phase 4 adds a web extractor here behind
//! the same trait. See `docs/extract.md` "Extractor registry and contract".
//
// status: extract-registry

mod command;
mod pdf;
pub mod syndication;
mod web;

use crate::contract::{Extracted, SidecarMeta};
use crate::trigger::glob_matches;
use crate::{Ctx, ExtractError, Extractor, Source};

/// Run the same readability + data-blob + htmd transform hiker's web ingest
/// uses, on an already-fetched HTML string (no network). Lets an external
/// renderer (hiker-crawler's CEF engine) produce byte-identical output to
/// ingest. Returns the best-of body as markdown, the parsed title on the
/// [`SidecarMeta`], and any discovered `next_urls`; `archive` is always `None`
/// (the single-file archive needs a subresource fetcher — the crawler builds
/// WARC separately). The public entry to the private [`web`] transform.
//
// status: crawler-preview-fidelity
pub fn extract_from_html(html: &str, base_url: &str) -> Extracted {
    web::extract_from_html(html, base_url)
}

/// The single-page website-to-markdown extractor. Matches `Source::Url`; the
/// fetch + transform pipeline (static fetch → data-blob probe → readability →
/// thin-page fallbacks → single-file HTML archive) lives in the [`web`] module.
/// Registered in `Registry::with_builtins` so a URL source routes here.
///
/// status: extract-web-static-fetch
#[derive(Debug, Default)]
pub struct WebExtractor;

impl Extractor for WebExtractor {
    fn name(&self) -> &str {
        "web-scrape"
    }

    fn version(&self) -> &str {
        // Bump to re-extract every scraped source on the next refresh pass
        // (e.g. after a readability/markdown-serializer fidelity improvement).
        "1"
    }

    fn matches(&self, source: &Source) -> bool {
        source.as_url().is_some()
    }

    fn extract(&self, source: &Source, _ctx: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        let Some(url) = source.as_url() else {
            return Ok(None);
        };
        web::scrape_url(url)
    }
}

/// The RSS / Atom / JSON-Feed extractor. Matches a feed-shaped URL (a path or
/// query that looks like a feed — see [`looks_like_feed`]), fetches it over the
/// same static-fetch path the web extractor uses, and parses it with `feed-rs`
/// ([`feed`]). It emits each entry's link as a `next_url` so a feed participates
/// in the frontier loop exactly like any crawl-capable extractor, and produces a
/// compact markdown index of the entries as its body. Registered ahead of
/// [`WebExtractor`] so a feed URL routes here rather than being read as an
/// HTML page; a non-feed URL fails [`looks_like_feed`] and falls through to the
/// web extractor. The richer per-entry path (guid dedup, child notes) is driven
/// by the feed-poll core (`crate::feed`), which calls [`syndication::parse`]
/// directly.
///
/// status: rss-feed-extractor
#[derive(Debug, Default)]
pub struct FeedExtractor;

impl Extractor for FeedExtractor {
    fn name(&self) -> &str {
        "rss"
    }

    fn version(&self) -> &str {
        // Bump to re-extract every feed this extractor owns on the next pass.
        "1"
    }

    fn matches(&self, source: &Source) -> bool {
        source.as_url().is_some_and(looks_like_feed)
    }

    fn extract(&self, source: &Source, _ctx: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        let Some(url) = source.as_url() else {
            return Ok(None);
        };
        let bytes = web::fetch_bytes(url)?;
        let parsed = syndication::parse(&bytes)?;
        if parsed.entries.is_empty() {
            return Ok(None);
        }
        // The entry links become `next_urls` (parse-never-execute) so the feed
        // is crawl-capable; the body is a compact index of the entries.
        let next_urls = parsed.entries.iter().filter_map(|e| e.link.clone()).collect();
        let mut body = String::new();
        for e in &parsed.entries {
            let title = e.title.as_deref().unwrap_or("(untitled)");
            match &e.link {
                Some(link) => body.push_str(&format!("- [{title}]({link})\n")),
                None => body.push_str(&format!("- {title}\n")),
            }
        }
        Ok(Some(Extracted {
            markdown: body,
            frontmatter: Some(SidecarMeta { title: parsed.title, source_url: Some(url.to_string()) }),
            archive: None,
            next_urls,
        }))
    }
}

/// Whether a URL looks like a feed — the routing heuristic that lets the feed
/// extractor claim feed URLs ahead of the web extractor without a network
/// round-trip. Matches the conventional shapes: a `.xml` / `.rss` / `.atom`
/// path, a path segment named `feed` / `rss` / `atom`, or a `feed` / `format=`
/// query hint. Deliberately conservative — anything not feed-shaped falls
/// through to the web extractor, which will itself fall back to a declared
/// full-text feed for thin pages (`extract-web-fallbacks`).
fn looks_like_feed(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let (path, query) = match lower.split_once('?') {
        Some((p, q)) => (p, q),
        None => (lower.as_str(), ""),
    };
    let path = path.trim_end_matches('/');
    path.ends_with(".rss")
        || path.ends_with(".atom")
        || path.ends_with(".xml")
        || path.ends_with("/feed")
        || path.ends_with("/rss")
        || path.ends_with("/atom")
        || path.contains("/feed/")
        || path.contains("/rss/")
        || query.contains("feed")
        || query.contains("format=rss")
        || query.contains("format=atom")
}

/// A trivial built-in extractor: plain-text files (`.txt`, `.log`, `.csv`, …)
/// pass through as their own markdown body. It exists to exercise the registry
/// + fallback chain + sidecar write end to end. It also demonstrates the
/// decline signal: a file that isn't valid UTF-8 text returns `Ok(None)` so a
/// later extractor in the chain could claim it.
///
/// status: extract-fallback-chain
#[derive(Debug, Default)]
pub struct PassthroughExtractor;

/// The plain-text extensions this extractor claims.
const TEXT_EXTS: &[&str] = &["txt", "text", "log", "csv", "tsv"];

impl Extractor for PassthroughExtractor {
    fn name(&self) -> &str {
        "passthrough"
    }

    fn version(&self) -> &str {
        // Bump to force re-extraction of everything this extractor owns.
        "1"
    }

    fn matches(&self, source: &Source) -> bool {
        source
            .extension()
            .is_some_and(|ext| TEXT_EXTS.contains(&ext.as_str()))
    }

    fn extract(&self, source: &Source, _ctx: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        let Some(path) = source.as_path() else {
            // URL sources aren't ours.
            return Ok(None);
        };
        let bytes = std::fs::read(path).map_err(|e| ExtractError::Io(e.to_string()))?;
        // Decline non-text input via the fallback signal so the registry can
        // try a later extractor instead of hard-failing.
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(None);
        };
        Ok(Some(Extracted::from_markdown(text)))
    }
}

/// Pure-Rust PDF text fast path. Registered ahead of the passthrough extractor
/// so it wins the `.pdf` match; declines (`Ok(None)`) on a scanned/image-only
/// PDF so the fallback chain can take over. Extraction logic + the
/// scanned-detect heuristic live in the [`pdf`] module.
///
/// status: extract-pdf-fast-path
#[derive(Debug, Default)]
pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn name(&self) -> &str {
        "pdf"
    }

    fn version(&self) -> &str {
        // Bump to re-extract every PDF this extractor owns on the next ingest
        // pass (e.g. after a `pdf-extract` upgrade that improves fidelity).
        "1"
    }

    fn matches(&self, source: &Source) -> bool {
        source.extension().as_deref() == Some("pdf")
    }

    fn extract(&self, source: &Source, _ctx: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        let Some(path) = source.as_path() else {
            // URL sources aren't ours (a remote PDF is fetched by a later
            // phase before it reaches a file extractor).
            return Ok(None);
        };
        Ok(pdf::run(path)?.map(Extracted::from_markdown))
    }
}

/// One user-configured command extractor: a path glob it claims plus the
/// command template to run for matching sources. Parsed from a vault
/// `[[extractor.command]]` table; the app/cli layer builds these from config
/// and registers them ahead of the built-in extractors when the user has opted
/// a glob into a command. Run logic lives in the [`command`] module.
///
/// status: extract-pdf-command-escape
#[derive(Debug, Clone)]
pub struct CommandExtractor {
    /// Gitignore-style glob over vault-relative paths that this command claims
    /// (e.g. `**/*.pdf`, `**/*.epub`), matched with [`glob_matches`].
    pub match_glob: String,
    /// The command + argument template. The first element is the program; the
    /// rest are arguments. `{input}` / `{output}` placeholders are substituted
    /// per element (never shell-interpolated).
    pub command: Vec<String>,
    /// A stable extractor name for the cache key / `hiker.extractor` pin.
    /// Defaults to `command:<glob>` when the config omits one.
    pub name: String,
    /// Version string for the cache key; bumping it re-runs the command over
    /// everything it owns. The user sets this when they change the command.
    pub version: String,
}

impl CommandExtractor {
    /// Build a command extractor for `match_glob` running `command`. Derives a
    /// default name (`command:<glob>`) and version (`1`); callers that parse a
    /// richer config can set [`Self::name`] / [`Self::version`] after.
    pub fn new(match_glob: impl Into<String>, command: Vec<String>) -> Self {
        let match_glob = match_glob.into();
        let name = format!("command:{match_glob}");
        Self { match_glob, command, name, version: "1".into() }
    }
}

impl Extractor for CommandExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn matches(&self, source: &Source) -> bool {
        // Match the source path against the configured glob. A `**/*.pdf` glob
        // claims any `.pdf` regardless of where the vault root sits.
        match source.as_path() {
            Some(p) => glob_matches(&self.match_glob, &p.to_string_lossy()),
            None => false,
        }
    }

    fn extract(&self, source: &Source, _ctx: &Ctx) -> Result<Option<Extracted>, ExtractError> {
        let Some(path) = source.as_path() else {
            return Ok(None);
        };
        Ok(command::run(self, path)?.map(Extracted::from_markdown))
    }
}

#[cfg(test)]
mod tests {
    use super::CommandExtractor;
    use crate::contract::Extracted;
    use crate::{Ctx, Extractor, Source};

    #[test]
    fn command_matches_on_glob() {
        let ce = CommandExtractor::new("**/*.pdf", vec!["cat".into(), "{input}".into()]);
        assert!(ce.matches(&Source::File("/vault/docs/a.pdf".into())));
        assert!(!ce.matches(&Source::File("/vault/docs/a.txt".into())));
        assert!(!ce.matches(&Source::Url("https://x".into())));
    }

    #[test]
    fn command_default_name_and_version() {
        let ce = CommandExtractor::new("**/*.epub", vec!["epub2txt".into()]);
        assert_eq!(ce.name(), "command:**/*.epub");
        assert_eq!(ce.version(), "1");
    }

    #[test]
    fn command_captures_stdout_as_body() {
        // `cat {input}` echoes the source bytes to stdout — a stand-in for any
        // user-configured text-producing tool.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("doc.fake");
        std::fs::write(&src, "extracted via command\n").unwrap();
        let ce = CommandExtractor::new("**/*.fake", vec!["cat".into(), "{input}".into()]);
        let out = ce.extract(&Source::File(src), &Ctx::default()).unwrap().unwrap();
        assert_eq!(out, Extracted::from_markdown("extracted via command\n"));
    }

    #[test]
    fn command_reads_output_file_when_templated() {
        // `cp {input} {output}` writes the body to the output file; the
        // extractor reads it back rather than capturing stdout.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("doc.fake");
        std::fs::write(&src, "body through output file").unwrap();
        let ce = CommandExtractor::new(
            "**/*.fake",
            vec!["cp".into(), "{input}".into(), "{output}".into()],
        );
        let out = ce.extract(&Source::File(src), &Ctx::default()).unwrap().unwrap();
        assert_eq!(out.markdown, "body through output file");
    }

    #[test]
    fn command_empty_output_declines() {
        // `true` succeeds and produces nothing → Ok(None) for the fallback chain.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("doc.fake");
        std::fs::write(&src, "ignored").unwrap();
        let ce = CommandExtractor::new("**/*.fake", vec!["true".into()]);
        assert!(ce.extract(&Source::File(src), &Ctx::default()).unwrap().is_none());
    }

    #[test]
    fn command_failing_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("doc.fake");
        std::fs::write(&src, "x").unwrap();
        let ce = CommandExtractor::new("**/*.fake", vec!["false".into()]);
        assert!(ce.extract(&Source::File(src), &Ctx::default()).is_err());
    }
}
