// ---------- cluster editor (Sprint B) ----------
//
// Backing IPC for `docs/cluster-editor.md`. Every command thinks at the
// `Trees`-shape level: trees + nodes + history; the cluster build pass
// resolves a `BuildScope` against the read store before delegating to
// `core::cluster::build_and_persist`. UI surface lives in
// `ui/src/clusterEditor/`.
//
// status: cluster-editor-sidebar-mode

use serde::{Deserialize, Serialize};
use tauri::State;

use hiker_core::store::Store;

use crate::{log_cmd_result, with_session, AppState, CmdError, CmdResult, VaultSession};

pub mod recluster;
pub mod review_tab;
pub mod undo_redo;

// Re-exports so `cmds::cluster::*` in lib.rs continues to see every
// Tauri command name regardless of which submodule actually defines it.
pub(crate) use recluster::recluster_subtree_in_worker;
pub(crate) use review_tab::{
    cluster_op_recluster_subtree_from_built, cluster_persist_built_tree, cluster_run_structural,
};
pub(crate) use undo_redo::{cluster_tree_redo, cluster_tree_undo};

#[derive(Debug, Serialize)]
pub(crate) struct ClusterTreeRowDto {
    id: String,
    name: String,
    source: String,
    state: String,
    scope_json: String,
    method_json: String,
    created_at_ms: i64,
    vault_snapshot: Option<String>,
}

