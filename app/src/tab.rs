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

// Stable panel-id vocabulary. These equal the corresponding activity ids
// and are referenced by the activity-bar toggle actions
// (`actions::toggle_panel`) and a few activities' reveal calls. The values
// match `Activity::id()`; `vault`/`trash` have no remaining const callers
// (their toggles route through the activity id directly), so they aren't
// listed here.
pub const PANEL_FILES: PanelId = "files";
pub const PANEL_CLUSTERS: PanelId = "clusters";
pub const PANEL_TRAILS: PanelId = "trails";
pub const PANEL_SEARCH: PanelId = "search";
/// The `context` container (backlinks + related) activity-bar mode. The
/// per-note discovery panels toggle the whole container; `backlinks` and
/// `related` are now `View`s inside it (`"context/backlinks"` /
/// `"context/related"`), not standalone activity ids.
pub const PANEL_CONTEXT: PanelId = "context";

#[derive(Debug, Clone)]
pub struct Tab {
    pub id: TabId,
    pub kind: TabKind,
    /// Sticky = user signalled intent to keep this tab open. Preview tabs
    /// are non-sticky and live in `AppState::preview_tab`.
    pub sticky: bool,
    /// Cross-tab wiring for viz tabs (graph / canvas). A `target` makes a
    /// node-click open the note into another tab group (DRIVE); a `source`
    /// makes the tab follow whatever note is active in another group
    /// (FOLLOW). Empty (the default) means the tab is self-contained.
    /// status: tab-linking
    pub link: TabLink,
}

impl Tab {
    /// Construct a self-contained (unlinked) tab. The common case — every
    /// non-link-aware open path uses this so the `link` default stays in one
    /// place. status: tab-linking
    pub const fn new(id: TabId, kind: TabKind, sticky: bool) -> Self {
        Self { id, kind, sticky, link: TabLink::new() }
    }
}

/// A reference to another open tab or tab group, the endpoint of a
/// [`TabLink`]. `GroupId`/`TileId` is the workbench's per-window group
/// handle; it is NOT stable across restart, so persisted links re-resolve
/// through a path key (see `Tab::persist_key` / the autosave snapshot).
/// status: tab-linking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRef {
    /// A specific tab, by id.
    Tab(TabId),
    /// A tab group (editor split), by workbench group handle.
    Group(egui_workbench::workspace::GroupId),
}

/// A viz tab's cross-tab wiring. `source` drives FOLLOW (this tab highlights
/// whatever note is active in the referenced group/tab); `target` drives
/// DRIVE (this tab's node-clicks open into the referenced group instead of
/// its own preview slot). Either may be set independently; both `None` is a
/// self-contained tab. status: tab-linking
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabLink {
    /// The group/tab this tab FOLLOWS (reads the active note from).
    pub source: Option<LinkRef>,
    /// The group/tab this tab DRIVES (opens clicked notes into).
    pub target: Option<LinkRef>,
}

impl TabLink {
    /// An empty link (no source, no target) — the default for every tab.
    pub const fn new() -> Self {
        Self { source: None, target: None }
    }
}

