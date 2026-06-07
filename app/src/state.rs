//! Top-level app state — this struct *is* the state.
//!
//! `AppState` used to be a 200+ field god struct. It's now a small
//! holder that delegates field ownership to typed compartments:
//!
//! - `VaultSession`: per-vault lifecycle (`Vault`, `Config`, services,
//!   long-lived channels + a `CancellationToken` for spawned tasks).
//!   Replaced atomically on vault switch — the old session's
//!   `cancel.cancel()` is called before the new session lands so the
//!   background tasks (watcher relay, indexer forwarder, direct LLM
//!   worker) shut down cleanly.
//! - `Session`: editor session (tabs, buffers, modal, nav, sidebar,
//!   trails, chat). Conceptually survives a vault swap; in practice
//!   it's rebuilt because everything ties back to vault paths.
//! - `UiCache`: per-frame snapshots (task / pending / skipped paths) so
//!   the render loop reads cheap caches instead of issuing SQLite
//!   round-trips from every panel.
//! - `PanelStates`: per-panel local UI state (discovery, clusters,
//!   trails, preview buffers, graph, cluster-graph layouts).
//! - `UiState`: window-level UI flags (sidebar widths, custom titlebar,
//!   help, profiler overlay).
//!
//! Callers access fields as `state.<compartment>.<field>`. Helper
//! methods (`next_tab_id`, `tab_by_id`, `push_toast`, `set_setting`)
//! live on `AppState` as thin forwarders so the most common call-site
//! patterns still read cleanly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use hiker_core::vault::Vault;
use hiker_core::activity::Activity;
use hiker_core::audit::AgentLog;
use hiker_core::autosave::Autosave;
use hiker_core::config::Config;
use hiker_core::indexer::Handle;
use hiker_core::store::Store;
use hiker_core::tasks::queue::Queue as TaskQueue;
use hiker_core::tasks::types::TaskRecord;
use hiker_core::trees::types::Db;
use hiker_core::watcher::{FileEvent, Watcher};

use crate::buffer::Buffer;
use crate::tab::{Tab, TabId, TabKind};

// ===========================================================================
// AppState — top-level
// ===========================================================================

pub struct AppState {
    pub vault_session: VaultSession,
    pub session: Session,
    /// File-tree UI state (expanded dirs, dir listing cache, selection,
    /// inline-rename draft, reveal scroll target). Relocated off
    /// `Session::file_tree` so the `files` activity surface can reach it
    /// through the registry `SurfaceCtx.state` slot, matching the other
    /// migrated activities. [feature-filetree-migration]
    pub file_tree_state: FileTreeState,
    pub ui_cache: UiCache,
    pub panels: PanelStates,
    /// Per-activity UI state for the migrated `clusters` activity.
    /// Top-level (sibling to `panels`) per `feature-state-ownership`:
    /// each migrated activity owns its state on `AppState` directly so
    /// `PanelStates` shrinks rather than growing into a god struct.
    pub clusters_state: crate::clusters::state::State,
    /// Per-activity UI state for the migrated `trails` activity
    /// (`feature-trails-migration`).
    pub trails_state: crate::trails::state::State,
    /// Per-activity UI state for the migrated `backlinks` activity
    /// (`feature-backlinks-migration`).
    pub backlinks_state: crate::backlinks::State,
    /// Per-activity UI state for the `appears-in` view — cached reverse
    /// references (canvases / boards / trails / trees). status: canvas-appears-in
    pub appears_in_state: crate::appears_in::State,
    /// Per-activity UI state for the migrated `related` activity
    /// (`feature-related-migration`).
    pub related_state: crate::related::State,
    /// Per-activity UI state for the migrated `search` activity
    /// (`feature-search-migration`).
    pub search_state: crate::search::state::State,
    /// Per-activity UI state for the migrated `vault` lens activity
    /// (chosen lens + collapsed groups; read-only, nothing persisted on
    /// notes). status: vault-view-mode
    pub vault_state: crate::vault_view::State,
    /// Per-activity UI state for the migrated `trash` activity. The panel
    /// is effectively stateless (listing read fresh from disk), but the
    /// registry hands every activity a state slice, so this is a
    /// zero-field marker. status: feature-trash-panel
    pub trash_state: crate::trash::State,
    /// Per-activity UI state for the `canvases` activity (lists the
    /// vault's `.canvas` files). Effectively stateless — the listing is
    /// read fresh from disk — so a zero-field marker keeps the registry
    /// `AppCtx::session` seam uniform. status: feature-state-ownership
    pub canvases_activity_state: crate::canvas_activity::State,
    /// Per-activity state for the migrated docked `chat` sidebar: the
    /// in-memory session registry + the lazy-discover gate. Relocated
    /// off `Session::chat` / `Session::chat_discovered`.
    pub chat_state: crate::chat::state::State,
    /// Per-session activity descriptor registry. Built in
    /// `bootstrap::open_vault` from `activity::builtin_activities()` plus
    /// (in Phase 3) plugin-derived activities. Sidebar/activity/hamburger
    /// consumers iterate this rather than hardcoding activity lists.
    /// `feature-registry`.
    pub activities: std::sync::Arc<crate::activity::ActivityRegistry>,
    pub ui: UiState,
    pub toasts: Vec<Toast>,
    /// Deferred-effect sink for activity surfaces. A `SurfaceCtx` borrows
    /// this as its `effects` field; each consumer drains it (running each
    /// closure with full `&mut AppState`) right after the surface
    /// returns. Lives on `AppState` so the narrow `SurfaceCtx` borrow can
    /// reach it disjointly from the other fields. See `activity::SurfaceCtx`.
    pub pending_effects: Vec<crate::activity::Effect>,
    /// What the sync engine last surfaced as needing the user — the blocked-doc
    /// paths, whether a content-key change is held, and whether a last-error is
    /// present. The update loop diffs the live snapshot against this each frame
    /// and fires a toast ONLY on a new item appearing (a transition), so a
    /// silent no-op round never spams. Reset on vault swap (fresh `AppState`).
    /// status: sync-attention-badge
    pub sync_attention_seen: SyncAttentionSeen,
    pub vault_switch: VaultSwitchState,
    /// IDE-style layout host. Wraps the editor tabs + side bars +
    /// activity bar + status bar. Kept on the top-level state so its
    /// borrow is disjoint from `session.tabs` / `session.buffers`,
    /// which the workbench's pane renderers read mutably each frame.
    pub workbench: egui_workbench::workspace::Workbench<
        crate::workbench_host::HikerWbTab,
        String,
    >,
}

