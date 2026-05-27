//! Trails sidebar - named trails with ordered waypoints. Per
//! `docs/trails.md` and the legacy `ui/src/trails/index.ts` reference
//! implementation. Read-only editing surface (no drag-to-reorder, no
//! inline annotation editing); the single in-sidebar editing verbs are
//! the per-card "Remove waypoint" and "Append from here" context-menu
//! entries plus the overflow menu (New / Rename / Delete).

use eframe::egui;

use crate::editor_pane;
use crate::state::{create_trail, AppState, Waypoint};
use crate::theme;

/// Deferred mutations collected while rendering the waypoint forest.
/// Rendering only borrows `&AppState`; each picked verb lands here and is
/// applied against `&mut AppState` after the render pass, dodging the
/// mutable-borrow overlap that one-shot closures would otherwise hit.
#[derive(Default)]
struct TrailActions {
    open: Option<String>,
    remove: Option<String>,
    set_append: Option<String>,
    toggle_expand: Option<String>,
    toggle_side: Option<String>,
    start_annot: Option<String>,
    save_annot: Option<(String, String)>,
    cancel_annot: bool,
    update_annot: Option<String>,
    move_op: Option<(String, crate::state::MoveOp)>,
}

/// Render context for the trails sidebar. Bundles `ui` + `state` so the
/// per-row helpers are `&mut self` methods (exempt from
/// `single_call_fn`) instead of free functions threading the same refs.
/// Construct with `TrailsView { ui, state }` and call `render`.
pub(crate) struct TrailsView<'a> {
    pub(crate) ui: &'a mut egui::Ui,
    pub(crate) state: &'a mut AppState,
}

impl TrailsView<'_> {
pub(crate) fn render(&mut self) {
    // Pick a visible trail: prefer the active one, else fall back to the
    // first trail. When no trails exist, surface a "New trail" prompt and
    // bail — the header/cursor rows have nothing to render.
    let visible_id = self
        .state.session.active_trail
        .clone()
        .filter(|id| self.state.session.trails.iter().any(|t| &t.id == id))
        .or_else(|| self.state.session.trails.first().map(|t| t.id.clone()));
    let Some(visible_id) = visible_id else {
        self.render_empty_state();
        return;
    };

    if self.state.panels.trails_ui.all_trails_picker_open {
        self.render_all_trails_picker(&visible_id);
    }

    self.header_row(&visible_id);
    self.rename_row(&visible_id);
    self.cursor_hint_row(&visible_id);

    self.ui.separator();

    let snapshot = self
        .state.session.trails
        .iter()
        .find(|t| t.id == visible_id)
        .map(|t| (t.name.clone(), t.waypoints.clone(), t.append_under.clone()))
        .unwrap_or_default();

    self.ui.label(
        egui::RichText::new(format!(
            "{} ({} waypoint{})",
            snapshot.0,
            snapshot.1.len(),
            if snapshot.1.len() == 1 { "" } else { "s" }
        ))
        .color(theme::muted())
        .small(),
    );

    if snapshot.1.is_empty() {
        self.ui.add_space(8.0);
        self.ui.label(
            egui::RichText::new("Empty trail - use the \"+\" pill in the editor toolbar or the \"Add to trail\" right-click verb to add waypoints.")
                .color(theme::muted())
                .italics()
                .small(),
        );
        return;
    }

    let cursor_path: Option<String> = self
        .state.session.active_tab
        .and_then(|id| self.state.tab_by_id(id))
        .and_then(|t| t.buffer_path().map(str::to_string));

    let mut actions = TrailActions::default();
    // Reorder via drag-and-drop: dropping in a card's top edge band inserts
    // before it (top band of the first card = trail head), the bottom edge
    // band inserts after it (bottom band of the last card = trail tail), and
    // the middle band nests as a child. See `resolve_drop` / `drop_band`.
    self.render_waypoints(
        &snapshot.1,
        cursor_path.as_deref(),
        snapshot.2.as_deref(),
        &mut Vec::new(),
        &mut actions,
    );
    self.apply_actions(&visible_id, actions);
}

/// Empty-trails fallback: trail icon + "New trail" prompt + usage tip.
fn render_empty_state(&mut self) {
    let state = &mut *self.state;
    self.ui.horizontal(|ui| {
        ui.add(crate::icons::ICONS.trail()).on_hover_text("Trails");
        ui.label(
            egui::RichText::new("No trails yet")
                .color(theme::muted())
                .small(),
        );
    });
    self.ui.add_space(8.0);
    if self.ui.button("New trail").clicked() {
        let name = format!("Trail {}", state.session.trails.len() + 1);
        let id = create_trail(state, &name);
        state.session.active_trail = Some(id);
        let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
    }
    self.ui.add_space(8.0);
    self.ui.label(
        egui::RichText::new("Tip: with a trail active, use the \"+\" pill in the editor toolbar or the \"Add to trail\" right-click verb in the file tree to add waypoints.")
            .color(theme::muted())
            .italics()
            .small(),
    );
}

