//! Per-node edits: rename, set_summary, set_policy, auto_set_name_summary.
//! Each one appends one history row with symmetric undo_args.

use super::super::storage::params;
use super::super::types::{NodePolicy, Db, Error};

impl Db {
    /// Set the policy on a node (or clear it with `None`). Appends a
    /// `set-policy` history entry with both prior and new policy in the
    /// undo payload.
    pub fn set_policy(
        &self,
        tree_id: &str,
        node_id: &str,
        policy: Option<&NodePolicy>,
    ) -> Result<(), Error> {
        let prior = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        let policy_json = match policy {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes SET policy = ?1 WHERE tree_id = ?2 AND node_id = ?3",
                params![policy_json, tree_id, node_id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({ "node_id": node_id, "policy": policy });
        let undo = serde_json::json!({ "node_id": node_id, "policy": prior.policy });
        self.append_history(tree_id, "set-policy", &args, &undo)?;
        Ok(())
    }

    /// Rename a node. Stamps `user_edited_name = true`. Appends a
    /// `rename` history entry.
    pub fn rename(&self, tree_id: &str, node_id: &str, new_name: &str) -> Result<(), Error> {
        let prior = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes
                 SET name = ?1, user_edited_name = 1
                 WHERE tree_id = ?2 AND node_id = ?3",
                params![new_name, tree_id, node_id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({ "node_id": node_id, "name": new_name });
        let undo = serde_json::json!({
            "node_id": node_id,
            "name": prior.name,
            "user_edited_name": prior.user_edited_name,
        });
        self.append_history(tree_id, "rename", &args, &undo)?;
        Ok(())
    }

    /// Apply an LLM-generated name + summary to a cluster node without
    /// flipping `user_edited_*`. Honors existing user-edit flags by
    /// leaving the corresponding field alone. Resets
    /// `summary_membership_churn` to 0 (a fresh summary captures the
    /// current member set). Appends a `raptor-summarize` history entry.
    /// Returns `(wrote_name, wrote_summary)`.
    pub fn auto_set_name_summary(
        &self,
        tree_id: &str,
        node_id: &str,
        new_name: &str,
        new_summary: &str,
    ) -> Result<(bool, bool), Error> {
        let prior = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        let write_name = !prior.user_edited_name;
        let write_summary = !prior.user_edited_summary;
        if !write_name && !write_summary {
            return Ok((false, false));
        }
        self.with_conn(|conn| {
            match (write_name, write_summary) {
                (true, true) => {
                    conn.execute(
                        "UPDATE cluster_nodes
                         SET name = ?1, summary = ?2, summary_membership_churn = 0
                         WHERE tree_id = ?3 AND node_id = ?4",
                        params![new_name, new_summary, tree_id, node_id],
                    )?;
                }
                (true, false) => {
                    conn.execute(
                        "UPDATE cluster_nodes
                         SET name = ?1, summary_membership_churn = 0
                         WHERE tree_id = ?2 AND node_id = ?3",
                        params![new_name, tree_id, node_id],
                    )?;
                }
                (false, true) => {
                    conn.execute(
                        "UPDATE cluster_nodes
                         SET summary = ?1, summary_membership_churn = 0
                         WHERE tree_id = ?2 AND node_id = ?3",
                        params![new_summary, tree_id, node_id],
                    )?;
                }
                (false, false) => unreachable!(),
            }
            Ok(())
        })?;
        let args = serde_json::json!({
            "node_id": node_id,
            "name": new_name,
            "summary": new_summary,
            "wrote_name": write_name,
            "wrote_summary": write_summary,
        });
        let undo = serde_json::json!({
            "node_id": node_id,
            "name": prior.name,
            "summary": prior.summary,
            "summary_membership_churn": prior.summary_membership_churn,
        });
        self.append_history(tree_id, "raptor-summarize", &args, &undo)?;
        Ok((write_name, write_summary))
    }

    /// Edit a cluster's summary text. Stamps `user_edited_summary = true`.
    /// Appends an `edit-summary` history entry.
    pub fn set_summary(
        &self,
        tree_id: &str,
        node_id: &str,
        new_summary: &str,
    ) -> Result<(), Error> {
        let prior = self.get_node(tree_id, node_id)?.ok_or_else(|| {
            Error::NodeNotFound {
                tree_id: tree_id.to_string(),
                node_id: node_id.to_string(),
            }
        })?;
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cluster_nodes
                 SET summary = ?1, user_edited_summary = 1
                 WHERE tree_id = ?2 AND node_id = ?3",
                params![new_summary, tree_id, node_id],
            )?;
            Ok(())
        })?;
        let args = serde_json::json!({ "node_id": node_id, "summary": new_summary });
        let undo = serde_json::json!({
            "node_id": node_id,
            "summary": prior.summary,
            "user_edited_summary": prior.user_edited_summary,
        });
        self.append_history(tree_id, "edit-summary", &args, &undo)?;
        Ok(())
    }
}
