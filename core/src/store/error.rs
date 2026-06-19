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
        // Mirror the layered-doc mapper's discipline (`ops/op_writes.rs::map_err`):
        // preserve the variant shape the kind-tagged frontend keys on. A
        // store-side "note not found" must reach the UI as `not_found`, not be
        // flattened to `io`. `HikerError` has no schema/dimension variants, so
        // those structural failures stay io-shaped (they're internal-state
        // faults, not user-addressable inputs).
        match e {
            Error::NotFound(_) => HikerError::NotFound(e.to_string()),
            Error::VersionMismatch { .. }
            | Error::EmbedDim { .. }
            | Error::Sqlite(_)
            | Error::Io(_) => HikerError::Io(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_store_errors_keep_their_shape_through_hikererror() {
        // A store-side "note not found" must reach the frontend as the
        // NotFound variant, not be flattened to Io (the bug this guards).
        assert!(matches!(
            HikerError::from(Error::NotFound("foo.md".into())),
            HikerError::NotFound(_)
        ));
        // Schema/dimension/io faults stay io-shaped.
        assert!(matches!(
            HikerError::from(Error::VersionMismatch {
                found: 1,
                expected: 2
            }),
            HikerError::Io(_)
        ));
        assert!(matches!(
            HikerError::from(Error::EmbedDim {
                got: 384,
                expected: 768
            }),
            HikerError::Io(_)
        ));
        assert!(matches!(
            HikerError::from(Error::Io(std::io::Error::other("disk"))),
            HikerError::Io(_)
        ));
    }
}
