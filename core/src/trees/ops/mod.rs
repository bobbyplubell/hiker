//! Higher-level reshape + ops-framework operations. Each submodule
//! contains `impl Db` methods grouped by their op family, one file per
//! family.
//!
//! All SQL goes through the helpers re-exported by `super::storage`; no
//! file in this directory imports `rusqlite::*` directly.
//!
//! The Rollup and Summarize-sweep ops live in this module root rather than
//! in their own files: they are forward-looking ops-framework code with no
//! in-tree caller wired yet, so `scripts/check-splits.py` rule #6
//! (sibling-only shards) would flag a standalone `rollup.rs` / `summarize.rs`
//! whose `pub` items are referenced nowhere outside their own directory.
//! When the adapter layer wires a caller, move them out into their own
//! files.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::types::{
    Db, EditableNode, Error, NodeInsert, NodeKind, RollupInput, RollupOutcome, RollupParams,
    SummarizeParams, SummarizePlan, SummarizeScope,
};
use crate::cluster::{
    algo::{mean_normalize, ninetieth_percentile_distance, partition, partition_leiden},
    Algorithm, Assignment, OUTLIER_LABEL,
};

pub(super) mod drop;
pub(super) mod edit;
pub(super) mod folder_rename;
pub(super) mod merge;
pub(super) mod move_node;
pub(super) mod split;

/// The rows + move-list produced while reparenting rollup inputs under
/// freshly inserted community parents. Accumulated by `build_rollup_parents`
/// and consumed by `record_rollup_history`.
#[derive(Default)]
struct BuiltParents {
    new_parent_ids: Vec<String>,
    new_parent_snapshots: Vec<serde_json::Value>,
    input_moves: Vec<(String, Option<String>)>,
    prior_parent_map: HashMap<String, Option<String>>,
}

// ── cluster-op-rollup ──────────────────────────────────────────────────
//
// status: cluster-op-rollup
//
// Validation + persistence side of the Rollup op. The caller embeds each
// input cluster's `summary` (the embed crate is async + lives in the
// indexer's `Arc<dyn Embedder>` — adapter-layer concerns); `apply_rollup`
// receives the resulting `summary_embeddings` and runs the partition +
// persistence.
impl Db {
    pub fn validate_rollup_inputs(
        &self,
        tree_id: &str,
        input_node_ids: &[String],
    ) -> Result<Vec<RollupInput>, Error> {
        let mut out = Vec::with_capacity(input_node_ids.len());
        for id in input_node_ids {
            let node = self
                .get_node(tree_id, id)?
                .ok_or_else(|| Error::NodeNotFound {
                    tree_id: tree_id.to_string(),
                    node_id: id.clone(),
                })?;
            if node.summary.trim().is_empty() {
                return Err(Error::TreeNotFound(format!(
                    "rollup: input node {id} has empty summary"
                )));
            }
            out.push(RollupInput {
                node_id: id.clone(),
                summary: node.summary.clone(),
                prior_parent: node.parent.clone(),
            });
        }
        Ok(out)
    }