// ===========================================================================
// VaultSwitchState — async vault-open lifecycle
// ===========================================================================

/// State machine for the runtime "open a different vault" flow.
///
/// - `Idle`: nothing pending.
/// - `Picking`: a folder picker is open on the tokio runtime. We poll the
///   oneshot each frame for the user's choice; this keeps the dialog off
///   the egui/winit thread so the UI never freezes while it's up (the
///   native `rfd` portal call is synchronous and would otherwise block
///   every repaint for the dialog's whole lifetime).
/// - `Requested(path)`: a UI action (picker result / confirm modal) queued
///   a path. The next `update()` frame transitions to `InProgress` by
///   spawning `bootstrap::open_vault` on the tokio runtime.
/// - `InProgress`: bootstrap is running on a tokio task; the UI keeps
///   rendering against the OLD vault while we poll a oneshot each frame.
///
/// If a second request lands while `InProgress`, we drop the receiver
/// (which causes the in-flight task to discard its result; the dropped
/// `AppState` then fires its own `cancel.cancel()` via `VaultSession`'s
/// Drop), then start the new request.
#[derive(Default)]
pub enum VaultSwitchState {
    #[default]
    Idle,
    Picking(oneshot::Receiver<Option<PathBuf>>),
    Requested(PathBuf),
    InProgress {
        rx: oneshot::Receiver<anyhow::Result<AppState>>,
        path: PathBuf,
    },
}

// ===========================================================================
// VaultSession — per-vault lifecycle
// ===========================================================================

pub struct VaultSession {
    pub vault: Arc<Vault>,
    pub vault_root: PathBuf,
    pub config: Arc<RwLock<Config>>,
    pub services: Services,
    pub events: VaultEvents,
    /// Cancellation token shared with every background task spawned for
    /// this vault (watcher relay, indexer progress forwarder, direct
    /// LLM worker). On vault swap the update loop calls
    /// `cancel.cancel()` before the new session lands so those tasks
    /// stop relaying into the now-stale state.
    pub cancel: CancellationToken,
}

pub struct Services {
    pub read_store: Arc<Mutex<Store>>,
    /// The vault's op log: the text write substrate every producer
    /// rides on (`op-log-ops-producer-helpers`). User saves and agent edits
    /// route through `core::ops::op_writes` against this handle. Seeded from the
    /// on-disk vault at open by `core::ops::op_writes::bootstrap`.
    pub oplog: Arc<hiker_core::oplog::OpLog>,
    pub trees: Arc<Db>,
    pub activity: Arc<Activity>,
    pub autosave: Arc<Autosave>,
    pub watcher: Arc<Watcher>,
    pub indexer: Arc<Handle>,
    // TODO: surface in the audit/agent-log UI panel.
    #[allow(dead_code)]
    pub audit: Arc<AgentLog>,
    pub tasks: Arc<TaskQueue>,
    pub mcp: Option<Arc<hiker_mcp::McpServerHandle>>,
    /// Shared `[mcp.tools]` snapshot the MCP handler reads at dispatch
    /// time. Created at vault open and mirrored by `set_setting` on
    /// every config change so settings-panel toggles (review_required,
    /// per-tool gates) take effect without an MCP restart. Always
    /// present even when MCP is disabled — keeps the wiring uniform.
    pub mcp_tools_cfg: Arc<std::sync::RwLock<hiker_core::config::sections::McpToolsConfig>>,
    /// status: mcp-tool-get-active-note, mcp-tool-get-open-notes,
    /// mcp-tool-get-selection
    ///
    /// Live snapshot of "what the user is looking at" the MCP UI-context
    /// read tools surface. The host writes it each frame via
    /// `refresh_ui_context_snapshot`; the MCP handler only reads.
    pub mcp_ui_context: hiker_mcp::ui_context::Shared,
    /// The live `hiker-sync` engine, present only when `[sync].enabled`. When
    /// sync is off this is `None` and nothing is constructed (no keys, no
    /// swarm, no listener). The Sync page renders a disabled state in that
    /// case. Wrapped in `Arc` so the page can clone a handle to spawn async
    /// `force_sync` / `discover` work off the frame loop.
    pub sync: Option<Arc<crate::sync_service::SyncService>>,
    /// The live git transport engine (`git.md`), present only when `[sync]
    /// .enabled` and `[sync].transport = "git"`. Mutually exclusive with the
    /// libp2p `sync` engine above by the single-bidirectional rule
    /// (`sync-single-bidirectional-transport`) — at most one of `sync` /
    /// `git_sync` is `Some`. The save site pokes whichever is present.
    pub git_sync: Option<Arc<crate::git_sync::GitSyncEngine>>,
}

