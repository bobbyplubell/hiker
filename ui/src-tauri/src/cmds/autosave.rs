use hiker_core::autosave::{Autosave, RecoveredEntry, TabState};
use serde::Serialize;
use tauri::State;

use crate::{log_cmd_result, AppState};

fn with_autosave<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Autosave) -> Result<R, String>,
) -> Result<R, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(&session.autosave)
}

#[tauri::command]
pub(crate) fn autosave_write(
    state: State<AppState>,
    path: String,
    contents: String,
) -> Result<(), String> {
    // Hash on the backend — frontend doesn't carry a blake3 dep, and
    // hashing a markdown buffer at 5s tick cadence is sub-millisecond
    // anyway. Same hash function (blake3) the rest of core uses, so
    // recover()'s on-disk-hash comparison stays apples-to-apples.
    let bytes = contents.as_bytes();
    let buffer_hash = hiker_core::hash_str(&contents);
    log_cmd_result(
        "autosave_write",
        with_autosave(&state, |a| {
            a.write(&path, bytes, &buffer_hash)
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
pub(crate) fn autosave_clear(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_clear",
        with_autosave(&state, |a| a.clear(&path).map_err(|e| e.to_string())),
    )
}

#[tauri::command]
pub(crate) fn autosave_save_tab_state(
    state: State<AppState>,
    state_payload: TabState,
) -> Result<(), String> {
    log_cmd_result(
        "autosave_save_tab_state",
        with_autosave(&state, |a| {
            a.save_tab_state(state_payload).map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
pub(crate) fn autosave_load_tab_state(state: State<AppState>) -> Result<Option<TabState>, String> {
    log_cmd_result(
        "autosave_load_tab_state",
        with_autosave(&state, |a| a.load_tab_state().map_err(|e| e.to_string())),
    )
}

/// Wire DTO for `autosave_recover` — the autosaved bytes ride as a UTF-8
/// string since hiker is a markdown editor and the frontend's CM6 can
/// only restore text-typed content. Non-UTF-8 sidecars (which shouldn't
/// happen for markdown buffers) become lossy strings; the recovery flow
/// still surfaces them so the user isn't silently denied their work.
#[derive(Serialize)]
pub(crate) struct RecoveredEntryDto {
    path: String,
    autosave_id: String,
    autosaved_content: String,
    autosaved_hash: String,
    on_disk_hash: Option<String>,
    saved_at_ms: i64,
}

impl From<RecoveredEntry> for RecoveredEntryDto {
    fn from(e: RecoveredEntry) -> Self {
        Self {
            path: e.path,
            autosave_id: e.autosave_id,
            autosaved_content: String::from_utf8_lossy(&e.autosaved_content).into_owned(),
            autosaved_hash: e.autosaved_hash,
            on_disk_hash: e.on_disk_hash,
            saved_at_ms: e.saved_at_ms,
        }
    }
}

#[tauri::command]
pub(crate) fn autosave_recover(state: State<AppState>) -> Result<Vec<RecoveredEntryDto>, String> {
    log_cmd_result(
        "autosave_recover",
        with_autosave(&state, |a| {
            a.recover()
                .map(|v| v.into_iter().map(RecoveredEntryDto::from).collect())
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
pub(crate) fn autosave_discard(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_discard",
        with_autosave(&state, |a| a.discard(&path).map_err(|e| e.to_string())),
    )
}
