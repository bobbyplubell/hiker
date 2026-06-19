//! Main indexer task loop. Pulled out of `mod.rs` to keep that file under
//! the project's per-file line cap. The loop owns the writer Store and
//! drives all job dispatch through helpers in `super::jobs`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{broadcast, mpsc, watch, OnceCell};

use crate::embed::{Embedder, Error};
use crate::store::Store;

// Only the most heavily-used parent items are imported; the rest are reached
// via explicit `super::` paths at their use sites so this file doesn't lean on
// a wide slice of its parent's namespace (per `check-splits` super-reach).
use super::jobs::handle_simple_job;
use super::{update_status, IndexJob, ProgressEvent};

/// Long-lived state for the indexer task loop. Bundled into a struct so the
/// driver entry point is a `self`-method (exempt from `single_call_fn`)
/// without needing to thread a 13-arg free function.
pub(super) struct IndexerLoop<F>
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, Error> + Send + 'static,
{
    pub vault: crate::vault::Vault,
    pub vault_root: PathBuf,
    pub store: Store,
    pub embedder_loader: F,
    pub rx: mpsc::Receiver<IndexJob>,
    pub progress: broadcast::Sender<ProgressEvent>,
    pub status: watch::Sender<super::IndexStatus>,
    pub pending: Arc<Mutex<HashSet<String>>>,
    pub embedder_cell: Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    pub self_tx: super::IndexJobTx,
    pub watcher_cell: Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    pub layered_cell: Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    /// status: inbox-rules
    pub inbox_cell: Arc<OnceCell<Arc<crate::inbox::Rules>>>,
    /// status: kind-lenient-validation
    pub kinds_cell: Arc<OnceCell<Arc<crate::kinds::Registry>>>,
    /// status: rule-triggers
    pub rules_cell: Arc<OnceCell<Arc<crate::rules::Engine>>>,
    pub tasks: Option<super::EmbedderLoadTaskPlumbing>,
}

impl<F> IndexerLoop<F>
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, Error> + Send + 'static,
{
pub(super) async fn run(self) {
    let IndexerLoop {
        vault,
        vault_root,
        mut store,
        embedder_loader,
        mut rx,
        progress,
        status,
        pending,
        embedder_cell,
        self_tx,
        watcher_cell,
        layered_cell,
        inbox_cell,
        kinds_cell,
        rules_cell,
        tasks,
    } = self;
    let tasks_queue: Option<Arc<crate::tasks::queue::Queue>> = tasks.as_ref().map(|p| p.queue.clone());

    // The remaining work is structured as methods on a `LoopState` borrow-
    // bundle so each phase reads as a `self`-method (exempt from
    // `single_call_fn`) and the orchestration stays under the cognitive
    // complexity cap.
    let state = LoopState {
        vault: &vault,
        vault_root: &vault_root,
        progress: &progress,
        status: &status,
        pending: &pending,
        embedder_cell: &embedder_cell,
        self_tx: &self_tx,
        watcher_cell: &watcher_cell,
        layered_cell: &layered_cell,
        inbox_cell: &inbox_cell,
        kinds_cell: &kinds_cell,
        rules_cell: &rules_cell,
        tasks_queue: tasks_queue.as_ref(),
    };

    let mut embedder = match state.load_and_resolve(embedder_loader, tasks.as_ref()).await {
        Some(e) => e,
        None => {
            state.drain_pending_on_failure(&mut rx).await;
            return;
        }
    };
    state.publish_embedder_ready(&mut store, &embedder);
    state.prune_ignored_tracked_docs(&mut store);

    while let Some(job) = rx.recv().await {
        // Called right after `recv().await` returns: the just-pulled job
        // is no longer in `rx` but is about to be processed, so include
        // it in the published count rather than flashing 0.
        {
            let len = rx.len() as u32 + 1;
            update_status(state.status, |s| s.queued = len);
        }
        state.handle_one_job(&mut store, &mut embedder, job).await;
        super::update_total_notes(state.status, &store);
        // Job done — published queue depth now reflects only the still-queued
        // jobs (no in-flight bump until the next recv).
        {
            let len = rx.len() as u32;
            update_status(state.status, |s| s.queued = len);
        }
    }
}

}