pub struct VaultEvents {
    pub fs_events: Mutex<UnboundedReceiver<FileEvent>>,
    pub indexer_events_rx: Mutex<UnboundedReceiver<String>>,
    pub mutation_events: Mutex<UnboundedReceiver<MutationEvent>>,
    pub mutation_events_tx: UnboundedSender<MutationEvent>,
    /// Bounded ring buffer drained from `indexer_events_rx` each frame
    /// (capped at `INDEXER_EVENTS_MAX`).
    pub indexer_events: VecDeque<String>,
    /// Receiver for human-readable sync progress lines pushed by the sync
    /// service and its background tasks. Drained each frame into
    /// `sync_events`. Present even when sync is disabled (the channel just
    /// stays empty) so the wiring is uniform.
    pub sync_events_rx: Mutex<UnboundedReceiver<String>>,
    /// Bounded ring buffer drained from `sync_events_rx` each frame (capped at
    /// `SYNC_EVENTS_MAX`). Backs the Sync page's progress log.
    pub sync_events: VecDeque<String>,
    /// Receiver for on-demand fork-diff fetch results pushed by the sync
    /// service's `fetch_fork_diff` task: `(path, Ok(their_text) | Err(message))`.
    /// Drained each frame into `panels.sync.fork_diffs` (the Sync page's
    /// "view diff" cache), mirroring the `sync_events` relay. Present even when
    /// sync is disabled (the channel just stays empty). [sync-fork-diff]
    pub fork_diff_rx: Mutex<UnboundedReceiver<crate::sync_service::ForkDiffResult>>,
    /// Latest task-queue snapshot pushed by the background pollster
    /// (`bootstrap::spawn_snapshot_poller`). The UI thread `.borrow()`s
    /// this each frame instead of calling `tasks.snapshot().await` from
    /// the render loop. Initial value is an empty `Vec`.
    pub task_snapshot_rx: watch::Receiver<Vec<TaskRecord>>,
    /// Latest skipped-paths snapshot pushed by the background pollster
    /// (every 3s). The file-tree row renderer reads from
    /// `ui_cache.skipped_paths` which is populated from this channel each
    /// frame; the read-store mutex never gets locked on the UI thread.
    pub skipped_paths_rx: watch::Receiver<HashSet<String>>,
}

impl Drop for VaultSession {
    fn drop(&mut self) {
        // Belt-and-braces: vault-swap path should `cancel.cancel()` before
        // swapping, but if anyone forgets we still shut tasks down on drop.
        self.cancel.cancel();
    }
}

// ===========================================================================
// Session — editor session
// ===========================================================================

pub struct Session {
    pub buffers: HashMap<String, Buffer>,
    pub tabs: Vec<Tab>,
    /// Last-persisted primary side-panel accordion snapshot. The
    /// autosave tick compares the live arrangement against this and
    /// rewrites `<vault>/.hiker/side-panel.json` only on change (the
    /// accordion mutates inside egui_workbench, so there's no dirty flag
    /// to hang off). [feature-multi-region-sidebar]
    pub side_panel_saved: Option<crate::side_panel_persist::SidePanelState>,
    pub active_tab: Option<TabId>,
    /// VSCode-style preview slot — at most one tab is "preview" (italic
    /// label, replaced by the next click).
    pub preview_tab: Option<TabId>,
    pub next_tab_id: u64,
    pub modal: Option<Modal>,
    pub nav: NavState,
    /// Last time autosave ticked.
    pub last_autosave_tick: Instant,
    /// Paths with a `NoteMutation` task we just submitted. Gates the
    /// wand-icon menu so concurrent mutations don't pile up.
    pub pending_mutations: HashSet<String>,
    /// Persisted canvas view state (camera pan/zoom + per-card scroll/zoom),
    /// keyed by canvas vault-relative path. The single source that survives a
    /// canvas tab close (the ephemeral `Pane` is dropped on close) and feeds the
    /// tab-state persistence on exit; restored on startup and applied to each
    /// pane on first creation. status: canvas-view-state-persist
    pub canvas_views: HashMap<String, hiker_core::autosave::CanvasViewState>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            buffers: HashMap::new(),
            tabs: Vec::new(),
            side_panel_saved: None,
            active_tab: None,
            preview_tab: None,
            next_tab_id: 1,
            modal: None,
            nav: NavState::default(),
            last_autosave_tick: Instant::now(),
            pending_mutations: HashSet::new(),
            canvas_views: HashMap::new(),
        }
    }
}

/// One entry in the back/forward navigation stack — what a Back/Forward press
/// restores into the active editor view. Path-only nav couldn't represent a
/// historical version (a `(path, op_id)` pair), so navigating to a version
/// dropped out of the stack and Back couldn't return; modelling the target
/// explicitly fixes that and keeps the stack logic unit-testable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavTarget {
    /// The live vault buffer for a path.
    File(String),
    /// A historical version of `path` at a specific accepted op.
    HistoryVersion { path: String, op_id: String },
    /// An article in an open `.zim` archive (`None` = the archive's main
    /// page). Lets the global Back/Forward stack walk in-archive link history
    /// the same way it walks notes. [zim-nav-stack]
    ZimArticle { zim_path: String, article: Option<String> },
}

#[derive(Default)]
pub struct NavState {
    /// Chronological history of visited targets. `idx` points at the current
    /// position; Back/Forward shift it. Pushing a new target while `idx` is
    /// not at the end drops the forward tail (a new branch). The stack
    /// mechanics live in the methods below so they can be tested without an
    /// `AppState`.
    pub history: Vec<NavTarget>,
    pub idx: Option<usize>,
    /// True while driving open_file from a back/forward action — so
    /// open_file doesn't push a new entry on top of the one we're
    /// navigating to.
    pub locked: bool,
    pub swipe_accum_x: f32,
    pub swipe_cooldown_until: Option<Instant>,
    pub swipe_last_activity: Option<Instant>,
    pub swipe_last_commit_dir: Option<i8>,
    pub swipe_armed_dir: Option<i8>,
    pub swipe_skip_rects: Vec<eframe::egui::Rect>,
}

// ===========================================================================
// UiCache — per-frame snapshots
// ===========================================================================

#[derive(Default)]
pub struct UiCache {
    pub task_snapshot: Vec<TaskRecord>,
    /// Per-frame snapshot of the vault's pending agent ops, read off the op
    /// log (`op_writes::list_pending_proposals`). Drives the pending-count
    /// badge (toolbar / status bar / Patch-review tab) and the chat-card
    /// "still-live" op-id set. Populated in `main::refresh_pending_proposals`.
    pub pending_snapshot: Vec<hiker_core::ops::op_writes::PendingProposal>,
    /// Per-frame snapshot of pending whole-file (`write_note`-shaped)
    /// proposals read off the op log. Backs the buffer review surface — the
    /// status-bar version dropdown's pending-proposal section, the pending-
    /// rewrite banner, and the agent-diff toggle — replacing the prior
    /// `pending_snapshot` feed for that surface. Populated in
    /// `main::refresh_whole_file_proposals`.
    pub whole_file_proposals: Vec<hiker_core::ops::op_writes::WholeFileProposal>,
    pub skipped_paths: HashSet<String>,
}

