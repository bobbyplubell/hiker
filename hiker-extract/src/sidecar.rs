//! The sidecar write path. Takes an [`Extracted`] result + the source
//! identity and writes the `.md` sidecar into the vault with the `hiker:`
//! provenance frontmatter from `design.md` "Source-derived notes". Two
//! destinations:
//!
//! - **Vault-internal non-md source** → `<full-source-filename>.md` beside it
//!   (e.g. `rm0090.pdf` → `rm0090.pdf.md`). Carries `hiker.source`,
//!   `source_sha256`, `source_mtime`, `type`. Re-extraction overwrites the
//!   body in place (behaves as a `fill_body: true` capture whose source is a
//!   local path).
//! - **URL clip** → a visible `mode: clip` capture note in the configured
//!   clip folder (`[extract].clip_folder`, default `clips/`), filename
//!   `<slugified-title>.md` (collision-suffixed `-2`, `-3`, …; falls back to
//!   a slug of the URL path when no title is found).
//!
//! Every written sidecar stamps `hiker.author: imported` and a specific
//! `hiker.provenance` label (`extract-sidecar-provenance`). Writing never
//! touches the source bytes (`extract-preserve-original`). See
//! `docs/extract.md` `extract-sidecar-write`.
//
// status: extract-sidecar-write
// status: extract-sidecar-provenance

use std::path::{Path, PathBuf};

use serde_yml::Value as Yaml;

use crate::contract::{CacheKey, Extracted, SidecarMeta};

/// The producing-extractor identity + provenance labels for one write. Bundled
/// so the sidecar-write entry points stay under the argument-count budget and
/// the caller hands one value through instead of five positional strings.
#[derive(Debug, Clone)]
pub struct Producer<'a> {
    /// Producing extractor's `name()` (part of the cache key).
    pub extractor_name: &'a str,
    /// Producing extractor's `version()` (part of the cache key).
    pub extractor_version: &'a str,
    /// The specific `hiker.provenance` label (`pdf`, `web-scrape`, …).
    pub provenance: &'a str,
}

/// The result of a sidecar write: where it landed plus the cache tag stamped
/// on it, so the caller can record it and re-ingest-skip on the next pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Absolute path of the written `.md` file.
    pub path: PathBuf,
    /// The cache tag (`extract-version-cache-key`) stamped in the
    /// frontmatter, for re-ingest comparison.
    pub cache_tag: String,
}

/// A sidecar write failure.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("write sidecar {0}: {1}")]
    Io(PathBuf, String),
    #[error("serialize frontmatter: {0}")]
    Serialize(String),
    #[error("no writable filename could be derived for the clip")]
    NoFilename,
}

/// Drives sidecar writes for one vault. Holds the vault root so destinations
/// resolve consistently. Pure I/O policy — the caller (app/cli trigger) hands
/// it an [`Extracted`] + the source + the producing extractor identity.
pub struct Writer {
    vault_root: PathBuf,
    /// Configured clip folder (vault-relative), e.g. `clips/`.
    clip_folder: String,
}

impl Writer {
    /// New writer for `vault_root` with the configured `clip_folder`
    /// (`[extract].clip_folder`).
    pub fn new(vault_root: impl Into<PathBuf>, clip_folder: impl Into<String>) -> Self {
        Self { vault_root: vault_root.into(), clip_folder: clip_folder.into() }
    }

    /// Write the sidecar for a **vault-internal non-md file** beside its
    /// source: `<full-source-filename>.md`. `source_abs` is the absolute path
    /// to the original file; `source_bytes` are its bytes (for the source
    /// hash); `source_mtime_iso` is its mtime as ISO-8601. Stamps the full
    /// `hiker:` provenance block. Overwrites in place on re-extraction.
    ///
    /// status: extract-sidecar-write
    pub fn write_file_sidecar(
        &self,
        source_abs: &Path,
        source_bytes: &[u8],
        source_mtime_iso: &str,
        extracted: &Extracted,
        producer: &Producer<'_>,
        source_type: &str,
    ) -> Result<WriteOutcome, WriteError> {
        let mut filename = source_abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        filename.push_str(".md");
        let dest = source_abs.with_file_name(&filename);

        let key = CacheKey::from_bytes(source_bytes, producer.extractor_name, producer.extractor_version);
        let fm = self.file_frontmatter(
            source_abs,
            &key,
            source_mtime_iso,
            source_type,
            producer.provenance,
            extracted.frontmatter.as_ref(),
        );
        let content = assemble(&fm, &extracted.markdown)?;
        atomic_write(&dest, content.as_bytes())?;
        Ok(WriteOutcome { path: dest, cache_tag: key.tag() })
    }