/// Borrowed state passed through the indexer loop's phase methods. Method
/// receivers keep each helper exempt from `clippy::single_call_fn` and let
/// the phases share the writer Store + status / progress / pending plumbing
/// without 10-arg signatures.
struct LoopState<'a> {
    vault: &'a crate::vault::Vault,
    vault_root: &'a PathBuf,
    progress: &'a broadcast::Sender<ProgressEvent>,
    status: &'a watch::Sender<super::IndexStatus>,
    pending: &'a Arc<Mutex<HashSet<String>>>,
    embedder_cell: &'a Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    self_tx: &'a super::IndexJobTx,
    watcher_cell: &'a Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    layered_cell: &'a Arc<OnceCell<Arc<crate::editing::LayeredDoc>>>,
    /// status: inbox-rules
    inbox_cell: &'a Arc<OnceCell<Arc<crate::inbox::Rules>>>,
    /// status: kind-lenient-validation
    kinds_cell: &'a Arc<OnceCell<Arc<crate::kinds::Registry>>>,
    /// status: rule-triggers
    rules_cell: &'a Arc<OnceCell<Arc<crate::rules::Engine>>>,
    tasks_queue: Option<&'a Arc<crate::tasks::queue::Queue>>,
}

impl<'a> LoopState<'a> {
    /// Drive the initial embedder load on `spawn_blocking`. Resolves the
    /// queue row, posts status / progress on failure, and returns the
    /// loaded embedder (or `None` if the load couldn't produce one).
    async fn load_and_resolve<F>(
        &self,
        embedder_loader: F,
        tasks: Option<&super::EmbedderLoadTaskPlumbing>,
    ) -> Option<Arc<dyn Embedder>>
    where
        F: FnOnce() -> Result<Arc<dyn Embedder>, Error> + Send + 'static,
    {
        let load_task_id = if let Some(p) = tasks {
            Some(super::submit_embedder_load_task(&p.queue, &p.initial_model_id).await)
        } else {
            None
        };
        let load = tokio::task::spawn_blocking(embedder_loader).await;
        // status: embedder-model-load-as-task
        // Resolve the queue row before falling into the success / failure
        // branches below. Errors on resolve are best-effort — a queue
        // hiccup shouldn't take down the indexer's startup path.
        if let (Some(p), Some(id)) = (tasks, load_task_id.as_ref()) {
            match &load {
                Ok(Ok(_)) => {
                    let _ = p
                        .queue
                        .submit_result(id, serde_json::json!({ "ok": true }))
                        .await;
                }
                Ok(Err(e)) => {
                    let _ = p.queue.fail(id, format!("embedder load: {e}")).await;
                }
                Err(e) => {
                    let _ = p.queue.fail(id, format!("embedder spawn: {e}")).await;
                }
            }
        }
        match load {
            Ok(Ok(e)) => Some(e),
            Ok(Err(e)) => {
                tracing::error!(error = %e, "indexer: embedder load failed");
                update_status(self.status, |s| s.last_error = Some(format!("embedder load: {e}")));
                let _ = self.progress.send(ProgressEvent::Error {
                    path: None,
                    message: format!("embedder load failed: {e}"),
                });
                None
            }
            Err(e) => {
                update_status(self.status, |s| s.last_error = Some(format!("embedder spawn: {e}")));
                None
            }
        }
    }

    /// Drain the queue emitting one Error per Upsert/Delete/Rename so the
    /// UI's outstanding counter actually decrements. FullScan jobs aren't
    /// counted in the UI's total (they fan out to per-file jobs) so
    /// dropping them silently is fine. Reply-bearing jobs receive an
    /// `embedder unavailable` error.
    async fn drain_pending_on_failure(&self, rx: &mut mpsc::Receiver<IndexJob>) {
        while let Some(job) = rx.recv().await {
            let path = match job {
                IndexJob::Upsert { rel_path, .. }
                | IndexJob::Created { rel_path }
                | IndexJob::Delete { rel_path } => Some(rel_path),
                IndexJob::Rename { to, .. } => Some(to),
                IndexJob::Move { reply, .. } => {
                    let _ = reply.send(Err(crate::errors::HikerError::Io("embedder unavailable".into())));
                    None
                }
                IndexJob::MoveFolder { reply, .. } => {
                    let _ = reply.send(Err(crate::errors::HikerError::Io("embedder unavailable".into())));
                    None
                }
                IndexJob::DeleteNote { reply, .. } => {
                    let _ = reply.send(Err(crate::errors::HikerError::Io("embedder unavailable".into())));
                    None
                }
                IndexJob::RestoreFromTrash { reply, .. } => {
                    let _ = reply.send(Err(crate::errors::HikerError::Io("embedder unavailable".into())));
                    None
                }
                IndexJob::ReloadEmbedder { reply, .. } => {
                    let _ = reply.send(Err(crate::errors::HikerError::Io("embedder unavailable".into())));
                    None
                }
                IndexJob::FullScan { .. }
                | IndexJob::TouchAccess { .. }
                | IndexJob::RulesDateSweep => None,
            };
            if path.is_some() {
                let _ = self.progress.send(ProgressEvent::Error {
                    path,
                    message: "embedder unavailable".into(),
                });
            }
        }
    }

