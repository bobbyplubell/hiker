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
    pub workbench: egui_workbench::workspace::Workbench<
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
    /// The WASM plugin host for this vault: loaded plugins + their live
    /// instances. UI-thread-owned (the engine instances aren't `Sync`), so it
    /// lives here rather than in the `Arc`-handle `Services` bag. Drives plugin
    /// panels through `&mut` in the frame loop.
    pub plugins: hiker_core::plugins::PluginHost,
    /// Cancellation token shared with every background task spawned for
    /// this vault (watcher relay, indexer progress forwarder, direct
    /// LLM worker). On vault swap the update loop calls
    /// `cancel.cancel()` before the new session lands so those tasks
    /// stop relaying into the now-stale state.
    pub cancel: CancellationToken,
}

pub struct Services {
    pub read_store: Arc<Mutex<Store>>,
    /// The vault's op log: the CRDT-shaped write substrate every producer
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
    /// The live `hiker-sync` engine, present only when `[sync].enabled`. When
    /// sync is off this is `None` and nothing is constructed (no keys, no
    /// swarm, no listener). The Sync page renders a disabled state in that
    /// case. Wrapped in `Arc` so the page can clone a handle to spawn async
    /// `force_sync` / `discover` work off the frame loop.
    pub sync: Option<Arc<crate::sync_service::SyncService>>,
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
    /// Id of the trail that receives manual append-waypoint actions.
    /// `None` = no active trail; the Add-to-trail verbs hide/disable.
    pub active_trail: Option<String>,
    /// Inline-rename draft for the trails sidebar.
    pub trail_rename: Option<(String, String)>,
    pub chat: crate::chat::state::ChatRegistry,
    /// True once `chat::session::discover` has been called for this
    /// vault — keeps the lazy disk walk from running every frame.
    pub chat_discovered: bool,
    /// Last time autosave ticked.
    pub last_autosave_tick: Instant,
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
            chat: crate::chat::state::ChatRegistry::new(),
            chat_discovered: false,
            last_autosave_tick: Instant::now(),
            pending_mutations: HashSet::new(),
        }
    }
}

/// One entry in the back/forward navigation stack — what a Back/Forward press
/// restores into the active editor view. Path-only nav couldn't represent a
/// historical snapshot (a `(path, op_id)` pair), so navigating to a snapshot
/// dropped out of the stack and Back couldn't return; modelling the target
/// explicitly fixes that and keeps the stack logic unit-testable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavTarget {
    /// The live vault buffer for a path.
    File(String),
    /// A historical snapshot of `path` at a specific accepted op.
    Snapshot { path: String, op_id: String },
}

