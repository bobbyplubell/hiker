// Phase 3 — vault lifecycle, top-strip controls, sidebar surfaces
// (tree, discovery, trails, clusters, sidebar mode), nav setup,
// vault-home, queue-detail, agent-changes, properties-pane,
// app-page-tabs.
//
// Preconditions: phase 2 (editor + status coordinator + setReadOnly are
// in ctx; chatPanel + patchReview + settingsPane in controllers).
// Outputs (controllers): vaultLifecycle, tree, vaultHome, queueDetail,
// nav, sidebarMode, discovery, trailsPanel, clusterEditor,
// clusterWiring, propertiesPane, appPageTabs.
// Outputs (services): focusSearchInput, openClusterTab,
// openClusterReviewTab, refreshTree, scheduleTreeRefreshFromWatcher,
// panelToast, navBack, navForward, checkpointNav, openAppPageTab,
// openPropertiesTab, openAgentTab, openSettingsPage.
// Outputs (ctx): refreshTree, revealInTree, openAppPageTab.
//
// status: bootstrap-phase-split

import { Ipc } from "../../ipc";
import { mountTree } from "../../tree";
import { mountVaultHome } from "../../vaultHome";
import { mountQueueDetail } from "../../queueDetail";
import { setupDiscovery } from "../../discovery/setup";
import { mountTrailsPanel } from "../../trails";
import { mountClusterEditor } from "../../clusterEditor";
import {
  installMembershipWatchers,
  refreshActiveTrailWaypointPaths,
} from "../../trails/membership";
import { mountPropertiesPane } from "../../propertiesPane";
import { setupSidebarMode } from "../../panels/sidebarMode";
import { installSidebarActions } from "../../panels/sidebarActions";
import { setupNavigation } from "../../navigation/setup";
import { setupClusterPaneWiring } from "../../clusterEditorPane/wiring";
import { mountAgentChanges } from "../agentChanges";
import { mountVaultLifecycle } from "../vaultLifecycle";
import { createApplyOpenedVault } from "../applyOpenedVault";
import { installTopStripControls } from "../topStripControls";
import { setupAppPageTabs } from "../appPageTabs";
import { showToast } from "../../widgets/toast";
import {
  activeTrailStore,
  getBuffer,
  getActivePath,
  getPreviewTabPath,
  getOpenBuffers,
  setBufferState,
  type Buffer,
} from "../state";
import { dom } from "../dom";
import { controllers } from "../controllers";
import { services } from "../services";
import { ctx, need } from "./ctx";
import { Logger } from "../../logger";

