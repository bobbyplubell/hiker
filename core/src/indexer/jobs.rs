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
use crate::oplog::OpLog;
use crate::store::dto::{MetaEntry, NoteUpsert};
use crate::store::Store;
use crate::watcher::is_ignored;

// Only the most heavily-used parent items are imported; the rest are reached
// via explicit `super::` paths at their use sites so this file doesn't lean on
// a wide slice of its parent's namespace (per `check-splits` super-reach).
use super::{update_status, Error, IndexJob, ProgressEvent};

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
    pub status: &'a watch::Sender<super::IndexStatus>,
    pub pending: &'a Arc<Mutex<HashSet<String>>>,
    pub self_tx: &'a super::IndexJobTx,
    pub watcher_cell: &'a Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    pub oplog_cell: &'a Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
    /// status: inbox-rules
    pub inbox_cell: &'a Arc<OnceCell<Arc<crate::inbox::Rules>>>,
}

/// Subset of `JobCtx` used by handlers that don't need the trails /
/// watcher plumbing — pure per-file ingest.
pub(super) struct UpsertCtx<'a> {
    pub vault_root: &'a Path,
    pub embedder: &'a Arc<dyn Embedder>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub status: &'a watch::Sender<super::IndexStatus>,
    /// Cell holding the vault's `OpLog` once `attach_oplog` runs. Per
    /// `op-log-bootstraps-first`, this is populated before the indexer
    /// processes any jobs in the steady state, so handlers read it via
    /// `oplog_cell.get()` to translate vault paths to `doc_id` for
    /// `notes.id`. status: store-path-is-identity
    pub oplog_cell: &'a Arc<OnceCell<Arc<OpLog>>>,
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
            oplog_cell: self.oplog_cell,
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
        let oplog = self.oplog_cell.get().cloned();
        let outcome = process_upsert(
            self.vault_root,
            store,
            self.embedder.clone(),
            oplog.as_deref(),
            &rel_path,
            force,
        )
        .await;
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
        // status: store-path-is-identity
        // Rename targets the `notes.path UNIQUE` row directly. False means
        // the source path was never indexed; treat the destination as a
        // fresh upsert. The op-log's `doc-index.db` rename happens through
        // `record_oplog_rename` on the Move/MoveFolder paths; watcher-
        // driven external renames here don't touch the op-log directly —
        // a subsequent ingest re-reads `doc_id` from the (already updated
        // or freshly minted) `doc-index.db` row.
        let process_result: Result<bool, Error> =
            store.rename_note_by_path(&from, &to).map_err(Error::from);
        match process_result {
            Ok(true) => {
                let _ = progress.send(ProgressEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                });
                // status: wikilink-rename-rewrite
                // Watcher-driven external rename: run the shared
                // referrer-rewrite pass (trails + boards + wikilinks).
                crate::links_rename::on_note_moved(
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
                // History-preserving restore: when the trashed entry references
                // an op-log `doc_id` (a tracked note whose `.ops` history was
                // retained, not purged, on delete), rebind `path → doc_id` to
                // that retained doc and clear its tombstone so the document
                // comes back with its full change history rather than as a
                // brand-new import. An entry with no `doc_id` (hand-dropped
                // file, never-seeded note, or folder — whose members rebind by
                // path on re-ingest) takes the existing fresh-import path.
                // status: vault-trash-restore
                record_oplog_restore(self.oplog_cell, &entry);
                // Re-ingest the restored .md files inline so the index
                // picks them up without waiting on watcher events
                // (which the caller suppressed). For folders, walk
                // the manifest's recorded members; for files, just the
                // single original_path. Because `notes.id` == the op-log
                // `doc_id` (`store-path-is-identity`), re-ingest reattaches
                // fresh chunks/embeddings under the rebound identity.
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
    pub status: &'a watch::Sender<super::IndexStatus>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub self_tx: &'a super::IndexJobTx,
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
            Some(q) => Some(super::submit_embedder_load_task(q, &model_id).await),
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
        // status: inbox-rules
        IndexJob::Created { rel_path } => {
            let final_path = run_inbox_rules(ctx, store, &rel_path).await;
            ctx.handle_upsert(store, final_path, false).await;
        }
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
            // status: note-companion-folder
            // move_note pairs the note's companion folder (when present),
            // returning every contained `(old, new)` member pair so the
            // reference-rewrite pass below covers the moved children too —
            // e.g. a renamed trail-doc whose waypoint-notes carry an
            // `hiker.in_trail` path that must follow the rename.
            let move_result = crate::vault::move_note(vault, store, None, &from, &to);
            // status: wikilink-rename-rewrite
            // After the path remap succeeds, run the shared rename-rewrite
            // pass over every referrer (trails, boards, wikilinks). Errors
            // inside each domain helper are logged, never propagated, so a
            // partial referrer update can't fail the move's reply.
            let reply_result = match move_result {
                Ok(companion_members) => {
                    record_oplog_rename(ctx.oplog_cell, &from, &to);
                    // status: note-companion-folder
                    // Run the companion-folder members FIRST: each member's
                    // pass updates the derived `trail_waypoints.waypoint_path`
                    // to its new path (and rewrites the trail-doc's
                    // `hiker.waypoints[]` entry). The trail-doc's own pass
                    // below then reads the *fresh* waypoint paths when
                    // rewriting each waypoint's `hiker.in_trail`.
                    for (old, new) in &companion_members {
                        record_oplog_rename(ctx.oplog_cell, old, new);
                        crate::links_rename::on_note_moved(
                            watcher_cell, ctx.oplog_cell, self_tx, vault, store, old, new,
                        )
                        .await;
                    }
                    crate::links_rename::on_note_moved(
                        watcher_cell, ctx.oplog_cell, self_tx, vault, store, &from, &to,
                    )
                    .await;
                    Ok(())
                }
                Err(e) => Err(e),
            };
            let _ = reply.send(reply_result);
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
            // status: wikilink-rename-rewrite
            if result.is_ok() {
                for (old, new) in &pairs {
                    record_oplog_rename(ctx.oplog_cell, old, new);
                    crate::links_rename::on_note_moved(
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
            // Resolve the doc_id while the path is still mapped, so the trash
            // entry can carry it for a history-preserving restore.
            // status: vault-trash-restore
            let doc_id = ctx
                .oplog_cell
                .get()
                .and_then(|log| log.doc_id_for_path(&rel).ok().flatten());
            let result = crate::vault::delete_note(vault, store, None, &trash, &rel, doc_id);
            match &result {
                Ok(entry) => {
                    // status: board-cards-derived-table
                    // `vault::delete_note` drops the `notes`/`chunks` rows but
                    // `board_cards` has no FK cascade, so a trashed board-doc's
                    // derived rows must be cleared explicitly (mirrors the
                    // `process_delete` cleanup the `IndexJob::Delete` path uses)
                    // so the trashed board drops off the Boards index.
                    // status: board-delete
                    clear_board_cards_for_delete(store, &rel, entry);
                    // `trail_waypoints` likewise has no FK cascade off
                    // `notes`, so a trashed trail-doc / waypoint-note must
                    // drop its derived rows explicitly — mirrors the
                    // `process_delete` cleanup the `IndexJob::Delete` / watcher
                    // path runs.
                    clear_trail_waypoints_for_delete(store, &rel, entry);
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

/// Evaluate the inbox rules against a freshly-created file. Returns the
/// path the file ends up at (same as input when no rule matched), so the
/// caller can upsert at the post-move location. Errors are logged and
/// degrade to "no rule applied" so a config mistake never blocks ingest.
///
/// status: inbox-rules
async fn run_inbox_rules(
    ctx: &JobCtx<'_>,
    store: &mut Store,
    rel_path: &str,
) -> String {
    let Some(rules) = ctx.inbox_cell.get() else {
        return rel_path.to_string();
    };
    if rules.is_empty() {
        return rel_path.to_string();
    }
    let watcher = ctx.watcher_cell.get().map(std::sync::Arc::as_ref);
    match rules.apply_to_created(ctx.vault, store, watcher, rel_path) {
        Ok(Some(applied)) => {
            let _ = ctx.progress.send(ProgressEvent::InboxApplied {
                rule_index: applied.rule_index as u32,
                original_path: rel_path.to_string(),
                final_path: applied.final_rel_path.clone(),
                moved_to: applied.moved_to.clone(),
                tagged: applied.tagged.clone(),
            });
            if let Some(new_path) = applied.moved_to.as_deref() {
                // The original path may have been indexed transiently —
                // make sure no stale row lingers for it before we upsert at
                // the new path.
                let _ = process_delete(store, rel_path);
                tracing::info!(
                    from = %rel_path,
                    to = %new_path,
                    "inbox: rule moved file",
                );
            }
            applied.final_rel_path
        }
        Ok(None) => rel_path.to_string(),
        Err(e) => {
            tracing::warn!(error = %e, path = %rel_path, "inbox: rule application failed");
            rel_path.to_string()
        }
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
        crate::oplog::writes::rename(log, from, to, &crate::oplog::shapes::Author::User)
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
            crate::oplog::writes::tombstone(log, path, &crate::oplog::shapes::Author::User)
        {
            tracing::warn!(error = %e, %path, "indexer: op-log tombstone failed");
        }
    }
}

/// Rebind the op-log `doc_id` of a restored trash entry so the document comes
/// back with its retained history. A no-op when the entry has no recorded
/// `doc_id` (hand-dropped file, never-seeded note, or a folder whose members
/// re-bind by path on re-ingest) or when no op-log handle is attached
/// (CLI / tests). Best-effort, like [`record_oplog_tombstone`]: a failure is
/// logged, not propagated — the filesystem restore already succeeded and the
/// note is recoverable; only its history rebind is at risk.
///
/// status: vault-trash-restore
fn record_oplog_restore(
    oplog_cell: &Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
    entry: &crate::trash::Entry,
) {
    let Some(doc_id) = &entry.doc_id else { return };
    let Some(log) = oplog_cell.get() else { return };
    if let Err(e) = crate::oplog::writes::restore(
        log,
        doc_id,
        &entry.original_path,
        &crate::oplog::shapes::Author::User,
    ) {
        tracing::warn!(
            error = %e,
            doc_id = %doc_id,
            path = %entry.original_path,
            "indexer: op-log restore rebind failed",
        );
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
    let oplog = ctx.oplog_cell.get().cloned();
    match process_upsert(
        ctx.vault_root,
        store,
        ctx.embedder.clone(),
        oplog.as_deref(),
        rel_path,
        false,
    )
    .await?
    {
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
    super::update_total_notes(status, store);
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
    oplog: Option<&OpLog>,
    rel_path: &str,
    force: bool,
) -> Result<UpsertOutcome, Error> {
    let chunker: &dyn Chunker = match super::path_extension(rel_path) {
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
    if size > super::MAX_FILE_BYTES {
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
        // status: store-path-is-identity
        let indexed_at = super::now_secs();
        store.upsert_note(&NoteUpsert {
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
        // metadata-only note. The note's path is its `note_meta` key.
        store.replace_note_metadata(rel_path, &note_metadata_entries(&contents))?;
        // status: trail-waypoints-derived-table
        // Also re-derive on the empty-body branch — waypoint-notes
        // intentionally have empty bodies (`trail-empty-waypoint-body`),
        // so the FM-only path is the common case for them.
        update_trail_waypoints_if_relevant(store, oplog, rel_path, &contents);
        // status: board-cards-derived-table
        update_board_cards_if_relevant(store, oplog, rel_path, &contents);
        return Ok(UpsertOutcome::Indexed);
    }

    // status: store-path-is-identity
    let indexed_at = super::now_secs();

    // status: trail-waypoints-derived-table
    // Re-derive `trail_waypoints` rows for trail-docs and waypoint
    // notes BEFORE we drop the file body — the parser needs the full
    // contents, but afterwards we don't, and the body can be up to
    // `MAX_FILE_BYTES` (5 MiB). Holding it alongside the chunks +
    // embeddings dominates per-file memory; dropping it here halves
    // the peak.
    update_trail_waypoints_if_relevant(store, oplog, rel_path, &contents);
    // status: board-cards-derived-table
    // Re-derive `board_cards` rows for board-docs before the body drops —
    // same ordering rationale as the trail re-derive above.
    update_board_cards_if_relevant(store, oplog, rel_path, &contents);
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
        path: rel_path,
        content_hash: &content_hash,
        mtime,
        size: size as i64,
        indexed_at,
        embedder_version: embedder.version(),
        chunks: zipped,
    })?;
    store.replace_note_metadata(rel_path, &meta_entries)?;

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
    oplog: Option<&OpLog>,
    rel_path: &str,
    contents: &str,
) {
    // Cheap kind discriminator: only attempt the parse on `.md` files.
    if !rel_path.ends_with(".md") {
        return;
    }
    let ingest = WaypointIngest { store, oplog, rel_path, contents };
    // status: note-companion-folder
    // Waypoints now live in the trail-doc's visible companion folder, so a
    // path prefix no longer distinguishes them from trail-docs. Dispatch on
    // `hiker.kind` instead: a note that parses as a waypoint
    // (`hiker.kind: waypoint`) writes a waypoint row; anything else routes
    // to the trail-doc rebuild path (which itself no-ops on a note that
    // isn't a trail-doc).
    if crate::trails::parse_waypoint(contents).is_ok() {
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
    oplog: Option<&'a OpLog>,
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
        // status: store-path-is-identity
        // Trail id is the op-log's `doc_id` for `fm.in_trail` (the
        // waypoint-note's parent trail-doc path). The source note is
        // referenced by path only (path-as-identity).
        let source_path = fm.references.clone();
        let trail_id = self
            .oplog
            .and_then(|log| log.doc_id_for_path(&fm.in_trail).unwrap_or(None))
            .unwrap_or_default();
        // Waypoint id (the legacy `WaypointRow.waypoint_id` column) is
        // the op-log's `doc_id` for the waypoint-note's own path under
        // path-as-identity — sourced from the same lookup the trail-doc
        // ingest uses to seed its row.
        let waypoint_id = self
            .oplog
            .and_then(|log| log.doc_id_for_path(self.rel_path).unwrap_or(None))
            .unwrap_or_default();
        let row = WaypointRow {
            waypoint_path: self.rel_path.to_string(),
            waypoint_id,
            trail_id,
            source_path,
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
        // status: store-path-is-identity
        // The trail's id is the op-log's `doc_id` for the trail-doc's
        // path; absent (oplog not seeded yet) is a soft no-op so the
        // next ingest re-derives once the cell is populated.
        let Some(log) = self.oplog else { return };
        let trail_id = match log.doc_id_for_path(self.rel_path) {
            Ok(Some(id)) => id,
            _ => return,
        };
        // Capture existing rows BEFORE the clear so we can preserve each
        // row's `source_path` (that column is owned by the per-waypoint
        // ingest path and isn't recoverable from the trail-doc alone).
        let existing_by_path: std::collections::HashMap<String, String> = self
            .store
            .waypoints_of(&trail_id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.waypoint_path, r.source_path))
            .collect();
        if let Err(e) = self.store.delete_trail_waypoints_by_trail(&trail_id) {
            tracing::warn!(
                error = %e,
                trail_id = %trail_id,
                "indexer: delete_trail_waypoints_by_trail failed",
            );
        }
        let store = self.store;
        walk_waypoints_depth_first(&fm.waypoints, &mut |parent_path, entry, tree_path| {
            let source_path = existing_by_path
                .get(&entry.path)
                .cloned()
                .unwrap_or_default();
            // status: store-path-is-identity
            // Waypoint id / parent waypoint id are the op-log doc_ids
            // for each waypoint-note path. Both default to empty when
            // the lookup misses — the rows still wire correctly via
            // `waypoint_path` / `source_path`.
            let waypoint_id = log
                .doc_id_for_path(&entry.path)
                .ok()
                .flatten()
                .unwrap_or_default();
            let parent_waypoint_id = parent_path
                .and_then(|p| log.doc_id_for_path(p).ok().flatten());
            let row = WaypointRow {
                waypoint_path: entry.path.clone(),
                waypoint_id,
                trail_id: trail_id.clone(),
                source_path,
                parent_waypoint_id,
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
fn update_board_cards_if_relevant(
    store: &mut Store,
    oplog: Option<&OpLog>,
    rel_path: &str,
    contents: &str,
) {
    use crate::boards::parse_board_for;
    use crate::store::dto::BoardCardRow;
    if !rel_path.ends_with(".md") {
        return;
    }
    let Ok(board) = parse_board_for(rel_path, contents) else { return };
    // status: store-path-is-identity
    // The board's storage key is the op-log's `doc_id` for the
    // board-doc's path; absent (oplog not seeded yet) is a soft no-op so
    // the next ingest re-derives once the cell is populated.
    let Some(log) = oplog else { return };
    let board_id = match log.doc_id_for_path(rel_path) {
        Ok(Some(id)) => id,
        _ => return,
    };
    let mut rows: Vec<BoardCardRow> = Vec::new();
    for col in &board.columns {
        for (ordinal, card) in col.cards.iter().enumerate() {
            // Freeform text cards reference no note, so they get no derived
            // row — the reverse "boards containing note" lookup and
            // auto-update-on-move don't apply to them. The card's column
            // position is still its ordinal. status: board-freeform-card
            let crate::boards::BoardCard::Note { path } = card else {
                continue;
            };
            // Path-as-identity: the card references its note by rel-path.
            rows.push(BoardCardRow {
                board_id: board_id.clone(),
                board_path: rel_path.to_string(),
                card_note_path: path.clone(),
                column_name: col.name.clone(),
                ordinal: ordinal as i64,
            });
        }
    }
    if let Err(e) = store.replace_board_cards(&board_id, &rows) {
        tracing::warn!(error = %e, path = %rel_path, "indexer: replace_board_cards failed");
    }
}

/// Clear the derived `board_cards` rows for a board-doc moved to trash via
/// `IndexJob::DeleteNote`. `board_cards` has no FK cascade off `notes`, so the
/// rows have to be dropped by board-doc path. Covers both a single board-doc
/// (`rel`) and every `.md` member of a deleted folder (`entry.members`).
/// Soft-error: a clear failure is logged, never propagated (matches the rest
/// of the derived-table maintenance). status: board-delete
fn clear_board_cards_for_delete(store: &mut Store, rel: &str, entry: &crate::trash::Entry) {
    let mut paths: Vec<&str> = vec![rel];
    if let Some(members) = &entry.members {
        paths.extend(members.iter().map(String::as_str));
    }
    for p in paths {
        if let Err(e) = store.delete_board_cards_by_board_path(p) {
            tracing::warn!(
                error = %e,
                path = %p,
                "indexer: delete_board_cards_by_board_path (delete-note) failed",
            );
        }
    }
}

/// Clear the derived `trail_waypoints` rows for a waypoint-note or source-note
/// moved to trash via `IndexJob::DeleteNote`. `trail_waypoints` has no FK
/// cascade off `notes`, so the rows have to be dropped by path. Covers both a
/// single note (`rel`) and every `.md` member of a deleted folder
/// (`entry.members`). `delete_trail_waypoint_by_path` matches on both
/// `waypoint_path` and `source_path`, so this drops a deleted waypoint-note's
/// own row and any waypoint pointing at a deleted source — mirroring the
/// `process_delete` (watcher / `IndexJob::Delete`) cleanup. Soft-error: a clear
/// failure is logged, never propagated. status: trail-waypoints-derived-table
fn clear_trail_waypoints_for_delete(store: &mut Store, rel: &str, entry: &crate::trash::Entry) {
    let mut paths: Vec<&str> = vec![rel];
    if let Some(members) = &entry.members {
        paths.extend(members.iter().map(String::as_str));
    }
    for p in paths {
        if let Err(e) = store.delete_trail_waypoint_by_path(p) {
            tracing::warn!(
                error = %e,
                path = %p,
                "indexer: delete_trail_waypoint_by_path (delete-note) failed",
            );
        }
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
    // status: store-path-is-identity / ingest-delete-cascade
    Ok(store.delete_note_by_path(rel_path)?)
}

