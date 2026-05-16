//! `merge_siblings` and `merge_children_up` reshape ops.

use super::super::storage::params;
use super::super::types::{EditableNode, Trees, TreesError};

impl Trees {
    /// Merge a set of sibling clusters into one. The first id in `node_ids`
    /// is kept as the survivor; every other listed node's children are
    /// re-parented under the survivor, then the absorbed nodes are
    /// deleted. Records a `merge-siblings` history row.
    ///
    /// Errors if any node is missing, if they don't share a parent, or
    /// if fewer than two ids are passed.
    ///
    /// status: cluster-editor-merge-siblings
    pub fn merge_siblings(
        &self,
        tree_id: &str,
        node_ids: &[String],
    ) -> Result<String, TreesError> {
        if node_ids.len() < 2 {
            return Err(TreesError::TreeNotFound(format!(
                "merge_siblings needs >=2 ids, got {}",
                node_ids.len()
            )));
        }
        let nodes: Vec<EditableNode> = node_ids
            .iter()
            .map(|id| {
                self.get_node(tree_id, id)?.ok_or_else(|| TreesError::NodeNotFound {
                    tree_id: tree_id.to_string(),
                    node_id: id.clone(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let parent = nodes[0].parent.clone();
        for n in &nodes[1..] {
            if n.parent != parent {
                return Err(TreesError::TreeNotFound(format!(
                    "merge_siblings: node {} has different parent",
                    n.id
                )));
            }
        }
        let survivor = node_ids[0].clone();
        let absorbed: Vec<String> = node_ids[1..].to_vec();
        // Snapshot the absorbed nodes so undo can restore them. Children
        // re-parent is reversible by recording each child's prior parent.
        let mut absorbed_snapshots: Vec<serde_json::Value> = Vec::new();
        let mut child_moves: Vec<serde_json::Value> = Vec::new();
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            for abs_id in &absorbed {
                // Snapshot full row.
                let snap = tx
                    .query_row(
                        "SELECT parent_id, kind, note_id, name, summary, user_edited_name,
                                user_edited_summary, policy, centroid, confidence, summary_membership_churn
                         FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                        params![tree_id, abs_id],
                        |row| {
                            Ok(serde_json::json!({
                                "parent_id": row.get::<_, Option<String>>(0)?,
                                "kind": row.get::<_, String>(1)?,
                                "note_id": row.get::<_, Option<String>>(2)?,
                                "name": row.get::<_, String>(3)?,
                                "summary": row.get::<_, String>(4)?,
                                "user_edited_name": row.get::<_, i32>(5)? != 0,
                                "user_edited_summary": row.get::<_, i32>(6)? != 0,
                                "policy": row.get::<_, Option<String>>(7)?,
                                "confidence": row.get::<_, f64>(9)?,
                                "summary_membership_churn": row.get::<_, i64>(10)?,
                            }))
                        },
                    )?;
                absorbed_snapshots.push(serde_json::json!({ "id": abs_id, "row": snap }));
                // Re-parent children under the survivor.
                let mut child_stmt = tx.prepare(
                    "SELECT node_id FROM cluster_nodes WHERE tree_id = ?1 AND parent_id = ?2",
                )?;
                let child_ids: Vec<String> = child_stmt
                    .query_map(params![tree_id, abs_id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(child_stmt);
                for cid in child_ids {
                    child_moves.push(serde_json::json!({
                        "child_id": cid,
                        "from": abs_id,
                        "to": survivor,
                    }));
                    tx.execute(
                        "UPDATE cluster_nodes SET parent_id = ?1
                         WHERE tree_id = ?2 AND node_id = ?3",
                        params![survivor, tree_id, cid],
                    )?;
                }
                tx.execute(
                    "DELETE FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, abs_id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        let args = serde_json::json!({
            "survivor": survivor,
            "absorbed": absorbed,
        });
        let undo = serde_json::json!({
            "survivor": survivor,
            "absorbed": absorbed_snapshots,
            "child_moves": child_moves,
        });
        self.append_history(tree_id, "merge-siblings", &args, &undo)?;
        // status: cluster-summary-staleness-counter — the survivor absorbed
        // every absorbed cluster's children, which is a real membership
        // change on the survivor's chain.
        let _ = self.bump_churn_chain(tree_id, &survivor, 1);
        Ok(survivor)
    }

    /// Flatten one level. Each child of `parent_id` that is itself a
    /// cluster has its children re-parented onto `parent_id`, then the
    /// emptied cluster nodes are deleted. Leaf children of `parent_id`
    /// stay put.
    ///
    /// status: cluster-editor-merge-children-up
    pub fn merge_children_up(&self, tree_id: &str, parent_id: &str) -> Result<(), TreesError> {
        // Snapshot so we can undo: each absorbed cluster + each
        // grand-child's prior parent.
        let mut absorbed_snapshots: Vec<serde_json::Value> = Vec::new();
        let mut grandchild_moves: Vec<serde_json::Value> = Vec::new();
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let mut child_stmt = tx.prepare(
                "SELECT node_id, kind FROM cluster_nodes WHERE tree_id = ?1 AND parent_id = ?2",
            )?;
            let children: Vec<(String, String)> = child_stmt
                .query_map(params![tree_id, parent_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(child_stmt);
            for (cid, kind) in &children {
                if kind != "cluster" {
                    continue;
                }
                let snap = tx
                    .query_row(
                        "SELECT parent_id, kind, note_id, name, summary, user_edited_name,
                                user_edited_summary, policy, confidence, summary_membership_churn
                         FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                        params![tree_id, cid],
                        |row| {
                            Ok(serde_json::json!({
                                "parent_id": row.get::<_, Option<String>>(0)?,
                                "kind": row.get::<_, String>(1)?,
                                "note_id": row.get::<_, Option<String>>(2)?,
                                "name": row.get::<_, String>(3)?,
                                "summary": row.get::<_, String>(4)?,
                                "user_edited_name": row.get::<_, i32>(5)? != 0,
                                "user_edited_summary": row.get::<_, i32>(6)? != 0,
                                "policy": row.get::<_, Option<String>>(7)?,
                                "confidence": row.get::<_, f64>(8)?,
                                "summary_membership_churn": row.get::<_, i64>(9)?,
                            }))
                        },
                    )?;
                absorbed_snapshots.push(serde_json::json!({ "id": cid, "row": snap }));
                let mut gc_stmt = tx.prepare(
                    "SELECT node_id FROM cluster_nodes WHERE tree_id = ?1 AND parent_id = ?2",
                )?;
                let gcs: Vec<String> = gc_stmt
                    .query_map(params![tree_id, cid], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(gc_stmt);
                for gc in gcs {
                    grandchild_moves.push(serde_json::json!({
                        "child_id": gc,
                        "from": cid,
                        "to": parent_id,
                    }));
                    tx.execute(
                        "UPDATE cluster_nodes SET parent_id = ?1
                         WHERE tree_id = ?2 AND node_id = ?3",
                        params![parent_id, tree_id, gc],
                    )?;
                }
                tx.execute(
                    "DELETE FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, cid],
                )?;
            }
            tx.commit()?;
            Ok(())
        })?;
        let args = serde_json::json!({ "parent_id": parent_id });
        let undo = serde_json::json!({
            "parent_id": parent_id,
            "absorbed": absorbed_snapshots,
            "grandchild_moves": grandchild_moves,
        });
        self.append_history(tree_id, "merge-children-up", &args, &undo)?;
        // status: cluster-summary-staleness-counter — the parent absorbed
        // grandchildren; bump its chain.
        let _ = self.bump_churn_chain(tree_id, parent_id, 1);
        Ok(())
    }
}
