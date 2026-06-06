//! Trails sidebar — the active trail rendered top-to-bottom, read-only,
//! per `docs/trails.md` §"Sidebar Trails mode". Trails are markdown
//! trail-docs on disk; this surface reads them live each frame via
//! `core::trails::list` / `get_trail` and drives every mutation (create /
//! append / remove / set-cursor / delete / activate) through the
//! `crate::trails::bridge` sync→async bridge, mirroring
//! `crate::panels::board`'s service-access + `Ctx::defer` pattern.
//!
//! Read-only invariant (`trails-mode-sidebar-read-only`): no
//! drag-to-reorder, no inline rename, no in-place annotation editing. The
//! one editing verb is per-card "Remove waypoint" (confirmed); cursor
//! verbs are "Append from here" / "Reset to main line".
//!
//! Active trail = `vault.active_trail` config. Activating a trail writes
//! that config and stamps `hiker.last_activated_at` on the trail-doc.

use eframe::egui;

use crate::activity::Ctx;
use crate::editor_pane;
use crate::trails::bridge;
use crate::trails::state::State;
use hiker_core::trails::ops::ResolutionOutcome;
use hiker_core::trails::{ResolvedWaypoint, TrailDetail, TrailListItem};
use hiker_theme as theme;

/// Deferred mutations collected while rendering the waypoint forest.
/// Rendering only borrows `&State` + the resolved `TrailDetail`; each
/// picked verb lands here and is applied afterward, dodging the
/// mutable-borrow overlap one-shot closures would hit.
#[derive(Default)]
struct TrailActions {
    open: Option<String>,
    remove: Option<String>,
    set_append: Option<String>,
    reset_append: bool,
    toggle_expand: Option<String>,
    toggle_side: Option<String>,
    /// A shared note-item base action (Open / Reveal-in-tree / Properties)
    /// picked from the waypoint card menu, paired with its note path.
    base: Option<(crate::item_menu::ItemAction, String)>,
}

/// Per-frame context for the trails sidebar. Wraps the narrow activity
/// `Ctx` so the render/mutation helpers can be `&mut self` methods on one
/// receiver (exempt from `single_call_fn`). Trail data is read live from
/// disk via [`crate::trails::bridge`]; transient UI state lives in the
/// activity's own `State` (via `ctx.state`); broad effects (open a note,
/// the remove-confirm modal, activation config write) ride `ctx.defer`.
pub(crate) struct TrailsCtx<'a, 'c> {
    pub(crate) ctx: &'a mut Ctx<'c>,
}

impl TrailsCtx<'_, '_> {
    /// Mutable handle to the activity's transient UI state slice.
    fn st(&mut self) -> &mut State {
        self.ctx.state.downcast_mut::<State>().expect("trails state")
    }

    /// Immutable handle to the activity's transient UI state slice.
    fn st_ref(&self) -> &State {
        self.ctx.state.downcast_ref::<State>().expect("trails state")
    }

    /// Push a toast onto the shared sink.
    fn toast(&mut self, message: impl Into<String>, level: crate::state::ToastLevel) {
        self.ctx.toasts.push(crate::state::Toast {
            message: message.into(),
            level,
            created_at: std::time::Instant::now(),
            undo: None,
        });
    }

    /// Create a fresh trail (default name) and activate it. The trail-doc
    /// opens in the editor so the user can name it (via the file-tree
    /// inline-rename / trail-doc body), matching the new-board gesture.
    fn create_and_activate(&mut self) {
        let name = default_trail_name(&bridge::list(self.ctx));
        match bridge::create_trail(self.ctx, &name) {
            Ok(rel) => {
                self.activate(&rel);
                let to_open = rel.clone();
                self.ctx
                    .defer(move |app| editor_pane::open_file(app, &to_open, false));
            }
            Err(e) => self.toast(format!("New trail failed: {e}"), crate::state::ToastLevel::Error),
        }
    }

