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

// ---- Static panel records -----------------------------------------------
//
// The `render` field is `fn(...)` — a bare function pointer. Non-
// capturing closures coerce to that type, so we use them directly in
// place of named per-panel render-shim functions (each of which would
// otherwise be a single-call free fn flagged by `clippy::single_call_fn`).

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
    render: |ui, app, _rt| crate::sidebar::files::FilesView { ui, state: app }.show(),
};
static P_CLUSTERS: DockPanel = DockPanel {
    id: PANEL_CLUSTERS,
    title: "Clusters",
    default_side: PanelSide::Left,
    render: |ui, app, _rt| crate::sidebar::PanelRender { ui, state: app }.clusters(),
};
static P_TRAILS: DockPanel = DockPanel {
    id: PANEL_TRAILS,
    title: "Trails",
    default_side: PanelSide::Left,
    render: |ui, app, _rt| crate::sidebar::PanelRender { ui, state: app }.trails(),
};
static P_SEARCH: DockPanel = DockPanel {
    id: PANEL_SEARCH,
    title: "Search",
    default_side: PanelSide::Right,
    render: |ui, app, _rt| {
        egui::ScrollArea::vertical()
            .id_salt("panel-search-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                crate::panels::search::View { ui, app }.show();
            });
    },
};
static P_RELATED: DockPanel = DockPanel {
    id: PANEL_RELATED,
    title: "Related",
    default_side: PanelSide::Right,
    render: |ui, app, _rt| {
        egui::ScrollArea::vertical()
            .id_salt("panel-related-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                crate::panels::related::View { ui, app }.show();
            });
    },
};
static P_BACKLINKS: DockPanel = DockPanel {
    id: PANEL_BACKLINKS,
    title: "Backlinks",
    default_side: PanelSide::Right,
    render: |ui, app, _rt| {
        egui::ScrollArea::vertical()
            .id_salt("panel-backlinks-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                crate::panels::backlinks::View { ui, app }.show();
            });
    },
};
static P_CHAT: DockPanel = DockPanel {
    id: PANEL_CHAT,
    title: "Chat",
    default_side: PanelSide::Right,
    render: |ui, app, rt| {
        if !app.session.chat_discovered {
            let vault_root = app.vault_session.vault_root.clone();
            crate::chat::session::discover(&mut app.session.chat, &vault_root);
            app.session.chat_discovered = true;
        }
        crate::chat::render::show(ui, app, None, crate::chat::render::Layout::SideBar, rt);
    },
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
