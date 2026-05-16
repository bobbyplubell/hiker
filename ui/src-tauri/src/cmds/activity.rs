// ---------- unified activity feed (changes + staging) ----------

use hiker_core::activity::{ActivityFilter, ActivityItem, ActivitySource};
use serde::Deserialize;
use tauri::State;

use crate::{log_cmd_result, AppState};

/// Argument shape for `activity_list*` commands. Mirrors
/// `hiker_core::activity::ActivityFilter` but kept independent so the
/// snake_case JSON wire stays stable if the core struct gains fields.
#[derive(Debug, Deserialize)]
pub(crate) struct ActivityFilterArg {
    #[serde(default)]
    source: ActivitySource,
    #[serde(default = "default_activity_limit")]
    limit: usize,
    #[serde(default)]
    author_pattern: Option<String>,
    #[serde(default)]
    since_ms: Option<i64>,
}

fn default_activity_limit() -> usize {
    200
}

impl From<ActivityFilterArg> for ActivityFilter {
    fn from(a: ActivityFilterArg) -> Self {
        ActivityFilter {
            source: a.source,
            limit: a.limit,
            author_pattern: a.author_pattern,
            since_ms: a.since_ms,
        }
    }
}

// status: activity-feed-merged-query
#[tauri::command]
pub(crate) fn activity_list(
    state: State<'_, AppState>,
    filter: Option<ActivityFilterArg>,
) -> Result<Vec<ActivityItem>, String> {
    let result = (|| -> Result<Vec<ActivityItem>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session.activity.list(f).map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_list", result)
}

// status: activity-feed-merged-query
// status: status-bar-version-dropdown-uses-unified-feed
#[tauri::command]
pub(crate) fn activity_list_for_path(
    state: State<'_, AppState>,
    path: String,
    filter: Option<ActivityFilterArg>,
) -> Result<Vec<ActivityItem>, String> {
    let result = (|| -> Result<Vec<ActivityItem>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session
            .activity
            .list_for_path(&path, f)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_list_for_path", result)
}

// status: activity-feed-merged-query
#[tauri::command]
pub(crate) fn activity_count(
    state: State<'_, AppState>,
    filter: Option<ActivityFilterArg>,
) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session.activity.count(f).map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_count", result)
}
