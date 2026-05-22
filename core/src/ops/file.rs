//! User-driven file mutations: create / move / delete / restore. Each op
//! sequences watcher suppression, the relevant `IndexJob`, and a changelog
//! row authored as `"user"`.
//!
//! See the module-level doc on `super` for the suppression-and-await
//! discipline these ops share with the agent and buffer variants.

use std::sync::Arc;

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::errors::HikerError;
use crate::hash_string;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::trash::{Trash, Entry};
use crate::vault::Vault;
use crate::watcher::Watcher;

use super::{append_change_best_effort, read_for_changelog};

/// Hash a file's bytes for the changelog row. UTF-8 paths use the same
/// `hash_string` as text writes so the same content yields the same hash on
/// round-trip; non-UTF-8 falls back to a raw blake3 over the bytes.
fn hash_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => hash_string(s),
        // Non-UTF-8 file: hash the raw bytes via blake3 directly. We don't
        // track this case in chunker dispatch, but the changelog is honest
        // about content regardless of encoding.
        Err(_) => blake3::hash(bytes).to_hex().to_string(),
    }
}

/// Create an empty new note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed name following `name_template`. The first free
/// `<template>-<N>.md` (1..1000) wins. Returns the rel path of the file
/// actually created so callers can open and inline-rename it.
///
/// `name_template` lets adapters pick their own UX policy without forking
/// the op — the app's tree button passes `"new-note"`; CLI / MCP can pass
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
            content_hash: Some(&hash_string("")),
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
