//! Legacy central tab area — superseded by `crate::workbench_host`.
//!
//! Kept around for one transitional pass: `close_tab_with_dirty_guard`
//! is still called from the workbench behavior's `on_tab_close`, and
//! the remaining bodies will be deleted once we're confident the
//! workbench integration covers their functionality.
#![allow(dead_code)]

//! Central tab area — `egui_tiles::Tree` over panes carrying
//! `DockTab`.
//!
//! `DockTab` is either a `Tab(TabId)` (mirrors `session.tabs` — the
//! source of truth for which buffer/page tabs are open) or
//! `Panel(String)` (a registered sidebar/discovery panel, see
//! `panels_registry`).
//!
//! `reconcile_dock` keeps `Tab` entries in sync with `session.tabs` and
//! `enforce_buffer_tabs_in_center` (post-frame) bounces any buffer tab
//! that landed in a sidebar back to the center tile.

use std::sync::Arc;

use eframe::egui;
use egui_tiles::{Behavior, Container, EditAction, SimplificationOptions, Tile, TileId, Tiles, UiResponse};

use crate::editor_pane;
use crate::layout::DockTreeExt as _;
use crate::panels;
use crate::panels_registry::PanelRegistry;
use crate::state::{AppState, Modal};
use crate::tab::{DockTab, TabId, TabKind};
use crate::theme;

impl AppState {
/// Format the tab's label text: name + dirty dot, italicised when the
/// tab is the preview slot.
fn render_label(&self, tab: &crate::tab::Tab, is_preview: bool) -> egui::WidgetText {
    let app = self;
    app.finalize_label(tab.label(), tab.buffer_path(), is_preview)
}

fn finalize_label(
    &self,
    mut text: String,
    path: Option<&str>,
    is_preview: bool,
) -> egui::WidgetText {
    let app = self;
    if let Some(p) = path
        && let Some(buf) = app.session.buffers.get(p)
        && buf.is_dirty()
    {
        text.push_str(" *");
    }
    let mut rich = egui::RichText::new(text);
    if is_preview {
        rich = rich.italics();
    }
    egui::WidgetText::from(rich)
}
}

/// Close a tab; if the underlying buffer is dirty, surface the dirty-close
/// modal instead of closing immediately.
pub fn close_tab_with_dirty_guard(app: &mut AppState, tab_id: TabId) {
    let dirty_path = app
        .tab_by_id(tab_id)
        .and_then(|t| t.buffer_path().map(str::to_string))
        .filter(|p| app.session.buffers.get(p).map(super::buffer::Buffer::is_dirty).unwrap_or(false));
    if let Some(path) = dirty_path {
        app.session.modal = Some(Modal::DirtyClose { path, tab_id });
    } else {
        editor_pane::close_tab(app, tab_id);
    }
}

/// Ensure the recorded `center_tile` is still a Tabs container. If it
/// has been collapsed away, pick a new one via the same heuristic the
/// loader uses, falling back to creating a fresh Tabs tile if nothing
/// suitable exists.
impl AppState {
fn ensure_center_tile(&mut self) -> TileId {
    let app = self;
    let current = app.session.center_tile;
    if matches!(
        app.session.dock.tiles.get(current),
        Some(Tile::Container(Container::Tabs(_)))
    ) {
        return current;
    }
    // Heuristic: first Tabs container with no panels.
    let new_center = app
        .session
        .dock
        .tiles
        .iter()
        .find_map(|(id, tile)| match tile {
            Tile::Container(Container::Tabs(tabs)) => {
                let has_panel = tabs.children.iter().any(|c| {
                    matches!(
                        app.session.dock.tiles.get(*c),
                        Some(Tile::Pane(DockTab::Panel(_))),
                    )
                });
                if !has_panel { Some(*id) } else { None }
            }
            _ => None,
        });
    if let Some(id) = new_center {
        app.session.center_tile = id;
        return id;
    }
    // Last resort: insert a fresh Tabs tile (orphaned — the user will
    // see no buffer tabs, but the app stays alive).
    let id = app.session.dock.tiles.insert_tab_tile(Vec::new());
    app.session.center_tile = id;
    id
}
}