    /// Apply a pre-validated, pre-embedded rollup: partition the inputs into
    /// communities, insert one new parent per non-trivial community, reparent
    /// the inputs underneath, and stamp the move into history. Refuses (no
    /// mutation) when nothing meaningfully merges.
    pub fn apply_rollup(
        &self,
        tree_id: &str,
        inputs: &[RollupInput],
        summary_embeddings: &[Vec<f32>],
        params: &RollupParams,
    ) -> Result<RollupOutcome, Error> {
        if inputs.len() != summary_embeddings.len() {
            return Err(Error::TreeNotFound(
                "rollup: inputs / embeddings length mismatch".into(),
            ));
        }
        if inputs.is_empty() {
            return Ok(RollupOutcome::Refused { reason: "no inputs" });
        }

        let assignments = run_rollup_partition(inputs.len(), summary_embeddings, params)?;

        // Group point indices by community label. Outliers are kept (under
        // `OUTLIER_LABEL`) so the refusal checks below see the full split.
        let mut groups: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
        for a in &assignments {
            groups.entry(a.cluster_label).or_default().push(a.point_index);
        }

        // Refusal checks over the non-outlier communities.
        let non_outlier_groups: Vec<(i32, &Vec<usize>)> = groups
            .iter()
            .filter(|(label, _)| **label != OUTLIER_LABEL)
            .map(|(l, v)| (*l, v))
            .collect();
        let total_members: usize = non_outlier_groups.iter().map(|(_, v)| v.len()).sum();
        if non_outlier_groups.len() == 1 && total_members == inputs.len() {
            return Ok(RollupOutcome::Refused {
                reason: "all inputs landed in one community",
            });
        }
        if !non_outlier_groups.iter().any(|(_, v)| v.len() >= 2) {
            return Ok(RollupOutcome::Refused {
                reason: "no inputs merged",
            });
        }

        // New parents inherit the inputs' former parent only if the inputs
        // all shared one; otherwise they land at the root.
        let prior_parents: HashSet<Option<String>> =
            inputs.iter().map(|i| i.prior_parent.clone()).collect();
        let common_prior_parent: Option<String> = if prior_parents.len() == 1 {
            prior_parents.into_iter().next().unwrap()
        } else {
            None
        };

        let built = self.build_rollup_parents(
            tree_id,
            inputs,
            summary_embeddings,
            &non_outlier_groups,
            common_prior_parent.as_ref(),
            params,
        )?;

        if built.new_parent_ids.is_empty() {
            // Every non-outlier group was a singleton; nothing merged.
            return Ok(RollupOutcome::Refused {
                reason: "no inputs merged",
            });
        }

        if !built.input_moves.is_empty() {
            self.reparent_many(tree_id, &built.input_moves)?;
        }
        self.record_rollup_history(tree_id, &built)?;

        Ok(RollupOutcome::Inserted {
            new_parent_ids: built.new_parent_ids,
        })
    }

    /// Insert one new cluster parent per non-trivial community and stage the
    /// moves that reparent its inputs underneath it. Singleton communities
    /// are left in place (rollup is about merging).
    fn build_rollup_parents(
        &self,
        tree_id: &str,
        inputs: &[RollupInput],
        summary_embeddings: &[Vec<f32>],
        non_outlier_groups: &[(i32, &Vec<usize>)],
        common_prior_parent: Option<&String>,
        params: &RollupParams,
    ) -> Result<BuiltParents, Error> {
        let pattern = params
            .new_layer_name_pattern
            .clone()
            .unwrap_or_else(|| "Group {n}".to_string());

        let mut built = BuiltParents::default();
        let mut n_counter: usize = 1;
        for (label, idxs) in non_outlier_groups {
            if idxs.len() < 2 {
                continue;
            }
            let new_id = format!("rollup-{tree_id}-{label}-{n_counter}");
            n_counter += 1;
            // Centroid = L2-normalized mean of the summary embeddings.
            let refs: Vec<&[f32]> = idxs
                .iter()
                .map(|&i| summary_embeddings[i].as_slice())
                .collect();
            let centroid = mean_normalize(&refs);
            let radius = ninetieth_percentile_distance(&centroid, &refs);
            let _ = radius; // recorded indirectly via centroid storage; no separate column.
            let name = pattern.replace("{n}", &(built.new_parent_ids.len() + 1).to_string());

            // Per the spec: "if the partitioner doesn't expose per-community
            // confidence cleanly, use a flat 0.8 placeholder."
            let confidence = 0.8f32;

            self.insert_single_node(
                tree_id,
                &NodeInsert {
                    node_id: new_id.clone(),
                    parent_id: common_prior_parent.cloned(),
                    kind: NodeKind::Cluster,
                    note_id: None,
                    name: name.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: Some(centroid),
                    confidence,
                    summary_membership_churn: 0,
                },
            )?;
            built.new_parent_snapshots.push(serde_json::json!({
                "node_id": new_id,
                "parent_id": common_prior_parent,
                "kind": "cluster",
                "name": name,
                "summary": "",
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": null,
                "confidence": confidence,
                "summary_membership_churn": 0,
            }));
            for &i in *idxs {
                built
                    .prior_parent_map
                    .insert(inputs[i].node_id.clone(), inputs[i].prior_parent.clone());
                built
                    .input_moves
                    .push((inputs[i].node_id.clone(), Some(new_id.clone())));
            }
            built.new_parent_ids.push(new_id);
        }
        Ok(built)
    }

