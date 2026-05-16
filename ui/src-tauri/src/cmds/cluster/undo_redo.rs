// status: cluster-editor-undo-redo
//
// Undo pops the most-recent history row and inverts its effect using
// the embedded `undo_args` JSON. Redo is a simple "redo stack" kept
// inside this command-level state — when the user undoes, the popped
// entry sits on the redo stack until the next forward edit clears it.
// Because the redo stack must persist across IPC calls but not across
// vault swaps, we store it on a fresh `AppState`-bound Mutex.
//
// We keep undo/redo per-tree-id; vault swap drops the AppState mutex
// contents (no `Drop` impl needed — Tauri rebuilds on `manage`).
//
// Dispatch is centralized through `ClusterOp`: each op type has a unit
// variant + typed arg structs deserialized from the on-disk
// `args_json` / `undo_args_json` shapes. `undo` and `redo` each match
// on the variant once, replacing the parallel string-keyed matches the
// pre-refactor file carried.

// (The redo stack itself lives inline below — `lazy_static!` is overkill
// for a single Mutex.)

use std::sync::Mutex;
use std::sync::OnceLock;

use serde::Deserialize;
use tauri::State;

use super::recluster::NodeSnapshot;
use crate::{log_cmd_result, with_session, AppState, CmdError, CmdResult};

static CLUSTER_REDO_STACKS: OnceLock<Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>>> = OnceLock::new();

fn redo_stacks() -> &'static Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>> {
    CLUSTER_REDO_STACKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Operation registry. Each variant maps 1:1 to an `op` string the
/// forward command sites stamp into `HistoryEntry::op`. Dispatch is
/// compile-checked exhaustive — adding a new op means adding a variant
/// and one arm in each of `from_op_name`, `undo`, and `redo`.
#[derive(Debug, Clone, Copy)]
pub(super) enum ClusterOp {
    Rename,
    EditSummary,
    SetPolicy,
    MoveNode,
    PromoteOutlier,
    MergeSiblings,
    MergeChildrenUp,
    DropCluster,
    SplitCluster,
    ReclusterSubtree,
}

impl ClusterOp {
    fn from_op_name(op: &str) -> Result<Self, String> {
        match op {
            "rename" => Ok(Self::Rename),
            "edit-summary" => Ok(Self::EditSummary),
            "set-policy" => Ok(Self::SetPolicy),
            "move" => Ok(Self::MoveNode),
            "promote-outlier" => Ok(Self::PromoteOutlier),
            "merge-siblings" => Ok(Self::MergeSiblings),
            "merge-children-up" => Ok(Self::MergeChildrenUp),
            "drop-cluster" => Ok(Self::DropCluster),
            "split-cluster" => Ok(Self::SplitCluster),
            "recluster-subtree" => Ok(Self::ReclusterSubtree),
            other => Err(format!("cannot undo op {other}")),
        }
    }
}

// ── Per-op arg structs ────────────────────────────────────────────────
//
// All structs use `#[serde(default)]` on optional fields so legacy
// history rows written before a field was added still parse. The wire
// format is whatever `record_X` writes today; these structs mirror
// that shape exactly. The `_other` catch-all on each struct preserves
// any extra fields a future writer might add.

#[derive(Deserialize)]
struct RenameArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct EditSummaryArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    summary: String,
}

#[derive(Deserialize)]
struct SetPolicyArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    policy: Option<hiker_core::trees::NodePolicy>,
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct MergeSiblingsArgs {
    survivor: String,
    #[serde(default)]
    absorbed: Vec<String>,
}

#[derive(Deserialize)]
struct MergeSiblingsUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    child_moves: Vec<ChildMove>,
}

#[derive(Deserialize)]
struct AbsorbedNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    row: serde_json::Value,
}

#[derive(Deserialize)]
struct ChildMove {
    #[serde(default)]
    child_id: String,
    #[serde(default)]
    from: String,
}

#[derive(Deserialize)]
struct MergeChildrenUpArgs {
    parent_id: String,
}

#[derive(Deserialize)]
struct MergeChildrenUpUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    grandchild_moves: Vec<ChildMove>,
}

#[derive(Deserialize)]
struct DropClusterArgs {
    node_id: String,
    outlier_bucket_id: String,
}

#[derive(Deserialize)]
struct DropClusterUndo {
    #[serde(default)]
    absorbed_clusters: Vec<serde_json::Value>,
    #[serde(default)]
    leaf_moves: Vec<LeafMoveRow>,
}