    /// Write a **URL clip** as a visible `mode: clip` capture note in the
    /// clip folder. Filename is the slugified title (collision-suffixed),
    /// falling back to a slug of the URL path. `fill_body: true` so the
    /// article lands in the body. Stamps `web-scrape`-style provenance via
    /// the `provenance` arg. Returns the written path.
    ///
    /// status: extract-sidecar-write
    pub fn write_url_clip(
        &self,
        url: &str,
        extracted: &Extracted,
        producer: &Producer<'_>,
    ) -> Result<WriteOutcome, WriteError> {
        let folder = self.vault_root.join(&self.clip_folder);
        let title = extracted
            .frontmatter
            .as_ref()
            .and_then(|m| m.title.as_deref());
        let stem = clip_stem(title, url);
        let dest = unique_path(&folder, &stem)?;

        let key = CacheKey::from_bytes(
            extracted.markdown.as_bytes(),
            producer.extractor_name,
            producer.extractor_version,
        );
        let fm = clip_frontmatter(url, &key, producer.provenance, extracted.frontmatter.as_ref());
        let content = assemble(&fm, &extracted.markdown)?;
        atomic_write(&dest, content.as_bytes())?;
        Ok(WriteOutcome { path: dest, cache_tag: key.tag() })
    }

    /// Write a clip's self-contained HTML archive into the note's companion
    /// folder (`<note-stem>/original.<ext>` beside the clip note). This is the
    /// offline artifact "view original" opens (`extract-open-original-external`
    /// / `extract-web-archive-singlefile`). Returns the archive path. `clip_path`
    /// is the just-written clip note; `archive` is the captured artifact.
    ///
    /// status: extract-web-archive-singlefile
    pub fn write_clip_archive(
        &self,
        clip_path: &Path,
        archive: &crate::contract::Archive,
    ) -> Result<PathBuf, WriteError> {
        let stem = clip_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or(WriteError::NoFilename)?;
        let companion = clip_path.with_file_name(&stem);
        let dest = companion.join(format!("original.{}", archive.extension));
        atomic_write(&dest, &archive.bytes)?;
        Ok(dest)
    }

    /// Build the `hiker:` provenance frontmatter for a file sidecar.
    fn file_frontmatter(
        &self,
        source_abs: &Path,
        key: &CacheKey,
        source_mtime_iso: &str,
        source_type: &str,
        provenance: &str,
        content_meta: Option<&SidecarMeta>,
    ) -> Yaml {
        let mut hiker = serde_yml::Mapping::new();
        hiker.insert(Yaml::from("source"), Yaml::from(source_abs.to_string_lossy().into_owned()));
        hiker.insert(Yaml::from("source_sha256"), Yaml::from(key.source_hash.clone()));
        hiker.insert(Yaml::from("source_mtime"), Yaml::from(source_mtime_iso.to_string()));
        hiker.insert(Yaml::from("type"), Yaml::from(source_type.to_string()));
        hiker.insert(Yaml::from("storage"), Yaml::from("sidecar"));
        // A file sidecar behaves as a fill_body:true capture over a local
        // path: linked + read-only, re-extraction overwrites in place.
        hiker.insert(Yaml::from("fill_body"), Yaml::from(true));
        hiker.insert(Yaml::from("link_state"), Yaml::from("linked"));
        stamp_provenance(&mut hiker, provenance, key);

        let mut root = serde_yml::Mapping::new();
        merge_content_meta(&mut root, content_meta);
        root.insert(Yaml::from("hiker"), Yaml::Mapping(hiker));
        Yaml::Mapping(root)
    }
}

/// Build the frontmatter for a URL clip capture note: a `mode: clip`
/// capture-spec note with provenance + source_url + captured cache tag.
fn clip_frontmatter(
    url: &str,
    key: &CacheKey,
    provenance: &str,
    content_meta: Option<&SidecarMeta>,
) -> Yaml {
    let mut hiker = serde_yml::Mapping::new();
    hiker.insert(Yaml::from("kind"), Yaml::from("capture"));
    hiker.insert(Yaml::from("fill_body"), Yaml::from(true));
    hiker.insert(Yaml::from("source"), Yaml::from(url.to_string()));
    hiker.insert(Yaml::from("source_url"), Yaml::from(url.to_string()));
    hiker.insert(Yaml::from("captured_at"), Yaml::from(now_iso8601()));
    hiker.insert(Yaml::from("storage"), Yaml::from("capture"));
    hiker.insert(Yaml::from("link_state"), Yaml::from("linked"));
    stamp_provenance(&mut hiker, provenance, key);

    let mut capture = serde_yml::Mapping::new();
    capture.insert(Yaml::from("mode"), Yaml::from("clip"));
    capture.insert(Yaml::from("source"), Yaml::from(url.to_string()));

    let mut root = serde_yml::Mapping::new();
    merge_content_meta(&mut root, content_meta);
    root.insert(Yaml::from("hiker"), Yaml::Mapping(hiker));
    root.insert(Yaml::from("capture"), Yaml::Mapping(capture));
    Yaml::Mapping(root)
}

