//! Path-level write helpers — `rename` and `tombstone` — surfaced from
//! the layered editing model. The filesystem half of these operations stays
//! the indexer's concern; this module records the *logical* layered-doc half
//! (the in-memory lifecycle state) alongside it.
//!
//! Pulled out of `crate::ops::op_writes` so the indexer can call these
//! directly without a sibling-module dependency on `ops`. Body-level
//! writes (the larger `op_writes` surface) stay in `ops` where they're
//! grouped with user/agent save flows.
//!
//! status: op-log-ops-producer-helpers
//! status: op-log-path-rename
//! status: op-log-tombstone

use super::{error::Error as SubstrateError, shapes::Author, LayeredDoc};
use crate::errors::HikerError;

fn map_err(e: SubstrateError) -> HikerError {
    use SubstrateError as E;
    match e {
        E::UnknownDoc(d) => HikerError::NotFound(format!("layered doc {d}")),
        E::UnknownPath(p) => HikerError::NotFound(format!("layered-doc path {p}")),
        E::UnknownPendingOp(op) => HikerError::NotFound(format!("layered-doc pending op {op}")),
        E::Anchor(msg) => HikerError::NotFound(format!("layered-doc anchor: {msg}")),
        other => HikerError::Io(other.to_string()),
    }
}

/// Tombstone a document on delete: resolve `rel` to its doc_id and record
/// the logical delete in the layered doc. The filesystem move-to-trash stays
/// the caller's concern (the indexer task owns it). No-op (returns `Ok`)
/// when the path has no doc — a never-seeded note.
pub fn tombstone(log: &LayeredDoc, rel: &str, author: &Author) -> Result<(), HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(());
    };
    log.tombstone_document(&doc_id, author).map_err(map_err)
}

/// Rename a document: resolve `from` to its doc_id and record the logical
/// rename (repointing `doc-index.db` and `meta.path`). The filesystem
/// rename stays the caller's concern. No-op when `from` has no doc.
pub fn rename(log: &LayeredDoc, from: &str, to: &str, author: &Author) -> Result<(), HikerError> {
    let Some(doc_id) = log.doc_id_for_path(from).map_err(map_err)? else {
        return Ok(());
    };
    log.rename_document(&doc_id, to, author).map_err(map_err)
}

/// Restore a previously-tombstoned document, recovering its history. Rebinds
/// `path → doc_id` to the retained `doc_id` and clears the tombstone (per
/// `op-log.md`'s "Offline delete and rename"). The filesystem move out of
/// trash stays the caller's concern; this records the logical resurrection so
/// the note comes back with its full change history rather than as a fresh
/// import. The caller supplies the `doc_id` from the trash manifest entry,
/// since the path mapping may have been repointed away while the doc was gone.
///
/// status: vault-trash-restore
pub fn restore(log: &LayeredDoc, doc_id: &str, path: &str, author: &Author) -> Result<(), HikerError> {
    log.restore_document(doc_id, path, author).map_err(map_err)
}
