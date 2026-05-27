//! Indexer task + walker + ingest pipeline. See docs/index.md.
//!
//! Runs as a single tokio task that owns the writer Store, an Arc<dyn
//! Embedder>, and an mpsc inbox of `IndexJob`s. Watcher and command-side
//! callers send jobs in; the task drains them serially. CPU-heavy embedding
//! goes through `spawn_blocking` so it doesn't starve the runtime.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch, OnceCell};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::embed::{Embedder, Error as EmbedError};
use crate::store::error::Error as StoreError;
use crate::store::Store;
use crate::watcher::{is_ignored, FileEvent};

mod jobs;
mod scheduler;

#[cfg(test)]
mod tests;


/// Submit a `TaskKind::EmbedderModelLoad` row to the queue around an
/// active `FastembedEmbedder::load_id` call. Returns the row's task id;
/// the caller resolves it via `Queue::submit_result` / `Queue::fail`
/// when the underlying `spawn_blocking` load returns.
///
/// status: embedder-model-load-as-task
pub(super) async fn submit_embedder_load_task(
    queue: &Arc<crate::tasks::queue::Queue>,
    model_id: &str,
) -> crate::tasks::types::TaskId {
    use crate::tasks::types::{Priority, Task, TaskKind, TaskPayload, TaskShape};
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
/// Max file size we'll attempt to index, in bytes. Past this size a file
/// is almost certainly not handwritten markdown (committed binaries,
/// generated dumps, vendored corpora). Memory growth from large files is
/// bounded separately by chunked embedding + early body drop in
/// `process_upsert`, so this cap exists to skip pathological inputs
/// rather than to bound per-file allocation.
pub(super) const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

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
        reply: tokio::sync::oneshot::Sender<Result<(), crate::errors::HikerError>>,
    },
    /// Folder-scoped move: fs rename of the whole directory + bulk index
    /// path update for every contained `.md` member, on the indexer's owned
    /// store. Reply oneshot returns the outcome.
    MoveFolder {
        from: String,
        to: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::errors::HikerError>>,
    },
    /// Soft-delete requested by a UI/CLI caller — fs move into vault trash +
    /// store cascade in one shot, on the indexer task. Reply oneshot returns
    /// the resulting `Entry` so the caller can drive an undo toast or
    /// CLI confirmation without a second roundtrip.
    DeleteNote {
        rel: String,
        reply: tokio::sync::oneshot::Sender<
            Result<crate::trash::Entry, crate::errors::HikerError>,
        >,
    },
    /// Restore a previously soft-deleted entry from the vault trash. Same
    /// reply pattern as DeleteNote — caller awaits the entry shape so the UI
    /// can confirm.
    RestoreFromTrash {
        id: String,
        reply: tokio::sync::oneshot::Sender<
            Result<crate::trash::Entry, crate::errors::HikerError>,
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
    /// via the same cell — see `Handle::embedder`), reseats
    /// `chunk_vecs` to the new dim via `store.ensure_chunk_vecs_dim`, and
    /// enqueues a full vault reindex. On failure the old embedder stays
    /// loaded and the caller (typically `set_setting`) is expected to
    /// roll back any on-disk TOML write. Reply oneshot returns the
    /// outcome.
    ///
    /// status: embedder-hot-reload-on-model-change
    ReloadEmbedder {
        model_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), crate::errors::HikerError>>,
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

/// Streaming progress event, sent over a broadcast channel for the
/// app bridge to forward as `hiker:reindex-progress`.
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
pub enum Error {
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
pub struct Handle {
    // `Option` so `shutdown` can take + drop it (dropping the sole remaining
    // sender is what closes the mpsc so the task's `rx.recv()` returns None).
    tx: Option<mpsc::Sender<IndexJob>>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Receiver<IndexStatus>,
    join: Option<JoinHandle<()>>,
    /// Vault-relative paths with an in-flight Upsert job (queued in the
    /// mpsc, recv'd, or actively processing). Backs the Queued tree-row
    /// marker via `cmd-file-index-state`.
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
    /// (`trail-auto-update-on-note-move`). Filled by the host (app /
    /// CLI) via `attach_watcher` after both the indexer and the watcher
    /// have started. CLI / tests that don't run a watcher leave this
    /// empty — `core::trails::on_note_moved` handles the missing-watcher
    /// case as a best-effort no-suppress write.
    watcher_cell: Arc<OnceCell<Arc<crate::watcher::Watcher>>>,
    /// Late-bound op-log handle. Filled by the host via `attach_oplog` after
    /// both the indexer and the op log have opened. The file-lifecycle jobs
    /// (`Move` / `MoveFolder` / `DeleteNote`) record the rename / tombstone in
    /// the op log through this so the `doc-index.db` mapping follows the move
    /// and the history feed sees deletes. CLI / tests without an op log leave
    /// it empty and the jobs skip the op-log update.
    oplog_cell: Arc<OnceCell<Arc<crate::oplog::OpLog>>>,
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

impl Handle {
    const fn tx(&self) -> &mpsc::Sender<IndexJob> {
        self.tx
            .as_ref()
            .expect("indexer sender used after shutdown")
    }

    pub async fn enqueue(&self, job: IndexJob) -> Result<(), Error> {
        if let IndexJob::Upsert { rel_path, .. } = &job {
            self.pending.lock().unwrap().insert(rel_path.clone());
        }
        self.tx().send(job).await.map_err(|_| Error::SendFailed)
    }

    /// Whether the file at `rel_path` currently has an in-flight Upsert job
    /// (queued in the mpsc, recv'd by the loop, or actively processing).
    /// Backs the Queued tree-row marker.
    pub fn is_pending(&self, rel_path: &str) -> bool {
        self.pending.lock().unwrap().contains(rel_path)
    }

    /// Snapshot the set of paths currently queued for indexing. The result
    /// is sorted alphabetically. UI uses this to populate the Queue panel
    /// without holding the `pending` mutex across rendering.
    pub fn pending_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.pending.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }

    /// Count of paths with an in-flight Upsert (queued + processing). Cheap
    /// — locks the pending set briefly and reads `len()` without copying.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    pub async fn index_path(&self, rel_path: impl Into<String>) -> Result<(), Error> {
        self.enqueue(IndexJob::Upsert { rel_path: rel_path.into(), force: false }).await
    }

    pub async fn full_scan(&self, force: bool) -> Result<(), Error> {
        self.enqueue(IndexJob::FullScan { force }).await
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
    ) -> Result<(), Error> {
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
    /// last_error). The app bridge forwards each change as
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
    /// path. The host calls this after both the indexer and the
    /// watcher are running. Idempotent first-write-wins per `OnceCell`
    /// semantics — subsequent calls log + ignore.
    ///
    /// status: trail-auto-update-on-note-move
    pub fn attach_watcher(&self, watcher: Arc<crate::watcher::Watcher>) {
        if self.watcher_cell.set(watcher).is_err() {
            tracing::warn!("indexer: watcher_cell already attached; ignoring");
        }
    }

    /// Late-bind the op-log handle so the file-lifecycle jobs can record
    /// renames / tombstones. The host calls this after both the indexer and
    /// the op log have opened. Idempotent first-write-wins per `OnceCell`.
    pub fn attach_oplog(&self, oplog: Arc<crate::oplog::OpLog>) {
        if self.oplog_cell.set(oplog).is_err() {
            tracing::warn!("indexer: oplog_cell already attached; ignoring");
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
    pub queue: Arc<crate::tasks::queue::Queue>,
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
pub fn start<F>(
    vault: crate::vault::Vault,
    store: Store,
    embedder_loader: F,
) -> Handle
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    start_indexer_with_tasks(vault, store, embedder_loader, None)
}

/// Same as `start` but threads a task-queue handle + the
/// initial model id into the loop so the embedder-load work surfaces
/// in the queue. status: embedder-model-load-as-task
pub fn start_indexer_with_tasks<F>(
    vault: crate::vault::Vault,
    store: Store,
    embedder_loader: F,
    tasks: Option<EmbedderLoadTaskPlumbing>,
) -> Handle
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
    let oplog_cell: Arc<OnceCell<Arc<crate::oplog::OpLog>>> = Arc::new(OnceCell::new());

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
    let oplog_cell_for_task = oplog_cell.clone();
    let join = tokio::spawn(
        crate::indexer::scheduler::IndexerLoop {
            vault,
            vault_root,
            store,
            embedder_loader,
            rx,
            progress: progress_for_task,
            status: status_tx,
            pending: pending_for_task,
            embedder_cell: embedder_cell_for_task,
            self_tx,
            watcher_cell: watcher_cell_for_task,
            oplog_cell: oplog_cell_for_task,
            tasks,
        }
        .run(),
    );

    Handle {
        tx: Some(tx),
        progress: progress_tx,
        status: status_rx,
        join: Some(join),
        pending,
        embedder: embedder_cell,
        watcher_cell,
        oplog_cell,
    }
}

/// Walk the vault, returning the jobs the indexer should run to bring the
/// store in line with the filesystem. Upserts for every `.md` file found,
/// Deletes for indexed paths whose files have vanished.
pub fn run_full_scan(vault_root: &Path, store: &Store, force: bool) -> Result<Vec<IndexJob>, Error> {
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
    let mut sc = ScanState {
        vault_root,
        on_disk: std::collections::HashSet::new(),
        total_files_seen: 0,
        filtered_non_md: 0,
        filtered_ignored: 0,
    };
    sc.walk_vault();
    let ScanState { on_disk, total_files_seen, filtered_non_md, filtered_ignored, .. } = sc;

    // Walk indexed paths first so we can use `on_disk` as a HashSet for
    // membership checks, then consume `on_disk` into the jobs list so
    // we never hold both `Vec<String>` and `HashSet<String>` copies of
    // every on-disk path simultaneously.
    let indexed = store.all_note_paths()?;
    let queued = on_disk.len();
    let mut jobs: Vec<IndexJob> = Vec::with_capacity(queued + indexed.len() / 8);
    let mut deleted = 0_u32;
    for path in indexed {
        if !on_disk.contains(path.as_str()) {
            jobs.push(IndexJob::Delete { rel_path: path });
            deleted += 1;
        }
    }
    for rel_path in on_disk {
        jobs.push(IndexJob::Upsert { rel_path, force });
    }

    tracing::info!(
        seen = total_files_seen,
        non_md = filtered_non_md,
        ignored = filtered_ignored,
        queued = queued as u32,
        deleted,
        "full_scan complete",
    );

    Ok(jobs)
}

/// Walker state for `run_full_scan`. Methods own the WalkDir traversal +
/// per-entry filtering so the top-level function stays under the cognitive
/// complexity cap and the helper is exempt from `single_call_fn`.
struct ScanState<'a> {
    vault_root: &'a Path,
    on_disk: std::collections::HashSet<String>,
    total_files_seen: u32,
    filtered_non_md: u32,
    filtered_ignored: u32,
}

impl<'a> ScanState<'a> {
    fn walk_vault(&mut self) {
        let vault_root = self.vault_root;
        let walker = WalkDir::new(vault_root).follow_links(false).into_iter();
        for entry in walker.filter_entry(|e| {
            // Pre-filter: skip whole subtrees we never want to enter.
            // `walkdir`'s `filter_entry` runs with absolute paths, so
            // resolve to a vault-relative form before consulting
            // `is_ignored`.
            let path = e.path();
            if path == vault_root {
                return true;
            }
            let Ok(p) = path.strip_prefix(vault_root) else {
                return false;
            };
            let rel = path_to_rel(p);
            if rel.is_empty() {
                return true;
            }
            !is_ignored(&rel)
        }) {
            self.classify_entry(entry);
        }
    }

    fn classify_entry(&mut self, entry: Result<walkdir::DirEntry, walkdir::Error>) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "full_scan: walk error");
                return;
            }
        };
        if !entry.file_type().is_file() {
            return;
        }
        self.total_files_seen += 1;
        let rel = match entry.path().strip_prefix(self.vault_root) {
            Ok(p) => path_to_rel(p),
            Err(_) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    vault_root = %self.vault_root.display(),
                    "full_scan: strip_prefix failed",
                );
                return;
            }
        };
        if is_ignored(&rel) {
            self.filtered_ignored += 1;
            return;
        }
        if !is_indexable_path(&rel) {
            self.filtered_non_md += 1;
            return;
        }
        self.on_disk.insert(rel);
    }
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