// ===========================================================================
// PanelStates — per-panel local UI state
// ===========================================================================

#[derive(Default)]
pub struct PanelStates {
    /// Per-board-tab UI state (View-as toggle, inline-rename drafts,
    /// pending column-delete confirm). Keyed by tab id.
    pub boards: HashMap<TabId, crate::panels::board::Pane>,
    /// Per-canvas-tab UI state (parsed `Canvas`, the `CanvasView` widget,
    /// View-as toggle, dirty / reload tracking). Keyed by tab id.
    /// status: canvas-tab
    pub canvases: HashMap<TabId, crate::panels::canvas::Pane>,
    pub graph: Option<crate::panels::graph::VaultPanel>,
    pub cluster_graph: HashMap<String, crate::panels::cluster_graph::ClusterView>,
    pub home: crate::panels::home::State,
    /// Sync page local UI state — the per-fork "view diff" cache. [sync-fork-diff]
    pub sync: crate::panels::sync::State,
    /// Floating live edit-preview overlay render cache. One slot suffices —
    /// at most one popup is up at a time (the span under the main caret).
    /// status: widget-edit-popup-preview
    pub edit_preview: crate::panels::buffer::widgets::edit_preview::Cache,
}

// ===========================================================================
// MutationEvent — wand-menu awaiter outcomes
// ===========================================================================

pub enum MutationEvent {
    Applied {
        source_path: String,
        mutation: String,
        content: String,
        source_hash_at_submit: String,
    },
    Failed {
        source_path: String,
        mutation: String,
        error: String,
    },
    Cancelled { source_path: String },
}

pub const INDEXER_EVENTS_MAX: usize = 200;
pub const SYNC_EVENTS_MAX: usize = 200;
pub const NAV_MAX: usize = 200;

// Trails are markdown trail-docs on disk (`core::trails`), read live each
// frame by the trails sidebar — there is no in-`AppState` trail model. The
// active trail is `vault.active_trail` config.

impl NavState {
    /// Record `target` as the new current entry: drop any forward tail (a new
    /// branch), skip a no-op (target equals the current entry), cap to
    /// `NAV_MAX` (dropping the oldest), and advance `idx`.
    pub fn push(&mut self, target: NavTarget) {
        if let Some(idx) = self.idx {
            self.history.truncate(idx + 1);
        }
        if self.history.last() == Some(&target) {
            return;
        }
        self.history.push(target);
        if self.history.len() > NAV_MAX {
            self.history.remove(0);
            // `idx` is recomputed below; the removal shifts everything left by
            // one but the new entry is still the last, so `idx` lands correctly.
        }
        self.idx = Some(self.history.len() - 1);
    }

    pub fn can_back(&self) -> bool {
        self.idx.is_some_and(|i| i > 0)
    }

    pub fn can_forward(&self) -> bool {
        self.idx.is_some_and(|i| i + 1 < self.history.len())
    }

    /// Move back one entry and return the target now current. `None` (no move)
    /// when already at the oldest entry.
    pub fn back(&mut self) -> Option<NavTarget> {
        let i = self.idx?;
        if i == 0 {
            return None;
        }
        self.idx = Some(i - 1);
        self.history.get(i - 1).cloned()
    }

    /// Move forward one entry and return the target now current. `None` when
    /// already at the newest entry.
    pub fn forward(&mut self) -> Option<NavTarget> {
        let i = self.idx?;
        if i + 1 >= self.history.len() {
            return None;
        }
        self.idx = Some(i + 1);
        self.history.get(i + 1).cloned()
    }

    /// The target at the current position.
    pub fn current(&self) -> Option<&NavTarget> {
        self.idx.and_then(|i| self.history.get(i))
    }
}

pub fn nav_push(state: &mut AppState, path: &str) {
    state.session.nav.push(NavTarget::File(path.to_string()));
}

/// Activate the tab with `id`, recording a nav-history entry when it carries a
/// buffer path that differs from the currently-active tab's (and we're not
/// mid back/forward, which sets `nav.locked`). Every tab-activation path — the
/// tab-strip click (reconciled in `main`), Ctrl-Tab cycling, and Ctrl-digit
/// jump — routes through here so switching tabs counts for navigation history
/// uniformly: Back from a switched-to tab returns to the one you left.
pub fn activate_tab(state: &mut AppState, id: TabId) {
    let prev_path = state
        .session
        .active_tab
        .and_then(|p| state.tab_by_id(p))
        .and_then(super::tab::Tab::buffer_path)
        .map(str::to_string);
    state.session.active_tab = Some(id);
    if state.session.nav.locked {
        return;
    }
    let next_path = state.tab_by_id(id).and_then(super::tab::Tab::buffer_path).map(str::to_string);
    if let Some(p) = next_path
        && prev_path.as_deref() != Some(p.as_str())
    {
        nav_push(state, &p);
    }
}

pub fn nav_can_back(state: &AppState) -> bool {
    state.session.nav.can_back()
}

pub fn nav_can_forward(state: &AppState) -> bool {
    state.session.nav.can_forward()
}

// ===========================================================================
// UiState — window-level UI
// ===========================================================================

