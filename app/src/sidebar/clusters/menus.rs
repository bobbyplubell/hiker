//! Right-click context menu for cluster-tree rows. Surfaces rename,
//! edit summary, policy submenu, split/recluster, summarize, merge
//! children up, drop cluster, promote-out-of-outliers, send-to-outliers,
//! and the Move-to… picker. Multi-select stage-moves / stage-tags live
//! in the toolbar above; here we operate on a single node.

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::{EditableNode, NodeKind, NodePolicy, Trees};

use crate::state::{AppState, ToastLevel};

pub(super) fn node_context_menu(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let is_cluster = node.kind == NodeKind::Cluster;
    let is_leaf = node.kind == NodeKind::Leaf;

    if !is_leaf && ui.button("Rename").clicked() {
        state.panels.clusters.renaming = Some((node.id.clone(), node.name.clone()));
        ui.close();
    }
    if !is_leaf && ui.button("Edit summary…").clicked() {
        state.panels.clusters.editing_summary = Some((node.id.clone(), node.summary.clone()));
        ui.close();
    }
    if is_cluster {
        ui.menu_button("Policy", |ui| {
            policy_submenu(ui, state, trees, tree_id, node);
        });
        ui.separator();
        if ui
            .button("Split / Recluster")
            .on_hover_text("Re-run clustering on this subtree using stored note embeddings")
            .clicked()
        {
            super::recluster_subtree(state, trees, tree_id, Some(&node.id));
            ui.close();
        }
        if ui
            .button("Summarize (LLM)")
            .on_hover_text("Generate a new summary for this cluster via the LLM")
            .clicked()
        {
            let ids = vec![node.id.clone()];
            super::summarize_subset(state, trees, tree_id, &ids);
            ui.close();
        }
        if ui.button("Merge children up").on_hover_text(
            "Flatten one level: each child cluster's children move up to this node.",
        ).clicked() {
            merge_children_up(state, trees, tree_id, node);
            ui.close();
        }
        if ui.button("Drop cluster").clicked() {
            drop_cluster(state, trees, tree_id, node);
            ui.close();
        }
    }
    if is_leaf {
        let parent_is_outlier_bucket = node
            .parent
            .as_ref()
            .and_then(|pid| state.panels.clusters.nodes.iter().find(|n| &n.id == pid))
            .is_some_and(|p| p.kind == NodeKind::OutlierBucket);
        if parent_is_outlier_bucket && ui.button("Promote out of outliers…").clicked() {
            // v0: route through the Move to… picker.
            show_move_targets(ui, state, trees, tree_id, node);
            ui.close();
        }
        if !parent_is_outlier_bucket && ui.button("Send to outliers").clicked() {
            promote_to_outlier_bucket(state, trees, tree_id, node);
            ui.close();
        }
    }
    ui.menu_button("Move to…", |ui| {
        show_move_targets(ui, state, trees, tree_id, node);
    });
    if !is_leaf {
        ui.menu_button("Merge with sibling…", |ui| {
            show_merge_siblings(ui, state, trees, tree_id, node);
        });
    }
    if ui.button("Collapse all").clicked() {
        state.panels.clusters.expanded.clear();
        ui.close();
    }
}

