//! Per-job handlers + the ingest pipeline. Driven by the loop in
//! `super::scheduler`. Everything here is `pub(super)` — these helpers are
//! the indexer's internal seam, not public API.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use tokio::sync::{broadcast, watch, OnceCell};

use crate::chunker::Chunker;
use crate::chunker::markdown::Markdown;
use crate::chunker::txt::Txt;
use crate::embed::{Embedder, Error as EmbedError, FastembedEmbedder};
use crate::hash_string;
use crate::store::dto::{new_id, MetaEntry, NoteUpsert};
use crate::store::Store;
use crate::watcher::is_ignored;

use super::{
    frontmatter_hiker_id, now_secs, path_extension, submit_embedder_load_task, update_status,
    update_total_notes, IndexJob, IndexJobTx, IndexStatus, Error, ProgressEvent,
    MAX_FILE_BYTES,
};

/// Borrow-bundle of the long-lived handles every indexer-job handler
/// in this module reads from. The writer `Store` is owned by the
/// scheduler loop and threaded through as a separate `&mut Store`
/// because none of the other handles need exclusive access; bundling
/// them here trims the per-handler signatures from 10+ args down to
/// `(&JobCtx, &mut Store, job-specific args)`.
pub(super) struct JobCtx<'a> {
    pub vault: &'a crate::vault::Vault,
    pub vault_root: &'a Path,
    pub embedder: &'a Arc<dyn Embedder>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub status: &'a watch::Sender<IndexStatus>,
    pub pending: &'a Arc<Mutex<HashSet<String>>>,
    pub self_tx: &'a IndexJobTx,
    pub watcher_cell: &'a Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    pub oplog_cell: &'a Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
}

/// Subset of `JobCtx` used by handlers that don't need the trails /
/// watcher plumbing — pure per-file ingest.
pub(super) struct UpsertCtx<'a> {
    pub vault_root: &'a Path,
    pub embedder: &'a Arc<dyn Embedder>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub status: &'a watch::Sender<IndexStatus>,
}

impl<'a> JobCtx<'a> {
    /// Narrow the job-level borrow bundle to the subset that pure-ingest
    /// helpers (`handle_upsert`, `handle_restore_from_trash`,
    /// `handle_inline_upsert`) actually need.
    pub(super) fn as_upsert_ctx(&self) -> UpsertCtx<'a> {
        UpsertCtx {
            vault_root: self.vault_root,
            embedder: self.embedder,
            progress: self.progress,
            status: self.status,
        }
    }

    async fn handle_upsert(&self, store: &mut Store, rel_path: String, force: bool) {
        let progress = self.progress;
        let status = self.status;
        let pending = self.pending;
        // Make sure the path is in the pending set even if it didn't go
        // through a tracking sender (e.g. enqueued by some legacy path);
        // remove on every terminal outcome below.
        pending.lock().unwrap().insert(rel_path.clone());
        let _ = progress.send(ProgressEvent::Started { path: rel_path.clone() });
        let outcome = process_upsert(self.vault_root, store, self.embedder.clone(), &rel_path, force).await;
        pending.lock().unwrap().remove(&rel_path);
        match outcome {
            Ok(UpsertOutcome::Indexed) => {
                tracing::debug!(path = %rel_path, "indexer: file indexed");
                let _ = progress.send(ProgressEvent::Finished { path: rel_path });
            }
            Ok(UpsertOutcome::Unchanged) => {
                let _ = progress.send(ProgressEvent::Skipped {
                    path: rel_path,
                    reason: "unchanged".into(),
                });
            }
            Ok(UpsertOutcome::Skipped(reason)) => {
                tracing::debug!(
                    path = %rel_path,
                    reason = %reason,
                    "indexer: file skipped",
                );
                let _ = progress.send(ProgressEvent::Skipped {
                    path: rel_path,
                    reason,
                });
            }
            Err(e) => {
                let msg = format!("{e}");
                tracing::error!(error = %e, path = %rel_path, "indexer: upsert failed");
                update_status(status, |s| s.last_error = Some(msg.clone()));
                let _ = progress.send(ProgressEvent::Error {
                    path: Some(rel_path),
                    message: msg,
                });
            }
        }
    }

    async fn handle_rename(&self, store: &mut Store, from: String, to: String) {
        let vault = self.vault;
        let progress = self.progress;
        let status = self.status;
        let self_tx = self.self_tx;
        let watcher_cell = self.watcher_cell;
        // Inline `process_rename` (used only here): rename the stored path
        // when an id is present, else return `Ok(false)` so the caller can
        // treat the destination as a new upsert.
        let process_result: Result<bool, Error> = (|| {
            let id = match store.id_for_path(&from)? {
                Some(id) => id,
                None => return Ok(false),
            };
            store.rename_note(&id, &to)?;
            Ok(true)
        })();
        match process_result {
            Ok(true) => {
                let _ = progress.send(ProgressEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                });
                // status: trail-auto-update-on-note-move
                // Watcher-driven external rename: run the trails update.
                run_trails_on_note_moved(
                    watcher_cell, self_tx, vault, store, &from, &to,
                )
                .await;
                // status: board-card-references
                run_boards_on_note_moved(
                    watcher_cell, self.oplog_cell, self_tx, vault, store, &from, &to,
                )
                .await;
            }
            Ok(false) => {
                let upsert_ctx = self.as_upsert_ctx();
                if let Err(e) = handle_inline_upsert(&upsert_ctx, store, &to).await {
                    let msg = format!("{e}");
                    update_status(status, |s| s.last_error = Some(msg.clone()));
                    let _ = progress.send(ProgressEvent::Error {
                        path: Some(to),
                        message: msg,
                    });
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                update_status(status, |s| s.last_error = Some(msg.clone()));
                let _ = progress.send(ProgressEvent::Error {
                    path: Some(to),
                    message: msg,
                });
            }
        }
    }
}

