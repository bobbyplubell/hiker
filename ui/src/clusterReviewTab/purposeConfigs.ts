// status: cluster-review-tab-config-section
// status: cluster-review-tab-rebuild-prefill
// status: cluster-review-tab-from-new-tree-action
// status: cluster-review-tab-from-recluster-action
//
// Per-purpose policy table for `clusterReviewTab`. Holds policy
// (which IPC, which defaults, which fields are visible); rendering /
// paint loops / event wiring stay in `mountClusterReviewTab`.

import { invoke } from "@tauri-apps/api/core";

import type {
  BuiltClusterTree,
  ClusterReviewForm,
  LeidenParams,
  ClusterParams,
  FolderParams,
  PaneState,
  Purpose,
  StructuralBuildDto,
} from "./index";

// ── Wire bindings ────────────────────────────────────────────────────

export const Api = {
  runStructural(args: {
    scope_json?: string;
    method_json: string;
    recluster_target?: { tree_id: string; node_id: string };
  }): Promise<StructuralBuildDto> {
    return invoke("cluster_run_structural", { args });
  },
  persist(args: {
    name: string;
    source: string;
    scope_json: string;
    method_json: string;
    tree: BuiltClusterTree;
    user_renamed: Record<string, string>;
    /// status: cluster-review-tab-confirm-skip-naming
    /// When false, the backend skips submitting `RaptorSummarize`
    /// tasks for un-renamed clusters — the tree lands with the
    /// placeholder `"Cluster N"` names intact and the user can run
    /// "Regenerate names" from the cluster pane later. Defaults to
    /// true on the backend if omitted (matches the original "Confirm
    /// and name" behavior).
    submit_naming: boolean;
  }): Promise<{ tree_id: string; task_ids: string[] }> {
    return invoke("cluster_persist_built_tree", { args });
  },
  reclusterFromBuilt(args: {
    tree_id: string;
    node_id: string;
    tree: BuiltClusterTree;
    carry_policies_down: boolean;
    user_renamed: Record<string, string>;
    /// status: cluster-review-tab-confirm-skip-naming
    /// When false, the backend skips submitting `RaptorSummarize` tasks
    /// for the new subtree — placeholder cluster names stay intact and
    /// the user can run "Regenerate names" from the cluster pane later.
    /// Mirrors the same flag on `cluster_persist_built_tree`.
    submit_naming: boolean;
  }): Promise<{ new_cluster_ids: string[]; task_ids: string[] }> {
    return invoke("cluster_op_recluster_subtree_from_built", { args });
  },
  listTrees(): Promise<
    Array<{ id: string; name: string; scope_json: string; method_json: string }>
  > {
    return invoke("cluster_trees_list").then((rows) =>
      (rows as Array<{ id: string; name: string; scope_json: string; method_json: string }>) ?? [],
    );
  },
};

// ── Tab-key + form defaults ──────────────────────────────────────────

export function purposeToKey(purpose: Purpose): string {
  // Mirrors the `__hiker:cluster-review:*` shape used by other app-page
  // tabs. The key is the dedup id (one tab per (kind, target)).
  switch (purpose.kind) {
    case "new-tree":
      return "__hiker:cluster-review:new-tree";
    case "recluster-subtree":
      return `__hiker:cluster-review:recluster-subtree:${purpose.treeId}:${purpose.nodeId}`;
    case "rebuild":
      return `__hiker:cluster-review:rebuild:${purpose.treeId}`;
  }
}

function defaultLeidenParams(): LeidenParams {
  return {
    k_nearest: 15,
    edge_weight_floor: 0.0,
    iterations: 100,
    min_cluster_size: 2,
    resolution: 1.0,
  };
}

function defaultClusterParams(): ClusterParams {
  return {
    algorithm: "leiden",
    min_cluster_size: 5,
    min_samples: null,
    min_clusters_to_recurse: 4,
    summary_confidence_threshold: 0.5,
    include_outliers: true,
    summarize: "none",
    leiden: defaultLeidenParams(),
    disable_recursion: false,
  };
}

function defaultFolderParams(): FolderParams {
  return { summarize: "none", include_outliers: true, outlier_threshold: 0.5 };
}

function methodJsonFromForm(form: ClusterReviewForm): string {
  if (form.method === "cluster") {
    // status: cluster-review-tab-structural-pass-no-llm
    // Force summarize=none on the way out — the structural pass
    // mandates it, but pinning here means the form's source-of-truth
    // matches what the backend will see.
    return JSON.stringify({
      kind: "cluster",
      params: { ...form.clusterParams, summarize: "none" },
    });
  }
  return JSON.stringify({
    kind: "from-folders",
    params: { ...form.folderParams, summarize: "none" },
  });
}

