// ── Clustering review tab Tauri commands ─────────────────────────────
//
// status: cluster-review-tab
// status: cluster-review-tab-run-clustering
// status: cluster-review-tab-structural-pass-no-llm
// status: cluster-review-tab-confirm-and-name
//
// These three commands replace the legacy `cluster_tree_create` /
// `cluster_op_recluster_subtree` end-to-end paths for the UI. The
// legacy commands stay for non-UI callers (CLI, tests) until those have
// alternate plumbing. The UI now drives a two-phase flow:
//
//   1. `cluster_run_structural` runs HDBSCAN-only (no LLM) and returns
//      a serialized `BuiltClusterTree` plus per-note titles. Nothing is
//      persisted.
//   2. `cluster_persist_built_tree` (new-tree / rebuild) or
//      `cluster_op_recluster_subtree_from_built` (recluster) takes the
//      structural DTO + user-renamed names, persists rows, and submits
//      `RaptorSummarize` tasks for the un-renamed clusters.
//
// Both confirm-and-name paths route through the shared
// `commit_built_tree` helper below: it dispatches on `CommitTarget`
// (NewTree vs SubtreeOf), runs the appropriate planning + apply path,
// and finishes with `submit_naming_tasks` so the post-commit
// RaptorSummarize fan-out is byte-identical between the two commands.

use serde::{Deserialize, Serialize};
use tauri::State;

use super::recluster::{
    apply_recluster_writes_with_user_edits, plan_recluster_from_built, snapshot_prior_subtree,
    snapshots_to_json_values, PlanFromBuiltArgs, TreeIndex,
};
use super::{default_source_oneshot, default_true, notes_for_recluster_target, notes_for_scope_via_session};
use crate::{log_cmd_result, with_session, AppState, CmdError, CmdResult};

