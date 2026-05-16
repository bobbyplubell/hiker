//! Tauri command surface + per-turn task ownership for the basic agent
//! loop. See `docs/llm.md` §"Event streams and Tauri command surface".
//!
//! Layering: the agent loop itself lives in `core::agent`; this module is
//! the adapter that runs it from Tauri commands. We're explicitly the
//! "5–15 line wrapper" shape per `docs/design.md` IPC rules — except the
//! per-session live state (the spec's `Arc<Mutex<HashMap<SessionId,
//! SessionState>>>`) lives here because it's tokio-task-shaped and
//! tied to one consumer (the chat panel). The CLI doesn't need it.
//
// status: agent-chat-command-surface
// status: agent-event-stream-shape
// status: chat-session-persisted-history
// status: chat-session-markdown-store
// status: chat-session-resume-latest
// status: chat-session-new-button
// status: chat-active-note-context-injection

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hiker_core::agent::{
    run_turn, AgentAudit, AgentEvent, AgentTurnInput, FinishReason, StopSignal, ToolDispatcher,
    TurnId,
};
use hiker_core::audit::AgentLog;
use hiker_core::config::Config;
use hiker_core::indexer::{IndexJob, IndexJobTx};
use hiker_core::llm::{GraniteLlmClient, LlmClient, Message, ToolDef};
use hiker_core::prompts::Prompts;
use hiker_core::sessions::{self, SessionId, SessionMeta};
use hiker_core::ChatContextBlock;
use hiker_mcp::McpAgentDispatcher;
use serde::Serialize;
use tauri::{AppHandle, State};
use tokio::sync::mpsc;

use crate::{with_session, AppState, CmdError, CmdResult};

/// Per-session live state. Persists across user turns (per
/// `chat-session-persisted-history`); only ends on explicit
/// `chat_session_new` or process exit.
pub struct SessionState {
    /// Accumulated message history. `chat_send` appends the user message
    /// + active-note context block (if any), then runs the loop. The
    /// resulting history sits here so the next turn picks up where we
    /// left off.
    history: Vec<Message>,
    system_prompt: String,
    tools: Vec<ToolDef>,
    /// Last turn's stop signal — replaced on each `chat_send` /
    /// `chat_continue`. The cancel/stop commands fire it; the live
    /// run_turn future observes it via `tokio::select!`.
    stop: StopSignal,
    /// True between the moment a turn task is spawned and its
    /// terminal-arm cleanup. Re-entry is refused so two LLM calls can't
    /// fly in parallel against one session.
    in_flight: bool,
    /// Path to the on-disk markdown file. Created at session-open;
    /// appended to on every `TurnFinished`. Markdown is the source of
    /// truth per `chat-session-markdown-store`.
    file_path: PathBuf,
    /// Vault-relative form of `file_path`, used to enqueue an
    /// `IndexJob::Upsert` after each append so search picks up new
    /// turns without waiting for the watcher (the file is under
    /// `.hiker/sessions/`, which the watcher carves out specifically).
    rel_path: String,
    /// Path of the active note injected on the last turn. Consecutive
    /// turns that view the same note collapse to a path-only "still
    /// viewing" reference rather than re-sending the body — keeps a
    /// long single-note conversation from burning context window.
    last_active_note: Option<String>,
}

#[derive(Default)]
pub struct ChatRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    sessions: HashMap<SessionId, Arc<Mutex<SessionState>>>,
    /// Single active session. The "implicit single-active-session"
    /// model from `docs/llm.md` §Sessions ("Multi-session UI" is
    /// deferred); commands without an explicit session id resolve to
    /// this one.
    active: Option<SessionId>,
}