function scopeJsonFromForm(form: ClusterReviewForm): string {
  // status: cluster-build-scope-source-types — every variant carries
  // an optional `source_types` filter. Empty array = "every indexable
  // extension." `"md"` covers both `.md` and `.markdown`.
  const source_types = form.sourceTypes;
  if (form.scope.kind === "vault") {
    return JSON.stringify({ kind: "vault", source_types });
  }
  if (form.scope.kind === "folder") {
    return JSON.stringify({ kind: "folder", rel: form.scope.rel, source_types });
  }
  return JSON.stringify({ kind: "notes", ids: form.scope.ids, source_types });
}

// ── PurposeConfig table ──────────────────────────────────────────────

export interface PurposeConfig {
  /// Render the page title.
  title(purpose: Purpose): string;
  /// Initial form values when the tab opens for this purpose.
  defaultForm(purpose: Purpose): ClusterReviewForm;
  /// Configuration-section field visibility flags.
  showLifecycleRow: boolean;
  showScopeRow: boolean;
  showCarryPoliciesRow: boolean;
  /// Run the structural pass for this purpose.
  runStructural(form: ClusterReviewForm, purpose: Purpose): Promise<StructuralBuildDto>;
  /// Persist the build result. Returns the tree_id of the persisted/updated
  /// tree and the count of naming tasks enqueued by the backend (0 when
  /// the path doesn't surface a task count).
  confirm(
    form: ClusterReviewForm,
    purpose: Purpose,
    result: StructuralBuildDto,
    opts: { submitNaming: boolean; userRenamed: Record<string, string> },
  ): Promise<{ treeId: string; taskCount: number }>;
  /// Toast message after a successful Confirm.
  confirmSuccessToast(opts: { submitNaming: boolean; taskCount: number }): string;
  /// Optional hook fired once per pane mount, after `defaultForm`. Used
  /// by `rebuild` today to fetch the existing tree's old scope/method
  /// and merge it into the form.
  onMount?(state: PaneState, repaint: (s: PaneState) => void): void | Promise<void>;
  /// Extra one-line-summary chunks to append for this purpose's config
  /// summary line. Receives the form and returns a list of "key=value" bits.
  summaryExtras?(form: ClusterReviewForm): string[];
}

// status: cluster-review-tab-rebuild-prefill
//
// Best-effort prefill of the form for the `rebuild` purpose. Fires
// after open() to fetch the tree's saved scope/method JSON; failures
// fall back to defaults silently.
function prefillRebuild(state: PaneState, repaint: (s: PaneState) => void): void {
  if (state.purpose.kind !== "rebuild") return;
  const treeId = state.purpose.treeId;
  void Api.listTrees().then((rows) => {
    const row = rows.find((r) => r.id === treeId);
    if (!row) return;
    state.form.name = `${row.name} (rebuild)`;
    try {
      const scope = JSON.parse(row.scope_json);
      if (scope?.kind === "vault") state.form.scope = { kind: "vault" };
      else if (scope?.kind === "folder" && typeof scope.rel === "string") {
        state.form.scope = { kind: "folder", rel: scope.rel };
      }
      // status: cluster-build-scope-source-types — carry the filter
      // forward on rebuild so the new tree inherits the same source-
      // types posture by default; user can edit before Run.
      if (Array.isArray(scope?.source_types)) {
        state.form.sourceTypes = scope.source_types.filter(
          (s: unknown): s is string => typeof s === "string",
        );
      }
    } catch {}
    try {
      const method = JSON.parse(row.method_json);
      if (method?.kind === "cluster" && method?.params) {
        state.form.method = "cluster";
        state.form.clusterParams = {
          ...state.form.clusterParams,
          ...method.params,
          summarize: "none",
        };
      } else if (method?.kind === "from-folders" && method?.params) {
        state.form.method = "folders";
        state.form.folderParams = {
          ...state.form.folderParams,
          ...method.params,
          summarize: "none",
        };
      }
    } catch {}
    repaint(state);
  });
}