#[derive(Default)]
pub struct UiState {
    pub custom_titlebar: bool,
    pub show_help: bool,
    pub show_profiler: bool,
    /// Data-driven toolbar layout (see `actions.rs` and `toolbar.rs`).
    /// Loaded from `.hiker/toolbars.json` on vault open; defaults to the
    /// single hard-coded top toolbar that mirrors the legacy layout.
    pub toolbars: Toolbars,
    /// When true, toolbars render with drag/right-click affordances for
    /// reordering and adding/removing buttons.
    pub customize_toolbars: bool,
    /// Command palette open flag.
    pub palette_open: bool,
    /// Current palette search query.
    pub palette_query: String,
    /// Currently-selected row index in the palette result list.
    pub palette_selected: usize,
    /// egui id of the chat composer's text field, recorded each frame it
    /// renders. The editor panel reads `Context::focused()` against this (and
    /// `search_input_id`) to tell when a host text field — not the editor —
    /// owns keyboard focus, so its panel-level Ctrl-Z handler can defer.
    pub chat_input_id: Option<eframe::egui::Id>,
    /// egui id of the discovery search box's text field; see `chat_input_id`.
    pub search_input_id: Option<eframe::egui::Id>,
    /// Fingerprint of the (system, editor, code) font triple last installed
    /// onto the egui context. `None` means "nothing installed yet". Used by
    /// the per-frame `install_user_fonts` re-application so flipping a font
    /// in settings takes effect immediately without a restart. Stored as the
    /// concatenation `"system\0editor\0code"` for a cheap equality compare.
    pub last_fonts_fp: Option<String>,
    /// When true, reader / focus mode also hides the global top bar (the
    /// custom titlebar or native top toolbar). Loaded from
    /// `ui.reader_hide_top_bar` at startup and mirrored by the View menu /
    /// settings checkbox. [view-reader-hide-top-bar]
    pub reader_hide_top_bar: bool,
    /// When true, reader / focus mode also hides the tab strip. Loaded from
    /// `ui.reader_hide_tabs`, mirrored by the View menu / reader-icon context
    /// menu. [view-reader-hide-tabs]
    pub reader_hide_tabs: bool,
    /// When true, reader / focus mode also hides each view's in-tab toolbar.
    /// Loaded from `ui.reader_hide_toolbar`, mirrored by the View menu /
    /// reader-icon context menu. [view-reader-hide-toolbar]
    pub reader_hide_toolbar: bool,
    /// Per-session MRU of command-palette action ids — most-recently
    /// invoked first. Floats recent picks above their fuzzy-match rank
    /// per `command-palette`'s recency rule. In-memory only.
    pub palette_mru: Vec<String>,
}