export function phase3_mountPanels(): void {
  const formatError = need("formatError");
  const isReadOnlyBuffer = need("isReadOnlyBuffer");
  const vaultIsOpen = need("vaultIsOpen");
  const editor = need("editor");
  const isDirty = need("isDirty");
  const updateStatus = need("updateStatus");
  const setReadOnly = need("setReadOnly");
  const scheduleChunkBoundariesRefresh = need("scheduleChunkBoundariesRefresh");
  const promotePreviewByPath = need("promotePreviewByPath");
  const settings = controllers.settings.get();
  const editorPane = controllers.editorPane.get();
  const patchReview = controllers.patchReview.get();
  const settingsPane = controllers.settingsPane.get();
  const snapshotPreview = controllers.snapshotPreview.get();

  const applyOpenedVault = createApplyOpenedVault();

  // Vault open / bootstrap-from-default flow.
  const vaultLifecycle = mountVaultLifecycle({
    applyOpenedVault: (path) => applyOpenedVault(path),
    formatError,
  });
  controllers.vaultLifecycle.set(vaultLifecycle);

  dom().vaultBar.pickBtn.addEventListener("click", () => void vaultLifecycle.openVault());
  // status: vault-home-screen, vault-home-button
  dom().vaultBar.homeBtn.addEventListener("click", () => { void openAppPageTab("home", {}); });
  dom().vaultBar.settingsBtn.addEventListener("click", () => { void openAppPageTab("settings", {}); });

  installTopStripControls();

  // status: tree-*
  const panelToast: (msg: string, opts?: { actionLabel?: string; onAction?: () => void }) => void = (
    msg,
    opts,
  ) => {
    if (opts?.actionLabel && opts.onAction) {
      showToast(msg, { label: opts.actionLabel, run: opts.onAction });
    } else {
      showToast(msg);
    }
  };
  services.panelToast.set(panelToast);

  const cssEscape = (s: string): string => CSS.escape(s);

  async function reloadActiveBufferAfterStagingAccept(targetPath: string): Promise<void> {
    const buffer = getBuffer();
    if (buffer?.path !== targetPath || buffer.mode.kind !== "file") return;
    if (isDirty()) {
      showToast(`${targetPath} was updated by accept; save to keep your changes.`);
      return;
    }
    try {
      const fresh = await Ipc.openForEdit({ rel: targetPath });
      editor.dispatch({
        changes: { from: 0, to: editor.getDocLength(), insert: fresh.contents },
      });
      buffer.loadedText = editor.getActiveText();
      buffer.token = fresh.token;
      updateStatus();
      scheduleChunkBoundariesRefresh(500);
    } catch (err) {
      Logger.error("ui::app", "staging accept reload failed", { err });
    }
  }
  async function acceptStagingFromPanel(
    proposal: { id: string; target_path: string },
    withOpen: boolean,
  ): Promise<void> {
    const outcome = await Ipc.stagingAccept({ proposalId: proposal.id });
    const target = outcome.target_path;
    if (withOpen) {
      const alreadyOpen = getOpenBuffers().has(target);
      await services.openFile(target, { preview: true });
      if (alreadyOpen) await reloadActiveBufferAfterStagingAccept(target);
    } else {
      await reloadActiveBufferAfterStagingAccept(target);
    }
  }

  const tree = mountTree({
    treeEl: dom().tree.treeEl,
    newNoteBtn: dom().tree.newNoteBtn,
    sidebarActionsBtn: dom().tree.sidebarActionsBtn,
    cssEscape,
    toast: panelToast,
    formatErr: formatError,
    settings,
    openNote: (rel, opts) => services.openFile(rel, opts),
    focusEditor: () => editor.focus(),
    getBuffer: () => getBuffer(),
    isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
    setBufferPath: (newPath) => {
      const buffer = getBuffer();
      if (buffer) { buffer.path = newPath; updateStatus(); }
    },
    isDirty,
    clearOpenBufferIfWithin: (deletedRel) => {
      const openBuffers = getOpenBuffers();
      const drop = [...openBuffers.keys()].filter(
        (p) => p === deletedRel || p.startsWith(deletedRel + "/"),
      );
      for (const p of drop) openBuffers.delete(p);
      const previewTabPath = getPreviewTabPath();
      const buffer = getBuffer();
      const clearPreview = previewTabPath !== null && drop.includes(previewTabPath);
      const clearActive =
        !!buffer &&
        (buffer.path === deletedRel || buffer.path.startsWith(deletedRel + "/"));
      if (clearPreview || clearActive) {
        setBufferState({
          ...(clearPreview ? { previewTabPath: null } : {}),
          ...(clearActive ? { buffer: null, activePath: null } : {}),
        });
      }
      if (clearActive) {
        editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: "" } });
        updateStatus();
      }
      controllers.tabStrip.tryGet()?.render();
    },
    refreshTrashBin: () => need("refreshTrashBin")(),
    renderIndexStatus: () => controllers.indexStatusView.tryGet()?.render(),
    onWaypointAppended: () => {
      void controllers.trailsPanel.tryGet()?.api.refresh();
      void refreshActiveTrailWaypointPaths();
    },
    onOpenProperties: (rel) => controllers.appPageTabs.get().openPropertiesTab(rel),
    onOpenStagingProposal: (proposal) => patchReview.openProposalReview(proposal),
    onAcceptStaging: (proposal) => acceptStagingFromPanel(proposal, /* withOpen */ true),
    onRejectStaging: (proposal) => Ipc.stagingReject({ proposalId: proposal.id }).then(() => undefined),
  });
  controllers.tree.set(tree);

  function refreshTree(): Promise<void> { return tree.api.refresh(); }
  function revealInTree(rel: string): Promise<void> { return tree.api.revealPath(rel); }
  function scheduleTreeRefreshFromWatcher(): void { tree.api.notifyWatcher(); }
  services.refreshTree.set(refreshTree);
  services.scheduleTreeRefreshFromWatcher.set(scheduleTreeRefreshFromWatcher);
  ctx.refreshTree = refreshTree;
  ctx.revealInTree = revealInTree;

  // status: vault-home-screen
  const vaultHome = mountVaultHome({
    toast: panelToast,
    formatErr: formatError,
    settings,
    openNote: (rel, opts) => services.openFile(rel, opts),
    focusEditor: () => editor.focus(),
    editorPaneEl: dom().editor.editorPaneEl,
    vaultHomeEl: dom().vaultHome.rootEl,
    homeBtn: dom().vaultBar.homeBtn,
    vaultPathEl: dom().vaultBar.vaultPathEl,
    titleEl: dom().vaultHome.titleEl,
    statsBodyEl: dom().vaultHome.statsBodyEl,
    modifiedListEl: dom().vaultHome.modifiedListEl,
    accessedListEl: dom().vaultHome.accessedListEl,
    newNoteBtn: dom().vaultHome.newNoteBtn,
    overviewEl: dom().vaultHome.overviewEl,
    detailEl: dom().vaultHome.detailEl,
    detailTitleEl: dom().vaultHome.detailTitleEl,
    detailCountEl: dom().vaultHome.detailCountEl,
    detailListEl: dom().vaultHome.detailListEl,
    detailFiltersEl: dom().vaultHome.detailFiltersEl,
    activitySectionEl: dom().vaultHome.activitySectionEl,
    activityHeaderEl: dom().vaultHome.activityHeaderEl,
    activityListEl: dom().vaultHome.activityListEl,
    getVaultIsOpen: () => vaultIsOpen(),
    onOpenSnapshot: (row) => snapshotPreview.open(row),
    onBeforeShow: () => {
      if (settingsPane.isVisible()) void settingsPane.setVisible(false);
      const qd = controllers.queueDetail.tryGet();
      if (qd?.isVisible()) {
        qd.setVisible(false);
        dom().vaultHome.overviewEl.hidden = false;
      }
    },
    onOpenPage: (kind, payload) => {
      if (kind === "home-detail") {
        void openAppPageTab("home-detail", payload);
      }
    },
    onOpenStagingProposal: (proposal) => patchReview.openProposalReview(proposal),
    onAcceptStaging: (proposal) => acceptStagingFromPanel(proposal, /* withOpen */ false),
    onRejectStaging: (proposal) => Ipc.stagingReject({ proposalId: proposal.id }).then(() => undefined),
  });
  controllers.vaultHome.set(vaultHome);

  // status: task-queue-home-detail-view
  const queueDetail = mountQueueDetail({
    toast: panelToast,
    formatErr: formatError,
    settings,
    openNote: (rel, opts) => services.openFile(rel, opts),
    focusEditor: () => editor.focus(),
    containerEl: dom().vaultHome.queueDetailEl,
  });
  controllers.queueDetail.set(queueDetail);

  // status: navigation-history-stack
  const navSetup = setupNavigation();
  controllers.nav.set(navSetup);
  services.navBack.set(async () => { await navSetup.nav.back(); });
  services.navForward.set(async () => { await navSetup.nav.forward(); });
  services.checkpointNav.set(() => navSetup.checkpointNav());

  // status: mcp-ui-refresh-on-agent-write
  mountAgentChanges({
    editor,
    openBuffers: getOpenBuffers(),
    getBuffer: () => getBuffer(),
    getActivePath: () => getActivePath(),
    getPreviewTabPath: () => getPreviewTabPath(),
    isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
    isDirty,
    setBufferState,
    setReadOnly,
    updateStatus,
    scheduleChunkBoundariesRefresh,
    renderTabStrip: () => controllers.tabStrip.tryGet()?.render(),
    scheduleTreeRefreshFromWatcher,
    getTreeSortOrder: () => tree.api.getSortOrder(),
    notifyChangesAppended: () => vaultHome.api.notifyChangesAppended(),
  });

  // ---------- panel toggles ----------
  const sidebarModeApi = setupSidebarMode();
  controllers.sidebarMode.set(sidebarModeApi);

  installSidebarActions();

  // ---------- discovery panel (search + related) ----------
  const discovery = setupDiscovery();
  controllers.discovery.set(discovery);
  services.focusSearchInput.set(() => discovery.api.focusInput());

  // status: trails-mode-body
  const sidebarTrailsBodyEl = document.getElementById("sidebar-trails-body");
  const trailsPanel = sidebarTrailsBodyEl
    ? mountTrailsPanel({
        toast: panelToast,
        formatErr: formatError,
        settings,
        openNote: (rel, opts) => services.openFile(rel, opts ?? {}).then(() => undefined),
        focusEditor: () => editor.focus(),
        rootEl: sidebarTrailsBodyEl,
        onWaypointRemoved: () => {
          void controllers.trash.tryGet()?.api.refresh();
          vaultHome.api.notifyChangesAppended();
        },
      })
    : null;
  controllers.trailsPanel.set(trailsPanel);

  // status: cluster-editor-sidebar-mode
  const sidebarClustersBodyEl = document.getElementById("sidebar-clusters-body");
  let clusterEditor = null as ReturnType<typeof mountClusterEditor> | null;
  if (sidebarClustersBodyEl) {
    sidebarClustersBodyEl.classList.remove("sidebar-mode-placeholder");
    sidebarClustersBodyEl.removeAttribute("hidden");
    sidebarClustersBodyEl.replaceChildren();
    clusterEditor = mountClusterEditor({
      rootEl: sidebarClustersBodyEl,
      openNote: (rel, opts) => services.openFile(rel, opts ?? {}).then(() => undefined),
      openPane: (treeId, treeName) => clusterWiring.openClusterTab(treeId, treeName),
      openNewTreeReview: () => clusterWiring.openClusterReviewTab({ kind: "new-tree" }),
      openReclusterReview: (treeId, nodeId, nodeName) =>
        clusterWiring.openClusterReviewTab({
          kind: "recluster-subtree",
          treeId,
          nodeId,
          nodeName,
        }),
    });
  }
  controllers.clusterEditor.set(clusterEditor);

  const clusterWiring = setupClusterPaneWiring();
  controllers.clusterWiring.set(clusterWiring);
  services.openClusterTab.set((tid, tname) => clusterWiring.openClusterTab(tid, tname));
  services.openClusterReviewTab.set((p) => clusterWiring.openClusterReviewTab(p));

  // Refresh on external active-trail mutations.
  {
    let lastActiveRel = activeTrailStore.get().rel;
    activeTrailStore.subscribe((s) => {
      if (s.rel === lastActiveRel) return;
      lastActiveRel = s.rel;
      trailsPanel?.api.onActiveTrailMaybeChanged();
    });
  }

  // status: note-properties-tab
  const propertiesPane = mountPropertiesPane({
    containerEl: dom().propertiesPane.paneEl,
  });
  controllers.propertiesPane.set(propertiesPane);

  const appPageTabs = setupAppPageTabs();
  controllers.appPageTabs.set(appPageTabs);
  function openAppPageTab(
    kind: "home" | "home-detail" | "queue" | "settings",
    payload?: Record<string, string>,
  ): Promise<void> {
    return appPageTabs.openAppPageTab(kind, payload);
  }
  services.openAppPageTab.set((kind, payload) => appPageTabs.openAppPageTab(kind, payload));
  services.openPropertiesTab.set((rel) => appPageTabs.openPropertiesTab(rel));
  services.openAgentTab.set((sid) => appPageTabs.openAgentTab(sid));
  services.openSettingsPage.set(() => { void openAppPageTab("settings", {}); });
  ctx.openAppPageTab = openAppPageTab;

  // status: trail-add-to-active-from-editor-verb membership watchers.
  installMembershipWatchers();
  void editorPane; void promotePreviewByPath;
}