impl ChatRegistry {
    fn entry(&self, id: &SessionId) -> Option<Arc<Mutex<SessionState>>> {
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.sessions.get(id).cloned())
    }

    fn active(&self) -> Option<SessionId> {
        self.inner.lock().ok().and_then(|i| i.active.clone())
    }

    fn set_active(&self, id: Option<SessionId>) {
        if let Ok(mut i) = self.inner.lock() {
            i.active = id;
        }
    }

    fn insert(&self, id: SessionId, state: Arc<Mutex<SessionState>>) {
        if let Ok(mut i) = self.inner.lock() {
            i.sessions.insert(id, state);
        }
    }

    /// Drop a session from the cache and clear the active slot if it
    /// matched. Used by `chat_session_delete` after the file is moved
    /// to the vault trash.
    fn forget(&self, id: &SessionId) {
        if let Ok(mut i) = self.inner.lock() {
            i.sessions.remove(id);
            if i.active.as_ref() == Some(id) {
                i.active = None;
            }
        }
    }
}

/// RAII cleanup guard. Whatever happens to the spawned task — terminal
/// finish, error return, or panic — clears `in_flight` on the entry.
/// Without this, a panic before the post-`run_turn` match arm would
/// leak the session into a permanently-busy state.
struct TurnGuard {
    entry: Arc<Mutex<SessionState>>,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.entry.lock() {
            guard.in_flight = false;
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumedTurnDto {
    pub user: String,
    pub agent: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSessionDto {
    pub session_id: String,
    pub rel_path: String,
    pub turns: Vec<ResumedTurnDto>,
}

/// Tauri command: start a turn within a session. If `session_id` is
/// `None`, the active session is used; if no session is active yet, a
/// new one is created lazily (matches the spec's "first send of an app
/// launch lazily creates a session if none active" rule).
#[tauri::command]
pub async fn chat_send(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: Option<String>,
    turn_id: String,
    message: String,
    context_blocks: Option<Vec<ChatContextBlock>>,
) -> CmdResult<String> {
    let turn_id = TurnId::from(turn_id);
    let prep = prepare_for_turn(&state)?;
    let sid = resolve_or_create_session(&state, &prep, session_id)?;

    // status: llm-acp-client-optional
    if !prep.acp_command.is_empty() {
        spawn_acp_turn(
            app,
            prep,
            sid.clone(),
            turn_id,
            message,
            context_blocks.unwrap_or_default(),
        )?;
    } else {
        spawn_turn_task(
            app,
            prep,
            sid.clone(),
            turn_id,
            Some(message),
            context_blocks.unwrap_or_default(),
        )?;
    }
    Ok(sid.0)
}

/// Tauri command: resume a paused turn (cap-hit). Per spec
/// `agent-iteration-cap-prompt`, Continue re-enters with the same
/// history and a fresh cap budget — it must NOT inject a new user
/// message.
#[tauri::command]
pub async fn chat_continue(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    turn_id: String,
) -> CmdResult<()> {
    let turn_id = TurnId::from(turn_id);
    let sid = SessionId(session_id);
    let prep = prepare_for_turn(&state)?;

    // status: llm-acp-client-optional
    if !prep.acp_command.is_empty() {
        spawn_acp_turn(app, prep, sid, turn_id, String::new(), Vec::new())
            .map(|_| ())
            .map_err(CmdError::from)
    } else {
        spawn_turn_task(app, prep, sid, turn_id, None, Vec::new())
            .map(|_| ())
            .map_err(CmdError::from)
    }
}

/// Tauri command: user-halt. Fires the `user_halt` arm of the stop
/// signal so `run_turn` produces `FinishReason::UserHalted`.
#[tauri::command]
pub async fn chat_stop(
    state: State<'_, AppState>,
    session_id: String,
    _turn_id: String,
) -> CmdResult<()> {
    let registry = registry_from_state(&state)?;
    if let Some(entry) = registry.entry(&SessionId(session_id))
        && let Ok(guard) = entry.lock()
    {
        guard.stop.user_halt();
    }
    Ok(())
}

/// Tauri command: cancel mid-stream. Fires the `cancel` arm so the loop
/// produces `FinishReason::Cancelled`.
#[tauri::command]
pub async fn chat_cancel(
    state: State<'_, AppState>,
    session_id: String,
    _turn_id: String,
) -> CmdResult<()> {
    let registry = registry_from_state(&state)?;
    if let Some(entry) = registry.entry(&SessionId(session_id))
        && let Ok(guard) = entry.lock()
    {
        guard.stop.cancel();
    }
    Ok(())
}

/// One row in the session-picker dropdown. The frontend uses
/// `first_user_preview` (first user message of the session, truncated)
/// as the row label so users can recognize past investigations at a
/// glance — `created_at_unix` covers the date, and `session_id`
/// uniquely identifies the file. Sessions arrive newest-first.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListItem {
    pub session_id: String,
    pub rel_path: String,
    pub mtime_unix: i64,
    pub first_user_preview: String,
    pub turn_count: u32,
    pub is_active: bool,
}

#[tauri::command]
pub fn chat_session_list(state: State<'_, AppState>) -> CmdResult<Vec<SessionListItem>> {
    let (vault_root, registry) =
        with_session(&state, |s| Ok((s.root.clone(), s.chat.clone())))?;
    let active = registry.active();
    let infos = sessions::list_sessions(&vault_root)?;
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let turns = sessions::parse_session(&info.abs_path).unwrap_or_default();
        let preview = turns
            .first()
            .map(|t| short_preview(&t.user, 60))
            .unwrap_or_else(|| "(empty session)".to_string());
        out.push(SessionListItem {
            is_active: active.as_ref() == Some(&info.id),
            session_id: info.id.0,
            rel_path: info.rel_path,
            mtime_unix: info.mtime_unix,
            first_user_preview: preview,
            turn_count: turns.len() as u32,
        });
    }
    Ok(out)
}

/// Tauri command: switch the active session to an existing on-disk one
/// and return the resumed transcript. Backs the session-picker
/// dropdown's "open this session" path.
///
/// status: llm-acp-client-optional
/// ACP limitation: when an external agent is active, the session file
/// is loaded only for display. Agent history is NOT seeded into the ACP
/// session — the external agent always starts fresh each turn.
/// ACP's `session/load` is not yet supported.
#[tauri::command]
pub fn chat_session_open(
    state: State<'_, AppState>,
    session_id: String,
) -> CmdResult<Option<ActiveSessionDto>> {
    let (vault_root, registry) =
        with_session(&state, |s| Ok((s.root.clone(), s.chat.clone())))?;
    let id = SessionId(session_id);
    // If the entry isn't yet in the registry, hydrate it from disk.
    if registry.entry(&id).is_none() {
        let infos = sessions::list_sessions(&vault_root)?;
        let Some(info) = infos.into_iter().find(|i| i.id == id) else {
            return Ok(None);
        };
        let turns = sessions::parse_session(&info.abs_path).unwrap_or_default();
        let mut history: Vec<Message> = Vec::with_capacity(turns.len() * 2);
        for t in &turns {
            history.push(Message::user(t.user.as_str()));
            history.push(Message::assistant(t.agent.as_str()));
        }
        let state_obj = SessionState {
            history,
            system_prompt: String::new(),
            tools: Vec::new(),
            stop: StopSignal::new(),
            in_flight: false,
            file_path: info.abs_path,
            rel_path: info.rel_path,
            last_active_note: None,
        };
        registry.insert(id.clone(), Arc::new(Mutex::new(state_obj)));
    }
    registry.set_active(Some(id.clone()));
    let entry = registry
        .entry(&id)
        .ok_or_else(|| CmdError::from("session vanished"))?;
    let (path, rel_path) = {
        let guard = entry.lock()?;
        (guard.file_path.clone(), guard.rel_path.clone())
    };
    let turns = sessions::parse_session(&path)
        .unwrap_or_default()
        .into_iter()
        .map(|t| ResumedTurnDto {
            user: t.user,
            agent: t.agent,
        })
        .collect();
    Ok(Some(ActiveSessionDto {
        session_id: id.0,
        rel_path,
        turns,
    }))
}

/// Tauri command: soft-delete a session through the regular vault
/// trash. The session file lives under `.hiker/sessions/` (carved out
/// from `is_ignored` so the indexer routes it like any note); deleting
/// it through `core::ops::delete` moves it to `.hiker/trash/`,
/// preserves the manifest entry for restore, and removes the index +
/// in-memory registry entries. Restore + permanent-delete ride the
/// existing `restore_trash_entry` / `permanent_delete_trash_entry`
/// commands — the trash bin doesn't need a session-specific surface.
///
/// status: chat-session-trash
#[tauri::command]
pub async fn chat_session_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) -> CmdResult<()> {
    let id = SessionId(session_id);
    let (vault_root, registry, watcher, vault, jobs, changes) = with_session(&state, |s| {
        Ok((
            s.root.clone(),
            s.chat.clone(),
            s.watcher.clone(),
            s.vault.clone(),
            s.indexer.job_sender(),
            s.changes.clone(),
        ))
    })?;
    // Resolve the session's vault-relative path. Prefer the registry's
    // cached entry (covers active + already-loaded sessions); fall back
    // to a disk walk for sessions not yet hydrated this session.
    let rel = if let Some(entry) = registry.entry(&id) {
        let g = entry.lock()?;
        g.rel_path.clone()
    } else {
        let infos = sessions::list_sessions(&vault_root)?;
        infos
            .into_iter()
            .find(|i| i.id == id)
            .map(|i| i.rel_path)
            .ok_or_else(|| CmdError::from("session not found"))?
    };

    hiker_core::ops::delete(&watcher, &jobs, &vault, Some(&changes), &rel)
        .await?;

    // Drop the in-memory entry. If the deleted session was the active
    // one, the next vault open / chat_session_active call will pick a
    // fresh resume target; meanwhile the dropdown is the user's way to
    // pick a different existing session.
    registry.forget(&id);

    crate::events::emit_trash_changed(&app);
    Ok(())
}