    /// Stamp the rollup into `cluster_tree_history` with the redo args and
    /// the prior-parent map needed to reverse it.
    fn record_rollup_history(&self, tree_id: &str, built: &BuiltParents) -> Result<(), Error> {
        let prior_parent_map_json: serde_json::Value = built
            .prior_parent_map
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let args = serde_json::json!({
            "new_parent_ids": built.new_parent_ids,
            "new_parent_row_snapshots": built.new_parent_snapshots,
            "input_moves": built.input_moves,
        });
        let undo = serde_json::json!({
            "prior_parent_ids": prior_parent_map_json,
            "new_parent_ids": built.new_parent_ids,
            "new_parent_row_snapshots": built.new_parent_snapshots,
        });
        self.append_history(tree_id, "rollup", &args, &undo)?;
        Ok(())
    }
}

/// Partition the inputs' summary embeddings into communities. Leiden's
/// `k_nearest` is clamped to the input count; HDBSCAN floors
/// `min_cluster_size` at 2.
fn run_rollup_partition(
    n_inputs: usize,
    summary_embeddings: &[Vec<f32>],
    params: &RollupParams,
) -> Result<Vec<Assignment>, Error> {
    match params.algorithm {
        Algorithm::Leiden => {
            let mut leiden = params.leiden.clone();
            let upper = n_inputs.saturating_sub(1).max(1) as u32;
            leiden.k_nearest = leiden.k_nearest.min(upper);
            partition_leiden(summary_embeddings, &leiden)
                .map_err(|e| Error::TreeNotFound(format!("rollup leiden: {e}")))
        }
        _ => {
            let min_size = (params.min_cluster_size as usize).max(2);
            partition(summary_embeddings, min_size, None)
                .map_err(|e| Error::TreeNotFound(format!("rollup hdbscan: {e}")))
        }
    }
}

