//! Inline patch-review accept/reject dispatch for the editor buffer panel.
//!
//! This is the surface `patch-review.md`'s module placement names: it owns
//! the buffer's pending-view re-materialization after a flip, the file-pill
//! bulk verbs (Accept all / Reject all), the per-hunk Agent verbs, and the
//! drift + multi-session breakdown the pill renders. Every write goes
//! through `core::ops::op_writes` (`flip_op_status`, `review_materializations`),
//! so the app layer never touches `core::oplog` directly and `staging.db` is
//! no longer read on this path.
//!
//! Hunk → op id resolution lives in `diff_overlay::attach_agent_hunk_widgets`
//! (via `op_writes::ops_in_hunk`); this module consumes the resolved op ids
//! and the queue (`OpLog::pending_ops` + `is_pending_drifted`) for the bulk /
//! drift / session views.
//
// status: op-log-per-hunk-accept-reject
// status: patch-review-multi-session

use super::patch_review_pill::{PillAction, PillMeta, SessionRow};
use super::BufCtx;
use crate::state::{AppState, ToastLevel};

impl BufCtx<'_> {
    /// Resolve a pill bulk action against the pending ops on this buffer's
    /// document (scoped to the active session). Accept-all flips every
    /// non-drifted pending op to `Accepted` (drifted ops are skipped per
    /// `patch-review.md`); Reject-all flips every pending op — drifted ops
    /// included — to `Rejected`. Both route through
    /// `op_writes::flip_op_status`. After either, the next frame's editor
    /// binding picks up the new `materialize_review` (accept folded the op into
    /// `accepted` and `working`; reject dropped it from the queue) and re-
    /// renders the buffer + overlay with the user's `working` edits intact — no
    /// reload from disk. A repaint is requested so the result shows immediately.
    ///
    /// status: op-log-per-hunk-accept-reject
    pub(super) fn apply_pill_action(&mut self, action: &PillAction) {
        // Grab the egui context before the `app` reborrow so the repaint
        // request after a flip doesn't collide with the `&mut self.app` borrow.
        let ctx = self.ui.ctx().clone();
        let app = &mut *self.app;
        let path: &str = self.path;
        let session = app
            .session
            .buffers
            .get(path)
            .and_then(|b| b.active_session.clone());
        if action.accept_all {
            let (accepted, drifted) = Self::session_pending_op_ids(app, path, session.as_deref());
            if !accepted.is_empty() {
                if let Err(e) = hiker_core::ops::op_writes::flip_op_status(
                    &app.vault_session.services.oplog,
                    path,
                    &accepted,
                    /* accept */ true,
                ) {
                    app.push_toast(format!("Accept all failed: {e}"), ToastLevel::Error);
                } else {
                    ctx.request_repaint();
                    let suffix = if drifted > 0 {
                        format!(" ({drifted} drifted skipped)")
                    } else {
                        String::new()
                    };
                    let n = accepted.len();
                    app.push_toast(
                        format!("Accepted {} hunk{}{}", n, if n == 1 { "" } else { "s" }, suffix),
                        ToastLevel::Info,
                    );
                }
            }
        }
        if action.reject_all {
            // Reject covers drifted ops too — pass every pending op id.
            let (mut all, _drifted) = Self::session_pending_op_ids(app, path, session.as_deref());
            all.extend(Self::session_drifted_op_ids(app, path, session.as_deref()));
            if !all.is_empty() {
                if let Err(e) = hiker_core::ops::op_writes::flip_op_status(
                    &app.vault_session.services.oplog,
                    path,
                    &all,
                    /* accept */ false,
                ) {
                    app.push_toast(format!("Reject all failed: {e}"), ToastLevel::Error);
                } else {
                    ctx.request_repaint();
                    let n = all.len();
                    app.push_toast(
                        format!("Rejected {} hunk{}", n, if n == 1 { "" } else { "s" }),
                        ToastLevel::Info,
                    );
                }
            }
        }
        if let Some(byte) = action.scroll_to_byte
            && let Some(buffer) = app.session.buffers.get_mut(path)
        {
            let line = buffer.editor.doc.byte_to_line(byte);
            let target_y = buffer.view.height_map.y_at_row_top(line) - 24.0;
            buffer.view.scroll_y = target_y.max(0.0);
        }
    }

    /// The non-drifted pending op ids for `path` scoped to `session`, plus
    /// the count of drifted ops in scope. Accept-all consumes the first
    /// element (the ids it flips); the count feeds the toast suffix. Reads
    /// the queue off the op log via `doc_id_for_path` + `pending_ops`.
    fn session_pending_op_ids(
        app: &AppState,
        path: &str,
        session: Option<&str>,
    ) -> (Vec<String>, usize) {
        let log = &app.vault_session.services.oplog;
        let Ok(Some(doc_id)) = log.doc_id_for_path(path) else {
            return (Vec::new(), 0);
        };
        let Ok(pending) = log.pending_ops(&doc_id) else {
            return (Vec::new(), 0);
        };
        let mut ids = Vec::new();
        let mut drifted = 0usize;
        for op in &pending {
            if session.is_some() && op.session_id.as_deref() != session {
                continue;
            }
            if log.is_pending_drifted(&doc_id, &op.op_id).unwrap_or(false) {
                drifted += 1;
            } else {
                ids.push(op.op_id.clone());
            }
        }
        (ids, drifted)
    }

    /// The drifted pending op ids for `path` scoped to `session`. Reject-all
    /// resolves these in addition to the non-drifted ones.
    fn session_drifted_op_ids(app: &AppState, path: &str, session: Option<&str>) -> Vec<String> {
        let log = &app.vault_session.services.oplog;
        let Ok(Some(doc_id)) = log.doc_id_for_path(path) else {
            return Vec::new();
        };
        let Ok(pending) = log.pending_ops(&doc_id) else {
            return Vec::new();
        };
        pending
            .iter()
            .filter(|op| session.is_none() || op.session_id.as_deref() == session)
            .filter(|op| log.is_pending_drifted(&doc_id, &op.op_id).unwrap_or(false))
            .map(|op| op.op_id.clone())
            .collect()
    }

    /// Drift count + per-session pending-op breakdown for the file pill.
    /// Walks the document's pending queue once: counts drifted ops in the
    /// active session (the pill's `(M drifted)` suffix) and tallies pending
    /// ops per session id (the multi-session rows). A `None` session id maps
    /// to the synthetic `"(unscoped)"` label so anonymous pending ops still
    /// list. Empty when the path has no doc or no pending ops.
    ///
    /// status: patch-review-multi-session
    pub(super) fn pill_meta(app: &AppState, path: &str, active_session: Option<&str>) -> PillMeta {
        let log = &app.vault_session.services.oplog;
        let mut meta = PillMeta::default();
        let Ok(Some(doc_id)) = log.doc_id_for_path(path) else {
            return meta;
        };
        let Ok(pending) = log.pending_ops(&doc_id) else {
            return meta;
        };
        let mut per_session: std::collections::BTreeMap<Option<String>, usize> =
            std::collections::BTreeMap::new();
        for op in &pending {
            *per_session.entry(op.session_id.clone()).or_insert(0) += 1;
            let in_scope = active_session.is_none() || op.session_id.as_deref() == active_session;
            if in_scope && log.is_pending_drifted(&doc_id, &op.op_id).unwrap_or(false) {
                meta.drifted += 1;
            }
        }
        meta.sessions = per_session
            .into_iter()
            .map(|(sid, count)| SessionRow { session_id: sid, pending: count })
            .collect();
        meta.active_session = active_session.map(str::to_string);
        meta
    }
}

