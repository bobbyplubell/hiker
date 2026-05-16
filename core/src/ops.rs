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

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::store::Store;
use crate::trash::{Trash, TrashEntry};
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Opaque buffer-identity token issued by `open_for_edit` and rotated by
/// every successful `commit_buffer` / drift resolution. Wraps the path the
/// token is bound to, the content hash that was on disk at issue time, and
/// the load timestamp so callers (UI, MCP, future agents) never have to
/// hold the hash themselves — they round-trip the token verbatim through
/// `commit_buffer` and we re-derive the drift-check inputs from it.
///
/// Fields are private; the type serializes as a JSON object for the Tauri
/// IPC seam (and any future ts-rs export) but the UI must not introspect
/// or reconstruct it. The whole point of this slug is to delete the
/// hash-as-cursor concept from the editor surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferToken {
    path: String,
    content_hash: String,
    opened_at_ms: i64,
}

impl BufferToken {
    fn new(path: &str, content_hash: &str) -> Self {
        let opened_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            path: path.to_string(),
            content_hash: content_hash.to_string(),
            opened_at_ms,
        }
    }

    /// Read accessor used by the (private) commit / resolve paths below.
    /// Never re-exported to adapters — the UI layer holds tokens, not
    /// hashes.
    fn hash(&self) -> &str {
        &self.content_hash
    }

    fn path(&self) -> &str {
        &self.path
    }
}

/// Result of `open_for_edit`. The token is opaque to callers; pair it with
/// `contents` to seed the editor and then round-trip the token unchanged on
/// every `commit_buffer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenForEditOutcome {
    pub contents: String,
    pub token: BufferToken,
}

/// Outcome of a `commit_buffer` call. `Written` is the success path; the
/// returned `token` replaces the caller's prior token so the next commit
/// drift-checks against this commit's on-disk state. `DriftDetected`
/// surfaces the on-disk state for the caller to render its modal — the
/// caller then dispatches to `resolve_drift` with the user's choice
/// (keep / take / cancel).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitOutcome {
    Written {
        new_hash: String,
        token: BufferToken,
    },
    DriftDetected {
        current_disk_text: String,
        current_hash: String,
    },
}

/// User's choice when resolving a drift conflict. Modal copy + default
/// focus stay in the UI; this is the typed dispatch surface so MCP / CLI /
/// future agents can drive the same conflict-resolution path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftChoice {
    /// Overwrite the on-disk version with the caller's `new_text`.
    KeepMine,
    /// Discard the caller's `new_text`; reload disk into the buffer. The
    /// returned `contents` + `token` reseed the caller.
    TakeTheirs,
    /// No-op. Caller should leave the buffer dirty so the next commit
    /// re-prompts.
    Cancel,
}

/// Result of `resolve_drift`. Mirrors the shapes the UI was juggling
/// inline (overwrite vs reload-from-disk vs no-op) so adapters can
/// dispatch on a typed variant rather than re-implementing the policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriftResolution {
    Written {
        new_hash: String,
        token: BufferToken,
    },
    TookTheirs {
        contents: String,
        token: BufferToken,
    },
    Cancelled,
}

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
    if let Some(c) = changes
        && let Err(e) = c.append(append)
    {
        tracing::warn!(error = %e, "changes: append failed");
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

/// Borrowed bundle for the four agent_* write helpers. The first six
/// arguments are identical across `agent_write_note`,
/// `agent_set_frontmatter`, `agent_apply_tag`, and `agent_remove_tag`;
/// bundling them keeps the signatures (and call sites) under the
/// `too_many_arguments` threshold without changing any behavior.
pub struct AgentWriteCtx<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub changes: Option<&'a Arc<Changes>>,
    pub client_id: &'a str,
    pub tool: &'a str,
}