#[derive(Debug, Deserialize)]
pub(crate) struct ClusterRunStructuralArgs {
    /// JSON of `core::cluster::BuildScope` — for the new-tree case;
    /// ignored when `recluster_target` is set.
    #[serde(default)]
    scope_json: Option<String>,
    /// JSON of `core::cluster::BuildMethod`. Carries the user-chosen
    /// `ClusterParams` / `FolderDeriveParams` (the structural pass
    /// forces `summarize = None` regardless).
    method_json: String,
    /// When set, scope is computed from the named subtree's leaves
    /// rather than `scope_json`. Used by the recluster-subtree flow.
    #[serde(default)]
    recluster_target: Option<ReclusterTarget>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReclusterTarget {
    pub(super) tree_id: String,
    pub(super) node_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct StructuralBuildDto {
    /// Echoed back so the persist command doesn't need to re-resolve
    /// scope — the resolved `BuildScope::Notes { ids }` is used directly.
    scope_json: String,
    method_json: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    /// Map of note_id → display title so the UI can render the
    /// preview rows without N more round-trips.
    note_titles: std::collections::HashMap<String, String>,
}

// status: cluster-review-tab-run-clustering
#[tauri::command]
pub(crate) fn cluster_run_structural(
    state: State<'_, AppState>,
    args: ClusterRunStructuralArgs,
) -> CmdResult<StructuralBuildDto> {
    let result = with_session(&state, |session| {
        let method: hiker_core::cluster::BuildMethod = serde_json::from_str(&args.method_json)
            .map_err(|e| CmdError::from(format!("method_json: {e}")))?;
        let (scope, notes) = match args.recluster_target {
            Some(target) => {
                let notes = notes_for_recluster_target(session, &target.tree_id, &target.node_id)?;
                if notes.len() < 4 {
                    return Err(CmdError::from("not enough embedded notes under this cluster to recluster (need >= 4)"));
                }
                let scope = hiker_core::cluster::BuildScope::Notes {
                    ids: notes.iter().map(|n| n.id.clone()).collect(),
                    source_types: Vec::new(),
                };
                (scope, notes)
            }
            None => {
                let scope_json = args
                    .scope_json
                    .as_deref()
                    .ok_or_else(|| CmdError::from("scope_json required when recluster_target is absent"))?;
                let scope: hiker_core::cluster::BuildScope = serde_json::from_str(scope_json)
                    .map_err(|e| CmdError::from(format!("scope_json: {e}")))?;
                let notes = notes_for_scope_via_session(session, &scope)?;
                if notes.is_empty() {
                    return Err(CmdError::from("no notes with embeddings found in scope"));
                }
                (scope, notes)
            }
        };
        // Capture titles before `notes` is consumed by the build pass.
        let note_titles: std::collections::HashMap<String, String> = notes
            .iter()
            .map(|n| (n.id.clone(), n.title.clone()))
            .collect();
        let build_result = hiker_core::cluster::build_tree_structural(
            scope.clone(),
            method.clone(),
            &notes,
        )
        .map_err(|e| CmdError::from(format!("structural build: {e}")))?;
        let scope_json = serde_json::to_string(&build_result.scope)
            .map_err(|e| CmdError::from(format!("scope serialize: {e}")))?;
        let method_json = serde_json::to_string(&build_result.method)
            .map_err(|e| CmdError::from(format!("method serialize: {e}")))?;
        Ok(StructuralBuildDto {
            scope_json,
            method_json,
            tree: build_result.tree,
            note_titles,
        })
    });
    log_cmd_result("cluster_run_structural", result)
}

/// Where a `commit_built_tree` call should land its writes. `NewTree`
/// persists a brand-new tree row; `SubtreeOf` replaces an existing
/// subtree in place via the recluster pipeline.
enum CommitTarget {
    NewTree {
        name: String,
        source: String,
        scope_json: String,
        method_json: String,
    },
    SubtreeOf {
        tree_id: String,
        node_id: String,
        carry_policies_down: bool,
    },
}

/// Shared inputs to `commit_built_tree`. Borrowed because the calling
/// commands still own the deserialized args at the call site.
struct CommitInputs<'a> {
    tree: &'a hiker_core::cluster::BuiltClusterTree,
    user_renamed: &'a std::collections::HashMap<String, String>,
    submit_naming: bool,
}

/// Output of `commit_built_tree`. Each Tauri command surfaces the
/// subset its wire DTO promises.
struct CommitOutcome {
    /// For `NewTree`: the newly minted tree id.
    /// For `SubtreeOf`: the existing tree id, echoed back.
    tree_id: String,
    /// New cluster ids actually persisted under the commit. For
    /// `NewTree` this is every cluster row created; for `SubtreeOf`
    /// this is the new subtree's cluster ids (already namespaced with
    /// the `recluster-{node_id}-` prefix).
    new_cluster_ids: Vec<String>,
    /// Task ids for submitted `RaptorSummarize` tasks. Empty when
    /// `submit_naming: false`.
    task_ids: Vec<String>,
}

/// Submit one `RaptorSummarize` task per cluster row in `new_cluster_ids`
/// whose persisted `user_edited_name` flag is `false`. Shared by both
/// confirm-and-name paths so the post-commit fan-out behavior is
/// identical (same priority, same metadata shape).
///
/// When `submit_naming` is `false` (the "Confirm (no naming)" button —
/// see `cluster-review-tab-confirm-skip-naming`) the function returns
/// an empty `Vec` without touching the queue.
///
/// When `new_cluster_ids` is `None`, every cluster row in the tree is
/// considered (the new-tree case persists the whole tree, so the filter
/// is just "kind == Cluster && !user_edited_name").
async fn submit_naming_tasks(
    trees: &hiker_core::trees::Trees,
    queue: &hiker_core::tasks::Queue,
    tree_id: &str,
    new_cluster_ids: Option<&[String]>,
    submit_naming: bool,
) -> CmdResult<Vec<String>> {
    let mut task_ids: Vec<String> = Vec::new();
    if !submit_naming {
        return Ok(task_ids);
    }
    let restrict: Option<std::collections::HashSet<&str>> =
        new_cluster_ids.map(|ids| ids.iter().map(|s| s.as_str()).collect());
    let nodes = trees.list_nodes(tree_id).map_err(|e| e.to_string())?;
    for n in nodes {
        if !matches!(n.kind, hiker_core::trees::NodeKind::Cluster) {
            continue;
        }
        if n.user_edited_name {
            continue;
        }
        if let Some(allowed) = restrict.as_ref()
            && !allowed.contains(n.id.as_str())
        {
            continue;
        }
        let task = hiker_core::tasks::Task {
            id: String::new(),
            kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                tree_id: tree_id.to_string(),
                cluster_node_id: n.id.clone(),
                level: 0,
            },
            priority: hiker_core::tasks::Priority::Normal,
            shape: hiker_core::tasks::TaskShape::Direct,
            payload: hiker_core::tasks::TaskPayload::default(),
            output_schema: None,
            submitted_at: std::time::SystemTime::now(),
            metadata: serde_json::json!({
                "tree_id": tree_id,
                "cluster_node_id": n.id,
            }),
        };
        let handle = queue.submit(task).await;
        task_ids.push(handle.id.clone());
    }
    Ok(task_ids)
}

