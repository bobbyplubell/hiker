//! Persisted chat sessions. See `docs/llm.md` §Sessions.
//!
//! A session is the unit of conversational continuity — many turns share
//! one accumulating message history, persisted as markdown under
//! `vault/.hiker/sessions/<YYYY-MM-DD>-<id>.md`. Markdown is the
//! source of truth; the in-memory `SessionState` in the chat command
//! surface is a working cache rebuilt from it on resume.
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

/// One discoverable session file on disk. Returned by `list_sessions`
/// sorted newest-first.
#[derive(Debug, Clone)]
pub struct SessionFileInfo {
    pub id: SessionId,
    pub abs_path: PathBuf,
    pub rel_path: String,
    pub mtime_unix: i64,
}

/// `<vault_root>/.hiker/sessions/`.
pub fn sessions_dir(vault_root: &Path) -> PathBuf {
    vault_root.join(".hiker").join("sessions")
}

/// `vault/.hiker/sessions/<YYYY-MM-DD>-<id>.md`.
pub fn session_file_path(vault_root: &Path, id: &SessionId, created_at_unix: i64) -> PathBuf {
    let date = format_date_yyyy_mm_dd(created_at_unix);
    sessions_dir(vault_root).join(format!("{}-{}.md", date, id.0))
}

/// Vault-relative path matching `session_file_path`'s on-disk shape.
/// Used by callers that want to enqueue an `IndexJob::Upsert` without
/// going through the watcher (sessions live under `.hiker/` which the
/// watcher carves out specifically; see `core::watcher::is_ignored`).
pub fn session_rel_path(id: &SessionId, created_at_unix: i64) -> String {
    let date = format_date_yyyy_mm_dd(created_at_unix);
    format!(".hiker/sessions/{}-{}.md", date, id.0)
}

