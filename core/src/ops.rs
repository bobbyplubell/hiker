//! Vault-level orchestration ops shared by every adapter (Tauri today, CLI /
//! MCP later). Each op owns the full sequence around a mutating action:
//! pre-suppress watcher paths → enumerate vault/trash members → send the
//! relevant `IndexJob` → await its oneshot reply → re-suppress for the TTL
//! window. Adapters call one function and translate the result.
//!
//! Why this lives in `core::ops` rather than as methods on `IndexerHandle`:
//! the orchestration spans `Watcher` + `IndexerHandle` + `Vault` + `Trash`,
//! and picking one to host the rest is dishonest. Free functions take borrows
//! of whichever handles they need.
//!
//! Senders, not handles. Each op takes an `&IndexJobTx` (the auto-pending-
//! tracking sender wrapper returned by `IndexerHandle::job_sender()`). This
//! matches what callers already do — clone a sender under whatever session
//! lock they hold, drop the lock before `.await`. Passing `&IndexerHandle`
//! would invite holding the handle across the await; the sender form makes
//! the constraint explicit.
//!
//! Watcher suppression. Indexer-side handlers call `crate::vault::*` with
//! `watcher: None` (see `IndexJob::{Move, MoveFolder, DeleteNote,
//! RestoreFromTrash}` in `core::indexer`). Suppression is therefore solely
//! the ops layer's job: pre-suppress before the job runs, re-suppress after
//! it completes so the TTL window starts close to when notify will surface
//! its events.

use std::sync::Arc;

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::trash::{Trash, TrashEntry};
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Read a file's bytes for inclusion in a changelog row. Best-effort: if the
/// file vanishes mid-op or read fails, return `None` and the row is appended
/// without a content blob. Better to log a hash-less row than to abort the
/// mutation that already succeeded on disk.
fn read_for_changelog(vault: &Vault, rel: &str) -> Option<Vec<u8>> {
    let abs = vault.abs_path(rel).ok()?;
    std::fs::read(abs).ok()
}

fn append_change_best_effort(
    changes: Option<&Arc<Changes>>,
    append: ChangeAppend<'_>,
) {
    if let Some(c) = changes {
        if let Err(e) = c.append(append) {
            tracing::warn!(error = %e, "changes: append failed");
        }
    }
}

/// Create an empty new note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed name following `name_template`. The first free
/// `<template>-<N>.md` (1..1000) wins. Returns the rel path of the file
/// actually created so callers can open and inline-rename it.
///
/// `name_template` lets adapters pick their own UX policy without forking
/// the op — Tauri's tree button passes `"new-note"`; CLI / MCP can pass
/// `"untitled"`, `"capture-2026-05-07"`, etc.
///
/// status: create-note-button
pub async fn create_with_suffix(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    folder: &str,
    name_template: &str,
) -> Result<String, HikerError> {
    let folder = folder.trim_end_matches('/');
    let mut created: Option<String> = None;
    let mut last_err: Option<HikerError> = None;
    for n in 1..1000 {
        let candidate = if folder.is_empty() {
            format!("{name_template}-{n}.md")
        } else {
            format!("{folder}/{name_template}-{n}.md")
        };
        watcher.suppress(candidate.clone());
        match vault.create_note(&candidate) {
            Ok(p) => {
                created = Some(p);
                break;
            }
            Err(HikerError::AlreadyExists(_)) => continue,
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    let created = match created {
        Some(p) => p,
        None => {
            return Err(last_err.unwrap_or_else(|| {
                HikerError::AlreadyExists(format!(
                    "ran out of {name_template}-N candidates"
                ))
            }));
        }
    };
    // Re-suppress so the TTL window starts close to when notify surfaces the
    // Created event, not at function entry.
    watcher.suppress(created.clone());

    // Append the changelog row before the index job so the recent-activity
    // widget can refresh via `hiker:changes-appended` even if the indexer is
    // still loading the embedder.
    let empty: &[u8] = &[];
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: &created,
            op: ChangeOp::Created,
            author: "user",
            content_hash: Some(&hash_str("")),
            content: Some(empty),
            rename_from: None,
            metadata: serde_json::json!({}),
        },
    );

    // Explicitly index the new file (the watcher event was suppressed).
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: created.clone(),
            force: false,
        })
        .await;
    Ok(created)
}

