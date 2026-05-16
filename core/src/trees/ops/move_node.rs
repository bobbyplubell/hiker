//! `move_node`, `reparent_many`, `promote_outlier` — the ops that
//! relocate one or more nodes to new parents.

use super::super::storage::{params, OptionalExtension};
use super::super::types::{Trees, TreesError};

impl Trees {
    /// Move a node to a new parent (or to the root when `new_parent` is
    /// `None`). Appends a `move` history entry.
    pub fn move_node(
        &self,
        tree_id: &str,
        node_id: &str,
        new_parent: Option<&str>,
    ) -> Result<(), TreesError> {
        let prior = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            TreesError::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes
                 SET parent_id = ?1
                 WHERE tree_id = ?2 AND node_id = ?3",
                params![new_parent, tree_id, node_id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({ "node_id": node_id, "parent_id": new_parent });
        let undo = serde_json::json!({ "node_id": node_id, "parent_id": prior.parent });
        self.append_history(tree_id, "move", &args, &undo)?;
        // status: cluster-summary-staleness-counter
        // Bump churn on the source and destination chains up to (but not
        // including) the LCA of old_parent and new_parent. The LCA and
        // its ancestors contain the moved node both before and after, so
        // their subtree leaf sets are unchanged and their summaries do
        // not become stale from the move.
        let new_parent_ancestors = match new_parent {
            Some(np) => self
                .ancestors_inclusive(tree_id, np)
                .unwrap_or_default(),
            None => std::collections::HashSet::new(),
        };
        let old_parent_ancestors = match prior.parent.as_deref() {
            Some(p) => self
                .ancestors_inclusive(tree_id, p)
                .unwrap_or_default(),
            None => std::collections::HashSet::new(),
        };
        if let Some(prev_parent) = prior.parent.as_deref() {
            let _ = self.bump_churn_chain_until(tree_id, prev_parent, &new_parent_ancestors, 1);
        }
        if let Some(np) = new_parent {
            let _ = self.bump_churn_chain_until(tree_id, np, &old_parent_ancestors, 1);
        }
        Ok(())
    }

    /// Re-parent a batch of nodes onto new parents. Used by split to
    /// move existing leaves under their new sub-cluster homes. Doesn't
    /// write history — the wrapping op does.
    ///
    /// status: cluster-summary-staleness-counter
    /// Bumps churn on each moved node's prior-parent chain and on each
    /// destination chain, mirroring `move_node`. Split (the main caller)
    /// relocates every leaf under a former cluster onto new sub-clusters,
    /// and the spec ("Every leaf insert or remove within a cluster's
    /// subtree increments the counter on that cluster and all its
    /// ancestors") covers the case — bumping here ensures the wrapping
    /// `cluster_op_split` doesn't need to repeat the walk, and any future
    /// caller is covered automatically.
    pub fn reparent_many(
        &self,
        tree_id: &str,
        moves: &[(String, Option<String>)],
    ) -> Result<(), TreesError> {
        // Capture prior parents before mutating so we can bump the source
        // chain after the move. Read in a short-lived scope to avoid
        // holding the mutex across the bump_churn_chain calls below
        // (which re-acquire it).
        let prior_parents: Vec<Option<String>> = self.with_conn(|conn| {
            let mut out = Vec::with_capacity(moves.len());
            for (id, _) in moves {
                let p: Option<Option<String>> = conn
                    .query_row(
                        "SELECT parent_id FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                        params![tree_id, id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?;
                out.push(p.unwrap_or(None));
            }
            Ok(out)
        })?;
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            for (id, parent) in moves {
                tx.execute(
                    "UPDATE cluster_nodes SET parent_id = ?1 WHERE tree_id = ?2 AND node_id = ?3",
                    params![parent, tree_id, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        // Bump churn on both ends of each move, stopping at the LCA so
        // common ancestors (whose subtree leaf set didn't change) don't
        // accumulate spurious staleness. Same shape as `move_node`.
        for ((_, new_parent), prev) in moves.iter().zip(prior_parents.iter()) {
            let new_parent_ancestors = match new_parent.as_deref() {
                Some(np) => self
                    .ancestors_inclusive(tree_id, np)
                    .unwrap_or_default(),
                None => std::collections::HashSet::new(),
            };
            let old_parent_ancestors = match prev.as_deref() {
                Some(p) => self
                    .ancestors_inclusive(tree_id, p)
                    .unwrap_or_default(),
                None => std::collections::HashSet::new(),
            };
            if let Some(prev_parent) = prev.as_deref() {
                let _ = self.bump_churn_chain_until(
                    tree_id,
                    prev_parent,
                    &new_parent_ancestors,
                    1,
                );
            }
            if let Some(np) = new_parent.as_deref() {
                let _ = self.bump_churn_chain_until(
                    tree_id,
                    np,
                    &old_parent_ancestors,
                    1,
                );
            }
        }
        Ok(())
    }

    /// Promote an outlier (leaf inside an outlier-bucket) to a regular
    /// cluster, or demote a regular leaf into an outlier bucket. Wraps
    /// `move_node` but records a distinct `promote-outlier` op so the
    /// history reads naturally. Caller passes the destination parent.
    ///
    /// status: cluster-editor-promote-outlier
    pub fn promote_outlier(
        &self,
        tree_id: &str,
        leaf_id: &str,
        new_parent: Option<&str>,
    ) -> Result<(), TreesError> {
        let prior = self.get_node(tree_id, leaf_id)?.ok_or_else(|| {
            TreesError::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: leaf_id.to_string(),
            }
        })?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes SET parent_id = ?1 WHERE tree_id = ?2 AND node_id = ?3",
                params![new_parent, tree_id, leaf_id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({ "leaf_id": leaf_id, "parent_id": new_parent });
        let undo = serde_json::json!({ "leaf_id": leaf_id, "parent_id": prior.parent });
        self.append_history(tree_id, "promote-outlier", &args, &undo)?;
        // status: cluster-summary-staleness-counter
        if let Some(prev_parent) = prior.parent.as_deref() {
            let _ = self.bump_churn_chain(tree_id, prev_parent, 1);
        }
        if let Some(np) = new_parent {
            let _ = self.bump_churn_chain(tree_id, np, 1);
        }
        Ok(())
    }
}