impl From<hiker_core::trees::TreeRow> for ClusterTreeRowDto {
    fn from(r: hiker_core::trees::TreeRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            source: r.source,
            state: r.state,
            scope_json: r.scope_json,
            method_json: r.method_json,
            created_at_ms: r.created_at_ms,
            vault_snapshot: r.vault_snapshot,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ClusterNodeDto {
    id: String,
    parent: Option<String>,
    kind: String,
    note_ref: Option<String>,
    note_path: Option<String>,
    note_title: Option<String>,
    name: String,
    summary: String,
    user_edited_name: bool,
    user_edited_summary: bool,
    policy_json: Option<String>,
    confidence: f32,
    summary_membership_churn: u32,
}

fn enrich_node(
    n: hiker_core::trees::EditableNode,
    store: &Store,
) -> ClusterNodeDto {
    let (note_path, note_title) = match &n.note_ref {
        Some(id) => match store.path_for_id(id) {
            Ok(Some(p)) => {
                let title = title_from_rel_path(&p);
                (Some(p), Some(title))
            }
            _ => (None, None),
        },
        None => (None, None),
    };
    let kind = match n.kind {
        hiker_core::trees::NodeKind::Cluster => "cluster",
        hiker_core::trees::NodeKind::Leaf => "leaf",
        hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
    };
    let policy_json = n
        .policy
        .as_ref()
        .and_then(|p| serde_json::to_string(p).ok());
    ClusterNodeDto {
        id: n.id,
        parent: n.parent,
        kind: kind.to_string(),
        note_ref: n.note_ref,
        note_path,
        note_title,
        name: n.name,
        summary: n.summary,
        user_edited_name: n.user_edited_name,
        user_edited_summary: n.user_edited_summary,
        policy_json,
        confidence: n.confidence,
        summary_membership_churn: n.summary_membership_churn,
    }
}

pub(crate) fn title_from_rel_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    last.strip_suffix(".md").unwrap_or(last).to_string()
}

/// Shared scaffolding for the "thin trees mutation" cluster commands:
/// acquire the session, run a single `Trees` method, route its error
/// through `CmdError::from(e.to_string())`, and run the final Result
/// through `log_cmd_result`. Equivalent to the hand-written
/// `with_session(&state, |s| s.trees.X(...).map_err(|e| e.to_string())?...)`
/// + `log_cmd_result(name, result)` pair every cluster_* mutation used
/// to repeat verbatim.
fn with_trees<R, E, F>(
    state: &State<'_, AppState>,
    cmd_name: &'static str,
    f: F,
) -> CmdResult<R>
where
    E: std::fmt::Display,
    F: FnOnce(&hiker_core::trees::Trees) -> Result<R, E>,
{
    let result = with_session(state, |session| {
        f(&session.trees).map_err(|e| CmdError::from(e.to_string()))
    });
    log_cmd_result(cmd_name, result)
}

// status: cluster-editor-triage-scheduled-rerun
//
// Best-effort parser for `[suggestions.triage].scheduled_rerun`. Sprint F
// supports simple duration suffixes (`s`/`m`/`h`/`d`); cron expressions
// (e.g. `"0 3 * * *"`) return `None` and are logged at startup so the
// user knows the value was unsupported. The cron parser proper is a
// follow-up — adding a dep just for Sprint F's lowest-priority slug is
// not justified.
pub(crate) fn parse_rerun_interval(s: &str) -> Option<std::time::Duration> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (num_part, unit) = match trimmed.chars().last() {
        Some(c) if "smhdSMHD".contains(c) => (&trimmed[..trimmed.len() - 1], c),
        _ => return None,
    };
    let n: u64 = num_part.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let secs = match unit.to_ascii_lowercase() {
        's' => n,
        'm' => n.checked_mul(60)?,
        'h' => n.checked_mul(3600)?,
        'd' => n.checked_mul(86400)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

// status: cluster-summarize-llm
//
// Build the LLM-backed cluster summarizer the build pipeline hands to
// `core::cluster::build_tree`. Reads the `cluster_summarize` prompt
// body (user/vault-scoped per `core::prompts`) and constructs a fresh
// `GraniteLlmClient` from the live `[llm]` config. Errors when LLM is
// disabled — there is no fallback path.
// Common path for the three cluster tauri commands: pull the queue
// handle off the session, submit a Task carrying the requested
// `TaskKind`, and return its id immediately. The direct worker
// dispatches into `DirectWorkerHandlers::try_handle` on its own
// thread; the IPC reply is sub-millisecond.
async fn submit_cluster_task(
    state: &State<'_, AppState>,
    kind: hiker_core::tasks::TaskKind,
) -> Result<String, String> {
    let tasks = with_session(state, |s| Ok(s.tasks.clone())).map_err(|e| e.to_string())?;
    let metadata = serde_json::json!({
        "variant": kind.variant_name(),
        "summary": kind.metadata_oneliner(),
    });
    let task = hiker_core::tasks::Task {
        id: String::new(),
        kind,
        priority: hiker_core::tasks::Priority::Normal,
        shape: hiker_core::tasks::TaskShape::Direct,
        payload: hiker_core::tasks::TaskPayload::default(),
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata,
    };
    let handle = tasks.submit(task).await;
    Ok(handle.id.clone())
}

// status: cluster-editor-sidebar-mode, cluster-editor-multiple-trees-open
#[tauri::command]
pub(crate) fn cluster_trees_list(state: State<'_, AppState>) -> CmdResult<Vec<ClusterTreeRowDto>> {
    let result = with_session(&state, |session| {
        let rows = session.trees.list_trees().map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    });
    log_cmd_result("cluster_trees_list", result)
}

// status: cluster-editor-sidebar-mode
#[tauri::command]
pub(crate) fn cluster_tree_get(
    state: State<'_, AppState>,
    tree_id: String,
) -> CmdResult<Vec<ClusterNodeDto>> {
    let result = with_session(&state, |session| {
        let nodes = session
            .trees
            .list_nodes(&tree_id)
            .map_err(|e| e.to_string())?;
        let store = session.read_store.lock()?;
        Ok(nodes.into_iter().map(|n| enrich_node(n, &store)).collect())
    });
    log_cmd_result("cluster_tree_get", result)
}

// status: cluster-editor-new-tree-action, cluster-editor-tree-creation-modal,
//         cluster-editor-build-scope-picker, cluster-editor-build-params-advanced-disclosure
#[derive(Debug, Deserialize)]
pub(crate) struct ClusterTreeCreateArgs {
    name: String,
    /// "one-shot" | "saved-triage" (lifecycle hint).
    #[serde(default = "default_source_oneshot")]
    source: String,
    /// JSON of `core::cluster::BuildScope`. Resolved against the read
    /// store on the backend.
    scope_json: String,
    /// JSON of `core::cluster::BuildMethod`. Carries params inside.
    method_json: String,
}

pub(super) fn default_source_oneshot() -> String {
    "one-shot".into()
}

#[tauri::command]
pub(crate) async fn cluster_tree_create(
    state: State<'_, AppState>,
    args: ClusterTreeCreateArgs,
) -> CmdResult<String> {
    // Submit a ClusterBuildTree task to the queue. The direct worker's
    // non-LLM dispatch arm (`DirectWorkerHandlers::try_handle`) does the
    // actual build; the IPC reply lands as soon as the row is enqueued
    // so the UI stays responsive and the queue page surfaces progress.
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterBuildTree {
            name: args.name,
            source: args.source,
            scope_json: args.scope_json,
            method_json: args.method_json,
        },
    )
    .await
    .map_err(CmdError::from);
    log_cmd_result("cluster_tree_create", result)
}

// status: cluster-build-rebuild
//
// Re-run the original build pipeline for `tree_id` against the current
// vault state. Produces a new draft tree row; the old tree is left
// intact so the user can compare / discard. User-edited names + summaries
// + policies on the old tree's clusters are preserved onto new clusters
// whose member-set Jaccard exceeds the merge threshold (0.5 default).
#[tauri::command]
pub(crate) async fn cluster_tree_rebuild(
    state: State<'_, AppState>,
    tree_id: String,
    new_name: Option<String>,
) -> CmdResult<String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterRebuildTree { tree_id, new_name },
    )
    .await
    .map_err(CmdError::from);
    log_cmd_result("cluster_tree_rebuild", result)
}

