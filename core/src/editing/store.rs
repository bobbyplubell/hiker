//! On-disk layout for the layered doc under `<vault>/.hiker/editing/`:
//!
//! ```text
//! <path>.pending     JSON-serialized Vec<PendingOp>
//! ```
//!
//! The `.ops` per-document history engine (zstd keyframe+delta frames, the
//! `op_history` query-index) is GONE (`hiker-core-rework-plan.md` WS1): local
//! history is now the plain-file snapshots under `.hiker/history/` (`core::snapshot`)
//! and git when integrated. The only durable per-document state this module
//! still owns is the un-accepted `.pending` queue; the document's `accepted`
//! content is the canonical `.md` on disk itself.
//!
//! The per-document `.pending` files are keyed by the document's vault-relative
//! path (`op-log-path-identity`): the path IS the document id, so the files
//! mirror the vault tree under the editing dir (`<editing>/notes/foo.md.pending`
//! for the document at `notes/foo.md`). A rename moves the path-keyed file to
//! its new location (`op-log-observed-move`). Parent directories are created on
//! demand before each write, and the directory scans that enumerate documents
//! walk the tree recursively, reconstructing the path from the nested filename.
//!
//! `accepted` is plain TEXT (no CRDT): the canonical `.md` on disk IS the
//! current accepted content (`op-log-disk-canonical`), so [`load_accepted`]
//! reads it straight off the file — there is no separate serialized history.
//! The `.pending` queue is written write-temp-then-rename + fsync so a crash
//! mid-write leaves either the prior file or no change — never a half-written one.
//
// status: op-log-path-identity
// status: op-log-observed-move
// status: op-log-store-layout
// status: op-log-atomic-write
// status: op-log-pending-survives-restart
// status: op-log-disk-canonical

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::error::Error;
use super::shapes::PendingOp;

/// Absolute path to a path-keyed per-document file with extension `ext`
/// (`pending`). The document id IS the vault path (`op-log-path-identity`), so
/// the file mirrors the vault tree under the editing dir: doc `notes/foo.md` →
/// `<editing>/notes/foo.md.<ext>`. Joining the `doc_id` as a relative path
/// preserves the nested directory structure.
pub(super) fn doc_file_path(editing_dir: &Path, doc_id: &str, ext: &str) -> PathBuf {
    let mut p = editing_dir.join(doc_id);
    let name = match p.file_name() {
        Some(n) => format!("{}.{ext}", n.to_string_lossy()),
        None => format!(".{ext}"),
    };
    p.set_file_name(name);
    p
}

/// Reconstruct a document's vault-relative path (its id) from a path-keyed
/// per-document file under `editing_dir`. The file is `<layered>/<path>.<ext>`, so
/// the doc id is the file's path relative to `editing_dir` with the trailing
/// `.<ext>` stripped. Returns `None` when `file` isn't under `editing_dir` or
/// doesn't end in `.<ext>`.
pub(super) fn doc_id_from_file(editing_dir: &Path, file: &Path, ext: &str) -> Option<String> {
    let rel = file.strip_prefix(editing_dir).ok()?;
    let rel = rel.to_str()?;
    let suffix = format!(".{ext}");
    let stem = rel.strip_suffix(&suffix)?;
    Some(stem.to_string())
}

