//! Vault-home + note-metadata Tauri commands.
//!
//! Groups three closely related read-side surfaces:
//! - vault-home stats widgets (`vault_home_stats`, recent-modified,
//!   recent-accessed, `note_accessed` access-stamp writer)
//! - note properties tab (`note_properties` + DTO)
//! - chat `@`-mention autocomplete + resolver (`chat_at_autocomplete`,
//!   `chat_resolve_at_note`)
//!
//! All three pull off the per-session `read_store` and surface
//! "metadata about a note/vault" — bundling them here keeps `cmds/`
//! navigable without three tiny files.
//!
//! status: vault-home-stats-widget, vault-home-recent-modified,
//! vault-home-recent-accessed, note-access-tracking,
//! note-properties-tab-content, chat-input-at-note,
//! chat-input-at-autocomplete-tauri-cmd

use hiker_core::indexer::IndexJob;
use hiker_core::store::{RecentNote, VaultStats};
use serde::Serialize;
use tauri::State;

use crate::{log_cmd_result, AppState};

/// Vault home stats payload: cheap counts off the index store, plus the live
/// queued count from the indexer handle. Surfaced by the home page; refreshed
/// on every `hiker:reindex-progress` tick.
///
/// status: vault-home-stats-widget
#[derive(Serialize)]
pub(crate) struct VaultHomeStats {
    total_notes: u32,
    total_chunks: u32,
    indexed: u32,
    skipped: u32,
    queued: u32,
}

#[tauri::command]
pub(crate) fn vault_home_stats(state: State<AppState>) -> Result<VaultHomeStats, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let stats: VaultStats = read_store.vault_stats().map_err(|e| e.to_string())?;
        let queued = session.indexer.status().queued;
        Ok(VaultHomeStats {
            total_notes: stats.total_notes,
            total_chunks: stats.total_chunks,
            indexed: stats.indexed,
            skipped: stats.skipped,
            queued,
        })
    })();
    log_cmd_result("vault_home_stats", result)
}

/// Top-N notes by filesystem mtime DESC. Backs the vault-home recently-modified
/// widget.
///
/// status: vault-home-recent-modified
#[tauri::command]
pub(crate) fn recent_notes_modified(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_mtime(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_modified", result)
}

/// Top-N notes by `last_accessed_at` DESC. Backs the vault-home
/// recently-accessed widget.
///
/// status: vault-home-recent-accessed
#[tauri::command]
pub(crate) fn recent_notes_accessed(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_access(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_accessed", result)
}

/// Stamp `notes.last_accessed_at` via the indexer's owned writer. Called
/// from the frontend whenever a note becomes the active buffer. No-op when
/// the note isn't yet in the index — the next ingest creates the row, and
/// subsequent opens record normally.
///
/// status: note-access-tracking
#[tauri::command]
pub(crate) async fn note_accessed(state: State<'_, AppState>, rel: String) -> Result<(), String> {
    let jobs = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.indexer.job_sender()
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let send_result = jobs
        .send(IndexJob::TouchAccess { rel_path: rel, ts })
        .await
        .map_err(|e| e.to_string());
    log_cmd_result("note_accessed", send_result)
}

// status: note-properties-tab-content
/// DTO returned by `note_properties`. Mirrors `core::store::NoteProperties`
/// plus the changes count from `core::changes`. The struct uses the same
/// `#[serde(rename_all = "camelCase")]` as the core type.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePropertiesDto {
    pub path: String,
    pub note_id: Option<String>,
    pub path_ids_id: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
    pub content_hash: Option<String>,
    pub extension: Option<String>,
    pub indexed_at: Option<i64>,
    pub embedder_version: Option<String>,
    pub skipped: Option<bool>,
    pub skip_reason: Option<String>,
    pub chunk_count: Option<i64>,
    pub last_accessed_at: Option<i64>,
    pub change_count: Option<i64>,
}

#[tauri::command]
pub(crate) fn note_properties(
    state: State<AppState>,
    rel: String,
) -> Result<NotePropertiesDto, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let mut props = read_store
            .note_properties(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("note not indexed: {rel}"))?;
        let change_count = session
            .changes
            .count_for_path(&rel)
            .map_err(|e| e.to_string())?;
        props.change_count = Some(change_count);
        Ok(NotePropertiesDto {
            path: props.path,
            note_id: props.note_id,
            path_ids_id: props.path_ids_id,
            mtime: props.mtime,
            size: props.size,
            content_hash: props.content_hash,
            extension: props.extension,
            indexed_at: props.indexed_at,
            embedder_version: props.embedder_version,
            skipped: props.skipped,
            skip_reason: props.skip_reason,
            chunk_count: props.chunk_count,
            last_accessed_at: props.last_accessed_at,
            change_count: props.change_count,
        })
    })();
    log_cmd_result("note_properties", result)
}

/// Resolve a chat `@<rel-path-without-extension>` token to a concrete
/// vault path + file body. Probes `.md`, `.markdown`, `.txt` in order.
/// Errors with "note not found: <rel>" if no extension resolves — the
/// frontend toasts this and aborts the send (per `chat-input-at-note`).
///
/// status: chat-input-at-note
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtNoteResolved {
    pub rel_path: String,
    pub content: String,
}

#[tauri::command]
pub(crate) fn chat_resolve_at_note(
    state: State<AppState>,
    rel_no_ext: String,
) -> Result<AtNoteResolved, String> {
    let result = (|| -> Result<AtNoteResolved, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let vault = session.vault.clone();
        drop(guard);
        for ext in hiker_core::indexer::INDEXABLE_EXTENSIONS {
            let candidate = format!("{}.{}", rel_no_ext, ext);
            if let Ok(abs) = vault.abs_path(&candidate) {
                if abs.is_file() {
                    if let Ok(content) = vault.read_file(&candidate) {
                        return Ok(AtNoteResolved {
                            rel_path: candidate,
                            content,
                        });
                    }
                }
            }
        }
        Err(format!("note not found: {rel_no_ext}"))
    })();
    log_cmd_result("chat_resolve_at_note", result)
}

/// Notes-table autocomplete for the chat `@`-mention popover. Empty
/// `prefix` returns the most-recently-accessed notes; non-empty filters by
/// case-insensitive basename substring with prefix-matches ranked first.
/// `limit` defaults to 10 to match the spec.
///
/// status: chat-input-at-autocomplete-tauri-cmd
#[tauri::command]
pub(crate) fn chat_at_autocomplete(
    state: State<AppState>,
    prefix: String,
    limit: Option<u32>,
) -> Result<Vec<hiker_core::store::AtSuggestion>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .at_autocomplete(&prefix, limit.unwrap_or(10) as usize)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("chat_at_autocomplete", result)
}
