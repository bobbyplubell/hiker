//! Registry of "panel" surfaces rendered in the workbench side bars.
//!
//! A *panel* is a sidebar/discovery-style surface (Files, Clusters,
//! Trails, Search, Related, Backlinks, Chat). Unlike `TabKind` tabs
//! (which represent open buffers/pages and live in `Session::tabs`),
//! panels are static: their lifetime is the whole app session and they
//! identify themselves by a stable `PanelId`.
//!
//! The workbench host (`workbench_host`) maps each activity-bar
//! `HikerMode` to a panel id and looks the renderer up here.

use std::sync::Arc;
use std::sync::LazyLock;

use eframe::egui;

use crate::state::AppState;
use crate::tab::PanelId;

pub struct DockPanel {
    pub id: PanelId,
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
pub const PANEL_VAULT: PanelId = "vault";
pub const PANEL_SEARCH: PanelId = "search";
pub const PANEL_RELATED: PanelId = "related";
pub const PANEL_BACKLINKS: PanelId = "backlinks";
pub const PANEL_CHAT: PanelId = "chat";
pub const PANEL_TRASH: PanelId = "trash";

static P_FILES: DockPanel = DockPanel {
    id: PANEL_FILES,
    render: |ui, app, _rt| crate::sidebar::files::FilesView { ui, state: app }.show(),
};
static P_CLUSTERS: DockPanel = DockPanel {
    id: PANEL_CLUSTERS,
    render: |ui, app, _rt| crate::clusters::render_sidebar(ui, app),
};
static P_TRAILS: DockPanel = DockPanel {
    id: PANEL_TRAILS,
    render: |ui, app, _rt| crate::trails::render_sidebar(ui, app),
};
static P_VAULT: DockPanel = DockPanel {
    id: PANEL_VAULT,
    render: |ui, app, _rt| crate::vault_view::render_sidebar(ui, app),
};
static P_SEARCH: DockPanel = DockPanel {
    id: PANEL_SEARCH,
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
    render: |ui, app, _rt| {
        egui::ScrollArea::vertical()
            .id_salt("panel-backlinks-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                crate::panels::backlinks::View { ui, app }.show();
            });
    },
};
static P_TRASH: DockPanel = DockPanel {
    id: PANEL_TRASH,
    render: |ui, app, _rt| crate::sidebar::trash::TrashView { ui, state: app }.show(),
};
static P_CHAT: DockPanel = DockPanel {
    id: PANEL_CHAT,
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
    &P_VAULT,
    &P_SEARCH,
    &P_RELATED,
    &P_BACKLINKS,
    &P_CHAT,
    &P_TRASH,
];

static REGISTRY: LazyLock<PanelRegistry> = LazyLock::new(|| PanelRegistry {
    panels: ALL.to_vec(),
});
