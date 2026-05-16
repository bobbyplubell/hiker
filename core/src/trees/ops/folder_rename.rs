//! `update_for_folder_rename` — live-update for FromFolders trees when a
//! note's containing folder changes.

use super::super::storage::params;
use super::super::types::{NodeInsert, NodeKind, Trees, TreesError};

impl Trees {
    /// Live-update a FromFolders tree to reflect a vault rename. Per
    /// `cluster-build-from-folders-live-update`: the leaf with `note_id =
    /// note_id` (if present in this tree) is re-parented under the folder
    /// cluster matching `new_folder`. The folder cluster is created lazily
    /// when it doesn't yet exist. Emptied folder clusters are dropped
    /// unless they carry an explicit policy (the user's rule survives a
    /// transient empty state). Centroid recomputation is the producer's
    /// concern (cheap: vector mean over members) — for Sprint D we just
    /// keep the existing centroid since the move doesn't shift it much
    /// when the average folder has many notes.
    ///
    /// `new_folder` is the folder portion of the new vault-relative
    /// path (e.g. `"research/embeddings"`); `""` means "vault root".
    ///
    /// status: cluster-build-from-folders-live-update
    /// status: cluster-build-from-folders-summary-staleness
    pub fn update_for_folder_rename(
        &self,
        tree_id: &str,
        note_id: &str,
        new_folder: &str,
    ) -> Result<bool, TreesError> {
        // Locate the leaf by note_id within this tree. Scan once over the
        // full node list — trees.db tables are small per-tree.
        let nodes = self.list_nodes(tree_id)?;
        let Some(leaf) = nodes.iter().find(|n| {
            matches!(n.kind, NodeKind::Leaf)
                && n.note_ref.as_deref() == Some(note_id)
        }) else {
            // Not present in this tree — nothing to update.
            return Ok(false);
        };
        let prior_parent = leaf.parent.clone();
        // Compute the new folder cluster id. Mirrors `build_from_folders`'s
        // shape in core::cluster so existing trees keep working.
        let safe_folder = new_folder.replace('/', "-");
        let new_folder_cluster_id = format!(
            "f-{}",
            if safe_folder.is_empty() { "_root" } else { &safe_folder }
        );
        if prior_parent.as_deref() == Some(new_folder_cluster_id.as_str()) {
            // Already parented correctly.
            return Ok(false);
        }

        // Find the root (parent_id = NULL).
        let root_id = nodes
            .iter()
            .find(|n| n.parent.is_none())
            .map(|n| n.id.clone());

        // Ensure the destination folder cluster exists.
        let exists = nodes
            .iter()
            .any(|n| n.id == new_folder_cluster_id);
        if !exists {
            // Insert a fresh cluster node parented at the root.
            let basename = new_folder.rsplit('/').next().unwrap_or("");
            let display_name = if basename.is_empty() {
                "vault root".to_string()
            } else {
                basename.to_string()
            };
            self.insert_single_node(
                tree_id,
                NodeInsert {
                    node_id: new_folder_cluster_id.clone(),
                    parent_id: root_id.clone(),
                    kind: NodeKind::Cluster,
                    note_id: None,
                    name: display_name,
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: 1.0,
                    summary_membership_churn: 0,
                },
            )?;
        }

        // Move the leaf. We bypass `move_node` here because that function
        // expects a NodeKind::Cluster destination via the same database
        // transaction and we want to capture the prior_parent for churn
        // bumping below.
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes SET parent_id = ?1 WHERE tree_id = ?2 AND node_id = ?3",
                params![new_folder_cluster_id, tree_id, leaf.id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({
            "note_id": note_id,
            "leaf_id": leaf.id,
            "new_folder": new_folder,
        });
        let undo = serde_json::json!({
            "leaf_id": leaf.id,
            "prior_parent": prior_parent,
        });
        self.append_history(tree_id, "folder-rename-update", &args, &undo)?;

        // Bump churn on both chains.
        if let Some(prev_parent) = prior_parent.as_deref() {
            let _ = self.bump_churn_chain(tree_id, prev_parent, 1);
        }
        let _ = self.bump_churn_chain(tree_id, &new_folder_cluster_id, 1);

        // Drop emptied folder clusters (unless they carry a policy or are
        // the root). Per spec: "emptied folders' nodes are dropped (unless
        // they carry an explicit policy, in which case they're kept as
        // empty placeholders so the user's rule survives a transient
        // empty state)."
        if let Some(prev_id) = prior_parent {
            let still_has_children: i64 = self.with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM cluster_nodes
                     WHERE tree_id = ?1 AND parent_id = ?2",
                    params![tree_id, prev_id],
                    |row| row.get(0),
                )?)
            })?;
            if still_has_children == 0 {
                let prev_node = self.get_node(tree_id, &prev_id)?;
                if let Some(pn) = prev_node {
                    let is_root = pn.parent.is_none();
                    let has_policy = pn.policy.is_some();
                    let is_folder_cluster = pn.id.starts_with("f-");
                    if !is_root && !has_policy && is_folder_cluster {
                        self.delete_node(tree_id, &pn.id)?;
                    }
                }
            }
        }

        Ok(true)
    }
}
