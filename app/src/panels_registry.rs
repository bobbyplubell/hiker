//! Registry of "panel" surfaces mountable in the central dock.
//!
//! A *panel* is a sidebar/discovery-style surface (Files, Clusters,
//! Trails, Search, Related, Backlinks, Chat). Unlike `TabKind` tabs
//! (which represent open buffers/pages and live in `Session::tabs`),
//! panels are static: their lifetime is the whole app session and they
//! identify themselves by a stable `PanelId` so layout files survive
//! refactors.
//!
//! The dock holds `DockTab::Tab(TabId)` and `DockTab::Panel(String)`.
//! When the viewer encounters a `Panel`, it looks the id up here and
//! invokes the render fn.

use std::sync::Arc;
use std::sync::LazyLock;

use eframe::egui;

use crate::state::AppState;
use crate::tab::PanelId;

/// Default-layout placement hint for a panel — used when the saved
/// layout is missing the panel and the bootstrap inserts it on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    Left,
    Right,
    #[allow(dead_code)]
    Center,
}

pub struct DockPanel {
    pub id: PanelId,
    pub title: &'static str,
    pub default_side: PanelSide,
    /// Render fn. Takes the runtime explicitly so chat/search can spawn
    /// tasks. Most panels ignore the runtime arg.
    pub render: fn(&mut egui::Ui, &mut AppState, &Arc<tokio::runtime::Runtime>),
}

pub struct PanelRegistry {
    panels: Vec<&'static DockPanel>,
}

impl PanelRegistry {
    pub fn all() -> &'static PanelRegistry {
        &REGISTRY
    }

    pub fn by_id(&self, id: &str) -> Option<&'static DockPanel> {
        self.panels.iter().copied().find(|p| p.id == id)
    }

    pub fn list(&self) -> &[&'static DockPanel] {
        &self.panels
    }
}

// ---- Render shims -------------------------------------------------------

fn render_files(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    crate::sidebar::files_panel(ui, app, rt);
}

fn render_clusters(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    crate::sidebar::clusters_panel(ui, app, rt);
}

fn render_trails(
    ui: &mut egui::Ui,
    app: &mut AppState,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    crate::sidebar::trails_panel(ui, app);
}

fn render_search(
    ui: &mut egui::Ui,
    app: &mut AppState,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    egui::ScrollArea::vertical()
        .id_salt("panel-search-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            crate::panels::search::show(ui, app);
        });
}

fn render_related(
    ui: &mut egui::Ui,
    app: &mut AppState,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    egui::ScrollArea::vertical()
        .id_salt("panel-related-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            crate::panels::related::show(ui, app);
        });
}

fn render_backlinks(
    ui: &mut egui::Ui,
    app: &mut AppState,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    egui::ScrollArea::vertical()
        .id_salt("panel-backlinks-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            crate::panels::backlinks::show(ui, app);
        });
}

fn render_chat(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    if !app.session.chat_discovered {
        let vault_root = app.vault_session.vault_root.clone();
        crate::chat::session::discover(&mut app.session.chat, &vault_root);
        app.session.chat_discovered = true;
    }
    crate::chat::render::show(ui, app, None, crate::chat::render::Layout::FullTab, rt);
}

// ---- Static panel records -----------------------------------------------

pub const PANEL_FILES: PanelId = "files";
pub const PANEL_CLUSTERS: PanelId = "clusters";
pub const PANEL_TRAILS: PanelId = "trails";
pub const PANEL_SEARCH: PanelId = "search";
pub const PANEL_RELATED: PanelId = "related";
pub const PANEL_BACKLINKS: PanelId = "backlinks";
pub const PANEL_CHAT: PanelId = "chat";

static P_FILES: DockPanel = DockPanel {
    id: PANEL_FILES,
    title: "Files",
    default_side: PanelSide::Left,
    render: render_files,
};
static P_CLUSTERS: DockPanel = DockPanel {
    id: PANEL_CLUSTERS,
    title: "Clusters",
    default_side: PanelSide::Left,
    render: render_clusters,
};
static P_TRAILS: DockPanel = DockPanel {
    id: PANEL_TRAILS,
    title: "Trails",
    default_side: PanelSide::Left,
    render: render_trails,
};
static P_SEARCH: DockPanel = DockPanel {
    id: PANEL_SEARCH,
    title: "Search",
    default_side: PanelSide::Right,
    render: render_search,
};
static P_RELATED: DockPanel = DockPanel {
    id: PANEL_RELATED,
    title: "Related",
    default_side: PanelSide::Right,
    render: render_related,
};
static P_BACKLINKS: DockPanel = DockPanel {
    id: PANEL_BACKLINKS,
    title: "Backlinks",
    default_side: PanelSide::Right,
    render: render_backlinks,
};
static P_CHAT: DockPanel = DockPanel {
    id: PANEL_CHAT,
    title: "Chat",
    default_side: PanelSide::Right,
    render: render_chat,
};

static ALL: &[&DockPanel] = &[
    &P_FILES,
    &P_CLUSTERS,
    &P_TRAILS,
    &P_SEARCH,
    &P_RELATED,
    &P_BACKLINKS,
    &P_CHAT,
];

static REGISTRY: LazyLock<PanelRegistry> = LazyLock::new(|| PanelRegistry {
    panels: ALL.to_vec(),
});
