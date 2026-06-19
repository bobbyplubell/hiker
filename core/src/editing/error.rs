//! Error type for the layered editing model. The failure modes the `LayeredDoc`
//! surface can hit — filesystem, JSON (de)serialization of the pending
//! queue, anchor resolution, and the fold round-trip invariant — funnel
//! through one enum so consumers match per-variant the same way they do for
//! `StoreError`.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("pending-queue json encode/decode: {0}")]
    Json(#[from] serde_json::Error),
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
    /// `accept_pending` refused because the op's edit was produced against
    /// `accepted + the session's prior pending ops` (the fallback path in
    /// `stage_pending`), and at least one of those predecessor ops
    /// is still pending — accepting this op alone would land a drifted
    /// update against positions the predecessor establishes. The user must
    /// accept (or reject) the listed predecessor(s) first. Local-only:
    /// pending ops never sync.
    #[error("op {op_id} depends on unaccepted predecessor(s): {predecessors:?}")]
    DependsOn {
        op_id: String,
        predecessors: Vec<String>,
    },
    /// A seed-from-disk registration (`LayeredDoc::seed_document`, the
    /// bootstrap/first-open path) found the file's on-disk bytes differ from
    /// the bytes being seeded. The seed path verifies instead of rewriting
    /// (`op-log-disk-canonical`), so rather than silently clobber the file —
    /// and churn its mtime — we surface the drift. Carries the vault-relative
    /// path. By construction (the seed text IS the bytes just read off disk)
    /// this only fires on a genuine race or bug.
    #[error("seed content for {path} does not match the file on disk")]
    SeedMismatch { path: String },
    /// Folding a full-text edit into the layered doc produced a materialized
    /// result that does NOT equal the intended target text — i.e. the span delta
    /// (`multi_span_delta`) + splice (`apply_spans_str`) round-trip diverged.
    /// This is the invariant guarding the fold-in path (user save): a divergence
    /// means `accepted` would silently disagree with what we meant to write, so
    /// we refuse to persist rather than emit drifted bytes. Carries the
    /// vault-relative path.
    #[error("fold round-trip diverged for {path}: spliced result != target text")]
    FoldRoundTrip { path: String },
    #[error("connection mutex poisoned")]
    Poisoned,
}
