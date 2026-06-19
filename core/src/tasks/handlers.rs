//! Direct-LLM worker entry point and the optional `NonLlm`
//! side-channel. The worker drains `Direct`-shape tasks via
//! `core::llm::chat`, with an optional pre-LLM handler for variants
//! that don't actually want a model call (e.g. `RaptorTriageMatch`).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::tasks::queue::StopSignal;
use crate::audit::{AgentLog, Entry};
use hiker_llm::{Client, Message};

use super::queue::{duration_ms, validate_against_schema, Queue};
use super::types::Task;

/// Pluggable side-channel for `TaskKind` variants that are not just an
/// LLM prompt. The direct worker checks this before falling back to
/// `Client::chat`, so producers like `cluster_triage_enqueue` get a
/// real classifier run on the consumer side rather than handing their
/// (empty) payload to the LLM.
///
/// status: task-queue-raptor-triage-match
pub trait NonLlm: Send + Sync {
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
/// `handlers` is the optional non-LLM side-channel (see `NonLlm`).
/// Pass `None` from contexts that don't need it (e.g. headless CLI / tests).
///
/// status: task-queue-direct-worker
/// status: task-queue-direct-worker-toggle
/// status: task-queue-structured-output-direct
/// status: task-queue-raptor-triage-match
pub async fn run_direct_worker(
    queue: Queue,
    client: Arc<dyn Client>,
    audit: Option<Arc<AgentLog>>,
    handlers: Option<Arc<dyn NonLlm>>,
    cancel: CancellationToken,
) {
    let worker = DirectWorker {
        queue,
        client,
        audit,
        handlers,
        cancel,
    };
    worker.run().await;
}

/// Owns the per-worker borrows. Splitting the main drain loop into
/// `&self` methods keeps each method under the cognitive-complexity
/// budget while sharing state (queue, client, audit log, non-LLM
/// handlers, cancel token) without a free-helper sprawl.
struct DirectWorker {
    queue: Queue,
    client: Arc<dyn Client>,
    audit: Option<Arc<AgentLog>>,
    handlers: Option<Arc<dyn NonLlm>>,
    cancel: CancellationToken,
}

impl DirectWorker {
    async fn run(self) {
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            let cfg = self.queue.cfg();
            // Live toggle: when `[tasks] direct_worker.enabled = false`,
            // the worker stays running but stops draining. Re-checked at
            // the top of each iteration so the user can flip the toggle
            // in the settings UI without a vault restart. (The
            // session-wide `tasks_cancel` token is the only hard exit.)
            if !cfg.direct_worker.enabled {
                tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
                }
            }
            let lease_secs = cfg.lease.default_secs.max(1);
            let Some((task, stop)) = self.queue.checkout_direct(lease_secs).await else {
                // Nothing eligible right now. Sleep briefly; either a
                // new task lands (event channel) or the grace window
                // passes.
                tokio::select! {
                    _ = self.cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
                }
            };
            self.process(task, stop).await;
        }
    }

    async fn process(&self, task: Task, stop: StopSignal) {
        let id = task.id.clone();
        let started = SystemTime::now();
        let outcome = self.drive_one_task(&task, &stop).await;
        self.record_outcome(&task, &id, started, outcome).await;
    }

    async fn drive_one_task(
        &self,
        task: &Task,
        stop: &StopSignal,
    ) -> Result<serde_json::Value, WorkerError> {
        let client = self.client.as_ref();
        let handlers = self.handlers.as_deref();
        // Non-LLM side-channel (e.g. RaptorTriageMatch). Checked
        // before we touch the LLM so a triage task with an empty
        // prompt doesn't get fed to the model — the producer
        // (cluster_triage_enqueue) carries its inputs on the kind
        // variant, not the prompt.
        if let Some(h) = handlers {
            match h.try_handle(task) {
                Ok(Some(value)) => return Ok(value),
                Ok(None) => {}
                Err(e) => return Err(WorkerError::Failed(e)),
            }
        }
        // Build messages. The producer-supplied prompt is the user
        // message; when an output_schema is set we append a
        // JSON-strict instruction (the v1 fallback for providers
        // without server-side structured output enforcement, per
        // `task-queue-structured-output-direct`).
        let mut user_prompt = task.payload.prompt.clone();
        if let Some(schema) = task.task_output_schema_str() {
            user_prompt.push_str(
                "\n\nRespond strictly as JSON matching this schema; do not wrap in markdown or prose:\n",
            );
            user_prompt.push_str(&schema);
        }
        let attempt = call_with_cancel(client, &user_prompt, stop).await?;
        if let Some(schema) = task.output_schema.as_ref() {
            return match parse_and_validate(&attempt, schema) {
                Ok(value) => Ok(value),
                Err(parse_err) => {
                    // Retry once with the parse error appended as
                    // guidance.
                    let mut retry_prompt = user_prompt.clone();
                    retry_prompt.push_str("\n\nThe previous attempt did not match the schema: ");
                    retry_prompt.push_str(&parse_err);
                    retry_prompt.push_str("\nReturn only valid JSON.");
                    let retry = call_with_cancel(client, &retry_prompt, stop).await?;
                    parse_and_validate(&retry, schema)
                        .map_err(|e| WorkerError::Failed(format!("schema_violation: {e}")))
                }
            };
        }
        // No schema → wrap the text response so the producer always
        // sees a JSON value (most callers will define a schema, but
        // the bare-text path keeps the queue useful for ad-hoc
        // producers).
        Ok(serde_json::Value::String(attempt))
    }

    async fn record_outcome(
        &self,
        task: &Task,
        id: &String,
        started: SystemTime,
        outcome: Result<serde_json::Value, WorkerError>,
    ) {
        let details = serde_json::json!({
            "task_id": id,
            "worker": "direct_llm",
            "priority": task.priority,
            "duration_ms": duration_ms(started, SystemTime::now()),
        });
        match outcome {
            Ok(value) => {
                if let Err(e) = self.queue.submit_result(id, value).await {
                    tracing::warn!(task = %id, error = %e, "tasks: submit_result failed");
                }
                if let Some(log) = self.audit.as_ref() {
                    log.record(&Entry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "ok",
                        error: None,
                        turn_id: None,
                        step_id: None,
                        details,
                    });
                }
            }
            Err(WorkerError::Cancelled) => {
                // Cancellation already flipped the slot to `Cancelled`
                // and resolved the producer handle; nothing more to do.
            }
            Err(WorkerError::Failed(reason)) => {
                if let Err(e) = self.queue.fail(id, reason.clone()).await {
                    tracing::warn!(task = %id, error = %e, "tasks: fail() failed");
                }
                if let Some(log) = self.audit.as_ref() {
                    log.record(&Entry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "error",
                        error: Some(reason),
                        turn_id: None,
                        step_id: None,
                        details,
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

async fn call_with_cancel(
    client: &dyn Client,
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
    // Strip common code-fence wrappers the model sometimes inserts.
    let trimmed = text.trim();
    let stripped = if let Some(rest) = trimmed.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            rest[..end].trim()
        } else {
            rest
        }
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            rest[..end].trim()
        } else {
            rest
        }
    } else {
        trimmed
    };
    let value: serde_json::Value =
        serde_json::from_str(stripped).map_err(|e| format!("not valid JSON: {e}"))?;
    validate_against_schema(schema, &value)?;
    Ok(value)
}

