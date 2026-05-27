//! `update_for_folder_rename` — live-update for FromFolders trees when a
//! note's containing folder changes. One `SetFrontmatter` write when the
//! tree actually changes; a no-op (no write) otherwise.

use super::super::types::{Db, EditableNode, Error, NodeKind};

impl Db {
    /// Live-update a FromFolders tree to reflect a vault rename. The leaf
    /// with `note_ref = note_id` (if present in this tree) is re-parented
    /// under the folder cluster matching `new_folder`, creating that cluster
    /// lazily. Emptied folder clusters are dropped unless they carry a policy
    /// (so the user's rule survives a transient empty state). `new_folder` is
    /// the folder portion of the new path (`""` = vault root).
    ///
    /// status: cluster-build-from-folders-live-update
    /// status: cluster-build-from-folders-summary-staleness
    pub fn update_for_folder_rename(
        &self,
        tree_id: &str,
        note_id: &str,
        new_folder: &str,
    ) -> Result<bool, Error> {
        // Decide (read-only) whether anything changes before writing.
        let nodes = self.list_nodes(tree_id)?;
        let Some(leaf) = nodes.iter().find(|n| {
            matches!(n.kind, NodeKind::Leaf) && n.note_ref.as_deref() == Some(note_id)
        }) else {
            return Ok(false);
        };
        let leaf_id = leaf.id.clone();
        let prior_parent = leaf.parent.clone();
        let safe_folder = new_folder.replace('/', "-");
        let new_folder_cluster_id = format!(
            "f-{}",
            if safe_folder.is_empty() { "_root" } else { &safe_folder }
        );
        if prior_parent.as_deref() == Some(new_folder_cluster_id.as_str()) {
            return Ok(false);
        }
        let root_id = nodes.iter().find(|n| n.parent.is_none()).map(|n| n.id.clone());
        let needs_cluster = !nodes.iter().any(|n| n.id == new_folder_cluster_id);
        let display_name = {
            let basename = new_folder.rsplit('/').next().unwrap_or("");
            if basename.is_empty() {
                "vault root".to_string()
            } else {
                basename.to_string()
            }
        };

        self.mutate(tree_id, |doc| {
            if needs_cluster {
                doc.insert(EditableNode {
                    id: new_folder_cluster_id.clone(),
                    parent: root_id.clone(),
                    kind: NodeKind::Cluster,
                    note_ref: None,
                    name: display_name.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: 1.0,
                    summary_membership_churn: 0,
                });
            }
            doc.set_parent(&leaf_id, Some(&new_folder_cluster_id));
            // Bump churn on both chains.
            if let Some(prev_parent) = prior_parent.as_deref() {
                doc.bump_churn(prev_parent, 1);
            }
            doc.bump_churn(&new_folder_cluster_id, 1);
            // Drop the emptied folder cluster unless it's the root or carries
            // a policy (the user's rule survives a transient empty state).
            if let Some(prev_id) = prior_parent.as_deref()
                && doc.child_ids(prev_id).is_empty()
                && let Some(pn) = doc.get(prev_id)
                && pn.parent.is_some()
                && pn.policy.is_none()
                && pn.id.starts_with("f-")
            {
                doc.remove(prev_id);
            }
            Ok(())
        })?;

        let args = serde_json::json!({
            "note_id": note_id,
            "leaf_id": leaf_id,
            "new_folder": new_folder,
        });
        let undo = serde_json::json!({ "leaf_id": leaf_id, "prior_parent": prior_parent });
        self.append_history(tree_id, "folder-rename-update", &args, &undo)?;
        Ok(true)
    }
}