/// What's in an editor tab's buffer. Each variant maps to a different
/// loading path and a different read/write posture.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferSource {
    /// Vault file — editable, dirty-tracked, autosaved.
    Vault { path: String },
    /// Historical version materialized from the layered doc — read-only.
    /// `op_id` is the accepted op (ulid) the content is reconstructed at.
    HistoryVersion { path: String, op_id: String },
    /// Pending layered-doc proposal content — read-only.
    PendingProposal { proposal_id: String, target_path: String },
    /// Trash entry — read-only.
    Trash { trash_path: String, original_path: String },
    /// A code file (`.rs`, `.py`, …) opened from the vault as read-only
    /// reference content. Never editable, never autosaved/layered-doc-tracked — code
    /// is reference content, only `.md` notes are authored. `path` is the
    /// vault-relative path. status: code-read-only-view
    CodeFile { path: String },
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
            BufferSource::CodeFile { path } => path,
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
    /// A historical version's content materialized from the layered doc
    /// (`content_at_op(path, op_id)`); `path` is the vault-relative path
    /// the op touched, retained for restore.
    HistoryVersion { op_id: String, path: String },
    /// Pending layered-doc proposal's stored before-text or content.
    PendingProposal { proposal_id: String },
    /// The file's content at a git revision (`GitBackend::show` through the
    /// git transport engine). `rev` is anything `git rev-parse` accepts; a
    /// path absent at the rev resolves to an empty base so the whole file
    /// reads as added. status: diff-source-git-ref
    GitRef { rev: String, path: String },
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
    /// Vault-wide graph view. Singleton (one global tab, like Home/Queue),
    /// but the kind carries an optional **focus target** — open "focused"
    /// lands the panel in `focus.path`'s depth-bounded neighbourhood instead
    /// of the full-vault overview (mirroring how `CodeGraph` carries its
    /// view-source) — and an optional **query scope** (`graph-scoped-query`):
    /// the query-doc whose matches bound the node universe ("graph of this
    /// smart folder"). Scope and focus are orthogonal and compose — the
    /// scope filters the universe, the focus drills within it. `None`/`None`
    /// is the plain overview open. status: graph-tab-focus
    Graph { focus: Option<GraphFocus>, scope_query: Option<String> },
    /// Board: a per-doc kanban view over a curated board-doc at `path`.
    /// Columns + card refs come from the board-doc frontmatter; a card move
    /// rewrites that frontmatter via the layered doc. Per-doc (like the cluster
    /// tabs), not a singleton. See `docs/kanban.md`.
    ///
    /// status: board-view
    Board { path: String },
    /// Canvas: a per-doc spatial editor over a `.canvas` JSON Canvas document
    /// at `path`. Nodes + edges come from the file's JSON; an edit
    /// re-serializes and persists through the layered doc exactly like a note.
    /// Per-doc (like Board), not a singleton. See `docs/canvas.md`.
    ///
    /// status: canvas-tab
    Canvas { path: String },
    /// Boards index: a singleton meta-page listing every board-doc in the
    /// vault (title + column/card counts) with click-to-open, New board,
    /// and per-row Delete. A non-buffer app-page like Home / Queue, since
    /// boards are per-doc and have no single home tab. See `docs/kanban.md`.
    ///
    /// status: board-index-page
    BoardsIndex,
    /// Patch review: lists pending proposals with accept/reject.
    PatchReview,
    /// Rules panel: every registered vault rule (name, trigger, enabled
    /// state, last firing) expanding to its recent firings off the layered-doc
    /// author projection, plus failed firings from the engine's
    /// diagnostics ring. Read-only in v1 — the TOML is the editing
    /// surface. Singleton, like Changes. See `docs/rules.md`.
    ///
    /// status: rule-firings-panel
    Rules,
    /// Indexer detail / control: model id, status, reindex.
    IndexerDetail,
    /// Git diff summary: a read-only viewer over the vault repo — pick a
    /// base rev (and optionally a head rev), see the changed paths, click a
    /// row to open the file with the `GitRef` diff overlay. Singleton, like
    /// Changes. status: diff-summary-panel
    GitDiff,
    /// Cluster Review tab: two-phase preview-then-persist for a fresh
    /// cluster build over the vault. Payload is the build configuration;
    /// the tab body holds a draft tree until the user persists.
    ClusterReview { config_json: String },
    /// Cluster tree visualised as a radial dendrogram. Payload is the
    /// `tree_id` to render.
    ClusterGraph { tree_id: String },
    /// Code graph: a code source rendered as a precise entity graph through the shared graph engine.
    /// The source is either a project note (`hiker.kind: project`, binds a repo descriptor) or a
    /// `.scip` index opened directly from the file tree (no project note). Per-source (like Board),
    /// not a singleton. See `docs/hiker-code.md` `code-graph-view-source`.
    CodeGraph { source: CodeSource },
    /// Project-config form: author/edit a project note via UI (sources → save). `source_note` is
    /// `Some(path)` when editing an existing project note, `None` for a new one. Per-form state on
    /// `AppState::panels.project_config`, keyed by tab id.
    ProjectConfig { source_note: Option<String> },
    /// ZIM viewer: an offline `.zim` archive (e.g. a Wikipedia export)
    /// rendered as HTML via the `hiker-htmlview` renderer. `zim_path`
    /// is the archive's vault-relative path; `article` is the currently
    /// shown article (`None` = the archive's main page). Clicking an
    /// in-archive link navigates within this same tab. Per-archive, keyed
    /// by path (like Board). status: zim-view
    ZimView { zim_path: String, article: Option<String> },
    /// Chart builder: the `hiker-charts` builder + live preview. Two sources
    /// (see [`ChartSource`]): a `.csv` opened directly (Export copies a ```chart
    /// block) or an inline ```chart block opened from a note for edit (Save
    /// splices the regenerated block back). See `panels::charts_tab`.
    /// status: chart-csv-tab, chart-open-in-builder
    ChartBuilder { source: ChartSource },
}

