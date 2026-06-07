//! On-disk layout for the op log under `<vault>/.hiker/oplog/`:
//!
//! ```text
//! <path>.ops         per-document history frames (zstd keyframe + delta,
//!                    self-describing: author/op-kind/surface/metadata)
//! <path>.pending     JSON-serialized Vec<PendingOp>
//! ```
//!
//! There is no `oplog_meta.db`: the editorial metadata the side table held now
//! rides on each `.ops` frame, and the fast query-index over it is the
//! REGENERABLE `op_history` table in the vault's `index.db` (see `meta`),
//! rebuilt by replaying frames (`op-log-no-oplog-db`).
//!
//! The per-document files are keyed by the document's vault-relative path
//! (`op-log-path-identity`): the path IS the document id, so the files mirror
//! the vault tree under the oplog dir (`<oplog>/notes/foo.md.ops` for the
//! document at `notes/foo.md`). A rename moves the path-keyed files to their
//! new location (`op-log-observed-move`). Parent directories are created on
//! demand before each write, and the directory scans that enumerate documents
//! walk the tree recursively, reconstructing the path from the nested filename.
//!
//! `accepted` is plain TEXT (no CRDT): the newest `.ops` frame's materialized
//! text IS the current accepted content, so the `.ops` log is the document's
//! sole durable representation — there is no separate serialized-CRDT base.
//! The `.pending` queue is written write-temp-then-rename + fsync so a crash
//! mid-write leaves either the prior file or no change — never a half-written
//! one; the `.ops` log is append-only with torn-trailing-frame tolerance.
//
// status: op-log-path-identity
// status: op-log-observed-move
// status: op-log-store-layout
// status: op-log-atomic-write
// status: op-log-pending-survives-restart
// status: op-log-accepted-op-retention

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

/// The per-op editorial metadata a `.ops` frame carries so the history is
/// **self-describing** — everything the regenerable `op_history` index needs to
/// rebuild a row by replay (`op-log-no-oplog-db`). Borrowed at write time
/// ([`RetainedOp::keyframe`] / [`RetainedOp::delta`]) so the commit path doesn't
/// pre-allocate strings. `content_hash` is NOT carried — it's blake3 of the
/// frame's materialized text, recomputed on replay (derivable, so not stored).
///
/// status: op-log-no-oplog-db
/// status: op-log-accepted-op-retention
pub(super) struct FrameMeta<'a> {
    /// Wire form of the [`super::shapes::Author`] (`Author::as_wire`).
    pub author: &'a str,
    /// Wire form of the [`super::shapes::OpKind`] (`OpKind::as_str`).
    pub op_kind: &'a str,
    /// `Rename { from }`'s prior path; `None` for every other op-kind.
    pub rename_from: Option<&'a str>,
    pub surface: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub batch_id: Option<&'a str>,
    /// The durable metadata subset (`durable_metadata` — the bulky pending edit
    /// text already dropped).
    pub metadata: &'a serde_json::Value,
}

/// The full description of one frame to retain: its id, materialized content +
/// tombstone, timestamp, and the self-describing [`FrameMeta`]. Bundled into
/// one borrowed struct so the retain path passes a single value rather than a
/// long positional argument list.
pub(super) struct FrameSpec<'a> {
    pub op_id: &'a str,
    pub text: &'a str,
    pub tombstone: bool,
    pub timestamp_ms: i64,
    pub meta: &'a FrameMeta<'a>,
}

/// One retained accepted-op frame in the per-doc history log (`<doc-id>.ops`).
/// Holds the *materialized content* as of that op (plus the tombstone flag) AND
/// the self-describing per-op metadata (author, op-kind, surface, session/batch
/// ids, durable metadata) — all `materialize_at(op_id)` needs to reconstruct
/// the document at that point AND all the regenerable `op_history` index needs
/// to rebuild a query-row by replay (`op-log-no-oplog-db`). A frame is either a
/// keyframe (`Full`) or a `Delta` against the previous frame; keyframes recur
/// every `KEYFRAME_INTERVAL` frames so reconstruction only ever walks a bounded
/// number of deltas. The newest intact frame's content IS the current
/// `accepted` (this log is the document's sole durable representation), so the
/// log stays linear in content; delta frames cut it further to roughly the size
/// of each edit.
///
/// status: op-log-accepted-op-retention
/// status: op-log-no-oplog-db
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RetainedOp {
    pub op_id: String,
    body: FrameBody,
    pub tombstone: bool,
    pub timestamp_ms: i64,
    /// Author wire string (`Author::as_wire`). `#[serde(default)]` so a frame
    /// written before the self-describing fields existed still loads (its
    /// metadata fields come back empty — a clean pre-1.0 reopen, not a
    /// migration).
    #[serde(default)]
    pub author: String,
    /// Op-kind wire string (`OpKind::as_str`).
    #[serde(default)]
    pub op_kind: String,
    /// `Rename { from }`'s prior path; `None` for every other op-kind.
    #[serde(default)]
    pub rename_from: Option<String>,
    #[serde(default)]
    pub surface: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    /// Durable producer metadata as a JSON STRING (not a `serde_json::Value` —
    /// bincode, the frame's non-self-describing codec, can't encode the
    /// untagged `Value`). Empty string ↔ `Null`/no metadata.
    #[serde(default)]
    pub metadata: String,
}

