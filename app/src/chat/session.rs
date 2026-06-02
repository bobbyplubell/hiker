//! Session lifecycle: discover sessions on disk, create new ones,
//! delete, switch active. Adapter over `hiker_core::sessions` that
//! materialises `ChatSession` values for the renderer.

use std::path::Path;
use std::sync::RwLock;

use hiker_core::config::Config;
use hiker_core::sessions::{self, SessionId, SessionMeta};

use crate::chat::state::{ChatRegistry, ChatRole, ChatSession, ChatTurn, ToolCard};

/// Read the configured chat-session folder (`[chat] chats_dir`) from the
/// shared config, falling back to the default `"chats/"` if the lock is
/// poisoned. Sessions live at `<chats_dir>/<date>-<id>.md` per
/// `chat-session-markdown-store`.
pub fn chats_dir(config: &RwLock<Config>) -> String {
    config
        .read()
        .map(|c| c.chat.chats_dir.clone())
        .unwrap_or_else(|_| "chats/".to_string())
}

/// Walk `<vault>/<chats_dir>/` (and its `imported/` subfolder) and hydrate
/// the registry with one `ChatSession` per file. Called once after
/// `bootstrap::open_vault` so the picker shows historic sessions.
pub fn discover(reg: &mut ChatRegistry, vault_root: &Path, chats_dir: &str) {
    let infos = match sessions::list(vault_root, chats_dir) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "chat: list failed");
            return;
        }
    };
    for info in infos {
        // Skip ones we already loaded (re-discovery is a no-op).
        if reg.sessions.contains_key(&info.id.0) {
            continue;
        }
        let turns = sessions::parse_session(&info.abs_path).unwrap_or_default();
        let preview = turns
            .first()
            .map(|t| reg.short_preview(&t.user, 60))
            .unwrap_or_else(|| "(empty session)".to_string());
        // Build the LLM-facing history (Vec<Message>) up front so the
        // session resumes with full tool-call alternation. We keep it
        // separately from `turns` so the renderer keeps its
        // role-by-role view.
        let resumed_history = sessions::resumed_turns_to_history(&turns);
        let mut chat_turns: Vec<ChatTurn> = Vec::new();
        for t in turns {
            if !t.user.is_empty() {
                chat_turns.push(ChatTurn {
                    role: ChatRole::User,
                    text: t.user,
                    tool: None,
                });
            }
            if !t.agent.is_empty() {
                chat_turns.push(ChatTurn {
                    role: ChatRole::Assistant,
                    text: t.agent,
                    tool: None,
                });
            }
            // Re-render persisted tool calls + results as Tool cards.
            for tc in &t.tool_calls {
                let matched = t
                    .tool_results
                    .iter()
                    .find(|r| r.call_id == tc.id);
                chat_turns.push(ChatTurn {
                    role: ChatRole::Tool,
                    text: String::new(),
                    tool: Some(ToolCard {
                        tool_name: tc.name.clone(),
                        args: tc.arguments.clone(),
                        result: matched.map(|r| r.output.clone()),
                        ok: matched.map(|r| r.ok).unwrap_or(true),
                        produced_write: false,
                        target_path: None,
                    }),
                });
            }
        }
        reg.upsert(ChatSession {
            id: info.id.0.clone(),
            preview,
            rel_path: info.rel_path,
            turns: chat_turns,
            pending: false,
            streaming_buf: String::new(),
            mtime_unix: info.mtime_unix,
            resumed_history,
        });
    }
    // Adopt the newest as active if nothing is active yet.
    if reg.active.is_none() {
        let mut all: Vec<(&String, &ChatSession)> = reg.sessions.iter().collect();
        all.sort_by_key(|x| std::cmp::Reverse(x.1.mtime_unix));
        if let Some((id, _)) = all.first() {
            reg.active = Some((*id).clone());
        }
    }
}

/// Mint a brand-new session, create its on-disk file, register it as
/// active. Returns the new session id.
pub fn create_new(
    reg: &mut ChatRegistry,
    vault_root: &Path,
    chats_dir: &str,
    model: &str,
    provider: &str,
) -> std::io::Result<String> {
    let id = SessionId::generate();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let meta = SessionMeta {
        id: id.clone(),
        created_at_unix: now,
        model: model.to_string(),
        provider: provider.to_string(),
    };
    sessions::create_session_file(vault_root, chats_dir, &meta)?;
    let rel = sessions::session_rel_path(chats_dir, &id, now);
    reg.upsert(ChatSession {
        id: id.0.clone(),
        preview: "(new session)".to_string(),
        rel_path: rel,
        turns: Vec::new(),
        pending: false,
        streaming_buf: String::new(),
        mtime_unix: now,
        resumed_history: Vec::new(),
    });
    reg.active = Some(id.0.clone());
    Ok(id.0)
}

/// Soft-delete: rm the on-disk file and forget the registry entry.
/// Stretch goal: route through `core::ops::delete` to land in the
/// vault trash; for now we just unlink.
pub fn delete(
    reg: &mut ChatRegistry,
    vault_root: &Path,
    chats_dir: &str,
    id: &str,
) -> std::io::Result<()> {
    // Look up the file path; fall back to a directory scan if the
    // entry was discovered without rel_path.
    let abs = if let Some(s) = reg.sessions.get(id)
        && !s.rel_path.is_empty()
    {
        vault_root.join(&s.rel_path)
    } else {
        let infos = sessions::list(vault_root, chats_dir)?;
        let Some(info) = infos.into_iter().find(|i| i.id.0 == id) else {
            reg.forget(id);
            return Ok(());
        };
        info.abs_path
    };
    if abs.exists() {
        std::fs::remove_file(&abs)?;
    }
    reg.forget(id);
    Ok(())
}

pub fn set_active(reg: &mut ChatRegistry, id: &str) {
    if reg.sessions.contains_key(id) {
        reg.active = Some(id.to_string());
    }
}