// ===========================================================================
// Toolbars — data-driven multi-toolbar layout
// ===========================================================================

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Toolbars {
    pub bars: Vec<Toolbar>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Toolbar {
    pub id: String,
    pub side: ToolbarSide,
    /// Ordered list of `ActionId`s (as strings, so the file format is
    /// stable across binary changes). Two synthetic ids — `"sep"` and
    /// `"spacer"` — are recognised by the renderer as layout primitives;
    /// `"actions.menu"` is the composite hamburger dropdown.
    pub actions: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolbarSide {
    Top,
    Bottom,
    Left,
    Right,
}

impl Default for Toolbars {
    fn default() -> Self {
        Self {
            bars: vec![Toolbar {
                id: "top".into(),
                side: ToolbarSide::Top,
                actions: [
                    "nav.back",
                    "nav.forward",
                    "nav.home",
                    "actions.menu",
                    "vault.switch",
                    "sep",
                    "vault.label",
                    "sep",
                    "spacer",
                    "view.reader_mode",
                    "view.menu",
                    "view.toggle_left_sidebar",
                    "view.toggle_right_sidebar",
                ]
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            }],
        }
    }
}

// ===========================================================================
// FileTreeState
// ===========================================================================

#[derive(Default)]
pub struct FileTreeState {
    pub expanded: HashSet<String>,
    pub dir_cache: HashMap<String, Vec<hiker_core::vault::DirEntryDto>>,
    pub selected_folder: Option<String>,
    pub trash_expanded: bool,
    pub renaming: Option<String>,
    pub renaming_text: String,
    pub scroll_target: Option<String>,
    /// Per-frame row-decoration snapshot the files feature renders from.
    /// Refreshed once per frame by the files sidebar surface via a
    /// deferred pre-pass (which has full `&mut AppState`), so the render
    /// path reads only this opaque snapshot rather than reaching past the
    /// narrow `activity::SurfaceCtx` into `session.buffers` / the skipped-paths
    /// channel / another feature's trail state. [feature-filetree-migration]
    pub deco: FileTreeDeco,
    /// Cached flattened, render-order row list for the virtualized tree.
    /// Rebuilt only when a structural change invalidates it (an expand /
    /// collapse toggle, or a directory-listing change) — see
    /// [`Self::invalidate_dir`] / [`Self::invalidate_all`] /
    /// [`Self::invalidate_flat`]. `None` means "rebuild on the next render".
    /// Decorations / child counts / the active-row highlight are NOT baked
    /// in here; they stay live per-render, so only structural edits ever
    /// invalidate the cache. Avoids the previous per-frame re-walk that
    /// cloned every expanded directory's listing.
    pub flat_cache: Option<Vec<crate::files::sidebar::FlatRow>>,
    /// Set when a structural change happens mid-render (an expand / collapse
    /// toggle), so the render epilogue drops the stale `flat_cache` it took
    /// out for the frame and rebuilds next frame rather than restoring it.
    pub flat_dirty: bool,
}

impl FileTreeState {
    /// Drop the cached listing for `dir` and invalidate the flattened-row
    /// cache, so the tree re-lists `dir` and re-flattens on the next render.
    /// Use after creating / moving / deleting an entry inside `dir`.
    pub fn invalidate_dir(&mut self, dir: &str) {
        self.dir_cache.remove(dir);
        self.flat_cache = None;
    }

    /// Drop every cached directory listing and invalidate the flattened-row
    /// cache (a full tree refresh). Use after a change whose scope isn't a
    /// single directory (sort change, bulk move, external refresh).
    pub fn invalidate_all(&mut self) {
        self.dir_cache.clear();
        self.flat_cache = None;
    }

    /// Invalidate just the flattened-row cache without dropping any directory
    /// listing — e.g. after toggling a folder's expanded state. Safe to call
    /// mid-render: also sets the `flat_dirty` flag the render epilogue honours.
    pub fn invalidate_flat(&mut self) {
        self.flat_cache = None;
        self.flat_dirty = true;
    }
}

/// Row-decoration snapshot for the files filetree. The sets are populated
/// from `AppState` data the narrow `activity::SurfaceCtx` doesn't carry, snapshotted
/// once per frame so the render path stays `SurfaceCtx`-only. Holds opaque path
/// strings — no feature-specific types leak in.
#[derive(Default)]
pub struct FileTreeDeco {
    /// Vault-relative paths whose loaded buffer is dirty (drives the
    /// trailing ` *` dirty-dot). Snapshot of `session.buffers`.
    pub dirty: HashSet<String>,
    /// Vault-relative paths the indexer marked skipped (drives the
    /// `  [skip]` marker). Snapshot of `ui_cache.skipped_paths`.
    pub skipped: HashSet<String>,
}

// ===========================================================================
// Toast / Undo
// ===========================================================================

pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub created_at: Instant,
    pub undo: Option<UndoSpec>,
}

/// The last sync-attention state the update loop notified on, so a toast fires
/// only when a NEW item appears (a transition), never on every silent round.
/// status: sync-attention-badge
#[derive(Default)]
pub struct SyncAttentionSeen {
    /// Blocked-doc paths we've already toasted about.
    pub blocked_paths: HashSet<String>,
    /// `(label, reason)` of per-doc/per-peer errors we've already toasted.
    pub errored: HashSet<(String, String)>,
    /// The peer fingerprint of a held content-key change we've already toasted.
    pub pending_key_peer: Option<String>,
}

#[derive(Clone, Copy)]
pub enum ToastLevel {
    Info,
    Warn,
    Error,
}

pub struct UndoSpec {
    pub label: String,
    /// Erased callback executed when the user clicks Undo. Survey of
    /// current call sites turned up zero in-tree producers — the field
    /// exists for future flows (the legacy TS UI fired it for path
    /// renames). When concrete callsites land, prefer adding a variant
    /// to a future `UndoAction` enum over a fresh closure; we kept the
    /// closure form here only because no real producers exist yet.
    pub action: Box<dyn FnOnce(&mut AppState) + Send>,
}

// ===========================================================================
// Modal + confirm intents
// ===========================================================================

pub enum Modal {
    Confirm {
        title: String,
        body: String,
        confirm_label: String,
        cancel_label: String,
        danger: bool,
        /// Pre-canned intent. Replaced the legacy
        /// `Box<dyn FnOnce(&mut AppState) + Send>` so the modal is
        /// `Send` without dynamic dispatch and so the available confirm
        /// actions are discoverable in one place.
        intent: ConfirmIntent,
    },
    DirtyClose {
        path: String,
        tab_id: TabId,
    },
    ConfirmDelete {
        path: String,
    },
    DiskDrift {
        path: String,
        in_buffer_text: String,
    },
    // `Modal::PathConflict` retired with `trail-path-conflict-modal`
    // under path-as-identity (`wikilink-path-form`): there's no ULID
    // half left to disagree with a recorded path, so the Keep mine /
    // Repoint / Break modal has no analogue. An unresolved reference is
    // simply an orphan; the user removes it via the per-card / per-
    // waypoint verbs.
}

/// Concrete confirm intents driving `Modal::Confirm`. New flows add a
/// new variant + a match arm in `apply_confirm`. Survey of current
/// callsites (see `app/src/{toolbar,sidebar/{trails,files,trash},
/// panels/settings/mod}.rs`):
///
/// - Toolbar / vault picker: `SwitchVault { path }` — queues a vault
///   swap via `state.pending_vault_switch`.
/// - Trails sidebar: `DeleteTrailWaypoint { trail_doc_rel, waypoint_path }`
///   — removes one waypoint (and any side-trail descendants) from a trail
///   via `core::trails::ops::remove_waypoint` (notes move to trash).
/// - Trash sidebar: `EmptyTrash` — purges every trashed item.
/// - Settings: `ResetScope { scope_path }` — writes `""` to the named
///   scope file and reloads config from disk.
/// - Settings (embedder swap): `ReloadEmbedder { scope, model_id }` —
///   persists `indexing.model` and posts a `ReloadEmbedder` job to the
///   indexer.
pub enum ConfirmIntent {
    SwitchVault {
        path: PathBuf,
    },
    DeleteTrailWaypoint {
        trail_doc_rel: String,
        waypoint_path: String,
    },
    EmptyTrash,
    ResetScope {
        scope_path: PathBuf,
    },
    ReloadEmbedder {
        scope: hiker_core::config::SettingsScope,
        model_id: String,
    },
}

// ===========================================================================
// AppState — helper methods
// ===========================================================================

impl AppState {
    pub const fn next_tab_id(&mut self) -> TabId {
        let id = TabId(self.session.next_tab_id);
        self.session.next_tab_id += 1;
        id
    }

    pub fn find_or_open_tab(
        &mut self,
        predicate: impl Fn(&TabKind) -> bool,
        build: impl FnOnce() -> TabKind,
    ) -> TabId {
        if let Some(existing) = self.session.tabs.iter().find(|t| predicate(&t.kind)) {
            let id = existing.id;
            self.session.active_tab = Some(id);
            return id;
        }
        let id = self.next_tab_id();
        self.session.tabs.push(Tab::new(id, build(), true));
        self.session.active_tab = Some(id);
        id
    }

    pub fn push_toast(&mut self, message: impl Into<String>, level: ToastLevel) {
        self.toasts.push(Toast {
            message: message.into(),
            level,
            created_at: Instant::now(),
            undo: None,
        });
    }

    pub fn tab_by_id(&self, id: TabId) -> Option<&Tab> {
        self.session.tabs.iter().find(|t| t.id == id)
    }

    pub fn tab_by_id_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.session.tabs.iter_mut().find(|t| t.id == id)
    }

    /// Persist a setting and swap the merged copy in
    /// `state.vault_session.config`. Failure raises an error toast.
    pub fn set_setting(
        &mut self,
        scope: hiker_core::config::SettingsScope,
        key: &str,
        value: &serde_json::Value,
        failure_label: &str,
    ) {
        let vault_root = self.vault_session.vault_root.clone();
        match hiker_core::config::Config::set(scope, key, value, &vault_root) {
            Ok(new_cfg) => {
                // Mirror the MCP-tools subtree into the shared RwLock the
                // MCP handler reads at dispatch time, so toggles like
                // `mcp.tools.review_required` take effect immediately
                // (the handler's RwLock is *not* the same as the main
                // Config; it's a snapshot taken at vault open).
                if let Ok(mut guard) = self.vault_session.services.mcp_tools_cfg.write() {
                    *guard = new_cfg.mcp.tools.clone();
                }
                if let Ok(mut guard) = self.vault_session.config.write() {
                    *guard = new_cfg;
                }
            }
            Err(err) => {
                self.push_toast(
                    format!("{failure_label}: {err}"),
                    ToastLevel::Error,
                );
            }
        }
    }
}