impl<'a> UpsertCtx<'a> {
    async fn handle_restore_from_trash(
        &self,
        vault: &crate::vault::Vault,
        store: &mut Store,
        id: &str,
    ) -> Result<crate::trash::Entry, crate::errors::HikerError> {
        let progress = self.progress;
        let status = self.status;
        let trash = crate::trash::Trash::open(vault.root());
        match crate::vault::restore_note(vault, None, &trash, id) {
            Ok(entry) => {
                // Re-ingest the restored .md files inline so the index
                // picks them up without waiting on watcher events
                // (which the caller suppressed). For folders, walk
                // the manifest's recorded members; for files, just the
                // single original_path.
                let to_index: Vec<String> = match &entry.members {
                    Some(m) => m.clone(),
                    None => vec![entry.original_path.clone()],
                };
                for rel_path in &to_index {
                    if let Err(e) = handle_inline_upsert(self, store, rel_path).await {
                        let msg = format!("{e}");
                        update_status(status, |s| s.last_error = Some(msg.clone()));
                        let _ = progress.send(ProgressEvent::Error {
                            path: Some(rel_path.clone()),
                            message: msg,
                        });
                    }
                }
                Ok(entry)
            }
            Err(e) => {
                let msg = format!("{e}");
                update_status(status, |s| s.last_error = Some(msg.clone()));
                let _ = progress.send(ProgressEvent::Error {
                    path: Some(id.to_string()),
                    message: msg,
                });
                Err(e)
            }
        }
    }
}

/// Handle `IndexJob::ReloadEmbedder` — load a fresh `FastembedEmbedder` for
/// `model_id` on `spawn_blocking`, and on success swap the live embedder
/// (loop-local binding + the shared cell observed by the search-query
/// embedder), reseat `chunk_vecs` to the new dim, and enqueue a full
/// `force = true` reindex so every note re-embeds against the new model.
///
/// Failure semantics (the "load embedder first, write TOML only on
/// success" posture used by `set_setting`): if the load fails, the old
/// embedder stays loaded (loop-local + cell untouched), the reply
/// returns `Err`, and no reindex is enqueued. If the load succeeds but
/// `ensure_chunk_vecs_dim` fails afterward, the new embedder is left in
/// place (we've already committed to it from the caller's perspective)
/// and the dim-rebuild error surfaces through both the reply and
/// `last_error`; the reindex still enqueues so the next run gets
/// another shot. Same-id reloads short-circuit and return Ok without
/// touching anything.
///
/// status: embedder-hot-reload-on-model-change
/// Borrow-bundle for the embedder-reload handler. Methods on this struct
/// stay exempt from `single_call_fn` (called only by the scheduler loop)
/// and split the work across small phases so the orchestrator stays under
/// the cognitive-complexity cap.
pub(super) struct ReloadCtx<'a> {
    pub embedder: &'a mut Arc<dyn Embedder>,
    pub embedder_cell: &'a Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    pub store: &'a mut Store,
    pub status: &'a watch::Sender<IndexStatus>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub self_tx: &'a IndexJobTx,
    pub tasks: Option<&'a Arc<crate::tasks::queue::Queue>>,
}

type ReloadReply = tokio::sync::oneshot::Sender<Result<(), crate::errors::HikerError>>;

type LoadResult = Result<Result<Arc<dyn Embedder>, crate::embed::Error>, tokio::task::JoinError>;