/// Bring the dock arrangement back in sync with `app.session.tabs`:
///   - Push any TabId present in `session.tabs` but missing from `dock`
///     into the center tile.
///   - Remove any TabId present in `dock` but missing from `session.tabs`.
///
/// Panels in the dock are NOT touched — their lifecycle is owned by
/// the layout loader + `panel.toggle.*` actions.
impl AppState {
pub fn reconcile_dock(&mut self) {
    let app = self;
    let want: Vec<TabId> = app.session.tabs.iter().map(|t| t.id).collect();
    let have: std::collections::HashSet<TabId> = app
        .session
        .dock
        .tiles
        .iter()
        .filter_map(|(_, t)| match t {
            Tile::Pane(DockTab::Tab(id)) => Some(*id),
            _ => None,
        })
        .collect();

    // Add new tabs.
    for id in &want {
        if have.contains(id) {
            continue;
        }
        let center = app.ensure_center_tile();
        let pane_id = app
            .session
            .dock
            .tiles
            .insert_pane(DockTab::Tab(*id));
        if let Some(Tile::Container(Container::Tabs(tabs))) =
            app.session.dock.tiles.get_mut(center)
        {
            tabs.add_child(pane_id);
            tabs.set_active(pane_id);
        }
        app.session.dock_dirty = true;
    }

    // Remove stale tabs (panes whose TabId isn't in want).
    let want_set: std::collections::HashSet<TabId> = want.iter().copied().collect();
    let stale: Vec<TileId> = app
        .session
        .dock
        .tiles
        .iter()
        .filter_map(|(id, tile)| match tile {
            Tile::Pane(DockTab::Tab(t)) if !want_set.contains(t) => Some(*id),
            _ => None,
        })
        .collect();
    for id in stale {
        app.session.dock.remove_recursively(id);
        app.session.dock_dirty = true;
    }

    // Sync active_tab → make the active tab visible (active in its Tabs
    // container).
    if let Some(active) = app.session.active_tab {
        app.session.dock.make_active(|_id, tile| {
            matches!(tile, Tile::Pane(DockTab::Tab(t)) if *t == active)
        });
    }
}
}

impl AppState {
/// Record current `TileId` for every panel currently in the dock. Used
/// by `panel.toggle.*` so re-toggling drops the panel back where the
/// user last had it.
fn record_panel_locations(&mut self) {
    let app = self;
    app.session.panel_locations.clear();
    for (id, tile) in app.session.dock.tiles.iter() {
        if let Tile::Pane(DockTab::Panel(panel_id)) = tile {
            // Record the parent (Tabs container) so re-insertion can
            // target it. If the pane has no parent (shouldn't happen
            // for an in-use tree) skip it.
            if let Some(parent) = app.session.dock.tiles.parent_of(*id) {
                app.session.panel_locations.insert(panel_id.clone(), parent);
            }
        }
    }
}

/// Pull the currently-active leaf pane's TabId, if any. Used to sync
/// `session.active_tab` after the user clicks a tab.
fn active_tab_in_center(&self) -> Option<TabId> {
    let app = self;
    let center = app.session.center_tile;
    let Tile::Container(Container::Tabs(tabs)) = app.session.dock.tiles.get(center)?
    else {
        return None;
    };
    let active = tabs.active?;
    match app.session.dock.tiles.get(active)? {
        Tile::Pane(DockTab::Tab(id)) => Some(*id),
        _ => None,
    }
}
}

struct HikerBehavior<'a> {
    app: &'a mut AppState,
    rt: &'a Arc<tokio::runtime::Runtime>,
}

impl<'a> Behavior<DockTab> for HikerBehavior<'a> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        pane: &mut DockTab,
    ) -> UiResponse {
        match pane {
            DockTab::Tab(id) => self.render_tab(ui, *id),
            DockTab::Panel(panel_id) => {
                let panel = PanelRegistry::all().by_id(panel_id);
                let Some(panel) = panel else {
                    ui.centered_and_justified(|ui| {
                        ui.label(format!("(unknown panel: {panel_id})"));
                    });
                    return UiResponse::None;
                };
                (panel.render)(ui, self.app, self.rt);
            }
        }
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &DockTab) -> egui::WidgetText {
        match pane {
            DockTab::Tab(id) => {
                let Some(t) = self.app.tab_by_id(*id).cloned() else {
                    return egui::WidgetText::from("(missing)");
                };
                let is_preview = self.app.session.preview_tab == Some(*id);
                self.app.render_label(&t, is_preview)
            }
            DockTab::Panel(panel_id) => {
                let title = PanelRegistry::all()
                    .by_id(panel_id)
                    .map(|p| p.title)
                    .unwrap_or("(unknown)");
                egui::WidgetText::from(title)
            }
        }
    }

    fn is_tab_closable(&self, tiles: &Tiles<DockTab>, tile_id: TileId) -> bool {
        matches!(tiles.get(tile_id), Some(Tile::Pane(DockTab::Tab(_))))
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<DockTab>, tile_id: TileId) -> bool {
        match tiles.get(tile_id) {
            Some(Tile::Pane(DockTab::Tab(id))) => {
                let id = *id;
                close_tab_with_dirty_guard(self.app, id);
                // Return false: the reconciler will remove the pane
                // when session.tabs is updated. If the dirty-close
                // modal intercepted, we keep the pane until the user
                // confirms.
                false
            }
            Some(Tile::Pane(DockTab::Panel(_))) => {
                self.app.session.dock_dirty = true;
                true
            }
            _ => true,
        }
    }

    fn on_edit(&mut self, edit_action: EditAction) {
        if matches!(
            edit_action,
            EditAction::TileDropped | EditAction::TileResized
        ) {
            self.app.session.dock_dirty = true;
        }
    }

    fn simplification_options(&self) -> SimplificationOptions {
        // Keep empty Tabs containers around so the center stays as a
        // drop target when there are no buffer tabs open. Single-child
        // tabs containers must also stay, since the panel-side
        // containers can shrink to one panel and we want them to keep
        // their tab strip.
        SimplificationOptions {
            prune_empty_tabs: false,
            prune_empty_containers: false,
            prune_single_child_tabs: false,
            prune_single_child_containers: false,
            all_panes_must_have_tabs: true,
            join_nested_linear_containers: true,
        }
    }
}

