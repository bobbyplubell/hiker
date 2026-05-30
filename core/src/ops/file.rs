//! User-driven file mutations: create / move / delete / restore. Each op
//! sequences watcher suppression and the relevant `IndexJob`.
//!
//! See the module-level doc on `super` for the suppression-and-await
//! discipline these ops share with the agent and buffer variants.

use crate::errors::HikerError;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::trash::{Trash, Entry};
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Create an empty new note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed name following `name_template`. The first free
/// `<template>-<N>.md` (1..1000) wins. Returns the rel path of the file
/// actually created so callers can open and inline-rename it.
///
/// `name_template` lets adapters pick their own UX policy without forking
/// the op — the app's tree button passes `"new-note"`; CLI / MCP can pass
/// `"untitled"`, `"capture-2026-05-07"`, etc.
///
/// status: create-note-core-cmd
pub async fn create_with_suffix(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
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

    // Explicitly index the new file (the watcher event was suppressed).
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: created.clone(),
            force: false,
        })
        .await;
    Ok(created)
}

/// Create a note at an exact vault-relative `rel` path, optionally seeding it
/// with `content` (empty string = blank note). Suppresses the watcher and
/// enqueues an `IndexJob::Upsert` so the new file is indexed without a
/// duplicate watcher-driven ingest — the same discipline as
/// `create_with_suffix`, but for callers that need a specific path (a
/// wikilink target whose name must resolve the link, a duplicate that must
/// carry the source's bytes).
///
/// Errors `AlreadyExists` if a file is already at `rel`, leaving the caller to
/// decide whether to retry with a different name.
///
/// status: create-note-core-cmd
pub async fn create_at(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    rel: &str,
    content: &str,
) -> Result<String, HikerError> {
    watcher.suppress(rel.to_string());
    let created = vault.create_note(rel)?;
    if !content.is_empty() {
        watcher.suppress(created.clone());
        vault.write_file(&created, content)?;
    }
    // Re-suppress so the TTL window starts close to when notify surfaces the
    // Created/Modified events, not at function entry.
    watcher.suppress(created.clone());

    // Explicitly index the new file (the watcher events were suppressed).
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
    rel: &str,
) -> Result<Entry, HikerError> {
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
    if let Ok(entry) = &result
        && let Some(members) = &entry.members
    {
        for m in members {
            watcher.suppress(m.clone());
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
    trash: &Trash,
    id: &str,
) -> Result<Entry, HikerError> {
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
    }
    result
}