impl<'a> ReloadCtx<'a> {
    /// status: embedder-hot-reload-on-model-change
    pub(super) async fn run(mut self, model_id: String, reply: ReloadReply) {
        // Same model id → no-op. The set_setting caller already short-circuits
        // on identical TOML values, but defensive: a redundant ReloadEmbedder
        // (e.g. from a future MCP path or test) shouldn't tear down chunk_vecs.
        //
        // status: embedder-model-load-as-task
        // The queue submit happens *after* the short-circuit so a redundant
        // reload doesn't create an empty / instantly-complete row.
        if self.embedder.version() == model_id {
            let _ = reply.send(Ok(()));
            return;
        }
        tracing::info!(
            from = self.embedder.version(),
            to = %model_id,
            "indexer: hot-reloading embedder",
        );
        let load_task_id = match self.tasks {
            Some(q) => Some(submit_embedder_load_task(q, &model_id).await),
            None => None,
        };
        let id_for_load = model_id.clone();
        let load: LoadResult = tokio::task::spawn_blocking(move || {
            FastembedEmbedder::load_id(&id_for_load).map(|e| {
                let arc: Arc<dyn Embedder> = Arc::new(e);
                arc
            })
        })
        .await;
        // Resolve the queue row up front so a downstream `ensure_chunk_vecs_dim`
        // failure doesn't leave the row stuck in Leased. Mirror of the startup
        // path's resolve.
        if let (Some(q), Some(id)) = (self.tasks, load_task_id.as_ref()) {
            match &load {
                Ok(Ok(_)) => {
                    let _ = q
                        .submit_result(id, serde_json::json!({ "ok": true }))
                        .await;
                }
                Ok(Err(e)) => {
                    let _ = q.fail(id, format!("embedder reload: {e}")).await;
                }
                Err(e) => {
                    let _ = q.fail(id, format!("embedder reload spawn: {e}")).await;
                }
            }
        }

        let new_embedder = match self.unwrap_load(load, &model_id) {
            Ok(e) => e,
            Err(msg) => {
                let _ = reply.send(Err(crate::errors::HikerError::Io(msg)));
                return;
            }
        };
        let new_dim = new_embedder.dim();
        self.swap_embedder(new_embedder);
        self.finalize_dim_rebuild(new_dim, reply);
        if let Err(e) = self.self_tx.send(IndexJob::FullScan { force: true }).await {
            tracing::warn!(error = %e, "indexer: failed to enqueue post-reload FullScan");
        }
    }

    /// Unpack the spawn_blocking + load result. On the error paths,
    /// reports through status + progress and returns the message string
    /// for the caller to send through `reply`.
    fn unwrap_load(
        &self,
        load: LoadResult,
        model_id: &str,
    ) -> Result<Arc<dyn Embedder>, String> {
        match load {
            Ok(Ok(e)) => Ok(e),
            Ok(Err(e)) => {
                let msg = format!("embedder reload failed: {e}");
                tracing::error!(error = %e, model = %model_id, "indexer: embedder reload failed");
                update_status(self.status, |s| s.last_error = Some(msg.clone()));
                let _ = self.progress.send(ProgressEvent::Error {
                    path: None,
                    message: msg.clone(),
                });
                Err(msg)
            }
            Err(e) => {
                let msg = format!("embedder reload spawn: {e}");
                tracing::error!(error = %e, "indexer: embedder reload spawn failed");
                update_status(self.status, |s| s.last_error = Some(msg.clone()));
                let _ = self.progress.send(ProgressEvent::Error {
                    path: None,
                    message: msg.clone(),
                });
                Err(msg)
            }
        }
    }

    fn swap_embedder(&mut self, new_embedder: Arc<dyn Embedder>) {
        // Swap the live Arc — loop-local first so any same-tick logic uses
        // it, then the shared cell so the search-query embedder picks it up
        // on its next read.
        *self.embedder = new_embedder.clone();
        match self.embedder_cell.write() {
            Ok(mut guard) => *guard = Some(new_embedder),
            Err(_) => tracing::error!("indexer: embedder cell lock poisoned on reload"),
        }
    }

    fn finalize_dim_rebuild(&mut self, new_dim: usize, reply: ReloadReply) {
        // Reseat chunk_vecs to the new dim. Same helper used at indexer
        // startup; drops + recreates the vec0 table and clears
        // notes.embedder_version so the upcoming reindex actually re-embeds.
        if let Err(e) = self.store.ensure_chunk_vecs_dim(new_dim) {
            let msg = format!("chunk_vecs rebuild after model swap: {e}");
            tracing::error!(error = %e, "indexer: ensure_chunk_vecs_dim failed after reload");
            update_status(self.status, |s| s.last_error = Some(msg.clone()));
            let _ = self.progress.send(ProgressEvent::Error {
                path: None,
                message: msg.clone(),
            });
            // The new embedder is already swapped in — the caller has
            // committed to this model. Report the dim-rebuild failure
            // but still enqueue the reindex so we don't leave the queue
            // empty with a half-applied change.
            let _ = reply.send(Err(crate::errors::HikerError::Io(msg)));
        } else {
            update_status(self.status, |s| {
                s.last_error = None;
                s.model_ready = true;
            });
            let _ = self.progress.send(ProgressEvent::ModelLoaded);
            let _ = reply.send(Ok(()));
        }
    }
}

