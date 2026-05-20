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
//! - `UiCache`: per-frame snapshots (task / staging / skipped paths) so
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

use hiker_core::Vault;
use hiker_core::activity::Activity;
use hiker_core::audit::AgentLog;
use hiker_core::autosave::Autosave;
use hiker_core::changes::Changes;
use hiker_core::config::Config;
use hiker_core::indexer::IndexerHandle;
use hiker_core::staging::Staging;
use hiker_core::store::Store;
use hiker_core::tasks::{Queue as TaskQueue, TaskRecord};
use hiker_core::trees::Trees;
use hiker_core::watcher::{FileEvent, Watcher};

use crate::buffer::Buffer;
use crate::tab::{DockTab, Tab, TabId, TabKind};

// ===========================================================================
// AppState — top-level
// ===========================================================================

pub struct AppState {
    pub vault_session: VaultSession,
    pub session: Session,
    pub ui_cache: UiCache,
    pub panels: PanelStates,
    pub ui: UiState,
    pub toasts: Vec<Toast>,
    pub vault_switch: VaultSwitchState,
    /// IDE-style layout host. Wraps the editor tabs + side bars +
    /// activity bar + status bar. Kept on the top-level state so its
    /// borrow is disjoint from `session.tabs` / `session.buffers`,
    /// which the workbench's pane renderers read mutably each frame.
    pub workbench: egui_workbench::Workbench<
        crate::workbench_host::HikerWbTab,
        crate::workbench_host::HikerMode,
    >,
}

// ===========================================================================
// VaultSwitchState — async vault-open lifecycle
// ===========================================================================

/// State machine for the runtime "open a different vault" flow.
///
/// - `Idle`: nothing pending.
/// - `Requested(path)`: a UI action (toolbar / confirm modal) queued a
///   path. The next `update()` frame transitions to `InProgress` by
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
    pub changes: Arc<Changes>,
    pub staging: Arc<Staging>,
    pub trees: Arc<Trees>,
    pub activity: Arc<Activity>,
    pub autosave: Arc<Autosave>,
    pub watcher: Arc<Watcher>,
    pub indexer: Arc<IndexerHandle>,
    // TODO: surface in the audit/agent-log UI panel.
    #[allow(dead_code)]
    pub audit: Arc<AgentLog>,
    pub tasks: Arc<TaskQueue>,
    pub mcp: Option<Arc<hiker_mcp::McpServerHandle>>,
}

pub struct VaultEvents {
    pub fs_events: Mutex<UnboundedReceiver<FileEvent>>,
    pub indexer_events_rx: Mutex<UnboundedReceiver<String>>,
    pub mutation_events: Mutex<UnboundedReceiver<MutationEvent>>,
    pub mutation_events_tx: UnboundedSender<MutationEvent>,
    /// Bounded ring buffer drained from `indexer_events_rx` each frame
    /// (capped at `INDEXER_EVENTS_MAX`).
    pub indexer_events: VecDeque<String>,
    /// Latest task-queue snapshot pushed by the background pollster
    /// (`bootstrap::spawn_snapshot_poller`). The UI thread `.borrow()`s
    /// this each frame instead of calling `tasks.snapshot().await` from
    /// the render loop. Initial value is an empty `Vec`.
    pub task_snapshot_rx: watch::Receiver<Vec<TaskRecord>>,
    /// Latest staging snapshot pushed by the background pollster. The UI
    /// thread `.borrow()`s this each frame instead of calling
    /// `staging.list_pending()` (SQLite round-trip) every frame.
    pub staging_snapshot_rx: watch::Receiver<Vec<hiker_core::staging::Proposal>>,
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
    /// `egui_tiles::Tree` arrangement mirror. `session.tabs` is the
    /// source of truth for "which tab ids exist"; `dock` mirrors them
    /// into the tree so egui_tiles can render the strip and let the
    /// user drag-to-split. `tabs::reconcile_dock` keeps them in sync at
    /// the top of every render.
    pub dock: egui_tiles::Tree<DockTab>,
    /// Tabs container that hosts buffer tabs (DockTab::Tab). The
    /// reconciler appends new tabs here; the post-frame enforcement
    /// moves stray buffer tabs back into it.
    pub center_tile: egui_tiles::TileId,
    /// Tabs container that holds left-side panels by default.
    pub left_tile: egui_tiles::TileId,
    /// Tabs container that holds right-side panels by default.
    pub right_tile: egui_tiles::TileId,
    /// Set whenever the dock arrangement mutates. The autosave tick
    /// serialises `dock` to `<vault>/.hiker/layout.json` whenever this
    /// flag is set, then clears it.
    pub dock_dirty: bool,
    /// Last-known `TileId` for each registered panel id. Updated each
    /// frame so panel-toggle can re-insert a hidden panel near where
    /// the user last had it.
    pub panel_locations: std::collections::HashMap<String, egui_tiles::TileId>,
    pub active_tab: Option<TabId>,
    /// VSCode-style preview slot — at most one tab is "preview" (italic
    /// label, replaced by the next click).
    pub preview_tab: Option<TabId>,
    pub next_tab_id: u64,
    pub modal: Option<Modal>,
    pub sidebar: SidebarState,
    pub nav: NavState,
    pub trails: Vec<Trail>,
    /// Id of the trail that receives `note_visited` appends. `None` =
    /// the auto-created "Recent" trail.
    pub active_trail: Option<String>,
    /// Inline-rename draft for the trails sidebar.
    pub trail_rename: Option<(String, String)>,
    pub chat: crate::chat::ChatRegistry,
    /// True once `chat::session::discover` has been called for this
    /// vault — keeps the lazy disk walk from running every frame.
    pub chat_discovered: bool,
    /// Last time autosave ticked.
    pub last_autosave_tick: Instant,
    /// Filter pill selection for the vault-home recent-activity feed.
    pub activity_filter: hiker_core::activity::ActivitySource,
    /// Paths with a `NoteMutation` task we just submitted. Gates the
    /// wand-icon menu so concurrent mutations don't pile up.
    pub pending_mutations: HashSet<String>,
}

