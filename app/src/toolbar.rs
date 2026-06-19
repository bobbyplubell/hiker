//! Data-driven multi-toolbar renderer. See `actions.rs` for the action
//! registry and `state::Toolbars` for the persisted layout. Each
//! `Toolbar` is a `Vec<ActionId>` plus a side; this module turns that
//! into egui panels.
//!
//! Layout primitives recognised inside `actions`:
//!   - `"sep"`: vertical/horizontal separator
//!   - `"spacer"`: flexible gap pushing subsequent items to the far edge
//!   - `"vault.label"`: muted vault-name label with the "Set as default
//!     vault" context menu
//!   - `"actions.menu"`: legacy hamburger composite
//!
//! Everything else is looked up in `ActionRegistry` and rendered as an
//! icon button with the action's tooltip + optional badge.

use eframe::egui;

use crate::actions::{
    self, ActionRegistry, ID_ACTIONS_MENU, ID_SEP, ID_SPACER, ID_VAULT_LABEL, ID_VIEW_MENU,
};
use crate::icons;
use crate::state::{AppState, Toolbar, ToolbarSide, ToastLevel};
use crate::tab::TabKind;
use hiker_theme as theme;

impl AppState {
/// Render every configured toolbar. Call once per frame, BEFORE side
/// panels — top/bottom panels claim their strip first.
///
/// When `overlay_command_center` is set, the VSCode-style command center
/// is painted centered over the *first* top toolbar so it shares that
/// strip instead of adding a row. Returns whether it was placed — the
/// caller renders a dedicated bar as a fallback when there's no top
/// toolbar to host it. [command-center-topbar]
pub fn render_toolbars(&mut self, ctx: &egui::Context, overlay_command_center: bool) -> bool {
    self.render_toolbar_panels(ctx, overlay_command_center, true)
}

/// Render only the non-top toolbars (bottom/left/right) as panels — used
/// when the top bar is folded into the custom titlebar.
pub fn render_secondary_toolbars(&mut self, ctx: &egui::Context) {
    self.render_toolbar_panels(ctx, false, false);
}

/// Render the first **Top** toolbar's items into an existing `ui` (used
/// to fold the toolbar into the custom titlebar's single strip). Returns
/// `true` if a top toolbar existed. [command-center-topbar]
pub fn render_top_bar_inline(&mut self, ui: &mut egui::Ui) -> Option<(f32, f32)> {
    let app = self;
    let idx = app.ui.toolbars.bars.iter().position(|b| b.side == ToolbarSide::Top)?;
    // The caller's `ui` is already a left-to-right, vertically-centered
    // layout spanning the titlebar (minus the window controls). The
    // spacer is respected, so the tail (e.g. sidebar toggles) right-aligns
    // next to the controls — its original placement. Returns the
    // head-right / tail-left x for the titlebar's drag-zone gaps.
    Some(render_bar_items(ui, app, idx, /* vertical */ false))
}

/// Shared toolbar renderer. `include_top` is false when the top bar has
/// been folded into the custom titlebar — only bottom/left/right bars
/// render as panels then.
fn render_toolbar_panels(
    &mut self,
    ctx: &egui::Context,
    overlay_command_center: bool,
    include_top: bool,
) -> bool {
    let app = self;
    // Clone the bars list (cheap — just a Vec of small structs) so we can
    // mutate `app` while iterating.
    let bars = app.ui.toolbars.bars.clone();
    let mut cc_placed = false;
    for (idx, bar) in bars.iter().enumerate() {
        let panel_id = format!("toolbar-{}-{idx}", bar.id);
        match bar.side {
            ToolbarSide::Top if !include_top => {}
            ToolbarSide::Top => {
                let place_cc = overlay_command_center && !cc_placed;
                egui::TopBottomPanel::top(panel_id)
                    .frame(panel_frame(ctx))
                    .show(ctx, |ui| {
                        let full = ui.max_rect();
                        render_bar_horizontal(ui, app, idx);
                        if place_cc {
                            app.command_center(ui, full);
                        }
                    });
                cc_placed |= place_cc;
            }
            ToolbarSide::Bottom => {
                egui::TopBottomPanel::bottom(panel_id)
                    .frame(panel_frame(ctx))
                    .show(ctx, |ui| render_bar_horizontal(ui, app, idx));
            }
            ToolbarSide::Left => {
                egui::SidePanel::left(panel_id)
                    .resizable(false)
                    .exact_width(36.0)
                    .frame(panel_frame(ctx))
                    .show(ctx, |ui| render_bar_vertical(ui, app, idx));
            }
            ToolbarSide::Right => {
                egui::SidePanel::right(panel_id)
                    .resizable(false)
                    .exact_width(36.0)
                    .frame(panel_frame(ctx))
                    .show(ctx, |ui| render_bar_vertical(ui, app, idx));
            }
        }
    }
    cc_placed
}
}

