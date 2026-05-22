//! `drop_cluster` — collapses a cluster subtree into the nearest outlier
//! bucket and records the inverse for undo.

use super::super::storage::params;
use super::super::types::{EditableNode, NodeKind, Db, Error};

impl Db {
    /// Drop a cluster: its leaf descendants are re-parented under the
    /// nearest outlier bucket; nested cluster nodes are deleted along
    /// with the dropped cluster. Records a `drop-cluster` history row.
    /// `outlier_bucket_id` is the destination — caller (UI) picks it
    /// from the tree's outlier nodes (or creates one if missing).
    ///
    /// status: cluster-editor-drop-cluster
    pub fn drop_cluster(
        &self,
        tree_id: &str,
        node_id: &str,
        outlier_bucket_id: &str,
    ) -> Result<(), Error> {
        // Collect every descendant (DFS) so we know which leaves to
        // re-parent and which clusters to delete.
        let all_nodes = self.list_nodes(tree_id)?;
        let mut children_by_parent: std::collections::HashMap<String, Vec<EditableNode>> =
            std::collections::HashMap::new();
        for n in all_nodes.iter().cloned() {
            if let Some(p) = n.parent.clone() {
                children_by_parent.entry(p).or_default().push(n);
            }
        }
        let mut to_visit: Vec<String> = vec![node_id.to_string()];
        let mut cluster_descendants: Vec<EditableNode> = Vec::new();
        let mut leaf_descendants: Vec<EditableNode> = Vec::new();
        while let Some(id) = to_visit.pop() {
            if let Some(kids) = children_by_parent.get(&id) {
                for k in kids {
                    match k.kind {
                        NodeKind::Leaf => leaf_descendants.push(k.clone()),
                        _ => {
                            cluster_descendants.push(k.clone());
                            to_visit.push(k.id.clone());
                        }
                    }
                }
            }
        }
        // Snapshot the cluster itself for restore.
        let dropped_node = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        // Record prior parents of all leaves so undo can re-attach.
        let leaf_moves: Vec<serde_json::Value> = leaf_descendants
            .iter()
            .map(|l| {
                serde_json::json!({
                    "leaf_id": l.id,
                    "prior_parent": l.parent,
                })
            })
            .collect();
        let absorbed_clusters: Vec<serde_json::Value> = cluster_descendants
            .iter()
            .chain(std::iter::once(&dropped_node))
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "parent_id": c.parent,
                    "kind": match c.kind { NodeKind::Cluster => "cluster", NodeKind::OutlierBucket => "outlier-bucket", _ => "leaf" },
                    "name": c.name,
                    "summary": c.summary,
                    "user_edited_name": c.user_edited_name,
                    "user_edited_summary": c.user_edited_summary,
                    "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                    "confidence": c.confidence,
                })
            })
            .collect();
        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            for l in &leaf_descendants {
                tx.execute(
                    "UPDATE cluster_nodes SET parent_id = ?1 WHERE tree_id = ?2 AND node_id = ?3",
                    params![outlier_bucket_id, tree_id, l.id],
                )?;
            }
            for c in &cluster_descendants {
                tx.execute(
                    "DELETE FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, c.id],
                )?;
            }
            tx.execute(
                "DELETE FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                params![tree_id, node_id],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        let args = serde_json::json!({
            "node_id": node_id,
            "outlier_bucket_id": outlier_bucket_id,
        });
        let undo = serde_json::json!({
            "node_id": node_id,
            "outlier_bucket_id": outlier_bucket_id,
            "leaf_moves": leaf_moves,
            "absorbed_clusters": absorbed_clusters,
        });
        self.append_history(tree_id, "drop-cluster", &args, &undo)?;
        // status: cluster-summary-staleness-counter — the outlier bucket
        // gained the dropped cluster's leaves; bump its chain.
        let _ = self.bump_churn_chain(tree_id, outlier_bucket_id, 1);
        Ok(())
    }
}