/// Dispatch a single non-FullScan job. Extracted so the FullScan handler can
/// call it directly on its scan results without re-entering the mpsc.
pub(super) async fn handle_simple_job(
    ctx: &JobCtx<'_>,
    store: &mut Store,
    job: IndexJob,
) {
    let vault = ctx.vault;
    let embedder = ctx.embedder;
    let progress = ctx.progress;
    let status = ctx.status;
    let self_tx = ctx.self_tx;
    let watcher_cell = ctx.watcher_cell;
    let _ = embedder; // some arms don't need it directly; per-handler ctxs pull it back in
    match job {
        IndexJob::Upsert { rel_path, force } => {
            ctx.handle_upsert(store, rel_path, force).await;
        }
        IndexJob::Delete { rel_path } => match process_delete(store, &rel_path) {
            Ok(true) => {
                let _ = progress.send(ProgressEvent::Deleted { path: rel_path });
            }
            Ok(false) => {}
            Err(e) => {
                let msg = format!("{e}");
                update_status(status, |s| s.last_error = Some(msg.clone()));
                let _ = progress.send(ProgressEvent::Error {
                    path: Some(rel_path),
                    message: msg,
                });
            }
        },
        IndexJob::Rename { from, to } => {
            ctx.handle_rename(store, from, to).await;
        }
        IndexJob::Move { from, to, reply } => {
            // The caller suppresses the watcher around this; we don't
            // need to here. Run vault::move_note on the indexer's owned
            // store so all writes flow through one connection.
            let result = crate::vault::move_note(vault, store, None, &from, &to);
            // status: trail-auto-update-on-note-move
            // After the path remap succeeds, sweep trails referencing the
            // moved note. Errors are swallowed (logged inside the helper)
            // so a partial trails update never fails the move's reply.
            if result.is_ok() {
                run_trails_on_note_moved(
                    watcher_cell, self_tx, vault, store, &from, &to,
                )
                .await;
                record_oplog_rename(ctx.oplog_cell, &from, &to);
                // status: board-card-references
                run_boards_on_note_moved(
                    watcher_cell, ctx.oplog_cell, self_tx, vault, store, &from, &to,
                )
                .await;
            }
            let _ = reply.send(result);
        }
        IndexJob::MoveFolder { from, to, reply } => {
            // Compute the (old, new) member pairs *before* the rename so
            // we can call on_note_moved per-pair after the rename. Walk
            // failures here are non-fatal: the trails sweep just runs over
            // whatever subset we collected.
            let pre_members = vault.walk_indexable_files(&from).unwrap_or_default();
            let from_prefix = format!("{from}/");
            let pairs: Vec<(String, String)> = pre_members
                .iter()
                .map(|m| {
                    let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
                    (m.clone(), format!("{to}/{suffix}"))
                })
                .collect();

            let result = crate::vault::move_folder(vault, store, None, &from, &to);
            // status: trail-auto-update-on-note-move
            if result.is_ok() {
                for (old, new) in &pairs {
                    run_trails_on_note_moved(
                        watcher_cell, self_tx, vault, store, old, new,
                    )
                    .await;
                    record_oplog_rename(ctx.oplog_cell, old, new);
                    // status: board-card-references
                    run_boards_on_note_moved(
                        watcher_cell, ctx.oplog_cell, self_tx, vault, store, old, new,
                    )
                    .await;
                }
            }
            let _ = reply.send(result);
        }
        IndexJob::DeleteNote { rel, reply } => {
            // Same shape as Move — caller handles watcher suppression
            // around the call. The trash handle is cheap to construct
            // (just a path) so we build one per call rather than threading
            // it through the loop signature.
            let trash = crate::trash::Trash::open(vault.root());
            let result = crate::vault::delete_note(vault, store, None, &trash, &rel);
            match &result {
                Ok(entry) => {
                    record_oplog_tombstone(ctx.oplog_cell, entry);
                    let _ = progress.send(ProgressEvent::Deleted {
                        path: entry.original_path.clone(),
                    });
                }
                Err(e) => {
                    let msg = format!("{e}");
                    update_status(status, |s| s.last_error = Some(msg.clone()));
                    let _ = progress.send(ProgressEvent::Error {
                        path: Some(rel.clone()),
                        message: msg,
                    });
                }
            }
            let _ = reply.send(result);
        }
        IndexJob::RestoreFromTrash { id, reply } => {
            let upsert_ctx = ctx.as_upsert_ctx();
            let result = upsert_ctx.handle_restore_from_trash(vault, store, &id).await;
            let _ = reply.send(result);
        }
        IndexJob::FullScan { .. } => {
            unreachable!("FullScan must be handled by the main loop, not handle_simple_job");
        }
        IndexJob::ReloadEmbedder { .. } => {
            unreachable!(
                "ReloadEmbedder must be handled by the main loop, not handle_simple_job"
            );
        }
        // status: note-access-tracking
        IndexJob::TouchAccess { rel_path, ts } => {
            if let Err(e) = store.touch_note_access(&rel_path, ts) {
                tracing::warn!(
                    error = %e,
                    path = %rel_path,
                    "indexer: touch_note_access failed",
                );
            }
        }
    }
}

