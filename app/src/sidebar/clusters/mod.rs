//! Cluster-trees sidebar body. Tree picker + hierarchical node list with
//! expand/collapse, inline rename, drag-and-drop reparenting, and
//! note-row click → editor preview. Backed by `hiker_core::trees::Trees`.
//!
//! Scope: list/select trees, browse nodes, rename / drop / promote,
//! reorder via DnD, open notes, build (via cluster_review tab),
//! summarize subset (single + multi), regenerate names, multi-select
//! stage-moves + stage-tags into the staging queue, undo/redo, graph
//! view, advanced params. Save-as-triage isn't ported yet.

mod forms;
mod menus;
mod rows;
mod toolbar;
mod undo_redo;

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::Trees;

use crate::state::AppState;
use crate::theme;

pub fn show(ui: &mut egui::Ui, state: &mut AppState, _rt: &Arc<tokio::runtime::Runtime>) {
    let trees = state.vault_session.services.trees.clone();

    hydrate_if_needed(state, &trees);

    header(ui, state);
    ui.add_space(4.0);
    tree_picker(ui, state);
    toolbar::show(ui, state, &trees);
    ui.separator();

    // Summary-edit modal popover. Lives at panel scope so the row
    // renderer can keep its allocate_exact_size shape and not contend
    // with a variable-height inline editor.
    forms::summary_edit_inline(ui, state, &trees);
    forms::policy_editors_inline(ui, state, &trees);
    forms::stage_forms_inline(ui, state, &trees);

    if let Some(tree_id) = state.panels.clusters.selected_tree.clone() {
        render_selected_tree(ui, state, &trees, &tree_id);
    } else if state.panels.clusters.trees.is_empty() {
        empty_message(
            ui,
            "No trees yet. \"Suggest reorganization\" will build one (stub).",
        );
    } else {
        empty_message(ui, "Select a tree above.");
    }
}

fn advanced_params_popover(ui: &mut egui::Ui, state: &mut AppState) {
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Advanced clustering parameters").strong().small());
            let p = &mut state.panels.clusters.advanced_params;
            egui::Grid::new("cluster-params-grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    ui.label("Min cluster size");
                    ui.add(egui::DragValue::new(&mut p.min_cluster_size).range(2..=500));
                    ui.end_row();
                    ui.label("Min samples");
                    ui.add(egui::DragValue::new(&mut p.min_samples).range(1..=50));
                    ui.end_row();
                    ui.label("k nearest");
                    ui.add(egui::DragValue::new(&mut p.k_nearest).range(2..=100));
                    ui.end_row();
                    ui.label("Algorithm");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut p.use_leiden, false, "HDBSCAN");
                        ui.selectable_value(&mut p.use_leiden, true, "Leiden");
                    });
                    ui.end_row();
                    ui.label("Outlier threshold");
                    ui.add(egui::Slider::new(&mut p.outlier_threshold, 0.0..=1.0));
                    ui.end_row();
                    ui.label("Include outliers");
                    ui.checkbox(&mut p.include_outliers, "");
                    ui.end_row();
                });
            if ui.small_button("Close").clicked() {
                state.panels.clusters.showing_advanced_params = false;
            }
        });
    ui.separator();
}

/// LLM-rename every cluster node whose name hasn't been user-edited.
#[allow(dead_code)]
// TODO: re-expose once the cluster review panel adds a "regenerate names" button.
pub(crate) fn regenerate_names_pub(state: &mut AppState, trees: &Arc<Trees>, tree_id: &str) {
    regenerate_names(state, trees, tree_id);
}

