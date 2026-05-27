//! Plugin host error type. One enum the host surfaces to the plugins panel
//! (load failures, hash mismatches, runtime traps) so a misbehaving plugin is
//! visible and contained rather than crashing the app.
//
// status: plugin-host

/// Errors from loading or running a plugin.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("plugin io: {0}")]
    Io(String),
    #[error("manifest parse: {0}")]
    Manifest(String),
    #[error("plugin changed on disk: expected {expected}, found {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("wasm engine: {0}")]
    Engine(String),
    #[error("plugin trap: {0}")]
    Trap(String),
    #[error("plugin abi: {0}")]
    Abi(String),
    #[error("no plugin with id {0}")]
    NotFound(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