/// Apply the deferred verbs collected during a render pass against the
/// trail identified by `visible_id`, persisting where the verb mutates
/// the trail forest.
fn apply_actions(&mut self, visible_id: &str, actions: TrailActions) {
    let state = &mut *self.state;
    if let Some((src, op)) = actions.move_op {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id) {
            // Resetting the append cursor whenever it might dangle is
            // simpler than tracking subtree moves precisely.
            trail.append_under = None;
            if trail.move_waypoint(&src, op) {
                let _ = crate::bootstrap::save_trails(
                    &state.vault_session.vault_root,
                    &state.session.trails,
                );
            }
        }
    }
    if let Some(path) = actions.open {
        editor_pane::open_file(state, &path, false);
    }
    if let Some(path) = actions.remove {
        // Mirrors the legacy `removeWaypoint` flow: compute the cascade
        // size and raise a danger-styled confirm modal; the actual drop
        // happens in the modal's confirm handler.
        let total = state
            .session.trails
            .iter()
            .find(|t| t.id == visible_id)
            .map(|t| descendant_count(&t.waypoints, &path))
            .unwrap_or(0);
        let sides = total.saturating_sub(1);
        let body = if sides > 0 {
            format!(
                "Remove this waypoint and {} side-trail waypoint{}? The trail entry is dropped - the underlying notes stay on disk.",
                sides,
                if sides == 1 { "" } else { "s" },
            )
        } else {
            "Remove this waypoint? The trail entry is dropped - the underlying note stays on disk."
                .to_string()
        };
        state.session.modal = Some(crate::state::Modal::Confirm {
            title: "Remove waypoint".to_string(),
            body,
            confirm_label: "Remove".to_string(),
            cancel_label: "Cancel".to_string(),
            danger: true,
            intent: crate::state::ConfirmIntent::DeleteTrailWaypoint {
                trail_id: visible_id.to_string(),
                path,
            },
        });
    }
    if let Some(path) = actions.set_append {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id) {
            trail.append_under = Some(path.clone());
        }
        let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        state.push_toast(
            format!("Appending under {}", basename(&path)),
            crate::state::ToastLevel::Info,
        );
    }
    if let Some(path) = actions.toggle_expand {
        if state.panels.trails_ui.expanded_path.as_deref() == Some(&path) {
            state.panels.trails_ui.expanded_path = None;
        } else {
            state.panels.trails_ui.expanded_path = Some(path);
        }
    }
    if let Some(path) = actions.toggle_side {
        if !state.panels.trails_ui.side_trail_collapsed.remove(&path) {
            state.panels.trails_ui.side_trail_collapsed.insert(path);
        }
    }
    if let Some(path) = actions.start_annot {
        let cur = state
            .session.trails
            .iter()
            .find(|t| t.id == visible_id)
            .and_then(|t| find_waypoint(&t.waypoints, &path).map(|w| w.annotation.clone()))
            .unwrap_or_default();
        state.panels.trails_ui.annotation_edit = Some((path, cur));
    }
    if let Some(text) = actions.update_annot
        && let Some((_, draft)) = state.panels.trails_ui.annotation_edit.as_mut()
    {
        *draft = text;
    }
    if actions.cancel_annot {
        state.panels.trails_ui.annotation_edit = None;
    }
    if let Some((path, body)) = actions.save_annot {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id)
            && let Some(wp) = crate::state::find_waypoint_mut(&mut trail.waypoints, &path)
        {
            wp.annotation = body;
            let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        }
        state.panels.trails_ui.annotation_edit = None;
    }
}

