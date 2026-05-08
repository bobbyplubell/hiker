//! JSONL audit log for every MCP tool call.
//
// status: mcp-audit-log-mcp-calls

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Per-day rotating JSONL writer at `<dir>/<YYYY-MM-DD>.jsonl`. One mutex
/// over the open file — calls are infrequent (one per agent tool call) so
/// no need for a buffered writer pool.
pub struct AuditLog {
    dir: PathBuf,
    log_full_input: bool,
    inner: Mutex<()>,
}

#[derive(Serialize)]
struct Entry<'a> {
    surface: &'a str,
    timestamp: String,
    tool: &'a str,
    /// Echoed back agent-friendly summary; full input redacted unless
    /// `log_full_input` is set.
    input: serde_json::Value,
    status: &'a str,
    /// Optional error message when `status == "error"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl AuditLog {
    pub fn new(dir: PathBuf, log_full_input: bool) -> Self {
        Self {
            dir,
            log_full_input,
            inner: Mutex::new(()),
        }
    }

    /// Append one row. Best-effort — logging failures don't propagate to
    /// the agent (the call result already succeeded or already failed for
    /// its own reasons; observability is a side concern).
    pub fn record(
        &self,
        tool: &str,
        input: &serde_json::Value,
        status: &str,
        error: Option<String>,
    ) {
        let now = OffsetDateTime::now_utc();
        let date = format!(
            "{:04}-{:02}-{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );
        let timestamp = now.format(&Rfc3339).unwrap_or_else(|_| "unknown".to_string());

        let masked_input = if self.log_full_input {
            input.clone()
        } else {
            redact_for_audit(input)
        };

        let entry = Entry {
            surface: "mcp-tool-call",
            timestamp,
            tool,
            input: masked_input,
            status,
            error,
        };
        let line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "mcp: audit serialize failed");
                return;
            }
        };

        let _guard = self.inner.lock().expect("audit mutex poisoned");
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            tracing::warn!(error = %e, dir = %self.dir.display(), "mcp: audit mkdir failed");
            return;
        }
        let path = self.dir.join(format!("{date}.jsonl"));
        let mut f = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "mcp: audit open failed");
                return;
            }
        };
        if let Err(e) = writeln!(f, "{line}") {
            tracing::warn!(error = %e, "mcp: audit write failed");
        }
    }
}

/// When `log_full_input` is off, redact obvious bulk fields. We keep
/// short identifying fields (`rel_path`, `top_k`, `query`'s length) so
/// debugging stays useful without persisting note bodies.
fn redact_for_audit(input: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = input else {
        return input.clone();
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    for (k, v) in map {
        let masked = match (k.as_str(), v) {
            ("content", serde_json::Value::String(s)) => {
                serde_json::json!({"redacted": true, "len": s.len()})
            }
            ("query", serde_json::Value::String(s)) => {
                serde_json::json!({"redacted": true, "len": s.len()})
            }
            ("fields", serde_json::Value::Object(_)) => {
                serde_json::json!({"redacted": true})
            }
            _ => v.clone(),
        };
        out.insert(k.clone(), masked);
    }
    serde_json::Value::Object(out)
}
