//! Embedding model wrapper. See docs/index.md.
//!
//! All fastembed-rs usage is confined to this module. Callers see only the
//! `Embedder` trait and the DTOs it returns. Both `load` and `embed_batch`
//! are synchronous + CPU-bound; tokio-aware callers (the indexer task) must
//! wrap them in `spawn_blocking`.
//!
//! status: embedder-fastembed-v5
//! status: embedder-model-selectable
//! status: embedder-version-per-model
//! status: embedder-dim-from-model

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use thiserror::Error;

/// Default model id — the v1 fastembed pick. Stays as the loader default and
/// the value `notes.embedder_version` gets when no `[indexing].model` is
/// configured. See `embedder-fastembed-bge-small`.
pub const DEFAULT_MODEL_ID: &str = "bge-small-en-v1.5";

#[derive(Debug, Error)]
pub enum Error {
    #[error("no platform data dir available")]
    NoDataDir,
    #[error("unknown embedder model id: {0}")]
    UnknownModel(String),
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
    /// re-embed naturally. For fastembed, this is the model id verbatim
    /// (`bge-small-en-v1.5`, `bge-m3`, `embedding-gemma-300m`).
    fn version(&self) -> &str;

    /// Output vector dimension. Source of truth for the chunk_vecs schema —
    /// `Store::ensure_chunk_vecs_dim(embedder.dim())` runs once at indexer
    /// startup before any ingest.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Synchronous and CPU-bound — wrap in
    /// `tokio::task::spawn_blocking` from async contexts. Empty input returns
    /// an empty vec without invoking the model.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error>;
}

/// Static registry of the supported fastembed models. Mirrors the spec table
/// in `docs/index.md` §"Embedder". The dim is duplicated from fastembed's
/// per-model `ModelInfo` so the loader can answer `Embedder::dim()` without
/// having to run an inference probe.
const KNOWN_MODELS: &[(&str, EmbeddingModel, usize)] = &[
    ("bge-small-en-v1.5", EmbeddingModel::BGESmallENV15, 384),
    ("bge-m3", EmbeddingModel::BGEM3, 1024),
    ("embedding-gemma-300m", EmbeddingModel::EmbeddingGemma300M, 768),
];

/// Resolve a model id (e.g. `"bge-small-en-v1.5"`) to its fastembed variant
/// and output dim. Returns `None` for unknown ids — callers convert that
/// into `Error::UnknownModel` or `HikerError::Config` as appropriate.
pub fn resolve_model(id: &str) -> Option<(EmbeddingModel, usize)> {
    KNOWN_MODELS
        .iter()
        .find(|(name, _, _)| *name == id)
        .map(|(_, m, d)| (m.clone(), *d))
}

/// Dim for a known model id, or `None` if the id isn't in the registry.
/// Used by the settings UI ("Dim change" bullet) and by validators that
/// want to reject an unsupported model id before any expensive load.
pub fn model_dim(id: &str) -> Option<usize> {
    resolve_model(id).map(|(_, d)| d)
}

/// True if `id` names a supported fastembed model. Backs the strict-load
/// validator in `core::config` (`[indexing].model`).
pub fn is_known_model(id: &str) -> bool {
    resolve_model(id).is_some()
}

/// All supported model ids, in spec order. Used by the settings dropdown
/// row builder so the TS side doesn't hand-maintain a parallel list.
pub fn supported_model_ids() -> Vec<&'static str> {
    KNOWN_MODELS.iter().map(|(name, _, _)| *name).collect()
}

/// Production embedder backed by fastembed-rs. Constructed once per process;
/// model weights load on `load_in` (fastembed downloads them on first call).
///
/// fastembed v5's `embed` takes `&mut self`, so the inner handle is wrapped
/// in a `Mutex`. The embedder is CPU-bound and called from the indexer's
/// `spawn_blocking` worker, so the lock is uncontended in practice.
pub struct FastembedEmbedder {
    inner: Mutex<TextEmbedding>,
    model_id: String,
    dim: usize,
    model_dir: PathBuf,
}

impl FastembedEmbedder {
    /// Resolve the platform-appropriate model cache directory and load the
    /// default model. Synchronous; multi-second on a cold cache. Wrap in
    /// `spawn_blocking` from async code.
    pub fn load() -> Result<Self, Error> {
        Self::load_id(DEFAULT_MODEL_ID)
    }

    /// Load a specific model by id (one of `KNOWN_MODELS`).
    pub fn load_id(model_id: &str) -> Result<Self, Error> {
        let dir = default_model_dir()?;
        Self::load_in(&dir, model_id)
    }