    /// Activate `trail_doc_rel`: write `vault.active_trail` config (via
    /// the deferred `set_setting`) and stamp the trail-doc's activation
    /// recency. Reset the per-trail expand state so a freshly-activated
    /// trail renders clean.
    fn activate(&mut self, trail_doc_rel: &str) {
        if let Err(e) = bridge::stamp_activated(self.ctx, trail_doc_rel) {
            tracing::warn!(error = %e, trail = %trail_doc_rel, "stamp last_activated_at failed");
        }
        self.st().expanded_path = None;
        self.st().side_trail_collapsed.clear();
        let rel = trail_doc_rel.to_string();
        self.ctx.defer(move |app| {
            app.set_setting(
                hiker_core::config::SettingsScope::Vault,
                "vault.active_trail",
                &serde_json::Value::String(rel),
                "Activate trail failed",
            );
        });
    }

    /// Clear the active trail (`vault.active_trail = null`).
    fn deactivate(&mut self) {
        self.ctx.defer(|app| {
            app.set_setting(
                hiker_core::config::SettingsScope::Vault,
                "vault.active_trail",
                &serde_json::Value::Null,
                "Clear active trail failed",
            );
        });
    }

    pub(crate) fn render(&mut self, ui: &mut egui::Ui) {
        let trails = bridge::list(self.ctx);
        // Visible trail: prefer the active one, else the first listed. When
        // the vault has no trails, surface the create prompt and bail.
        let active = bridge::active_trail_rel(self.ctx);
        let visible_rel = active
            .clone()
            .filter(|rel| trails.iter().any(|t| &t.rel_path == rel))
            .or_else(|| trails.first().map(|t| t.rel_path.clone()));
        let Some(visible_rel) = visible_rel else {
            self.render_empty_state(ui);
            return;
        };

        if self.st_ref().all_trails_picker_open {
            self.render_all_trails_picker(ui, &trails, &visible_rel);
        }

        let detail = bridge::get_trail(self.ctx, &visible_rel);
        self.header_row(ui, &trails, &visible_rel, active.is_some());

        let Some(detail) = detail else {
            ui.add_space(8.0);
            ui.colored_label(error_color(), "trail unreadable");
            return;
        };

        self.cursor_hint_row(ui, &detail);
        ui.separator();

        let count = count_waypoints(&detail.waypoints);
        ui.label(
            egui::RichText::new(format!(
                "{} ({} waypoint{})",
                title_of(&detail.rel_path),
                count,
                if count == 1 { "" } else { "s" }
            ))
            .color(theme::muted())
            .small(),
        );

        if detail.waypoints.is_empty() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Empty trail - use the \"+ <trail>\" pill in the editor toolbar or the \"Add to active trail\" right-click verb to add waypoints.")
                    .color(theme::muted())
                    .italics()
                    .small(),
            );
            return;
        }

        let cursor_path = self.ctx.active_path.clone();
        let mut actions = TrailActions::default();
        {
            let view = ForestView {
                state: self.st_ref(),
                append_under: detail.append_under.as_deref(),
                active_tab: cursor_path.as_deref(),
            };
            view.forest(ui, &detail.waypoints, &mut actions);
        }
        self.apply_actions(&visible_rel, actions);
    }

    /// Empty-vault fallback: trail icon + "New trail" prompt + usage tip.
    fn render_empty_state(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(crate::icons::ICONS.trail()).on_hover_text("Trails");
            ui.label(egui::RichText::new("No trails yet").color(theme::muted()).small());
        });
        ui.add_space(8.0);
        if ui.button("New trail").clicked() {
            self.create_and_activate();
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Tip: with a trail active, use the \"+ <trail>\" pill in the editor toolbar or the \"Add to active trail\" right-click verb in the file tree to add waypoints.")
                .color(theme::muted())
                .italics()
                .small(),
        );
    }

    /// Apply the deferred verbs collected during a render pass against the
    /// trail at `trail_rel`. Mutations route through the bridge (async
    /// core) or `ctx.defer` (broad `&mut AppState`).
    fn apply_actions(&mut self, trail_rel: &str, actions: TrailActions) {
        if let Some((action, path)) = actions.base {
            self.ctx
                .defer(move |app| crate::item_menu::apply_item_action(app, action, &path));
        }
        if let Some(path) = actions.open {
            self.ctx.defer(move |app| editor_pane::open_file(app, &path, false));
        }
        if let Some(path) = actions.remove {
            self.queue_remove_modal(trail_rel, &path);
        }
        if let Some(path) = actions.set_append {
            if let Err(e) = bridge::set_append_cursor(self.ctx, trail_rel, Some(&path)) {
                self.toast(format!("Set cursor failed: {e}"), crate::state::ToastLevel::Error);
            } else {
                self.toast(
                    format!("Appending under {}", basename(&path)),
                    crate::state::ToastLevel::Info,
                );
            }
        }
        if actions.reset_append
            && let Err(e) = bridge::set_append_cursor(self.ctx, trail_rel, None)
        {
            self.toast(format!("Reset cursor failed: {e}"), crate::state::ToastLevel::Error);
        }
        if let Some(path) = actions.toggle_expand {
            self.st().toggle_expanded(&path);
        }
        if let Some(path) = actions.toggle_side {
            if !self.st().side_trail_collapsed.remove(&path) {
                self.st().side_trail_collapsed.insert(path);
            }
        }
    }

    /// Raise the danger-styled remove-confirm modal. Computes the cascade
    /// size via the bridge, then defers setting `session.modal`; the actual
    /// remove runs in the confirm handler (`ConfirmIntent::DeleteTrailWaypoint`).
    fn queue_remove_modal(&mut self, trail_rel: &str, path: &str) {
        let total = bridge::descendant_count(self.ctx, trail_rel, path);
        let sides = total.saturating_sub(1);
        let body = if sides > 0 {
            format!(
                "Remove this waypoint and {} side-trail waypoint{}? Removed waypoint-notes move to trash.",
                sides,
                if sides == 1 { "" } else { "s" },
            )
        } else {
            "Remove this waypoint? The waypoint-note moves to trash.".to_string()
        };
        let trail_doc_rel = trail_rel.to_string();
        let waypoint_path = path.to_string();
        self.ctx.defer(move |app| {
            app.session.modal = Some(crate::state::Modal::Confirm {
                title: "Remove waypoint".to_string(),
                body,
                confirm_label: "Remove".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: true,
                intent: crate::state::ConfirmIntent::DeleteTrailWaypoint {
                    trail_doc_rel,
                    waypoint_path,
                },
            });
        });
    }

    fn header_row(
        &mut self,
        ui: &mut egui::Ui,
        trails: &[TrailListItem],
        visible_rel: &str,
        has_active: bool,
    ) {
        let mut new_active: Option<String> = None;
        let mut clear_active = false;
        let mut open_picker = false;
        let mut open_trail_doc = false;
        let mut do_new_trail = false;
        let mut do_delete = false;

        ui.horizontal(|ui| {
            ui.add(crate::icons::ICONS.trail()).on_hover_text("Trails");
            header_dropdown(
                ui,
                trails,
                visible_rel,
                has_active,
                &mut new_active,
                &mut clear_active,
                &mut open_picker,
            );

            let head_btn = ui
                .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Compass)))
                .on_hover_text("Open trail-doc");
            if head_btn.clicked() {
                open_trail_doc = true;
            }

            self.expand_all_toggle(ui, !trails.is_empty());
            overflow_menu(ui, &mut do_new_trail, &mut do_delete);
        });

        if open_picker {
            self.st().all_trails_picker_open = true;
        }
        if let Some(rel) = new_active {
            self.activate(&rel);
        }
        if clear_active {
            self.deactivate();
        }
        if do_new_trail {
            self.create_and_activate();
        }
        if do_delete {
            self.delete_visible(visible_rel);
        }
        if open_trail_doc {
            let rel = visible_rel.to_string();
            self.ctx.defer(move |app| editor_pane::open_file(app, &rel, false));
        }
    }

    /// Delete the visible trail (cascade) and clear the active trail.
    fn delete_visible(&mut self, trail_rel: &str) {
        match bridge::delete_trail(self.ctx, trail_rel) {
            Ok(()) => {
                self.deactivate();
                self.toast(
                    format!("Deleted trail {}", title_of(trail_rel)),
                    crate::state::ToastLevel::Info,
                );
            }
            Err(e) => {
                self.toast(format!("Delete trail failed: {e}"), crate::state::ToastLevel::Error)
            }
        }
    }

    /// Expand-all toggle (header chevron).
    fn expand_all_toggle(&mut self, ui: &mut egui::Ui, enabled: bool) {
        let (icon, tip) = if self.st_ref().expand_all {
            (crate::icons::ICONS.image(crate::icons::Icon::ChevronDown), "Collapse all")
        } else {
            (crate::icons::ICONS.image(crate::icons::Icon::ChevronRight), "Expand all")
        };
        let mut toggled = false;
        ui.add_enabled_ui(enabled, |ui| {
            if ui.add(egui::Button::image(icon).small()).on_hover_text(tip).clicked() {
                toggled = true;
            }
        });
        if toggled {
            let on = !self.st_ref().expand_all;
            self.st().expand_all = on;
            if !on {
                self.st().expanded_path = None;
            }
        }
    }

    /// Floating "All trails" picker — flat alphabetical list over every
    /// trail in the vault. Useful when the dropdown's recent cap hides the
    /// wanted trail.
    fn render_all_trails_picker(
        &mut self,
        ui: &mut egui::Ui,
        trails: &[TrailListItem],
        visible_rel: &str,
    ) {
        let egui_ctx = ui.ctx().clone();
        let mut open = true;
        let mut to_activate: Option<String> = None;
        let mut close_after = false;
        let mut sorted: Vec<&TrailListItem> = trails.iter().collect();
        sorted.sort_by_key(|t| t.title.to_lowercase());
        egui::Window::new("All trails")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(320.0)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
            .show(&egui_ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} trail{}",
                        trails.len(),
                        if trails.len() == 1 { "" } else { "s" }
                    ))
                    .color(theme::muted())
                    .small(),
                );
                ui.separator();
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for t in sorted {
                        let prefix = if t.rel_path == visible_rel { "* " } else { "  " };
                        if ui
                            .add(
                                egui::Button::new(format!("{prefix}{}", t.title))
                                    .min_size(egui::vec2(ui.available_width(), 0.0)),
                            )
                            .clicked()
                        {
                            to_activate = Some(t.rel_path.clone());
                            close_after = true;
                        }
                    }
                });
            });
        if let Some(rel) = to_activate {
            self.activate(&rel);
        }
        if !open || close_after {
            self.st().all_trails_picker_open = false;
        }
    }

    /// Append-cursor hint row (`trail-append-cursor-indicator`): "Appending
    /// to main line" (cursor null) or "Appending under <basename>" with a
    /// "Reset to main line" button (cursor set).
    fn cursor_hint_row(&mut self, ui: &mut egui::Ui, detail: &TrailDetail) {
        let under = detail.append_under.clone();
        let exists = under
            .as_deref()
            .map(|p| find_waypoint(&detail.waypoints, p).is_some())
            .unwrap_or(false);
        let mut reset = false;
        ui.horizontal(|ui| {
            let (label, color) = match under.as_deref() {
                Some(p) if exists => {
                    (format!("Appending under {}", basename(p)), theme::accent())
                }
                Some(_) => ("Appending under (missing)".to_string(), theme::accent()),
                None => ("Appending to main line".to_string(), theme::muted()),
            };
            ui.label(egui::RichText::new(label).color(color).small());
            if under.is_some()
                && ui
                    .small_button("Reset to main line")
                    .on_hover_text("New appends land at the trail's main line")
                    .clicked()
            {
                reset = true;
            }
        });
        if reset {
            let rel = detail.rel_path.clone();
            if let Err(e) = bridge::set_append_cursor(self.ctx, &rel, None) {
                self.toast(format!("Reset cursor failed: {e}"), crate::state::ToastLevel::Error);
            }
        }
    }

}

