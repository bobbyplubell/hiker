// Phase 2 — chat panel, editor pane, patch-review, snapshot preview,
// index-status view, settings pane, plus the central `updateStatus`
// coordinator and the view-toggle setter shims.
//
// Preconditions: phase 1 (DOM cached, `controllers.settings`,
// `ctx.formatError`, `ctx.isReadOnlyBuffer`, `ctx.vaultIsOpen`,
// `ctx.persistSetting`).
// Outputs (controllers): chatPanel, editorPane, patchReview,
// snapshotPreview, indexStatusView, settingsPane.
// Outputs (services): isDirty, scheduleChunkBoundariesRefresh,
// getHideFrontmatterEnabled, applySettingsToUi, updateStatus, save,
// refreshAgentDiffBtn, refreshWriteNotePendingBanner,
// refreshPendingProposalsCache, pendingEditProposalsForPath,
// pendingWriteProposalsForPath, clearWriteNoteTargetExistsCache,
// openProposalReview, chatNewSession.
// Outputs (ctx): editor, save, isDirty, refreshChunkBoundaries,
// scheduleChunkBoundariesRefresh, updateStatus, setReadOnly,
// renderActiveTab, promotePreviewIfActive, promotePreviewByPath,
// getLivePreviewEnabled, getHideFrontmatterEnabled.
//
// status: bootstrap-phase-split

import { toCMKeymap } from "../../editor/keybinds";
import { mountChatPanel } from "../../chat";
import { mountEditorPane } from "../../editorPane";
import { mountSnapshotPreview } from "../../snapshotPreview";
import { mountSettingsPane } from "../../settings";
import { mountIndexStatusView } from "../indexStatusView";
import { setupPatchReviewWiring } from "../../patchReview/wiring";
import { showToast } from "../../widgets/toast";
import { confirm3 } from "../../widgets/confirm";
import {
  getBuffer,
  getActivePath,
  setBufferState,
  addInFlightMutationPath,
  removeInFlightMutationPath,
  viewSettingsStore,
  type Buffer,
} from "../state";
import { dom } from "../dom";
import { controllers } from "../controllers";
import { services } from "../services";
import {
  applySettingsToUi as applySettingsToUiImpl,
  type Settings,
} from "../settingsApply";
import { sortOrderFromSettings } from "../../tree";
import { createRenderActiveTab } from "../renderActiveTab";
import { ctx, need } from "./ctx";

