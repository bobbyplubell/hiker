//! Cluster-tree action strip: undo/redo, selection-aware actions
//! (summarize subset, stage moves, stage tags, clear selection),
//! regenerate names, advanced-params toggle, graph view, discard tree.
//!
//! Extracted from `mod.rs` so the panel-level entry point stays a
//! shallow orchestration of (header, picker, toolbar, inline forms,
//! tree body). Per-button click handlers delegate back to free helpers
//! in `super` (`regenerate_names`, `summarize_subset`, `mark_dirty`,
//! `advanced_params_popover`) so this module stays focused on the
//! toolbar layout.

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::Trees;

use crate::state::AppState;
use crate::theme;
use hiker_core::trees::NodeKind;

use super::{advanced_params_popover, mark_dirty, regenerate_names, summarize_subset, undo_redo};

/// Heuristic for "this tree's clusters still carry placeholder names" per
/// `cluster-editor-pane-name-clusters-cta`. Recomputed live per repaint
/// from the already-hydrated `state.panels.clusters.nodes` (no extra cache; the
/// node list is small and a name/regex check is cheap).
///
/// Placeholder ≡ cluster node whose `name` matches `^Cluster \d+$`, whose
/// `summary` is empty, and which has not been user-edited. The tree is
/// considered to be in placeholder-name state if it has at least one
/// cluster and *every* cluster matches the placeholder shape.
fn tree_has_placeholder_names(nodes: &[hiker_core::trees::EditableNode]) -> bool {
    let mut saw_cluster = false;
    for n in nodes {
        if !matches!(n.kind, NodeKind::Cluster) {
            continue;
        }
        saw_cluster = true;
        let placeholder = !n.user_edited_name
            && n.summary.is_empty()
            && is_placeholder_name(&n.name);
        if !placeholder {
            return false;
        }
    }
    saw_cluster
}

