//! Indexer-shaped Tauri commands.
//!
//! `index` / `index_status` / `index_state_for` / `count_notes_in` /
//! `compute_diff` — thin wrappers over `core::indexer` + the per-session
//! `IndexerHandle` + read store. The `compute_diff` command is pure
//! text-in / diff-out and lives here for proximity to `index_state_for`
//! (both are read-side helpers exercised by the same UI surfaces).
//!
//! status: tauri-cmd-file-index-state, diff-core-module

use hiker_core::indexer::{IndexJob, IndexStatus};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{log_cmd_result, with_session, AppState, CmdResult};

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum IndexScope {
    All,
    Path { rel: String },
}

#[tauri::command]
pub(crate) async fn index(state: State<'_, AppState>, scope: IndexScope) -> CmdResult<()> {
    let result = (|| -> CmdResult<(IndexJob, hiker_core::indexer::IndexJobTx)> {
        let job_sender = with_session(&state, |s| Ok(s.indexer.job_sender()))?;
        let job = match scope {
            // Explicit user-driven reindex: bypass the hash short-circuit so a
            // click on the menu actually re-embeds even when content is unchanged.
            IndexScope::All => IndexJob::FullScan { force: true },
            IndexScope::Path { rel } => IndexJob::Upsert { rel_path: rel, force: true },
        };
        Ok((job, job_sender))
    })();
    let send_result: CmdResult<()> = match result {
        Ok((job, sender)) => {
            sender.send(job).await?;
            Ok(())
        }
        Err(e) => Err(e),
    };
    log_cmd_result("index", send_result)
}

/// Per-file index state for the tree-row markers and the active-file
/// status-bar mirror. See docs/index.md `tauri-cmd-file-index-state`.
///
/// status: tauri-cmd-file-index-state
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum IndexState {
    Indexed,
    Unsupported,
    Skipped { reason: String },
    Queued,
}

#[tauri::command]
pub(crate) fn index_state_for(state: State<AppState>, rel: String) -> CmdResult<IndexState> {
    let result = with_session(&state, |session| {
        if !hiker_core::indexer::is_indexable_path(&rel) {
            return Ok(IndexState::Unsupported);
        }
        if session.indexer.is_pending(&rel) {
            return Ok(IndexState::Queued);
        }
        let read_store = session.read_store.lock()?;
        match read_store.get_note_by_path(&rel).map_err(|e| e.to_string())? {
            Some(row) if row.skipped => Ok(IndexState::Skipped {
                reason: row.skip_reason.unwrap_or_else(|| "skipped".into()),
            }),
            Some(_) => Ok(IndexState::Indexed),
            // No row yet for a supported file — either it's about to be indexed
            // or the watcher hasn't surfaced its create event. Either way, the
            // user's mental model is "queued."
            None => Ok(IndexState::Queued),
        }
    });
    log_cmd_result("index_state_for", result)
}

/// Recursive count of indexable files under a folder. Backs the
/// delete-confirm modal so the UI doesn't have to walk the tree itself
/// via N round-trip `list_dir` calls. Empty vec / 0 for a file path.
/// Filters via `core::indexer::is_indexable_path` so the count matches
/// the indexer's allowlist (md / markdown / txt at v1) — same rule that
/// drives `tauri-cmd-file-index-state`.
#[tauri::command]
pub(crate) fn count_notes_in(state: State<AppState>, rel: String) -> CmdResult<u32> {
    let result = with_session(&state, |session| {
        let files = session.vault.walk_indexable_files(&rel)?;
        Ok(u32::try_from(files.len()).unwrap_or(u32::MAX))
    });
    log_cmd_result("count_notes_in", result)
}

/// status: diff-core-module
/// Thin wrapper over `core::diff::compute`. Pure text-in / diff-out — no
/// session lock, no I/O, no async. The UI passes both strings (current
/// buffer text, snapshot blob via `change_content`, derived file via
/// `read_file`, etc.) and renders the returned `DiffResult`.
#[tauri::command]
pub(crate) fn compute_diff(
    before: String,
    after: String,
    intraline: Option<bool>,
) -> hiker_core::diff::DiffResult {
    hiker_core::diff::compute_with_intraline(&before, &after, intraline.unwrap_or(false))
}

#[tauri::command]
pub(crate) fn index_status(state: State<AppState>) -> CmdResult<IndexStatus> {
    let result = with_session(&state, |session| Ok(session.indexer.status()));
    log_cmd_result("index_status", result)
}
