// Phase 4 — trash, tabs, openFile/openFileApi, autosave,
// addToTrailPill, tabStrip, writeNoteReview, taskQueueTile, chat-expand
// → agent-tab listener.
//
// Preconditions: phase 3 (tree, vaultHome, navSetup, settingsPane,
// appPageTabs, patchReview, indexStatusView, sidebarMode controllers
// populated; `ctx.refreshTree`, `ctx.revealInTree`,
// `ctx.openAppPageTab` set).
// Outputs (controllers): trash, tabs, openFileApi, propertiesPane(...),
// autosave, tabStrip, writeNoteReview, taskQueueTile.
// Outputs (services): refreshTrashBin, openFile, closeTab,
// activateTabInner, handleWatcherConflictDirty, openWriteNoteReview.
// Outputs (ctx): openFile, closeTab, activateTabInner, refreshTrashBin.
//
// status: bootstrap-phase-split

import { Ipc } from "../../ipc";
import { mountTrash } from "../../trash";
import { mountTabs } from "../tabs";
import { mountOpenFile } from "../openFile";
import { mountAutosave } from "../autosave";
import { mountTabStrip } from "../../tabStrip";
import { mountAddToTrailPill } from "../../trails/addToTrailPill";
import { setupWriteNoteReview } from "../../patchReview/writeNote";
import { setupTaskQueueTile } from "../taskQueueTile";
import {
  bumpActivationCounter,
  bufferStore,
  getBuffer,
  getActivePath,
  getPreviewTabPath,
  getOpenBuffers,
  getInFlightMutationPaths,
  setBufferState,
  type Buffer,
} from "../state";
import { refreshActiveTrailWaypointPaths } from "../../trails/membership";
import { dom } from "../dom";
import { controllers } from "../controllers";
import { services } from "../services";
import { ctx, need } from "./ctx";

