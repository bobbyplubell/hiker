//! Trails sidebar - named trails with ordered waypoints. Per
//! `docs/trails.md` and the legacy `ui/src/trails/index.ts` reference
//! implementation. Read-only editing surface (no drag-to-reorder, no
//! inline annotation editing); the single in-sidebar editing verbs are
//! the per-card "Remove waypoint" and "Append from here" context-menu
//! entries plus the overflow menu (New / Rename / Delete).
#![allow(clippy::items_after_test_module)]

use eframe::egui;

use crate::editor_pane;
use crate::state::{create_trail, AppState, Waypoint};
use crate::theme;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    // Pick a visible trail: prefer the active one, else fall back to the
    // first trail. When no trails exist, surface a "New trail" prompt and
    // bail — the header/cursor rows have nothing to render.
    let visible_id = state
        .session.active_trail
        .clone()
        .filter(|id| state.session.trails.iter().any(|t| &t.id == id))
        .or_else(|| state.session.trails.first().map(|t| t.id.clone()));
    let Some(visible_id) = visible_id else {
        ui.horizontal(|ui| {
            ui.add(crate::icons::trail()).on_hover_text("Trails");
            ui.label(
                egui::RichText::new("No trails yet")
                    .color(theme::muted())
                    .small(),
            );
        });
        ui.add_space(8.0);
        if ui.button("New trail").clicked() {
            let name = format!("Trail {}", state.session.trails.len() + 1);
            let id = create_trail(state, &name);
            state.session.active_trail = Some(id);
            let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Tip: with a trail active, use the \"+\" pill in the editor toolbar or the \"Add to trail\" right-click verb in the file tree to add waypoints.")
                .color(theme::muted())
                .italics()
                .small(),
        );
        return;
    };

    if state.panels.trails_ui.all_trails_picker_open {
        render_all_trails_picker(ui.ctx(), state, &visible_id);
    }

    header_row(ui, state, &visible_id);
    rename_row(ui, state, &visible_id);
    cursor_hint_row(ui, state, &visible_id);

    ui.separator();

    let snapshot = state
        .session.trails
        .iter()
        .find(|t| t.id == visible_id)
        .map(|t| (t.name.clone(), t.waypoints.clone(), t.append_under.clone()))
        .unwrap_or_default();

    ui.label(
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
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("Empty trail - use the \"+\" pill in the editor toolbar or the \"Add to trail\" right-click verb to add waypoints.")
                .color(theme::muted())
                .italics()
                .small(),
        );
        return;
    }

    let cursor_path: Option<String> = state
        .session.active_tab
        .and_then(|id| state.tab_by_id(id))
        .and_then(|t| t.buffer_path().map(str::to_string));

    let mut to_open: Option<String> = None;
    let mut to_remove: Option<String> = None;
    let mut to_set_append: Option<String> = None;
    let mut to_toggle_expand: Option<String> = None;
    let mut to_toggle_side: Option<String> = None;
    let mut to_start_annot: Option<String> = None;
    let mut to_save_annot: Option<(String, String)> = None;
    let mut to_cancel_annot: bool = false;
    let mut to_update_annot: Option<String> = None;
    let mut to_move: Option<(String, crate::state::MoveOp)> = None;
    // "Drop here for start of trail" zone above the first card.
    head_drop_zone(ui, &mut to_move);
    render_waypoints(
        ui,
        state,
        &snapshot.1,
        cursor_path.as_deref(),
        snapshot.2.as_deref(),
        &mut Vec::new(),
        &mut to_open,
        &mut to_remove,
        &mut to_set_append,
        &mut to_toggle_expand,
        &mut to_toggle_side,
        &mut to_start_annot,
        &mut to_save_annot,
        &mut to_cancel_annot,
        &mut to_update_annot,
        &mut to_move,
        /*is_root=*/ true,
    );
    // "Drop here for end of trail" zone below the last card.
    tail_drop_zone(ui, &mut to_move);
    if let Some((src, op)) = to_move {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id) {
            // Resetting the append cursor whenever it might dangle is
            // simpler than tracking subtree moves precisely.
            trail.append_under = None;
            if crate::state::move_waypoint(&mut trail.waypoints, &src, op) {
                let _ = crate::bootstrap::save_trails(
                    &state.vault_session.vault_root,
                    &state.session.trails,
                );
            }
        }
    }
    if let Some(path) = to_open {
        editor_pane::open_file(state, &path, false);
    }
    if let Some(path) = to_remove {
        prompt_remove_waypoint(state, visible_id.clone(), path);
    }
    if let Some(path) = to_set_append {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id) {
            trail.append_under = Some(path.clone());
        }
        let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        state.push_toast(
            format!("Appending under {}", basename(&path)),
            crate::state::ToastLevel::Info,
        );
    }
    if let Some(path) = to_toggle_expand {
        if state.panels.trails_ui.expanded_path.as_deref() == Some(&path) {
            state.panels.trails_ui.expanded_path = None;
        } else {
            state.panels.trails_ui.expanded_path = Some(path);
        }
    }
    if let Some(path) = to_toggle_side {
        if !state.panels.trails_ui.side_trail_collapsed.remove(&path) {
            state.panels.trails_ui.side_trail_collapsed.insert(path);
        }
    }
    if let Some(path) = to_start_annot {
        let cur = state
            .session.trails
            .iter()
            .find(|t| t.id == visible_id)
            .and_then(|t| find_waypoint(&t.waypoints, &path).map(|w| w.annotation.clone()))
            .unwrap_or_default();
        state.panels.trails_ui.annotation_edit = Some((path, cur));
    }
    if let Some(text) = to_update_annot
        && let Some((_, draft)) = state.panels.trails_ui.annotation_edit.as_mut()
    {
        *draft = text;
    }
    if to_cancel_annot {
        state.panels.trails_ui.annotation_edit = None;
    }
    if let Some((path, body)) = to_save_annot {
        if let Some(trail) = state.session.trails.iter_mut().find(|t| t.id == visible_id)
            && let Some(wp) = crate::state::find_waypoint_mut(&mut trail.waypoints, &path)
        {
            wp.annotation = body;
            let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
        }
        state.panels.trails_ui.annotation_edit = None;
    }
}