// status: cluster-editor-discard-draft
#[tauri::command]
pub(crate) fn cluster_tree_discard(state: State<'_, AppState>, tree_id: String) -> CmdResult<()> {
    let result = with_session(&state, |session| {
        session.trees.delete_tree(&tree_id).map_err(|e| e.to_string())?;
        Ok(())
    });
    log_cmd_result("cluster_tree_discard", result)
}

// status: cluster-editor-edit-name-summary
#[tauri::command]
pub(crate) fn cluster_node_rename(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    name: String,
) -> CmdResult<()> {
    with_trees(&state, "cluster_node_rename", |t| {
        t.rename(&tree_id, &node_id, &name)
    })
}

#[tauri::command]
pub(crate) fn cluster_node_set_summary(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    summary: String,
) -> CmdResult<()> {
    with_trees(&state, "cluster_node_set_summary", |t| {
        t.set_summary(&tree_id, &node_id, &summary)
    })
}

// status: cluster-editor-move-note-between-clusters, cluster-editor-promote-outlier
#[tauri::command]
pub(crate) fn cluster_node_move(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    new_parent: Option<String>,
) -> CmdResult<()> {
    with_trees(&state, "cluster_node_move", |t| {
        t.move_node(&tree_id, &node_id, new_parent.as_deref())
    })
}

// status: cluster-editor-merge-siblings
#[tauri::command]
pub(crate) fn cluster_op_merge_siblings(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
) -> CmdResult<String> {
    with_trees(&state, "cluster_op_merge_siblings", |t| {
        t.merge_siblings(&tree_id, &node_ids)
    })
}

// status: cluster-editor-merge-children-up
#[tauri::command]
pub(crate) fn cluster_op_merge_children_up(
    state: State<'_, AppState>,
    tree_id: String,
    parent_id: String,
) -> CmdResult<()> {
    with_trees(&state, "cluster_op_merge_children_up", |t| {
        t.merge_children_up(&tree_id, &parent_id)
    })
}

// status: cluster-editor-drop-cluster
#[tauri::command]
pub(crate) fn cluster_op_drop_cluster(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    outlier_bucket_id: String,
) -> CmdResult<()> {
    with_trees(&state, "cluster_op_drop_cluster", |t| {
        t.drop_cluster(&tree_id, &node_id, &outlier_bucket_id)
    })
}

// status: cluster-editor-promote-outlier
#[tauri::command]
pub(crate) fn cluster_op_promote_outlier(
    state: State<'_, AppState>,
    tree_id: String,
    leaf_id: String,
    new_parent: Option<String>,
) -> CmdResult<()> {
    with_trees(&state, "cluster_op_promote_outlier", |t| {
        t.promote_outlier(&tree_id, &leaf_id, new_parent.as_deref())
    })
}

// status: cluster-editor-split-cluster
// status: cluster-op-split
//
// Run HDBSCAN against just this cluster's leaf members; insert one new
// sub-cluster per HDBSCAN label, and re-parent each leaf onto its new
// sub-cluster. Thin wrapper around `Trees::split_cluster`, which owns
// the partitioning + persistence + history logic (per the ops-framework
// migration). The user-facing one-shot Split affordance passes
// `recurse = false` so the behavior matches the legacy command; the
// build recipe (forthcoming) wires `recurse = true`.
#[tauri::command]
pub(crate) fn cluster_op_split(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
) -> CmdResult<Vec<String>> {
    let result = with_session(&state, |session| {
        // Snapshot a path-resolving closure over the read-side store so
        // `Trees::split_cluster` doesn't need to import `core::store`.
        // Holding the store lock for the duration of one Split is fine —
        // splits are infrequent + fast at vault scale.
        let store = session.read_store.lock()?;
        let embed_for_leaf = |note_id: &str| -> Option<Vec<f32>> {
            let path = store.path_for_id(note_id).ok().flatten()?;
            store.note_embedding_for_path(&path).ok().flatten()
        };
        let params = hiker_core::cluster::ClusterParams {
            // recurse defaults to false; user-driven Split is one-level.
            ..Default::default()
        };
        let outcome = session
            .trees
            .split_cluster(&tree_id, Some(&node_id), &params, &embed_for_leaf, None)
            .map_err(|e| e.to_string())?;
        Ok(outcome.new_clusters)
    });
    log_cmd_result("cluster_op_split", result)
}

