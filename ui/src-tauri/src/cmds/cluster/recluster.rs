// status: cluster-editor-recluster-subtree
// status: cluster-editor-recluster-subtree-policy-loss
// status: cluster-editor-recluster-subtree-placement-decoupled
//
// Worker-side recluster pipeline split out of `cluster.rs`. The
// `recluster_subtree_in_worker` entry point is invoked from the direct
// worker's non-LLM dispatch (`DirectWorkerHandlers::try_handle`) so the
// LLM-heavy rebuild + the tree-mutation pass both happen on the worker
// thread rather than on the IPC channel. The helpers exposed at
// `pub(super)` are reused by `review_tab.rs` for the structural /
// confirm-and-name flow.

use super::title_from_rel_path;
use crate::{CmdError, DirectWorkerHandlers};

/// Typed snapshot of a single cluster_nodes row as it flows through the
/// recluster pipeline and into `trees.db` history blobs (per
/// `cluster-editor-undo-redo`, `cluster-editor-recluster-subtree`). The
/// wire format on disk is JSON-shaped exactly like the legacy
/// `serde_json::json!({...})` blobs the planner used to emit — fields are
/// re-ordered alphabetically by serde_json's default `BTreeMap`-backed
/// `Map`, identically to the legacy `json!` literal, so existing history
/// rows in users' `trees.db` round-trip on undo.
///
/// `policy` holds the *serialized* `NodePolicy` JSON string, not the
/// deserialized struct — this matches the legacy
/// `policy.as_ref().and_then(|p| serde_json::to_string(p).ok())` storage
/// shape. Readers re-parse via `serde_json::from_str` at the apply
/// boundary.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct NodeSnapshot {
    pub(super) id: String,
    pub(super) parent_id: Option<String>,
    pub(super) kind: NodeSnapshotKind,
    pub(super) note_id: Option<String>,
    pub(super) name: String,
    pub(super) summary: String,
    pub(super) user_edited_name: bool,
    pub(super) user_edited_summary: bool,
    /// Stored as a serialized JSON *string* (matching the existing wire
    /// format in `trees.db` history rows). Serialized via
    /// `serde_json::to_string(&NodePolicy)`.
    pub(super) policy: Option<String>,
    pub(super) confidence: f32,
    pub(super) summary_membership_churn: u32,
    /// Only populated when emitted by the recluster planner; absent from
    /// `snapshot_prior_subtree` outputs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub(super) level: Option<usize>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum NodeSnapshotKind {
    Cluster,
    OutlierBucket,
    Leaf,
}

impl NodeSnapshotKind {
    fn from_node_kind(k: &hiker_core::trees::NodeKind) -> Self {
        match k {
            hiker_core::trees::NodeKind::Cluster => Self::Cluster,
            hiker_core::trees::NodeKind::OutlierBucket => Self::OutlierBucket,
            hiker_core::trees::NodeKind::Leaf => Self::Leaf,
        }
    }
}

/// Convert a slice of typed snapshots into the legacy `Vec<serde_json::Value>`
/// shape `record_recluster_subtree` / `record_split` accept. Kept at the
/// boundary so on-disk history rows stay byte-identical to the
/// pre-refactor `json!` shape.
pub(super) fn snapshots_to_json_values(
    snaps: &[NodeSnapshot],
) -> Result<Vec<serde_json::Value>, CmdError> {
    snaps
        .iter()
        .map(|s| serde_json::to_value(s).map_err(CmdError::from))
        .collect()
}