    /// Same as `load_id` but with an explicit model cache directory. Useful
    /// for tests and for callers that want the location overridden via
    /// settings.
    pub fn load_in(model_dir: &Path, model_id: &str) -> Result<Self, Error> {
        let (variant, expected_dim) = resolve_model(model_id)
            .ok_or_else(|| Error::UnknownModel(model_id.to_string()))?;
        std::fs::create_dir_all(model_dir)?;
        let inner = TextEmbedding::try_new(
            TextInitOptions::new(variant)
                .with_cache_dir(model_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .map_err(|e| Error::Load(format!("{e:#}")))?;
        let me = Self {
            inner: Mutex::new(inner),
            model_id: model_id.to_string(),
            dim: expected_dim,
            model_dir: model_dir.to_path_buf(),
        };
        // Sanity-check the dimension early so a model registry / runtime
        // mismatch surfaces on load, not on the first index pass.
        let probe = {
            let mut guard = me.inner.lock().expect("embedder mutex poisoned");
            guard
                .embed(vec!["dimension probe"], None)
                .map_err(|e| Error::Embed(e.to_string()))?
        };
        let got = probe.first().map(std::vec::Vec::len).unwrap_or(0);
        if got != expected_dim {
            return Err(Error::DimMismatch {
                got,
                expected: expected_dim,
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
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // fastembed wants `impl AsRef<[S]>`-shaped input; collect to Vec<&str>.
        let refs: Vec<&str> = texts.iter().map(std::string::String::as_str).collect();
        let out = {
            let mut guard = self.inner.lock().expect("embedder mutex poisoned");
            guard
                .embed(refs, None)
                .map_err(|e| Error::Embed(e.to_string()))?
        };
        for v in &out {
            if v.len() != self.dim {
                return Err(Error::DimMismatch {
                    got: v.len(),
                    expected: self.dim,
                });
            }
        }
        Ok(out)
    }
}

/// Resolve the platform model dir: `~/.local/share/hiker/models/` on Linux,
/// `~/Library/Application Support/com.hiker.Hiker/models/` on macOS,
/// `%LOCALAPPDATA%\hiker\models\` on Windows. See docs/index.md.
///
/// Windows uses a deliberately shallow path (skipping the `hiker\hiker\data\`
/// nesting `directories::ProjectDirs` produces) so the full huggingface cache
/// layout — `models--<org>--<name>\snapshots\<40-char sha>\onnx\model.onnx` —
/// stays under the 260-char MAX_PATH limit without requiring long-path
/// support to be enabled at the OS level.
pub fn default_model_dir() -> Result<PathBuf, Error> {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(local).join("hiker").join("models"));
        }
    }
    let dirs = directories::ProjectDirs::from("com", "hiker", "Hiker")
        .ok_or(Error::NoDataDir)?;
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
            dim: crate::store::DEFAULT_EMBED_DIM,
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

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
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
    use crate::store::DEFAULT_EMBED_DIM;

    #[test]
    fn mock_embedder_returns_correct_dim() {
        let emb = MockEmbedder::new("test");
        let out = emb
            .embed_batch(&["hello".to_string(), "world".to_string()])
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), DEFAULT_EMBED_DIM);
        assert_eq!(out[1].len(), DEFAULT_EMBED_DIM);
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
        assert!(!dir.as_os_str().is_empty());
    }

    #[test]
    fn known_models_have_expected_dims() {
        assert_eq!(model_dim("bge-small-en-v1.5"), Some(384));
        assert_eq!(model_dim("bge-m3"), Some(1024));
        assert_eq!(model_dim("embedding-gemma-300m"), Some(768));
        assert_eq!(model_dim("nonsense"), None);
    }

    #[test]
    fn supported_model_ids_lists_v1_set() {
        let ids = supported_model_ids();
        assert_eq!(
            ids,
            vec!["bge-small-en-v1.5", "bge-m3", "embedding-gemma-300m"]
        );
    }

    /// Real-model smoke test for the default model. Downloads ~30MB on first
    /// run and pegs CPU briefly. Run manually with `cargo test -- --ignored`.
    #[test]
    #[ignore = "downloads model + slow"]
    fn fastembed_load_and_embed_default() {
        let tmp = tempfile::tempdir().unwrap();
        let emb = FastembedEmbedder::load_in(tmp.path(), DEFAULT_MODEL_ID).unwrap();
        assert_eq!(emb.dim(), DEFAULT_EMBED_DIM);
        assert_eq!(emb.version(), DEFAULT_MODEL_ID);

        let out = emb
            .embed_batch(&[
                "Hiker is a personal notes app".to_string(),
                "Embedding similar text should produce similar vectors".to_string(),
            ])
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), DEFAULT_EMBED_DIM);
        // Determinism within a single load (same model state).
        let again = emb
            .embed_batch(&["Hiker is a personal notes app".to_string()])
            .unwrap();
        assert_eq!(out[0], again[0]);
    }
}
