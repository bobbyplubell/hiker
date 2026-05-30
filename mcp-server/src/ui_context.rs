//! Live snapshot of editor-UI state surfaced to MCP read tools.
//!
//! status: mcp-tool-get-active-note
//! status: mcp-tool-get-open-notes
//! status: mcp-tool-get-selection
//!
//! `core` doesn't know about tabs or the active editor — that state lives
//! in the app process. The app re-publishes a small `Snapshot`
//! each frame; the MCP handlers read from it under a short RwLock and
//! never touch app-internal types.
//!
//! Mirrors the per-frame snapshot pattern `ui_cache.staging_snapshot` /
//! `ui_cache.pending_snapshot` uses: the app produces, the MCP read tools
//! consume. The lock is held for microseconds per call and the producer
//! cadence is the UI frame, so contention is a non-issue.

use std::sync::{Arc, RwLock};

/// One open buffer tab in the editor's tab strip, in visible order.
/// Non-buffer kinds (Home, Settings, Queue, Agent, Board, etc.) are
/// omitted by the producer; consumers don't see them at all.
#[derive(Debug, Clone, Default)]
pub struct OpenBufferTab {
    /// Vault-relative path of the buffer's source note.
    pub path: String,
    /// True when this tab is the currently focused tab. At most one entry
    /// in `open_tabs` carries `active: true`; zero is also valid (an
    /// app-page tab is active, or no tab is focused).
    pub active: bool,
}

/// Active buffer's selection/cursor state, when the focused tab is a
/// buffer tab. `None` when the focused tab is an app page (Home, Settings,
/// Queue, etc.) or no tab is focused.
#[derive(Debug, Clone, Default)]
pub struct ActiveBuffer {
    pub path: String,
    /// Primary-range head byte offset.
    pub cursor_byte: usize,
    /// `(start_byte, end_byte)` of the primary selection range when
    /// non-empty (head != anchor).
    pub selection: Option<(usize, usize)>,
}

/// Snapshot the app refreshes each frame; the MCP handlers read it under
/// a short read-lock.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Ordered (tab-strip visible order) list of open buffer tabs. Empty
    /// when only app-page tabs are open (or no tabs at all).
    pub open_tabs: Vec<OpenBufferTab>,
    /// `Some(_)` when the focused tab is a buffer tab; `None` otherwise.
    pub active_buffer: Option<ActiveBuffer>,
}

/// Shared handle. The app holds one, hands a clone into `McpDeps`, and
/// the MCP handler reads from it.
pub type Shared = Arc<RwLock<Snapshot>>;

pub fn shared_empty() -> Shared {
    Arc::new(RwLock::new(Snapshot::default()))
}
