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

import { Logger } from "../logger";
import { describeErr as formatErr } from "../ipc/runCommand";
import { showToast } from "../widgets/toast";
import { confirmDanger } from "../widgets/confirm";
import { el } from "../widgets/dom";
import {
  beginInlineEdit,
  renderCheckboxRow,
  renderNumberRow,
  renderRadioRow,
  renderTextRow,
} from "./formRows";

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

export interface LeidenParams {
  k_nearest: number;
  edge_weight_floor: number;
  iterations: number;
  min_cluster_size: number;
  resolution: number;
}

export interface ClusterParams {
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

export interface FolderParams {
  summarize: "llm" | "none";
  include_outliers: boolean;
  outlier_threshold: number;
}

export interface ClusterReviewForm {
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

export interface PaneState {
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

import { PURPOSE_CONFIGS, purposeToKey } from "./purposeConfigs";

// ── Module state ────────────────────────────────────────────────────

const panes = new Map<string, PaneState>();

// ── Mount ────────────────────────────────────────────────────────────

export function mountClusterReviewTab(deps: ClusterReviewDeps): ClusterReviewApi {
  const root = deps.rootEl;
  root.classList.add("cluster-review-tab");

  function open(purpose: Purpose): string {
    const key = purposeToKey(purpose);
    const existing = panes.get(key);
    if (existing) {
      // status: cluster-review-tab-deduplication
      // Preserve form + result on re-activation.
      return key;
    }
    const cfg = PURPOSE_CONFIGS[purpose.kind];
    const state: PaneState = {
      key,
      purpose,
      form: cfg.defaultForm(purpose),
      userRenamed: new Map(),
      result: null,
      running: false,
      confirming: false,
      configCollapsed: false,
      advancedOpen: false,
    };
    panes.set(key, state);
    if (cfg.onMount) void cfg.onMount(state, paint);
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
    return el("h2", {
      class: "crt-title",
      text: PURPOSE_CONFIGS[state.purpose.kind].title(state.purpose),
    });
  }

  function renderActions(state: PaneState): HTMLElement {
    const confirmBtn = el("button", {
      class: "crt-btn crt-btn-primary",
      text: state.confirming ? "Submitting…" : "Confirm and name →",
      attrs: { type: "button" },
      disabled: state.running || state.confirming || !state.result,
      onClick: () => void confirmAndName(state, true),
    });
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

    return el("div", { class: "crt-actions" }, [
      el("button", {
        class: "crt-btn crt-btn-primary",
        text: state.running
          ? "Running…"
          : state.result
            ? "Re-run clustering"
            : "Run clustering",
        attrs: { type: "button" },
        disabled: state.running || state.confirming,
        onClick: () => void runStructural(state),
      }),
      confirmBtn,
      // status: cluster-review-tab-confirm-skip-naming
      // Second confirm button — persists the tree but skips the LLM
      // naming pass, so placeholder names ("Cluster 1", "Cluster 2", …)
      // stay intact. Now offered for the recluster path too: the
      // `cluster_op_recluster_subtree_from_built` command honors the same
      // `submit_naming: false` flag and short-circuits its
      // `RaptorSummarize` submission loop.
      el("button", {
        class: "crt-btn",
        text: state.confirming ? "Submitting…" : "Confirm (no naming)",
        attrs: { type: "button" },
        disabled: state.running || state.confirming || !state.result,
        title:
          "Persist the tree with placeholder cluster names. Run 'Regenerate names' from the cluster pane later to LLM-name clusters.",
        onClick: () => void confirmAndName(state, false),
      }),
      el("button", {
        class: "crt-btn",
        text: "Discard",
        attrs: { type: "button" },
        disabled: state.running || state.confirming,
        onClick: () => void discard(state),
      }),
    ]);
  }

  // status: cluster-review-tab-config-section
  function renderConfig(state: PaneState): HTMLElement {
    const cfg = PURPOSE_CONFIGS[state.purpose.kind];
    const wrap = el("section", { class: "crt-section" }, [
      el("button", {
        class: "crt-section-header",
        text: state.configCollapsed ? "▸ Configuration" : "▾ Configuration",
        attrs: { type: "button" },
        onClick: () => {
          state.configCollapsed = !state.configCollapsed;
          paint(state);
        },
      }),
    ]);

    if (state.configCollapsed) {
      wrap.appendChild(el("div", {
        class: "crt-section-summary",
        text: oneLineConfigSummary(state),
      }));
      return wrap;
    }

    const body = el("div", { class: "crt-section-body" });

    // Name
    body.appendChild(renderTextRow("Name", state.form.name, (v) => {
      state.form.name = v;
    }));

    // Lifecycle — hidden for recluster
    if (cfg.showLifecycleRow) {
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
    if (cfg.showScopeRow) {
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
      const renderTypeBox = (id: string, label: string) => {
        const cb = el("input", {
          attrs: { type: "checkbox" },
          on: {
            change: () => {
              const cur = new Set(state.form.sourceTypes);
              if (cb.checked) cur.add(id);
              else cur.delete(id);
              state.form.sourceTypes = Array.from(cur);
            },
          },
        });
        cb.checked = state.form.sourceTypes.includes(id);
        return el("label", { class: "crt-radio" }, [
          cb,
          el("span", { text: label }),
        ]);
      };
      body.appendChild(el("div", { class: "crt-row" }, [
        el("span", { class: "crt-row-label", text: "Source types" }),
        el("span", { class: "crt-radio-group" }, [
          renderTypeBox("md", "Markdown (.md)"),
          renderTypeBox("txt", "Plain text (.txt)"),
        ]),
      ]));
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
    if (cfg.showCarryPoliciesRow) {
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
    const det = el("details", {
      class: "crt-advanced",
      on: { toggle: () => { state.advancedOpen = det.open; } },
    }, [
      el("summary", { text: "Advanced" }),
    ]);
    det.open = state.advancedOpen;

    if (state.form.method === "cluster") {
      const p = state.form.clusterParams;
      const selectRow = (label: string, options: string[], current: string, onChange: (v: string) => void) => {
        const sel = el("select", {
          class: "crt-input",
          on: { change: () => onChange(sel.value) },
        }, options.map((o) => {
          const opt = el("option", { text: o });
          opt.value = o;
          if (o === current) opt.selected = true;
          return opt;
        }));
        return el("label", { class: "crt-row" }, [
          el("span", { class: "crt-row-label", text: label }),
          sel,
        ]);
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
      const dCb = el("input", {
        attrs: { type: "checkbox" },
        on: { change: () => { p.disable_recursion = dCb.checked; } },
      });
      dCb.checked = p.disable_recursion;
      det.appendChild(el("label", { class: "crt-row" }, [
        el("span", {
          class: "crt-row-label",
          text: "disable_recursion (single-level tree)",
        }),
        dCb,
      ]));
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
        det.appendChild(el("p", {
          class: "crt-row-note",
          text: "(Outlier threshold only applies when Include outliers is on.)",
        }));
      }
    }
    return det;
  }

  function renderResult(state: PaneState): HTMLElement {
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

    const body = el("div", { class: "crt-section-body" });

    if (state.running) {
      body.appendChild(el("p", {
        class: "crt-empty",
        text: "Clustering in progress — this runs locally, no LLM calls.",
      }));
    } else if (!state.result) {
      body.appendChild(el("p", { class: "crt-empty", text: "No result yet." }));
    } else {
      const list = el("div", { class: "crt-result-list" });
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

    return el("section", { class: "crt-section" }, [
      el("div", {
        class: "crt-section-header crt-section-header-static",
        text: label,
      }),
      body,
    ]);
  }

  // status: cluster-review-tab-structure-preview
  // status: cluster-review-tab-rename-before-llm
  function renderResultCluster(state: PaneState, c: BuiltClusterNode): HTMLElement {
    const displayName = state.userRenamed.get(c.id) ?? c.name;
    const isEdited = state.userRenamed.has(c.id);
    const nameSpan = el("span", {
      class: isEdited
        ? "crt-cluster-name crt-cluster-name-edited"
        : "crt-cluster-name",
      text: displayName,
      title: "Click to rename before Confirm",
    });
    nameSpan.addEventListener("click", () => beginInlineEdit(nameSpan, displayName, (v) => {
      const trimmed = v.trim();
      if (!trimmed || trimmed === c.name) {
        state.userRenamed.delete(c.id);
      } else {
        state.userRenamed.set(c.id, trimmed);
      }
      paint(state);
    }));

    const headChildren: (ChildNode | null)[] = [
      el("span", { class: "crt-cluster-icon", text: "◉" }),
      nameSpan,
      el("span", { class: "crt-cluster-count", text: `(${c.members.length})` }),
    ];
    if (c.radius > 0) {
      headChildren.push(el("span", {
        class: "crt-cluster-radius",
        text: `r=${c.radius.toFixed(2)}`,
        title: "90th-percentile member distance from centroid",
      }));
    }

    const titles = state.result?.note_titles ?? {};
    const sample = c.members.slice(0, 3);
    const memberItems: HTMLElement[] = sample.map((id) =>
      el("li", { text: titles[id] ?? id }),
    );
    if (c.members.length > sample.length) {
      memberItems.push(el("li", {
        class: "crt-cluster-members-more",
        text: `… and ${c.members.length - sample.length} more`,
      }));
    }
    return el("div", { class: "crt-cluster" }, [
      el("div", { class: "crt-cluster-head" }, headChildren),
      el("ul", { class: "crt-cluster-members" }, memberItems),
    ]);
  }

  function renderResultOutliers(state: PaneState, outliers: string[]): HTMLElement {
    const titles = state.result?.note_titles ?? {};
    const sample = outliers.slice(0, 3);
    const memberItems: HTMLElement[] = sample.map((id) =>
      el("li", { text: titles[id] ?? id }),
    );
    if (outliers.length > sample.length) {
      memberItems.push(el("li", {
        class: "crt-cluster-members-more",
        text: `… and ${outliers.length - sample.length} more`,
      }));
    }
    return el("div", { class: "crt-cluster crt-cluster-outliers" }, [
      el("div", { class: "crt-cluster-head" }, [
        el("span", { class: "crt-cluster-icon", text: "◇" }),
        el("span", { class: "crt-cluster-name", text: "Outliers" }),
        el("span", { class: "crt-cluster-count", text: `(${outliers.length})` }),
      ]),
      el("ul", { class: "crt-cluster-members" }, memberItems),
    ]);
  }

  function oneLineConfigSummary(state: PaneState): string {
    const cfg = PURPOSE_CONFIGS[state.purpose.kind];
    const bits: string[] = [];
    bits.push(`name=${JSON.stringify(state.form.name)}`);
    if (cfg.showLifecycleRow) {
      bits.push(`lifecycle=${state.form.lifecycle}`);
    }
    if (cfg.showScopeRow) {
      if (state.form.scope.kind === "vault") bits.push("scope=vault");
      else if (state.form.scope.kind === "folder") bits.push(`scope=folder:${state.form.scope.rel}`);
    }
    bits.push(`types=${state.form.sourceTypes.join("+") || "none"}`);
    bits.push(`method=${state.form.method}`);
    if (state.form.method === "cluster") {
      bits.push(`min_cs=${state.form.clusterParams.min_cluster_size}`);
    }
    if (cfg.summaryExtras) {
      for (const extra of cfg.summaryExtras(state.form)) bits.push(extra);
    }
    return bits.join("  ·  ");
  }

  // ── Run / Confirm / Discard ──────────────────────────────────────

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
      const cfg = PURPOSE_CONFIGS[state.purpose.kind];
      const result = await cfg.runStructural(state.form, state.purpose);
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
      const cfg = PURPOSE_CONFIGS[state.purpose.kind];
      const { treeId, taskCount } = await cfg.confirm(state.form, state.purpose, state.result, {
        submitNaming,
        userRenamed: renamedRecord,
      });
      showToast(cfg.confirmSuccessToast({ submitNaming, taskCount }));
      await deps.transitionToPane(state.key, treeId, state.form.name);
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