impl Default for Session {
    fn default() -> Self {
        let bundle = crate::layout::default_dock();
        Self {
            buffers: HashMap::new(),
            tabs: Vec::new(),
            dock: bundle.tree,
            center_tile: bundle.center_tile,
            left_tile: bundle.left_tile,
            right_tile: bundle.right_tile,
            dock_dirty: false,
            panel_locations: std::collections::HashMap::new(),
            active_tab: None,
            preview_tab: None,
            next_tab_id: 1,
            modal: None,
            sidebar: SidebarState::default(),
            nav: NavState::default(),
            trails: Vec::new(),
            active_trail: None,
            trail_rename: None,
            chat: crate::chat::ChatRegistry::new(),
            chat_discovered: false,
            last_autosave_tick: Instant::now(),
            activity_filter: hiker_core::activity::ActivitySource::Merged,
            pending_mutations: HashSet::new(),
        }
    }
}

#[derive(Default)]
pub struct NavState {
    /// Chronological history (with duplicates). `idx` points at the
    /// current position; back/forward shift it.
    pub history: Vec<String>,
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
    pub staging_snapshot: Vec<hiker_core::staging::Proposal>,
    pub skipped_paths: HashSet<String>,
}

// ===========================================================================
// PanelStates — per-panel local UI state
// ===========================================================================