export function phase2_mountEditorCore(): void {
  const formatError = need("formatError");
  const isReadOnlyBuffer = need("isReadOnlyBuffer");
  const vaultIsOpen = need("vaultIsOpen");
  const persistSetting = need("persistSetting");

  // status: chat-panel-pinned-bottom
  const chatPanel = mountChatPanel({
    appEl: dom().editor.appEl,
    regionEl: dom().chat.regionEl,
    handleEl: dom().chat.handleEl,
    collapseBtnEl: dom().chat.collapseBtnEl,
    sessionMenuBtnEl: dom().chat.sessionMenuBtnEl,
    sessionMenuLabelEl: dom().chat.sessionMenuLabelEl,
    panelEl: dom().discovery.panelEl,
    transcriptEl: dom().chat.transcriptEl,
    formEl: dom().chat.formEl,
    inputEl: dom().chat.inputEl,
    sendBtnEl: dom().chat.sendBtnEl,
    onResizePersist: (fraction) => {
      if (!vaultIsOpen()) return;
      void persistSetting("vault", "vault.chat_height", fraction);
    },
    onInputHeightPersist: (heightPx) => {
      if (!vaultIsOpen()) return;
      void persistSetting("vault", "vault.chat_input_height", heightPx);
    },
    onOpenNoteLink: (rel) => {
      // Lazy: `services.openFile` is registered in phase 4. By the time
      // this callback runs (user click), the service is in place.
      void services.openFile(rel, { preview: true });
    },
    onOpenStagingProposal: (proposal) => {
      void controllers.patchReview.get().openProposalReview(proposal);
    },
    toast: (message) => showToast(message, undefined, 6000),
  });
  controllers.chatPanel.set(chatPanel);
  services.chatNewSession.set(() => chatPanel.newSession());

  let livePreviewEnabled = viewSettingsStore.get().livePreviewEnabled;
  let hideFrontmatterEnabled = viewSettingsStore.get().hideFrontmatterEnabled;
  viewSettingsStore.subscribe((s) => {
    livePreviewEnabled = s.livePreviewEnabled;
    hideFrontmatterEnabled = s.hideFrontmatterEnabled;
  });
  services.getHideFrontmatterEnabled.set(() => hideFrontmatterEnabled);
  ctx.getLivePreviewEnabled = () => livePreviewEnabled;
  ctx.getHideFrontmatterEnabled = () => hideFrontmatterEnabled;

  // status: tab-kinds
  // Buffer-kind tab renderer. Owns CM6 view construction, the EditorHost,
  // buffer-scoped toolbar wiring (Save / Diff / View / Mutations), the
  // bottom status bar, the mode-controls slot, and the save/drift/dirty
  // pipeline.
  const editorPane = mountEditorPane({
    handleDriftDetected: (rel, newText, extraMetadata) => {
      return controllers.openFileApi.get().handleDriftDetected(rel, newText, extraMetadata);
    },
    handleSaveError: (err) => controllers.openFileApi.get().handleSaveError(err),
    keymap: toCMKeymap(),
    onAfterSave: (savedPath, ok) => {
      if (ok) {
        controllers.discovery.tryGet()?.api.scheduleRelatedRefresh(savedPath, 500);
        scheduleChunkBoundariesRefresh(500);
        if (savedPath) controllers.autosave.tryGet()?.clearPath(savedPath);
      }
    },
    onStatusPulse: () => {
      if (isDirty()) promotePreviewIfActive();
      controllers.tabStrip.tryGet()?.render();
      renderActiveTab();
    },
    onMutationInFlightChanged: (path, inFlight) => {
      if (inFlight) {
        addInFlightMutationPath(path);
        promotePreviewByPath(path);
      } else {
        removeInFlightMutationPath(path);
      }
      const buffer = getBuffer();
      if (buffer && buffer.mode.kind === "file" && buffer.path === path) {
        setReadOnly(inFlight);
      }
      editorPane.modeControls.render();
    },
  });
  controllers.editorPane.set(editorPane);
  const editor = editorPane.host;
  ctx.editor = editor;

  // View-toggle setter wrappers — thin local shims around the EditorHost.
  function setChunkBoundariesEnabled(on: boolean): void { editor.setChunkBoundariesEnabled(on); }
  function setHideFrontmatterEnabled(on: boolean): void { editor.setHideFrontmatter(on); }
  function setWhitespaceEnabled(on: boolean): void { editor.setWhitespaceEnabled(on); }
  function setLineNumbersVisible(on: boolean): void { editor.setLineNumbersVisible(on); }
  function setWordWrapEnabled(on: boolean): void { editor.setWordWrapEnabled(on); }
  function setLivePreviewEnabled(on: boolean): void { editor.setLivePreviewEnabled(on); }
  function setIntralineDiffEnabled(on: boolean): void { void editor.setIntralineDiffEnabled(on); }

  function isDirty(): boolean { return editor.isDirty(); }
  function refreshChunkBoundaries(): void { editor.refreshChunkBoundaries(); }
  function scheduleChunkBoundariesRefresh(delayMs: number): void {
    editor.scheduleChunkBoundariesRefresh(delayMs);
  }
  services.isDirty.set(isDirty);
  services.scheduleChunkBoundariesRefresh.set(scheduleChunkBoundariesRefresh);

  function setReadOnly(ro: boolean, _mode: "trash" | "snapshot" | "mutation" | null = null): void {
    editor.setReadOnly(ro);
    editorPane.modeControls.render();
  }

  async function save(): Promise<boolean> {
    return editorPane.save();
  }
  services.save.set(save);

  function applySettingsToUi(s: Settings): void {
    applySettingsToUiImpl(s, {
      setLivePreviewEnabled,
      setWordWrapEnabled,
      setLineNumbersVisible,
      setWhitespaceEnabled,
      setChunkBoundariesEnabled,
      setHideFrontmatterEnabled,
      setIntralineDiffEnabled,
      setTreeSortFromSettings: (sortBy) => {
        void controllers.tree.get().api.setSortOrder(sortOrderFromSettings(sortBy), false);
      },
      appEl: dom().editor.appEl,
      trashBinEl: dom().trash.binEl,
      trashChevronEl: dom().trash.chevronEl,
      setChatEnabled: (on) => controllers.chatPanel.get().setEnabled(on),
      setChatHeight: (h) => controllers.chatPanel.get().setHeight(h),
      setChatInputHeight: (px) => controllers.chatPanel.get().setInputHeight(px),
      setSidebarWidth: (px) => controllers.sidebarMode.get().setSidebarWidthVar(px),
      setDiscoveryWidth: (px) => controllers.sidebarMode.get().setDiscoveryWidthVar(px),
      setSidebarMode: (mode) => controllers.sidebarMode.get().setSidebarMode(mode, false),
      setSearchMode: (mode, on) => controllers.discovery.get().api.setMode(mode, on, false),
      setSearchSection: (section, expanded) =>
        controllers.discovery.get().api.setSectionExpanded(section, expanded, false),
      setLexicalOpts: (opts) => controllers.discovery.get().api.setLexicalOpts(opts),
      setSemanticOpts: (opts) => controllers.discovery.get().api.setSemanticOpts(opts),
      syncToggleButtons: () => controllers.sidebarMode.get().syncToggleButtons(),
    });
  }
  services.applySettingsToUi.set(applySettingsToUi);

  function promotePreviewIfActive(): void { controllers.tabs.get().promotePreviewIfActive(); }
  function promotePreviewByPath(rel: string): void { controllers.tabs.get().promotePreviewByPath(rel); }

  /// Coordinator for every paint that used to live in the monolithic
  /// `updateStatus()`. The pure status-bar paint moved to `./editorPane`;
  /// the secondary fan-outs below are *peer concerns*.
  function updateStatus(): void {
    const dirty = isDirty();
    // status: editor-preview-tab-promotion
    if (dirty) promotePreviewIfActive();
    editorPane.repaintStatusBar();
    controllers.indexStatusView.tryGet()?.render();
    editorPane.mutationsMenu.refreshButtonState();
    const buffer = getBuffer();
    // status: editor-diff-vs-disk-toggle — clean buffer + diff active is
    // unreachable user state; force the toggle off before the
    // mode-controls renderer reads it.
    if (
      !dirty
      && buffer?.mode.kind === "file"
      && editorPane.dirtyBufferDiff.isActive()
    ) {
      editorPane.dirtyBufferDiff.forceOff();
    }
    editorPane.modeControls.render();
    controllers.patchReview.tryGet()?.refreshAgentDiffBtn();
    controllers.patchReview.tryGet()?.refreshWriteNotePendingBanner();
    controllers.tabStrip.tryGet()?.render();
    renderActiveTab();
  }
  services.updateStatus.set(updateStatus);

  // Patch-review + write-note pending banner wiring.
  const patchReview = setupPatchReviewWiring();
  controllers.patchReview.set(patchReview);
  services.refreshAgentDiffBtn.set(() => patchReview.refreshAgentDiffBtn());
  services.refreshWriteNotePendingBanner.set(() => patchReview.refreshWriteNotePendingBanner());
  services.refreshPendingProposalsCache.set(() => patchReview.refreshPendingProposalsCache());
  services.pendingEditProposalsForPath.set((p) => patchReview.pendingEditProposalsForPath(p));
  services.pendingWriteProposalsForPath.set((p) => patchReview.pendingWriteProposalsForPath(p));
  services.clearWriteNoteTargetExistsCache.set(() => patchReview.clearWriteNoteTargetExistsCache());
  services.openProposalReview.set((p) => patchReview.openProposalReview(p));

  // status: snapshot-preview-mode
  const snapshotPreview = mountSnapshotPreview({
    editor,
    getBuffer: () => getBuffer(),
    setBuffer: (b) => { setBufferState({ buffer: b as Buffer | null }); },
    getHideFrontmatterEnabled: () => hideFrontmatterEnabled,
    renderModeControls: () => editorPane.modeControls.render(),
    onClose: () => {
      const vh = controllers.vaultHome.get();
      vh.setVisible(true);
      if (vh.api.activeDetailView()?.kind !== "recent-activity") {
        vh.api.showDetail("recent-activity");
      }
    },
    onRestore: (row) => controllers.vaultHome.get().api.doRestoreSnapshot(row),
    isVaultHomeVisible: () => controllers.vaultHome.get().isVisible(),
    setVaultHomeVisible: (on) => controllers.vaultHome.get().setVisible(on),
    formatError,
  });
  controllers.snapshotPreview.set(snapshotPreview);

  // status: status-bar-index-label, status-bar-active-file-index-state
  const indexStatusView = mountIndexStatusView({
    statusIndexEl: dom().statusBar.statusIndexEl,
    getBuffer: () => getBuffer(),
    isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
    isVaultOpen: () => vaultIsOpen(),
    getIndexState: (p) => controllers.tree.get().api.getIndexState(p),
    setIndexState: (p, s) => controllers.tree.get().api.setIndexState(p, s),
    fetchIndexState: (p) => controllers.tree.get().api.fetchIndexState(p),
    cssEscape: (s) => CSS.escape(s),
  });
  controllers.indexStatusView.set(indexStatusView);

  // status: settings-pane-mode
  const settingsPane = mountSettingsPane({
    paneEl: dom().settingsPane.paneEl,
    settingsBtn: dom().vaultBar.settingsBtn,
    vaultPathEl: dom().vaultBar.vaultPathEl,
    guardDirtyBuffer: async () => {
      const buffer = getBuffer();
      if (!buffer || !isDirty()) return true;
      const choice = await confirm3(
        `${buffer.path} has unsaved changes.`,
        "Save & switch",
        "Discard & switch",
        "Cancel",
      );
      if (choice === "cancel") return false;
      if (choice === "a") return await save();
      return true;
    },
    onEnter: () => {
      const vh = controllers.vaultHome.tryGet();
      if (vh?.isVisible()) vh.setVisible(false);
    },
    onSettingApplied: (cfg) => { applySettingsToUi(cfg); },
  });
  controllers.settingsPane.set(settingsPane);

  // renderActiveTab — re-evaluates editor chrome + app-page surface
  // visibility on every status pulse.
  const renderActiveTab = createRenderActiveTab();

  // Tab-cycling service shims (the underlying impl lives in
  // `controllers.tabs`, mounted in phase 4).
  services.cycleTab.set((delta) => controllers.tabs.get().cycleTab(delta));
  services.jumpToTab.set((n) => controllers.tabs.get().jumpToTab(n));
  services.closeActiveTab.set(() => {
    const ap = getActivePath();
    if (ap) void services.closeTab(ap);
  });

  ctx.save = save;
  ctx.isDirty = isDirty;
  ctx.refreshChunkBoundaries = refreshChunkBoundaries;
  ctx.scheduleChunkBoundariesRefresh = scheduleChunkBoundariesRefresh;
  ctx.updateStatus = updateStatus;
  ctx.setReadOnly = setReadOnly;
  ctx.renderActiveTab = renderActiveTab;
  ctx.promotePreviewIfActive = promotePreviewIfActive;
  ctx.promotePreviewByPath = promotePreviewByPath;
}
