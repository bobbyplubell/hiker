// Task queue commands. Standard wrapper shape over `core::tasks::Queue`.
//
// status: task-queue-row-details, task-queue-row-cancel-action

use tauri::State;

use crate::{log_cmd_result, AppState};

/// Snapshot of the current queue state — every non-terminal task plus the
/// most-recent terminal ones inside the retention window. Backed by
/// `core::tasks::Queue::snapshot`. The frontend's queue panel seeds itself
/// with this once at mount and applies `hiker:queue-event` deltas after.
#[tauri::command]
pub(crate) async fn tasks_snapshot(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::tasks::TaskRecord>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("tasks_snapshot", Ok::<_, String>(queue.snapshot().await))
}

/// status: task-queue-row-details
/// Lazy inspection: prompt + final result + final error + metadata for
/// a single task id. Returns `None` if the id has already been GC'd
/// past `terminal_retention_secs` (the user can scroll the queue tile
/// fast enough to miss the row).
#[tauri::command]
pub(crate) async fn task_details(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<hiker_core::tasks::TaskDetails>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("task_details", Ok::<_, String>(queue.details(&id).await))
}

/// status: task-queue-row-cancel-action
/// Cancel a task by id. Behavior depends on lease state — see
/// `core::tasks::Queue::cancel`.
#[tauri::command]
pub(crate) async fn tasks_cancel(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    queue.cancel(&id).await;
    log_cmd_result("tasks_cancel", Ok::<(), String>(()))
}
