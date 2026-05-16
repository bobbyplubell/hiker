//! Note-mutation producer surface.
//!
//! status: note-mutations-menu
//! status: note-mutations-menu-task-shape
//! status: note-mutation-reformat-as-markdown
//! status: note-mutation-replace-original
//! status: note-mutation-discard-derived
//!
//! The mutations menu submits a `Direct` `High`-priority task carrying the
//! buffer's *live* text (per `chat-active-note-context-injection`'s same
//! rule) plus the source extension. The direct-LLM worker drains it and
//! produces text; on success the awaiter spawned here emits
//! `hiker:note-mutation-applied` carrying the result content + the
//! source-hash captured at submit time so the frontend can replace the
//! open buffer (or hold + toast if the buffer was closed).

use serde::Serialize;
use tauri::{Emitter, State};

use crate::{log_cmd_result, AppState};

/// Frontend payload for a successful mutation result. The frontend
/// dispatches a single CM6 transaction replacing the active buffer's
/// content (when the buffer is still open and its content hash matches
/// `source_hash_at_submit`) or holds the result for a click-to-apply
/// toast (when the buffer has been closed).
#[derive(Debug, Clone, Serialize)]
struct NoteMutationAppliedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation_kind: &'a str,
    content: &'a str,
    source_hash_at_submit: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct NoteMutationFailedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation: &'a str,
    error: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteMutationSubmitOutcome {
    pub task_id: String,
}

/// status: note-mutations-menu-task-shape
/// Submit a note-mutation task. `mutation` selects the prompt feature key
/// and is recorded in the changes-row metadata when the user accepts
/// (`note-mutation-replace-original`). Returns the task id immediately;
/// callers watch `hiker:queue-event` (and the new
/// `hiker:note-mutation-completed` / `-failed` events) for terminal state.
#[tauri::command]
pub(crate) async fn submit_note_mutation(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    let outcome = submit_note_mutation_inner(state, app, rel, mutation, source_extension, content)
        .await;
    log_cmd_result("submit_note_mutation", outcome)
}

async fn submit_note_mutation_inner(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    if mutation != "reformat-as-markdown" {
        return Err(format!("unknown mutation: {mutation}"));
    }

    // Grab the per-vault handles we need before awaiting anywhere — clone
    // out from under the sync mutex. The source hash captured here is the
    // pre-mutation on-disk hash; the frontend uses it at apply-time to
    // decide whether the buffer's content still matches what the LLM saw.
    let (queue, prompts, source_hash) = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let source_hash = session
            .vault
            .read_file_with_hash(&rel)
            .map(|(_, h)| h)
            .map_err(|e| e.to_string())?;
        (
            session.tasks.clone(),
            session.prompts.clone(),
            source_hash,
        )
    };

    let title = std::path::Path::new(&rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel)
        .to_string();

    let prompt = prompts
        .render(
            "note_mutation_reformat_as_markdown",
            [
                ("title", title.as_str()),
                ("content", content.as_str()),
                ("source_extension", source_extension.as_str()),
            ],
        )
        .map_err(|e| e.to_string())?;

    let task = hiker_core::tasks::Task {
        id: String::new(),
        kind: hiker_core::tasks::TaskKind::NoteMutation {
            mutation: mutation.clone(),
            source_path: rel.clone(),
        },
        priority: hiker_core::tasks::Priority::High,
        shape: hiker_core::tasks::TaskShape::Direct,
        payload: hiker_core::tasks::TaskPayload {
            prompt,
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata: serde_json::json!({
            "source_hash_at_submit": source_hash,
        }),
    };

    let handle = queue.submit(task).await;
    let task_id = handle.id.clone();

    // Spawn the awaiter. On Completed → emit the result content as a
    // frontend event so the UI can replace the open buffer in a single
    // CM6 transaction (or hold + toast if the buffer is closed). On
    // Failed → toast via event. On Cancelled → silent (the user
    // already knows; queue events drive the widget).
    let app_for_await = app.clone();
    let rel_for_await = rel.clone();
    let mutation_for_await = mutation.clone();
    let source_hash_for_await = source_hash.clone();
    let task_id_for_await = task_id.clone();
    tokio::spawn(async move {
        let task_id = task_id_for_await;
        let outcome = handle.await_outcome().await;
        match outcome {
            hiker_core::tasks::TaskOutcome::Completed { value, .. } => {
                let body_owned: String;
                let result_body: &str = match &value {
                    serde_json::Value::String(s) => s.as_str(),
                    other => {
                        body_owned = serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| other.to_string());
                        body_owned.as_str()
                    }
                };
                // Empty / whitespace-only completions almost certainly
                // mean the provider returned a malformed or refused
                // response — replacing the buffer with empty bytes is a
                // worse failure than surfacing the problem.
                if result_body.trim().is_empty() {
                    let _ = app_for_await.emit(
                        "hiker:note-mutation-failed",
                        &NoteMutationFailedEvent {
                            task_id: &task_id,
                            source_path: &rel_for_await,
                            mutation: &mutation_for_await,
                            error: "empty response from LLM provider",
                        },
                    );
                    return;
                }
                let _ = app_for_await.emit(
                    "hiker:note-mutation-applied",
                    &NoteMutationAppliedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation_kind: &mutation_for_await,
                        content: result_body,
                        source_hash_at_submit: &source_hash_for_await,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Failed { error, .. } => {
                let _ = app_for_await.emit(
                    "hiker:note-mutation-failed",
                    &NoteMutationFailedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation: &mutation_for_await,
                        error: &error,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Cancelled { .. } => {
                // No preview, no toast — the queue widget already showed
                // the cancellation.
            }
        }
    });

    Ok(NoteMutationSubmitOutcome { task_id })
}
