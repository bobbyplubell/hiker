//! Indexer task + walker + ingest pipeline. See docs/index.md.
//!
//! Runs as a single tokio task that owns the writer Store, an Arc<dyn
//! Embedder>, and an mpsc inbox of `IndexJob`s. Watcher and command-side
//! callers send jobs in; the task drains them serially. CPU-heavy embedding
//! goes through `spawn_blocking` so it doesn't starve the runtime.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch, OnceCell};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::chunker::{Chunker, MarkdownChunker, TxtChunker};
use crate::embed::{Embedder, EmbedError, FastembedEmbedder};
use crate::hash::hash_str;
use crate::store::{new_id, NoteUpsert, Store, StoreError};
use crate::watcher::{is_ignored, FileEvent};

/// Submit a `TaskKind::EmbedderModelLoad` row to the queue around an
/// active `FastembedEmbedder::load_id` call. Returns the row's task id;
/// the caller resolves it via `Queue::submit_result` / `Queue::fail`
/// when the underlying `spawn_blocking` load returns.
///
/// status: embedder-model-load-as-task
async fn submit_embedder_load_task(
    queue: &Arc<crate::tasks::Queue>,
    model_id: &str,
) -> crate::tasks::TaskId {
    use crate::tasks::{Priority, Task, TaskKind, TaskPayload, TaskShape};
    let task = Task {
        id: String::new(), // queue stamps a ULID
        kind: TaskKind::EmbedderModelLoad {
            model_id: model_id.to_string(),
        },
        priority: Priority::High,
        shape: TaskShape::Direct,
        payload: TaskPayload::default(),
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata: serde_json::Value::Null,
    };
    queue.submit_self_managed(task).await
}

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
    /// Hot-swap the loaded embedder to `model_id`. The indexer loads the
    /// new `FastembedEmbedder` via `spawn_blocking`; on success it swaps
    /// the live `Arc<dyn Embedder>` (visible to the search-query embedder
    /// via the same cell — see `IndexerHandle::embedder`), reseats
    /// `chunk_vecs` to the new dim via `store.ensure_chunk_vecs_dim`, and
    /// enqueues a full vault reindex. On failure the old embedder stays
    /// loaded and the caller (typically `set_setting`) is expected to
    /// roll back any on-disk TOML write. Reply oneshot returns the
    /// outcome.
    ///
    /// status: embedder-hot-reload-on-model-change
    ReloadEmbedder {
        model_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::error::HikerError>>,
    },
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
    /// Filled by the indexer task once `embedder_loader` resolves; later
    /// hot-swapped on `IndexJob::ReloadEmbedder`. Exposed via `embedder()`
    /// so the search command can embed query strings off the same loaded
    /// model without re-loading. Stays `None` while the model is loading
    /// and after a load failure (search returns empty until ready,
    /// mirroring the `embedder-first-run-nonblocking` posture).
    ///
    /// RwLock (not OnceCell) so `ReloadEmbedder` can replace the inner
    /// Arc in place; readers see the swap on their next `embedder()` call.
    ///
    /// status: embedder-hot-reload-on-model-change
    embedder: Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    /// Late-bound watcher reference, used by the trails auto-update path
    /// (`trail-auto-update-on-note-move`). Filled by the host (Tauri /
    /// CLI) via `attach_watcher` after both the indexer and the watcher
    /// have started. CLI / tests that don't run a watcher leave this
    /// empty — `core::trails::on_note_moved` handles the missing-watcher
    /// case as a best-effort no-suppress write.
    watcher_cell: Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    /// Late-bound changelog reference. Same shape as `watcher_cell` —
    /// optional so CLI / tests can run without a changelog.
    changes_cell: Arc<OnceCell<Arc<crate::changes::Changes>>>,
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
    /// command's query-string embedding hop. Picks up the latest
    /// hot-swapped model on each call (`embedder-hot-reload-on-model-change`).
    pub fn embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder.read().ok().and_then(|g| g.clone())
    }

    /// Closure form of `embedder()` — usable across module boundaries
    /// without exporting the cell type. The returned closure clones the
    /// inner Arc on each call (cheap), or yields `None` while the model
    /// is still loading or after a load failure. Reads the live cell on
    /// every invocation so a `ReloadEmbedder` swap is visible to existing
    /// holders of the provider.
    pub fn embedder_provider(&self) -> Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync> {
        let cell = self.embedder.clone();
        Arc::new(move || cell.read().ok().and_then(|g| g.clone()))
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

    /// Late-bind the filesystem watcher used by the trails auto-update
    /// path. The Tauri layer calls this after both the indexer and the
    /// watcher are running. Idempotent first-write-wins per `OnceCell`
    /// semantics — subsequent calls log + ignore.
    ///
    /// status: trail-auto-update-on-note-move
    pub fn attach_watcher(&self, watcher: Arc<crate::watcher::Watcher>) {
        if self.watcher_cell.set(watcher).is_err() {
            tracing::warn!("indexer: watcher_cell already attached; ignoring");
        }
    }

    /// Late-bind the changelog used by the trails auto-update path.
    ///
    /// status: trail-auto-update-on-note-move
    pub fn attach_changes(&self, changes: Arc<crate::changes::Changes>) {
        if self.changes_cell.set(changes).is_err() {
            tracing::warn!("indexer: changes_cell already attached; ignoring");
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

/// Optional task-queue plumbing for the indexer. When `Some`, every
/// `FastembedEmbedder::load_id` call inside the indexer task (first-run
/// startup + hot-reload on `[indexing].model` change) is wrapped in a
/// `TaskKind::EmbedderModelLoad` row so the UI can surface the work.
/// Hosts that don't have a task queue (CLI, tests) pass `None`.
///
/// status: embedder-model-load-as-task
pub struct EmbedderLoadTaskPlumbing {
    pub queue: Arc<crate::tasks::Queue>,
    /// Model id used for the startup load. Hot-reload calls pass their
    /// own id via the `ReloadEmbedder` job payload — this field is just
    /// the seed for the very first load.
    pub initial_model_id: String,
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
    start_indexer_with_tasks(vault, store, embedder_loader, None)
}

/// Same as `start_indexer` but threads a task-queue handle + the
/// initial model id into the loop so the embedder-load work surfaces
/// in the queue. status: embedder-model-load-as-task
pub fn start_indexer_with_tasks<F>(
    vault: crate::vault::Vault,
    store: Store,
    embedder_loader: F,
    tasks: Option<EmbedderLoadTaskPlumbing>,
) -> IndexerHandle
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    let vault_root = vault.root().to_path_buf();
    let (tx, rx) = mpsc::channel::<IndexJob>(256);
    let (progress_tx, _) = broadcast::channel::<ProgressEvent>(PROGRESS_CAPACITY);
    let (status_tx, status_rx) = watch::channel(IndexStatus::default());
    let pending: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let embedder_cell: Arc<RwLock<Option<Arc<dyn Embedder>>>> = Arc::new(RwLock::new(None));

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

    let watcher_cell: Arc<OnceCell<Arc<crate::watcher::Watcher>>> = Arc::new(OnceCell::new());
    let changes_cell: Arc<OnceCell<Arc<crate::changes::Changes>>> = Arc::new(OnceCell::new());

    let progress_for_task = progress_tx.clone();
    let pending_for_task = pending.clone();
    let embedder_cell_for_task = embedder_cell.clone();
    // Build a self-targeting IndexJobTx the loop can hand to
    // `core::trails::on_note_moved` so trail-doc / waypoint-note rewrites
    // re-enqueue Upserts to refresh the derived `trail_waypoints` rows.
    // status: trail-auto-update-on-note-move
    let self_tx = IndexJobTx {
        tx: tx.clone(),
        pending: pending.clone(),
    };
    let watcher_cell_for_task = watcher_cell.clone();
    let changes_cell_for_task = changes_cell.clone();
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
        self_tx,
        watcher_cell_for_task,
        changes_cell_for_task,
        tasks,
    ));

    IndexerHandle {
        tx: Some(tx),
        progress: progress_tx,
        status: status_rx,
        join: Some(join),
        pending,
        embedder: embedder_cell,
        watcher_cell,
        changes_cell,
    }
}

