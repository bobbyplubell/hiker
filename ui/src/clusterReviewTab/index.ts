// status: cluster-review-tab
// status: cluster-review-tab-kind
// status: cluster-review-tab-from-new-tree-action
// status: cluster-review-tab-from-recluster-action
// status: cluster-review-tab-rebuild-prefill
// status: cluster-review-tab-config-section
// status: cluster-review-tab-run-clustering
// status: cluster-review-tab-iterate
// status: cluster-review-tab-structure-preview
// status: cluster-review-tab-rename-before-llm
// status: cluster-review-tab-confirm-and-name
// status: cluster-review-tab-confirm-skip-naming
// status: cluster-review-tab-transition-to-pane
// status: cluster-review-tab-no-persistence-until-confirm
// status: cluster-review-tab-discard
// status: cluster-review-tab-deduplication
// status: cluster-review-tab-advanced-disclosure
//
// Clustering review tab — replaces the legacy `+ Suggest reorganization`
// / "Recluster subtree…" modals. Hosts the full clustering build flow:
//
//   1. Configure (name / lifecycle / scope / method / include-outliers
//      / carry-policies-down / advanced disclosure)
//   2. Run structural pass via `cluster_run_structural` (HDBSCAN only,
//      no LLM)
//   3. Review the in-memory result; inline-rename placeholder cluster
//      names if desired
//   4. Confirm — persists the tree and submits LLM naming tasks for
//      un-renamed clusters; the tab flips in place to the existing
//      `cluster-pane` kind for the newly-persisted tree.
//
// Per-tab state lives in this module's `panes` map keyed by tab key.
// Result + form state are in-memory only — autosave records the kind +
// the synthetic path key, not the form; restoring a tab returns to the
// configure phase with defaults.

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { confirmDanger } from "../widgets/confirm";

// ── Wire types ──────────────────────────────────────────────────────

export interface BuiltClusterNode {
  id: string;
  members: string[];
  centroid: number[];
  radius: number;
  name: string;
  summary: string;
  confidence: number;
}

export interface BuiltClusterTree {
  levels: BuiltClusterNode[][];
  outliers: string[];
}

export interface StructuralBuildDto {
  scope_json: string;
  method_json: string;
  tree: BuiltClusterTree;
  note_titles: Record<string, string>;
}

export type Purpose =
  | { kind: "new-tree" }
  | { kind: "recluster-subtree"; treeId: string; nodeId: string; nodeName?: string }
  | { kind: "rebuild"; treeId: string };

interface LeidenParams {
  k_nearest: number;
  edge_weight_floor: number;
  iterations: number;
  min_cluster_size: number;
  resolution: number;
}

interface ClusterParams {
  algorithm: "hdbscan" | "hybrid" | "gmm" | "leiden";
  min_cluster_size: number;
  min_samples: number | null;
  min_clusters_to_recurse: number;
  summary_confidence_threshold: number;
  include_outliers: boolean;
  summarize: "llm" | "none";
  // status: cluster-leiden-params
  leiden: LeidenParams;
  // status: cluster-review-tab-disable-recursion
  disable_recursion: boolean;
}

interface FolderParams {
  summarize: "llm" | "none";
  include_outliers: boolean;
  outlier_threshold: number;
}

interface ClusterReviewForm {
  name: string;
  /// One of "sapling" | "evergreen". Ignored for non-new-tree purposes.
  lifecycle: "sapling" | "evergreen";
  /// "vault" | { folder: rel } | { notes: ids[] }. Ignored for non-new-tree.
  scope: { kind: "vault" } | { kind: "folder"; rel: string } | { kind: "notes"; ids: string[] };
  /// status: cluster-build-scope-source-types
  /// File-extension filter applied on top of `scope`. Empty array = "every
  /// indexable extension" (matches legacy behavior). Otherwise the build
  /// pass + triage classifier only see notes whose extension is in the
  /// list. `"md"` covers both `.md` and `.markdown`.
  sourceTypes: string[];
  method: "cluster" | "folders";
  /// Carry-policies-down (recluster only).
  carryPoliciesDown: boolean;
  /// Cluster method params (advanced disclosure).
  clusterParams: ClusterParams;
  /// FromFolders method params (advanced disclosure).
  folderParams: FolderParams;
}

interface PaneState {
  key: string;
  purpose: Purpose;
  form: ClusterReviewForm;
  /// Set of build-pass cluster ids the user has renamed.
  userRenamed: Map<string, string>;
  result: StructuralBuildDto | null;
  /// True while a structural pass is in-flight.
  running: boolean;
  /// True while a Confirm pass is in-flight.
  confirming: boolean;
  /// True when the Configuration section is collapsed (auto-collapses
  /// after the first successful Run).
  configCollapsed: boolean;
  /// True when the Advanced disclosure inside the Configuration section
  /// is open. Persisted across repaints (method / algorithm switches)
  /// so toggling either of those doesn't collapse it.
  advancedOpen: boolean;
}

