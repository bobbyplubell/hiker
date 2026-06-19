//! UI-context read tools — `get_active_note` / `get_open_notes` /
//! `get_selection`. Read-only; surface the live UI snapshot
//! ([`crate::ui_context::Snapshot`]) so an attached agent can see
//! what the user is currently looking at without a round-trip through
//! `get_note` / out-of-band channels.
//!
//! status: mcp-tool-get-active-note
//! status: mcp-tool-get-open-notes
//! status: mcp-tool-get-selection

use rmcp::model::{CallToolResult, ErrorData};

use super::App;
use crate::handler::params::{
    structured, translate_hiker_err, GetActiveNote, GetOpenNotes, GetSelection,
};

impl App {
    /// status: mcp-tool-get-active-note
    /// Returns the focused buffer tab's path + cursor byte offset +
    /// (if non-empty) selection. When the active tab is an app page
    /// (Home / Settings / Queue / etc.) returns `{ path: null }`.
    /// Read-only — does NOT populate the per-session read set
    /// (`mcp-read-before-write`). Only `get_note` does.
    pub(in crate::handler) async fn read_active_note(
        &self,
        _p: &GetActiveNote,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("get_active_note")?;
        let snap = self
            .state
            .ui_context
            .read()
            .map_err(|_| ErrorData::internal_error("ui_context lock poisoned", None))?;
        let payload = match &snap.active_buffer {
            Some(ab) => {
                let selection = ab
                    .selection
                    .map(|(s, e)| serde_json::json!({"start_byte": s, "end_byte": e}));
                serde_json::json!({
                    "path": ab.path,
                    "cursor_byte": ab.cursor_byte,
                    "selection": selection,
                })
            }
            None => serde_json::json!({"path": serde_json::Value::Null}),
        };
        Ok(structured(payload))
    }

    /// status: mcp-tool-get-open-notes
    /// Returns the ordered list of currently-open buffer tabs as
    /// `[{path, active}]`. Non-buffer kinds (Home / Settings / Queue /
    /// Board / Agent / etc.) are omitted by the producer. Read-only.
    pub(in crate::handler) async fn read_open_notes(
        &self,
        _p: &GetOpenNotes,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("get_open_notes")?;
        let snap = self
            .state
            .ui_context
            .read()
            .map_err(|_| ErrorData::internal_error("ui_context lock poisoned", None))?;
        let tabs: Vec<serde_json::Value> = snap
            .open_tabs
            .iter()
            .map(|t| serde_json::json!({"path": t.path, "active": t.active}))
            .collect();
        Ok(structured(serde_json::Value::Array(tabs)))
    }

    /// status: mcp-tool-get-selection
    /// Returns `{path, start_byte, end_byte, text}` for the active
    /// buffer's non-empty selection; `{path: null}` when empty or no
    /// buffer is focused. The selection text is materialised from the
    /// agent's view of the file (layered-doc replica when present, on-disk
    /// bytes otherwise) — same source as `get_note(detail=full)` — so
    /// the byte range and text are derived from one consistent snapshot.
    /// Read-only.
    pub(in crate::handler) async fn read_selection(
        &self,
        _p: &GetSelection,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("get_selection")?;
        let (path, range) = {
            let snap = self
                .state
                .ui_context
                .read()
                .map_err(|_| ErrorData::internal_error("ui_context lock poisoned", None))?;
            match &snap.active_buffer {
                Some(ab) => match ab.selection {
                    Some(r) => (ab.path.clone(), Some(r)),
                    None => (String::new(), None),
                },
                None => (String::new(), None),
            }
        };
        let Some((start, end)) = range else {
            return Ok(structured(serde_json::json!({
                "path": serde_json::Value::Null,
            })));
        };
        // Materialize the buffer text from the agent's own view (layered-doc
        // replica) so the slice matches what `get_note` would return.
        // Fall back to on-disk bytes when there is no layered-doc doc yet.
        let content = match self.agent_view_content(&path)? {
            Some((text, _)) => text,
            None => self
                .state
                .vault
                .read_file(&path)
                .map_err(translate_hiker_err)?,
        };
        // Clamp + char-boundary-snap the range to the materialized text so
        // a stale snapshot (cursor recorded against an older view, doc
        // shorter now) can't panic on a slice.
        let len = content.len();
        let (start, end) = (start.min(len), end.min(len));
        let start = snap_boundary(&content, start);
        let end = snap_boundary(&content, end);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let text = content[start..end].to_string();
        Ok(structured(serde_json::json!({
            "path": path,
            "start_byte": start,
            "end_byte": end,
            "text": text,
        })))
    }
}

/// Snap `byte` down to the nearest UTF-8 char boundary in `s`. Used by
/// `get_selection` to defensively clamp a stale UI-snapshot byte offset
/// against the freshly-materialised content before slicing — otherwise a
/// mid-codepoint offset would panic the handler.
const fn snap_boundary(s: &str, byte: usize) -> usize {
    if byte >= s.len() {
        return s.len();
    }
    let mut b = byte;
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}
