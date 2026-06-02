//! Extraction subsystem: turn a non-markdown source (a file on disk, a URL)
//! into a searchable hiker note by writing an extracted-text `.md` sidecar
//! into the vault. This is a decoupled leaf crate — `hiker-core` does not
//! depend on it. The sidecar `.md` written to disk is the only seam: core's
//! watcher/indexer ingests it through the ordinary markdown path with no
//! knowledge that extraction exists. The `app`/`cli` layers wire this crate;
//! nothing about extraction is reachable from `core`. See `docs/extract.md`.
//!
//! Phase 2 (this module) is the foundation: the [`Extractor`] trait, the
//! ordered [`Registry`] that routes a [`Source`] to the first matching
//! extractor, the [`Extracted`] contract (with `next_urls` for crawl
//! capability), the fallback chain (`Ok(None)` skips to the next match), the
//! version-aware [`contract::CacheKey`], the capture-spec-note frontmatter model, and
//! the sidecar write path with provenance stamping. The real PDF (Phase 3)
//! and web (Phase 4) extractors land later behind this same registry.
//
// status: extract-crate-decoupled
// status: extract-registry

pub mod builtin;
pub mod capture;
pub mod companion;
pub mod contract;
pub mod crawl;
pub mod feed;
pub mod scrape;
pub mod sidecar;
pub mod trigger;

use contract::Extracted;

use std::path::{Path, PathBuf};

/// A thing to extract: either a file already inside (or reachable on) the
/// filesystem, or a URL to fetch. Extractors route on this via
/// [`Extractor::matches`]; the registry hands the same value to
/// [`Extractor::extract`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file on disk. `path` is the absolute path to the original source
    /// bytes; hiker never modifies, moves, or deletes it
    /// (`extract-preserve-original`).
    File(PathBuf),
    /// A URL to fetch (web clip, feed, crawl seed).
    Url(String),
}

impl Source {
    /// The lowercased file extension (without the dot) for a `File` source,
    /// or `None` for a URL or an extension-less file. Extractors route on
    /// this in `matches`.
    pub fn extension(&self) -> Option<String> {
        match self {
            Source::File(p) => p
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase),
            Source::Url(_) => None,
        }
    }

    /// The filesystem path for a `File` source, else `None`.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Source::File(p) => Some(p.as_path()),
            Source::Url(_) => None,
        }
    }

    /// The URL string for a `Url` source, else `None`.
    pub const fn as_url(&self) -> Option<&str> {
        match self {
            Source::Url(u) => Some(u.as_str()),
            Source::File(_) => None,
        }
    }
}

/// Side-channel context handed to an extractor on `extract`. Kept minimal in
/// Phase 2; the web/crawl phases extend it (HTTP client, crawl-scope params,
/// rate-limiter) without changing the trait signature — extractors read the
/// fields they need and ignore the rest.
#[derive(Debug, Default, Clone)]
pub struct Ctx {
    /// A per-source override read from sidecar frontmatter
    /// (`hiker.extractor: <name>`). When set, the registry routes straight
    /// to the named extractor and bypasses match order
    /// (`extract-per-source-override`). Carried on the context so an
    /// extractor can also see it, but the routing decision lives in
    /// [`Registry::route`].
    pub pinned_extractor: Option<String>,
}

/// A source→markdown extractor. Built-in and trait-based; there is no runtime
/// plugin loading on this path (the binary formats need native libraries and
/// form a small finite set). Adding a new type is one module under
/// [`builtin`] plus one registration line in [`Registry::with_builtins`].
///
/// status: extract-registry
pub trait Extractor: Send + Sync {
    /// Stable short identifier, e.g. `"pdf"`, `"web-scrape"`, `"passthrough"`.
    /// Participates in the cache key and is what `hiker.extractor` pins to.
    fn name(&self) -> &str;

    /// Version string; bumping it re-extracts everything this extractor owns
    /// on the next ingest pass (mirrors `embedder-version-tag`). Part of the
    /// cache key (`extract-version-cache-key`).
    fn version(&self) -> &str;

    /// Cheap routing predicate over the source identity (extension / MIME /
    /// URL pattern). May return true for several extractors — the registry
    /// tries them in order and the fallback chain (`Ok(None)`) skips the
    /// ones that turn out not to handle the input.
    fn matches(&self, source: &Source) -> bool;

    /// Produce the extracted markdown (+ optional archive + follow-up links),
    /// or `Ok(None)` if this extractor matched but can't actually handle the
    /// input (the registry then tries the next match —
    /// `extract-fallback-chain`). `Err` is a hard failure (I/O, malformed
    /// source the extractor was sure it owned).
    fn extract(&self, source: &Source, ctx: &Ctx) -> Result<Option<Extracted>, ExtractError>;
}

