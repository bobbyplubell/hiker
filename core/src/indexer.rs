//! Indexer task + walker + ingest pipeline. See docs/index.md.
//!
//! Runs as a single tokio task that owns the writer Store, an Arc<dyn
//! Embedder>, and an mpsc inbox of `IndexJob`s. Watcher and command-side
//! callers send jobs in; the task drains them serially. CPU-heavy embedding
//! goes through `spawn_blocking` so it doesn't starve the runtime.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch, OnceCell};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::chunker::{Chunker, MarkdownChunker, TxtChunker};
use crate::embed::{Embedder, EmbedError};
use crate::hash::hash_str;
use crate::store::{new_id, NoteUpsert, Store, StoreError};
use crate::watcher::{is_ignored, FileEvent};

const PROGRESS_CAPACITY: usize = 256;
/// Max markdown size we'll attempt to index, in bytes. Larger files are
/// almost certainly not handwritten markdown (committed binaries, generated
/// dumps); skipping them keeps the embedder from thrashing.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum IndexJob {
    /// Index a single file. `force = true` bypasses the content_hash +
    /// embedder_version short-circuit so an explicit user reindex actually
    /// re-embeds even when bytes are unchanged.
    Upsert { rel_path: String, force: bool },
    Delete { rel_path: String },
    Rename { from: String, to: String },
    /// Explicit move requested by a UI/CLI caller — fs rename + index update
    /// in one shot, executed on the indexer task so all writes flow through
    /// the indexer's owned store connection. Reply oneshot returns the
    /// outcome to the requester.
    Move {
        from: String,
        to: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::error::HikerError>>,
    },
    /// Folder-scoped move: fs rename of the whole directory + bulk index
    /// path update for every contained `.md` member, on the indexer's owned
    /// store. Reply oneshot returns the outcome.
    MoveFolder {
        from: String,
        to: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::error::HikerError>>,
    },
    /// Soft-delete requested by a UI/CLI caller — fs move into vault trash +
    /// store cascade in one shot, on the indexer task. Reply oneshot returns
    /// the resulting `TrashEntry` so the caller can drive an undo toast or
    /// CLI confirmation without a second roundtrip.
    DeleteNote {
        rel: String,
        reply: tokio::sync::oneshot::Sender<
            Result<crate::trash::TrashEntry, crate::error::HikerError>,
        >,
    },
    /// Restore a previously soft-deleted entry from the vault trash. Same
    /// reply pattern as DeleteNote — caller awaits the entry shape so the UI
    /// can confirm.
    RestoreFromTrash {
        id: String,
        reply: tokio::sync::oneshot::Sender<
            Result<crate::trash::TrashEntry, crate::error::HikerError>,
        >,
    },
    /// Walk the vault, enqueuing Upserts for `.md` files and Deletes for
    /// indexed paths whose files have vanished. `force` propagates into the
    /// produced child Upserts (see `IndexJob::Upsert`).
    FullScan { force: bool },
    /// Stamp `notes.last_accessed_at` for an opened note. Fire-and-forget —
    /// no reply, no progress event; the recents widget reads the column on
    /// next refresh. No-op when the path isn't yet indexed.
    ///
    /// status: note-access-tracking
    TouchAccess { rel_path: String, ts: i64 },
}

/// Snapshot of indexer state, served to the UI on demand.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStatus {
    /// True once the embedder has finished loading.
    pub model_ready: bool,
    /// Approximate count of jobs sitting in the queue right now.
    pub queued: u32,
    /// Notes currently in the index (live count from the store).
    pub total_notes: u32,
    /// Last error message surfaced by the indexer task, if any.
    pub last_error: Option<String>,
}

/// Streaming progress event, sent over a broadcast channel for the Tauri
/// bridge to forward as `hiker:reindex-progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressEvent {
    ModelLoaded,
    Started { path: String },
    Finished { path: String },
    Skipped { path: String, reason: String },
    Deleted { path: String },
    Renamed { from: String, to: String },
    ScanComplete { scanned: u32, queued: u32 },
    Error { path: Option<String>, message: String },
}

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("embed: {0}")]
    Embed(#[from] EmbedError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("send failed: indexer task is shut down")]
    SendFailed,
}