/// Run the trails auto-update sweep after a successful path remap. Reads
/// `watcher_cell` so callers don't need to know whether the host has wired
/// it — CLI / tests run without it attached and the sweep degrades to a
/// write-without-suppress shape. Errors are swallowed (logged inside
/// `core::trails::on_note_moved`).
///
/// status: trail-auto-update-on-note-move
async fn run_trails_on_note_moved(
    watcher_cell: &Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    self_tx: &IndexJobTx,
    vault: &crate::vault::Vault,
    store: &mut Store,
    from: &str,
    to: &str,
) {
    let watcher_arc = watcher_cell.get().cloned();
    let watcher_ref = watcher_arc.as_deref();
    if let Err(e) = crate::trails::ops::on_note_moved(
        watcher_ref,
        Some(self_tx),
        vault,
        store,
        from,
        to,
    )
    .await
    {
        tracing::warn!(error = %e, %from, %to,
            "indexer: trails on_note_moved sweep failed");
    }
}

/// Run the boards auto-update sweep after a successful path remap —
/// the board mirror of `run_trails_on_note_moved`. Reads the op-log handle
/// (when wired) so the card-path rewrite rides the user-save path; CLI /
/// tests run without it and the sweep degrades to a suppressed write.
///
/// status: board-card-references
async fn run_boards_on_note_moved(
    watcher_cell: &Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    oplog_cell: &Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
    self_tx: &IndexJobTx,
    vault: &crate::vault::Vault,
    store: &mut Store,
    from: &str,
    to: &str,
) {
    let watcher_arc = watcher_cell.get().cloned();
    let watcher_ref = watcher_arc.as_deref();
    let log_arc = oplog_cell.get().cloned();
    let log_ref = log_arc.as_deref();
    // Routed through the `boards` module root (not `boards::ops`) so the
    // card-path remap is a first-class board concern; the heavy lifting
    // stays in `boards::ops`.
    if let Err(e) = crate::boards::on_note_moved(
        watcher_ref,
        Some(self_tx),
        log_ref,
        vault,
        store,
        from,
        to,
    )
    .await
    {
        tracing::warn!(error = %e, %from, %to,
            "indexer: boards on_note_moved sweep failed");
    }
}

/// Record a rename in the op log so the `doc-index.db` path mapping follows
/// the move (otherwise the moved note's history orphans and its next save
/// seeds a fresh doc) and the history feed sees the rename. Best-effort: no
/// op-log handle (CLI / tests) or an unmapped path is a silent no-op; a
/// failure is logged, not propagated — the filesystem move already succeeded.
fn record_oplog_rename(
    oplog_cell: &Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
    from: &str,
    to: &str,
) {
    let Some(log) = oplog_cell.get() else { return };
    if let Err(e) =
        crate::ops::op_writes::rename(log, from, to, &crate::oplog::shapes::Author::User)
    {
        tracing::warn!(error = %e, %from, %to, "indexer: op-log rename failed");
    }
}

/// Record a tombstone in the op log for a soft-deleted entry — the note, plus
/// every member when a folder was deleted. Same best-effort posture as
/// [`record_oplog_rename`].
fn record_oplog_tombstone(
    oplog_cell: &Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
    entry: &crate::trash::Entry,
) {
    let Some(log) = oplog_cell.get() else { return };
    let paths = match &entry.members {
        Some(members) => members.clone(),
        None => vec![entry.original_path.clone()],
    };
    for path in &paths {
        if let Err(e) =
            crate::ops::op_writes::tombstone(log, path, &crate::oplog::shapes::Author::User)
        {
            tracing::warn!(error = %e, %path, "indexer: op-log tombstone failed");
        }
    }
}

/// Convenience for the rename "from-path-not-indexed" branch: do an upsert
/// without re-emitting Started/Finished pairs.
async fn handle_inline_upsert(
    ctx: &UpsertCtx<'_>,
    store: &mut Store,
    rel_path: &str,
) -> Result<(), Error> {
    let progress = ctx.progress;
    let status = ctx.status;
    let _ = progress.send(ProgressEvent::Started { path: rel_path.to_string() });
    match process_upsert(ctx.vault_root, store, ctx.embedder.clone(), rel_path, false).await? {
        UpsertOutcome::Indexed => {
            let _ = progress.send(ProgressEvent::Finished { path: rel_path.to_string() });
        }
        UpsertOutcome::Unchanged => {
            let _ = progress.send(ProgressEvent::Skipped {
                path: rel_path.to_string(),
                reason: "unchanged".into(),
            });
        }
        UpsertOutcome::Skipped(reason) => {
            let _ = progress.send(ProgressEvent::Skipped {
                path: rel_path.to_string(),
                reason,
            });
        }
    }
    update_total_notes(status, store);
    Ok(())
}

enum UpsertOutcome {
    Indexed,
    Unchanged,
    Skipped(String),
}

/// Flatten a note's frontmatter into `note_meta` write entries. Empty when
/// the file has no (well-formed) frontmatter. status: store-note-metadata-index
fn note_metadata_entries(contents: &str) -> Vec<MetaEntry> {
    match crate::frontmatter::split(contents).frontmatter {
        Some(fm) => crate::frontmatter::flatten(&fm)
            .into_iter()
            .map(|f| MetaEntry {
                key: f.key,
                value: f.value,
                num: f.num,
            })
            .collect(),
        None => Vec::new(),
    }
}