fn short_preview(s: &str, max: usize) -> String {
    let one_line: String = s
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(max)
        .collect();
    if one_line.chars().count() < s.chars().count() {
        format!("{one_line}…")
    } else {
        one_line
    }
}

/// Tauri command: end the active session and start a fresh one. The
/// "explicit user action" half of `chat-session-new-button`; the lazy
/// path inside `chat_send` covers the "first send of an app launch"
/// case.
#[tauri::command]
pub async fn chat_session_new(state: State<'_, AppState>) -> CmdResult<String> {
    let prep = prepare_for_turn(&state)?;
    let sid = create_session(&state, &prep)?;
    Ok(sid.0)
}

/// Tauri command: what's the currently-active session, if any, plus
/// the resumed transcript so the frontend can paint the panel on vault
/// open. Drives `chat-session-resume-latest`.
#[tauri::command]
pub fn chat_session_active(state: State<'_, AppState>) -> CmdResult<Option<ActiveSessionDto>> {
    let registry = registry_from_state(&state)?;
    let Some(sid) = registry.active() else { return Ok(None) };
    let Some(entry) = registry.entry(&sid) else { return Ok(None) };
    let path = {
        let guard = entry.lock()?;
        guard.file_path.clone()
    };
    let turns = sessions::parse_session(&path)?
        .into_iter()
        .map(|t| ResumedTurnDto {
            user: t.user,
            agent: t.agent,
        })
        .collect::<Vec<_>>();
    let rel_path = {
        let guard = entry.lock()?;
        guard.rel_path.clone()
    };
    Ok(Some(ActiveSessionDto {
        session_id: sid.0,
        rel_path,
        turns,
    }))
}

