//! Main indexer task loop. Pulled out of `mod.rs` to keep that file under
//! the project's per-file line cap. The loop owns the writer Store and
//! drives all job dispatch through helpers in `super::jobs`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{broadcast, mpsc, watch, OnceCell};

use crate::embed::{Embedder, EmbedError};
use crate::store::Store;

use super::jobs::{handle_reload_embedder, handle_simple_job, JobCtx};
use super::{
    run_full_scan, submit_embedder_load_task, update_queue_count_idle,
    update_queue_count_in_flight, update_status, update_total_notes, EmbedderLoadTaskPlumbing,
    IndexJob, IndexJobTx, IndexStatus, ProgressEvent,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn indexer_loop<F>(
    vault: crate::vault::Vault,
    vault_root: PathBuf,
    mut store: Store,
    embedder_loader: F,
    mut rx: mpsc::Receiver<IndexJob>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Sender<IndexStatus>,
    pending: Arc<Mutex<HashSet<String>>>,
    embedder_cell: Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    self_tx: IndexJobTx,
    watcher_cell: Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    changes_cell: Arc<OnceCell<Arc<crate::changes::Changes>>>,
    tasks: Option<EmbedderLoadTaskPlumbing>,
) where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    // Load the embedder on a blocking thread. Until it returns, jobs queue
    // up in the mpsc channel without being processed.
    //
    // status: embedder-model-load-as-task
    // Wrap the load in a queue task (if the host provided one) so the
    // user gets a visible row for the work — first-run downloads can
    // take minutes on `bge-m3` and the previous silent path left the
    // user wondering whether anything was happening.
    // Clone the queue handle out of the plumbing so we can keep using
    // it for the hot-reload path after the initial load resolves.
    let tasks_queue: Option<Arc<crate::tasks::Queue>> = tasks.as_ref().map(|p| p.queue.clone());
    let load_task_id = if let Some(p) = tasks.as_ref() {
        Some(submit_embedder_load_task(&p.queue, &p.initial_model_id).await)
    } else {
        None
    };
    let load = tokio::task::spawn_blocking(embedder_loader).await;
    // status: embedder-model-load-as-task
    // Resolve the queue row before falling into the success / failure
    // branches below. Errors on resolve are best-effort — a queue
    // hiccup shouldn't take down the indexer's startup path.
    if let (Some(p), Some(id)) = (tasks.as_ref(), load_task_id.as_ref()) {
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
    let mut embedder: Arc<dyn Embedder> = match load {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "indexer: embedder load failed");
            update_status(&status, |s| s.last_error = Some(format!("embedder load: {e}")));
            let _ = progress.send(ProgressEvent::Error {
                path: None,
                message: format!("embedder load failed: {e}"),
            });
            // Drain the queue emitting one Error per Upsert/Delete/Rename so
            // the UI's outstanding counter actually decrements. FullScan jobs
            // aren't counted in the UI's total (they fan out to per-file
            // jobs) so dropping them silently is fine.
            while let Some(job) = rx.recv().await {
                let path = match job {
                    IndexJob::Upsert { rel_path, .. } | IndexJob::Delete { rel_path } => Some(rel_path),
                    IndexJob::Rename { to, .. } => Some(to),
                    IndexJob::Move { reply, .. } => {
                        let _ = reply.send(Err(crate::error::HikerError::Io(
                            "embedder unavailable".into(),
                        )));
                        None
                    }
                    IndexJob::MoveFolder { reply, .. } => {
                        let _ = reply.send(Err(crate::error::HikerError::Io(
                            "embedder unavailable".into(),
                        )));
                        None
                    }
                    IndexJob::DeleteNote { reply, .. } => {
                        let _ = reply.send(Err(crate::error::HikerError::Io(
                            "embedder unavailable".into(),
                        )));
                        None
                    }
                    IndexJob::RestoreFromTrash { reply, .. } => {
                        let _ = reply.send(Err(crate::error::HikerError::Io(
                            "embedder unavailable".into(),
                        )));
                        None
                    }
                    IndexJob::FullScan { .. } => None,
                    IndexJob::TouchAccess { .. } => None,
                    IndexJob::ReloadEmbedder { reply, .. } => {
                        let _ = reply.send(Err(crate::error::HikerError::Io(
                            "embedder unavailable".into(),
                        )));
                        None
                    }
                };
                if path.is_some() {
                    let _ = progress.send(ProgressEvent::Error {
                        path,
                        message: "embedder unavailable".into(),
                    });
                }
            }
            return;
        }
        Err(e) => {
            update_status(&status, |s| s.last_error = Some(format!("embedder spawn: {e}")));
            return;
        }
    };
    // status: store-rebuild-chunk-vecs-on-dim-change
    // Reseat the `chunk_vecs` table to the loaded embedder's dim before any
    // ingest runs. No-op when the on-disk dim already matches (the common
    // case); otherwise drops + recreates the vec0 table and clears the
    // per-note caches that go stale at the new dim.
    if let Err(e) = store.ensure_chunk_vecs_dim(embedder.dim()) {
        tracing::error!(error = %e, "indexer: ensure_chunk_vecs_dim failed");
        update_status(&status, |s| {
            s.last_error = Some(format!("chunk_vecs rebuild: {e}"));
        });
    }
    update_status(&status, |s| {
        s.model_ready = true;
        s.last_error = None;
    });
    // Publish the loaded embedder so search/related callers can embed
    // query strings off the same model. `ReloadEmbedder` later swaps the
    // inner Arc in place; the cell stays alive across model changes.
    match embedder_cell.write() {
        Ok(mut guard) => *guard = Some(embedder.clone()),
        Err(_) => tracing::error!("indexer: embedder cell lock poisoned at init"),
    }
    tracing::info!(
        embedder_version = embedder.version(),
        dim = embedder.dim(),
        "indexer: embedder ready",
    );
    let _ = progress.send(ProgressEvent::ModelLoaded);

    while let Some(job) = rx.recv().await {
        update_queue_count_in_flight(&status, &rx);
        match job {
            IndexJob::FullScan { force } => match run_full_scan(&vault_root, &store, force) {
                Ok(jobs) => {
                    // Count Upsert/Delete jobs so the UI can show the queue
                    // depth as "Indexing N pending" before we start chewing.
                    let scanned = jobs.len() as u32;
                    let queued = jobs
                        .iter()
                        .filter(|j| {
                            matches!(j, IndexJob::Upsert { .. } | IndexJob::Delete { .. })
                        })
                        .count() as u32;
                    let _ = progress.send(ProgressEvent::ScanComplete { scanned, queued });
                    // Process scan results inline rather than re-enqueueing
                    // through `tx`: the indexer task is both producer and
                    // consumer of that mpsc, so a vault with more than
                    // `channel_capacity` notes would deadlock — `tx.send`
                    // blocks once the buffer fills, but no one is calling
                    // `rx.recv` to drain.
                    for j in jobs {
                        if let IndexJob::Upsert { rel_path, .. } = &j {
                            pending.lock().unwrap().insert(rel_path.clone());
                        }
                        let ctx = JobCtx {
                            vault: &vault,
                            vault_root: &vault_root,
                            embedder: &embedder,
                            progress: &progress,
                            status: &status,
                            pending: &pending,
                            self_tx: &self_tx,
                            watcher_cell: &watcher_cell,
                            changes_cell: &changes_cell,
                        };
                        handle_simple_job(&ctx, &mut store, j).await;
                        update_total_notes(&status, &store);
                    }
                }
                Err(e) => {
                    let msg = format!("{e}");
                    update_status(&status, |s| s.last_error = Some(msg.clone()));
                    let _ = progress.send(ProgressEvent::Error {
                        path: None,
                        message: msg,
                    });
                }
            },
            // status: embedder-hot-reload-on-model-change
            // Handled inline (not via `handle_simple_job`) so the new
            // model can be assigned to the loop-local `embedder`
            // binding; subsequent jobs see the swap immediately.
            IndexJob::ReloadEmbedder { model_id, reply } => {
                handle_reload_embedder(
                    &mut embedder,
                    &embedder_cell,
                    &mut store,
                    &status,
                    &progress,
                    &self_tx,
                    &pending,
                    model_id,
                    reply,
                    tasks_queue.as_ref(),
                )
                .await;
            }
            other => {
                let ctx = JobCtx {
                    vault: &vault,
                    vault_root: &vault_root,
                    embedder: &embedder,
                    progress: &progress,
                    status: &status,
                    pending: &pending,
                    self_tx: &self_tx,
                    watcher_cell: &watcher_cell,
                    changes_cell: &changes_cell,
                };
                handle_simple_job(&ctx, &mut store, other).await;
            }
        }
        update_total_notes(&status, &store);
        // Job done — published queue depth now reflects only the still-queued
        // jobs (no in-flight bump until the next recv).
        update_queue_count_idle(&status, &rx);
    }
}