/// The current time as an ISO-8601 / RFC-3339 UTC string for the clip's
/// `captured_at` stamp (`scrape-cmd`). Falls back to the empty string only if
/// the clock is unreadable, which never happens in practice.
fn now_iso8601() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Stamp the coarse authorship axis (`author: imported`), the specific
/// `provenance` label, and the extract cache tag onto a `hiker:` mapping.
///
/// status: extract-sidecar-provenance
fn stamp_provenance(hiker: &mut serde_yml::Mapping, provenance: &str, key: &CacheKey) {
    hiker.insert(Yaml::from("author"), Yaml::from("imported"));
    hiker.insert(Yaml::from("provenance"), Yaml::from(provenance.to_string()));
    hiker.insert(Yaml::from("extract_key"), Yaml::from(key.tag()));
}

/// Merge extractor-supplied content metadata (title) into the root mapping.
fn merge_content_meta(root: &mut serde_yml::Mapping, content_meta: Option<&SidecarMeta>) {
    if let Some(title) = content_meta.and_then(|m| m.title.as_ref()) {
        root.insert(Yaml::from("title"), Yaml::from(title.clone()));
    }
}

/// The filename stem for a URL clip: the slugified title, or a slug of the
/// URL path when no title is found.
fn clip_stem(title: Option<&str>, url: &str) -> String {
    let from_title = title.map(slugify).filter(|s| !s.is_empty());
    from_title.unwrap_or_else(|| {
        let path_slug = slugify(url_path(url));
        if path_slug.is_empty() { "clip".to_string() } else { path_slug }
    })
}

/// Extract the path portion of a URL for slug fallback (everything after the
/// host, sans scheme/query). Best-effort string handling — no URL crate dep
/// in Phase 2.
fn url_path(url: &str) -> &str {
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_host = no_scheme.split_once('/').map(|(_, rest)| rest).unwrap_or("");
    after_host.split(['?', '#']).next().unwrap_or(after_host)
}

/// Slugify a title/string into a filesystem-safe, lowercase, hyphen-joined
/// stem. ASCII-alphanumerics pass through (lowercased); every other run
/// collapses to a single `-`; leading/trailing `-` are trimmed.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Pick a non-colliding `<stem>.md` path inside `folder`, suffixing `-2`,
/// `-3`, … on collision. Creates `folder` if missing.
fn unique_path(folder: &Path, stem: &str) -> Result<PathBuf, WriteError> {
    if stem.is_empty() {
        return Err(WriteError::NoFilename);
    }
    std::fs::create_dir_all(folder)
        .map_err(|e| WriteError::Io(folder.to_path_buf(), e.to_string()))?;
    let first = folder.join(format!("{stem}.md"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..10_000 {
        let candidate = folder.join(format!("{stem}-{n}.md"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(WriteError::NoFilename)
}

/// Re-assemble a `.md` file from a frontmatter mapping + body. Mirrors
/// `core::frontmatter::assemble` (kept local to honor the crate boundary —
/// `hiker-extract` must not depend on `core`). An empty mapping emits no
/// frontmatter block.
fn assemble(frontmatter: &Yaml, body: &str) -> Result<String, WriteError> {
    let is_empty = match frontmatter {
        Yaml::Mapping(m) => m.is_empty(),
        Yaml::Null => true,
        _ => false,
    };
    if is_empty {
        return Ok(body.to_string());
    }
    let yaml = serde_yml::to_string(frontmatter)
        .map_err(|e| WriteError::Serialize(e.to_string()))?;
    let yaml = yaml.trim_end_matches('\n');
    Ok(format!("---\n{yaml}\n---\n{body}"))
}

/// Atomic write-then-rename so a crash mid-write can't leave a half-file
/// (same posture as `core::config::atomic_write`). Creates parent dirs.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| WriteError::Io(parent.to_path_buf(), e.to_string()))?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| WriteError::Io(tmp.clone(), e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| WriteError::Io(path.to_path_buf(), e.to_string()))?;
    Ok(())
}