// status: cluster-editor-recluster-subtree
// status: cluster-editor-recluster-subtree-policy-loss
// status: cluster-editor-recluster-subtree-placement-decoupled
//
// Re-run the full recursive cluster build pipeline against just the
// selected node's leaf descendants, then replace the subtree in place.
// The selected node's own row is preserved (id, name, summary,
// user-edit flags, policy); every descendant cluster row is deleted,
// freshly-built cluster nodes are inserted under the selected node,
// and the surviving leaves re-parent onto their new positions.
//
// Differs from `cluster_op_split`: split is one-level (one HDBSCAN pass
// produces a single new layer of children); recluster runs the full
// recursive build_tree pipeline so every level beneath the selected
// node is rebuilt. Always emits a `Cluster`-shaped subtree regardless
// of the surrounding tree's method (matches Split's behavior per
// `cluster-build-from-folders-uniform-output`).
//
// The reshape is structural only — it does not touch the filesystem.
// Already-placed notes stay where they are on disk; future triage
// classifications use the new structure (per
// `cluster-editor-recluster-subtree-placement-decoupled`).
#[derive(Debug, Deserialize)]
pub(crate) struct ClusterOpReclusterArgs {
    tree_id: String,
    node_id: String,
    /// JSON of `core::cluster::ClusterParams`. UI builds this from the
    /// advanced disclosure (with `min_cluster_size` halved by default).
    cluster_params_json: String,
    /// When true, copy the selected node's resolved policy onto every
    /// new direct child as an explicit policy. Default off per spec.
    #[serde(default)]
    carry_policies_down: bool,
}

#[tauri::command]
pub(crate) async fn cluster_op_recluster_subtree(
    state: State<'_, AppState>,
    args: ClusterOpReclusterArgs,
) -> CmdResult<String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterReclusterSubtree {
            tree_id: args.tree_id,
            node_id: args.node_id,
            cluster_params_json: args.cluster_params_json,
            carry_policies_down: args.carry_policies_down,
        },
    )
    .await
    .map_err(CmdError::from);
    log_cmd_result("cluster_op_recluster_subtree", result)
}

