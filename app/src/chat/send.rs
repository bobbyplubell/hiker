//! Send pipeline: push the user turn into the session, mark it
//! pending, spawn a tokio task that runs the agent turn loop.
//!
//! Calls `core::agent::run_turn` with a `GraniteLlmClient` built from
//! the active `[llm]` config and a no-op `ToolDispatcher` (tool wiring
//! lives in MCP and the agent-changes feed, which haven't been
//! ported). Streams `TextDelta` events into `ChatEvent::Delta` so the
//! UI gets per-chunk updates.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use hiker_core::agent::{
    Event, TurnInput, FinishReason, StopSignal, ToolDispatchError, ToolDispatcher,
    TurnId,
};
use hiker_core::config::Config;
use hiker_llm::{Client, Message, ToolCall, ToolResult};
use hiker_core::sessions;

use crate::chat::session;
use crate::chat::state::{ChatEvent, ChatRegistry, ChatRole, ChatTurn};

/// Public entry point invoked by the renderer when the user presses
/// send. Creates a session lazily if none is active, persists the
/// user turn to disk, and kicks off the reply task. `mcp_handler` is the
/// in-process MCP handler from `McpServerHandle::agent_handler()`; when
/// `Some`, agent tool calls dispatch through the real vault tool surface
/// (search, get_note, write_note, etc., per `agent-tool-routing-via-mcp`).
/// When `None` (MCP disabled), tool calls error back to the model as
/// `UnknownTool`.
impl ChatRegistry {
pub fn send(
    &mut self,
    vault_root: &Path,
    config: Arc<std::sync::RwLock<Config>>,
    mcp_handler: &Option<Arc<hiker_mcp::handler::App>>,
    message: &str,
) {
    let reg = self;
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return;
    }
    // Lazy-create on first send of an app launch (mirrors the
    // chat-session-new-button "lazy" half).
    let session_id = match reg.active.clone() {
        Some(id) => id,
        None => {
            let (model, provider) = config
                .read()
                .map(|c| (c.llm.provider.model.clone(), c.llm.provider.backend.clone()))
                .unwrap_or_else(|_| ("stub-model".into(), "stub".into()));
            let chats_dir = session::chats_dir(&config);
            match session::create_new(reg, vault_root, &chats_dir, &model, &provider) {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!(error = %err, "chat: create_new on lazy-send failed");
                    return;
                }
            }
        }
    };

    let user_text = trimmed.to_string();
    let user_preview = reg.short_preview(&user_text, 60);
    let Some(s) = reg.sessions.get_mut(&session_id) else { return };
    // Snapshot the prior conversation as `Message` history. If the session
    // was resumed from disk, prefer the structured `resumed_history` (which
    // preserves tool-call alternation per `chat-session-markdown-store`) so
    // the agent sees a coherent record of past tool use. Otherwise fall
    // back to the text-only in-memory turns.
    let prior_history: Vec<Message> = if !s.resumed_history.is_empty() {
        s.resumed_history.clone()
    } else {
        s.turns
            .iter()
            .map(|t| match t.role {
                ChatRole::User => Message::user(t.text.clone()),
                ChatRole::Assistant => Message::assistant(t.text.clone()),
                ChatRole::Tool => Message::user(t.text.clone()),
            })
            .collect()
    };
    s.turns.push(ChatTurn {
        role: ChatRole::User,
        text: user_text.clone(),
        tool: None,
    });
    if s.preview == "(new session)" || s.preview == "(empty session)" {
        s.preview = user_preview;
    }
    s.pending = true;
    let file_rel = s.rel_path.clone();
    let tx = reg.tx.clone();
    let vault_root_owned = vault_root.to_path_buf();

    // Register a stop signal for this session so the UI can halt the
    // turn via the Stop button. Cleared when the reply finishes/errors
    // in `pump_events`. A new send replaces any stale entry — there
    // shouldn't be one since `pending` blocks re-sends, but be defensive.
    let stop = StopSignal::new();
    reg.stop_signals.insert(session_id.clone(), stop.clone());

    // Spawn the reply task. Keep the user-message persistence inside
    // the task so it stays off the egui frame thread.
    let mcp_handler_owned = mcp_handler.clone();
    let task = ReplyTask {
        tx,
        session_id,
        user_message: user_text,
        prior_history,
        file_rel,
        vault_root: vault_root_owned,
        config,
        mcp_handler: mcp_handler_owned,
        stop,
    };
    // The whole egui frame runs inside `runtime.enter()`, so the ambient
    // tokio handle is live during render — no explicit runtime handle is
    // threaded through. Both the docked sidebar surface (narrow `SurfaceCtx`, no
    // runtime handle) and the full-tab view share this path.
    tokio::spawn(async move {
        task.run().await;
    });
}
}