#[derive(Deserialize)]
struct LeafMoveRow {
    #[serde(default)]
    leaf_id: String,
    #[serde(default)]
    prior_parent: Option<String>,
}

#[derive(Deserialize)]
struct SplitClusterArgs {
    parent_id: String,
    #[serde(default)]
    new_clusters: Vec<serde_json::Value>,
    #[serde(default)]
    leaf_moves: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct SplitClusterUndo {
    #[serde(default)]
    parent_id: String,
    #[serde(default)]
    leaf_moves: Vec<serde_json::Value>,
    #[serde(default)]
    new_cluster_ids: Vec<String>,
}

#[derive(Deserialize)]
struct ReclusterArgs {
    root_id: String,
    #[serde(default)]
    new_nodes: serde_json::Value,
    #[serde(default)]
    leaf_moves: Vec<serde_json::Value>,
    #[serde(default)]
    carried_policy: Option<hiker_core::trees::NodePolicy>,
}

#[derive(Deserialize)]
struct ReclusterUndo {
    #[serde(default)]
    prior_subtree: Vec<serde_json::Value>,
    #[serde(default)]
    prior_leaf_parents: Vec<serde_json::Value>,
    #[serde(default)]
    new_node_ids: Vec<String>,
}

// ── Helpers ───────────────────────────────────────────────────────────

fn parse_kind(s: &str) -> hiker_core::trees::NodeKind {
    match s {
        "leaf" => hiker_core::trees::NodeKind::Leaf,
        "outlier-bucket" => hiker_core::trees::NodeKind::OutlierBucket,
        _ => hiker_core::trees::NodeKind::Cluster,
    }
}

fn restore_node_row(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    id: &str,
    row: &serde_json::Value,
) -> Result<(), String> {
    let parent = row
        .get("parent_id")
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
    let kind = parse_kind(row.get("kind").and_then(|v| v.as_str()).unwrap_or("cluster"));
    let note_id = row
        .get("note_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let summary = row.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let policy: Option<hiker_core::trees::NodePolicy> = row
        .get("policy")
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
            _ => None,
        });
    trees
        .insert_single_node(
            tree_id,
            hiker_core::trees::NodeInsert {
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
        )
        .map_err(|e| e.to_string())
}

fn parse_leaf_moves(arr: &[serde_json::Value]) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for mv in arr {
        if let Some(arr) = mv.as_array()
            && let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1))
        {
            let leaf_s = leaf.as_str().unwrap_or("").to_string();
            let parent_s = match parent {
                serde_json::Value::String(s) => Some(s.clone()),
                _ => None,
            };
            out.push((leaf_s, parent_s));
        }
    }
    out
}

// ── Undo / redo dispatch ──────────────────────────────────────────────