/// Resolve a `BuildScope` to a `Vec<NoteInput>` by walking the read
/// store. Lazy-populates missing note embeddings. Standalone twin of
/// `DirectWorkerHandlers::notes_for_scope` for the IPC-side commands.
pub(super) fn notes_for_scope_via_session(
    session: &VaultSession,
    scope: &hiker_core::cluster::BuildScope,
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let mut store = session.read_store.lock().map_err(|e| e.to_string())?;
    let candidate_paths: Vec<String> = match scope {
        hiker_core::cluster::BuildScope::Vault { .. } => {
            store.all_note_paths().map_err(|e| e.to_string())?
        }
        hiker_core::cluster::BuildScope::Folder { rel, .. } => {
            let prefix = if rel.ends_with('/') || rel.is_empty() {
                rel.clone()
            } else {
                format!("{rel}/")
            };
            store
                .all_note_paths()
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|p| prefix.is_empty() || p.starts_with(&prefix))
                .collect()
        }
        hiker_core::cluster::BuildScope::Notes { ids, .. } => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Ok(Some(p)) = store.path_for_id(id) {
                    out.push(p);
                }
            }
            out
        }
    };
    // status: cluster-build-scope-source-types
    let mut notes: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for path in candidate_paths {
        if !scope.matches_path(&path) {
            continue;
        }
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let note_id = match store.id_for_path(&path) {
            Ok(Some(i)) => i,
            _ => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
        notes.push(hiker_core::cluster::NoteInput {
            id: note_id,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    Ok(notes)
}

/// For the recluster-subtree case: walk the descendants of
/// `(tree_id, node_id)` and pull their leaves' embeddings the same way
/// `recluster_subtree_in_worker` does. Returns the resolved note inputs
/// (which the structural build will operate on).
pub(super) fn notes_for_recluster_target(
    session: &VaultSession,
    tree_id: &str,
    node_id: &str,
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let all = session
        .trees
        .list_nodes(tree_id)
        .map_err(|e| e.to_string())?;
    let mut children_by_parent: std::collections::HashMap<
        String,
        Vec<hiker_core::trees::EditableNode>,
    > = std::collections::HashMap::new();
    for n in all.iter().cloned() {
        if let Some(p) = n.parent.clone() {
            children_by_parent.entry(p).or_default().push(n);
        }
    }
    let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut stack = vec![node_id.to_string()];
    while let Some(id) = stack.pop() {
        if let Some(kids) = children_by_parent.get(&id) {
            for k in kids {
                match k.kind {
                    hiker_core::trees::NodeKind::Leaf => descendant_leaves.push(k.clone()),
                    _ => stack.push(k.id.clone()),
                }
            }
        }
    }
    let mut store = session.read_store.lock().map_err(|e| e.to_string())?;
    let mut note_inputs: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for l in &descendant_leaves {
        let Some(nid) = l.note_ref.clone() else { continue };
        let Ok(Some(path)) = store.path_for_id(&nid) else { continue };
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
        note_inputs.push(hiker_core::cluster::NoteInput {
            id: nid,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    Ok(note_inputs)
}

pub(super) fn default_true() -> bool {
    true
}

// status: cluster-editor-set-policy
#[tauri::command]
pub(crate) fn cluster_node_set_policy(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    policy_json: Option<String>,
) -> CmdResult<()> {
    let parsed: CmdResult<Option<hiker_core::trees::NodePolicy>> = match policy_json {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).map(Some).map_err(Into::into),
        _ => Ok(None),
    };
    let policy = match parsed {
        Ok(p) => p,
        Err(e) => return log_cmd_result("cluster_node_set_policy", Err(e)),
    };
    with_trees(&state, "cluster_node_set_policy", |t| {
        t.set_policy(&tree_id, &node_id, policy)
    })
}

// status: cluster-editor-apply-action
// status: suggestions-apply-cmd
//
// Walk the tree, resolve each leaf's effective policy via the walk-up
// rule, and emit one `staging.db` row per `Tag` / `Move` leaf. Returns
// the produced staging row ids + per-bucket counts (skipped /
// unpolicied / frozen) for the batch-review pane header.
#[tauri::command]
pub(crate) fn cluster_apply(
    state: State<'_, AppState>,
    tree_id: String,
) -> CmdResult<hiker_core::suggest::ApplyOutcome> {
    let result = with_session(&state, |session| {
        let store = session.read_store.lock()?;
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        Ok(hiker_core::suggest::apply_tree(
            &session.trees,
            &tree_id,
            &session.vault,
            &store,
            &session.staging,
            Some(&history),
        )
        .map_err(|e| e.to_string())?)
    });
    log_cmd_result("cluster_apply", result)
}

// status: cluster-editor-multi-select-stage-move
#[tauri::command]
pub(crate) fn cluster_stage_moves(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    target_folder: String,
) -> CmdResult<Vec<String>> {
    let result = with_session(&state, |session| {
        let store = session.read_store.lock()?;
        Ok(hiker_core::suggest::stage_moves(
            &session.trees,
            hiker_core::suggest::StageMoveArgs {
                tree_id: &tree_id,
                node_ids: &node_ids,
                target_folder: &target_folder,
            },
            &store,
            &session.staging,
        )
        .map_err(|e| e.to_string())?)
    });
    log_cmd_result("cluster_stage_moves", result)
}

// status: cluster-editor-multi-select-stage-tag
#[tauri::command]
pub(crate) fn cluster_stage_tags(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    tag_slug: String,
) -> CmdResult<Vec<String>> {
    let result = with_session(&state, |session| {
        let store = session.read_store.lock()?;
        Ok(hiker_core::suggest::stage_tags(
            &session.trees,
            hiker_core::suggest::StageTagArgs {
                tree_id: &tree_id,
                node_ids: &node_ids,
                tag_slug: &tag_slug,
            },
            &session.vault,
            &store,
            &session.staging,
        )
        .map_err(|e| e.to_string())?)
    });
    log_cmd_result("cluster_stage_tags", result)
}

// status: cluster-editor-sapling-evergreen-lifecycle, cluster-editor-apply-action
// Free-string setter for the tree's lifecycle state. Sprint C uses
// `"draft"` / `"applied"`; Sprint D adds `"saved-as-triage"`.
#[tauri::command]
pub(crate) fn cluster_tree_set_state(
    state: State<'_, AppState>,
    tree_id: String,
    new_state: String,
) -> CmdResult<()> {
    let result = with_session(&state, |session| {
        session
            .trees
            .set_tree_state(&tree_id, &new_state)
            .map_err(|e| e.to_string())?;
        Ok(())
    });
    log_cmd_result("cluster_tree_set_state", result)
}

// status: cluster-editor-triage-on-save
// status: cluster-editor-triage-via-staging
// status: triage-classifier-engine
// status: triage-staging-proposals
//
// Synchronous triage trigger. Resolves the note's embedding from the
// store, walks every `saved-as-triage` tree, and emits one staging row
// per matched policy. Returns the per-tree outcomes so the caller can
// log + toast appropriately. The async path (RaptorTriageMatch task)
// is wrapper'd by `cluster_triage_enqueue` below; both share this
// classifier.
#[tauri::command]
pub(crate) fn cluster_triage_run(
    state: State<'_, AppState>,
    rel: String,
    author_class: Option<String>,
) -> CmdResult<Vec<hiker_core::suggest::TriageOutcome>> {
    let result = with_session(&state, |session| {
        let store = session.read_store.lock()?;
        let note_id = store
            .id_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| CmdError::from(format!("note not indexed: {rel}")))?;
        let embedding = store
            .note_embedding_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| CmdError::from(format!("no embedding for {rel}")))?;
        let cfg_triage = session.config.read().expect("config lock poisoned")
            .suggestions
            .triage
            .clone();
        let opts = hiker_core::suggest::TriageOpts {
            review_required: cfg_triage.review_required,
            scope: cfg_triage.scope.clone(),
            beam_width: 2,
        };
        let ac = match author_class.as_deref() {
            Some("agent") => hiker_core::suggest::NoteAuthorClass::Agent,
            _ => hiker_core::suggest::NoteAuthorClass::User,
        };
        Ok(hiker_core::suggest::triage_all_saved_trees(
            hiker_core::suggest::TriageBatch {
                trees: &session.trees,
                vault: &session.vault,
                store: &store,
                staging: &session.staging,
                note_id: &note_id,
                source_path: &rel,
                embedding: &embedding,
                author_class: ac,
                opts: &opts,
            },
        )
        .map_err(|e| e.to_string())?)
    });
    log_cmd_result("cluster_triage_run", result)
}

// status: cluster-editor-triage-via-task-queue
//
// Async triage trigger: enqueues one `RaptorTriageMatch` task per
// saved-as-triage tree. The worker pool drains the queue and emits
// staging rows via the same classifier as `cluster_triage_run`. Returns
// the queued task ids so the caller can correlate them with queue
// events.
#[tauri::command]
pub(crate) async fn cluster_triage_enqueue(
    state: State<'_, AppState>,
    rel: String,
) -> CmdResult<Vec<String>> {
    let result = async {
        let (queue, trees) =
            with_session(&state, |s| Ok((s.tasks.clone(), s.trees.clone())))?;
        let tree_rows = trees.list_trees().map_err(|e| e.to_string())?;
        let saved: Vec<String> = tree_rows
            .into_iter()
            .filter(|t| t.state == "saved-as-triage")
            .map(|t| t.id)
            .collect();
        let mut task_ids: Vec<String> = Vec::with_capacity(saved.len());
        for tree_id in saved {
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorTriageMatch {
                    tree_id: tree_id.clone(),
                    source_path: rel.clone(),
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": tree_id,
                    "source_path": rel,
                }),
            };
            let handle = queue.submit(task).await;
            task_ids.push(handle.id.clone());
        }
        Ok::<_, CmdError>(task_ids)
    }
    .await;
    log_cmd_result("cluster_triage_enqueue", result)
}

// status: cluster-editor-regenerate-via-task-queue
// status: cluster-editor-llm-actions-via-task-queue
// status: cluster-op-summarize-sweep
//
// Enqueue one `RaptorSummarize` task per non-user-edited cluster node
// in the tree. Caller-facing affordance is "Regenerate names" on the
// expanded pane's toolbar — the worker writes the new name/summary back
// through `trees.rename` / `trees.set_summary` and resets the node's
// `summary_membership_churn`.
//
// Refactored to call `Trees::plan_summarize_sweep` under the hood so the
// selection / ordering logic is shared with `cluster_summarize`. The
// `Regenerate names` toolbar verb is `scope: All` with no subtree filter.
#[tauri::command]
pub(crate) async fn cluster_regenerate_names(
    state: State<'_, AppState>,
    tree_id: String,
) -> CmdResult<Vec<String>> {
    let params = hiker_core::trees::SummarizeParams {
        scope: hiker_core::trees::SummarizeScope::All,
        subtree_root: None,
        recursive: true,
        summarize_mode: hiker_core::cluster::SummarizeMode::Llm,
        overwrite_user_edited: false,
    };
    let result = cluster_summarize_inner(state, tree_id, params).await;
    log_cmd_result("cluster_regenerate_names", result.map(|o| o.enqueued))
}

// status: cluster-op-summarize-sweep
//
// Generic Summarize-sweep entry point. The `scope_json` argument is a
// serialized `SummarizeScope`; `params_json` carries the rest of
// `SummarizeParams` (subtree_root, recursive, summarize_mode,
// overwrite_user_edited).
#[derive(serde::Serialize, Clone, Debug)]
pub(crate) struct SummarizeSweepOutcome {
    pub enqueued: Vec<String>,
    pub skipped_user_edited: Vec<String>,
    pub skipped_fresh: Vec<String>,
    /// Id of the umbrella `ClusterSummarize` queue row (or empty when
    /// the umbrella was skipped because no targets were selected).
    pub queue_row_id: String,
}

#[tauri::command]
pub(crate) async fn cluster_summarize(
    state: State<'_, AppState>,
    tree_id: String,
    params_json: String,
) -> CmdResult<SummarizeSweepOutcome> {
    let result = async {
        let params: hiker_core::trees::SummarizeParams = serde_json::from_str(&params_json)
            .map_err(|e| CmdError::from(format!("cluster_summarize: parse params: {e}")))?;
        cluster_summarize_inner(state, tree_id, params).await
    }
    .await;
    log_cmd_result("cluster_summarize", result)
}

async fn cluster_summarize_inner(
    state: State<'_, AppState>,
    tree_id: String,
    params: hiker_core::trees::SummarizeParams,
) -> CmdResult<SummarizeSweepOutcome> {
    let (queue, plan) = with_session(&state, |session| {
        let plan = session
            .trees
            .plan_summarize_sweep(&tree_id, &params)
            .map_err(|e| e.to_string())?;
        Ok((session.tasks.clone(), plan))
    })?;
    let mut queue_row_id = String::new();
    if !plan.enqueued.is_empty() {
        // Submit umbrella ClusterSummarize task first at High priority.
        let umbrella = hiker_core::tasks::Task {
            id: String::new(),
            kind: hiker_core::tasks::TaskKind::ClusterSummarize {
                tree_id: tree_id.clone(),
                scope_kind: plan.scope_kind.clone(),
                n_targets: plan.enqueued.len() as u32,
            },
            priority: hiker_core::tasks::Priority::High,
            shape: hiker_core::tasks::TaskShape::Direct,
            payload: hiker_core::tasks::TaskPayload::default(),
            output_schema: None,
            submitted_at: std::time::SystemTime::now(),
            metadata: serde_json::json!({
                "tree_id": tree_id,
                "scope_kind": plan.scope_kind,
                "n_targets": plan.enqueued.len(),
            }),
        };
        let handle = queue.submit(umbrella).await;
        queue_row_id = handle.id.clone();
    }
    // Per-cluster fan-out, in submission order (deepest-first per the
    // plan).
    for cluster_id in &plan.enqueued {
        let task = hiker_core::tasks::Task {
            id: String::new(),
            kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                tree_id: tree_id.clone(),
                cluster_node_id: cluster_id.clone(),
                level: 0,
            },
            priority: hiker_core::tasks::Priority::Normal,
            shape: hiker_core::tasks::TaskShape::Direct,
            payload: hiker_core::tasks::TaskPayload::default(),
            output_schema: None,
            submitted_at: std::time::SystemTime::now(),
            metadata: serde_json::json!({
                "tree_id": tree_id,
                "cluster_node_id": cluster_id,
                "umbrella_id": queue_row_id,
            }),
        };
        let _ = queue.submit(task).await;
    }
    Ok(SummarizeSweepOutcome {
        enqueued: plan.enqueued,
        skipped_user_edited: plan.skipped_user_edited,
        skipped_fresh: plan.skipped_fresh,
        queue_row_id,
    })
}

