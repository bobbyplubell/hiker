//! In-memory session undo/redo log for the cluster editor
//! (`cluster-editor-edit-history`, `cluster-editor-undo-redo`).
//!
//! Edits ride the op-log on disk (each is a `SetFrontmatter` op on the tree
//! doc); undo/redo is a *session* concept layered on top — a per-tree stack
//! of `{ op, args, undo_args }` held in `Db.history`. It does **not** persist
//! across restarts (there is no `cluster_tree_history` table anymore);
//! cross-session "revert to an earlier state" rides the tree doc's version
//! history instead. The method surface is unchanged so the host's
//! undo/redo dispatch keeps working.

use super::store::now_ms;
use super::types::{Db, Error, HistoryEntry, NodePolicy};

/// The full reshape payload for a `recluster-subtree` history entry.
pub struct ReclusterSnapshot<'a> {
    pub root_id: &'a str,
    pub prior_subtree: &'a [serde_json::Value],
    pub prior_leaf_parents: &'a [(String, Option<String>)],
    pub new_nodes: &'a [serde_json::Value],
    pub leaf_moves: &'a [(String, Option<String>)],
    pub carried_policy: Option<&'a NodePolicy>,
}

impl Db {
    /// Append one edit to the tree's session undo log. Returns the assigned
    /// `seq` (monotonic per tree, per process).
    pub fn append_history(
        &self,
        tree_id: &str,
        op: &str,
        args: &serde_json::Value,
        undo_args: &serde_json::Value,
    ) -> Result<i64, Error> {
        let args_json = serde_json::to_string(args)?;
        let undo_json = serde_json::to_string(undo_args)?;
        let mut map = self.history.lock().map_err(|_| Error::Poisoned)?;
        let log = map.entry(tree_id.to_string()).or_default();
        let next_seq = log.last().map(|e| e.seq + 1).unwrap_or(1);
        log.push(HistoryEntry {
            seq: next_seq,
            ts_ms: now_ms(),
            op: op.to_string(),
            args_json,
            undo_args_json: undo_json,
        });
        Ok(next_seq)
    }

    /// Record a `split-cluster` history entry. The caller runs the actual
    /// HDBSCAN-against-members pass and the node mutations; this ties the
    /// change to a single undo step. `new_clusters` carries enough metadata
    /// to *replay* on redo (re-running the partition is non-deterministic).
    ///
    /// status: cluster-editor-split-cluster
    pub fn record_split(
        &self,
        tree_id: &str,
        parent_id: &str,
        new_clusters: &[serde_json::Value],
        leaf_moves: &[(String, Option<String>)],
    ) -> Result<i64, Error> {
        let new_cluster_ids: Vec<String> = new_clusters
            .iter()
            .filter_map(|c| c.get("node_id").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        let args = serde_json::json!({
            "parent_id": parent_id,
            "new_cluster_ids": new_cluster_ids,
            "new_clusters": new_clusters,
            "leaf_moves": leaf_moves,
        });
        let undo = serde_json::json!({
            "parent_id": parent_id,
            "new_cluster_ids": new_cluster_ids,
            "new_clusters": new_clusters,
            "leaf_moves": leaf_moves,
        });
        self.append_history(tree_id, "split-cluster", &args, &undo)?;
        Ok(0)
    }

    /// Record a `recluster-subtree` history entry. The caller runs the
    /// clustering pass and node mutations; this persists enough state to
    /// replay (redo) and reverse (undo) the reshape.
    ///
    /// status: cluster-editor-recluster-subtree
    /// status: cluster-editor-recluster-subtree-policy-loss
    pub fn record_recluster_subtree(
        &self,
        tree_id: &str,
        snapshot: &ReclusterSnapshot<'_>,
    ) -> Result<i64, Error> {
        let &ReclusterSnapshot {
            root_id,
            prior_subtree,
            prior_leaf_parents,
            new_nodes,
            leaf_moves,
            carried_policy,
        } = snapshot;
        let new_node_ids: Vec<String> = new_nodes
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        let carried_policy_json = match carried_policy {
            Some(p) => Some(serde_json::to_value(p)?),
            None => None,
        };
        let args = serde_json::json!({
            "root_id": root_id,
            "new_node_ids": new_node_ids,
            "new_nodes": new_nodes,
            "leaf_moves": leaf_moves,
            "carried_policy": carried_policy_json,
        });
        let undo = serde_json::json!({
            "root_id": root_id,
            "new_node_ids": new_node_ids,
            "prior_subtree": prior_subtree,
            "prior_leaf_parents": prior_leaf_parents,
            "leaf_moves": leaf_moves,
        });
        self.append_history(tree_id, "recluster-subtree", &args, &undo)
    }

    // ── Undo / redo ─────────────────────────────────────────────────

    /// Pop the most-recent history entry for a tree (`None` when the log is
    /// empty). Used by undo: the caller reads `entry.op` + `entry.undo_args`
    /// and inverts the change.
    pub fn pop_last_history(&self, tree_id: &str) -> Result<Option<HistoryEntry>, Error> {
        let mut map = self.history.lock().map_err(|_| Error::Poisoned)?;
        Ok(map.get_mut(tree_id).and_then(Vec::pop))
    }

    /// Read the session history log for a tree, oldest first.
    pub fn history(&self, tree_id: &str) -> Result<Vec<HistoryEntry>, Error> {
        let map = self.history.lock().map_err(|_| Error::Poisoned)?;
        Ok(map.get(tree_id).cloned().unwrap_or_default())
    }
}