async fn process_upsert(
    vault_root: &Path,
    store: &mut Store,
    embedder: Arc<dyn Embedder>,
    rel_path: &str,
    force: bool,
) -> Result<UpsertOutcome, Error> {
    let chunker: &dyn Chunker = match path_extension(rel_path) {
        Some(ext) if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") => {
            &Markdown
        }
        Some(ext) if ext.eq_ignore_ascii_case("txt") => &Txt,
        _ => return Ok(UpsertOutcome::Skipped("unsupported extension".into())),
    };
    if is_ignored(rel_path) {
        return Ok(UpsertOutcome::Skipped("ignored path".into()));
    }
    let abs = vault_root.join(rel_path);

    let metadata = match tokio::fs::metadata(&abs).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // File vanished between event and read — treat as Delete.
            process_delete(store, rel_path)?;
            return Ok(UpsertOutcome::Skipped("missing on disk; deleted".into()));
        }
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_file() {
        return Ok(UpsertOutcome::Skipped("not a file".into()));
    }
    let size = metadata.len();
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if size > MAX_FILE_BYTES {
        // Persist a Skipped row so the UI can mark this file across launches
        // (per index.md `cmd-file-index-state`). Reason string is the
        // exact human-readable text the tooltip / status bar will display.
        let reason = "file too large";
        store.upsert_skipped(rel_path, reason, mtime, size as i64)?;
        return Ok(UpsertOutcome::Skipped(reason.into()));
    }
    let bytes = tokio::fs::read(&abs).await?;
    let contents = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            let reason = "not UTF-8";
            store.upsert_skipped(rel_path, reason, mtime, size as i64)?;
            return Ok(UpsertOutcome::Skipped(reason.into()));
        }
    };
    let content_hash = hash_string(&contents);

    // Short-circuit: same content + same embedder version → no-op. Skipped
    // when `force` is set so an explicit user reindex actually re-embeds.
    if !force
        && let Some(existing) = store.get_note_by_path(rel_path)?
        && existing.content_hash == content_hash
        && existing.embedder_version == embedder.version()
    {
        return Ok(UpsertOutcome::Unchanged);
    }

    // Chunk + embed. Embed is CPU-heavy: spawn_blocking.
    let chunks = chunker.chunk(&contents);
    if chunks.is_empty() {
        // Empty note: still record the row so deletes/renames work, but no
        // embeddings to insert.
        // Adopt frontmatter `hiker.id` when present and `path_ids` empty,
        // same as the chunked branch — see
        // `bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids`.
        let id = match store.id_for_path(rel_path)? {
            Some(id) => id,
            None => frontmatter_hiker_id(&contents).unwrap_or_else(new_id),
        };
        let indexed_at = now_secs();
        store.upsert_note(&NoteUpsert {
            id: &id,
            path: rel_path,
            content_hash: &content_hash,
            mtime,
            size: size as i64,
            indexed_at,
            embedder_version: embedder.version(),
            chunks: Vec::new(),
        })?;
        // status: store-note-metadata-index
        // Re-derive the metadata index even for empty-body notes: their
        // frontmatter (tags, lifecycle, author) is the whole point of a
        // metadata-only note.
        store.replace_note_metadata(&id, &note_metadata_entries(&contents))?;
        // status: trail-waypoints-derived-table
        // Also re-derive on the empty-body branch — waypoint-notes
        // intentionally have empty bodies (`trail-empty-waypoint-body`),
        // so the FM-only path is the common case for them.
        update_trail_waypoints_if_relevant(store, rel_path, &contents);
        // status: board-cards-derived-table
        update_board_cards_if_relevant(store, rel_path, &contents);
        return Ok(UpsertOutcome::Indexed);
    }

    // bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids:
    // adopt the source's `hiker.id` from frontmatter when the indexer
    // has no `path_ids` row yet. Otherwise the case where a user-action
    // pre-stamped the file (e.g. capture flow that bypasses the indexer
    // for a moment) would mint a *different* ULID into `path_ids`,
    // diverging from the value already written into the file.
    let id = match store.id_for_path(rel_path)? {
        Some(id) => id,
        None => frontmatter_hiker_id(&contents).unwrap_or_else(new_id),
    };
    let indexed_at = now_secs();

    // status: trail-waypoints-derived-table
    // Re-derive `trail_waypoints` rows for trail-docs and waypoint
    // notes BEFORE we drop the file body — the parser needs the full
    // contents, but afterwards we don't, and the body can be up to
    // `MAX_FILE_BYTES` (5 MiB). Holding it alongside the chunks +
    // embeddings dominates per-file memory; dropping it here halves
    // the peak.
    update_trail_waypoints_if_relevant(store, rel_path, &contents);
    // status: board-cards-derived-table
    // Re-derive `board_cards` rows for board-docs before the body drops —
    // same ordering rationale as the trail re-derive above.
    update_board_cards_if_relevant(store, rel_path, &contents);
    // status: store-note-metadata-index
    // Flatten frontmatter now, before the body is dropped — the upsert
    // below runs after `drop(contents)` to bound peak memory, but the
    // entries are small owned strings cheap to carry across the embed.
    let meta_entries = note_metadata_entries(&contents);
    drop(contents);

    // Embed in capped batches. The embedder library (fastembed/onnx)
    // tokenizes the entire input batch into one tensor before running
    // inference; a single 5 MiB file can yield 1000+ chunks, and
    // embedding all of them at once allocates a tensor proportional to
    // batch_size × max_seq_len × hidden_dim. Capping the batch bounds
    // the embedder's transient memory regardless of file size.
    const EMBED_BATCH_SIZE: usize = 16;
    let chunk_count = chunks.len();
    let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunk_count);
    let embed_start = std::time::Instant::now();
    for batch in chunks.chunks(EMBED_BATCH_SIZE) {
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let emb_clone = embedder.clone();
        let batch_emb =
            tokio::task::spawn_blocking(move || emb_clone.embed_batch(&texts))
                .await
                .map_err(|e| Error::Embed(EmbedError::Embed(e.to_string())))??;
        embeddings.extend(batch_emb);
    }
    tracing::debug!(
        batch_size = chunk_count,
        elapsed_ms = embed_start.elapsed().as_millis() as u64,
        path = %rel_path,
        "embedder: batch embedded",
    );

    let zipped: Vec<_> = chunks.into_iter().zip(embeddings).collect();
    store.upsert_note(&NoteUpsert {
        id: &id,
        path: rel_path,
        content_hash: &content_hash,
        mtime,
        size: size as i64,
        indexed_at,
        embedder_version: embedder.version(),
        chunks: zipped,
    })?;
    store.replace_note_metadata(&id, &meta_entries)?;

    Ok(UpsertOutcome::Indexed)
}

