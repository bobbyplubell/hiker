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

use crate::actions::{self, ActionRegistry, ID_ACTIONS_MENU, ID_SEP, ID_SPACER, ID_VAULT_LABEL};
use crate::icons;
use crate::state::{AppState, Toolbar, ToolbarSide, ToastLevel};
use crate::tab::TabKind;
use crate::theme;

/// Render every configured toolbar. Call once per frame, BEFORE side
/// panels — top/bottom panels claim their strip first.
pub fn render_all(ctx: &egui::Context, app: &mut AppState) {
    // Clone the bars list (cheap — just a Vec of small structs) so we can
    // mutate `app` while iterating.
    let bars = app.ui.toolbars.bars.clone();
    for (idx, bar) in bars.iter().enumerate() {
        let panel_id = format!("toolbar-{}-{idx}", bar.id);
        match bar.side {
            ToolbarSide::Top => {
                egui::TopBottomPanel::top(panel_id)
                    .frame(panel_frame(ctx))
                    .show(ctx, |ui| render_bar_horizontal(ui, app, idx));
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
fn render_bar_items(
    ui: &mut egui::Ui,
    app: &mut AppState,
    bar_idx: usize,
    vertical: bool,
) {
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
        ui.with_layout(layout, |ui| {
            for (slot, id) in ordered {
                render_single(ui, app, bar_idx, *slot, id, customize, &mut pending);
            }
        });
    }
    if customize {
        customize_add_button(ui, app, bar_idx, &mut pending);
    }

    apply_pending(app, bar_idx, pending);
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

fn apply_pending(app: &mut AppState, bar_idx: usize, ops: Vec<BarOp>) {
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
            render_vault_label(ui, app);
        }
        ID_ACTIONS_MENU => {
            render_actions_menu(ui, app);
        }
        _ => {
            render_action_button(ui, app, bar_idx, slot, id, customize, pending);
        }
    }
}

fn render_vault_label(ui: &mut egui::Ui, state: &mut AppState) {
    let vault_label = state
        .vault_session
        .vault_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| state.vault_session.vault_root.to_string_lossy().into_owned());
    let resp = ui.label(egui::RichText::new(vault_label).color(theme::muted()));
    resp.context_menu(|ui| {
        if ui.button("Set as default vault").clicked() {
            let p = state.vault_session.vault_root.to_string_lossy().to_string();
            match hiker_core::config::Config::set(
                hiker_core::config::SettingsScope::User,
                "vault.default",
                serde_json::json!(p),
                &state.vault_session.vault_root,
            ) {
                Ok(_) => state.push_toast("Set as default vault", ToastLevel::Info),
                Err(err) => state.push_toast(
                    format!("Set default failed: {err}"),
                    ToastLevel::Error,
                ),
            }
            ui.close();
        }
    });
}

fn render_action_button(
    ui: &mut egui::Ui,
    app: &mut AppState,
    bar_idx: usize,
    slot: usize,
    id: &str,
    customize: bool,
    pending: &mut Vec<BarOp>,
) {
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
                let btn = ui.add_enabled(enabled, egui::Button::image((action.icon)()));
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

/// "+ Add" / "+ New toolbar" affordance shown at the end of every bar
/// while in customize mode.
fn customize_add_button(
    ui: &mut egui::Ui,
    _app: &mut AppState,
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
/// session", with a combined queue+staging badge on the trigger.
fn render_actions_menu(ui: &mut egui::Ui, state: &mut AppState) {
    let queue_count = pending_task_count(state);
    let staging_count = pending_staging_count(state);
    let total = queue_count + staging_count;
    let trigger = ui.horizontal(|ui| {
        let r = ui.add(egui::Button::image(icons::menu()));
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
        if tab_menu_row(ui, TabKind::Queue, queue_count) {
            open_singleton_tab(state, TabKind::Queue);
            ui.close();
        }
        if menu_row(ui, icons::brain(), "Index", 0) {
            open_singleton_tab(state, TabKind::IndexerDetail);
            ui.close();
        }
        if tab_menu_row(ui, TabKind::Settings, 0) {
            open_singleton_tab(state, TabKind::Settings);
            ui.close();
        }
        if tab_menu_row(ui, TabKind::Graph, 0) {
            open_singleton_tab(state, TabKind::Graph);
            ui.close();
        }
        if tab_menu_row(ui, TabKind::PatchReview, staging_count) {
            open_singleton_tab(state, TabKind::PatchReview);
            ui.close();
        }
        if tab_menu_row(ui, TabKind::AgentChanges, 0) {
            open_singleton_tab(state, TabKind::AgentChanges);
            ui.close();
        }
        if tab_menu_row(ui, TabKind::Plugins, 0) {
            open_singleton_tab(state, TabKind::Plugins);
            ui.close();
        }
        ui.separator();
        if menu_row(ui, icons::plus(), "New chat session", 0) {
            crate::actions::dispatch(state, "chat.new_session");
            ui.close();
        }
        ui.separator();
        if menu_row(ui, icons::wrench(), "Customize toolbars", 0) {
            crate::actions::dispatch(state, "view.toolbar_customize");
            ui.close();
        }
        if menu_row(ui, icons::search(), "Command palette", 0) {
            crate::actions::dispatch(state, "palette.open");
            ui.close();
        }
    });
}

fn tab_menu_row(ui: &mut egui::Ui, kind: TabKind, count: usize) -> bool {
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

fn pending_staging_count(state: &AppState) -> usize {
    state.ui_cache.staging_snapshot.len()
}

fn pending_task_count(state: &AppState) -> usize {
    use hiker_core::tasks::TaskState;
    state
        .ui_cache
        .task_snapshot
        .iter()
        .filter(|r| matches!(r.state, TaskState::Queued | TaskState::Leased))
        .count()
}

/// Queue a vault switch, prompting first if any buffer is dirty so we
/// don't silently discard unsaved work. Matches the legacy guard from
/// `ui/src/topBar/index.ts` where the picker asks before tearing down.
pub(crate) fn queue_vault_switch(state: &mut AppState, path: std::path::PathBuf) {
    let dirty: Vec<String> = state
        .session
        .buffers
        .iter()
        .filter(|(_, b)| b.is_dirty())
        .map(|(p, _)| p.clone())
        .collect();
    if dirty.is_empty() {
        state.vault_switch = crate::state::VaultSwitchState::Requested(path.clone());
        state.push_toast(
            format!("Switching vault to {}", path.display()),
            ToastLevel::Info,
        );
        return;
    }
    let body = format!(
        "{} unsaved buffer{} will be discarded:\n  {}",
        dirty.len(),
        if dirty.len() == 1 { "" } else { "s" },
        dirty.join("\n  "),
    );
    state.session.modal = Some(crate::state::Modal::Confirm {
        title: "Switch vault?".into(),
        body,
        confirm_label: "Discard and switch".into(),
        cancel_label: "Cancel".into(),
        danger: true,
        intent: crate::state::ConfirmIntent::SwitchVault { path: path.clone() },
    });
}

pub(crate) fn open_singleton_tab(state: &mut AppState, kind: TabKind) {
    let kind_disc = std::mem::discriminant(&kind);
    state.find_or_open_tab(|k| std::mem::discriminant(k) == kind_disc, || kind);
}