/// Unified commit path for both the new-tree persist flow
/// (`cluster_persist_built_tree`) and the recluster-from-built flow
/// (`cluster_op_recluster_subtree_from_built`).
///
/// Dispatches on `target`: `NewTree` inserts a fresh tree + nodes
/// (applying user-renamed names + `user_edited_name = true` on the way
/// in); `SubtreeOf` runs the recluster planner + apply path against an
/// existing tree, snapshots the prior subtree, and records a history
/// row for undo. Both branches end with the shared
/// `submit_naming_tasks` fan-out so a non-user-renamed cluster gets a
/// `RaptorSummarize` task either way.
async fn commit_built_tree(
    trees: &hiker_core::trees::Trees,
    queue: &hiker_core::tasks::Queue,
    target: CommitTarget,
    inputs: CommitInputs<'_>,
) -> CmdResult<CommitOutcome> {
    match target {
        CommitTarget::NewTree {
            name,
            source,
            scope_json,
            method_json,
        } => {
            // Insert tree + nodes.
            let tree_id = trees
                .insert_tree(hiker_core::trees::TreeInsert {
                    id: None,
                    name,
                    source,
                    state: "draft".to_string(),
                    scope_json,
                    method_json,
                    vault_snapshot: None,
                })
                .map_err(|e| CmdError::from(format!("insert_tree: {e}")))?;
            let mut inserts = hiker_core::cluster::result_to_node_inserts_pub(inputs.tree);
            // Apply user-renamed names + the `user_edited_name = 1` flag.
            for ins in &mut inserts {
                if let Some(new_name) = inputs.user_renamed.get(&ins.node_id) {
                    ins.name = new_name.clone();
                    ins.user_edited_name = true;
                }
            }
            trees
                .insert_nodes(&tree_id, &inserts)
                .map_err(|e| CmdError::from(format!("insert_nodes: {e}")))?;
            // Collect the persisted cluster ids for the outcome. The
            // new-tree path treats every cluster row in the freshly
            // inserted tree as "new", which matches the
            // `submit_naming_tasks` default (None → unrestricted).
            let new_cluster_ids: Vec<String> = inserts
                .iter()
                .filter(|i| matches!(i.kind, hiker_core::trees::NodeKind::Cluster))
                .map(|i| i.node_id.clone())
                .collect();
            // Submit one RaptorSummarize task per cluster node whose name
            // is not user-edited. Mirrors `cluster_regenerate_names`.
            // status: cluster-review-tab-confirm-skip-naming
            // When the caller asks to skip the naming pass (the "Confirm
            // (no naming)" button), bypass the queue submission entirely
            // and return an empty `task_ids` list. The tree persists with
            // placeholder names; the user can run "Regenerate names" from
            // the cluster pane to fill them in later.
            let task_ids =
                submit_naming_tasks(trees, queue, &tree_id, None, inputs.submit_naming).await?;
            Ok(CommitOutcome {
                tree_id,
                new_cluster_ids,
                task_ids,
            })
        }
        CommitTarget::SubtreeOf {
            tree_id,
            node_id,
            carry_policies_down,
        } => {
            let all = trees.list_nodes(&tree_id).map_err(|e| e.to_string())?;
            let index = TreeIndex::build(&all);
            let root_node = index
                .get(&node_id)
                .ok_or_else(|| CmdError::from(format!("node not found: {}", node_id)))?;
            if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
                return Err(CmdError::from("recluster only works on cluster nodes"));
            }
            let (descendant_clusters, descendant_leaves) = {
                let (cs, ls) = index.descendants_of(&node_id);
                (
                    cs.into_iter().cloned().collect::<Vec<_>>(),
                    ls.into_iter().cloned().collect::<Vec<_>>(),
                )
            };
            let resolved_policy = index.inherited_policy_of(&node_id).cloned();
            let prior_subtree = snapshot_prior_subtree(&descendant_clusters);
            let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
                .iter()
                .map(|l| (l.id.clone(), l.parent.clone()))
                .collect();

            let ns = format!("recluster-{}", node_id);
            let plan = plan_recluster_from_built(PlanFromBuiltArgs {
                tree: inputs.tree,
                node_id: &node_id,
                descendant_leaves: &descendant_leaves,
                carry_policies_down,
                resolved_policy: resolved_policy.as_ref(),
                user_renamed: inputs.user_renamed,
                ns: &ns,
            });
            let new_nodes_snapshot = plan.new_nodes_snapshot;
            let new_cluster_ids = plan.new_cluster_ids;
            let leaf_moves = plan.leaf_moves;

            let preserved_chain: Vec<(String, u32)> = index
                .ancestor_chain(&node_id)
                .into_iter()
                .map(|(id, churn)| (id.to_string(), churn))
                .collect();
            apply_recluster_writes_with_user_edits(
                trees,
                &tree_id,
                &descendant_clusters,
                &new_nodes_snapshot,
                &leaf_moves,
                &preserved_chain,
                &new_cluster_ids,
            )?;

            let prior_subtree_json = snapshots_to_json_values(&prior_subtree)?;
            let new_nodes_json = snapshots_to_json_values(&new_nodes_snapshot)?;
            trees
                .record_recluster_subtree(
                    &tree_id,
                    &node_id,
                    &prior_subtree_json,
                    &prior_leaf_parents,
                    &new_nodes_json,
                    &leaf_moves,
                    if carry_policies_down {
                        resolved_policy.as_ref()
                    } else {
                        None
                    },
                )
                .map_err(|e| e.to_string())?;

            // Submit RaptorSummarize tasks for the new clusters that aren't
            // user-renamed. When the caller asks to skip the naming pass (the
            // "Confirm (no naming)" button for the recluster path, mirroring
            // `cluster-review-tab-confirm-skip-naming`), bypass the queue
            // submission entirely and return an empty `task_ids` list. The
            // user can run "Regenerate names" from the cluster pane later.
            let task_ids = submit_naming_tasks(
                trees,
                queue,
                &tree_id,
                Some(&new_cluster_ids),
                inputs.submit_naming,
            )
            .await?;
            Ok(CommitOutcome {
                tree_id,
                new_cluster_ids,
                task_ids,
            })
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClusterPersistArgs {
    name: String,
    /// "one-shot" | "saved-triage" — lifecycle hint, mirrors
    /// `cluster_tree_create`.
    #[serde(default = "default_source_oneshot")]
    source: String,
    scope_json: String,
    method_json: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    /// Map of build-pass cluster id → user-supplied name. User-renamed
    /// nodes land with `user_edited_name = 1` and skip the LLM naming
    /// pass.
    #[serde(default)]
    user_renamed: std::collections::HashMap<String, String>,
    /// When false, the persist call skips the `RaptorSummarize` task
    /// submission step — the tree lands with placeholder names
    /// (`"Cluster N"`) intact, and the user can run "Regenerate names"
    /// later from the cluster pane to fill in LLM-generated names.
    /// Default true preserves the original "Confirm and name" behavior
    /// for callers that don't set the flag.
    #[serde(default = "default_true")]
    submit_naming: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClusterPersistResult {
    tree_id: String,
    /// Submitted task ids (one `RaptorSummarize` per un-renamed cluster).
    task_ids: Vec<String>,
}

// status: cluster-review-tab-confirm-and-name
#[tauri::command]
pub(crate) async fn cluster_persist_built_tree(
    state: State<'_, AppState>,
    args: ClusterPersistArgs,
) -> CmdResult<ClusterPersistResult> {
    let result = async {
        let (trees, queue) =
            with_session(&state, |s| Ok((s.trees.clone(), s.tasks.clone())))?;
        let outcome = commit_built_tree(
            &trees,
            &queue,
            CommitTarget::NewTree {
                name: args.name,
                source: args.source,
                scope_json: args.scope_json,
                method_json: args.method_json,
            },
            CommitInputs {
                tree: &args.tree,
                user_renamed: &args.user_renamed,
                submit_naming: args.submit_naming,
            },
        )
        .await?;
        Ok::<_, CmdError>(ClusterPersistResult {
            tree_id: outcome.tree_id,
            task_ids: outcome.task_ids,
        })
    }
    .await;
    log_cmd_result("cluster_persist_built_tree", result)
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClusterReclusterFromBuiltArgs {
    tree_id: String,
    node_id: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    #[serde(default)]
    carry_policies_down: bool,
    /// Map of build-pass cluster id → user-supplied name (rename-before-Confirm).
    #[serde(default)]
    user_renamed: std::collections::HashMap<String, String>,
    /// When false, the recluster persist step skips the `RaptorSummarize`
    /// task submission — the new subtree lands with the build-pass
    /// placeholder cluster names intact and the user can run
    /// "Regenerate names" from the cluster pane later. Mirrors the
    /// `submit_naming` flag on `cluster_persist_built_tree`. Default true
    /// preserves the original "Confirm and name →" behavior for callers
    /// that don't set the flag.
    #[serde(default = "default_true")]
    submit_naming: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClusterReclusterFromBuiltResult {
    new_cluster_ids: Vec<String>,
    task_ids: Vec<String>,
}

// status: cluster-review-tab-confirm-and-name (recluster branch)
//
// Replace the selected subtree with the pre-built structural tree. The
// clustering already ran; this command only persists. Mirrors
// `recluster_subtree_in_worker`'s replace-subtree shape, minus the LLM
// summarizer call. Submits one `RaptorSummarize` task per non-user-renamed
// new cluster.
#[tauri::command]
pub(crate) async fn cluster_op_recluster_subtree_from_built(
    state: State<'_, AppState>,
    args: ClusterReclusterFromBuiltArgs,
) -> CmdResult<ClusterReclusterFromBuiltResult> {
    let result = async {
        let (trees, queue) =
            with_session(&state, |s| Ok((s.trees.clone(), s.tasks.clone())))?;
        let outcome = commit_built_tree(
            &trees,
            &queue,
            CommitTarget::SubtreeOf {
                tree_id: args.tree_id,
                node_id: args.node_id,
                carry_policies_down: args.carry_policies_down,
            },
            CommitInputs {
                tree: &args.tree,
                user_renamed: &args.user_renamed,
                submit_naming: args.submit_naming,
            },
        )
        .await?;
        Ok::<_, CmdError>(ClusterReclusterFromBuiltResult {
            new_cluster_ids: outcome.new_cluster_ids,
            task_ids: outcome.task_ids,
        })
    }
    .await;
    log_cmd_result("cluster_op_recluster_subtree_from_built", result)
}