/// Soft-error helper that re-derives `trail_waypoints` rows for a file
/// that may be a trail-doc or a waypoint-note.
///
/// Two ingest paths share this function:
///
///   - **Trail-doc ingest** is the authoritative re-derive: it walks
///     the recursive `hiker.waypoints` tree, clears every existing row
///     for `trail_id`, and re-inserts one row per waypoint with
///     correct `parent_waypoint_id` + `tree_path` filled in. This is
///     the canonical population path; tree-shape edits to the
///     trail-doc reach the table here.
///   - **Waypoint-note ingest** writes a single row keyed on the
///     waypoint's own frontmatter (`hiker.in_trail`, `hiker.references`,
///     `hiker.id`). It cannot know its own `parent_waypoint_id` /
///     `tree_path` from the waypoint-note alone — that information
///     lives in the parent trail-doc. So those columns are written as
///     `(NULL, "")` here; the next trail-doc ingest fills the
///     canonical values via the depth-first re-derive above. The
///     append-waypoint op enqueues both upserts (waypoint-note then
///     trail-doc), so the canonical fill follows immediately.
fn update_trail_waypoints_if_relevant(
    store: &mut Store,
    rel_path: &str,
    contents: &str,
) {
    // Cheap kind discriminator: only attempt the parse on `.md` files.
    if !rel_path.ends_with(".md") {
        return;
    }
    let ingest = WaypointIngest { store, rel_path, contents };
    if rel_path.starts_with(".hiker/trails/") && rel_path.contains("/waypoints/") {
        ingest.upsert_waypoint_row();
    } else {
        ingest.rebuild_trail_doc_rows();
    }
}

/// Bundled refs for the two derived-table update paths. Methods stay exempt
/// from `clippy::single_call_fn` and split the work so the dispatcher above
/// stays under the cognitive-complexity cap.
struct WaypointIngest<'a> {
    store: &'a mut Store,
    rel_path: &'a str,
    contents: &'a str,
}

