//! Persisted chat sessions. See `docs/llm.md` §Sessions.
//!
//! A session is the unit of conversational continuity — many turns share
//! one accumulating message history, persisted as markdown under the
//! visible chats folder `vault/<chats_dir>/<YYYY-MM-DD>-<id>.md` (default
//! `chats/`, configurable via `[chat] chats_dir`). Sessions are ordinary
//! visible notes, not hidden under `.hiker/` (`subsystem-notes-visible`).
//! Markdown is the source of truth; the in-memory `SessionState` in the
//! chat command surface is a working cache rebuilt from it on resume.
//!
//! File shape:
//!
//! ```text
//! ---
//! hiker.session_id: 01HF...
//! hiker.created_at: 2026-05-08T12:34:56Z
//! hiker.model: claude-sonnet-4-7
//! hiker.provider: anthropic
//! hiker.turn_count: 3
//! ---
//!
//! ## User
//!
//! find me notes about hiking
//!
//! ## Agent
//!
//! Here are three matching notes...
//!
//! ## User
//!
//! ...
//! ```
//
// status: chat-session-markdown-store

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use hiker_llm::{Message, Role, ToolCall, ToolResult};

/// Wraps the per-session id. Cheap to clone, opaque to callers; the
/// chat command surface mints fresh ones via `SessionId::generate`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Mint a fresh session id. Format: 12 lowercase alphanumeric chars
    /// derived from the high-precision timestamp + a small random
    /// suffix. Not cryptographic — collision-resistant enough for
    /// per-vault session files.
    pub fn generate() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // Mix a small bit of randomness so two ids minted in the same
        // nanosecond still differ.
        let salt = (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
        let v = now ^ salt;
        Self(format!("{:012x}", v & 0xFFFFFFFFFFFF))
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Frontmatter we write at session-file creation. `turn_count` is
/// stamped on the first append; subsequent appends do not rewrite the
/// header — keeping the header static lets the file be valid frontmatter
/// even mid-session.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: SessionId,
    pub created_at_unix: i64,
    pub model: String,
    pub provider: String,
}

/// One discoverable session file on disk. Returned by `list`
/// sorted newest-first.
#[derive(Debug, Clone)]
pub struct SessionFileInfo {
    pub id: SessionId,
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub mtime_unix: i64,
}

/// Normalize a configured `chats_dir` to a vault-relative path with no
/// leading/trailing slashes (`"chats/"` → `"chats"`). Falls back to the
/// default `"chats"` when the value is empty after trimming.
fn normalize_chats_dir(chats_dir: &str) -> &str {
    let trimmed = chats_dir.trim_matches('/');
    if trimmed.is_empty() {
        "chats"
    } else {
        trimmed
    }
}

/// Absolute path of the chats folder: `<vault_root>/<chats_dir>/`.
pub fn dir(vault_root: &Path, chats_dir: &str) -> PathBuf {
    vault_root.join(normalize_chats_dir(chats_dir))
}

/// `vault/<chats_dir>/<YYYY-MM-DD>-<id>.md`.
pub fn session_file_path(
    vault_root: &Path,
    chats_dir: &str,
    id: &SessionId,
    created_at_unix: i64,
) -> PathBuf {
    let date = format_date_yyyy_mm_dd(created_at_unix);
    dir(vault_root, chats_dir).join(format!("{}-{}.md", date, id.0))
}

/// Vault-relative path matching `session_file_path`'s on-disk shape.
/// Sessions are ordinary visible notes in `<chats_dir>/`, so the watcher
/// + indexer pick them up like any other note (no `.hiker/` carve-out).
pub fn session_rel_path(chats_dir: &str, id: &SessionId, created_at_unix: i64) -> String {
    let date = format_date_yyyy_mm_dd(created_at_unix);
    format!("{}/{}-{}.md", normalize_chats_dir(chats_dir), date, id.0)
}

