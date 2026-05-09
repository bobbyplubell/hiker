//! Basic in-hiker agent loop. See `docs/llm.md` §`core::agent`.
//!
//! A message-history + tool-dispatch loop on top of `core::llm`. Default
//! backend for the chat panel; ACP is the optional escape hatch for users
//! who prefer Claude Code / Goose / etc. The loop is "just enough":
//!
//! 1. Append the user's message to history.
//! 2. Call `LlmClient::chat_with_tools` with the system prompt + tool defs.
//! 3. If the response carries tool calls, dispatch each through the
//!    injected `ToolDispatcher` (with a per-call timeout circuit-breaker),
//!    append both the assistant's tool-use message and the synthesized
//!    tool-result message to history, then loop.
//! 4. If the response is terminal (no tool calls), append the assistant
//!    text to history and emit `TurnFinished`.
//! 5. If the iteration cap is reached first, emit `IterationCapHit` and
//!    return — the caller decides whether to invoke the loop again
//!    (Continue resets the budget per `agent-iteration-cap-prompt`).
//!
//! Tool implementations are not in this module; they ride on the
//! `ToolDispatcher` trait so the in-process MCP server (per
//! `agent-tool-routing-via-mcp`) can plug in at the adapter layer without
//! `core::agent` depending on the `mcp-server` crate.
//!
//! The Tauri command surface (`chat_send` / `chat_continue` / `chat_stop`
//! / `chat_cancel`) and the `Arc<Mutex<HashMap<TurnId, TurnState>>>` that
//! addresses turns from outside live in the UI adapter — that's the
//! `agent-chat-command-surface` slug.
//
// status: llm-basic-agent-loop
// status: agent-event-stream-shape
// status: agent-iteration-cap-prompt
// status: agent-tool-call-timeout

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::audit::{AgentLog, AuditEntry};
use crate::config::LlmAgentConfig;
use crate::llm::{AgentChunk, LlmClient, LlmError, Message, ToolCall, ToolDef, ToolResult};

/// Discriminated union of events the chat panel renders. One per LLM call
/// (`StepStarted` / `StepFinished`), one per delta (`TextDelta`), one per
/// tool-call lifecycle slot. Same enum is emitted by `core::agent` and
/// (when it lands) `core::acp` — the panel renders both backends
/// identically.
///
/// `step_id` increments on each tool-loop iteration within a turn; a
/// turn that ends after one LLM call has `step_id = 0` only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted { turn_id: TurnId, user_message_summary: String },
    StepStarted { turn_id: TurnId, step_id: u32 },
    TextDelta { turn_id: TurnId, step_id: u32, text: String },
    ToolCallStart { turn_id: TurnId, step_id: u32, call_id: String, tool_name: String },
    ToolCallArgsDelta { turn_id: TurnId, step_id: u32, call_id: String, args_delta: String },
    ToolCallComplete { turn_id: TurnId, step_id: u32, call_id: String, args: String },
    ToolResult {
        turn_id: TurnId,
        step_id: u32,
        call_id: String,
        ok: bool,
        summary: String,
    },
    StepFinished { turn_id: TurnId, step_id: u32, finish_reason: FinishReason },
    IterationCapHit { turn_id: TurnId, completed_iterations: u32 },
    TurnFinished { turn_id: TurnId, finish_reason: FinishReason },
    Error { turn_id: TurnId, step_id: Option<u32>, message: String },
}

/// Why a step or turn ended. The naming follows OpenAI / Anthropic
/// conventions so the chat panel doesn't have to translate per-provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Model produced a terminal user-facing response.
    EndTurn,
    /// Model asked to call one or more tools (step boundary, not turn).
    ToolUse,
    /// Iteration cap fired before terminal response.
    CapHit,
    /// User invoked `chat_stop`.
    UserHalted,
    /// User invoked `chat_cancel`.
    Cancelled,
    /// Provider returned an error mid-turn.
    Errored,
}