// Body of the recluster operation, lifted from the original sync tauri
// command. Runs from inside the direct-worker's non-LLM dispatch
// (`DirectWorkerHandlers::try_handle`) so the LLM-heavy rebuild + the
// tree-mutation pass both happen on the worker thread rather than on
// the IPC channel. Operates on `DirectWorkerHandlers`' refs rather
// than reaching back into the session.
pub(crate) fn recluster_subtree_in_worker(
    handlers: &DirectWorkerHandlers,
    tree_id: &str,
    node_id: &str,
    cluster_params_json: &str,
    carry_policies_down: bool,
) -> Result<serde_json::Value, String> {
    let params: hiker_core::cluster::ClusterParams = serde_json::from_str(cluster_params_json)
        .map_err(|e| format!("cluster_params_json: {e}"))?;

    let all = handlers
        .trees
        .list_nodes(tree_id)
        .map_err(|e| e.to_string())?;
    let index = TreeIndex::build(&all);
    let root_node = index
        .get(node_id)
        .ok_or_else(|| format!("node not found: {node_id}"))?;
    if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
        return Err("recluster only works on cluster nodes".into());
    }
    let (descendant_clusters, descendant_leaves) = {
        let (cs, ls) = index.descendants_of(node_id);
        (
            cs.into_iter().cloned().collect::<Vec<_>>(),
            ls.into_iter().cloned().collect::<Vec<_>>(),
        )
    };
    if descendant_leaves.len() < 4 {
        return Err("not enough leaves under this cluster to recluster (need >= 4)".into());
    }
    let resolved_policy = index.inherited_policy_of(node_id).cloned();
    let note_inputs = gather_note_inputs_for_leaves(handlers, &descendant_leaves)?;
    if note_inputs.len() < 4 {
        return Err("not enough embedded notes to recluster (need >= 4)".into());
    }

    let prior_subtree = snapshot_prior_subtree(&descendant_clusters);
    let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
        .iter()
        .map(|l| (l.id.clone(), l.parent.clone()))
        .collect();

    // Run the recursive build pass. Always Cluster-method (per spec).
    let summarizer = handlers.cluster_summarizer()?;
    let scope = hiker_core::cluster::BuildScope::Notes {
        ids: note_inputs.iter().map(|n| n.id.clone()).collect(),
        source_types: Vec::new(),
    };
    let build_method = hiker_core::cluster::BuildMethod::Cluster { params: params.clone() };
    let result =
        hiker_core::cluster::build_tree(scope, build_method, &note_inputs, &summarizer)
            .map_err(|e| format!("recluster build: {e}"))?;

    let plan = plan_recluster_subtree(PlanReclusterArgs {
        result: &result,
        node_id,
        descendant_leaves: &descendant_leaves,
        carry_policies_down,
        resolved_policy: resolved_policy.as_ref(),
    });
    let new_nodes_snapshot = plan.new_nodes_snapshot;
    let new_cluster_ids = plan.new_cluster_ids;
    let leaf_moves = plan.leaf_moves;

    let preserved_chain: Vec<(String, u32)> = index
        .ancestor_chain(node_id)
        .into_iter()
        .map(|(id, churn)| (id.to_string(), churn))
        .collect();
    apply_recluster_writes(
        handlers,
        tree_id,
        &descendant_clusters,
        &new_nodes_snapshot,
        &leaf_moves,
        &preserved_chain,
        &new_cluster_ids,
    )?;

    let prior_subtree_json =
        snapshots_to_json_values(&prior_subtree).map_err(|e| e.to_string())?;
    let new_nodes_json =
        snapshots_to_json_values(&new_nodes_snapshot).map_err(|e| e.to_string())?;
    handlers
        .trees
        .record_recluster_subtree(
            tree_id,
            node_id,
            &prior_subtree_json,
            &prior_leaf_parents,
            &new_nodes_json,
            &leaf_moves,
            if carry_policies_down {
                resolved_policy.as_ref()
            } else {
                None
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({ "new_cluster_ids": new_cluster_ids }))
}

/// Borrowed index over a `Vec<EditableNode>` used by the recluster
/// pipeline. Replaces the previous trio of free helpers
/// (`build_by_id_map` / `collect_subtree_descendants` /
/// `resolve_inherited_policy` / `collect_preserved_churn_chain`) so the
/// callers walk the node list once and reuse the result.
pub(super) struct TreeIndex<'a> {
    by_id: std::collections::HashMap<&'a str, &'a hiker_core::trees::EditableNode>,
    children_by_parent:
        std::collections::HashMap<&'a str, Vec<&'a hiker_core::trees::EditableNode>>,
}