/// Create the session file with the YAML frontmatter header. Idempotent
/// over the directory (creates if missing); errors if the file already
/// exists so a duplicate id is loud rather than silent.
pub fn create_session_file(
    vault_root: &Path,
    chats_dir: &str,
    meta: &SessionMeta,
) -> io::Result<PathBuf> {
    let dir = dir(vault_root, chats_dir);
    fs::create_dir_all(&dir)?;
    let path = session_file_path(vault_root, chats_dir, &meta.id, meta.created_at_unix);
    if path.exists() {
        return Ok(path);
    }
    let iso_created_at = {
        // Minimal ISO 8601 in UTC. Same civil-from-days math as
        // `format_date_yyyy_mm_dd`; we tack on hh:mm:ss for the time-of-day.
        let day_secs = meta.created_at_unix.rem_euclid(86_400);
        let h = day_secs / 3_600;
        let m = (day_secs % 3_600) / 60;
        let s = day_secs % 60;
        format!(
            "{}T{:02}:{:02}:{:02}Z",
            format_date_yyyy_mm_dd(meta.created_at_unix),
            h,
            m,
            s
        )
    };
    let header = format!(
        "---\nhiker.session_id: {}\nhiker.created_at: {}\nhiker.model: {}\nhiker.provider: {}\n---\n\n",
        meta.id.0,
        iso_created_at,
        meta.model,
        meta.provider,
    );
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    f.write_all(header.as_bytes())?;
    f.flush()?;
    Ok(path)
}

/// Append one user/agent turn to an existing session file. `tool_blocks`
/// renders each tool call as a fenced code block under the agent
/// section (one ```hiker-tool-call``` block per call). Legacy/raw entry
/// point retained for callers that already have stringified blocks.
pub fn append_turn(
    path: &Path,
    user_message: &str,
    agent_text: &str,
    tool_blocks: &[String],
) -> io::Result<()> {
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    let mut s = String::new();
    s.push_str("## User\n\n");
    s.push_str(user_message.trim_end());
    s.push_str("\n\n## Agent\n\n");
    if !agent_text.is_empty() {
        s.push_str(agent_text.trim_end());
        s.push_str("\n\n");
    }
    for block in tool_blocks {
        s.push_str("```hiker-tool-call\n");
        s.push_str(block.trim_end());
        s.push_str("\n```\n\n");
    }
    f.write_all(s.as_bytes())?;
    f.flush()
}

/// Append one structured turn, persisting tool calls and tool results as
/// paired `hiker-tool-call` / `hiker-tool-result` fenced JSON blocks under
/// the agent section. This is the canonical entry point per
/// `chat-session-markdown-store` — tool-call structure round-trips on
/// resume.
pub fn append_turn_structured(
    path: &Path,
    user_message: &str,
    agent_text: &str,
    tool_calls: &[ToolCall],
    tool_results: &[ToolResult],
) -> io::Result<()> {
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    let mut s = String::new();
    s.push_str("## User\n\n");
    s.push_str(user_message.trim_end());
    s.push_str("\n\n## Agent\n\n");
    if !agent_text.is_empty() {
        s.push_str(agent_text.trim_end());
        s.push_str("\n\n");
    }
    for tc in tool_calls {
        s.push_str("```hiker-tool-call\n");
        let json = serde_json::to_string(tc)
            .unwrap_or_else(|_| String::from("{}"));
        s.push_str(&json);
        s.push_str("\n```\n\n");
        // Pair the matching result (if any) directly after the call so
        // resumed history keeps the assistant→tool-result alternation
        // a provider expects.
        if let Some(res) = tool_results.iter().find(|r| r.call_id == tc.id) {
            s.push_str("```hiker-tool-result\n");
            let json = serde_json::to_string(res)
                .unwrap_or_else(|_| String::from("{}"));
            s.push_str(&json);
            s.push_str("\n```\n\n");
        }
    }
    // Orphan results (no matching call_id in tool_calls) — write them too
    // so we don't lose data. Rare but possible if the agent loop emitted a
    // synthetic error result.
    for res in tool_results {
        if !tool_calls.iter().any(|c| c.id == res.call_id) {
            s.push_str("```hiker-tool-result\n");
            let json = serde_json::to_string(res)
                .unwrap_or_else(|_| String::from("{}"));
            s.push_str(&json);
            s.push_str("\n```\n\n");
        }
    }
    f.write_all(s.as_bytes())?;
    f.flush()
}

