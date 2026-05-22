//! Error type for the staging store and its bridge from `HikerError`.

use std::io;

use thiserror::Error;

use crate::changes::Error as ChangesError;
use crate::errors::HikerError;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("proposal not found: {0}")]
    ProposalNotFound(String),
    #[error("disk drift: file changed since proposal (expected hash {expected}, found {found})")]
    DiskDrift { expected: String, found: String },
    #[error("missing content: proposal {0} has no content to write")]
    MissingContent(String),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    /// status: staging-per-edit-proposals
    /// Anchor (`old_str`) failed to resolve against current disk on accept:
    /// either zero matches (`anchor_missing`) or multiple matches without
    /// `replace_all` (`anchor_not_unique`).
    #[error("edit anchor: {0}")]
    AnchorConflict(String),
    #[error("changes error: {0}")]
    Changes(#[from] ChangesError),
    #[error("vault error: {0}")]
    Vault(String),
    #[error("connection mutex poisoned")]
    Poisoned,
}

impl From<HikerError> for Error {
    fn from(e: HikerError) -> Self {
        match e {
            HikerError::DiskDrift { expected, found } => Error::DiskDrift { expected, found },
            HikerError::Io(s) => Error::Io(io::Error::other(s)),
            HikerError::NotFound(s) => Error::ProposalNotFound(s),
            _ => Error::Vault(e.to_string()),
        }
    }
}