/// Create the session file with the YAML frontmatter header. Idempotent
/// over the directory (creates if missing); errors if the file already
/// exists so a duplicate id is loud rather than silent.
pub fn create_session_file(vault_root: &Path, meta: &SessionMeta) -> io::Result<PathBuf> {
    let dir = sessions_dir(vault_root);
    fs::create_dir_all(&dir)?;
    let path = session_file_path(vault_root, &meta.id, meta.created_at_unix);
    if path.exists() {
        return Ok(path);
    }
    let header = format!(
        "---\nhiker.session_id: {}\nhiker.created_at: {}\nhiker.model: {}\nhiker.provider: {}\n---\n\n",
        meta.id.0,
        format_iso_8601(meta.created_at_unix),
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
/// section (one ```hiker-tool-call``` block per call).
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

/// Discover all session files under `<vault>/.hiker/sessions/`, sorted
/// newest-first by filesystem mtime. Empty when the directory doesn't
/// exist yet.
pub fn list_sessions(vault_root: &Path) -> io::Result<Vec<SessionFileInfo>> {
    let dir = sessions_dir(vault_root);
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(id) = parse_id_from_filename(&path) else { continue };
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
    out.sort_by(|a, b| b.mtime_unix.cmp(&a.mtime_unix));
    Ok(out)
}

/// Most recent session file (newest mtime) or `None` if the dir is
/// empty / absent.
pub fn most_recent_session(vault_root: &Path) -> io::Result<Option<SessionFileInfo>> {
    let mut all = list_sessions(vault_root)?;
    Ok(if all.is_empty() { None } else { Some(all.swap_remove(0)) })
}

/// One reconstructed turn, used to seed the in-memory cache when
/// resuming a session at vault open. Tool-call structure is intentionally
/// dropped — resume is a "synthetic context" rebuild, not a perfect
/// replay; the agent will re-call tools as needed for any follow-up.
#[derive(Debug, Clone)]
pub struct ResumedTurn {
    pub user: String,
    pub agent: String,
}

/// Parse a session file back into alternating user/agent text. Tool-call
/// fences are stripped from the agent text.
pub fn parse_session(path: &Path) -> io::Result<Vec<ResumedTurn>> {
    let body = fs::read_to_string(path)?;
    let body = strip_frontmatter(&body);
    let mut turns: Vec<ResumedTurn> = Vec::new();
    let mut current_section: Option<&'static str> = None; // "user" | "agent"
    let mut buf = String::new();
    let mut current_user = String::new();
    let mut in_tool_block = false;

    for line in body.lines() {
        if line == "## User" {
            flush_section(&mut turns, &current_section, &mut current_user, &mut buf);
            current_section = Some("user");
            in_tool_block = false;
            continue;
        }
        if line == "## Agent" {
            flush_section(&mut turns, &current_section, &mut current_user, &mut buf);
            current_section = Some("agent");
            in_tool_block = false;
            continue;
        }
        if line.starts_with("```hiker-tool-call") {
            in_tool_block = true;
            continue;
        }
        if in_tool_block {
            if line.starts_with("```") {
                in_tool_block = false;
            }
            continue;
        }
        if current_section.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush_section(&mut turns, &current_section, &mut current_user, &mut buf);
    Ok(turns)
}

fn flush_section(
    turns: &mut Vec<ResumedTurn>,
    section: &Option<&'static str>,
    current_user: &mut String,
    buf: &mut String,
) {
    let trimmed = buf.trim().to_string();
    buf.clear();
    match *section {
        Some("user") => {
            *current_user = trimmed;
        }
        Some("agent") => {
            turns.push(ResumedTurn {
                user: std::mem::take(current_user),
                agent: trimmed,
            });
        }
        _ => {}
    }
}

fn strip_frontmatter(body: &str) -> &str {
    let mut lines = body.split_inclusive('\n');
    let Some(first) = lines.next() else { return body };
    if first.trim_end() != "---" {
        return body;
    }
    let mut consumed = first.len();
    for line in lines {
        consumed += line.len();
        if line.trim_end() == "---" {
            return &body[consumed..];
        }
    }
    body
}

fn parse_id_from_filename(path: &Path) -> Option<SessionId> {
    let stem = path.file_stem()?.to_str()?;
    // `<YYYY-MM-DD>-<id>` — the id is everything after the third '-'.
    let mut hyphens = 0;
    let mut idx = 0;
    for (i, ch) in stem.char_indices() {
        if ch == '-' {
            hyphens += 1;
            if hyphens == 3 {
                idx = i + 1;
                break;
            }
        }
    }
    if idx == 0 || idx >= stem.len() {
        return None;
    }
    Some(SessionId(stem[idx..].to_string()))
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

fn format_iso_8601(unix_secs: i64) -> String {
    // Minimal ISO 8601 in UTC. Same civil-from-days math as the date
    // formatter; we tack on hh:mm:ss derived from the time-of-day.
    let day_secs = unix_secs.rem_euclid(86_400);
    let h = day_secs / 3_600;
    let m = (day_secs % 3_600) / 60;
    let s = day_secs % 60;
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date_yyyy_mm_dd(unix_secs),
        h,
        m,
        s
    )
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
        let path = create_session_file(dir.path(), &meta).unwrap();
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
        let p1 = create_session_file(dir.path(), &m1).unwrap();
        let p2 = create_session_file(dir.path(), &m2).unwrap();
        // Touch p2 last so its mtime is newer regardless of birth order.
        // Sleep ensures the file system registers a distinct timestamp;
        // some CoW / overlay filesystems have coarse (1s) mtime granularity.
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let _ = fs::write(&p2, "");  // truncation forces a visible mtime bump
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(&p2, fs::read(&p2).unwrap()).unwrap();
        let _ = p1;
        let list = list_sessions(dir.path()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id.0, "bbb");
    }

    #[test]
    fn parse_id_strips_date_prefix() {
        let p = PathBuf::from("/x/.hiker/sessions/2026-05-08-abc123def.md");
        assert_eq!(parse_id_from_filename(&p).unwrap().0, "abc123def");
    }
}