/// Hard extraction failure. A *soft* "I don't handle this, try the next"
/// outcome is `Ok(None)` from [`Extractor::extract`], not an error.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("read source: {0}")]
    Io(String),
    #[error("source is not valid UTF-8 text")]
    NotUtf8,
    #[error("extractor `{0}`: {1}")]
    Extractor(String, String),
}

/// The ordered built-in extractor set. Routes a [`Source`] to the first
/// matching extractor, honoring a per-source `hiker.extractor` pin and the
/// `Ok(None)` fallback chain.
///
/// status: extract-registry
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl Registry {
    /// An empty registry. Mostly for tests; production callers use
    /// [`Registry::with_builtins`].
    pub fn empty() -> Self {
        Self { extractors: Vec::new() }
    }

    /// The production registry with every built-in extractor registered in
    /// priority order. The PDF fast path is registered ahead of the passthrough
    /// text extractor so it wins the `.pdf` match (and declines via `Ok(None)`
    /// on scanned PDFs, letting the chain continue). Phase 4 adds the web
    /// extractor for URL sources. User-wired
    /// [`builtin::CommandExtractor`]s are prepended by the app/cli layer
    /// (ahead of these built-ins) for the globs the user opted in. Adding a
    /// built-in type is one line here.
    pub fn with_builtins() -> Self {
        let mut reg = Self::empty();
        // status: extract-registry
        // The feed extractor claims feed-shaped `Source::Url`s; it is
        // registered ahead of the web extractor so a feed URL routes to the
        // `feed-rs` parser rather than being read as an HTML page. A non-feed
        // URL fails its match heuristic and falls through to the web extractor.
        reg.register(Box::new(builtin::FeedExtractor));
        // The web extractor claims `Source::Url`; the file extractors claim
        // extensions. Their match domains don't overlap, so order between them
        // is immaterial — the web extractor leads only for readability.
        reg.register(Box::new(builtin::WebExtractor));
        reg.register(Box::new(builtin::PdfExtractor));
        reg.register(Box::new(builtin::PassthroughExtractor));
        reg
    }

    /// Append an extractor to the end of the match order.
    pub fn register(&mut self, extractor: Box<dyn Extractor>) {
        self.extractors.push(extractor);
    }

    /// The extractors that `matches()` this source, in registration order. A
    /// `ctx.pinned_extractor` restricts the candidate list to that single
    /// named extractor (and only if it also matches), implementing the
    /// per-source override (`extract-per-source-override`).
    fn candidates<'a>(&'a self, source: &Source, ctx: &Ctx) -> Vec<&'a dyn Extractor> {
        self.extractors
            .iter()
            .map(std::convert::AsRef::as_ref)
            .filter(|e| match ctx.pinned_extractor.as_deref() {
                Some(pinned) => e.name() == pinned && e.matches(source),
                None => e.matches(source),
            })
            .collect()
    }

    /// Find the extractor that would be tried *first* for this source
    /// (before the fallback chain runs). Useful for cache-key derivation
    /// without running extraction. Honors the pin.
    pub fn route(&self, source: &Source, ctx: &Ctx) -> Option<&dyn Extractor> {
        self.candidates(source, ctx).into_iter().next()
    }

    /// Run extraction: try each matching extractor in order; the first that
    /// returns `Ok(Some(_))` wins. An `Ok(None)` advances to the next match
    /// (`extract-fallback-chain`). Returns `Ok(None)` if nothing matched or
    /// every match declined. An extractor `Err` aborts the chain — a hard
    /// failure is not a "try the next" signal.
    ///
    /// status: extract-fallback-chain
    pub fn extract(&self, source: &Source, ctx: &Ctx) -> Result<Option<RoutedExtract>, ExtractError> {
        for extractor in self.candidates(source, ctx) {
            match extractor.extract(source, ctx)? {
                Some(extracted) => {
                    return Ok(Some(RoutedExtract {
                        extractor_name: extractor.name().to_string(),
                        extractor_version: extractor.version().to_string(),
                        extracted,
                    }));
                }
                None => continue,
            }
        }
        Ok(None)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// The result of a successful [`Registry::extract`]: the extracted content
/// plus the identity of the extractor that produced it (so the caller can
/// stamp provenance and build the cache key without re-routing).
#[derive(Debug, Clone)]
pub struct RoutedExtract {
    pub extractor_name: String,
    pub extractor_version: String,
    pub extracted: Extracted,
}

#[cfg(test)]
mod tests;
