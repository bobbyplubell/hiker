//! Offline build pipeline: cluster → summarize → flatten, plus the
//! `Db`-persistence wrappers (`persist`, `rebuild_and_persist`)
//! and the FromFolders alternate method. The placement classifier in
//! `tree.rs` shares the `Node` shape but lives on the online
//! path; this module produces the richer `BuiltClusterNode` described
//! in `clustering.md` §"Output: what suggestions consume".
//!
//! status: cluster-build-recursive
//! status: cluster-tree-output
//! status: cluster-build-from-folders

pub mod stream;

use super::algo::{
    cosine_similarity, l2_normalize, mean_normalize, ninetieth_percentile_distance, partition,
    partition_leiden,
};
use super::{
    BuildError, BuildMethod, BuildResult, BuildScope, BuiltClusterNode, BuiltClusterTree,
    Algorithm, Assignment, Error, Id, Params,
    FolderDeriveParams, MemberInfo, NoteInput, OUTLIER_LABEL, Phase, SummarizeInput, SummarizeMode,
    SummaryOutput, Summarizer,
};
use crate::trees::types::{Db, EditableNode, NodeInsert, NodeKind, TreeInsert};

/// Build a cluster tree from a resolved set of notes. Per
/// `cluster-build-recursive` + `cluster-build-cluster-method` (and
/// `cluster-build-from-folders` for the folder-derived branch).
///
/// The producer is responsible for resolving `scope` → `Vec<NoteInput>`
/// (the embeddings + per-note summary the level-0 pass needs). This
/// function then runs the recursive cluster → summarize → embed pipeline,
/// or — for `BuildMethod::FromFolders` — walks the per-note `folder`
/// strings to mirror the filesystem.
///
/// `summarizer` provides the per-cluster naming. Producers in production
/// hand in `LlmSummarizer`; tests pass small in-memory mocks.
pub fn tree(
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<BuildResult, BuildError> {
    if notes.is_empty() {
        return Err(BuildError::EmptyScope);
    }
    let tree = match &method {
        BuildMethod::Cluster { params } => {
            let mut sctx = StreamCtx {
                tx: None,
                cancel: Arc::new(AtomicBool::new(false)),
                items_processed: 0,
                clusters_found: 0,
                outliers: 0,
                partition_loop_counter: 0,
                max_partition_level_emitted: -1,
            };
            build_cluster_tree(notes, params, summarizer, &mut sctx)?
        }
        BuildMethod::FromFolders { params } => build_from_folders(notes, params, summarizer)?,
    };
    Ok(BuildResult {
        scope,
        method,
        tree,
    })
}

/// Convenience: build a fresh tree and persist it as a tree `.md`. The
/// resulting `cluster_trees` row + `cluster_nodes` rows are written
/// under one transaction (per `cluster-editor-draft-persistence` —
/// every node is editable from the moment it lands). Returns the new
/// `tree_id`.
pub fn persist(
    trees: &Db,
    name: &str,
    source: &str,
    scope: &BuildScope,
    method: &BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<String, BuildError> {
    let result = tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    let tree_id = trees
        .insert_tree(TreeInsert {
            id: None,
            name: name.to_string(),
            source: source.to_string(),
            state: "draft".to_string(),
            scope_json,
            method_json,
            vault_snapshot: None,
        })
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;
    let inserts = result_to_node_inserts(&result.tree);
    trees
        .insert_nodes(&tree_id, &inserts)
        .map_err(|e| BuildError::Summarizer(format!("insert_nodes: {e}")))?;
    Ok(tree_id)
}

/// Re-build an existing tree against the current vault state. Re-uses
/// the tree's saved `scope` + `method` (from `cluster_trees.scope` /
/// `.method`), re-runs `tree`, and persists a *new* tree row —
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

    let result = tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    let new_tree_id = trees
        .insert_tree(TreeInsert {
            id: None,
            name: new_name.to_string(),
            source: old_row.source.clone(),
            state: "draft".to_string(),
            scope_json,
            method_json,
            vault_snapshot: None,
        })
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;

    let mut inserts = result_to_node_inserts(&result.tree);

    // Compute per-old-cluster note-id member sets. Walk old_nodes once
    // to build child→parent map + leaf note ids per cluster.
    use std::collections::{HashMap, HashSet};
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
                        if let Some(nid) = &node.note_ref {
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

    trees
        .insert_nodes(&new_tree_id, &inserts)
        .map_err(|e| BuildError::Summarizer(format!("insert_nodes: {e}")))?;
    Ok(new_tree_id)
}

/// Flatten a `BuiltClusterTree` into the row shape `core::trees`
/// consumes. Top of the tree is the highest-level cluster (root); levels
/// descend with cluster-kind rows; the leaf level produces `leaf`-kind
/// rows under their parent clusters. Outliers attach as `leaf`-kind rows
/// under a dedicated `outlier-bucket` node parented at the root.
/// Public view onto `result_to_node_inserts` for callers outside this
/// module (e.g. the `cluster_persist_built_tree` command that drives
/// the clustering review tab's Confirm-and-name step).
///
/// status: cluster-review-tab-confirm-and-name
pub fn result_to_node_inserts_pub(tree: &BuiltClusterTree) -> Vec<NodeInsert> {
    result_to_node_inserts(tree)
}

pub(super) fn result_to_node_inserts(tree: &BuiltClusterTree) -> Vec<NodeInsert> {
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
    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
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

// ── Build recipe: top-down divisive Split ────────────────────────────
//
// status: cluster-build-recipe
// status: cluster-build-cluster-method
//
// The `Cluster` build method composes the ops-framework primitives per
// `clustering.md` §"Build recipe":
//
//   1. Split { target: virtual_root(scope), params: { recurse: true, ... } }
//      → produces a tree of leaf clusters with placeholder names.
//   2. Summarize { scope: All } (gated separately by Confirm-and-name).
//   3. (Optional, default off) Rollup over the top layer.
//
// `build_cluster_tree` runs step 1 only; summarization is invoked either
// (a) per-cluster by the recipe's inline `summarizer` argument when the
// user picks Confirm-and-name in the review tab, or (b) deferred when
// `SummarizeMode::None` is forced by the structural pass. Step 3 is an
// explicit cluster-editor verb and is not invoked here.
//
// **Algorithm shape**: top-down divisive. The first Split partitions the
// virtual root's note embeddings using `LeidenParams.top_level_resolution`
// (default 0.3) so the coarse top-level cut produces 3–8 broad clusters.
// Each top-level child is then recursively re-split using the regular
// `LeidenParams.resolution` (default 1.0) — at every sub-split the
// algorithm runs on *actual note embeddings within that branch's member
// set*, not on centroids-of-centroids. Sub-splits stop per branch when
// either child member count `<=` `leaf_min_size` OR child cohesion
// radius `<` `leaf_cohesion_threshold`. Hard 16-level safety cap.
//
// The legacy `min_clusters_to_recurse` knob is deserialized for
// backwards-compat (`#[serde(default, skip_serializing)]`) and ignored
// at runtime per the spec.

/// One node in the in-memory divisive tree the recipe builds before
/// flattening into `BuiltClusterTree`. A `branch` carries child nodes
/// (sub-clusters); a `leaf` carries note-id members directly. The
/// distinction maps onto `BuiltClusterNode.members` content (cluster
/// ids vs note ids) at flatten time.
enum SplitNode {
    /// Cluster that was further split into sub-clusters.
    Branch {
        id: String,
        centroid: Vec<f32>,
        radius: f32,
        name: String,
        summary: String,
        confidence: f32,
        children: Vec<SplitNode>,
    },
    /// Cluster that was not further split — its members are note ids.
    Leaf {
        id: String,
        centroid: Vec<f32>,
        radius: f32,
        name: String,
        summary: String,
        confidence: f32,
        note_ids: Vec<String>,
    },
}

use self::stream::StreamCtx;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Outcome of a partition pass: communities keyed by label, mapped to the
/// member indices in each, plus the outlier indices the partitioner peeled
/// off (folded into the first child after recursion).
type PartitionSplit = (std::collections::BTreeMap<i32, Vec<usize>>, Vec<usize>);

pub(super) fn build_cluster_tree(
    notes: &[NoteInput],
    params: &Params,
    summarizer: &dyn Summarizer,
    sctx: &mut StreamCtx,
) -> Result<BuiltClusterTree, BuildError> {
    Builder { notes, params, summarizer }.run(sctx)
}

/// Borrow-bundle for the build entry point. Splitting `run` into
/// `&self` methods keeps each phase under the cognitive-complexity
/// budget while sharing `notes`, `params`, and the summarizer with no
/// free-helper sprawl.
struct Builder<'a> {
    notes: &'a [NoteInput],
    params: &'a Params,
    summarizer: &'a dyn Summarizer,
}

impl<'a> Builder<'a> {
    fn run(&self, sctx: &mut StreamCtx) -> Result<BuiltClusterTree, BuildError> {
        // GMM isn't wired yet (linfa-clustering doesn't ship HDBSCAN;
        // see `clustering.md` §"Crate choice"). Producers requesting
        // `Gmm` fall back to `Hdbscan` on every Split call.
        //
        // status: cluster-algorithm-selectable (partial — gmm path stubbed)
        if matches!(self.params.algorithm, Algorithm::Gmm) {
            tracing::warn!(
                "cluster: gmm algorithm not yet supported; falling back to hdbscan"
            );
        }
        tracing::info!(
            algorithm = ?self.params.algorithm,
            note_count = self.notes.len(),
            recurse = !self.params.disable_recursion,
            leaf_min_size = self.params.leaf_min_size,
            leaf_cohesion_threshold = self.params.leaf_cohesion_threshold,
            top_level_resolution = self.params.leiden.top_level_resolution,
            resolution = self.params.leiden.resolution,
            include_outliers = self.params.include_outliers,
            "cluster: build recipe entry — top-down divisive Split from virtual root"
        );
        let (top_groups, top_outliers) = self.top_level_split(sctx)?;
        let top_level_nodes = self.build_top_level_nodes(top_groups, sctx)?;
        let outlier_ids: Vec<String> = if self.params.include_outliers {
            top_outliers.iter().map(|&i| self.notes[i].id.clone()).collect()
        } else {
            // By this point every outlier was force-routed into a
            // cluster via the recovery pass; anything still here is a
            // degenerate case (zero centroids, etc.). Drop it so the
            // output doesn't contradict `include_outliers = false`.
            Vec::new()
        };
        sctx.check_cancel()?;
        sctx.emit_phase(Phase::Finalizing);
        let ctx = self.split_branch_ctx();
        let tree = ctx.flatten_split_forest(top_level_nodes, outlier_ids);
        sctx.emit_counters();
        tracing::info!(
            total_levels = tree.levels.len(),
            per_level_counts = ?tree.levels.iter().map(std::vec::Vec::len).collect::<Vec<_>>(),
            outliers = tree.outliers.len(),
            "cluster: build recipe finished"
        );
        Ok(tree)
    }

    /// ── Step 1: top-level Split against the virtual root ─────────
    /// The first Split is special:
    ///   - Uses `top_level_resolution` (Leiden only) for a coarser cut.
    ///   - Handles outliers (Hybrid / `include_outliers = false`) by
    ///     force-routing them into the nearest top-level community.
    ///   - Requires at least 2 cohesive communities (else
    ///     `VaultTooSmall`).
    ///
    /// Sub-splits below use the regular `resolution` and silently
    /// fold outliers into a per-branch outlier list (the spec
    /// doesn't ask for recursive Hybrid recovery).
    fn top_level_split(
        &self,
        sctx: &mut StreamCtx,
    ) -> Result<PartitionSplit, BuildError> {
        sctx.check_cancel()?;
        sctx.emit_partition_phase_if_new(0);
        let indices: Vec<usize> = (0..self.notes.len()).collect();
        let top_assignments = partition_indices(
            self.notes,
            &indices,
            self.params,
            /* top_level */ true,
        )?;
        sctx.check_cancel()?;
        let mut top_groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for a in &top_assignments {
            top_groups
                .entry(a.cluster_label)
                .or_default()
                .push(indices[a.point_index]);
        }
        let mut top_outliers: Vec<usize> =
            top_groups.remove(&OUTLIER_LABEL).unwrap_or_default();
        if top_groups.len() < 2 {
            return Err(BuildError::VaultTooSmall {
                found: self.notes.len(),
            });
        }
        // Hybrid / force-routing applies only at the top level.
        let hybrid_recovery_for_algo = matches!(self.params.algorithm, Algorithm::Hybrid)
            && !matches!(self.params.algorithm, Algorithm::Leiden);
        if hybrid_recovery_for_algo || !self.params.include_outliers {
            top_outliers = self.recover_outliers(&mut top_groups, &top_outliers);
        }
        tracing::info!(
            top_level_clusters = top_groups.len(),
            outliers = top_outliers.len(),
            "cluster: top-level Split produced communities"
        );
        // Surface the outlier count to the progress stream now that
        // the top-level Split has settled.
        sctx.outliers = top_outliers.len() as u32;
        sctx.emit_counters();
        Ok((top_groups, top_outliers))
    }

    /// `include_outliers = false` → force-route every outlier into
    /// its nearest cluster (threshold `-1.0` admits everything). Per
    /// `cluster-build-cluster-method`'s "outlier recovery loop with
    /// cosine threshold dropped to -1.0" requirement.
    fn recover_outliers(
        &self,
        top_groups: &mut std::collections::BTreeMap<i32, Vec<usize>>,
        outliers: &[usize],
    ) -> Vec<usize> {
        let interim_centroids: std::collections::BTreeMap<i32, Vec<f32>> = top_groups
            .iter()
            .map(|(label, idxs)| {
                let refs: Vec<&[f32]> = idxs
                    .iter()
                    .map(|&i| self.notes[i].embedding.as_slice())
                    .collect();
                (*label, mean_normalize(&refs))
            })
            .collect();
        let threshold: f32 = if !self.params.include_outliers { -1.0 } else { 0.6 };
        let mut still_outliers: Vec<usize> = Vec::new();
        for &i in outliers {
            let q = l2_normalize(&self.notes[i].embedding);
            let mut best: Option<(i32, f32)> = None;
            for (label, centroid) in &interim_centroids {
                let s = cosine_similarity(&q, centroid);
                match best {
                    Some((_, bs)) if s <= bs => {}
                    _ => best = Some((*label, s)),
                }
            }
            match best {
                Some((label, score)) if score >= threshold => {
                    top_groups.entry(label).or_default().push(i);
                }
                _ => still_outliers.push(i),
            }
        }
        still_outliers
    }

    fn split_branch_ctx(&self) -> SplitBranchCtx<'a> {
        // Build a `SplitNode` per top-level community. Recursively
        // sub-split unless `disable_recursion` is set; the recursion
        // stops per-branch on `leaf_min_size` /
        // `leaf_cohesion_threshold` / 16-level cap.
        const MAX_DEPTH: u8 = 16;
        SplitBranchCtx {
            notes: self.notes,
            params: self.params,
            summarizer: self.summarizer,
            recurse: !self.params.disable_recursion,
            max_depth: MAX_DEPTH,
        }
    }

    fn build_top_level_nodes(
        &self,
        top_groups: std::collections::BTreeMap<i32, Vec<usize>>,
        sctx: &mut StreamCtx,
    ) -> Result<Vec<SplitNode>, BuildError> {
        let ctx = self.split_branch_ctx();
        ctx.split_top_level_groups(top_groups, sctx)
    }
}

/// Partition the `indices` subset of `notes` by their embeddings. The
/// `top_level` flag swaps in `LeidenParams.top_level_resolution` for
/// `resolution`; sub-splits get the normal `resolution`. Returns the
/// partitioner's `Assignment`s with `point_index` indexing into
/// the *local* `indices` slice (i.e. 0..indices.len()) — callers
/// translate back to global `notes` indices themselves.
fn partition_indices(
    notes: &[NoteInput],
    indices: &[usize],
    params: &Params,
    top_level: bool,
) -> Result<Vec<Assignment>, Error> {
    let embeddings: Vec<Vec<f32>> =
        indices.iter().map(|&i| notes[i].embedding.clone()).collect();
    match params.algorithm {
        Algorithm::Leiden => {
            let mut leiden = params.leiden.clone();
            if top_level {
                leiden.resolution = params.leiden.top_level_resolution;
            }
            // Clamp k_nearest to n-1 so the kNN graph build doesn't ask
            // for more neighbors than exist in the local subset.
            let upper = indices.len().saturating_sub(1).max(1) as u32;
            leiden.k_nearest = leiden.k_nearest.min(upper);
            partition_leiden(&embeddings, &leiden)
        }
        _ => partition(
            &embeddings,
            params.min_cluster_size as usize,
            params.min_samples.map(|x| x as usize),
        ),
    }
}

/// Build a `SplitNode` for one branch. Computes centroid + radius +
/// summary for this cluster, then either:
///   - emits a `Leaf` when stop conditions trip (member count
///     `<=` `leaf_min_size`, OR cohesion radius `<` `leaf_cohesion_threshold`,
///     OR recursion disabled, OR depth cap reached, OR sub-split fails
///     to produce >= 2 communities), or
///   - emits a `Branch` containing recursively-split children.
///
/// `member_idxs` are indices into the outer `notes` slice.
///
/// Borrow-bundle of the invariants that stay fixed across every
/// recursion frame of `recursive_split_branch`: the notes slice,
/// cluster params, summarizer trait object, whether sub-splits run,
/// and the recursion cap. Only `id`, `member_idxs`, and `depth`
/// differ per frame.
struct SplitBranchCtx<'a> {
    notes: &'a [NoteInput],
    params: &'a Params,
    summarizer: &'a dyn Summarizer,
    recurse: bool,
    max_depth: u8,
}

