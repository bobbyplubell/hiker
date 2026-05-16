// status: cluster-editor-sidebar-mode
// status: cluster-editor-multiple-trees-open
// status: cluster-editor-mode-menu
// status: cluster-editor-new-tree-action
// status: cluster-editor-tree-creation-modal
// status: cluster-editor-build-params-advanced-disclosure
// status: cluster-editor-build-scope-picker
// status: cluster-editor-outlier-virtual-node
// status: cluster-editor-node-operations
// status: cluster-editor-move-note-between-clusters
// status: cluster-editor-merge-siblings
// status: cluster-editor-merge-children-up
// status: cluster-editor-split-cluster
// status: cluster-editor-recluster-subtree
// status: cluster-editor-recluster-subtree-policy-loss
// status: cluster-editor-recluster-subtree-placement-decoupled
// status: cluster-editor-drop-cluster
// status: cluster-editor-promote-outlier
// status: cluster-editor-edit-name-summary
// status: cluster-editor-user-edit-provenance
// status: cluster-editor-multi-select
// status: cluster-editor-undo-redo
// status: cluster-editor-discard-draft
// status: cluster-editor-pane-expand
// status: cluster-editor-pane-leaf-click-opens-note (sidebar path — leaf row click opens note)
// status: cluster-editor-multi-select-stage-move
// status: cluster-editor-multi-select-stage-tag
//
// Cluster-trees sidebar body. Hosts the list of open trees (saved
// triage + ephemeral one-shot drafts) with inline hierarchical rows.
// All reshape ops route through the per-tree Tauri commands wired by
// `cluster_*` in `ui/src-tauri/src/lib.rs`. The row-level rendering +
// context menus + multi-select toolbar live in `./rowPrimitive.ts`
// (per `cluster-editor-row-primitive`); this module is responsible
// only for the surface chrome (header, tree list, per-tree
// expand/collapse + undo/redo + Expand-to-pane buttons) and the
// surface-local state.

import { invoke } from "@tauri-apps/api/core";
import { onHikerEventAs } from "../events";
import { Logger } from "../logger";
import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { describeErr } from "../ipc/runCommand";
import { Icons } from "../icons";
import {
  Api,
  onDragStateChange,
  renderMultiSelectToolbar,
  renderPromoteBand,
  renderSiblingsWithOutliers,
  type ClusterNodeRow,
  type TreeRowDeps,
  type TreeRowSurfaceState,
} from "./rowPrimitive";

export type { ClusterNodeRow, ClusterTreeRow } from "./rowPrimitive";

// Sidebar-only surface API calls (the shared primitive owns the row-
// level Api shim).
const SidebarApi = {
  create(args: {
    name: string;
    source: string;
    scopeJson: string;
    methodJson: string;
  }): Promise<string> {
    return invoke("cluster_tree_create", { args: {
      name: args.name,
      source: args.source,
      scope_json: args.scopeJson,
      method_json: args.methodJson,
    } });
  },
  discard(treeId: string): Promise<void> {
    return invoke("cluster_tree_discard", { treeId });
  },
  undo(treeId: string): Promise<boolean> {
    return invoke("cluster_tree_undo", { treeId });
  },
  redo(treeId: string): Promise<boolean> {
    return invoke("cluster_tree_redo", { treeId });
  },
};

// ── Module state ────────────────────────────────────────────────────

export interface ClusterEditorDeps {
  rootEl: HTMLElement;
  /// Open a note in the editor pane.
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// status: cluster-editor-pane-expand, cluster-editor-pane-mode
  openPane?: (treeId: string, treeName?: string) => Promise<void> | void;
  /// status: cluster-review-tab-from-new-tree-action
  openNewTreeReview: () => void;
  /// status: cluster-review-tab-from-recluster-action
  openReclusterReview: (treeId: string, nodeId: string, nodeName: string) => void;
}

export interface ClusterEditorApi {
  refresh: () => Promise<void>;
  newTree: () => void;
  openModeMenu: (triggerEl: HTMLElement) => void;
}

interface TreeUIState extends TreeRowSurfaceState {
  open: boolean; // whether the tree's header section is expanded
}

