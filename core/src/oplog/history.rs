//! Accepted-op history: the per-doc `<doc-id>.ops` retention log and the
//! point-in-time reconstruction it backs (`materialize_at`). Each frame is a
//! keyframe (full text) or a delta against the previous frame (per
//! `op-log.md`'s retention section), so the log stays small while any past
//! version reconstructs by decoding from the nearest keyframe forward. This is
//! a second `impl OpLog` block kept here so `mod.rs` stays within its
//! file-length budget; it shares the same lock / `ensure_loaded` machinery
//! defined alongside `OpLog` in `mod.rs`.

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
    /// the live `accepted` Doc and never decodes Yrs on the read path, so there
    /// is no transaction-aliasing risk.
    ///
    /// status: op-log-history-materialization
    pub fn materialize_at(&self, doc_id: &str, op_id: &str) -> Result<Option<DocContent>, Error> {
        // File read outside the lock — the frames are immutable history and
        // the live `accepted` Doc is never touched by reconstruction.
        let frames = super::store::load_ops(&self.oplog_dir, doc_id)?;
        let Some(idx) = frames.iter().position(|f| f.op_id == op_id) else {
            return Ok(None);
        };
        let text = Self::reconstruct_frame(&frames, idx)?;
        Ok(Some(DocContent { text, tombstone: frames[idx].tombstone }))
    }

    /// Append a history frame for an accepted op to `<doc-id>.ops`, as a delta
    /// against the previous frame's text or — every [`KEYFRAME_INTERVAL`]
    /// frames, on a tombstone, or right after a (re)open — a self-contained
    /// keyframe. Advances the in-memory delta tracking. Per
    /// `op-log-accepted-op-retention`.
    pub(super) fn retain_frame(
        oplog_dir: &std::path::Path,
        doc_id: &str,
        state: &mut DocState,
        op_id: String,
        text: &str,
        tombstone: bool,
        timestamp_ms: i64,
    ) -> Result<(), Error> {
        let keyframe = state.last_retained_text.is_none()
            || state.deltas_since_keyframe >= KEYFRAME_INTERVAL
            || tombstone;
        let frame = if keyframe {
            super::store::RetainedOp::keyframe(op_id, text, tombstone, timestamp_ms)?
        } else {
            let prev = state.last_retained_text.as_deref().unwrap_or_default();
            super::store::RetainedOp::delta(op_id, text, prev, tombstone, timestamp_ms)?
        };
        super::store::append_op(oplog_dir, doc_id, &frame)?;
        state.deltas_since_keyframe = if keyframe { 0 } else { state.deltas_since_keyframe + 1 };
        state.last_retained_text = Some(text.to_string());
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