fn panel_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::default()
        .fill(ctx.style().visuals.panel_fill)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .stroke(egui::Stroke::new(1.0, theme::divider()))
}

fn render_bar_horizontal(ui: &mut egui::Ui, app: &mut AppState, bar_idx: usize) {
    ui.horizontal_centered(|ui| {
        render_bar_items(ui, app, bar_idx, /* vertical */ false);
    });
}

fn render_bar_vertical(ui: &mut egui::Ui, app: &mut AppState, bar_idx: usize) {
    ui.vertical(|ui| {
        render_bar_items(ui, app, bar_idx, /* vertical */ true);
    });
}

/// Walk the bar's action ids and dispatch each to the right renderer.
/// `spacer` flips the layout into right-to-left (horizontal) or
/// bottom-to-top (vertical) so subsequent items pin to the far edge.
///
/// Returns `(head_right, tail_left)` — the x where the leading (head)
/// items end and the x where the trailing (tail) items begin. The
/// titlebar uses these to place its drag zones in the empty gap between
/// them (around the command center). Meaningful only for horizontal bars.
fn render_bar_items(
    ui: &mut egui::Ui,
    app: &mut AppState,
    bar_idx: usize,
    vertical: bool,
) -> (f32, f32) {
    let action_ids = app.ui.toolbars.bars[bar_idx].actions.clone();
    let customize = app.ui.customize_toolbars;
    let mut pending: Vec<BarOp> = Vec::new();

    // Partition into "head" (before any spacer) and "tail" (after the
    // first spacer). The tail renders inside a far-anchored sublayout.
    let mut head: Vec<(usize, String)> = Vec::new();
    let mut tail: Vec<(usize, String)> = Vec::new();
    let mut in_far_zone = false;
    for (i, id) in action_ids.iter().enumerate() {
        if id == ID_SPACER {
            in_far_zone = true;
            continue;
        }
        if in_far_zone {
            tail.push((i, id.clone()));
        } else {
            head.push((i, id.clone()));
        }
    }

    for (slot, id) in &head {
        render_single(ui, app, bar_idx, *slot, id, customize, &mut pending);
    }
    let head_right = ui.min_rect().right();
    let mut tail_left = ui.max_rect().right();
    if !tail.is_empty() {
        let layout = if vertical {
            egui::Layout::bottom_up(egui::Align::Center)
        } else {
            egui::Layout::right_to_left(egui::Align::Center)
        };
        // For horizontal far-zone the iteration order needs reversing so
        // the visual order matches the data order (right_to_left flips).
        let ordered: Vec<&(usize, String)> = if vertical {
            tail.iter().collect()
        } else {
            tail.iter().rev().collect()
        };
        let inner = ui.with_layout(layout, |ui| {
            for (slot, id) in ordered {
                render_single(ui, app, bar_idx, *slot, id, customize, &mut pending);
            }
        });
        tail_left = inner.response.rect.left();
    }
    if customize {
        app.customize_add_button(ui, bar_idx, &mut pending);
    }

    app.apply_pending(bar_idx, pending);
    (head_right, tail_left)
}

/// Pending mutation against a toolbar — collected while iterating, then
/// applied at the end so we never restructure the list mid-render.
enum BarOp {
    Remove { slot: usize },
    Append { id: String },
    InsertAt { slot: usize, id: String },
    /// Reorder: drag source slot → destination slot.
    Move { from: usize, to: usize },
    /// Spawn a new toolbar on the given side and (optionally) seed it
    /// with the action id we dropped onto it.
    NewToolbar { side: ToolbarSide, seed: Option<String> },
}