impl<'a> WaypointIngest<'a> {
    fn upsert_waypoint_row(self) {
        use crate::store::dto::WaypointRow;
        use crate::trails::parse_waypoint;
        let fm = match parse_waypoint(self.contents) {
            Ok(fm) => fm,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    path = %self.rel_path,
                    "indexer: waypoint parse failed (file may be mid-edit)",
                );
                return;
            }
        };
        // Source id comes from the index lookup at ingest time; may be
        // None if the source hasn't been indexed yet.
        let source_id = match self.store.id_for_path(&fm.references.path) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.rel_path,
                    "indexer: source id_for_path lookup failed",
                );
                None
            }
        };
        let row = WaypointRow {
            waypoint_path: self.rel_path.to_string(),
            waypoint_id: fm.id,
            trail_id: fm.in_trail.id,
            source_id,
            source_path: fm.references.path,
            // Tree-position columns are owned by the trail-doc ingest
            // path; written as the empty / NULL default here. The trail-
            // doc ingest that follows `append_waypoint` enqueues both, so
            // the canonical values land within the same indexer drain.
            parent_waypoint_id: None,
            tree_path: String::new(),
        };
        if let Err(e) = self.store.upsert_trail_waypoint(&row) {
            tracing::warn!(
                error = %e,
                path = %self.rel_path,
                "indexer: upsert_trail_waypoint failed",
            );
        }
    }

    /// Trail-doc ingest: clear + re-insert every row for `trail_id` so
    /// tree-shape changes (re-parent, reorder, remove) propagate to the
    /// derived table. Frontmatter is the source of truth.
    ///
    /// status: trail-waypoints-derived-table
    /// status: trail-side-trail-shape
    fn rebuild_trail_doc_rows(self) {
        use crate::store::dto::WaypointRow;
        use crate::trails::{parse_trail_doc_for, walk_waypoints_depth_first};
        let Ok(fm) = parse_trail_doc_for(self.rel_path, self.contents) else { return };
        let trail_id = fm.id.clone();
        // Capture existing rows BEFORE the clear so we can preserve each
        // row's `source_id` / `source_path` (those columns are owned by
        // the per-waypoint ingest path and aren't recoverable from the
        // trail-doc alone).
        let existing_by_path: std::collections::HashMap<String, (Option<String>, String)> = self
            .store
            .waypoints_of(&trail_id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.waypoint_path, (r.source_id, r.source_path)))
            .collect();
        if let Err(e) = self.store.delete_trail_waypoints_by_trail(&trail_id) {
            tracing::warn!(
                error = %e,
                trail_id = %trail_id,
                "indexer: delete_trail_waypoints_by_trail failed",
            );
        }
        let store = self.store;
        walk_waypoints_depth_first(&fm.waypoints, &mut |parent_id, entry, tree_path| {
            let (source_id, source_path) = existing_by_path
                .get(&entry.path)
                .cloned()
                .unwrap_or((None, String::new()));
            let row = WaypointRow {
                waypoint_path: entry.path.clone(),
                waypoint_id: entry.id.clone(),
                trail_id: trail_id.clone(),
                source_id,
                source_path,
                parent_waypoint_id: parent_id.map(str::to_string),
                tree_path: tree_path.to_string(),
            };
            if let Err(e) = store.upsert_trail_waypoint(&row) {
                tracing::warn!(
                    error = %e,
                    path = %entry.path,
                    "indexer: upsert_trail_waypoint (trail-doc walk) failed",
                );
            }
        });
    }
}

/// Re-derive `board_cards` rows for a board-doc on ingest. Mirrors
/// `update_trail_waypoints_if_relevant`'s trail-doc path: parse the
/// `hiker.columns` frontmatter, clear every existing row for the board's
/// id, and re-insert one row per card with its column + ordinal. The
/// board-doc frontmatter is the source of truth (clear-then-reinsert).
/// Soft-error: a parse failure (non-board note, mid-edit) is a silent
/// no-op.
///
/// status: board-cards-derived-table
fn update_board_cards_if_relevant(store: &mut Store, rel_path: &str, contents: &str) {
    use crate::boards::parse_board_for;
    use crate::store::dto::BoardCardRow;
    if !rel_path.ends_with(".md") {
        return;
    }
    let Ok(board) = parse_board_for(rel_path, contents) else { return };
    let mut rows: Vec<BoardCardRow> = Vec::new();
    for col in &board.columns {
        for (ordinal, card) in col.cards.iter().enumerate() {
            rows.push(BoardCardRow {
                board_id: board.id.clone(),
                board_path: rel_path.to_string(),
                card_note_id: card.id.clone(),
                card_note_path: card.path.clone(),
                column_name: col.name.clone(),
                ordinal: ordinal as i64,
            });
        }
    }
    if let Err(e) = store.replace_board_cards(&board.id, &rows) {
        tracing::warn!(error = %e, path = %rel_path, "indexer: replace_board_cards failed");
    }
}

fn process_delete(store: &mut Store, rel_path: &str) -> Result<bool, Error> {
    // status: trail-waypoints-derived-table
    // Drop any derived waypoint row that referenced this path — both for
    // a waypoint-note being deleted (waypoint_path match) and for a
    // source note being deleted (source_path match) so orphaned rows
    // don't linger.
    if let Err(e) = store.delete_trail_waypoint_by_path(rel_path) {
        tracing::warn!(
            error = %e,
            path = %rel_path,
            "indexer: delete_trail_waypoint_by_path failed",
        );
    }
    // status: board-cards-derived-table
    // Drop any board_cards rows for a deleted board-doc (board_path match).
    // Cards pointing at a deleted *source* note are left in place — a card
    // for a deleted note renders as a broken card; the user removes or
    // repoints it.
    if let Err(e) = store.delete_board_cards_by_board_path(rel_path) {
        tracing::warn!(
            error = %e,
            path = %rel_path,
            "indexer: delete_board_cards_by_board_path failed",
        );
    }
    let id = match store.id_for_path(rel_path)? {
        Some(id) => id,
        None => return Ok(false),
    };
    store.delete_note(&id)?;
    Ok(true)
}

