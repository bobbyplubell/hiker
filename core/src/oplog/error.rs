//! Error type for the op-log substrate. All failure modes the `OpLog`
//! surface can hit — SQLite, filesystem, Yrs update decode/apply, JSON
//! (de)serialization of the pending queue, bincode of the history frames,
//! and the fail-loud schema-version guard — funnel through one enum so
//! consumers match per-variant the same way they do for `StoreError`.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("pending-queue json encode/decode: {0}")]
    Json(#[from] serde_json::Error),
    #[error("history-frame encode/decode: {0}")]
    Bincode(#[from] bincode::Error),
    /// A serialized Yrs update failed to decode (corruption) or failed to
    /// apply against the target Doc. Carries the doc id for correlation.
    #[error("yrs update for doc {doc_id}: {message}")]
    YrsUpdate { doc_id: String, message: String },
    /// The `old_str` anchor of a producer edit did not resolve to exactly
    /// one byte range in `materialize(accepted)`. Distinguishes "no match"
    /// from "multiple matches without replace_all" via the message.
    #[error("anchor: {0}")]
    Anchor(String),
    /// No document is registered for the given vault-relative path.
    #[error("unknown document path: {0}")]
    UnknownPath(String),
    /// No document is registered for the given doc id.
    #[error("unknown document id: {0}")]
    UnknownDoc(String),
    /// No pending op with the given id exists in the queue.
    #[error("unknown pending op: {0}")]
    UnknownPendingOp(String),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("connection mutex poisoned")]
    Poisoned,
}
