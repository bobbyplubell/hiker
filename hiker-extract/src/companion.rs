//! Writing a captured child note into a capture-spec note's companion folder,
//! with the `hiker.parent` stamp that drives Vault-mode nesting
//! (`crawl-child-parent`). This is shared by both the crawl frontier loop and
//! the feed-poll core: a crawl page and a feed entry are both "a child written
//! into `<note-name>/` beside the parent capture note, stamped with the parent
//! ULID". The companion folder is `<dir>/<name>/` (`note-companion-folder` in
//! `files.md`); the path is computed *locally* rather than pulling a `core`
//! dependency in just for it (both producers only ever *create* children — they
//! never rename, so they don't need core's `move_note` pairing). The stamp, not
//! folder membership, is the nesting authority. See `docs/extract.md`
//! `crawl-child-parent`.
//
// status: crawl-child-parent

use std::path::{Path, PathBuf};

use serde_yml::Value as Yaml;

/// Everything needed to write one captured child page into the companion
/// folder. Bundled so the entry point stays under the argument-count budget.
pub struct ChildWrite<'a> {
    /// The parent note's `<name>/` companion folder (absolute).
    pub companion_dir: &'a Path,
    /// The child's filename stem (slugged title / URL).
    pub stem: &'a str,
    /// The (already wikilink-rewritten) markdown body.
    pub markdown: &'a str,
    /// The page title for the `title` frontmatter field.
    pub title: Option<&'a str>,
    /// The page's source URL (stamped for provenance + re-crawl mapping).
    pub source_url: &'a str,
    /// The parent note's ULID — stamped as `hiker.parent`.
    pub parent_ulid: &'a str,
    /// The `hiker.provenance` label (`web-crawl`, `rss`, …).
    pub provenance: &'a str,
    /// An optional self-contained HTML archive captured with the page.
    pub archive: Option<&'a [u8]>,
}

/// The companion folder path for a note at `note_path`
/// (`<dir>/<name>.md` → `<dir>/<name>/`). Computed locally to avoid a `core`
/// dependency (`note-companion-folder` is the trivial `<dir>/<name>/` rule).
pub fn dir_for(note_path: &Path) -> PathBuf {
    let stem = note_path.file_stem().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    note_path.with_file_name(stem)
}

/// Write one captured child page. Creates `<companion>/<stem>.md`
/// (collision-suffixed `-2`, `-3`, …), stamping the `hiker:` provenance block
/// with `parent`, `source_url`, `provenance`, and `author: imported`. Drops the
/// archive (when present) at `<companion>/<stem>/original.html` so "view
/// original" opens it offline. Returns the written note path.
///
/// status: crawl-child-parent
pub fn write_child(w: &ChildWrite<'_>) -> Result<PathBuf, std::io::Error> {
    std::fs::create_dir_all(w.companion_dir)?;
    let dest = unique_md_path(w.companion_dir, w.stem);
    let fm = child_frontmatter(w);
    let content = assemble(&fm, w.markdown);
    atomic_write(&dest, content.as_bytes())?;

    if let Some(bytes) = w.archive {
        let archive_companion = dest.with_file_name(
            dest.file_stem().map(std::ffi::OsStr::to_os_string).unwrap_or_default(),
        );
        std::fs::create_dir_all(&archive_companion)?;
        atomic_write(&archive_companion.join("original.html"), bytes)?;
    }
    Ok(dest)
}

/// Build the captured-page frontmatter: a `hiker:` block with the parent
/// stamp + provenance + source URL, plus a top-level `title`.
fn child_frontmatter(w: &ChildWrite<'_>) -> Yaml {
    let mut hiker = serde_yml::Mapping::new();
    hiker.insert(Yaml::from("parent"), Yaml::from(w.parent_ulid.to_string()));
    hiker.insert(Yaml::from("source"), Yaml::from(w.source_url.to_string()));
    hiker.insert(Yaml::from("source_url"), Yaml::from(w.source_url.to_string()));
    hiker.insert(Yaml::from("author"), Yaml::from("imported"));
    hiker.insert(Yaml::from("provenance"), Yaml::from(w.provenance.to_string()));
    hiker.insert(Yaml::from("storage"), Yaml::from("capture"));

    let mut root = serde_yml::Mapping::new();
    if let Some(title) = w.title {
        root.insert(Yaml::from("title"), Yaml::from(title.to_string()));
    }
    root.insert(Yaml::from("hiker"), Yaml::Mapping(hiker));
    Yaml::Mapping(root)
}

/// Pick a non-colliding `<stem>.md` inside `dir`, suffixing `-2`, `-3`, … .
fn unique_md_path(dir: &Path, stem: &str) -> PathBuf {
    let stem = if stem.is_empty() { "page" } else { stem };
    let first = dir.join(format!("{stem}.md"));
    if !first.exists() {
        return first;
    }
    for n in 2..100_000 {
        let candidate = dir.join(format!("{stem}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}.md"))
}

/// Assemble a `.md` file from a frontmatter mapping + body (local copy, no
/// `core` dep — mirrors `sidecar::assemble`).
fn assemble(frontmatter: &Yaml, body: &str) -> String {
    let yaml = serde_yml::to_string(frontmatter).unwrap_or_default();
    let yaml = yaml.trim_end_matches('\n');
    format!("---\n{yaml}\n---\n{body}")
}

/// Atomic write-then-rename, creating parent dirs.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