fn header_row(&mut self, visible_id: &str) {
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    let visible_name = state
        .session.trails
        .iter()
        .find(|t| t.id == visible_id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "-".to_string());

    let mut open_trail_doc: Option<String> = None;

    ui.horizontal(|ui| {
        ui.add(crate::icons::ICONS.trail()).on_hover_text("Trails");

        // Dropdown button - popover with "None"-like behavior replaced
        // by the always-present "Recent" trail (the new app's stand-in
        // for legacy's None). Legacy `trails-mode-active-trail-dropdown`
        // ordering: most-recently-activated first, alphabetical on tie.
        let mut ordered: Vec<crate::state::Trail> = state.session.trails.clone();
        ordered.sort_by(|a, b| {
            b.last_activated_at_ms
                .cmp(&a.last_activated_at_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut new_active: Option<String> = None;
        let dropdown_btn = ui.add(
            egui::Button::new(format!("{visible_name} v"))
                .min_size(egui::vec2(0.0, 22.0)),
        );
        const RECENT_CAP: usize = 8;
        let mut open_picker = false;
        egui::Popup::menu(&dropdown_btn).show(|ui| {
            for t in ordered.iter().take(RECENT_CAP) {
                let selected = t.id == visible_id;
                let prefix = if selected { "* " } else { "  " };
                if ui
                    .button(format!("{prefix}{}", t.name))
                    .clicked()
                {
                    new_active = Some(t.id.clone());
                    ui.close();
                }
            }
            if ordered.len() > RECENT_CAP {
                ui.separator();
                if ui.button("All trails...").clicked() {
                    open_picker = true;
                    ui.close();
                }
            } else {
                // Always offer the flat picker - even with a small list
                // it's a familiar entry point (legacy parity).
                ui.separator();
                if ui.button("All trails...").clicked() {
                    open_picker = true;
                    ui.close();
                }
            }
        });
        if open_picker {
            state.panels.trails_ui.all_trails_picker_open = true;
        }
        if let Some(id) = new_active {
            activate_trail(state, &id);
        }

        // Trail-head icon (legacy `trails-mode-trail-head-icon`): opens
        // the trail-doc. The new app generates the doc on demand under
        // `.hiker/trails/<slug>.md`.
        let head_btn = ui
            .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Compass)))
            .on_hover_text("Open trail-doc");
        if head_btn.clicked() {
            open_trail_doc = Some(visible_id.to_string());
        }

        // Expand-all toggle (legacy header chevron).
        let has_waypoints = state
            .session.trails
            .iter()
            .find(|t| t.id == visible_id)
            .map(|t| !t.waypoints.is_empty())
            .unwrap_or(false);
        let (icon, tip) = if state.panels.trails_ui.expand_all {
            (crate::icons::ICONS.image(crate::icons::Icon::ChevronDown), "Collapse all")
        } else {
            (crate::icons::ICONS.image(crate::icons::Icon::ChevronRight), "Expand all")
        };
        ui.add_enabled_ui(has_waypoints, |ui| {
            if ui
                .add(egui::Button::image(icon).small())
                .on_hover_text(tip)
                .clicked()
            {
                state.panels.trails_ui.expand_all = !state.panels.trails_ui.expand_all;
                if !state.panels.trails_ui.expand_all {
                    state.panels.trails_ui.expanded_path = None;
                }
            }
        });

        // Overflow menu - New / Rename / Delete. Legacy keeps these out
        // of the always-visible sidebar; the overflow is the new-app
        // affordance so users can still manage trails without opening
        // the editor.
        let overflow = ui
            .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Menu)))
            .on_hover_text("Trail actions");
        egui::Popup::menu(&overflow).show(|ui| {
            if ui.button("New trail").clicked() {
                let name = format!("Trail {}", state.session.trails.len() + 1);
                let id = create_trail(state, &name);
                state.session.active_trail = Some(id);
                let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
                ui.close();
            }
            {
                if ui.button("Rename trail").clicked() {
                    let cur = state
                        .session.trails
                        .iter()
                        .find(|t| t.id == visible_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_default();
                    state.session.trail_rename = Some((visible_id.to_string(), cur));
                    ui.close();
                }
                if ui.button("Delete trail").clicked() {
                    state.session.trails.retain(|t| t.id != visible_id);
                    state.session.active_trail = None;
                    state.session.trail_rename = None;
                    let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
                    ui.close();
                }
            }
        });
    });

    if let Some(id) = open_trail_doc {
        self.write_and_open_trail_doc(&id);
    }
}

/// Floating "All trails" picker - flat alphabetical list over every
/// trail in the vault, with a search box. Legacy `openAllTrailsPicker`
/// equivalent; useful when the dropdown's recent cap hides the trail
/// the user wants.
fn render_all_trails_picker(&mut self, visible_id: &str) {
    let ctx = self.ui.ctx().clone();
    let state = &mut *self.state;
    let mut open = true;
    let mut to_activate: Option<String> = None;
    let mut close_after = false;
    egui::Window::new("All trails")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(320.0)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .show(&ctx, |ui| {
            ui.label(
                egui::RichText::new(format!("{} trail{}", state.session.trails.len(),
                    if state.session.trails.len() == 1 { "" } else { "s" }))
                    .color(theme::muted())
                    .small(),
            );
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    let mut sorted: Vec<&crate::state::Trail> = state.session.trails.iter().collect();
                    sorted.sort_by_key(|t| t.name.to_lowercase());
                    for t in sorted {
                        let selected = t.id == visible_id;
                        let prefix = if selected { "* " } else { "  " };
                        if ui
                            .add(egui::Button::new(format!("{prefix}{}", t.name))
                                .min_size(egui::vec2(ui.available_width(), 0.0)))
                            .clicked()
                        {
                            to_activate = Some(t.id.clone());
                            close_after = true;
                        }
                    }
                });
        });
    if let Some(id) = to_activate {
        activate_trail(state, &id);
    }
    if !open || close_after {
        state.panels.trails_ui.all_trails_picker_open = false;
    }
}

