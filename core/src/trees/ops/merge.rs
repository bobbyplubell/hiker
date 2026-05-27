//! `merge_siblings` and `merge_children_up` reshape ops. Each is one
//! `SetFrontmatter` write (`Db::mutate`) plus a session-history entry whose
//! undo_args snapshot the absorbed nodes + the child re-parents.

use super::super::store::snapshot_full;
use super::super::types::{Db, Error, NodeKind};

impl Db {
    /// Merge a set of sibling clusters into one. The first id in `node_ids`
    /// is kept as the survivor; every other listed node's children are
    /// re-parented under the survivor, then the absorbed nodes are deleted.
    /// Records a `merge-siblings` history row.
    ///
    /// status: cluster-editor-merge-siblings
    pub fn merge_siblings(&self, tree_id: &str, node_ids: &[String]) -> Result<String, Error> {
        if node_ids.len() < 2 {
            return Err(Error::TreeNotFound(format!(
                "merge_siblings needs >=2 ids, got {}",
                node_ids.len()
            )));
        }
        let survivor = node_ids[0].clone();
        let absorbed: Vec<String> = node_ids[1..].to_vec();
        let (absorbed_snapshots, child_moves) = self.mutate(tree_id, |doc| {
            // Validate existence + shared parent.
            let nodes: Vec<_> = node_ids
                .iter()
                .map(|id| {
                    doc.get(id).cloned().ok_or_else(|| Error::NodeNotFound {
                        tree_id: tree_id.to_string(),
                        node_id: id.clone(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let parent = nodes[0].parent.clone();
            for n in &nodes[1..] {
                if n.parent != parent {
                    return Err(Error::TreeNotFound(format!(
                        "merge_siblings: node {} has different parent",
                        n.id
                    )));
                }
            }
            let mut absorbed_snapshots: Vec<serde_json::Value> = Vec::new();
            let mut child_moves: Vec<serde_json::Value> = Vec::new();
            for abs_id in &absorbed {
                if let Some(node) = doc.get(abs_id).cloned() {
                    absorbed_snapshots
                        .push(serde_json::json!({ "id": abs_id, "row": snapshot_full(&node) }));
                }
                for cid in doc.child_ids(abs_id) {
                    child_moves.push(serde_json::json!({
                        "child_id": cid, "from": abs_id, "to": survivor,
                    }));
                    doc.set_parent(&cid, Some(&survivor));
                }
                doc.remove(abs_id);
            }
            // status: cluster-summary-staleness-counter — the survivor
            // absorbed every absorbed cluster's children.
            doc.bump_churn(&survivor, 1);
            Ok((absorbed_snapshots, child_moves))
        })?;
        let args = serde_json::json!({ "survivor": survivor, "absorbed": absorbed });
        let undo = serde_json::json!({
            "survivor": survivor,
            "absorbed": absorbed_snapshots,
            "child_moves": child_moves,
        });
        self.append_history(tree_id, "merge-siblings", &args, &undo)?;
        Ok(survivor)
    }

    /// Flatten one level. Each child of `parent_id` that is itself a cluster
    /// has its children re-parented onto `parent_id`, then the emptied
    /// cluster nodes are deleted. Leaf children of `parent_id` stay put.
    ///
    /// status: cluster-editor-merge-children-up
    pub fn merge_children_up(&self, tree_id: &str, parent_id: &str) -> Result<(), Error> {
        let (absorbed_snapshots, grandchild_moves) = self.mutate(tree_id, |doc| {
            let mut absorbed_snapshots: Vec<serde_json::Value> = Vec::new();
            let mut grandchild_moves: Vec<serde_json::Value> = Vec::new();
            let children = doc.children(Some(parent_id));
            for child in &children {
                if !matches!(child.kind, NodeKind::Cluster) {
                    continue;
                }
                absorbed_snapshots
                    .push(serde_json::json!({ "id": child.id, "row": snapshot_full(child) }));
                for gc in doc.child_ids(&child.id) {
                    grandchild_moves.push(serde_json::json!({
                        "child_id": gc, "from": child.id, "to": parent_id,
                    }));
                    doc.set_parent(&gc, Some(parent_id));
                }
                doc.remove(&child.id);
            }
            // status: cluster-summary-staleness-counter
            doc.bump_churn(parent_id, 1);
            Ok((absorbed_snapshots, grandchild_moves))
        })?;
        let args = serde_json::json!({ "parent_id": parent_id });
        let undo = serde_json::json!({
            "parent_id": parent_id,
            "absorbed": absorbed_snapshots,
            "grandchild_moves": grandchild_moves,
        });
        self.append_history(tree_id, "merge-children-up", &args, &undo)?;
        Ok(())
    }
}