impl AppState {
fn apply_pending(&mut self, bar_idx: usize, ops: Vec<BarOp>) {
    let app = self;
    if ops.is_empty() {
        return;
    }
    let mut dirty = false;
    for op in ops {
        match op {
            BarOp::Remove { slot } => {
                let bar = &mut app.ui.toolbars.bars[bar_idx];
                if slot < bar.actions.len() {
                    bar.actions.remove(slot);
                    dirty = true;
                }
            }
            BarOp::Append { id } => {
                app.ui.toolbars.bars[bar_idx].actions.push(id);
                dirty = true;
            }
            BarOp::InsertAt { slot, id } => {
                let bar = &mut app.ui.toolbars.bars[bar_idx];
                let pos = slot.min(bar.actions.len());
                bar.actions.insert(pos, id);
                dirty = true;
            }
            BarOp::Move { from, to } => {
                let bar = &mut app.ui.toolbars.bars[bar_idx];
                if from < bar.actions.len() && to <= bar.actions.len() && from != to {
                    let item = bar.actions.remove(from);
                    let dest = if to > from { to - 1 } else { to };
                    let dest = dest.min(bar.actions.len());
                    bar.actions.insert(dest, item);
                    dirty = true;
                }
            }
            BarOp::NewToolbar { side, seed } => {
                let id = format!("custom-{}", app.ui.toolbars.bars.len() + 1);
                let actions = match seed {
                    Some(s) => vec![s],
                    None => Vec::new(),
                };
                app.ui.toolbars.bars.push(Toolbar { id, side, actions });
                dirty = true;
            }
        }
    }
    if dirty {
        actions::persist_toolbars(app);
    }
}
}

/// Render a single slot in a toolbar. Special-cases the layout ids
/// (`sep`, `vault.label`, `actions.menu`); everything else falls through
/// to the registry lookup.
fn render_single(
    ui: &mut egui::Ui,
    app: &mut AppState,
    bar_idx: usize,
    slot: usize,
    id: &str,
    customize: bool,
    pending: &mut Vec<BarOp>,
) {
    match id {
        ID_SEP => {
            ui.separator();
        }
        ID_SPACER => {
            // Handled by caller — should not arrive here.
        }
        ID_VAULT_LABEL => {
            app.render_vault_label(ui);
        }
        ID_ACTIONS_MENU => {
            app.render_actions_menu(ui);
        }
        ID_VIEW_MENU => {
            app.render_view_menu(ui);
        }
        _ => {
            app.render_action_button(ui, bar_idx, slot, id, customize, pending);
        }
    }
}

impl AppState {
fn render_vault_label(&mut self, ui: &mut egui::Ui) {
    let state = self;
    let vault_label = state
        .vault_session
        .vault_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.vault_session.vault_root.to_string_lossy().into_owned());
    let resp = ui.label(egui::RichText::new(vault_label).color(theme::muted()));
    let mut chosen = None;
    resp.context_menu(|ui| chosen = egui_workbench::menu::show(ui, build_vault_label_menu()));
    if let Some(VaultLabelVerb::SetAsDefault) = chosen {
        let p = state.vault_session.vault_root.to_string_lossy().to_string();
        match hiker_core::config::Config::set(
            hiker_core::config::SettingsScope::User,
            "vault.default",
            &serde_json::json!(p),
            &state.vault_session.vault_root,
        ) {
            Ok(_) => state.push_toast("Set as default vault", ToastLevel::Info),
            Err(err) => state.push_toast(
                format!("Set default failed: {err}"),
                ToastLevel::Error,
            ),
        }
    }
}
}

/// The single verb on the toolbar vault-label menu.
#[derive(Clone, Copy)]
enum VaultLabelVerb {
    SetAsDefault,
}

/// Build the toolbar vault-label menu (status: ctxmenu-toolbar). One verb:
/// persist the current vault as the user-scope default.
fn build_vault_label_menu() -> egui_workbench::menu::Menu<VaultLabelVerb> {
    egui_workbench::menu::Menu::new().action("Set as default vault", VaultLabelVerb::SetAsDefault)
}

