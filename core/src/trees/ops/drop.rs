//! `drop_cluster` — collapses a cluster subtree into the nearest outlier
//! bucket and records the inverse for undo. One `SetFrontmatter` write.

use std::collections::HashMap;

use super::super::types::{Db, EditableNode, Error, NodeKind};

impl Db {
    /// Drop a cluster: its leaf descendants are re-parented under
    /// `outlier_bucket_id`; nested cluster nodes are deleted along with the
    /// dropped cluster. Records a `drop-cluster` history row.
    ///
    /// status: cluster-editor-drop-cluster
    pub fn drop_cluster(
        &self,
        tree_id: &str,
        node_id: &str,
        outlier_bucket_id: &str,
    ) -> Result<(), Error> {
        let (leaf_moves, absorbed_clusters) = self.mutate(tree_id, |doc| {
            let dropped = doc.get(node_id).cloned().ok_or_else(|| Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            })?;
            // children_by_parent over a snapshot of the node list.
            let mut children_by_parent: HashMap<String, Vec<EditableNode>> = HashMap::new();
            for n in doc.nodes.iter().cloned() {
                if let Some(p) = n.parent.clone() {
                    children_by_parent.entry(p).or_default().push(n);
                }
            }
            // DFS the subtree, splitting leaves from clusters.
            let mut to_visit = vec![node_id.to_string()];
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
            let leaf_moves: Vec<serde_json::Value> = leaf_descendants
                .iter()
                .map(|l| serde_json::json!({ "leaf_id": l.id, "prior_parent": l.parent }))
                .collect();
            let absorbed_clusters: Vec<serde_json::Value> = cluster_descendants
                .iter()
                .chain(std::iter::once(&dropped))
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "parent_id": c.parent,
                        "kind": c.kind.as_str(),
                        "name": c.name,
                        "summary": c.summary,
                        "user_edited_name": c.user_edited_name,
                        "user_edited_summary": c.user_edited_summary,
                        "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                        "confidence": c.confidence,
                    })
                })
                .collect();
            // Apply: re-parent leaves to the outlier bucket; delete clusters.
            for l in &leaf_descendants {
                doc.set_parent(&l.id, Some(outlier_bucket_id));
            }
            for c in &cluster_descendants {
                doc.remove(&c.id);
            }
            doc.remove(node_id);
            // status: cluster-summary-staleness-counter
            doc.bump_churn(outlier_bucket_id, 1);
            Ok((leaf_moves, absorbed_clusters))
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
        Ok(())
    }
}