fn regenerate_names(state: &mut AppState, trees: &Arc<Trees>, tree_id: &str) {
    if state.panels.clusters.llm_job_in_flight {
        state.push_toast(
            "A naming run is already in flight",
            crate::state::ToastLevel::Info,
        );
        return;
    }
    let Some(client) = build_llm_client(state) else {
        return;
    };
    let nodes = state.panels.clusters.nodes.clone();
    let targets: Vec<(String, String)> = nodes
        .iter()
        .filter(|n| {
            matches!(n.kind, hiker_core::trees::NodeKind::Cluster) && !n.user_edited_name
        })
        .map(|n| (n.id.clone(), n.summary.clone()))
        .collect();
    if targets.is_empty() {
        state.push_toast(
            "No clusters need regeneration",
            crate::state::ToastLevel::Info,
        );
        return;
    }

    let trees_for_task = trees.clone();
    let tree_id_owned = tree_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<crate::state::LlmJobOutcome>();
    tokio::spawn(async move {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for (node_id, summary) in &targets {
            let members = collect_member_titles(&nodes, node_id);
            let prompt = format!(
                "Provide a short name (<6 words) for this cluster.\nMembers:\n{}\n\nRespond with only the name.",
                members.join("\n")
            );
            let messages = vec![hiker_core::llm::Message::user(prompt)];
            let resp = match client.chat(&messages).await {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(node = %node_id, error = %err, "regenerate-names: llm call failed");
                    failed += 1;
                    continue;
                }
            };
            let new_name = resp.lines().next().unwrap_or("").trim().to_string();
            if new_name.is_empty() {
                failed += 1;
                continue;
            }
            match trees_for_task.auto_set_name_summary(&tree_id_owned, node_id, &new_name, summary) {
                Ok(_) => succeeded += 1,
                Err(err) => {
                    tracing::warn!(error = %err, "auto_set_name_summary failed");
                    failed += 1;
                }
            }
        }
        let _ = tx.send((succeeded, failed));
    });
    state.panels.clusters.llm_job_in_flight = true;
    state.panels.clusters.llm_job_rx = Some(rx);
    state.push_toast(
        "Regenerating cluster names...",
        crate::state::ToastLevel::Info,
    );
}

/// Construct an `Arc<dyn LlmClient>` from current settings, pushing an
/// error toast if the LLM is disabled or the client failed to build.
/// Returns `None` in either failure case; callers should early-return.
fn build_llm_client(state: &mut AppState) -> Option<Arc<dyn hiker_core::llm::LlmClient>> {
    use hiker_core::llm::GraniteLlmClient;
    let llm_cfg = state
        .vault_session
        .config
        .read()
        .map(|c| c.llm.clone())
        .unwrap_or_default();
    if !llm_cfg.enabled {
        state.push_toast(
            "LLM is disabled in settings",
            crate::state::ToastLevel::Warn,
        );
        return None;
    }
    match GraniteLlmClient::from_config(&llm_cfg) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn hiker_core::llm::LlmClient>),
        Err(err) => {
            state.push_toast(
                format!("LLM client error: {err}"),
                crate::state::ToastLevel::Error,
            );
            None
        }
    }
}

/// Poll the in-flight LLM naming task's result channel; on completion
/// clear the gate, surface a summary toast, and mark the tree dirty so
/// the new names land in the UI.
pub(crate) fn poll_llm_job(state: &mut AppState) {
    let Some(mut rx) = state.panels.clusters.llm_job_rx.take() else {
        return;
    };
    match rx.try_recv() {
        Ok((succeeded, failed)) => {
            state.panels.clusters.llm_job_in_flight = false;
            state.push_toast(
                format!("LLM naming: {succeeded} succeeded, {failed} failed"),
                crate::state::ToastLevel::Info,
            );
            mark_dirty(state);
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
            state.panels.clusters.llm_job_rx = Some(rx);
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            state.panels.clusters.llm_job_in_flight = false;
            state.push_toast(
                "LLM naming aborted",
                crate::state::ToastLevel::Warn,
            );
        }
    }
}

fn collect_member_titles(
    nodes: &[hiker_core::trees::EditableNode],
    cluster_id: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![cluster_id.to_string()];
    while let Some(id) = stack.pop() {
        for n in nodes.iter().filter(|n| n.parent.as_deref() == Some(id.as_str())) {
            if matches!(n.kind, hiker_core::trees::NodeKind::Leaf) {
                out.push(n.name.clone());
            } else {
                stack.push(n.id.clone());
            }
        }
        if out.len() > 50 {
            out.truncate(50);
            break;
        }
    }
    out
}