/// `Config::set` + config swap variant that takes a shared `&AppState`.
/// Failure is logged via `tracing` and otherwise silent.
pub fn set_setting_quiet(
    app: &AppState,
    scope: hiker_core::config::SettingsScope,
    key: &str,
    value: &serde_json::Value,
    log_target: &str,
) {
    match hiker_core::config::Config::set(
        scope,
        key,
        value,
        &app.vault_session.vault_root,
    ) {
        Ok(new_cfg) => {
            if let Ok(mut guard) = app.vault_session.services.mcp_tools_cfg.write() {
                *guard = new_cfg.mcp.tools.clone();
            }
            if let Ok(mut guard) = app.vault_session.config.write() {
                *guard = new_cfg;
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, key, "{}: persist setting failed", log_target);
        }
    }
}

// ===========================================================================
// apply_confirm — dispatch for `Modal::Confirm { intent }`
// ===========================================================================

impl AppState {
    /// Whether a view's in-tab toolbar (canvas create toolbar, editor toolbar,
    /// board/graph action rows, …) should be hidden this frame: reader mode is
    /// active AND the user opted into hiding toolbars in it. Reader mode shows
    /// them by default. [view-reader-hide-toolbar]
    #[must_use]
    pub const fn reader_hides_view_toolbar(&self) -> bool {
        self.workbench.reader_mode() && self.ui.reader_hide_toolbar
    }

    pub fn apply_confirm(&mut self, intent: ConfirmIntent) {
    let state = self;
    match intent {
        ConfirmIntent::SwitchVault { path } => {
            let display = path.display().to_string();
            state.vault_switch = VaultSwitchState::Requested(path);
            state.push_toast(
                format!("Switching vault to {}", display),
                ToastLevel::Info,
            );
        }
        ConfirmIntent::DeleteTrailWaypoint { trail_doc_rel, waypoint_path } => {
            // Remove the waypoint (and any side-trail descendants) via the
            // async core verb: the waypoint-notes move to trash and the
            // trail-doc frontmatter drops the subtree. Run synchronously on
            // the frame's tokio runtime; the sidebar re-reads next paint.
            let watcher = state.vault_session.services.watcher.clone();
            let jobs = state.vault_session.services.indexer.job_sender();
            let vault = state.vault_session.vault.clone();
            let trash = hiker_core::trash::Trash::open(&state.vault_session.vault_root);
            let result = match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle.block_on(async {
                    hiker_core::trails::ops::remove_waypoint(
                        &watcher,
                        &jobs,
                        &vault,
                        &trash,
                        &trail_doc_rel,
                        &waypoint_path,
                    )
                    .await
                }),
                Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
            };
            match result {
                Ok(outcome) => {
                    let removed = outcome.removed_count;
                    let msg = if removed > 1 {
                        let sides = removed - 1;
                        format!(
                            "Waypoint and {} side-trail waypoint{} removed",
                            sides,
                            if sides == 1 { "" } else { "s" },
                        )
                    } else {
                        "Waypoint removed".to_string()
                    };
                    state.push_toast(msg, ToastLevel::Info);
                }
                Err(err) => {
                    state.push_toast(format!("Remove waypoint failed: {err}"), ToastLevel::Error);
                }
            }
        }
        ConfirmIntent::EmptyTrash => {
            let trash = hiker_core::trash::Trash::open(&state.vault_session.vault_root);
            let items = trash.list_from_disk().unwrap_or_default();
            let mut purged = 0usize;
            let mut failed = 0usize;
            for it in items {
                match trash.permanent_delete(&it.trashed_name) {
                    Ok(()) => purged += 1,
                    Err(err) => {
                        failed += 1;
                        tracing::warn!(
                            error = %err,
                            name = %it.trashed_name,
                            "empty-trash: purge failed for entry",
                        );
                    }
                }
            }
            if failed > 0 {
                state.push_toast(
                    format!("Purged {purged} items, {failed} failed"),
                    ToastLevel::Warn,
                );
            } else {
                state.push_toast(
                    format!("Purged {purged} item{}", if purged == 1 { "" } else { "s" }),
                    ToastLevel::Info,
                );
            }
        }
        ConfirmIntent::ResetScope { scope_path } => {
            if let Err(err) = std::fs::write(&scope_path, "") {
                state.push_toast(
                    format!("Reset failed: {err}"),
                    ToastLevel::Error,
                );
                return;
            }
            if let Ok(fresh) = Config::load(&state.vault_session.vault_root) {
                if let Ok(mut g) = state.vault_session.services.mcp_tools_cfg.write() {
                    *g = fresh.mcp.tools.clone();
                }
                if let Ok(mut g) = state.vault_session.config.write() {
                    *g = fresh;
                }
            }
            state.push_toast("Scope reset to defaults", ToastLevel::Info);
        }
        ConfirmIntent::ReloadEmbedder { scope, model_id } => {
            state.set_setting(
                scope,
                "indexing.model",
                &serde_json::Value::String(model_id.clone()),
                "Reload embedder failed",
            );
            let indexer = state.vault_session.services.indexer.clone();
            let tx = indexer.job_sender();
            let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let job = hiker_core::indexer::IndexJob::ReloadEmbedder {
                    model_id,
                    reply: reply_tx,
                };
                handle.spawn(async move {
                    let _ = tx.send(job).await;
                });
            }
        }
    }
    }
}

#[cfg(test)]
mod nav_tests {
    //! Back/forward navigation-stack mechanics, exercised on `NavState` alone
    //! (no `AppState`) so the regression-prone scenarios stay fast and
    //! exhaustive. Integration of these targets with real tabs is covered by
    //! the app-level `nav` tests.
    use super::{NavState, NavTarget};