export const PURPOSE_CONFIGS: Record<Purpose["kind"], PurposeConfig> = {
  "new-tree": {
    title: () => "Cluster review: new tree",
    defaultForm: () => {
      const today = new Date().toISOString().slice(0, 10);
      return {
        name: `${today} reorg`,
        lifecycle: "sapling",
        scope: { kind: "vault" },
        sourceTypes: ["md", "txt"],
        method: "cluster",
        carryPoliciesDown: false,
        clusterParams: defaultClusterParams(),
        folderParams: defaultFolderParams(),
      };
    },
    showLifecycleRow: true,
    showScopeRow: true,
    showCarryPoliciesRow: false,
    runStructural: (form) =>
      Api.runStructural({
        scope_json: scopeJsonFromForm(form),
        method_json: methodJsonFromForm(form),
      }),
    confirm: async (form, _purpose, result, opts) => {
      const source = form.lifecycle === "evergreen" ? "saved-triage" : "one-shot";
      const res = await Api.persist({
        name: form.name.trim() || "untitled",
        source,
        scope_json: result.scope_json,
        method_json: result.method_json,
        tree: result.tree,
        user_renamed: opts.userRenamed,
        submit_naming: opts.submitNaming,
      });
      return { treeId: res.tree_id, taskCount: res.task_ids.length };
    },
    confirmSuccessToast: ({ submitNaming, taskCount }) =>
      submitNaming
        ? `Tree persisted. ${taskCount} naming tasks queued.`
        : "Tree persisted with placeholder names — run 'Regenerate names' later to LLM-name clusters.",
  },
  "recluster-subtree": {
    title: (purpose) => {
      if (purpose.kind !== "recluster-subtree") return "";
      return `Subcluster: "${purpose.nodeName ?? purpose.nodeId}"`;
    },
    defaultForm: (purpose) => {
      const nodeName =
        purpose.kind === "recluster-subtree" ? purpose.nodeName ?? "subtree" : "subtree";
      return {
        name: `Subcluster ${nodeName}`,
        lifecycle: "sapling",
        scope: { kind: "vault" },
        sourceTypes: ["md", "txt"],
        method: "cluster",
        carryPoliciesDown: false,
        clusterParams: defaultClusterParams(),
        folderParams: defaultFolderParams(),
      };
    },
    showLifecycleRow: false,
    showScopeRow: false,
    showCarryPoliciesRow: true,
    runStructural: (form, purpose) => {
      if (purpose.kind !== "recluster-subtree") {
        throw new Error("runStructural: expected recluster-subtree purpose");
      }
      return Api.runStructural({
        method_json: methodJsonFromForm(form),
        recluster_target: {
          tree_id: purpose.treeId,
          node_id: purpose.nodeId,
        },
      });
    },
    confirm: async (form, purpose, result, opts) => {
      if (purpose.kind !== "recluster-subtree") {
        throw new Error("confirm: expected recluster-subtree purpose");
      }
      await Api.reclusterFromBuilt({
        tree_id: purpose.treeId,
        node_id: purpose.nodeId,
        tree: result.tree,
        carry_policies_down: form.carryPoliciesDown,
        user_renamed: opts.userRenamed,
        submit_naming: opts.submitNaming,
      });
      return { treeId: purpose.treeId, taskCount: 0 };
    },
    confirmSuccessToast: ({ submitNaming }) =>
      submitNaming
        ? "Subtree reclustered. Naming tasks queued."
        : "Subtree reclustered with placeholder names — run 'Regenerate names' later to LLM-name clusters.",
    summaryExtras: (form) => [`carry=${form.carryPoliciesDown}`],
  },
  rebuild: {
    title: () => "Cluster review: rebuild",
    defaultForm: () => {
      return {
        name: "rebuild",
        lifecycle: "evergreen",
        scope: { kind: "vault" },
        sourceTypes: ["md", "txt"],
        method: "cluster",
        carryPoliciesDown: false,
        clusterParams: defaultClusterParams(),
        folderParams: defaultFolderParams(),
      };
    },
    showLifecycleRow: true,
    showScopeRow: true,
    showCarryPoliciesRow: false,
    runStructural: (form) =>
      Api.runStructural({
        scope_json: scopeJsonFromForm(form),
        method_json: methodJsonFromForm(form),
      }),
    confirm: async (form, _purpose, result, opts) => {
      const source = form.lifecycle === "evergreen" ? "saved-triage" : "one-shot";
      const res = await Api.persist({
        name: form.name.trim() || "untitled",
        source,
        scope_json: result.scope_json,
        method_json: result.method_json,
        tree: result.tree,
        user_renamed: opts.userRenamed,
        submit_naming: opts.submitNaming,
      });
      return { treeId: res.tree_id, taskCount: res.task_ids.length };
    },
    confirmSuccessToast: ({ submitNaming, taskCount }) =>
      submitNaming
        ? `Tree persisted. ${taskCount} naming tasks queued.`
        : "Tree persisted with placeholder names — run 'Regenerate names' later to LLM-name clusters.",
    onMount: (state, repaint) => prefillRebuild(state, repaint),
  },
};