impl<'a> SplitBranchCtx<'a> {
    fn split_top_level_groups(
        &self,
        top_groups: std::collections::BTreeMap<i32, Vec<usize>>,
        sctx: &mut StreamCtx,
    ) -> Result<Vec<SplitNode>, BuildError> {
        let mut out: Vec<SplitNode> = Vec::new();
        for (label, idxs) in top_groups.into_iter() {
            // Per-cluster cancellation check at the top level — `idxs`
            // may be large and `recursive_split_branch` may not return
            // for a while if the sub-split is deep.
            sctx.check_cancel()?;
            let id = format!("c0-{label}");
            // Top-level clusters have `parent = None` per
            // `cluster-build-progress-stream`.
            let node = self.recursive_split_branch(id, &idxs, /* depth */ 1, &None, sctx)?;
            out.push(node);
        }
        Ok(out)
    }

    /// Flatten the top-down divisive forest into a `BuiltClusterTree`. The
    /// `levels` contract per `cluster-tree-output`:
    ///
    /// - `levels[0]` = leaf clusters (`members` are note ids).
    /// - `levels[k>0]` = parent clusters (`members` are child cluster ids).
    /// - `levels.last()` = top-level (root candidates).
    fn flatten_split_forest(
        &self,
        top_level: Vec<SplitNode>,
        outliers: Vec<String>,
    ) -> BuiltClusterTree {
        let mut levels: Vec<Vec<BuiltClusterNode>> = Vec::new();
        let mut top_ids: Vec<String> = Vec::new();
        let mut top_centroids: Vec<Vec<f32>> = Vec::new();

        for node in top_level {
            let (_lvl, id, centroid) = place_in_levels(node, &mut levels);
            top_ids.push(id);
            top_centroids.push(centroid);
        }

        // Add a synthetic vault root when there's more than one top-level
        // cluster. With exactly one, the persistence flatten
        // (`result_to_node_inserts`) treats it as root naturally.
        if top_ids.len() > 1 {
            let refs: Vec<&[f32]> =
                top_centroids.iter().map(std::vec::Vec::as_slice).collect();
            let centroid = mean_normalize(&refs);
            // Place above every other level.
            let target_level = levels.len();
            while levels.len() <= target_level {
                levels.push(Vec::new());
            }
            levels[target_level].push(BuiltClusterNode {
                id: "vault-root".to_string(),
                members: top_ids,
                centroid,
                radius: 0.0,
                name: String::new(),
                summary: String::new(),
                confidence: 1.0,
            });
        }

        BuiltClusterTree { levels, outliers }
    }
}