    /// One-time startup pass: untrack docs whose path is now excluded by the
    /// composed ignore matcher but were seeded into the layered doc + search index
    /// before the ignore rule existed (or before the ingest seams were
    /// unified onto the matcher) — e.g. a nested repo's test-fixture `.txt`
    /// captured before a `.hikerignore` excluded them. For each tracked
    /// layered doc whose file STILL EXISTS on disk yet is now ignored, drop the
    /// layered doc (`forget_document` — not a tombstone-to-trash; the file was
    /// never meant to be tracked, and is left untouched on disk) and remove
    /// its search-index rows. A gone-from-disk ignored path is NOT forgotten
    /// here — that's a real delete, handled by reconcile / the watcher with
    /// the trash safety net. No-op without an attached layered doc (CLI / tests).
    ///
    /// status: op-log-doc-id-bootstrap
    fn prune_ignored_tracked_docs(&self, store: &mut Store) {
        let Some(log) = self.layered_cell.get() else { return };
        // Tracked docs are the vault's indexable files the layered doc has a document
        // for. The `.ops`-scan enumeration is gone (the engine is retired), so
        // walk the vault and keep paths the layered doc tracks (a `.md` on disk under
        // path-identity = a tracked doc). A still-on-disk file that is now
        // ignored is what this prunes; a gone file is a real delete handled
        // elsewhere.
        let candidates = match self.vault.walk_indexable_files("") {
            Ok(rels) => rels,
            Err(e) => {
                tracing::warn!(error = %e, "prune-ignored: vault walk failed");
                return;
            }
        };
        let doc_ids: Vec<String> = candidates
            .into_iter()
            .filter(|rel| matches!(log.doc_id_for_path(rel), Ok(Some(_))))
            .collect();
        let mut pruned = 0usize;
        for doc_id in &doc_ids {
            if self.untrack_if_ignored(log, store, doc_id) {
                pruned += 1;
            }
        }
        if pruned > 0 {
            tracing::info!(pruned, "prune-ignored: untracked now-ignored docs seeded before the ignore rule");
        }
    }

    /// Untrack `doc_id` iff its file still exists but is now ignored: drop the
    /// layered doc (`forget_document`) and its search-index rows. Returns
    /// whether it was untracked. A forget failure aborts before the store
    /// delete so the two never disagree.
    fn untrack_if_ignored(
        &self,
        log: &crate::editing::LayeredDoc,
        store: &mut Store,
        doc_id: &str,
    ) -> bool {
        let Some(rel) = self.prunable_ignored_path(log, doc_id) else {
            return false;
        };
        if let Err(e) = log.forget_document(doc_id) {
            tracing::warn!(path = %rel, error = %e, "prune-ignored: forget_document failed");
            return false;
        }
        if let Err(e) = store.delete_note_by_path(&rel) {
            tracing::warn!(path = %rel, error = %e, "prune-ignored: store delete failed");
        }
        true
    }

    /// The vault-relative path to untrack for `doc_id`, or `None` to keep it:
    /// a tracked doc whose file STILL EXISTS but is now excluded by the
    /// composed ignore matcher. A gone-from-disk ignored path returns `None`
    /// (a genuine delete — reconcile tombstones it to trash with the
    /// recoverability the prune deliberately does not provide).
    fn prunable_ignored_path(&self, log: &crate::editing::LayeredDoc, doc_id: &str) -> Option<String> {
        let rel = log.path_for_doc(doc_id).ok().flatten()?;
        let ignored = crate::ignore::is_ignored_in(self.vault_root, &rel, false);
        let exists = self.vault_root.join(&rel).exists();
        (ignored && exists).then_some(rel)
    }

    /// Reseat chunk_vecs to the loaded embedder's dim, publish the shared
    /// cell, and emit `ProgressEvent::ModelLoaded`.
    fn publish_embedder_ready(&self, store: &mut Store, embedder: &Arc<dyn Embedder>) {
        // status: store-rebuild-chunk-vecs-on-dim-change
        // Reseat the `chunk_vecs` table to the loaded embedder's dim
        // before any ingest runs. No-op when the on-disk dim already
        // matches (the common case); otherwise drops + recreates the vec0
        // table and clears the per-note caches that go stale at the new
        // dim.
        if let Err(e) = store.ensure_chunk_vecs_dim(embedder.dim()) {
            tracing::error!(error = %e, "indexer: ensure_chunk_vecs_dim failed");
            update_status(self.status, |s| {
                s.last_error = Some(format!("chunk_vecs rebuild: {e}"));
            });
        }
        update_status(self.status, |s| {
            s.model_ready = true;
            s.last_error = None;
        });
        // Publish the loaded embedder so search/related callers can embed
        // query strings off the same model. `ReloadEmbedder` later swaps
        // the inner Arc in place; the cell stays alive across model
        // changes.
        match self.embedder_cell.write() {
            Ok(mut guard) => *guard = Some(embedder.clone()),
            Err(_) => tracing::error!("indexer: embedder cell lock poisoned at init"),
        }
        tracing::info!(
            embedder_version = embedder.version(),
            dim = embedder.dim(),
            "indexer: embedder ready",
        );
        let _ = self.progress.send(ProgressEvent::ModelLoaded);
    }