/// Discover all session files under `<vault>/<chats_dir>/`, sorted
/// newest-first by filesystem mtime. Recurses one level into the
/// `imported/` subfolder so imported sessions surface in the picker
/// alongside native ones. Empty when the directory doesn't exist yet.
pub fn list(vault_root: &Path, chats_dir: &str) -> io::Result<Vec<SessionFileInfo>> {
    let root = dir(vault_root, chats_dir);
    let mut out = Vec::new();
    collect_sessions(&root, vault_root, &mut out)?;
    let imported = root.join("imported");
    if imported.is_dir() {
        collect_sessions(&imported, vault_root, &mut out)?;
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.mtime_unix));
    Ok(out)
}

/// Append every `*.md` session file directly under `dir` to `out`. A
/// missing directory is treated as empty (not an error) so a fresh vault
/// lists cleanly.
fn collect_sessions(
    dir: &Path,
    vault_root: &Path,
    out: &mut Vec<SessionFileInfo>,
) -> io::Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        // `<YYYY-MM-DD>-<id>` (native) or `<source>-<YYYY-MM-DD>-<id>`
        // (imported) — the id is the final hyphen-delimited segment.
        let id_opt = (|| -> Option<SessionId> {
            let stem = path.file_stem()?.to_str()?;
            let tail = stem.rsplit('-').next()?;
            if tail.is_empty() || tail == stem {
                return None;
            }
            Some(SessionId(tail.to_string()))
        })();
        let Some(id) = id_opt else { continue };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let rel = path
            .strip_prefix(vault_root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        out.push(SessionFileInfo {
            id,
            abs_path: path,
            rel_path: rel,
            mtime_unix: mtime,
        });
    }
    Ok(())
}

/// Most recent session file (newest mtime) or `None` if the dir is
/// empty / absent.
pub fn most_recent_session(
    vault_root: &Path,
    chats_dir: &str,
) -> io::Result<Option<SessionFileInfo>> {
    let mut all = list(vault_root, chats_dir)?;
    Ok(if all.is_empty() { None } else { Some(all.swap_remove(0)) })
}

/// One reconstructed turn, used to seed the in-memory cache when
/// resuming a session at vault open. Tool-call structure is preserved per
/// `chat-session-markdown-store` so the agent sees its prior tool use on
/// resume; otherwise the model infers from a tool-call-stripped history
/// that it can write notes without ever invoking `write_note`.
#[derive(Debug, Clone, Default)]
pub struct ResumedTurn {
    pub user: String,
    pub agent: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
}

/// Translate a sequence of resumed turns into the provider-shaped message
/// history the agent loop consumes. User text becomes a `User` message;
/// agent text plus tool calls become one `Assistant` message; tool results
/// become a `User` message with `tool_results` populated (matching the
/// shape `chat_with_tools` produces for fresh turns).
pub fn resumed_turns_to_history(turns: &[ResumedTurn]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(turns.len() * 2);
    for t in turns {
        if !t.user.is_empty() {
            out.push(Message::user(t.user.clone()));
        }
        if !t.agent.is_empty() || !t.tool_calls.is_empty() {
            out.push(Message {
                role: Role::Assistant,
                content: t.agent.clone(),
                tool_calls: t.tool_calls.clone(),
                tool_results: Vec::new(),
            });
        }
        if !t.tool_results.is_empty() {
            out.push(Message {
                role: Role::User,
                content: String::new(),
                tool_calls: Vec::new(),
                tool_results: t.tool_results.clone(),
            });
        }
    }
    out
}