/// Semantic re-split of a subtree. Pulls leaf embeddings from the read
/// store, hands them to `Trees::split_cluster`, then optionally regenerates
/// names on the new sub-clusters via the LLM.
pub(crate) fn recluster_subtree(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    target_node_id: Option<&str>,
) {
    let store_mutex = state.vault_session.services.read_store.clone();
    let p = state.panels.clusters.advanced_params.clone();
    let algorithm = if p.use_leiden {
        hiker_core::cluster::ClusterAlgorithm::Leiden
    } else {
        hiker_core::cluster::ClusterAlgorithm::Hdbscan
    };
    let leiden = hiker_core::cluster::LeidenParams {
        k_nearest: p.k_nearest as u32,
        ..hiker_core::cluster::LeidenParams::default()
    };
    let params = hiker_core::cluster::ClusterParams {
        min_cluster_size: p.min_cluster_size as u32,
        min_samples: Some(p.min_samples as u32),
        algorithm,
        leiden,
        summarize: hiker_core::cluster::SummarizeMode::None,
        ..hiker_core::cluster::ClusterParams::default()
    };
    let resolver = |leaf_id: &str| -> Option<Vec<f32>> {
        let store = store_mutex.lock().ok()?;
        store.note_embedding_for_path(leaf_id).ok().flatten()
    };
    match trees.split_cluster(tree_id, target_node_id, &params, &resolver, None) {
        Ok(outcome) => {
            let n = outcome.new_clusters.len();
            state.push_toast(
                format!("Recluster produced {n} sub-clusters"),
                crate::state::ToastLevel::Info,
            );
            mark_dirty(state);
            // Drop any redo stack — a fresh forward op invalidates it.
            state.panels.clusters.redo_stacks.remove(tree_id);
        }
        Err(err) => state.push_toast(
            format!("Recluster failed: {err}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

/// LLM-summarize a focused subset of clusters. Pairs of (name, summary)
/// land via `Trees::auto_set_name_summary` so user-edited rows are
/// preserved.
pub(crate) fn summarize_subset(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node_ids: &[String],
) {
    if state.panels.clusters.llm_job_in_flight {
        state.push_toast(
            "A naming run is already in flight",
            crate::state::ToastLevel::Info,
        );
        return;
    }
    let Some(client) = build_llm_client(state) else {
        return;
    };
    let nodes = state.panels.clusters.nodes.clone();
    // Materialize the (id, name) targets up front so the spawned task
    // doesn't need to re-walk the nodes list per iteration.
    let targets: Vec<(String, String)> = node_ids
        .iter()
        .filter_map(|nid| {
            let n = nodes.iter().find(|x| &x.id == nid)?;
            if !matches!(n.kind, hiker_core::trees::NodeKind::Cluster) {
                return None;
            }
            Some((n.id.clone(), n.name.clone()))
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    let trees_for_task = trees.clone();
    let tree_id_owned = tree_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<crate::state::LlmJobOutcome>();
    tokio::spawn(async move {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for (nid, name) in &targets {
            let members = collect_member_titles(&nodes, nid);
            let prompt = format!(
                "Write a 1-sentence summary for this cluster.\nMembers:\n{}",
                members.join("\n")
            );
            let messages = vec![hiker_core::llm::Message::user(prompt)];
            let summary = match client.chat(&messages).await {
                Ok(s) => s.trim().to_string(),
                Err(err) => {
                    tracing::warn!(error = %err, "summarize-subset llm failed");
                    failed += 1;
                    continue;
                }
            };
            if summary.is_empty() {
                failed += 1;
                continue;
            }
            if trees_for_task
                .auto_set_name_summary(&tree_id_owned, nid, name, &summary)
                .is_ok()
            {
                succeeded += 1;
            } else {
                failed += 1;
            }
        }
        let _ = tx.send((succeeded, failed));
    });
    state.panels.clusters.llm_job_in_flight = true;
    state.panels.clusters.llm_job_rx = Some(rx);
    state.push_toast(
        "Summarizing clusters...",
        crate::state::ToastLevel::Info,
    );
}

fn header(ui: &mut egui::Ui, state: &mut AppState) {
    // Single entry point per `cluster-editor-new-tree-action` — the
    // review tab is the surface where the user picks algorithm (Cluster
    // partitioners or From folders), tunes params, runs the structural
    // pass, and confirms.
    let mut new_tree_clicked = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Cluster trees")
                .color(theme::muted())
                .strong(),
        );
    });
    if ui
        .add_sized(
            [ui.available_width(), 22.0],
            egui::Button::new("+ New tree"),
        )
        .on_hover_text("Open the cluster review tab to build a new tree")
        .clicked()
    {
        new_tree_clicked = true;
    }
    if new_tree_clicked {
        // status: cluster-editor-new-tree-action
        crate::panels::cluster_review::open(
            state,
            crate::panels::cluster_review::ReviewConfig::default(),
        );
    }
}

fn tree_picker(ui: &mut egui::Ui, state: &mut AppState) {
    let trees = &state.panels.clusters.trees;
    let selected_label = state
        .panels.clusters
        .selected_tree
        .as_ref()
        .and_then(|id| trees.iter().find(|t| &t.id == id))
        .map(|t| format!("{} ({})", t.name, t.state))
        .unwrap_or_else(|| "(select a tree)".to_string());

    egui::ComboBox::from_id_salt("cluster-tree-picker")
        .width(ui.available_width() - 8.0)
        .selected_text(selected_label)
        .show_ui(ui, |ui| {
            if trees.is_empty() {
                ui.label(
                    egui::RichText::new("(no trees)")
                        .color(theme::muted())
                        .italics(),
                );
            }
            for t in trees {
                let is_sel = state.panels.clusters.selected_tree.as_deref() == Some(t.id.as_str());
                let label = format!("{}  [{}]", t.name, t.state);
                if ui.selectable_label(is_sel, label).clicked() {
                    state.panels.clusters.selected_tree = Some(t.id.clone());
                    state.panels.clusters.dirty = true;
                    state.panels.clusters.renaming = None;
                }
            }
        });
}

fn render_selected_tree(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
) {
    if state.panels.clusters.nodes.is_empty() {
        empty_message(ui, "This tree has no nodes.");
        return;
    }
    rows::show_tree(ui, state, trees, tree_id);
}

fn hydrate_if_needed(state: &mut AppState, trees: &Arc<Trees>) {
    if !state.panels.clusters.loaded {
        match trees.list_trees() {
            Ok(rows) => {
                state.panels.clusters.trees = rows;
                state.panels.clusters.loaded = true;
                // Auto-select the first tree on first load so the picker
                // isn't an empty prompt.
                if state.panels.clusters.selected_tree.is_none() {
                    state.panels.clusters.selected_tree =
                        state.panels.clusters.trees.first().map(|t| t.id.clone());
                }
                state.panels.clusters.dirty = true;
            }
            Err(err) => {
                state.push_toast(
                    format!("Failed to list trees: {}", err),
                    crate::state::ToastLevel::Error,
                );
                state.panels.clusters.loaded = true;
            }
        }
    }
    if state.panels.clusters.dirty {
        if let Some(id) = state.panels.clusters.selected_tree.clone() {
            match trees.list_nodes(&id) {
                Ok(nodes) => state.panels.clusters.nodes = nodes,
                Err(err) => {
                    state.push_toast(
                        format!("Failed to load tree nodes: {}", err),
                        crate::state::ToastLevel::Error,
                    );
                    state.panels.clusters.nodes.clear();
                }
            }
        } else {
            state.panels.clusters.nodes.clear();
        }
        state.panels.clusters.dirty = false;
    }
}

fn empty_message(ui: &mut egui::Ui, msg: &str) {
    ui.add_space(20.0);
    ui.label(egui::RichText::new(msg).color(theme::muted()).italics());
}

/// Re-list trees + nodes from disk on the next frame.
pub(crate) fn mark_dirty(state: &mut AppState) {
    state.panels.clusters.dirty = true;
}
