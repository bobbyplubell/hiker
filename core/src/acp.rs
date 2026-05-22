//! ACP (Agent Client Protocol) backend — an alternative to the basic
//! in-hiker agent loop. When `cfg.acp.command` is non-empty, the chat
//! panel launches an external ACP agent and routes through it instead of
//! `core::agent`.
//!
//! The `command` config value is the full command line (e.g.
//! `"auggie --acp"`, `"gemini --acp"`). The first whitespace-delimited
//! token is the binary; the rest are arguments. The agent binary must be
//! installed separately — hiker only invokes it.
//!
//! status: llm-acp-client-optional

use std::path::Path;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, McpServer, McpServerHttp, NewSessionRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, ToolCallStatus,
};
use agent_client_protocol::{ByteStreams, Client};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::agent::{Audit, Event, FinishReason, StopSignal, TurnId};

/// Why an ACP turn errored.
#[derive(Debug, Error)]
pub enum Error {
    #[error("agent process spawn failed: {0}")]
    Spawn(String),
    #[error("ACP protocol error: {0}")]
    Protocol(String),
    #[error("event channel closed")]
    EventChannelClosed,
}

/// Outcome of one ACP turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub finish_reason: FinishReason,
    pub agent_text: String,
}

/// Borrowed bundle of inputs to `run_acp_turn`. Pulled out of the
/// positional argument list to keep the function under the
/// `too_many_arguments` threshold; semantics are unchanged.
///
/// `command_line` is the user-configured full command (e.g.
/// `"auggie --acp"`). The first whitespace-delimited token is the
/// binary; the remaining tokens are arguments.
///
/// `context_blocks` are context blocks from the chat panel (active-note
/// injection, `@`-mentions). The function weaves them into the prompt
/// string — the ACP agent interprets them as part of the user message.
pub struct TurnInput<'a> {
    pub command_line: &'a str,
    pub vault_root: &'a Path,
    pub mcp_port: u16,
    pub user_message: &'a str,
    pub context_blocks: &'a [crate::ChatContextBlock],
    pub session_id: &'a str,
    pub event_tx: &'a mpsc::Sender<Event>,
    pub stop: StopSignal,
    pub audit: Option<Audit>,
}

/// Shared streaming state for one ACP turn: the turn id every emitted
/// `Event` is tagged with, plus the channel they go out on. Methods carry
/// the per-update session handling so `run_acp_turn` stays small.
struct UpdateSink<'a> {
    turn_id: &'a TurnId,
    event_tx: &'a mpsc::Sender<Event>,
}