/// Called by `open_vault_at_inner` after the session is constructed.
/// Looks for the most-recent on-disk session and adopts it as the
/// active one, hydrating the in-memory cache with a synthetic history
/// derived from the markdown. If no sessions exist yet, leaves the
/// registry empty — the next `chat_send` will create one.
///
/// status: chat-session-resume-latest
pub fn resume_latest_at_open(
    registry: &Arc<ChatRegistry>,
    vault_root: &std::path::Path,
    cfg: &Config,
) -> std::io::Result<()> {
    let Some(latest) = sessions::most_recent_session(vault_root)? else {
        return Ok(());
    };
    let turns = sessions::parse_session(&latest.abs_path).unwrap_or_default();
    let mut history: Vec<Message> = Vec::with_capacity(turns.len() * 2);
    for t in &turns {
        history.push(Message::user(t.user.as_str()));
        history.push(Message::assistant(t.agent.as_str()));
    }
    let state = SessionState {
        history,
        system_prompt: String::new(), // refreshed on each chat_send
        tools: Vec::new(),
        stop: StopSignal::new(),
        in_flight: false,
        file_path: latest.abs_path.clone(),
        rel_path: latest.rel_path.clone(),
        last_active_note: None,
    };
    let sid = latest.id;
    registry.insert(sid.clone(), Arc::new(Mutex::new(state)));
    registry.set_active(Some(sid));
    let _ = cfg; // reserved — model/provider stamp lives in the file's frontmatter
    Ok(())
}

