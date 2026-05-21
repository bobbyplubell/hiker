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
    WorkbenchBehavior, WorkbenchTheme,
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
            HikerMode::Trails => icons::trail(),
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
    /// True when the tab body draws its own edge-to-edge surface (the
    /// markdown editor + its bottom status strip) and shouldn't get the
    /// workbench's standard pane-content inset. Cached at sync time so
    /// `DocumentTab::wants_pane_content_inset` can answer without
    /// reaching back into `AppState`.
    pub edge_to_edge: bool,
}

impl DocumentTab for HikerWbTab {
    fn title(&self) -> egui::WidgetText {
        self.cached_label.clone().into()
    }
    fn is_dirty(&self) -> bool {
        self.cached_dirty
    }
    fn wants_pane_content_inset(&self) -> bool {
        !self.edge_to_edge
    }
}

/// Build a fresh workbench with the default activity (`Files`) selected
/// and the Chat (secondary) side bar visible.
pub fn new_workbench() -> Workbench<HikerWbTab, HikerMode> {
    let mut wb = Workbench::default();
    wb.activity_bar.set_active(Some(HikerMode::Files));
    wb.secondary_side_bar.visible = true;
    // Global bottom status strip: the editor used to host its own
    // version dropdown + indexer + Ln/Col row inside the buffer pane;
    // that strip is gone and `status_bar_ui` below feeds the same info
    // (plus a default vault row for non-buffer tabs) into the workbench
    // strip so it spans the full window width.
    wb.status_bar.visible = true;
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
        edge_to_edge: bool,
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
            let state = if Some(t.id) == preview_id || !t.sticky {
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
                edge_to_edge: matches!(t.kind, crate::tab::TabKind::Editor { .. }),
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
                    t.edge_to_edge = w.edge_to_edge;
                }
            }
            None => {
                let tab = HikerWbTab {
                    id: w.id,
                    cached_label: w.label,
                    cached_dirty: w.dirty,
                    edge_to_edge: w.edge_to_edge,
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

    // Activation pass: drive the workbench's visible active tab from
    // `app.session.active_tab`. Without this, browser-style back/
    // forward (`editor_pane::nav_go`) updates `active_tab` but the
    // workbench's tab strip stays on whatever the user last clicked —
    // i.e. the visible pane doesn't actually navigate. The reverse
    // direction (workbench click → session.active_tab) is handled by
    // the focus-driven sync in `HikerWbBehavior::pane_ui` callers via
    // the existing tab-strip event plumbing.
    if let Some(active) = active_id
        && let Some(handle) = app.workbench.editor_area.handle_for(|t| t.id == active)
    {
        app.workbench.set_active(handle);
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

    fn on_preview_promoted(&mut self, tab: &HikerWbTab) {
        // egui-workbench promoted this tab from Preview → Regular
        // (double-click on the tab in the strip, or a "Keep open" menu
        // action). Mirror the promotion into hiker's per-tab `sticky`
        // flag and clear the session-level preview slot so the next
        // non-sticky `open_file` allocates a fresh preview tab instead
        // of swapping the just-pinned tab's contents.
        let id = tab.id;
        if let Some(t) = self.app.tab_by_id_mut(id) {
            t.sticky = true;
        }
        if self.app.session.preview_tab == Some(id) {
            self.app.session.preview_tab = None;
        }
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

    fn side_bar_action_buttons(&mut self, ui: &mut egui::Ui, mode: &HikerMode) {
        if matches!(mode, HikerMode::Files)
            && ui
                .add(egui::Button::image(crate::icons::plus()).small())
                .on_hover_text("New note")
                .clicked()
        {
            crate::sidebar::new_note(self.app);
        }
    }

    fn side_bar_actions_menu(&mut self, ui: &mut egui::Ui, mode: &HikerMode) {
        if matches!(mode, HikerMode::Files) {
            if ui.button("Refresh tree").clicked() {
                self.app.session.sidebar.dir_cache.clear();
                self.app.push_toast("File tree refreshed", crate::state::ToastLevel::Info);
                ui.close();
            }
            ui.separator();
            ui.label(
                egui::RichText::new("Sort by")
                    .color(crate::theme::muted())
                    .small(),
            );
            use hiker_core::config::TreeSortBy;
            let cur = self
                .app
                .vault_session
                .config
                .read()
                .ok()
                .map(|c| c.vault.tree.sort_by)
                .unwrap_or(TreeSortBy::NameAsc);
            for (label, val) in [
                ("Name A -> Z", TreeSortBy::NameAsc),
                ("Name Z -> A", TreeSortBy::NameDesc),
                ("Modified (newest)", TreeSortBy::MtimeDesc),
                ("Modified (oldest)", TreeSortBy::MtimeAsc),
            ] {
                let prefix = if cur == val { "* " } else { "  " };
                if ui.button(format!("{prefix}{label}")).clicked() {
                    let s = match val {
                        TreeSortBy::NameAsc => "name_asc",
                        TreeSortBy::NameDesc => "name_desc",
                        TreeSortBy::MtimeDesc => "mtime_desc",
                        TreeSortBy::MtimeAsc => "mtime_asc",
                    };
                    crate::sidebar::persist_tree_sort(self.app, s);
                    self.app.session.sidebar.dir_cache.clear();
                    ui.close();
                }
            }
        }
    }

    fn secondary_side_bar_action_buttons(&mut self, ui: &mut egui::Ui) {
        let active_id = self.app.session.chat.active.clone();
        if active_id.is_some()
            && ui
                .add(egui::Button::image(crate::icons::trash()).small())
                .on_hover_text("Delete this session")
                .clicked()
            && let Some(id) = active_id.as_deref()
        {
            let vault_root = self.app.vault_session.vault_root.clone();
            if let Err(err) =
                crate::chat::session::delete(&mut self.app.session.chat, &vault_root, id)
            {
                tracing::warn!(error = %err, "chat: delete failed");
            }
        }
        if ui
            .add(egui::Button::image(crate::icons::plus()).small())
            .on_hover_text("New session")
            .clicked()
        {
            let vault_root = self.app.vault_session.vault_root.clone();
            let (model, provider) = self
                .app
                .vault_session
                .config
                .read()
                .map(|c| (c.llm.provider.model.clone(), c.llm.provider.backend.clone()))
                .unwrap_or_else(|_| ("stub-model".into(), "stub".into()));
            if let Err(err) = crate::chat::session::create_new(
                &mut self.app.session.chat,
                &vault_root,
                &model,
                &provider,
            ) {
                tracing::warn!(error = %err, "chat: create_new failed");
            }
        }
    }

    fn secondary_side_bar_title_ui(&mut self, ui: &mut egui::Ui) {
        ui.add(crate::icons::chat());
        crate::chat::render::render_session_picker(ui, self.app);
    }

    fn secondary_side_bar_ui(&mut self, ui: &mut egui::Ui) {
        if let Some(panel) = PanelRegistry::all().by_id(PANEL_CHAT) {
            (panel.render)(ui, self.app, self.rt);
        }
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        // Active-tab-driven status content: when a Buffer tab is
        // focused, render the per-buffer version dropdown + indexer
        // state + Ln:Col / word count row that used to live inside
        // the editor pane. For any other tab kind, fall back to the
        // vault-level row (vault name + staging / task counters).
        // Other panels can grow their own status content the same way
        // by matching on `TabKind` here.
        let active_buffer_path = self
            .app
            .session
            .active_tab
            .and_then(|id| self.app.tab_by_id(id))
            .and_then(|t| t.kind.vault_path().map(|p| p.to_string()));
        if let Some(path) = active_buffer_path {
            crate::panels::buffer::status_bar(ui, self.app, &path);
            return;
        }

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

    fn theme(&self, style: &egui::Style) -> WorkbenchTheme {
        // The default focused-group stroke (2px, `selection.bg_fill` at
        // ~60% alpha) paints on top of the tile rect — over a light
        // theme it alpha-blends to a near-white border around every edge
        // of the central pane and inside the minimap, which the user
        // reads as stray white padding. Zero out the width to disable
        // the overlay while keeping the rest of the default theme.
        WorkbenchTheme {
            focused_group_border_width: 0.0,
            ..WorkbenchTheme::from_egui_style(style)
        }
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
        TabKind::Editor { buffer, diff } => render_editor_tab(ui, app, rt, buffer, diff),
        TabKind::Home => panels::home::show(ui, app),
        TabKind::HomeDetail { which } => render_home_detail(ui, app, which),
        TabKind::Queue => panels::queue::show(ui, app),
        TabKind::QueueDetail { task_id } => panels::queue::show_detail(ui, app, &task_id),
        TabKind::Settings => panels::settings::show(ui, app),
        TabKind::Properties { path } => panels::properties::show(ui, app, &path),
        TabKind::Graph => panels::graph::show(ui, app),
        TabKind::Agent { session_id } => panels::agent::show(ui, app, &session_id, rt),
        TabKind::PatchReview => panels::patch_review::show(ui, app),
        TabKind::Plugins => panels::plugins::show(ui, app),
        TabKind::IndexerDetail => panels::indexer_detail::show(ui, app, rt),
        TabKind::Changes => panels::changes::show(ui, app),
        TabKind::ClusterReview { config_json } => {
            panels::cluster_review::show(ui, app, tab_id, &config_json)
        }
        TabKind::ClusterGraph { tree_id } => panels::cluster_graph::show(ui, app, &tree_id),
    }
}

fn render_home_detail(ui: &mut egui::Ui, app: &mut AppState, which: HomeDetail) {
    panels::home::show_detail(ui, app, &which);
}

/// Dispatch an Editor-kind tab to the appropriate panel renderer based on
/// the buffer source. Diff is presented by the underlying panel as a mode
/// of the same editor widget (no separate panel for "diff active").
fn render_editor_tab(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
    buffer: crate::tab::BufferSource,
    diff: Option<crate::tab::DiffSource>,
) {
    use crate::tab::BufferSource;
    // Diff (when set) renders as a decoration layer on the same editor
    // widget — same tab, same cursor / selection — not a separate panel.
    let _ = diff;
    match buffer {
        BufferSource::Vault { path } => panels::buffer::show(ui, app, &path, rt),
        BufferSource::Snapshot { path, change_id } => {
            panels::snapshot_preview::show(ui, app, &path, &change_id);
        }
        BufferSource::StagingProposal { proposal_id, target_path } => {
            panels::staging_preview::show(ui, app, &proposal_id, &target_path);
        }
        BufferSource::Trash { trash_path, original_path } => {
            panels::trash_preview::show(ui, app, &trash_path, &original_path);
        }
    }
}
