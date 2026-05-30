//! Tab kinds (mirrors hiker's `tab-kinds` from docs/editor.md).
//!
//! Each tab carries an opaque `TabId` (stable across the session) plus a
//! `TabKind` payload. App-level state for buffers lives in
//! `AppState::buffers`, keyed by path — the buffer tab kind just stores the
//! buffer source and looks up the buffer when it needs it.
//!
//! The `Editor` variant is the only buffer-backed kind: it carries a
//! `BufferSource` (what's *in* the editor — a vault file, a history version,
//! a pending proposal, or a trash entry) plus an optional `DiffSource`
//! (the comparison target when diff mode is active). Diff is a mode of
//! this tab, not a separate kind.

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TabId(pub u64);

/// Identifier for a registered panel mounted in the dock. Stable string
/// so layout files survive panel-registry shuffles.
pub type PanelId = &'static str;

// Stable panel-id vocabulary. These equal the corresponding feature ids
// and are referenced by the activity-bar toggle actions
// (`actions::toggle_panel`) and a few features' reveal calls. The values
// match `Feature::id()`; `vault`/`trash` have no remaining const callers
// (their toggles route through the feature id directly), so they aren't
// listed here.
pub const PANEL_FILES: PanelId = "files";
pub const PANEL_CLUSTERS: PanelId = "clusters";
pub const PANEL_TRAILS: PanelId = "trails";
pub const PANEL_SEARCH: PanelId = "search";
pub const PANEL_RELATED: PanelId = "related";
pub const PANEL_BACKLINKS: PanelId = "backlinks";
pub const PANEL_CHAT: PanelId = "chat";

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
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferSource {
    /// Vault file — editable, dirty-tracked, autosaved.
    Vault { path: String },
    /// Historical version materialized from the op log — read-only.
    /// `op_id` is the accepted op (ulid) the content is reconstructed at.
    HistoryVersion { path: String, op_id: String },
    /// Pending op-log proposal content — read-only.
    PendingProposal { proposal_id: String, target_path: String },
    /// Trash entry — read-only.
    Trash { trash_path: String, original_path: String },
}

impl BufferSource {
    /// The vault-relative path the source identifies (for dirty-marker
    /// lookups, version-dropdown population, reveal-in-tree, etc.).
    pub fn path(&self) -> &str {
        match self {
            BufferSource::Vault { path } => path,
            BufferSource::HistoryVersion { path, .. } => path,
            BufferSource::PendingProposal { target_path, .. } => target_path,
            BufferSource::Trash { original_path, .. } => original_path,
        }
    }
}