struct TurnPreparation {
    client: Arc<dyn LlmClient>,
    dispatcher: Arc<dyn ToolDispatcher>,
    agent_cfg: hiker_core::config::LlmAgentConfig,
    system_prompt: String,
    tools: Vec<ToolDef>,
    registry: Arc<ChatRegistry>,
    audit: Arc<AgentLog>,
    vault_root: PathBuf,
    model: String,
    provider: String,
    jobs: IndexJobTx,
    // status: llm-acp-client-optional
    acp_command: String,
    mcp_port: Option<u16>,
}

/// Resolve the session id (passed in or implicit-active) or lazily
/// create a fresh one if no session has been started this app launch.
fn resolve_or_create_session(
    state: &State<'_, AppState>,
    prep: &TurnPreparation,
    explicit: Option<String>,
) -> Result<SessionId, String> {
    if let Some(s) = explicit {
        return Ok(SessionId(s));
    }
    if let Some(sid) = prep.registry.active() {
        return Ok(sid);
    }
    create_session(state, prep)
}

/// Create a new on-disk session and register it as the active one.
fn create_session(
    _state: &State<'_, AppState>,
    prep: &TurnPreparation,
) -> Result<SessionId, String> {
    let id = SessionId::generate();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let meta = SessionMeta {
        id: id.clone(),
        created_at_unix: now,
        model: prep.model.clone(),
        provider: prep.provider.clone(),
    };
    let path = sessions::create_session_file(&prep.vault_root, &meta).map_err(|e| e.to_string())?;
    let rel = sessions::session_rel_path(&id, now);
    let state = SessionState {
        history: Vec::new(),
        system_prompt: prep.system_prompt.clone(),
        tools: prep.tools.clone(),
        stop: StopSignal::new(),
        in_flight: false,
        file_path: path,
        rel_path: rel,
        last_active_note: None,
    };
    prep.registry.insert(id.clone(), Arc::new(Mutex::new(state)));
    prep.registry.set_active(Some(id.clone()));
    Ok(id)
}