// ===== free helpers =====

/// Error / broken-reference accent (the theme has no dedicated error token).
const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}

/// A default `new-trail-N` basename not colliding with an existing trail
/// title, mirroring the `create_with_suffix` shape `create_trail` itself
/// re-applies on disk.
fn default_trail_name(trails: &[TrailListItem]) -> String {
    if !trails.iter().any(|t| t.title == "new-trail") {
        return "new-trail".to_string();
    }
    for n in 1..1000 {
        let cand = format!("new-trail-{n}");
        if !trails.iter().any(|t| t.title == cand) {
            return cand;
        }
    }
    "new-trail".to_string()
}

/// Active-trail dropdown: "None" (clears active) + recency-ordered recent
/// trails + an "All trails…" entry into the flat picker.
fn header_dropdown(
    ui: &mut egui::Ui,
    trails: &[TrailListItem],
    visible_rel: &str,
    has_active: bool,
    new_active: &mut Option<String>,
    clear_active: &mut bool,
    open_picker: &mut bool,
) {
    let mut ordered: Vec<&TrailListItem> = trails.iter().collect();
    ordered.sort_by(|a, b| {
        b.last_activated_at
            .cmp(&a.last_activated_at)
            .then_with(|| a.title.cmp(&b.title))
    });
    let visible_name = trails
        .iter()
        .find(|t| t.rel_path == visible_rel)
        .map(|t| t.title.clone())
        .unwrap_or_else(|| "-".to_string());
    let dropdown_btn =
        ui.add(egui::Button::new(format!("{visible_name} v")).min_size(egui::vec2(0.0, 22.0)));
    const RECENT_CAP: usize = 8;
    egui::Popup::menu(&dropdown_btn).show(|ui| {
        let none_prefix = if has_active { "  " } else { "* " };
        if ui.button(format!("{none_prefix}None")).clicked() {
            *clear_active = true;
            ui.close();
        }
        ui.separator();
        for t in ordered.iter().take(RECENT_CAP) {
            let prefix = if has_active && t.rel_path == visible_rel { "* " } else { "  " };
            if ui.button(format!("{prefix}{}", t.title)).clicked() {
                *new_active = Some(t.rel_path.clone());
                ui.close();
            }
        }
        ui.separator();
        if ui.button("All trails...").clicked() {
            *open_picker = true;
            ui.close();
        }
    });
}