/// Per-frame state for one `recursive_split_branch` invocation.
/// Centroid / radius / summary are computed once at frame entry; the
/// `emit_leaf` and child-handling methods read them off `self` so the
/// recursion body stays focussed on control flow instead of plumbing.
struct BranchFrame<'a, 'b> {
    ctx: &'a SplitBranchCtx<'b>,
    id: String,
    member_idxs: &'a [usize],
    depth: u8,
    parent_id: Option<Id>,
    centroid: Vec<f32>,
    radius: f32,
    name: String,
    summary: String,
    confidence: f32,
}

impl<'b> SplitBranchCtx<'b> {
    /// Build a `BranchFrame` for this recursion level. Pre-computes
    /// centroid / radius / summary so the recursion body in
    /// `recursive_split_branch` can stay focussed on control flow.
    fn open_branch<'a>(
        &'a self,
        id: String,
        member_idxs: &'a [usize],
        depth: u8,
        parent_id: Option<Id>,
    ) -> Result<BranchFrame<'a, 'b>, BuildError> {
        let ctx = self;
        let notes = ctx.notes;
        let refs: Vec<&[f32]> = member_idxs
            .iter()
            .map(|&i| notes[i].embedding.as_slice())
            .collect();
        let centroid = mean_normalize(&refs);
        let radius = ninetieth_percentile_distance(&centroid, &refs);
        let infos: Vec<MemberInfo<'_>> = member_idxs
            .iter()
            .map(|&i| MemberInfo {
                title: &notes[i].title,
                summary: &notes[i].summary,
            })
            .collect();
        // Summarize at this cluster's level (depth from top, 0-indexed
        // for the summarizer's `level` field — keeps the LLM prompt
        // shape consistent with prior pipeline).
        let SummaryOutput {
            name,
            summary,
            confidence,
        } = run_summarizer(
            ctx.params.summarize,
            depth as usize - 1,
            infos,
            ctx.summarizer,
        )?;
        Ok(BranchFrame {
            ctx,
            id,
            member_idxs,
            depth,
            parent_id,
            centroid,
            radius,
            name,
            summary,
            confidence,
        })
    }
}

