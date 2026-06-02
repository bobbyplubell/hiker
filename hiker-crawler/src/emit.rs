//! The three emit targets (`crawler-emit-targets`, `crawler-handoff`).
//!
//! One [`Selection`] yields one of three artifacts, each an *existing* hiker
//! seam — the crawler is a generator, not a new ingestion path:
//!
//! 1. a `mode: crawl` capture-spec note (`crawl-job-note`) — [`crawl_params`] +
//!    [`write_crawl_job`], built from shared
//!    [`hiker_extract::capture::CrawlParams`] and written by the canonical
//!    [`hiker_extract::crawl::write_job_note`] (no hand-templated frontmatter);
//! 2. a Lua/wasm source plugin (`plugin-source-extractor`) — [`source_plugin`];
//! 3. a `manifest.json` directory of markdown + WARC (`extract-manifest-import`)
//!    that `hiker_extract::crawl::manifest::import_dir` ingests —
//!    [`write_manifest`], serializing the shared
//!    [`hiker_extract::crawl::manifest::Manifest`] directly.
//!
//! The two code-generating targets run in deterministic or LLM-assisted mode
//! ([`AuthorMode`], `crawler-emit-mode`); deterministic is the offline default.
//! The deterministic body below templates the picked selectors; the LLM path
//! hands the selection to the shared client via [`crate::llm_author`].

use std::path::Path;

use hiker_extract::capture::{CrawlMode, CrawlParams};
use hiker_extract::crawl::manifest::Manifest;
use hiker_extract::crawl::write_job_note;

use crate::picker::{LinkStrategy, Selection};

/// How a code-generating emitter authors its output (`crawler-emit-mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthorMode {
    /// Template the picked selectors straight into the artifact. Offline,
    /// reproducible, always available.
    #[default]
    Deterministic,
    /// Hand the selected DOM + samples to a model to author a robust
    /// extractor. Opt-in; authored via [`crate::llm_author`].
    Llm,
}

/// A sensible page-count seatbelt for crawler-authored jobs (`crawl-governance`).
/// The user edits it in the crawl-job form before running.
const DEFAULT_MAX_PAGES: u32 = 50;
/// A polite default delay between fetches, in milliseconds (`crawl-governance`).
const DEFAULT_RATE_LIMIT_MS: u64 = 500;

/// Build the shared [`CrawlParams`] a [`Selection`] describes
/// (`crawler-emit-crawl-job`). Exposed so the app can preview/inspect the exact
/// parameters before writing the note. The per-job link strategy
/// (`crawler-link-strategy`) drives `mode` + `depth`; these are ordinary
/// `crawl-modes` parameters to hiker's frontier loop.
#[must_use]
pub fn crawl_params(sel: &Selection) -> CrawlParams {
    // StaticList → list (depth 0, follow nothing); PluginDriven → deep (the
    // extractor owns discovery). Dynamic is the listing-aware case: a pick that
    // matched many nodes (`has_repeat`) marks the seed as a hub/listing whose
    // matches seed the crawl, so it maps to `hub` (depth-1 link harvest); a
    // single-value Dynamic pick stays a full `deep` crawl. Depth + extract-seed
    // take the mode defaults (crawler-element-picker, crawler-link-strategy).
    let mode = match sel.link {
        LinkStrategy::StaticList => CrawlMode::List,
        LinkStrategy::PluginDriven => CrawlMode::Deep,
        LinkStrategy::Dynamic if sel.has_repeat() => CrawlMode::Hub,
        LinkStrategy::Dynamic => CrawlMode::Deep,
    };
    CrawlParams {
        seeds: vec![sel.seed_url.clone()],
        mode,
        depth: mode.default_depth(),
        follow_pattern: None,
        extract_pattern: None,
        extract_seed: mode.default_extract_seed(),
        max_pages: DEFAULT_MAX_PAGES,
        rate_limit_ms: DEFAULT_RATE_LIMIT_MS,
        artifact_retention: None,
    }
}

/// Write the crawl-job note for `sel` to `dest`, minting `job_ulid` as the
/// job's `hiker.id`. Renders frontmatter via the canonical shared writer
/// ([`write_job_note`] → `Spec::to_yaml`) so the on-disk shape can't drift from
/// what hiker reads back (`crawler-handoff`).
pub fn write_crawl_job(dest: &Path, sel: &Selection, job_ulid: &str) -> std::io::Result<()> {
    write_job_note(dest, &crawl_params(sel), job_ulid)
}