/// Per-hunk Agent verbs on `AppState`. The diff overlay dispatches a hunk's
/// Accept / Reject button to these with the op ids `ops_in_hunk` resolved for
/// that hunk's current-text range. Methods with `&mut self` receivers are
/// exempt from `clippy::single_call_fn`.
impl AppState {
    /// Per-hunk Accept: flip every pending op contributing to the hunk to
    /// `Accepted` via `op_writes::flip_op_status`, which applies the op's Yrs
    /// update to `accepted` *and* `working` and atomically rewrites the `.md`.
    /// No reload from disk: the next frame's editor binding picks up the new
    /// `materialize_review` (with the op now folded into `accepted`/`working`
    /// and dropped from the pending queue) via its reverse step, with the
    /// user's other `working` edits intact. The caller requests a repaint so
    /// the result shows immediately.
    ///
    /// status: op-log-per-hunk-accept-reject
    pub(super) fn apply_hunk_accept(&mut self, path: &str, op_ids: &[String]) {
        match hiker_core::ops::op_writes::flip_op_status(
            &self.vault_session.services.oplog,
            path,
            op_ids,
            /* accept */ true,
        ) {
            Ok(()) => {
                let n = op_ids.len();
                self.push_toast(
                    format!("Accepted {} hunk{}", n, if n == 1 { "" } else { "s" }),
                    ToastLevel::Info,
                );
            }
            Err(e) => self.push_toast(format!("Accept failed: {e}"), ToastLevel::Error),
        }
    }