export interface ClusterReviewDeps {
  rootEl: HTMLElement;
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// Flip the current tab in place to the cluster-pane for `treeId`
  /// (post-Confirm transition). Called once per Confirm.
  transitionToPane: (tabKey: string, treeId: string, treeName: string) => Promise<void> | void;
  /// Close the tab by key (used by the Discard button).
  closeTab: (tabKey: string) => void;
  /// Check whether the LLM is enabled in settings. Drives the Confirm
  /// button's disabled state + tooltip. Resolved lazily on each render.
  llmEnabled: () => Promise<boolean>;
}

export interface ClusterReviewApi {
  /// Open (or activate) the review tab for `purpose`. Returns the tab
  /// key the host should hand to its tab-activation machinery.
  open(purpose: Purpose): string;
  /// Look up an existing tab for `purpose`. Returns the key or null.
  findKey(purpose: Purpose): string | null;
  /// Render the tab whose key matches the current host-active tab.
  showTab(tabKey: string): void;
  /// True when this tab is unsaved + has a result (the close-guard
  /// prompts before dropping it).
  hasUnsavedResult(tabKey: string): boolean;
  /// Drop the pane state for `tabKey` (called from `onTabClosed`).
  dropTab(tabKey: string): void;
  /// Hide the pane root entirely (called from `renderActiveTab` when
  /// the active tab is not a cluster-review tab).
  hide(): void;
}