impl AppState {
fn render_action_button(
    &mut self,
    ui: &mut egui::Ui,
    bar_idx: usize,
    slot: usize,
    id: &str,
    customize: bool,
    pending: &mut Vec<BarOp>,
) {
    let app = self;
    let Some(action) = ActionRegistry::all().by_id(id) else {
        // Unknown id — show a placeholder so the user can right-click to
        // remove it.
        let resp = ui.add(egui::Button::new(format!("?{}", id)).small());
        if customize {
            resp.context_menu(|ui| {
                if ui.button("Remove").clicked() {
                    pending.push(BarOp::Remove { slot });
                    ui.close();
                }
            });
        }
        return;
    };
    let enabled = action.enabled.map(|f| f(app)).unwrap_or(true);
    let badge_text = action.badge.and_then(|f| f(app));
    let resp = ui
        .scope(|ui| {
            ui.horizontal(|ui| {
                let btn = ui.add_enabled(enabled, egui::Button::image(crate::icons::ICONS.image(action.icon)));
                if let Some(b) = &badge_text {
                    ui.label(
                        egui::RichText::new(b)
                            .small()
                            .color(egui::Color32::WHITE)
                            .background_color(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                    );
                }
                btn
            })
            .inner
        })
        .inner
        .on_hover_text(action.label);

    if !customize && resp.clicked() {
        crate::actions::dispatch(app, action.id);
    }
    // Right-clicking the reader-mode (book) button surfaces the reader-view-
    // specific options — the same "hide … in reader mode" toggles the eye View
    // menu holds — straight from the reader icon. [global-view-menu]
    if !customize && action.id == "view.reader_mode" {
        resp.context_menu(|ui| {
            ui.label(egui::RichText::new("Reader options").strong());
            reader_view_options(app, ui);
        });
    }
    if customize {
        // Drag-to-reorder. Use egui dnd via Memory: when dragged, store
        // the source slot in memory; on drop into another slot, emit
        // Move.
        let drag_id = ui.id().with(("toolbar-drag", bar_idx));
        if resp.drag_started() {
            ui.ctx().memory_mut(|m| m.data.insert_temp(drag_id, slot));
        }
        if resp.drag_stopped() {
            let from: Option<usize> =
                ui.ctx().memory(|m| m.data.get_temp(drag_id));
            ui.ctx().memory_mut(|m| m.data.remove::<usize>(drag_id));
            if let Some(from) = from
                && from != slot
            {
                pending.push(BarOp::Move { from, to: slot });
            }
        }
        resp.context_menu(|ui| {
            ui.label(egui::RichText::new(action.label).strong());
            ui.separator();
            if ui.button("Remove from toolbar").clicked() {
                pending.push(BarOp::Remove { slot });
                ui.close();
            }
            ui.menu_button("Insert action before this", |ui| {
                add_action_picker(ui, |chosen| {
                    pending.push(BarOp::InsertAt {
                        slot,
                        id: chosen.into(),
                    });
                });
            });
        });
    }
}
}

impl AppState {
/// "+ Add" / "+ New toolbar" affordance shown at the end of every bar
/// while in customize mode.
fn customize_add_button(
    &mut self,
    ui: &mut egui::Ui,
    _bar_idx: usize,
    pending: &mut Vec<BarOp>,
) {
    ui.menu_button("+", |ui| {
        ui.label(egui::RichText::new("Add to this toolbar").strong());
        ui.separator();
        add_action_picker(ui, |chosen| {
            pending.push(BarOp::Append { id: chosen.into() });
        });
        ui.separator();
        ui.label(egui::RichText::new("Layout").strong());
        if ui.button("Separator").clicked() {
            pending.push(BarOp::Append { id: ID_SEP.into() });
            ui.close();
        }
        if ui.button("Spacer (flexible)").clicked() {
            pending.push(BarOp::Append { id: ID_SPACER.into() });
            ui.close();
        }
        ui.separator();
        ui.label(egui::RichText::new("New toolbar").strong());
        for (lbl, side) in [
            ("Top", ToolbarSide::Top),
            ("Bottom", ToolbarSide::Bottom),
            ("Left", ToolbarSide::Left),
            ("Right", ToolbarSide::Right),
        ] {
            if ui.button(lbl).clicked() {
                pending.push(BarOp::NewToolbar { side, seed: None });
                ui.close();
            }
        }
    });
}
}

/// Submenu listing every registered action plus the synthetic layout
/// items. `pick` is called with the chosen id (heap String would be
/// nicer but `&'static str` keeps the closure simple — every id we
/// surface here is `'static`).
fn add_action_picker(ui: &mut egui::Ui, mut pick: impl FnMut(&'static str)) {
    let actions = ActionRegistry::all().list();
    let mut last_cat: Option<crate::actions::ActionCategory> = None;
    for a in actions {
        if last_cat != Some(a.category) {
            if last_cat.is_some() {
                ui.separator();
            }
            ui.label(
                egui::RichText::new(a.category.label())
                    .small()
                    .color(theme::muted()),
            );
            last_cat = Some(a.category);
        }
        if ui.button(a.label).clicked() {
            pick(a.id);
            ui.close();
        }
    }
}

/// Hamburger "More actions" composite. Mirrors the legacy
/// `actions_menu` shape: lists the open-tab actions and "New chat
/// session", with a combined queue+pending badge on the trigger.
impl AppState {
fn render_actions_menu(&mut self, ui: &mut egui::Ui) {
    let state = self;
    let queue_count = state.pending_task_count();
    let pending_count = state.pending_proposal_count();
    let total = queue_count + pending_count;
    let trigger = ui.horizontal(|ui| {
        let r = ui.add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Menu)));
        if total > 0 {
            ui.label(
                egui::RichText::new(format!("{}", total))
                    .small()
                    .color(egui::Color32::WHITE)
                    .background_color(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
            );
        }
        r
    });
    let trigger_resp = trigger.inner.on_hover_text("More actions");
    egui::Popup::menu(&trigger_resp).show(|ui| {
        if tab_menu_row(ui, &TabKind::Queue, queue_count) {
            open_singleton_tab(state, TabKind::Queue);
            ui.close();
        }
        if menu_row(ui, icons::ICONS.image(crate::icons::Icon::Brain), "Index", 0) {
            open_singleton_tab(state, TabKind::IndexerDetail);
            ui.close();
        }
        if tab_menu_row(ui, &TabKind::Settings, 0) {
            open_singleton_tab(state, TabKind::Settings);
            ui.close();
        }
        if tab_menu_row(ui, &TabKind::Graph { focus: None, scope_query: None }, 0) {
            open_singleton_tab(state, TabKind::Graph { focus: None, scope_query: None });
            ui.close();
        }
        // status: board-index-page
        if tab_menu_row(ui, &TabKind::BoardsIndex, 0) {
            open_singleton_tab(state, TabKind::BoardsIndex);
            ui.close();
        }
        // status: rule-firings-panel
        if tab_menu_row(ui, &TabKind::Rules, 0) {
            open_singleton_tab(state, TabKind::Rules);
            ui.close();
        }
        if tab_menu_row(ui, &TabKind::PatchReview, pending_count) {
            open_singleton_tab(state, TabKind::PatchReview);
            ui.close();
        }
        // status: diff-summary-panel
        if tab_menu_row(ui, &TabKind::GitDiff, 0) {
            open_singleton_tab(state, TabKind::GitDiff);
            ui.close();
        }
        ui.separator();
        if menu_row(ui, icons::ICONS.image(crate::icons::Icon::Plus), "New chat session", 0) {
            crate::actions::dispatch(state, "chat.new_session");
            ui.close();
        }
        ui.separator();
        if menu_row(ui, icons::ICONS.image(crate::icons::Icon::Wrench), "Customize toolbars", 0) {
            crate::actions::dispatch(state, "view.toolbar_customize");
            ui.close();
        }
        if menu_row(ui, icons::ICONS.image(crate::icons::Icon::Search), "Command palette", 0) {
            crate::actions::dispatch(state, "palette.open");
            ui.close();
        }
        // Activity-registry entries. Per `feature-consumer-hamburger`,
        // the hamburger walks the registry and renders any activity
        // that returns a `HamburgerEntry`. The hardcoded rows above
        // stay until each owning activity migrates them in.
        render_activity_hamburger_entries(ui, state);
    });
}
}