impl<'a, 'b> BranchFrame<'a, 'b> {
    /// Emit the leaf cluster event + return the `Leaf` node. Called
    /// from every "branch decided to be a leaf" exit point.
    /// status: cluster-build-progress-stream
    fn emit_leaf(&self, sctx: &mut StreamCtx) -> SplitNode {
        let notes = self.ctx.notes;
        let note_ids: Vec<String> =
            self.member_idxs.iter().map(|&i| notes[i].id.clone()).collect();
        sctx.items_processed = sctx.items_processed.saturating_add(note_ids.len() as u32);
        sctx.emit_cluster(
            BuiltClusterNode {
                id: self.id.clone(),
                members: note_ids.clone(),
                centroid: self.centroid.clone(),
                radius: self.radius,
                name: self.name.clone(),
                summary: self.summary.clone(),
                confidence: self.confidence,
            },
            self.parent_id.clone(),
        );
        sctx.emit_counters();
        SplitNode::Leaf {
            id: self.id.clone(),
            centroid: self.centroid.clone(),
            radius: self.radius,
            name: self.name.clone(),
            summary: self.summary.clone(),
            confidence: self.confidence,
            note_ids,
        }
    }

    /// Stop-condition check. Returns `Some(leaf)` if this frame should
    /// emit a leaf rather than sub-split.
    fn try_stop(&self, sctx: &mut StreamCtx) -> Option<SplitNode> {
        let params = self.ctx.params;
        let too_small = self.member_idxs.len() <= params.leaf_min_size as usize;
        let too_tight = self.radius < params.leaf_cohesion_threshold;
        let at_cap = self.depth >= self.ctx.max_depth;
        if !self.ctx.recurse || too_small || too_tight || at_cap {
            let reason = if !self.ctx.recurse {
                "disable_recursion"
            } else if at_cap {
                "16-level cap"
            } else if too_small {
                "member_count <= leaf_min_size"
            } else {
                "radius < leaf_cohesion_threshold"
            };
            tracing::debug!(
                id = %self.id,
                depth = self.depth,
                members = self.member_idxs.len(),
                radius = self.radius,
                reason,
                "cluster: branch stopped — emitting leaf cluster"
            );
            return Some(self.emit_leaf(sctx));
        }
        None
    }