impl NavTarget {
    /// The vault-relative path this target concerns (for tab matching / the
    /// status bar), regardless of variant.
    pub fn path(&self) -> &str {
        match self {
            NavTarget::File(p) | NavTarget::Snapshot { path: p, .. } => p,
        }
    }
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
    pub search: crate::panels::search::State,
    pub related: crate::panels::related::State,
    pub backlinks: crate::panels::backlinks::State,
    #[allow(dead_code)]
    pub chat_dock: crate::panels::discovery_pane::ChatDockState,
    pub clusters: ClusterUiState,
    pub trails_ui: TrailsUiState,
    /// Per-board-tab UI state (View-as toggle, inline-rename drafts,
    /// pending column-delete confirm). Keyed by tab id.
    pub boards: HashMap<TabId, crate::panels::board::Pane>,
    pub graph: Option<crate::panels::graph::State>,
    pub cluster_graph: HashMap<String, crate::panels::cluster_graph::ClusterGraph>,
    pub home: crate::panels::home::State,
    /// Sync page local UI state — the per-fork "view diff" cache. [sync-fork-diff]
    pub sync: crate::panels::sync::State,
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
pub const TRAILS_MAX: usize = 50;
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

pub fn nav_can_back(state: &AppState) -> bool {
    state.session.nav.can_back()
}

pub fn nav_can_forward(state: &AppState) -> bool {
    state.session.nav.can_forward()
}

/// Manually append `path` as a waypoint of the currently active trail.
/// No-op when no trail is active. When `append_under` is set, the new
/// waypoint nests under that cursor; otherwise it lands at the root tail.
pub fn trail_append_waypoint(state: &mut AppState, path: &str) {
    let Some(trail_id) = state.session.active_trail.clone() else {
        return;
    };
    let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == trail_id) else {
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

    if trail.waypoints.last().map(|w| w.path == path).unwrap_or(false) {
        return;
    }
    trail.waypoints.push(wp);
    if trail.waypoints.len() > TRAILS_MAX {
        let drop = trail.waypoints.len() - TRAILS_MAX;
        trail.waypoints.drain(0..drop);
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

/// Waypoint drag-and-drop placement target.
#[derive(Debug, Clone)]
pub enum MoveOp {
    /// Insert as a sibling immediately before `target`.
    Before(String),
    /// Insert as a sibling immediately after `target`.
    After(String),
    /// Append as a child of `target`.
    Child(String),
    /// Place at the root-level head of the trail.
    Head,
    /// Place at the root-level tail of the trail.
    Tail,
}

fn take_waypoint(waypoints: &mut Vec<Waypoint>, path: &str) -> Option<Waypoint> {
    if let Some(pos) = waypoints.iter().position(|w| w.path == path) {
        return Some(waypoints.remove(pos));
    }
    for w in waypoints.iter_mut() {
        if let Some(taken) = take_waypoint(&mut w.children, path) {
            return Some(taken);
        }
    }
    None
}

impl Trail {
/// Locate `target` within this trail's waypoint forest, returning
/// `(parent_path, index)` — `parent_path` is `None` for a root-level
/// hit. `&self` method so the single caller (`move_waypoint`) doesn't
/// trip `single_call_fn`.
fn locate_waypoint(&self, target: &str) -> Option<(Option<String>, usize)> {
    let waypoints = &self.waypoints;
    for (i, w) in waypoints.iter().enumerate() {
        if w.path == target {
            return Some((None, i));
        }
    }
    fn descend(w: &Waypoint, target: &str) -> Option<(Option<String>, usize)> {
        for (i, c) in w.children.iter().enumerate() {
            if c.path == target {
                return Some((Some(w.path.clone()), i));
            }
            if let Some(found) = descend(c, target) {
                return Some(found);
            }
        }
        None
    }
    for w in waypoints {
        if let Some(found) = descend(w, target) {
            return Some(found);
        }
    }
    None
}
}

/// True when `path` is `ancestor` itself or appears anywhere within
/// `ancestor`'s subtree. Used to reject moves that would cycle.
fn is_in_subtree(waypoints: &[Waypoint], ancestor: &str, path: &str) -> bool {
    fn walk(w: &Waypoint, path: &str) -> bool {
        w.path == path || w.children.iter().any(|c| walk(c, path))
    }
    for w in waypoints {
        if w.path == ancestor {
            return walk(w, path);
        }
        if is_in_subtree(&w.children, ancestor, path) {
            return true;
        }
    }
    false
}

impl Trail {
/// Move waypoint `src` to a new position per `op`. Returns true on
/// success. No-op (returns false) when the source isn't present or the
/// move would create a cycle (dropping a node into its own subtree).
pub fn move_waypoint(&mut self, src: &str, op: MoveOp) -> bool {
    match &op {
        MoveOp::Before(t) | MoveOp::After(t) | MoveOp::Child(t) => {
            if src == t.as_str() || is_in_subtree(&self.waypoints, src, t) {
                return false;
            }
        }
        MoveOp::Head | MoveOp::Tail => {}
    }
    let Some(item) = take_waypoint(&mut self.waypoints, src) else {
        return false;
    };
    match op {
        MoveOp::Tail => {
            self.waypoints.push(item);
            true
        }
        MoveOp::Head => {
            self.waypoints.insert(0, item);
            true
        }
        MoveOp::Child(target) => {
            if let Some(parent) = find_waypoint_mut(&mut self.waypoints, &target) {
                parent.children.push(item);
                true
            } else {
                self.waypoints.push(item);
                false
            }
        }
        MoveOp::Before(target) => match self.locate_waypoint(&target) {
            Some((None, idx)) => {
                self.waypoints.insert(idx, item);
                true
            }
            Some((Some(parent_path), idx)) => {
                if let Some(parent) = find_waypoint_mut(&mut self.waypoints, &parent_path) {
                    parent.children.insert(idx, item);
                    true
                } else {
                    self.waypoints.push(item);
                    false
                }
            }
            None => {
                self.waypoints.push(item);
                false
            }
        },
        MoveOp::After(target) => match self.locate_waypoint(&target) {
            Some((None, idx)) => {
                self.waypoints.insert(idx + 1, item);
                true
            }
            Some((Some(parent_path), idx)) => {
                if let Some(parent) = find_waypoint_mut(&mut self.waypoints, &parent_path) {
                    parent.children.insert(idx + 1, item);
                    true
                } else {
                    self.waypoints.push(item);
                    false
                }
            }
            None => {
                self.waypoints.push(item);
                false
            }
        },
    }
}
}

#[cfg(test)]
mod move_tests {
    use super::*;
    fn wp(path: &str, children: Vec<Waypoint>) -> Waypoint {
        Waypoint { path: path.into(), at_ms: 0, children, annotation: String::new() }
    }
    fn tr(waypoints: Vec<Waypoint>) -> Trail {
        Trail {
            id: "t".into(),
            name: "t".into(),
            waypoints,
            created_at_ms: 0,
            last_activated_at_ms: 0,
            append_under: None,
        }
    }
    #[test]
    fn move_to_tail_reorders_root() {
        let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
        assert!(t.move_waypoint("a", MoveOp::Tail));
        assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
    }
    #[test]
    fn move_before_inserts_at_root() {
        let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
        assert!(t.move_waypoint("c", MoveOp::Before("a".into())));
        assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["c", "a", "b"]);
    }
    #[test]
    fn move_as_child_nests() {
        let mut t = tr(vec![wp("a", vec![]), wp("b", vec![])]);
        assert!(t.move_waypoint("b", MoveOp::Child("a".into())));
        assert_eq!(t.waypoints.len(), 1);
        assert_eq!(t.waypoints[0].path, "a");
        assert_eq!(t.waypoints[0].children[0].path, "b");
    }
    #[test]
    fn move_after_inserts_following_target_at_root() {
        let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
        assert!(t.move_waypoint("a", MoveOp::After("b".into())));
        assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "a", "c"]);
    }
    #[test]
    fn move_after_last_appends_to_tail() {
        let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
        assert!(t.move_waypoint("a", MoveOp::After("c".into())));
        assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
    }
    #[test]
    fn move_after_nested_target_stays_in_parent_list() {
        let mut t = tr(vec![wp("a", vec![wp("a1", vec![]), wp("a2", vec![])]), wp("b", vec![])]);
        assert!(t.move_waypoint("b", MoveOp::After("a1".into())));
        let children = t.waypoints[0].children.iter().map(|w| w.path.as_str()).collect::<Vec<_>>();
        assert_eq!(children, vec!["a1", "b", "a2"]);
    }
    #[test]
    fn cycle_drop_into_own_subtree_rejected() {
        let mut t = tr(vec![wp("a", vec![wp("a1", vec![])])]);
        assert!(!t.move_waypoint("a", MoveOp::Child("a1".into())));
        // Untouched.
        assert_eq!(t.waypoints[0].path, "a");
        assert_eq!(t.waypoints[0].children[0].path, "a1");
    }
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
// SidebarState
// ===========================================================================

#[derive(Default)]
pub struct SidebarState {
    pub expanded: HashSet<String>,
    pub dir_cache: HashMap<String, Vec<hiker_core::vault::DirEntryDto>>,
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
    pub trees: Vec<hiker_core::trees::types::TreeRow>,
    pub selected_tree: Option<String>,
    pub nodes: Vec<hiker_core::trees::types::EditableNode>,
    pub expanded: HashSet<String>,
    pub renaming: Option<(String, String)>,
    pub editing_summary: Option<(String, String)>,
    pub editing_tag_policy: Option<(String, String, bool)>,
    pub editing_move_policy: Option<(String, String, bool)>,
    pub selected_nodes: HashSet<String>,
    pub editing_stage_move_target: Option<String>,
    pub editing_stage_tag_slug: Option<String>,
    pub redo_stacks: HashMap<String, Vec<hiker_core::trees::types::HistoryEntry>>,
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
    /// A stored double-link reference whose recorded path now points at a
    /// note with a different ULID than the one recorded (the core's
    /// `ResolutionOutcome::PathConflict`). Offers Keep mine / Repoint /
    /// Break. Reusable across reference surfaces (boards, trails) via the
    /// `target` discriminator. status: trail-path-conflict-modal
    PathConflict {
        /// The recorded path that now resolves to a different identity.
        path: String,
        /// The ULID the reference recorded.
        recorded_id: String,
        /// The ULID the note currently at `path` carries.
        current_path_id: String,
        /// Which reference surface + entry the resolution applies to.
        target: PathConflictTarget,
    },
}

/// Identifies the concrete reference whose `PathConflict` the modal resolves.
/// One variant per reference surface so the single modal serves boards and
/// (when its app-side waypoint model carries a ULID) trails alike.
///
/// status: trail-path-conflict-modal
#[derive(Clone)]
pub enum PathConflictTarget {
    /// A board card: identified by its board-doc path + card id. "Repoint"
    /// rewrites the card's stored path to the note now at `path`; "Break"
    /// removes the card. status: board-card-references
    BoardCard { board_rel: String, card_id: String },
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
        NavTarget::Snapshot { path: p.to_string(), op_id: op.to_string() }
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
