//! Chat panel scaffolding. The session model
//! and on-disk markdown store come from `hiker_core::sessions`; this
//! module wraps them with the egui-side in-memory state, channel
//! plumbing for async assistant replies, and the renderer shared
//! between the full-tab `panels::agent` view and the discovery panel's
//! docked region.
//!
//! Scope: session list + create + switch + delete, message list with
//! markdown + wikilink rendering, tool-call cards with per-card
//! collapse, @-mention autocomplete (indexer-backed), @selection
//! insertion, active-note context injection, animated typing indicator,
//! Stop button driven by a per-session StopSignal, input box with
//! Cmd-Enter send. `send::dispatch_reply` wires `core::agent::run_turn`
//! against the live MCP handler when one is attached and a no-op
//! dispatcher otherwise; assistant deltas stream into the transcript
//! through an mpsc channel pumped each frame.

pub mod md_preview;
pub mod render;
pub mod send;
pub mod session;
pub mod sidebar;
pub mod state;

use eframe::egui;

use crate::activity::{Activity, Ctx, View};
use crate::icons;

/// Zero-sized `Activity` impl for the docked chat sidebar. Pure
/// descriptor: holds no state. The real state (the in-memory session
/// registry + lazy-discover gate) lives in `AppState::chat_state`; the
/// sidebar surface reaches it via `Ctx::state.downcast_mut::<State>()`
/// and routes broad effects (open a linked note, accept/reject a pending
/// op, the active-note send injection) through `Ctx::defer`.
///
/// Scope: only the docked secondary-side-bar chat region. The full-tab
/// agent conversation is a separate `TabKind::Agent` surface that still
/// renders against `&mut AppState` via `render::show_tab`.
pub struct Chat;

impl Activity for Chat {
    fn id(&self) -> &'static str {
        "chat"
    }
    fn label(&self) -> &'static str {
        "Chat"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Chat)
    }
    fn views(&self) -> Vec<&dyn View> {
        vec![&ChatSidebar]
    }
    /// Chat docks into the secondary (right) side bar. It renders through
    /// the same generic `SidePanelStack` path as the left activities, just
    /// on the right stack, and is summoned via the right-sidebar toggle
    /// rather than the left activity strip. [feature-consumer-activity-bar]
    fn default_location(&self) -> egui_workbench::side_bar::Location {
        egui_workbench::side_bar::Location::RightBar
    }
}

struct ChatSidebar;

impl View for ChatSidebar {
    fn id(&self) -> &'static str {
        "chat"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        sidebar::render_sidebar(ui, ctx);
    }
}

