//! In-memory chat registry: sessions keyed by id, a single active
//! session pointer, plus an mpsc channel the spawned reply tasks drain
//! their results into. The egui update loop pumps the receiver each
//! frame (mirrors the `fs_events` pattern in `state.rs`) and folds
//! events back into the matching session.
#![allow(clippy::items_after_test_module)]

use std::collections::HashMap;
use std::sync::Mutex;

use hiker_core::agent::StopSignal;
use hiker_core::llm::Message;

/// One message in a session transcript. Most turns are plain
/// user/assistant text; `Tool` turns carry the structured tool-call
/// record so the chat panel can render the dedicated card UI
/// (mirrors `ui/src/chat/toolCard.ts`).
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub text: String,
    /// When present, this turn is a tool invocation rather than text.
    pub tool: Option<ToolCard>,
}

/// Structured tool-call view used by the chat renderer.
#[derive(Debug, Clone)]
pub struct ToolCard {
    pub tool_name: String,
    pub args: String,
    /// Some(_) once the tool returned; None while still in flight.
    pub result: Option<String>,
    pub ok: bool,
    /// Whether this tool call produced a pending op-log write (a write/edit
    /// tool that returned `status: "written"` or `"staged"`). The card's
    /// Accept / Reject buttons resolve the live pending ops for `target_path`
    /// off the op log at render time (`agent-write-review-mode`); read-only
    /// tools (`search_notes`, `get_note`) leave this `false` so they never
    /// surface review affordances even when they touch the same path.
    pub produced_write: bool,
    /// Vault-relative path the tool acted on, sniffed from the result
    /// payload. Drives the header-click "open the affected note" UX from
    /// `ui/src/chat/toolCard.ts` (`TouchedNoteRouting`).
    pub target_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
}

/// Per-session in-memory state. `turns` is the rendered transcript;
/// `pending` is true between user-send and the assistant reply
/// arriving (drives the "..." indicator).
#[derive(Debug, Clone, Default)]
pub struct ChatSession {
    pub id: String,
    /// First-user-message preview used as the row label in the picker.
    pub preview: String,
    /// On-disk vault-relative path. Empty until the session file is
    /// created (lazy on first send).
    pub rel_path: String,
    pub turns: Vec<ChatTurn>,
    pub pending: bool,
    /// Streaming buffer: when a reply task posts incremental text it
    /// lands here; on finish we push it as a turn and clear.
    pub streaming_buf: String,
    /// Unix mtime — populated when discovered or written. Drives the
    /// newest-first sort in the picker.
    pub mtime_unix: i64,
    /// Provider-shaped history seeded from on-disk session markdown when
    /// the session is resumed. Tool calls and tool results are preserved
    /// here so the next turn the agent runs against this session sees a
    /// coherent assistant→tool-result alternation
    /// (`chat-session-markdown-store`). Empty for freshly-created sessions
    /// — those build history from `turns` instead.
    pub resumed_history: Vec<Message>,
}

/// One message produced by a spawned reply task. Folded back into the
/// matching session on the egui frame loop.
#[derive(Debug)]
pub enum ChatEvent {
    /// Streaming token delta. Appends to the active session's
    /// `streaming_buf`.
    Delta { session_id: String, text: String },
    /// Reply finished. Promotes `streaming_buf` to a turn and clears
    /// the pending flag.
    Finished { session_id: String },
    /// Reply errored. Replaces `streaming_buf` with the error text and
    /// clears the pending flag.
    // TODO: producer side is not wired yet; reply errors currently surface
    // via Finished with an error string. Keep variant so apply() handles it
    // once the streaming layer differentiates the two.
    #[allow(dead_code)]
    Error { session_id: String, message: String },
    /// A tool was invoked. Surfaces as a tool turn card with no result
    /// yet (`ok: true, result: None`).
    ToolCall { session_id: String, name: String, args: String },
    /// The tool finished. Updates the most recent matching `Tool` turn
    /// with the result.
    ToolResult {
        session_id: String,
        name: String,
        ok: bool,
        result: String,
    },
}