/// Agent write of a note's full body. Routes through the indexer so the
/// post-write upsert runs against the same writer the UI uses; appends an
/// `author='agent:<client_id>'` changelog row with the post-write content
/// blob (rollback substrate per `mcp.md`'s authorship + audit-trail spec).
///
/// `expected_hash` enables drift-aware writes (`write_file_checked` shape):
/// `Some(h)` runs the on-disk hash compare and errors `DiskDrift` if the
/// file has changed since the agent last read it; `None` is an unconditional
/// write. Returns the new content hash.
///
/// status: mcp-tool-write-note
pub async fn agent_write_note(
    ctx: &AgentWriteCtx<'_>,
    rel: &str,
    content: &str,
    expected_hash: Option<&str>,
) -> Result<String, HikerError> {
    ctx.watcher.suppress(rel.to_string());

    // Snapshot the pre-write content as a baseline if this is the first time
    // hiker has touched the path (mirrors the UI's `ensure_baseline` hook on
    // user saves so rollback of an agent-authored save has somewhere to go).
    if let (Some(c), Ok((pre_text, pre_hash))) = (ctx.changes, ctx.vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (agent write)");
    }

    let abs = ctx.vault.abs_path(rel)?;
    let existed = abs.exists();
    let new_hash = match expected_hash {
        Some(h) => ctx.vault.write_file_checked(rel, h, content)?,
        None => {
            ctx.vault.write_file(rel, content)?;
            hash_str(content)
        }
    };

    // Re-suppress so the TTL window starts close to when notify surfaces the
    // post-write event.
    ctx.watcher.suppress(rel.to_string());

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let author = format!("agent:{}", ctx.client_id);
    let metadata = serde_json::json!({"tool": ctx.tool});
    append_change_best_effort(
        ctx.changes,
        ChangeAppend {
            path: rel,
            op,
            author: &author,
            content_hash: Some(&new_hash),
            content: Some(content.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    // Re-index the new content so search/related see the agent's changes.
    let _ = ctx
        .jobs
        .send(IndexJob::Upsert {
            rel_path: rel.to_string(),
            force: false,
        })
        .await;

    Ok(new_hash)
}

/// Agent merge of frontmatter fields. Reads the existing file, merges
/// `fields` into the frontmatter (recursing into nested maps), stamps
/// `hiker.author: agent-authored`, and writes the result. Errors if the
/// note doesn't exist (`NotFound`) — frontmatter on a missing file would
/// require deciding whether to create it, and the spec defers that to
/// `write_note` instead.
///
/// status: mcp-tool-set-frontmatter
pub async fn agent_set_frontmatter(
    ctx: &AgentWriteCtx<'_>,
    rel: &str,
    fields: serde_json::Value,
) -> Result<String, HikerError> {
    let existing = ctx.vault.read_file(rel)?;
    let merged = crate::frontmatter::merge_agent_patch(&existing, fields)
        .map_err(|e| HikerError::Io(format!("frontmatter: {e}")))?;
    agent_write_note(ctx, rel, &merged, None).await
}

/// Agent tag append. Convenience over `agent_set_frontmatter` for the most
/// common case. Idempotent — re-applying an already-present tag is a no-op
/// (still writes since the file's content hash may differ on whitespace,
/// but the resulting tag list is unique).
///
/// status: mcp-tool-apply-tag-remove-tag
pub async fn agent_apply_tag(
    ctx: &AgentWriteCtx<'_>,
    rel: &str,
    tag: &str,
) -> Result<String, HikerError> {
    let existing_tags = read_existing_tags(ctx.vault, rel)?;
    let mut tags = existing_tags;
    if !tags.iter().any(|t| t == tag) {
        tags.push(tag.to_string());
    }
    agent_set_frontmatter(ctx, rel, serde_json::json!({"tags": tags})).await
}

/// Agent tag removal. No-op if the tag isn't present. Mirrors `agent_apply_tag`.
///
/// status: mcp-tool-apply-tag-remove-tag
pub async fn agent_remove_tag(
    ctx: &AgentWriteCtx<'_>,
    rel: &str,
    tag: &str,
) -> Result<String, HikerError> {
    let mut tags = read_existing_tags(ctx.vault, rel)?;
    tags.retain(|t| t != tag);
    agent_set_frontmatter(ctx, rel, serde_json::json!({"tags": tags})).await
}

fn read_existing_tags(vault: &Vault, rel: &str) -> Result<Vec<String>, HikerError> {
    let src = vault.read_file(rel)?;
    let split = crate::frontmatter::split(&src);
    let Some(fm) = split.frontmatter else {
        return Ok(Vec::new());
    };
    let serde_yml::Value::Mapping(m) = fm else {
        return Ok(Vec::new());
    };
    let Some(serde_yml::Value::Sequence(seq)) = m.get("tags") else {
        return Ok(Vec::new());
    };
    Ok(seq
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect())
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

/// Read `rel` from disk and mint an opaque `BufferToken` capturing its
/// hash + path + load time. The caller seeds the editor with `contents`
/// and round-trips the token verbatim through `commit_buffer` —
/// hash-as-cursor stays inside core.
pub fn open_for_edit(vault: &Vault, rel: &str) -> Result<OpenForEditOutcome, HikerError> {
    let (contents, hash) = vault.read_file_with_hash(rel)?;
    Ok(OpenForEditOutcome {
        contents,
        token: BufferToken::new(rel, &hash),
    })
}

/// Write a buffer's new text using the drift-check encoded in `token`.
///
/// On success, appends a `'modified'` (or `'created'` if the file didn't
/// exist) row to the changelog with `extra_metadata` merged in, and
/// returns `Written { new_hash, token }` — the new token replaces the
/// caller's prior one for the next commit.
///
/// On drift, returns `DriftDetected { current_disk_text, current_hash }`
/// instead of erroring. The adapter renders its modal and dispatches to
/// `resolve_drift` with the user's choice. Other I/O errors propagate as
/// before.
///
/// `extra_metadata` carries one-shot context (e.g.
/// `{ "mutation": "<kind>" }` per `note-mutation-stash-changes-tag`); a
/// non-object value is treated as `{}` to match the existing Tauri
/// command shape.
pub fn commit_buffer(
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    token: &BufferToken,
    new_text: &str,
    extra_metadata: serde_json::Value,
) -> Result<CommitOutcome, HikerError> {
    let rel = token.path();
    let abs = vault.abs_path(rel)?;
    let existed = abs.exists();

    // Drift inspection: re-read disk and compare its hash to the token's
    // captured hash. On mismatch we surface the on-disk state to the
    // caller via `DriftDetected` rather than erroring; the adapter then
    // dispatches to `resolve_drift`.
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let on_disk = String::from_utf8(bytes)
                .map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            let found = hash_str(&on_disk);
            if found != token.hash() {
                return Ok(CommitOutcome::DriftDetected {
                    current_disk_text: on_disk,
                    current_hash: found,
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !token.hash().is_empty() {
                return Ok(CommitOutcome::DriftDetected {
                    current_disk_text: String::new(),
                    current_hash: String::new(),
                });
            }
        }
        Err(e) => return Err(e.into()),
    }

    // Baseline-on-first-save: snapshot the pre-write state if no
    // changelog row exists for this path yet, so rollback has somewhere
    // to go. No-op when a row already exists. Read failures fall through
    // silently — better to log a baseline-less write than to refuse it.
    if existed
        && let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (commit_buffer)");
    }

    vault.write_file(rel, new_text)?;
    let new_hash = hash_str(new_text);

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let metadata = match extra_metadata {
        serde_json::Value::Object(_) => extra_metadata,
        _ => serde_json::json!({}),
    };
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(new_text.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    Ok(CommitOutcome::Written {
        new_hash: new_hash.clone(),
        token: BufferToken::new(rel, &new_hash),
    })
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the adapter; this is the typed surface for the action
/// each branch represents.
///
/// - `KeepMine` — unconditional write of `new_text`, append a changelog
///   row, return `Written { new_hash, token }`.
/// - `TakeTheirs` — read disk, return `TookTheirs { contents, token }`.
///   No write, no changelog row. Caller reseeds its buffer.
/// - `Cancel` — no-op. Caller leaves the buffer dirty so the next commit
///   re-prompts.
pub fn resolve_drift(
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    rel: &str,
    choice: DriftChoice,
    new_text: &str,
    extra_metadata: serde_json::Value,
) -> Result<DriftResolution, HikerError> {
    match choice {
        DriftChoice::KeepMine => {
            let abs = vault.abs_path(rel)?;
            let existed = abs.exists();
            if existed
                && let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
                && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
            {
                tracing::warn!(error = %e, "changes: ensure_baseline failed (resolve_drift keep_mine)");
            }
            vault.write_file(rel, new_text)?;
            let new_hash = hash_str(new_text);
            let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
            let metadata = match extra_metadata {
                serde_json::Value::Object(_) => extra_metadata,
                _ => serde_json::json!({}),
            };
            append_change_best_effort(
                changes,
                ChangeAppend {
                    path: rel,
                    op,
                    author: "user",
                    content_hash: Some(&new_hash),
                    content: Some(new_text.as_bytes()),
                    rename_from: None,
                    metadata,
                },
            );
            Ok(DriftResolution::Written {
                new_hash: new_hash.clone(),
                token: BufferToken::new(rel, &new_hash),
            })
        }
        DriftChoice::TakeTheirs => {
            let (contents, hash) = vault.read_file_with_hash(rel)?;
            Ok(DriftResolution::TookTheirs {
                contents,
                token: BufferToken::new(rel, &hash),
            })
        }
        DriftChoice::Cancel => Ok(DriftResolution::Cancelled),
    }
}

/// Ensure the note at `rel` has `hiker.id` set in its frontmatter. If it
/// already has one, return it. Otherwise mint a fresh ULID, write the
/// stamped file through the watcher-suppression + changelog pattern that
/// the agent-frontmatter ops use (author = `"user"` since stamping is
/// triggered by a user-initiated action — adding a waypoint, future
/// wikilink targeting, etc.), and return the new id.
///
/// Caller is responsible for invoking this lazily — i.e. only when a
/// note is about to become a reference target. Per the `lazy` mode in
/// `note-id-stamping`, un-referenced notes stay untouched. The `all`
/// mode's startup-pass that stamps every note proactively isn't wired
/// yet — see TODO in `core::indexer`'s startup scan.
///
/// status: note-id-stamping
///
/// `store` is the indexer's read-side store handle, used to **adopt** the
/// existing `path_ids` ULID for `rel` when the indexer has already minted
/// one for this path. This keeps the two ULID systems in lockstep:
/// `path_ids[rel] == frontmatter_hiker_id == every reference's recorded
/// id`. Without this, freshly-minted ULIDs from `new_id()` would not match
/// what `Store::id_for_path` later returns, so `resolve_reference` would
/// surface every just-stamped note as a `PathConflict` orphan in the
/// Trails sidebar (`bug-id-stamping-mints-fresh-ulid-instead-of-adopting-
/// path-ids`).
///
/// Edge case: if the source already carries a `hiker.id` in frontmatter
/// AND `path_ids` has a different id for the same path, that's a
/// pre-existing inconsistency we don't silently rewrite — the frontmatter
/// id is returned as-is (with a warn log) since clobbering a
/// user-visible value risks data loss. The bug at hand only manifests on
/// fresh stamps; pre-existing mismatches warrant a separate slug if they
/// ever appear in real data.
pub async fn ensure_note_id_stamped(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    store: &mut Store,
    rel: &str,
) -> Result<String, HikerError> {
    // Fast path: existing id, no write. If `path_ids` disagrees, log and
    // prefer the frontmatter id (see doc-comment edge-case note above).
    let existing = vault.read_file(rel)?;
    let split = crate::frontmatter::split(&existing);
    if let Some(id) = read_hiker_id(&split.frontmatter) {
        if let Ok(Some(path_id)) = store.id_for_path(rel)
            && path_id != id
        {
            tracing::warn!(
                path = %rel,
                frontmatter_id = %id,
                path_ids_id = %path_id,
                "ensure_note_id_stamped: pre-existing id mismatch; \
                 keeping frontmatter id (resolve_reference may surface \
                 PathConflict until reconciled)",
            );
        }
        return Ok(id);
    }

    // Mint + write. Mirror `agent_set_frontmatter`'s shape: suppress the
    // watcher around the write so notify can't surface a stale event,
    // baseline-snapshot if first touch, append a `'modified'`/`'created'`
    // changelog row, then re-suppress + re-index.
    //
    // Adopt the indexer's existing `path_ids` row when present, so the
    // stamped id matches what `Store::id_for_path` will return later.
    // Only mint a fresh ULID when the path has never been ingested.
    let new_id = match store.id_for_path(rel) {
        Ok(Some(existing)) => existing,
        Ok(None) => crate::store::new_id(),
        Err(e) => {
            tracing::warn!(
                path = %rel,
                error = %e,
                "ensure_note_id_stamped: id_for_path lookup failed; minting fresh",
            );
            crate::store::new_id()
        }
    };
    let patch = serde_json::json!({ "hiker": { "id": new_id.clone() } });
    let merged = merge_user_patch(&existing, patch)?;

    watcher.suppress(rel.to_string());

    if let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (id stamp)");
    }

    let abs = vault.abs_path(rel)?;
    let existed = abs.exists();
    vault.write_file(rel, &merged)?;
    let new_hash = hash_str(&merged);

    watcher.suppress(rel.to_string());

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let metadata = serde_json::json!({"reason": "note-id-stamping"});
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(merged.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: rel.to_string(),
            force: false,
        })
        .await;

    Ok(new_id)
}

/// Pull `hiker.id` out of an already-split frontmatter block, if present.
fn read_hiker_id(fm: &Option<serde_yml::Value>) -> Option<String> {
    let serde_yml::Value::Mapping(map) = fm.as_ref()? else { return None };
    let serde_yml::Value::Mapping(hiker) = map.get("hiker")? else { return None };
    hiker.get("id")?.as_str().map(|s| s.to_string())
}

/// Same as `frontmatter::merge_agent_patch` but does not stamp
/// `hiker.author = agent-authored` — the id-stamping path is user-
/// initiated, not agent-authored, so we keep author untouched. Other
/// `hiker.*` siblings round-trip unchanged.
fn merge_user_patch(
    source: &str,
    patch: serde_json::Value,
) -> Result<String, HikerError> {
    let split_view = crate::frontmatter::split(source);
    let mut fm = match split_view.frontmatter {
        Some(v) => v,
        None => serde_yml::Value::Mapping(Default::default()),
    };
    if !matches!(fm, serde_yml::Value::Mapping(_)) {
        fm = serde_yml::Value::Mapping(Default::default());
    }
    if let serde_json::Value::Object(_) = patch {
        crate::frontmatter::merge_json_into_yaml(&mut fm, patch);
    }
    crate::frontmatter::assemble(&fm, split_view.body)
        .map_err(|e| HikerError::Io(format!("frontmatter: {e}")))
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
