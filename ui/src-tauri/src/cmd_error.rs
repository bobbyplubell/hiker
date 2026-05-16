//! Typed error envelope for `#[tauri::command]` functions.
//!
//! Every Tauri command used to return `Result<X, String>` and constructed
//! that String via dozens of `.map_err(|e| e.to_string())` calls. The
//! Tauri IPC layer only requires that the error type be `serde::Serialize`
//! — it doesn't actually mandate a String — so we replace the ad-hoc
//! conversions with a `CmdError` enum carrying `From` impls for every
//! error source that flows through the command surface.
//!
//! Wire format: `CmdError` serializes as a plain JSON string (its
//! `Display` impl). The frontend has always treated command errors as
//! opaque strings (`String(err)` everywhere — see `ui/src/`'s catch
//! arms), so the bytes that cross the IPC boundary are byte-identical
//! to the pre-refactor shape. The typed variants are a Rust-side
//! convenience for `?` operator + `From` conversions, not a wire-format
//! change.

use serde::{Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CmdError {
    /// Wraps a `hiker_core::HikerError`. The Display impl on `HikerError`
    /// already produces the same string the old `map_err(|e| e.to_string())`
    /// chains emitted.
    #[error("{0}")]
    Core(#[from] hiker_core::HikerError),

    /// `serde_json::Error` from `serde_json::from_str` / `to_string` in
    /// command bodies (scope_json / method_json / policy_json parsing).
    #[error("{0}")]
    Json(#[from] serde_json::Error),

    /// `std::io::Error` from filesystem ops invoked directly in command
    /// bodies (e.g. the snapshot/rollback path's `std::fs::read`).
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// `tokio::task::JoinError` from `spawn_blocking` joins (search
    /// query-embed, rollup summary embeddings).
    #[error("{0}")]
    Join(#[from] tokio::task::JoinError),

    /// Catch-all for ad-hoc string errors produced inside a command body
    /// — preserves the exact string the legacy `Err("…".to_string())` /
    /// `format!("…")` paths produced. Constructed via `From<String>` so
    /// `?` works on `Result<_, String>` values returned by helper
    /// functions that still use the legacy shape.
    #[error("{0}")]
    Other(String),
}

impl CmdError {
    /// Cheap constructor for the common "no vault open" guard that fires
    /// at the top of nearly every command body. Kept as a `&'static str`
    /// rather than a dedicated variant so the wire bytes match the
    /// legacy `"no vault open".to_string()` exactly.
    pub(crate) fn no_vault_open() -> Self {
        CmdError::Other("no vault open".to_string())
    }
}

impl From<String> for CmdError {
    fn from(s: String) -> Self {
        CmdError::Other(s)
    }
}

impl From<&str> for CmdError {
    fn from(s: &str) -> Self {
        CmdError::Other(s.to_string())
    }
}

impl<T> From<std::sync::PoisonError<T>> for CmdError {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        CmdError::Other(e.to_string())
    }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for CmdError {
    fn from(e: tokio::sync::mpsc::error::SendError<T>) -> Self {
        CmdError::Other(e.to_string())
    }
}

/// Wire-format preservation: serialize as the Display string only. The
/// frontend has always seen command errors as opaque strings (the JS
/// catch arms across `ui/src/` do `String(err)` / `err.message`); the
/// typed enum is for the Rust side. If a future surface wants
/// structured discrimination, swap this for a tagged-enum derive — the
/// `CmdError` variants are already shaped for that.
impl Serialize for CmdError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// Convenience alias matching the legacy `Result<X, String>` shape so
/// command signatures read as a one-token swap.
pub(crate) type CmdResult<T> = Result<T, CmdError>;