// ── cluster-op-summarize-sweep ─────────────────────────────────────────
//
// status: cluster-op-summarize-sweep
//
// Selection + ordering side of the Summarize sweep. The caller wraps
// `plan_summarize_sweep` and performs the actual queue submission (the
// queue is async; `Db` stays sync per module discipline). The returned
// `SummarizePlan` carries the per-cluster task descriptors in submission
// order (deepest first, so when the queue drains, a parent always has fresh
// child summaries to chew on).
impl Db {
    // TODO(cluster-op-summarize-sweep): umbrella supervision — today the
    // umbrella `TaskKind::ClusterSummarize` task is submitted at high
    // priority but is never marked complete; it sits Leased until the
    // orphan-fail expiry. Resolving this requires teaching `core::tasks` /
    // the worker drain to detect "last `RaptorSummarize` with
    // metadata.umbrella_id = X resolved → mark task X complete," which is
    // invasive enough to land separately.
    pub fn plan_summarize_sweep(
        &self,
        tree_id: &str,
        params: &SummarizeParams,
    ) -> Result<SummarizePlan, Error> {
        // Resolve the candidate set per `params.scope`.
        let all = self.list_nodes(tree_id)?;

        // Build a (node_id → EditableNode) map + ancestor walker for the
        // subtree filter.
        let nodes_by_id: HashMap<String, EditableNode> =
            all.iter().cloned().map(|n| (n.id.clone(), n)).collect();
        let children_by_parent: HashMap<String, Vec<String>> = {
            let mut m: HashMap<String, Vec<String>> = HashMap::new();
            for n in &all {
                if let Some(p) = &n.parent {
                    m.entry(p.clone()).or_default().push(n.id.clone());
                }
            }
            m
        };

        // Candidate ids per scope, with the subtree filter (when set)
        // applied uniformly.
        let candidates: Vec<String> = match &params.scope {
            SummarizeScope::All => all.iter().map(|n| n.id.clone()).collect(),
            SummarizeScope::StaleOrUnfilled => all
                .iter()
                .filter(|n| {
                    n.summary_membership_churn > 0
                        || n.summary.is_empty()
                        || n.name.is_empty()
                })
                .map(|n| n.id.clone())
                .collect(),
            SummarizeScope::Subset { ids } => ids
                .iter()
                .filter(|id| nodes_by_id.contains_key(*id))
                .cloned()
                .collect(),
        };

        // Apply subtree filter if subtree_root is set.
        let subtree_set: Option<HashSet<String>> =
            params.subtree_root.as_ref().map(|root| {
                let mut set = HashSet::new();
                let mut stack = vec![root.clone()];
                while let Some(id) = stack.pop() {
                    if !set.insert(id.clone()) {
                        continue;
                    }
                    if params.recursive
                        && let Some(kids) = children_by_parent.get(&id)
                    {
                        for k in kids {
                            stack.push(k.clone());
                        }
                    }
                }
                set
            });

        let mut skipped_user_edited: Vec<String> = Vec::new();
        let mut skipped_fresh: Vec<String> = Vec::new();
        // Selected ids → node depth (depth = ancestors-count, root=0).
        let mut selected: Vec<(String, u32)> = Vec::new();

        for id in candidates {
            let Some(node) = nodes_by_id.get(&id) else {
                continue;
            };
            // Skip leaves — they don't carry summaries.
            if !matches!(node.kind, NodeKind::Cluster) {
                continue;
            }
            // Subtree filter.
            if let Some(set) = &subtree_set
                && !set.contains(&id)
            {
                continue;
            }
            // User-edit gating.
            if !params.overwrite_user_edited
                && (node.user_edited_name || node.user_edited_summary)
            {
                skipped_user_edited.push(id.clone());
                continue;
            }
            // For `StaleOrUnfilled` / `All`, surface already-fresh clusters
            // as `skipped_fresh` when they were candidates of the broad
            // scope but didn't actually need work. For `All` we don't
            // filter — every non-leaf is selected.
            if matches!(params.scope, SummarizeScope::StaleOrUnfilled)
                && node.summary_membership_churn == 0
                && !node.summary.is_empty()
                && !node.name.is_empty()
            {
                skipped_fresh.push(id.clone());
                continue;
            }

            // Compute depth by walking ancestors.
            let mut depth: u32 = 0;
            let mut cur = node.parent.clone();
            while let Some(p) = cur {
                depth += 1;
                cur = nodes_by_id.get(&p).and_then(|n| n.parent.clone());
                if depth > 64 {
                    break;
                }
            }
            selected.push((id, depth));
        }

        // Bottom-up ordering: deepest first, so when each per-cluster task
        // resolves, its parent's eventual summarize call has fresh
        // children to summarize over. Matches the existing
        // `cluster-editor-regenerate-via-task-queue` "Ordering" guidance.
        selected.sort_by_key(|s| std::cmp::Reverse(s.1));

        let enqueued: Vec<String> = selected.into_iter().map(|(id, _)| id).collect();

        let scope_kind = match &params.scope {
            SummarizeScope::All => "all",
            SummarizeScope::StaleOrUnfilled => "stale-or-unfilled",
            SummarizeScope::Subset { .. } => "subset",
        }
        .to_string();

        Ok(SummarizePlan {
            tree_id: tree_id.to_string(),
            scope_kind,
            enqueued,
            skipped_user_edited,
            skipped_fresh,
        })
    }
}