/// The "other side" of a diff. Resolves through existing services to a
/// rope at render time.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// On-disk text at the given vault-relative path.
    Disk { path: String },
    /// Another open buffer's live text.
    LiveBuffer { path: String },
    /// A historical version's content materialized from the op log
    /// (`content_at_op(path, op_id)`); `path` is the vault-relative path
    /// the op touched, retained for restore.
    HistoryVersion { op_id: String, path: String },
    /// Pending op-log proposal's stored before-text or content.
    PendingProposal { proposal_id: String },
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
    /// Editor tab: a buffer (vault file, history version, proposal, or trash
    /// entry) optionally layered with a diff against another source.
    Editor { buffer: BufferSource, diff: Option<DiffSource> },
    Home,
    /// "Recent activity" / "Version history" home-detail sub-page.
    HomeDetail { which: HomeDetail },
    Queue,
    QueueDetail { task_id: String },
    Settings,
    Properties { path: String },
    /// Vault-wide graph view (deferred — placeholder for v1).
    Graph,
    /// Board: a per-doc kanban view over a curated board-doc at `path`.
    /// Columns + card refs come from the board-doc frontmatter; a card move
    /// rewrites that frontmatter via the op-log. Per-doc (like the cluster
    /// tabs), not a singleton. See `docs/kanban.md`.
    ///
    /// status: board-view
    Board { path: String },
    /// Boards index: a singleton meta-page listing every board-doc in the
    /// vault (title + column/card counts) with click-to-open, New board,
    /// and per-row Delete. A non-buffer app-page like Home / Queue, since
    /// boards are per-doc and have no single home tab. See `docs/kanban.md`.
    ///
    /// status: board-index-page
    BoardsIndex,
    /// Chat session as a full tab (vs. docked at bottom of discovery).
    Agent { session_id: String },
    /// Patch review: lists pending proposals with accept/reject.
    PatchReview,
    /// Plugins host (stub): lists `.hiker/plugins.json` entries.
    Plugins,
    /// Indexer detail / control: model id, status, reindex.
    IndexerDetail,
    /// Sync detail / control: device fingerprint, enrollment, force-sync,
    /// discovery, recent synced items.
    Sync,
    /// Unified activity / changes feed: every pending op-log proposal
    /// plus every accepted op, with author + op + source filter chips.
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
    VersionHistory,
    /// Per-row version history view: lists every accepted op that
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

    /// Construct a history-version preview editor tab. The buffer holds the
    /// version's content read-only; the diff layer shows how the
    /// version differs from the current on-disk text of the same path.
    pub fn version_preview(path: impl Into<String>, op_id: impl Into<String>) -> Self {
        let p = path.into();
        TabKind::Editor {
            buffer: BufferSource::HistoryVersion { path: p.clone(), op_id: op_id.into() },
            diff: Some(DiffSource::Disk { path: p }),
        }
    }

    /// Construct a pending-proposal review tab. The buffer holds the
    /// proposed full-file content read-only; the diff layer shows how it
    /// would change the current on-disk text of the target path.
    pub fn pending_preview(proposal_id: impl Into<String>, target_path: impl Into<String>) -> Self {
        let t = target_path.into();
        TabKind::Editor {
            buffer: BufferSource::PendingProposal {
                proposal_id: proposal_id.into(),
                target_path: t.clone(),
            },
            diff: Some(DiffSource::Disk { path: t }),
        }
    }

    /// Construct a read-only trash-preview editor tab. The buffer holds
    /// the trashed file's on-disk content; no diff layer.
    pub fn trash_preview(
        trash_path: impl Into<String>,
        original_path: impl Into<String>,
    ) -> Self {
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

    /// Returns the diff source if this Editor tab has diff mode active.
    pub const fn diff_source(&self) -> Option<&DiffSource> {
        match self {
            TabKind::Editor { diff: Some(d), .. } => Some(d),
            _ => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            // Editor-tab label is the buffer's basename, prefixed by source
            // kind for non-vault (read-only) sources. The diff toggle
            // doesn't change the label.
            TabKind::Editor { buffer, .. } => match buffer {
                BufferSource::Vault { path } => path_basename(path),
                BufferSource::HistoryVersion { path, .. } => {
                    format!("Version · {}", path_basename(path))
                }
                BufferSource::PendingProposal { target_path, .. } => {
                    format!("Pending · {}", path_basename(target_path))
                }
                BufferSource::Trash { original_path, .. } => {
                    format!("Trash · {}", path_basename(original_path))
                }
            },
            TabKind::Home => "Home".to_string(),
            TabKind::HomeDetail { which } => match which {
                HomeDetail::VersionHistory => "Version history".to_string(),
                HomeDetail::ActivityRow { path } => {
                    format!("Version history · {}", path_basename(path))
                }
            },
            TabKind::Queue => "Queue".to_string(),
            TabKind::QueueDetail { task_id } => {
                format!("Task · {}", &task_id[..task_id.len().min(8)])
            }
            TabKind::Settings => "Settings".to_string(),
            TabKind::Properties { path } => format!("Properties · {}", path_basename(path)),
            TabKind::Graph => "Graph".to_string(),
            TabKind::Board { path } => format!("Board · {}", path_basename(path)),
            TabKind::BoardsIndex => "Boards".to_string(),
            TabKind::Agent { .. } => "Chat".to_string(),
            TabKind::PatchReview => "Patch review".to_string(),
            TabKind::Plugins => "Plugins".to_string(),
            TabKind::IndexerDetail => "Index".to_string(),
            TabKind::Sync => "Sync".to_string(),
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
                BufferSource::Vault { .. } => icons::ICONS.image(crate::icons::Icon::File),
                BufferSource::HistoryVersion { .. } => icons::ICONS.image(crate::icons::Icon::Clock),
                BufferSource::PendingProposal { .. } => icons::ICONS.image(crate::icons::Icon::Edit),
                BufferSource::Trash { .. } => icons::ICONS.image(crate::icons::Icon::Trash),
            },
            TabKind::Home => icons::ICONS.image(crate::icons::Icon::Home),
            TabKind::HomeDetail { .. } => icons::ICONS.image(crate::icons::Icon::Home),
            TabKind::Queue => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::QueueDetail { .. } => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::Settings => icons::ICONS.image(crate::icons::Icon::Settings),
            TabKind::Properties { .. } => icons::ICONS.image(crate::icons::Icon::Info),
            TabKind::Graph => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::Board { .. } => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::BoardsIndex => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::Agent { .. } => icons::ICONS.image(crate::icons::Icon::Chat),
            TabKind::PatchReview => icons::ICONS.image(crate::icons::Icon::Robot),
            TabKind::Plugins => icons::ICONS.image(crate::icons::Icon::Plugin),
            TabKind::IndexerDetail => icons::ICONS.image(crate::icons::Icon::Compass),
            // No dedicated sync glyph in the icon set; `Restore` is the
            // circular-arrow "refresh" mark, the closest fit for sync.
            TabKind::Sync => icons::ICONS.image(crate::icons::Icon::Restore),
            TabKind::Changes => icons::ICONS.image(crate::icons::Icon::Clock),
            TabKind::ClusterReview { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::ClusterGraph { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
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
    pub const fn shows_buffer_chrome(&self) -> bool {
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
            // Board tabs are per-doc: persist the board-doc path so the
            // tab reopens in board view on restore (the "board:" prefix
            // disambiguates from a plain buffer tab on the same path).
            // status: board-view
            TabKind::Board { path } => (format!("board:{path}"), "board".into()),
            // Singleton Boards index page. status: board-index-page
            TabKind::BoardsIndex => (":boards_index".into(), "boards_index".into()),
            TabKind::PatchReview => (":patch_review".into(), "patch_review".into()),
            TabKind::Plugins => (":plugins".into(), "plugins".into()),
            TabKind::IndexerDetail => (":indexer".into(), "indexer".into()),
            TabKind::Sync => (":sync".into(), "sync".into()),
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