    /// Run the sub-split partition. `Err` here means the partitioner
    /// itself errored; a `Ok` with `<2` communities still means "leaf"
    /// — the caller checks. Outliers are split out so the caller can
    /// fold them into the first child after recursion.
    fn sub_split(
        &self,
        sctx: &mut StreamCtx,
    ) -> Result<PartitionSplit, Error> {
        let sub_assignments = partition_indices(
            self.ctx.notes,
            self.member_idxs,
            self.ctx.params,
            /* top_level */ false,
        )?;
        let mut sub_groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for a in &sub_assignments {
            // Periodic per-node cancellation check inside the
            // partition assignment loop, per
            // `cluster-build-async-pass`. Cheap atomic load amortized
            // via `PARTITION_CHECK_INTERVAL`.
            if sctx.check_cancel_periodic().is_err() {
                // Cancellation surfaces through `recursive_split_branch`
                // directly; here we just bail.
                break;
            }
            if a.cluster_label == OUTLIER_LABEL {
                // Per spec, sub-splits don't run a Hybrid-style
                // recovery; outliers at this level fold back into the
                // *parent* cluster as plain members.
                continue;
            }
            sub_groups
                .entry(a.cluster_label)
                .or_default()
                .push(self.member_idxs[a.point_index]);
        }
        let sub_outlier_local: Vec<usize> = sub_assignments
            .iter()
            .filter(|a| a.cluster_label == OUTLIER_LABEL)
            .map(|a| self.member_idxs[a.point_index])
            .collect();
        Ok((sub_groups, sub_outlier_local))
    }