/// Lowercased file extension of a vault-relative path, or None for paths with
/// no extension (or a trailing dot only).
pub(super) fn path_extension(rel_path: &str) -> Option<&str> {
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

pub(super) fn count_notes(store: &Store) -> Result<u32, Error> {
    store.count_notes().map_err(Error::Store)
}

pub(super) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(super) fn update_status(status: &watch::Sender<IndexStatus>, mut f: impl FnMut(&mut IndexStatus)) {
    status.send_modify(|s| f(s));
}

pub(super) fn update_total_notes(status: &watch::Sender<IndexStatus>, store: &Store) {
    match count_notes(store) {
        Ok(n) => update_status(status, |s| {
            s.total_notes = n;
            // Successful count clears any prior count error; real indexing
            // errors are pinned through their own update_status calls.
            if let Some(msg) = &s.last_error
                && msg.starts_with("count_notes failed")
            {
                s.last_error = None;
            }
        }),
        Err(e) => {
            update_status(status, |s| {
                s.last_error = Some(format!("count_notes failed: {e}"));
            });
        }
    }
}

/// Read `hiker.id` from a note's frontmatter, returning None if there's
/// no frontmatter, no `hiker:` block, or no `id:` field. Used by
/// `process_upsert` to keep `path_ids` in lockstep with whatever id the
/// file already declares — avoids the "two ULIDs for one note" failure
/// mode that produced
/// `bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids`.
pub(super) fn frontmatter_hiker_id(contents: &str) -> Option<String> {
    let split = crate::frontmatter::split(contents);
    let fm = split.frontmatter?;
    let serde_yml::Value::Mapping(map) = fm else { return None };
    let serde_yml::Value::Mapping(hiker) = map.get("hiker")? else { return None };
    hiker.get("id")?.as_str().map(std::string::ToString::to_string)
}

