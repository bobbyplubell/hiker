use super::file::{create_with_suffix, delete, move_folder, move_note, restore};
use crate::embed::{Error, Embedder};
use crate::indexer::{self, Handle};
use crate::store::Store;
use crate::trash::Trash;
use crate::vault::Vault;
use crate::watcher::Watcher;
use std::sync::Arc;
use tempfile::TempDir;

/// Stub embedder so the indexer task starts immediately and emits a
/// ModelLoaded event without needing real model files. Returns a
/// 384-dim zero vector for any input.
struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, Error> {
        Ok(batch.iter().map(|_| vec![0.0; 384]).collect())
    }
    fn version(&self) -> &str {
        "zero-test"
    }
    fn dim(&self) -> usize {
        384
    }
}

fn open_vault(td: &TempDir) -> Vault {
    Vault::open(td.path()).expect("open vault")
}

fn start_indexer(vault: Vault, store: Store) -> Handle {
    indexer::start(vault, store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_suffix_picks_first_free_slot() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    let p1 = create_with_suffix(&watcher, &idx.job_sender(), &vault, None, "", "new-note")
        .await
        .unwrap();
    assert_eq!(p1, "new-note-1.md");
    let p2 = create_with_suffix(&watcher, &idx.job_sender(), &vault, None, "", "new-note")
        .await
        .unwrap();
    assert_eq!(p2, "new-note-2.md");

    // Custom template — no collision with new-note-* slots.
    let p3 = create_with_suffix(&watcher, &idx.job_sender(), &vault, None, "", "draft")
        .await
        .unwrap();
    assert_eq!(p3, "draft-1.md");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_note_renames_existing_file() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::write(td.path().join("a.md"), "hello").unwrap();
    move_note(&watcher, &idx.job_sender(), &vault, None, "a.md", "b.md")
        .await
        .unwrap();
    assert!(!td.path().join("a.md").exists());
    assert!(td.path().join("b.md").exists());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_folder_renames_directory_with_members() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::create_dir(td.path().join("src")).unwrap();
    std::fs::write(td.path().join("src/a.md"), "x").unwrap();
    std::fs::write(td.path().join("src/b.md"), "y").unwrap();

    move_folder(&watcher, &idx.job_sender(), &vault, None, "src", "dst")
        .await
        .unwrap();
    assert!(!td.path().join("src").exists());
    assert!(td.path().join("dst/a.md").exists());
    assert!(td.path().join("dst/b.md").exists());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_note_suppresses_watcher_events_for_both_paths() {
    use crate::watcher::FileEvent;
    use std::time::{Duration, Instant};
    use tokio::time::timeout;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    // Subscribe before the op so any event the rename produces lands in
    // our channel. Settle briefly so the watcher's bridge thread is up.
    let mut rx = watcher.subscribe();
    std::fs::write(td.path().join("a.md"), b"x").unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    move_note(&watcher, &idx.job_sender(), &vault, None, "a.md", "b.md")
        .await
        .unwrap();

    // Drive a positive control after the op so we have something
    // unambiguous to wait for; once we see it, no `a.md`/`b.md` event
    // ever surfaced past the watcher's debounce + suppression TTL.
    std::fs::write(td.path().join("decoy.md"), b"y").unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_decoy = false;
    while Instant::now() < deadline && !saw_decoy {
        match timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Ok(ev)) => {
                let path = match &ev {
                    FileEvent::Created { path } | FileEvent::Modified { path } => {
                        path.clone()
                    }
                    FileEvent::Deleted { path } => path.clone(),
                    FileEvent::Renamed { to, .. } => to.clone(),
                    FileEvent::Overflow => continue,
                };
                assert!(
                    path != "a.md" && path != "b.md",
                    "ops::move_note leaked watcher event for suppressed path: {ev:?}",
                );
                if path == "decoy.md" {
                    saw_decoy = true;
                }
            }
            _ => continue,
        }
    }
    assert!(saw_decoy, "expected to see the decoy write surface");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_then_restore_round_trips() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::write(td.path().join("note.md"), "body").unwrap();

    let entry = delete(&watcher, &idx.job_sender(), &vault, None, "note.md")
        .await
        .unwrap();
    assert!(!td.path().join("note.md").exists());
    assert_eq!(entry.original_path, "note.md");

    let trash = Trash::open(td.path());
    let restored = restore(&watcher, &idx.job_sender(), &vault, None, &trash, &entry.id)
        .await
        .unwrap();
    assert_eq!(restored.original_path, "note.md");
    assert!(td.path().join("note.md").exists());

    idx.shutdown().await;
}
