//! Vault-level orchestration ops shared by every adapter (app, CLI,
//! MCP). Each op owns the full sequence around a mutating action:
//! pre-suppress watcher paths → enumerate vault/trash members → send the
//! relevant `IndexJob` → await its oneshot reply → re-suppress for the TTL
//! window. Adapters call one function and translate the result.
//!
//! Why this lives in `core::ops` rather than as methods on `IndexerHandle`:
//! the orchestration spans `Watcher` + `IndexerHandle` + `Vault` + `Trash`,
//! and picking one to host the rest is dishonest. Free functions take borrows
//! of whichever handles they need.
//!
//! Senders, not handles. Each op takes an `&IndexJobTx` (the auto-pending-
//! tracking sender wrapper returned by `IndexerHandle::job_sender()`). This
//! matches what callers already do — clone a sender under whatever session
//! lock they hold, drop the lock before `.await`. Passing `&IndexerHandle`
//! would invite holding the handle across the await; the sender form makes
//! the constraint explicit.
//!
//! Watcher suppression. Indexer-side handlers call `crate::vault::*` with
//! `watcher: None` (see `IndexJob::{Move, MoveFolder, DeleteNote,
//! RestoreFromTrash}` in `core::indexer`). Suppression is therefore solely
//! the ops layer's job: pre-suppress before the job runs, re-suppress after
//! it completes so the TTL window starts close to when notify will surface
//! its events.
//!
//! Module layout. The ops are grouped by caller type:
//!
//! - [`file_ops`] — user-driven file mutations (`create_with_suffix`,
//!   `move_note`, `move_folder`, `delete`, `restore`). These are the verbs
//!   exposed by the file tree, the cluster-editor's apply path, and the
//!   delete/restore flows.
//! - [`agent_ops`] — MCP-routed writes (`agent_write_note`,
//!   `agent_set_frontmatter`, `agent_apply_tag`, `agent_remove_tag`) +
//!   `AgentWriteCtx`. Author the changelog row as `agent:<client_id>` and
//!   ride the staging path when review mode is on.
//! - [`buffer_ops`] — editor-buffer lifecycle (`open_for_edit`,
//!   `commit_buffer`, `resolve_drift`, `ensure_note_id_stamped`) + the
//!   `BufferToken` family of types. Owns the drift-check policy and the
//!   id-stamping path that user-driven waypoint creation rides.
//!
//! Every entry from the prior `core::ops` flat module re-exports here so
//! `hiker_core::ops::FOO` continues to resolve unchanged.

use std::sync::Arc;

use crate::changes::{ChangeAppend, Changes};
use crate::vault::Vault;

mod agent_ops;
mod buffer_ops;
mod file_ops;

pub use agent_ops::{
    agent_apply_tag, agent_remove_tag, agent_set_frontmatter, agent_write_note, AgentWriteCtx,
};
pub use buffer_ops::{
    commit_buffer, ensure_note_id_stamped, open_for_edit, resolve_drift, BufferToken,
    CommitOutcome, DriftChoice, DriftResolution, OpenForEditOutcome,
};
pub use file_ops::{create_with_suffix, delete, move_folder, move_note, restore};

/// Read a file's bytes for inclusion in a changelog row. Best-effort: if the
/// file vanishes mid-op or read fails, return `None` and the row is appended
/// without a content blob. Better to log a hash-less row than to abort the
/// mutation that already succeeded on disk.
pub(super) fn read_for_changelog(vault: &Vault, rel: &str) -> Option<Vec<u8>> {
    let abs = vault.abs_path(rel).ok()?;
    std::fs::read(abs).ok()
}

pub(super) fn append_change_best_effort(
    changes: Option<&Arc<Changes>>,
    append: ChangeAppend<'_>,
) {
    if let Some(c) = changes
        && let Err(e) = c.append(append)
    {
        tracing::warn!(error = %e, "changes: append failed");
    }
}

#[cfg(test)]
mod tests;
