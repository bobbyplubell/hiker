//! Embedding model wrapper. See docs/index.md.
//!
//! All fastembed-rs usage is confined to this module. Callers see only the
//! `Embedder` trait and the DTOs it returns. Both `load` and `embed_batch`
//! are synchronous + CPU-bound; tokio-aware callers (the indexer task) must
//! wrap them in `spawn_blocking`.

use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use thiserror::Error;

use crate::store::EMBED_DIM;

/// Version string stored in `notes.embedder_version`. Bump this whenever the
/// model identity changes — the indexer treats a mismatch as "re-embed
/// everything." Format is the model id verbatim so it's recognizable.
pub const EMBEDDER_VERSION: &str = "bge-small-en-v1.5";

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("no platform data dir available")]
    NoDataDir,
    #[error("model load: {0}")]
    Load(String),
    #[error("embed: {0}")]
    Embed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("dim mismatch from model: got {got}, expected {expected}")]
    DimMismatch { got: usize, expected: usize },
}

/// Narrow, opaque embedding interface. The concrete implementation
/// (fastembed-rs today, possibly candle / cloud / multilingual later) lives
/// behind this trait so swapping it is a one-module change.
pub trait Embedder: Send + Sync {
    /// Stable identifier for the model (and any preprocessing) currently in
    /// use. Stored alongside each note's row in the index; bumps trigger
    /// re-embed naturally.
    fn version(&self) -> &str;

    /// Output vector dimension. Must match `store::EMBED_DIM` for the index
    /// to accept the vectors.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Synchronous and CPU-bound — wrap in
    /// `tokio::task::spawn_blocking` from async contexts. Empty input returns
    /// an empty vec without invoking the model.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Production embedder backed by fastembed-rs. Constructed once per process;
/// model weights load lazily on first embed call (fastembed handles that
/// internally), but the model files themselves download on `load`.
pub struct FastembedEmbedder {
    inner: TextEmbedding,
    model_dir: PathBuf,
}

impl FastembedEmbedder {
    /// Resolve the platform-appropriate model cache directory and load (or
    /// download on first call) the bge-small model. Synchronous; multi-second
    /// on a cold cache. Wrap in `spawn_blocking` from async code.
    pub fn load() -> Result<Self, EmbedError> {
        let dir = default_model_dir()?;
        Self::load_in(&dir)
    }

    /// Same as `load` but with an explicit model cache directory. Useful for
    /// tests and for callers that want the location overridden via settings.
    pub fn load_in(model_dir: &Path) -> Result<Self, EmbedError> {
        std::fs::create_dir_all(model_dir)?;
        let inner = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(model_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .map_err(|e| EmbedError::Load(e.to_string()))?;
        let me = Self {
            inner,
            model_dir: model_dir.to_path_buf(),
        };
        // Sanity-check the dimension early so a mismatch surfaces on load,
        // not on the first index pass.
        let probe = me
            .inner
            .embed(vec!["dimension probe"], None)
            .map_err(|e| EmbedError::Embed(e.to_string()))?;
        let got = probe.first().map(|v| v.len()).unwrap_or(0);
        if got != EMBED_DIM {
            return Err(EmbedError::DimMismatch {
                got,
                expected: EMBED_DIM,
            });
        }
        Ok(me)
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }
}

impl Embedder for FastembedEmbedder {
    fn version(&self) -> &str {
        EMBEDDER_VERSION
    }

    fn dim(&self) -> usize {
        EMBED_DIM
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // fastembed wants Vec<&str>-shaped input; clone is cheap (string refs).
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let out = self
            .inner
            .embed(refs, None)
            .map_err(|e| EmbedError::Embed(e.to_string()))?;
        for v in &out {
            if v.len() != EMBED_DIM {
                return Err(EmbedError::DimMismatch {
                    got: v.len(),
                    expected: EMBED_DIM,
                });
            }
        }
        Ok(out)
    }
}

/// Resolve the platform model dir: `~/.local/share/hiker/models/` on Linux,
/// `~/Library/Application Support/com.hiker.Hiker/models/` on macOS,
/// `%APPDATA%\hiker\hiker\data\models\` on Windows. See docs/index.md.
pub fn default_model_dir() -> Result<PathBuf, EmbedError> {
    let dirs = directories::ProjectDirs::from("com", "hiker", "Hiker")
        .ok_or(EmbedError::NoDataDir)?;
    Ok(dirs.data_dir().join("models"))
}

/// Deterministic embedder for tests of other modules. Hashes input text into
/// a fixed-dimension vector; same text always produces the same vector,
/// distinct texts produce distinct vectors. Not for production use.
pub struct MockEmbedder {
    version: String,
    dim: usize,
}

impl MockEmbedder {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            dim: EMBED_DIM,
        }
    }

    pub fn with_dim(version: impl Into<String>, dim: usize) -> Self {
        Self {
            version: version.into(),
            dim,
        }
    }
}

impl Embedder for MockEmbedder {
    fn version(&self) -> &str {
        &self.version
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            let h = blake3::hash(t.as_bytes());
            let bytes = h.as_bytes();
            // Spread the 32 hash bytes across `dim` floats by tiling and
            // mapping each byte to [-1, 1].
            let mut v = Vec::with_capacity(self.dim);
            for i in 0..self.dim {
                let b = bytes[i % bytes.len()];
                v.push((b as f32 / 127.5) - 1.0);
            }
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_embedder_returns_correct_dim() {
        let emb = MockEmbedder::new("test");
        let out = emb
            .embed_batch(&["hello".to_string(), "world".to_string()])
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), EMBED_DIM);
        assert_eq!(out[1].len(), EMBED_DIM);
    }

    #[test]
    fn mock_embedder_is_deterministic() {
        let emb = MockEmbedder::new("test");
        let a = emb.embed_batch(&["same input".to_string()]).unwrap();
        let b = emb.embed_batch(&["same input".to_string()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn mock_embedder_distinguishes_inputs() {
        let emb = MockEmbedder::new("test");
        let out = emb
            .embed_batch(&["one".to_string(), "two".to_string()])
            .unwrap();
        assert_ne!(out[0], out[1]);
    }

    #[test]
    fn mock_embedder_empty_batch() {
        let emb = MockEmbedder::new("test");
        let out = emb.embed_batch(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn mock_embedder_reports_version() {
        let emb = MockEmbedder::new("v0");
        assert_eq!(emb.version(), "v0");
    }

    #[test]
    fn default_model_dir_resolves() {
        let dir = default_model_dir().unwrap();
        // Don't create it here — just confirm we got a non-empty path.
        assert!(!dir.as_os_str().is_empty());
    }

    /// Real-model smoke test. Downloads ~30MB on first run and pegs CPU
    /// briefly. Run manually with `cargo test -- --ignored`.
    #[test]
    #[ignore = "downloads model + slow"]
    fn fastembed_load_and_embed() {
        let tmp = tempfile::tempdir().unwrap();
        let emb = FastembedEmbedder::load_in(tmp.path()).unwrap();
        assert_eq!(emb.dim(), EMBED_DIM);
        assert_eq!(emb.version(), EMBEDDER_VERSION);

        let out = emb
            .embed_batch(&[
                "Hiker is a personal notes app".to_string(),
                "Embedding similar text should produce similar vectors".to_string(),
            ])
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), EMBED_DIM);
        // Determinism within a single load (same model state).
        let again = emb
            .embed_batch(&["Hiker is a personal notes app".to_string()])
            .unwrap();
        assert_eq!(out[0], again[0]);
    }
}