#[derive(Default)]
pub struct PanelStates {
    pub search: crate::panels::search::SearchState,
    pub related: crate::panels::related::RelatedState,
    pub backlinks: crate::panels::backlinks::BacklinksState,
    #[allow(dead_code)]
    pub chat_dock: crate::panels::discovery_pane::ChatDockState,
    pub clusters: ClusterUiState,
    pub trails_ui: TrailsUiState,
    pub preview_buffers: HashMap<String, crate::panels::diff_view::PreviewBuffer>,
    pub graph: Option<crate::panels::graph::GraphState>,
    pub cluster_graph: HashMap<String, crate::panels::cluster_graph::ClusterGraphState>,
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
pub const TRAILS_MAX: usize = 50;
pub const RECENT_TRAIL: &str = "Recent";
pub const NAV_MAX: usize = 200;

// ===========================================================================
// Trails (data + free fns kept at `crate::state::` for compat with imports)
// ===========================================================================

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Trail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub waypoints: Vec<Waypoint>,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub last_activated_at_ms: i64,
    #[serde(default)]
    pub append_under: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Waypoint {
    pub path: String,
    #[serde(default)]
    pub at_ms: i64,
    #[serde(default)]
    pub children: Vec<Waypoint>,
    #[serde(default)]
    pub annotation: String,
}

pub fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn ensure_recent_trail(state: &mut AppState) -> String {
    if let Some(t) = state.session.trails.iter().find(|t| t.name == RECENT_TRAIL) {
        return t.id.clone();
    }
    let id = format!("recent-{}", now_ms_i64());
    state.session.trails.push(Trail {
        id: id.clone(),
        name: RECENT_TRAIL.to_string(),
        waypoints: Vec::new(),
        created_at_ms: now_ms_i64(),
        last_activated_at_ms: now_ms_i64(),
        append_under: None,
    });
    id
}

pub fn create_trail(state: &mut AppState, name: &str) -> String {
    let id = format!("trail-{}", now_ms_i64());
    state.session.trails.push(Trail {
        id: id.clone(),
        name: name.to_string(),
        waypoints: Vec::new(),
        created_at_ms: now_ms_i64(),
        last_activated_at_ms: now_ms_i64(),
        append_under: None,
    });
    id
}

pub fn nav_push(state: &mut AppState, path: &str) {
    let cap = NAV_MAX;
    let nav = &mut state.session.nav;
    if let Some(idx) = nav.idx {
        nav.history.truncate(idx + 1);
    }
    if nav.history.last().map(|p| p == path).unwrap_or(false) {
        return;
    }
    nav.history.push(path.to_string());
    if nav.history.len() > cap {
        nav.history.remove(0);
    }
    nav.idx = Some(nav.history.len() - 1);
}

pub fn nav_can_back(state: &AppState) -> bool {
    state.session.nav.idx.map(|i| i > 0).unwrap_or(false)
}

pub fn nav_can_forward(state: &AppState) -> bool {
    let nav = &state.session.nav;
    nav.idx.map(|i| i + 1 < nav.history.len()).unwrap_or(false)
}

pub fn note_visited(state: &mut AppState, path: &str) {
    let target_id = match state.session.active_trail.clone() {
        Some(id) if state.session.trails.iter().any(|t| t.id == id) => id,
        _ => ensure_recent_trail(state),
    };
    let is_recent = state
        .session
        .trails
        .iter()
        .find(|t| t.id == target_id)
        .map(|t| t.name == RECENT_TRAIL)
        .unwrap_or(true);
    let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == target_id) else {
        return;
    };
    let wp = Waypoint {
        path: path.to_string(),
        at_ms: now_ms_i64(),
        children: Vec::new(),
        annotation: String::new(),
    };

    if let Some(under) = trail.append_under.clone() {
        if let Some(parent) = find_waypoint_mut(&mut trail.waypoints, &under) {
            if parent.children.last().map(|w| w.path == path).unwrap_or(false) {
                return;
            }
            parent.children.push(wp);
            return;
        }
        tracing::warn!(
            append_under = %under,
            trail_id = %trail.id,
            "stale trail append_under cursor; resetting to root"
        );
        trail.append_under = None;
    }

    if is_recent {
        if let Some(pos) = trail.waypoints.iter().position(|w| w.path == path) {
            trail.waypoints.remove(pos);
        }
        trail.waypoints.insert(0, wp);
        if trail.waypoints.len() > TRAILS_MAX {
            trail.waypoints.truncate(TRAILS_MAX);
        }
    } else {
        if trail.waypoints.last().map(|w| w.path == path).unwrap_or(false) {
            return;
        }
        trail.waypoints.push(wp);
        if trail.waypoints.len() > TRAILS_MAX {
            let drop = trail.waypoints.len() - TRAILS_MAX;
            trail.waypoints.drain(0..drop);
        }
    }
}

#[derive(Debug, Default)]
pub struct TrailsUiState {
    pub expanded_path: Option<String>,
    pub expand_all: bool,
    pub side_trail_collapsed: HashSet<String>,
    pub annotation_edit: Option<(String, String)>,
    pub all_trails_picker_open: bool,
}

pub fn find_waypoint_mut<'a>(
    waypoints: &'a mut [Waypoint],
    path: &str,
) -> Option<&'a mut Waypoint> {
    for w in waypoints.iter_mut() {
        if w.path == path {
            return Some(w);
        }
        if let Some(found) = find_waypoint_mut(&mut w.children, path) {
            return Some(found);
        }
    }
    None
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
                    "panel.toggle.search",
                    "panel.toggle.files",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            }],
        }
    }
}

