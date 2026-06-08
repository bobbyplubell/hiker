//! The `check_diagram` tool — a stateless syntax check of a diagram source
//! (mermaid / wavedrom / latex) behind the shared `hiker-diagram` `check()`
//! seam. No vault access; lets an agent validate a diagram before it writes
//! the fenced block into a note.
//!
//! status: diagram-agent-check

use hiker_diagram::{Diagnostic, Severity};
use rmcp::model::{CallToolResult, ErrorData};

use super::App;
use crate::handler::params::{structured, CheckDiagram};

/// Map a [`Severity`] to its lowercase wire string.
const fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

/// Render one [`Diagnostic`] as the tool's JSON object shape.
fn diagnostic_json(d: &Diagnostic) -> serde_json::Value {
    let span = match &d.span {
        Some(r) => serde_json::json!({ "start": r.start, "end": r.end }),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "message": d.message,
        "severity": severity_str(d.severity),
        "span": span,
    })
}

impl App {
    /// status: diagram-agent-check
    /// Validate `p.src` as a `p.lang` diagram via
    /// [`hiker_core::diagrams::check_diagram`] and return
    /// `{ ok, diagnostics: [...] }`. `ok` is true iff there are no
    /// diagnostics. Stateless — no vault read, so it does not touch the
    /// per-session read set.
    pub(in crate::handler) fn run_check_diagram(
        &self,
        p: &CheckDiagram,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("check_diagram")?;
        let diags = hiker_core::diagrams::check_diagram(&p.lang, &p.src);
        let payload = serde_json::json!({
            "ok": diags.is_empty(),
            "diagnostics": diags.iter().map(diagnostic_json).collect::<Vec<_>>(),
        });
        Ok(structured(payload))
    }
}