/// Cooperative stop signal that the chat command surface uses to ask
/// `run_turn` to wind down. Carries both the cancellation token (for
/// awaits / `select!` legs) and a discriminator so the loop can produce
/// the right `FinishReason` — `chat_stop` ⇒ `UserHalted`, `chat_cancel`
/// ⇒ `Cancelled`. Cheap to clone (one `Arc` of an atomic + a
/// `CancellationToken`).
#[derive(Clone, Default)]
pub struct StopSignal {
    token: CancellationToken,
    kind: Arc<AtomicU8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopKindRepr {
    None = 0,
    UserHalt = 1,
    Cancel = 2,
}

impl StopSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the underlying token for `tokio::select!` arms or other
    /// callers that want a `WaitForCancellationFuture`.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Stop the turn as a user-initiated halt. Maps to
    /// `FinishReason::UserHalted` once the loop notices.
    pub fn user_halt(&self) {
        // Only escalate when no kind has been recorded — `cancel` is the
        // harsher signal and shouldn't be downgraded by a later `user_halt`.
        let _ = self.kind.compare_exchange(
            StopKindRepr::None as u8,
            StopKindRepr::UserHalt as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.token.cancel();
    }

    /// Stop the turn as a cancellation. Maps to `FinishReason::Cancelled`.
    pub fn cancel(&self) {
        let _ = self.kind.compare_exchange(
            StopKindRepr::None as u8,
            StopKindRepr::Cancel as u8,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        self.token.cancel();
    }

    /// Map the stored kind to the corresponding `FinishReason`. `None`
    /// shouldn't happen in practice (the loop only consults this once it
    /// knows the token has fired) but defaults to `Cancelled` to match
    /// the historical behavior.
    pub fn finish_reason(&self) -> FinishReason {
        match self.kind.load(Ordering::SeqCst) {
            x if x == StopKindRepr::UserHalt as u8 => FinishReason::UserHalted,
            _ => FinishReason::Cancelled,
        }
    }
}

/// Identifier for one user-message → terminal-response cycle. Wraps a
/// `String` so callers can use UUIDs / ULIDs / monotonic counters as
/// they prefer; we don't generate them here. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

impl From<String> for TurnId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for TurnId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Adapter contract for executing one tool call. Implementers are
/// expected to be cheap to clone (`Arc<Inner>` shape) so the loop can hand
/// the dispatcher to per-call timeout tasks without coordinating
/// lifetimes. Errors as `Err` are surfaced to the model as `ok = false`
/// tool-results so it can recover; never aborts the turn.
#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// Dispatch one tool call. `arguments_json` is the model-emitted JSON
    /// string; the dispatcher decodes / validates / executes / encodes the
    /// result back to a string. The agent loop never inspects the body.
    async fn dispatch(
        &self,
        name: &str,
        arguments_json: &str,
    ) -> Result<String, ToolDispatchError>;
}

#[derive(Debug, Error)]
pub enum ToolDispatchError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("tool execution: {0}")]
    Execution(String),
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("event channel closed before turn finished")]
    EventChannelClosed,
}

/// Audit-log binding for one turn. Optional — passing `None` means the
/// loop runs without writing rows, which is the right default for
/// tests. Production callers attach the session's shared `AgentLog`
/// and the prompt feature slug they're driving (`chat_system` for the
/// chat panel, future feature names for fan-out).
#[derive(Clone)]
pub struct AgentAudit {
    pub log: Arc<AgentLog>,
    pub feature: &'static str,
}

/// Inputs for one turn. The caller is responsible for accumulating
/// history across turns; the loop only mutates the local copy passed in
/// for this turn.
pub struct AgentTurnInput {
    pub turn_id: TurnId,
    /// Optional system prompt (the chat panel's preamble describing the
    /// vault tools etc.). Threaded into every LLM call this turn.
    pub system_prompt: Option<String>,
    /// Conversation history *not including* `user_message`. The loop
    /// appends `user_message`, the assistant's tool-use messages, the
    /// tool-result messages, and the assistant's terminal text in order.
    pub history: Vec<Message>,
    /// `Some` for a user-initiated send; `None` when the chat panel is
    /// resuming a paused turn (cap-hit) — the spec is explicit that
    /// Continue must not inject an empty user message, since some
    /// providers reject empty user turns and the model interprets a
    /// literal empty turn as a no-content prompt. With `None`, the loop
    /// re-enters the existing history and just resets the cap budget.
    pub user_message: Option<String>,
    pub tools: Vec<ToolDef>,
}

/// Output of one turn. `history` is the new accumulated conversation
/// (the caller stores it for the next turn). `finish_reason` distinguishes
/// terminal completion vs. cap-hit vs. user-halt vs. error so the chat
/// panel can render the right "you can continue from here" affordance.
#[derive(Debug, Clone)]
pub struct AgentTurnOutput {
    pub history: Vec<Message>,
    pub finish_reason: FinishReason,
    pub iterations: u32,
}

