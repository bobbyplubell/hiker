//! Tab kinds (mirrors hiker's `tab-kinds` from docs/editor.md).
//!
//! Each tab carries an opaque `TabId` (stable across the session) plus a
//! `TabKind` payload. App-level state for buffers lives in
//! `AppState::buffers`, keyed by path — the buffer tab kind just stores the
//! path and looks up the buffer when it needs it.
//
// TODO(tab-panel-trait): a full `TabPanel` trait that consolidates the
// label / icon / buffer_path / shows_buffer_chrome / persist_key /
// body-dispatch matches into one location would tidy this up further.
// The shape would be either:
//
//   (a) per-variant structs (e.g. `BufferTab { path: String }`) each
//       implementing `TabPanel`, with `TabKind` becoming an enum of
//       structs that returns `&dyn TabPanel` via a single dispatch
//       match. ~127 TabKind:: construction/pattern sites in app/src
//       would need touching. Too invasive for a refactor pass.
//
//   (b) `impl TabPanel for TabKind` with the matches stuck inside the
//       trait methods. This just relocates matches rather than
//       eliminating them, and `show` carries a runtime arg that doesn't
//       compose with the otherwise uniform method signatures.
//
// `persist_key` below was the most isolated win and is centralised
// here; the rest stays scattered until the trait approach is worth a
// dedicated PR.

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TabId(pub u64);

/// Identifier for a registered panel mounted in the dock. Stable string
/// so layout files survive panel-registry shuffles.
pub type PanelId = &'static str;

/// A node mounted inside the central `DockState`. Buffer / page tabs
/// reference `Session::tabs` by id; panels reference the panel registry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DockTab {
    Tab(TabId),
    Panel(String),
}

impl DockTab {
    pub fn panel(id: PanelId) -> Self {
        DockTab::Panel(id.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub id: TabId,
    pub kind: TabKind,
    /// Sticky = user signalled intent to keep this tab open. Preview tabs
    /// are non-sticky and live in `AppState::preview_tab`.
    pub sticky: bool,
}

// TODO: TrashPreview / QueueDetail / Agent variants are wired through
// open_*-tab callers in adjacent panels but not constructed here yet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum TabKind {
    /// Editable note buffer. Payload: vault-relative path.
    Buffer { path: String },
    /// Read-only preview of a trash entry. Payload: absolute trash path.
    TrashPreview { trash_path: String, original_path: String },
    /// Snapshot preview against a change-id.
    SnapshotPreview { path: String, change_id: String },
    /// Diff between the live buffer and the file on disk. Live (read-only
    /// preview rebuilt each frame from the current buffer text).
    BufferDiff { path: String },
    /// Staging proposal preview.
    StagingPreview { proposal_id: String, target_path: String },
    Home,
    /// "Recent activity" / "Snapshots" home-detail sub-page.
    HomeDetail { which: HomeDetail },
    Queue,
    QueueDetail { task_id: String },
    Settings,
    Properties { path: String },
    /// Vault-wide graph view (deferred — placeholder for v1).
    Graph,
    /// Chat session as a full tab (vs. docked at bottom of discovery).
    Agent { session_id: String },
    /// Patch review: lists pending staging proposals with accept/reject.
    PatchReview,
    /// Plugins host (stub): lists `.hiker/plugins.json` entries.
    Plugins,
    /// Indexer detail / control: model id, status, reindex.
    IndexerDetail,
    /// Activity feed scoped to `author LIKE 'agent:%'`.
    AgentChanges,
    /// Cluster Review tab: two-phase preview-then-persist for a fresh
    /// cluster build over the vault. Payload is the build configuration;
    /// the tab body holds a draft tree until the user persists.
    ClusterReview { config_json: String },
    /// Cluster tree visualised as a radial dendrogram. Payload is the
    /// `tree_id` to render. Mirrors the legacy `clusterEditorPane/graphView/`
    /// subsystem: cluster nodes as bubbles, leaves at the rim, edges
    /// drawn from parent → child.
    ClusterGraph { tree_id: String },
}

#[derive(Debug, Clone)]
pub enum HomeDetail {
    RecentActivity,
    Snapshots,
    /// Per-row version history view: lists every changes-log entry that
    /// touched the given vault-relative path, newest first. Spec:
    /// `vault-home-recent-activity-detail`.
    ActivityRow { path: String },
}

impl TabKind {
    pub fn label(&self) -> String {
        match self {
            TabKind::Buffer { path } => path_basename(path),
            TabKind::TrashPreview { original_path, .. } => {
                format!("Trash · {}", path_basename(original_path))
            }
            TabKind::SnapshotPreview { path, .. } => {
                format!("Snapshot · {}", path_basename(path))
            }
            TabKind::BufferDiff { path } => format!("Diff · {}", path_basename(path)),
            TabKind::StagingPreview { target_path, .. } => {
                format!("Staging · {}", path_basename(target_path))
            }
            TabKind::Home => "Home".to_string(),
            TabKind::HomeDetail { which } => match which {
                HomeDetail::RecentActivity => "Recent activity".to_string(),
                HomeDetail::Snapshots => "Snapshots".to_string(),
                HomeDetail::ActivityRow { path } => {
                    format!("History · {}", path_basename(path))
                }
            },
            TabKind::Queue => "Queue".to_string(),
            TabKind::QueueDetail { task_id } => format!("Task · {}", &task_id[..task_id.len().min(8)]),
            TabKind::Settings => "Settings".to_string(),
            TabKind::Properties { path } => format!("Properties · {}", path_basename(path)),
            TabKind::Graph => "Graph".to_string(),
            TabKind::Agent { .. } => "Chat".to_string(),
            TabKind::PatchReview => "Patch review".to_string(),
            TabKind::Plugins => "Plugins".to_string(),
            TabKind::IndexerDetail => "Index".to_string(),
            TabKind::AgentChanges => "Agent changes".to_string(),
            TabKind::ClusterReview { .. } => "Cluster review".to_string(),
            TabKind::ClusterGraph { .. } => "Cluster graph".to_string(),
        }
    }