/// Display scope for the code-graph view: the whole graph (`Overview`) or the 1–3-hop
/// neighbourhood of the **selected** node. Selection and scope are orthogonal — clicking always
/// selects; the scope dial decides whether the display recenters on it. Lives here (beside
/// [`CodeSource`]) so both `state::NavTarget` and the code-graph panel share the same type.
/// status: code-graph-scope-hops
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Overview,
    /// 1–3 hops around the selected node (the panel clamps the count).
    Hops(u8),
}

/// A [`TabKind::Graph`] tab's optional focus target: the note whose
/// depth-bounded neighbourhood the panel opens on (the "Open in graph"
/// dispatch target). The vault analogue of [`CodeSource`]'s view-source
/// payload. status: graph-tab-focus
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFocus {
    /// Vault-relative note path of the focus anchor.
    pub path: String,
    /// Neighbourhood depth in hops (the panel clamps to the dial's 1–3).
    pub depth: u8,
}

impl GraphFocus {
    /// Workspace-restore key for a focused graph tab: `graph:<depth>:<path>`
    /// (the unfocused singleton keeps its historical `:graph` key).
    /// status: graph-tab-focus
    pub fn persist_key(&self) -> String {
        format!("graph:{}:{}", self.depth, self.path)
    }

    /// Parse a `graph:<depth>:<path>` restore key back into a focus target;
    /// `None` for malformed keys (the restore path then skips the tab).
    pub fn from_persist_key(key: &str) -> Option<Self> {
        let rest = key.strip_prefix("graph:")?;
        let (depth, path) = rest.split_once(':')?;
        let depth = depth.parse().ok()?;
        (!path.is_empty()).then(|| Self { path: path.to_string(), depth })
    }
}

/// What a [`TabKind::CodeGraph`] tab is viewing. status: code-graph-view-source
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeSource {
    /// A project note (`hiker.kind: project`); its `repo` source descriptor is bound to a SCIP
    /// adapter. Vault-relative note path.
    Project(String),
    /// A `.scip` index opened directly (no project note). Vault-relative path; the repo root for
    /// previews defaults to the index's own directory.
    Index(String),
}

impl CodeSource {
    /// The vault-relative path this source points at (note or `.scip`).
    pub fn path(&self) -> &str {
        match self {
            CodeSource::Project(p) | CodeSource::Index(p) => p,
        }
    }
    /// Stable per-source key for the per-tab state map (disambiguates note vs index on same stem).
    pub fn key(&self) -> String {
        match self {
            CodeSource::Project(p) => format!("project:{p}"),
            CodeSource::Index(p) => format!("index:{p}"),
        }
    }
}

