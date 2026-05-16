//! cluster-op-split (`Trees::split_cluster`).
//!
//! Layer-straddle note (per the refactor brief's cleanup item #2): the
//! recursive Split algorithm appears in two places —
//! `core::cluster::recursive_split_branch` (build pass; operates on
//! `Vec<NoteInput>`, returns a `BuiltClusterTree`) and the user-verb
//! version here (operates on persisted leaf rows + DB writes per
//! recursion level for undo). On reread these share only the partition
//! call; everything else (member harvest, leaf-row reparent, per-level
//! history rows, virtual-root vs real-node entry shapes) differs. Both
//! sides already share `core::cluster::partition` / `partition_leiden`.
//! Lifting a `split_levels(input, callbacks)` helper either pushes those
//! callbacks through a generic interface that obscures the per-side
//! semantics, or it just shuffles the loop body around without removing
//! duplication. Left as-is — flagged here for any future revisit.

use super::super::types::{
    EditableNode, NodeInsert, NodeKind, SplitOutcome, Trees, TreesError,
};

/// Borrow-bundle of the invariants that stay fixed across every
/// recursion frame of `split_cluster_recursive`: the tree id, cluster
/// params, leaf-embedding resolver, and recursion cap. Only the
/// `target_node_id`, `virtual_root_inputs`, `is_top_level`, and
/// `depth` differ per frame; `outcome` is the shared `&mut`
/// accumulator threaded separately.
pub(in crate::trees) struct SplitRecursionCtx<'a> {
    pub tree_id: &'a str,
    pub params: &'a crate::cluster::ClusterParams,
    pub embeddings_for_leaf: &'a dyn Fn(&str) -> Option<Vec<f32>>,
    pub max_depth: u8,
}

impl Trees {
    // ── cluster-op-split ───────────────────────────────────────────────
    //
    // status: cluster-op-split
    //
    // Orchestrates the Split operation. Resolves the target (virtual root
    // vs real cluster node) into a set of leaf members, fetches their
    // embeddings via the caller-supplied resolver, runs
    // `core::cluster::partition` (or `partition_leiden`), inserts one new
    // sub-cluster row per HDBSCAN/Leiden community, and reparents the
    // affected leaves. When `params.recurse = true`, each newly-produced
    // child is recursively re-split while it remains both above the
    // `leaf_min_size` member count and above the `leaf_cohesion_threshold`
    // radius; one history row per recursion level lands so undo can step
    // back through levels.
    //
    // Module discipline: this method takes a closure to fetch leaf
    // embeddings rather than importing `core::store`, mirroring the
    // existing `Trees::*` shape. The Tauri command in `ui/src-tauri/src/lib.rs`
    // wires up the resolver.
    pub fn split_cluster(
        &self,
        tree_id: &str,
        target_node_id: Option<&str>,
        params: &crate::cluster::ClusterParams,
        embeddings_for_leaf: &dyn Fn(&str) -> Option<Vec<f32>>,
        virtual_root_inputs: Option<&[crate::cluster::NoteInput]>,
    ) -> Result<SplitOutcome, TreesError> {
        const MAX_RECURSION_LEVELS: u8 = 16;
        let mut outcome = SplitOutcome {
            new_clusters: Vec::new(),
            total_levels: 0,
            outliers: Vec::new(),
        };
        let ctx = SplitRecursionCtx {
            tree_id,
            params,
            embeddings_for_leaf,
            max_depth: MAX_RECURSION_LEVELS,
        };
        self.split_cluster_recursive(
            &ctx,
            target_node_id,
            virtual_root_inputs,
            true,
            0,
            &mut outcome,
        )?;
        Ok(outcome)
    }