    /// Finalize a branch with `children` already built and any
    /// sub-level outliers folded in. Emits the `ClusterDiscovered`
    /// event in child-first order.
    fn finalize_branch(self, children: Vec<SplitNode>, sctx: &mut StreamCtx) -> SplitNode {
        // status: cluster-build-progress-stream
        let child_ids: Vec<String> = children
            .iter()
            .map(|node| match node {
                SplitNode::Leaf { id, .. } | SplitNode::Branch { id, .. } => id.clone(),
            })
            .collect();
        sctx.emit_cluster(
            BuiltClusterNode {
                id: self.id.clone(),
                members: child_ids,
                centroid: self.centroid.clone(),
                radius: self.radius,
                name: self.name.clone(),
                summary: self.summary.clone(),
                confidence: self.confidence,
            },
            self.parent_id.clone(),
        );
        sctx.emit_counters();
        SplitNode::Branch {
            id: self.id,
            centroid: self.centroid,
            radius: self.radius,
            name: self.name,
            summary: self.summary,
            confidence: self.confidence,
            children,
        }
    }
}

impl<'a> SplitBranchCtx<'a> {
    fn recursive_split_branch(
        &self,
        id: String,
        member_idxs: &[usize],
        depth: u8,
        parent_id: &Option<Id>,
        sctx: &mut StreamCtx,
    ) -> Result<SplitNode, BuildError> {
        // Level-boundary cancellation check on every recursion frame
        // entry.
        sctx.check_cancel()?;
        sctx.emit_partition_phase_if_new(depth as u32);
        let frame = self.open_branch(id, member_idxs, depth, parent_id.clone())?;
        if let Some(leaf) = frame.try_stop(sctx) {
            return Ok(leaf);
        }
        // Recursive sub-split using the normal `resolution`. If the
        // partitioner errors, the branch can't be refined further —
        // emit a leaf cluster instead. We don't propagate the
        // partition error: a sub-split is allowed to fail to refine
        // without aborting the whole build (the per-branch outcome is
        // "this stays a leaf cluster").
        let (sub_groups, sub_outlier_local) = match frame.sub_split(sctx) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(
                    id = %frame.id,
                    depth = frame.depth,
                    error = %e,
                    "cluster: sub-split partition errored — emitting leaf cluster"
                );
                return Ok(frame.emit_leaf(sctx));
            }
        };
        if sub_groups.len() < 2 {
            tracing::debug!(
                id = %frame.id,
                depth = frame.depth,
                members = frame.member_idxs.len(),
                sub_communities = sub_groups.len(),
                "cluster: sub-split produced <2 communities — emitting leaf cluster"
            );
            return Ok(frame.emit_leaf(sctx));
        }
        tracing::debug!(
            id = %frame.id,
            depth = frame.depth,
            sub_communities = sub_groups.len(),
            sub_outliers = sub_outlier_local.len(),
            "cluster: sub-split accepted"
        );
        let mut children: Vec<SplitNode> = Vec::new();
        for (label, child_idxs) in sub_groups.into_iter() {
            sctx.check_cancel()?;
            let child_id = format!("{}-s{label}", frame.id);
            let child = self.recursive_split_branch(
                child_id,
                &child_idxs,
                frame.depth + 1,
                &Some(frame.id.clone()),
                sctx,
            )?;
            children.push(child);
        }
        // Sub-level outliers are folded into the first child cluster
        // as plain members so they remain reachable in the persisted
        // tree. Matches the build recipe's "every note gets a home
        // under the top-level community" intent.
        if !sub_outlier_local.is_empty()
            && let Some(first) = children.first_mut()
        {
            fold_into_first_leaf(first, &sub_outlier_local, self.notes);
        }
        Ok(frame.finalize_branch(children, sctx))
    }
}