export function mountClusterEditor(deps: ClusterEditorDeps): ClusterEditorApi {
  const root = deps.rootEl;
  root.classList.add("cluster-editor");
  let trees: TreeUIState[] = [];

  // ── Render ────────────────────────────────────────────────────────

  function paint(): void {
    root.replaceChildren();

    const header = document.createElement("div");
    header.className = "ce-header";
    const titleEl = document.createElement("span");
    titleEl.className = "ce-header-title";
    titleEl.textContent = "Cluster trees";
    header.appendChild(titleEl);
    root.appendChild(header);

    const action = document.createElement("button");
    action.type = "button";
    action.className = "ce-action-primary";
    action.textContent = "+ Suggest reorganization";
    // status: cluster-review-tab-from-new-tree-action
    action.addEventListener("click", () => deps.openNewTreeReview());
    root.appendChild(action);

    if (trees.length === 0) {
      const empty = document.createElement("p");
      empty.className = "ce-empty";
      empty.textContent = "No trees yet. Click “Suggest reorganization” to build one.";
      root.appendChild(empty);
      return;
    }

    for (const t of trees) {
      root.appendChild(renderTree(t));
    }
  }

  function rowDeps(state: TreeUIState): TreeRowDeps {
    return {
      refresh: () => refreshOne(state),
      repaint: () => paint(),
      openNote: deps.openNote,
      openReclusterReview: deps.openReclusterReview,
    };
  }

  function renderTree(state: TreeUIState): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "ce-tree";

    const head = document.createElement("div");
    head.className = "ce-tree-head";
    const chev = document.createElement("span");
    chev.className = "ce-chev";
    chev.textContent = state.open ? "▾" : "▸";
    chev.addEventListener("click", (e) => {
      e.stopPropagation();
      state.open = !state.open;
      paint();
    });
    head.appendChild(chev);
    const nameSpan = document.createElement("span");
    nameSpan.className = "ce-tree-name";
    nameSpan.textContent = state.tree.name;
    head.appendChild(nameSpan);
    const pill = document.createElement("span");
    pill.className = `ce-state-pill ce-state-${state.tree.state}`;
    pill.textContent = state.tree.state;
    head.appendChild(pill);
    const spacer = document.createElement("span");
    spacer.style.flex = "1";
    head.appendChild(spacer);
    if (state.open && state.selection.size > 0) {
      head.appendChild(renderMultiSelectToolbar(state, rowDeps(state)));
    }
    const undoBtn = document.createElement("button");
    undoBtn.type = "button";
    undoBtn.className = "ce-tree-icon-btn";
    undoBtn.title = "Undo";
    undoBtn.textContent = "↶";
    undoBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        const did = await SidebarApi.undo(state.tree.id);
        if (!did) {
          showToast("Nothing to undo");
          return;
        }
        await refreshOne(state);
      } catch (err) {
        Logger.error("ui::clusterEditor", "undo failed", { err });
        showToast(`Undo failed: ${describeErr(err)}`);
      }
    });
    head.appendChild(undoBtn);
    const redoBtn = document.createElement("button");
    redoBtn.type = "button";
    redoBtn.className = "ce-tree-icon-btn";
    redoBtn.title = "Redo";
    redoBtn.textContent = "↷";
    redoBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        const did = await SidebarApi.redo(state.tree.id);
        if (!did) {
          showToast("Nothing to redo");
          return;
        }
        await refreshOne(state);
      } catch (err) {
        Logger.error("ui::clusterEditor", "redo failed", { err });
        showToast(`Redo failed: ${describeErr(err)}`);
      }
    });
    head.appendChild(redoBtn);
    const expandBtn = document.createElement("button");
    expandBtn.type = "button";
    expandBtn.className = "ce-tree-icon-btn";
    expandBtn.title = "Expand";
    expandBtn.setAttribute("aria-label", "Expand");
    expandBtn.innerHTML = Icons.expand({ size: 12, strokeWidth: 1.4 });
    expandBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      // status: cluster-editor-pane-expand
      if (deps.openPane) {
        void deps.openPane(state.tree.id, state.tree.name);
      } else {
        showToast("Expanded pane is unavailable in this build");
      }
    });
    head.appendChild(expandBtn);
    head.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      openTreeRowMenu(state, e.clientX, e.clientY);
    });
    wrap.appendChild(head);

    if (state.open) {
      const body = document.createElement("div");
      body.className = "ce-tree-body";
      // status: cluster-editor-dnd-visual-feedback
      // Promote-to-top band renders above the tree's root list while a
      // drag is in flight; surfaces below the band still receive drop
      // events normally.
      const band = renderPromoteBand(state, rowDeps(state));
      if (band) body.appendChild(band);
      const rootNodes = state.nodes.filter((n) => n.parent === null);
      const renderedRoot = renderSiblingsWithOutliers(state, rowDeps(state), rootNodes, 0);
      for (const el of renderedRoot) body.appendChild(el);
      wrap.appendChild(body);
    }
    return wrap;
  }

  function openTreeRowMenu(state: TreeUIState, x: number, y: number): void {
    const items: CtxMenuItem[] = [
      {
        label: "Discard draft",
        run: async () => {
          if (!confirm(`Discard draft "${state.tree.name}"? Edits will be lost.`)) return;
          try {
            await SidebarApi.discard(state.tree.id);
            await refresh();
          } catch (err) {
            showToast(`Discard failed: ${describeErr(err)}`);
          }
        },
      },
      {
        label: "Collapse all",
        run: () => {
          state.expanded.clear();
          paint();
        },
      },
    ];
    openContextMenu(x, y, items);
  }

  // ── Data ──────────────────────────────────────────────────────────

  async function refresh(): Promise<void> {
    try {
      const rows = await Api.list();
      const prev = new Map(trees.map((t) => [t.tree.id, t]));
      const next: TreeUIState[] = [];
      for (const r of rows) {
        const old = prev.get(r.id);
        const nodes = await Api.get(r.id).catch((err) => {
          Logger.error("ui::clusterEditor", "tree_get failed", { err });
          return [] as ClusterNodeRow[];
        });
        next.push({
          tree: r,
          nodes,
          expanded: old?.expanded ?? new Set<string>(),
          open: old?.open ?? next.length === 0,
          selection: old?.selection ?? new Set<string>(),
        });
      }
      trees = next;
      paint();
    } catch (err) {
      Logger.error("ui::clusterEditor", "refresh failed", { err });
    }
  }

  async function refreshOne(state: TreeUIState): Promise<void> {
    try {
      state.nodes = await Api.get(state.tree.id);
      paint();
    } catch (err) {
      Logger.error("ui::clusterEditor", "refreshOne failed", { err });
    }
  }

  function openModeMenu(triggerEl: HTMLElement): void {
    const rect = triggerEl.getBoundingClientRect();
    const items: CtxMenuItem[] = [
      {
        // status: cluster-review-tab-from-new-tree-action
        label: "New tree…",
        run: () => deps.openNewTreeReview(),
      },
      {
        label: "Discard all drafts",
        run: async () => {
          const drafts = trees.filter((t) => t.tree.source === "one-shot");
          if (drafts.length === 0) {
            showToast("No drafts to discard");
            return;
          }
          if (!confirm(`Discard ${drafts.length} draft(s)?`)) return;
          for (const d of drafts) {
            try {
              await SidebarApi.discard(d.tree.id);
            } catch (err) {
              Logger.error("ui::clusterEditor", "discard failed", { err });
            }
          }
          await refresh();
        },
      },
      {
        label: "Refresh",
        run: () => void refresh(),
      },
    ];
    openContextMenu(rect.right, rect.bottom, items, triggerEl);
  }

  // status: cluster-editor-dnd-visual-feedback
  // Re-paint when a drag starts or ends so the promote-to-top band
  // surfaces and tears down without polling.
  onDragStateChange(() => paint());

  // Initial load.
  void refresh();

  // Auto-refresh when a cluster-build task completes.
  const pendingClusterBuilds = new Set<string>();
  const CLUSTER_BUILD_KINDS = new Set([
    "cluster_build_tree",
    "cluster_rebuild_tree",
    "cluster_recluster_subtree",
    "raptor_summarize",
  ]);
  type QueueEvt = {
    event: string;
    id?: string;
    kind?: { type?: string };
  };
  void onHikerEventAs<QueueEvt>("hiker:queue-event", (payload) => {
    const p = payload;
    if (p.event === "task_queued") {
      const kindType = p.kind?.type;
      if (p.id && kindType && CLUSTER_BUILD_KINDS.has(kindType)) {
        pendingClusterBuilds.add(p.id);
      }
      return;
    }
    if (
      p.id &&
      (p.event === "task_completed" ||
        p.event === "task_failed" ||
        p.event === "task_cancelled")
    ) {
      if (pendingClusterBuilds.delete(p.id)) {
        void refresh();
      }
    }
  });

  return {
    refresh,
    newTree: () => deps.openNewTreeReview(),
    openModeMenu,
  };
}
