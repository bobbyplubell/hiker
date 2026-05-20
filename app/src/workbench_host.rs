//! Hiker ↔ `egui_workbench` bridge.
//!
//! Owns the host-side `Workbench` integration: the activity-mode enum,
//! the per-frame `WorkbenchBehavior` adapter, and the sync helper that
//! keeps `AppState::session.tabs` (the canonical list of open buffers
//! and pages) in lock-step with the workbench's editor area.
//!
//! Tab payloads inside the workbench are intentionally a thin
//! view-model ([`HikerWbTab`]). The real tab data lives in
//! `Session::tabs` / `Session::buffers`; the view-model carries just
//! enough cached state (label, dirty flag) for the workbench's
//! tab-strip rendering to be self-sufficient. We refresh it each frame
//! before calling `Workbench::ui`.

use std::sync::Arc;

use eframe::egui;
use egui_workbench::{
    ActivityItem, DocumentTab, OpenTabOptions, TabState, TabUiContext, Workbench,
    WorkbenchBehavior,
};

use crate::icons;
use crate::panels;
use crate::panels_registry::{
    PANEL_BACKLINKS, PANEL_CHAT, PANEL_CLUSTERS, PANEL_FILES, PANEL_RELATED, PANEL_SEARCH,
    PANEL_TRAILS, PanelRegistry,
};
use crate::state::AppState;
use crate::tab::{HomeDetail, TabId, TabKind};

/// Activity-bar mode. Each variant maps to one entry in the left-edge
/// activity strip and selects the corresponding sidebar content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HikerMode {
    Files,
    Clusters,
    Trails,
    Search,
    Related,
    Backlinks,
}

impl HikerMode {
    fn label(&self) -> &'static str {
        match self {
            HikerMode::Files => "Files",
            HikerMode::Clusters => "Clusters",
            HikerMode::Trails => "Trails",
            HikerMode::Search => "Search",
            HikerMode::Related => "Related",
            HikerMode::Backlinks => "Backlinks",
        }
    }

    fn panel_id(&self) -> &'static str {
        match self {
            HikerMode::Files => PANEL_FILES,
            HikerMode::Clusters => PANEL_CLUSTERS,
            HikerMode::Trails => PANEL_TRAILS,
            HikerMode::Search => PANEL_SEARCH,
            HikerMode::Related => PANEL_RELATED,
            HikerMode::Backlinks => PANEL_BACKLINKS,
        }
    }

    fn icon(&self) -> egui::Image<'static> {
        match self {
            HikerMode::Files => icons::folder(),
            HikerMode::Clusters => icons::cluster_tree(),
            HikerMode::Trails => icons::walk(),
            HikerMode::Search => icons::search(),
            HikerMode::Related => icons::graph(),
            HikerMode::Backlinks => icons::bookmark(),
        }
    }
}

/// View-model for an editor tab inside the workbench. Carries a
/// `TabId` pointer back into `Session::tabs` plus enough cached state
/// (label, dirty) that the workbench's tab strip can render without
/// consulting `AppState`.
#[derive(Clone)]
pub struct HikerWbTab {
    pub id: TabId,
    pub cached_label: String,
    pub cached_dirty: bool,
}

impl DocumentTab for HikerWbTab {
    fn title(&self) -> egui::WidgetText {
        self.cached_label.clone().into()
    }
    fn is_dirty(&self) -> bool {
        self.cached_dirty
    }
}

/// Build a fresh workbench with the default activity (`Files`) selected
/// and the Chat (secondary) side bar visible.
pub fn new_workbench() -> Workbench<HikerWbTab, HikerMode> {
    let mut wb = Workbench::default();
    wb.activity_bar.set_active(Some(HikerMode::Files));
    wb.secondary_side_bar.visible = true;
    wb
}

/// Sync `app.session.tabs` into the workbench's editor area. Opens
/// workbench tabs for newly-added `Session::tabs` entries, closes
/// workbench tabs whose backing `TabId` is gone, and refreshes the
/// cached label / dirty marker for survivors. Mirrors the role of
/// `tabs::reconcile_dock` for the legacy dock layout.
pub fn sync_tabs(app: &mut AppState) {
    // Snapshot the desired set first (immutable borrow of `session`).
    struct Want {
        id: TabId,
        label: String,
        dirty: bool,
        state: TabState,
        is_active: bool,
    }
    let active_id = app.session.active_tab;
    let preview_id = app.session.preview_tab;
    let want: Vec<Want> = app
        .session
        .tabs
        .iter()
        .map(|t| {
            let dirty = t
                .buffer_path()
                .and_then(|p| app.session.buffers.get(p))
                .map(|b| b.is_dirty())
                .unwrap_or(false);
            let state = if Some(t.id) == preview_id {
                TabState::Preview
            } else if !t.sticky {
                TabState::Preview
            } else {
                TabState::Regular
            };
            Want {
                id: t.id,
                label: t.label(),
                dirty,
                state,
                is_active: Some(t.id) == active_id,
            }
        })
        .collect();

    // Index of TabId → workbench TabHandle for survivors.
    let mut existing: std::collections::HashMap<TabId, egui_workbench::TabHandle> =
        std::collections::HashMap::new();
    for (handle, tab) in app.workbench.iter_tabs() {
        existing.insert(tab.id, handle);
    }

    let want_ids: std::collections::HashSet<TabId> =
        want.iter().map(|w| w.id).collect();

    // Close workbench tabs that no longer have a Session::tabs entry.
    let stale: Vec<_> = existing
        .iter()
        .filter(|(id, _)| !want_ids.contains(id))
        .map(|(_, h)| *h)
        .collect();
    for h in stale {
        app.workbench.close_tab(h);
    }

    // Open / refresh tabs we want present.
    for w in want {
        match existing.get(&w.id).copied() {
            Some(handle) => {
                if let Some(t) = app.workbench.editor_area.get_mut(handle) {
                    t.cached_label = w.label;
                    t.cached_dirty = w.dirty;
                }
            }
            None => {
                let tab = HikerWbTab {
                    id: w.id,
                    cached_label: w.label,
                    cached_dirty: w.dirty,
                };
                app.workbench.open_tab(
                    tab,
                    OpenTabOptions {
                        state: w.state,
                        focus: w.is_active,
                        ..OpenTabOptions::default()
                    },
                );
            }
        }
    }
}

