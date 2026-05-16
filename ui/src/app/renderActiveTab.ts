// status: tab-kinds
//
// Re-evaluates editor chrome + app-page surface visibility based on the
// active tab's kind discriminator. Called on every status pulse.
// Reads singletons directly.

import {
  getActivePath,
  getBuffer,
  getOpenBuffers,
} from "./state";
import type { TabKind } from "./state";
import { dom } from "./dom";
import { controllers } from "./controllers";

export function createRenderActiveTab(): () => void {
  function activeKind(): TabKind | null {
    const b = getBuffer();
    if (!b) return null;
    return b.kind;
  }

  return function renderActiveTab(): void {
    const d = dom();
    const appEl = d.editor.appEl;
    const saveBtn = d.editor.saveBtn;
    const diffBtn = d.editor.diffBtn;
    const modeControlsEl = d.editor.modeControlsEl;
    const writeNotePendingBannerEl = d.editor.writeNotePendingBannerEl;
    const vaultHome = controllers.vaultHome.get();
    const queueDetail = controllers.queueDetail.get();
    const settingsPane = controllers.settingsPane.get();
    const propertiesPane = controllers.propertiesPane.get();
    const clusterWiring = controllers.clusterWiring.get();

    const kind = activeKind();
    const isBuffer = kind === "buffer";
    d.editor.editorEl.hidden = !isBuffer;
    // Buffer-only toolbar buttons
    saveBtn.hidden = !isBuffer;
    diffBtn.hidden = !isBuffer;
    d.editor.viewMenuBtn.hidden = !isBuffer;
    d.editor.mutationsMenuBtn.hidden = !isBuffer;
    modeControlsEl.hidden = !isBuffer;
    // status: write-note-pending-banner — buffer-scoped chrome.
    // `refreshWriteNotePendingBanner` already enforces this on its own
    // path; force-hide here so non-buffer tabs never leak a stale banner
    // from a previously active buffer.
    if (!isBuffer) writeNotePendingBannerEl.hidden = true;
    const trailPill = document.getElementById("add-to-trail-pill");
    if (trailPill) trailPill.hidden = !isBuffer;
    const statusBarEl = document.getElementById("status-bar");
    if (statusBarEl) statusBarEl.hidden = !isBuffer;
    // Page-kind surfaces
    const isHome = kind === "home";
    const isHomeDetail = kind === "home-detail";
    const isQueue = kind === "queue";
    const isSettings = kind === "settings";
    const isProperties = kind === "properties";
    // status: cluster-editor-pane-mode
    const isClusterPane = kind === "cluster-pane";
    // status: cluster-review-tab-kind
    const isClusterReview = kind === "cluster-review";
    d.vaultHome.rootEl.hidden = !(isHome || isHomeDetail || isQueue);
    d.settingsPane.paneEl.hidden = !isSettings;
    d.propertiesPane.paneEl.hidden = !isProperties;
    const clusterPaneEl = clusterWiring.clusterPaneEl;
    if (clusterPaneEl) clusterPaneEl.hidden = !isClusterPane;
    if (!isClusterPane) {
      clusterWiring.clusterEditorPane?.hide();
    }
    const clusterReviewPaneEl = clusterWiring.clusterReviewPaneEl;
    if (clusterReviewPaneEl) clusterReviewPaneEl.hidden = !isClusterReview;
    const activePath = getActivePath();
    const clusterReviewTab = clusterWiring.clusterReviewTab;
    if (isClusterReview && activePath) {
      clusterReviewTab?.showTab(activePath);
    } else {
      clusterReviewTab?.hide();
    }
    // Hide the editor toolbar in the cluster-pane / cluster-review tabs —
    // each has its own toolbar (the cluster-review tab carries Run /
    // Confirm / Discard) and the editor toolbar's controls (mutations
    // menu, view options, dirty-diff toggle, etc) don't apply.
    const editorToolbarEl = document.getElementById("editor-toolbar");
    if (editorToolbarEl) editorToolbarEl.hidden = isClusterPane || isClusterReview;
    d.vaultHome.overviewEl.hidden = !isHome;
    d.vaultHome.detailEl.hidden = !isHomeDetail;
    d.vaultHome.queueDetailEl.hidden = !isQueue;
    if (isHome) void vaultHome.api.refresh();
    if (isHomeDetail) {
      const key = activePath;
      let view = "recent-activity";
      if (key && key.startsWith("__hiker:home-detail:")) {
        view = key.slice("__hiker:home-detail:".length);
      }
      if (view === "recent-activity") vaultHome.api.showDetail(view);
    }
    if (isQueue) {
      queueDetail.setVisible(true);
      queueDetail.api.setFilter("tasks");
    }
    if (isSettings) {
      void settingsPane.refresh();
    } else {
      if (settingsPane.isVisible()) void settingsPane.setVisible(false);
    }
    if (isProperties) {
      const key = activePath;
      if (key && key.startsWith("__hiker:properties:")) {
        const rel = key.slice("__hiker:properties:".length);
        void propertiesPane.update(rel);
      }
    }
    // Active state for home/settings buttons
    d.vaultBar.homeBtn.classList.toggle("active", isHome || isHomeDetail);
    d.vaultBar.settingsBtn.classList.toggle("active", isSettings);
    // Collapse docked chat while any agent tab is open
    const hasAgentTab = [...getOpenBuffers().values()].some(e => e.buffer.kind === "agent");
    appEl.classList.toggle("agent-tab-open", hasAgentTab);
  };
}
