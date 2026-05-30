//! Adapters that synthesize `EditableNode` rows from an un-persisted
//! `BuiltClusterTree` (post-Done) or from the live-reveal buffers
//! (mid-pass). The shared graph renderer (`panels::cluster_graph`)
//! consumes `EditableNode`; this module is the bridge between the
//! review tab's in-memory build state and the renderer's input shape.
//!
//! status: cluster-review-tab-result-graph-view

use std::collections::HashMap;

use hiker_core::cluster::{BuiltClusterNode, BuiltClusterTree};
use hiker_core::trees::types::{EditableNode, NodeKind};

/// Adapter (post-Done): synthesize an `EditableNode` row for every
/// cluster in `tree.levels` + every leaf member. Mirrors the shape
/// `result_to_node_inserts_pub` writes the tree's `.md`, but as
/// `EditableNode` (the graph renderer's input shape) instead of
/// `NodeInsert`.
///
/// Honors inline-renamed cluster names from the review pane's
/// `user_renamed` map so the graph view's labels track the tree view.
/// No policy color (none exists pre-persistence) and `churn = 0` (no
/// staleness tint), per the spec.
/// Zero-sized namespace for the `EditableNode` adapters. A struct (rather
/// than free fns) so the single-call entry points stay inherent methods.
pub(super) struct Adapter;

impl Adapter {
    pub(super) fn built_tree_to_editable_nodes(
    &self,
    tree: &BuiltClusterTree,
    user_renamed: &HashMap<String, String>,
) -> Vec<EditableNode> {
    let mut out: Vec<EditableNode> = Vec::new();
    if tree.levels.is_empty() {
        return out;
    }

    // Parent lookup: cluster levels 1.. carry child-cluster ids in
    // `members`; level 0 carries note ids. Match the persistence
    // builder's logic.
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for level in tree.levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }

    let top_level = tree.levels.len() - 1;
    let top = &tree.levels[top_level];
    let synthesized_root = top.len() != 1;
    let root_id = if synthesized_root {
        Some("root".to_string())
    } else {
        None
    };
    if synthesized_root {
        for n in top {
            parent_of.insert(n.id.clone(), "root".to_string());
        }
        out.push(EditableNode {
            id: "root".to_string(),
            parent: None,
            kind: NodeKind::Cluster,
            note_path: None,
            name: "Vault root".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        });
    }

    for (level_idx, level) in tree.levels.iter().enumerate() {
        for node in level {
            let parent = if level_idx == top_level && !synthesized_root {
                None
            } else {
                parent_of.get(&node.id).cloned()
            };
            let renamed = user_renamed.get(&node.id).cloned();
            let user_edited = renamed.is_some();
            out.push(EditableNode {
                id: node.id.clone(),
                parent,
                kind: NodeKind::Cluster,
                note_path: None,
                name: renamed.unwrap_or_else(|| node.name.clone()),
                summary: node.summary.clone(),
                user_edited_name: user_edited,
                user_edited_summary: false,
                policy: None,
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            });
        }
    }

    // Leaves under level-0 clusters.
    if let Some(leaf_level) = tree.levels.first() {
        for cluster in leaf_level {
            for note_id in &cluster.members {
                let leaf_id = format!("leaf-{}", note_id);
                out.push(EditableNode {
                    id: leaf_id,
                    parent: Some(cluster.id.clone()),
                    kind: NodeKind::Leaf,
                    note_path: Some(note_id.clone()),
                    name: note_id.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: cluster.confidence,
                    summary_membership_churn: 0,
                });
            }
        }
    }

    // Outliers under the (real or synthesized) root.
    if !tree.outliers.is_empty()
        && let Some(rid) = root_id.as_deref().or_else(|| top.first().map(|n| n.id.as_str()))
    {
        out.push(EditableNode {
            id: "outliers".to_string(),
            parent: Some(rid.to_string()),
            kind: NodeKind::OutlierBucket,
            note_path: None,
            name: "Outliers".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 0.0,
            summary_membership_churn: 0,
        });
        for note_id in &tree.outliers {
            out.push(EditableNode {
                id: format!("leaf-outlier-{}", note_id),
                parent: Some("outliers".to_string()),
                kind: NodeKind::Leaf,
                note_path: Some(note_id.clone()),
                name: note_id.clone(),
                summary: String::new(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: None,
                confidence: 0.0,
                summary_membership_churn: 0,
            });
        }
    }

    out
    }

    /// Adapter (mid-build live reveal): synthesize an `EditableNode` slice
    /// from the pane's incremental `live_top` + `live_pending_children`
    /// buffers. Same encoding rules as the post-Done adapter; gracefully
    /// degrades when only partial clusters are present so the graph
    /// re-renders as new clusters arrive.
    pub(super) fn live_to_editable_nodes(
    &self,
    live_top: &[BuiltClusterNode],
    live_children: &HashMap<String, Vec<BuiltClusterNode>>,
    user_renamed: &HashMap<String, String>,
) -> Vec<EditableNode> {
    let mut out: Vec<EditableNode> = Vec::new();
    if live_top.is_empty() {
        return out;
    }

    fn walk(
        node: &BuiltClusterNode,
        parent: Option<&str>,
        live_children: &HashMap<String, Vec<BuiltClusterNode>>,
        user_renamed: &HashMap<String, String>,
        out: &mut Vec<EditableNode>,
    ) {
        let renamed = user_renamed.get(&node.id).cloned();
        let user_edited = renamed.is_some();
        out.push(EditableNode {
            id: node.id.clone(),
            parent: parent.map(std::string::ToString::to_string),
            kind: NodeKind::Cluster,
            note_path: None,
            name: renamed.unwrap_or_else(|| node.name.clone()),
            summary: node.summary.clone(),
            user_edited_name: user_edited,
            user_edited_summary: false,
            policy: None,
            centroid: Some(node.centroid.clone()),
            confidence: node.confidence,
            summary_membership_churn: 0,
        });
        if let Some(children) = live_children.get(&node.id) {
            for child in children {
                walk(child, Some(&node.id), live_children, user_renamed, out);
            }
        } else {
            for note_id in &node.members {
                out.push(EditableNode {
                    id: format!("leaf-{}", note_id),
                    parent: Some(node.id.clone()),
                    kind: NodeKind::Leaf,
                    note_path: Some(note_id.clone()),
                    name: note_id.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: node.confidence,
                    summary_membership_churn: 0,
                });
            }
        }
    }
    for n in live_top {
        walk(n, None, live_children, user_renamed, &mut out);
    }
    out
    }
}
