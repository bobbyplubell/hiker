//! Vault file IO + note/trash mutation Tauri commands.
//!
//! Two cohesive groups live here:
//!
//! 1. **File IO** — `list_dir`, `read_file(_with_hash)`, `write_file(_checked)`,
//!    `open_for_edit` / `commit_buffer` / `resolve_drift`. Plus the
//!    `merge_extra_metadata` helper and the `FileWithHash` DTO they share.
//! 2. **Note + trash mutations** — `create_note`, `move_note`, `move_folder`,
//!    `delete_note`, `restore_trash_entry`, `list_trash`, `empty_trash`,
//!    `permanent_delete_trash_entry`, plus `reveal_in_file_manager` /
//!    `reveal_path` and the `chunks_for` companion.
//!
//! `reveal_path` is `pub(crate)` because `lib.rs::reveal_config_file` also
//! calls it. Bootstrap-only commands (`open_vault_at`, recheck spawners)
//! stay in `lib.rs` — they're not about the vault file surface, they wire
//! the session up.
//!
//! status: create-note-button, drag-and-drop-move, status-bar-path-reveal,
//! delete-note-core-cmd, vault-trash-restore, tree-trash-disk-listing,
//! vault-trash-empty, tree-trash-restore-action, tauri-cmd-chunks-for-path

use hiker_core::changes::ChangeOp;
use hiker_core::config::TreeSortBy;
use hiker_core::store::ChunkBounds;
use hiker_core::trash::{Trash, TrashEntry, TrashListItem};
use hiker_core::{DirEntryDto, HikerError};
use serde::Serialize;
use tauri::State;

use crate::{log_cmd_result, with_session, with_vault, AppState, CmdResult};

#[tauri::command]
pub(crate) fn list_dir(
    state: State<AppState>,
    rel: String,
    sort: Option<TreeSortBy>,
) -> CmdResult<Vec<DirEntryDto>> {
    let result = with_session(&state, |session| {
        let order = match sort {
            Some(o) => o,
            None => session.config.read()?.vault.tree.sort_by,
        };
        Ok(session.vault.list_dir(&rel, order)?)
    });
    log_cmd_result("list_dir", result)
}

#[tauri::command]
pub(crate) fn read_file(state: State<AppState>, rel: String) -> CmdResult<String> {
    log_cmd_result(
        "read_file",
        with_vault(&state, |v| Ok(v.read_file(&rel)?)),
    )
}

#[derive(Serialize)]
pub(crate) struct FileWithHash {
    contents: String,
    hash: String,
}

#[tauri::command]
pub(crate) fn read_file_with_hash(state: State<AppState>, rel: String) -> CmdResult<FileWithHash> {
    log_cmd_result(
        "read_file_with_hash",
        with_vault(&state, |v| {
            let (contents, hash) = v.read_file_with_hash(&rel)?;
            Ok(FileWithHash { contents, hash })
        }),
    )
}

/// status: note-mutation-stash-changes-tag
/// Build the `metadata` JSON for a save's changes-row. Frontend may pass
/// `extra_metadata` to stamp one-shot context (e.g.
/// `{ "mutation": "<kind>" }` for the save that accepts an in-buffer
/// mutation). Object inputs are taken as-is; non-object / `None` falls
/// back to the empty object — same default as before this hook landed.
fn merge_extra_metadata(extra: Option<serde_json::Value>) -> serde_json::Value {
    match extra {
        Some(serde_json::Value::Object(_)) => extra.unwrap(),
        _ => serde_json::json!({}),
    }
}

#[tauri::command]
pub(crate) fn write_file(
    state: State<AppState>,
    rel: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> CmdResult<()> {
    let result = with_session(&state, |session| {
        let abs = session.vault.abs_path(&rel)?;
        let existed = abs.exists();
        // Baseline-on-first-save: if the file already existed but the
        // changelog has no row for it, snapshot the pre-write state so
        // rollback of this save has somewhere to go. Read failures fall
        // through silently — better to log a hash-less save than refuse
        // the write.
        if existed
            && let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel)
            && let Err(e) = session.changes.ensure_baseline(
                &rel,
                "user",
                pre_text.as_bytes(),
                &pre_hash,
            )
        {
            tracing::warn!(error = %e, "changes: ensure_baseline failed");
        }
        session.vault.write_file(&rel, &contents)?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        let hash = hiker_core::hash_str(&contents);
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file) failed");
        }
        Ok(())
    });
    log_cmd_result("write_file", result)
}

