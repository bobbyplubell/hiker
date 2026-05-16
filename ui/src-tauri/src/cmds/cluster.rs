// ---------- cluster editor (Sprint B) ----------
//
// Backing IPC for `docs/cluster-editor.md`. Every command thinks at the
// `Trees`-shape level: trees + nodes + history; the cluster build pass
// resolves a `BuildScope` against the read store before delegating to
// `core::cluster::build_and_persist`. UI surface lives in
// `ui/src/clusterEditor/`.
//
// status: cluster-editor-sidebar-mode

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::State;

use hiker_core::store::Store;

use crate::{log_cmd_result, AppState, DirectWorkerHandlers, VaultSession};

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
    let tasks = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
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
pub(crate) fn cluster_trees_list(state: State<'_, AppState>) -> Result<Vec<ClusterTreeRowDto>, String> {
    let result = (|| -> Result<Vec<ClusterTreeRowDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let rows = session.trees.list_trees().map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    })();
    log_cmd_result("cluster_trees_list", result)
}

// status: cluster-editor-sidebar-mode
#[tauri::command]
pub(crate) fn cluster_tree_get(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<Vec<ClusterNodeDto>, String> {
    let result = (|| -> Result<Vec<ClusterNodeDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let nodes = session
            .trees
            .list_nodes(&tree_id)
            .map_err(|e| e.to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        Ok(nodes.into_iter().map(|n| enrich_node(n, &store)).collect())
    })();
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

fn default_source_oneshot() -> String {
    "one-shot".into()
}

#[tauri::command]
pub(crate) async fn cluster_tree_create(
    state: State<'_, AppState>,
    args: ClusterTreeCreateArgs,
) -> Result<String, String> {
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
    .await;
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
) -> Result<String, String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterRebuildTree { tree_id, new_name },
    )
    .await;
    log_cmd_result("cluster_tree_rebuild", result)
}

// status: cluster-editor-discard-draft
#[tauri::command]
pub(crate) fn cluster_tree_discard(state: State<'_, AppState>, tree_id: String) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.trees.delete_tree(&tree_id).map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_tree_discard", result)
}

// status: cluster-editor-edit-name-summary
#[tauri::command]
pub(crate) fn cluster_node_rename(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    name: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .rename(&tree_id, &node_id, &name)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_rename", result)
}

#[tauri::command]
pub(crate) fn cluster_node_set_summary(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    summary: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_summary(&tree_id, &node_id, &summary)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_set_summary", result)
}

// status: cluster-editor-move-note-between-clusters, cluster-editor-promote-outlier
#[tauri::command]
pub(crate) fn cluster_node_move(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    new_parent: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .move_node(&tree_id, &node_id, new_parent.as_deref())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_move", result)
}

// status: cluster-editor-merge-siblings
#[tauri::command]
pub(crate) fn cluster_op_merge_siblings(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
) -> Result<String, String> {
    let result = (|| -> Result<String, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .merge_siblings(&tree_id, &node_ids)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_merge_siblings", result)
}

// status: cluster-editor-merge-children-up
#[tauri::command]
pub(crate) fn cluster_op_merge_children_up(
    state: State<'_, AppState>,
    tree_id: String,
    parent_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .merge_children_up(&tree_id, &parent_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_merge_children_up", result)
}

// status: cluster-editor-drop-cluster
#[tauri::command]
pub(crate) fn cluster_op_drop_cluster(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    outlier_bucket_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .drop_cluster(&tree_id, &node_id, &outlier_bucket_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_drop_cluster", result)
}

// status: cluster-editor-promote-outlier
#[tauri::command]
pub(crate) fn cluster_op_promote_outlier(
    state: State<'_, AppState>,
    tree_id: String,
    leaf_id: String,
    new_parent: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .promote_outlier(&tree_id, &leaf_id, new_parent.as_deref())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_promote_outlier", result)
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
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        // Snapshot a path-resolving closure over the read-side store so
        // `Trees::split_cluster` doesn't need to import `core::store`.
        // Holding the store lock for the duration of one Split is fine —
        // splits are infrequent + fast at vault scale.
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
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
    })();
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
) -> Result<String, String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterReclusterSubtree {
            tree_id: args.tree_id,
            node_id: args.node_id,
            cluster_params_json: args.cluster_params_json,
            carry_policies_down: args.carry_policies_down,
        },
    )
    .await;
    log_cmd_result("cluster_op_recluster_subtree", result)
}