impl RetainedOp {
    /// Build a self-contained keyframe from a frame spec, compressing the full
    /// text and stamping the self-describing metadata.
    ///
    /// status: op-log-accepted-op-retention
    pub(super) fn keyframe(spec: &FrameSpec<'_>) -> Result<Self, Error> {
        let zstd = zstd::encode_all(spec.text.as_bytes(), HISTORY_ZSTD_LEVEL)?;
        Ok(Self::assemble(FrameBody::Full(zstd), spec))
    }

    /// Build a delta frame from a frame spec: `spec.text` compressed against
    /// `prev_text` (the previous frame's materialized text) as a zstd
    /// dictionary, stamping the self-describing metadata.
    ///
    /// status: op-log-accepted-op-retention
    pub(super) fn delta(spec: &FrameSpec<'_>, prev_text: &str) -> Result<Self, Error> {
        let mut c = zstd::bulk::Compressor::with_dictionary(HISTORY_ZSTD_LEVEL, prev_text.as_bytes())?;
        let zstd = c.compress(spec.text.as_bytes())?;
        let body = FrameBody::Delta { zstd, len: spec.text.len() };
        Ok(Self::assemble(body, spec))
    }

    /// Stamp the spec's id/tombstone/timestamp + self-describing metadata onto
    /// a frame body.
    fn assemble(body: FrameBody, spec: &FrameSpec<'_>) -> Self {
        let meta = spec.meta;
        Self {
            op_id: spec.op_id.to_string(),
            body,
            tombstone: spec.tombstone,
            timestamp_ms: spec.timestamp_ms,
            author: meta.author.to_string(),
            op_kind: meta.op_kind.to_string(),
            rename_from: meta.rename_from.map(str::to_string),
            surface: meta.surface.map(str::to_string),
            session_id: meta.session_id.map(str::to_string),
            batch_id: meta.batch_id.map(str::to_string),
            // Serialize the durable metadata to a JSON string for bincode. A
            // `Null` (the common no-metadata case) becomes an empty string so a
            // frame with no producer metadata carries no extra bytes.
            metadata: match meta.metadata {
                serde_json::Value::Null => String::new(),
                v => serde_json::to_string(v).unwrap_or_default(),
            },
        }
    }

    /// The durable metadata parsed back to an owned JSON value. An empty
    /// `metadata` string (no producer metadata) decodes to `Null`.
    pub(super) fn metadata_value(&self) -> serde_json::Value {
        if self.metadata.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&self.metadata).unwrap_or(serde_json::Value::Null)
        }
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

/// Absolute path to a path-keyed per-document file with extension `ext`
/// (`ops` / `pending`). The document id IS the vault path
/// (`op-log-path-identity`), so the file mirrors the vault tree under the
/// oplog dir: doc `notes/foo.md` → `<oplog>/notes/foo.md.<ext>`. Joining the
/// `doc_id` as a relative path preserves the nested directory structure.
pub(super) fn doc_file_path(oplog_dir: &Path, doc_id: &str, ext: &str) -> PathBuf {
    let mut p = oplog_dir.join(doc_id);
    let name = match p.file_name() {
        Some(n) => format!("{}.{ext}", n.to_string_lossy()),
        None => format!(".{ext}"),
    };
    p.set_file_name(name);
    p
}

/// Reconstruct a document's vault-relative path (its id) from a path-keyed
/// per-document file under `oplog_dir`. The file is `<oplog>/<path>.<ext>`, so
/// the doc id is the file's path relative to `oplog_dir` with the trailing
/// `.<ext>` stripped. Returns `None` when `file` isn't under `oplog_dir` or
/// doesn't end in `.<ext>`.
pub(super) fn doc_id_from_file(oplog_dir: &Path, file: &Path, ext: &str) -> Option<String> {
    let rel = file.strip_prefix(oplog_dir).ok()?;
    let rel = rel.to_str()?;
    let suffix = format!(".{ext}");
    let stem = rel.strip_suffix(&suffix)?;
    Some(stem.to_string())
}