impl<'a> TreeIndex<'a> {
    pub(super) fn build(nodes: &'a [hiker_core::trees::EditableNode]) -> Self {
        let mut by_id: std::collections::HashMap<&'a str, &'a hiker_core::trees::EditableNode> =
            std::collections::HashMap::new();
        let mut children_by_parent: std::collections::HashMap<
            &'a str,
            Vec<&'a hiker_core::trees::EditableNode>,
        > = std::collections::HashMap::new();
        for n in nodes {
            by_id.insert(n.id.as_str(), n);
            if let Some(p) = n.parent.as_deref() {
                children_by_parent.entry(p).or_default().push(n);
            }
        }
        Self {
            by_id,
            children_by_parent,
        }
    }

    pub(super) fn get(&self, id: &str) -> Option<&'a hiker_core::trees::EditableNode> {
        self.by_id.get(id).copied()
    }

    /// Walk the subtree under `root_id` and split descendants into
    /// clusters (for snapshot + deletion) and leaves (to feed the
    /// rebuild and to know prior parents for undo).
    pub(super) fn descendants_of(
        &self,
        root_id: &str,
    ) -> (
        Vec<&'a hiker_core::trees::EditableNode>,
        Vec<&'a hiker_core::trees::EditableNode>,
    ) {
        let mut descendant_clusters: Vec<&'a hiker_core::trees::EditableNode> = Vec::new();
        let mut descendant_leaves: Vec<&'a hiker_core::trees::EditableNode> = Vec::new();
        let mut stack: Vec<&str> = vec![root_id];
        while let Some(id) = stack.pop() {
            if let Some(kids) = self.children_by_parent.get(id) {
                for k in kids {
                    match k.kind {
                        hiker_core::trees::NodeKind::Leaf => {
                            descendant_leaves.push(*k);
                        }
                        _ => {
                            descendant_clusters.push(*k);
                            stack.push(k.id.as_str());
                        }
                    }
                }
            }
        }
        (descendant_clusters, descendant_leaves)
    }

    pub(super) fn inherited_policy_of(
        &self,
        node_id: &str,
    ) -> Option<&'a hiker_core::trees::NodePolicy> {
        let mut cursor: Option<&str> = Some(node_id);
        while let Some(id) = cursor {
            let n = self.by_id.get(id)?;
            if let Some(p) = &n.policy {
                return Some(p);
            }
            cursor = n.parent.as_deref();
        }
        None
    }

    /// Walk from `node_id` up to the root, returning `(id,
    /// summary_membership_churn)` for each ancestor (inclusive of
    /// `node_id`).
    pub(super) fn ancestor_chain(&self, node_id: &str) -> Vec<(&'a str, u32)> {
        let mut chain: Vec<(&'a str, u32)> = Vec::new();
        let mut cursor: Option<&str> = Some(node_id);
        while let Some(id) = cursor {
            let Some(n) = self.by_id.get(id).copied() else {
                break;
            };
            chain.push((n.id.as_str(), n.summary_membership_churn));
            cursor = n.parent.as_deref();
        }
        chain
    }
}

