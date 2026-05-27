//! Per-node edits: rename, set_summary, set_policy, auto_set_name_summary.
//! Each one is a single `SetFrontmatter` write plus one session-history
//! entry with symmetric undo_args.

use super::super::types::{Db, Error, NodePolicy};

impl Db {
    /// Set the policy on a node (or clear it with `None`). Records a
    /// `set-policy` history entry with both prior and new policy.
    pub fn set_policy(
        &self,
        tree_id: &str,
        node_id: &str,
        policy: Option<&NodePolicy>,
    ) -> Result<(), Error> {
        let prior = self.mutate(tree_id, |doc| {
            let n = doc.get_mut(node_id).ok_or_else(|| Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            })?;
            let prior = n.policy.clone();
            n.policy = policy.cloned();
            Ok(prior)
        })?;
        let args = serde_json::json!({ "node_id": node_id, "policy": policy });
        let undo = serde_json::json!({ "node_id": node_id, "policy": prior });
        self.append_history(tree_id, "set-policy", &args, &undo)?;
        Ok(())
    }

    /// Rename a node. Stamps `user_edited_name = true`. Records a `rename`
    /// history entry.
    pub fn rename(&self, tree_id: &str, node_id: &str, new_name: &str) -> Result<(), Error> {
        let (prior_name, prior_flag) = self.mutate(tree_id, |doc| {
            let n = doc.get_mut(node_id).ok_or_else(|| Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            })?;
            let prior = (n.name.clone(), n.user_edited_name);
            n.name = new_name.to_string();
            n.user_edited_name = true;
            Ok(prior)
        })?;
        let args = serde_json::json!({ "node_id": node_id, "name": new_name });
        let undo = serde_json::json!({
            "node_id": node_id,
            "name": prior_name,
            "user_edited_name": prior_flag,
        });
        self.append_history(tree_id, "rename", &args, &undo)?;
        Ok(())
    }

    /// Apply an LLM-generated name + summary to a cluster node without
    /// flipping `user_edited_*`. Honors existing user-edit flags by leaving
    /// the corresponding field alone. Resets `summary_membership_churn` to 0
    /// (a fresh summary captures the current member set). Records a
    /// `raptor-summarize` history entry. Returns `(wrote_name, wrote_summary)`.
    pub fn auto_set_name_summary(
        &self,
        tree_id: &str,
        node_id: &str,
        new_name: &str,
        new_summary: &str,
    ) -> Result<(bool, bool), Error> {
        let (wrote, prior_name, prior_summary, prior_churn) = self.mutate(tree_id, |doc| {
            let n = doc.get_mut(node_id).ok_or_else(|| Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            })?;
            let write_name = !n.user_edited_name;
            let write_summary = !n.user_edited_summary;
            let prior = (n.name.clone(), n.summary.clone(), n.summary_membership_churn);
            if write_name {
                n.name = new_name.to_string();
            }
            if write_summary {
                n.summary = new_summary.to_string();
            }
            if write_name || write_summary {
                n.summary_membership_churn = 0;
            }
            Ok(((write_name, write_summary), prior.0, prior.1, prior.2))
        })?;
        if !wrote.0 && !wrote.1 {
            return Ok((false, false));
        }
        let args = serde_json::json!({
            "node_id": node_id,
            "name": new_name,
            "summary": new_summary,
            "wrote_name": wrote.0,
            "wrote_summary": wrote.1,
        });
        let undo = serde_json::json!({
            "node_id": node_id,
            "name": prior_name,
            "summary": prior_summary,
            "summary_membership_churn": prior_churn,
        });
        self.append_history(tree_id, "raptor-summarize", &args, &undo)?;
        Ok(wrote)
    }

    /// Edit a cluster's summary text. Stamps `user_edited_summary = true`.
    /// Records an `edit-summary` history entry.
    pub fn set_summary(
        &self,
        tree_id: &str,
        node_id: &str,
        new_summary: &str,
    ) -> Result<(), Error> {
        let (prior_summary, prior_flag) = self.mutate(tree_id, |doc| {
            let n = doc.get_mut(node_id).ok_or_else(|| Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            })?;
            let prior = (n.summary.clone(), n.user_edited_summary);
            n.summary = new_summary.to_string();
            n.user_edited_summary = true;
            Ok(prior)
        })?;
        let args = serde_json::json!({ "node_id": node_id, "summary": new_summary });
        let undo = serde_json::json!({
            "node_id": node_id,
            "summary": prior_summary,
            "user_edited_summary": prior_flag,
        });
        self.append_history(tree_id, "edit-summary", &args, &undo)?;
        Ok(())
    }
}