    pub(in crate::trees) fn split_cluster_recursive(
        &self,
        ctx: &SplitRecursionCtx<'_>,
        target_node_id: Option<&str>,
        virtual_root_inputs: Option<&[crate::cluster::NoteInput]>,
        is_top_level: bool,
        depth: u8,
        outcome: &mut SplitOutcome,
    ) -> Result<(), TreesError> {
        let tree_id = ctx.tree_id;
        let params = ctx.params;
        let embeddings_for_leaf = ctx.embeddings_for_leaf;
        let max_depth = ctx.max_depth;
        if depth >= max_depth {
            return Ok(());
        }
        // 1. Collect (note_id, embedding) members of this target.
        //    Real-node: walk the subtree, harvest leaves, pull embeddings
        //    via the resolver. Virtual-root: take the caller's NoteInputs.
        //    For history we also need the leaves' existing node_ids when
        //    the target is a real node — those leaves get reparented.
        let mut leaf_node_ids: Vec<String> = Vec::new();
        let mut leaf_note_refs: Vec<String> = Vec::new();
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        match (target_node_id, virtual_root_inputs) {
            (Some(real_target), _) => {
                let all = self.list_nodes(tree_id)?;
                let mut children_by_parent: std::collections::HashMap<String, Vec<EditableNode>> =
                    std::collections::HashMap::new();
                for n in all.iter().cloned() {
                    if let Some(p) = n.parent.clone() {
                        children_by_parent.entry(p).or_default().push(n);
                    }
                }
                let mut stack = vec![real_target.to_string()];
                while let Some(id) = stack.pop() {
                    if let Some(kids) = children_by_parent.get(&id) {
                        for k in kids {
                            if matches!(k.kind, NodeKind::Leaf) {
                                let Some(note_ref) = k.note_ref.clone() else { continue };
                                let Some(emb) = embeddings_for_leaf(&note_ref) else {
                                    continue;
                                };
                                leaf_node_ids.push(k.id.clone());
                                leaf_note_refs.push(note_ref);
                                embeddings.push(emb);
                            } else {
                                stack.push(k.id.clone());
                            }
                        }
                    }
                }
            }
            (None, Some(inputs)) => {
                // Virtual-root: caller resolved the scope into NoteInputs.
                // No existing leaf node rows to reparent — the caller is
                // expected to wire any leaf-insert step separately (this
                // path is used by the build recipe, which is out of scope
                // for the Wave-A landing — only the op surface is here).
                for n in inputs {
                    leaf_note_refs.push(n.id.clone());
                    embeddings.push(n.embedding.clone());
                }
            }
            (None, None) => {
                return Err(TreesError::TreeNotFound(format!(
                    "split_cluster: virtual-root target requires virtual_root_inputs"
                )));
            }
        }

        if embeddings.len() < 4 {
            // Not enough members to split — fold them into the outcome
            // as outliers at this branch and stop the recursion. Top-level
            // user-driven splits already surface this as an error from
            // the Tauri command; recursive sub-splits silently stop.
            if !is_top_level {
                return Ok(());
            }
            return Err(TreesError::TreeNotFound(format!(
                "split_cluster: not enough members ({}) to split (need >= 4)",
                embeddings.len()
            )));
        }

        // 2. Partition. Use top_level_resolution on the top-level Leiden
        //    call per the build recipe; sub-recursive calls use the normal
        //    `resolution`.
        let assignments = match params.algorithm {
            crate::cluster::ClusterAlgorithm::Leiden => {
                let mut leiden_lvl = params.leiden.clone();
                if is_top_level {
                    leiden_lvl.resolution = params.leiden.top_level_resolution;
                }
                let upper = embeddings.len().saturating_sub(1).max(1) as u32;
                leiden_lvl.k_nearest = leiden_lvl.k_nearest.min(upper);
                crate::cluster::partition_leiden(&embeddings, &leiden_lvl).map_err(|e| {
                    TreesError::TreeNotFound(format!("split_cluster leiden: {e}"))
                })?
            }
            _ => {
                let min_size = (embeddings.len() / 4).max(2);
                crate::cluster::partition(&embeddings, min_size, None).map_err(|e| {
                    TreesError::TreeNotFound(format!("split_cluster hdbscan: {e}"))
                })?
            }
        };
        let mut groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        let mut branch_outliers: Vec<String> = Vec::new();
        for a in &assignments {
            if a.cluster_label == crate::cluster::OUTLIER_LABEL {
                branch_outliers.push(leaf_note_refs[a.point_index].clone());
                continue;
            }
            groups.entry(a.cluster_label).or_default().push(a.point_index);
        }
        outcome.outliers.extend(branch_outliers);

        if groups.len() < 2 {
            if !is_top_level {
                return Ok(());
            }
            return Err(TreesError::TreeNotFound(
                "split_cluster: produced fewer than 2 clusters".into(),
            ));
        }

        // 3. Insert new cluster rows + reparent leaves. Only the real-node
        //    target reparents existing leaf node rows — virtual-root mode
        //    just records the partition (used by the future build recipe).
        let parent_for_inserts = target_node_id.map(str::to_string);
        let mut new_cluster_ids_this_level: Vec<String> = Vec::new();
        let mut new_clusters_snapshot: Vec<serde_json::Value> = Vec::new();
        let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
        // Track per-new-cluster (id, member indices) for the recursion
        // step + the in-memory child shape for sub-split history.
        let mut new_children: Vec<(String, Vec<usize>, Vec<f32>, f32)> = Vec::new();

        let id_prefix = match target_node_id {
            Some(t) => format!("split-{}-d{}", t, depth),
            None => format!("split-root-d{}", depth),
        };

        for (label, idxs) in groups {
            let new_id = format!("{id_prefix}-{label}");
            // Compute centroid + radius from the member embeddings.
            let refs: Vec<&[f32]> = idxs.iter().map(|&i| embeddings[i].as_slice()).collect();
            let centroid = crate::cluster::mean_normalize(&refs);
            let radius = crate::cluster::ninetieth_percentile_distance(&centroid, &refs);
            // Placeholder name; the spec calls for LLM-named children via
            // a separate Summarize sweep, which is `cluster-op-summarize-sweep`.
            let name = format!("Cluster {}", label.max(0));
            let insert = NodeInsert {
                node_id: new_id.clone(),
                parent_id: parent_for_inserts.clone(),
                kind: NodeKind::Cluster,
                note_id: None,
                name: name.clone(),
                summary: String::new(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: Some(centroid.clone()),
                confidence: 0.5,
                summary_membership_churn: 0,
            };
            self.insert_single_node(tree_id, insert)?;
            // Only reparent existing leaf rows on real-node targets.
            if target_node_id.is_some() {
                for &i in &idxs {
                    leaf_moves.push((leaf_node_ids[i].clone(), Some(new_id.clone())));
                }
            }
            new_clusters_snapshot.push(serde_json::json!({
                "node_id": new_id,
                "parent_id": parent_for_inserts,
                "kind": "cluster",
                "name": name,
                "summary": "",
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": null,
                "confidence": 0.5,
                "summary_membership_churn": 0,
            }));
            new_cluster_ids_this_level.push(new_id.clone());
            new_children.push((new_id, idxs, centroid, radius));
        }

        if !leaf_moves.is_empty() {
            self.reparent_many(tree_id, &leaf_moves)?;
            // Freshly-inserted sub-clusters have summaries that describe
            // exactly the leaves they just received — so the churn that
            // `reparent_many` bumped on them is misleading. Zero it out;
            // future leaf moves into / out of these clusters will
            // accumulate real churn from a true baseline. Matches the
            // pre-ops-framework `cluster_op_split` semantics.
            for id in &new_cluster_ids_this_level {
                let _ = self.reset_churn(tree_id, id);
            }
        }

        // 4. Per-level history row. One row per recursion level so undo
        //    can step back through levels (per the spec).
        if let Some(parent_id) = target_node_id {
            self.record_split(tree_id, parent_id, &new_clusters_snapshot, &leaf_moves)?;
        } else {
            // Virtual-root split: no parent_id to anchor against; record
            // with a sentinel so the history row still exists for audit.
            // Undo of a virtual-root split isn't useful (the rows live on
            // a freshly-built tree that's still in `draft` state — the
            // user discards the tree, not the op).
            self.record_split(
                tree_id,
                "__virtual_root__",
                &new_clusters_snapshot,
                &leaf_moves,
            )?;
        }

        outcome
            .new_clusters
            .extend(new_cluster_ids_this_level.clone());
        outcome.total_levels = outcome.total_levels.max(depth + 1);

        // 5. Recursion. Stop conditions trip per branch independently.
        if params.recurse && depth + 1 < max_depth {
            for (new_id, idxs, _centroid, radius) in new_children {
                if idxs.len() <= params.leaf_min_size as usize {
                    continue;
                }
                if radius <= params.leaf_cohesion_threshold {
                    continue;
                }
                // Only real-node recursion is supported. Virtual-root
                // recursion needs additional plumbing (the children
                // didn't reparent any existing leaf rows, so a sub-split
                // against them has no leaves to walk through). The build
                // recipe will wire that case when it lands.
                if target_node_id.is_some() {
                    self.split_cluster_recursive(
                        ctx,
                        Some(&new_id),
                        None,
                        false,
                        depth + 1,
                        outcome,
                    )?;
                }
            }
        }

        Ok(())
    }
}
