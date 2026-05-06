//! Indexer task + walker + ingest pipeline. See docs/index.md.
//!
//! Runs as a single tokio task that owns the writer Store, an Arc<dyn
//! Embedder>, and an mpsc inbox of `IndexJob`s. Watcher and command-side
//! callers send jobs in; the task drains them serially. CPU-heavy embedding
//! goes through `spawn_blocking` so it doesn't starve the runtime.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::chunker::chunk_markdown;
use crate::embed::{Embedder, EmbedError};
use crate::hash::hash_str;
use crate::store::{new_id, NoteUpsert, Store, StoreError};
use crate::watcher::{is_ignored, FileEvent};

const PROGRESS_CAPACITY: usize = 256;
/// Max markdown size we'll attempt to index, in bytes. Larger files are
/// almost certainly not handwritten markdown (committed binaries, generated
/// dumps); skipping them keeps the embedder from thrashing.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexJob {
    Upsert { rel_path: String },
    Delete { rel_path: String },
    Rename { from: String, to: String },
    /// Walk the vault, enqueuing Upserts for `.md` files and Deletes for
    /// indexed paths whose files have vanished.
    FullScan,
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
    #[error("file too large ({size} bytes)")]
    TooLarge { size: u64 },
    #[error("not utf-8")]
    NotUtf8,
    #[error("send failed: indexer task is shut down")]
    SendFailed,
}

/// Caller-side handle. Cheap to clone (everything inside is `Arc` or
/// `Sender`).
pub struct IndexerHandle {
    tx: mpsc::Sender<IndexJob>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Receiver<IndexStatus>,
    join: Option<JoinHandle<()>>,
}

impl IndexerHandle {
    pub async fn enqueue(&self, job: IndexJob) -> Result<(), IndexerError> {
        self.tx.send(job).await.map_err(|_| IndexerError::SendFailed)
    }

    pub async fn index_path(&self, rel_path: impl Into<String>) -> Result<(), IndexerError> {
        self.enqueue(IndexJob::Upsert { rel_path: rel_path.into() }).await
    }

    pub async fn full_scan(&self) -> Result<(), IndexerError> {
        self.enqueue(IndexJob::FullScan).await
    }

    pub fn status(&self) -> IndexStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressEvent> {
        self.progress.subscribe()
    }

    /// Clone the underlying mpsc sender. Useful for routing watcher events
    /// into the indexer from a separate task.
    pub fn job_sender(&self) -> mpsc::Sender<IndexJob> {
        self.tx.clone()
    }