// ===========================================================================
// SidebarState
// ===========================================================================

#[derive(Default)]
pub struct SidebarState {
    pub expanded: HashSet<String>,
    pub dir_cache: HashMap<String, Vec<hiker_core::DirEntryDto>>,
    pub selected_folder: Option<String>,
    pub trash_expanded: bool,
    pub renaming: Option<String>,
    pub renaming_text: String,
    pub scroll_target: Option<String>,
}

// ===========================================================================
// ClusterUiState
// ===========================================================================

/// Outcome posted by a background LLM-naming task. `(succeeded, failed)`.
pub type LlmJobOutcome = (usize, usize);

#[derive(Default)]
pub struct ClusterUiState {
    pub trees: Vec<hiker_core::trees::TreeRow>,
    pub selected_tree: Option<String>,
    pub nodes: Vec<hiker_core::trees::EditableNode>,
    pub expanded: HashSet<String>,
    pub renaming: Option<(String, String)>,
    pub editing_summary: Option<(String, String)>,
    pub editing_tag_policy: Option<(String, String, bool)>,
    pub editing_move_policy: Option<(String, String, bool)>,
    pub selected_nodes: HashSet<String>,
    pub editing_stage_move_target: Option<String>,
    pub editing_stage_tag_slug: Option<String>,
    pub redo_stacks: HashMap<String, Vec<hiker_core::trees::HistoryEntry>>,
    pub showing_advanced_params: bool,
    pub advanced_params: AdvancedClusterParams,
    pub dirty: bool,
    pub loaded: bool,
    pub review_panes: HashMap<TabId, crate::panels::cluster_review::ReviewPane>,
    /// True while a background LLM naming run (regenerate / summarize
    /// subset) is in flight. Gates the "Regenerate names" /
    /// "Summarize subset" buttons so the user can't double-fire.
    pub llm_job_in_flight: bool,
    /// Result channel for the in-flight naming task. The UI loop polls
    /// each frame; on completion we surface a toast and clear the gate.
    pub llm_job_rx: Option<oneshot::Receiver<LlmJobOutcome>>,
}

#[derive(Debug, Clone)]
pub struct AdvancedClusterParams {
    pub min_cluster_size: usize,
    pub min_samples: usize,
    pub k_nearest: usize,
    pub edge_weight_floor: f32,
    pub iterations: u32,
    pub resolution: f32,
    pub use_leiden: bool,
    pub outlier_threshold: f32,
    pub include_outliers: bool,
    pub summary_confidence_threshold: f32,
    pub disable_recursion: bool,
}

impl Default for AdvancedClusterParams {
    fn default() -> Self {
        Self {
            min_cluster_size: 5,
            min_samples: 2,
            k_nearest: 15,
            edge_weight_floor: 0.0,
            iterations: 100,
            resolution: 1.0,
            use_leiden: false,
            outlier_threshold: 0.5,
            include_outliers: true,
            summary_confidence_threshold: 0.5,
            disable_recursion: false,
        }
    }
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
    Recovery {
        entries: Vec<hiker_core::autosave::RecoveredEntry>,
    },
    ConfirmDelete {
        path: String,
    },
    DiskDrift {
        path: String,
        in_buffer_text: String,
    },
}