/// Caller-side handle. Cheap to clone fields (everything inside is `Arc` or
/// a `Sender`); the handle itself is single-owner so `shutdown` can move and
/// drop the sender to signal the task.
pub struct IndexerHandle {
    // `Option` so `shutdown` can take + drop it (dropping the sole remaining
    // sender is what closes the mpsc so the task's `rx.recv()` returns None).
    tx: Option<mpsc::Sender<IndexJob>>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Receiver<IndexStatus>,
    join: Option<JoinHandle<()>>,
    /// Vault-relative paths with an in-flight Upsert job (queued in the
    /// mpsc, recv'd, or actively processing). Backs the Queued tree-row
    /// marker via `tauri-cmd-file-index-state`.
    pending: Arc<Mutex<HashSet<String>>>,
    /// Filled by the indexer task once `embedder_loader` resolves.
    /// Exposed via `embedder()` so the search command can embed query
    /// strings off the same loaded model without re-loading. Stays
    /// `None` while the model is loading and after a load failure
    /// (search returns empty until ready, mirroring the
    /// `embedder-first-run-nonblocking` posture).
    embedder: Arc<OnceCell<Arc<dyn Embedder>>>,
}

/// Thin wrapper around the indexer's mpsc sender that auto-tracks Upsert
/// paths in the `pending` set so callers don't have to remember.
#[derive(Clone)]
pub struct IndexJobTx {
    tx: mpsc::Sender<IndexJob>,
    pending: Arc<Mutex<HashSet<String>>>,
}

impl IndexJobTx {
    pub async fn send(
        &self,
        job: IndexJob,
    ) -> Result<(), mpsc::error::SendError<IndexJob>> {
        if let IndexJob::Upsert { rel_path, .. } = &job {
            self.pending.lock().unwrap().insert(rel_path.clone());
        }
        self.tx.send(job).await
    }
}

impl IndexerHandle {
    fn tx(&self) -> &mpsc::Sender<IndexJob> {
        self.tx
            .as_ref()
            .expect("indexer sender used after shutdown")
    }

    pub async fn enqueue(&self, job: IndexJob) -> Result<(), IndexerError> {
        if let IndexJob::Upsert { rel_path, .. } = &job {
            self.pending.lock().unwrap().insert(rel_path.clone());
        }
        self.tx().send(job).await.map_err(|_| IndexerError::SendFailed)
    }

    /// Whether the file at `rel_path` currently has an in-flight Upsert job
    /// (queued in the mpsc, recv'd by the loop, or actively processing).
    /// Backs the Queued tree-row marker.
    pub fn is_pending(&self, rel_path: &str) -> bool {
        self.pending.lock().unwrap().contains(rel_path)
    }

    pub async fn index_path(&self, rel_path: impl Into<String>) -> Result<(), IndexerError> {
        self.enqueue(IndexJob::Upsert { rel_path: rel_path.into(), force: false }).await
    }

    pub async fn full_scan(&self) -> Result<(), IndexerError> {
        self.enqueue(IndexJob::FullScan { force: false }).await
    }

    /// Stamp a note's `last_accessed_at` via the indexer's owned writer.
    /// Fire-and-forget — caller can drop the future without losing the stamp
    /// once it's queued. No-op when the note isn't yet indexed.
    ///
    /// status: note-access-tracking
    pub async fn touch_access(
        &self,
        rel_path: impl Into<String>,
        ts: i64,
    ) -> Result<(), IndexerError> {
        self.enqueue(IndexJob::TouchAccess {
            rel_path: rel_path.into(),
            ts,
        })
        .await
    }


    pub fn status(&self) -> IndexStatus {
        self.status.borrow().clone()
    }