impl UpdateSink<'_> {
    /// Build the combined prompt: the user message first, then each context
    /// block as a separate `[hiker context] …` paragraph.
    fn build_prompt(&self, user_message: &str, context_blocks: &[crate::ChatContextBlock]) -> String {
        let mut prompt_text = user_message.to_string();
        for block in context_blocks {
            prompt_text.push_str("\n\n[hiker context]");
            if !block.rel_path.is_empty() {
                prompt_text.push_str(&format!(" `{}`", block.rel_path));
            }
            if let Some(ref range) = block.line_range {
                prompt_text.push_str(&format!(" ({range})"));
            }
            prompt_text.push_str(":\n\n");
            prompt_text.push_str(&block.content);
        }
        prompt_text
    }

    /// One-line summary of the user message for `Event::TurnStarted`,
    /// truncated to 80 chars with an ellipsis.
    fn user_message_summary(&self, user_message: &str) -> String {
        let max_chars = 80usize;
        if user_message.chars().count() <= max_chars {
            user_message.to_string()
        } else {
            let mut out: String = user_message
                .chars()
                .take(max_chars.saturating_sub(1))
                .collect();
            out.push('…');
            out
        }
    }

    /// Translate one session-update notification into the corresponding
    /// `Event`(s) and append any agent text to `agent_text`.
    async fn on_notification(&self, notif: &SessionNotification, agent_text: &mut String) {
        match &notif.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(ref tc) = chunk.content {
                    let text = tc.text.clone();
                    agent_text.push_str(&text);
                    let _ = self
                        .event_tx
                        .send(Event::TextDelta {
                            turn_id: self.turn_id.clone(),
                            step_id: 0,
                            text,
                        })
                        .await;
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(ref tc) = chunk.content {
                    let text = format!("(thinking) {}", tc.text);
                    let _ = self
                        .event_tx
                        .send(Event::TextDelta {
                            turn_id: self.turn_id.clone(),
                            step_id: 0,
                            text,
                        })
                        .await;
                }
            }
            SessionUpdate::ToolCall(tc) => {
                let _ = self
                    .event_tx
                    .send(Event::ToolCallStart {
                        turn_id: self.turn_id.clone(),
                        step_id: 0,
                        call_id: tc.tool_call_id.to_string(),
                        tool_name: tc.title.clone(),
                    })
                    .await;
            }
            SessionUpdate::ToolCallUpdate(tcu) => match &tcu.fields.status {
                Some(ToolCallStatus::Completed) => {
                    let _ = self
                        .event_tx
                        .send(Event::ToolResult {
                            turn_id: self.turn_id.clone(),
                            step_id: 0,
                            call_id: tcu.tool_call_id.to_string(),
                            ok: true,
                            summary: "completed".into(),
                            output: None,
                        })
                        .await;
                }
                Some(ToolCallStatus::Failed) => {
                    let _ = self
                        .event_tx
                        .send(Event::ToolResult {
                            turn_id: self.turn_id.clone(),
                            step_id: 0,
                            call_id: tcu.tool_call_id.to_string(),
                            ok: false,
                            summary: "failed".into(),
                            output: None,
                        })
                        .await;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Spawn an external ACP agent, send a prompt, stream session updates
/// as `Event`s, and return the final text + stop reason.
pub async fn run_acp_turn(input: TurnInput<'_>) -> Result<TurnOutcome, Error> {
    let TurnInput {
        command_line,
        vault_root,
        mcp_port,
        user_message,
        context_blocks,
        session_id,
        event_tx,
        stop,
        audit: _audit,
    } = input;
    // Split the command line into binary + args on whitespace.
    let mut parts = command_line.split_ascii_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| Error::Spawn("empty command line".into()))?;
    let args: Vec<&str> = parts.collect();
    let turn_id = TurnId::from(session_id);

    let prompt_text = UpdateSink {
        turn_id: &turn_id,
        event_tx,
    }
    .build_prompt(user_message, context_blocks);

    // Spawn the agent subprocess.
    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| Error::Spawn(format!("spawn {command_line}: {e}")))?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::Spawn("no stdin".into()))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Spawn("no stdout".into()))?;

    let byte_streams = ByteStreams::new(
        child_stdin.compat_write(),
        child_stdout.compat(),
    );

    let vault_root = vault_root.to_path_buf();
    let mcp_url = format!("http://127.0.0.1:{mcp_port}");
    let event_tx = event_tx.clone();

    let outcome: std::sync::Mutex<Option<(FinishReason, String)>> =
        std::sync::Mutex::new(None);

    Client
        .builder()
        // Auto-allow permission requests: pick the first option.
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                if let Some(first) = request.options.first() {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            first.option_id.clone(),
                        )),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(byte_streams, async |cx| {
            // Initialize the connection.
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await
                .map_err(|e| {
                    agent_client_protocol::util::internal_error(format!("init failed: {e}"))
                })?;

            // Build a session request with the hiker MCP server attached
            // so the agent connects to our in-process MCP server.
            let session_req = NewSessionRequest::new(&vault_root)
                .mcp_servers(vec![McpServer::Http(McpServerHttp::new(
                    "hiker",
                    &mcp_url,
                ))]);

            let session_result = cx
                .build_session_from(session_req)
                .block_task()
                .run_until(async |mut session| {
                    let sink = UpdateSink {
                        turn_id: &turn_id,
                        event_tx: &event_tx,
                    };
                    let _ = event_tx
                        .send(Event::TurnStarted {
                            turn_id: turn_id.clone(),
                            user_message_summary: sink.user_message_summary(user_message),
                        })
                        .await;

                    session
                        .send_prompt(&prompt_text)
                        .map_err(|e| {
                            agent_client_protocol::util::internal_error(format!("send_prompt: {e}"))
                        })?;

                    let mut agent_text = String::new();

                    loop {
                        let update = tokio::select! {
                            _ = stop.token().cancelled() => {
                                let reason = stop.finish_reason();
                                let _ = event_tx.send(Event::TurnFinished {
                                    turn_id: turn_id.clone(),
                                    finish_reason: reason,
                                }).await;
                                return Ok((reason, agent_text));
                            }
                            upd = session.read_update() => {
                                upd.map_err(|e| {
                                    agent_client_protocol::util::internal_error(format!("read_update: {e}"))
                                })?
                            }
                        };

                        match update {
                            agent_client_protocol::SessionMessage::SessionMessage(dispatch) => {
                                agent_client_protocol::util::MatchDispatch::new(dispatch)
                                    .if_notification(async |notif: SessionNotification| {
                                        sink.on_notification(&notif, &mut agent_text).await;
                                        Ok(())
                                    })
                                    .await
                                    .otherwise_ignore()
                                    .map_err(|e| {
                                        agent_client_protocol::util::internal_error(format!("dispatch error: {e}"))
                                    })?;
                            }
                            agent_client_protocol::SessionMessage::StopReason(stop_reason) => {
                                use agent_client_protocol::schema::StopReason;
                                let finish = match stop_reason {
                                    StopReason::EndTurn => FinishReason::EndTurn,
                                    StopReason::MaxTokens => FinishReason::CapHit,
                                    StopReason::Cancelled => FinishReason::Cancelled,
                                    _ => FinishReason::EndTurn,
                                };
                                let _ = event_tx
                                    .send(Event::TurnFinished {
                                        turn_id: turn_id.clone(),
                                        finish_reason: finish,
                                    })
                                    .await;
                                return Ok((finish, agent_text));
                            }
                            _ => {
                                // Unknown session message variant — ignore
                                // silently (ACP schema may add new variants).
                            }
                        }
                    }
                })
                .await?;

            *outcome.lock().unwrap() = Some(session_result);
            Ok(())
        })
        .await
        .map_err(|e| Error::Protocol(format!("connect: {e}")))?;

    let (finish_reason, agent_text) =
        outcome.into_inner().unwrap().ok_or_else(|| {
            Error::Protocol("no session result".into())
        })?;

    Ok(TurnOutcome {
        finish_reason,
        agent_text,
    })
}