fn rename_row(&mut self, visible_id: &str) {
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    let Some((rename_id, _)) = state.session.trail_rename.clone() else {
        return;
    };
    if rename_id != visible_id {
        return;
    }
    let mut draft = state
        .session.trail_rename
        .as_ref()
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    ui.horizontal(|ui| {
        let resp = ui.add(egui::TextEdit::singleline(&mut draft).hint_text("Trail name"));
        resp.request_focus();
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if enter {
            if let Some(t) = state.session.trails.iter_mut().find(|t| t.id == visible_id)
                && !draft.trim().is_empty()
            {
                t.name = draft.trim().to_string();
                let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
            }
            state.session.trail_rename = None;
        } else if escape || (resp.lost_focus() && !enter) {
            state.session.trail_rename = None;
        } else if let Some((_, text)) = state.session.trail_rename.as_mut() {
            *text = draft;
        }
    });
}

fn cursor_hint_row(&mut self, visible_id: &str) {
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    let (under, exists) = state
        .session.trails
        .iter()
        .find(|t| t.id == visible_id)
        .map(|t| {
            let exists = t
                .append_under
                .as_ref()
                .map(|p| find_waypoint(&t.waypoints, p).is_some())
                .unwrap_or(false);
            (t.append_under.clone(), exists)
        })
        .unwrap_or((None, false));

    // Legacy `trail-append-cursor-indicator`: header hint row beneath
    // the dropdown / expand-all row. When the cursor is set we render
    // the label in the accent color and surface a "Reset to root"
    // button - without that button the user has no way to clear an
    // active append cursor (`trail-reset-cursor-verb`).
    ui.horizontal(|ui| {
        let (label, color) = match under.as_deref() {
            Some(p) if exists => (
                format!("Appending under {}", basename(p)),
                theme::accent(),
            ),
            Some(_) => ("Appending under (missing)".to_string(), theme::accent()),
            None => ("Appending to root".to_string(), theme::muted()),
        };
        ui.label(egui::RichText::new(label).color(color).small());
        if under.is_some()
            && ui
                .small_button("Reset to root")
                .on_hover_text("Stop appending under cursor - new visits land at the trail root")
                .clicked()
        {
            if let Some(t) = state.session.trails.iter_mut().find(|t| t.id == visible_id) {
                t.append_under = None;
            }
            let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        }
    });
}

fn render_waypoints(
    &mut self,
    waypoints: &[Waypoint],
    cursor_path: Option<&str>,
    append_under: Option<&str>,
    ordinal: &mut Vec<usize>,
    actions: &mut TrailActions,
) {
    let state: &AppState = self.state;
    let mut wv = WaypointView { ui: self.ui, state };
    wv.forest(waypoints, cursor_path, append_under, ordinal, actions);
}

/// Regenerate `.hiker/trails/<slug>.md` from the trail's current
/// waypoint forest and open it in the editor. Legacy parity for the
/// trail-head verb - in the new app the JSON is authoritative, so the
/// doc is overwritten each open.
fn write_and_open_trail_doc(&mut self, trail_id: &str) {
    let state = &mut *self.state;
    let Some(trail) = state.session.trails.iter().find(|t| t.id == trail_id) else {
        return;
    };
    // Slug the trail name into a filesystem-safe stem: lowercase,
    // collapse non-alphanumeric runs to a single `-`, trim edge dashes.
    let mut stem = String::new();
    let mut last_dash = false;
    for ch in trail.name.chars() {
        if ch.is_alphanumeric() {
            stem.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            stem.push('-');
            last_dash = true;
        }
    }
    let stem = stem.trim_matches('-').to_string();
    let stem = if stem.is_empty() {
        trail_id.to_string()
    } else {
        stem
    };
    let rel = format!(".hiker/trails/{}.md", stem);
    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", trail.name));
    if trail.waypoints.is_empty() {
        body.push_str("_(no waypoints yet)_\n");
    } else {
        write_trail_doc_section(&mut body, &trail.waypoints, &mut Vec::new());
    }
    let abs = state.vault_session.vault_root.join(&rel);
    if let Some(parent) = abs.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&abs, body).is_err() {
        state.push_toast("Failed to write trail-doc", crate::state::ToastLevel::Error);
        return;
    }
    state.session.sidebar.dir_cache.remove(".hiker/trails");
    state.session.sidebar.dir_cache.remove(".hiker");
    crate::editor_pane::open_file(state, &rel, false);
}

}

// ===== free helpers =====

fn write_trail_doc_section(out: &mut String, waypoints: &[Waypoint], ordinal: &mut Vec<usize>) {
    for (idx, wp) in waypoints.iter().enumerate() {
        ordinal.push(idx + 1);
        let tree = ordinal
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(".");
        let base = wp.path.rsplit('/').next().unwrap_or(wp.path.as_str());
        out.push_str(&format!("- **{}** [[{}|{}]]\n", tree, wp.path, base));
        for line in wp.annotation.lines() {
            if !line.trim().is_empty() {
                out.push_str(&format!("  > {}\n", line));
            }
        }
        if !wp.children.is_empty() {
            write_trail_doc_section(out, &wp.children, ordinal);
        }
        ordinal.pop();
    }
}

