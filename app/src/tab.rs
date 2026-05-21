//! Tab kinds (mirrors hiker's `tab-kinds` from docs/editor.md).
//!
//! Each tab carries an opaque `TabId` (stable across the session) plus a
//! `TabKind` payload. App-level state for buffers lives in
//! `AppState::buffers`, keyed by path — the buffer tab kind just stores the
//! buffer source and looks up the buffer when it needs it.
//!
//! The `Editor` variant is the only buffer-backed kind: it carries a
//! `BufferSource` (what's *in* the editor — a vault file, a snapshot blob,
//! a staging proposal, or a trash entry) plus an optional `DiffSource`
//! (the comparison target when diff mode is active). Diff is a mode of
//! this tab, not a separate kind.

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

/// What's in an editor tab's buffer. Each variant maps to a different
/// loading path and a different read/write posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferSource {
    /// Vault file — editable, dirty-tracked, autosaved.
    Vault { path: String },
    /// Historical snapshot from `changes.db` — read-only.
    Snapshot { path: String, change_id: String },
    /// Staging proposal content — read-only.
    StagingProposal { proposal_id: String, target_path: String },
    /// Trash entry — read-only.
    Trash { trash_path: String, original_path: String },
}

impl BufferSource {
    /// The vault-relative path the source identifies (for dirty-marker
    /// lookups, version-dropdown population, reveal-in-tree, etc.).
    pub fn path(&self) -> &str {
        match self {
            BufferSource::Vault { path } => path,
            BufferSource::Snapshot { path, .. } => path,
            BufferSource::StagingProposal { target_path, .. } => target_path,
            BufferSource::Trash { original_path, .. } => original_path,
        }
    }

    /// True for sources that map to an editable vault buffer.
    pub fn is_vault(&self) -> bool {
        matches!(self, BufferSource::Vault { .. })
    }
}

/// The "other side" of a diff. Resolves through existing services to a
/// rope at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// On-disk text at the given vault-relative path.
    Disk { path: String },
    /// Another open buffer's live text.
    LiveBuffer { path: String },
    /// `changes.db` row content (`content_at(change_id)`); `path` is the
    /// vault-relative path the change touched, retained for restore.
    ChangesDb { change_id: String, path: String },
    /// Staging proposal's stored before-text or content.
    StagingProposal { proposal_id: String },
    /// Trashed file content.
    Trash { trash_path: String },
    /// Empty rope (e.g. comparing a new-note proposal against "no file").
    Empty,
}

// TODO: TrashPreview / QueueDetail / Agent variants are wired through
// open_*-tab callers in adjacent panels but not constructed here yet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum TabKind {
    /// Editor tab: a buffer (vault file, snapshot, proposal, or trash
    /// entry) optionally layered with a diff against another source.
    Editor { buffer: BufferSource, diff: Option<DiffSource> },
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
    /// Unified activity / changes feed: every pending staging proposal
    /// plus every committed `changes.db` row, with author + op + source
    /// filter chips.
    Changes,
    /// Cluster Review tab: two-phase preview-then-persist for a fresh
    /// cluster build over the vault. Payload is the build configuration;
    /// the tab body holds a draft tree until the user persists.
    ClusterReview { config_json: String },
    /// Cluster tree visualised as a radial dendrogram. Payload is the
    /// `tree_id` to render.
    ClusterGraph { tree_id: String },
}

#[derive(Debug, Clone)]
pub enum HomeDetail {
    Snapshots,
    /// Per-row version history view: lists every changes-log entry that
    /// touched the given vault-relative path, newest first.
    ActivityRow { path: String },
}

impl TabKind {
    /// Construct a plain vault buffer tab (no diff active).
    pub fn vault_buffer(path: impl Into<String>) -> Self {
        TabKind::Editor {
            buffer: BufferSource::Vault { path: path.into() },
            diff: None,
        }
    }

    /// Construct an editor tab for a vault file with the on-disk diff active
    /// (the dirty-buffer diff toggle's default state).
    pub fn buffer_diff(path: impl Into<String>) -> Self {
        let p = path.into();
        TabKind::Editor {
            buffer: BufferSource::Vault { path: p.clone() },
            diff: Some(DiffSource::Disk { path: p }),
        }
    }