/// Shared task spawn for `chat_send` and `chat_continue`. `message ==
/// None` means "Continue" semantics: don't push a new user message,
/// just re-run the loop over existing history.
fn spawn_turn_task(
    app: AppHandle,
    prep: TurnPreparation,
    session_id: SessionId,
    turn_id: TurnId,
    message: Option<String>,
    context_blocks: Vec<ChatContextBlock>,
) -> Result<(), String> {
    let TurnPreparation {
        client,
        dispatcher,
        agent_cfg,
        system_prompt,
        tools,
        registry,
        audit,
        jobs,
        ..
    } = prep;

    let entry = registry
        .entry(&session_id)
        .ok_or_else(|| "session not found".to_string())?;

    let stop = StopSignal::new();
    let (history_snapshot, file_path, rel_path) = {
        let mut guard = entry.lock().map_err(|e| e.to_string())?;
        if guard.in_flight {
            return Err("turn already in flight".to_string());
        }
        guard.in_flight = true;
        guard.stop = stop.clone();
        guard.system_prompt = system_prompt.clone();
        guard.tools = tools.clone();

        // status: chat-active-note-context-injection
        // status: chat-input-at-mentions
        // Inject the composed context blocks as turn-scoped synthetic
        // user-context messages *only* on user-driven sends (not on
        // Continue, which resumes a paused turn already in flight). Per
        // `chat-active-note-context-injection`, consecutive turns viewing
        // the same active note collapse to a path-only "still viewing"
        // reference; the explicit `@`-mentions never collapse — they
        // always send the resolved body.
        if message.is_some() {
            let mut active_note_path: Option<String> = None;
            for block in &context_blocks {
                if block.rel_path.is_empty() {
                    continue;
                }
                let body = match block.kind.as_str() {
                    "activeNote" => {
                        let same = guard.last_active_note.as_deref()
                            == Some(block.rel_path.as_str());
                        active_note_path = Some(block.rel_path.clone());
                        if same {
                            format!(
                                "[hiker context] user is still viewing `{}`",
                                block.rel_path
                            )
                        } else {
                            format!(
                                "[hiker context] user is currently viewing `{}` — its current buffer contents:\n\n{}",
                                block.rel_path, block.content
                            )
                        }
                    }
                    "selection" => {
                        let where_at = block
                            .line_range
                            .as_deref()
                            .map(|r| format!("`{}` ({})", block.rel_path, r))
                            .unwrap_or_else(|| format!("`{}`", block.rel_path));
                        format!(
                            "[hiker context] user attached the selection from {where_at}:\n\n{}",
                            block.content
                        )
                    }
                    // "note" or unknown kind → treat as a full-note attach.
                    _ => format!(
                        "[hiker context] user attached `{}`:\n\n{}",
                        block.rel_path, block.content
                    ),
                };
                guard.history.push(Message::user(body));
            }
            guard.last_active_note = active_note_path;
        }

        (guard.history.clone(), guard.file_path.clone(), guard.rel_path.clone())
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let app_for_events = app.clone();
    let event_pump = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            crate::events::emit_chat_event(&app_for_events, &ev);
        }
    });

    let entry_for_task = entry.clone();
    let turn_id_for_task = turn_id.clone();
    let session_id_for_task = session_id.clone();
    let user_message_for_persist = message.clone();
    tokio::spawn(async move {
        // RAII guard: clears in_flight regardless of how this future
        // exits — Ok, Err, or panic.
        let _guard = TurnGuard {
            entry: entry_for_task.clone(),
        };

        let input = AgentTurnInput {
            turn_id: turn_id_for_task.clone(),
            system_prompt: Some(system_prompt),
            history: history_snapshot,
            user_message: message,
            tools,
        };
        let agent_audit = AgentAudit {
            log: audit,
            feature: "chat_system",
        };
        let outcome = run_turn(
            input,
            client,
            dispatcher,
            &agent_cfg,
            &tx,
            stop,
            Some(agent_audit),
        )
        .await;
        match outcome {
            Ok(out) => {
                let terminal = matches!(
                    out.finish_reason,
                    FinishReason::EndTurn
                        | FinishReason::Cancelled
                        | FinishReason::UserHalted
                        | FinishReason::Errored
                );
                let new_history = out.history.clone();
                if let Ok(mut g) = entry_for_task.lock() {
                    g.history = new_history.clone();
                }
                if terminal
                    && let Some(user_msg) = user_message_for_persist.as_deref()
                {
                    persist_turn(
                        &file_path,
                        &rel_path,
                        user_msg,
                        &new_history,
                        &jobs,
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id_for_task.0,
                    turn_id = %turn_id_for_task.0,
                    "agent turn errored",
                );
            }
        }
        drop(tx);
        let _ = event_pump.await;
    });

    Ok(())
}

/// Append the just-finished turn to the session markdown file and
/// nudge the indexer so search picks it up. Best-effort: errors are
/// logged, never propagated — a failed write shouldn't surface to the
/// chat panel as a turn failure.
async fn persist_turn(
    file_path: &std::path::Path,
    rel_path: &str,
    user_message: &str,
    history: &[Message],
    jobs: &IndexJobTx,
) {
    let (agent_text, tool_blocks) = render_latest_agent_turn(history);
    if let Err(e) = sessions::append_turn(file_path, user_message, &agent_text, &tool_blocks) {
        tracing::warn!(error = %e, "sessions: append_turn failed");
        return;
    }
    if let Err(e) = jobs
        .send(IndexJob::Upsert {
            rel_path: rel_path.to_string(),
            force: false,
        })
        .await
    {
        tracing::warn!(error = %e, "sessions: enqueue Upsert failed");
    }
}