fn activate_trail(state: &mut AppState, id: &str) {
    state.session.active_trail = Some(id.to_string());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Some(t) = state.session.trails.iter_mut().find(|t| t.id == id) {
        t.last_activated_at_ms = now_ms;
    }
    let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn find_waypoint<'a>(waypoints: &'a [Waypoint], path: &str) -> Option<&'a Waypoint> {
    for w in waypoints {
        if w.path == path {
            return Some(w);
        }
        if let Some(found) = find_waypoint(&w.children, path) {
            return Some(found);
        }
    }
    None
}

/// Read-only render context for the waypoint forest. Rendering only
/// reads `AppState` (a shared ref), so the recursive forest walk can
/// freely re-borrow `state` inside `ui.indent` closures while writing
/// picked verbs into `TrailActions`. The per-card helpers are `&mut
/// self` methods, exempt from `single_call_fn`.
struct WaypointView<'a> {
    ui: &'a mut egui::Ui,
    state: &'a AppState,
}

impl WaypointView<'_> {
    /// Walk a sibling list, rendering each card and recursing into any
    /// expanded side-trail children one indent deeper.
    fn forest(
        &mut self,
        waypoints: &[Waypoint],
        cursor_path: Option<&str>,
        append_under: Option<&str>,
        ordinal: &mut Vec<usize>,
        actions: &mut TrailActions,
    ) {
        for (idx, wp) in waypoints.iter().enumerate() {
            ordinal.push(idx + 1);
            let tree_path = ordinal
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(".");
            self.single(wp, &tree_path, cursor_path, append_under, actions);
            let side_collapsed =
                self.state.panels.trails_ui.side_trail_collapsed.contains(&wp.path);
            if !wp.children.is_empty() && !side_collapsed {
                let state = self.state;
                self.ui.indent(("trail-children", &wp.path), |ui| {
                    ui.add_space(2.0);
                    let mut child = WaypointView { ui, state };
                    child.forest(&wp.children, cursor_path, append_under, ordinal, actions);
                });
            }
            ordinal.pop();
        }
    }

    /// Per-waypoint card: drag-drop frame, header row, expanded body, and
    /// context menu. Each sub-piece is its own method so the orchestrator
    /// stays readable and under the line budget.
    fn single(
        &mut self,
        wp: &Waypoint,
        tree_path: &str,
        cursor_path: Option<&str>,
        append_under: Option<&str>,
        actions: &mut TrailActions,
    ) {
        let base = basename(&wp.path);
        let exists = self.state.vault_session.vault_root.join(&wp.path).exists();
        let is_cursor = append_under == Some(wp.path.as_str());
        let is_active_tab = cursor_path == Some(wp.path.as_str());
        let expanded = self.state.panels.trails_ui.expand_all
            || self.state.panels.trails_ui.expanded_path.as_deref() == Some(wp.path.as_str());

        let (fill, stroke_color) = if !exists {
            (
                egui::Color32::from_rgb(0x3a, 0x36, 0x36),
                egui::Color32::from_rgb(0xb9, 0x6a, 0x6a),
            )
        } else if is_cursor {
            (theme::active_bg(), theme::accent())
        } else {
            (theme::active_bg(), theme::divider())
        };

        let card_frame = egui::Frame::default()
            .fill(fill)
            .stroke(egui::Stroke::new(1.0, stroke_color))
            .inner_margin(egui::Margin::symmetric(6, 4));
        let drag_id = self.ui.make_persistent_id(("trail-wp-drag", &wp.path));
        let card = CardCtx { wp, tree_path, base, exists, is_cursor, is_active_tab, expanded };
        let state = self.state;
        let (zone_resp, drop_payload) =
            self.ui.dnd_drop_zone::<String, _>(card_frame, |ui| {
                let drag = ui.dnd_drag_source(drag_id, wp.path.clone(), |ui| {
                    let mut row = WaypointView { ui, state };
                    row.card_header(&card, actions);
                    row.card_body(&card, actions);
                });
                drag.response
            });
        let drag_resp = zone_resp.inner;
        self.resolve_drop(wp, &drag_resp, drop_payload, exists, actions);
        self.waypoint_context_menu(wp, &drag_resp, exists, is_cursor, actions);
        self.ui.add_space(2.0);
    }

    /// Header row of a card: collapse chevron, ordinal, cursor/active
    /// markers, file/warning icon, title, and the expand chevron.
    fn card_header(&mut self, card: &CardCtx, actions: &mut TrailActions) {
        let wp = card.wp;
        let state = self.state;
        self.ui.horizontal(|ui| {
            // Side-trail collapse chevron (only when has children).
            if !wp.children.is_empty() {
                let side_collapsed =
                    state.panels.trails_ui.side_trail_collapsed.contains(&wp.path);
                let (icon, tip) = if side_collapsed {
                    (crate::icons::ICONS.image(crate::icons::Icon::ChevronRight), "Expand side trail")
                } else {
                    (crate::icons::ICONS.image(crate::icons::Icon::ChevronDown), "Collapse side trail")
                };
                if ui
                    .add(egui::Button::image(icon).small())
                    .on_hover_text(tip)
                    .clicked()
                {
                    actions.toggle_side = Some(wp.path.clone());
                }
            }

            // Sequence ordinal (legacy `tree_path`).
            ui.label(
                egui::RichText::new(card.tree_path)
                    .color(theme::muted())
                    .small()
                    .monospace(),
            );

            // Append-cursor / active-tab indicator.
            if card.is_cursor {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Walk).tint(theme::accent()))
                    .on_hover_text("Append cursor - new visits land here");
            } else if card.is_active_tab {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Dot).tint(theme::muted()))
                    .on_hover_text("Currently open");
            }

            // File / warning icon.
            if card.exists {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::File));
            } else {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Warning).tint(theme::muted()));
            }

            // Basename (strong).
            let title_text = if card.exists {
                egui::RichText::new(card.base).strong()
            } else {
                egui::RichText::new(card.base)
                    .strong()
                    .color(theme::muted())
                    .strikethrough()
            };
            ui.add(egui::Label::new(title_text).truncate())
                .on_hover_text(&wp.path);

            if !card.exists {
                ui.label(
                    egui::RichText::new("broken reference")
                        .color(egui::Color32::from_rgb(0xb9, 0x6a, 0x6a))
                        .small()
                        .italics(),
                );
            }

            // Right-edge expand chevron.
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let icon = if card.expanded {
                        crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
                    } else {
                        crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
                    };
                    if ui.add(egui::Button::image(icon).small()).clicked() {
                        actions.toggle_expand = Some(wp.path.clone());
                    }
                },
            );
        });
    }

    /// Below-header content: collapsed snippet, or (when expanded) the
    /// parent path, timestamp, annotation editor/text, and action row.
    fn card_body(&mut self, card: &CardCtx, actions: &mut TrailActions) {
        let wp = card.wp;
        let state = self.state;
        if !card.expanded {
            let snippet = self.first_non_empty_line(&wp.annotation);
            if !snippet.is_empty() {
                self.ui.label(
                    egui::RichText::new(snippet)
                        .color(theme::muted())
                        .small(),
                );
            }
            return;
        }
        self.ui.add_space(2.0);
        if let Some((parent, _)) = wp.path.rsplit_once('/') {
            self.ui.label(
                egui::RichText::new(parent)
                    .color(theme::muted())
                    .small()
                    .monospace(),
            );
        }
        if wp.at_ms > 0 {
            let ts = self.format_ts(wp.at_ms);
            self.ui.label(
                egui::RichText::new(ts)
                    .color(theme::muted())
                    .small(),
            );
        }
        self.ui.add_space(2.0);

        // Annotation body - inline editor when this waypoint is the
        // current annotation-edit target, else a paragraph / placeholder.
        let editing = state
            .panels.trails_ui
            .annotation_edit
            .as_ref()
            .map(|(p, _)| p == &wp.path)
            .unwrap_or(false);
        if editing {
            let mut draft = state
                .panels.trails_ui
                .annotation_edit
                .as_ref()
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            let resp = self.ui.add(
                egui::TextEdit::multiline(&mut draft)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("Annotation (markdown)"),
            );
            if resp.changed() {
                actions.update_annot = Some(draft.clone());
            }
            let escape = self.ui.input(|i| i.key_pressed(egui::Key::Escape));
            self.ui.horizontal(|ui| {
                if ui.small_button("Save").clicked() {
                    actions.save_annot = Some((wp.path.clone(), draft.clone()));
                }
                if ui.small_button("Cancel").clicked() || escape {
                    actions.cancel_annot = true;
                }
            });
        } else if wp.annotation.trim().is_empty() {
            self.ui.label(
                egui::RichText::new("(no annotation)")
                    .color(theme::muted())
                    .small()
                    .italics(),
            );
        } else {
            self.ui.label(egui::RichText::new(&wp.annotation).small());
        }

        self.ui.horizontal(|ui| {
            if card.exists && ui.small_button("Open").clicked() {
                actions.open = Some(wp.path.clone());
            }
            if !editing && ui.small_button("Edit annotation").clicked() {
                actions.start_annot = Some(wp.path.clone());
            }
            if card.exists && !card.is_cursor && ui.small_button("Append here").clicked() {
                actions.set_append = Some(wp.path.clone());
            }
        });
    }

    /// Resolve a card's drop payload + whole-frame click into open/move
    /// verbs. The pointer's vertical band within the card decides placement
    /// (`drop_band`): top edge inserts a sibling before, bottom edge a
    /// sibling after, the middle nests as a child. While a drag hovers the
    /// card, paints a single insertion-line (above/below) or outline (into)
    /// so the target is visible before release. Self-drops are ignored.
    fn resolve_drop(
        &mut self,
        wp: &Waypoint,
        drag_resp: &egui::Response,
        drop_payload: Option<std::sync::Arc<String>>,
        exists: bool,
        actions: &mut TrailActions,
    ) {
        let card_rect = drag_resp.rect;
        let pointer_y = self
            .ui
            .input(|i| i.pointer.interact_pos())
            .map(|p| p.y)
            .unwrap_or(card_rect.center().y);
        let band = drop_band(pointer_y, card_rect.top(), card_rect.bottom());

        // Live feedback while a waypoint is being dragged over this card.
        let dragging =
            egui::DragAndDrop::has_payload_of_type::<String>(self.ui.ctx());
        if dragging && self.ui.rect_contains_pointer(card_rect) {
            self.paint_drop_indicator(card_rect, band);
        }

        if let Some(src) = drop_payload {
            let op = match band {
                DropBand::Above => crate::state::MoveOp::Before(wp.path.clone()),
                DropBand::Below => crate::state::MoveOp::After(wp.path.clone()),
                DropBand::Into => crate::state::MoveOp::Child(wp.path.clone()),
            };
            if (*src) != wp.path {
                actions.move_op = Some(((*src).clone(), op));
            }
        }
        if exists && drag_resp.clicked() {
            actions.open = Some(wp.path.clone());
        }
    }

    /// Paint the drag-target hint for `band` over `card_rect`: a horizontal
    /// insertion line at the card's top or bottom edge for sibling drops, or
    /// a full-card outline for a nest-as-child drop.
    fn paint_drop_indicator(&self, card_rect: egui::Rect, band: DropBand) {
        let painter = self.ui.painter();
        let accent = theme::accent();
        match band {
            DropBand::Above | DropBand::Below => {
                let y = if band == DropBand::Above {
                    card_rect.top()
                } else {
                    card_rect.bottom()
                };
                painter.hline(
                    card_rect.x_range(),
                    y,
                    egui::Stroke::new(2.0, accent),
                );
            }
            DropBand::Into => {
                painter.rect_stroke(
                    card_rect,
                    2.0,
                    egui::Stroke::new(1.5, accent),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }

    /// Right-click verbs: Open, Remove, Append-from-here / reset cursor.
    fn waypoint_context_menu(
        &mut self,
        wp: &Waypoint,
        drag_resp: &egui::Response,
        exists: bool,
        is_cursor: bool,
        actions: &mut TrailActions,
    ) {
        drag_resp.context_menu(|ui| {
            if exists && ui.button("Open").clicked() {
                actions.open = Some(wp.path.clone());
                ui.close();
            }
            if ui.button("Remove from trail").clicked() {
                actions.remove = Some(wp.path.clone());
                ui.close();
            }
            ui.add_enabled_ui(exists && !is_cursor, |ui| {
                if ui.button("Append from here").clicked() {
                    actions.set_append = Some(wp.path.clone());
                    ui.close();
                }
            });
            if is_cursor && ui.button("Reset append cursor").clicked() {
                actions.set_append = None;
                // Cursor reset routes through the hint-row button; the
                // context menu surface keeps the verb discoverable per
                // `trail-reset-cursor-verb`.
                ui.close();
            }
        });
    }

    /// First non-empty, trimmed line of `s` (collapsed-card snippet).
    fn first_non_empty_line(&self, s: &str) -> String {
        for raw in s.lines() {
            let line = raw.trim();
            if !line.is_empty() {
                return line.to_string();
            }
        }
        String::new()
    }

    /// Coarse "visited Xm ago" relative timestamp from epoch millis. We
    /// avoid pulling in chrono for a muted footnote.
    fn format_ts(&self, ms: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let delta = (now - ms).max(0) / 1000;
        if delta < 60 {
            format!("visited {}s ago", delta)
        } else if delta < 3600 {
            format!("visited {}m ago", delta / 60)
        } else if delta < 86400 {
            format!("visited {}h ago", delta / 3600)
        } else {
            format!("visited {}d ago", delta / 86400)
        }
    }
}

/// Precomputed per-card flags + display strings, threaded into the card
/// sub-render methods so each takes a single struct rather than a long
/// argument list.
struct CardCtx<'a> {
    wp: &'a Waypoint,
    tree_path: &'a str,
    base: &'a str,
    exists: bool,
    is_cursor: bool,
    is_active_tab: bool,
    expanded: bool,
}

/// Count of waypoints rooted at `path` (inclusive). 1 = leaf,
/// \>1 = side-trail head with that many total nodes. Returns 0 when
/// the path isn't found anywhere in the forest.
fn descendant_count(waypoints: &[Waypoint], path: &str) -> usize {
    fn subtree(wp: &Waypoint) -> usize {
        1 + wp.children.iter().map(subtree).sum::<usize>()
    }
    for wp in waypoints {
        if wp.path == path {
            return subtree(wp);
        }
        let c = descendant_count(&wp.children, path);
        if c > 0 {
            return c;
        }
    }
    0
}

/// Where a drag-release over a waypoint card lands relative to that card.
/// Decided purely from the pointer's vertical position within the row rect:
/// the top edge band inserts the dragged waypoint as a sibling *before* the
/// card, the bottom edge band as a sibling *after* it, and the wide middle
/// band nests it as a child (side-trail re-parent). The above/below bands
/// replace the old discrete head/tail drop strips — dropping in the top band
/// of the first card lands at the trail head, the bottom band of the last
/// card at the tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropBand {
    Above,
    Into,
    Below,
}

