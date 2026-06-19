//! Adapter that converts the clustering algorithm's neutral build output
//! (`cluster::BuiltClusterTree`) into the tree-storage representation
//! (`Db` rows: `TreeInsert` + `NodeInsert`/`NodeKind`).
//!
//! This is the one-way seam between the two modules: `core::trees`
//! depends on `core::cluster` (the storage/adapter layer consumes the
//! algorithm), never the reverse. The clustering build pipeline returns
//! only `BuiltClusterTree` and stays free of any `trees::types` shape;
//! the `Db`/`NodeKind` assembly lives here, on the storage side.
//!
//! status: cluster-tree-output
//! status: cluster-review-tab-confirm-single-path
//! status: cluster-build-rebuild

use std::collections::{HashMap, HashSet};

use crate::cluster::algo::mean_normalize;
use crate::cluster::{
    self, BuildError, BuildMethod, BuildScope, BuiltClusterTree, NoteInput, Summarizer,
};

use super::types::{Db, EditableNode, NodeInsert, NodeKind, TreeInsert};

/// Build a fresh tree and persist it as a tree `.md`. The resulting
/// `cluster_trees` row + `cluster_nodes` rows are written under one
/// transaction (per `cluster-editor-draft-persistence` — every node is
/// editable from the moment it lands). Returns the new `tree_id`.
pub fn persist(
    trees: &Db,
    name: &str,
    source: &str,
    scope: &BuildScope,
    method: &BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<String, BuildError> {
    let result = cluster::build::tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    let inserts = node_inserts(&result.tree);
    // Single atomic write: tree + nodes together. The old insert_tree (empty)
    // → insert_nodes two-step landed nodes as a layered-doc diff of the empty→full
    // frontmatter, which could corrupt the `nodes:` block. status:
    // cluster-review-tab-confirm-single-path
    let tree_id = trees
        .insert_tree_with_nodes(
            TreeInsert {
                id: None,
                name: name.to_string(),
                source: source.to_string(),
                state: "draft".to_string(),
                scope_json,
                method_json,
                vault_snapshot: None,
            },
            &inserts,
        )
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;
    Ok(tree_id)
}

/// Re-build an existing tree against the current vault state. Re-uses
/// the tree's saved `scope` + `method` (from `cluster_trees.scope` /
/// `.method`), re-runs the build, and persists a *new* tree row —
/// the original tree is left intact so the user can compare / discard.
/// User-edited fields (`user_edited_name`, `user_edited_summary`,
/// `policy`) on the old tree are preserved onto new clusters whose
/// member-set Jaccard against the old cluster exceeds `merge_threshold`
/// (0.5 by default — matches the spec's "preserve where membership
/// overlaps significantly" wording in the rollout doc).
///
/// Returns the new tree id.
///
/// Per `cluster-build-rebuild`.
///
/// status: cluster-build-rebuild
pub fn rebuild_and_persist(
    trees: &Db,
    old_tree_id: &str,
    new_name: &str,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
    merge_threshold: f32,
) -> Result<String, BuildError> {
    let old_row = trees
        .get_tree(old_tree_id)
        .map_err(|e| BuildError::Summarizer(format!("get_tree: {e}")))?
        .ok_or_else(|| BuildError::Summarizer(format!("tree not found: {old_tree_id}")))?;
    let scope: BuildScope = serde_json::from_str(&old_row.scope_json)
        .map_err(|e| BuildError::Summarizer(format!("scope deserialize: {e}")))?;
    let method: BuildMethod = serde_json::from_str(&old_row.method_json)
        .map_err(|e| BuildError::Summarizer(format!("method deserialize: {e}")))?;
    let old_nodes = trees
        .list_nodes(old_tree_id)
        .map_err(|e| BuildError::Summarizer(format!("list_nodes: {e}")))?;

    let result = cluster::build::tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    // Tree row is created together with its nodes in one atomic write at the
    // end (after merge-preservation rewrites the inserts) so there is never an
    // empty-nodes intermediate, and the nodes never land as a layered-doc diff of
    // the empty→full frontmatter. status: cluster-review-tab-confirm-single-path
    let new_tree = TreeInsert {
        id: None,
        name: new_name.to_string(),
        source: old_row.source.clone(),
        state: "draft".to_string(),
        scope_json,
        method_json,
        vault_snapshot: None,
    };

    let mut inserts = node_inserts(&result.tree);

    // Compute per-old-cluster note-id member sets. Walk old_nodes once
    // to build child→parent map + leaf note ids per cluster.
    let mut old_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut old_node_by_id: HashMap<String, &EditableNode> = HashMap::new();
    for n in &old_nodes {
        old_node_by_id.insert(n.id.clone(), n);
        if let Some(p) = &n.parent {
            old_children.entry(p.clone()).or_default().push(n.id.clone());
        }
    }
    fn collect_old_notes(
        id: &str,
        old_children: &HashMap<String, Vec<String>>,
        old_node_by_id: &HashMap<String, &EditableNode>,
        acc: &mut HashSet<String>,
    ) {
        if let Some(kids) = old_children.get(id) {
            for k in kids {
                if let Some(node) = old_node_by_id.get(k) {
                    if matches!(node.kind, NodeKind::Leaf) {
                        if let Some(nid) = &node.note_path {
                            acc.insert(nid.clone());
                        }
                    } else {
                        collect_old_notes(k, old_children, old_node_by_id, acc);
                    }
                }
            }
        }
    }
    let mut old_cluster_members: HashMap<String, HashSet<String>> = HashMap::new();
    for n in &old_nodes {
        if matches!(n.kind, NodeKind::Cluster) {
            let mut s = HashSet::new();
            collect_old_notes(&n.id, &old_children, &old_node_by_id, &mut s);
            old_cluster_members.insert(n.id.clone(), s);
        }
    }

    // Build new clusters' note-id member sets from `inserts`.
    let mut new_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut new_node_kind: HashMap<String, NodeKind> = HashMap::new();
    let mut new_note_ref: HashMap<String, Option<String>> = HashMap::new();
    for n in &inserts {
        if let Some(p) = &n.parent_id {
            new_children.entry(p.clone()).or_default().push(n.node_id.clone());
        }
        new_node_kind.insert(n.node_id.clone(), n.kind);
        new_note_ref.insert(n.node_id.clone(), n.note_id.clone());
    }
    fn collect_new_notes(
        id: &str,
        new_children: &HashMap<String, Vec<String>>,
        new_node_kind: &HashMap<String, NodeKind>,
        new_note_ref: &HashMap<String, Option<String>>,
        acc: &mut HashSet<String>,
    ) {
        if let Some(kids) = new_children.get(id) {
            for k in kids {
                if let Some(kind) = new_node_kind.get(k) {
                    if matches!(kind, NodeKind::Leaf) {
                        if let Some(Some(nid)) = new_note_ref.get(k) {
                            acc.insert(nid.clone());
                        }
                    } else {
                        collect_new_notes(k, new_children, new_node_kind, new_note_ref, acc);
                    }
                }
            }
        }
    }

    // For each new cluster, find the old cluster with the highest
    // Jaccard. If above the threshold, transfer user-edited name /
    // summary + policy.
    for ins in inserts.iter_mut() {
        if !matches!(ins.kind, NodeKind::Cluster) {
            continue;
        }
        let mut new_members: HashSet<String> = HashSet::new();
        collect_new_notes(
            &ins.node_id,
            &new_children,
            &new_node_kind,
            &new_note_ref,
            &mut new_members,
        );
        if new_members.is_empty() {
            continue;
        }
        let mut best_id: Option<&String> = None;
        let mut best_jaccard: f32 = 0.0;
        for (old_id, old_members) in &old_cluster_members {
            if old_members.is_empty() {
                continue;
            }
            let inter = new_members.intersection(old_members).count() as f32;
            let union = new_members.union(old_members).count() as f32;
            if union <= 0.0 {
                continue;
            }
            let j = inter / union;
            if j > best_jaccard {
                best_jaccard = j;
                best_id = Some(old_id);
            }
        }
        if best_jaccard >= merge_threshold
            && let Some(old_id) = best_id
            && let Some(old_node) = old_node_by_id.get(old_id)
        {
            if old_node.user_edited_name {
                ins.name = old_node.name.clone();
                ins.user_edited_name = true;
            }
            if old_node.user_edited_summary {
                ins.summary = old_node.summary.clone();
                ins.user_edited_summary = true;
            }
            if old_node.policy.is_some() {
                ins.policy = old_node.policy.clone();
            }
        }
    }

    let new_tree_id = trees
        .insert_tree_with_nodes(new_tree, &inserts)
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;
    Ok(new_tree_id)
}

/// Flatten a `BuiltClusterTree` into the `NodeInsert` rows `core::trees`
/// persists. Top of the tree is the highest-level cluster (root); levels
/// descend with cluster-kind rows; the leaf level produces `leaf`-kind
/// rows under their parent clusters. Outliers attach as `leaf`-kind rows
/// under a dedicated `outlier-bucket` node parented at the root.
///
/// This is the structural conversion from the algorithm's neutral output
/// to the tree-storage shape. It is the only place `NodeKind` is derived
/// from cluster-build structure (cluster levels → `Cluster`, level-0
/// members → `Leaf`, the outlier bucket → `OutlierBucket`).
///
/// status: cluster-tree-output
/// status: cluster-review-tab-confirm-single-path
pub fn node_inserts(tree: &BuiltClusterTree) -> Vec<NodeInsert> {
    let mut out: Vec<NodeInsert> = Vec::new();
    if tree.levels.is_empty() {
        return out;
    }
    // Determine the root. If the top level has exactly one node, that's
    // root. Otherwise synthesize a root that owns the top-level nodes.
    let top_level = tree.levels.len() - 1;
    let top = &tree.levels[top_level];
    let (root_id, synthesized_root) = if top.len() == 1 {
        (top[0].id.clone(), false)
    } else {
        ("root".to_string(), true)
    };

    // Build a parent lookup: for each child id, who's its parent?
    // The build process records `members` on each `BuiltClusterNode`:
    // - cluster levels (1..N): members are child cluster ids
    // - level 0: members are note ids
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for level in tree.levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    if synthesized_root {
        for n in top {
            parent_of.insert(n.id.clone(), root_id.clone());
        }
    }

    // Write the synthesized root, if any.
    if synthesized_root {
        // Centroid for the synthesized root = mean of top-level
        // centroids, L2-normalized.
        let top_centroids: Vec<&[f32]> = top.iter().map(|n| n.centroid.as_slice()).collect();
        let centroid = mean_normalize(&top_centroids);
        out.push(NodeInsert {
            node_id: root_id.clone(),
            parent_id: None,
            kind: NodeKind::Cluster,
            note_id: None,
            name: "Vault root".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: Some(centroid),
            confidence: 1.0,
            summary_membership_churn: 0,
        });
    }

    // Emit cluster nodes for every level.
    for (level_idx, level) in tree.levels.iter().enumerate() {
        for node in level {
            let parent = if level_idx == top_level && !synthesized_root {
                None
            } else {
                parent_of.get(&node.id).cloned()
            };
            out.push(NodeInsert {
                node_id: node.id.clone(),
                parent_id: parent,
                kind: NodeKind::Cluster,
                note_id: None,
                name: node.name.clone(),
                summary: node.summary.clone(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            });
        }
    }

    // Emit leaf nodes under their level-0 cluster.
    if let Some(leaf_level) = tree.levels.first() {
        for cluster in leaf_level {
            for note_id in &cluster.members {
                let leaf_id = format!("leaf-{}", note_id);
                out.push(NodeInsert {
                    node_id: leaf_id,
                    parent_id: Some(cluster.id.clone()),
                    kind: NodeKind::Leaf,
                    note_id: Some(note_id.clone()),
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

    // Outliers bucket, parented at root.
    if !tree.outliers.is_empty() {
        let bucket_id = "outliers".to_string();
        out.push(NodeInsert {
            node_id: bucket_id.clone(),
            parent_id: Some(root_id.clone()),
            kind: NodeKind::OutlierBucket,
            note_id: None,
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
            out.push(NodeInsert {
                node_id: format!("leaf-{}", note_id),
                parent_id: Some(bucket_id.clone()),
                kind: NodeKind::Leaf,
                note_id: Some(note_id.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{
        Params, SummarizeInput, SummaryOutput,
    };
    use std::sync::Arc;

    /// Deterministic stand-in for the production summarizer so the
    /// adapter test asserts on persisted row shape without an LLM.
    struct MockSummarizer;

    impl Summarizer for MockSummarizer {
        fn summarize(&self, input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
            let name = input
                .members
                .first()
                .map(|m| format!("cluster: {}", m.title))
                .unwrap_or_else(|| "empty cluster".to_string());
            Ok(SummaryOutput {
                name,
                summary: format!("{} members", input.members.len()),
                confidence: 0.7,
            })
        }
    }

    fn mk_note(id: &str, folder: &str, base: f32) -> NoteInput {
        NoteInput {
            id: id.into(),
            title: format!("Note {id}"),
            summary: format!("notes about {folder}"),
            folder: folder.into(),
            embedding: vec![base, base + 0.01, base - 0.01, base + 0.005],
        }
    }

    #[test]
    fn build_and_persist_writes_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let trees = Db::new(
            Arc::new(crate::editing::LayeredDoc::open(dir.path()).unwrap()),
            Arc::new(crate::vault::Vault::open(dir.path()).unwrap()),
        )
        .unwrap();
        let mut notes: Vec<NoteInput> = Vec::new();
        for i in 0..6 {
            notes.push(mk_note(&format!("a{i}"), "research", 0.0 + (i as f32) * 0.001));
        }
        for i in 0..6 {
            notes.push(mk_note(&format!("b{i}"), "cooking", 10.0 + (i as f32) * 0.001));
        }
        let params = Params {
            min_cluster_size: 3,
            disable_recursion: true,
            ..Default::default()
        };
        let summarizer = MockSummarizer;
        let tree_id = persist(
            &trees,
            "test build",
            "one-shot",
            &BuildScope::Vault { source_types: Vec::new() },
            &BuildMethod::Cluster { params },
            &notes,
            &summarizer,
        )
        .unwrap();
        let row = trees.get_tree(&tree_id).unwrap().unwrap();
        assert_eq!(row.state, "draft");
        let nodes = trees.list_nodes(&tree_id).unwrap();
        assert!(nodes.iter().any(|n| matches!(n.kind, NodeKind::Cluster)));
        assert!(nodes.iter().any(|n| matches!(n.kind, NodeKind::Leaf)));
    }
}