/// Fold extra note indices into the first leaf descendant of `node`,
/// recomputing centroid + radius locally. Used to absorb sub-level
/// partition outliers (see `recursive_split_branch`).
fn fold_into_first_leaf(node: &mut SplitNode, extra_idxs: &[usize], notes: &[NoteInput]) {
    match node {
        SplitNode::Leaf {
            centroid,
            radius,
            note_ids,
            ..
        } => {
            for &i in extra_idxs {
                note_ids.push(notes[i].id.clone());
            }
            // Recompute centroid + radius over the full member set.
            // Need to rebuild the embedding refs from `note_ids`; we
            // don't have a id→idx map handy, but we can recompute from
            // the new combined set: walk `notes` for ids that match.
            // Cheaper: append `extra_idxs` embeddings to the prior
            // mean by re-deriving from a built set.
            let by_id: std::collections::HashMap<&str, &NoteInput> =
                notes.iter().map(|n| (n.id.as_str(), n)).collect();
            let mut refs: Vec<&[f32]> = Vec::with_capacity(note_ids.len());
            for nid in note_ids.iter() {
                if let Some(n) = by_id.get(nid.as_str()) {
                    refs.push(n.embedding.as_slice());
                }
            }
            *centroid = mean_normalize(&refs);
            *radius = ninetieth_percentile_distance(centroid, &refs);
        }
        SplitNode::Branch { children, .. } => {
            if let Some(first) = children.first_mut() {
                fold_into_first_leaf(first, extra_idxs, notes);
            }
        }
    }
}

/// Flatten the top-down divisive forest into a `BuiltClusterTree`. The
/// `levels` contract per `cluster-tree-output`:
///
/// - `levels[0]` = leaf clusters (`members` are note ids).
/// - `levels[k>0]` = parent clusters (`members` are child cluster ids).
/// - `levels.last()` = top-level (root candidates).
///
/// Because the divisive build produces uneven branch depths, we pack
/// each cluster at `level = max_descendant_depth + 1` (a leaf cluster
/// sits at level 0; its parent at level 1; etc., taking the *max* of
/// each child's level so parents always sit above all their children).
///
/// To keep `result_to_node_inserts`'s "synthesize a root iff top.len() != 1"
/// machinery happy when the virtual-root Split produces top-level
/// clusters at different levels (which happens when some branches went
/// deeper than others), we **always** add a synthetic vault root to
/// `levels` when there is more than one top-level cluster. The root's
/// `members` are the top-level cluster ids. When there is exactly one
/// top-level cluster (theoretically impossible since we error
/// `VaultTooSmall` below 2 communities, but defended for safety), it
/// becomes the natural root.
/// Recursively place a `SplitNode` into `levels`. Returns the level
/// index, id, and centroid of the placed node. A leaf cluster lands at
/// level 0; a branch lands at `1 + max(child levels)`.
fn place_in_levels(
    node: SplitNode,
    levels: &mut Vec<Vec<BuiltClusterNode>>,
) -> (usize, String, Vec<f32>) {
    match node {
        SplitNode::Leaf {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            note_ids,
        } => {
            while levels.is_empty() {
                levels.push(Vec::new());
            }
            let built = BuiltClusterNode {
                id: id.clone(),
                members: note_ids,
                centroid: centroid.clone(),
                radius,
                name,
                summary,
                confidence,
            };
            levels[0].push(built);
            (0, id, centroid)
        }
        SplitNode::Branch {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            children,
        } => {
            let mut child_ids: Vec<String> = Vec::with_capacity(children.len());
            let mut max_child_level: usize = 0;
            for child in children {
                let (lvl, child_id, _c) = place_in_levels(child, levels);
                child_ids.push(child_id);
                max_child_level = max_child_level.max(lvl);
            }
            let level_idx = max_child_level + 1;
            while levels.len() <= level_idx {
                levels.push(Vec::new());
            }
            let built = BuiltClusterNode {
                id: id.clone(),
                members: child_ids,
                centroid: centroid.clone(),
                radius,
                name,
                summary,
                confidence,
            };
            levels[level_idx].push(built);
            (level_idx, id, centroid)
        }
    }
}

fn run_summarizer(
    mode: SummarizeMode,
    level: usize,
    members: Vec<MemberInfo<'_>>,
    summarizer: &dyn Summarizer,
) -> Result<SummaryOutput, BuildError> {
    match mode {
        // status: cluster-review-tab-structural-pass-no-llm
        // `SummarizeMode::None` short-circuits the summarizer call
        // entirely so the structural pass requires no LLM client. Names
        // are left blank here; the caller (`tree_structural`)
        // assigns placeholder `"Cluster N"` names ordered by
        // member-count-descending so the result panel has something
        // human-meaningful to show before Confirm-and-name fires.
        SummarizeMode::None => {
            let _ = members;
            Ok(SummaryOutput {
                name: String::new(),
                summary: String::new(),
                confidence: 0.0,
            })
        }
        SummarizeMode::Llm => summarizer.summarize(SummarizeInput { level, members }),
    }
}