/// Ensure the parent directory of `path` exists so a path-keyed write into a
/// nested vault subtree (`<oplog>/notes/foo.md.ops`) doesn't fail on a missing
/// intermediate dir.
fn ensure_parent(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Absolute path to the `<path>.pending` queue file.
pub(super) fn pending_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    doc_file_path(oplog_dir, doc_id, "pending")
}

/// Write `bytes` to `final_path` via a sibling temp file: create, fsync,
/// rename. The `.pending` (and on-disk `.md`) save discipline (per
/// `op-log-atomic-write`).
///
/// status: op-log-atomic-write
pub(super) fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), Error> {
    ensure_parent(final_path)?;
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
/// lives in `.ops`. Same tolerance as [`load_ops`]' torn-frame handling.
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

/// Absolute path to the `<path>.ops` history log file.
pub(super) fn ops_path(oplog_dir: &Path, doc_id: &str) -> PathBuf {
    doc_file_path(oplog_dir, doc_id, "ops")
}

/// Append one retained-op frame to `<doc-id>.ops`: a `u32-le` length prefix
/// followed by the bincode-encoded [`RetainedOp`], then fsync. Append-only so
/// it never rewrites prior history; a crash mid-append can leave a torn
/// trailing frame, which [`load_ops`] tolerates by stopping at the first
/// short/undecodable frame (the newest *intact* frame stays canonical for
/// *current* state, so at most the in-flight op is at risk — the on-disk `.md`
/// reconciles it as an external edit on reopen).
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
    ensure_parent(&path)?;
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

/// Reconstruct the current accepted `(text, tombstone)` of `doc_id` from its
/// `.ops` history: the NEWEST intact frame is the live accepted state (the log
/// is the document's sole durable representation now that `accepted` is plain
/// text — `op-log-materialization`). Decodes from the nearest preceding
/// keyframe forward, each delta using the running text as its dictionary.
/// `Ok(None)` when the doc has no frames yet (a brand-new / unknown doc).
///
/// status: op-log-accepted-op-retention
/// status: op-log-materialization
pub(super) fn load_accepted(oplog_dir: &Path, doc_id: &str) -> Result<Option<(String, bool)>, Error> {
    let frames = load_ops(oplog_dir, doc_id)?;
    let Some(last) = frames.len().checked_sub(1) else {
        return Ok(None);
    };
    // Walk back to the nearest keyframe, then decode forward to `last`.
    let mut start = last;
    while start > 0 && !frames[start].is_keyframe() {
        start -= 1;
    }
    let mut text = frames[start].decode("")?;
    for frame in &frames[start + 1..=last] {
        text = frame.decode(&text)?;
    }
    Ok(Some((text, frames[last].tombstone)))
}

/// Recursively walk `oplog_dir` and collect the document id (reconstructed
/// vault-relative path) of every per-document file ending in `.<ext>`. The
/// directory mirrors the vault tree (`op-log-store-layout`), so a flat
/// `read_dir` no longer enumerates documents — the scan must descend. No
/// SQLite db lives under the oplog dir anymore (the query-index is in the
/// vault's `index.db`), so only the `.<ext>` per-document files are collected.
///
/// status: op-log-store-layout
pub(super) fn scan_doc_ids(oplog_dir: &Path, ext: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    scan_doc_ids_into(oplog_dir, oplog_dir, ext, &mut out)?;
    Ok(out)
}

fn scan_doc_ids_into(
    oplog_dir: &Path,
    dir: &Path,
    ext: &str,
    out: &mut Vec<String>,
) -> Result<(), Error> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            scan_doc_ids_into(oplog_dir, &path, ext, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext)
            && let Some(doc_id) = doc_id_from_file(oplog_dir, &path, ext)
        {
            out.push(doc_id);
        }
    }
    Ok(())
}

/// Relocate every per-document file for `from` to `to` on a rename — the
/// document id IS the path, so a rename moves the `.ops` / `.pending` files to
/// their new path-keyed location (`op-log-observed-move`). Missing source files
/// are fine (a never-persisted doc, or a partial set); parent directories at
/// the destination are created on demand. Best-effort per file: the whole set
/// relocates so history follows the rename.
///
/// status: op-log-observed-move
pub(super) fn move_doc_files(oplog_dir: &Path, from: &str, to: &str) -> Result<(), Error> {
    for ext in ["ops", "pending"] {
        let src = doc_file_path(oplog_dir, from, ext);
        if !src.exists() {
            continue;
        }
        let dst = doc_file_path(oplog_dir, to, ext);
        ensure_parent(&dst)?;
        fs::rename(&src, &dst)?;
    }
    Ok(())
}