// Body of the recluster operation, lifted from the original sync tauri
// command. Runs from inside the direct-worker's non-LLM dispatch
// (`DirectWorkerHandlers::try_handle`) so the LLM-heavy rebuild + the
// tree-mutation pass both happen on the worker thread rather than on
// the IPC channel. Operates on `DirectWorkerHandlers`' refs rather
// than reaching back into the session.
pub(crate) fn recluster_subtree_in_worker(
    handlers: &DirectWorkerHandlers,
    tree_id: &str,
    node_id: &str,
    cluster_params_json: &str,
    carry_policies_down: bool,
) -> Result<serde_json::Value, String> {
    let params: hiker_core::cluster::ClusterParams = serde_json::from_str(cluster_params_json)
        .map_err(|e| format!("cluster_params_json: {e}"))?;

    // Walk the subtree under `node_id` to collect every descendant
    // cluster (for snapshot + deletion) and every leaf (to feed the
    // rebuild and to know prior parents for undo).
    let all = handlers
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
    let mut by_id: std::collections::HashMap<String, hiker_core::trees::EditableNode> =
        std::collections::HashMap::new();
    for n in all.iter().cloned() {
        by_id.insert(n.id.clone(), n);
    }
    let root_node = by_id
        .get(node_id)
        .cloned()
        .ok_or_else(|| format!("node not found: {node_id}"))?;
    if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
        return Err("recluster only works on cluster nodes".into());
    }

    let mut descendant_clusters: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut stack = vec![node_id.to_string()];
    while let Some(id) = stack.pop() {
        if let Some(kids) = children_by_parent.get(&id) {
            for k in kids {
                match k.kind {
                    hiker_core::trees::NodeKind::Leaf => {
                        descendant_leaves.push(k.clone());
                    }
                    _ => {
                        descendant_clusters.push(k.clone());
                        stack.push(k.id.clone());
                    }
                }
            }
        }
    }
    if descendant_leaves.len() < 4 {
        return Err("not enough leaves under this cluster to recluster (need >= 4)".into());
    }

    let resolved_policy: Option<hiker_core::trees::NodePolicy> = {
        let mut cursor: Option<String> = Some(node_id.to_string());
        let mut found = None;
        while let Some(id) = cursor {
            if let Some(n) = by_id.get(&id) {
                if let Some(p) = &n.policy {
                    found = Some(p.clone());
                    break;
                }
                cursor = n.parent.clone();
            } else {
                break;
            }
        }
        found
    };

    // Pull each leaf's note embedding to feed build_tree.
    let mut store = handlers.read_store.lock().map_err(|e| e.to_string())?;
    let mut note_inputs: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for l in &descendant_leaves {
        let Some(note_id_) = l.note_ref.clone() else {
            continue;
        };
        let Ok(Some(path)) = store.path_for_id(&note_id_) else {
            continue;
        };
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        note_inputs.push(hiker_core::cluster::NoteInput {
            id: note_id_,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    drop(store);
    if note_inputs.len() < 4 {
        return Err("not enough embedded notes to recluster (need >= 4)".into());
    }

    let prior_subtree: Vec<serde_json::Value> = descendant_clusters
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "parent_id": c.parent,
                "kind": match c.kind {
                    hiker_core::trees::NodeKind::Cluster => "cluster",
                    hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
                    hiker_core::trees::NodeKind::Leaf => "leaf",
                },
                "note_id": c.note_ref,
                "name": c.name,
                "summary": c.summary,
                "user_edited_name": c.user_edited_name,
                "user_edited_summary": c.user_edited_summary,
                "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                "confidence": c.confidence,
                "summary_membership_churn": c.summary_membership_churn,
            })
        })
        .collect();
    let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
        .iter()
        .map(|l| (l.id.clone(), l.parent.clone()))
        .collect();

    // Run the recursive build pass. Always Cluster-method (per spec).
    let summarizer = handlers.cluster_summarizer()?;
    let scope = hiker_core::cluster::BuildScope::Notes {
        ids: note_inputs.iter().map(|n| n.id.clone()).collect(),
        source_types: Vec::new(),
    };
    let build_method = hiker_core::cluster::BuildMethod::Cluster { params: params.clone() };
    let result =
        hiker_core::cluster::build_tree(scope, build_method, &note_inputs, &summarizer)
            .map_err(|e| format!("recluster build: {e}"))?;

    let ns = format!("recluster-{node_id}");
    let rename_id = |id: &str| -> String { format!("{}-{}", ns, id) };

    let levels = &result.tree.levels;
    let mut new_nodes_snapshot: Vec<serde_json::Value> = Vec::new();
    let mut new_cluster_ids: Vec<String> = Vec::new();

    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for level in levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    let top_level_idx = levels.len() - 1;
    let top = &levels[top_level_idx];
    let mut absorbed_top_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if top.len() == 1 {
        absorbed_top_ids.insert(top[0].id.clone());
    }

    for (level_idx, level) in levels.iter().enumerate().rev() {
        for node in level {
            if absorbed_top_ids.contains(&node.id) {
                continue;
            }
            let new_id = rename_id(&node.id);
            let parent_id = match parent_of.get(&node.id) {
                Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                _ => node_id.to_string(),
            };
            let policy = if carry_policies_down && parent_id == node_id {
                resolved_policy.clone()
            } else {
                None
            };
            let insert = hiker_core::trees::NodeInsert {
                node_id: new_id.clone(),
                parent_id: Some(parent_id.clone()),
                kind: hiker_core::trees::NodeKind::Cluster,
                note_id: None,
                name: node.name.clone(),
                summary: node.summary.clone(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: policy.clone(),
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            };
            new_nodes_snapshot.push(serde_json::json!({
                "id": new_id,
                "parent_id": parent_id,
                "kind": "cluster",
                "note_id": null,
                "name": insert.name,
                "summary": insert.summary,
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                "confidence": insert.confidence,
                "summary_membership_churn": 0,
                "level": level_idx,
            }));
            new_cluster_ids.push(new_id);
        }
    }

    let mut leaf_target: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(leaf_level) = levels.first() {
        for cluster in leaf_level {
            let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                node_id.to_string()
            } else {
                rename_id(&cluster.id)
            };
            for note_id_ in &cluster.members {
                leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
            }
        }
    }
    for note_id_ in &result.tree.outliers {
        leaf_target
            .entry(note_id_.clone())
            .or_insert_with(|| node_id.to_string());
    }

    let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
    for l in &descendant_leaves {
        let target = l
            .note_ref
            .as_ref()
            .and_then(|nid| leaf_target.get(nid).cloned())
            .unwrap_or_else(|| node_id.to_string());
        leaf_moves.push((l.id.clone(), Some(target)));
    }

    let preserved_chain: Vec<(String, u32)> = {
        let mut chain: Vec<(String, u32)> = Vec::new();
        let mut cursor: Option<String> = Some(node_id.to_string());
        while let Some(id) = cursor {
            if let Some(n) = by_id.get(&id) {
                chain.push((n.id.clone(), n.summary_membership_churn));
                cursor = n.parent.clone();
            } else {
                break;
            }
        }
        chain
    };

    for c in &descendant_clusters {
        handlers
            .trees
            .delete_node(tree_id, &c.id)
            .map_err(|e| e.to_string())?;
    }
    for snap in &new_nodes_snapshot {
        let id = snap.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let parent_id = snap
            .get("parent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let policy: Option<hiker_core::trees::NodePolicy> = snap.get("policy").and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
            _ => None,
        });
        let centroid = None;
        handlers
            .trees
            .insert_single_node(
                tree_id,
                hiker_core::trees::NodeInsert {
                    node_id: id,
                    parent_id,
                    kind: hiker_core::trees::NodeKind::Cluster,
                    note_id: None,
                    name: snap
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    summary: snap
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy,
                    centroid,
                    confidence: snap
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as f32,
                    summary_membership_churn: 0,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    handlers
        .trees
        .reparent_many(tree_id, &leaf_moves)
        .map_err(|e| e.to_string())?;

    for (id, prior) in &preserved_chain {
        let _ = handlers.trees.set_churn(tree_id, id, *prior);
    }
    for id in &new_cluster_ids {
        let _ = handlers.trees.reset_churn(tree_id, id);
    }

    handlers
        .trees
        .record_recluster_subtree(
            tree_id,
            node_id,
            &prior_subtree,
            &prior_leaf_parents,
            &new_nodes_snapshot,
            &leaf_moves,
            if carry_policies_down {
                resolved_policy.as_ref()
            } else {
                None
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({ "new_cluster_ids": new_cluster_ids }))
}

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
struct ReclusterTarget {
    tree_id: String,
    node_id: String,
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

/// Resolve a `BuildScope` to a `Vec<NoteInput>` by walking the read
/// store. Lazy-populates missing note embeddings. Standalone twin of
/// `DirectWorkerHandlers::notes_for_scope` for the IPC-side commands.
fn notes_for_scope_via_session(
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
fn notes_for_recluster_target(
    session: &VaultSession,
    target: &ReclusterTarget,
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let all = session
        .trees
        .list_nodes(&target.tree_id)
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
    let mut stack = vec![target.node_id.clone()];
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

// status: cluster-review-tab-run-clustering
#[tauri::command]
pub(crate) fn cluster_run_structural(
    state: State<'_, AppState>,
    args: ClusterRunStructuralArgs,
) -> Result<StructuralBuildDto, String> {
    let result = (|| -> Result<StructuralBuildDto, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let method: hiker_core::cluster::BuildMethod = serde_json::from_str(&args.method_json)
            .map_err(|e| format!("method_json: {e}"))?;
        let (scope, notes) = match args.recluster_target {
            Some(target) => {
                let notes = notes_for_recluster_target(session, &target)?;
                if notes.len() < 4 {
                    return Err("not enough embedded notes under this cluster to recluster (need >= 4)".into());
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
                    .ok_or_else(|| "scope_json required when recluster_target is absent".to_string())?;
                let scope: hiker_core::cluster::BuildScope = serde_json::from_str(scope_json)
                    .map_err(|e| format!("scope_json: {e}"))?;
                let notes = notes_for_scope_via_session(session, &scope)?;
                if notes.is_empty() {
                    return Err("no notes with embeddings found in scope".into());
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
        .map_err(|e| format!("structural build: {e}"))?;
        let scope_json = serde_json::to_string(&build_result.scope)
            .map_err(|e| format!("scope serialize: {e}"))?;
        let method_json = serde_json::to_string(&build_result.method)
            .map_err(|e| format!("method serialize: {e}"))?;
        Ok(StructuralBuildDto {
            scope_json,
            method_json,
            tree: build_result.tree,
            note_titles,
        })
    })();
    log_cmd_result("cluster_run_structural", result)
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

fn default_true() -> bool {
    true
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
) -> Result<ClusterPersistResult, String> {
    let result = async {
        let (trees, queue) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.trees.clone(), session.tasks.clone())
        };
        // Insert tree + nodes.
        let tree_id = trees
            .insert_tree(hiker_core::trees::TreeInsert {
                id: None,
                name: args.name,
                source: args.source,
                state: "draft".to_string(),
                scope_json: args.scope_json,
                method_json: args.method_json,
                vault_snapshot: None,
            })
            .map_err(|e| format!("insert_tree: {e}"))?;
        let mut inserts = hiker_core::cluster::result_to_node_inserts_pub(&args.tree);
        // Apply user-renamed names + the `user_edited_name = 1` flag.
        for ins in &mut inserts {
            if let Some(new_name) = args.user_renamed.get(&ins.node_id) {
                ins.name = new_name.clone();
                ins.user_edited_name = true;
            }
        }
        trees
            .insert_nodes(&tree_id, &inserts)
            .map_err(|e| format!("insert_nodes: {e}"))?;
        // Submit one RaptorSummarize task per cluster node whose name
        // is not user-edited. Mirrors `cluster_regenerate_names`.
        // status: cluster-review-tab-confirm-skip-naming
        // When the caller asks to skip the naming pass (the "Confirm
        // (no naming)" button), bypass the queue submission entirely
        // and return an empty `task_ids` list. The tree persists with
        // placeholder names; the user can run "Regenerate names" from
        // the cluster pane to fill them in later.
        let mut task_ids: Vec<String> = Vec::new();
        if !args.submit_naming {
            return Ok::<_, String>(ClusterPersistResult { tree_id, task_ids });
        }
        let nodes = trees.list_nodes(&tree_id).map_err(|e| e.to_string())?;
        for n in nodes {
            if !matches!(n.kind, hiker_core::trees::NodeKind::Cluster) {
                continue;
            }
            if n.user_edited_name {
                continue;
            }
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                    tree_id: tree_id.clone(),
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
        Ok::<_, String>(ClusterPersistResult { tree_id, task_ids })
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
) -> Result<ClusterReclusterFromBuiltResult, String> {
    let result = async {
        let (trees, queue) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.trees.clone(), session.tasks.clone())
        };

        // Walk the existing subtree under `node_id` to snapshot the
        // prior state for undo + collect leaves (so we know which leaves
        // need re-parenting onto the new structure).
        let all = trees.list_nodes(&args.tree_id).map_err(|e| e.to_string())?;
        let mut children_by_parent: std::collections::HashMap<
            String,
            Vec<hiker_core::trees::EditableNode>,
        > = std::collections::HashMap::new();
        for n in all.iter().cloned() {
            if let Some(p) = n.parent.clone() {
                children_by_parent.entry(p).or_default().push(n);
            }
        }
        let mut by_id: std::collections::HashMap<String, hiker_core::trees::EditableNode> =
            std::collections::HashMap::new();
        for n in all.iter().cloned() {
            by_id.insert(n.id.clone(), n);
        }
        let root_node = by_id
            .get(&args.node_id)
            .cloned()
            .ok_or_else(|| format!("node not found: {}", args.node_id))?;
        if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
            return Err("recluster only works on cluster nodes".into());
        }

        let mut descendant_clusters: Vec<hiker_core::trees::EditableNode> = Vec::new();
        let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
        let mut stack = vec![args.node_id.clone()];
        while let Some(id) = stack.pop() {
            if let Some(kids) = children_by_parent.get(&id) {
                for k in kids {
                    match k.kind {
                        hiker_core::trees::NodeKind::Leaf => descendant_leaves.push(k.clone()),
                        _ => {
                            descendant_clusters.push(k.clone());
                            stack.push(k.id.clone());
                        }
                    }
                }
            }
        }

        let resolved_policy: Option<hiker_core::trees::NodePolicy> = {
            let mut cursor: Option<String> = Some(args.node_id.clone());
            let mut found = None;
            while let Some(id) = cursor {
                if let Some(n) = by_id.get(&id) {
                    if let Some(p) = &n.policy {
                        found = Some(p.clone());
                        break;
                    }
                    cursor = n.parent.clone();
                } else {
                    break;
                }
            }
            found
        };

        let prior_subtree: Vec<serde_json::Value> = descendant_clusters
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "parent_id": c.parent,
                    "kind": match c.kind {
                        hiker_core::trees::NodeKind::Cluster => "cluster",
                        hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
                        hiker_core::trees::NodeKind::Leaf => "leaf",
                    },
                    "note_id": c.note_ref,
                    "name": c.name,
                    "summary": c.summary,
                    "user_edited_name": c.user_edited_name,
                    "user_edited_summary": c.user_edited_summary,
                    "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                    "confidence": c.confidence,
                    "summary_membership_churn": c.summary_membership_churn,
                })
            })
            .collect();
        let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
            .iter()
            .map(|l| (l.id.clone(), l.parent.clone()))
            .collect();

        // Plan the new node inserts, mirroring the namespaced-id pattern
        // from `recluster_subtree_in_worker` so collisions with existing
        // ids in `trees.db` are impossible.
        let ns = format!("recluster-{}", args.node_id);
        let rename_id = |id: &str| -> String { format!("{}-{}", ns, id) };

        let levels = &args.tree.levels;
        let mut new_nodes_snapshot: Vec<serde_json::Value> = Vec::new();
        let mut new_cluster_ids: Vec<String> = Vec::new();

        let mut parent_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for level in levels.iter().skip(1) {
            for node in level {
                for child in &node.members {
                    parent_of.insert(child.clone(), node.id.clone());
                }
            }
        }
        let top_level_idx = if levels.is_empty() { 0 } else { levels.len() - 1 };
        let top = if levels.is_empty() { &[][..] } else { &levels[top_level_idx][..] };
        let mut absorbed_top_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if top.len() == 1 {
            absorbed_top_ids.insert(top[0].id.clone());
        }

        for (level_idx, level) in levels.iter().enumerate().rev() {
            for node in level {
                if absorbed_top_ids.contains(&node.id) {
                    continue;
                }
                let new_id = rename_id(&node.id);
                let parent_id = match parent_of.get(&node.id) {
                    Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                    _ => args.node_id.clone(),
                };
                let policy = if args.carry_policies_down && parent_id == args.node_id {
                    resolved_policy.clone()
                } else {
                    None
                };
                let user_renamed_name = args.user_renamed.get(&node.id).cloned();
                let final_name = user_renamed_name.clone().unwrap_or_else(|| node.name.clone());
                new_nodes_snapshot.push(serde_json::json!({
                    "id": new_id,
                    "parent_id": parent_id,
                    "kind": "cluster",
                    "note_id": null,
                    "name": final_name,
                    "summary": node.summary,
                    "user_edited_name": user_renamed_name.is_some(),
                    "user_edited_summary": false,
                    "policy": policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                    "confidence": node.confidence,
                    "summary_membership_churn": 0,
                    "level": level_idx,
                }));
                new_cluster_ids.push(new_id);
            }
        }

        let mut leaf_target: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(leaf_level) = levels.first() {
            for cluster in leaf_level {
                let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                    args.node_id.clone()
                } else {
                    rename_id(&cluster.id)
                };
                for note_id_ in &cluster.members {
                    leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
                }
            }
        }
        for note_id_ in &args.tree.outliers {
            leaf_target
                .entry(note_id_.clone())
                .or_insert_with(|| args.node_id.clone());
        }

        let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
        for l in &descendant_leaves {
            let target = l
                .note_ref
                .as_ref()
                .and_then(|nid| leaf_target.get(nid).cloned())
                .unwrap_or_else(|| args.node_id.clone());
            leaf_moves.push((l.id.clone(), Some(target)));
        }

        let preserved_chain: Vec<(String, u32)> = {
            let mut chain: Vec<(String, u32)> = Vec::new();
            let mut cursor: Option<String> = Some(args.node_id.clone());
            while let Some(id) = cursor {
                if let Some(n) = by_id.get(&id) {
                    chain.push((n.id.clone(), n.summary_membership_churn));
                    cursor = n.parent.clone();
                } else {
                    break;
                }
            }
            chain
        };

        // Mutate trees.db in the same shape as `recluster_subtree_in_worker`.
        for c in &descendant_clusters {
            trees.delete_node(&args.tree_id, &c.id).map_err(|e| e.to_string())?;
        }
        for snap in &new_nodes_snapshot {
            let id = snap.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let parent_id = snap
                .get("parent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let policy: Option<hiker_core::trees::NodePolicy> =
                snap.get("policy").and_then(|v| match v {
                    serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
                    _ => None,
                });
            let user_edited_name = snap
                .get("user_edited_name")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            trees
                .insert_single_node(
                    &args.tree_id,
                    hiker_core::trees::NodeInsert {
                        node_id: id,
                        parent_id,
                        kind: hiker_core::trees::NodeKind::Cluster,
                        note_id: None,
                        name: snap
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        summary: snap
                            .get("summary")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        user_edited_name,
                        user_edited_summary: false,
                        policy,
                        centroid: None,
                        confidence: snap
                            .get("confidence")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0) as f32,
                        summary_membership_churn: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        trees
            .reparent_many(&args.tree_id, &leaf_moves)
            .map_err(|e| e.to_string())?;

        for (id, prior) in &preserved_chain {
            let _ = trees.set_churn(&args.tree_id, id, *prior);
        }
        for id in &new_cluster_ids {
            let _ = trees.reset_churn(&args.tree_id, id);
        }

        trees
            .record_recluster_subtree(
                &args.tree_id,
                &args.node_id,
                &prior_subtree,
                &prior_leaf_parents,
                &new_nodes_snapshot,
                &leaf_moves,
                if args.carry_policies_down {
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
        let mut task_ids: Vec<String> = Vec::new();
        if !args.submit_naming {
            return Ok::<_, String>(ClusterReclusterFromBuiltResult {
                new_cluster_ids,
                task_ids,
            });
        }
        let user_renamed_new_ids: std::collections::HashSet<String> = args
            .user_renamed
            .keys()
            .map(|k| format!("{}-{}", ns, k))
            .collect();
        for id in &new_cluster_ids {
            if user_renamed_new_ids.contains(id) {
                continue;
            }
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                    tree_id: args.tree_id.clone(),
                    cluster_node_id: id.clone(),
                    level: 0,
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": args.tree_id,
                    "cluster_node_id": id,
                }),
            };
            let handle = queue.submit(task).await;
            task_ids.push(handle.id.clone());
        }

        Ok::<_, String>(ClusterReclusterFromBuiltResult {
            new_cluster_ids,
            task_ids,
        })
    }
    .await;
    log_cmd_result("cluster_op_recluster_subtree_from_built", result)
}

// status: cluster-editor-set-policy
#[tauri::command]
pub(crate) fn cluster_node_set_policy(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    policy_json: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let policy: Option<hiker_core::trees::NodePolicy> = match policy_json {
            Some(s) if !s.is_empty() => Some(serde_json::from_str(&s).map_err(|e| e.to_string())?),
            _ => None,
        };
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_policy(&tree_id, &node_id, policy)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_set_policy", result)
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
) -> Result<hiker_core::suggest::ApplyOutcome, String> {
    let result = (|| -> Result<hiker_core::suggest::ApplyOutcome, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        hiker_core::suggest::apply_tree(
            &session.trees,
            &tree_id,
            &session.vault,
            &store,
            &session.staging,
            Some(&history),
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_apply", result)
}

// status: cluster-editor-multi-select-stage-move
#[tauri::command]
pub(crate) fn cluster_stage_moves(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    target_folder: String,
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        hiker_core::suggest::stage_moves(
            &session.trees,
            hiker_core::suggest::StageMoveArgs {
                tree_id: &tree_id,
                node_ids: &node_ids,
                target_folder: &target_folder,
            },
            &store,
            &session.staging,
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_stage_moves", result)
}

// status: cluster-editor-multi-select-stage-tag
#[tauri::command]
pub(crate) fn cluster_stage_tags(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    tag_slug: String,
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        hiker_core::suggest::stage_tags(
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
        .map_err(|e| e.to_string())
    })();
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
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_tree_state(&tree_id, &new_state)
            .map_err(|e| e.to_string())
    })();
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
) -> Result<Vec<hiker_core::suggest::TriageOutcome>, String> {
    let result = (|| -> Result<Vec<hiker_core::suggest::TriageOutcome>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let note_id = store
            .id_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("note not indexed: {rel}"))?;
        let embedding = store
            .note_embedding_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no embedding for {rel}"))?;
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
        hiker_core::suggest::triage_all_saved_trees(
            &session.trees,
            &session.vault,
            &store,
            &session.staging,
            &note_id,
            &rel,
            &embedding,
            ac,
            &opts,
        )
        .map_err(|e| e.to_string())
    })();
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
) -> Result<Vec<String>, String> {
    let result = async {
        let (queue, trees) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.tasks.clone(), session.trees.clone())
        };
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
        Ok::<_, String>(task_ids)
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
) -> Result<Vec<String>, String> {
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
) -> Result<SummarizeSweepOutcome, String> {
    let result = async {
        let params: hiker_core::trees::SummarizeParams = serde_json::from_str(&params_json)
            .map_err(|e| format!("cluster_summarize: parse params: {e}"))?;
        cluster_summarize_inner(state, tree_id, params).await
    }
    .await;
    log_cmd_result("cluster_summarize", result)
}

