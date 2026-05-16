// ---------------------------------------------------------------------------
// Trails commands (slice U1)
// ---------------------------------------------------------------------------
// Tauri seams over `hiker_core::trails::*`. Each is the standard
// `parse args -> snapshot session deps -> call core -> return DTO` shape
// (see the rule in `hiker-dev` skill: commands are wrappers, not
// orchestrators). The `core::trails` ops own watcher suppression and
// changes append; this layer just plumbs the session deps in.
//
// status: active-trail-state

use hiker_core::config::SettingsScope;
use hiker_core::store::Store;
use hiker_core::trash::{Trash, TrashEntry};
use hiker_core::{config::Config, HikerError};
use serde::Serialize;
use tauri::State;

use crate::{log_cmd_result, AppState};

#[derive(Serialize)]
pub(crate) struct TrailCreatedDto {
    trail_doc_rel: String,
    trail_id: String,
}

#[derive(Serialize)]
pub(crate) struct WaypointAppendedDto {
    waypoint_rel: String,
    waypoint_id: String,
    trail_id: String,
}

#[derive(Serialize)]
pub(crate) struct WaypointRemovedDto {
    removed_count: u32,
}

#[tauri::command]
pub(crate) async fn trail_create(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let result = trail_create_inner(state, name).await;
    log_cmd_result("trail_create", result)
}

async fn trail_create_inner(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let (watcher, vault, jobs, changes, trails_cfg) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let cfg = session
            .config
            .read()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            cfg.trails.clone(),
        )
    };
    let outcome = hiker_core::trails::create_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trails_cfg,
        &name,
    )
    .await?;
    Ok(TrailCreatedDto {
        trail_doc_rel: outcome.trail_doc_rel,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
pub(crate) async fn trail_append_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
    let result = trail_append_waypoint_inner(
        state,
        trail_doc_rel,
        source_rel,
        parent_waypoint_id,
        annotation,
    )
    .await;
    log_cmd_result("trail_append_waypoint", result)
}

async fn trail_append_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
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
    // Open a fresh Store reader for the call. `Store::open` against an
    // existing db is cheap (sub-ms warm) and is the documented per-command
    // read pattern (see `VaultSession.read_store` doc-comment). We can't
    // hand out the shared `Arc<Mutex<Store>>` here because the call holds
    // the reference across `.await` points and `MutexGuard` isn't `Send`.
    let mut store = Store::open(vault.root()).map_err(|e| HikerError::Io(e.to_string()))?;
    let outcome = hiker_core::trails::append_waypoint(
        hiker_core::trails::AppendWaypointArgs {
            watcher: &watcher,
            jobs: &jobs,
            vault: &vault,
            changes: Some(&changes),
            store: &mut store,
            trail_doc_rel: &trail_doc_rel,
            source_rel: &source_rel,
            parent_waypoint_id: parent_waypoint_id.as_deref(),
            annotation: annotation.as_deref(),
        },
    )
    .await?;
    Ok(WaypointAppendedDto {
        waypoint_rel: outcome.waypoint_rel,
        waypoint_id: outcome.waypoint_id,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
pub(crate) async fn trail_remove_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let result = trail_remove_waypoint_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_remove_waypoint", result)
}

async fn trail_remove_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
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
            Trash::open(session.vault.root()),
        )
    };
    let outcome = hiker_core::trails::remove_waypoint(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
        &waypoint_id,
    )
    .await?;
    Ok(WaypointRemovedDto {
        removed_count: outcome.removed_count,
    })
}

#[tauri::command]
pub(crate) fn trail_descendant_count(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<u32, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        hiker_core::trails::descendant_count(
            &session.vault,
            &trail_doc_rel,
            &waypoint_id,
        )
    })();
    log_cmd_result("trail_descendant_count", result)
}

#[tauri::command]
pub(crate) async fn trail_delete(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<(), HikerError> {
    let result = trail_delete_inner(app, state, trail_doc_rel).await;
    log_cmd_result("trail_delete", result.map(|_| ()))
}

async fn trail_delete_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
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
            Trash::open(session.vault.root()),
        )
    };
    let entry = hiker_core::trails::delete_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
    )
    .await?;
    // Trash bin auto-refresh hook — same shape as `delete_note_inner`.
    crate::events::emit_trash_changed(&app);
    Ok(entry)
}

#[tauri::command]
pub(crate) fn trails_list(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::trails::TrailListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::list_trails(&session.vault, &store)
    })();
    log_cmd_result("trails_list", result)
}

#[tauri::command]
pub(crate) fn trail_get(
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<hiker_core::trails::TrailDetail, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::get_trail(&session.vault, &store, &trail_doc_rel)
    })();
    log_cmd_result("trail_get", result)
}

/// Reverse-lookup: which trails contain `source_rel` as a waypoint at
/// any depth. Each hit pairs the derived-table `trail_id` with its
/// trail-doc rel-path so the UI can decide membership for a specific
/// trail (e.g. "is this note already a waypoint of the *active* trail?")
/// without a second round-trip per trail.
///
/// status: trail-add-to-active-from-editor-verb
#[tauri::command]
pub(crate) fn trails_containing_note(
    state: State<'_, AppState>,
    source_rel: String,
) -> Result<Vec<hiker_core::trails::TrailsContainingNoteHit>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::trails_containing_note_with_paths(
            &session.vault,
            &store,
            &source_rel,
        )
    })();
    log_cmd_result("trails_containing_note", result)
}

/// Set (or clear, with `None`) the active trail. Persists
/// `vault.active_trail` via the standard settings write-back path and
/// stamps `hiker.last_activated_at` on the trail-doc when activating.
///
/// status: active-trail-state
#[tauri::command]
pub(crate) async fn trail_set_active(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_active_inner(state, trail_doc_rel).await;
    log_cmd_result("trail_set_active", result)
}

async fn trail_set_active_inner(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    // Snapshot deps for the (optional) timestamp stamp before we touch
    // the settings file.
    let (watcher, vault, jobs, changes, root) = {
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
            session.root.clone(),
        )
    };

    // Stamp the trail-doc's `last_activated_at` first (only when
    // activating a non-None value). If stamping fails we still proceed
    // to persist the setting — the timestamp is dropdown-ordering chrome,
    // not load-bearing for activation correctness.
    if let Some(rel) = trail_doc_rel.as_deref()
        && let Err(e) = hiker_core::trails::stamp_last_activated_at(
            &watcher,
            &jobs,
            &vault,
            Some(&changes),
            rel,
        )
        .await
    {
        tracing::warn!(error = %e, path = %rel,
            "trail_set_active: stamp_last_activated_at failed; proceeding");
    }

    let value = match trail_doc_rel {
        Some(s) => serde_json::Value::String(s),
        None => serde_json::Value::Null,
    };
    let updated =
        Config::set(SettingsScope::Vault, "vault.active_trail", value, &root)?;
    {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated;
    }
    Ok(())
}

/// Set (or clear with `None`) the trail-doc's append cursor. Used by
/// the "Append from here" waypoint verb (`trail-append-from-here-verb`)
/// and the Trails-mode header's "Reset to main line" action
/// (`trail-reset-cursor-verb`); both surfaces land in slice C2.
///
/// status: trail-append-cursor
#[tauri::command]
pub(crate) async fn trail_set_append_cursor(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_append_cursor_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_set_append_cursor", result)
}

async fn trail_set_append_cursor_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
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
    hiker_core::trails::set_append_cursor(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trail_doc_rel,
        waypoint_id.as_deref(),
    )
    .await
}
