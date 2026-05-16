//! Direct-LLM worker entry point and the optional `NonLlmHandlers`
//! side-channel. The worker drains `Direct`-shape tasks via
//! `core::llm::chat`, with an optional pre-LLM handler for variants
//! that don't actually want a model call (e.g. `RaptorTriageMatch`).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::agent::StopSignal;
use crate::audit::{AgentLog, AuditEntry};
use crate::llm::{LlmClient, Message};

use super::queue::{duration_ms, validate_against_schema, Queue};
use super::types::Task;

/// Pluggable side-channel for `TaskKind` variants that are not just an
/// LLM prompt. The direct worker checks this before falling back to
/// `LlmClient::chat`, so producers like `cluster_triage_enqueue` get a
/// real classifier run on the consumer side rather than handing their
/// (empty) payload to the LLM.
///
/// status: task-queue-raptor-triage-match
pub trait NonLlmHandlers: Send + Sync {
    /// Process `task` synchronously without consulting the LLM. Return
    /// `Ok(None)` to fall through to the default LLM path, `Ok(Some(v))`
    /// to short-circuit with a successful result, or `Err(s)` to fail
    /// the task with `s`.
    fn try_handle(&self, task: &Task) -> Result<Option<serde_json::Value>, String>;
}

/// Spawn the in-process direct-LLM worker. Drains `Direct`-shape tasks
/// one at a time per instance; spawn `parallelism` instances if the
/// config asks for more. Returns when `cancel` fires.
///
/// `handlers` is the optional non-LLM side-channel (see `NonLlmHandlers`).
/// Pass `None` from contexts that don't need it (e.g. headless CLI / tests).
///
/// status: task-queue-direct-worker
/// status: task-queue-direct-worker-toggle
/// status: task-queue-structured-output-direct
/// status: task-queue-raptor-triage-match
pub async fn run_direct_worker(
    queue: Queue,
    client: Arc<dyn LlmClient>,
    audit: Option<Arc<AgentLog>>,
    handlers: Option<Arc<dyn NonLlmHandlers>>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let cfg = queue.cfg();
        // Live toggle: when `[tasks] direct_worker.enabled = false`, the
        // worker stays running but stops draining. Re-checked at the top
        // of each iteration so the user can flip the toggle in the
        // settings UI without a vault restart. (The session-wide
        // `tasks_cancel` token is the only hard exit.)
        if !cfg.direct_worker.enabled {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
            }
        }
        let lease_secs = cfg.lease.default_secs.max(1);
        let Some((task, stop)) = queue.checkout_direct(lease_secs).await else {
            // Nothing eligible right now. Sleep briefly; either a new
            // task lands (event channel) or the grace window passes.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
            }
        };

        let id = task.id.clone();
        let started = SystemTime::now();
        let outcome = drive_one_task(client.as_ref(), handlers.as_deref(), &task, &stop).await;
        match outcome {
            Ok(value) => {
                if let Err(e) = queue.submit_result(&id, value).await {
                    tracing::warn!(task = %id, error = %e, "tasks: submit_result failed");
                }
                if let Some(log) = audit.as_ref() {
                    log.record(&AuditEntry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "ok",
                        error: None,
                        turn_id: None,
                        step_id: None,
                        details: serde_json::json!({
                            "task_id": id,
                            "worker": "direct_llm",
                            "priority": task.priority,
                            "duration_ms": duration_ms(started, SystemTime::now()),
                        }),
                    });
                }
            }
            Err(WorkerError::Cancelled) => {
                // Cancellation already flipped the slot to `Cancelled` and
                // resolved the producer handle; nothing more to do.
            }
            Err(WorkerError::Failed(reason)) => {
                if let Err(e) = queue.fail(&id, reason.clone()).await {
                    tracing::warn!(task = %id, error = %e, "tasks: fail() failed");
                }
                if let Some(log) = audit.as_ref() {
                    log.record(&AuditEntry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "error",
                        error: Some(reason),
                        turn_id: None,
                        step_id: None,
                        details: serde_json::json!({
                            "task_id": id,
                            "worker": "direct_llm",
                            "priority": task.priority,
                            "duration_ms": duration_ms(started, SystemTime::now()),
                        }),
                    });
                }
            }
        }
    }
}

enum WorkerError {
    Cancelled,
    Failed(String),
}

async fn drive_one_task(
    client: &dyn LlmClient,
    handlers: Option<&dyn NonLlmHandlers>,
    task: &Task,
    stop: &StopSignal,
) -> Result<serde_json::Value, WorkerError> {
    // Non-LLM side-channel (e.g. RaptorTriageMatch). Checked before we
    // touch the LLM so a triage task with an empty prompt doesn't get
    // fed to the model — the producer (cluster_triage_enqueue) carries
    // its inputs on the kind variant, not the prompt.
    if let Some(h) = handlers {
        match h.try_handle(task) {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(e) => return Err(WorkerError::Failed(e)),
        }
    }
    // Build messages. The producer-supplied prompt is the user message;
    // when an output_schema is set we append a JSON-strict instruction
    // (the v1 fallback for providers without server-side structured
    // output enforcement, per `task-queue-structured-output-direct`).
    let mut user_prompt = task.payload.prompt.clone();
    if let Some(schema) = task.task_output_schema_str() {
        user_prompt.push_str(
            "\n\nRespond strictly as JSON matching this schema; do not wrap in markdown or prose:\n",
        );
        user_prompt.push_str(&schema);
    }

    let attempt = call_with_cancel(client, &user_prompt, stop).await?;
    if let Some(schema) = task.output_schema.as_ref() {
        match parse_and_validate(&attempt, schema) {
            Ok(value) => return Ok(value),
            Err(parse_err) => {
                // Retry once with the parse error appended as guidance.
                let mut retry_prompt = user_prompt.clone();
                retry_prompt.push_str("\n\nThe previous attempt did not match the schema: ");
                retry_prompt.push_str(&parse_err);
                retry_prompt.push_str("\nReturn only valid JSON.");
                let retry = call_with_cancel(client, &retry_prompt, stop).await?;
                return parse_and_validate(&retry, schema)
                    .map_err(|e| WorkerError::Failed(format!("schema_violation: {e}")));
            }
        }
    }
    // No schema → wrap the text response so the producer always sees a
    // JSON value (most callers will define a schema, but the bare-text
    // path keeps the queue useful for ad-hoc producers).
    Ok(serde_json::Value::String(attempt))
}

async fn call_with_cancel(
    client: &dyn LlmClient,
    prompt: &str,
    stop: &StopSignal,
) -> Result<String, WorkerError> {
    let messages = vec![Message::user(prompt)];
    tokio::select! {
        _ = stop.token().cancelled() => Err(WorkerError::Cancelled),
        out = client.chat(&messages) => out.map_err(|e| WorkerError::Failed(e.to_string())),
    }
}

fn parse_and_validate(
    text: &str,
    schema: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Strip common code-fence wrappers the model sometimes inserts.
    let stripped = strip_code_fence(text.trim());
    let value: serde_json::Value =
        serde_json::from_str(stripped).map_err(|e| format!("not valid JSON: {e}"))?;
    validate_against_schema(schema, &value)?;
    Ok(value)
}

fn strip_code_fence(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    s
}
