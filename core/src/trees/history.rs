//! History-log operations + record_* helpers used by the cluster-editor
//! ops. Reads/writes `cluster_tree_history` via the SQL helpers in
//! `super::storage`.

use super::storage::{params, OptionalExtension};
use super::storage::now_ms;
use super::types::{HistoryEntry, NodePolicy, Db, Error};

/// The full reshape payload for a `recluster-subtree` history entry.
/// Splits naturally into the prior-state snapshot used to *reverse* the
/// reshape (`prior_subtree`, `prior_leaf_parents`) and the new-state
/// payload used to *replay* it (`new_nodes`, `leaf_moves`,
/// `carried_policy`), all anchored at `root_id`.
pub struct ReclusterSnapshot<'a> {
    /// Subtree root the reshape is anchored at.
    pub root_id: &'a str,
    /// Each deleted cluster's full row shape + prior `parent_id`, so
    /// undo can re-insert them exactly.
    pub prior_subtree: &'a [serde_json::Value],
    /// `(leaf_id, prior_parent)` parentage captured before the reshape.
    pub prior_leaf_parents: &'a [(String, Option<String>)],
    /// Inserted cluster rows in dependency order (parents before
    /// children) so redo can restore each in turn.
    pub new_nodes: &'a [serde_json::Value],
    /// `(leaf_id, new_parent)` moves applied by the reshape.
    pub leaf_moves: &'a [(String, Option<String>)],
    /// Policy carried onto direct children when the user opted into
    /// "Carry policies down".
    pub carried_policy: Option<&'a NodePolicy>,
}

impl Db {
    /// Append one edit to `cluster_tree_history`. Public so the build
    /// pipeline (and any future op outside this module's vocabulary)
    /// can stamp custom op kinds. Returns the assigned `seq`.
    pub fn append_history(
        &self,
        tree_id: &str,
        op: &str,
        args: &serde_json::Value,
        undo_args: &serde_json::Value,
    ) -> Result<i64, Error> {
        let args_json = serde_json::to_string(args)?;
        let undo_json = serde_json::to_string(undo_args)?;
        let now = now_ms();
        self.with_conn(|conn| {
            let next_seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM cluster_tree_history WHERE tree_id = ?1",
                params![tree_id],
                |row| row.get(0),
            )?;
            conn.execute(
                "INSERT INTO cluster_tree_history (tree_id, seq, ts_ms, op, args, undo_args)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![tree_id, next_seq, now, op, args_json, undo_json],
            )?;
            Ok(next_seq)
        })
    }

    /// Record a `split-cluster` history entry. Caller (cluster editor)
    /// runs the actual HDBSCAN-against-members pass via `core::cluster`
    /// and uses `insert_single_node` + `reparent_many` to land the new
    /// sub-clusters; this method ties the change to a single undo step.
    ///
    /// `new_clusters` carries enough per-cluster metadata (id, name,
    /// summary, confidence, …) to *replay* the split deterministically
    /// on redo — re-running HDBSCAN against the same members is
    /// non-deterministic, so redo restores the snapshot rather than
    /// re-clustering (per `cluster-editor-undo-redo`).
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

    /// Record a `recluster-subtree` history entry. Like `record_split`,
    /// the caller is responsible for running the actual clustering pass
    /// (`core::cluster::tree` against the subtree's leaves) and
    /// for the cluster_nodes mutations (delete descendants, insert new
    /// cluster rows, reparent leaves). This method only persists enough
    /// state on the history row to replay (redo) and reverse (undo) the
    /// reshape.
    ///
    /// `prior_subtree` carries each deleted cluster's full row shape +
    /// its prior `parent_id` so undo can re-insert them exactly. The
    /// snapshot also records the resolved policy carried onto direct
    /// children (when "Carry policies down" was checked) so the user's
    /// intent is preserved in the audit log.
    ///
    /// `new_nodes` carries the inserted cluster rows in dependency order
    /// (parents before children) so redo can restore_node_row each one
    /// in turn. `leaf_moves` is the `(leaf_id, new_parent)` list.
    ///
    /// Per `cluster-editor-recluster-subtree`,
    /// `cluster-editor-recluster-subtree-policy-loss` (the carried
    /// policies and the explicit "policies are lost" semantics), and the
    /// undo-snapshots-prior-subtree clause.
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
    //
    // The history table records `args` (forward) + `undo_args` (reverse)
    // for every mutation. `pop_history` returns the most-recent entry
    // and removes it so a subsequent `append_history` doesn't conflict
    // with the seq sequence. Redo is implemented by re-applying the
    // popped entry's `args` — the UI re-routes through the regular ops.
    //
    // Edit replays in this module are deliberately keyed off operation
    // name strings so new ops added by the cluster editor (Sprint B)
    // and triage (Sprint C) compose cleanly without a central enum.
    //
    // status: cluster-editor-undo-redo

    /// Pop the most-recent history entry for a tree (returns `None`
    /// when the log is empty). Used by undo: caller reads `entry.op` +
    /// `entry.undo_args` and inverts the change.
    pub fn pop_last_history(&self, tree_id: &str) -> Result<Option<HistoryEntry>, Error> {
        self.with_conn(|conn| {
            let entry: Option<HistoryEntry> = conn
                .query_row(
                    "SELECT seq, ts_ms, op, args, undo_args
                     FROM cluster_tree_history
                     WHERE tree_id = ?1
                     ORDER BY seq DESC LIMIT 1",
                    params![tree_id],
                    |row| {
                        Ok(HistoryEntry {
                            seq: row.get(0)?,
                            ts_ms: row.get(1)?,
                            op: row.get(2)?,
                            args_json: row.get(3)?,
                            undo_args_json: row.get(4)?,
                        })
                    },
                )
                .optional()?;
            if let Some(e) = &entry {
                conn.execute(
                    "DELETE FROM cluster_tree_history WHERE tree_id = ?1 AND seq = ?2",
                    params![tree_id, e.seq],
                )?;
            }
            Ok(entry)
        })
    }

    /// Read the history log for a tree, oldest first.
    pub fn history(&self, tree_id: &str) -> Result<Vec<HistoryEntry>, Error> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, ts_ms, op, args, undo_args FROM cluster_tree_history
                 WHERE tree_id = ?1 ORDER BY seq ASC",
            )?;
            let rows = stmt
                .query_map(params![tree_id], |row| {
                    Ok(HistoryEntry {
                        seq: row.get(0)?,
                        ts_ms: row.get(1)?,
                        op: row.get(2)?,
                        args_json: row.get(3)?,
                        undo_args_json: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}