fn policy_submenu(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let current = node.policy.clone();
    let is_freeze = matches!(current, Some(NodePolicy::Freeze));
    let is_tag = matches!(current, Some(NodePolicy::Tag { .. }));
    let is_move = matches!(current, Some(NodePolicy::Move { .. }));
    if ui
        .selectable_label(current.is_none(), "Default (no policy)")
        .on_hover_text("Clear any policy from this cluster")
        .clicked()
    {
        apply_policy(state, trees, tree_id, &node.id, None);
        ui.close();
    }
    if ui
        .selectable_label(is_freeze, "Freeze")
        .on_hover_text("Reclustering won't touch this subtree")
        .clicked()
    {
        apply_policy(state, trees, tree_id, &node.id, Some(NodePolicy::Freeze));
        ui.close();
    }
    let (existing_tag_slug, existing_tag_req) = match &node.policy {
        Some(NodePolicy::Tag { slug, require_review }) => (slug.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui
        .selectable_label(is_tag, "Set Tag policy…")
        .clicked()
    {
        state.panels.clusters.editing_tag_policy =
            Some((node.id.clone(), existing_tag_slug, existing_tag_req));
        ui.close();
    }
    let (existing_move_folder, existing_move_req) = match &node.policy {
        Some(NodePolicy::Move { folder, require_review }) => (folder.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui
        .selectable_label(is_move, "Set Move policy…")
        .clicked()
    {
        state.panels.clusters.editing_move_policy =
            Some((node.id.clone(), existing_move_folder, existing_move_req));
        ui.close();
    }
}

fn apply_policy(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node_id: &str,
    policy: Option<NodePolicy>,
) {
    match trees.set_policy(tree_id, node_id, policy) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Set policy failed: {}", err), ToastLevel::Error),
    }
}

fn merge_children_up(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    match trees.merge_children_up(tree_id, &node.id) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Merge failed: {}", err), ToastLevel::Error),
    }
}

fn show_merge_siblings(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let parent = node.parent.clone();
    let siblings: Vec<EditableNode> = state
        .panels.clusters
        .nodes
        .iter()
        .filter(|n| {
            n.id != node.id
                && n.parent == parent
                && matches!(n.kind, NodeKind::Cluster)
        })
        .cloned()
        .collect();
    if siblings.is_empty() {
        ui.label("(no sibling clusters)");
        return;
    }
    for sib in siblings {
        let label = format!("* {}", sib.name);
        if ui.button(label).clicked() {
            let ids = vec![node.id.clone(), sib.id.clone()];
            match trees.merge_siblings(tree_id, &ids) {
                Ok(_) => super::mark_dirty(state),
                Err(err) => state.push_toast(
                    format!("Merge siblings failed: {}", err),
                    ToastLevel::Error,
                ),
            }
            ui.close();
        }
    }
}

fn drop_cluster(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let Some(bucket) = state
        .panels.clusters
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::OutlierBucket)
        .cloned()
    else {
        state.push_toast("This tree has no outlier bucket", ToastLevel::Warn);
        return;
    };
    match trees.drop_cluster(tree_id, &node.id, &bucket.id) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Drop failed: {}", err), ToastLevel::Error),
    }
}

fn promote_to_outlier_bucket(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let Some(bucket) = state
        .panels.clusters
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::OutlierBucket)
        .cloned()
    else {
        state.push_toast("This tree has no outlier bucket", ToastLevel::Warn);
        return;
    };
    match trees.promote_outlier(tree_id, &node.id, Some(&bucket.id)) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Move failed: {}", err), ToastLevel::Error),
    }
}

fn show_move_targets(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
) {
    let descendants = collect_descendants(&state.panels.clusters.nodes, &node.id);
    let candidates: Vec<EditableNode> = state
        .panels.clusters
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Cluster | NodeKind::OutlierBucket)
                && n.id != node.id
                && !descendants.contains(&n.id)
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        ui.label("(no valid targets)");
        return;
    }
    for c in candidates {
        let glyph = if c.kind == NodeKind::OutlierBucket { "?" } else { "*" };
        let label = format!("{} {}", glyph, c.name);
        if ui.button(label).clicked() {
            let result = if node.kind == NodeKind::Leaf {
                trees.promote_outlier(tree_id, &node.id, Some(&c.id))
            } else {
                trees.move_node(tree_id, &node.id, Some(&c.id))
            };
            match result {
                Ok(()) => super::mark_dirty(state),
                Err(err) => state.push_toast(
                    format!("Move failed: {}", err),
                    ToastLevel::Error,
                ),
            }
            ui.close();
        }
    }
}

fn collect_descendants(
    nodes: &[EditableNode],
    root: &str,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(id) = stack.pop() {
        for n in nodes.iter().filter(|n| n.parent.as_deref() == Some(id.as_str())) {
            if out.insert(n.id.clone()) {
                stack.push(n.id.clone());
            }
        }
    }
    out
}
