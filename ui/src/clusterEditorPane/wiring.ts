// status: cluster-editor-pane-mode, cluster-editor-batch-review-pane-mode
// status: cluster-review-tab-kind, cluster-review-tab
// status: cluster-review-tab-from-new-tree-action
// status: cluster-review-tab-from-recluster-action
// status: cluster-review-tab-rebuild-prefill
// status: cluster-review-tab-deduplication
//
// Host wiring for the cluster-editor pane + cluster-review tab. Reads
// singletons directly.

import { Ipc, invokeWithLogging } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { mountClusterEditorPane, type ClusterEditorPaneApi } from "./index";
import {
  mountClusterReviewTab,
  type ClusterReviewApi,
  type Purpose as ClusterReviewPurpose,
} from "../clusterReviewTab";
import type { Buffer } from "../app/state";
import { getOpenBuffers, bumpActivationCounter } from "../app/state";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export interface ClusterPaneWiringApi {
  clusterEditorPane: ClusterEditorPaneApi | null;
  clusterReviewTab: ClusterReviewApi | null;
  clusterPaneEl: HTMLElement | null;
  clusterReviewPaneEl: HTMLElement | null;
  openClusterReviewTab(purpose: ClusterReviewPurpose): void;
  openClusterTab(treeId: string, treeName?: string): Promise<void>;
  getCurrentClusterTabKey(): string | null;
}

