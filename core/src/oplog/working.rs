//! The `working` overlay verbs — the user's uncommitted edits as `user` ops
//! on top of `accepted` (per `op-log.md`'s "Layered document model"). The
//! editable buffer is `materialize(accepted + working)`; the agent's pending
//! ops render on top as the review overlay (`materialize_review`). These are
//! a second `impl OpLog` block kept here so `mod.rs` stays within its
//! file-length budget; they share the same private lock / `ensure_loaded`
//! machinery defined alongside `OpLog`. `commit_working` (the Save bridge into
//! the commit path) stays in `mod.rs` next to `commit_text_edit`.

use super::error::Error;
use super::overlay;
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
            if state.working.is_none() {
                state.working = Some(state.accepted.clone());
            }
            let working = state.working.as_ref().unwrap();
            // Splice `[byte_start, byte_start + byte_len)` → `new_text`. A single
            // span, applied defensively (a drifted span is skipped, never panics).
            state.working = Some(overlay::apply_spans_str(
                working,
                &[(byte_start, byte_len, new_text.to_string())],
            ));
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
    /// Localization is load-bearing for sync, not just churn: a full replace of
    /// the whole working text rewrites every byte, so a concurrent peer edit
    /// mirroring onto that overlay
    /// ([`apply_remote_update`](super::OpLog::apply_remote_update) keeps
    /// `materialize_working == accepted + working`) finds no unchanged context to
    /// anchor against and its content can land at byte 0 — the reported
    /// `<number>{`-prepended canvas corruption. Keeping the diff localized
    /// preserves the unchanged structure so a concurrent peer edit anchors in
    /// place and merges cleanly. The whole step runs under ONE lock so a
    /// concurrent [`commit_working`](Self::commit_working) can't tear it.
    /// status: op-log-working-layer, canvas-oplog-binding
    pub fn replace_working(&self, doc_id: &str, new_text: &str) -> Result<(), Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            if state.working.is_none() {
                state.working = Some(state.accepted.clone());
            }
            let current = state.working.as_deref().unwrap();
            let spans = crate::merge::multi_span_delta(current, new_text);
            state.working = Some(overlay::apply_spans_str(current, &spans));
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
            // The editable buffer's tombstone is the accepted flag (the working
            // overlay is text-only; a tombstone is never an uncommitted edit).
            Ok(DocContent {
                text: state.working_text().to_string(),
                tombstone: state.accepted_tombstone,
            })
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
            let base_text = state.working_text().to_string();
            let base_tombstone = state.accepted_tombstone;
            // Fast path: no pending op in scope → review == base. Skip the fold,
            // which would otherwise run every frame on a clean buffer. This is
            // the common case (no agent edits pending).
            let has_pending = state
                .pending
                .iter()
                .any(|op| session.is_none() || op.session_id.as_deref() == session);
            if !has_pending {
                return Ok(DocContent {
                    text: base_text,
                    tombstone: base_tombstone,
                });
            }
            // Fold the session's pending ops onto the base text by splicing,
            // skipping a drifted op: its anchor no longer matches `accepted`
            // (e.g. a sync/external edit rewrote the region). The inline review
            // shows only cleanly-applicable proposals; drifted ops surface
            // (flagged) through the patch-review queue. Pending text ops don't
            // change the tombstone flag — read it from the base doc.
            let folded = overlay::fold_session_text(&base_text, &state.pending, session, |pos| {
                Self::op_drifted(state, doc_id, pos)
            });
            Ok(DocContent {
                text: folded,
                tombstone: base_tombstone,
            })
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