fn is_placeholder_name(name: &str) -> bool {
    // Matches `^Cluster \d+$` without pulling in a regex dep.
    let Some(rest) = name.strip_prefix("Cluster ") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn show(ui: &mut egui::Ui, state: &mut AppState, trees: &Arc<Trees>) {
    let Some(tree_id) = state.panels.clusters.selected_tree.clone() else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(egui::Button::image_and_text(crate::icons::undo(), "Undo").small())
            .on_hover_text("Undo the last edit on this tree")
            .clicked()
        {
            match undo_redo::undo(trees, &tree_id) {
                Ok((op, entry)) => {
                    state.push_toast(format!("Undid '{op}'"), crate::state::ToastLevel::Info);
                    // Cap per-tree redo stack so cluster history can't
                    // grow without bound. Each entry holds JSON blobs
                    // (prior_subtree, absorbed_clusters); leaving them
                    // uncapped means a long cluster-editing session
                    // accumulates indefinitely. 32 entries is more than
                    // any user will redo through interactively.
                    const REDO_STACK_CAP: usize = 32;
                    let stack = state
                        .panels.clusters
                        .redo_stacks
                        .entry(tree_id.clone())
                        .or_default();
                    if stack.len() >= REDO_STACK_CAP {
                        // Drop the oldest (front) so the most recent
                        // undos stay redoable.
                        stack.remove(0);
                    }
                    stack.push(entry);
                    mark_dirty(state);
                }
                Err(undo_redo::UndoError::NothingToUndo) => {
                    state.push_toast("Nothing to undo", crate::state::ToastLevel::Info);
                }
                Err(err) => {
                    state.push_toast(
                        format!("Undo failed: {err}"),
                        crate::state::ToastLevel::Error,
                    );
                }
            }
        }
        let redo_has = state
            .panels.clusters
            .redo_stacks
            .get(&tree_id)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if ui
            .add_enabled(
                redo_has,
                egui::Button::image_and_text(crate::icons::redo(), "Redo").small(),
            )
            .on_hover_text("Re-apply the last undone edit")
            .clicked()
            && let Some(stack) = state.panels.clusters.redo_stacks.get_mut(&tree_id)
            && let Some(entry) = stack.pop()
        {
            match undo_redo::redo(trees, &tree_id, &entry) {
                Ok(op) => {
                    state.push_toast(
                        format!("Redid '{op}'"),
                        crate::state::ToastLevel::Info,
                    );
                    mark_dirty(state);
                }
                Err(err) => state.push_toast(
                    format!("Redo failed: {err}"),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        ui.separator();
        // Selection-aware actions.
        let selected_count = state.panels.clusters.selected_nodes.len();
        if selected_count > 0 {
            ui.label(
                egui::RichText::new(format!("{selected_count} selected"))
                    .small()
                    .color(theme::muted()),
            );
            let llm_busy = state.panels.clusters.llm_job_in_flight;
            if ui
                .add_enabled(!llm_busy, egui::Button::new("Summarize subset").small())
                .on_hover_text(if llm_busy { "LLM naming in flight" } else { "Summarize the selected clusters via the LLM" })
                .clicked()
            {
                let ids: Vec<String> =
                    state.panels.clusters.selected_nodes.iter().cloned().collect();
                summarize_subset(state, trees, &tree_id, &ids);
            }
            let resp_stage_move = ui.small_button("Stage moves…");
            if resp_stage_move.clicked() {
                state.panels.clusters.editing_stage_move_target = Some(String::new());
            }
            let resp_stage_tag = ui.small_button("Stage tags…");
            if resp_stage_tag.clicked() {
                state.panels.clusters.editing_stage_tag_slug = Some(String::new());
            }
            if ui.small_button("Clear selection").clicked() {
                state.panels.clusters.selected_nodes.clear();
            }
            ui.separator();
        }
        // status: cluster-editor-pane-name-clusters-cta
        // Contextual rename: when every cluster still carries a
        // placeholder name (`Cluster N`, no summary, not user-edited),
        // surface the verb as a primary CTA — "Name clusters with LLM".
        // Otherwise behave as the regular "Regenerate names" button.
        // Both labels invoke the same `regenerate_names` flow.
        let placeholder_state = tree_has_placeholder_names(&state.panels.clusters.nodes);
        let (label, hover) = if placeholder_state {
            (
                "Name clusters with LLM",
                "LLM-name every cluster in this tree (placeholder names \
                 detected). Same task-queue flow as regenerate.",
            )
        } else {
            (
                "Regenerate names",
                "LLM-rename every cluster not user-edited",
            )
        };
        let llm_busy = state.panels.clusters.llm_job_in_flight;
        let busy_hover = "LLM naming in flight";
        let effective_hover = if llm_busy { busy_hover } else { hover };
        let clicked = if placeholder_state {
            // Primary-CTA styling: accent fill + white text. No existing
            // primary-button convention in this codebase, so we synthesise
            // one inline from `theme::accent()`.
            let btn = egui::Button::new(
                egui::RichText::new(label)
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(theme::accent())
            .small();
            ui.add_enabled(!llm_busy, btn).on_hover_text(effective_hover).clicked()
        } else {
            ui.add_enabled(!llm_busy, egui::Button::new(label).small())
                .on_hover_text(effective_hover)
                .clicked()
        };
        if clicked {
            regenerate_names(state, trees, &tree_id);
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::settings(), "Params").small())
            .on_hover_text("Advanced clustering parameters")
            .clicked()
        {
            state.panels.clusters.showing_advanced_params = !state.panels.clusters.showing_advanced_params;
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::graph(), "Graph view").small())
            .on_hover_text("Open a radial graph of this cluster tree")
            .clicked()
        {
            use crate::tab::TabKind;
            // Singleton-per-tree: focus an existing graph tab if one's
            // open. Otherwise spawn a fresh ClusterGraph tab.
            let tid = tree_id.clone();
            let tid_for_build = tid.clone();
            state.find_or_open_tab(
                |k| matches!(k, TabKind::ClusterGraph { tree_id: x } if x == &tid),
                || TabKind::ClusterGraph { tree_id: tid_for_build },
            );
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::trash(), "Discard tree").small())
            .on_hover_text("Delete this tree from the registry")
            .clicked()
        {
            match trees.delete_tree(&tree_id) {
                Ok(()) => {
                    state.push_toast("Tree discarded", crate::state::ToastLevel::Info);
                    state.panels.clusters.selected_tree = None;
                    state.panels.clusters.nodes.clear();
                    state.panels.clusters.selected_nodes.clear();
                    state.panels.clusters.redo_stacks.remove(&tree_id);
                    state.panels.clusters.loaded = false;
                    state.panels.clusters.dirty = true;
                }
                Err(err) => state.push_toast(
                    format!("Discard failed: {err}"),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
    });
    if state.panels.clusters.showing_advanced_params {
        advanced_params_popover(ui, state);
    }
}