/// Walk back from the end of the history collecting agent text and
/// tool-call fences for the most recent turn. The agent loop's
/// per-iteration history shape is: assistant(text + tool_calls?) —
/// optionally followed by user(tool_results) — repeated, then a
/// terminal assistant(text). We collect every assistant message after
/// the most recent user message that *isn't* a tool-results message.
fn render_latest_agent_turn(history: &[Message]) -> (String, Vec<String>) {
    use hiker_core::llm::Role;
    // Find the boundary: most recent User message whose tool_results is empty.
    let mut start = 0usize;
    for (i, m) in history.iter().enumerate().rev() {
        if m.role == Role::User && m.tool_results.is_empty() {
            start = i + 1;
            break;
        }
    }
    let mut text_parts = Vec::new();
    let mut tool_blocks = Vec::new();
    for m in &history[start..] {
        if m.role == Role::Assistant {
            if !m.content.is_empty() {
                text_parts.push(m.content.clone());
            }
            for c in &m.tool_calls {
                let block = serde_json::json!({
                    "name": c.name,
                    "arguments": c.arguments,
                });
                tool_blocks.push(serde_json::to_string_pretty(&block).unwrap_or_default());
            }
        }
        if m.role == Role::User && !m.tool_results.is_empty() {
            for r in &m.tool_results {
                let block = serde_json::json!({
                    "tool_result": r.name,
                    "ok": r.ok,
                    "output": summarize(&r.output, 200),
                });
                tool_blocks.push(serde_json::to_string_pretty(&block).unwrap_or_default());
            }
        }
    }
    (text_parts.join("\n\n"), tool_blocks)
}

fn summarize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pull every dependency the loop needs out of the open vault session.
fn prepare_for_turn(state: &State<'_, AppState>) -> Result<TurnPreparation, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    let cfg: Config = session
        .config
        .read()
        .map_err(|e| e.to_string())?
        .clone();
    if !cfg.llm.enabled {
        return Err("llm disabled".to_string());
    }
    let llm_cfg = cfg.llm.clone();
    let mcp = session
        .mcp
        .as_ref()
        .ok_or_else(|| "mcp server not running".to_string())?;
    let agent_handler = mcp.agent_handler();
    let dispatcher: Arc<dyn ToolDispatcher> =
        Arc::new(McpAgentDispatcher::new(agent_handler));
    let client: Arc<dyn LlmClient> = Arc::new(
        GraniteLlmClient::from_config(&llm_cfg).map_err(|e| e.to_string())?,
    );
    // status: mcp-tool-toggles
    // Pass the live `[mcp.tools]` snapshot so the per-tool gates apply
    // to the advertised list — the model only sees tools it can
    // actually call.
    let tools = hiker_mcp::agent_tool_defs_filtered(
        cfg.llm.enabled && cfg.tasks.expose_to_chat_agent,
        Some(&cfg.mcp.tools),
    );
    let vault_name = session
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vault")
        .to_string();
    let prompts: Arc<Prompts> = session.prompts.clone();
    let system_prompt = prompts
        .render("chat_system", [("vault_name", vault_name.as_str())])
        .map_err(|e| e.to_string())?;

    // status: llm-acp-client-optional
    let mcp_port = mcp.addr().port();
    let acp_command = cfg.acp.command.clone();

    Ok(TurnPreparation {
        client,
        dispatcher,
        agent_cfg: llm_cfg.agent.clone(),
        system_prompt,
        tools,
        registry: session.chat.clone(),
        audit: session.audit.clone(),
        vault_root: session.root.clone(),
        model: llm_cfg.provider.model.clone(),
        provider: llm_cfg.provider.backend.clone(),
        jobs: session.indexer.job_sender(),
        acp_command,
        mcp_port: Some(mcp_port),
    })
}

fn registry_from_state(state: &State<'_, AppState>) -> Result<Arc<ChatRegistry>, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    Ok(session.chat.clone())
}