const Api = {
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

// ── Module state ────────────────────────────────────────────────────

const panes = new Map<string, PaneState>();

function purposeToKey(purpose: Purpose): string {
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

function defaultForm(purpose: Purpose): ClusterReviewForm {
  const today = new Date().toISOString().slice(0, 10);
  switch (purpose.kind) {
    case "new-tree":
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
    case "recluster-subtree":
      return {
        name: `Subcluster ${purpose.nodeName ?? "subtree"}`,
        lifecycle: "sapling",
        scope: { kind: "vault" },
        sourceTypes: ["md", "txt"],
        method: "cluster",
        carryPoliciesDown: false,
        clusterParams: defaultClusterParams(),
        folderParams: defaultFolderParams(),
      };
    case "rebuild":
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
  }
}

// ── Mount ────────────────────────────────────────────────────────────

export function mountClusterReviewTab(deps: ClusterReviewDeps): ClusterReviewApi {
  const root = deps.rootEl;
  root.classList.add("cluster-review-tab");

  // status: cluster-review-tab-rebuild-prefill
  //
  // Best-effort prefill of the form for the `rebuild` purpose. Fires
  // after open() to fetch the tree's saved scope/method JSON; failures
  // fall back to defaults silently.
  function prefillRebuild(state: PaneState): void {
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
      paint(state);
    });
  }

  function open(purpose: Purpose): string {
    const key = purposeToKey(purpose);
    const existing = panes.get(key);
    if (existing) {
      // status: cluster-review-tab-deduplication
      // Preserve form + result on re-activation.
      return key;
    }
    const state: PaneState = {
      key,
      purpose,
      form: defaultForm(purpose),
      userRenamed: new Map(),
      result: null,
      running: false,
      confirming: false,
      configCollapsed: false,
      advancedOpen: false,
    };
    panes.set(key, state);
    if (purpose.kind === "rebuild") prefillRebuild(state);
    return key;
  }

  function findKey(purpose: Purpose): string | null {
    const key = purposeToKey(purpose);
    return panes.has(key) ? key : null;
  }

  function showTab(tabKey: string): void {
    const state = panes.get(tabKey);
    root.hidden = !state;
    if (!state) return;
    paint(state);
  }

  function hide(): void {
    root.hidden = true;
  }

  function dropTab(tabKey: string): void {
    panes.delete(tabKey);
  }

  function hasUnsavedResult(tabKey: string): boolean {
    const state = panes.get(tabKey);
    return !!(state && state.result);
  }

  // ── Render ────────────────────────────────────────────────────────

  function paint(state: PaneState): void {
    root.replaceChildren();

    root.appendChild(renderHeader(state));
    root.appendChild(renderActions(state));
    root.appendChild(renderConfig(state));
    root.appendChild(renderResult(state));
  }

  function renderHeader(state: PaneState): HTMLElement {
    const h = document.createElement("h2");
    h.className = "crt-title";
    let title = "Cluster review: new tree";
    if (state.purpose.kind === "recluster-subtree") {
      title = `Subcluster: "${state.purpose.nodeName ?? state.purpose.nodeId}"`;
    } else if (state.purpose.kind === "rebuild") {
      title = `Cluster review: rebuild`;
    }
    h.textContent = title;
    return h;
  }

  function renderActions(state: PaneState): HTMLElement {
    const row = document.createElement("div");
    row.className = "crt-actions";

    const runBtn = document.createElement("button");
    runBtn.type = "button";
    runBtn.className = "crt-btn crt-btn-primary";
    runBtn.textContent = state.running
      ? "Running…"
      : state.result
        ? "Re-run clustering"
        : "Run clustering";
    runBtn.disabled = state.running || state.confirming;
    runBtn.addEventListener("click", () => void runStructural(state));
    row.appendChild(runBtn);

    const confirmBtn = document.createElement("button");
    confirmBtn.type = "button";
    confirmBtn.className = "crt-btn crt-btn-primary";
    confirmBtn.textContent = state.confirming ? "Submitting…" : "Confirm and name →";
    confirmBtn.disabled = state.running || state.confirming || !state.result;
    confirmBtn.addEventListener("click", () => void confirmAndName(state, true));
    row.appendChild(confirmBtn);
    // Lazy LLM-enabled check — if disabled, override the disabled state
    // and tooltip. The skip-naming button below is NOT gated on this
    // since its whole point is to avoid the LLM call.
    void deps.llmEnabled().then((enabled) => {
      if (!enabled && !state.confirming && state.result) {
        confirmBtn.disabled = true;
        confirmBtn.title =
          "LLM is disabled in settings. Enable [llm] to name clusters.";
      }
    });

    // status: cluster-review-tab-confirm-skip-naming
    // Second confirm button — persists the tree but skips the LLM
    // naming pass, so placeholder names ("Cluster 1", "Cluster 2", …)
    // stay intact. Now offered for the recluster path too: the
    // `cluster_op_recluster_subtree_from_built` command honors the same
    // `submit_naming: false` flag and short-circuits its
    // `RaptorSummarize` submission loop.
    const confirmNoNameBtn = document.createElement("button");
    confirmNoNameBtn.type = "button";
    confirmNoNameBtn.className = "crt-btn";
    confirmNoNameBtn.textContent = state.confirming
      ? "Submitting…"
      : "Confirm (no naming)";
    confirmNoNameBtn.disabled =
      state.running || state.confirming || !state.result;
    confirmNoNameBtn.title =
      "Persist the tree with placeholder cluster names. Run 'Regenerate names' from the cluster pane later to LLM-name clusters.";
    confirmNoNameBtn.addEventListener("click", () =>
      void confirmAndName(state, false),
    );
    row.appendChild(confirmNoNameBtn);

    const discardBtn = document.createElement("button");
    discardBtn.type = "button";
    discardBtn.className = "crt-btn";
    discardBtn.textContent = "Discard";
    discardBtn.disabled = state.running || state.confirming;
    discardBtn.addEventListener("click", () => void discard(state));
    row.appendChild(discardBtn);

    return row;
  }

  // status: cluster-review-tab-config-section
  function renderConfig(state: PaneState): HTMLElement {
    const wrap = document.createElement("section");
    wrap.className = "crt-section";
    const header = document.createElement("button");
    header.type = "button";
    header.className = "crt-section-header";
    header.textContent = state.configCollapsed ? "▸ Configuration" : "▾ Configuration";
    header.addEventListener("click", () => {
      state.configCollapsed = !state.configCollapsed;
      paint(state);
    });
    wrap.appendChild(header);

    if (state.configCollapsed) {
      const summary = document.createElement("div");
      summary.className = "crt-section-summary";
      summary.textContent = oneLineConfigSummary(state);
      wrap.appendChild(summary);
      return wrap;
    }

    const body = document.createElement("div");
    body.className = "crt-section-body";

    // Name
    body.appendChild(renderTextRow("Name", state.form.name, (v) => {
      state.form.name = v;
    }));

    // Lifecycle — hidden for recluster
    if (state.purpose.kind !== "recluster-subtree") {
      body.appendChild(
        renderRadioRow(
          "Lifecycle",
          [
            { value: "sapling", label: "Sapling — one-shot reorganization" },
            { value: "evergreen", label: "Evergreen — save as active triage" },
          ],
          state.form.lifecycle,
          (v) => {
            state.form.lifecycle = v as "sapling" | "evergreen";
          },
        ),
      );
    }

    // Scope — hidden for recluster
    if (state.purpose.kind !== "recluster-subtree") {
      const scopeKind = state.form.scope.kind;
      const scopeRow = renderRadioRow(
        "Scope",
        [
          { value: "vault", label: "Whole vault" },
          { value: "folder", label: "Folder" },
        ],
        scopeKind === "notes" ? "vault" : scopeKind,
        (v) => {
          if (v === "vault") state.form.scope = { kind: "vault" };
          else state.form.scope = { kind: "folder", rel: "" };
          paint(state);
        },
      );
      body.appendChild(scopeRow);
      if (state.form.scope.kind === "folder") {
        const folder = state.form.scope.rel;
        body.appendChild(
          renderTextRow("Folder (vault-relative)", folder, (v) => {
            if (state.form.scope.kind === "folder") {
              state.form.scope = { kind: "folder", rel: v };
            }
          }),
        );
      }
    }

    // status: cluster-build-scope-source-types
    // Source-type filter — applies after scope. Always visible (also on
    // recluster, since a recluster of a mixed-type subtree may want to
    // narrow to one type). Empty selection is rejected at Run-time below
    // since "no types selected" produces an empty input set.
    {
      const wrap = document.createElement("div");
      wrap.className = "crt-row";
      const lbl = document.createElement("span");
      lbl.className = "crt-row-label";
      lbl.textContent = "Source types";
      wrap.appendChild(lbl);
      const group = document.createElement("span");
      group.className = "crt-radio-group";
      const renderTypeBox = (id: string, label: string) => {
        const box = document.createElement("label");
        box.className = "crt-radio";
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = state.form.sourceTypes.includes(id);
        cb.addEventListener("change", () => {
          const cur = new Set(state.form.sourceTypes);
          if (cb.checked) cur.add(id);
          else cur.delete(id);
          state.form.sourceTypes = Array.from(cur);
        });
        box.appendChild(cb);
        const txt = document.createElement("span");
        txt.textContent = label;
        box.appendChild(txt);
        return box;
      };
      group.appendChild(renderTypeBox("md", "Markdown (.md)"));
      group.appendChild(renderTypeBox("txt", "Plain text (.txt)"));
      wrap.appendChild(group);
      body.appendChild(wrap);
    }

    // Method
    body.appendChild(
      renderRadioRow(
        "Method",
        [
          { value: "cluster", label: "Cluster (RAPTOR-shaped)" },
          { value: "folders", label: "From folders" },
        ],
        state.form.method,
        (v) => {
          state.form.method = v as "cluster" | "folders";
          paint(state);
        },
      ),
    );

    // Include outliers
    body.appendChild(
      renderCheckboxRow(
        "Include outliers",
        state.form.method === "cluster"
          ? state.form.clusterParams.include_outliers
          : state.form.folderParams.include_outliers,
        (v) => {
          if (state.form.method === "cluster") {
            state.form.clusterParams.include_outliers = v;
          } else {
            state.form.folderParams.include_outliers = v;
          }
        },
      ),
    );

    // Carry policies down — recluster only
    if (state.purpose.kind === "recluster-subtree") {
      body.appendChild(
        renderCheckboxRow(
          "Carry policies down from selected node",
          state.form.carryPoliciesDown,
          (v) => {
            state.form.carryPoliciesDown = v;
          },
        ),
      );
    }

    // status: cluster-review-tab-advanced-disclosure
    body.appendChild(renderAdvanced(state));

    wrap.appendChild(body);
    return wrap;
  }

  function renderAdvanced(state: PaneState): HTMLElement {
    const det = document.createElement("details");
    det.className = "crt-advanced";
    det.open = state.advancedOpen;
    det.addEventListener("toggle", () => {
      state.advancedOpen = det.open;
    });
    const sum = document.createElement("summary");
    sum.textContent = "Advanced";
    det.appendChild(sum);

    if (state.form.method === "cluster") {
      const p = state.form.clusterParams;
      const selectRow = (label: string, options: string[], current: string, onChange: (v: string) => void) => {
        const wrap = document.createElement("label");
        wrap.className = "crt-row";
        const lbl = document.createElement("span");
        lbl.className = "crt-row-label";
        lbl.textContent = label;
        wrap.appendChild(lbl);
        const sel = document.createElement("select");
        sel.className = "crt-input";
        for (const o of options) {
          const opt = document.createElement("option");
          opt.value = o;
          opt.textContent = o;
          if (o === current) opt.selected = true;
          sel.appendChild(opt);
        }
        sel.addEventListener("change", () => onChange(sel.value));
        wrap.appendChild(sel);
        return wrap;
      };
      // status: cluster-leiden
      // Algorithm select drives which tunables are shown below.
      det.appendChild(selectRow("algorithm", ["hdbscan", "leiden", "hybrid", "gmm"], p.algorithm, (v) => {
        p.algorithm = v as ClusterParams["algorithm"];
        paint(state);
      }));
      if (p.algorithm === "leiden") {
        // status: cluster-leiden-params
        const lp = p.leiden;
        det.appendChild(renderNumberRow("k_nearest", String(lp.k_nearest), 2, 1, (v) => {
          lp.k_nearest = Math.max(2, v);
        }));
        det.appendChild(renderNumberRow("edge_weight_floor", String(lp.edge_weight_floor), 0, 0.05, (v) => {
          lp.edge_weight_floor = v;
        }));
        det.appendChild(renderNumberRow("resolution (γ; higher = finer)", String(lp.resolution), 0, 0.1, (v) => {
          lp.resolution = Math.max(0, v);
        }));
        det.appendChild(renderNumberRow("iterations", String(lp.iterations), 1, 10, (v) => {
          lp.iterations = Math.max(1, v);
        }));
        det.appendChild(renderNumberRow("min_cluster_size", String(lp.min_cluster_size), 1, 1, (v) => {
          lp.min_cluster_size = Math.max(1, v);
        }));
      } else {
        det.appendChild(renderNumberRow("min_cluster_size", String(p.min_cluster_size), 2, 1, (v) => {
          p.min_cluster_size = Math.max(2, v);
        }));
        det.appendChild(renderNumberRow(
          "min_samples (blank for auto)",
          p.min_samples == null ? "" : String(p.min_samples),
          1, 1,
          (v, raw) => {
            if (raw.trim() === "") p.min_samples = null;
            else p.min_samples = Math.max(1, v);
          },
        ));
      }
      // Common tunables — apply across algorithms.
      det.appendChild(renderNumberRow("min_clusters_to_recurse", String(p.min_clusters_to_recurse), 2, 1, (v) => {
        p.min_clusters_to_recurse = Math.max(2, v);
      }));
      det.appendChild(renderNumberRow(
        "summary_confidence_threshold",
        String(p.summary_confidence_threshold),
        0, 0.05,
        (v) => {
          p.summary_confidence_threshold = v;
        },
      ));
      // status: cluster-review-tab-disable-recursion
      // Toggle that short-circuits the recursive merge loop in
      // `core::cluster::build_cluster_tree`. Surfaces as a checkbox so
      // it reads as opt-in rather than overloading numeric tunables.
      const disableRow = document.createElement("label");
      disableRow.className = "crt-row";
      const dLbl = document.createElement("span");
      dLbl.className = "crt-row-label";
      dLbl.textContent = "disable_recursion (single-level tree)";
      disableRow.appendChild(dLbl);
      const dCb = document.createElement("input");
      dCb.type = "checkbox";
      dCb.checked = p.disable_recursion;
      dCb.addEventListener("change", () => {
        p.disable_recursion = dCb.checked;
      });
      disableRow.appendChild(dCb);
      det.appendChild(disableRow);
    } else {
      const p = state.form.folderParams;
      if (p.include_outliers) {
        det.appendChild(renderNumberRow(
          "outlier_threshold",
          String(p.outlier_threshold),
          0, 0.05,
          (v) => {
            p.outlier_threshold = v;
          },
        ));
      } else {
        const note = document.createElement("p");
        note.className = "crt-row-note";
        note.textContent = "(Outlier threshold only applies when Include outliers is on.)";
        det.appendChild(note);
      }
    }
    return det;
  }

  function renderResult(state: PaneState): HTMLElement {
    const wrap = document.createElement("section");
    wrap.className = "crt-section";
    const header = document.createElement("div");
    header.className = "crt-section-header crt-section-header-static";
    let label = "▾ Result";
    if (state.result) {
      const tree = state.result.tree;
      const leafCount = tree.levels[0]?.length ?? 0;
      const memberTotal = tree.levels[0]?.reduce((a, c) => a + c.members.length, 0) ?? 0;
      label = `▾ Result (${leafCount} clusters, ${memberTotal} notes, ${tree.outliers.length} outliers) — structural only`;
    } else if (state.running) {
      label = "▾ Result — running…";
    } else {
      label = "▾ Result — click Run clustering to build";
    }
    header.textContent = label;
    wrap.appendChild(header);

    const body = document.createElement("div");
    body.className = "crt-section-body";

    if (state.running) {
      const p = document.createElement("p");
      p.className = "crt-empty";
      p.textContent = "Clustering in progress — this runs locally, no LLM calls.";
      body.appendChild(p);
    } else if (!state.result) {
      const p = document.createElement("p");
      p.className = "crt-empty";
      p.textContent = "No result yet.";
      body.appendChild(p);
    } else {
      const list = document.createElement("div");
      list.className = "crt-result-list";
      const tree = state.result.tree;
      const leaf = tree.levels[0] ?? [];
      // Display in member-count-descending order (matches the build's
      // placeholder-naming order, so "Cluster 1" lines up at the top).
      const ordered = [...leaf].sort((a, b) => b.members.length - a.members.length);
      for (const c of ordered) {
        list.appendChild(renderResultCluster(state, c));
      }
      if (tree.outliers.length > 0) {
        list.appendChild(renderResultOutliers(state, tree.outliers));
      }
      body.appendChild(list);
    }

    wrap.appendChild(body);
    return wrap;
  }

  // status: cluster-review-tab-structure-preview
  // status: cluster-review-tab-rename-before-llm
  function renderResultCluster(state: PaneState, c: BuiltClusterNode): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "crt-cluster";

    const head = document.createElement("div");
    head.className = "crt-cluster-head";
    const ic = document.createElement("span");
    ic.className = "crt-cluster-icon";
    ic.textContent = "◉";
    head.appendChild(ic);
    const nameSpan = document.createElement("span");
    nameSpan.className = "crt-cluster-name";
    const displayName = state.userRenamed.get(c.id) ?? c.name;
    nameSpan.textContent = displayName;
    if (state.userRenamed.has(c.id)) nameSpan.classList.add("crt-cluster-name-edited");
    nameSpan.title = "Click to rename before Confirm";
    nameSpan.addEventListener("click", () => beginInlineEdit(nameSpan, displayName, (v) => {
      const trimmed = v.trim();
      if (!trimmed || trimmed === c.name) {
        state.userRenamed.delete(c.id);
      } else {
        state.userRenamed.set(c.id, trimmed);
      }
      paint(state);
    }));
    head.appendChild(nameSpan);
    const cnt = document.createElement("span");
    cnt.className = "crt-cluster-count";
    cnt.textContent = `(${c.members.length})`;
    head.appendChild(cnt);
    if (c.radius > 0) {
      const rad = document.createElement("span");
      rad.className = "crt-cluster-radius";
      rad.textContent = `r=${c.radius.toFixed(2)}`;
      rad.title = "90th-percentile member distance from centroid";
      head.appendChild(rad);
    }
    wrap.appendChild(head);

    const titles = state.result?.note_titles ?? {};
    const sample = c.members.slice(0, 3);
    const members = document.createElement("ul");
    members.className = "crt-cluster-members";
    for (const id of sample) {
      const li = document.createElement("li");
      li.textContent = titles[id] ?? id;
      members.appendChild(li);
    }
    if (c.members.length > sample.length) {
      const li = document.createElement("li");
      li.className = "crt-cluster-members-more";
      li.textContent = `… and ${c.members.length - sample.length} more`;
      members.appendChild(li);
    }
    wrap.appendChild(members);
    return wrap;
  }

  function renderResultOutliers(state: PaneState, outliers: string[]): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "crt-cluster crt-cluster-outliers";
    const head = document.createElement("div");
    head.className = "crt-cluster-head";
    const ic = document.createElement("span");
    ic.className = "crt-cluster-icon";
    ic.textContent = "◇";
    head.appendChild(ic);
    const name = document.createElement("span");
    name.className = "crt-cluster-name";
    name.textContent = "Outliers";
    head.appendChild(name);
    const cnt = document.createElement("span");
    cnt.className = "crt-cluster-count";
    cnt.textContent = `(${outliers.length})`;
    head.appendChild(cnt);
    wrap.appendChild(head);

    const titles = state.result?.note_titles ?? {};
    const sample = outliers.slice(0, 3);
    const list = document.createElement("ul");
    list.className = "crt-cluster-members";
    for (const id of sample) {
      const li = document.createElement("li");
      li.textContent = titles[id] ?? id;
      list.appendChild(li);
    }
    if (outliers.length > sample.length) {
      const li = document.createElement("li");
      li.className = "crt-cluster-members-more";
      li.textContent = `… and ${outliers.length - sample.length} more`;
      list.appendChild(li);
    }
    wrap.appendChild(list);
    return wrap;
  }

  // ── Form-row helpers ──────────────────────────────────────────────

  function renderTextRow(label: string, value: string, onChange: (v: string) => void): HTMLElement {
    const wrap = document.createElement("label");
    wrap.className = "crt-row";
    const lbl = document.createElement("span");
    lbl.className = "crt-row-label";
    lbl.textContent = label;
    wrap.appendChild(lbl);
    const inp = document.createElement("input");
    inp.type = "text";
    inp.className = "crt-input";
    inp.value = value;
    inp.addEventListener("input", () => onChange(inp.value));
    wrap.appendChild(inp);
    return wrap;
  }

  function renderNumberRow(
    label: string,
    initial: string,
    min: number,
    step: number,
    onChange: (n: number, raw: string) => void,
  ): HTMLElement {
    const wrap = document.createElement("label");
    wrap.className = "crt-row";
    const lbl = document.createElement("span");
    lbl.className = "crt-row-label";
    lbl.textContent = label;
    wrap.appendChild(lbl);
    const inp = document.createElement("input");
    inp.type = "number";
    inp.className = "crt-input";
    inp.value = initial;
    inp.min = String(min);
    inp.step = String(step);
    inp.addEventListener("input", () => {
      const raw = inp.value;
      const n = Number(raw);
      if (Number.isFinite(n)) onChange(n, raw);
      else onChange(min, raw);
    });
    wrap.appendChild(inp);
    return wrap;
  }

  function renderCheckboxRow(label: string, checked: boolean, onChange: (v: boolean) => void): HTMLElement {
    const wrap = document.createElement("label");
    wrap.className = "crt-row crt-row-checkbox";
    const inp = document.createElement("input");
    inp.type = "checkbox";
    inp.checked = checked;
    inp.addEventListener("change", () => onChange(inp.checked));
    wrap.appendChild(inp);
    const lbl = document.createElement("span");
    lbl.className = "crt-row-label";
    lbl.textContent = label;
    wrap.appendChild(lbl);
    return wrap;
  }

  function renderRadioRow(
    label: string,
    options: { value: string; label: string }[],
    current: string,
    onChange: (v: string) => void,
  ): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "crt-row";
    const lbl = document.createElement("span");
    lbl.className = "crt-row-label";
    lbl.textContent = label;
    wrap.appendChild(lbl);
    const group = document.createElement("div");
    group.className = "crt-radio-group";
    for (const o of options) {
      const rowLab = document.createElement("label");
      rowLab.className = "crt-radio";
      const inp = document.createElement("input");
      inp.type = "radio";
      inp.name = label;
      inp.value = o.value;
      inp.checked = o.value === current;
      inp.addEventListener("change", () => {
        if (inp.checked) onChange(o.value);
      });
      rowLab.appendChild(inp);
      rowLab.appendChild(document.createTextNode(" " + o.label));
      group.appendChild(rowLab);
    }
    wrap.appendChild(group);
    return wrap;
  }

  function beginInlineEdit(
    el: HTMLElement,
    initial: string,
    commit: (v: string) => void,
  ): void {
    const input = document.createElement("input");
    input.type = "text";
    input.className = "crt-inline-edit";
    input.value = initial;
    const parent = el.parentElement;
    if (!parent) return;
    parent.replaceChild(input, el);
    input.focus();
    input.select();
    let done = false;
    const finish = (save: boolean) => {
      if (done) return;
      done = true;
      if (save) commit(input.value);
      else parent.replaceChild(el, input);
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        finish(true);
      } else if (e.key === "Escape") {
        e.preventDefault();
        finish(false);
      }
    });
    input.addEventListener("blur", () => finish(true));
  }

  function oneLineConfigSummary(state: PaneState): string {
    const bits: string[] = [];
    bits.push(`name=${JSON.stringify(state.form.name)}`);
    if (state.purpose.kind !== "recluster-subtree") {
      bits.push(`lifecycle=${state.form.lifecycle}`);
      if (state.form.scope.kind === "vault") bits.push("scope=vault");
      else if (state.form.scope.kind === "folder") bits.push(`scope=folder:${state.form.scope.rel}`);
    }
    bits.push(`types=${state.form.sourceTypes.join("+") || "none"}`);
    bits.push(`method=${state.form.method}`);
    if (state.form.method === "cluster") {
      bits.push(`min_cs=${state.form.clusterParams.min_cluster_size}`);
    }
    if (state.purpose.kind === "recluster-subtree") {
      bits.push(`carry=${state.form.carryPoliciesDown}`);
    }
    return bits.join("  ·  ");
  }

  // ── Run / Confirm / Discard ──────────────────────────────────────

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

  // status: cluster-review-tab-run-clustering
  // status: cluster-review-tab-iterate
  async function runStructural(state: PaneState): Promise<void> {
    if (state.running) return;
    // status: cluster-build-scope-source-types — guard against an empty
    // type selection. An empty `sourceTypes` array means "every indexable
    // extension," which is the legacy default; an empty *selection* on
    // the form is a different state (the user unchecked everything) and
    // would produce an empty note set. Flag it before the IPC round-trip.
    if (state.form.sourceTypes.length === 0) {
      showToast("Select at least one source type (Markdown or Plain text)");
      return;
    }
    state.running = true;
    state.result = null;
    state.userRenamed.clear();
    paint(state);
    try {
      const methodJson = methodJsonFromForm(state.form);
      let result: StructuralBuildDto;
      if (state.purpose.kind === "recluster-subtree") {
        result = await Api.runStructural({
          method_json: methodJson,
          recluster_target: {
            tree_id: state.purpose.treeId,
            node_id: state.purpose.nodeId,
          },
        });
      } else {
        const scopeJson = scopeJsonFromForm(state.form);
        result = await Api.runStructural({
          scope_json: scopeJson,
          method_json: methodJson,
        });
      }
      state.result = result;
      state.configCollapsed = true;
      showToast(
        `Clustering done — ${result.tree.levels[0]?.length ?? 0} clusters, ${result.tree.outliers.length} outliers`,
      );
    } catch (err) {
      Logger.error("ui::clusterReviewTab", "run failed", { err });
      showToast(`Clustering failed: ${formatErr(err)}`);
    } finally {
      state.running = false;
      paint(state);
    }
  }

  // status: cluster-review-tab-confirm-and-name
  // status: cluster-review-tab-confirm-skip-naming
  // status: cluster-review-tab-transition-to-pane
  //
  // `submitNaming = false` is the "Confirm (no naming)" path: persist
  // the tree with placeholder cluster names ("Cluster 1", "Cluster 2",
  // …) but don't enqueue `RaptorSummarize` tasks. The user can run
  // "Regenerate names" from the cluster pane later. Both the new-tree /
  // rebuild path (`cluster_persist_built_tree`) and the recluster path
  // (`cluster_op_recluster_subtree_from_built`) honor the flag and
  // short-circuit their `RaptorSummarize` submission loop.
  async function confirmAndName(
    state: PaneState,
    submitNaming: boolean,
  ): Promise<void> {
    if (state.confirming || !state.result) return;
    if (submitNaming) {
      const enabled = await deps.llmEnabled();
      if (!enabled) {
        showToast("LLM is disabled in settings — enable [llm] before Confirm");
        return;
      }
    }
    state.confirming = true;
    paint(state);
    try {
      const renamedRecord: Record<string, string> = {};
      for (const [k, v] of state.userRenamed.entries()) renamedRecord[k] = v;
      if (state.purpose.kind === "recluster-subtree") {
        await Api.reclusterFromBuilt({
          tree_id: state.purpose.treeId,
          node_id: state.purpose.nodeId,
          tree: state.result.tree,
          carry_policies_down: state.form.carryPoliciesDown,
          user_renamed: renamedRecord,
          submit_naming: submitNaming,
        });
        if (submitNaming) {
          showToast("Subtree reclustered. Naming tasks queued.");
        } else {
          showToast(
            "Subtree reclustered with placeholder names — run 'Regenerate names' later to LLM-name clusters.",
          );
        }
        await deps.transitionToPane(state.key, state.purpose.treeId, state.form.name);
      } else {
        const source = state.form.lifecycle === "evergreen" ? "saved-triage" : "one-shot";
        const res = await Api.persist({
          name: state.form.name.trim() || "untitled",
          source,
          scope_json: state.result.scope_json,
          method_json: state.result.method_json,
          tree: state.result.tree,
          user_renamed: renamedRecord,
          submit_naming: submitNaming,
        });
        if (submitNaming) {
          showToast(`Tree persisted. ${res.task_ids.length} naming tasks queued.`);
        } else {
          showToast(
            "Tree persisted with placeholder names — run 'Regenerate names' later to LLM-name clusters.",
          );
        }
        await deps.transitionToPane(state.key, res.tree_id, state.form.name);
      }
      // Tab has flipped to cluster-pane; drop our pane state.
      panes.delete(state.key);
    } catch (err) {
      Logger.error("ui::clusterReviewTab", "confirm failed", { err });
      showToast(`Confirm failed: ${formatErr(err)}`);
      state.confirming = false;
      paint(state);
    }
  }

  // status: cluster-review-tab-discard
  async function discard(state: PaneState): Promise<void> {
    if (state.result) {
      const ok = await confirmDanger(
        "Discard the clustering result? You'll need to re-run to get it back.",
        "Discard",
      );
      if (!ok) return;
    }
    panes.delete(state.key);
    deps.closeTab(state.key);
  }

  return { open, findKey, showTab, hasUnsavedResult, dropTab, hide };
}

function formatErr(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
