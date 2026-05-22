//! JSONL agent log shared across every LLM-driven surface. See
//! `docs/llm.md` §"Audit log".
//!
//! One row per LLM call (any module, any feature type) appended to
//! `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl`. Daily rotation; the
//! file is opened on each `record` call (calls are infrequent — at most
//! one per agent step, one per fan-out item, one per background save).
//!
//! Surfaces are spec-defined: `core::llm` (background / fan-out direct
//! callers), `core::agent` (basic agent loop turns), `core::acp`
//! (external ACP agent), `mcp-tool-call` (MCP tool dispatch — the
//! `mcp-audit-log-mcp-calls` slug). The shared writer means a debugging
//! trail can correlate panel events with audit rows by `turn_id` /
//! `step_id`, and there's exactly one place to add a new feature type.
//!
//! Full prompt/response text is gated on `[llm.audit] log_full_prompt =
//! true` (default off — `obs-no-content` discipline). Callers that
//! carry redaction logic (the MCP wrapper) consult the toggle and pass
//! redacted `details` when off.
//
// status: llm-audit-log

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Per-day rotating JSONL writer at `<dir>/<YYYY-MM-DD>.jsonl`. One mutex
/// over the writer so concurrent `record` calls serialize cleanly.
pub struct AgentLog {
    dir: PathBuf,
    /// Mirror of `[llm.audit] log_full_prompt`. Callers that have
    /// content to redact (e.g. MCP tool inputs, `core::llm` prompts)
    /// consult this before populating `details`. The audit log itself
    /// is content-blind once `record` is called — whatever's in
    /// `details` gets written.
    log_full_content: bool,
    inner: Mutex<()>,
}

/// One audit row. Callers fill in the surface / feature / status fields
/// and any free-form `details` (tool call counts, finish reasons,
/// redacted-or-not input, etc.). Empty optionals are skipped on the
/// wire so daily files stay readable.
#[derive(Serialize)]
pub struct Entry<'a> {
    /// Spec-defined surface: `core::llm`, `core::agent`, `core::acp`,
    /// or `mcp-tool-call`. New surfaces should be added to this doc
    /// comment when they land.
    pub surface: &'a str,
    /// Feature slug or tool name. For chat turns, the prompt feature
    /// (e.g. `chat_system`); for MCP, the tool name; for fan-out, the
    /// fan-out's slug.
    pub feature: &'a str,
    /// `ok` / `error` / `stale_prompt` / `cancelled` / etc. Stable set
    /// per surface; the audit log doesn't enumerate them.
    pub status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Discriminator for the agent path so panel events and audit rows
    /// can be correlated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<u32>,
    /// Free-form per-call payload. Already redacted by the caller when
    /// `log_full_content` is off and the field would carry user text.
    #[serde(skip_serializing_if = "is_null", default)]
    pub details: serde_json::Value,
}

fn is_null(v: &serde_json::Value) -> bool {
    v.is_null()
}

impl AgentLog {
    pub const fn new(dir: PathBuf, log_full_content: bool) -> Self {
        Self {
            dir,
            log_full_content,
            inner: Mutex::new(()),
        }
    }

    /// Whether the writer is configured to keep full prompt / response
    /// text. Callers consult this before stuffing a body into `details`.
    pub const fn log_full_content(&self) -> bool {
        self.log_full_content
    }

    /// Directory the per-day file lives in. Useful for tests + the
    /// future audit-log viewer.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one row. Best-effort — logging failures don't propagate
    /// to the caller, since the underlying call already succeeded or
    /// already failed for its own reasons; observability is a side
    /// concern.
    pub fn record(&self, entry: &Entry<'_>) {
        let now = OffsetDateTime::now_utc();
        let date = format!(
            "{:04}-{:02}-{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );
        let timestamp = now.format(&Rfc3339).unwrap_or_else(|_| "unknown".to_string());
        let Some(line) = self.serialize_line(timestamp, entry) else {
            return;
        };
        self.append_line(&date, &line);
    }

    /// Wrap an entry in a small envelope (so the timestamp lands first
    /// in the JSON object — readability concern when grep'ing files)
    /// and serialise. Returns `None` and logs a warning on failure so
    /// `record` can early-return without expanding its branch budget.
    fn serialize_line(&self, timestamp: String, entry: &Entry<'_>) -> Option<String> {
        #[derive(Serialize)]
        struct Envelope<'a> {
            timestamp: String,
            #[serde(flatten)]
            entry: &'a Entry<'a>,
        }
        let envelope = Envelope { timestamp, entry };
        match serde_json::to_string(&envelope) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "audit: serialize failed");
                None
            }
        }
    }

    /// Append `line` (already JSON-encoded, no trailing newline) to
    /// `<dir>/<date>.jsonl`, creating the directory if needed.
    /// Best-effort: any IO failure is logged and swallowed.
    fn append_line(&self, date: &str, line: &str) {
        let _guard = self.inner.lock().expect("audit mutex poisoned");
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(error = %e, dir = %self.dir.display(), "audit: mkdir failed");
            return;
        }
        let path = self.dir.join(format!("{date}.jsonl"));
        let mut f = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "audit: open failed");
                return;
            }
        };
        if let Err(e) = writeln!(f, "{line}") {
            tracing::warn!(error = %e, "audit: write failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn read_today(dir: &Path) -> String {
        let now = OffsetDateTime::now_utc();
        let date = format!(
            "{:04}-{:02}-{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );
        std::fs::read_to_string(dir.join(format!("{date}.jsonl"))).unwrap()
    }

    #[test]
    fn record_writes_jsonl_with_expected_fields() {
        let dir = tempdir().unwrap();
        let log = AgentLog::new(dir.path().to_path_buf(), false);
        log.record(&Entry {
            surface: "core::agent",
            feature: "chat_system",
            status: "ok",
            error: None,
            turn_id: Some("t1"),
            step_id: Some(0),
            details: serde_json::json!({"tool_calls": 1}),
        });
        let body = read_today(dir.path());
        assert!(body.contains("\"surface\":\"core::agent\""));
        assert!(body.contains("\"feature\":\"chat_system\""));
        assert!(body.contains("\"status\":\"ok\""));
        assert!(body.contains("\"turn_id\":\"t1\""));
        assert!(body.contains("\"step_id\":0"));
        assert!(body.contains("\"tool_calls\":1"));
        // Empty optionals omitted.
        assert!(!body.contains("\"error\""));
        // Lines terminated with newline.
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn record_appends_multiple_rows() {
        let dir = tempdir().unwrap();
        let log = AgentLog::new(dir.path().to_path_buf(), false);
        for i in 0..3 {
            log.record(&Entry {
                surface: "core::agent",
                feature: "chat_system",
                status: "ok",
                error: None,
                turn_id: Some("t1"),
                step_id: Some(i),
                details: serde_json::Value::Null,
            });
        }
        let body = read_today(dir.path());
        assert_eq!(body.lines().count(), 3);
    }
}