async fn cluster_summarize_inner(
    state: State<'_, AppState>,
    tree_id: String,
    params: hiker_core::trees::SummarizeParams,
) -> Result<SummarizeSweepOutcome, String> {
    let (queue, plan) = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let plan = session
            .trees
            .plan_summarize_sweep(&tree_id, &params)
            .map_err(|e| e.to_string())?;
        (session.tasks.clone(), plan)
    };
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
) -> Result<hiker_core::trees::RollupOutcome, String> {
    let result = async {
        let params: hiker_core::trees::RollupParams = serde_json::from_str(&params_json)
            .map_err(|e| format!("cluster_op_rollup: parse params: {e}"))?;
        // Validate inputs + collect their summaries. Bail before
        // embedding to avoid a wasted embedder round-trip on bad inputs.
        let (trees, embedder) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.trees.clone(), session.indexer.embedder())
        };
        let embedder = embedder
            .ok_or_else(|| "cluster_op_rollup: embedder not yet loaded".to_string())?;
        let inputs = trees
            .validate_rollup_inputs(&tree_id, &params.input_node_ids)
            .map_err(|e| e.to_string())?;
        let summaries: Vec<String> = inputs.iter().map(|i| i.summary.clone()).collect();
        // Embed via spawn_blocking so the fastembed call doesn't park a
        // tokio worker thread (mirrors the indexer's pattern).
        let emb_clone = embedder.clone();
        let summary_embeddings = tokio::task::spawn_blocking(move || emb_clone.embed_batch(&summaries))
            .await
            .map_err(|e| format!("cluster_op_rollup: join: {e}"))?
            .map_err(|e| format!("cluster_op_rollup: embed: {e}"))?;
        let outcome = trees
            .apply_rollup(&tree_id, &inputs, &summary_embeddings, &params)
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(outcome)
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
) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
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
    })();
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
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        history
            .record_rejection(&fingerprint, &note_path, &action)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_record_rejection", result)
}

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