/// App-level chat registry. Lives on `AppState`. `tx` is cheap-cloned
/// into spawned reply tasks; `rx` is drained each frame by
/// `pump_events`.
pub struct ChatRegistry {
    pub sessions: HashMap<String, ChatSession>,
    pub active: Option<String>,
    /// Sender that reply tasks post into. Cloned per task.
    pub tx: tokio::sync::mpsc::UnboundedSender<ChatEvent>,
    /// Drained by `pump_events` on each frame. Wrapped in a Mutex so
    /// `AppState` stays Send while still letting the immediate-mode
    /// loop drain mutably.
    pub rx: Mutex<tokio::sync::mpsc::UnboundedReceiver<ChatEvent>>,
    /// Composed user input per session. Keyed by session id so each
    /// session keeps its draft when the user flips between them.
    pub drafts: HashMap<String, String>,
    /// Per-session stop signal for an in-flight turn. Inserted by
    /// `send::send` before spawning, removed when the reply
    /// finishes/errors. Drives the chat panel's Stop button
    /// (`chat-panel-stop-button`).
    pub stop_signals: HashMap<String, StopSignal>,
    /// Read-only editor instances backing markdown previews inside tool
    /// cards, keyed by `session:turn:field`. Persisted here (not on the
    /// cloned `ChatSession`/`ToolCard`) so each preview keeps its
    /// measured content height and built decoration layer across frames.
    pub md_previews: crate::chat::md_preview::Cache,
}

