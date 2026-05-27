//! On-disk layout for the op log under `<vault>/.hiker/oplog/`:
//!
//! ```text
//! <doc-id>.yrs       Yrs Doc serialized state (v2 update format, binary)
//! <doc-id>.pending   bincode-serialized Vec<PendingOp>
//! oplog_meta.db      SQLite side table (see `meta`)
//! doc-index.db       SQLite path → doc_id map (see `meta`)
//! ```
//!
//! Both the `.yrs` snapshot and the `.pending` queue are written
//! write-temp-then-rename + fsync so a crash mid-write leaves either the
//! prior file or no change — never a half-written one.
//
// status: op-log-store-layout
// status: op-log-atomic-write
// status: op-log-pending-survives-restart
// status: op-log-compaction

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::Error;
use super::shapes::PendingOp;

/// zstd level for retained history frames. Level 3 is the zstd default — a
/// strong size win on repetitive prose for negligible CPU at the sizes a
/// single note reaches.
const HISTORY_ZSTD_LEVEL: i32 = 3;

/// The body of a retained history frame: either a self-contained keyframe or
/// a delta against the previous frame's text.
///
/// status: op-log-accepted-op-retention
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum FrameBody {
    /// zstd of the full materialized text — a self-contained keyframe.
    Full(Vec<u8>),
    /// The materialized text zstd-compressed using the *previous* frame's text
    /// as the compression dictionary — tiny for an incremental edit, since the
    /// shared prefix/suffix compress to back-references into the dictionary.
    /// `len` is the decompressed byte length (the decoder's output capacity).
    /// Reconstructed by decoding from the nearest preceding keyframe forward,
    /// each step using the running text as the dictionary.
    Delta { zstd: Vec<u8>, len: usize },
}

/// One retained accepted-op frame in the per-doc history log (`<doc-id>.ops`).
/// Holds the *materialized content* as of that op (plus the tombstone flag) —
/// all `materialize_at(op_id)` needs to reconstruct the document at that point.
/// A frame is either a keyframe (`Full`) or a `Delta` against the previous
/// frame; keyframes recur every `KEYFRAME_INTERVAL` frames so reconstruction
/// only ever walks a bounded number of deltas. Storing content (not the full
/// Yrs Doc state, which carries the doc's *entire* op history in every frame)
/// keeps the log linear in content; delta frames cut it further to roughly the
/// size of each edit.
///
/// status: op-log-accepted-op-retention
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RetainedOp {
    pub op_id: String,
    body: FrameBody,
    pub tombstone: bool,
    pub timestamp_ms: i64,
}

impl RetainedOp {
    /// Build a self-contained keyframe from the materialized content as of an
    /// op, compressing the full text.
    ///
    /// status: op-log-accepted-op-retention
    pub(super) fn keyframe(
        op_id: String,
        text: &str,
        tombstone: bool,
        timestamp_ms: i64,
    ) -> Result<Self, Error> {
        let zstd = zstd::encode_all(text.as_bytes(), HISTORY_ZSTD_LEVEL)?;
        Ok(Self { op_id, body: FrameBody::Full(zstd), tombstone, timestamp_ms })
    }

    /// Build a delta frame: `text` compressed against `prev_text` (the previous
    /// frame's materialized text) as a zstd dictionary.
    ///
    /// status: op-log-accepted-op-retention
    pub(super) fn delta(
        op_id: String,
        text: &str,
        prev_text: &str,
        tombstone: bool,
        timestamp_ms: i64,
    ) -> Result<Self, Error> {
        let mut c = zstd::bulk::Compressor::with_dictionary(HISTORY_ZSTD_LEVEL, prev_text.as_bytes())?;
        let zstd = c.compress(text.as_bytes())?;
        let body = FrameBody::Delta { zstd, len: text.len() };
        Ok(Self { op_id, body, tombstone, timestamp_ms })
    }

    /// Whether this frame is a self-contained keyframe (no delta replay needed).
    pub(super) const fn is_keyframe(&self) -> bool {
        matches!(self.body, FrameBody::Full(_))
    }

    /// Decode this frame's materialized text. A keyframe ignores `prev_text`; a
    /// delta decompresses against `prev_text` (which must be the previous
    /// frame's reconstructed text, the same dictionary the encoder used).
    ///
    /// status: op-log-history-materialization
    pub(super) fn decode(&self, prev_text: &str) -> Result<String, Error> {
        let bytes = match &self.body {
            FrameBody::Full(zstd) => zstd::decode_all(zstd.as_slice())?,
            FrameBody::Delta { zstd, len } => {
                let mut d = zstd::bulk::Decompressor::with_dictionary(prev_text.as_bytes())?;
                d.decompress(zstd, *len)?
            }
        };
        String::from_utf8(bytes).map_err(|e| {
            Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
        })
    }
}