/// Run one agent turn end-to-end. Emits `AgentEvent`s on `events_tx` as it
/// progresses; consumes `cancel` to short-circuit on user cancel.
///
/// The loop body is deliberately small — one LLM call per iteration, fan
/// out tool dispatches in parallel within the iteration, append a single
/// pair of (tool-use, tool-result) messages to history per iteration, and
/// loop. The cap is the only outer-loop terminator besides "model
/// produced a terminal text response."
pub async fn run_turn(
    input: AgentTurnInput,
    client: Arc<dyn LlmClient>,
    dispatcher: Arc<dyn ToolDispatcher>,
    cfg: &LlmAgentConfig,
    events_tx: &mpsc::Sender<AgentEvent>,
    stop: StopSignal,
    audit: Option<AgentAudit>,
) -> Result<AgentTurnOutput, AgentError> {
    let turn_id = input.turn_id.clone();
    let mut history = input.history;

    let user_summary = match input.user_message.as_deref() {
        Some(msg) => summarize(msg, 80),
        // "(continuing)" surfaces in the agent log; the chat panel ignores
        // TurnStarted for resumed turns since the cap row already showed
        // the user the pause.
        None => "(continuing)".to_string(),
    };
    let _ = events_tx
        .send(AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
            user_message_summary: user_summary,
        })
        .await;

    if let Some(msg) = input.user_message.as_deref() {
        history.push(Message::user(msg));
    }

    let tool_timeout = Duration::from_secs(cfg.tool_timeout_secs);
    let cap = cfg.iteration_cap.max(1);

    let mut step_id: u32 = 0;
    loop {
        if stop.is_cancelled() {
            let reason = stop.finish_reason();
            let _ = events_tx
                .send(AgentEvent::TurnFinished {
                    turn_id: turn_id.clone(),
                    finish_reason: reason,
                })
                .await;
            return Ok(AgentTurnOutput {
                history,
                finish_reason: reason,
                iterations: step_id,
            });
        }

        if step_id >= cap {
            // Cap hit *before* this iteration's call. Suspend; caller
            // decides whether to reinvoke run_turn (Continue) or drop.
            let _ = events_tx
                .send(AgentEvent::IterationCapHit {
                    turn_id: turn_id.clone(),
                    completed_iterations: step_id,
                })
                .await;
            return Ok(AgentTurnOutput {
                history,
                finish_reason: FinishReason::CapHit,
                iterations: step_id,
            });
        }

        let _ = events_tx
            .send(AgentEvent::StepStarted {
                turn_id: turn_id.clone(),
                step_id,
            })
            .await;

        // Build messages with the system prompt re-prepended each call —
        // the underlying provider rebuilds per call anyway, and threading
        // the system prompt into every call is the safe shape regardless
        // of the provider's caching behavior.
        let mut msgs: Vec<Message> = Vec::with_capacity(history.len() + 1);
        if let Some(sys) = input.system_prompt.as_deref() {
            msgs.push(Message::system(sys));
        }
        msgs.extend(history.iter().cloned());

        let stream_result = run_step_stream(
            client.as_ref(),
            &msgs,
            &input.tools,
            &turn_id,
            step_id,
            events_tx,
            stop.token(),
        )
        .await;
        let step = match stream_result {
            Ok(s) => {
                record_step_audit(audit.as_ref(), &turn_id, step_id, "ok", None, &s);
                s
            }
            Err(e) => {
                let msg = e.to_string();
                record_step_audit_err(audit.as_ref(), &turn_id, step_id, &msg);
                let _ = events_tx
                    .send(AgentEvent::Error {
                        turn_id: turn_id.clone(),
                        step_id: Some(step_id),
                        message: msg,
                    })
                    .await;
                let _ = events_tx
                    .send(AgentEvent::TurnFinished {
                        turn_id: turn_id.clone(),
                        finish_reason: FinishReason::Errored,
                    })
                    .await;
                return Err(AgentError::Llm(e));
            }
        };

        if step.tool_calls.is_empty() {
            // Terminal text response. Record it and finish.
            history.push(Message::assistant(step.text.clone()));
            let _ = events_tx
                .send(AgentEvent::StepFinished {
                    turn_id: turn_id.clone(),
                    step_id,
                    finish_reason: FinishReason::EndTurn,
                })
                .await;
            let _ = events_tx
                .send(AgentEvent::TurnFinished {
                    turn_id: turn_id.clone(),
                    finish_reason: FinishReason::EndTurn,
                })
                .await;
            return Ok(AgentTurnOutput {
                history,
                finish_reason: FinishReason::EndTurn,
                iterations: step_id + 1,
            });
        }

        // Record the assistant's tool-use intent before dispatching, so a
        // mid-dispatch cancel still leaves a coherent history.
        let mut assistant = Message::assistant(step.text.clone());
        assistant.tool_calls = step.tool_calls.clone();
        history.push(assistant);

        let mut results: Vec<ToolResult> = Vec::with_capacity(step.tool_calls.len());
        for c in &step.tool_calls {
            let result = dispatch_with_timeout(
                dispatcher.as_ref(),
                &c.name,
                &c.arguments,
                tool_timeout,
                stop.token(),
            )
            .await;
            let (ok, output, summary) = match result {
                Ok(s) => (true, s.clone(), summarize(&s, 120)),
                Err(reason) => (false, reason.clone(), reason.clone()),
            };
            let _ = events_tx
                .send(AgentEvent::ToolResult {
                    turn_id: turn_id.clone(),
                    step_id,
                    call_id: c.id.clone(),
                    ok,
                    summary,
                })
                .await;
            results.push(ToolResult {
                call_id: c.id.clone(),
                name: c.name.clone(),
                output,
                ok,
            });
        }

        let mut tool_msg = Message::user("");
        tool_msg.tool_results = results;
        history.push(tool_msg);

        let _ = events_tx
            .send(AgentEvent::StepFinished {
                turn_id: turn_id.clone(),
                step_id,
                finish_reason: FinishReason::ToolUse,
            })
            .await;

        step_id += 1;
    }
}