export function setupClusterPaneWiring(): ClusterPaneWiringApi {
  let currentClusterTabKey: string | null = null;

  // status: cluster-editor-pane-mode, cluster-editor-batch-review-pane-mode
  const clusterPaneEl = document.getElementById("cluster-editor-pane");
  let clusterEditorPane: ClusterEditorPaneApi | null = null;
  if (clusterPaneEl) {
    clusterEditorPane = mountClusterEditorPane({
      rootEl: clusterPaneEl,
      openNote: (rel, opts) => services.openFile(rel, opts ?? {}).then(() => undefined),
      closePane: () => {
        const key = currentClusterTabKey;
        if (key) {
          void services.closeTab(key);
        }
      },
      // status: cluster-review-tab-from-recluster-action
      openReclusterReview: (treeId, nodeId, nodeName) =>
        openClusterReviewTab({
          kind: "recluster-subtree",
          treeId,
          nodeId,
          nodeName,
        }),
    });
  }

  // status: cluster-review-tab-kind
  // status: cluster-review-tab
  //
  // Clustering review tab — mounted into `#cluster-review-pane`. The
  // module owns its own per-tab state (form + in-memory structural
  // result); the host plumbs the surrounding tab lifecycle (open / show /
  // close-guard / transition-to-pane-on-confirm).
  const clusterReviewPaneEl = document.getElementById("cluster-review-pane");
  let clusterReviewTab: ClusterReviewApi | null = null;
  if (clusterReviewPaneEl) {
    clusterReviewTab = mountClusterReviewTab({
      rootEl: clusterReviewPaneEl,
      openNote: (rel, opts) => services.openFile(rel, opts ?? {}).then(() => undefined),
      transitionToPane: async (tabKey, treeId, treeName) => {
        // Tab transitions in place — drop the cluster-review buffer entry,
        // open a cluster-pane tab in its slot. The cluster-pane mount
        // (`openClusterTab`) re-uses the existing key shape, so we close
        // the review tab then open the pane tab; the user's "current tab"
        // visibly shifts to the new one.
        getOpenBuffers().delete(tabKey);
        controllers.autosave.get().scheduleTabStatePush();
        await openClusterTab(treeId, treeName);
        // Sidebar's Cluster trees list reads `cluster_trees_list` on mount
        // and on explicit refresh — kick a refresh so the just-persisted
        // tree (new-tree case) or the reshaped subtree (recluster case)
        // shows up without requiring a vault re-open.
        void controllers.clusterEditor.tryGet()?.refresh();
      },
      closeTab: (tabKey) => {
        void services.closeTab(tabKey);
      },
      llmEnabled: async () => {
        try {
          const cfg = await Ipc.getSettings<{ llm: { enabled: boolean } }>();
          return !!cfg.llm?.enabled;
        } catch {
          return false;
        }
      },
    });
  }

  // status: cluster-review-tab-from-new-tree-action
  // status: cluster-review-tab-from-recluster-action
  // status: cluster-review-tab-rebuild-prefill
  // status: cluster-review-tab-deduplication
  //
  // Open or activate the clustering review tab for `purpose`. Dedup keys:
  // `new-tree` is a singleton; recluster keys on `(treeId, nodeId)`;
  // rebuild keys on `treeId`. Re-opening the same purpose activates the
  // existing tab and preserves the form + result state.
  function openClusterReviewTab(purpose: ClusterReviewPurpose): void {
    if (!clusterReviewTab) {
      showToast("Cluster review pane is unavailable in this build");
      return;
    }
    const openBuffers = getOpenBuffers();
    const key = clusterReviewTab.open(purpose);
    const existing = openBuffers.get(key);
    let label = "Cluster review";
    if (purpose.kind === "new-tree") label = "Cluster review: new tree";
    else if (purpose.kind === "recluster-subtree") {
      label = `Subcluster: ${purpose.nodeName ?? ""}`.trim();
    } else if (purpose.kind === "rebuild") label = "Cluster review: rebuild";
    if (existing) {
      existing.buffer.displayLabel = label;
      services.activateTabInner(key);
      return;
    }
    const buf: Buffer = {
      path: key,
      loadedText: "",
      token: null,
      kind: "cluster-review",
      displayLabel: label,
      mode: { kind: "file" },
      pendingChangesMetadata: null,
      preview: false,
    };
    openBuffers.set(key, {
      buffer: buf,
      savedState: null,
      lastActivatedAt: bumpActivationCounter(),
    });
    services.activateTabInner(key);
    controllers.autosave.get().scheduleTabStatePush();
  }

  // status: cluster-editor-pane-mode
  /// Open or activate the cluster-editor pane tab for `treeId`. The tab
  /// is sticky (no preview slot) — the pane is heavy enough that a
  /// preview-eviction surprise would be jarring. Re-opening for the same
  /// tree re-paints the tree view without losing batch-review state.
  async function openClusterTab(treeId: string, treeName?: string): Promise<void> {
    const openBuffers = getOpenBuffers();
    const key = services.appPageTabKey("cluster-pane", treeId);
    currentClusterTabKey = key;
    const existing = openBuffers.get(key);
    if (existing) {
      if (treeName) existing.buffer.displayLabel = treeName;
      services.activateTabInner(key);
      await clusterEditorPane?.showTree(treeId);
      if (!treeName) void hydrateClusterTabLabel(key, treeId);
      return;
    }
    const buf: Buffer = {
      path: key,
      loadedText: "",
      token: null,
      kind: "cluster-pane",
      displayLabel: treeName ?? "Cluster",
      mode: { kind: "cluster-tree", treeId },
      pendingChangesMetadata: null,
      preview: false,
    };
    openBuffers.set(key, {
      buffer: buf,
      savedState: null,
      lastActivatedAt: bumpActivationCounter(),
    });
    services.activateTabInner(key);
    await clusterEditorPane?.showTree(treeId);
    if (!treeName) void hydrateClusterTabLabel(key, treeId);
  }

  // Fetch the tree row and copy its `name` onto the open buffer so the
  // tab strip shows it (instead of the `__hiker:cluster-pane:<id>` key).
  // Only relevant when the caller didn't pass a name — the autosave-restore
  // path opens cluster tabs without one.
  async function hydrateClusterTabLabel(key: string, treeId: string): Promise<void> {
    try {
      const rows = await invokeWithLogging<Array<{ id: string; name: string }>>(
        "cluster_trees_list",
        undefined,
      );
      const row = rows.find((r) => r.id === treeId);
      if (!row) return;
      const entry = getOpenBuffers().get(key);
      if (!entry) return;
      entry.buffer.displayLabel = row.name;
      controllers.tabStrip.get().render();
    } catch (err) {
      Logger.error("ui::clusterEditor", "hydrateClusterTabLabel failed", { err });
    }
  }

  return {
    clusterEditorPane,
    clusterReviewTab,
    clusterPaneEl,
    clusterReviewPaneEl,
    openClusterReviewTab,
    openClusterTab,
    getCurrentClusterTabKey: () => currentClusterTabKey,
  };
}