/// Overflow menu — New / Delete. Rename happens by renaming the trail-doc
/// in the file tree (the trail carries its identity in frontmatter), so it
/// isn't a sidebar verb under the read-only invariant.
fn overflow_menu(ui: &mut egui::Ui, do_new: &mut bool, do_delete: &mut bool) {
    let overflow = ui
        .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Menu)))
        .on_hover_text("Trail actions");
    egui::Popup::menu(&overflow).show(|ui| {
        if ui.button("New trail").clicked() {
            *do_new = true;
            ui.close();
        }
        if ui.button("Delete trail").clicked() {
            *do_delete = true;
            ui.close();
        }
    });
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Trail-doc title: basename without `.md`.
fn title_of(rel: &str) -> &str {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.strip_suffix(".md").unwrap_or(base)
}

fn count_waypoints(waypoints: &[ResolvedWaypoint]) -> usize {
    waypoints.iter().map(|w| 1 + count_waypoints(&w.children)).sum()
}

fn find_waypoint<'a>(
    waypoints: &'a [ResolvedWaypoint],
    path: &str,
) -> Option<&'a ResolvedWaypoint> {
    for w in waypoints {
        if w.waypoint_rel == path {
            return Some(w);
        }
        if let Some(found) = find_waypoint(&w.children, path) {
            return Some(found);
        }
    }
    None
}