/// Concrete confirm intents driving `Modal::Confirm`. New flows add a
/// new variant + a match arm in `apply_confirm`. Survey of current
/// callsites (see `app/src/{toolbar,sidebar/{trails,files,trash},
/// panels/settings/mod}.rs`):
///
/// - Toolbar / vault picker: `SwitchVault { path }` — queues a vault
///   swap via `state.pending_vault_switch`.
/// - Trails sidebar: `DeleteTrailWaypoint { trail_id, path }` — removes
///   one waypoint (and any side-trail descendants) from a trail.
/// - Files sidebar: `SoftDeleteIntoTrash { path }` — moves the named
///   file under `<vault>/.hiker/trash/` as a v0 soft-delete.
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
        trail_id: String,
        path: String,
    },
    SoftDeleteIntoTrash {
        path: String,
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
    pub fn next_tab_id(&mut self) -> TabId {
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
        self.session.tabs.push(Tab { id, kind: build(), sticky: true });
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
        value: serde_json::Value,
        failure_label: &str,
    ) {
        let vault_root = self.vault_session.vault_root.clone();
        match hiker_core::config::Config::set(scope, key, value, &vault_root) {
            Ok(new_cfg) => {
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
    value: serde_json::Value,
    log_target: &str,
) {
    match hiker_core::config::Config::set(
        scope,
        key,
        value,
        &app.vault_session.vault_root,
    ) {
        Ok(new_cfg) => {
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

pub fn apply_confirm(state: &mut AppState, intent: ConfirmIntent) {
    match intent {
        ConfirmIntent::SwitchVault { path } => {
            let display = path.display().to_string();
            state.vault_switch = VaultSwitchState::Requested(path);
            state.push_toast(
                format!("Switching vault to {}", display),
                ToastLevel::Info,
            );
        }
        ConfirmIntent::DeleteTrailWaypoint { trail_id, path } => {
            // Lift the trail-rm helpers — defined locally to keep this
            // module self-contained instead of leaking back into trails.rs.
            fn subtree_size(ws: &[Waypoint]) -> usize {
                ws.iter().map(|w| 1 + subtree_size(&w.children)).sum()
            }
            fn remove_waypoint_recursive(ws: &mut Vec<Waypoint>, target: &str) {
                if let Some(pos) = ws.iter().position(|w| w.path == target) {
                    ws.remove(pos);
                    return;
                }
                for w in ws.iter_mut() {
                    remove_waypoint_recursive(&mut w.children, target);
                }
            }
            let removed = if let Some(trail) = state
                .session
                .trails
                .iter_mut()
                .find(|t| t.id == trail_id)
            {
                let before = subtree_size(&trail.waypoints);
                remove_waypoint_recursive(&mut trail.waypoints, &path);
                if trail.append_under.as_deref() == Some(path.as_str()) {
                    trail.append_under = None;
                }
                before.saturating_sub(subtree_size(&trail.waypoints))
            } else {
                0
            };
            let _ = crate::bootstrap::save_trails(
                &state.vault_session.vault_root,
                &state.session.trails,
            );
            let msg = if removed > 1 {
                let sides = removed - 1;
                format!(
                    "Waypoint and {} side-trail waypoint{} removed",
                    sides,
                    if sides == 1 { "" } else { "s" },
                )
            } else if removed == 1 {
                "Waypoint removed".to_string()
            } else {
                "Waypoint not found".to_string()
            };
            state.push_toast(msg, ToastLevel::Info);
        }
        ConfirmIntent::SoftDeleteIntoTrash { path } => {
            let rel = path;
            let abs = match state.vault_session.vault.abs_path(&rel) {
                Ok(p) => p,
                Err(err) => {
                    state.push_toast(
                        format!("Delete failed: {}", err),
                        ToastLevel::Error,
                    );
                    return;
                }
            };
            let trash_dir = state
                .vault_session
                .vault
                .root()
                .join(".hiker")
                .join("trash");
            if let Err(err) = std::fs::create_dir_all(&trash_dir) {
                state.push_toast(
                    format!("Create trash dir failed: {}", err),
                    ToastLevel::Error,
                );
                return;
            }
            let basename = rel
                .rsplit_once('/')
                .map(|(_, b)| b)
                .unwrap_or(rel.as_str());
            let target = trash_dir.join(format!("{}-{}", now_ms_i64(), basename));
            if let Err(err) = std::fs::rename(&abs, &target) {
                state.push_toast(
                    format!("Delete failed: {}", err),
                    ToastLevel::Error,
                );
                return;
            }
            // Close any open tabs for the deleted path + drop its buffer.
            let to_close: Vec<TabId> = state
                .session
                .tabs
                .iter()
                .filter(|t| t.buffer_path() == Some(rel.as_str()))
                .map(|t| t.id)
                .collect();
            for id in to_close {
                crate::editor_pane::close_tab(state, id);
            }
            state.session.buffers.remove(&rel);
            let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            state.session.sidebar.dir_cache.remove(parent);
            state.push_toast(format!("Moved {} to trash", rel), ToastLevel::Info);
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
            if let Ok(fresh) = Config::load(&state.vault_session.vault_root)
                && let Ok(mut g) = state.vault_session.config.write()
            {
                *g = fresh;
            }
            state.push_toast("Scope reset to defaults", ToastLevel::Info);
        }
        ConfirmIntent::ReloadEmbedder { scope, model_id } => {
            state.set_setting(
                scope,
                "indexing.model",
                serde_json::Value::String(model_id.clone()),
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