/// Atomic note rename. Routes through the indexer task so the fs rename and
/// store path remap share its owned writer connection.
///
/// status: move-note-core-cmd
pub async fn move_note(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    from: &str,
    to: &str,
) -> Result<(), HikerError> {
    watcher.suppress(from.to_string());
    watcher.suppress(to.to_string());

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    jobs.send(IndexJob::Move {
        from: from.to_string(),
        to: to.to_string(),
        reply: reply_tx,
    })
    .await
    .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped move reply".into()))?;

    // Re-suppress so the TTL window starts close to when notify surfaces its
    // events post-rename.
    watcher.suppress(from.to_string());
    watcher.suppress(to.to_string());

    if result.is_ok() {
        let body = read_for_changelog(vault, to);
        let hash = body.as_deref().map(hash_bytes);
        append_change_best_effort(
            changes,
            ChangeAppend {
                path: to,
                op: ChangeOp::Renamed,
                author: "user",
                content_hash: hash.as_deref(),
                content: body.as_deref(),
                rename_from: Some(from),
                metadata: serde_json::json!({}),
            },
        );
    }
    result
}

/// Folder rename: fs rename of the whole directory + bulk store path remap
/// for every contained indexed file. Empty subfolders move with the rename
/// for free (single fs rename).
///
/// Pre-suppression covers the folder root, every member at its old path, and
/// every member at its new path so cross-platform notify ordering can't
/// surface a stale Created/Deleted pair.
///
/// status: drag-and-drop-move
pub async fn move_folder(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    from: &str,
    to: &str,
) -> Result<(), HikerError> {
    // Pre-suppress every member at its old AND new path. Walk failures
    // here are non-fatal — the indexer-side `move_folder` will re-walk on
    // its own; we just lose some pre-suppression coverage.
    let members = vault.walk_indexable_files(from).unwrap_or_default();
    let from_prefix = format!("{from}/");
    watcher.suppress(from.to_string());
    watcher.suppress(to.to_string());
    for m in &members {
        watcher.suppress(m.clone());
        let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
        watcher.suppress(format!("{to}/{suffix}"));
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    jobs.send(IndexJob::MoveFolder {
        from: from.to_string(),
        to: to.to_string(),
        reply: reply_tx,
    })
    .await
    .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped move_folder reply".into()))?;

    watcher.suppress(from.to_string());
    watcher.suppress(to.to_string());
    for m in &members {
        watcher.suppress(m.clone());
        let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
        watcher.suppress(format!("{to}/{suffix}"));
    }

    // One Renamed row per affected note — folder renames touch every member.
    if result.is_ok() {
        for m in &members {
            let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
            let new_path = format!("{to}/{suffix}");
            let body = read_for_changelog(vault, &new_path);
            let hash = body.as_deref().map(hash_bytes);
            append_change_best_effort(
                changes,
                ChangeAppend {
                    path: &new_path,
                    op: ChangeOp::Renamed,
                    author: "user",
                    content_hash: hash.as_deref(),
                    content: body.as_deref(),
                    rename_from: Some(m),
                    metadata: serde_json::json!({}),
                },
            );
        }
    }
    result
}

/// Soft-delete a note or folder. Routes through the indexer task — moves the
/// source into vault trash, removes any matching index entries, appends a
/// trash manifest entry. Returns the manifest entry so the caller can drive
/// an undo affordance without a second roundtrip.
///
/// status: delete-note-core-cmd
pub async fn delete(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    rel: &str,
) -> Result<TrashEntry, HikerError> {
    // Pre-suppress the root + every `.md` member. On Linux/macOS `fs::rename`
    // of a directory is a single inode op so notify shouldn't emit per-child
    // events — but other platforms may, and the cost of pre-suppressing is
    // tiny. The post-op re-suppression below covers the same paths again.
    watcher.suppress(rel.to_string());
    let members = vault.walk_indexable_files(rel).unwrap_or_default();
    for m in &members {
        watcher.suppress(m.clone());
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    jobs.send(IndexJob::DeleteNote {
        rel: rel.to_string(),
        reply: reply_tx,
    })
    .await
    .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped delete reply".into()))?;

    watcher.suppress(rel.to_string());
    if let Ok(entry) = &result {
        if let Some(members) = &entry.members {
            for m in members {
                watcher.suppress(m.clone());
            }
        }
        // Append one Deleted row per affected note (members for folder
        // deletes, just the path for files). Content is NULL for deletes;
        // rollback walks back to the row before the delete for the prior
        // content blob.
        match &entry.members {
            Some(members) => {
                for m in members {
                    append_change_best_effort(
                        changes,
                        ChangeAppend {
                            path: m,
                            op: ChangeOp::Deleted,
                            author: "user",
                            content_hash: None,
                            content: None,
                            rename_from: None,
                            metadata: serde_json::json!({}),
                        },
                    );
                }
            }
            None => append_change_best_effort(
                changes,
                ChangeAppend {
                    path: rel,
                    op: ChangeOp::Deleted,
                    author: "user",
                    content_hash: None,
                    content: None,
                    rename_from: None,
                    metadata: serde_json::json!({}),
                },
            ),
        }
    }
    result
}

/// Restore a previously soft-deleted entry from the vault trash. Routes
/// through the indexer task so the post-restore re-ingest runs on the owned
/// writer connection. Returns the manifest entry that was restored.
///
/// status: vault-trash-restore
pub async fn restore(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    trash: &Trash,
    id: &str,
) -> Result<TrashEntry, HikerError> {
    // Resolve the entry up front so suppression is in place before the
    // indexer task fires fs::rename. Manifest read failures here are
    // non-fatal — the indexer-side restore will surface its own NotFound.
    if let Ok(Some(entry)) = trash.find(id) {
        watcher.suppress(entry.original_path.clone());
        if let Some(members) = &entry.members {
            for m in members {
                watcher.suppress(m.clone());
            }
        }
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    jobs.send(IndexJob::RestoreFromTrash {
        id: id.to_string(),
        reply: reply_tx,
    })
    .await
    .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped restore reply".into()))?;

    if let Ok(entry) = &result {
        watcher.suppress(entry.original_path.clone());
        if let Some(members) = &entry.members {
            for m in members {
                watcher.suppress(m.clone());
            }
        }
        // Per spec: restore logs as a fresh `'created'` event (the prior
        // `'deleted'` row stays in the log; restoration is a new event).
        match &entry.members {
            Some(members) => {
                for m in members {
                    let body = read_for_changelog(vault, m);
                    let hash = body.as_deref().map(hash_bytes);
                    append_change_best_effort(
                        changes,
                        ChangeAppend {
                            path: m,
                            op: ChangeOp::Created,
                            author: "user",
                            content_hash: hash.as_deref(),
                            content: body.as_deref(),
                            rename_from: None,
                            metadata: serde_json::json!({"restored": true}),
                        },
                    );
                }
            }
            None => {
                let body = read_for_changelog(vault, &entry.original_path);
                let hash = body.as_deref().map(hash_bytes);
                append_change_best_effort(
                    changes,
                    ChangeAppend {
                        path: &entry.original_path,
                        op: ChangeOp::Created,
                        author: "user",
                        content_hash: hash.as_deref(),
                        content: body.as_deref(),
                        rename_from: None,
                        metadata: serde_json::json!({"restored": true}),
                    },
                );
            }
        }
    }
    result
}

fn hash_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => hash_str(s),
        // Non-UTF-8 file: hash the raw bytes via blake3 directly. We don't
        // track this case in chunker dispatch, but the changelog is honest
        // about content regardless of encoding.
        Err(_) => blake3::hash(bytes).to_hex().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{EmbedError, Embedder};
    use crate::indexer::{start_indexer, IndexerHandle};
    use crate::store::Store;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Stub embedder so the indexer task starts immediately and emits a
    /// ModelLoaded event without needing real model files. Returns a
    /// 384-dim zero vector for any input.
    struct ZeroEmbedder;
    impl Embedder for ZeroEmbedder {
        fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
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

    fn start(vault: Vault, store: Store) -> IndexerHandle {
        start_indexer(vault, store, || {
            Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_with_suffix_picks_first_free_slot() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);

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
        let idx = start(vault.clone(), store);

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
        let idx = start(vault.clone(), store);

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
        let idx = start(vault.clone(), store);

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
        let idx = start(vault.clone(), store);

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
}
