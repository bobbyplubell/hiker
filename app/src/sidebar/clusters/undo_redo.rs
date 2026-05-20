//! Per-tree undo for cluster-tree edits. Plain function over `Trees`.
//!
//! Strategy: pop the most-recent history row via `Trees::pop_last_history`,
//! parse its `op` + `undo_args` JSON, and invert the change. Simple ops
//! (rename, edit-summary, set-policy, move, promote-outlier) just call the
//! corresponding forward setter with the prior value, then pop the
//! resulting forward history row so undo stays idempotent. Reshape ops
//! (merge-*, drop-cluster, split-cluster) replay snapshotted node rows
//! and reparent leaves directly.
//!
//! Redo + recluster-subtree undo are intentionally deferred — they need
//! the LLM worker dispatch path that hasn't been ported yet. Cluster ops
//! that produced a recluster history row will surface a "not yet
//! supported" toast from the caller rather than corrupt state.

use std::sync::Arc;

use hiker_core::trees::{NodeInsert, NodeKind, NodePolicy, Trees, TreesError};

#[derive(Debug)]
pub enum UndoError {
    Trees(TreesError),
    Parse(String),
    Unsupported(String),
    NothingToUndo,
}

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoError::Trees(e) => write!(f, "trees: {e}"),
            UndoError::Parse(e) => write!(f, "parse: {e}"),
            UndoError::Unsupported(s) => write!(f, "undo for `{s}` not implemented yet"),
            UndoError::NothingToUndo => write!(f, "nothing to undo"),
        }
    }
}

impl From<TreesError> for UndoError {
    fn from(e: TreesError) -> Self {
        UndoError::Trees(e)
    }
}

/// Pop and invert the most-recent history row for `tree_id`. The
/// popped entry is returned so the caller can push it onto a per-tree
/// redo stack.
pub fn undo(
    trees: &Arc<Trees>,
    tree_id: &str,
) -> Result<(String, hiker_core::trees::HistoryEntry), UndoError> {
    let Some(entry) = trees.pop_last_history(tree_id)? else {
        return Err(UndoError::NothingToUndo);
    };
    apply_undo(trees, tree_id, &entry.op, &entry.undo_args_json)?;
    Ok((entry.op.clone(), entry))
}

/// Re-apply a previously-undone history entry. The entry is the value
/// returned by `undo`; on success, history gets a fresh row matching the
/// re-applied op.
pub fn redo(
    trees: &Arc<Trees>,
    tree_id: &str,
    entry: &hiker_core::trees::HistoryEntry,
) -> Result<String, UndoError> {
    apply_redo(trees, tree_id, &entry.op, &entry.args_json, &entry.undo_args_json)?;
    Ok(entry.op.clone())
}

fn apply_redo(
    trees: &Arc<Trees>,
    tree_id: &str,
    op: &str,
    args_json: &str,
    _undo_args_json: &str,
) -> Result<(), UndoError> {
    match op {
        "rename" => {
            let a: RenameArgs = parse(args_json)?;
            trees.rename(tree_id, &a.node_id, &a.name)?;
        }
        "edit-summary" => {
            let a: EditSummaryArgs = parse(args_json)?;
            trees.set_summary(tree_id, &a.node_id, &a.summary)?;
        }
        "set-policy" => {
            let a: SetPolicyArgs = parse(args_json)?;
            trees.set_policy(tree_id, &a.node_id, a.policy)?;
        }
        "move" | "promote-outlier" => {
            let a: MoveArgs = parse(args_json)?;
            trees.move_node(tree_id, a.pick_id(), a.parent_id.as_deref())?;
        }
        "merge-siblings" => {
            let a: MergeSiblingsArgs = parse(args_json)?;
            let mut ids = vec![a.survivor];
            ids.extend(a.absorbed);
            trees.merge_siblings(tree_id, &ids)?;
        }
        "merge-children-up" => {
            let a: MergeChildrenUpArgs = parse(args_json)?;
            trees.merge_children_up(tree_id, &a.parent_id)?;
        }
        "drop-cluster" => {
            let a: DropClusterArgs = parse(args_json)?;
            trees.drop_cluster(tree_id, &a.node_id, &a.outlier_bucket_id)?;
        }
        other => {
            return Err(UndoError::Unsupported(format!("redo {other}")));
        }
    }
    Ok(())
}