fn gather_note_inputs_for_leaves(
    handlers: &DirectWorkerHandlers,
    descendant_leaves: &[hiker_core::trees::EditableNode],
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let mut store = handlers.read_store.lock().map_err(|e| e.to_string())?;
    let mut note_inputs: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for l in descendant_leaves {
        let Some(note_id_) = l.note_ref.clone() else {
            continue;
        };
        let Ok(Some(path)) = store.path_for_id(&note_id_) else {
            continue;
        };
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        note_inputs.push(hiker_core::cluster::NoteInput {
            id: note_id_,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    Ok(note_inputs)
}

pub(super) fn snapshot_prior_subtree(
    descendant_clusters: &[hiker_core::trees::EditableNode],
) -> Vec<NodeSnapshot> {
    descendant_clusters
        .iter()
        .map(|c| NodeSnapshot {
            id: c.id.clone(),
            parent_id: c.parent.clone(),
            kind: NodeSnapshotKind::from_node_kind(&c.kind),
            note_id: c.note_ref.clone(),
            name: c.name.clone(),
            summary: c.summary.clone(),
            user_edited_name: c.user_edited_name,
            user_edited_summary: c.user_edited_summary,
            policy: c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
            confidence: c.confidence,
            summary_membership_churn: c.summary_membership_churn,
            level: None,
        })
        .collect()
}

struct PlanReclusterArgs<'a> {
    result: &'a hiker_core::cluster::BuildResult,
    node_id: &'a str,
    descendant_leaves: &'a [hiker_core::trees::EditableNode],
    carry_policies_down: bool,
    resolved_policy: Option<&'a hiker_core::trees::NodePolicy>,
}

pub(super) struct ReclusterPlan {
    pub(super) new_nodes_snapshot: Vec<NodeSnapshot>,
    pub(super) new_cluster_ids: Vec<String>,
    pub(super) leaf_moves: Vec<(String, Option<String>)>,
}

fn plan_recluster_subtree(args: PlanReclusterArgs<'_>) -> ReclusterPlan {
    let PlanReclusterArgs {
        result,
        node_id,
        descendant_leaves,
        carry_policies_down,
        resolved_policy,
    } = args;
    let ns = format!("recluster-{node_id}");
    let rename_id = |id: &str| -> String { format!("{ns}-{id}") };

    let levels = &result.tree.levels;
    let mut new_nodes_snapshot: Vec<NodeSnapshot> = Vec::new();
    let mut new_cluster_ids: Vec<String> = Vec::new();

    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for level in levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    let top = &levels[levels.len() - 1];
    let mut absorbed_top_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if top.len() == 1 {
        absorbed_top_ids.insert(top[0].id.clone());
    }

    for (level_idx, level) in levels.iter().enumerate().rev() {
        for node in level {
            if absorbed_top_ids.contains(&node.id) {
                continue;
            }
            let new_id = rename_id(&node.id);
            let parent_id = match parent_of.get(&node.id) {
                Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                _ => node_id.to_string(),
            };
            let policy = if carry_policies_down && parent_id == node_id {
                resolved_policy.cloned()
            } else {
                None
            };
            new_nodes_snapshot.push(NodeSnapshot {
                id: new_id.clone(),
                parent_id: Some(parent_id),
                kind: NodeSnapshotKind::Cluster,
                note_id: None,
                name: node.name.clone(),
                summary: node.summary.clone(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                confidence: node.confidence,
                summary_membership_churn: 0,
                level: Some(level_idx),
            });
            new_cluster_ids.push(new_id);
        }
    }

    let mut leaf_target: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(leaf_level) = levels.first() {
        for cluster in leaf_level {
            let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                node_id.to_string()
            } else {
                rename_id(&cluster.id)
            };
            for note_id_ in &cluster.members {
                leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
            }
        }
    }
    for note_id_ in &result.tree.outliers {
        leaf_target
            .entry(note_id_.clone())
            .or_insert_with(|| node_id.to_string());
    }

    let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
    for l in descendant_leaves {
        let target = l
            .note_ref
            .as_ref()
            .and_then(|nid| leaf_target.get(nid).cloned())
            .unwrap_or_else(|| node_id.to_string());
        leaf_moves.push((l.id.clone(), Some(target)));
    }

    ReclusterPlan {
        new_nodes_snapshot,
        new_cluster_ids,
        leaf_moves,
    }
}

fn apply_recluster_writes(
    handlers: &DirectWorkerHandlers,
    tree_id: &str,
    descendant_clusters: &[hiker_core::trees::EditableNode],
    new_nodes_snapshot: &[NodeSnapshot],
    leaf_moves: &[(String, Option<String>)],
    preserved_chain: &[(String, u32)],
    new_cluster_ids: &[String],
) -> Result<(), String> {
    for c in descendant_clusters {
        handlers
            .trees
            .delete_node(tree_id, &c.id)
            .map_err(|e| e.to_string())?;
    }
    for snap in new_nodes_snapshot {
        // `policy` is the *serialized* `NodePolicy` JSON string in the
        // wire format; re-parse it back into the typed struct before
        // handing it to the trees DB. Empty / missing strings round-trip
        // to `None` (matching the legacy `.and_then(serde_json::from_str)`
        // chain).
        let policy: Option<hiker_core::trees::NodePolicy> = snap
            .policy
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok());
        handlers
            .trees
            .insert_single_node(
                tree_id,
                hiker_core::trees::NodeInsert {
                    node_id: snap.id.clone(),
                    parent_id: snap.parent_id.clone(),
                    kind: hiker_core::trees::NodeKind::Cluster,
                    note_id: None,
                    name: snap.name.clone(),
                    summary: snap.summary.clone(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy,
                    centroid: None,
                    confidence: snap.confidence,
                    summary_membership_churn: 0,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    handlers
        .trees
        .reparent_many(tree_id, leaf_moves)
        .map_err(|e| e.to_string())?;
    for (id, prior) in preserved_chain {
        let _ = handlers.trees.set_churn(tree_id, id, *prior);
    }
    for id in new_cluster_ids {
        let _ = handlers.trees.reset_churn(tree_id, id);
    }
    Ok(())
}

pub(super) struct PlanFromBuiltArgs<'a> {
    pub(super) tree: &'a hiker_core::cluster::BuiltClusterTree,
    pub(super) node_id: &'a str,
    pub(super) descendant_leaves: &'a [hiker_core::trees::EditableNode],
    pub(super) carry_policies_down: bool,
    pub(super) resolved_policy: Option<&'a hiker_core::trees::NodePolicy>,
    pub(super) user_renamed: &'a std::collections::HashMap<String, String>,
    pub(super) ns: &'a str,
}

pub(super) fn plan_recluster_from_built(args: PlanFromBuiltArgs<'_>) -> ReclusterPlan {
    let PlanFromBuiltArgs {
        tree,
        node_id,
        descendant_leaves,
        carry_policies_down,
        resolved_policy,
        user_renamed,
        ns,
    } = args;
    let rename_id = |id: &str| -> String { format!("{ns}-{id}") };
    let levels = &tree.levels;
    let mut new_nodes_snapshot: Vec<NodeSnapshot> = Vec::new();
    let mut new_cluster_ids: Vec<String> = Vec::new();

    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for level in levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    let top: &[hiker_core::cluster::BuiltClusterNode] = if levels.is_empty() {
        &[]
    } else {
        &levels[levels.len() - 1]
    };
    let mut absorbed_top_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if top.len() == 1 {
        absorbed_top_ids.insert(top[0].id.clone());
    }

    for (level_idx, level) in levels.iter().enumerate().rev() {
        for node in level {
            if absorbed_top_ids.contains(&node.id) {
                continue;
            }
            let new_id = rename_id(&node.id);
            let parent_id = match parent_of.get(&node.id) {
                Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                _ => node_id.to_string(),
            };
            let policy = if carry_policies_down && parent_id == node_id {
                resolved_policy.cloned()
            } else {
                None
            };
            let user_renamed_name = user_renamed.get(&node.id).cloned();
            let final_name = user_renamed_name.clone().unwrap_or_else(|| node.name.clone());
            new_nodes_snapshot.push(NodeSnapshot {
                id: new_id.clone(),
                parent_id: Some(parent_id),
                kind: NodeSnapshotKind::Cluster,
                note_id: None,
                name: final_name,
                summary: node.summary.clone(),
                user_edited_name: user_renamed_name.is_some(),
                user_edited_summary: false,
                policy: policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                confidence: node.confidence,
                summary_membership_churn: 0,
                level: Some(level_idx),
            });
            new_cluster_ids.push(new_id);
        }
    }

    let mut leaf_target: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(leaf_level) = levels.first() {
        for cluster in leaf_level {
            let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                node_id.to_string()
            } else {
                rename_id(&cluster.id)
            };
            for note_id_ in &cluster.members {
                leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
            }
        }
    }
    for note_id_ in &tree.outliers {
        leaf_target
            .entry(note_id_.clone())
            .or_insert_with(|| node_id.to_string());
    }

    let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
    for l in descendant_leaves {
        let target = l
            .note_ref
            .as_ref()
            .and_then(|nid| leaf_target.get(nid).cloned())
            .unwrap_or_else(|| node_id.to_string());
        leaf_moves.push((l.id.clone(), Some(target)));
    }

    ReclusterPlan {
        new_nodes_snapshot,
        new_cluster_ids,
        leaf_moves,
    }
}

/// Like `apply_recluster_writes` but reads `user_edited_name` from the
/// snapshot so user-renamed clusters preserve their flag.
pub(super) fn apply_recluster_writes_with_user_edits(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    descendant_clusters: &[hiker_core::trees::EditableNode],
    new_nodes_snapshot: &[NodeSnapshot],
    leaf_moves: &[(String, Option<String>)],
    preserved_chain: &[(String, u32)],
    new_cluster_ids: &[String],
) -> Result<(), String> {
    for c in descendant_clusters {
        trees
            .delete_node(tree_id, &c.id)
            .map_err(|e| e.to_string())?;
    }
    for snap in new_nodes_snapshot {
        // See `apply_recluster_writes`: `policy` is the *serialized*
        // `NodePolicy` JSON string; re-parse before handing it to trees DB.
        let policy: Option<hiker_core::trees::NodePolicy> = snap
            .policy
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok());
        trees
            .insert_single_node(
                tree_id,
                hiker_core::trees::NodeInsert {
                    node_id: snap.id.clone(),
                    parent_id: snap.parent_id.clone(),
                    kind: hiker_core::trees::NodeKind::Cluster,
                    note_id: None,
                    name: snap.name.clone(),
                    summary: snap.summary.clone(),
                    user_edited_name: snap.user_edited_name,
                    user_edited_summary: false,
                    policy,
                    centroid: None,
                    confidence: snap.confidence,
                    summary_membership_churn: 0,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    trees
        .reparent_many(tree_id, leaf_moves)
        .map_err(|e| e.to_string())?;
    for (id, prior) in preserved_chain {
        let _ = trees.set_churn(tree_id, id, *prior);
    }
    for id in new_cluster_ids {
        let _ = trees.reset_churn(tree_id, id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-format equivalence: `serde_json::to_value(&NodeSnapshot)`
    /// must produce the exact same JSON shape as the legacy `json!({})`
    /// blob the recluster planner used to emit. Locked-in so that users'
    /// `trees.db` history rows written before this refactor still
    /// round-trip on undo / redo.
    #[test]
    fn node_snapshot_planner_wire_format_matches_legacy_json() {
        let policy_string = Some("{\"action\":\"freeze\"}".to_string());
        let snap = NodeSnapshot {
            id: "recluster-c1".to_string(),
            parent_id: Some("root".to_string()),
            kind: NodeSnapshotKind::Cluster,
            note_id: None,
            name: "Cluster 1".to_string(),
            summary: "summary".to_string(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: policy_string.clone(),
            confidence: 0.875,
            summary_membership_churn: 0,
            level: Some(2),
        };
        let got = serde_json::to_value(&snap).unwrap();
        let expected = serde_json::json!({
            "id": "recluster-c1",
            "parent_id": "root",
            "kind": "cluster",
            "note_id": null,
            "name": "Cluster 1",
            "summary": "summary",
            "user_edited_name": false,
            "user_edited_summary": false,
            "policy": policy_string,
            "confidence": 0.875_f32,
            "summary_membership_churn": 0,
            "level": 2,
        });
        assert_eq!(got, expected);
    }

    /// Prior-subtree snapshots omit `level`. The serializer must skip
    /// the field when `None` so the wire format matches the legacy
    /// `snapshot_prior_subtree` blob exactly.
    #[test]
    fn node_snapshot_prior_subtree_wire_format_omits_level() {
        let snap = NodeSnapshot {
            id: "n42".to_string(),
            parent_id: None,
            kind: NodeSnapshotKind::OutlierBucket,
            note_id: Some("note-7".to_string()),
            name: "Outliers".to_string(),
            summary: String::new(),
            user_edited_name: true,
            user_edited_summary: false,
            policy: None,
            confidence: 0.0,
            summary_membership_churn: 3,
            level: None,
        };
        let got = serde_json::to_value(&snap).unwrap();
        let expected = serde_json::json!({
            "id": "n42",
            "parent_id": null,
            "kind": "outlier-bucket",
            "note_id": "note-7",
            "name": "Outliers",
            "summary": "",
            "user_edited_name": true,
            "user_edited_summary": false,
            "policy": null,
            "confidence": 0.0_f32,
            "summary_membership_churn": 3,
        });
        assert_eq!(got, expected);
        // And the round-trip deserialization restores the same struct.
        let back: NodeSnapshot = serde_json::from_value(got).unwrap();
        assert!(back.level.is_none());
        assert_eq!(back.summary_membership_churn, 3);
    }
}
