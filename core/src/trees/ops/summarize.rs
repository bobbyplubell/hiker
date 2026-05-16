//! cluster-op-summarize-sweep (`Trees::plan_summarize_sweep`).
//!
//! TODO(cluster-op-summarize-sweep): umbrella supervision — today the
//! umbrella `TaskKind::ClusterSummarize` task is submitted at high
//! priority but is never marked complete; it sits Leased until the
//! orphan-fail expiry. Resolving this requires teaching `core::tasks` /
//! the worker drain to detect "last `RaptorSummarize` with
//! metadata.umbrella_id = X resolved → mark task X complete," which is
//! invasive enough to land separately.

use super::super::types::{
    EditableNode, NodeKind, SummarizeParams, SummarizePlan, SummarizeScope, Trees, TreesError,
};

impl Trees {
    // ── cluster-op-summarize-sweep ─────────────────────────────────────
    //
    // status: cluster-op-summarize-sweep
    //
    // Selection + ordering side of the Summarize sweep. The Tauri command
    // wraps this method and performs the actual queue submission (the
    // queue is async; Trees stays sync per module discipline). Returns a
    // `SummarizePlan` carrying the umbrella task descriptor + per-cluster
    // task descriptors in **submission order** (deepest first, so when
    // the queue drains, the parent always has fresh child summaries to
    // chew on).
    pub fn plan_summarize_sweep(
        &self,
        tree_id: &str,
        params: &SummarizeParams,
    ) -> Result<SummarizePlan, TreesError> {
        // Resolve the candidate set per `params.scope`.
        let all = self.list_nodes(tree_id)?;

        // Build a (node_id → EditableNode) map + ancestor walker for the
        // subtree filter.
        let nodes_by_id: std::collections::HashMap<String, EditableNode> =
            all.iter().cloned().map(|n| (n.id.clone(), n)).collect();
        let children_by_parent: std::collections::HashMap<String, Vec<String>> = {
            let mut m: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
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
        let subtree_set: Option<std::collections::HashSet<String>> =
            params.subtree_root.as_ref().map(|root| {
                let mut set = std::collections::HashSet::new();
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
        selected.sort_by(|a, b| b.1.cmp(&a.1));

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