impl AppState {
/// Global "View options" menu — an eye-icon button on the top strip that
/// opens a popup of workbench-level view toggles. Modelled on
/// [`Self::render_actions_menu`]. Leaves room for future global view
/// options. [global-view-menu]
fn render_view_menu(&mut self, ui: &mut egui::Ui) {
    let state = self;
    let trigger = ui.add(egui::ImageButton::new(
        icons::ICONS.image(crate::icons::Icon::Eye),
    ));
    let trigger = trigger.on_hover_text("View options");
    egui::Popup::menu(&trigger).show(|ui| {
        // Reader mode toggle — dispatches the same global action as the
        // book button and Ctrl+R.
        let mut reader = state.workbench.reader_mode();
        if ui.checkbox(&mut reader, "Reader mode").changed() {
            crate::actions::dispatch(state, "view.reader_mode");
        }
        reader_view_options(state, ui);
    });
}
}

/// The reader-mode-specific view options (the "hide … in reader mode" toggles).
/// Shared by the eye-icon View menu and the reader-icon right-click menu, so
/// both surface the same controls. Each reads + flips the mirrored `ui.*`
/// setting and persists it vault-scoped, so the behaviour is reachable without
/// opening Settings. [view-reader-hide-top-bar, view-reader-hide-tabs,
/// view-reader-hide-toolbar]
fn reader_view_options(state: &mut AppState, ui: &mut egui::Ui) {
    let mut hide_top = state.ui.reader_hide_top_bar;
    if ui.checkbox(&mut hide_top, "Hide top bar in reader mode").changed() {
        state.ui.reader_hide_top_bar = hide_top;
        commit_vault_bool(state, "ui.reader_hide_top_bar", hide_top);
    }
    let mut hide_tabs = state.ui.reader_hide_tabs;
    if ui.checkbox(&mut hide_tabs, "Hide tabs in reader mode").changed() {
        state.ui.reader_hide_tabs = hide_tabs;
        commit_vault_bool(state, "ui.reader_hide_tabs", hide_tabs);
    }
    let mut hide_toolbar = state.ui.reader_hide_toolbar;
    if ui.checkbox(&mut hide_toolbar, "Hide toolbar in reader mode").changed() {
        state.ui.reader_hide_toolbar = hide_toolbar;
        commit_vault_bool(state, "ui.reader_hide_toolbar", hide_toolbar);
    }
}