/// Per-frame `WorkbenchBehavior` adapter. Lives only for the duration
/// of one `Workbench::ui` call.
pub struct HikerWbBehavior<'a> {
    pub app: &'a mut AppState,
    pub rt: &'a Arc<tokio::runtime::Runtime>,
}

impl<'a> WorkbenchBehavior<HikerWbTab, HikerMode> for HikerWbBehavior<'a> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tab: &mut HikerWbTab,
        _ctx: TabUiContext<'_>,
    ) {
        let Some(kind) = self.app.tab_by_id(tab.id).map(|t| t.kind.clone()) else {
            ui.centered_and_justified(|ui| {
                ui.label("(missing tab)");
            });
            return;
        };
        render_tab_body(ui, self.app, self.rt, tab.id, kind);
    }

    fn on_tab_close(&mut self, tab: &HikerWbTab) -> bool {
        // Defer to hiker's dirty-close guard. Returning `false` makes
        // egui_tiles keep the pane until the next sync removes it; the
        // guard either calls `editor_pane::close_tab` (synchronous
        // remove from `Session::tabs`) or surfaces the dirty-close
        // modal (close happens later on user confirmation).
        crate::tabs::close_tab_with_dirty_guard(self.app, tab.id);
        false
    }

    fn activity_items(&self) -> Vec<ActivityItem<HikerMode>> {
        [
            HikerMode::Files,
            HikerMode::Clusters,
            HikerMode::Trails,
            HikerMode::Search,
            HikerMode::Related,
            HikerMode::Backlinks,
        ]
        .into_iter()
        .map(|m| ActivityItem {
            label: m.label().to_string(),
            icon: Some(m.icon()),
            mode: m,
            badge: None,
        })
        .collect()
    }

    fn side_bar_title(&self, mode: &HikerMode) -> egui::WidgetText {
        mode.label().into()
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &HikerMode) {
        if let Some(panel) = PanelRegistry::all().by_id(mode.panel_id()) {
            (panel.render)(ui, self.app, self.rt);
        } else {
            ui.weak(format!("(panel '{}' missing)", mode.panel_id()));
        }
    }

    fn secondary_side_bar_title(&self) -> egui::WidgetText {
        "Chat".into()
    }

    fn secondary_side_bar_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(panel) = PanelRegistry::all().by_id(PANEL_CHAT) {
            (panel.render)(ui, self.app, self.rt);
        }
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let vault = self
                .app
                .vault_session
                .vault_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("vault");
            ui.weak(vault);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let staging_n = self.app.ui_cache.staging_snapshot.len();
                if staging_n > 0 {
                    ui.weak(format!("{staging_n} staged"));
                    ui.separator();
                }
                let tasks_n = self.app.ui_cache.task_snapshot.len();
                if tasks_n > 0 {
                    ui.weak(format!("{tasks_n} task{}", if tasks_n == 1 { "" } else { "s" }));
                }
            });
        });
    }
}

/// Render the body of a hiker tab. Routes by `TabKind` to the existing
/// per-kind panel renderers. Lifted out of [`HikerWbBehavior::pane_ui`]
/// so the match doesn't fight the behavior trait's borrow contract.
fn render_tab_body(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
    tab_id: TabId,
    kind: TabKind,
) {
    match kind {
        TabKind::Buffer { path } => panels::buffer::show(ui, app, &path, rt),
        TabKind::Home => panels::home::show(ui, app),
        TabKind::HomeDetail { which } => render_home_detail(ui, app, which),
        TabKind::Queue => panels::queue::show(ui, app),
        TabKind::QueueDetail { task_id } => panels::queue::show_detail(ui, app, &task_id),
        TabKind::Settings => panels::settings::show(ui, app),
        TabKind::Properties { path } => panels::properties::show(ui, app, &path),
        TabKind::Graph => panels::graph::show(ui, app),
        TabKind::Agent { session_id } => panels::agent::show(ui, app, &session_id, rt),
        TabKind::TrashPreview {
            trash_path,
            original_path,
        } => panels::trash_preview::show(ui, app, &trash_path, &original_path),
        TabKind::SnapshotPreview { path, change_id } => {
            panels::snapshot_preview::show(ui, app, &path, &change_id)
        }
        TabKind::BufferDiff { path } => panels::buffer_diff::show(ui, app, &path),
        TabKind::StagingPreview {
            proposal_id,
            target_path,
        } => panels::staging_preview::show(ui, app, &proposal_id, &target_path),
        TabKind::PatchReview => panels::patch_review::show(ui, app),
        TabKind::Plugins => panels::plugins::show(ui, app),
        TabKind::IndexerDetail => panels::indexer_detail::show(ui, app, rt),
        TabKind::AgentChanges => panels::agent_changes::show(ui, app),
        TabKind::ClusterReview { config_json } => {
            panels::cluster_review::show(ui, app, tab_id, &config_json)
        }
        TabKind::ClusterGraph { tree_id } => panels::cluster_graph::show(ui, app, &tree_id),
    }
}

fn render_home_detail(ui: &mut egui::Ui, app: &mut AppState, which: HomeDetail) {
    panels::home::show_detail(ui, app, &which);
}