// (The redo stack itself lives inline below — `lazy_static!` is overkill
// for a single Mutex.)

use std::sync::OnceLock;
static CLUSTER_REDO_STACKS: OnceLock<Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>>> = OnceLock::new();

fn redo_stacks() -> &'static Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>> {
    CLUSTER_REDO_STACKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn invert_history(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    entry: &hiker_core::trees::HistoryEntry,
) -> Result<(), String> {
    let undo: serde_json::Value =
        serde_json::from_str(&entry.undo_args_json).map_err(|e| e.to_string())?;
    match entry.op.as_str() {
        "rename" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = undo.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // Direct DB poke — we need to preserve the prior
            // user_edited_name flag too; rename() always stamps it true.
            let user_edited = undo
                .get("user_edited_name")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            trees
                .rename(tree_id, node_id, name)
                .map_err(|e| e.to_string())?;
            // Hop one more time to flip the flag back if needed.
            if !user_edited {
                // Pop the entry we just appended so undo stays
                // idempotent — we don't want the inverse to leak
                // forward history.
                let _ = trees.pop_last_history(tree_id);
            } else {
                let _ = trees.pop_last_history(tree_id);
            }
            Ok(())
        }
        "edit-summary" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let summary = undo.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            trees
                .set_summary(tree_id, node_id, summary)
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        "set-policy" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let policy = undo.get("policy");
            let policy_val: Option<hiker_core::trees::NodePolicy> = match policy {
                Some(serde_json::Value::Null) | None => None,
                Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| e.to_string())?),
            };
            trees
                .set_policy(tree_id, node_id, policy_val)
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        "move" | "promote-outlier" => {
            // Stored `node_id` (or `leaf_id`) + prior `parent_id`.
            let node_id = undo
                .get("node_id")
                .or_else(|| undo.get("leaf_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parent = match undo.get("parent_id") {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            trees
                .move_node(tree_id, node_id, parent.as_deref())
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        // Reshape ops below do bulk DB work; we apply the recorded
        // inverse directly without re-routing through the high-level
        // methods (which would mutate history).
        "merge-siblings" => undo_merge_siblings(trees, tree_id, &undo),
        "merge-children-up" => undo_merge_children_up(trees, tree_id, &undo),
        "drop-cluster" => undo_drop_cluster(trees, tree_id, &undo),
        "split-cluster" => undo_split(trees, tree_id, &undo),
        "recluster-subtree" => undo_recluster_subtree(trees, tree_id, &undo),
        other => Err(format!("cannot undo op {other}")),
    }
}

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

fn undo_merge_siblings(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let child_moves = undo
        .get("child_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for abs in absorbed {
        let id = abs.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let row = abs.get("row").cloned().unwrap_or(serde_json::Value::Null);
        restore_node_row(trees, tree_id, id, &row)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in child_moves {
        let cid = mv.get("child_id").and_then(|v| v.as_str()).unwrap_or("");
        let from = mv.get("from").and_then(|v| v.as_str()).unwrap_or("");
        moves.push((cid.to_string(), Some(from.to_string())));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_merge_children_up(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let grand = undo
        .get("grandchild_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for abs in absorbed {
        let id = abs.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let row = abs.get("row").cloned().unwrap_or(serde_json::Value::Null);
        restore_node_row(trees, tree_id, id, &row)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in grand {
        let cid = mv.get("child_id").and_then(|v| v.as_str()).unwrap_or("");
        let from = mv.get("from").and_then(|v| v.as_str()).unwrap_or("");
        moves.push((cid.to_string(), Some(from.to_string())));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_drop_cluster(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed_clusters")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let leaf_moves = undo
        .get("leaf_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Re-insert each cluster row with its prior parent. We re-insert in
    // the recorded order — children before parents would fail FK
    // expectations, but cluster_nodes doesn't have an FK on parent_id,
    // so order is loose.
    for c in absorbed {
        let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        restore_node_row(trees, tree_id, &id, &c)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in leaf_moves {
        let leaf = mv.get("leaf_id").and_then(|v| v.as_str()).unwrap_or("");
        let pp = match mv.get("prior_parent") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        moves.push((leaf.to_string(), pp));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_split(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    // Re-parent the leaves back to their original parent (which equals
    // `parent_id` here — split moved them onto new sub-clusters under
    // `parent_id`).
    let parent_id = undo.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
    let leaf_moves = undo
        .get("leaf_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in leaf_moves {
        // `leaf_moves` is `[(leaf_id, new_parent)]`; the inverse parks
        // the leaf back under the parent it was split out of.
        if let Some(arr) = mv.as_array() {
            if let (Some(leaf), Some(_)) = (arr.first(), arr.get(1)) {
                if let Some(s) = leaf.as_str() {
                    moves.push((s.to_string(), Some(parent_id.to_string())));
                }
            }
        }
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())?;
    // Delete the synthesized sub-clusters.
    let new_clusters = undo
        .get("new_cluster_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for nc in new_clusters {
        if let Some(s) = nc.as_str() {
            trees
                .delete_node(tree_id, s)
                .map_err(|e| e.to_string())?;
        }
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
fn undo_recluster_subtree(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    // Delete the new cluster rows the forward op inserted. We delete
    // before re-inserting the prior subtree so a transient overlap on
    // (tree_id, node_id) primary keys can't happen — even though the
    // namespaced ids guarantee no collision in practice.
    let new_ids = undo
        .get("new_node_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for id in &new_ids {
        if let Some(s) = id.as_str() {
            trees
                .delete_node(tree_id, s)
                .map_err(|e| e.to_string())?;
        }
    }
    // Re-insert each prior cluster row.
    let prior = undo
        .get("prior_subtree")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for row in &prior {
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        restore_node_row(trees, tree_id, &id, row)?;
    }
    // Re-parent every leaf back to its prior parent.
    let prior_leaves = undo
        .get("prior_leaf_parents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in &prior_leaves {
        if let Some(arr) = mv.as_array() {
            if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                let leaf_s = leaf.as_str().unwrap_or("").to_string();
                let parent_s = match parent {
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => None,
                };
                moves.push((leaf_s, parent_s));
            }
        }
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub(crate) fn cluster_tree_undo(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<bool, String> {
    let result = (|| -> Result<bool, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let trees = session.trees.clone();
        drop(guard);
        let Some(entry) = trees.pop_last_history(&tree_id).map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        invert_history(&trees, &tree_id, &entry)?;
        let mut stacks = redo_stacks().lock().map_err(|e| e.to_string())?;
        stacks.entry(tree_id).or_default().push(entry);
        Ok(true)
    })();
    log_cmd_result("cluster_tree_undo", result)
}

#[tauri::command]
pub(crate) fn cluster_tree_redo(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<bool, String> {
    let result = (|| -> Result<bool, String> {
        // Pop from the redo stack and re-apply the forward args.
        let popped = {
            let mut stacks = redo_stacks().lock().map_err(|e| e.to_string())?;
            stacks.entry(tree_id.clone()).or_default().pop()
        };
        let Some(entry) = popped else { return Ok(false) };
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let trees = session.trees.clone();
        drop(guard);
        let args: serde_json::Value =
            serde_json::from_str(&entry.args_json).map_err(|e| e.to_string())?;
        // Re-apply by the same op-keyed dispatch as forward edits.
        // Simpler ops route through the existing methods (which write a
        // fresh history row); reshape ops re-build using the recorded
        // args.
        match entry.op.as_str() {
            "rename" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                trees.rename(&tree_id, node_id, name).map_err(|e| e.to_string())?;
            }
            "edit-summary" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                trees.set_summary(&tree_id, node_id, summary).map_err(|e| e.to_string())?;
            }
            "set-policy" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let policy = args.get("policy");
                let policy_val: Option<hiker_core::trees::NodePolicy> = match policy {
                    Some(serde_json::Value::Null) | None => None,
                    Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| e.to_string())?),
                };
                trees.set_policy(&tree_id, node_id, policy_val).map_err(|e| e.to_string())?;
            }
            "move" | "promote-outlier" => {
                let node_id = args
                    .get("node_id")
                    .or_else(|| args.get("leaf_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let parent = match args.get("parent_id") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    _ => None,
                };
                trees.move_node(&tree_id, node_id, parent.as_deref()).map_err(|e| e.to_string())?;
            }
            "merge-siblings" => {
                // Re-run the forward op against the recorded
                // [survivor, ...absorbed] ids. Undo restored the
                // absorbed nodes so the IDs are valid again.
                let survivor = args
                    .get("survivor")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "merge-siblings redo: missing survivor".to_string())?;
                let absorbed = args
                    .get("absorbed")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut node_ids: Vec<String> = vec![survivor.to_string()];
                for a in absorbed {
                    if let Some(s) = a.as_str() {
                        node_ids.push(s.to_string());
                    }
                }
                trees
                    .merge_siblings(&tree_id, &node_ids)
                    .map_err(|e| e.to_string())?;
            }
            "merge-children-up" => {
                let parent_id = args
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "merge-children-up redo: missing parent_id".to_string())?;
                trees
                    .merge_children_up(&tree_id, parent_id)
                    .map_err(|e| e.to_string())?;
            }
            "drop-cluster" => {
                let node_id = args
                    .get("node_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "drop-cluster redo: missing node_id".to_string())?;
                let bucket = args
                    .get("outlier_bucket_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "drop-cluster redo: missing outlier_bucket_id".to_string())?;
                trees
                    .drop_cluster(&tree_id, node_id, bucket)
                    .map_err(|e| e.to_string())?;
            }
            "split-cluster" => {
                // HDBSCAN is non-deterministic, so we don't re-cluster
                // on redo — we replay the snapshotted result. The
                // forward op recorded each new cluster's full row
                // shape + the leaf moves, so we just re-insert and
                // re-parent. Then `record_split` lays down a fresh
                // history row so a subsequent undo round-trips.
                let parent_id = args
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "split-cluster redo: missing parent_id".to_string())?;
                let new_clusters = args
                    .get("new_clusters")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if new_clusters.is_empty() {
                    return Err(
                        "split-cluster redo: legacy history row lacks new_clusters snapshot"
                            .into(),
                    );
                }
                for c in &new_clusters {
                    let id = c
                        .get("node_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    restore_node_row(&trees, &tree_id, &id, c)?;
                }
                let leaf_moves_json = args
                    .get("leaf_moves")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &leaf_moves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            leaf_moves.push((leaf_s, parent_s));
                        }
                    }
                }
                trees
                    .reparent_many(&tree_id, &leaf_moves)
                    .map_err(|e| e.to_string())?;
                trees
                    .record_split(&tree_id, parent_id, &new_clusters, &leaf_moves)
                    .map_err(|e| e.to_string())?;
            }
            "recluster-subtree" => {
                // HDBSCAN is non-deterministic and the build pipeline
                // is recursive on top — re-running won't reproduce the
                // same subtree. So redo replays from the snapshot: the
                // forward op recorded every new cluster row and the
                // (leaf_id, new_parent) moves; we re-insert and
                // re-parent, then lay down a fresh history row so a
                // subsequent undo round-trips. The descendants the
                // forward op deleted were *restored* by undo, so we
                // delete them again here before re-inserting.
                let root_id = args
                    .get("root_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "recluster-subtree redo: missing root_id".to_string())?;
                let undo_args: serde_json::Value = serde_json::from_str(&entry.undo_args_json)
                    .map_err(|e| e.to_string())?;
                // Walk the tree to find every descendant cluster of
                // root_id and delete it (undo restored them).
                let all = trees
                    .list_nodes(&tree_id)
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
                let mut to_delete: Vec<String> = Vec::new();
                let mut stack = vec![root_id.to_string()];
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
                    trees.delete_node(&tree_id, id).map_err(|e| e.to_string())?;
                }
                // Re-insert the new cluster rows from the snapshot
                // (order in args is already top-down — parents first).
                let new_nodes = args
                    .get("new_nodes")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if new_nodes.is_empty() {
                    return Err(
                        "recluster-subtree redo: legacy history row lacks new_nodes snapshot"
                            .into(),
                    );
                }
                for snap in &new_nodes {
                    let id = snap
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    restore_node_row(&trees, &tree_id, &id, snap)?;
                }
                // Re-parent every leaf onto its recorded new home.
                let leaf_moves_json = args
                    .get("leaf_moves")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &leaf_moves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            leaf_moves.push((leaf_s, parent_s));
                        }
                    }
                }
                trees
                    .reparent_many(&tree_id, &leaf_moves)
                    .map_err(|e| e.to_string())?;
                // Re-record the history row so undo round-trips.
                let prior_subtree = undo_args
                    .get("prior_subtree")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let prior_leaves_json = undo_args
                    .get("prior_leaf_parents")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut prior_leaves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &prior_leaves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            prior_leaves.push((leaf_s, parent_s));
                        }
                    }
                }
                let carried_policy: Option<hiker_core::trees::NodePolicy> = args
                    .get("carried_policy")
                    .and_then(|v| match v {
                        serde_json::Value::Null => None,
                        other => serde_json::from_value(other.clone()).ok(),
                    });
                trees
                    .record_recluster_subtree(
                        &tree_id,
                        root_id,
                        &prior_subtree,
                        &prior_leaves,
                        &new_nodes,
                        &leaf_moves,
                        carried_policy.as_ref(),
                    )
                    .map_err(|e| e.to_string())?;
            }
            other => {
                return Err(format!("redo unsupported for op {other}"));
            }
        }
        Ok(true)
    })();
    log_cmd_result("cluster_tree_redo", result)
}