// status: cluster-op-rollup
//
// Roll up a set of sibling clusters into a new parent cluster. Embeds
// each input's summary live via the indexer's `Arc<dyn Embedder>`, then
// hands off to `Trees::apply_rollup` for partition + persistence.
//
// Returns the `RollupOutcome` so the UI can surface the refusal reason
// (`"all inputs landed in one community"` / `"no inputs merged"`)
// verbatim.
#[tauri::command]
pub(crate) async fn cluster_op_rollup(
    state: State<'_, AppState>,
    tree_id: String,
    params_json: String,
) -> CmdResult<hiker_core::trees::RollupOutcome> {
    let result = async {
        let params: hiker_core::trees::RollupParams = serde_json::from_str(&params_json)
            .map_err(|e| CmdError::from(format!("cluster_op_rollup: parse params: {e}")))?;
        // Validate inputs + collect their summaries. Bail before
        // embedding to avoid a wasted embedder round-trip on bad inputs.
        let (trees, embedder) =
            with_session(&state, |s| Ok((s.trees.clone(), s.indexer.embedder())))?;
        let embedder = embedder
            .ok_or_else(|| CmdError::from("cluster_op_rollup: embedder not yet loaded"))?;
        let inputs = trees
            .validate_rollup_inputs(&tree_id, &params.input_node_ids)
            .map_err(|e| e.to_string())?;
        let summaries: Vec<String> = inputs.iter().map(|i| i.summary.clone()).collect();
        // Embed via spawn_blocking so the fastembed call doesn't park a
        // tokio worker thread (mirrors the indexer's pattern).
        let emb_clone = embedder.clone();
        let summary_embeddings = tokio::task::spawn_blocking(move || emb_clone.embed_batch(&summaries))
            .await
            .map_err(|e| CmdError::from(format!("cluster_op_rollup: join: {e}")))?
            .map_err(|e| CmdError::from(format!("cluster_op_rollup: embed: {e}")))?;
        let outcome = trees
            .apply_rollup(&tree_id, &inputs, &summary_embeddings, &params)
            .map_err(|e| e.to_string())?;
        Ok::<_, CmdError>(outcome)
    }
    .await;
    log_cmd_result("cluster_op_rollup", result)
}