    /// Single source of truth for the icon associated with this tab
    /// kind. Returned as a fresh `egui::Image` each call so callers can
    /// attach sizing/tint on top. The tab strip and the toolbar's
    /// "More actions" menu both route through here so the same
    /// destination shows the same symbol everywhere it appears.
    pub fn icon(&self) -> eframe::egui::Image<'static> {
        use crate::icons;
        match self {
            TabKind::Buffer { .. } => icons::file(),
            TabKind::TrashPreview { .. } => icons::trash(),
            TabKind::SnapshotPreview { .. } => icons::clock(),
            TabKind::BufferDiff { .. } => icons::chart(),
            TabKind::StagingPreview { .. } => icons::edit(),
            TabKind::Home => icons::home(),
            TabKind::HomeDetail { .. } => icons::home(),
            TabKind::Queue => icons::clipboard(),
            TabKind::QueueDetail { .. } => icons::clipboard(),
            TabKind::Settings => icons::settings(),
            TabKind::Properties { .. } => icons::info(),
            TabKind::Graph => icons::graph(),
            TabKind::Agent { .. } => icons::chat(),
            TabKind::PatchReview => icons::check(),
            TabKind::Plugins => icons::plugin(),
            TabKind::IndexerDetail => icons::compass(),
            TabKind::AgentChanges => icons::robot(),
            TabKind::ClusterReview { .. } => icons::graph(),
            TabKind::ClusterGraph { .. } => icons::graph(),
        }
    }
}

impl Tab {
    pub fn label(&self) -> String {
        self.kind.label()
    }

    // TODO: surface in dock tab titles when egui_dock supports image+text
    // titles (current 0.17 `TabViewer::title` returns plain `WidgetText`,
    // so the per-tab icon shown by the legacy custom strip is dropped in
    // the dock-rendered strip). `TabKind::icon` is still in use by the
    // toolbar Actions menu.
    #[allow(dead_code)]
    pub fn icon(&self) -> eframe::egui::Image<'static> {
        self.kind.icon()
    }

    /// True if the tab kind shows the buffer-scoped chrome (editor
    /// toolbar, status bar). Buffer-only per `editor.md`.
    #[allow(dead_code)] // TODO: wire into tab renderer to gate per-tab chrome.
    pub fn shows_buffer_chrome(&self) -> bool {
        matches!(
            &self.kind,
            TabKind::Buffer { .. }
                | TabKind::TrashPreview { .. }
                | TabKind::SnapshotPreview { .. }
                | TabKind::StagingPreview { .. }
                | TabKind::BufferDiff { .. }
        )
    }

    /// Workspace-restore key for this tab: `Some((key, kind_str))` if the
    /// tab survives a restart, `None` if the kind needs payload data we
    /// don't persist (TrashPreview, SnapshotPreview, StagingPreview,
    /// BufferDiff, Properties, Agent, QueueDetail, HomeDetail,
    /// ClusterReview, ClusterGraph). Centralised here so callers in
    /// `main.rs` and any future restore path stay in sync.
    pub fn persist_key(&self) -> Option<(String, String)> {
        Some(match &self.kind {
            TabKind::Buffer { path } => (path.clone(), "buffer".into()),
            TabKind::Home => (":home".into(), "home".into()),
            TabKind::Queue => (":queue".into(), "queue".into()),
            TabKind::Settings => (":settings".into(), "settings".into()),
            TabKind::Graph => (":graph".into(), "graph".into()),
            TabKind::PatchReview => (":patch_review".into(), "patch_review".into()),
            TabKind::Plugins => (":plugins".into(), "plugins".into()),
            TabKind::IndexerDetail => (":indexer".into(), "indexer".into()),
            TabKind::AgentChanges => (":agent_changes".into(), "agent_changes".into()),
            // Variants intentionally skipped: HomeDetail, TrashPreview,
            // SnapshotPreview, BufferDiff, StagingPreview, QueueDetail,
            // Properties, Agent, ClusterReview, ClusterGraph — restoring
            // these would require re-staging payload state we don't
            // round-trip yet.
            _ => return None,
        })
    }

    /// Buffer path the tab is about, if any. Used for dirty-marker
    /// lookups and version-dropdown population.
    pub fn buffer_path(&self) -> Option<&str> {
        match &self.kind {
            TabKind::Buffer { path } => Some(path.as_str()),
            TabKind::TrashPreview { original_path, .. } => Some(original_path.as_str()),
            TabKind::SnapshotPreview { path, .. } => Some(path.as_str()),
            TabKind::BufferDiff { path } => Some(path.as_str()),
            TabKind::StagingPreview { target_path, .. } => Some(target_path.as_str()),
            TabKind::Properties { path } => Some(path.as_str()),
            _ => None,
        }
    }
}

fn path_basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}