    fn file(p: &str) -> NavTarget {
        NavTarget::File(p.to_string())
    }
    fn snap(p: &str, op: &str) -> NavTarget {
        NavTarget::HistoryVersion { path: p.to_string(), op_id: op.to_string() }
    }
    fn zim(z: &str, article: Option<&str>) -> NavTarget {
        NavTarget::ZimArticle {
            zim_path: z.to_string(),
            article: article.map(str::to_string),
        }
    }

    #[test]
    fn empty_stack_has_no_moves() {
        let nav = NavState::default();
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(nav.current(), None);
    }

    #[test]
    fn single_push_is_current_with_no_moves() {
        let mut nav = NavState::default();
        nav.push(file("a"));
        assert_eq!(nav.current(), Some(&file("a")));
        assert!(!nav.can_back(), "nothing before the only entry");
        assert!(!nav.can_forward());
    }

    #[test]
    fn back_and_forward_walk_the_stack() {
        let mut nav = NavState::default();
        nav.push(file("a"));
        nav.push(file("b"));
        nav.push(file("c"));
        assert!(nav.can_back() && !nav.can_forward());
        assert_eq!(nav.back(), Some(file("b")));
        assert_eq!(nav.back(), Some(file("a")));
        assert!(!nav.can_back() && nav.can_forward());
        assert_eq!(nav.back(), None, "back past the oldest is a no-op");
        assert_eq!(nav.current(), Some(&file("a")), "stayed at the oldest");
        assert_eq!(nav.forward(), Some(file("b")));
        assert_eq!(nav.forward(), Some(file("c")));
        assert_eq!(nav.forward(), None, "forward past the newest is a no-op");
        assert_eq!(nav.current(), Some(&file("c")));
    }

    #[test]
    fn pushing_after_back_truncates_the_forward_tail() {
        let mut nav = NavState::default();
        nav.push(file("a"));
        nav.push(file("b"));
        nav.push(file("c"));
        nav.back(); // at b
        nav.back(); // at a
        nav.push(file("d")); // new branch from a
        assert_eq!(nav.current(), Some(&file("d")));
        assert!(!nav.can_forward(), "b and c were dropped");
        assert_eq!(nav.history, vec![file("a"), file("d")]);
    }

    #[test]
    fn adjacent_duplicate_is_skipped() {
        let mut nav = NavState::default();
        nav.push(file("a"));
        nav.push(file("a")); // same as current → no-op
        assert_eq!(nav.history, vec![file("a")]);
        nav.push(file("b"));
        nav.push(file("a")); // not adjacent to the earlier a → recorded
        assert_eq!(nav.history, vec![file("a"), file("b"), file("a")]);
    }

    #[test]
    fn re_pushing_current_after_back_is_a_noop() {
        let mut nav = NavState::default();
        nav.push(file("a"));
        nav.push(file("b"));
        nav.back(); // at a
        nav.push(file("a")); // re-selecting where we are
        assert_eq!(nav.history, vec![file("a")], "forward tail dropped, no dup added");
        assert_eq!(nav.current(), Some(&file("a")));
    }

    #[test]
    fn snapshots_and_files_interleave_and_round_trip() {
        // The bug this guards: a snapshot must be a first-class nav target so
        // Back returns from it to the live file.
        let mut nav = NavState::default();
        nav.push(file("a"));
        nav.push(snap("a", "op1"));
        nav.push(snap("a", "op2"));
        assert_eq!(nav.back(), Some(snap("a", "op1")));
        assert_eq!(nav.back(), Some(file("a")), "Back from a snapshot returns to the live file");
        assert!(!nav.can_back());
        assert_eq!(nav.forward(), Some(snap("a", "op1")));
        // A different snapshot of the same path is a distinct entry.
        assert_ne!(snap("a", "op1"), snap("a", "op2"));
    }

    #[test]
    fn zim_articles_walk_the_stack_like_browser_history() {
        // ZIM in-archive links record as first-class nav targets so the
        // top-bar Back/Forward walk article history. [zim-nav-stack]
        let mut nav = NavState::default();
        nav.push(zim("wiki.zim", None)); // main page
        nav.push(zim("wiki.zim", Some("Rust")));
        nav.push(zim("wiki.zim", Some("Borrow_checker")));
        assert_eq!(nav.back(), Some(zim("wiki.zim", Some("Rust"))));
        assert_eq!(nav.back(), Some(zim("wiki.zim", None)), "Back reaches the main page");
        assert_eq!(nav.forward(), Some(zim("wiki.zim", Some("Rust"))));
        // Distinct articles (and the main page) are distinct entries.
        assert_ne!(zim("wiki.zim", None), zim("wiki.zim", Some("Rust")));
        // ZIM articles interleave with note files on the one global stack.
        nav.push(file("notes/x.md"));
        assert_eq!(nav.back(), Some(zim("wiki.zim", Some("Rust"))));
    }

    #[test]
    fn canvas_opens_record_as_file_targets_and_interleave() {
        // Opening a `.canvas` records a `NavTarget::File` on the one global
        // stack (canvas::open → nav_push → NavTarget::File), interleaved with
        // notes and snapshots like every other surface. [canvas-nav-stack]
        let mut nav = NavState::default();
        nav.push(file("notes/a.md"));
        nav.push(file("boards/plan.canvas"));
        nav.push(file("notes/b.md"));
        assert_eq!(nav.back(), Some(file("boards/plan.canvas")));
        assert_eq!(nav.back(), Some(file("notes/a.md")), "Back reaches the prior note");
        assert_eq!(nav.forward(), Some(file("boards/plan.canvas")), "Forward returns to the canvas");
        // A canvas file is an ordinary File target — no distinct variant.
        assert_eq!(nav.current(), Some(&file("boards/plan.canvas")));
    }

    #[test]
    fn cap_drops_oldest_and_keeps_idx_at_newest() {
        let mut nav = NavState::default();
        for i in 0..(super::NAV_MAX + 1) {
            nav.push(file(&i.to_string()));
        }
        assert_eq!(nav.history.len(), super::NAV_MAX, "capped");
        assert_eq!(nav.current(), Some(&file(&super::NAV_MAX.to_string())), "newest is current");
        assert_eq!(nav.history.first(), Some(&file("1")), "oldest (0) dropped");
        assert!(nav.can_back() && !nav.can_forward());
    }
}