/// Ensure the parent directory of `path` exists so a path-keyed write into a
/// nested vault subtree (`<layered>/notes/foo.md.pending`) doesn't fail on a
/// missing intermediate dir.
fn ensure_parent(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Absolute path to the `<path>.pending` queue file.
pub(super) fn pending_path(editing_dir: &Path, doc_id: &str) -> PathBuf {
    doc_file_path(editing_dir, doc_id, "pending")
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
    editing_dir: &Path,
    doc_id: &str,
    pending: &[PendingOp],
) -> Result<(), Error> {
    let path = pending_path(editing_dir, doc_id);
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
/// rather than failing the document open: pending ops are local, non-synced
/// editorial state, so an unreadable queue costs at most some un-reviewed
/// proposals — never document content, which lives in the canonical `.md`. The
/// unreadable bytes are set aside as `<doc-id>.pending.corrupt` (instead of
/// being overwritten by the next save) so the proposals are recoverable by
/// hand and the loss is visible on disk, not just in a log line.
///
/// status: op-log-pending-survives-restart
pub(super) fn load_pending(editing_dir: &Path, doc_id: &str) -> Result<Vec<PendingOp>, Error> {
    let path = pending_path(editing_dir, doc_id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&path)?;
    match serde_json::from_slice(&bytes) {
        Ok(ops) => Ok(ops),
        Err(e) => {
            let corrupt = path.with_extension("pending.corrupt");
            match fs::rename(&path, &corrupt) {
                Ok(()) => tracing::warn!(doc_id, error = %e,
                    "layered: unreadable .pending queue; un-reviewed proposals \
                     set aside as .pending.corrupt and queue reset to empty"),
                Err(re) => tracing::warn!(doc_id, error = %e, rename_error = %re,
                    "layered: unreadable .pending queue; treating as empty \
                     (could not set the bytes aside — next save overwrites)"),
            }
            Ok(Vec::new())
        }
    }
}

/// Load a document's current accepted `(text, tombstone)` from the canonical
/// `.md` on disk. The path IS the doc id, and the on-disk file IS the accepted
/// content (`op-log-disk-canonical`), so this reads `<vault>/<doc-id>` directly.
/// `Ok(None)` when the file is absent (a brand-new / unknown / deleted doc) or
/// is not valid UTF-8 (an unreadable doc the caller must seed/skip, not crash
/// on). `tombstone` is always `false` here — a tombstone is in-memory lifecycle
/// state on an open doc; a doc whose `.md` is gone simply reads as unknown.
///
/// status: op-log-disk-canonical
/// status: op-log-materialization
pub(super) fn load_accepted(
    editing_dir: &Path,
    doc_id: &str,
) -> Result<Option<(String, bool)>, Error> {
    let abs = super::vault_root_of(editing_dir).join(doc_id);
    match fs::read(&abs) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(Some((text, false))),
            Err(_) => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Recursively walk `editing_dir` and collect the document id (reconstructed
/// vault-relative path) of every per-document file ending in `.<ext>`. The
/// directory mirrors the vault tree (`op-log-store-layout`), so a flat
/// `read_dir` no longer enumerates documents — the scan must descend. Only the
/// `.pending` per-document files live here now (the `.ops` engine is retired),
/// so this enumerates docs that have a queued pending edit.
///
/// status: op-log-store-layout
pub(super) fn scan_doc_ids(editing_dir: &Path, ext: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    scan_doc_ids_into(editing_dir, editing_dir, ext, &mut out)?;
    Ok(out)
}

fn scan_doc_ids_into(
    editing_dir: &Path,
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
            scan_doc_ids_into(editing_dir, &path, ext, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext)
            && let Some(doc_id) = doc_id_from_file(editing_dir, &path, ext)
        {
            out.push(doc_id);
        }
    }
    Ok(())
}

/// Relocate the per-document `.pending` file for `from` to `to` on a rename —
/// the document id IS the path, so a rename moves the queue file to its new
/// path-keyed location (`op-log-observed-move`). The canonical `.md` itself is
/// moved by the caller. A missing source file is fine (a doc with no pending
/// queue); parent directories at the destination are created on demand.
///
/// status: op-log-observed-move
pub(super) fn move_doc_files(editing_dir: &Path, from: &str, to: &str) -> Result<(), Error> {
    let src = doc_file_path(editing_dir, from, "pending");
    if !src.exists() {
        return Ok(());
    }
    let dst = doc_file_path(editing_dir, to, "pending");
    ensure_parent(&dst)?;
    fs::rename(&src, &dst)?;
    Ok(())
}

/// Remove a document's per-doc substrate files (the `.pending` queue). Used by
/// [`LayeredDoc::forget_document`](super::LayeredDoc::forget_document) to untrack a doc
/// entirely. The on-disk `.md` / `.txt` is NOT touched — only layered editing model
/// is dropped — and a missing file is not an error (idempotent).
pub(super) fn remove_doc_files(editing_dir: &Path, doc_id: &str) -> Result<(), Error> {
    let p = doc_file_path(editing_dir, doc_id, "pending");
    if p.exists() {
        fs::remove_file(&p)?;
    }
    Ok(())
}
