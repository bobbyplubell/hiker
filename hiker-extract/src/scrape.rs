//! The `hiker scrape <url>` / `hiker refresh` orchestration — the reusable core
//! behind the thin CLI adapters (`scrape-cmd`). One gesture: run the web
//! extractor over a URL, write a visible `mode: clip` capture note into the
//! configured clip folder (with `web-scrape` provenance, `source_url`, and
//! `captured_at` stamped by the sidecar write path), and drop the
//! self-contained HTML archive into the note's companion folder. `refresh`
//! re-scrapes every clip note already in the vault. Lives in `hiker-extract`
//! (not the adapter) so the CLI, and later the GUI quick-capture, share one
//! implementation. See `docs/extract.md` `scrape-cmd` / `capture-quick-from-url`.
//
// status: scrape-cmd

use std::path::{Path, PathBuf};

use crate::sidecar::{Producer, WriteError, Writer};
use crate::{Ctx, ExtractError, Registry, Source};

/// What one scrape produced: the clip note path and the archive path (when an
/// archive was captured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipped {
    /// The visible `mode: clip` capture note.
    pub clip_path: PathBuf,
    /// The self-contained HTML archive in the note's companion folder, if one
    /// was captured.
    pub archive_path: Option<PathBuf>,
}

/// A scrape failure: either extraction (network/parse) or the sidecar write.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("extract {0}: {1}")]
    Extract(String, ExtractError),
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error("no content extracted from {0}")]
    Empty(String),
}

/// Scrape one `url` into a visible clip note under `clip_folder` (vault-relative;
/// the `--into` override or `[extract].clip_folder` default). Runs the built-in
/// registry (the web extractor claims the URL), writes the clip + archive, and
/// returns where they landed. The clip stamps `web-scrape` provenance,
/// `author: imported`, `source_url`, and `captured_at` via the sidecar write
/// path (`scrape-cmd`).
///
/// This is also the core of the "New from URL" quick-capture
/// (`capture-quick-from-url`): it auto-creates a `mode: clip` note with
/// `fill_body: true` (the sidecar write path stamps both) and runs it
/// immediately. The GUI affordance that calls this is deferred; the
/// auto-create + fill_body + run core lands here.
///
/// status: scrape-cmd
/// status: capture-quick-from-url
pub fn scrape(vault_root: &Path, clip_folder: &str, url: &str) -> Result<Clipped, Error> {
    let registry = Registry::with_builtins();
    let source = Source::Url(url.to_string());
    let routed = registry
        .extract(&source, &Ctx::default())
        .map_err(|e| Error::Extract(url.to_string(), e))?
        .ok_or_else(|| Error::Empty(url.to_string()))?;

    let writer = Writer::new(vault_root, clip_folder);
    let producer = Producer {
        extractor_name: &routed.extractor_name,
        extractor_version: &routed.extractor_version,
        provenance: "web-scrape",
    };
    let written = writer.write_url_clip(url, &routed.extracted, &producer)?;

    let archive_path = match &routed.extracted.archive {
        Some(archive) => Some(writer.write_clip_archive(&written.path, archive)?),
        None => None,
    };
    Ok(Clipped { clip_path: written.path, archive_path })
}

/// Re-fetch one `url` and return the routed extract WITHOUT writing a clip —
/// the re-extraction primitive the host drives onto an *existing* clip via the
/// op-log (`extract-web-versioned`). The leaf crate produces the body, archive,
/// and extractor identity; the host (which links `core`) lands the body as an
/// `extractor` op on the existing sidecar (`op-log-reextract-replace`) — so the
/// version history comes from the op-log, not a fresh collision-suffixed clip,
/// and `hiker-extract` stays `core`-free (it never touches the op-log).
///
/// status: extract-web-versioned
pub fn re_extract_url(url: &str) -> Result<crate::RoutedExtract, Error> {
    let registry = Registry::with_builtins();
    let source = Source::Url(url.to_string());
    registry
        .extract(&source, &Ctx::default())
        .map_err(|e| Error::Extract(url.to_string(), e))?
        .ok_or_else(|| Error::Empty(url.to_string()))
}

/// Find every clip note in the vault clip folder paired with its `source_url`
/// — the set of `(clip_path, source_url)` the host re-fetches on `hiker refresh`.
/// The host then drives each through [`re_extract_url`] + the op-log re-extract
/// path; this just enumerates which clips are re-fetchable. Kept here (next to
/// `clip_source_url`) so the walk + the frontmatter read stay together.
///
/// status: extract-web-versioned
pub fn refreshable_clips(vault_root: &Path, clip_folder: &str) -> Vec<(PathBuf, String)> {
    let folder = vault_root.join(clip_folder);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&folder) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(url) = clip_source_url(&path) else { continue };
        out.push((path, url));
    }
    out
}

/// Read a clip note's `hiker.source_url` (the recorded source of a previous
/// scrape) from its frontmatter, or `None` if it isn't a scraped clip.
fn clip_source_url(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let body = content.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    let fm: serde_yml::Value = serde_yml::from_str(&body[..end + 1]).ok()?;
    fm.get("hiker")?
        .get("source_url")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::clip_source_url;

    #[test]
    fn reads_source_url_from_clip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.md");
        std::fs::write(
            &path,
            "---\nhiker:\n  kind: capture\n  source_url: https://example.com/x\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(clip_source_url(&path).as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn non_clip_note_has_no_source_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.md");
        std::fs::write(&path, "---\ntitle: plain\n---\nbody\n").unwrap();
        assert!(clip_source_url(&path).is_none());
    }
}