// status: cluster-build-from-folders-live-update
//
// On a vault rename, walk every saved-as-triage `from-folders` tree and
// re-parent the affected leaf. Wired below to the watcher's rename
// event stream alongside the indexer subscription.
#[tauri::command]
pub(crate) fn cluster_folder_rename_update(
    state: State<'_, AppState>,
    rel_from: String,
    rel_to: String,
) -> CmdResult<u32> {
    let result = with_session(&state, |session| {
        let store = session.read_store.lock()?;
        let note_id = match store
            .id_for_path(&rel_to)
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => return Ok(0),
        };
        drop(store);
        let new_folder = rel_to
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        let trees_rows = session.trees.list_trees().map_err(|e| e.to_string())?;
        let mut n = 0u32;
        for t in trees_rows {
            if t.state != "saved-as-triage" {
                continue;
            }
            // Cheap filter: only `from-folders` method trees track the
            // filesystem. Detect via the method JSON's `kind`.
            let is_folders = serde_json::from_str::<serde_json::Value>(&t.method_json)
                .ok()
                .and_then(|v| {
                    v.get("kind")
                        .and_then(|k| k.as_str())
                        .map(|s| s == "from-folders")
                })
                .unwrap_or(false);
            if !is_folders {
                continue;
            }
            let updated = session
                .trees
                .update_for_folder_rename(&t.id, &note_id, &new_folder)
                .map_err(|e| e.to_string())?;
            if updated {
                n += 1;
            }
        }
        let _ = rel_from; // currently unused; kept on the signature so the caller
                          // doesn't have to fish the prior path out of the watcher
                          // event twice.
        Ok(n)
    });
    log_cmd_result("cluster_folder_rename_update", result)
}

// status: suggestions-rejection-history
// Records a rejected cluster-editor row in
// `.hiker/suggestion-history.json`. Called by the batch-review pane
// alongside `staging_reject` for any row whose metadata carries a
// `tree_member_fingerprint`.
#[tauri::command]
pub(crate) fn cluster_record_rejection(
    state: State<'_, AppState>,
    fingerprint: String,
    note_path: String,
    action: String,
) -> CmdResult<()> {
    let result = with_session(&state, |session| {
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        history
            .record_rejection(&fingerprint, &note_path, &action)
            .map_err(|e| e.to_string())?;
        Ok(())
    });
    log_cmd_result("cluster_record_rejection", result)
}
