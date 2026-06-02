//! The extractor return contract. An [`Extracted`] is what every extractor
//! produces: the markdown body, optional structured frontmatter the extractor
//! wants stamped, an optional binary archive (Phase 4's self-contained HTML),
//! and the follow-up links it found. An empty `next_urls` is a plain one-page
//! extractor; a non-empty one makes the extractor crawl-capable without any
//! other change (the crawl loop, a later phase, consumes them). See
//! `docs/extract.md` `extract-contract-next-urls`.
//
// status: extract-contract-next-urls

/// What an extractor produces for one source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    /// The extracted markdown — becomes the sidecar/capture-note body.
    pub markdown: String,
    /// Extractor-supplied frontmatter fields to merge onto the sidecar
    /// (e.g. a parsed page title, author, published date). The sidecar write
    /// path adds the `hiker:` provenance/source block on top
    /// (`extract-sidecar-provenance`); this carries the *content* metadata.
    pub frontmatter: Option<SidecarMeta>,
    /// An optional binary artifact captured alongside the text — Phase 4's
    /// self-contained single-file HTML archive
    /// (`extract-web-archive-singlefile`). Phase 2 never sets it. Retained
    /// per the artifact-retention cascade by the caller.
    pub archive: Option<Archive>,
    /// Links the extractor found in the source. Empty for a one-page
    /// extractor; populated to make the extractor crawl-capable. The crawl
    /// frontier loop (later phase) governs which to actually visit; a
    /// non-crawl ingest ignores them.
    pub next_urls: Vec<String>,
}

impl Extracted {
    /// A plain one-page extraction with just a markdown body.
    pub fn from_markdown(markdown: impl Into<String>) -> Self {
        Self { markdown: markdown.into(), ..Self::default() }
    }
}

/// Content-metadata fields an extractor wants stamped onto the sidecar. These
/// are *not* the `hiker:` provenance block (the write path owns that) — they
/// are the human-facing fields parsed out of the source (title, etc.). Kept
/// as an explicit struct rather than a free-form map so the contract is
/// typed; web/feed extractors extend it as their phases land.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidecarMeta {
    /// Page / document title, used both as a frontmatter `title` field and
    /// (for URL clips) as the slug source for the note filename.
    pub title: Option<String>,
    /// Original URL for a URL source (`source_url` frontmatter on clips).
    pub source_url: Option<String>,
}

/// A captured binary artifact (the per-capture HTML archive). Phase 2 defines
/// the shape so the contract is stable; no Phase 2 extractor emits one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// File extension for the artifact, e.g. `"html"`.
    pub extension: String,
    /// The artifact bytes.
    pub bytes: Vec<u8>,
}

/// Identity of one extracted-content result. Keyed on `(source content hash,
/// extractor name, extractor version)` so that a changed source OR a bumped
/// extractor version re-extracts on the next ingest pass — the same mechanism
/// as `embedder-version-tag` in `index.md`. Two extractions produce the same
/// sidecar iff their keys are equal. See `docs/extract.md`
/// `extract-version-cache-key`.
//
// status: extract-version-cache-key
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// blake3 hex of the source's content bytes.
    pub source_hash: String,
    /// The producing extractor's `name()`.
    pub extractor_name: String,
    /// The producing extractor's `version()`. Bumping this changes the key
    /// even on unchanged source bytes, forcing re-extraction.
    pub extractor_version: String,
}

impl CacheKey {
    /// Build a key from the raw source bytes and the producing extractor's
    /// identity. Hashes the bytes with blake3 (the same hash core uses for
    /// `notes.content_hash`).
    pub fn from_bytes(bytes: &[u8], extractor_name: &str, extractor_version: &str) -> Self {
        Self {
            source_hash: blake3::hash(bytes).to_hex().to_string(),
            extractor_name: extractor_name.to_string(),
            extractor_version: extractor_version.to_string(),
        }
    }

    /// Build a key from an already-computed blake3 hex hash (e.g. one the
    /// indexer already has) plus the extractor identity.
    pub fn from_hash(source_hash: impl Into<String>, extractor_name: &str, extractor_version: &str) -> Self {
        Self {
            source_hash: source_hash.into(),
            extractor_name: extractor_name.to_string(),
            extractor_version: extractor_version.to_string(),
        }
    }

    /// A stable, compact string form of the key suitable for stamping on the
    /// sidecar frontmatter (`hiker.extract_key`) and string-comparing on
    /// re-ingest. Shape: `<name>@<version>#<source_hash>`.
    pub fn tag(&self) -> String {
        format!("{}@{}#{}", self.extractor_name, self.extractor_version, self.source_hash)
    }
}
