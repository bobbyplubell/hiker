//! Vault-level orchestration ops shared by every adapter (app, CLI,
//! MCP). Each op owns the full sequence around a mutating action:
//! pre-suppress watcher paths → enumerate vault/trash members → send the
//! relevant `IndexJob` → await its oneshot reply → re-suppress for the TTL
//! window. Adapters call one function and translate the result.
//!
//! Why this lives in `core::ops` rather than as methods on `Handle`:
//! the orchestration spans `Watcher` + `Handle` + `Vault` + `Trash`,
//! and picking one to host the rest is dishonest. Free functions take borrows
//! of whichever handles they need.
//!
//! Senders, not handles. Each op takes an `&IndexJobTx` (the auto-pending-
//! tracking sender wrapper returned by `Handle::job_sender()`). This
//! matches what callers already do — clone a sender under whatever session
//! lock they hold, drop the lock before `.await`. Passing `&Handle`
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
//! - [`file`] — user-driven file mutations (`create_with_suffix`,
//!   `move_note`, `move_folder`, `delete`, `restore`). These are the verbs
//!   exposed by the file tree, the cluster-editor's apply path, and the
//!   delete/restore flows.
//! - [`agent`] — MCP-routed writes (`write_note`,
//!   `set_frontmatter`, `apply_tag`, `remove_tag`) +
//!   `WriteCtx`. Queue as pending layered-doc ops authored `agent:<client_id>`
//!   when review mode is on.
//!
//! Every entry from the prior `core::ops` flat module re-exports here so
//! `hiker_core::ops::FOO` continues to resolve unchanged.

pub mod agent;
pub mod file;
pub mod op_writes;

#[cfg(test)]
mod tests;