/// Owned inputs for one spawned reply task. Bundling the nine fields
/// into a struct keeps the spawned worker a single `&self`-style method
/// (`run`) instead of a wide free function.
struct ReplyTask {
    tx: tokio::sync::mpsc::UnboundedSender<ChatEvent>,
    session_id: String,
    user_message: String,
    prior_history: Vec<Message>,
    file_rel: String,
    vault_root: std::path::PathBuf,
    config: Arc<std::sync::RwLock<Config>>,
    mcp_handler: Option<Arc<hiker_mcp::handler::App>>,
    stop: StopSignal,
}

impl ReplyTask {
/// Run one full agent turn: build the LLM client, drive `run_turn`,
/// translate `Event`s into `ChatEvent`s for the UI, and persist
/// the (user, assistant) pair to the session file on completion.
async fn run(self) {
    let ReplyTask {
        tx,
        session_id,
        user_message,
        prior_history,
        file_rel,
        vault_root,
        config,
        mcp_handler,
        stop,
    } = self;
    // Build the LLM client from the active [llm] config snapshot. If
    // construction fails (bad backend, missing key) surface the error
    // through ChatEvent so the user sees it in-panel and the session is
    // unblocked.
    let llm_cfg = match config.read() {
        Ok(g) => g.llm.clone(),
        Err(err) => {
            let _ = tx.send(ChatEvent::Delta {
                session_id: session_id.clone(),
                text: format!("(chat error: config lock poisoned: {err})"),
            });
            let _ = tx.send(ChatEvent::Finished { session_id });
            return;
        }
    };
    let client = match hiker_core::llm::client_from_config(&llm_cfg) {
        Ok(c) => Arc::new(c) as Arc<dyn Client>,
        Err(err) => {
            let _ = tx.send(ChatEvent::Delta {
                session_id: session_id.clone(),
                text: format!(
                    "(chat error: couldn't build LLM client — check Settings → LLM. {err})"
                ),
            });
            let _ = tx.send(ChatEvent::Finished { session_id });
            return;
        }
    };

    // Tool dispatch: when MCP is up we route through the in-process
    // `App`, sharing one tool registry / audit log with the
    // external rmcp surface (`agent-tool-routing-via-mcp`). When MCP is
    // disabled the no-op dispatcher errors back to the model.
    let dispatcher: Arc<dyn ToolDispatcher> = match mcp_handler {
        Some(h) => Arc::new(hiker_mcp::agent_bridge::McpAgentDispatcher::new(h)),
        None => Arc::new(NoToolsDispatcher),
    };
    // Tool defs advertised to the LLM. Mirrors the agent-bridge filter so
    // disabled tools don't appear in the schema the model sees.
    let (mcp_tools_cfg, expose_tasks) = {
        let cfg = config.read();
        match cfg {
            Ok(c) => (Some(c.mcp.tools.clone()), c.tasks.expose_to_chat_agent),
            Err(_) => (None, false),
        }
    };
    let tool_defs = hiker_mcp::agent_bridge::agent_tool_defs_filtered(
        expose_tasks,
        mcp_tools_cfg.as_ref(),
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<Event>(64);
    let session_for_events = session_id.clone();
    let tx_for_events = tx.clone();
    // Forward AgentEvents into ChatEvents while run_turn is producing.
    // Also collects the structured tool-call/tool-result records so the
    // session file can persist them as `hiker-tool-call` /
    // `hiker-tool-result` fenced blocks (`chat-session-markdown-store`).
    let forward = tokio::spawn(async move {
        let mut assistant_acc = String::new();
        // Maintain call_id → tool_name so ToolResult can be paired back
        // with the corresponding card. Args accumulate per call_id from
        // streaming `ToolCallArgsDelta` events.
        let mut call_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut call_args: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut completed_calls: Vec<ToolCall> = Vec::new();
        let mut completed_results: Vec<ToolResult> = Vec::new();
        while let Some(ev) = event_rx.recv().await {
            match ev {
                Event::TextDelta { text, .. } => {
                    assistant_acc.push_str(&text);
                    if tx_for_events
                        .send(ChatEvent::Delta {
                            session_id: session_for_events.clone(),
                            text,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Event::Error { message, .. } => {
                    // Route turn errors to `ChatEvent::Error` (not `Delta`) so
                    // the error consumer in `chat::state` fires: it clears the
                    // streaming buffer, records the failure as a distinct
                    // assistant turn, and clears the pending flag. Surfacing
                    // errors as plain deltas left that consumer unreachable.
                    let _ = tx_for_events.send(ChatEvent::Error {
                        session_id: session_for_events.clone(),
                        message,
                    });
                }
                Event::ToolCallStart { call_id, tool_name, .. } => {
                    call_names.insert(call_id.clone(), tool_name.clone());
                    let _ = tx_for_events.send(ChatEvent::ToolCall {
                        session_id: session_for_events.clone(),
                        name: tool_name,
                        args: String::new(),
                    });
                }
                Event::ToolCallArgsDelta { call_id, args_delta, .. } => {
                    call_args.entry(call_id).or_default().push_str(&args_delta);
                }
                Event::ToolCallComplete { call_id, args, .. } => {
                    // The agent loop hands back the canonical args string
                    // here — use it in preference to the accumulated
                    // delta buffer.
                    call_args.insert(call_id, args);
                }
                Event::ToolResult { call_id, ok, summary, output, .. } => {
                    let name = call_names.remove(&call_id).unwrap_or_default();
                    let args = call_args.remove(&call_id).unwrap_or_default();
                    let result_text = output.clone().unwrap_or_else(|| summary.clone());
                    completed_calls.push(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: args,
                    });
                    completed_results.push(ToolResult {
                        call_id,
                        name: name.clone(),
                        output: result_text.clone(),
                        ok,
                    });
                    let _ = tx_for_events.send(ChatEvent::ToolResult {
                        session_id: session_for_events.clone(),
                        name,
                        ok,
                        result: result_text,
                    });
                }
                _ => {}
            }
        }
        (assistant_acc, completed_calls, completed_results)
    });

    let turn_input = TurnInput {
        turn_id: TurnId(hiker_core::store::dto::new_id()),
        system_prompt: Some(
            "You are a helpful assistant embedded in the hiker note-taking app. \
             The user is working in a local markdown vault. Answer concisely. \
             If you don't know something, say so."
                .to_string(),
        ),
        history: prior_history,
        user_message: Some(user_message.clone()),
        tools: tool_defs,
    };
    let result = hiker_core::agent::run_turn(
        turn_input,
        client,
        dispatcher,
        &llm_cfg.agent,
        &event_tx,
        stop,
        None,
    )
    .await;
    drop(event_tx);
    let (assistant_text, completed_calls, completed_results) =
        forward.await.unwrap_or_default();

    let _ = tx.send(ChatEvent::Finished {
        session_id: session_id.clone(),
    });

    // Persist (user, assistant, tool_calls, tool_results) to disk on
    // success. Best-effort; a failed write shouldn't tear the in-memory
    // state down. Tool calls and results round-trip per
    // `chat-session-markdown-store`.
    let abs = vault_root.join(&file_rel);
    if abs.exists() {
        let reply = match &result {
            Ok(out) if matches!(out.finish_reason, FinishReason::EndTurn) => {
                assistant_text.clone()
            }
            Ok(out) => format!(
                "{}\n\n(finish: {:?})",
                assistant_text, out.finish_reason
            ),
            Err(err) => format!("(turn errored: {err})"),
        };
        if let Err(err) = sessions::append_turn_structured(
            &abs,
            &user_message,
            &reply,
            &completed_calls,
            &completed_results,
        ) {
            tracing::warn!(error = %err, "chat: append_turn_structured failed");
        }
    }
}
}

/// No-op tool dispatcher. Tools land when MCP + agent-changes are
/// ported; until then any tool the model tries to call returns an error
/// the model can recover from.
struct NoToolsDispatcher;

#[async_trait]
impl ToolDispatcher for NoToolsDispatcher {
    async fn dispatch(
        &self,
        name: &str,
        _arguments_json: &str,
    ) -> Result<String, ToolDispatchError> {
        Err(ToolDispatchError::UnknownTool(name.to_string()))
    }
}

impl ChatRegistry {
    /// First-line, char-capped preview of a message — used to label a
    /// session in the picker from its opening user turn.
    pub(in crate::chat) fn short_preview(&self, s: &str, max: usize) -> String {
        let one_line: String = s.lines().next().unwrap_or("").chars().take(max).collect();
        if one_line.chars().count() < s.chars().count() {
            format!("{one_line}…")
        } else {
            one_line
        }
    }
}