export function phase4_mountTabsAndOpenFile(): void {
  const formatError = need("formatError");
  const editor = need("editor");
  const isDirty = need("isDirty");
  const updateStatus = need("updateStatus");
  const setReadOnly = need("setReadOnly");
  const save = need("save");
  const vaultIsOpen = need("vaultIsOpen");
  const refreshChunkBoundaries = need("refreshChunkBoundaries");
  const scheduleChunkBoundariesRefresh = need("scheduleChunkBoundariesRefresh");
  const promotePreviewByPath = need("promotePreviewByPath");
  const revealInTree = need("revealInTree");
  const openAppPageTab = need("openAppPageTab");

  const settings = controllers.settings.get();
  const vaultHome = controllers.vaultHome.get();
  const settingsPane = controllers.settingsPane.get();
  const patchReview = controllers.patchReview.get();
  const navSetup = controllers.nav.get();
  const appPageTabs = controllers.appPageTabs.get();
  const chatPanel = controllers.chatPanel.get();

  const cssEscape = (s: string): string => CSS.escape(s);
  const getHideFrontmatterEnabled = need("getHideFrontmatterEnabled");
  const getLivePreviewEnabled = need("getLivePreviewEnabled");

  // ---------- trash bin ----------
  const trash = mountTrash({
    binEl: dom().trash.binEl,
    headerEl: dom().trash.headerEl,
    listEl: dom().trash.listEl,
    chevronEl: dom().trash.chevronEl,
    labelEl: dom().trash.labelEl,
    editor: editor!,
    getBuffer: () => getBuffer(),
    setBuffer: (b) => { setBufferState({ buffer: b as Buffer | null }); },
    cssEscape,
    isVaultIsOpen: () => vaultIsOpen(),
    settings,
    isVaultHomeVisible: () => vaultHome.isVisible(),
    setVaultHomeVisible: (on) => vaultHome.setVisible(on),
    refreshTree: () => need("refreshTree")(),
    formatError,
  });
  controllers.trash.set(trash);
  function refreshTrashBin(): Promise<void> { return trash.api.refresh(); }
  services.refreshTrashBin.set(refreshTrashBin);
  ctx.refreshTrashBin = refreshTrashBin;

  // status: navigation-history-stack — snapshot + trash wrappers.
  navSetup.installSnapshotWrappers();
  navSetup.installTrashWrappers();

  // Forward decls: tabs and openFileApi reference each other.
  function activateTabInner(rel: string): void { tabs.activateTab(rel); }
  ctx.activateTabInner = activateTabInner;
  services.activateTabInner.set(activateTabInner);

  async function closeTab(rel: string): Promise<void> {
    // status: cluster-review-tab-discard
    const clusterWiring = controllers.clusterWiring.get();
    if (clusterWiring.clusterReviewTab?.hasUnsavedResult(rel)) {
      const ok = await import("../../widgets/confirm").then((m) =>
        m.confirmDanger(
          "Discard the clustering result? You'll need to re-run to get it back.",
          "Discard",
        ),
      );
      if (!ok) return;
    }
    clusterWiring.clusterReviewTab?.dropTab(rel);
    await tabs.closeTab(rel);
    const openBuffers = getOpenBuffers();
    if (!openBuffers.has(rel)) {
      controllers.autosave.tryGet()?.clearPath(rel);
    }
    controllers.autosave.tryGet()?.scheduleTabStatePush();
  }
  services.closeTab.set(closeTab);
  ctx.closeTab = closeTab;

  const tabs = mountTabs({
    editor: editor!,
    setBufferState,
    getBuffer: () => getBuffer(),
    getActivePath: () => getActivePath(),
    getPreviewTabPath: () => getPreviewTabPath(),
    openBuffers: getOpenBuffers(),
    bumpActivationCounter,
    inFlightMutationPaths: getInFlightMutationPaths(),
    getLivePreviewEnabled,
    getHideFrontmatterEnabled,
    getOpenFileApi: () => openFileApi,
    onShowHome: () => { void openAppPageTab("home", {}); },
    save: () => save(),
    isDirty: () => isDirty(),
    revealInTree: (rel) => revealInTree(rel),
    updateStatus,
    refreshChunkBoundaries,
    renderTabStrip: () => controllers.tabStrip.get().render(),
    pruneNavTab: (rel) => navSetup.nav.pruneTab(rel),
    checkpointNav: () => navSetup.checkpointNav(),
    setReadOnly: (ro) => setReadOnly(ro),
  });
  controllers.tabs.set(tabs);

  const openFileApi = mountOpenFile({
    editor: editor!,
    setBufferState,
    getBuffer: () => getBuffer(),
    getActivePath: () => getActivePath(),
    getPreviewTabPath: () => getPreviewTabPath(),
    openBuffers: getOpenBuffers(),
    bumpActivationCounter,
    inFlightMutationPaths: getInFlightMutationPaths(),
    activateTab: activateTabInner,
    hideVaultHomeIfVisible: () => {
      if (vaultHome.isVisible()) vaultHome.setVisible(false);
    },
    hideSettingsPaneIfVisible: () => {
      if (settingsPane.isVisible()) { void settingsPane.setVisible(false); }
    },
    revealInTree: (rel) => revealInTree(rel),
    updateStatus,
    refreshChunkBoundaries,
    scheduleChunkBoundariesRefresh,
    renderTabStrip: () => controllers.tabStrip.get().render(),
    pruneNavTab: (rel) => navSetup.nav.pruneTab(rel),
    checkpointNav: () => navSetup.checkpointNav(),
    applyTookTheirs: (rel, contents, token) => {
      const buf = getBuffer();
      if (!buf || buf.path !== rel) return;
      editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: contents } });
      buf.loadedText = editor.getActiveText();
      buf.token = token;
      updateStatus();
    },
  });
  controllers.openFileApi.set(openFileApi);
  services.handleWatcherConflictDirty.set((rel) => openFileApi.handleWatcherConflictDirty(rel));

  // Write-note review + patch-review / write-note-review mode-controls.
  const writeNoteReview = setupWriteNoteReview();
  controllers.writeNoteReview.set(writeNoteReview);
  services.openWriteNoteReview.set((proposal) => writeNoteReview.openWriteNoteReview(proposal));

  // status: note-open-routes-to-pending-review
  async function openFile(rel: string, opts?: { preview?: boolean }): Promise<void> {
    // Auto-routing: whole-file proposals → write-note review;
    // edit_note proposals → live file then enter patch-review mode.
    const editProposals = patchReview.pendingEditProposalsForPath(rel);
    if (editProposals.length === 0) {
      const writeProposals = patchReview.pendingWriteProposalsForPath(rel);
      if (writeProposals.length > 0) {
        writeProposals.sort((a, b) => b.created_at_ms - a.created_at_ms);
        await writeNoteReview.openWriteNoteReview(writeProposals[0]);
        return;
      }
      return openFileApi.openFile(rel, opts);
    }
    await openFileApi.openFile(rel, opts);
    const buffer = getBuffer();
    if (buffer && buffer.path === rel && buffer.mode.kind === "file") {
      await patchReview.enterPatchReviewMode(rel);
    }
  }
  services.openFile.set(openFile);
  ctx.openFile = openFile;

  // status: autosave-write-tick, autosave-tab-state-store, autosave-readonly-skipped
  const autosave = mountAutosave({
    editor,
    openBuffers: getOpenBuffers(),
    getBuffer: () => getBuffer(),
    getActivePath: () => getActivePath(),
    getPreviewTabPath: () => getPreviewTabPath(),
    isActiveDirty: () => isDirty(),
  });
  controllers.autosave.set(autosave);

  // status: autosave-tab-state-store — event-driven tab-state push.
  bufferStore.subscribe(() => {
    if (!vaultIsOpen()) return;
    autosave.scheduleTabStatePush();
  });

  // status: trail-add-to-active-from-editor-verb
  const addToTrailPill = mountAddToTrailPill({
    onAppended: () => {
      void controllers.trailsPanel.tryGet()?.api.refresh();
      void refreshActiveTrailWaypointPaths();
    },
  });
  addToTrailPill.setTrailDocPredicate((rel) => controllers.tree.get().api.isTrailDoc(rel));

  // status: editor-tab-strip
  const tabStrip = mountTabStrip({
    hostEl: dom().editor.tabStripEl,
    getTabs: () => tabs.tabSnapshots(),
    getActivePath: () => getActivePath(),
    onActivate: (path) => activateTabInner(path),
    onClose: (path) => void closeTab(path),
    onCloseOthers: (path) => {
      void (async () => {
        const openBuffers = getOpenBuffers();
        const others = [...openBuffers.keys()].filter((p) => p !== path);
        for (const p of others) {
          await closeTab(p);
          if (openBuffers.has(p)) return;
        }
      })();
    },
    onCloseToRight: (path) => {
      void (async () => {
        const openBuffers = getOpenBuffers();
        const order = [...openBuffers.keys()];
        const idx = order.indexOf(path);
        if (idx < 0) return;
        const targets = order.slice(idx + 1);
        for (const p of targets) {
          await closeTab(p);
          if (openBuffers.has(p)) return;
        }
      })();
    },
    onRevealInTree: (path) => { void revealInTree(path); },
    onPromote: (path) => promotePreviewByPath(path),
  });
  controllers.tabStrip.set(tabStrip);

  // status: chat-panel-expand-to-editor
  dom().chat.expandBtnEl.addEventListener("click", () => {
    const sid = chatPanel.getActiveSessionId!();
    if (sid) void appPageTabs.openAgentTab(sid);
  });

  const taskQueueTile = setupTaskQueueTile();
  controllers.taskQueueTile.set(taskQueueTile);

  void Ipc; // referenced by closures above via patchReview/trash modules
}
