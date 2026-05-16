// ---------- changelog query / rollback commands ----------

use hiker_core::changes::{ChangeOp, ChangeRow, Changes};
use hiker_core::HikerError;
use serde::Serialize;
use tauri::State;

use crate::{log_cmd_result, AppState};

/// Most recent changelog rows across the whole vault. Backs the home-page
/// recent-activity widget preview and detail view.
///
/// status: vault-home-recent-activity-widget
#[tauri::command]
pub(crate) fn recent_changes(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<ChangeRow>, HikerError> {
    let result = (|| -> Result<Vec<ChangeRow>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .recent(limit.unwrap_or(50))
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("recent_changes", result)
}

/// Total changelog row count. Backs the widget's "any rows yet?" gate so a
/// post-upgrade fresh vault doesn't render a confusing zero-count tile.
#[tauri::command]
pub(crate) fn changes_count(state: State<AppState>) -> Result<i64, HikerError> {
    let result = (|| -> Result<i64, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .count()
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("changes_count", result)
}

/// Pull the post-op content blob for a change. Returns an empty string for
/// `op='deleted'` rows. Decoded as UTF-8 with a fallback to lossy so the
/// detail-view diff renderer always has something to show.
#[tauri::command]
pub(crate) fn change_content(
    state: State<AppState>,
    change_id: i64,
) -> Result<Option<String>, HikerError> {
    let result = (|| -> Result<Option<String>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let blob = session
            .changes
            .content_at(change_id)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        Ok(blob.map(|b| String::from_utf8_lossy(&b).into_owned()))
    })();
    log_cmd_result("change_content", result)
}

/// Roll the file at `change.path` back to the most recent prior content
/// before `change_id`. Implementation per `changes.md` "Rollback":
///
/// 1. Resolve `(prior_id, prior_content)` via `previous_content_for_path`.
/// 2. Write that content via the standard `write_file_checked` path. The
///    write itself appends a *new* `'modified'` row tagged with
///    `metadata.rolled_back_from` so the activity feed shows the linkage.
///
/// Errors:
/// - `not_found` — no prior content within retention; rollback impossible.
/// - `drift` — the on-disk file changed since the change row was appended.
///   Caller can prompt the user to overwrite.
///
/// status: changes-rollback-helper
/// status: vault-home-recent-activity-detail
#[tauri::command]
pub(crate) async fn rollback_change(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result("rollback_change", rollback_change_inner(state, change_id).await)
}

#[derive(Serialize)]
pub(crate) struct RollbackOutcome {
    /// The id of the change row whose content was just rolled back to.
    /// Used by the UI's un-rollback affordance ("recently rolled back —
    /// restore?") so it knows which path/state was just left behind.
    prior_change_id: i64,
    /// The path that was rolled back. Convenience for UI refresh; identical
    /// to the original change row's path field.
    path: String,
    /// New on-disk hash after the rollback write. The Tauri write also
    /// appended a new changelog row; the UI re-reads `recent_changes` to
    /// pick that up.
    new_hash: String,
}

async fn rollback_change_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    // Resolve everything off the session up front so we don't hold the
    // session lock across the await/IO of the write.
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let row = session
            .changes
            .recent(0)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        let _ = row; // shut clippy up; we instead query via history below.
        // Resolve the change's path via a direct lookup — `recent` would
        // miss rows past the default window. The history call filters by
        // path post-hoc, so we use a single-row query.
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let (prior_id, prior_bytes) = changes_arc
        .previous_content_for_path(&target_path, change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "no earlier version of {target_path} is recorded — this is the oldest change in the log for this file"
            ))
        })?;

    let prior_content = String::from_utf8(prior_bytes)
        .map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    // Compute current on-disk hash for the drift-aware write. Empty hash
    // when the file is missing — matches the contract of write_file_checked.
    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash = vault.write_file_checked(&target_path, &current_hash, &prior_content)?;

    // Append the rollback row directly (rather than relying on the `write_file`
    // command) so we can stamp `metadata.rolled_back_from = <change_id>` per
    // spec — and so the on-disk file write + changelog append happen here as
    // one logical step instead of being routed through the Tauri write_file
    // command which doesn't carry the metadata.
    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(prior_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"rolled_back_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (rollback) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: prior_id,
        path: target_path,
        new_hash,
    })
}

/// Restore the file's content to match the given snapshot row. Writes the
/// row's `content` blob back to its `path` and appends a new `'modified'`
/// row stamped `metadata.restored_from = change_id`.
///
/// Different from `rollback_change` (which uses
/// `previous_content_for_path` to walk *before* the change): this command
/// matches the snapshot mental model — each row IS a saved version, and
/// "Restore" writes that version. The two share the changelog primitives
/// but live side-by-side: agent rollback per `mcp.md` calls
/// `rollback_change`; the home-page activity widget calls
/// `restore_snapshot`.
///
/// Errors:
/// - `not_found` — change row doesn't exist or has no content (e.g. a
///   `'deleted'` row, which carries NULL content by design).
/// - `drift` — the on-disk file changed since `expected_hash` was taken.
///   Surfaced as the same drift error `write_file_checked` produces; the
///   UI prompts the user.
///
/// status: vault-home-recent-activity-detail
#[tauri::command]
pub(crate) async fn restore_snapshot(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result(
        "restore_snapshot",
        restore_snapshot_inner(state, change_id).await,
    )
}

async fn restore_snapshot_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let blob = changes_arc
        .content_at(change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "change {change_id} has no recorded content (deleted-row snapshots can't be restored directly — restore an earlier created/modified row instead)"
            ))
        })?;

    let snapshot_content =
        String::from_utf8(blob).map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash =
        vault.write_file_checked(&target_path, &current_hash, &snapshot_content)?;

    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(snapshot_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"restored_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (restore_snapshot) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: change_id,
        path: target_path,
        new_hash,
    })
}

/// Look up the path of a single change by id. Walks `recent` widely enough
/// to find it; rollback targets are usually recent so this is fine in
/// practice. Falls back to `NotFound` if the row is past the search window
/// (in which case retention has likely already dropped its content too).
fn lookup_change_path(changes: &Changes, change_id: i64) -> Result<String, HikerError> {
    // 5000 rows is well past the default 50-per-pair retention; if we don't
    // find it here, it's effectively gone.
    let rows = changes
        .recent(5000)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    rows.into_iter()
        .find(|r| r.id == change_id)
        .map(|r| r.path)
        .ok_or_else(|| HikerError::NotFound(format!("change {change_id}")))
}