    /// Construct a snapshot-preview editor tab (read-only snapshot blob
    /// with diff against current disk active by default).
    pub fn snapshot_preview(path: impl Into<String>, change_id: impl Into<String>) -> Self {
        let p = path.into();
        let cid = change_id.into();
        TabKind::Editor {
            buffer: BufferSource::Snapshot { path: p.clone(), change_id: cid.clone() },
            diff: Some(DiffSource::ChangesDb { change_id: cid, path: p }),
        }
    }

    /// Construct a staging-proposal review tab (proposal content read-only,
    /// diff against current disk active).
    pub fn staging_preview(proposal_id: impl Into<String>, target_path: impl Into<String>) -> Self {
        let pid = proposal_id.into();
        let target = target_path.into();
        TabKind::Editor {
            buffer: BufferSource::StagingProposal {
                proposal_id: pid.clone(),
                target_path: target.clone(),
            },
            diff: Some(DiffSource::StagingProposal { proposal_id: pid }),
        }
    }

    /// Construct a trash-preview tab (trashed file content read-only, no diff).
    pub fn trash_preview(trash_path: impl Into<String>, original_path: impl Into<String>) -> Self {
        TabKind::Editor {
            buffer: BufferSource::Trash {
                trash_path: trash_path.into(),
                original_path: original_path.into(),
            },
            diff: None,
        }
    }

    /// Returns the underlying vault-file path if this is an editable vault
    /// buffer (i.e. `Editor { buffer: Vault { path }, .. }`).
    pub fn vault_path(&self) -> Option<&str> {
        match self {
            TabKind::Editor { buffer: BufferSource::Vault { path }, .. } => Some(path.as_str()),
            _ => None,
        }
    }

    /// Returns the buffer-source's path for any `Editor` tab (vault path,
    /// snapshot path, proposal target path, trash original path).
    pub fn editor_path(&self) -> Option<&str> {
        match self {
            TabKind::Editor { buffer, .. } => Some(buffer.path()),
            _ => None,
        }
    }

    /// Returns the buffer source if this is an Editor tab.
    pub fn buffer_source(&self) -> Option<&BufferSource> {
        match self {
            TabKind::Editor { buffer, .. } => Some(buffer),
            _ => None,
        }
    }

    /// Returns the diff source if this Editor tab has diff mode active.
    pub fn diff_source(&self) -> Option<&DiffSource> {
        match self {
            TabKind::Editor { diff: Some(d), .. } => Some(d),
            _ => None,
        }
    }

    /// True if this is an Editor tab whose buffer is a read-only preview
    /// (snapshot, staging proposal, or trash entry).
    pub fn is_readonly_preview(&self) -> bool {
        matches!(
            self,
            TabKind::Editor {
                buffer: BufferSource::Snapshot { .. }
                    | BufferSource::StagingProposal { .. }
                    | BufferSource::Trash { .. },
                ..
            }
        )
    }

    pub fn label(&self) -> String {
        match self {
            TabKind::Editor { buffer, diff } => editor_label(buffer, diff.as_ref()),
            TabKind::Home => "Home".to_string(),
            TabKind::HomeDetail { which } => match which {
                HomeDetail::Snapshots => "Snapshots".to_string(),
                HomeDetail::ActivityRow { path } => {
                    format!("History · {}", path_basename(path))
                }
            },
            TabKind::Queue => "Queue".to_string(),
            TabKind::QueueDetail { task_id } => {
                format!("Task · {}", &task_id[..task_id.len().min(8)])
            }
            TabKind::Settings => "Settings".to_string(),
            TabKind::Properties { path } => format!("Properties · {}", path_basename(path)),
            TabKind::Graph => "Graph".to_string(),
            TabKind::Agent { .. } => "Chat".to_string(),
            TabKind::PatchReview => "Patch review".to_string(),
            TabKind::Plugins => "Plugins".to_string(),
            TabKind::IndexerDetail => "Index".to_string(),
            TabKind::Changes => "Changes".to_string(),
            TabKind::ClusterReview { .. } => "Cluster review".to_string(),
            TabKind::ClusterGraph { .. } => "Cluster graph".to_string(),
        }
    }

