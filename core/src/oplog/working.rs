//! The `working` overlay verbs — the user's uncommitted edits as `user` ops
//! on top of `accepted` (per `op-log.md`'s "Layered document model"). The
//! editable buffer is `materialize(accepted + working)`; the agent's pending
//! ops render on top as the review overlay (`materialize_review`). These are
//! a second `impl OpLog` block kept here so `mod.rs` stays within its
//! file-length budget; they share the same private lock / `ensure_loaded`
//! machinery defined alongside `OpLog`. `commit_working` (the Save bridge into
//! the commit path) stays in `mod.rs` next to `commit_text_edit`.

use super::doc;
use super::error::Error;
use super::{DocContent, OpLog};

impl OpLog {
    /// Apply one uncommitted user edit to the `working` overlay — the editor's
    /// forward binding (editor change set → `working`). The span replaces
    /// `[byte_start, byte_start + byte_len)` with `new_text` at plain buffer
    /// offsets (the editable buffer is `materialize(accepted + working)`, both
    /// byte-indexed, so no coordinate translation). On the first edit since the
    /// buffer was clean, `working` is seeded as a clone of `accepted`. Nothing
    /// is persisted: `working` is in-memory only until [`commit_working`](Self::commit_working).
    ///
    /// status: op-log-working-layer
    /// status: op-log-editor-binding
    pub fn apply_working_edit(
        &self,
        doc_id: &str,
        byte_start: usize,
        byte_len: usize,
        new_text: &str,
    ) -> Result<(), Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let working = state
                .working
                .get_or_insert_with(|| doc::clone_doc(&state.accepted));
            doc::apply_replace(working, byte_start, byte_len, new_text);
            Ok(())
        })
    }

    /// Make `materialize_working(doc_id) == new_text` by applying the MINIMAL
    /// localized diff (`multi_span_delta`) between the current working text and
    /// `new_text` to `working` — NOT a whole-span remove-all + insert-all. The
    /// canvas forward binding has no editor change set to walk (it re-serializes
    /// the whole model each edit), so it hands the op log the full text; this
    /// localizes it, exactly as `docs/canvas.md` specifies ("minimal localized
    /// text ops on the working layer").
    ///
    /// Localization is load-bearing for sync, not just churn: a full remove-all +
    /// insert-all TOMBSTONES the entire working structure and re-authors it. When
    /// a peer delta later mirrors onto that overlay
    /// ([`apply_remote_update`](super::OpLog::apply_remote_update) keeps
    /// `materialize_working == accepted + working`), the peer's ops are anchored
    /// in now-tombstoned content and Yrs relocates them to byte 0 — the reported
    /// `<number>{`-prepended canvas corruption. Keeping the diff localized
    /// preserves the unchanged structure so a concurrent peer edit anchors in
    /// place and merges cleanly. The whole step runs under ONE lock so a
    /// concurrent [`commit_working`](Self::commit_working) can't tear it.
    /// status: op-log-working-layer, canvas-oplog-binding
    pub fn replace_working(&self, doc_id: &str, new_text: &str) -> Result<(), Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let working = state
                .working
                .get_or_insert_with(|| doc::clone_doc(&state.accepted));
            let current = doc::materialize(working).text;
            let spans = doc::multi_span_delta(&current, new_text);
            doc::apply_replaces(working, &spans);
            Ok(())
        })
    }

    /// The editable buffer: `materialize(accepted + working)`. When the buffer
    /// is clean (`working` is `None`) this equals `materialize(accepted)`.
    /// Pending agent ops are *not* included — they render as the review overlay
    /// on top via [`materialize_review`](Self::materialize_review).
    ///
    /// status: op-log-working-layer
    /// status: op-log-layered-model
    pub fn materialize_working(&self, doc_id: &str) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let doc = state.working.as_ref().unwrap_or(&state.accepted);
            Ok(doc::materialize(doc).into())
        })
    }

    /// The review overlay's "current" side: `materialize(accepted + working +
    /// pending(session))`. Clones the working-or-accepted doc and applies the
    /// session's pending updates on top (best-effort — a drifted op's apply
    /// error is swallowed, matching `materialize_pending_view`). `session =
    /// None` applies the whole queue. The app diffs this against
    /// [`materialize_working`](Self::materialize_working) to render the inline
    /// review of the agent's proposals while the user keeps editing.
    ///
    /// status: op-log-working-layer
    /// status: op-log-layered-model
    pub fn materialize_review(
        &self,
        doc_id: &str,
        session: Option<&str>,
    ) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let base = state.working.as_ref().unwrap_or(&state.accepted);
            // Fast path: no pending op in scope → review == base. Skip the
            // `clone_doc` (a full Yrs encode→decode) + re-apply, which would
            // otherwise run every frame on a clean buffer. This is the common
            // case (no agent edits pending).
            let has_pending = state
                .pending
                .iter()
                .any(|op| session.is_none() || op.session_id.as_deref() == session);
            if !has_pending {
                return Ok(doc::materialize(base).into());
            }
            let view = doc::clone_doc(base);
            for pos in 0..state.pending.len() {
                let in_session =
                    session.is_none() || state.pending[pos].session_id.as_deref() == session;
                if !in_session {
                    continue;
                }
                // Skip a drifted op: its anchor no longer matches `accepted`
                // (e.g. a sync/external edit rewrote the region), so applying it
                // best-effort would interleave into a positional-merge garble.
                // The inline review shows only cleanly-applicable proposals;
                // drifted ops surface (flagged) through the patch-review queue.
                if Self::op_drifted(state, doc_id, pos) {
                    continue;
                }
                let _ = doc::apply_update(&view, doc_id, &state.pending[pos].yrs_update);
            }
            Ok(doc::materialize(&view).into())
        })
    }

    /// Discard the user's uncommitted edits: drop the `working` overlay so the
    /// buffer reverts to `materialize(accepted)`. Nothing reached disk, so
    /// there is nothing to roll back.
    ///
    /// status: op-log-working-layer
    pub fn discard_working(&self, doc_id: &str) -> Result<(), Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            state.working = None;
            Ok(())
        })
    }

    /// Whether the buffer has uncommitted user edits (`working` is `Some`).
    ///
    /// status: op-log-working-layer
    pub fn has_working_edits(&self, doc_id: &str) -> Result<bool, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(state.working.is_some())
        })
    }
}
