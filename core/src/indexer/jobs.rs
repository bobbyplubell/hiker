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
use crate::editing::LayeredDoc;
use crate::store::dto::{MetaEntry, NoteUpsert};
use crate::store::Store;

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
    pub layered_cell: &'a Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    /// status: inbox-rules
    pub inbox_cell: &'a Arc<OnceCell<Arc<crate::inbox::Rules>>>,
    /// status: kind-lenient-validation
    pub kinds_cell: &'a Arc<OnceCell<Arc<crate::kinds::Registry>>>,
    /// status: rule-triggers
    pub rules_cell: &'a Arc<OnceCell<Arc<crate::rules::Engine>>>,
}

/// Subset of `JobCtx` used by handlers that don't need the trails /
/// watcher plumbing — pure per-file ingest.
pub(super) struct UpsertCtx<'a> {
    pub vault_root: &'a Path,
    pub embedder: &'a Arc<dyn Embedder>,
    pub progress: &'a broadcast::Sender<ProgressEvent>,
    pub status: &'a watch::Sender<super::IndexStatus>,
    /// Cell holding the vault's `LayeredDoc` once `attach_layered` runs. Per
    /// `op-log-bootstraps-first`, this is populated before the indexer
    /// processes any jobs in the steady state, so handlers read it via
    /// `layered_cell.get()` to translate vault paths to `doc_id` for
    /// `notes.id`. status: store-path-is-identity
    pub layered_cell: &'a Arc<OnceCell<Arc<LayeredDoc>>>,
    /// Compiled kind registry for lenient per-note validation, when the
    /// host attached one. status: kind-lenient-validation
    pub kinds_cell: &'a Arc<OnceCell<Arc<crate::kinds::Registry>>>,
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
            layered_cell: self.layered_cell,
            kinds_cell: self.kinds_cell,
        }
    }

    async fn handle_upsert(
        &self,
        store: &mut Store,
        rel_path: String,
        force: bool,
        created: bool,
    ) {
        let progress = self.progress;
        let status = self.status;
        let pending = self.pending;
        // Make sure the path is in the pending set even if it didn't go
        // through a tracking sender;
        // remove on every terminal outcome below.
        pending.lock().unwrap().insert(rel_path.clone());
        let _ = progress.send(ProgressEvent::Started { path: rel_path.clone() });
        let layered = self.layered_cell.get().cloned();
        let kinds = self.kinds_cell.get().cloned();
        let outcome = process_upsert(
            self.vault_root,
            store,
            self.embedder.clone(),
            layered.as_deref(),
            kinds.as_deref(),
            &rel_path,
            force,
        )
        .await;
        pending.lock().unwrap().remove(&rel_path);
        match outcome {
            Ok(UpsertOutcome::Indexed(events)) => {
                tracing::debug!(path = %rel_path, "indexer: file indexed");
                let _ = progress.send(ProgressEvent::Finished { path: rel_path.clone() });
                // status: rule-triggers
                // The rule pass hooks in post-index, right after the derived
                // tables the triggers watch have been re-derived — on the
                // indexer task, the single-writer discipline.
                self.run_rule_pass(store, &rel_path, created, events);
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
        // fresh upsert. The layered doc's `doc-index.db` rename happens through
        // `record_layered_rename` on the Move/MoveFolder paths; watcher-
        // driven external renames here don't touch the layered doc directly —
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
                let sweep = crate::links_rename::RenameSweepCtx {
                    watcher_cell,
                    layered_cell: self.layered_cell,
                    kinds_cell: self.kinds_cell,
                    jobs: self_tx,
                    vault,
                };
                crate::links_rename::on_note_moved(&sweep, store, &from, &to).await;
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

    /// Run the vault-rules pass for one ingested path (`docs/rules.md`):
    /// fold in the `note-created` event when this ingest came from the
    /// watcher's create path, then hand the detected events to the engine.
    /// A no-op without an attached engine (CLI / tests).
    ///
    /// status: rule-triggers
    fn run_rule_pass(
        &self,
        store: &Store,
        rel_path: &str,
        created: bool,
        mut events: Vec<crate::rules::RuleEvent>,
    ) {
        if created {
            events.push(crate::rules::RuleEvent::NoteCreated {
                path: rel_path.to_string(),
            });
        }
        if events.is_empty() {
            return;
        }
        self.with_rules_engine(store, |engine, fire| engine.on_events(fire, &events));
    }

    /// Assemble a `FireCtx` from the job context and run `op` with it,
    /// enqueueing a re-index for every path whose staged firing was
    /// applied (non-blocking — a full queue defers to the ambient watcher
    /// route). No-op when no engine / layered doc is attached or no rules are
    /// registered.
    ///
    /// Each successfully enqueued path is also registered for watcher
    /// self-write suppression (`watcher-suppress-self-writes`, the
    /// `ops::file` suppress-then-Upsert discipline): the firing's layered-doc
    /// accept already rewrote the `.md`, and the explicit job here is the
    /// ingest path, so the watcher echo would only queue a duplicate
    /// upsert and a redundant external-edit reconcile. A path whose
    /// enqueue FAILED is deliberately left unsuppressed — the ambient
    /// watcher route is then the only ingest path, and suppressing it
    /// would leave the index stale until the next scan.
    ///
    /// status: rule-triggers
    fn with_rules_engine(
        &self,
        store: &Store,
        op: impl FnOnce(&crate::rules::Engine, &crate::rules::FireCtx<'_>) -> Vec<String>,
    ) {
        let Some(engine) = self.rules_cell.get() else { return };
        if engine.is_empty() {
            return;
        }
        let Some(log) = self.layered_cell.get() else { return };
        let fallback;
        let kinds: &crate::kinds::Registry = match self.kinds_cell.get() {
            Some(registry) => registry,
            None => {
                fallback = crate::kinds::Registry::empty();
                &fallback
            }
        };
        let fire = crate::rules::FireCtx {
            vault: self.vault,
            store,
            log,
            kinds,
        };
        for rel in op(engine, &fire) {
            match self.self_tx.try_upsert(rel.clone(), false) {
                Ok(()) => {
                    if let Some(watcher) = self.watcher_cell.get() {
                        watcher.suppress(rel);
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        path = %rel,
                        "rules: re-index enqueue deferred to the watcher route",
                    );
                }
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
                // Identity-preserving restore: when the trashed entry references
                // a layered-doc `doc_id` (a tracked note whose doc was
                // retained, not purged, on delete), rebind `path → doc_id` to
                // that retained doc and clear its tombstone so the document
                // comes back under its original identity rather than as a
                // brand-new import. An entry with no `doc_id` (hand-dropped
                // file, never-seeded note, or folder — whose members rebind by
                // path on re-ingest) takes the existing fresh-import path.
                // status: vault-trash-restore
                record_layered_restore(self.layered_cell, &entry);
                // Re-ingest the restored .md files inline so the index
                // picks them up without waiting on watcher events
                // (which the caller suppressed). For folders, walk
                // the manifest's recorded members; for files, just the
                // single original_path. Because `notes.id` == the layered-doc
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
            // status: rule-triggers
            // The watcher's create event IS the `note-created` seam: the
            // upsert below is the note's first ingest, post-inbox-rules, so
            // the vault-rules pass sees the note where the inbox rule left
            // it (`rule-inbox-relation`). FullScan upserts never carry the
            // created flag — a fresh index over an existing vault is not a
            // wave of note creations.
            ctx.handle_upsert(store, final_path, false, true).await;
        }
        IndexJob::Upsert { rel_path, force } => {
            ctx.handle_upsert(store, rel_path, force, false).await;
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
                    record_layered_rename(ctx.layered_cell, &from, &to);
                    let sweep = crate::links_rename::RenameSweepCtx {
                        watcher_cell,
                        layered_cell: ctx.layered_cell,
                        kinds_cell: ctx.kinds_cell,
                        jobs: self_tx,
                        vault,
                    };
                    // status: note-companion-folder
                    // Run the companion-folder members FIRST: each member's
                    // pass updates the derived `trail_waypoints.waypoint_path`
                    // to its new path (and rewrites the trail-doc's
                    // `hiker.waypoints[]` entry). The trail-doc's own pass
                    // below then reads the *fresh* waypoint paths when
                    // rewriting each waypoint's `hiker.in_trail`.
                    for (old, new) in &companion_members {
                        record_layered_rename(ctx.layered_cell, old, new);
                        crate::links_rename::on_note_moved(&sweep, store, old, new).await;
                    }
                    crate::links_rename::on_note_moved(&sweep, store, &from, &to).await;
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
                let sweep = crate::links_rename::RenameSweepCtx {
                    watcher_cell,
                    layered_cell: ctx.layered_cell,
                    kinds_cell: ctx.kinds_cell,
                    jobs: self_tx,
                    vault,
                };
                for (old, new) in &pairs {
                    record_layered_rename(ctx.layered_cell, old, new);
                    crate::links_rename::on_note_moved(&sweep, store, old, new).await;
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
                .layered_cell
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
                    // status: pm-epic-derived-table
                    // `list_refs` rides the same lifecycle: a trashed
                    // list-like note (epic / plan) drops its member rows.
                    clear_list_refs_for_delete(store, &rel, entry);
                    // `trail_waypoints` likewise has no FK cascade off
                    // `notes`, so a trashed trail-doc / waypoint-note must
                    // drop its derived rows explicitly — mirrors the
                    // `process_delete` cleanup the `IndexJob::Delete` / watcher
                    // path runs.
                    clear_trail_waypoints_for_delete(store, &rel, entry);
                    record_layered_tombstone(ctx.layered_cell, entry);
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
        // status: rule-triggers
        // The `date-passed` sweep — enqueued by the host at vault open and
        // on its daily tick; the per-rule watermark in the store's
        // `meta(key, value)` sidecar makes redundant enqueues free.
        IndexJob::RulesDateSweep => {
            let now_epoch = super::now_secs() as f64;
            ctx.with_rules_engine(store, |engine, fire| engine.date_sweep(fire, now_epoch));
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
    // The layered-doc handle attributes the rule's writes (`auto:inbox`): the
    // tag merge stages + auto-flips, the move records a logical rename.
    let log = ctx.layered_cell.get().map(std::sync::Arc::as_ref);
    match rules.apply_to_created(ctx.vault, store, watcher, log, rel_path) {
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

/// Record a rename in the layered doc so the `doc-index.db` path mapping follows
/// the move (otherwise the moved note's identity orphans and its next save
/// seeds a fresh doc). Best-effort: no
/// layered-doc handle (CLI / tests) or an unmapped path is a silent no-op; a
/// failure is logged, not propagated — the filesystem move already succeeded.
fn record_layered_rename(
    layered_cell: &Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    from: &str,
    to: &str,
) {
    let Some(log) = layered_cell.get() else { return };
    if let Err(e) =
        crate::editing::writes::rename(log, from, to, &crate::editing::shapes::Author::User)
    {
        tracing::warn!(error = %e, %from, %to, "indexer: op-log rename failed");
    }
}

/// Record a tombstone in the layered doc for a soft-deleted entry — the note, plus
/// every member when a folder was deleted. Same best-effort posture as
/// [`record_layered_rename`].
fn record_layered_tombstone(
    layered_cell: &Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    entry: &crate::trash::Entry,
) {
    let Some(log) = layered_cell.get() else { return };
    let paths = match &entry.members {
        Some(members) => members.clone(),
        None => vec![entry.original_path.clone()],
    };
    for path in &paths {
        if let Err(e) =
            crate::editing::writes::tombstone(log, path, &crate::editing::shapes::Author::User)
        {
            tracing::warn!(error = %e, %path, "indexer: op-log tombstone failed");
        }
    }
}

/// Rebind the layered-doc `doc_id` of a restored trash entry so the document comes
/// back under its retained identity. A no-op when the entry has no recorded
/// `doc_id` (hand-dropped file, never-seeded note, or a folder whose members
/// re-bind by path on re-ingest) or when no layered-doc handle is attached
/// (CLI / tests). Best-effort, like [`record_layered_tombstone`]: a failure is
/// logged, not propagated — the filesystem restore already succeeded and the
/// note is recoverable; only its history rebind is at risk.
///
/// status: vault-trash-restore
fn record_layered_restore(
    layered_cell: &Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    entry: &crate::trash::Entry,
) {
    let Some(doc_id) = &entry.doc_id else { return };
    let Some(log) = layered_cell.get() else { return };
    if let Err(e) = crate::editing::writes::restore(
        log,
        doc_id,
        &entry.original_path,
        &crate::editing::shapes::Author::User,
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
    let layered = ctx.layered_cell.get().cloned();
    let kinds = ctx.kinds_cell.get().cloned();
    match process_upsert(
        ctx.vault_root,
        store,
        ctx.embedder.clone(),
        layered.as_deref(),
        kinds.as_deref(),
        rel_path,
        false,
    )
    .await?
    {
        // The inline-upsert callers (rename fallback, trash restore) are
        // not rule trigger seams — detected events are dropped here.
        UpsertOutcome::Indexed(_) => {
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
    /// Ingest landed; carries the rule-pass trigger events the derived-
    /// table diffs detected (`rule-triggers`).
    Indexed(Vec<crate::rules::RuleEvent>),
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
    layered: Option<&LayeredDoc>,
    kinds: Option<&crate::kinds::Registry>,
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
    if crate::ignore::is_ignored_in(vault_root, rel_path, false) {
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

    // status: spec-anchor-index
    // Re-derive the spec-anchor index BEFORE the unchanged short-circuit: a pure text
    // scan (no embedding), cheap enough to run unconditionally — which is also how
    // pre-existing dbs (the table ships without a schema-version bump) backfill on
    // their next full scan instead of waiting for content edits.
    store.replace_spec_anchors(rel_path, &crate::wikilink::scan_spec_anchors(&contents))?;

    let existing = store.get_note_by_path(rel_path)?;
    // Short-circuit: same content + same embedder version → no-op. Skipped
    // when `force` is set so an explicit user reindex actually re-embeds.
    if !force
        && let Some(existing) = &existing
        && existing.content_hash == content_hash
        && existing.embedder_version == embedder.version()
    {
        return Ok(UpsertOutcome::Unchanged);
    }

    // status: rule-triggers
    // Pre-ingest state the rule pass diffs against: whether the note row
    // already existed (a first ingest is creation, never a frontmatter
    // change — so a fresh index over an existing vault fires nothing) and
    // the prior `note_meta` rows (the `frontmatter-changed` before-rows).
    let pre_existing = existing.is_some();
    let prior_meta = match store.note_metadata(rel_path) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, path = %rel_path, "indexer: prior note_meta read failed");
            Vec::new()
        }
    };

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
        let meta_entries = note_metadata_entries(&contents);
        store.replace_note_metadata(rel_path, &meta_entries)?;
        update_note_problems(store, kinds, rel_path, &meta_entries);
        // status: trail-waypoints-derived-table
        // Also re-derive on the empty-body branch — waypoint-notes
        // intentionally have empty bodies (`trail-empty-waypoint-body`),
        // so the FM-only path is the common case for them.
        crate::trails::ingest::update_trail_waypoints_if_relevant(store, layered, rel_path, &contents);
        // status: board-cards-derived-table
        let card_events = update_board_cards_if_relevant(store, layered, kinds, rel_path, &contents);
        // status: pm-epic-derived-table
        update_list_refs_if_relevant(store, kinds, rel_path, &contents);
        return Ok(UpsertOutcome::Indexed(rule_events(
            rel_path,
            pre_existing,
            &prior_meta,
            &meta_entries,
            card_events,
        )));
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
    crate::trails::ingest::update_trail_waypoints_if_relevant(store, layered, rel_path, &contents);
    // status: board-cards-derived-table
    // Re-derive `board_cards` rows for board-docs before the body drops —
    // same ordering rationale as the trail re-derive above.
    let card_events = update_board_cards_if_relevant(store, layered, kinds, rel_path, &contents);
    // status: pm-epic-derived-table
    // Re-derive `list_refs` rows for list-like notes (epic / plan) before
    // the body drops — same ordering rationale.
    update_list_refs_if_relevant(store, kinds, rel_path, &contents);
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
    update_note_problems(store, kinds, rel_path, &meta_entries);

    Ok(UpsertOutcome::Indexed(rule_events(
        rel_path,
        pre_existing,
        &prior_meta,
        &meta_entries,
        card_events,
    )))
}

/// Assemble one ingest's rule-pass trigger events (`docs/rules.md`): the
/// `card-moved` diffs the board re-derive detected, plus a
/// `frontmatter-changed` event when a pre-existing note's indexed metadata
/// changed across `replace_note_metadata`. A first ingest (no prior note
/// row) never reads as a frontmatter change — `note-created` is the
/// watcher-create path's event, folded in by the caller.
///
/// status: rule-triggers
fn rule_events(
    rel_path: &str,
    pre_existing: bool,
    prior_meta: &[MetaEntry],
    new_meta: &[MetaEntry],
    card_events: Vec<crate::rules::RuleEvent>,
) -> Vec<crate::rules::RuleEvent> {
    let mut events = card_events;
    if pre_existing && crate::rules::meta_changed(prior_meta, new_meta) {
        events.push(crate::rules::RuleEvent::FrontmatterChanged {
            path: rel_path.to_string(),
        });
    }
    events
}

/// Re-derive the lenient-validation problems report for a note on ingest
/// (`docs/kinds.md`): when the note's `hiker.kind` names a registered kind,
/// validate the flattened frontmatter against the definition and replace
/// the derived `note_problems` rows; otherwise clear them (unregistered
/// kinds are never validated). Violations only ever produce report rows —
/// the write is never blocked, the file never rewritten. Soft-error: a
/// store failure is logged, never propagated.
///
/// status: kind-lenient-validation
fn update_note_problems(
    store: &mut Store,
    kinds: Option<&crate::kinds::Registry>,
    rel_path: &str,
    entries: &[MetaEntry],
) {
    use crate::kinds::RefTarget;
    let Some(registry) = kinds else { return };
    let kind = entries
        .iter()
        .find(|e| e.key == "hiker.kind")
        .and_then(|e| registry.get(&e.value));
    let problems = match kind {
        Some(kind) => {
            let reader: &Store = store;
            let resolve = |path: &str| -> RefTarget {
                match reader.note_exists(path) {
                    Ok(true) => RefTarget::Found {
                        kind: reader.meta_value(path, "hiker.kind").ok().flatten(),
                    },
                    _ => RefTarget::Missing,
                }
            };
            crate::kinds::validate_note(kind, entries, &resolve)
        }
        None => Vec::new(),
    };
    if let Err(e) = store.replace_note_problems(rel_path, &problems) {
        tracing::warn!(
            error = %e,
            path = %rel_path,
            "indexer: replace_note_problems failed",
        );
    }
}

/// Re-derive `board_cards` rows for a board-doc on ingest. Mirrors
/// `update_trail_waypoints_if_relevant`'s trail-doc path: parse the
/// `hiker.columns` frontmatter, clear every existing row for the board's
/// id, and re-insert one row per card with its column + ordinal. The
/// board-doc frontmatter is the source of truth (clear-then-reinsert).
/// Soft-error: a parse failure (non-board note, mid-edit) is a silent
/// no-op. The registry extends the parse gate to board-like kinds, so a
/// sprint board-doc derives `board_cards` rows exactly like a plain board
/// (`sprint-board-subtype`).
///
/// Returns the `card-moved` trigger events the replace exposed: the prior
/// rows are read before the clear-then-reinsert, and a note card present
/// in both row sets under a different column is a move (`rule-triggers` —
/// DnD, MCP ops, and hand edits all converge at this one derived-table
/// seam). Empty for non-boards and on any soft-error.
///
/// status: board-cards-derived-table
/// status: sprint-board-subtype
/// status: rule-triggers
fn update_board_cards_if_relevant(
    store: &mut Store,
    layered: Option<&LayeredDoc>,
    kinds: Option<&crate::kinds::Registry>,
    rel_path: &str,
    contents: &str,
) -> Vec<crate::rules::RuleEvent> {
    use crate::boards::parse_board_for;
    use crate::store::dto::BoardCardRow;
    if !rel_path.ends_with(".md") {
        return Vec::new();
    }
    let Ok(board) = parse_board_for(rel_path, contents, kinds) else {
        return Vec::new();
    };
    // status: store-path-is-identity
    // The board's storage key is the layered doc's `doc_id` for the
    // board-doc's path; absent (layered not seeded yet) is a soft no-op so
    // the next ingest re-derives once the cell is populated.
    let Some(log) = layered else { return Vec::new() };
    let board_id = match log.doc_id_for_path(rel_path) {
        Ok(Some(id)) => id,
        _ => return Vec::new(),
    };
    // The before-rows for the card-moved diff, read prior to the replace.
    let prior_rows = store.cards_of(&board_id).unwrap_or_default();
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
        return Vec::new();
    }
    crate::rules::card_moves(rel_path, &prior_rows, &rows)
}

/// Re-derive `list_refs` rows for a list-like note (epic / plan / any
/// registered list-like kind) on ingest — the `board_cards` lifecycle
/// exactly (`pm-epic-derived-table`): parse the `hiker.refs` frontmatter
/// through the registry-aware list-doc gate, clear every existing row for
/// the list's path, and re-insert one row per member in order. The
/// frontmatter is the source of truth (clear-then-reinsert); keyed on the
/// note's vault path directly (path-as-identity), so no layered-doc handle is
/// needed. Soft-error: a parse failure (non-list note, mid-edit, no
/// registry attached) is a silent no-op.
///
/// status: pm-epic-derived-table
fn update_list_refs_if_relevant(
    store: &mut Store,
    kinds: Option<&crate::kinds::Registry>,
    rel_path: &str,
    contents: &str,
) {
    if !rel_path.ends_with(".md") {
        return;
    }
    let Ok(doc) = crate::pm::parse_list_doc_for(rel_path, contents, kinds) else {
        return;
    };
    if let Err(e) = store.replace_list_refs(rel_path, &doc.refs) {
        tracing::warn!(error = %e, path = %rel_path, "indexer: replace_list_refs failed");
    }
}

/// Clear the derived `list_refs` rows for a list-like note moved to trash
/// via `IndexJob::DeleteNote`. `list_refs` has no FK cascade off `notes`,
/// so the rows are dropped by list path — a single note (`rel`) plus every
/// member of a deleted folder (`entry.members`), mirroring the board /
/// trail cleanups. Rows pointing at a deleted *member* note stay: the
/// rollup counts a missing member under `backlog` until the user edits the
/// epic. Soft-error: a clear failure is logged, never propagated.
/// status: pm-epic-derived-table
fn clear_list_refs_for_delete(store: &mut Store, rel: &str, entry: &crate::trash::Entry) {
    let mut paths: Vec<&str> = vec![rel];
    if let Some(members) = &entry.members {
        paths.extend(members.iter().map(String::as_str));
    }
    for p in paths {
        if let Err(e) = store.delete_list_refs_by_list(p) {
            tracing::warn!(
                error = %e,
                path = %p,
                "indexer: delete_list_refs_by_list (delete-note) failed",
            );
        }
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
    // status: pm-epic-derived-table
    // Drop any list_refs rows for a deleted list-like note (list_path
    // match). Rows pointing at a deleted *member* stay — the rollup counts
    // a missing member under `backlog` until the user edits the epic.
    if let Err(e) = store.delete_list_refs_by_list(rel_path) {
        tracing::warn!(
            error = %e,
            path = %rel_path,
            "indexer: delete_list_refs_by_list failed",
        );
    }
    // status: store-path-is-identity / ingest-delete-cascade
    Ok(store.delete_note_by_path(rel_path)?)
}