impl<'a> HikerBehavior<'a> {
    fn render_tab(&mut self, ui: &mut egui::Ui, id: TabId) {
        let Some(kind) = self.app.tab_by_id(id).map(|t| t.kind.clone()) else {
            ui.centered_and_justified(|ui| {
                ui.label("(missing tab)");
            });
            return;
        };
        match kind {
            TabKind::Editor { buffer, diff } => {
                use crate::tab::BufferSource;
                let _ = diff;
                let key = match &buffer {
                    BufferSource::Vault { path } => Some(path.clone()),
                    _ => crate::editor_pane::ensure_readonly_buffer_loaded(self.app, &buffer),
                };
                if let Some(k) = key {
                    panels::buffer::show(ui, self.app, &k, self.rt);
                }
            }
            TabKind::Home => panels::home::show(ui, self.app),
            TabKind::HomeDetail { which } => panels::home::show_detail(ui, self.app, &which),
            TabKind::Queue => panels::queue::show(ui, self.app),
            TabKind::QueueDetail { task_id } => {
                panels::queue::show_detail(ui, self.app, &task_id)
            }
            TabKind::Settings => panels::settings::show(ui, self.app),
            TabKind::Properties { path } => panels::properties::show(ui, self.app, &path),
            TabKind::Graph => panels::graph::show(ui, self.app),
            TabKind::Agent { session_id } => {
                panels::agent::show(ui, self.app, &session_id, self.rt)
            }
            TabKind::PatchReview => panels::patch_review::show(ui, self.app),
            TabKind::Plugins => panels::plugins::show(ui, self.app),
            TabKind::IndexerDetail => panels::indexer_detail::show(ui, self.app, self.rt),
            TabKind::Changes => panels::changes::show(ui, self.app),
            TabKind::ClusterReview { config_json } => {
                panels::cluster_review::show(ui, self.app, id, &config_json)
            }
            TabKind::ClusterGraph { tree_id } => {
                panels::cluster_graph::show(ui, self.app, &tree_id)
            }
        }
    }
}

/// Central pane: render the tile tree. The tree is owned by
/// `Session::dock`; we swap it out into a local for the render call so
/// the `Behavior` impl can hold `&mut AppState`.
impl AppState {
pub fn dock_body(&mut self, ctx: &egui::Context, rt: &Arc<tokio::runtime::Runtime>) {
    let app = self;
    app.reconcile_dock();
    egui::CentralPanel::default()
        .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(0))
        .show(ctx, |ui| {
            if app.session.tabs.is_empty()
                && app
                    .session
                    .dock
                    .tiles
                    .iter()
                    .all(|(_, t)| !matches!(t, Tile::Pane(_)))
            {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("No tab open")
                            .color(theme::muted()),
                    );
                });
                return;
            }
            let mut dock = std::mem::replace(
                &mut app.session.dock,
                egui_tiles::Tree::empty("hiker-dock-placeholder"),
            );
            {
                let mut behavior = HikerBehavior { app, rt };
                dock.ui(&mut behavior, ui);
            }
            // Post-frame enforcement: any DockTab::Tab not under
            // center_tile gets bounced back. Mirror image: any
            // DockTab::Panel ending up in the center is left where it
            // is (less common, and disruptive to fix automatically).
            dock.enforce_buffer_tabs_in_center(app.session.center_tile);
            app.session.dock = dock;

            // Sync focused tab back to `session.active_tab`.
            if let Some(new_active) = app.active_tab_in_center()
                && app.session.active_tab != Some(new_active)
            {
                app.session.active_tab = Some(new_active);
                if !app.session.nav.locked
                    && let Some(path) = app
                        .tab_by_id(new_active)
                        .and_then(|t| t.buffer_path())
                        .map(std::string::ToString::to_string)
                {
                    crate::state::nav_push(app, &path);
                }
            }
            app.record_panel_locations();
        });
}
}