/// First non-empty, trimmed line of `s` (collapsed-card snippet).
fn first_non_empty_line(s: &str) -> String {
    for raw in s.lines() {
        let line = raw.trim();
        if !line.is_empty() {
            return line.to_string();
        }
    }
    String::new()
}

/// Read-only render context for the waypoint forest. Rendering reads the
/// transient `State` (a shared ref) + cursor/active markers; the recursive
/// walk writes picked verbs into `TrailActions`.
struct ForestView<'a> {
    state: &'a State,
    append_under: Option<&'a str>,
    active_tab: Option<&'a str>,
}

impl ForestView<'_> {
    /// Walk a sibling list, rendering each card and recursing into any
    /// expanded side-trail children one indent deeper.
    fn forest(&self, ui: &mut egui::Ui, waypoints: &[ResolvedWaypoint], actions: &mut TrailActions) {
        for wp in waypoints {
            self.single(ui, wp, actions);
            let side_collapsed = self.state.side_trail_collapsed.contains(&wp.waypoint_rel);
            if !wp.children.is_empty() && !side_collapsed {
                ui.indent(("trail-children", &wp.waypoint_rel), |ui| {
                    ui.add_space(2.0);
                    self.forest(ui, &wp.children, actions);
                });
            }
        }
    }

    /// Per-waypoint card: header row, optional expanded body, context menu.
    fn single(&self, ui: &mut egui::Ui, wp: &ResolvedWaypoint, actions: &mut TrailActions) {
        let orphan = matches!(wp.resolution, ResolutionOutcome::Orphan);
        let is_cursor = self.append_under == Some(wp.waypoint_rel.as_str());
        let is_active_tab = self.active_tab == Some(wp.source_path.as_str());
        let expanded = self.state.is_expanded(&wp.waypoint_rel);

        let (fill, stroke_color) = if orphan {
            (egui::Color32::from_rgb(0x3a, 0x36, 0x36), egui::Color32::from_rgb(0xb9, 0x6a, 0x6a))
        } else if is_cursor {
            (theme::active_bg(), theme::accent())
        } else {
            (theme::active_bg(), theme::divider())
        };
        let card_frame = egui::Frame::default()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke_color))
            .inner_margin(egui::Margin::symmetric(6, 4));

        let card = CardCtx { wp, orphan, is_cursor, is_active_tab, expanded };
        let resp = card_frame
            .show(ui, |ui| {
                self.card_header(ui, &card, actions);
                self.card_body(ui, &card, actions);
            })
            .response;
        let resp = resp.interact(egui::Sense::click());
        if !orphan && resp.clicked() {
            actions.open = Some(wp.source_path.clone());
        }
        waypoint_context_menu(&resp, wp, orphan, is_cursor, actions);
        ui.add_space(2.0);
    }

    /// Header row: side-trail collapse chevron, ordinal, cursor/active
    /// markers, file/warning icon, basename, expand chevron.
    fn card_header(&self, ui: &mut egui::Ui, card: &CardCtx, actions: &mut TrailActions) {
        let wp = card.wp;
        ui.horizontal(|ui| {
            if !wp.children.is_empty() {
                let collapsed = self.state.side_trail_collapsed.contains(&wp.waypoint_rel);
                let (icon, tip) = if collapsed {
                    (crate::icons::ICONS.image(crate::icons::Icon::ChevronRight), "Expand side trail")
                } else {
                    (crate::icons::ICONS.image(crate::icons::Icon::ChevronDown), "Collapse side trail")
                };
                if ui.add(egui::Button::image(icon).small()).on_hover_text(tip).clicked() {
                    actions.toggle_side = Some(wp.waypoint_rel.clone());
                }
            }
            ui.label(
                egui::RichText::new(&wp.tree_path).color(theme::muted()).small().monospace(),
            );
            if card.is_cursor {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Walk).tint(theme::accent()))
                    .on_hover_text("Append cursor - new appends land here");
            } else if card.is_active_tab {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Dot).tint(theme::muted()))
                    .on_hover_text("Currently open");
            }
            if card.orphan {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Warning).tint(theme::muted()));
            } else {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::File));
            }
            let base = basename(&wp.source_path);
            let title_text = if card.orphan {
                egui::RichText::new(base).strong().color(theme::muted()).strikethrough()
            } else {
                egui::RichText::new(base).strong()
            };
            ui.add(egui::Label::new(title_text).truncate()).on_hover_text(&wp.source_path);
            if card.orphan {
                ui.label(
                    egui::RichText::new("broken reference")
                        .color(egui::Color32::from_rgb(0xb9, 0x6a, 0x6a))
                        .small()
                        .italics(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let icon = if card.expanded {
                    crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
                } else {
                    crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
                };
                if ui.add(egui::Button::image(icon).small()).clicked() {
                    actions.toggle_expand = Some(wp.waypoint_rel.clone());
                }
            });
        });
    }

    /// Below-header content: collapsed annotation snippet, or (when
    /// expanded) the full annotation + an action row.
    fn card_body(&self, ui: &mut egui::Ui, card: &CardCtx, actions: &mut TrailActions) {
        let wp = card.wp;
        if !card.expanded {
            let snippet = first_non_empty_line(&wp.annotation_body);
            if !snippet.is_empty() {
                ui.label(egui::RichText::new(snippet).color(theme::muted()).small());
            }
            return;
        }
        ui.add_space(2.0);
        if wp.annotation_body.trim().is_empty() {
            ui.label(
                egui::RichText::new("(no annotation)").color(theme::muted()).small().italics(),
            );
        } else {
            ui.label(egui::RichText::new(&wp.annotation_body).small());
        }
        ui.horizontal(|ui| {
            if !card.orphan && ui.small_button("Open source").clicked() {
                actions.open = Some(wp.source_path.clone());
            }
            // The waypoint-note is editable in the full editor (annotation
            // editing is not a sidebar verb per the read-only invariant).
            if ui.small_button("Edit annotation").clicked() {
                actions.base = Some((
                    crate::item_menu::ItemAction::Open,
                    wp.waypoint_rel.clone(),
                ));
            }
            if !card.orphan && !card.is_cursor && ui.small_button("Append here").clicked() {
                actions.set_append = Some(wp.waypoint_rel.clone());
            }
        });
    }
}