fn header_row(ui: &mut egui::Ui, state: &mut AppState, visible_id: &str) {
    let visible_name = state
        .session.trails
        .iter()
        .find(|t| t.id == visible_id)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "-".to_string());

    let mut open_trail_doc: Option<String> = None;

    ui.horizontal(|ui| {
        ui.add(crate::icons::trail()).on_hover_text("Trails");

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
            .add(egui::Button::image(crate::icons::compass()))
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
            (crate::icons::chevron_down(), "Collapse all")
        } else {
            (crate::icons::chevron_right(), "Expand all")
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
            .add(egui::Button::image(crate::icons::menu()))
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
        write_and_open_trail_doc(state, &id);
    }
}

/// Slug a trail name into a filesystem-safe stem: lowercase, collapse
/// non-alphanumeric runs into a single `-`, trim leading/trailing `-`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Regenerate `.hiker/trails/<slug>.md` from the trail's current
/// waypoint forest and open it in the editor. Legacy parity for the
/// trail-head verb - in the new app the JSON is authoritative, so the
/// doc is overwritten each open.
fn write_and_open_trail_doc(state: &mut AppState, trail_id: &str) {
    let Some(trail) = state.session.trails.iter().find(|t| t.id == trail_id) else {
        return;
    };
    let stem = slug(&trail.name);
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

fn write_trail_doc_section(out: &mut String, waypoints: &[Waypoint], ordinal: &mut Vec<usize>) {
    for (idx, wp) in waypoints.iter().enumerate() {
        ordinal.push(idx + 1);
        let tree = ordinal
            .iter()
            .map(|n| n.to_string())
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

/// Floating "All trails" picker - flat alphabetical list over every
/// trail in the vault, with a search box. Legacy `openAllTrailsPicker`
/// equivalent; useful when the dropdown's recent cap hides the trail
/// the user wants.
fn render_all_trails_picker(ctx: &egui::Context, state: &mut AppState, visible_id: &str) {
    let mut open = true;
    let mut to_activate: Option<String> = None;
    let mut close_after = false;
    egui::Window::new("All trails")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(320.0)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .show(ctx, |ui| {
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

fn rename_row(ui: &mut egui::Ui, state: &mut AppState, visible_id: &str) {
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

fn cursor_hint_row(ui: &mut egui::Ui, state: &mut AppState, visible_id: &str) {
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

#[allow(clippy::too_many_arguments)]
fn render_waypoints(
    ui: &mut egui::Ui,
    state: &AppState,
    waypoints: &[Waypoint],
    cursor_path: Option<&str>,
    append_under: Option<&str>,
    ordinal: &mut Vec<usize>,
    to_open: &mut Option<String>,
    to_remove: &mut Option<String>,
    to_set_append: &mut Option<String>,
    to_toggle_expand: &mut Option<String>,
    to_toggle_side: &mut Option<String>,
    to_start_annot: &mut Option<String>,
    to_save_annot: &mut Option<(String, String)>,
    to_cancel_annot: &mut bool,
    to_update_annot: &mut Option<String>,
    to_move: &mut Option<(String, crate::state::MoveOp)>,
    _is_root: bool,
) {
    for (idx, wp) in waypoints.iter().enumerate() {
        ordinal.push(idx + 1);
        let tree_path = ordinal
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(".");
        render_single_waypoint(
            ui,
            state,
            wp,
            &tree_path,
            cursor_path,
            append_under,
            to_open,
            to_remove,
            to_set_append,
            to_toggle_expand,
            to_toggle_side,
            to_start_annot,
            to_save_annot,
            to_cancel_annot,
            to_update_annot,
            to_move,
        );
        let side_collapsed = state.panels.trails_ui.side_trail_collapsed.contains(&wp.path);
        if !wp.children.is_empty() && !side_collapsed {
            ui.indent(("trail-children", &wp.path), |ui| {
                ui.add_space(2.0);
                render_waypoints(
                    ui,
                    state,
                    &wp.children,
                    cursor_path,
                    append_under,
                    ordinal,
                    to_open,
                    to_remove,
                    to_set_append,
                    to_toggle_expand,
                    to_toggle_side,
                    to_start_annot,
                    to_save_annot,
                    to_cancel_annot,
                    to_update_annot,
                    to_move,
                    /*is_root=*/ false,
                );
            });
        }
        ordinal.pop();
    }
}

/// Drop strip above the first waypoint — releasing a dragged card here
/// makes it the new head of the trail.
fn head_drop_zone(ui: &mut egui::Ui, to_move: &mut Option<(String, crate::state::MoveOp)>) {
    let frame = egui::Frame::default().inner_margin(egui::Margin::symmetric(0, 2));
    let (_, payload) = ui.dnd_drop_zone::<String, _>(frame, |ui| {
        ui.allocate_response(
            egui::vec2(ui.available_width(), 6.0),
            egui::Sense::hover(),
        );
    });
    if let Some(src) = payload {
        *to_move = Some(((*src).clone(), crate::state::MoveOp::Head));
    }
}

/// Drop strip below the last waypoint — releasing a dragged card here
/// makes it the new tail of the trail.
fn tail_drop_zone(ui: &mut egui::Ui, to_move: &mut Option<(String, crate::state::MoveOp)>) {
    let frame = egui::Frame::default().inner_margin(egui::Margin::symmetric(0, 4));
    let (_, payload) = ui.dnd_drop_zone::<String, _>(frame, |ui| {
        ui.allocate_response(
            egui::vec2(ui.available_width(), 12.0),
            egui::Sense::hover(),
        );
    });
    if let Some(src) = payload {
        *to_move = Some(((*src).clone(), crate::state::MoveOp::Tail));
    }
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

/// Mirrors the legacy `removeWaypoint` flow: fetch the cascade size,
/// show a danger-styled confirm modal with side-trail count, and on
/// approval drop the waypoint + toast with the cascaded count.
fn prompt_remove_waypoint(state: &mut AppState, trail_id: String, path: String) {
    let total = state
        .session.trails
        .iter()
        .find(|t| t.id == trail_id)
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
            trail_id,
            path,
        },
    });
}

#[allow(dead_code)] // used by tests below; keep for forthcoming UI surface
fn subtree_size(waypoints: &[Waypoint]) -> usize {
    waypoints
        .iter()
        .map(|w| 1 + subtree_size(&w.children))
        .sum()
}

#[allow(dead_code)] // used by tests below; keep for forthcoming UI surface
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

#[cfg(test)]
mod remove_tests {
    use super::*;
    use crate::state::Waypoint;

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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_single_waypoint(
    ui: &mut egui::Ui,
    state: &AppState,
    wp: &Waypoint,
    tree_path: &str,
    cursor_path: Option<&str>,
    append_under: Option<&str>,
    to_open: &mut Option<String>,
    to_remove: &mut Option<String>,
    to_set_append: &mut Option<String>,
    to_toggle_expand: &mut Option<String>,
    to_toggle_side: &mut Option<String>,
    to_start_annot: &mut Option<String>,
    to_save_annot: &mut Option<(String, String)>,
    to_cancel_annot: &mut bool,
    to_update_annot: &mut Option<String>,
    to_move: &mut Option<(String, crate::state::MoveOp)>,
) {
    let base = basename(&wp.path);
    let exists = state.vault_session.vault_root.join(&wp.path).exists();
    let is_cursor = append_under == Some(wp.path.as_str());
    let is_active_tab = cursor_path == Some(wp.path.as_str());
    let expanded = state.panels.trails_ui.expand_all
        || state.panels.trails_ui.expanded_path.as_deref() == Some(wp.path.as_str());

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
    let drag_id = ui.make_persistent_id(("trail-wp-drag", &wp.path));
    let (zone_resp, drop_payload) = ui.dnd_drop_zone::<String, _>(card_frame, |ui| {
        let drag = ui.dnd_drag_source(drag_id, wp.path.clone(), |ui| {
            ui.horizontal(|ui| {
                // Side-trail collapse chevron (only when has children).
                if !wp.children.is_empty() {
                    let side_collapsed =
                        state.panels.trails_ui.side_trail_collapsed.contains(&wp.path);
                    let (icon, tip) = if side_collapsed {
                        (crate::icons::chevron_right(), "Expand side trail")
                    } else {
                        (crate::icons::chevron_down(), "Collapse side trail")
                    };
                    if ui
                        .add(egui::Button::image(icon).small())
                        .on_hover_text(tip)
                        .clicked()
                    {
                        *to_toggle_side = Some(wp.path.clone());
                    }
                }

                // Sequence ordinal (legacy `tree_path`).
                ui.label(
                    egui::RichText::new(tree_path)
                        .color(theme::muted())
                        .small()
                        .monospace(),
                );

                // Append-cursor indicator.
                if is_cursor {
                    ui.add(crate::icons::walk().tint(theme::accent()))
                        .on_hover_text("Append cursor - new visits land here");
                } else if is_active_tab {
                    ui.add(crate::icons::dot().tint(theme::muted()))
                        .on_hover_text("Currently open");
                }

                // File / warning icon.
                if exists {
                    ui.add(crate::icons::file());
                } else {
                    ui.add(crate::icons::warning().tint(theme::muted()));
                }

                // Basename (strong).
                let title_text = if exists {
                    egui::RichText::new(base).strong()
                } else {
                    egui::RichText::new(base)
                        .strong()
                        .color(theme::muted())
                        .strikethrough()
                };
                ui.add(egui::Label::new(title_text).truncate())
                    .on_hover_text(&wp.path);

                if !exists {
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
                        let icon = if expanded {
                            crate::icons::chevron_down()
                        } else {
                            crate::icons::chevron_right()
                        };
                        if ui.add(egui::Button::image(icon).small()).clicked() {
                            *to_toggle_expand = Some(wp.path.clone());
                        }
                    },
                );
            });

            // Collapsed-card snippet: first non-empty line of the
            // annotation, dimmed and small. Legacy `firstNonEmptyLine`.
            if !expanded {
                let snippet = first_non_empty_line(&wp.annotation);
                if !snippet.is_empty() {
                    ui.label(
                        egui::RichText::new(snippet)
                            .color(theme::muted())
                            .small(),
                    );
                }
            }

            if expanded {
                ui.add_space(2.0);
                if let Some((parent, _)) = wp.path.rsplit_once('/') {
                    ui.label(
                        egui::RichText::new(parent)
                            .color(theme::muted())
                            .small()
                            .monospace(),
                    );
                }
                if wp.at_ms > 0 {
                    ui.label(
                        egui::RichText::new(format_ts(wp.at_ms))
                            .color(theme::muted())
                            .small(),
                    );
                }
                ui.add_space(2.0);

                // Annotation body - inline editor when this waypoint is
                // the current annotation-edit target, otherwise rendered
                // as a wrapped paragraph (or muted placeholder when
                // empty). Legacy `waypoint-card-body` + "edit
                // annotation" verb.
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
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut draft)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .hint_text("Annotation (markdown)"),
                    );
                    if resp.changed() {
                        *to_update_annot = Some(draft.clone());
                    }
                    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                    ui.horizontal(|ui| {
                        if ui.small_button("Save").clicked() {
                            *to_save_annot = Some((wp.path.clone(), draft.clone()));
                        }
                        if ui.small_button("Cancel").clicked() || escape {
                            *to_cancel_annot = true;
                        }
                    });
                } else if wp.annotation.trim().is_empty() {
                    ui.label(
                        egui::RichText::new("(no annotation)")
                            .color(theme::muted())
                            .small()
                            .italics(),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(&wp.annotation).small(),
                    );
                }

                ui.horizontal(|ui| {
                    if exists && ui.small_button("Open").clicked() {
                        *to_open = Some(wp.path.clone());
                    }
                    if !editing && ui.small_button("Edit annotation").clicked() {
                        *to_start_annot = Some(wp.path.clone());
                    }
                    if exists && !is_cursor && ui.small_button("Append here").clicked() {
                        *to_set_append = Some(wp.path.clone());
                    }
                });
            }
        });
        drag.response
    });
    let drag_resp = zone_resp.inner;

    // Resolve the drop payload to a move op: drops in the top half of
    // the card become "sibling before me", drops in the bottom half
    // become "append as my child". Equal hitboxes so reorder vs. nest
    // are both easy to land. Self-drops are ignored.
    if let Some(src) = drop_payload {
        let card_rect = drag_resp.rect;
        let pointer_y = ui
            .input(|i| i.pointer.interact_pos())
            .map(|p| p.y)
            .unwrap_or(card_rect.center().y);
        let op = if pointer_y < card_rect.center().y {
            crate::state::MoveOp::Before(wp.path.clone())
        } else {
            crate::state::MoveOp::Child(wp.path.clone())
        };
        if (*src) != wp.path {
            *to_move = Some(((*src).clone(), op));
        }
    }

    // Whole-frame click: open the file if it exists and the click
    // didn't land on one of the interactive buttons inside.
    if exists && drag_resp.clicked() {
        *to_open = Some(wp.path.clone());
    }
    drag_resp.context_menu(|ui| {
        if exists && ui.button("Open").clicked() {
            *to_open = Some(wp.path.clone());
            ui.close();
        }
        if ui.button("Remove from trail").clicked() {
            *to_remove = Some(wp.path.clone());
            ui.close();
        }
        let already_cursor = is_cursor;
        ui.add_enabled_ui(exists && !already_cursor, |ui| {
            if ui.button("Append from here").clicked() {
                *to_set_append = Some(wp.path.clone());
                ui.close();
            }
        });
        if already_cursor && ui.button("Reset append cursor").clicked() {
            *to_set_append = None;
            // Cursor reset routes through the hint-row button; the
            // context menu surface keeps the verb discoverable per
            // `trail-reset-cursor-verb`.
            ui.close();
        }
    });

    ui.add_space(2.0);
}

fn first_non_empty_line(s: &str) -> String {
    for raw in s.lines() {
        let line = raw.trim();
        if !line.is_empty() {
            return line.to_string();
        }
    }
    String::new()
}

fn format_ts(ms: i64) -> String {
    // Coarse human-readable timestamp: seconds-since-epoch is fine for a
    // muted footnote ("visited 12m ago"). We avoid pulling in chrono.
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