/// Render a Lua source-plugin skeleton tuned to the site
/// (`crawler-emit-source-plugin`). Deterministic mode templates the selectors
/// into the `extract` entry point; LLM mode (TODO) authors a robust extractor.
/// The result installs through the normal `plugin-install-flow` consent gate.
#[must_use]
pub fn source_plugin(sel: &Selection, mode: AuthorMode) -> String {
    if mode == AuthorMode::Llm {
        return crate::llm_author::author_source_plugin(sel);
    }
    let picks = sel
        .fields
        .iter()
        .map(|f| format!("  fields[\"{}\"] = query(doc, \"{}\")", f.name, f.selector))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "-- Source plugin authored by hiker-crawler for {seed}\n\
         -- Registers as an extractor (plugin-source-extractor); returns\n\
         -- {{ markdown, frontmatter?, next_urls? }} like a built-in extractor.\n\
         function extract(doc)\n  local fields = {{}}\n{picks}\n  \
         return to_markdown(fields)\nend\n",
        seed = sel.seed_url,
    )
}

/// Serialize `manifest` to `<dir>/manifest.json` — the `crawler-direct-warc`
/// handoff shape `hiker_extract::crawl::manifest::import_dir` consumes. The
/// shared [`Manifest`] type is written directly (no mirror), so the on-disk
/// shape stays in lockstep with the importer.
pub fn write_manifest(dir: &Path, manifest: &Manifest) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(dir.join("manifest.json"), json)
}

#[cfg(test)]
mod tests {
    use hiker_extract::capture::CrawlMode;
    use hiker_extract::crawl::manifest::{Manifest, Page};

    use super::{AuthorMode, crawl_params, source_plugin, write_manifest};
    use crate::picker::{Field, LinkStrategy, Selection};

    /// A selection over a listing page; `repeat` adds a list/repeat field so the
    /// `has_repeat`-sensitive mode mapping can be exercised both ways.
    fn selection(link: LinkStrategy, repeat: bool) -> Selection {
        let mut sel = Selection::new("https://example.com/list");
        sel.link = link;
        sel.push(Field {
            name: "title".to_owned(),
            selector: "h1".to_owned(),
            repeat: false,
            sample: "Hi".to_owned(),
        });
        if repeat {
            sel.push(Field {
                name: "links".to_owned(),
                selector: ".card a".to_owned(),
                repeat: true,
                sample: String::new(),
            });
        }
        sel
    }

    #[test]
    fn static_list_maps_to_list_mode_depth_zero() {
        let p = crawl_params(&selection(LinkStrategy::StaticList, false));
        assert_eq!(p.mode, CrawlMode::List);
        assert_eq!(p.depth, 0);
        assert_eq!(p.seeds, vec!["https://example.com/list".to_owned()]);
    }

    #[test]
    fn plugin_driven_maps_to_deep() {
        let p = crawl_params(&selection(LinkStrategy::PluginDriven, true));
        assert_eq!(p.mode, CrawlMode::Deep);
    }

    #[test]
    fn dynamic_is_deep_for_a_single_pick_and_hub_for_a_repeat_pick() {
        let single = crawl_params(&selection(LinkStrategy::Dynamic, false));
        assert_eq!(single.mode, CrawlMode::Deep);
        let listing = crawl_params(&selection(LinkStrategy::Dynamic, true));
        assert_eq!(listing.mode, CrawlMode::Hub);
        assert_eq!(listing.depth, 1);
    }

    #[test]
    fn deterministic_source_plugin_templates_each_picked_selector() {
        let lua = source_plugin(&selection(LinkStrategy::Dynamic, true), AuthorMode::Deterministic);
        assert!(lua.contains("function extract(doc)"));
        assert!(lua.contains(r#"fields["title"] = query(doc, "h1")"#));
        assert!(lua.contains(r#"fields["links"] = query(doc, ".card a")"#));
        assert!(lua.contains("https://example.com/list"));
    }

    #[test]
    fn write_manifest_round_trips_through_the_shared_type() {
        let dir = std::env::temp_dir().join(format!("hiker-crawler-emit-{}", ulid::Ulid::new()));
        let manifest = Manifest {
            pages: vec![Page {
                url: "https://example.com/list".to_owned(),
                output_file: "page-0.md".to_owned(),
                title: Some("Hi".to_owned()),
                archive_file: None,
                links: vec![],
            }],
        };
        write_manifest(&dir, &manifest).expect("write manifest dir");
        let json =
            std::fs::read_to_string(dir.join("manifest.json")).expect("read manifest.json");
        let parsed: Manifest = serde_json::from_str(&json).expect("parse manifest.json");
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.pages[0].output_file, "page-0.md");
        assert_eq!(parsed.pages[0].title.as_deref(), Some("Hi"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