/// What a [`TabKind::ChartBuilder`] tab is editing. status: chart-csv-tab
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartSource {
    /// A `.csv` opened directly from the vault. Export → clipboard.
    Csv { path: String },
    /// An inline ```` ```chart ```` block opened from a note for editing. `key`
    /// (the block's open-time byte offset, as a string) disambiguates multiple
    /// charts in one note so each gets its own builder pane. Save splices the
    /// regenerated block back into the note. status: chart-open-in-builder
    NoteBlock { note: String, key: String },
}

impl ChartSource {
    /// Stable per-source key for the builder-pane map + tab identity.
    pub fn pane_key(&self) -> String {
        match self {
            ChartSource::Csv { path } => format!("csv:{path}"),
            ChartSource::NoteBlock { note, key } => format!("note:{note}#{key}"),
        }
    }

    /// The vault path the source is about (the CSV, or the host note).
    pub fn host_path(&self) -> &str {
        match self {
            ChartSource::Csv { path } => path,
            ChartSource::NoteBlock { note, .. } => note,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HomeDetail {
    /// Per-note version history view: lists every plain-file snapshot of the
    /// given vault-relative path, newest first.
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

    /// Construct a git-diff editor tab (the diff-summary panel's row open):
    /// the buffer holds the live vault file; the diff layer shows how it
    /// differs from the file's content at git rev `rev`.
    /// status: diff-source-git-ref
    pub fn git_diff_preview(path: impl Into<String>, rev: impl Into<String>) -> Self {
        let p = path.into();
        TabKind::Editor {
            buffer: BufferSource::Vault { path: p.clone() },
            diff: Some(DiffSource::GitRef { rev: rev.into(), path: p }),
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

    /// Construct a read-only code-file editor tab. The buffer holds the code
    /// file's content read-only (plain text, no syntax highlighting in this
    /// phase); no diff layer. status: code-read-only-view
    pub fn code_preview(path: impl Into<String>) -> Self {
        TabKind::Editor {
            buffer: BufferSource::CodeFile { path: path.into() },
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
                // Read-only code reference; plain basename like a vault file
                // (the read-only posture is conveyed by the lock-free chrome,
                // not a label prefix). status: code-read-only-view
                BufferSource::CodeFile { path } => path_basename(path),
            },
            TabKind::Home => "Home".to_string(),
            TabKind::HomeDetail { which } => match which {
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
            TabKind::Graph { .. } => "Graph".to_string(),
            TabKind::Board { path } => format!("Board · {}", path_basename(path)),
            TabKind::Canvas { path } => format!("Canvas · {}", path_basename(path)),
            TabKind::BoardsIndex => "Boards".to_string(),
            TabKind::PatchReview => "Patch review".to_string(),
            TabKind::Rules => "Rules".to_string(),
            TabKind::IndexerDetail => "Index".to_string(),
            TabKind::GitDiff => "Git diff".to_string(),
            TabKind::ClusterReview { .. } => "Cluster review".to_string(),
            TabKind::ClusterGraph { .. } => "Cluster graph".to_string(),
            TabKind::CodeGraph { source } => {
                format!("Code graph · {}", path_basename(source.path()))
            }
            TabKind::ProjectConfig { source_note } => match source_note {
                Some(p) => format!("Project · {}", path_basename(p)),
                None => "New project".to_string(),
            },
            TabKind::ZimView { zim_path, .. } => {
                format!("ZIM · {}", path_basename(zim_path))
            }
            TabKind::ChartBuilder { source } => format!("Chart · {}", path_basename(source.host_path())),
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
                // Code reference content — the braces glyph reads as "code".
                // status: code-read-only-view
                BufferSource::CodeFile { .. } => icons::ICONS.image(crate::icons::Icon::Braces),
            },
            TabKind::Home => icons::ICONS.image(crate::icons::Icon::Home),
            TabKind::HomeDetail { .. } => icons::ICONS.image(crate::icons::Icon::Home),
            TabKind::Queue => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::QueueDetail { .. } => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::Settings => icons::ICONS.image(crate::icons::Icon::Settings),
            TabKind::Properties { .. } => icons::ICONS.image(crate::icons::Icon::Info),
            TabKind::Graph { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::Board { .. } => icons::ICONS.image(crate::icons::Icon::Clipboard),
            // The spatial-graph glyph reads closest for a canvas of nodes +
            // edges in the icon set.
            TabKind::Canvas { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::BoardsIndex => icons::ICONS.image(crate::icons::Icon::Clipboard),
            TabKind::PatchReview => icons::ICONS.image(crate::icons::Icon::Robot),
            // No dedicated automation glyph; the settings gear reads as
            // "configured behavior" for the rules surface.
            TabKind::Rules => icons::ICONS.image(crate::icons::Icon::Settings),
            TabKind::IndexerDetail => icons::ICONS.image(crate::icons::Icon::Compass),
            TabKind::GitDiff => icons::ICONS.image(crate::icons::Icon::Diff),
            TabKind::ClusterReview { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::ClusterGraph { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::CodeGraph { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
            TabKind::ProjectConfig { .. } => icons::ICONS.image(crate::icons::Icon::Wrench),
            // Offline encyclopedia archive — the compass "go read out
            // there, but cached" reads closest in the icon set.
            TabKind::ZimView { .. } => icons::ICONS.image(crate::icons::Icon::Compass),
            // A chart of plotted data reads closest to the spatial-graph glyph
            // in the icon set (same choice as Canvas).
            TabKind::ChartBuilder { .. } => icons::ICONS.image(crate::icons::Icon::Graph),
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
    ///
    /// NOTE: `link` is intentionally NOT round-tripped here. Links reference
    /// a `GroupId` (`TileId`), which is not restart-stable, and groups have no
    /// persisted identity to re-resolve against. For v1 tab-linking is
    /// in-session only. TODO(tab-linking-persist): persist links by the linked
    /// group's active-tab `persist_key`, re-resolving to a `GroupId` after the
    /// workbench layout restores. status: tab-linking
    pub fn persist_key(&self) -> Option<(String, String)> {
        Some(match &self.kind {
            TabKind::Editor { buffer: BufferSource::Vault { path }, diff: None } => {
                (path.clone(), "buffer".into())
            }
            TabKind::Home => (":home".into(), "home".into()),
            TabKind::Queue => (":queue".into(), "queue".into()),
            TabKind::Settings => (":settings".into(), "settings".into()),
            // The unfocused singleton keeps its historical `:graph` key; a
            // focused tab round-trips its target through the prefixed key so
            // the focus param survives a restart, and a query-scoped tab
            // through `graphq:<query-path>` (scope outranks focus in the key
            // — the LANDING state restores via the persisted view-state
            // record either way, the graph-tab-focus posture).
            // status: graph-tab-focus, graph-scoped-query
            TabKind::Graph { scope_query: Some(q), .. } => {
                (format!("graphq:{q}"), "graph".into())
            }
            TabKind::Graph { focus: None, scope_query: None } => {
                (":graph".into(), "graph".into())
            }
            TabKind::Graph { focus: Some(f), scope_query: None } => {
                (f.persist_key(), "graph".into())
            }
            // Board tabs are per-doc: persist the board-doc path so the
            // tab reopens in board view on restore (the "board:" prefix
            // disambiguates from a plain buffer tab on the same path).
            // status: board-view
            TabKind::Board { path } => (format!("board:{path}"), "board".into()),
            // Canvas tabs are per-doc: persist the `.canvas` path so the tab
            // reopens in canvas view on restore (the "canvas:" prefix
            // disambiguates from a plain buffer tab on the same path).
            // status: canvas-tab
            TabKind::Canvas { path } => (format!("canvas:{path}"), "canvas".into()),
            // Singleton Boards index page. status: board-index-page
            TabKind::BoardsIndex => (":boards_index".into(), "boards_index".into()),
            // Singleton Rules panel. status: rule-firings-panel
            TabKind::Rules => (":rules".into(), "rules".into()),
            // ZIM tabs are per-archive: persist the archive path so the tab
            // reopens on the main page after restart. The current article
            // (if any) is intentionally not persisted — restore lands on the
            // archive's main page. status: zim-view
            TabKind::ZimView { zim_path, article: None } => {
                (format!("zim:{zim_path}"), "zim".into())
            }
            // A CSV chart-builder tab is per-CSV: persist the path so it reopens
            // in the builder on restore (the "chart:" prefix disambiguates from a
            // plain buffer tab). A note-block builder is ephemeral (its source is
            // a transient byte offset) — it isn't persisted. status: chart-csv-tab
            TabKind::ChartBuilder { source: ChartSource::Csv { path } } => {
                (format!("chart:{path}"), "chart".into())
            }
            TabKind::PatchReview => (":patch_review".into(), "patch_review".into()),
            TabKind::IndexerDetail => (":indexer".into(), "indexer".into()),
            // Singleton git diff-summary page. status: diff-summary-panel
            TabKind::GitDiff => (":git_diff".into(), "git_diff".into()),
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
            // A canvas tab is about its `.canvas` vault path, so tab-switch nav
            // and dirty-marker lookups resolve it. status: canvas-nav-stack
            TabKind::Canvas { path } => Some(path.as_str()),
            // A chart-builder tab is about its host vault path (the CSV, or the
            // note an inline block lives in). status: chart-csv-tab
            TabKind::ChartBuilder { source } => Some(source.host_path()),
            _ => None,
        }
    }
}

fn path_basename(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

#[cfg(test)]
mod tests {
    use super::{GraphFocus, Tab, TabId, TabKind};

    /// The focused graph tab's persist key round-trips its focus target
    /// (path + depth); the unfocused singleton keeps the historical `:graph`
    /// key; malformed keys parse to `None`. status: graph-tab-focus
    #[test]
    fn graph_focus_persist_key_round_trips() {
        let focus = GraphFocus { path: "notes/a board.md".to_string(), depth: 2 };
        let tab = Tab::new(
            TabId(1),
            TabKind::Graph { focus: Some(focus.clone()), scope_query: None },
            true,
        );
        let (key, kind) = tab.persist_key().expect("focused graph tab persists");
        assert_eq!(kind, "graph");
        assert_eq!(key, "graph:2:notes/a board.md");
        assert_eq!(GraphFocus::from_persist_key(&key), Some(focus));

        let plain = Tab::new(TabId(2), TabKind::Graph { focus: None, scope_query: None }, true);
        assert_eq!(plain.persist_key(), Some((":graph".to_string(), "graph".to_string())));

        // A query-scoped tab persists through the `graphq:` key — scope
        // outranks focus in the key; the landing restores via the view-state
        // record. status: graph-scoped-query
        let scoped = Tab::new(
            TabId(3),
            TabKind::Graph {
                focus: Some(GraphFocus { path: "n.md".to_string(), depth: 1 }),
                scope_query: Some("queries/rust.md".to_string()),
            },
            true,
        );
        assert_eq!(
            scoped.persist_key(),
            Some(("graphq:queries/rust.md".to_string(), "graph".to_string()))
        );

        assert_eq!(GraphFocus::from_persist_key("graph:nope"), None);
        assert_eq!(GraphFocus::from_persist_key("graph:x:notes/a.md"), None);
        assert_eq!(GraphFocus::from_persist_key("graph:2:"), None);
        assert_eq!(GraphFocus::from_persist_key(":graph"), None);
    }
}