/// One step's result after consuming the streaming response. `text` is
/// the concatenated text deltas; `tool_calls` is the assembled list of
/// `ToolUseComplete` chunks (deduplicated by `index`, ordered).
struct StepOutcome {
    text: String,
    tool_calls: Vec<ToolCall>,
}

/// Drive one streaming step: call `LlmClient::chat_stream_with_tools`,
/// emit `AgentEvent`s as chunks arrive, and return the assembled
/// `StepOutcome` for the loop to act on. If `chat_stream_with_tools` is
/// not implemented, falls back to `chat_with_tools` for the same
/// semantics — matters for the mock client and any future non-streaming
/// `LlmClient` impl.
async fn run_step_stream(
    client: &dyn LlmClient,
    messages: &[Message],
    tools: &[ToolDef],
    turn_id: &TurnId,
    step_id: u32,
    events_tx: &mpsc::Sender<AgentEvent>,
    cancel: &CancellationToken,
) -> Result<StepOutcome, LlmError> {
    let mut stream = match client.chat_stream_with_tools(messages, tools).await {
        Ok(s) => s,
        Err(LlmError::Unsupported { .. }) => {
            let resp = client.chat_with_tools(messages, tools).await?;
            if let Some(text) = resp.text.as_deref().filter(|t| !t.is_empty()) {
                let _ = events_tx
                    .send(AgentEvent::TextDelta {
                        turn_id: turn_id.clone(),
                        step_id,
                        text: text.to_string(),
                    })
                    .await;
            }
            for c in &resp.tool_calls {
                let _ = events_tx
                    .send(AgentEvent::ToolCallStart {
                        turn_id: turn_id.clone(),
                        step_id,
                        call_id: c.id.clone(),
                        tool_name: c.name.clone(),
                    })
                    .await;
                let _ = events_tx
                    .send(AgentEvent::ToolCallComplete {
                        turn_id: turn_id.clone(),
                        step_id,
                        call_id: c.id.clone(),
                        args: c.arguments.clone(),
                    })
                    .await;
            }
            return Ok(StepOutcome {
                text: resp.text.unwrap_or_default(),
                tool_calls: resp.tool_calls,
            });
        }
        Err(e) => return Err(e),
    };

    // index → (call_id, name) so InputDelta events can name the call.
    let mut active: std::collections::BTreeMap<usize, (String, String)> =
        std::collections::BTreeMap::new();
    let mut completed: std::collections::BTreeMap<usize, ToolCall> =
        std::collections::BTreeMap::new();
    let mut text = String::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            next = stream.next() => {
                let Some(item) = next else { break; };
                match item {
                    Ok(AgentChunk::Text(t)) => {
                        if !t.is_empty() {
                            text.push_str(&t);
                            let _ = events_tx
                                .send(AgentEvent::TextDelta {
                                    turn_id: turn_id.clone(),
                                    step_id,
                                    text: t,
                                })
                                .await;
                        }
                    }
                    Ok(AgentChunk::ToolUseStart { index, call_id, name }) => {
                        active.insert(index, (call_id.clone(), name.clone()));
                        let _ = events_tx
                            .send(AgentEvent::ToolCallStart {
                                turn_id: turn_id.clone(),
                                step_id,
                                call_id,
                                tool_name: name,
                            })
                            .await;
                    }
                    Ok(AgentChunk::ToolUseInputDelta { index, partial_args }) => {
                        if let Some((call_id, _)) = active.get(&index) {
                            let _ = events_tx
                                .send(AgentEvent::ToolCallArgsDelta {
                                    turn_id: turn_id.clone(),
                                    step_id,
                                    call_id: call_id.clone(),
                                    args_delta: partial_args,
                                })
                                .await;
                        }
                    }
                    Ok(AgentChunk::ToolUseComplete { index, call }) => {
                        let _ = events_tx
                            .send(AgentEvent::ToolCallComplete {
                                turn_id: turn_id.clone(),
                                step_id,
                                call_id: call.id.clone(),
                                args: call.arguments.clone(),
                            })
                            .await;
                        completed.insert(index, call);
                    }
                    Ok(AgentChunk::Done { .. }) => break,
                    Err(e) => return Err(e),
                }
            }
        }
    }

    let tool_calls: Vec<ToolCall> = completed.into_values().collect();
    Ok(StepOutcome { text, tool_calls })
}