/// Right-click verbs: shared base (Open / Reveal / Properties), Remove,
/// Append-from-here / Reset-cursor.
fn waypoint_context_menu(
    resp: &egui::Response,
    wp: &ResolvedWaypoint,
    orphan: bool,
    is_cursor: bool,
    actions: &mut TrailActions,
) {
    let mut chosen = None;
    resp.context_menu(|ui| {
        chosen = egui_workbench::menu::show(ui, build_waypoint_menu(wp, orphan, is_cursor));
    });
    if let Some(verb) = chosen {
        match verb {
            WaypointVerb::Base(action) => actions.base = Some((action, wp.source_path.clone())),
            WaypointVerb::Remove => actions.remove = Some(wp.waypoint_rel.clone()),
            WaypointVerb::AppendFromHere => actions.set_append = Some(wp.waypoint_rel.clone()),
            WaypointVerb::ResetAppend => actions.reset_append = true,
        }
    }
}

/// Right-click verbs for a trail waypoint card.
#[derive(Clone, Copy)]
enum WaypointVerb {
    Base(crate::item_menu::ItemAction),
    Remove,
    AppendFromHere,
    ResetAppend,
}

/// Build the right-click menu for a trail waypoint card (status:
/// ctxmenu-trails). A resolved source note gets the universal Open /
/// Reveal-in-tree / Properties base; a broken (orphan) reference keeps
/// just the trail extras. "Append from here" only when resolved and not
/// the current cursor; "Reset append cursor" only on the cursor row.
fn build_waypoint_menu(
    wp: &ResolvedWaypoint,
    orphan: bool,
    is_cursor: bool,
) -> egui_workbench::menu::Menu<WaypointVerb> {
    let mut menu = egui_workbench::menu::Menu::new();
    if !orphan {
        menu = crate::item_menu::note_item_base(
            &wp.source_path,
            crate::item_menu::BaseOpts { reveal: true },
            WaypointVerb::Base,
        )
        .section();
    }
    menu = menu.action("Remove from trail", WaypointVerb::Remove);
    menu = menu.action_with(
        egui_workbench::menu::Action::new("Append from here", WaypointVerb::AppendFromHere).enabled(
            if !orphan && !is_cursor {
                egui_workbench::menu::Enabled::Yes
            } else {
                egui_workbench::menu::Enabled::No(std::borrow::Cow::Borrowed(""))
            },
        ),
    );
    if is_cursor {
        menu = menu.action("Reset append cursor", WaypointVerb::ResetAppend);
    }
    menu
}

/// Precomputed per-card flags threaded into the card sub-render methods.
struct CardCtx<'a> {
    wp: &'a ResolvedWaypoint,
    orphan: bool,
    is_cursor: bool,
    is_active_tab: bool,
    expanded: bool,
}
