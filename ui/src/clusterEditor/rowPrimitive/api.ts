// status: cluster-editor-row-primitive
//
// Tauri shim + wire DTOs shared by both surfaces of the cluster editor
// (sidebar `ui/src/clusterEditor/index.ts` + expanded pane
// `ui/src/clusterEditorPane/index.ts`). The shim wraps the `invoke`
// calls so consumers don't repeat the command-name + camelCase param
// dance.

import { invoke } from "@tauri-apps/api/core";

// ── Wire types ──────────────────────────────────────────────────────

export interface ClusterTreeRow {
  id: string;
  name: string;
  source: string;
  state: string;
  scope_json: string;
  method_json: string;
  created_at_ms: number;
  vault_snapshot: string | null;
}

export interface ClusterNodeRow {
  id: string;
  parent: string | null;
  kind: "cluster" | "leaf" | "outlier-bucket";
  note_ref: string | null;
  note_path: string | null;
  note_title: string | null;
  name: string;
  summary: string;
  user_edited_name: boolean;
  user_edited_summary: boolean;
  policy_json: string | null;
  confidence: number;
  summary_membership_churn: number;
}

// Outcome of the `cluster_summarize` Tauri command (the umbrella +
// per-cluster `RaptorSummarize` queue rows are submitted by the
// command; this DTO reports what was enqueued vs. skipped so the UI
// can render a toast). Mirrors `SummarizeSweepOutcome` in
// `ui/src-tauri/src/lib.rs`. Per `cluster-op-summarize-sweep`.
export interface SummarizeSweepOutcome {
  enqueued: string[];
  skipped_user_edited: string[];
  skipped_fresh: string[];
  queue_row_id: string;
}

// ── Tauri shim (shared by both surfaces) ────────────────────────────

export const Api = {
  list(): Promise<ClusterTreeRow[]> {
    return invoke("cluster_trees_list");
  },
  get(treeId: string): Promise<ClusterNodeRow[]> {
    return invoke("cluster_tree_get", { treeId });
  },
  rename(treeId: string, nodeId: string, name: string): Promise<void> {
    return invoke("cluster_node_rename", { treeId, nodeId, name });
  },
  setSummary(treeId: string, nodeId: string, summary: string): Promise<void> {
    return invoke("cluster_node_set_summary", { treeId, nodeId, summary });
  },
  move(treeId: string, nodeId: string, newParent: string | null): Promise<void> {
    return invoke("cluster_node_move", { treeId, nodeId, newParent });
  },
  setPolicy(treeId: string, nodeId: string, policyJson: string | null): Promise<void> {
    return invoke("cluster_node_set_policy", { treeId, nodeId, policyJson });
  },
  mergeSiblings(treeId: string, nodeIds: string[]): Promise<string> {
    return invoke("cluster_op_merge_siblings", { treeId, nodeIds });
  },
  mergeChildrenUp(treeId: string, parentId: string): Promise<void> {
    return invoke("cluster_op_merge_children_up", { treeId, parentId });
  },
  dropCluster(treeId: string, nodeId: string, outlierBucketId: string): Promise<void> {
    return invoke("cluster_op_drop_cluster", { treeId, nodeId, outlierBucketId });
  },
  promoteOutlier(treeId: string, leafId: string, newParent: string | null): Promise<void> {
    return invoke("cluster_op_promote_outlier", { treeId, leafId, newParent });
  },
  split(treeId: string, nodeId: string): Promise<string[]> {
    return invoke("cluster_op_split", { treeId, nodeId });
  },
  regenerateNames(treeId: string): Promise<string[]> {
    return invoke("cluster_regenerate_names", { treeId });
  },
  // status: cluster-editor-summarize-verb
  //
  // Subset-scope Summarize for the right-click "Summarize" verb and the
  // multi-select toolbar "Summarize" button. Always non-recursive and
  // preserves user-edited names/summaries. The umbrella + per-cluster
  // queue rows are submitted by the Tauri command; the returned outcome
  // is used by callers to render a toast.
  summarizeSubset(treeId: string, ids: string[]): Promise<SummarizeSweepOutcome> {
    const params = {
      scope: { kind: "subset", ids },
      subtree_root: null,
      recursive: false,
      summarize_mode: "llm",
      overwrite_user_edited: false,
    };
    return invoke("cluster_summarize", {
      treeId,
      paramsJson: JSON.stringify(params),
    });
  },
  stageMoves(treeId: string, nodeIds: string[], targetFolder: string): Promise<string[]> {
    return invoke("cluster_stage_moves", { treeId, nodeIds, targetFolder });
  },
  stageTags(treeId: string, nodeIds: string[], tagSlug: string): Promise<string[]> {
    return invoke("cluster_stage_tags", { treeId, nodeIds, tagSlug });
  },
};