/// Open `rel` for editing — read its bytes and mint an opaque
/// `BufferToken`. The UI seeds CM6 with `contents` and round-trips the
/// token verbatim through `commit_buffer`; it never holds the hash.
#[tauri::command]
pub(crate) fn open_for_edit(
    state: State<AppState>,
    rel: String,
) -> Result<hiker_core::ops::OpenForEditOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::open_for_edit(&session.vault, &rel)
    })();
    log_cmd_result("open_for_edit", result)
}

/// Commit a buffer's new text using the drift-check encoded in `token`.
/// Returns `Written { new_hash, token }` on success or `DriftDetected
/// { current_disk_text, current_hash }` on conflict — the UI shows its
/// modal and dispatches to `resolve_drift`.
#[tauri::command]
pub(crate) fn commit_buffer(
    state: State<AppState>,
    token: hiker_core::ops::BufferToken,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::CommitOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::commit_buffer(
            &session.vault,
            Some(&session.changes),
            &token,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("commit_buffer", result)
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the UI; this is the typed action surface.
#[tauri::command]
pub(crate) fn resolve_drift(
    state: State<AppState>,
    rel: String,
    choice: hiker_core::ops::DriftChoice,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::DriftResolution, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::resolve_drift(
            &session.vault,
            Some(&session.changes),
            &rel,
            choice,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("resolve_drift", result)
}

#[tauri::command]
pub(crate) fn write_file_checked(
    state: State<AppState>,
    rel: String,
    expected_hash: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        // Detect created-vs-modified before the write. The drift check
        // upstream means `expected_hash` is empty for first-write (file
        // missing); after the write we tag the row accordingly.
        let abs = session.vault.abs_path(&rel)?;
        let existed = abs.exists();
        // Baseline-on-first-save: snapshot the pre-write content before
        // overwriting so rollback of this save restores the prior state.
        // No-op when the changelog already has a row for this path.
        if existed
            && let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel)
            && let Err(e) = session.changes.ensure_baseline(
                &rel,
                "user",
                pre_text.as_bytes(),
                &pre_hash,
            )
        {
            tracing::warn!(error = %e, "changes: ensure_baseline failed");
        }
        let new_hash = session
            .vault
            .write_file_checked(&rel, &expected_hash, &contents)?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file_checked) failed");
        }
        Ok(new_hash)
    })();
    log_cmd_result("write_file_checked", result)
}

/// Create a new empty note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed `new-note-N.md` name. Returns the rel path of the
/// file actually created so the UI can open and inline-rename it.
///
/// status: create-note-button
#[tauri::command]
pub(crate) async fn create_note(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    log_cmd_result("create_note", create_note_inner(state, folder).await)
}

async fn create_note_inner(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::create_with_suffix(&watcher, &jobs, &vault, Some(&changes), &folder, "new-note").await
}

/// Atomic note rename. Backs both tree drag-and-drop and inline rename of
/// freshly-created notes. Errors leave both sides untouched per the spec.
///
/// status: drag-and-drop-move
#[tauri::command]
pub(crate) async fn move_note(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_note", move_note_inner(state, from, to).await)
}

async fn move_note_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_note(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Reveal a vault note in the OS file manager (Finder on macOS, Explorer on
/// Windows, default file manager on Linux). Backs the status-bar basename
/// click target.
///
/// status: status-bar-path-reveal
#[tauri::command]
pub(crate) fn reveal_in_file_manager(state: State<AppState>, rel: String) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let abs = session.vault.abs_path(&rel)?;
        reveal_path(&abs).map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("reveal_in_file_manager", result)
}

/// Spawn the platform's reveal-in-file-manager command. Runs the spawn
/// without waiting — the file manager UI is the user's concern, not ours.
#[cfg(target_os = "macos")]
pub(crate) fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg("-R").arg(abs).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", abs.display()))
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    // Linux has no portable "select this file" verb. Open the parent
    // directory in the user's file manager — close enough that the user
    // can pick the file out by sight, no dependency on a specific DE.
    let target = abs.parent().unwrap_or(abs);
    std::process::Command::new("xdg-open").arg(target).spawn()?;
    Ok(())
}

