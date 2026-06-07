//! Accepted-op history: the per-doc `<doc-id>.ops` retention log and the
//! point-in-time reconstruction it backs (`materialize_at`). Each frame is a
//! keyframe (full text) or a delta against the previous frame (per
//! `op-log.md`'s retention section), so the log stays small while any past
//! version reconstructs by decoding from the nearest keyframe forward. This is
//! a second `impl OpLog` block kept here so `mod.rs` stays within its
//! file-length budget; it shares the same lock / `ensure_loaded` machinery
//! defined alongside `OpLog` in `mod.rs`.

use rusqlite::Connection;

use super::error::Error;
use super::{DocContent, DocState, OpLog};

/// A keyframe (full snapshot) is written every Nth history frame; the frames
/// between are deltas. Bounds `materialize_at` reconstruction to decoding at
/// most this many deltas from the nearest keyframe.
const KEYFRAME_INTERVAL: usize = 16;

impl OpLog {
    /// `materialize(accepted as of `op_id`)` — the document's accepted content
    /// at the historical point of a specific accepted op, reconstructed from
    /// the retained frames in `<doc-id>.ops`. `Ok(None)` when no retained frame
    /// matches `op_id` (an unknown op, a pre-retention op, or a lifecycle
    /// marker with no content frame). The version-dropdown preview, snapshot
    /// diff, and rollback all read through here. Reconstruction never touches
    /// the live `accepted` text on the read path, so there is no aliasing risk.
    ///
    /// status: op-log-history-materialization
    pub fn materialize_at(&self, doc_id: &str, op_id: &str) -> Result<Option<DocContent>, Error> {
        // File read outside the lock — the frames are immutable history and
        // the live `accepted` text is never touched by reconstruction.
        let frames = super::store::load_ops(&self.oplog_dir, doc_id)?;
        let Some(idx) = frames.iter().position(|f| f.op_id == op_id) else {
            return Ok(None);
        };
        let text = Self::reconstruct_frame(&frames, idx)?;
        Ok(Some(DocContent { text, tombstone: frames[idx].tombstone }))
    }

    /// Append a self-describing history frame for an accepted op to
    /// `<doc-id>.ops`, as a delta against the previous frame's text or — every
    /// [`KEYFRAME_INTERVAL`] frames, on a tombstone, or right after a (re)open —
    /// a self-contained keyframe. The frame carries the per-op metadata
    /// (`meta`); right after the append a matching row is inserted into the
    /// regenerable `op_history` index so it stays current without a full replay
    /// (`op-log-no-oplog-db` / `changes-query-api`). Advances the in-memory delta
    /// tracking. Per `op-log-accepted-op-retention`.
    ///
    /// `spec` names the frame (`op_id`), its materialized content + tombstone,
    /// its timestamp, and the self-describing author/op-kind/surface/session/
    /// batch/durable metadata. The index row's `content_hash` is computed here
    /// from `spec.text`, so the incremental insert matches a full replay's
    /// reconstruction exactly.
    ///
    /// status: op-log-accepted-op-retention
    /// status: op-log-no-oplog-db
    pub(super) fn retain_frame(
        oplog_dir: &std::path::Path,
        index: &Connection,
        doc_id: &str,
        state: &mut DocState,
        spec: &super::store::FrameSpec<'_>,
    ) -> Result<(), Error> {
        let keyframe = state.last_retained_text.is_none()
            || state.deltas_since_keyframe >= KEYFRAME_INTERVAL
            || spec.tombstone;
        let frame = if keyframe {
            super::store::RetainedOp::keyframe(spec)?
        } else {
            let prev = state.last_retained_text.as_deref().unwrap_or_default();
            super::store::RetainedOp::delta(spec, prev)?
        };
        super::store::append_op(oplog_dir, doc_id, &frame)?;
        // Keep the regenerable query-index current: append the matching row.
        let hash = super::content_hash(spec.text);
        let meta = spec.meta;
        super::meta::insert_history(
            index,
            &super::meta::HistoryRow {
                doc_id,
                op_id: spec.op_id,
                author_wire: meta.author,
                op_kind: meta.op_kind,
                rename_from: meta.rename_from,
                timestamp_ms: spec.timestamp_ms,
                content_hash: &hash,
                surface: meta.surface,
                session_id: meta.session_id,
                batch_id: meta.batch_id,
                metadata: meta.metadata,
            },
        )?;
        state.deltas_since_keyframe = if keyframe { 0 } else { state.deltas_since_keyframe + 1 };
        state.last_retained_text = Some(spec.text.to_string());
        Ok(())
    }

    /// Reconstruct the materialized text of frame `idx` by decoding from the
    /// nearest preceding keyframe forward (each delta uses the running text as
    /// its dictionary). Per `op-log-history-materialization`.
    fn reconstruct_frame(frames: &[super::store::RetainedOp], idx: usize) -> Result<String, Error> {
        let mut start = idx;
        while start > 0 && !frames[start].is_keyframe() {
            start -= 1;
        }
        let mut text = frames[start].decode("")?;
        for frame in &frames[start + 1..=idx] {
            text = frame.decode(&text)?;
        }
        Ok(text)
    }
}