    /// Loaded embedder, or `None` while the model is still loading or
    /// after a load failure. Cheap clone (Arc); intended for the search
    /// command's query-string embedding hop.
    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder.get().cloned()
    }

    /// Closure form of `embedder()` — usable across module boundaries
    /// without exporting the `OnceCell` type. The returned closure clones
    /// the inner Arc on each call (cheap), or yields `None` while the
    /// model is still loading or after a load failure.
    pub fn embedder_provider(&self) -> Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync> {
        let cell = self.embedder.clone();
        Arc::new(move || cell.get().cloned())
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressEvent> {
        self.progress.subscribe()
    }

    /// Subscribe to status changes (model_ready / queued / total_notes /
    /// last_error). The Tauri bridge forwards each change as
    /// `hiker:index-status` so the frontend can drop its 2s `index_status`
    /// poll. Initial value is observable via `borrow()` on the receiver
    /// without waiting for the next change.
    pub fn subscribe_status(&self) -> watch::Receiver<IndexStatus> {
        self.status.clone()
    }

    /// Clone the auto-tracking job sender. Each `send(IndexJob::Upsert{..})`
    /// updates the pending-paths set so `is_pending` reflects queued jobs
    /// without the caller having to remember.
    pub fn job_sender(&self) -> IndexJobTx {
        IndexJobTx {
            tx: self.tx().clone(),
            pending: self.pending.clone(),
        }
    }

    /// Stop the indexer task gracefully and wait for it to finish.
    pub async fn shutdown(mut self) {
        // Drop the held sender so the task's `recv()` returns `None`. Any
        // outstanding clones (e.g. those passed to `route_watcher_events`
        // via `job_sender`) will be dropped when their tasks see the
        // broadcast close — we only need to ensure *this* sender goes away.
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), join).await;
        }
    }
}

/// Construct the indexer. The embedder begins loading on a blocking thread
/// immediately; jobs queued before load completes wait until it does.
///
/// `store` is moved into the indexer task (writer ownership). `embedder` is
/// created by a closure so the (slow, fallible) load happens *inside* the
/// task, giving callers a non-blocking startup.
pub fn start_indexer<F>(
    vault: crate::vault::Vault,
    store: Store,
    embedder_loader: F,
) -> IndexerHandle
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    let vault_root = vault.root().to_path_buf();
    let (tx, rx) = mpsc::channel::<IndexJob>(256);
    let (progress_tx, _) = broadcast::channel::<ProgressEvent>(PROGRESS_CAPACITY);
    let (status_tx, status_rx) = watch::channel(IndexStatus::default());
    let pending: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let embedder_cell: Arc<OnceCell<Arc<dyn Embedder>>> = Arc::new(OnceCell::new());

    // Seed total_notes from the store; surface a count failure as
    // last_error rather than silently showing 0 (a corrupted index would
    // otherwise look like an empty vault).
    let (initial_total, initial_error) = match count_notes(&store) {
        Ok(n) => (n, None),
        Err(e) => (0, Some(format!("count_notes failed: {e}"))),
    };
    let _ = status_tx.send(IndexStatus {
        total_notes: initial_total,
        last_error: initial_error,
        ..IndexStatus::default()
    });

    let progress_for_task = progress_tx.clone();
    let pending_for_task = pending.clone();
    let embedder_cell_for_task = embedder_cell.clone();
    let join = tokio::spawn(indexer_loop(
        vault,
        vault_root,
        store,
        embedder_loader,
        rx,
        progress_for_task,
        status_tx,
        pending_for_task,
        embedder_cell_for_task,
    ));

    IndexerHandle {
        tx: Some(tx),
        progress: progress_tx,
        status: status_rx,
        join: Some(join),
        pending,
        embedder: embedder_cell,
    }
}

async fn indexer_loop<F>(
    vault: crate::vault::Vault,
    vault_root: PathBuf,
    mut store: Store,
    embedder_loader: F,
    mut rx: mpsc::Receiver<IndexJob>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Sender<IndexStatus>,
    pending: Arc<Mutex<HashSet<String>>>,
    embedder_cell: Arc<OnceCell<Arc<dyn Embedder>>>,
) where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    // Load the embedder on a blocking thread. Until it returns, jobs queue
    // up in the mpsc channel without being processed.
    let load = tokio::task::spawn_blocking(embedder_loader).await;
    let embedder: Arc<dyn Embedder> = match load {
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
    update_status(&status, |s| {
        s.model_ready = true;
        s.last_error = None;
    });
    // Publish the loaded embedder so search/related callers can embed
    // query strings off the same model. `set` only fails if the cell was
    // already filled (we never fill it elsewhere), so the error is unreachable
    // — log defensively and move on rather than panicking.
    if embedder_cell.set(embedder.clone()).is_err() {
        tracing::error!("indexer: embedder OnceCell already filled");
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
                        handle_simple_job(
                            &vault,
                            &vault_root,
                            &mut store,
                            &embedder,
                            &progress,
                            &status,
                            &pending,
                            j,
                        )
                        .await;
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
            other => {
                handle_simple_job(
                    &vault,
                    &vault_root,
                    &mut store,
                    &embedder,
                    &progress,
                    &status,
                    &pending,
                    other,
                )
                .await;
            }
        }
        update_total_notes(&status, &store);
        // Job done — published queue depth now reflects only the still-queued
        // jobs (no in-flight bump until the next recv).
        update_queue_count_idle(&status, &rx);
    }
}