    /// Per-hunk Reject: flip every pending op contributing to the hunk to
    /// `Rejected` via `op_writes::flip_op_status`, which writes the rejected
    /// audit row and drops the op from the queue, leaving `accepted` and
    /// `working` untouched. No reload from disk: the next frame's editor
    /// binding re-renders the overlay against the now-smaller pending queue
    /// with the user's `working` edits intact. The caller requests a repaint.
    ///
    /// status: op-log-per-hunk-accept-reject
    pub(super) fn apply_hunk_reject(&mut self, path: &str, op_ids: &[String]) {
        match hiker_core::ops::op_writes::flip_op_status(
            &self.vault_session.services.oplog,
            path,
            op_ids,
            /* accept */ false,
        ) {
            Ok(()) => {
                let n = op_ids.len();
                self.push_toast(
                    format!("Rejected {} hunk{}", n, if n == 1 { "" } else { "s" }),
                    ToastLevel::Info,
                );
            }
            Err(e) => self.push_toast(format!("Reject failed: {e}"), ToastLevel::Error),
        }
    }

    /// Conflict-hunk "Keep theirs" (per `op-log-merge-conflict`): the agent op
    /// and the user's `working` edit touch the same region; the user keeps the
    /// agent's version. First revert the user's overlapping `working` edit back
    /// to the accepted bytes (`revert` is the precomputed `apply_working_edit`
    /// args — `(byte_start, byte_len, accepted_text)`), so the pending op now
    /// lands against canonical text; then accept the op via `flip_op_status`.
    /// The next frame's editor binding re-materializes the buffer (the revert
    /// shows immediately, the accepted content folds in), with the user's other
    /// `working` edits intact. The caller requests a repaint.
    ///
    /// status: op-log-merge-conflict
    pub(super) fn apply_hunk_keep_theirs(
        &mut self,
        path: &str,
        op_ids: &[String],
        revert: &(usize, usize, String),
    ) {
        let log = self.vault_session.services.oplog.as_ref();
        let doc_id = match log.doc_id_for_path(path) {
            Ok(Some(id)) => id,
            Ok(None) => {
                self.push_toast(format!("Keep theirs failed: no doc for {path}"), ToastLevel::Error);
                return;
            }
            Err(e) => {
                self.push_toast(format!("Keep theirs failed: {e}"), ToastLevel::Error);
                return;
            }
        };
        let (byte_start, byte_len, ref accepted_text) = *revert;
        if let Err(e) = log.apply_working_edit(&doc_id, byte_start, byte_len, accepted_text) {
            self.push_toast(format!("Keep theirs failed (revert): {e}"), ToastLevel::Error);
            return;
        }
        match hiker_core::ops::op_writes::flip_op_status(
            &self.vault_session.services.oplog,
            path,
            op_ids,
            /* accept */ true,
        ) {
            Ok(()) => self.push_toast("Kept agent's version".to_string(), ToastLevel::Info),
            Err(e) => self.push_toast(format!("Keep theirs failed: {e}"), ToastLevel::Error),
        }
    }
}
