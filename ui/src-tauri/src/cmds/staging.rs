// Tauri command surface for core::staging. Each command is the standard
// shape: parse args → snapshot session deps → call core → translate errors
// → return DTO.
//
// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject

use hiker_core::staging::{AcceptOutcome, Proposal, StagingFilter};
use serde::Deserialize;
use tauri::State;

use crate::{log_cmd_result, with_session, AppState, CmdResult};

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StagingFilterArg {
    path: Option<String>,
    trail_id: Option<String>,
    surface: Option<String>,
    session_id: Option<String>,
}

impl From<StagingFilterArg> for StagingFilter {
    fn from(a: StagingFilterArg) -> Self {
        StagingFilter {
            path: a.path,
            trail_id: a.trail_id,
            surface: a.surface,
            session_id: a.session_id,
            state: None,
        }
    }
}

#[tauri::command]
pub(crate) fn staging_list(
    state: State<'_, AppState>,
    filter: Option<StagingFilterArg>,
) -> CmdResult<Vec<Proposal>> {
    let result = with_session(&state, |session| {
        let f: StagingFilter = filter.unwrap_or_default().into();
        Ok(session.staging.list(&f).map_err(|e| e.to_string())?)
    });
    log_cmd_result("staging_list", result)
}

#[tauri::command]
pub(crate) fn staging_count(state: State<'_, AppState>) -> CmdResult<u32> {
    let result = with_session(&state, |session| {
        Ok(session
            .staging
            .count(&StagingFilter::default())
            .map_err(|e| e.to_string())?)
    });
    log_cmd_result("staging_count", result)
}

#[tauri::command]
pub(crate) fn staging_accept(
    state: State<'_, AppState>,
    proposal_id: String,
) -> CmdResult<AcceptOutcome> {
    let result = with_session(&state, |session| {
        let outcome = session
            .staging
            .accept(&proposal_id, &session.vault, Some(&session.changes))
            .map_err(|e| e.to_string())?;
        Ok(outcome)
    });
    log_cmd_result("staging_accept", result)
}

#[tauri::command]
pub(crate) fn staging_reject(
    state: State<'_, AppState>,
    proposal_id: String,
) -> CmdResult<()> {
    let result = with_session(&state, |session| {
        session
            .staging
            .reject(&proposal_id)
            .map_err(|e| e.to_string())?;
        Ok(())
    });
    log_cmd_result("staging_reject", result)
}

#[tauri::command]
pub(crate) fn staging_accept_all(
    state: State<'_, AppState>,
) -> CmdResult<Vec<AcceptOutcome>> {
    let result = with_session(&state, |session| {
        Ok(session
            .staging
            .accept_all(
                &StagingFilter::default(),
                &session.vault,
                Some(&session.changes),
            )
            .map_err(|e| e.to_string())?)
    });
    log_cmd_result("staging_accept_all", result)
}

/// Read the proposed `.md` content for a staging proposal so the frontend
/// can open it as a read-only preview buffer with the snapshot-preview diff
/// toggle pattern.
///
/// status: staging-review-activity-detail-filter
#[tauri::command]
pub(crate) fn staging_content(
    state: State<'_, AppState>,
    proposal_id: String,
) -> CmdResult<String> {
    let result = with_session(&state, |session| {
        Ok(session
            .staging
            .content(&proposal_id)
            .map_err(|e| e.to_string())?)
    });
    log_cmd_result("staging_content", result)
}