/// Persist a vault-scoped bool config key and swap the in-memory copy so a
/// later read sees it. Mirrors the settings panel's `commit` path; toasts
/// the result. [view-reader-hide-top-bar]
fn commit_vault_bool(state: &mut AppState, key: &str, value: bool) {
    let vault_root = state.vault_session.vault_root.clone();
    match hiker_core::config::Config::set(
        hiker_core::config::SettingsScope::Vault,
        key,
        &serde_json::json!(value),
        &vault_root,
    ) {
        Ok(new_cfg) => {
            if let Ok(mut guard) = state.vault_session.config.write() {
                *guard = new_cfg;
            }
        }
        Err(err) => state.push_toast(format!("Save {key} failed: {err}"), ToastLevel::Error),
    }
}

/// Walk the per-vault activity registry and render any entries opting
/// into the hamburger menu. Built fresh per open so dynamic entries
/// (e.g. plugin activities in Phase 3) appear as soon as they register.
/// [feature-consumer-hamburger]
fn render_activity_hamburger_entries(ui: &mut egui::Ui, state: &mut AppState) {
    let activities = state.activities.clone();
    let entries: Vec<(String, &'static str)> = activities
        .iter()
        .filter_map(|f| f.hamburger().map(|h| (f.id().to_string(), h.label())))
        .collect();
    if entries.is_empty() {
        return;
    }
    ui.separator();
    for (activity_id, label) in entries {
        if ui.button(label).clicked() {
            crate::activity::dispatch_hamburger(state, &activity_id);
            ui.close();
        }
    }
}

fn tab_menu_row(ui: &mut egui::Ui, kind: &TabKind, count: usize) -> bool {
    let label = kind.label();
    menu_row(ui, kind.icon(), &label, count)
}

fn menu_row(ui: &mut egui::Ui, image: egui::Image<'_>, label: &str, count: usize) -> bool {
    let resp = ui.horizontal(|ui| {
        let r = ui.add(egui::Button::image_and_text(image, label));
        if count > 0 {
            ui.label(
                egui::RichText::new(format!("{}", count))
                    .small()
                    .color(egui::Color32::WHITE)
                    .background_color(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
            );
        }
        r
    });
    resp.inner.clicked()
}

impl AppState {
fn pending_proposal_count(&self) -> usize {
    self.ui_cache.pending_snapshot.len()
}

fn pending_task_count(&self) -> usize {
    use hiker_core::tasks::types::TaskState;
    self
        .ui_cache
        .task_snapshot
        .iter()
        .filter(|r| matches!(r.state, TaskState::Queued | TaskState::Leased))
        .count()
}
}

pub(crate) fn open_singleton_tab(state: &mut AppState, kind: TabKind) {
    let kind_disc = std::mem::discriminant(&kind);
    state.find_or_open_tab(|k| std::mem::discriminant(k) == kind_disc, || kind);
}