/// Fraction of a card's height claimed by each edge band (top/bottom). The
/// middle `1 - 2*EDGE` is the nest-as-child zone. 0.3 keeps "above/below"
/// the dominant gesture (the bug owner's ask) while leaving a comfortable
/// central target for re-parenting into a side trail.
const DROP_EDGE_FRACTION: f32 = 0.3;

/// Decide the drop band from the pointer's y against a row rect. Pure so it
/// can be unit-tested without an egui context. Degenerate (zero-height)
/// rects fall back to a top/bottom split at the midpoint with no middle band.
fn drop_band(pointer_y: f32, top: f32, bottom: f32) -> DropBand {
    let height = bottom - top;
    if height <= 0.0 {
        return if pointer_y < top { DropBand::Above } else { DropBand::Below };
    }
    let edge = height * DROP_EDGE_FRACTION;
    if pointer_y < top + edge {
        DropBand::Above
    } else if pointer_y > bottom - edge {
        DropBand::Below
    } else {
        DropBand::Into
    }
}

#[cfg(test)]
mod drop_band_tests {
    use super::{drop_band, DropBand};

    // Row spanning y in [100, 200]; edge bands are the top/bottom 30px.
    #[test]
    fn top_edge_is_above() {
        assert_eq!(drop_band(105.0, 100.0, 200.0), DropBand::Above);
        assert_eq!(drop_band(129.0, 100.0, 200.0), DropBand::Above);
    }
    #[test]
    fn bottom_edge_is_below() {
        assert_eq!(drop_band(195.0, 100.0, 200.0), DropBand::Below);
        assert_eq!(drop_band(171.0, 100.0, 200.0), DropBand::Below);
    }
    #[test]
    fn middle_is_into() {
        assert_eq!(drop_band(150.0, 100.0, 200.0), DropBand::Into);
        assert_eq!(drop_band(131.0, 100.0, 200.0), DropBand::Into);
        assert_eq!(drop_band(169.0, 100.0, 200.0), DropBand::Into);
    }
    #[test]
    fn degenerate_rect_splits_at_top() {
        assert_eq!(drop_band(99.0, 100.0, 100.0), DropBand::Above);
        assert_eq!(drop_band(101.0, 100.0, 100.0), DropBand::Below);
    }
}