/// Absolute path to the `<doc-id>.yrs` snapshot file.
pub(super) fn yrs_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    oplog_dir.join(format!("{doc_id}.yrs"))
}

/// Absolute path to the `<doc-id>.pending` queue file.
pub(super) fn pending_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    oplog_dir.join(format!("{doc_id}.pending"))
}

/// Write `bytes` to `final_path` via a sibling temp file: create, fsync,
/// rename. The `.yrs` and `.pending` save discipline (per `op-log-atomic-write`).
///
/// status: op-log-atomic-write
pub(super) fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let tmp = final_path.with_extension(match final_path.extension() {
        Some(ext) => format!("{}.tmp", ext.to_string_lossy()),
        None => "tmp".to_string(),
    });
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, final_path)?;
    Ok(())
}

/// Persist a Doc's full serialized state to `<doc-id>.yrs`.
///
/// status: op-log-atomic-write
pub(super) fn save_yrs(oplog_dir: &Path, doc_id: &str, bytes: &[u8]) -> Result<(), Error> {
    write_atomic(&yrs_path(oplog_dir, doc_id), bytes)
}

/// Read `<doc-id>.yrs`, or `None` when the file doesn't exist yet.
pub(super) fn load_yrs(oplog_dir: &Path, doc_id: &str) -> Result<Option<Vec<u8>>, Error> {
    let path = yrs_path(oplog_dir, doc_id);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(fs::read(&path)?))
}

/// Persist the pending queue to `<doc-id>.pending` (JSON — self-describing,
/// so the tagged `OpKind` and free-form `metadata` serialize directly). An
/// empty queue removes the file.
///
/// status: op-log-pending-survives-restart
pub(super) fn save_pending(
    oplog_dir: &Path,
    doc_id: &str,
    pending: &[PendingOp],
) -> Result<(), Error> {
    let path = pending_path(oplog_dir, doc_id);
    if pending.is_empty() {
        if path.exists() {
            fs::remove_file(&path)?;
        }
        return Ok(());
    }
    let bytes = serde_json::to_vec(pending)?;
    write_atomic(&path, &bytes)
}

/// Read and decode `<doc-id>.pending`. Missing file → empty queue. This is
/// what reconstitutes pending ops across restarts.
///
/// A `.pending` whose bytes don't parse as JSON is treated as an empty queue
/// (and overwritten on the next save) rather than failing the document open:
/// pending ops are local, non-synced editorial state, so an unreadable queue
/// costs at most some un-reviewed proposals — never document content, which
/// lives in `.yrs`. Same tolerance as [`load_ops`]' torn-frame handling.
///
/// status: op-log-pending-survives-restart
pub(super) fn load_pending(oplog_dir: &Path, doc_id: &str) -> Result<Vec<PendingOp>, Error> {
    let path = pending_path(oplog_dir, doc_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    match serde_json::from_slice(&bytes) {
        Ok(ops) => Ok(ops),
        Err(e) => {
            tracing::warn!(doc_id, error = %e, "oplog: unreadable .pending queue; treating as empty");
            Ok(Vec::new())
        }
    }
}

/// Absolute path to the `<doc-id>.ops` history log file.
pub(super) fn ops_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    oplog_dir.join(format!("{doc_id}.ops"))
}

/// Append one retained-op frame to `<doc-id>.ops`: a `u32-le` length prefix
/// followed by the bincode-encoded [`RetainedOp`], then fsync. Append-only so
/// it never rewrites prior history; a crash mid-append can leave a torn
/// trailing frame, which [`load_ops`] tolerates by stopping at the first
/// short/undecodable frame (the `.yrs` snapshot stays canonical for *current*
/// state, so at most the in-flight op's history granularity is at risk).
///
/// status: op-log-accepted-op-retention
pub(super) fn append_op(oplog_dir: &Path, doc_id: &str, rec: &RetainedOp) -> Result<(), Error> {
    let body = bincode::serialize(rec)?;
    let len = u32::try_from(body.len()).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "retained op frame exceeds u32 length",
        ))
    })?;
    let path = ops_path(oplog_dir, doc_id);
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(&len.to_le_bytes())?;
    f.write_all(&body)?;
    f.sync_all()?;
    Ok(())
}