/// Parse a session file back into alternating user/agent turns,
/// preserving any `hiker-tool-call` / `hiker-tool-result` JSON blocks in
/// the structured fields of `ResumedTurn`. Tool-call fences are stripped
/// from the agent prose so callers can render the text cleanly.
pub fn parse_session(path: &Path) -> io::Result<Vec<ResumedTurn>> {
    let body = fs::read_to_string(path)?;
    let body = {
        // Inline strip_frontmatter.
        let mut lines = body.split_inclusive('\n');
        match lines.next() {
            Some(first) if first.trim_end() == "---" => {
                let mut consumed = first.len();
                let mut result: &str = &body;
                for line in lines {
                    consumed += line.len();
                    if line.trim_end() == "---" {
                        result = &body[consumed..];
                        break;
                    }
                }
                result
            }
            _ => &body,
        }
    };
    let mut turns: Vec<ResumedTurn> = Vec::new();
    let mut current_section: Option<&'static str> = None; // "user" | "agent"
    let mut text_buf = String::new();
    let mut block_buf = String::new();
    let mut current_user = String::new();
    let mut pending_calls: Vec<ToolCall> = Vec::new();
    let mut pending_results: Vec<ToolResult> = Vec::new();
    // None = not in a block; Some("call"|"result") = currently inside a
    // tool fence of that kind.
    let mut in_block: Option<&'static str> = None;

    for line in body.lines() {
        if in_block.is_some() {
            if line.starts_with("```") {
                let kind = in_block.take().unwrap_or("");
                let parsed = block_buf.trim();
                if !parsed.is_empty() {
                    match kind {
                        "call" => {
                            if let Ok(tc) = serde_json::from_str::<ToolCall>(parsed) {
                                pending_calls.push(tc);
                            }
                        }
                        "result" => {
                            if let Ok(tr) = serde_json::from_str::<ToolResult>(parsed) {
                                pending_results.push(tr);
                            }
                        }
                        _ => {}
                    }
                }
                block_buf.clear();
            } else {
                block_buf.push_str(line);
                block_buf.push('\n');
            }
            continue;
        }
        if line == "## User" {
            flush_section(
                &mut turns,
                &current_section,
                &mut current_user,
                &mut text_buf,
                &mut pending_calls,
                &mut pending_results,
            );
            current_section = Some("user");
            continue;
        }
        if line == "## Agent" {
            flush_section(
                &mut turns,
                &current_section,
                &mut current_user,
                &mut text_buf,
                &mut pending_calls,
                &mut pending_results,
            );
            current_section = Some("agent");
            continue;
        }
        if line.starts_with("```hiker-tool-call") {
            in_block = Some("call");
            continue;
        }
        if line.starts_with("```hiker-tool-result") {
            in_block = Some("result");
            continue;
        }
        if current_section.is_some() {
            text_buf.push_str(line);
            text_buf.push('\n');
        }
    }
    flush_section(
        &mut turns,
        &current_section,
        &mut current_user,
        &mut text_buf,
        &mut pending_calls,
        &mut pending_results,
    );
    Ok(turns)
}

fn flush_section(
    turns: &mut Vec<ResumedTurn>,
    section: &Option<&'static str>,
    current_user: &mut String,
    text_buf: &mut String,
    pending_calls: &mut Vec<ToolCall>,
    pending_results: &mut Vec<ToolResult>,
) {
    let trimmed = text_buf.trim().to_string();
    text_buf.clear();
    match *section {
        Some("user") => {
            *current_user = trimmed;
        }
        Some("agent") => {
            turns.push(ResumedTurn {
                user: std::mem::take(current_user),
                agent: trimmed,
                tool_calls: std::mem::take(pending_calls),
                tool_results: std::mem::take(pending_results),
            });
        }
        _ => {}
    }
}

