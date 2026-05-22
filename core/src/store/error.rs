//! Error type for the index store and its `HikerError` bridge.

use crate::errors::HikerError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("embedding dimension mismatch: got {got}, expected {expected}")]
    EmbedDim { got: usize, expected: usize },
    #[error("note not found: {0}")]
    NotFound(String),
}

impl From<Error> for HikerError {
    fn from(e: Error) -> Self {
        HikerError::Io(e.to_string())
    }
}