/// Mirrors the legacy `removeWaypoint` flow: fetch the cascade size,
/// show a danger-styled confirm modal with side-trail count, and on
/// approval drop the waypoint + toast with the cascaded count.
#[cfg(test)]
mod remove_tests {
    use crate::state::Waypoint;

    fn remove_waypoint_recursive(waypoints: &mut Vec<Waypoint>, path: &str) -> bool {
        if let Some(pos) = waypoints.iter().position(|w| w.path == path) {
            waypoints.remove(pos);
            return true;
        }
        for child in waypoints.iter_mut() {
            if remove_waypoint_recursive(&mut child.children, path) {
                return true;
            }
        }
        false
    }

    fn wp(path: &str, children: Vec<Waypoint>) -> Waypoint {
        Waypoint {
            path: path.to_string(),
            at_ms: 0,
            children,
            annotation: String::new(),
        }
    }

    #[test]
    fn removes_top_level_waypoint() {
        let mut v = vec![wp("a.md", vec![]), wp("b.md", vec![])];
        assert!(remove_waypoint_recursive(&mut v, "a.md"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "b.md");
    }

    #[test]
    fn removes_nested_waypoint() {
        let mut v = vec![wp(
            "root.md",
            vec![wp("nested.md", vec![wp("deep.md", vec![])])],
        )];
        assert!(remove_waypoint_recursive(&mut v, "deep.md"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].children.len(), 1);
        assert!(v[0].children[0].children.is_empty());
    }

    #[test]
    fn returns_false_on_missing_path() {
        let mut v = vec![wp("a.md", vec![])];
        assert!(!remove_waypoint_recursive(&mut v, "missing.md"));
        assert_eq!(v.len(), 1);
    }
}