    /// Dispatch one `IndexJob` from the loop. FullScan + ReloadEmbedder
    /// are special-cased; the rest fall through to `handle_simple_job`.
    async fn handle_one_job(
        &self,
        store: &mut Store,
        embedder: &mut Arc<dyn Embedder>,
        job: IndexJob,
    ) {
        match job {
            IndexJob::FullScan { force } => {
                self.handle_full_scan(store, embedder, force).await;
            }
            // status: embedder-hot-reload-on-model-change
            // Handled inline (not via `handle_simple_job`) so the new
            // model can be assigned to the loop-local `embedder`
            // binding; subsequent jobs see the swap immediately.
            IndexJob::ReloadEmbedder { model_id, reply } => {
                let ctx = super::jobs::ReloadCtx {
                    embedder,
                    embedder_cell: self.embedder_cell,
                    store,
                    status: self.status,
                    progress: self.progress,
                    self_tx: self.self_tx,
                    tasks: self.tasks_queue,
                };
                ctx.run(model_id, reply).await;
            }
            other => {
                let ctx = super::jobs::JobCtx {
                    vault: self.vault,
                    vault_root: self.vault_root,
                    embedder,
                    progress: self.progress,
                    status: self.status,
                    pending: self.pending,
                    self_tx: self.self_tx,
                    watcher_cell: self.watcher_cell,
                    layered_cell: self.layered_cell,
                    inbox_cell: self.inbox_cell,
                    kinds_cell: self.kinds_cell,
                    rules_cell: self.rules_cell,
                };
                handle_simple_job(&ctx, store, other).await;
            }
        }
    }

    async fn handle_full_scan(
        &self,
        store: &mut Store,
        embedder: &Arc<dyn Embedder>,
        force: bool,
    ) {
        let jobs = match super::run_full_scan(self.vault_root, store, force) {
            Ok(jobs) => jobs,
            Err(e) => {
                let msg = format!("{e}");
                update_status(self.status, |s| s.last_error = Some(msg.clone()));
                let _ = self.progress.send(ProgressEvent::Error {
                    path: None,
                    message: msg,
                });
                return;
            }
        };
        // Count Upsert/Delete jobs so the UI can show the queue depth as
        // "Indexing N pending" before we start chewing.
        let scanned = jobs.len() as u32;
        let queued = jobs
            .iter()
            .filter(|j| matches!(j, IndexJob::Upsert { .. } | IndexJob::Delete { .. }))
            .count() as u32;
        let _ = self.progress.send(ProgressEvent::ScanComplete { scanned, queued });
        // Pre-populate the `pending` set with every Upsert path up front
        // so `pending_count()` reflects total work remaining throughout
        // the scan, not just the one currently in flight. Each
        // `handle_upsert_job` call removes its path on completion, so the
        // count counts down from N to 0 as the loop progresses.
        {
            let mut p = self.pending.lock().unwrap();
            for j in &jobs {
                if let IndexJob::Upsert { rel_path, .. } = j {
                    p.insert(rel_path.clone());
                }
            }
        }
        // Process scan results inline rather than re-enqueueing through
        // `tx`: the indexer task is both producer and consumer of that
        // mpsc, so a vault with more than `channel_capacity` notes would
        // deadlock — `tx.send` blocks once the buffer fills, but no one
        // is calling `rx.recv` to drain.
        for j in jobs {
            if let IndexJob::Upsert { rel_path, .. } = &j {
                self.pending.lock().unwrap().insert(rel_path.clone());
            }
            let ctx = super::jobs::JobCtx {
                vault: self.vault,
                vault_root: self.vault_root,
                embedder,
                progress: self.progress,
                status: self.status,
                pending: self.pending,
                self_tx: self.self_tx,
                watcher_cell: self.watcher_cell,
                layered_cell: self.layered_cell,
                inbox_cell: self.inbox_cell,
                kinds_cell: self.kinds_cell,
                rules_cell: self.rules_cell,
            };
            handle_simple_job(&ctx, store, j).await;
            super::update_total_notes(self.status, store);
        }
    }
}

