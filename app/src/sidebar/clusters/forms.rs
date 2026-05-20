//! Inline modal-style editors that hang at the top of the cluster
//! sidebar body: edit-summary, tag-policy + move-policy, stage-moves +
//! stage-tags target prompts. Each renders only when its corresponding
//! `state.panels.clusters.editing_*` slot is `Some(...)` — clicking the
//! triggering row or toolbar button populates the slot, and the form
//! clears it on Save/Cancel.
//!
//! Extracted from `mod.rs` so the panel-level entry point stays a thin
//! orchestration layer. Helpers that perform the actual staging
//! (`do_stage_moves`, `do_stage_tags`) live here too since they're only
//! called from `stage_forms_inline`'s Stage buttons.

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::Trees;

use crate::state::AppState;
use crate::theme;

use super::mark_dirty;

pub(super) fn summary_edit_inline(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
) {
    let Some((node_id, mut draft)) = state.panels.clusters.editing_summary.clone() else {
        return;
    };
    let Some(tree_id) = state.panels.clusters.selected_tree.clone() else {
        return;
    };
    let target_name = state
        .panels.clusters
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| node_id.clone());
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Edit summary · {target_name}")).small().strong(),
            );
            let resp = ui.add(
                egui::TextEdit::multiline(&mut draft)
                    .desired_rows(3)
                    .desired_width(ui.available_width()),
            );
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    match trees.set_summary(&tree_id, &node_id, draft.trim()) {
                        Ok(()) => mark_dirty(state),
                        Err(err) => state.push_toast(
                            format!("Set summary failed: {err}"),
                            crate::state::ToastLevel::Error,
                        ),
                    }
                    state.panels.clusters.editing_summary = None;
                    return;
                }
                if ui.button("Cancel").clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)))
                {
                    state.panels.clusters.editing_summary = None;
                    return;
                }
                state.panels.clusters.editing_summary = Some((node_id.clone(), draft.clone()));
            });
        });
    ui.separator();
}

pub(super) fn policy_editors_inline(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
) {
    let Some(tree_id) = state.panels.clusters.selected_tree.clone() else {
        return;
    };
    if let Some((node_id, mut slug, mut require_review)) =
        state.panels.clusters.editing_tag_policy.clone()
    {
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Tag policy").strong().small());
                ui.horizontal(|ui| {
                    ui.label("Slug");
                    ui.add(
                        egui::TextEdit::singleline(&mut slug)
                            .hint_text("e.g. project-x")
                            .desired_width(160.0),
                    );
                });
                ui.checkbox(&mut require_review, "Require review before apply");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() && !slug.trim().is_empty() {
                        let policy = hiker_core::trees::NodePolicy::Tag {
                            slug: slug.trim().to_string(),
                            require_review,
                        };
                        if let Err(err) = trees.set_policy(&tree_id, &node_id, Some(policy)) {
                            state.push_toast(
                                format!("Set tag policy failed: {err}"),
                                crate::state::ToastLevel::Error,
                            );
                        } else {
                            mark_dirty(state);
                        }
                        state.panels.clusters.editing_tag_policy = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.panels.clusters.editing_tag_policy = None;
                        return;
                    }
                    state.panels.clusters.editing_tag_policy =
                        Some((node_id.clone(), slug.clone(), require_review));
                });
            });
        ui.separator();
    }
    if let Some((node_id, mut folder, mut require_review)) =
        state.panels.clusters.editing_move_policy.clone()
    {
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Move policy").strong().small());
                ui.horizontal(|ui| {
                    ui.label("Folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut folder)
                            .hint_text("e.g. archive/old")
                            .desired_width(200.0),
                    );
                });
                ui.checkbox(&mut require_review, "Require review before apply");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() && !folder.trim().is_empty() {
                        let policy = hiker_core::trees::NodePolicy::Move {
                            folder: folder.trim().to_string(),
                            require_review,
                        };
                        if let Err(err) = trees.set_policy(&tree_id, &node_id, Some(policy)) {
                            state.push_toast(
                                format!("Set move policy failed: {err}"),
                                crate::state::ToastLevel::Error,
                            );
                        } else {
                            mark_dirty(state);
                        }
                        state.panels.clusters.editing_move_policy = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.panels.clusters.editing_move_policy = None;
                        return;
                    }
                    state.panels.clusters.editing_move_policy =
                        Some((node_id.clone(), folder.clone(), require_review));
                });
            });
        ui.separator();
    }
}

pub(super) fn stage_forms_inline(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
) {
    let Some(tree_id) = state.panels.clusters.selected_tree.clone() else {
        return;
    };
    if let Some(mut target) = state.panels.clusters.editing_stage_move_target.clone() {
        let selected: Vec<String> = state.panels.clusters.selected_nodes.iter().cloned().collect();
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Stage moves · {} selected",
                        selected.len()
                    ))
                    .strong()
                    .small(),
                );
                ui.horizontal(|ui| {
                    ui.label("Target folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut target)
                            .hint_text("e.g. archive/projects")
                            .desired_width(220.0),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Stage").clicked() && !target.trim().is_empty() {
                        do_stage_moves(state, trees, &tree_id, &selected, target.trim());
                        state.panels.clusters.editing_stage_move_target = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.panels.clusters.editing_stage_move_target = None;
                        return;
                    }
                    state.panels.clusters.editing_stage_move_target = Some(target.clone());
                });
            });
        ui.separator();
    }
    if let Some(mut slug) = state.panels.clusters.editing_stage_tag_slug.clone() {
        let selected: Vec<String> = state.panels.clusters.selected_nodes.iter().cloned().collect();
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("Stage tags · {} selected", selected.len()))
                        .strong()
                        .small(),
                );
                ui.horizontal(|ui| {
                    ui.label("Tag slug");
                    ui.add(
                        egui::TextEdit::singleline(&mut slug)
                            .hint_text("e.g. project-x")
                            .desired_width(220.0),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Stage").clicked() && !slug.trim().is_empty() {
                        do_stage_tags(state, trees, &tree_id, &selected, slug.trim());
                        state.panels.clusters.editing_stage_tag_slug = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.panels.clusters.editing_stage_tag_slug = None;
                        return;
                    }
                    state.panels.clusters.editing_stage_tag_slug = Some(slug.clone());
                });
            });
        ui.separator();
    }
}

fn do_stage_moves(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node_ids: &[String],
    target_folder: &str,
) {
    let store_mutex = state.vault_session.services.read_store.clone();
    let staging = state.vault_session.services.staging.clone();
    let store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let args = hiker_core::suggest::StageMoveArgs {
        tree_id,
        node_ids,
        target_folder,
    };
    match hiker_core::suggest::stage_moves(trees, args, &store, &staging) {
        Ok(ids) => state.push_toast(
            format!("Staged {} moves", ids.len()),
            crate::state::ToastLevel::Info,
        ),
        Err(err) => state.push_toast(
            format!("Stage moves failed: {err}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

fn do_stage_tags(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node_ids: &[String],
    tag_slug: &str,
) {
    let store_mutex = state.vault_session.services.read_store.clone();
    let staging = state.vault_session.services.staging.clone();
    let store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let args = hiker_core::suggest::StageTagArgs {
        tree_id,
        node_ids,
        tag_slug,
    };
    match hiker_core::suggest::stage_tags(trees, args, &state.vault_session.vault, &store, &staging) {
        Ok(ids) => state.push_toast(
            format!("Staged {} tags", ids.len()),
            crate::state::ToastLevel::Info,
        ),
        Err(err) => state.push_toast(
            format!("Stage tags failed: {err}"),
            crate::state::ToastLevel::Error,
        ),
    }
}
