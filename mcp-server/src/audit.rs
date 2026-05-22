//! MCP-tool-call audit-log adapter. The on-disk JSONL writer lives in
//! `core::audit` (per `llm-audit-log`); this thin wrapper carries the
//! per-tool input-redaction policy (`[mcp.audit] log_full_input`) and
//! emits rows with `surface = "mcp-tool-call"` so MCP tool calls share
//! the same daily file as agent / LLM-direct calls.
//
// status: mcp-audit-log-mcp-calls

use std::sync::Arc;

use hiker_core::audit::{AgentLog, Entry};

pub struct Log {
    inner: Arc<AgentLog>,
    log_full_input: bool,
}

impl Log {
    pub const fn new(inner: Arc<AgentLog>, log_full_input: bool) -> Self {
        Self {
            inner,
            log_full_input,
        }
    }

    /// Append one row. Best-effort — logging failures don't propagate
    /// to the caller (the call result already succeeded or already
    /// failed for its own reasons; observability is a side concern).
    pub fn record(
        &self,
        tool: &str,
        input: &serde_json::Value,
        status: &str,
        error: Option<String>,
    ) {
        let masked = if self.log_full_input {
            input.clone()
        } else {
            self.redact_for_audit(input)
        };
        self.inner.record(&Entry {
            surface: "mcp-tool-call",
            feature: tool,
            status,
            error,
            turn_id: None,
            step_id: None,
            details: serde_json::json!({ "input": masked }),
        });
    }
}

impl Log {
    /// When `log_full_input` is off, redact obvious bulk fields. We
    /// keep short identifying fields (`rel_path`, `top_k`, `query`'s
    /// length) so debugging stays useful without persisting note
    /// bodies.
    fn redact_for_audit(&self, input: &serde_json::Value) -> serde_json::Value {
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
}