fn format_date_yyyy_mm_dd(unix_secs: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant, public domain). Avoids
    // pulling chrono / time just for a YYYY-MM-DD stamp.
    let days = unix_secs.div_euclid(86_400);
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let doe = (days - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_create_append_parse() {
        let dir = tempdir().unwrap();
        let meta = SessionMeta {
            id: SessionId("abc123".into()),
            created_at_unix: 1_754_654_400, // 2025-08-08T12:00:00Z
            model: "claude-sonnet-4-7".into(),
            provider: "anthropic".into(),
        };
        let path = create_session_file(dir.path(), "chats/", &meta).unwrap();
        append_turn(&path, "hello", "hi there", &[]).unwrap();
        append_turn(
            &path,
            "search the vault",
            "Found 2 notes.",
            &[r#"{"name":"search_notes","args":{"q":"hiker"}}"#.to_string()],
        )
        .unwrap();
        let turns = parse_session(&path).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user, "hello");
        assert_eq!(turns[0].agent, "hi there");
        assert_eq!(turns[1].user, "search the vault");
        // Tool fence is dropped from the parsed agent text.
        assert_eq!(turns[1].agent, "Found 2 notes.");
    }

    #[test]
    fn tool_call_and_result_round_trip() {
        let dir = tempdir().unwrap();
        let meta = SessionMeta {
            id: SessionId("abc123".into()),
            created_at_unix: 1_754_654_400,
            model: "claude-sonnet-4-7".into(),
            provider: "anthropic".into(),
        };
        let path = create_session_file(dir.path(), "chats/", &meta).unwrap();
        let tc = ToolCall {
            id: "call_1".into(),
            name: "search_notes".into(),
            arguments: r#"{"q":"hiker"}"#.into(),
        };
        let tr = ToolResult {
            call_id: "call_1".into(),
            name: "search_notes".into(),
            output: r#"{"hits":[]}"#.into(),
            ok: true,
        };
        append_turn_structured(
            &path,
            "find hiking notes",
            "Searching...",
            std::slice::from_ref(&tc),
            std::slice::from_ref(&tr),
        )
        .unwrap();
        let turns = parse_session(&path).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user, "find hiking notes");
        assert_eq!(turns[0].agent, "Searching...");
        assert_eq!(turns[0].tool_calls.len(), 1);
        assert_eq!(turns[0].tool_calls[0].id, "call_1");
        assert_eq!(turns[0].tool_results.len(), 1);
        assert_eq!(turns[0].tool_results[0].output, r#"{"hits":[]}"#);

        let hist = resumed_turns_to_history(&turns);
        // user / assistant(w/ tool_calls) / user(w/ tool_results)
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[1].role, Role::Assistant);
        assert_eq!(hist[1].tool_calls.len(), 1);
        assert_eq!(hist[2].role, Role::User);
        assert_eq!(hist[2].tool_results.len(), 1);
    }

    #[test]
    fn multi_turn_resumed_history_preserves_alternation() {
        // Two turns: first plain text, second with a tool call+result.
        // resumed_turns_to_history should emit exactly the message
        // sequence the provider expects:
        //   user1 / assistant1 / user2 / assistant2(tool_calls)
        //   / user(tool_results)
        let dir = tempdir().unwrap();
        let meta = SessionMeta {
            id: SessionId("multi".into()),
            created_at_unix: 1_754_654_400,
            model: "claude-sonnet-4-7".into(),
            provider: "anthropic".into(),
        };
        let path = create_session_file(dir.path(), "chats/", &meta).unwrap();
        append_turn(&path, "hello", "hi", &[]).unwrap();
        let tc = ToolCall {
            id: "c1".into(),
            name: "search_notes".into(),
            arguments: r#"{"q":"x"}"#.into(),
        };
        let tr = ToolResult {
            call_id: "c1".into(),
            name: "search_notes".into(),
            output: r#"{"hits":[]}"#.into(),
            ok: true,
        };
        append_turn_structured(
            &path,
            "second q",
            "second a",
            std::slice::from_ref(&tc),
            std::slice::from_ref(&tr),
        )
        .unwrap();

        let turns = parse_session(&path).unwrap();
        assert_eq!(turns.len(), 2);
        let hist = resumed_turns_to_history(&turns);
        assert_eq!(hist.len(), 5, "user/assistant/user/assistant+tc/user+tr");
        assert_eq!(hist[0].role, Role::User);
        assert_eq!(hist[0].content, "hello");
        assert_eq!(hist[1].role, Role::Assistant);
        assert_eq!(hist[1].content, "hi");
        assert!(hist[1].tool_calls.is_empty());
        assert_eq!(hist[2].role, Role::User);
        assert_eq!(hist[2].content, "second q");
        assert_eq!(hist[3].role, Role::Assistant);
        assert_eq!(hist[3].tool_calls.len(), 1);
        assert_eq!(hist[3].tool_calls[0].name, "search_notes");
        assert_eq!(hist[4].role, Role::User);
        assert_eq!(hist[4].tool_results.len(), 1);
        assert!(hist[4].tool_results[0].ok);
    }

    #[test]
    fn list_sessions_sorts_newest_first() {
        let dir = tempdir().unwrap();
        let m1 = SessionMeta {
            id: SessionId("aaa".into()),
            created_at_unix: 1_700_000_000,
            model: "m".into(),
            provider: "p".into(),
        };
        let m2 = SessionMeta {
            id: SessionId("bbb".into()),
            created_at_unix: 1_800_000_000,
            model: "m".into(),
            provider: "p".into(),
        };
        let p1 = create_session_file(dir.path(), "chats/", &m1).unwrap();
        let p2 = create_session_file(dir.path(), "chats/", &m2).unwrap();
        // Touch p2 last so its mtime is newer regardless of birth order.
        // Sleep ensures the file system registers a distinct timestamp;
        // some CoW / overlay filesystems have coarse (1s) mtime granularity.
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let _ = fs::write(&p2, "");  // truncation forces a visible mtime bump
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(&p2, fs::read(&p2).unwrap()).unwrap();
        let _ = p1;
        let list = list(dir.path(), "chats/").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id.0, "bbb");
    }

    #[test]
    fn list_parses_native_and_imported_ids() {
        // Native sessions live at <chats_dir>/<date>-<id>.md; imported
        // sessions at <chats_dir>/imported/<source>-<date>-<id>.md. `list`
        // recurses one level into `imported/` and extracts the trailing
        // hyphen-delimited segment as the id in both cases.
        let dir = tempdir().unwrap();
        let chats = dir.path().join("chats");
        let imported = chats.join("imported");
        fs::create_dir_all(&imported).unwrap();
        fs::write(chats.join("2026-05-08-abc123def.md"), "---\n---\n").unwrap();
        fs::write(
            imported.join("claude-code-2026-05-01-xyz789.md"),
            "---\n---\n",
        )
        .unwrap();
        let infos = list(dir.path(), "chats/").unwrap();
        let mut ids: Vec<String> = infos.iter().map(|i| i.id.0.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["abc123def".to_string(), "xyz789".to_string()]);
        // The imported entry's rel_path is under chats/imported/.
        assert!(infos
            .iter()
            .any(|i| i.rel_path == "chats/imported/claude-code-2026-05-01-xyz789.md"));
    }

    #[test]
    fn session_rel_path_normalizes_trailing_slash() {
        let id = SessionId("deadbeef".into());
        assert_eq!(
            session_rel_path("chats/", &id, 1_754_654_400),
            "chats/2025-08-08-deadbeef.md"
        );
        // A bare value without trailing slash works identically.
        assert_eq!(
            session_rel_path("conversations", &id, 1_754_654_400),
            "conversations/2025-08-08-deadbeef.md"
        );
    }
}