    /// Single source of truth for the icon associated with this tab
    /// kind. Returned as a fresh `egui::Image` each call so callers can
    /// attach sizing/tint on top.
    pub fn icon(&self) -> eframe::egui::Image<'static> {
        use crate::icons;
        match self {
            TabKind::Editor { buffer, .. } => match buffer {
                BufferSource::Vault { .. } => icons::file(),
                BufferSource::Snapshot { .. } => icons::clock(),
                BufferSource::StagingProposal { .. } => icons::edit(),
                BufferSource::Trash { .. } => icons::trash(),
            },
            TabKind::Home => icons::home(),
            TabKind::HomeDetail { .. } => icons::home(),
            TabKind::Queue => icons::clipboard(),
            TabKind::QueueDetail { .. } => icons::clipboard(),
            TabKind::Settings => icons::settings(),
            TabKind::Properties { .. } => icons::info(),
            TabKind::Graph => icons::graph(),
            TabKind::Agent { .. } => icons::chat(),
            TabKind::PatchReview => icons::robot(),
            TabKind::Plugins => icons::plugin(),
            TabKind::IndexerDetail => icons::compass(),
            TabKind::Changes => icons::clock(),
            TabKind::ClusterReview { .. } => icons::graph(),
            TabKind::ClusterGraph { .. } => icons::graph(),
        }
    }
}

fn editor_label(buffer: &BufferSource, _diff: Option<&DiffSource>) -> String {
    match buffer {
        BufferSource::Vault { path } => path_basename(path),
        BufferSource::Snapshot { path, .. } => format!("Snapshot · {}", path_basename(path)),
        BufferSource::StagingProposal { target_path, .. } => {
            format!("Staging · {}", path_basename(target_path))
        }
        BufferSource::Trash { original_path, .. } => {
            format!("Trash · {}", path_basename(original_path))
        }
    }
}

impl Tab {
    pub fn label(&self) -> String {
        self.kind.label()
    }

    #[allow(dead_code)]
    pub fn icon(&self) -> eframe::egui::Image<'static> {
        self.kind.icon()
    }

    /// True if the tab kind shows the buffer-scoped chrome (editor
    /// toolbar, status bar). Editor tabs only.
    #[allow(dead_code)]
    pub fn shows_buffer_chrome(&self) -> bool {
        matches!(&self.kind, TabKind::Editor { .. })
    }

    /// Workspace-restore key for this tab: `Some((key, kind_str))` if the
    /// tab survives a restart, `None` if the kind needs payload data we
    /// don't persist (preview-style editor tabs, Properties, Agent,
    /// QueueDetail, HomeDetail, ClusterReview, ClusterGraph).
    pub fn persist_key(&self) -> Option<(String, String)> {
        Some(match &self.kind {
            TabKind::Editor { buffer: BufferSource::Vault { path }, diff: None } => {
                (path.clone(), "buffer".into())
            }
            TabKind::Home => (":home".into(), "home".into()),
            TabKind::Queue => (":queue".into(), "queue".into()),
            TabKind::Settings => (":settings".into(), "settings".into()),
            TabKind::Graph => (":graph".into(), "graph".into()),
            TabKind::PatchReview => (":patch_review".into(), "patch_review".into()),
            TabKind::Plugins => (":plugins".into(), "plugins".into()),
            TabKind::IndexerDetail => (":indexer".into(), "indexer".into()),
            TabKind::Changes => (":changes".into(), "changes".into()),
            // Variants intentionally skipped: HomeDetail, non-Vault Editor
            // buffers, Editor tabs with diff active, QueueDetail,
            // Properties, Agent, ClusterReview, ClusterGraph.
            _ => return None,
        })
    }

    /// Buffer path the tab is about, if any. Used for dirty-marker
    /// lookups and version-dropdown population.
    pub fn buffer_path(&self) -> Option<&str> {
        match &self.kind {
            TabKind::Editor { buffer, .. } => Some(buffer.path()),
            TabKind::Properties { path } => Some(path.as_str()),
            _ => None,
        }
    }
}

fn path_basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}