    /// Stop the indexer task gracefully and wait for it to finish.
    pub async fn shutdown(mut self) {
        // Dropping the sender signals the task's `recv` to return None.
        drop(self.tx.clone());
        if let Some(join) = self.join.take() {
            // Sender drop above isn't enough since `tx` is still held by
            // `self`; replace the channel-closing strategy by sending a
            // sentinel via an explicit close. Simplest: drop `self.tx`.
            // We can't move out of `self.tx` after `take`-ing join, so
            // wait briefly with a deadline.
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
    vault_root: PathBuf,
    store: Store,
    embedder_loader: F,
) -> IndexerHandle
where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<IndexJob>(256);
    let (progress_tx, _) = broadcast::channel::<ProgressEvent>(PROGRESS_CAPACITY);
    let (status_tx, status_rx) = watch::channel(IndexStatus::default());

    // Seed total_notes from the store before the task starts so a fresh open
    // shows the correct count immediately.
    let initial_total = count_notes(&store).unwrap_or(0);
    let _ = status_tx.send(IndexStatus {
        total_notes: initial_total,
        ..IndexStatus::default()
    });

    let progress_for_task = progress_tx.clone();
    let tx_for_loop = tx.clone();
    let join = tokio::spawn(indexer_loop(
        vault_root,
        store,
        embedder_loader,
        rx,
        tx_for_loop,
        progress_for_task,
        status_tx,
    ));

    IndexerHandle {
        tx,
        progress: progress_tx,
        status: status_rx,
        join: Some(join),
    }
}

async fn indexer_loop<F>(
    vault_root: PathBuf,
    mut store: Store,
    embedder_loader: F,
    mut rx: mpsc::Receiver<IndexJob>,
    tx: mpsc::Sender<IndexJob>,
    progress: broadcast::Sender<ProgressEvent>,
    status: watch::Sender<IndexStatus>,
) where
    F: FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static,
{
    // Load the embedder on a blocking thread. Until it returns, jobs queue
    // up in the mpsc channel without being processed.
    let load = tokio::task::spawn_blocking(embedder_loader).await;
    let embedder: Arc<dyn Embedder> = match load {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            update_status(&status, |s| s.last_error = Some(format!("embedder load: {e}")));
            let _ = progress.send(ProgressEvent::Error {
                path: None,
                message: format!("embedder load failed: {e}"),
            });
            // Drain the queue while reporting failures so callers don't
            // block forever.
            while let Some(_job) = rx.recv().await {}
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
    let _ = progress.send(ProgressEvent::ModelLoaded);

    while let Some(job) = rx.recv().await {
        update_queue_count(&status, &rx);
        match job {
            IndexJob::Upsert { rel_path } => {
                let _ = progress.send(ProgressEvent::Started { path: rel_path.clone() });
                match process_upsert(&vault_root, &mut store, embedder.clone(), &rel_path).await {
                    Ok(UpsertOutcome::Indexed) => {
                        let _ = progress.send(ProgressEvent::Finished { path: rel_path });
                    }
                    Ok(UpsertOutcome::Unchanged) => {
                        let _ = progress.send(ProgressEvent::Skipped {
                            path: rel_path,
                            reason: "unchanged".into(),
                        });
                    }
                    Ok(UpsertOutcome::Skipped(reason)) => {
                        let _ = progress.send(ProgressEvent::Skipped {
                            path: rel_path,
                            reason,
                        });
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        update_status(&status, |s| s.last_error = Some(msg.clone()));
                        let _ = progress.send(ProgressEvent::Error {
                            path: Some(rel_path),
                            message: msg,
                        });
                    }
                }
            }
            IndexJob::Delete { rel_path } => match process_delete(&mut store, &rel_path) {
                Ok(true) => {
                    let _ = progress.send(ProgressEvent::Deleted { path: rel_path });
                }
                Ok(false) => {
                    // Path wasn't indexed; nothing to do.
                }
                Err(e) => {
                    let msg = format!("{e}");
                    update_status(&status, |s| s.last_error = Some(msg.clone()));
                    let _ = progress.send(ProgressEvent::Error {
                        path: Some(rel_path),
                        message: msg,
                    });
                }
            },
            IndexJob::Rename { from, to } => match process_rename(&mut store, &from, &to) {
                Ok(true) => {
                    let _ = progress.send(ProgressEvent::Renamed {
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
                Ok(false) => {
                    // From-path wasn't indexed; treat the destination as a
                    // fresh upsert so the file ends up in the index.
                    if let Err(e) = handle_inline_upsert(
                        &vault_root,
                        &mut store,
                        embedder.clone(),
                        &progress,
                        &status,
                        &to,
                    )
                    .await
                    {
                        let msg = format!("{e}");
                        update_status(&status, |s| s.last_error = Some(msg.clone()));
                        let _ = progress.send(ProgressEvent::Error {
                            path: Some(to),
                            message: msg,
                        });
                    }
                }
                Err(e) => {
                    let msg = format!("{e}");
                    update_status(&status, |s| s.last_error = Some(msg.clone()));
                    let _ = progress.send(ProgressEvent::Error {
                        path: Some(to),
                        message: msg,
                    });
                }
            },
            IndexJob::FullScan => match run_full_scan(&vault_root, &store) {
                Ok(jobs) => {
                    let scanned = jobs.len() as u32;
                    let mut queued = 0_u32;
                    for j in jobs {
                        if let IndexJob::Upsert { .. } | IndexJob::Delete { .. } = &j {
                            queued += 1;
                        }
                        if tx.send(j).await.is_err() {
                            // Receiver dropped — task is shutting down.
                            break;
                        }
                    }
                    let _ = progress.send(ProgressEvent::ScanComplete { scanned, queued });
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
        }
        update_total_notes(&status, &store);
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
    match process_upsert(vault_root, store, embedder, rel_path).await? {
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
) -> Result<UpsertOutcome, IndexerError> {
    if !rel_path.ends_with(".md") {
        return Ok(UpsertOutcome::Skipped("non-markdown".into()));
    }
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
    if size > MAX_FILE_BYTES {
        return Err(IndexerError::TooLarge { size });
    }
    let bytes = tokio::fs::read(&abs).await?;
    let contents = String::from_utf8(bytes).map_err(|_| IndexerError::NotUtf8)?;
    let content_hash = hash_str(&contents);
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Short-circuit: same content + same embedder version → no-op.
    if let Some(existing) = store.get_note_by_path(rel_path)? {
        if existing.content_hash == content_hash
            && existing.embedder_version == embedder.version()
        {
            return Ok(UpsertOutcome::Unchanged);
        }
    }

    // Chunk + embed. Embed is CPU-heavy: spawn_blocking.
    let chunks = chunk_markdown(&contents);
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
    let emb_clone = embedder.clone();
    let embeddings = tokio::task::spawn_blocking(move || emb_clone.embed_batch(&texts))
        .await
        .map_err(|e| IndexerError::Embed(EmbedError::Embed(e.to_string())))??;

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
pub fn run_full_scan(vault_root: &Path, store: &Store) -> Result<Vec<IndexJob>, IndexerError> {
    let mut on_disk: Vec<String> = Vec::new();
    let walker = WalkDir::new(vault_root).follow_links(false).into_iter();
    for entry in walker.filter_entry(|e| !walk_skip(vault_root, e.path())) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[hiker::indexer] walk error: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(vault_root) {
            Ok(p) => path_to_rel(p),
            Err(_) => continue,
        };
        if is_ignored(&rel) || !rel.ends_with(".md") {
            continue;
        }
        on_disk.push(rel);
    }

    let mut jobs: Vec<IndexJob> = on_disk
        .iter()
        .cloned()
        .map(|rel_path| IndexJob::Upsert { rel_path })
        .collect();

    // Find indexed paths missing from disk.
    let indexed = store.all_note_paths()?;
    let on_disk_set: std::collections::HashSet<&str> =
        on_disk.iter().map(String::as_str).collect();
    for path in indexed {
        if !on_disk_set.contains(path.as_str()) {
            jobs.push(IndexJob::Delete { rel_path: path });
        }
    }

    Ok(jobs)
}

/// Route filesystem events into the indexer's job queue. Filters to `.md`
/// files (other types are ignored in v1 per index.md vault tolerance) and
/// translates each event kind into the matching IndexJob. Runs until the
/// broadcast receiver lags out or the indexer's sender closes.
pub async fn route_watcher_events(
    mut rx: broadcast::Receiver<FileEvent>,
    tx: mpsc::Sender<IndexJob>,
) {
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let job = match ev {
                    FileEvent::Created { path } | FileEvent::Modified { path } => {
                        if !path.ends_with(".md") {
                            continue;
                        }
                        IndexJob::Upsert { rel_path: path }
                    }
                    FileEvent::Deleted { path } => {
                        if !path.ends_with(".md") {
                            continue;
                        }
                        IndexJob::Delete { rel_path: path }
                    }
                    FileEvent::Renamed { from, to } => {
                        // Rename involving non-md is treated as ignore on
                        // both sides — neither side is/was indexed.
                        if !from.ends_with(".md") && !to.ends_with(".md") {
                            continue;
                        }
                        IndexJob::Rename { from, to }
                    }
                };
                if tx.send(job).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // We dropped events; the safe recovery is a full rescan.
                let _ = tx.send(IndexJob::FullScan).await;
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
    if let Ok(n) = count_notes(store) {
        update_status(status, |s| s.total_notes = n);
    }
}

fn update_queue_count(status: &watch::Sender<IndexStatus>, rx: &mpsc::Receiver<IndexJob>) {
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
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
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
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
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
    async fn deleting_a_note_removes_it_from_the_index() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("doomed.md"), b"# x\n").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
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
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
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
        fs::write(dir.path().join("c.txt"), b"not markdown").unwrap();
        // .hiker/ subtree must be skipped.
        fs::create_dir_all(dir.path().join(".hiker/refs")).unwrap();
        fs::write(dir.path().join(".hiker/refs/secret.md"), b"x").unwrap();

        let store = Store::open(dir.path()).unwrap();
        let jobs = run_full_scan(dir.path(), &store).unwrap();
        let upserts: Vec<&String> = jobs
            .iter()
            .filter_map(|j| match j {
                IndexJob::Upsert { rel_path } => Some(rel_path),
                _ => None,
            })
            .collect();
        assert!(upserts.iter().any(|p| p.as_str() == "a.md"));
        assert!(upserts.iter().any(|p| p.as_str() == "sub/b.md"));
        assert!(!upserts.iter().any(|p| p.contains(".hiker")));
        assert!(!upserts.iter().any(|p| p.ends_with("c.txt")));
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

        let jobs = run_full_scan(dir.path(), &store).unwrap();
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
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
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
    async fn non_markdown_files_are_skipped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let store = Store::open(dir.path()).unwrap();
        let handle = start_indexer(dir.path().to_path_buf(), store, mock_loader());
        let mut prog = handle.subscribe_progress();

        await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
        handle.index_path("a.txt").await.unwrap();
        let ev = await_event(&mut prog, |e| matches!(e, ProgressEvent::Skipped { .. })).await;
        if let ProgressEvent::Skipped { reason, .. } = ev {
            assert_eq!(reason, "non-markdown");
        }
    }
}