/// Folder rename: fs rename of the whole directory + bulk store path remap
/// for every contained `.md` file. Backs tree drag-and-drop of folder rows.
/// Empty subfolders move with the rename for free.
#[tauri::command]
pub(crate) async fn move_folder(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_folder", move_folder_inner(state, from, to).await)
}

async fn move_folder_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_folder(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Soft-delete a note or folder. Backs the tree context-menu Delete entry
/// (`tree-context-delete`). Mirrors `move_note` shape: suppress watcher,
/// route through the indexer task so all writes go through its owned store
/// connection, await the reply, re-suppress for the post-op TTL window.
///
/// status: delete-note-core-cmd
#[tauri::command]
pub(crate) async fn delete_note(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result("delete_note", delete_note_inner(app, state, rel).await)
}

async fn delete_note_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::delete(&watcher, &jobs, &vault, Some(&changes), &rel).await;
    // Trash bin auto-refresh hook: forgetting this emit breaks
    // `tree-trash-flat-by-deleted` silently, so it stays in the Tauri layer
    // (core::ops doesn't depend on tauri).
    if result.is_ok() {
        crate::events::emit_trash_changed(&app);
    }
    result
}

/// Restore a previously soft-deleted entry from the vault trash. Backs the
/// undo affordance on the post-delete toast (`tree-context-delete`) and the
/// CLI `hiker trash restore` command.
///
/// status: vault-trash-restore
#[tauri::command]
pub(crate) async fn restore_trash_entry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result(
        "restore_trash_entry",
        restore_trash_entry_inner(app, state, id).await,
    )
}

async fn restore_trash_entry_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, trash, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            Trash::open(session.vault.root()),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::restore(&watcher, &jobs, &vault, Some(&changes), &trash, &id).await;
    // Trash bin auto-refresh hook — kept in the Tauri layer, see
    // `delete_note_inner` for the same rationale.
    if result.is_ok() {
        crate::events::emit_trash_changed(&app);
    }
    result
}

/// Disk-true listing of the vault trash. Backs the trash bin pinned at the
/// top of the file tree.
///
/// status: tree-trash-disk-listing
#[tauri::command]
pub(crate) fn list_trash(state: State<AppState>) -> Result<Vec<TrashListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.list_from_disk()
    })();
    log_cmd_result("list_trash", result)
}

/// Permanently empty the vault trash. Irrecoverable.
///
/// status: vault-trash-empty
#[tauri::command]
pub(crate) fn empty_trash(app: tauri::AppHandle, state: State<AppState>) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.empty()
    })();
    if result.is_ok() {
        crate::events::emit_trash_changed(&app);
    }
    log_cmd_result("empty_trash", result)
}

/// Permanently delete a single trash entry by its on-disk basename. Works on
/// orphaned entries too.
///
/// status: tree-trash-restore-action
#[tauri::command]
pub(crate) fn permanent_delete_trash_entry(
    app: tauri::AppHandle,
    state: State<AppState>,
    trashed_name: String,
) -> Result<(), HikerError> {
    tracing::info!(
        command = "permanent_delete_trash_entry",
        trashed_name = %trashed_name,
        "tauri cmd",
    );
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.permanent_delete(&trashed_name)
    })();
    if result.is_ok() {
        crate::events::emit_trash_changed(&app);
    }
    log_cmd_result("permanent_delete_trash_entry", result)
}

/// Ordered chunk bounds for the active note. Empty vec when the note has
/// no row in the store (unsupported / queued / never indexed) or has zero
/// chunks. Spec: never errors on absence.
///
/// status: tauri-cmd-chunks-for-path
#[tauri::command]
pub(crate) fn chunks_for(state: State<AppState>, rel: String) -> CmdResult<Vec<ChunkBounds>> {
    let result = with_session(&state, |session| {
        let mut bounds = {
            let read_store = session.read_store.lock()?;
            read_store.chunk_bounds_for(&rel).map_err(|e| e.to_string())?
        };
        // Read the file once and enrich each row's UTF-8 byte offsets with
        // matching UTF-16 char offsets. JS strings (and CM6) index by UTF-16
        // code units, so this saves the frontend from re-doing the encode
        // step every time it wants to map a chunk into the editor.
        if !bounds.is_empty()
            && let Ok(text) = session.vault.read_file(&rel)
        {
            hiker_core::store::enrich_char_offsets(&text, &mut bounds);
        }
        Ok(bounds)
    });
    log_cmd_result("chunks_for", result)
}