fn apply_undo(
    trees: &Arc<Trees>,
    tree_id: &str,
    op: &str,
    undo_args_json: &str,
) -> Result<(), UndoError> {
    match op {
        "rename" => {
            let u: RenameArgs = parse(undo_args_json)?;
            trees.rename(tree_id, &u.node_id, &u.name)?;
            let _ = trees.pop_last_history(tree_id);
        }
        "edit-summary" => {
            let u: EditSummaryArgs = parse(undo_args_json)?;
            trees.set_summary(tree_id, &u.node_id, &u.summary)?;
            let _ = trees.pop_last_history(tree_id);
        }
        "set-policy" => {
            let u: SetPolicyArgs = parse(undo_args_json)?;
            trees.set_policy(tree_id, &u.node_id, u.policy)?;
            let _ = trees.pop_last_history(tree_id);
        }
        "move" | "promote-outlier" => {
            let u: MoveArgs = parse(undo_args_json)?;
            let node_id = u.pick_id();
            trees.move_node(tree_id, node_id, u.parent_id.as_deref())?;
            let _ = trees.pop_last_history(tree_id);
        }
        "merge-siblings" => {
            let u: MergeSiblingsUndo = parse(undo_args_json)?;
            for abs in &u.absorbed {
                restore_node_row(trees, tree_id, &abs.id, &abs.row)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .child_moves
                .into_iter()
                .map(|m| (m.child_id, Some(m.from)))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "merge-children-up" => {
            let u: MergeChildrenUpUndo = parse(undo_args_json)?;
            for abs in &u.absorbed {
                restore_node_row(trees, tree_id, &abs.id, &abs.row)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .grandchild_moves
                .into_iter()
                .map(|m| (m.child_id, Some(m.from)))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "drop-cluster" => {
            let u: DropClusterUndo = parse(undo_args_json)?;
            for c in &u.absorbed_clusters {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                restore_node_row(trees, tree_id, &id, c)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .leaf_moves
                .into_iter()
                .map(|m| (m.leaf_id, m.prior_parent))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "split-cluster" => {
            let u: SplitClusterUndo = parse(undo_args_json)?;
            let mut moves: Vec<(String, Option<String>)> = Vec::new();
            for mv in &u.leaf_moves {
                if let Some(arr) = mv.as_array()
                    && let Some(leaf) = arr.first()
                    && let Some(s) = leaf.as_str()
                {
                    moves.push((s.to_string(), Some(u.parent_id.clone())));
                }
            }
            trees.reparent_many(tree_id, &moves)?;
            for nc in &u.new_cluster_ids {
                trees.delete_node(tree_id, nc)?;
            }
        }
        "recluster-subtree" => {
            let u: ReclusterUndo = parse(undo_args_json)?;
            for id in &u.new_node_ids {
                trees.delete_node(tree_id, id)?;
            }
            for snap in &u.prior_subtree {
                let id = snap
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                restore_node_row(trees, tree_id, &id, snap)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .prior_leaf_parents
                .iter()
                .filter_map(|mv| {
                    let arr = mv.as_array()?;
                    let leaf = arr.first()?.as_str()?.to_string();
                    let parent = arr.get(1).and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        _ => None,
                    });
                    Some((leaf, parent))
                })
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "raptor-summarize" => {
            let u: RaptorSummarizeUndo = parse(undo_args_json)?;
            trees.set_summary(tree_id, &u.node_id, &u.summary)?;
            let _ = trees.pop_last_history(tree_id);
            // Name restore: rename also stamps user_edited_name, but the
            // raptor-summarize undo carries the prior name explicitly.
            // We rename through the public method to keep history sane,
            // then pop again.
            trees.rename(tree_id, &u.node_id, &u.name)?;
            let _ = trees.pop_last_history(tree_id);
        }
        other => {
            return Err(UndoError::Unsupported(other.to_string()));
        }
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, UndoError> {
    serde_json::from_str(s).map_err(|e| UndoError::Parse(e.to_string()))
}

fn parse_kind(s: &str) -> NodeKind {
    match s {
        "leaf" => NodeKind::Leaf,
        "outlier-bucket" => NodeKind::OutlierBucket,
        _ => NodeKind::Cluster,
    }
}

fn restore_node_row(
    trees: &Arc<Trees>,
    tree_id: &str,
    id: &str,
    row: &serde_json::Value,
) -> Result<(), UndoError> {
    let parent = row.get("parent_id").and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    });
    let kind = parse_kind(row.get("kind").and_then(|v| v.as_str()).unwrap_or("cluster"));
    let note_id = row
        .get("note_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let summary = row
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let policy: Option<NodePolicy> = row.get("policy").and_then(|v| match v {
        serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
        _ => None,
    });
    trees.insert_single_node(
        tree_id,
        NodeInsert {
            node_id: id.to_string(),
            parent_id: parent,
            kind,
            note_id,
            name,
            summary,
            user_edited_name: row
                .get("user_edited_name")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            user_edited_summary: row
                .get("user_edited_summary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            policy,
            centroid: None,
            confidence: row
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as f32,
            summary_membership_churn: row
                .get("summary_membership_churn")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .max(0) as u32,
        },
    )?;
    Ok(())
}

// ── Per-op arg structs ────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RenameArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize)]
struct EditSummaryArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    summary: String,
}

#[derive(serde::Deserialize)]
struct SetPolicyArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    policy: Option<NodePolicy>,
}

#[derive(serde::Deserialize)]
struct MoveArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    leaf_id: String,
    #[serde(default)]
    parent_id: Option<String>,
}

impl MoveArgs {
    fn pick_id(&self) -> &str {
        if !self.node_id.is_empty() {
            &self.node_id
        } else {
            &self.leaf_id
        }
    }
}

#[derive(serde::Deserialize)]
struct MergeSiblingsUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    child_moves: Vec<ChildMove>,
}

#[derive(serde::Deserialize)]
struct MergeChildrenUpUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    grandchild_moves: Vec<ChildMove>,
}

#[derive(serde::Deserialize)]
struct AbsorbedNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    row: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct ChildMove {
    #[serde(default)]
    child_id: String,
    #[serde(default)]
    from: String,
}

#[derive(serde::Deserialize)]
struct DropClusterUndo {
    #[serde(default)]
    absorbed_clusters: Vec<serde_json::Value>,
    #[serde(default)]
    leaf_moves: Vec<LeafMoveRow>,
}

#[derive(serde::Deserialize)]
struct LeafMoveRow {
    #[serde(default)]
    leaf_id: String,
    #[serde(default)]
    prior_parent: Option<String>,
}

#[derive(serde::Deserialize)]
struct SplitClusterUndo {
    #[serde(default)]
    parent_id: String,
    #[serde(default)]
    leaf_moves: Vec<serde_json::Value>,
    #[serde(default)]
    new_cluster_ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct MergeSiblingsArgs {
    #[serde(default)]
    survivor: String,
    #[serde(default)]
    absorbed: Vec<String>,
}

#[derive(serde::Deserialize)]
struct MergeChildrenUpArgs {
    #[serde(default)]
    parent_id: String,
}

#[derive(serde::Deserialize)]
struct DropClusterArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    outlier_bucket_id: String,
}

#[derive(serde::Deserialize)]
struct ReclusterUndo {
    #[serde(default)]
    prior_subtree: Vec<serde_json::Value>,
    #[serde(default)]
    prior_leaf_parents: Vec<serde_json::Value>,
    #[serde(default)]
    new_node_ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RaptorSummarizeUndo {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
}