impl ChatRegistry {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            sessions: HashMap::new(),
            active: None,
            tx,
            rx: Mutex::new(rx),
            drafts: HashMap::new(),
            stop_signals: HashMap::new(),
            md_previews: HashMap::new(),
        }
    }

    pub fn active_session(&self) -> Option<&ChatSession> {
        self.active.as_ref().and_then(|id| self.sessions.get(id))
    }

    pub fn upsert(&mut self, session: ChatSession) {
        self.sessions.insert(session.id.clone(), session);
    }

    /// Drop a session entirely (used by delete). Clears the active
    /// slot if it matched.
    pub fn forget(&mut self, id: &str) {
        self.sessions.remove(id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

}

impl Default for ChatRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod sniff_tests {
    use super::*;

    #[test]
    fn target_path_from_args_handles_common_shapes() {
        let reg = ChatRegistry::default();
        assert_eq!(
            reg.sniff_target_path_from_args(r#"{"path":"notes/foo.md","content":"x"}"#),
            Some("notes/foo.md".to_string()),
        );
        assert_eq!(reg.sniff_target_path_from_args(""), None);
        assert_eq!(reg.sniff_target_path_from_args("not json"), None);
        // Tools without a path field (search_notes etc.) return None.
        assert_eq!(
            reg.sniff_target_path_from_args(r#"{"q":"hiker","k":5}"#),
            None,
        );
    }

    #[test]
    fn target_path_from_result_prefers_target_path_field() {
        let reg = ChatRegistry::default();
        assert_eq!(
            reg.sniff_target_path_from_result(
                r#"{"status":"staged","staging_id":"01H","target_path":"a/b.md"}"#
            ),
            Some("a/b.md".to_string()),
        );
        // Falls back to `path` when `target_path` is absent.
        assert_eq!(
            reg.sniff_target_path_from_result(r#"{"path":"c/d.md"}"#),
            Some("c/d.md".to_string()),
        );
        assert_eq!(reg.sniff_target_path_from_result(""), None);
    }

    #[test]
    fn produced_write_true_for_written_and_staged() {
        let reg = ChatRegistry::default();
        // Direct mode (default): the edit staged into the op-log pending
        // queue and the result reports `written` — the card should offer
        // review.
        assert!(reg.result_produced_write(r#"{"status":"written"}"#));
        // Legacy review mode still reports `staged`.
        assert!(reg.result_produced_write(r#"{"status":"staged","staging_id":"01HXAB"}"#));
    }

    #[test]
    fn produced_write_false_for_reads_and_garbage() {
        let reg = ChatRegistry::default();
        // Read-only tool results carry no write status.
        assert!(!reg.result_produced_write(r#"{"hits":[]}"#));
        assert!(!reg.result_produced_write("garbage"));
        assert!(!reg.result_produced_write(""));
    }
}

impl ChatRegistry {
/// Pull a vault-relative path out of a tool-call arguments JSON blob.
/// Every write/edit tool param shape (per
/// `mcp-server/src/handler/params.rs`) keys the target by `path`, so a
/// flat lookup is enough. Returns `None` when the JSON is unreadable or
/// the field is absent (read-only tools like `search_notes` don't carry
/// one).
fn sniff_target_path_from_args(&self, args_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args_json).ok()?;
    v.get("path")
        .and_then(|p| p.as_str())
        .map(std::string::ToString::to_string)
}

/// Variant that reads from a tool *result* payload — used as a fallback
/// when the args weren't available (e.g. tool-call-complete arrived
/// without args streamed). The handler shapes use `target_path` on
/// proposal responses and `path` on most others.
fn sniff_target_path_from_result(&self, result_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(result_json).ok()?;
    if let Some(s) = v.get("target_path").and_then(|p| p.as_str()) {
        return Some(s.to_string());
    }
    v.get("path")
        .and_then(|p| p.as_str())
        .map(std::string::ToString::to_string)
}

/// Whether a tool result indicates the call produced a pending note-edit
/// write. Write/edit tools (`write_note`, `edit_note`, `set_frontmatter`,
/// `apply_tag`, `remove_tag`) return `status: "written"` (direct mode, which
/// stages the edit into the op log's pending queue per
/// `op-log-ops-producer-helpers`) or `status: "staged"` (legacy review mode).
/// Read-only tools carry no such status, so this returns `false` and their
/// cards never surface Accept / Reject. The actual op ids are resolved off the
/// op log by `target_path` at render time — the result payload doesn't carry
/// them, so this is just the "did this card write?" gate.
fn result_produced_write(&self, result_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(result_json) else {
        return false;
    };
    matches!(
        v.get("status").and_then(|s| s.as_str()),
        Some("written" | "staged")
    )
}

/// Drain the reply-event channel and fold each event into the
/// matching session. Called once per frame from the panel renderers
/// before drawing — keeps the immediate-mode loop the canonical place
/// where session state mutates.
pub fn pump_events(&mut self) {
    let reg = self;
    let mut events: Vec<ChatEvent> = Vec::new();
    if let Ok(mut rx) = reg.rx.lock() {
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }
    for ev in events {
        match ev {
            ChatEvent::Delta { session_id, text } => {
                if let Some(s) = reg.sessions.get_mut(&session_id) {
                    s.streaming_buf.push_str(&text);
                }
            }
            ChatEvent::Finished { session_id } => {
                reg.stop_signals.remove(&session_id);
                if let Some(s) = reg.sessions.get_mut(&session_id) {
                    let text = std::mem::take(&mut s.streaming_buf);
                    if !text.is_empty() {
                        s.turns.push(ChatTurn {
                            role: ChatRole::Assistant,
                            text,
                            tool: None,
                        });
                    }
                    s.pending = false;
                }
            }
            ChatEvent::Error { session_id, message } => {
                reg.stop_signals.remove(&session_id);
                if let Some(s) = reg.sessions.get_mut(&session_id) {
                    s.streaming_buf.clear();
                    s.turns.push(ChatTurn {
                        role: ChatRole::Assistant,
                        text: format!("(error) {}", message),
                        tool: None,
                    });
                    s.pending = false;
                }
            }
            ChatEvent::ToolCall { session_id, name, args } => {
                let target_path = reg.sniff_target_path_from_args(&args);
                if let Some(s) = reg.sessions.get_mut(&session_id) {
                    s.turns.push(ChatTurn {
                        role: ChatRole::Tool,
                        text: String::new(),
                        tool: Some(ToolCard {
                            tool_name: name,
                            args,
                            result: None,
                            ok: true,
                            produced_write: false,
                            target_path,
                        }),
                    });
                }
            }
            ChatEvent::ToolResult { session_id, name, ok, result } => {
                let produced_write = reg.result_produced_write(&result);
                let result_target = reg.sniff_target_path_from_result(&result);
                if let Some(s) = reg.sessions.get_mut(&session_id) {
                    // Find the most-recent in-flight tool card matching
                    // `name` and fold the result in. Falls back to a new
                    // turn if we missed the call event.
                    let mut updated = false;
                    for turn in s.turns.iter_mut().rev() {
                        if turn.role == ChatRole::Tool
                            && let Some(tool) = turn.tool.as_mut()
                            && tool.tool_name == name
                            && tool.result.is_none()
                        {
                            tool.result = Some(result.clone());
                            tool.ok = ok;
                            tool.produced_write = produced_write;
                            if tool.target_path.is_none() {
                                tool.target_path = result_target.clone();
                            }
                            updated = true;
                            break;
                        }
                    }
                    if !updated {
                        s.turns.push(ChatTurn {
                            role: ChatRole::Tool,
                            text: String::new(),
                            tool: Some(ToolCard {
                                tool_name: name,
                                args: String::new(),
                                result: Some(result),
                                ok,
                                produced_write,
                                target_path: result_target,
                            }),
                        });
                    }
                }
            }
        }
    }
}
}
