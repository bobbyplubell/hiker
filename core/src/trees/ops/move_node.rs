//! `move_node`, `reparent_many`, `promote_outlier` — the ops that relocate
//! one or more nodes to new parents. Each is a single `SetFrontmatter` write
//! (`Db::mutate`); churn is bumped in-memory on the loaded tree.

use std::collections::HashSet;

use super::super::types::{Db, Error};

impl Db {
    /// Move a node to a new parent (or to the root when `new_parent` is
    /// `None`). Records a `move` history entry.
    pub fn move_node(
        &self,
        tree_id: &str,
        node_id: &str,
        new_parent: Option<&str>,
    ) -> Result<(), Error> {
        let prior_parent = self.mutate(tree_id, |doc| {
            let prior_parent = doc
                .get(node_id)
                .ok_or_else(|| Error::NodeNotFound {
                    tree_id: tree_id.to_string(),
                    node_id: node_id.to_string(),
                })?
                .parent
                .clone();
            doc.set_parent(node_id, new_parent);
            // status: cluster-summary-staleness-counter
            // Bump churn on the source and destination chains up to (but not
            // including) the LCA of old_parent and new_parent — those common
            // ancestors' subtree leaf sets are unchanged by the move.
            let new_parent_ancestors = match new_parent {
                Some(np) => doc.ancestors_inclusive(np),
                None => HashSet::new(),
            };
            let old_parent_ancestors = match prior_parent.as_deref() {
                Some(p) => doc.ancestors_inclusive(p),
                None => HashSet::new(),
            };
            if let Some(prev_parent) = prior_parent.as_deref() {
                doc.bump_churn_until(prev_parent, &new_parent_ancestors, 1);
            }
            if let Some(np) = new_parent {
                doc.bump_churn_until(np, &old_parent_ancestors, 1);
            }
            Ok(prior_parent)
        })?;
        let args = serde_json::json!({ "node_id": node_id, "parent_id": new_parent });
        let undo = serde_json::json!({ "node_id": node_id, "parent_id": prior_parent });
        self.append_history(tree_id, "move", &args, &undo)?;
        Ok(())
    }

    /// Re-parent a batch of nodes onto new parents. Used by split to move
    /// existing leaves under their new sub-cluster homes. Doesn't record
    /// history — the wrapping op does. Bumps churn on each moved node's
    /// prior-parent and destination chains (LCA-aware), mirroring `move_node`.
    ///
    /// status: cluster-summary-staleness-counter
    pub fn reparent_many(
        &self,
        tree_id: &str,
        moves: &[(String, Option<String>)],
    ) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            for (id, new_parent) in moves {
                let prior_parent = doc.get(id).and_then(|n| n.parent.clone());
                doc.set_parent(id, new_parent.as_deref());
                let new_parent_ancestors = match new_parent.as_deref() {
                    Some(np) => doc.ancestors_inclusive(np),
                    None => HashSet::new(),
                };
                let old_parent_ancestors = match prior_parent.as_deref() {
                    Some(p) => doc.ancestors_inclusive(p),
                    None => HashSet::new(),
                };
                if let Some(prev_parent) = prior_parent.as_deref() {
                    doc.bump_churn_until(prev_parent, &new_parent_ancestors, 1);
                }
                if let Some(np) = new_parent.as_deref() {
                    doc.bump_churn_until(np, &old_parent_ancestors, 1);
                }
            }
            Ok(())
        })
    }

    /// Promote an outlier (leaf inside an outlier-bucket) to a regular
    /// cluster, or demote a regular leaf into an outlier bucket. Records a
    /// distinct `promote-outlier` op so the history reads naturally.
    ///
    /// status: cluster-editor-promote-outlier
    pub fn promote_outlier(
        &self,
        tree_id: &str,
        leaf_id: &str,
        new_parent: Option<&str>,
    ) -> Result<(), Error> {
        let prior_parent = self.mutate(tree_id, |doc| {
            let prior_parent = doc
                .get(leaf_id)
                .ok_or_else(|| Error::NodeNotFound {
                    tree_id: tree_id.to_string(),
                    node_id: leaf_id.to_string(),
                })?
                .parent
                .clone();
            doc.set_parent(leaf_id, new_parent);
            // status: cluster-summary-staleness-counter
            if let Some(prev_parent) = prior_parent.as_deref() {
                doc.bump_churn(prev_parent, 1);
            }
            if let Some(np) = new_parent {
                doc.bump_churn(np, 1);
            }
            Ok(prior_parent)
        })?;
        let args = serde_json::json!({ "leaf_id": leaf_id, "parent_id": new_parent });
        let undo = serde_json::json!({ "leaf_id": leaf_id, "parent_id": prior_parent });
        self.append_history(tree_id, "promote-outlier", &args, &undo)?;
        Ok(())
    }
}