/// Shared task spawn for `chat_send` and `chat_continue` via the ACP
/// backend. Parallel to `spawn_turn_task` but calls
/// `core::acp::run_acp_turn` instead of `core::agent::run_turn`.
///
/// status: llm-acp-client-optional
fn spawn_acp_turn(
    app: AppHandle,
    prep: TurnPreparation,
    session_id: SessionId,
    turn_id: TurnId,
    message: String,
    context_blocks: Vec<ChatContextBlock>,
) -> Result<(), String> {
    let TurnPreparation {
        acp_command,
        mcp_port,
        system_prompt,
        registry,
        audit,
        vault_root,
        jobs,
        ..
    } = prep;

    let entry = registry
        .entry(&session_id)
        .ok_or_else(|| "session not found".to_string())?;

    let mcp_port = mcp_port.ok_or_else(|| "mcp server not running".to_string())?;
    let command_line = acp_command.trim().to_string();
    if command_line.is_empty() {
        return Err("ACP command not configured".to_string());
    }

    let stop = StopSignal::new();
    let (file_path, rel_path) = {
        let mut guard = entry.lock().map_err(|e| e.to_string())?;
        if guard.in_flight {
            return Err("turn already in flight".to_string());
        }
        guard.in_flight = true;
        guard.stop = stop.clone();
        guard.system_prompt = system_prompt.clone();

        // Inject context blocks as synthetic history for persistence.
        let mut active_note_path: Option<String> = None;
        for block in &context_blocks {
            if block.rel_path.is_empty() {
                continue;
            }
            let body = match block.kind.as_str() {
                "activeNote" => {
                    let same = guard.last_active_note.as_deref()
                        == Some(block.rel_path.as_str());
                    active_note_path = Some(block.rel_path.clone());
                    if same {
                        format!(
                            "[hiker context] user is still viewing `{}`",
                            block.rel_path
                        )
                    } else {
                        format!(
                            "[hiker context] user is currently viewing `{}` — its current buffer contents:\n\n{}",
                            block.rel_path, block.content
                        )
                    }
                }
                "selection" => {
                    let where_at = block
                        .line_range
                        .as_deref()
                        .map(|r| format!("`{}` ({})", block.rel_path, r))
                        .unwrap_or_else(|| format!("`{}`", block.rel_path));
                    format!(
                        "[hiker context] user attached the selection from {where_at}:\n\n{}",
                        block.content
                    )
                }
                _ => format!(
                    "[hiker context] user attached `{}`:\n\n{}",
                    block.rel_path, block.content
                ),
            };
            guard.history.push(Message::user(body));
        }
        guard.last_active_note = active_note_path;

        (guard.file_path.clone(), guard.rel_path.clone())
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let app_for_events = app.clone();
    let event_pump = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            crate::events::emit_chat_event(&app_for_events, &ev);
        }
    });

    let entry_for_task = entry.clone();
    let user_message_for_persist = message.clone();
    tokio::spawn(async move {
        let _guard = TurnGuard {
            entry: entry_for_task.clone(),
        };

        let agent_audit = AgentAudit {
            log: audit,
            feature: "chat_system",
        };
        let outcome = hiker_core::acp::run_acp_turn(hiker_core::acp::AcpTurnInput {
            command_line: &command_line,
            vault_root: &vault_root,
            mcp_port,
            user_message: &message,
            context_blocks: &context_blocks,
            session_id: &turn_id.0,
            event_tx: &tx,
            stop,
            audit: Some(agent_audit),
        })
        .await;

        match outcome {
            Ok(out) => {
                if !user_message_for_persist.is_empty() {
                    let history = vec![
                        Message::user(&user_message_for_persist),
                        Message::assistant(&out.agent_text),
                    ];
                    persist_turn(
                        &file_path,
                        &rel_path,
                        &user_message_for_persist,
                        &history,
                        &jobs,
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id.0,
                    turn_id = %turn_id.0,
                    "acp turn errored",
                );
            }
        }
        drop(tx);
        let _ = event_pump.await;
    });

    Ok(())
}