/// No-op summarizer used by the structural-only build path
/// (`tree_structural`). Cannot actually be invoked because the
/// structural path forces `SummarizeMode::None` on every method param;
/// returns an error loudly if it ever is, so an accidental misuse is
/// observable rather than silent.
///
/// status: cluster-review-tab-structural-pass-no-llm
pub struct NoopSummarizer;

impl Summarizer for NoopSummarizer {
    fn summarize(&self, _input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
        Err(BuildError::Summarizer(
            "NoopSummarizer cannot summarize — structural pass should have set SummarizeMode::None".into(),
        ))
    }
}

/// Run a structural-only cluster build: no LLM calls, no `Summarizer`
/// dependency. Forces `SummarizeMode::None` on the method's params (so a
/// caller that passes `Llm` doesn't accidentally hit the summarizer) and
/// assigns placeholder names (`"Cluster 1"`, `"Cluster 2"`, …) to the
/// resulting leaf-level clusters in member-count-descending order.
/// Recursive levels above the leaf level are left with empty names —
/// the user only ever sees the leaf level in the clustering review
/// panel; higher levels exist only as parents for the persisted tree
/// shape.
///
/// status: cluster-review-tab-run-clustering
/// status: cluster-review-tab-structural-pass-no-llm
pub fn tree_structural(
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
) -> Result<BuildResult, BuildError> {
    let forced_method = match method {
        BuildMethod::Cluster { mut params } => {
            params.summarize = SummarizeMode::None;
            BuildMethod::Cluster { params }
        }
        BuildMethod::FromFolders { mut params } => {
            params.summarize = SummarizeMode::None;
            BuildMethod::FromFolders { params }
        }
    };
    let noop = NoopSummarizer;
    let mut result = tree(scope, forced_method, notes, &noop)?;
    // Walk only level 0 (leaf-level clusters) for placeholder naming.
    // FromFolders already sets the folder basename as the name in
    // `SummarizeMode::None`, so we leave those alone; the heuristic
    // below treats any cluster whose `name` is empty as needing a
    // placeholder.
    if let Some(leaf_level) = result.tree.levels.get_mut(0) {
        let mut order: Vec<usize> = (0..leaf_level.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(leaf_level[i].members.len()));
        let mut next_n: usize = 1;
        for &i in &order {
            if leaf_level[i].name.is_empty() {
                leaf_level[i].name = format!("Cluster {}", next_n);
                next_n += 1;
            }
        }
    }
    Ok(result)
}

// ── FromFolders method ───────────────────────────────────────────────

pub(super) fn build_from_folders(
    notes: &[NoteInput],
    params: &FolderDeriveParams,
    summarizer: &dyn Summarizer,
) -> Result<BuiltClusterTree, BuildError> {
    // Group notes by folder. Each unique folder is one leaf-level cluster.
    let mut by_folder: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, n) in notes.iter().enumerate() {
        by_folder.entry(n.folder.clone()).or_default().push(i);
    }
    if by_folder.is_empty() {
        return Err(BuildError::EmptyScope);
    }

    let mut level0: Vec<BuiltClusterNode> = Vec::new();
    for (folder, idxs) in &by_folder {
        let safe_folder = folder.replace('/', "-");
        let id = format!("f-{}", if safe_folder.is_empty() { "_root" } else { &safe_folder });
        let refs: Vec<&[f32]> = idxs
            .iter()
            .map(|&i| notes[i].embedding.as_slice())
            .collect();
        let centroid = mean_normalize(&refs);
        let radius = ninetieth_percentile_distance(&centroid, &refs);
        let members: Vec<String> = idxs.iter().map(|&i| notes[i].id.clone()).collect();
        // Default name is the folder basename per spec.
        let basename = folder.rsplit('/').next().unwrap_or("");
        let default_name = if basename.is_empty() {
            "vault root".to_string()
        } else {
            basename.to_string()
        };
        let SummaryOutput { name, summary, confidence } = match params.summarize {
            SummarizeMode::Llm => {
                let infos: Vec<MemberInfo<'_>> = idxs
                    .iter()
                    .map(|&i| MemberInfo {
                        title: &notes[i].title,
                        summary: &notes[i].summary,
                    })
                    .collect();
                let mut out = run_summarizer(params.summarize, 0, infos, summarizer)?;
                if out.name.is_empty() {
                    out.name = default_name.clone();
                }
                out
            }
            SummarizeMode::None => SummaryOutput {
                name: default_name.clone(),
                summary: String::new(),
                confidence: 1.0,
            },
        };
        level0.push(BuiltClusterNode {
            id,
            members,
            centroid,
            radius,
            // FromFolders trees have confidence 1.0 per the spec: the
            // folder structure is the source of truth, not a guess.
            name,
            summary,
            confidence: confidence.max(1.0),
        });
    }

    // FromFolders is a single-level tree (root synthesized at flatten
    // time per `result_to_node_inserts` when level0.len() > 1).
    Ok(BuiltClusterTree {
        levels: vec![level0],
        outliers: Vec::new(),
    })
}