/// Dispatch one tool call with a per-call timeout. Timeout / cancel
/// surface as `Err(reason)` strings; the loop folds those into a
/// `ToolResult { ok: false, output: reason }` so the model can decide to
/// retry, try a different tool, or give up. Per spec
/// (`agent-tool-call-timeout`), timeouts never bubble as turn-killing
/// errors.
async fn dispatch_with_timeout(
    dispatcher: &dyn ToolDispatcher,
    name: &str,
    args: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<String, String> {
    tokio::select! {
        _ = cancel.cancelled() => Err("turn cancelled".to_string()),
        r = tokio::time::timeout(timeout, dispatcher.dispatch(name, args)) => match r {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(format!("tool error: {e}")),
            Err(_) => Err("tool timed out".to_string()),
        }
    }
}

/// Write one audit row per successful LLM call. Per spec
/// (`docs/llm.md` §"Audit log"), every LLM call appends a row keyed by
/// surface (`core::agent` here) and feature (the prompt slug). Tool
/// call counts and the step's finish reason go in `details` so a
/// debugging trail can correlate panel events with audit entries.
fn record_step_audit(
    audit: Option<&AgentAudit>,
    turn_id: &TurnId,
    step_id: u32,
    status: &str,
    error: Option<String>,
    step: &StepOutcome,
) {
    let Some(a) = audit else { return };
    let finish = if step.tool_calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    let mut details = serde_json::json!({
        "tool_calls": step.tool_calls.len(),
        "finish_reason": finish,
    });
    if a.log.log_full_content() {
        details["text"] = serde_json::Value::String(step.text.clone());
    }
    a.log.record(&AuditEntry {
        surface: "core::agent",
        feature: a.feature,
        status,
        error,
        turn_id: Some(&turn_id.0),
        step_id: Some(step_id),
        details,
    });
}

fn record_step_audit_err(
    audit: Option<&AgentAudit>,
    turn_id: &TurnId,
    step_id: u32,
    err_msg: &str,
) {
    let Some(a) = audit else { return };
    a.log.record(&AuditEntry {
        surface: "core::agent",
        feature: a.feature,
        status: "error",
        error: Some(err_msg.to_string()),
        turn_id: Some(&turn_id.0),
        step_id: Some(step_id),
        details: serde_json::Value::Null,
    });
}

fn summarize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{AgentChunk, ChatStream, Role, ToolCall, ToolChatResponse, ToolStream};
    use std::sync::Mutex;

    fn agent_cfg(cap: u32, tool_timeout_secs: u64) -> LlmAgentConfig {
        LlmAgentConfig {
            iteration_cap: cap,
            tool_timeout_secs,
        }
    }

    /// Scriptable client: returns each pre-set `ToolChatResponse` in
    /// order, one per `chat_with_tools` call. Panics if it runs out.
    struct ScriptedClient {
        responses: Mutex<std::collections::VecDeque<ToolChatResponse>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<ToolChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        async fn chat(&self, _: &[Message]) -> Result<String, LlmError> {
            unimplemented!()
        }
        async fn chat_stream(&self, _: &[Message]) -> Result<ChatStream, LlmError> {
            unimplemented!()
        }
        async fn chat_with_tools(
            &self,
            _messages: &[Message],
            _tools: &[ToolDef],
        ) -> Result<ToolChatResponse, LlmError> {
            let mut q = self.responses.lock().unwrap();
            Ok(q.pop_front().expect("scripted client out of responses"))
        }
    }

    /// Dispatcher that records the calls it sees and returns canned
    /// outputs. `responses` is a vec of (tool_name, output) — first call
    /// to `tool_name` consumes the corresponding output. Errors and
    /// timeouts can be simulated via `behavior`.
    enum DispatcherBehavior {
        Echo,
        Sleep(Duration),
        Fail(String),
    }

    struct RecordingDispatcher {
        behavior: DispatcherBehavior,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl RecordingDispatcher {
        fn new(behavior: DispatcherBehavior) -> Self {
            Self {
                behavior,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ToolDispatcher for RecordingDispatcher {
        async fn dispatch(
            &self,
            name: &str,
            arguments_json: &str,
        ) -> Result<String, ToolDispatchError> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments_json.to_string()));
            match &self.behavior {
                DispatcherBehavior::Echo => Ok(format!("echo:{arguments_json}")),
                DispatcherBehavior::Sleep(d) => {
                    tokio::time::sleep(*d).await;
                    Ok("done".to_string())
                }
                DispatcherBehavior::Fail(reason) => {
                    Err(ToolDispatchError::Execution(reason.clone()))
                }
            }
        }
    }

    fn collect_events(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn turn_with_no_tools_finishes_in_one_step() {
        let client = Arc::new(ScriptedClient::new(vec![ToolChatResponse {
            text: Some("hello there".into()),
            tool_calls: vec![],
        }]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t1"),
                system_prompt: None,
                history: vec![],
                user_message: Some("hi".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::EndTurn);
        assert_eq!(out.iterations, 1);
        // user + assistant
        assert_eq!(out.history.len(), 2);
        assert_eq!(out.history[0].role, Role::User);
        assert_eq!(out.history[1].role, Role::Assistant);
        assert_eq!(out.history[1].content, "hello there");
        assert!(matches!(events.first(), Some(AgentEvent::TurnStarted { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnFinished { finish_reason: FinishReason::EndTurn, .. })));
    }

    #[tokio::test]
    async fn turn_dispatches_tool_then_terminates() {
        let client = Arc::new(ScriptedClient::new(vec![
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "search".into(),
                    arguments: r#"{"q":"hiker"}"#.into(),
                }],
            },
            ToolChatResponse {
                text: Some("found 3 notes".into()),
                tool_calls: vec![],
            },
        ]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t2"),
                system_prompt: Some("you are a vault assistant".into()),
                history: vec![],
                user_message: Some("find hiker".into()),
                tools: vec![ToolDef {
                    name: "search".into(),
                    description: "Search the vault".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }],
            },
            client,
            dispatcher.clone(),
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::EndTurn);
        assert_eq!(out.iterations, 2);
        // user + assistant(tool_use) + user(tool_result) + assistant(text)
        assert_eq!(out.history.len(), 4);
        assert_eq!(out.history[1].tool_calls.len(), 1);
        assert_eq!(out.history[2].tool_results.len(), 1);
        assert_eq!(out.history[2].tool_results[0].name, "search");
        assert!(out.history[2].tool_results[0].ok);

        let calls = dispatcher.calls();
        assert_eq!(calls, vec![("search".into(), r#"{"q":"hiker"}"#.into())]);

        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStart { tool_name, .. } if tool_name == "search")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { ok: true, .. })));
    }

    #[tokio::test]
    async fn iteration_cap_pauses_loop_with_event() {
        // Three tool calls in a row; cap = 2 means the third never fires.
        let client = Arc::new(ScriptedClient::new(vec![
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "noop".into(),
                    arguments: "{}".into(),
                }],
            },
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "noop".into(),
                    arguments: "{}".into(),
                }],
            },
        ]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(2, 30);

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t3"),
                system_prompt: None,
                history: vec![],
                user_message: Some("go".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::CapHit);
        assert_eq!(out.iterations, 2);
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::IterationCapHit { completed_iterations: 2, .. })));
    }

    #[tokio::test]
    async fn tool_timeout_synthesizes_failed_result_and_loop_continues() {
        // First step: model asks for slow tool. Second step: terminal.
        let client = Arc::new(ScriptedClient::new(vec![
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "slow".into(),
                    arguments: "{}".into(),
                }],
            },
            ToolChatResponse {
                text: Some("gave up".into()),
                tool_calls: vec![],
            },
        ]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Sleep(
            Duration::from_secs(30),
        )));
        let (tx, mut rx) = mpsc::channel(64);
        // 0s tool timeout — `tokio::time::timeout(Duration::ZERO, fut)`
        // elapses on the first poll of the inner future without parking,
        // so the test completes in milliseconds despite the dispatcher's
        // 30s sleep behavior.
        let cfg = agent_cfg(10, 0);

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t4"),
                system_prompt: None,
                history: vec![],
                user_message: Some("go".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();
        drop(tx);

        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::EndTurn);
        // history: user + assistant(tool_use) + user(tool_result fail) + assistant(text)
        assert_eq!(out.history.len(), 4);
        assert!(!out.history[2].tool_results[0].ok);
        assert_eq!(out.history[2].tool_results[0].output, "tool timed out");
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { ok: false, summary, .. } if summary == "tool timed out"
        )));
    }

    #[tokio::test]
    async fn tool_failure_surfaces_as_failed_result_not_turn_error() {
        let client = Arc::new(ScriptedClient::new(vec![
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "broken".into(),
                    arguments: "{}".into(),
                }],
            },
            ToolChatResponse {
                text: Some("recovered".into()),
                tool_calls: vec![],
            },
        ]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Fail(
            "boom".into(),
        )));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t5"),
                system_prompt: None,
                history: vec![],
                user_message: Some("go".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::EndTurn);
        assert!(!out.history[2].tool_results[0].ok);
        assert!(out.history[2].tool_results[0].output.contains("boom"));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { ok: false, .. }
        )));
    }

    /// Client that emits a pre-scripted stream of `AgentChunk`s for
    /// `chat_stream_with_tools`. Verifies the loop assembles tool calls
    /// from streamed `ToolUseComplete` chunks and emits intermediate
    /// `TextDelta` / `ToolCallArgsDelta` events.
    struct StreamingClient {
        chunks: Mutex<Option<Vec<AgentChunk>>>,
        terminal: Mutex<Option<ToolChatResponse>>,
    }

    #[async_trait]
    impl LlmClient for StreamingClient {
        async fn chat(&self, _: &[Message]) -> Result<String, LlmError> {
            unimplemented!()
        }
        async fn chat_stream(&self, _: &[Message]) -> Result<ChatStream, LlmError> {
            unimplemented!()
        }
        async fn chat_with_tools(
            &self,
            _: &[Message],
            _: &[ToolDef],
        ) -> Result<ToolChatResponse, LlmError> {
            self.terminal
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| LlmError::Provider("no terminal".into()))
        }
        async fn chat_stream_with_tools(
            &self,
            _: &[Message],
            _: &[ToolDef],
        ) -> Result<ToolStream, LlmError> {
            let chunks = self
                .chunks
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| LlmError::Provider("no chunks".into()))?;
            let mapped: Vec<Result<AgentChunk, LlmError>> =
                chunks.into_iter().map(Ok).collect();
            Ok(Box::pin(futures::stream::iter(mapped)))
        }
    }

    #[tokio::test]
    async fn streaming_step_assembles_tool_calls_and_emits_args_deltas() {
        let chunks = vec![
            AgentChunk::Text("looking…".into()),
            AgentChunk::ToolUseStart {
                index: 0,
                call_id: "c1".into(),
                name: "search".into(),
            },
            AgentChunk::ToolUseInputDelta {
                index: 0,
                partial_args: "{\"q\":".into(),
            },
            AgentChunk::ToolUseInputDelta {
                index: 0,
                partial_args: "\"hiker\"}".into(),
            },
            AgentChunk::ToolUseComplete {
                index: 0,
                call: ToolCall {
                    id: "c1".into(),
                    name: "search".into(),
                    arguments: r#"{"q":"hiker"}"#.into(),
                },
            },
            AgentChunk::Done { stop_reason: "tool_use".into() },
        ];
        let client = Arc::new(StreamingClient {
            chunks: Mutex::new(Some(chunks)),
            terminal: Mutex::new(Some(ToolChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            })),
        });
        // Need a streaming response for the second iteration too — empty
        // = terminal text via the chat_with_tools fallback.
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);

        // Reset client.terminal so the second iteration's
        // chat_stream_with_tools call falls through to chat_with_tools
        // with the canned ToolChatResponse.
        // Provide a second chunks list with just `Done` so the streaming
        // path returns no tool_calls and the loop terminates.
        // Simpler: make the stream's *second* call return an empty stream.
        struct OneShot {
            inner: StreamingClient,
            second_terminal: Mutex<Option<ToolChatResponse>>,
        }

        #[async_trait]
        impl LlmClient for OneShot {
            async fn chat(&self, _: &[Message]) -> Result<String, LlmError> {
                unimplemented!()
            }
            async fn chat_stream(&self, _: &[Message]) -> Result<ChatStream, LlmError> {
                unimplemented!()
            }
            async fn chat_with_tools(
                &self,
                _: &[Message],
                _: &[ToolDef],
            ) -> Result<ToolChatResponse, LlmError> {
                self.second_terminal
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| LlmError::Provider("no terminal".into()))
            }
            async fn chat_stream_with_tools(
                &self,
                msgs: &[Message],
                tools: &[ToolDef],
            ) -> Result<ToolStream, LlmError> {
                // First call: streamed chunks. Second call: pretend the
                // streaming method isn't implemented so the loop falls
                // through to chat_with_tools.
                let first = self.inner.chunks.lock().unwrap().is_some();
                if first {
                    self.inner.chat_stream_with_tools(msgs, tools).await
                } else {
                    Err(LlmError::Unsupported { feature: "chat_stream_with_tools" })
                }
            }
        }

        let one = Arc::new(OneShot {
            inner: StreamingClient {
                chunks: client.chunks.lock().unwrap().take().map(|c| c).into(),
                terminal: Mutex::new(None),
            },
            second_terminal: Mutex::new(Some(ToolChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            })),
        });

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t-stream"),
                system_prompt: None,
                history: vec![],
                user_message: Some("find hiker".into()),
                tools: vec![],
            },
            one,
            dispatcher,
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();
        drop(tx);

        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::EndTurn);
        // Confirm streaming-specific events fired.
        let arg_deltas: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCallArgsDelta { .. }))
            .collect();
        assert_eq!(arg_deltas.len(), 2, "expected 2 args-delta events");
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text, .. } if text == "looking…"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCallComplete { args, .. } if args == r#"{"q":"hiker"}"#
        )));
    }

    #[tokio::test]
    async fn cancel_short_circuits_turn() {
        let client = Arc::new(ScriptedClient::new(vec![ToolChatResponse {
            text: Some("ok".into()),
            tool_calls: vec![],
        }]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);
        let stop = StopSignal::new();
        stop.cancel(); // cancel before the loop even starts

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t6"),
                system_prompt: None,
                history: vec![],
                user_message: Some("go".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            stop,
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::Cancelled);
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnFinished { finish_reason: FinishReason::Cancelled, .. }
        )));
    }

    #[tokio::test]
    async fn continue_resume_does_not_inject_empty_user_message() {
        // First turn caps; second invocation re-enters with `user_message:
        // None` and must not push a literal empty user message.
        let client = Arc::new(ScriptedClient::new(vec![
            ToolChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "noop".into(),
                    arguments: "{}".into(),
                }],
            },
            ToolChatResponse {
                text: Some("done".into()),
                tool_calls: vec![],
            },
        ]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        // Cap=1: first call hits the cap and pauses.
        let cfg = agent_cfg(1, 30);

        let first = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t-cont"),
                system_prompt: None,
                history: vec![],
                user_message: Some("kick off".into()),
                tools: vec![],
            },
            client.clone(),
            dispatcher.clone(),
            &cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(first.finish_reason, FinishReason::CapHit);

        let cont_cfg = agent_cfg(10, 30);
        let cont = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t-cont"),
                system_prompt: None,
                history: first.history.clone(),
                user_message: None,
                tools: vec![],
            },
            client,
            dispatcher,
            &cont_cfg,
            &tx,
            StopSignal::new(),
            None,
        )
        .await
        .unwrap();
        drop(tx);
        let _ = collect_events(&mut rx);

        // Count user-role messages that are content-bearing (not tool
        // results). The continue path must not have appended an empty
        // user turn, so the only user message in history is "kick off".
        let user_msgs: Vec<_> = cont
            .history
            .iter()
            .filter(|m| m.role == Role::User && m.tool_results.is_empty())
            .collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "kick off");
        assert_eq!(cont.finish_reason, FinishReason::EndTurn);
    }

    #[tokio::test]
    async fn user_halt_short_circuits_with_user_halted_finish() {
        let client = Arc::new(ScriptedClient::new(vec![ToolChatResponse {
            text: Some("ok".into()),
            tool_calls: vec![],
        }]));
        let dispatcher = Arc::new(RecordingDispatcher::new(DispatcherBehavior::Echo));
        let (tx, mut rx) = mpsc::channel(64);
        let cfg = agent_cfg(10, 30);
        let stop = StopSignal::new();
        stop.user_halt();

        let out = run_turn(
            AgentTurnInput {
                turn_id: TurnId::from("t-halt"),
                system_prompt: None,
                history: vec![],
                user_message: Some("go".into()),
                tools: vec![],
            },
            client,
            dispatcher,
            &cfg,
            &tx,
            stop,
            None,
        )
        .await
        .unwrap();

        drop(tx);
        let events = collect_events(&mut rx);
        assert_eq!(out.finish_reason, FinishReason::UserHalted);
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnFinished { finish_reason: FinishReason::UserHalted, .. }
        )));
    }
}