#[allow(clippy::too_many_arguments)]
async fn indexer_loop<F>(
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
                        handle_simple_job(
                            &vault,
                            &vault_root,
                            &mut store,
                            &embedder,
                            &progress,
                            &status,
                            &pending,
                            &self_tx,
                            &watcher_cell,
                            &changes_cell,
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
                handle_simple_job(
                    &vault,
                    &vault_root,
                    &mut store,
                    &embedder,
                    &progress,
                    &status,
                    &pending,
                    &self_tx,
                    &watcher_cell,
                    &changes_cell,
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
#[allow(clippy::too_many_arguments)]
async fn handle_reload_embedder(
    embedder: &mut Arc<dyn Embedder>,
    embedder_cell: &Arc<RwLock<Option<Arc<dyn Embedder>>>>,
    store: &mut Store,
    status: &watch::Sender<IndexStatus>,
    progress: &broadcast::Sender<ProgressEvent>,
    self_tx: &IndexJobTx,
    pending: &Arc<Mutex<HashSet<String>>>,
    model_id: String,
    reply: tokio::sync::oneshot::Sender<Result<(), crate::error::HikerError>>,
    tasks: Option<&Arc<crate::tasks::Queue>>,
) {
    // Same model id → no-op. The set_setting caller already short-circuits
    // on identical TOML values, but defensive: a redundant ReloadEmbedder
    // (e.g. from a future MCP path or test) shouldn't tear down chunk_vecs.
    //
    // status: embedder-model-load-as-task
    // The queue submit happens *after* the short-circuit so a redundant
    // reload doesn't create an empty / instantly-complete row.
    if embedder.version() == model_id {
        let _ = reply.send(Ok(()));
        return;
    }
    tracing::info!(
        from = embedder.version(),
        to = %model_id,
        "indexer: hot-reloading embedder",
    );
    // Enqueue a queue row for the load so the user sees the work in the
    // queue badge / detail page.
    let load_task_id = if let Some(q) = tasks {
        Some(submit_embedder_load_task(q, &model_id).await)
    } else {
        None
    };
    let id_for_load = model_id.clone();
    let load = tokio::task::spawn_blocking(move || {
        FastembedEmbedder::load_id(&id_for_load).map(|e| {
            let arc: Arc<dyn Embedder> = Arc::new(e);
            arc
        })
    })
    .await;
    // Resolve the queue row up front so a downstream `ensure_chunk_vecs_dim`
    // failure doesn't leave the row stuck in Leased. Mirror of the startup
    // path's resolve.
    if let (Some(q), Some(id)) = (tasks, load_task_id.as_ref()) {
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
    let new_embedder: Arc<dyn Embedder> = match load {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            let msg = format!("embedder reload failed: {e}");
            tracing::error!(error = %e, model = %model_id, "indexer: embedder reload failed");
            update_status(status, |s| s.last_error = Some(msg.clone()));
            let _ = progress.send(ProgressEvent::Error { path: None, message: msg.clone() });
            let _ = reply.send(Err(crate::error::HikerError::Io(msg)));
            return;
        }
        Err(e) => {
            let msg = format!("embedder reload spawn: {e}");
            tracing::error!(error = %e, "indexer: embedder reload spawn failed");
            update_status(status, |s| s.last_error = Some(msg.clone()));
            let _ = progress.send(ProgressEvent::Error { path: None, message: msg.clone() });
            let _ = reply.send(Err(crate::error::HikerError::Io(msg)));
            return;
        }
    };
    let new_dim = new_embedder.dim();
    // Swap the live Arc — loop-local first so any same-tick logic uses
    // it, then the shared cell so the search-query embedder picks it up
    // on its next read.
    *embedder = new_embedder.clone();
    match embedder_cell.write() {
        Ok(mut guard) => *guard = Some(new_embedder),
        Err(_) => tracing::error!("indexer: embedder cell lock poisoned on reload"),
    }
    // Reseat chunk_vecs to the new dim. Same helper used at indexer
    // startup; drops + recreates the vec0 table and clears
    // notes.embedder_version so the upcoming reindex actually re-embeds.
    if let Err(e) = store.ensure_chunk_vecs_dim(new_dim) {
        let msg = format!("chunk_vecs rebuild after model swap: {e}");
        tracing::error!(error = %e, "indexer: ensure_chunk_vecs_dim failed after reload");
        update_status(status, |s| s.last_error = Some(msg.clone()));
        let _ = progress.send(ProgressEvent::Error { path: None, message: msg.clone() });
        // The new embedder is already swapped in — the caller has
        // committed to this model. Report the dim-rebuild failure
        // but still enqueue the reindex so we don't leave the queue
        // empty with a half-applied change.
        let _ = reply.send(Err(crate::error::HikerError::Io(msg)));
    } else {
        update_status(status, |s| {
            s.last_error = None;
            s.model_ready = true;
        });
        let _ = progress.send(ProgressEvent::ModelLoaded);
        let _ = reply.send(Ok(()));
    }
    // Enqueue a full vault reindex with `force = true` so the
    // (now-cleared) embedder_version short-circuit doesn't keep any
    // rows from being re-embedded. self_tx tracks pending paths
    // automatically once per-file Upserts fan out inside the loop.
    let _ = self_tx; // appease borrow checker — used below
    let _ = pending; // (the FullScan handler manages pending itself)
    if let Err(e) = self_tx.send(IndexJob::FullScan { force: true }).await {
        tracing::warn!(error = %e, "indexer: failed to enqueue post-reload FullScan");
    }
}

/// Dispatch a single non-FullScan job. Extracted so the FullScan handler can
/// call it directly on its scan results without re-entering the mpsc.
#[allow(clippy::too_many_arguments)]
async fn handle_simple_job(
    vault: &crate::vault::Vault,
    vault_root: &Path,
    store: &mut Store,
    embedder: &Arc<dyn Embedder>,
    progress: &broadcast::Sender<ProgressEvent>,
    status: &watch::Sender<IndexStatus>,
    pending: &Arc<Mutex<HashSet<String>>>,
    self_tx: &IndexJobTx,
    watcher_cell: &Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    changes_cell: &Arc<OnceCell<Arc<crate::changes::Changes>>>,
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
                // status: trail-auto-update-on-note-move
                // Watcher-driven external rename: run the trails update.
                run_trails_on_note_moved(
                    watcher_cell, self_tx, vault, changes_cell, store, &from, &to,
                )
                .await;
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
            // status: trail-auto-update-on-note-move
            // After the path remap succeeds, sweep trails referencing the
            // moved note. Errors are swallowed (logged inside the helper)
            // so a partial trails update never fails the move's reply.
            if result.is_ok() {
                run_trails_on_note_moved(
                    watcher_cell, self_tx, vault, changes_cell, store, &from, &to,
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
                        watcher_cell, self_tx, vault, changes_cell, store, old, new,
                    )
                    .await;
                }
            }
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
/// `watcher_cell` / `changes_cell` so callers don't need to know whether
/// the host has wired them — CLI / tests run with neither attached and
/// the sweep degrades to a write-without-suppress / no-changelog shape.
/// Errors are swallowed (logged inside `core::trails::on_note_moved`).
///
/// status: trail-auto-update-on-note-move
async fn run_trails_on_note_moved(
    watcher_cell: &Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    self_tx: &IndexJobTx,
    vault: &crate::vault::Vault,
    changes_cell: &Arc<OnceCell<Arc<crate::changes::Changes>>>,
    store: &mut Store,
    from: &str,
    to: &str,
) {
    let watcher_arc = watcher_cell.get().cloned();
    let changes_arc = changes_cell.get().cloned();
    let watcher_ref = watcher_arc.as_deref();
    let changes_ref = changes_arc.as_ref();
    if let Err(e) = crate::trails::on_note_moved(
        watcher_ref,
        Some(self_tx),
        vault,
        changes_ref,
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
        // Adopt frontmatter `hiker.id` when present and `path_ids` empty,
        // same as the chunked branch — see
        // `bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids`.
        let id = match store.id_for_path(rel_path)? {
            Some(id) => id,
            None => frontmatter_hiker_id(&contents).unwrap_or_else(new_id),
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
        // status: trail-waypoints-derived-table
        // Also re-derive on the empty-body branch — waypoint-notes
        // intentionally have empty bodies (`trail-empty-waypoint-body`),
        // so the FM-only path is the common case for them.
        update_trail_waypoints_if_relevant(store, rel_path, &contents);
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

    // status: trail-waypoints-derived-table
    // After the standard notes/chunks upsert, also re-derive the
    // `trail_waypoints` rows if this file is a trail-doc or waypoint.
    // Parse failures here are soft errors (the file might be mid-edit) —
    // warn-and-continue rather than failing the whole ingest.
    update_trail_waypoints_if_relevant(store, rel_path, &contents);

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
    use crate::trails::{parse_trail_doc_for, parse_waypoint, walk_waypoints_depth_first};
    use crate::store::WaypointRow;

    // Cheap kind discriminator: only attempt the parse on `.md` files.
    if !rel_path.ends_with(".md") {
        return;
    }
    let is_under_waypoints =
        rel_path.starts_with(".hiker/trails/") && rel_path.contains("/waypoints/");

    if is_under_waypoints {
        match parse_waypoint(contents) {
            Ok(fm) => {
                // Source id comes from the index lookup at ingest time;
                // may be None if the source hasn't been indexed yet.
                let source_id = match store.id_for_path(&fm.references.path) {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %rel_path,
                            "indexer: source id_for_path lookup failed",
                        );
                        None
                    }
                };
                let row = WaypointRow {
                    waypoint_path: rel_path.to_string(),
                    waypoint_id: fm.id,
                    trail_id: fm.in_trail.id,
                    source_id,
                    source_path: fm.references.path,
                    // Tree-position columns are owned by the trail-doc
                    // ingest path; written as the empty / NULL default
                    // here. The trail-doc ingest that follows
                    // `append_waypoint` enqueues both, so the canonical
                    // values land within the same indexer drain.
                    parent_waypoint_id: None,
                    tree_path: String::new(),
                };
                if let Err(e) = store.upsert_trail_waypoint(&row) {
                    tracing::warn!(
                        error = %e,
                        path = %rel_path,
                        "indexer: upsert_trail_waypoint failed",
                    );
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    path = %rel_path,
                    "indexer: waypoint parse failed (file may be mid-edit)",
                );
            }
        }
        return;
    }

    // Trail-doc ingest: clear + re-insert every row for `trail_id` so
    // tree-shape changes (re-parent, reorder, remove) propagate to the
    // derived table. Frontmatter is the source of truth.
    //
    // status: trail-waypoints-derived-table
    // status: trail-side-trail-shape
    if let Ok(fm) = parse_trail_doc_for(rel_path, contents) {
        let trail_id = fm.id.clone();
        // Capture existing rows BEFORE the clear so we can preserve
        // each row's `source_id` / `source_path` (those columns are
        // owned by the per-waypoint ingest path and aren't
        // recoverable from the trail-doc alone).
        let existing_by_path: std::collections::HashMap<String, (Option<String>, String)> =
            store
                .waypoints_of(&trail_id)
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.waypoint_path, (r.source_id, r.source_path)))
                .collect();
        if let Err(e) = store.delete_trail_waypoints_by_trail(&trail_id) {
            tracing::warn!(
                error = %e,
                trail_id = %trail_id,
                "indexer: delete_trail_waypoints_by_trail failed",
            );
        }
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

/// Read `hiker.id` from a note's frontmatter, returning None if there's
/// no frontmatter, no `hiker:` block, or no `id:` field. Used by
/// `process_upsert` to keep `path_ids` in lockstep with whatever id the
/// file already declares — avoids the "two ULIDs for one note" failure
/// mode that produced
/// `bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids`.
fn frontmatter_hiker_id(contents: &str) -> Option<String> {
    let split = crate::frontmatter::split(contents);
    let fm = split.frontmatter?;
    let serde_yml::Value::Mapping(map) = fm else { return None };
    let serde_yml::Value::Mapping(hiker) = map.get("hiker")? else { return None };
    hiker.get("id")?.as_str().map(|s| s.to_string())
}

fn process_delete(store: &mut Store, rel_path: &str) -> Result<bool, IndexerError> {
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
    // TODO(note-id-stamping): when `[indexing] id_stamping = "all"`, the
    // startup scan should walk every md note here and lazy-stamp its
    // `hiker.id` via `core::ops::ensure_note_id_stamped` for any note
    // missing one. Slice 1 lands the helper + config; slice 2 wires it
    // here (and at trail/waypoint creation time, which is the lazy-mode
    // trigger). For now this scan is `lazy`-mode-shaped only.
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

    // status: trail-waypoints-derived-table
    #[tokio::test]
    async fn ingesting_trail_doc_and_waypoint_populates_derived_table() {
        let dir = tempdir().unwrap();
        let trail_id = "01HTRAILTEST";
        let waypoint_id = "01HWPTEST";
        let source_id = "01HSRCTEST"; // not used directly; source uses path-based lookup
        let _ = source_id;

        // Write the source note first so the indexer assigns an id we can
        // look up on the waypoint's source_id column.
        std::fs::create_dir_all(dir.path().join("research")).unwrap();
        std::fs::write(
            dir.path().join("research/raptor.md"),
            "# Raptor\n\nbody.\n",
        )
        .unwrap();

        // Trail-doc.
        std::fs::create_dir_all(dir.path().join("trails")).unwrap();
        let trail_doc = format!(
            "---\nhiker:\n  kind: trail\n  id: {trail_id}\n  waypoints:\n    - id: {waypoint_id}\n      path: .hiker/trails/{trail_id}/waypoints/0001--raptor.md\n---\nbody\n"
        );
        std::fs::write(dir.path().join("trails/my-trail.md"), trail_doc).unwrap();

        // Waypoint-note.
        let waypoint_dir = dir
            .path()
            .join(format!(".hiker/trails/{trail_id}/waypoints"));
        std::fs::create_dir_all(&waypoint_dir).unwrap();
        let wp = format!(
            "---\nhiker:\n  kind: waypoint\n  id: {waypoint_id}\n  references:\n    id: WILLBELOOKEDUP\n    path: research/raptor.md\n  in_trail:\n    id: {trail_id}\n    path: trails/my-trail.md\n---\n"
        );
        std::fs::write(waypoint_dir.join("0001--raptor.md"), wp).unwrap();

        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(
            crate::vault::Vault::open(dir.path()).unwrap(),
            store,
            mock_loader(),
        );
        let mut prog = handle.subscribe_progress();
        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

        // Index source first so its id is available when the waypoint
        // ingests; then trail-doc; then waypoint-note.
        handle.index_path("research/raptor.md").await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == "research/raptor.md")
        })
        .await;

        handle
            .index_path(format!(
                ".hiker/trails/{trail_id}/waypoints/0001--raptor.md"
            ))
            .await
            .unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path }
                if path.ends_with("0001--raptor.md"))
        })
        .await;

        // Trail-doc ingested AFTER the waypoint-note so the depth-first
        // walk sees the per-row `source_path` and produces canonical
        // `parent_waypoint_id` + `tree_path` values. Mirrors
        // `append_waypoint`'s waypoint-then-trail-doc enqueue order.
        handle.index_path("trails/my-trail.md").await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == "trails/my-trail.md")
        })
        .await;

        // Verify derived rows.
        let store2 = Store::open(dir.path()).unwrap();
        let waypoints = store2.waypoints_of(trail_id).unwrap();
        assert_eq!(waypoints.len(), 1);
        assert_eq!(waypoints[0].waypoint_id, waypoint_id);
        assert_eq!(waypoints[0].tree_path, "1");
        assert_eq!(waypoints[0].source_path, "research/raptor.md");
        assert!(waypoints[0].parent_waypoint_id.is_none());
        // source_id was looked up via the just-ingested source note.
        assert!(waypoints[0].source_id.is_some());

        let containing = store2.trails_containing_note("research/raptor.md").unwrap();
        assert_eq!(containing.len(), 1);
        assert_eq!(containing[0].trail_id, trail_id);
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
