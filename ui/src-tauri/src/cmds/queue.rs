// Task queue commands. Standard wrapper shape over `core::tasks::Queue`.
//
// status: task-queue-row-details, task-queue-row-cancel-action

use tauri::State;

use crate::{log_cmd_result, with_session_async, AppState, CmdResult};

/// Snapshot of the current queue state — every non-terminal task plus the
/// most-recent terminal ones inside the retention window. Backed by
/// `core::tasks::Queue::snapshot`. The frontend's queue panel seeds itself
/// with this once at mount and applies `hiker:queue-event` deltas after.
#[tauri::command]
pub(crate) async fn tasks_snapshot(
    state: State<'_, AppState>,
) -> CmdResult<Vec<hiker_core::tasks::TaskRecord>> {
    let result = with_session_async(
        &state,
        |s| Ok(s.tasks.clone()),
        |queue| async move { Ok(queue.snapshot().await) },
    )
    .await;
    log_cmd_result("tasks_snapshot", result)
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
) -> CmdResult<Option<hiker_core::tasks::TaskDetails>> {
    let result = with_session_async(
        &state,
        |s| Ok(s.tasks.clone()),
        |queue| async move { Ok(queue.details(&id).await) },
    )
    .await;
    log_cmd_result("task_details", result)
}

/// status: task-queue-row-cancel-action
/// Cancel a task by id. Behavior depends on lease state — see
/// `core::tasks::Queue::cancel`.
#[tauri::command]
pub(crate) async fn tasks_cancel(
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<()> {
    let result = with_session_async(
        &state,
        |s| Ok(s.tasks.clone()),
        |queue| async move {
            queue.cancel(&id).await;
            Ok(())
        },
    )
    .await;
    log_cmd_result("tasks_cancel", result)
}