/// Dispatch a single non-FullScan job. Extracted so the FullScan handler can
/// call it directly on its scan results without re-entering the mpsc.
async fn handle_simple_job(
    vault: &crate::vault::Vault,
    vault_root: &Path,
    store: &mut Store,
    embedder: &Arc<dyn Embedder>,
    progress: &broadcast::Sender<ProgressEvent>,
    status: &watch::Sender<IndexStatus>,
    pending: &Arc<Mutex<HashSet<String>>>,
    job: IndexJob,
) {
    match job {
        IndexJob::Upsert { rel_path, force } => {
            // Make sure the path is in the pending set even if it didn't go
            // through a tracking sender (e.g. enqueued by some legacy path);
            // remove on every terminal outcome below.
            pending.lock().unwrap().insert(rel_path.clone());
            let _ = progress.send(ProgressEvent::Started { path: rel_path.clone() });
            let outcome = process_upsert(vault_root, store, embedder.clone(), &rel_path, force).await;
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
        IndexJob::Rename { from, to } => match process_rename(store, &from, &to) {
            Ok(true) => {
                let _ = progress.send(ProgressEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
            Ok(false) => {
                if let Err(e) = handle_inline_upsert(
                    vault_root,
                    store,
                    embedder.clone(),
                    progress,
                    status,
                    &to,
                )
                .await
                {
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
        },
        IndexJob::Move { from, to, reply } => {
            // The Tauri layer suppresses the watcher around this; we don't
            // need to here. Run vault::move_note on the indexer's owned
            // store so all writes flow through one connection.
            let result = crate::vault::move_note(vault, store, None, &from, &to);
            let _ = reply.send(result);
        }
        IndexJob::MoveFolder { from, to, reply } => {
            let result = crate::vault::move_folder(vault, store, None, &from, &to);
            let _ = reply.send(result);
        }
        IndexJob::DeleteNote { rel, reply } => {
            // Same shape as Move — Tauri layer handles watcher suppression
            // around the call. The trash handle is cheap to construct
            // (just a path) so we build one per call rather than threading
            // it through the loop signature.
            let trash = crate::trash::Trash::open(vault.root());
            let result = crate::vault::delete_note(vault, store, None, &trash, &rel);
            match &result {
                Ok(entry) => {
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
            let trash = crate::trash::Trash::open(vault.root());
            let restore_result = crate::vault::restore_note(vault, None, &trash, &id);
            let result = match restore_result {
                Ok(entry) => {
                    // Re-ingest the restored .md files inline so the index
                    // picks them up without waiting on watcher events
                    // (which the Tauri layer suppressed). For folders, walk
                    // the manifest's recorded members; for files, just the
                    // single original_path.
                    let to_index: Vec<String> = match &entry.members {
                        Some(m) => m.clone(),
                        None => vec![entry.original_path.clone()],
                    };
                    for rel_path in &to_index {
                        if let Err(e) = handle_inline_upsert(
                            vault_root,
                            store,
                            embedder.clone(),
                            progress,
                            status,
                            rel_path,
                        )
                        .await
                        {
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
                        path: Some(id.clone()),
                        message: msg,
                    });
                    Err(e)
                }
            };
            let _ = reply.send(result);
        }
        IndexJob::FullScan { .. } => {
            unreachable!("FullScan must be handled by the main loop, not handle_simple_job");
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

/// Convenience for the rename "from-path-not-indexed" branch: do an upsert
/// without re-emitting Started/Finished pairs.
async fn handle_inline_upsert(
    vault_root: &Path,
    store: &mut Store,
    embedder: Arc<dyn Embedder>,
    progress: &broadcast::Sender<ProgressEvent>,
    status: &watch::Sender<IndexStatus>,
    rel_path: &str,
) -> Result<(), IndexerError> {
    let _ = progress.send(ProgressEvent::Started { path: rel_path.to_string() });
    match process_upsert(vault_root, store, embedder, rel_path, false).await? {
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

async fn process_upsert(
    vault_root: &Path,
    store: &mut Store,
    embedder: Arc<dyn Embedder>,
    rel_path: &str,
    force: bool,
) -> Result<UpsertOutcome, IndexerError> {
    let chunker: &dyn Chunker = match path_extension(rel_path) {
        Some(ext) if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") => {
            &MarkdownChunker
        }
        Some(ext) if ext.eq_ignore_ascii_case("txt") => &TxtChunker,
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
        // (per index.md `tauri-cmd-file-index-state`). Reason string is the
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
    let content_hash = hash_str(&contents);

    // Short-circuit: same content + same embedder version → no-op. Skipped
    // when `force` is set so an explicit user reindex actually re-embeds.
    if !force {
        if let Some(existing) = store.get_note_by_path(rel_path)? {
            if existing.content_hash == content_hash
                && existing.embedder_version == embedder.version()
            {
                return Ok(UpsertOutcome::Unchanged);
            }
        }
    }

    // Chunk + embed. Embed is CPU-heavy: spawn_blocking.
    let chunks = chunker.chunk(&contents);
    if chunks.is_empty() {
        // Empty note: still record the row so deletes/renames work, but no
        // embeddings to insert.
        let id = match store.id_for_path(rel_path)? {
            Some(id) => id,
            None => new_id(),
        };
        let indexed_at = now_secs();
        store.upsert_note(NoteUpsert {
            id: &id,
            path: rel_path,
            content_hash: &content_hash,
            mtime,
            size: size as i64,
            indexed_at,
            embedder_version: embedder.version(),
            chunks: Vec::new(),
        })?;
        return Ok(UpsertOutcome::Indexed);
    }

    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let batch_size = texts.len();
    let emb_clone = embedder.clone();
    let embed_start = std::time::Instant::now();
    let embeddings = tokio::task::spawn_blocking(move || emb_clone.embed_batch(&texts))
        .await
        .map_err(|e| IndexerError::Embed(EmbedError::Embed(e.to_string())))??;
    tracing::debug!(
        batch_size,
        elapsed_ms = embed_start.elapsed().as_millis() as u64,
        path = %rel_path,
        "embedder: batch embedded",
    );

    let id = match store.id_for_path(rel_path)? {
        Some(id) => id,
        None => new_id(),
    };
    let indexed_at = now_secs();
    let zipped: Vec<_> = chunks.into_iter().zip(embeddings.into_iter()).collect();
    store.upsert_note(NoteUpsert {
        id: &id,
        path: rel_path,
        content_hash: &content_hash,
        mtime,
        size: size as i64,
        indexed_at,
        embedder_version: embedder.version(),
        chunks: zipped,
    })?;
    Ok(UpsertOutcome::Indexed)
}

fn process_delete(store: &mut Store, rel_path: &str) -> Result<bool, IndexerError> {
    let id = match store.id_for_path(rel_path)? {
        Some(id) => id,
        None => return Ok(false),
    };
    store.delete_note(&id)?;
    Ok(true)
}

fn process_rename(store: &mut Store, from: &str, to: &str) -> Result<bool, IndexerError> {
    let id = match store.id_for_path(from)? {
        Some(id) => id,
        None => return Ok(false),
    };
    store.rename_note(&id, to)?;
    Ok(true)
}

/// Walk the vault, returning the jobs the indexer should run to bring the
/// store in line with the filesystem. Upserts for every `.md` file found,
/// Deletes for indexed paths whose files have vanished.
pub fn run_full_scan(vault_root: &Path, store: &Store, force: bool) -> Result<Vec<IndexJob>, IndexerError> {
    tracing::info!(
        vault_root = %vault_root.display(),
        force,
        "full_scan starting",
    );
    let mut on_disk: Vec<String> = Vec::new();
    let mut total_files_seen = 0_u32;
    let mut filtered_non_md = 0_u32;
    let mut filtered_ignored = 0_u32;
    let walker = WalkDir::new(vault_root).follow_links(false).into_iter();
    for entry in walker.filter_entry(|e| !walk_skip(vault_root, e.path())) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "full_scan: walk error");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        total_files_seen += 1;
        let rel = match entry.path().strip_prefix(vault_root) {
            Ok(p) => path_to_rel(p),
            Err(_) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    vault_root = %vault_root.display(),
                    "full_scan: strip_prefix failed",
                );
                continue;
            }
        };
        if is_ignored(&rel) {
            filtered_ignored += 1;
            continue;
        }
        if !is_indexable_path(&rel) {
            filtered_non_md += 1;
            continue;
        }
        on_disk.push(rel);
    }
    let mut jobs: Vec<IndexJob> = on_disk
        .iter()
        .cloned()
        .map(|rel_path| IndexJob::Upsert { rel_path, force })
        .collect();

    // Find indexed paths missing from disk.
    let indexed = store.all_note_paths()?;
    let on_disk_set: std::collections::HashSet<&str> =
        on_disk.iter().map(String::as_str).collect();
    let mut deleted = 0_u32;
    for path in indexed {
        if !on_disk_set.contains(path.as_str()) {
            jobs.push(IndexJob::Delete { rel_path: path });
            deleted += 1;
        }
    }

    tracing::info!(
        seen = total_files_seen,
        non_md = filtered_non_md,
        ignored = filtered_ignored,
        queued = on_disk.len() as u32,
        deleted,
        "full_scan complete",
    );

    Ok(jobs)
}

/// Route filesystem events into the indexer's job queue. Filters to `.md`
/// files (other types are ignored in v1 per index.md vault tolerance) and
/// translates each event kind into the matching IndexJob. Runs until the
/// broadcast receiver lags out or the indexer's sender closes.
pub async fn route_watcher_events(
    mut rx: broadcast::Receiver<FileEvent>,
    tx: IndexJobTx,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let job = match ev {
                    FileEvent::Created { path } | FileEvent::Modified { path } => {
                        if !is_indexable_path(&path) {
                            continue;
                        }
                        IndexJob::Upsert { rel_path: path, force: false }
                    }
                    FileEvent::Deleted { path } => {
                        if !is_indexable_path(&path) {
                            continue;
                        }
                        IndexJob::Delete { rel_path: path }
                    }
                    FileEvent::Renamed { from, to } => {
                        // Rename involving an unsupported extension on both
                        // sides means neither side is/was indexed.
                        if !is_indexable_path(&from) && !is_indexable_path(&to) {
                            continue;
                        }
                        IndexJob::Rename { from, to }
                    }
                    FileEvent::Overflow => IndexJob::FullScan { force: false },
                };
                if tx.send(job).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // We dropped events; the safe recovery is a full rescan.
                let _ = tx.send(IndexJob::FullScan { force: false }).await;
            }
        }
    }
}

/// Pre-filter for the walker: skip whole subtrees we never want to enter.
/// We can't rely on `is_ignored` alone because that takes a relative path,
/// and `walkdir`'s `filter_entry` runs with absolute paths.
fn walk_skip(vault_root: &Path, path: &Path) -> bool {
    if path == vault_root {
        return false;
    }
    let rel = match path.strip_prefix(vault_root) {
        Ok(p) => path_to_rel(p),
        Err(_) => return true,
    };
    if rel.is_empty() {
        return false;
    }
    is_ignored(&rel)
}

/// Lowercased file extension of a vault-relative path, or None for paths with
/// no extension (or a trailing dot only).
fn path_extension(rel_path: &str) -> Option<&str> {
    let basename = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let dot = basename.rfind('.')?;
    let ext = &basename[dot + 1..];
    if ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

/// Canonical list of extensions the indexer can chunk + embed. Single
/// source of truth — the walker, watcher router, per-file chunker dispatch,
/// `Vault::walk_indexable_files`, and the frontend tree-row markers all
/// consult this list (the frontend gets it via the `Config` snapshot
/// returned by `get_settings`, see settings.md). Lower-case; comparisons
/// elsewhere use `eq_ignore_ascii_case`.
// status: txt-extension-recognized
pub const INDEXABLE_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

/// Whether the indexer considers this path's extension supported.
pub fn is_indexable_path(rel_path: &str) -> bool {
    let Some(ext) = path_extension(rel_path) else { return false };
    INDEXABLE_EXTENSIONS
        .iter()
        .any(|e| ext.eq_ignore_ascii_case(e))
}

fn path_to_rel(p: &Path) -> String {
    let mut out = String::new();
    for (i, comp) in p.components().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    out
}

fn count_notes(store: &Store) -> Result<u32, StoreError> {
    store.count_notes()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn update_status(status: &watch::Sender<IndexStatus>, mut f: impl FnMut(&mut IndexStatus)) {
    status.send_modify(|s| f(s));
}

fn update_total_notes(status: &watch::Sender<IndexStatus>, store: &Store) {
    match count_notes(store) {
        Ok(n) => update_status(status, |s| {
            s.total_notes = n;
            // Successful count clears any prior count error; real indexing
            // errors are pinned through their own update_status calls.
            if let Some(msg) = &s.last_error {
                if msg.starts_with("count_notes failed") {
                    s.last_error = None;
                }
            }
        }),
        Err(e) => {
            update_status(status, |s| {
                s.last_error = Some(format!("count_notes failed: {e}"));
            });
        }
    }
}

// Called right after `recv().await` returns: the just-pulled job is no
// longer in `rx` but is about to be processed, so include it in the
// published count rather than flashing 0 between recv and the next event.
fn update_queue_count_in_flight(
    status: &watch::Sender<IndexStatus>,
    rx: &mpsc::Receiver<IndexJob>,
) {
    let len = rx.len() as u32 + 1;
    update_status(status, |s| s.queued = len);
}

fn update_queue_count_idle(status: &watch::Sender<IndexStatus>, rx: &mpsc::Receiver<IndexJob>) {
    let len = rx.len() as u32;
    update_status(status, |s| s.queued = len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::MockEmbedder;
    use std::fs;
    use tempfile::tempdir;

    fn mock_loader() -> impl FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static {
        || Ok(Arc::new(MockEmbedder::new("mock-v1")) as Arc<dyn Embedder>)
    }

    async fn await_event<F>(rx: &mut broadcast::Receiver<ProgressEvent>, pred: F) -> ProgressEvent
    where
        F: Fn(&ProgressEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timed out waiting for progress event")
                .expect("progress channel closed");
            if pred(&ev) {
                return ev;
            }
        }
    }

    #[tokio::test]
    async fn indexer_indexes_a_markdown_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("alpha.md"), b"# Alpha\n\nbody.\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        // Wait for ModelLoaded so the loader future has resolved.
        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

        handle.index_path("alpha.md").await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == "alpha.md")
        })
        .await;

        // Reopen a reader Store and verify rows.
        let store2 = Store::open(dir.path()).unwrap();
        let note = store2.get_note_by_path("alpha.md").unwrap().unwrap();
        let chunks = store2.get_note_chunks(&note.id).unwrap();
        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn unchanged_file_is_skipped_on_second_index() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("x.md"), b"# X\n\nbody.\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

        handle.index_path("x.md").await.unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

        handle.index_path("x.md").await.unwrap();
        let ev = await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Skipped { reason, .. } if reason == "unchanged")
                || matches!(e, ProgressEvent::Finished { .. })
        })
        .await;
        assert!(matches!(ev, ProgressEvent::Skipped { .. }));
    }

    #[tokio::test]
    async fn force_reindex_bypasses_unchanged_short_circuit() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("y.md"), b"# Y\n\nbody.\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

        handle.index_path("y.md").await.unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

        // force = true → identical bytes still produce a Finished, not Skipped.
        handle
            .job_sender()
            .send(IndexJob::Upsert { rel_path: "y.md".into(), force: true })
            .await
            .unwrap();
        let ev = await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path, .. } if path == "y.md")
                || matches!(e, ProgressEvent::Skipped { path, .. } if path == "y.md")
        })
        .await;
        assert!(matches!(ev, ProgressEvent::Finished { .. }));
    }

    #[tokio::test]
    async fn deleting_a_note_removes_it_from_the_index() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("doomed.md"), b"# x\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        handle.index_path("doomed.md").await.unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

        handle
            .enqueue(IndexJob::Delete { rel_path: "doomed.md".into() })
            .await
            .unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Deleted { .. })).await;

        let store2 = Store::open(dir.path()).unwrap();
        assert!(store2.get_note_by_path("doomed.md").unwrap().is_none());
    }

    #[tokio::test]
    async fn renaming_preserves_id() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("old.md"), b"# x\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        handle.index_path("old.md").await.unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

        let store_check = Store::open(dir.path()).unwrap();
        let id_before = store_check
            .get_note_by_path("old.md")
            .unwrap()
            .unwrap()
            .id;
        drop(store_check);

        handle
            .enqueue(IndexJob::Rename {
                from: "old.md".into(),
                to: "new.md".into(),
            })
            .await
            .unwrap();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::Renamed { .. })).await;

        let store_after = Store::open(dir.path()).unwrap();
        assert!(store_after.get_note_by_path("old.md").unwrap().is_none());
        let after = store_after.get_note_by_path("new.md").unwrap().unwrap();
        assert_eq!(after.id, id_before);
    }

    #[test]
    fn full_scan_finds_md_files_and_skips_hiker_dir() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"a").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.md"), b"b").unwrap();
        fs::write(dir.path().join("c.log"), b"not indexed").unwrap();
        // .hiker/ subtree must be skipped.
        fs::create_dir_all(dir.path().join(".hiker/refs")).unwrap();
        fs::write(dir.path().join(".hiker/refs/secret.md"), b"x").unwrap();

        let store = Store::open(dir.path()).unwrap();
        let jobs = run_full_scan(dir.path(), &store, false).unwrap();
        let upserts: Vec<&String> = jobs
            .iter()
            .filter_map(|j| match j {
                IndexJob::Upsert { rel_path, .. } => Some(rel_path),
                _ => None,
            })
            .collect();
        assert!(upserts.iter().any(|p| p.as_str() == "a.md"));
        assert!(upserts.iter().any(|p| p.as_str() == "sub/b.md"));
        assert!(!upserts.iter().any(|p| p.contains(".hiker")));
        assert!(!upserts.iter().any(|p| p.ends_with("c.log")));
    }

    #[test]
    fn full_scan_emits_delete_for_missing_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("present.md"), b"x").unwrap();

        // Pre-populate the store with a note for a path that doesn't exist
        // on disk.
        let mut store = Store::open(dir.path()).unwrap();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "ghost.md",
                content_hash: "h",
                mtime: 0,
                size: 0,
                indexed_at: 0,
                embedder_version: "mock-v1",
                chunks: Vec::new(),
            })
            .unwrap();

        let jobs = run_full_scan(dir.path(), &store, false).unwrap();
        let deletes: Vec<&String> = jobs
            .iter()
            .filter_map(|j| match j {
                IndexJob::Delete { rel_path } => Some(rel_path),
                _ => None,
            })
            .collect();
        assert!(deletes.iter().any(|p| p.as_str() == "ghost.md"));
    }

    #[tokio::test]
    async fn missing_file_during_upsert_is_treated_as_delete() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        // Upsert a path that doesn't exist on disk.
        handle.index_path("nope.md").await.unwrap();
        let ev = await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Skipped { .. } | ProgressEvent::Error { .. })
        })
        .await;
        // No panic, no error — just a skip with reason about missing-on-disk.
        assert!(matches!(ev, ProgressEvent::Skipped { .. }));
    }

    #[tokio::test]
    async fn unsupported_extensions_are_skipped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), b"x").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        handle.index_path("a.bin").await.unwrap();
        let ev = await_event(&mut prog, |e| matches!(e, ProgressEvent::Skipped { .. })).await;
        if let ProgressEvent::Skipped { reason, .. } = ev {
            assert_eq!(reason, "unsupported extension");
        }
    }

    #[tokio::test]
    async fn txt_files_are_indexed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), b"first paragraph.\n\nsecond paragraph.\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        handle.index_path("note.txt").await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == "note.txt")
        })
        .await;

        let store2 = Store::open(dir.path()).unwrap();
        let note = store2.get_note_by_path("note.txt").unwrap().unwrap();
        let chunks = store2.get_note_chunks(&note.id).unwrap();
        assert!(!chunks.is_empty());
        assert!(chunks.iter().any(|c| c.text.contains("first paragraph")));
        assert!(chunks.iter().any(|c| c.text.contains("second paragraph")));
    }
}