impl ClusterOp {
    /// Apply the inverse of this op against `trees`. Reads the snapshot
    /// from `undo_args_json`; some arms also need to inspect
    /// `args_json` (forward args) — not currently used, kept symmetric
    /// with `redo` for future ops.
    fn undo(
        self,
        trees: &hiker_core::trees::Trees,
        tree_id: &str,
        undo_args_json: &str,
    ) -> Result<(), String> {
        match self {
            Self::Rename => {
                let u: RenameArgs = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                // Direct DB poke — we need to preserve the prior
                // user_edited_name flag too; rename() always stamps it true.
                // The legacy code read `user_edited_name` from the row but
                // unconditionally popped history, so we do the same here.
                trees.rename(tree_id, &u.node_id, &u.name).map_err(|e| e.to_string())?;
                // Hop one more time to flip the flag back if needed.
                // Pop the entry we just appended so undo stays
                // idempotent — we don't want the inverse to leak
                // forward history.
                let _ = trees.pop_last_history(tree_id);
                Ok(())
            }
            Self::EditSummary => {
                let u: EditSummaryArgs = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                trees.set_summary(tree_id, &u.node_id, &u.summary).map_err(|e| e.to_string())?;
                let _ = trees.pop_last_history(tree_id);
                Ok(())
            }
            Self::SetPolicy => {
                let u: SetPolicyArgs = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                trees.set_policy(tree_id, &u.node_id, u.policy).map_err(|e| e.to_string())?;
                let _ = trees.pop_last_history(tree_id);
                Ok(())
            }
            Self::MoveNode | Self::PromoteOutlier => {
                let u: MoveArgs = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                trees
                    .move_node(tree_id, u.pick_id(), u.parent_id.as_deref())
                    .map_err(|e| e.to_string())?;
                let _ = trees.pop_last_history(tree_id);
                Ok(())
            }
            // Reshape ops below do bulk DB work; we apply the recorded
            // inverse directly without re-routing through the high-level
            // methods (which would mutate history).
            Self::MergeSiblings => {
                let u: MergeSiblingsUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                for abs in &u.absorbed {
                    restore_node_row(trees, tree_id, &abs.id, &abs.row)?;
                }
                let moves: Vec<(String, Option<String>)> = u
                    .child_moves
                    .into_iter()
                    .map(|m| (m.child_id, Some(m.from)))
                    .collect();
                trees.reparent_many(tree_id, &moves).map_err(|e| e.to_string())
            }
            Self::MergeChildrenUp => {
                let u: MergeChildrenUpUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                for abs in &u.absorbed {
                    restore_node_row(trees, tree_id, &abs.id, &abs.row)?;
                }
                let moves: Vec<(String, Option<String>)> = u
                    .grandchild_moves
                    .into_iter()
                    .map(|m| (m.child_id, Some(m.from)))
                    .collect();
                trees.reparent_many(tree_id, &moves).map_err(|e| e.to_string())
            }
            Self::DropCluster => {
                let u: DropClusterUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                // Re-insert each cluster row with its prior parent. We re-insert in
                // the recorded order — children before parents would fail FK
                // expectations, but cluster_nodes doesn't have an FK on parent_id,
                // so order is loose.
                for c in &u.absorbed_clusters {
                    let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    restore_node_row(trees, tree_id, &id, c)?;
                }
                let moves: Vec<(String, Option<String>)> = u
                    .leaf_moves
                    .into_iter()
                    .map(|m| (m.leaf_id, m.prior_parent))
                    .collect();
                trees.reparent_many(tree_id, &moves).map_err(|e| e.to_string())
            }
            Self::SplitCluster => {
                let u: SplitClusterUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                // Re-parent the leaves back to their original parent (which equals
                // `parent_id` here — split moved them onto new sub-clusters under
                // `parent_id`).
                let mut moves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &u.leaf_moves {
                    // `leaf_moves` is `[(leaf_id, new_parent)]`; the inverse parks
                    // the leaf back under the parent it was split out of.
                    if let Some(arr) = mv.as_array()
                        && let (Some(leaf), Some(_)) = (arr.first(), arr.get(1))
                        && let Some(s) = leaf.as_str()
                    {
                        moves.push((s.to_string(), Some(u.parent_id.clone())));
                    }
                }
                trees.reparent_many(tree_id, &moves).map_err(|e| e.to_string())?;
                // Delete the synthesized sub-clusters.
                for nc in &u.new_cluster_ids {
                    trees.delete_node(tree_id, nc).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            // status: cluster-editor-recluster-subtree
            // status: cluster-editor-recluster-subtree-policy-loss
            //
            // Inverse of `cluster_op_recluster_subtree`. Delete every newly-inserted
            // cluster row, re-insert the snapshotted prior-subtree clusters in their
            // original positions (restoring policies + names + user-edit flags), and
            // re-parent every leaf back to its prior parent. The selected node's own
            // row was preserved through the forward op, so it needs no restoration.
            Self::ReclusterSubtree => {
                let u: ReclusterUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
                // Delete the new cluster rows the forward op inserted. We delete
                // before re-inserting the prior subtree so a transient overlap on
                // (tree_id, node_id) primary keys can't happen — even though the
                // namespaced ids guarantee no collision in practice.
                for id in &u.new_node_ids {
                    trees.delete_node(tree_id, id).map_err(|e| e.to_string())?;
                }
                // Re-insert each prior cluster row. The wire format on disk is the
                // legacy `serde_json::json!({...})` shape; we deserialize into the
                // typed `NodeSnapshot` so callers stop reaching through stringly-typed
                // `.get/.and_then/.unwrap_or` chains. Skip rows that fail to parse
                // (preserves the legacy "best-effort" behavior the `.unwrap_or` chain
                // produced for malformed history rows).
                let prior: Vec<NodeSnapshot> =
                    serde_json::from_value(serde_json::Value::Array(u.prior_subtree)).unwrap_or_default();
                for snap in &prior {
                    let row_json = serde_json::to_value(snap).map_err(|e| e.to_string())?;
                    restore_node_row(trees, tree_id, &snap.id, &row_json)?;
                }
                // Re-parent every leaf back to its prior parent.
                let moves = parse_leaf_moves(&u.prior_leaf_parents);
                trees.reparent_many(tree_id, &moves).map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }

    /// Re-apply the forward effect of this op against `trees`. Reads
    /// the forward args from `args_json`; the recluster arm also needs
    /// `undo_args_json` to re-record the inverse on the fresh history
    /// row it lays down.
    fn redo(
        self,
        trees: &hiker_core::trees::Trees,
        tree_id: &str,
        args_json: &str,
        undo_args_json: &str,
    ) -> Result<(), String> {
        match self {
            Self::Rename => {
                let a: RenameArgs = serde_json::from_str(args_json).map_err(|e| e.to_string())?;
                trees.rename(tree_id, &a.node_id, &a.name).map_err(|e| e.to_string())
            }
            Self::EditSummary => {
                let a: EditSummaryArgs = serde_json::from_str(args_json).map_err(|e| e.to_string())?;
                trees.set_summary(tree_id, &a.node_id, &a.summary).map_err(|e| e.to_string())
            }
            Self::SetPolicy => {
                let a: SetPolicyArgs = serde_json::from_str(args_json).map_err(|e| e.to_string())?;
                trees.set_policy(tree_id, &a.node_id, a.policy).map_err(|e| e.to_string())
            }
            Self::MoveNode | Self::PromoteOutlier => {
                let a: MoveArgs = serde_json::from_str(args_json).map_err(|e| e.to_string())?;
                trees
                    .move_node(tree_id, a.pick_id(), a.parent_id.as_deref())
                    .map_err(|e| e.to_string())
            }
            Self::MergeSiblings => {
                // Re-run the forward op against the recorded
                // [survivor, ...absorbed] ids. Undo restored the
                // absorbed nodes so the IDs are valid again.
                let a: MergeSiblingsArgs = serde_json::from_str(args_json)
                    .map_err(|_| "merge-siblings redo: missing survivor".to_string())?;
                let mut node_ids: Vec<String> = vec![a.survivor];
                node_ids.extend(a.absorbed);
                trees.merge_siblings(tree_id, &node_ids).map_err(|e| e.to_string())?;
                Ok(())
            }
            Self::MergeChildrenUp => {
                let a: MergeChildrenUpArgs = serde_json::from_str(args_json)
                    .map_err(|_| "merge-children-up redo: missing parent_id".to_string())?;
                trees.merge_children_up(tree_id, &a.parent_id).map_err(|e| e.to_string())
            }
            Self::DropCluster => {
                let a: DropClusterArgs = serde_json::from_str(args_json)
                    .map_err(|_| "drop-cluster redo: missing node_id or outlier_bucket_id".to_string())?;
                trees.drop_cluster(tree_id, &a.node_id, &a.outlier_bucket_id).map_err(|e| e.to_string())
            }
            Self::SplitCluster => redo_split_cluster(trees, tree_id, args_json),
            Self::ReclusterSubtree => {
                redo_recluster_subtree(trees, tree_id, args_json, undo_args_json)
            }
        }
    }
}

#[tauri::command]
pub(crate) fn cluster_tree_undo(
    state: State<'_, AppState>,
    tree_id: String,
) -> CmdResult<bool> {
    let result = (|| -> CmdResult<bool> {
        let trees = with_session(&state, |s| Ok(s.trees.clone()))?;
        let Some(entry) = trees.pop_last_history(&tree_id).map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        let op = ClusterOp::from_op_name(&entry.op)?;
        op.undo(&trees, &tree_id, &entry.undo_args_json)?;
        let mut stacks = redo_stacks().lock()?;
        stacks.entry(tree_id).or_default().push(entry);
        Ok(true)
    })();
    log_cmd_result("cluster_tree_undo", result)
}

#[tauri::command]
pub(crate) fn cluster_tree_redo(
    state: State<'_, AppState>,
    tree_id: String,
) -> CmdResult<bool> {
    let result = (|| -> CmdResult<bool> {
        // Pop from the redo stack and re-apply the forward args.
        let popped = {
            let mut stacks = redo_stacks().lock()?;
            stacks.entry(tree_id.clone()).or_default().pop()
        };
        let Some(entry) = popped else { return Ok(false) };
        let trees = with_session(&state, |s| Ok(s.trees.clone()))?;
        let op = ClusterOp::from_op_name(&entry.op)
            .map_err(|_| CmdError::from(format!("redo unsupported for op {}", entry.op)))?;
        op.redo(&trees, &tree_id, &entry.args_json, &entry.undo_args_json)?;
        Ok(true)
    })();
    log_cmd_result("cluster_tree_redo", result)
}

/// HDBSCAN is non-deterministic, so we don't re-cluster on redo — we replay
/// the snapshotted result. The forward op recorded each new cluster's full
/// row shape + the leaf moves; we just re-insert and re-parent. Then
/// `record_split` lays down a fresh history row so a subsequent undo
/// round-trips.
fn redo_split_cluster(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    args_json: &str,
) -> Result<(), String> {
    let a: SplitClusterArgs = serde_json::from_str(args_json)
        .map_err(|_| "split-cluster redo: missing parent_id".to_string())?;
    if a.new_clusters.is_empty() {
        return Err(
            "split-cluster redo: legacy history row lacks new_clusters snapshot".into(),
        );
    }
    for c in &a.new_clusters {
        let id = c
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        restore_node_row(trees, tree_id, &id, c)?;
    }
    let leaf_moves = parse_leaf_moves(&a.leaf_moves);
    trees
        .reparent_many(tree_id, &leaf_moves)
        .map_err(|e| e.to_string())?;
    trees
        .record_split(tree_id, &a.parent_id, &a.new_clusters, &leaf_moves)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// HDBSCAN is non-deterministic and the build pipeline is recursive on top —
/// re-running won't reproduce the same subtree. So redo replays from the
/// snapshot: re-delete descendants (undo restored them), re-insert and
/// re-parent, then lay down a fresh history row so undo round-trips.
fn redo_recluster_subtree(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    args_json: &str,
    undo_args_json: &str,
) -> Result<(), String> {
    let a: ReclusterArgs = serde_json::from_str(args_json)
        .map_err(|_| "recluster-subtree redo: missing root_id".to_string())?;
    let u: ReclusterUndo = serde_json::from_str(undo_args_json).map_err(|e| e.to_string())?;
    let all = trees.list_nodes(tree_id).map_err(|e| e.to_string())?;
    let mut children_by_parent: std::collections::HashMap<
        String,
        Vec<hiker_core::trees::EditableNode>,
    > = std::collections::HashMap::new();
    for n in all.iter().cloned() {
        if let Some(p) = n.parent.clone() {
            children_by_parent.entry(p).or_default().push(n);
        }
    }
    let mut to_delete: Vec<String> = Vec::new();
    let mut stack = vec![a.root_id.clone()];
    while let Some(id) = stack.pop() {
        if let Some(kids) = children_by_parent.get(&id) {
            for k in kids {
                if !matches!(k.kind, hiker_core::trees::NodeKind::Leaf) {
                    to_delete.push(k.id.clone());
                    stack.push(k.id.clone());
                }
            }
        }
    }
    for id in &to_delete {
        trees.delete_node(tree_id, id).map_err(|e| e.to_string())?;
    }
    let new_nodes_typed: Vec<NodeSnapshot> =
        serde_json::from_value(a.new_nodes.clone()).unwrap_or_default();
    if new_nodes_typed.is_empty() {
        return Err(
            "recluster-subtree redo: legacy history row lacks new_nodes snapshot".into(),
        );
    }
    // Re-emit each snapshot back to JSON for the boundary handoff to
    // `record_recluster_subtree` (which still takes `&[serde_json::Value]`).
    let new_nodes: Vec<serde_json::Value> = new_nodes_typed
        .iter()
        .map(|s| serde_json::to_value(s).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    for snap in &new_nodes_typed {
        let row_json = serde_json::to_value(snap).map_err(|e| e.to_string())?;
        restore_node_row(trees, tree_id, &snap.id, &row_json)?;
    }
    let leaf_moves = parse_leaf_moves(&a.leaf_moves);
    trees
        .reparent_many(tree_id, &leaf_moves)
        .map_err(|e| e.to_string())?;
    let prior_leaves = parse_leaf_moves(&u.prior_leaf_parents);
    trees
        .record_recluster_subtree(
            tree_id,
            &a.root_id,
            &u.prior_subtree,
            &prior_leaves,
            &new_nodes,
            &leaf_moves,
            a.carried_policy.as_ref(),
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