/// Read every complete frame from `<doc-id>.ops` in append order. Missing
/// file → empty. Stops at the first frame whose declared length runs past the
/// remaining bytes or fails to decode (a torn trailing frame from a crash
/// mid-append), returning the frames read so far.
///
/// status: op-log-accepted-op-retention
pub(super) fn load_ops(oplog_dir: &Path, doc_id: &str) -> Result<Vec<RetainedOp>, Error> {
    let path = ops_path(oplog_dir, doc_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
        ]) as usize;
        let start = pos + 4;
        let end = start + len;
        if end > bytes.len() {
            break; // torn trailing frame
        }
        match bincode::deserialize::<RetainedOp>(&bytes[start..end]) {
            Ok(rec) => out.push(rec),
            Err(_) => break, // corrupt trailing frame
        }
        pos = end;
    }
    Ok(out)
}

/// Absolute path to the `<doc-id>.yrslog` incremental-update log.
pub(super) fn yrslog_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    oplog_dir.join(format!("{doc_id}.yrslog"))
}

/// Append one Yrs delta frame to `<doc-id>.yrslog`: a `u32-le` length prefix
/// followed by the v2-update bytes, then fsync. Append-only, so a commit costs
/// O(edit) not O(doc size) — the full `.yrs` base is only rewritten on
/// compaction. Same framing + torn-trailing-frame discipline as [`append_op`];
/// a crash mid-append loses at most the in-flight commit's delta from the log
/// (the `.md` stays canonical, so it reconciles as an external edit on reopen).
///
/// status: op-log-yrs-delta-log
pub(super) fn append_yrslog(oplog_dir: &Path, doc_id: &str, frame: &[u8]) -> Result<(), Error> {
    let len = u32::try_from(frame.len()).map_err(|_| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "yrs delta frame exceeds u32 length",
        ))
    })?;
    let path = yrslog_path(oplog_dir, doc_id);
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(&len.to_le_bytes())?;
    f.write_all(frame)?;
    f.sync_all()?;
    Ok(())
}

/// Read every complete delta frame from `<doc-id>.yrslog` in append order.
/// Missing file → empty. Stops at the first frame whose declared length runs
/// past the remaining bytes (a torn trailing frame from a crash mid-append),
/// returning the frames read so far — the `.yrs` base + the intact frames stay
/// canonical.
///
/// status: op-log-yrs-delta-log
pub(super) fn load_yrslog(oplog_dir: &Path, doc_id: &str) -> Result<Vec<Vec<u8>>, Error> {
    let path = yrslog_path(oplog_dir, doc_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let start = pos + 4;
        let end = start + len;
        if end > bytes.len() {
            break; // torn trailing frame
        }
        out.push(bytes[start..end].to_vec());
        pos = end;
    }
    Ok(out)
}

/// Remove the `<doc-id>.yrslog` after its deltas have been folded back into the
/// `.yrs` base by compaction. A missing file is fine (nothing to clear).
///
/// status: op-log-yrs-delta-log
pub(super) fn clear_yrslog(oplog_dir: &Path, doc_id: &str) -> Result<(), Error> {
    let path = yrslog_path(oplog_dir, doc_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

/// Whether the `.yrs` base + its `.yrslog` deltas have grown past `threshold ×`
/// the materialized content size and should be folded into a fresh compact
/// `.yrs` snapshot (and the log cleared). Checked on open. A missing/empty
/// materialization with a non-trivially large on-disk size still triggers, but
/// a freshly-seeded empty doc (under the 1 KiB floor) does not.
///
/// status: op-log-compaction
pub(super) fn needs_compaction(oplog_dir: &Path, doc_id: &str, materialized_len: usize, threshold: f32) -> bool {
    let yrs_len = fs::metadata(yrs_path(oplog_dir, doc_id)).map(|m| m.len()).unwrap_or(0);
    let log_len = fs::metadata(yrslog_path(oplog_dir, doc_id)).map(|m| m.len()).unwrap_or(0);
    let file_len = (yrs_len + log_len) as f64;
    // A small floor: never compact under 1 KiB — the rewrite cost isn't worth
    // it and the ratio math is noisy at tiny sizes.
    if file_len < 1024.0 {
        return false;
    }
    let base = (materialized_len as f64).max(1.0);
    file_len > base * threshold as f64
}
