//! cluster-op-rollup (`Trees::validate_rollup_inputs`, `Trees::apply_rollup`).

use super::super::types::{
    NodeInsert, NodeKind, RollupInput, RollupOutcome, RollupParams, Trees, TreesError,
};

impl Trees {
    // ── cluster-op-rollup ──────────────────────────────────────────────
    //
    // status: cluster-op-rollup
    //
    // Validation + persistence side of the Rollup op. The caller is
    // responsible for embedding each input cluster's `summary` (the embed
    // crate is async + lives in the indexer's `Arc<dyn Embedder>` —
    // adapter-layer concerns); `apply_rollup` receives the resulting
    // `summary_embeddings` and runs the partition + persistence.
    pub fn validate_rollup_inputs(
        &self,
        tree_id: &str,
        input_node_ids: &[String],
    ) -> Result<Vec<RollupInput>, TreesError> {
        let mut out = Vec::with_capacity(input_node_ids.len());
        for id in input_node_ids {
            let node = self
                .get_node(tree_id, id)?
                .ok_or_else(|| TreesError::NodeNotFound {
                    tree_id: tree_id.to_string(),
                    node_id: id.clone(),
                })?;
            if node.summary.trim().is_empty() {
                return Err(TreesError::TreeNotFound(format!(
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

    /// Apply a pre-validated, pre-embedded rollup. The partition step is
    /// inlined here so the persistence has direct access to the resulting
    /// communities; the embedding (and the embedder hot-reload concern)
    /// stays in the adapter layer.
    pub fn apply_rollup(
        &self,
        tree_id: &str,
        inputs: &[RollupInput],
        summary_embeddings: &[Vec<f32>],
        params: &RollupParams,
    ) -> Result<RollupOutcome, TreesError> {
        if inputs.len() != summary_embeddings.len() {
            return Err(TreesError::TreeNotFound(
                "rollup: inputs / embeddings length mismatch".into(),
            ));
        }
        if inputs.is_empty() {
            return Ok(RollupOutcome::Refused {
                reason: "no inputs",
            });
        }
        // Run the partition over the summary embeddings.
        let assignments = match params.algorithm {
            crate::cluster::ClusterAlgorithm::Leiden => {
                let mut leiden = params.leiden.clone();
                let upper = inputs.len().saturating_sub(1).max(1) as u32;
                leiden.k_nearest = leiden.k_nearest.min(upper);
                crate::cluster::partition_leiden(summary_embeddings, &leiden).map_err(|e| {
                    TreesError::TreeNotFound(format!("rollup leiden: {e}"))
                })?
            }
            _ => {
                let min_size = (params.min_cluster_size as usize).max(2);
                crate::cluster::partition(summary_embeddings, min_size, None).map_err(|e| {
                    TreesError::TreeNotFound(format!("rollup hdbscan: {e}"))
                })?
            }
        };
        let mut groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for a in &assignments {
            if a.cluster_label == crate::cluster::OUTLIER_LABEL {
                // Outliers stay where they are. Effectively their own
                // group; tracked separately so the refusal check below
                // sees them.
                groups.entry(a.cluster_label).or_default().push(a.point_index);
                continue;
            }
            groups.entry(a.cluster_label).or_default().push(a.point_index);
        }

        // Refusal checks.
        let non_outlier_groups: Vec<(i32, &Vec<usize>)> = groups
            .iter()
            .filter(|(label, _)| **label != crate::cluster::OUTLIER_LABEL)
            .map(|(l, v)| (*l, v))
            .collect();
        let total_members: usize = non_outlier_groups.iter().map(|(_, v)| v.len()).sum();
        if non_outlier_groups.len() == 1 && total_members == inputs.len() {
            return Ok(RollupOutcome::Refused {
                reason: "all inputs landed in one community",
            });
        }
        let any_merged = non_outlier_groups.iter().any(|(_, v)| v.len() >= 2);
        if !any_merged {
            return Ok(RollupOutcome::Refused {
                reason: "no inputs merged",
            });
        }

        // Determine the new parents' common prior parent (the inputs'
        // former parent if they shared one, else None).
        let prior_parents: std::collections::HashSet<Option<String>> =
            inputs.iter().map(|i| i.prior_parent.clone()).collect();
        let common_prior_parent: Option<String> = if prior_parents.len() == 1 {
            prior_parents.into_iter().next().unwrap()
        } else {
            None
        };

        // Build the new parent row per non-trivial community and reparent.
        let pattern = params
            .new_layer_name_pattern
            .clone()
            .unwrap_or_else(|| "Group {n}".to_string());

        let mut new_parent_ids: Vec<String> = Vec::new();
        let mut new_parent_snapshots: Vec<serde_json::Value> = Vec::new();
        let mut input_moves: Vec<(String, Option<String>)> = Vec::new();
        let mut prior_parent_map: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        let mut n_counter: usize = 1;
        for (label, idxs) in &non_outlier_groups {
            // Singletons stay where they are (rollup is about merging).
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
            let centroid = crate::cluster::mean_normalize(&refs);
            let radius = crate::cluster::ninetieth_percentile_distance(&centroid, &refs);
            let _ = radius; // recorded indirectly via centroid storage; no separate column.
            let name = pattern.replace("{n}", &(new_parent_ids.len() + 1).to_string());

            // Per the spec: "if the partitioner doesn't expose
            // per-community confidence cleanly, use a flat 0.8 placeholder."
            let confidence = 0.8f32;

            self.insert_single_node(
                tree_id,
                NodeInsert {
                    node_id: new_id.clone(),
                    parent_id: common_prior_parent.clone(),
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
            new_parent_snapshots.push(serde_json::json!({
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
                prior_parent_map.insert(inputs[i].node_id.clone(), inputs[i].prior_parent.clone());
                input_moves.push((inputs[i].node_id.clone(), Some(new_id.clone())));
            }
            new_parent_ids.push(new_id);
        }

        if new_parent_ids.is_empty() {
            // Every non-outlier group was a singleton; refuse.
            return Ok(RollupOutcome::Refused {
                reason: "no inputs merged",
            });
        }

        if !input_moves.is_empty() {
            self.reparent_many(tree_id, &input_moves)?;
        }

        // History row.
        let prior_parent_map_json: serde_json::Value = prior_parent_map
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let args = serde_json::json!({
            "new_parent_ids": new_parent_ids,
            "new_parent_row_snapshots": new_parent_snapshots,
            "input_moves": input_moves,
        });
        let undo = serde_json::json!({
            "prior_parent_ids": prior_parent_map_json,
            "new_parent_ids": new_parent_ids,
            "new_parent_row_snapshots": new_parent_snapshots,
        });
        self.append_history(tree_id, "rollup", &args, &undo)?;

        Ok(RollupOutcome::Inserted { new_parent_ids })
    }
}
