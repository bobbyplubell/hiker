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

use crate::agent::{AgentAudit, AgentEvent, FinishReason, StopSignal, TurnId};

/// Why an ACP turn errored.
#[derive(Debug, Error)]
pub enum AcpError {
    #[error("agent process spawn failed: {0}")]
    Spawn(String),
    #[error("ACP protocol error: {0}")]
    Protocol(String),
    #[error("event channel closed")]
    EventChannelClosed,
}

/// Outcome of one ACP turn.
#[derive(Debug, Clone)]
pub struct AcpTurnOutcome {
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
pub struct AcpTurnInput<'a> {
    pub command_line: &'a str,
    pub vault_root: &'a Path,
    pub mcp_port: u16,
    pub user_message: &'a str,
    pub context_blocks: &'a [crate::ChatContextBlock],
    pub session_id: &'a str,
    pub event_tx: &'a mpsc::Sender<AgentEvent>,
    pub stop: StopSignal,
    pub audit: Option<AgentAudit>,
}

/// Spawn an external ACP agent, send a prompt, stream session updates
/// as `AgentEvent`s, and return the final text + stop reason.
pub async fn run_acp_turn(input: AcpTurnInput<'_>) -> Result<AcpTurnOutcome, AcpError> {
    let AcpTurnInput {
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
        .ok_or_else(|| AcpError::Spawn("empty command line".into()))?;
    let args: Vec<&str> = parts.collect();
    let turn_id = TurnId::from(session_id);

    // Build the combined prompt: user message first, then each context
    // block as a separate "[hiker context] …" paragraph.
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

    // Spawn the agent subprocess.
    let mut child = tokio::process::Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| AcpError::Spawn(format!("spawn {command_line}: {e}")))?;

    let child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| AcpError::Spawn("no stdin".into()))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| AcpError::Spawn("no stdout".into()))?;

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
                    let _ = event_tx
                        .send(AgentEvent::TurnStarted {
                            turn_id: turn_id.clone(),
                            user_message_summary: summarize(user_message, 80),
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
                                let _ = event_tx.send(AgentEvent::TurnFinished {
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
                                        translate_update(
                                            &notif.update,
                                            &turn_id,
                                            &event_tx,
                                            &mut agent_text,
                                        )
                                        .await;
                                        Ok(())
                                    })
                                    .await
                                    .otherwise_ignore()
                                    .map_err(|e| {
                                        agent_client_protocol::util::internal_error(format!("dispatch error: {e}"))
                                    })?;
                            }
                            agent_client_protocol::SessionMessage::StopReason(stop_reason) => {
                                let finish = acp_stop_to_finish(&stop_reason);
                                let _ = event_tx
                                    .send(AgentEvent::TurnFinished {
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
        .map_err(|e| AcpError::Protocol(format!("connect: {e}")))?;

    let (finish_reason, agent_text) =
        outcome.into_inner().unwrap().ok_or_else(|| {
            AcpError::Protocol("no session result".into())
        })?;

    Ok(AcpTurnOutcome {
        finish_reason,
        agent_text,
    })
}

/// Translate one `SessionUpdate` from ACP into one or more
/// `AgentEvent`s and accumulate text into `agent_text`.
async fn translate_update(
    update: &SessionUpdate,
    turn_id: &TurnId,
    event_tx: &mpsc::Sender<AgentEvent>,
    agent_text: &mut String,
) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(ref tc) = chunk.content {
                let text = tc.text.clone();
                agent_text.push_str(&text);
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        turn_id: turn_id.clone(),
                        step_id: 0,
                        text,
                    })
                    .await;
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(ref tc) = chunk.content {
                let text = format!("(thinking) {}", tc.text);
                let _ = event_tx
                    .send(AgentEvent::TextDelta {
                        turn_id: turn_id.clone(),
                        step_id: 0,
                        text,
                    })
                    .await;
            }
        }
        SessionUpdate::ToolCall(tc) => {
            let _ = event_tx
                .send(AgentEvent::ToolCallStart {
                    turn_id: turn_id.clone(),
                    step_id: 0,
                    call_id: tc.tool_call_id.to_string(),
                    tool_name: tc.title.clone(),
                })
                .await;
        }
        SessionUpdate::ToolCallUpdate(tcu) => {
            match &tcu.fields.status {
                Some(ToolCallStatus::Completed) => {
                    let _ = event_tx
                        .send(AgentEvent::ToolResult {
                            turn_id: turn_id.clone(),
                            step_id: 0,
                            call_id: tcu.tool_call_id.to_string(),
                            ok: true,
                            summary: "completed".into(),
                            output: None,
                        })
                        .await;
                }
                Some(ToolCallStatus::Failed) => {
                    let _ = event_tx
                        .send(AgentEvent::ToolResult {
                            turn_id: turn_id.clone(),
                            step_id: 0,
                            call_id: tcu.tool_call_id.to_string(),
                            ok: false,
                            summary: "failed".into(),
                            output: None,
                        })
                        .await;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn acp_stop_to_finish(stop_reason: &agent_client_protocol::schema::StopReason) -> FinishReason {
    use agent_client_protocol::schema::StopReason;
    match stop_reason {
        StopReason::EndTurn => FinishReason::EndTurn,
        StopReason::MaxTokens => FinishReason::CapHit,
        StopReason::Cancelled => FinishReason::Cancelled,
        _ => FinishReason::EndTurn,
    }
}

fn summarize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}
