import { Ipc, invokeWithLogging } from "./ipc";
import { Logger } from "./logger";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { EditorView } from "@codemirror/view";
import { register, validate, toCMKeymap } from "./editor/keybinds";
import { mountEditorPane, type EditorPaneApi } from "./editorPane";
import { mountTabs, type TabsApi } from "./app/tabs";
import { mountChatPanel } from "./chat";
import { mountSettingsPane, type SettingsPaneApi } from "./settings";
import {
  mountSnapshotPreview,
  type SnapshotPreviewApi,
} from "./snapshotPreview";
import { mountTrash, type TrashController } from "./trash";
import {
  mountTree,
  type TreeController,
  sortOrderFromSettings,
} from "./tree";
import { showToast } from "./widgets/toast";
import { confirm3 } from "./widgets/confirm";
import { mountVaultHome, type VaultHomeController } from "./vaultHome";
import { mountQueueDetail, type QueueDetailController } from "./queueDetail";
import { mountTabStrip, type TabStripApi } from "./tabStrip";
import { mountDiscovery, type DiscoveryController } from "./discovery";
import { mountTrailsPanel, type TrailsController } from "./trails";
import { mountClusterEditor, type ClusterEditorApi } from "./clusterEditor";
import {
  mountClusterReviewTab,
  type ClusterReviewApi,
  type Purpose as ClusterReviewPurpose,
} from "./clusterReviewTab";
import {
  mountClusterEditorPane,
  type ClusterEditorPaneApi,
} from "./clusterEditorPane";
import { mountAddToTrailPill } from "./trails/addToTrailPill";
import {
  installMembershipWatchers,
  refreshActiveTrailWaypointPaths,
} from "./trails/membership";
import {
  mountNavigation,
  installNavigationSwipe,
  type NavApi,
  type NavState,
} from "./navigation";
import { iconButton } from "./modeControls";
import { Icons } from "./icons";
import { hideFrontmatter } from "./editor/hideFrontmatter";
import { applyEditPure } from "./patchReview";
import type { Proposal } from "./ipc";
import { mountPropertiesPane, type PropertiesPaneApi } from "./propertiesPane";
// Stores hoist main.ts's globals into a single source of truth + subscribe
// surface. Local `let` bindings below stay as derived caches kept in sync
// via `*Store.subscribe(...)`; every write rides through `*Store.set` /
// `update` so cross-module observers (next: the chat coupling bug) see
// changes without bespoke deps closures.
import {
  bufferStore,
  tabStore,
  viewSettingsStore,
  inFlightMutationsStore,
  activeTrailStore,
  type Buffer,
  type BufferApi,
  type ActiveBufferSnapshot,
  type TabKind,
} from "./app/state";
import { applySettingsToUi as applySettingsToUiImpl, type Settings } from "./app/settingsApply";
import { mountVaultLifecycle } from "./app/vaultLifecycle";
import { mountIndexStatusBus } from "./app/indexStatusBus";
import { installWindowKeybindings } from "./app/keybindings";
import { mountOpenFile } from "./app/openFile";
import { mountAutosave, type AutosaveApi } from "./app/autosave";
import { mountIndexStatusView, type IndexStatusViewApi } from "./app/indexStatusView";
import { mountAgentChanges } from "./app/agentChanges";
import { captureDomRefs } from "./app/domRefs";
import { createSettingsManager } from "./settings/manager";
import { emit as emitBusEvent } from "./events/bus";

// `DirEntry` re-exported from `./tree`. Editable buffers go through
// `Ipc.openForEdit` / `Ipc.commitBuffer` (per `bug-buffer-hash-tracking-in-ui`);
// read-only preview surfaces use `Ipc.readFile`.
// `TrashEntry` / `TrashListItem` now live in `./trash`.
// `RelatedHit` / `SearchNoteHit` / `SearchResponse` now live in `./discovery`.
// `IndexState` re-exported from `./tree`. `IndexStatus` / `ProgressEvent`
// live in `./app/indexStatusBus`. `Settings` lives in `./app/settingsApply`.

type SettingsScope = "user" | "vault";

// status: settings-write-back, bug-persist-setting-duplicated-per-module
// Single `SettingsManager` for the whole UI. Panels accept this `settings`
// instance in their deps (or import it from `./settings/manager` if they
// don't need a test seam) instead of bespoke `persistSetting` closures.
// Failures are logged but never propagated to the user — a flip that
// worked locally should not show an error toast just because the disk
// write failed; the in-memory change still took effect for the session.
const settings = createSettingsManager({ logTarget: "ui::app" });

// Backwards-compat shim for the handful of host-side call sites below
// (View menu, sidebar/related toggles, chat-panel resize) that read like
// `persistSetting("vault", key, value)`. The panels themselves have moved
// to the typed `settings.setVaultSetting(...)` surface.
async function persistSetting(
  scope: SettingsScope,
  key: string,
  value: unknown,
): Promise<void> {
  if (scope === "user") {
    return settings.setUserSetting(key, value);
  }
  return settings.setVaultSetting(key, value);
}

// Bootstrap orchestration: every UI mount, listener registration, and
// keybind register() call lives inside `bootstrap()` so each mount's
// returned API is held as a local `const` and captured by every later
// closure via lexical scope. The forward-decl `let X: T | null = null`
// scaffolding the file used to carry — needed because the early
// `updateStatus()` paint ran before mounts completed — is gone; the
// early paint moved to the tail of bootstrap, after every mount.
async function bootstrap(): Promise<void> {

// Apply a freshly loaded `Settings` snapshot to every UI surface that
// reflects a setting. Wraps `./app/settingsApply` with the host's per-
// surface mutators (View toggles, tree sort, chat panel, discovery
// modes, sidebar toggles).
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
      void tree.api.setSortOrder(sortOrderFromSettings(sortBy), false);
    },
    appEl,
    trashBinEl,
    trashChevronEl,
    setChatEnabled: (on) => chatPanel.setEnabled(on),
    setChatHeight: (h) => chatPanel.setHeight(h),
    setChatInputHeight: (px) => chatPanel.setInputHeight(px),
    setSidebarWidth: (px) => setSidebarWidthVar(px),
    setDiscoveryWidth: (px) => setDiscoveryWidthVar(px),
    setSidebarMode: (mode) => setSidebarMode(mode, false),
    setSearchMode: (mode, on) => discovery.api.setMode(mode, on, false),
    setSearchSection: (section, expanded) =>
      discovery.api.setSectionExpanded(section, expanded, false),
    setLexicalOpts: (opts) => discovery.api.setLexicalOpts(opts),
    setSemanticOpts: (opts) => discovery.api.setSemanticOpts(opts),
    syncToggleButtons,
  });
}

// One-shot DOM-id capture (see `./app/domRefs`). Refs are grouped by
// domain so the bag stays grep-able as a unit; bootstrap then passes
// slices into each mount.
const dom = captureDomRefs();
const {
  appEl,
  editorEl,
  editorPaneEl,
  saveBtn,
  diffBtn,
  agentDiffBtn,
  modeControlsEl,
  writeNotePendingBannerEl,
  writeNotePendingBannerLabelEl,
  writeNotePendingBannerBtn,
} = dom.editor;
const { statusPathEl, statusCursorEl, statusWordsEl, statusIndexEl } = dom.statusBar;
const { pickBtn, vaultPathEl, homeBtn, settingsBtn } = dom.vaultBar;
const { treeEl, newNoteBtn, sidebarActionsBtn } = dom.tree;
const {
  binEl: trashBinEl,
  headerEl: trashHeaderEl,
  listEl: trashListEl,
  chevronEl: trashChevronEl,
  labelEl: trashLabelEl,
} = dom.trash;
const {
  panelEl: discoveryPanelEl,
  relatedListEl,
  searchInputEl,
  searchClearBtn,
  toggleModeSemanticBtn,
  toggleModeLexicalBtn,
  searchSectionEl,
  searchListEl,
  searchCountEl,
  searchSpinnerEl,
  relatedSectionEl,
  relatedCountEl,
  toggleSidebarBtn,
  toggleRelatedBtn,
} = dom.discovery;
const {
  regionEl: chatRegionEl,
  handleEl: chatHandleEl,
  collapseBtnEl: chatCollapseBtnEl,
  transcriptEl: chatTranscriptEl,
  formEl: chatFormEl,
  inputEl: chatInputEl,
  sendBtnEl: chatSendBtnEl,
  sessionMenuBtnEl: chatSessionMenuBtnEl,
  sessionMenuLabelEl: chatSessionMenuLabelEl,
} = dom.chat;
const { paneEl: settingsPaneEl } = dom.settingsPane;
const { rootEl: vaultHomeEl } = dom.vaultHome;

// Cross-module read surface for the active buffer + selection. Backed by
// `bufferStore` (path / mode) and the EditorHost (text + selection).
// The chat panel (and future consumers) take this `BufferApi` instead of
// bespoke `getActiveNote` / `getActiveSelection` deps closures over main's
// internals. `editor` below is a `const` mounted further down in
// bootstrap; `snapshotActiveBuffer` captures it lexically and only runs
// after construction completes (chat polls on send).
function snapshotActiveBuffer(): ActiveBufferSnapshot | null {
  if (!buffer || isReadOnlyBuffer(buffer)) return null;
  const state = editor.getState();
  const text = state.doc.toString();
  const sel = state.selection.main;
  let selection: { text: string; lineRange: string } | null = null;
  if (sel.from !== sel.to) {
    const selText = state.sliceDoc(sel.from, sel.to);
    if (selText.trim()) {
      const startLine = state.doc.lineAt(sel.from).number;
      const endLine = state.doc.lineAt(sel.to).number;
      selection = {
        text: selText,
        lineRange:
          startLine === endLine ? `L${startLine}` : `L${startLine}-L${endLine}`,
      };
    }
  }
  return { relPath: buffer.path, bufferText: text, selection };
}
const bufferApi: BufferApi = {
  getActive: () => snapshotActiveBuffer(),
  onChanged: (cb) => bufferStore.subscribe(() => cb(snapshotActiveBuffer())),
};

const chatPanel = mountChatPanel({
  appEl,
  regionEl: chatRegionEl,
  handleEl: chatHandleEl,
  collapseBtnEl: chatCollapseBtnEl,
  sessionMenuBtnEl: chatSessionMenuBtnEl,
  sessionMenuLabelEl: chatSessionMenuLabelEl,
  panelEl: discoveryPanelEl,
  transcriptEl: chatTranscriptEl,
  formEl: chatFormEl,
  inputEl: chatInputEl,
  sendBtnEl: chatSendBtnEl,
  onResizePersist: (fraction) => {
    if (!vaultIsOpen()) return;
    void persistSetting("vault", "vault.chat_height", fraction);
  },
  onInputHeightPersist: (heightPx) => {
    if (!vaultIsOpen()) return;
    void persistSetting("vault", "vault.chat_input_height", heightPx);
  },
  // status: chat-panel-note-link-render
  // status: editor-preview-tab-from-open-callsites
  onOpenNoteLink: (rel) => {
    void openFile(rel, { preview: true });
  },
  // Routes the chat tool-card's header-click for staged write/edit
  // proposals through the same staging-preview seam the activity
  // widget uses, so we don't `openFile` against a path that's only
  // staged. Fixes `bug-chat-tool-card-no-link-for-staged-writes`.
  onOpenStagingProposal: (proposal) => {
    void openProposalReview(proposal);
  },
  // status: chat-active-note-context-injection
  // status: chat-input-at-selection
  // Chat reads the active editable buffer + selection through the
  // shared `BufferApi` (see `app/state.ts`) instead of bespoke closures
  // over `buffer` / `view`. Preview-mode buffers (trash / snapshot /
  // mutation) yield `null` from `getActive()` so they don't inject.
  bufferApi,
  toast: (message) => showToast(message, undefined, 6000),
});

// `Buffer` / `BufferMode` / `OpenBufferEntry` now live in `./app/state`
// alongside the stores that own them.

// `buffer`, `activePath`, `previewTabPath`, `openBuffers`, and
// `activationCounter` are owned by `bufferStore` / `tabStore` (see
// `./app/state`). The locals below are *derived caches* kept in sync via
// `subscribe`; reads stay ergonomic (e.g. `buffer?.path`) while writes
// ride through the store's setters so cross-module observers (chat panel
// per `bug-chat-couples-to-main-buffer-globals`, future panels) see
// changes without bespoke deps closures.
let buffer: Buffer | null = bufferStore.get().buffer;
let activePath: string | null = bufferStore.get().activePath;
let previewTabPath: string | null = bufferStore.get().previewTabPath;
bufferStore.subscribe((s) => {
  buffer = s.buffer;
  activePath = s.activePath;
  previewTabPath = s.previewTabPath;
});

/// Setter helper — atomic update of any subset of buffer-state fields.
/// Use this instead of bare `buffer = X` / `activePath = X` so the store
/// is the source of truth and subscribers fire exactly once per logical
/// transition (rather than once per field).
function setBufferState(patch: Partial<{
  buffer: Buffer | null;
  activePath: string | null;
  previewTabPath: string | null;
}>): void {
  bufferStore.update((s) => ({ ...s, ...patch }));
}

// status: editor-tab-strip, multi-buffer-in-memory-only
// Open file-mode buffers, keyed by vault-relative path. Mutated in place
// (Map .set / .delete) — subscribers don't fire on per-key mutation, but
// every relevant call site re-renders the tab strip / nav state directly.
// Future cross-module observers should call `tabStore.set({...s})` after
// mutating the Map to trigger notification (none today).
const openBuffers = tabStore.get().openBuffers;
let activationCounter = tabStore.get().activationCounter;
function bumpActivationCounter(): number {
  activationCounter += 1;
  tabStore.update((s) => ({ ...s, activationCounter }));
  return activationCounter;
}

// status: note-mutation-buffer-ro-while-in-flight
// Local Set is the same instance held by `inFlightMutationsStore` so
// `.has(path)` checks stay zero-IPC. After `add`/`delete` we re-fire
// subscribers via `inFlightMutationsStore.set({ paths })`.
const inFlightMutationPaths = inFlightMutationsStore.get().paths;
function addInFlightMutationPath(path: string): void {
  inFlightMutationPaths.add(path);
  inFlightMutationsStore.set({ paths: inFlightMutationPaths });
}
function removeInFlightMutationPath(path: string): void {
  inFlightMutationPaths.delete(path);
  inFlightMutationsStore.set({ paths: inFlightMutationPaths });
}

// `mutationsMenu`, `modeControls`, `dirtyBufferDiff`, `tabStrip`, and
// `nav` are mounted further down inside `bootstrap()` and held as plain
// `const`s. Closures that touch them (event listeners, store
// subscriptions, store-driven paint helpers) capture them via lexical
// scope; the early `updateStatus()` paint moved to the tail of
// `bootstrap()`, after every mount, so the TDZ-avoidance forward-decls
// the file used to carry are gone.
function checkpointNav(): void {
  nav.checkpoint();
}

/// True for any read-only preview buffer (trash / snapshot) or any
/// non-buffer-kind tab (home, queue, settings, agent, graph, properties).
/// Most code paths share the "no save, no dirty state, switch without
/// prompt" behavior. // status: tab-kinds
function isReadOnlyBuffer(b: Buffer | null): boolean {
  if (!b) return true;
  if (b.kind !== "buffer") return true;
  return b.mode.kind !== "file";
}

// CM6 view + the six per-feature compartments + the path-extension /
// chunk-boundary / save / status-paint plumbing all live in
// `./app/editor`. View-toggle flags are owned by `viewSettingsStore`;
// locals below mirror them so the View menu items + status reads stay
// ergonomic.
// Two mirrors retained because tabs / openFile / mode-controls reads
// happen on hot paths (every tab activation) where a `viewSettingsStore.get()`
// would be slightly noisier and the subscription below already keeps
// these in lockstep with the canonical store. Other view-toggle flags
// (chunk boundaries, whitespace, etc.) read the store directly via the
// `viewMenu` module.
let livePreviewEnabled = viewSettingsStore.get().livePreviewEnabled;
let hideFrontmatterEnabled = viewSettingsStore.get().hideFrontmatterEnabled;

// Vault lifecycle state machine. `vaultIsOpen` used to be a bare
// `let` flag flipped at the top of `applyOpenedVault`; it now narrows
// on `vaultLifecycle.getState().kind`. The predicate "vault is usable"
// matches both `opening` (mid-transition, applyOpenedVault running)
// and `open` (settled) so reads from inside applyOpenedVault keep
// seeing "open" — preserves the identical pre-refactor timing.
function vaultIsOpen(): boolean {
  const kind = vaultLifecycle.getState().kind;
  return kind === "open" || kind === "opening";
}

// Keep the two retained view-toggle mirrors in sync with the canonical
// `viewSettingsStore`. Other flags (chunk boundaries, whitespace, line
// numbers, render-txt-as-markdown, word-wrap) are read directly off
// the store from `viewMenu` / editor host call sites.
viewSettingsStore.subscribe((s) => {
  livePreviewEnabled = s.livePreviewEnabled;
  hideFrontmatterEnabled = s.hideFrontmatterEnabled;
});

// View-toggle setter wrappers — thin local shims around the EditorHost
// surface so call sites (the View menu items in `buildViewMenuItems`,
// plus `applySettingsToUi`) don't have to thread the host through.
function setChunkBoundariesEnabled(on: boolean): void {
  editor.setChunkBoundariesEnabled(on);
}
function setHideFrontmatterEnabled(on: boolean): void {
  editor.setHideFrontmatter(on);
}
function setWhitespaceEnabled(on: boolean): void {
  editor.setWhitespaceEnabled(on);
}
function setLineNumbersVisible(on: boolean): void {
  editor.setLineNumbersVisible(on);
}
function setWordWrapEnabled(on: boolean): void {
  editor.setWordWrapEnabled(on);
}
function setLivePreviewEnabled(on: boolean): void {
  editor.setLivePreviewEnabled(on);
}
function setIntralineDiffEnabled(on: boolean): void {
  void editor.setIntralineDiffEnabled(on);
}

function isDirty(): boolean {
  return editor.isDirty();
}
function refreshChunkBoundaries(): void {
  editor.refreshChunkBoundaries();
}
function scheduleChunkBoundariesRefresh(delayMs: number): void {
  editor.scheduleChunkBoundariesRefresh(delayMs);
}

// status: tab-kinds
/// Returns the `kind` discriminator of the active buffer (or the
/// pending app-page tab key prefix), or `null` when no tab is active.
function activeKind(): TabKind | null {
  if (!buffer) return null;
  return buffer.kind;
}

// status: tab-kinds
/// Stable synthetic key for a app-page tab. Buffer tabs use the
/// vault-relative path as their key; app-page tabs use a prefixed
/// sentinel so they live alongside buffer entries in `openBuffers`
/// without colliding with real paths.
function appPageTabKey(kind: string, view?: string): string {
  return view ? `__hiker:${kind}:${view}` : `__hiker:${kind}`;
}

// status: tab-kinds
/// Re-evaluates editor chrome + app-page surface visibility based on
/// the active tab's kind discriminator. Called on every status pulse.
function renderActiveTab(): void {
  const kind = activeKind();
  const isBuffer = kind === "buffer";
  dom.editor.editorEl.hidden = !isBuffer;
  // Buffer-only toolbar buttons
  saveBtn.hidden = !isBuffer;
  diffBtn.hidden = !isBuffer;
  dom.editor.viewMenuBtn.hidden = !isBuffer;
  dom.editor.mutationsMenuBtn.hidden = !isBuffer;
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
  dom.vaultHome.rootEl.hidden = !(isHome || isHomeDetail || isQueue);
  dom.settingsPane.paneEl.hidden = !isSettings;
  dom.propertiesPane.paneEl.hidden = !isProperties;
  if (clusterPaneEl) clusterPaneEl.hidden = !isClusterPane;
  if (!isClusterPane) {
    clusterEditorPane?.hide();
  }
  if (clusterReviewPaneEl) clusterReviewPaneEl.hidden = !isClusterReview;
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
  dom.vaultHome.overviewEl.hidden = !isHome;
  dom.vaultHome.detailEl.hidden = !isHomeDetail;
  dom.vaultHome.queueDetailEl.hidden = !isQueue;
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
  dom.vaultBar.homeBtn.classList.toggle("active", isHome || isHomeDetail);
  dom.vaultBar.settingsBtn.classList.toggle("active", isSettings);
  // Collapse docked chat while any agent tab is open
  const hasAgentTab = [...openBuffers.values()].some(e => e.buffer.kind === "agent");
  appEl.classList.toggle("agent-tab-open", hasAgentTab);
}

/// Coordinator for every paint that used to live in the monolithic
/// `updateStatus()`. The pure status-bar paint moved to
/// `./editorPane`; the secondary fan-outs below are *peer concerns*
/// that just happened to share the same trigger. Now also re-evaluates
/// `renderActiveTab()` on every pulse.
function updateStatus(): void {
  const dirty = isDirty();
  // status: editor-preview-tab-promotion
  if (dirty) promotePreviewIfActive();
  editorPane.repaintStatusBar();
  // Center status label mirrors the active buffer's index state.
  indexStatusView.render();
  // Wand button enable depends on active buffer's path / mode /
  // extension / dirtiness — re-evaluate on every status pulse.
  editorPane.mutationsMenu.refreshButtonState();
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
  // status: patch-review-agent-diff-toggle
  refreshAgentDiffBtn();
  // status: write-note-pending-banner
  refreshWriteNotePendingBanner();
  tabStrip.render();
  // status: tab-kinds — re-evaluate editor chrome visibility after
  // every kind transition.
  renderActiveTab();
}

register({
  id: "editor.save",
  keys: "Mod-s",
  label: "Save current buffer",
  run: () => {
    const savedPath = buffer?.path ?? null;
    void save().then((ok) => {
      // status: autosave-write-tick — clear the sidecar on success so a
      // crash after a clean save doesn't surface a false-positive
      // recovery on next open.
      if (ok && savedPath) autosave.clearPath(savedPath);
    });
    return true;
  },
});
// status: search-keybind-ctrl-space
// Inside the editor, this binding wins over CM6's default `Ctrl-Space →
// startCompletion`. Outside the editor (tree, status bar, anywhere with
// focus), the document-level keydown handler installed in
// `installSearchFocusKeybind()` covers the global case. The keybind
// registry doesn't currently support a `scope` field — see editor.md
// "Bindings only fire when the editor has DOM focus" — so the global
// half lives outside the registry until that scope refactor lands.
register({
  id: "search.focusInput",
  keys: "Ctrl-Space",
  label: "Focus search input",
  run: () => {
    discovery.api.focusInput();
    return true;
  },
});
// status: chat-session-new-button
// Reserved keybind for the "New chat session" affordance. The shortcut
// itself is bound here so power users can fire it without touching the
// button; the button still ships the same call.
register({
  id: "chat.new-session",
  keys: "Mod-Shift-n",
  label: "Start a new chat session",
  run: () => {
    void chatPanel.newSession();
    return true;
  },
});
// status: editor-tab-keybinds
// Tab close / cycle / jump. Registered in the CM6 keymap so the editor
// case works; a window-level keydown listener (further down) covers the
// case where focus is outside CM6 (tree, sidebar, status bar). Two
// sinks for one set of bindings is a wart of `keybind-registry`'s
// editor-only scope; the spec acknowledges it under "When a future
// binding needs to fire outside the editor".
register({
  id: "tab.close",
  keys: "Mod-w",
  label: "Close active tab",
  run: () => {
    if (activePath) void closeTab(activePath);
    return true;
  },
});
register({
  id: "tab.next",
  keys: "Ctrl-Tab",
  label: "Next tab",
  run: () => {
    cycleTab(+1);
    return true;
  },
});
register({
  id: "tab.previous",
  keys: "Ctrl-Shift-Tab",
  label: "Previous tab",
  run: () => {
    cycleTab(-1);
    return true;
  },
});
for (let i = 1; i <= 9; i++) {
  const idx = i;
  register({
    id: `tab.jump-${idx}`,
    keys: `Mod-${idx}`,
    label: `Jump to tab ${idx === 9 ? "(last)" : idx}`,
    run: () => {
      jumpToTab(idx);
      return true;
    },
  });
}
// status: navigation-keybind
// Browser-conventional Cmd/Ctrl-[ for back, Cmd/Ctrl-] for forward.
// Registered in CM6 so they fire when the editor has focus; a window-
// level keydown handler further down covers tree / sidebar / status-bar
// focus and adds the Linux/Windows-conventional Alt-Left / Alt-Right.
register({
  id: "navigation.back",
  keys: "Mod-[",
  label: "Navigate back",
  run: () => {
    void nav.back();
    return true;
  },
});
register({
  id: "navigation.forward",
  keys: "Mod-]",
  label: "Navigate forward",
  run: () => {
    void nav.forward();
    return true;
  },
});
validate();

// status: tab-kinds
// Buffer-kind tab renderer. Owns CM6 view construction, the EditorHost,
// buffer-scoped toolbar wiring (Save / Diff / View / Mutations), the
// bottom status bar, the mode-controls slot, and the save/drift/dirty
// pipeline. This is the peer of the other kind modules (vaultHome,
// queueDetail, settings, chat, properties).
const cssEscape = (s: string): string => CSS.escape(s);
const editorPane: EditorPaneApi = mountEditorPane({
  parentEl: editorEl,
  saveBtn,
  diffBtn,
  modeControlsEl,
  viewMenuBtn: dom.editor.viewMenuBtn,
  mutationsMenuBtn: dom.editor.mutationsMenuBtn,
  statusPathEl,
  statusCursorEl,
  statusWordsEl,
  treeEl,
  getBuffer: () => buffer,
  setBufferState,
  handleDriftDetected: (rel, newText, extraMetadata) => {
    return openFileApi.handleDriftDetected(rel, newText, extraMetadata);
  },
  handleSaveError: (err) => openFileApi.handleSaveError(err),
  isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
  keymap: toCMKeymap(),
  onAfterSave: (savedPath, ok) => {
    if (ok) {
      discovery.api.scheduleRelatedRefresh(savedPath, 500);
      scheduleChunkBoundariesRefresh(500);
      if (savedPath) autosave.clearPath(savedPath);
    }
  },
  onStatusPulse: () => {
    // Host fan-out after every internal status-bar paint: preview-
    // promotion, tab-strip render, and renderActiveTab.
    if (isDirty()) promotePreviewIfActive();
    tabStrip.render();
    renderActiveTab();
  },
  inFlightMutationPaths,
  settings,
  syncToggleButtons,
  cssEscape,
  formatError,
  onMutationInFlightChanged: (path, inFlight) => {
    if (inFlight) {
      addInFlightMutationPath(path);
      promotePreviewByPath(path);
    } else {
      removeInFlightMutationPath(path);
    }
    if (buffer && buffer.mode.kind === "file" && buffer.path === path) {
      setReadOnly(inFlight);
    }
    editorPane.modeControls.render();
  },
  // status: status-bar-path-reveal
  onRevealInFileManager: (rel) => Ipc.revealInFileManager({ rel }),
  // status: status-bar-version-dropdown-selection
  onSelectCurrentVersion: (path) => {
    // Re-open the live file. The normal open path handles exiting
    // snapshot / staging preview modes; opens non-preview, sticky.
    void openFile(path, { preview: false });
  },
  onSelectSnapshotVersion: (row) => snapshotPreview.open(row),
  onSelectStagingVersion: (proposal) => openProposalReview(proposal),
  // status: patch-review-per-hunk-accept
  onPatchReviewAcceptHunk: (p) => acceptPatchReviewHunk(p),
  onPatchReviewRejectHunk: (p) => rejectPatchReviewHunk(p),
  onExitPatchReview: () => exitPatchReviewMode(),
});

// Backwards-compat shim so existing code referencing `editor.*` (CM6
// dispatch, getState, etc.) keeps working. The `editorPane.host` is the
// canonical EditorHost; this re-export avoids touching hundreds of
// call sites.
const editor = editorPane.host;

async function save(): Promise<boolean> {
  return editorPane.save();
}

// status: patch-review-mode
// status: patch-review-agent-diff-toggle
// status: patch-review-readonly-while-active
// Enter / exit patch-review mode against the active buffer's file. The
// buffer mode flips to `"patch-review"`, CM6 goes read-only, and the
// active proposal snapshot is pushed into the hunk renderer.
async function enterPatchReviewMode(rel: string): Promise<void> {
  const buf = buffer;
  if (!buf || buf.kind !== "buffer") return;
  if (buf.path !== rel) return;
  await refreshPendingProposalsCache();
  const proposals = pendingEditProposalsForPath(rel);
  if (proposals.length === 0) return;
  // Force the user-diff toggle off before entering patch-review per
  // `patch-review-toggles-mutually-exclusive`.
  if (editorPane.dirtyBufferDiff.isActive()) {
    editorPane.dirtyBufferDiff.forceOff();
  }
  buf.mode = { kind: "patch-review", targetPath: rel };
  editorPane.patchReview.setProposals(proposals);
  editor.setReadOnly(true);
  refreshAgentDiffBtn();
  updateStatus();
}

function exitPatchReviewMode(): void {
  const buf = buffer;
  if (!buf || buf.mode.kind !== "patch-review") return;
  buf.mode = { kind: "file" };
  editorPane.patchReview.setProposals([]);
  editor.setReadOnly(false);
  refreshAgentDiffBtn();
  updateStatus();
}

// Repaint the agent-diff toolbar button's enable / pressed state. Greys
// when the active buffer has no pending `edit_note` proposals, pressed
// when patch-review mode is active.
function refreshAgentDiffBtn(): void {
  const buf = buffer;
  const inReview = buf?.mode.kind === "patch-review";
  let hasPending = false;
  if (buf && buf.kind === "buffer") {
    hasPending = pendingEditProposalsForPath(buf.path).length > 0;
  }
  agentDiffBtn.disabled = !inReview && !hasPending;
  agentDiffBtn.classList.toggle("active", inReview);
  // `bug-patch-review-gutter-not-restored-on-exit`: the gutter's
  // visibility is a derived state of the toggle, not an imperative
  // side-effect of enter/exit. Reapplying on every status pulse keeps
  // the class in sync even when CM6 internal re-renders or focus
  // events would otherwise lose an imperatively-added class.
  editor.dom.classList.toggle("hiker-patch-review-active", inReview);
  if (inReview) {
    agentDiffBtn.title = "Exit agent-edit review";
  } else if (!hasPending) {
    agentDiffBtn.title = "No pending agent edits";
  } else {
    agentDiffBtn.title = "Review agent edits";
  }
}

agentDiffBtn.addEventListener("click", () => {
  if (agentDiffBtn.disabled) return;
  const buf = buffer;
  if (!buf) return;
  if (buf.mode.kind === "patch-review") {
    exitPatchReviewMode();
  } else if (buf.mode.kind === "file") {
    void enterPatchReviewMode(buf.path);
  }
});

// status: write-note-pending-banner
// Cache of path → "does this exist on disk" probes so the banner's label
// can distinguish "Pending rewrite for this note" from "Pending new-note
// proposal" without re-issuing readFile every paint. Populated lazily on
// first sighting of a path; invalidated on staging-changed (cheap to
// re-probe; rare event).
const writeNoteTargetExistsCache = new Map<string, boolean>();
let writeNoteBannerLatestProposalId: string | null = null;

function refreshWriteNotePendingBanner(): void {
  const buf = buffer;
  // Visible iff the active buffer is in plain editing mode AND has at
  // least one pending write-shaped proposal targeting its path.
  if (!buf || buf.kind !== "buffer" || buf.mode.kind !== "file") {
    writeNotePendingBannerEl.hidden = true;
    writeNoteBannerLatestProposalId = null;
    return;
  }
  const writeProposals = pendingWriteProposalsForPath(buf.path);
  if (writeProposals.length === 0) {
    writeNotePendingBannerEl.hidden = true;
    writeNoteBannerLatestProposalId = null;
    return;
  }
  // Newest proposal wins (matches `note-open-routes-to-pending-review`).
  const sorted = writeProposals
    .slice()
    .sort((a, b) => b.created_at_ms - a.created_at_ms);
  const latest = sorted[0];
  writeNoteBannerLatestProposalId = latest.id;
  const path = buf.path;
  const cached = writeNoteTargetExistsCache.get(path);
  // Origin suffix mirrors `write-note-review-mode-label` so the user
  // sees the same provenance framing in both surfaces. Unknown surfaces
  // render no suffix rather than leaking an internal token.
  let origin = "";
  if (latest.surface === "chat") origin = " · chat";
  else if (latest.surface === "trails") origin = " · trail";
  else if (latest.surface === "batch-mutation") origin = " · batch";
  const paint = (exists: boolean): void => {
    const base = exists
      ? "Pending rewrite for this note"
      : "Pending new-note proposal";
    writeNotePendingBannerLabelEl.textContent = base + origin;
  };
  if (cached !== undefined) {
    paint(cached);
  } else {
    // Default to the existing-file framing while the probe resolves —
    // it's the common case. Update once readFile returns.
    paint(true);
    void Ipc.readFile({ rel: path })
      .then(() => writeNoteTargetExistsCache.set(path, true))
      .catch(() => writeNoteTargetExistsCache.set(path, false))
      .finally(() => {
        // Guard: buffer may have switched away during the probe.
        const cur = buffer;
        if (
          cur
          && cur.kind === "buffer"
          && cur.mode.kind === "file"
          && cur.path === path
        ) {
          const known = writeNoteTargetExistsCache.get(path);
          if (known !== undefined) paint(known);
        }
      });
  }
  writeNotePendingBannerEl.hidden = false;
}

writeNotePendingBannerBtn.addEventListener("click", () => {
  const id = writeNoteBannerLatestProposalId;
  if (!id) return;
  const proposal = pendingProposalsCache.find((p) => p.id === id);
  if (!proposal) return;
  void openWriteNoteReview(proposal);
});

// status: patch-review-mode
// Local cache of pending staging proposals — pulled at vault open + on
// every `hiker:staging-changed` event. The cache backs the patch-review
// hunk decoration set, the agent-diff toggle's grey-when-empty state,
// and the openFile auto-routing rule.
let pendingProposalsCache: Proposal[] = [];
function pendingEditProposalsForPath(path: string): Proposal[] {
  return pendingProposalsCache.filter(
    (p) => p.target_path === path && p.action === "edit_note" && p.edit,
  );
}
function pendingWriteProposalsForPath(path: string): Proposal[] {
  return pendingProposalsCache.filter(
    (p) => p.target_path === path && p.action !== "edit_note",
  );
}
async function refreshPendingProposalsCache(): Promise<void> {
  try {
    pendingProposalsCache = await Ipc.stagingList();
  } catch {
    pendingProposalsCache = [];
  }
}

// status: patch-review-per-hunk-accept
// status: patch-review-dirty-buffer-transactional-accept
// Per-hunk accept handler. Routes:
// 1. If the active buffer is the same path AND dirty, pre-check that the
//    edit can apply to the in-memory text — refuse with a toast when the
//    user's edits clobber the anchor.
// 2. Call `staging.accept(id)`. Rust re-anchors against current disk and
//    writes via `write_file_checked`. Anchor / drift failures surface as
//    errors; we surface them as a toast.
// 3. On success, dispatch the new buffer text (computed by applying the
//    same edit to the in-memory buffer) + refresh loadedText / token via
//    a fresh `open_for_edit` so subsequent commits ride a valid token.
async function acceptPatchReviewHunk(proposal: Proposal): Promise<void> {
  const edit = proposal.edit;
  if (!edit) return;
  const buf = buffer;
  const isActiveTarget =
    buf !== null
    && buf.path === proposal.target_path
    && (buf.mode.kind === "file" || buf.mode.kind === "patch-review");
  let bufferAppliedText: string | null = null;
  if (isActiveTarget && buf && buf.mode.kind === "file") {
    const currentBuffer = editor.getActiveText();
    bufferAppliedText = applyEditPure(currentBuffer, edit);
    if (bufferAppliedText === null) {
      showToast(
        "Your edits conflict with this proposal — save or revert first to accept.",
      );
      return;
    }
  }
  try {
    await Ipc.stagingAccept({ proposalId: proposal.id });
  } catch (err) {
    showToast("Accept failed: " + formatError(err));
    return;
  }
  // Re-sync local cache before re-rendering decorations.
  await refreshPendingProposalsCache();
  if (isActiveTarget && buf) {
    try {
      const fresh = await Ipc.openForEdit({ rel: proposal.target_path });
      // For patch-review buffers (read-only by mode), just reset
      // loadedText + token. For file buffers with user edits, dispatch
      // `bufferAppliedText` (the edit applied to the user's text) so the
      // user's edits + agent's edit both land.
      const dispatchText =
        buf.mode.kind === "file" && bufferAppliedText !== null
          ? bufferAppliedText
          : fresh.contents;
      editor.dispatch({
        changes: { from: 0, to: editor.getDocLength(), insert: dispatchText },
      });
      buf.loadedText = fresh.contents;
      buf.token = fresh.token;
    } catch (err) {
      Logger.error("ui::app", "patch-review accept reload failed", { err });
    }
  }
  // Re-paint decorations against the new doc/proposals snapshot.
  editorPane.patchReview.setProposals(
    pendingEditProposalsForPath(proposal.target_path),
  );
  updateStatus();
  if (buf?.mode.kind === "patch-review") {
    // If the user accepted the last applyable hunk, automatically exit
    // patch-review mode back to plain editing.
    const remaining = pendingEditProposalsForPath(proposal.target_path);
    if (remaining.length === 0) exitPatchReviewMode();
  }
}

async function rejectPatchReviewHunk(proposal: Proposal): Promise<void> {
  try {
    await Ipc.stagingReject({ proposalId: proposal.id });
  } catch (err) {
    showToast("Reject failed: " + formatError(err));
    return;
  }
  await refreshPendingProposalsCache();
  editorPane.patchReview.setProposals(
    pendingEditProposalsForPath(proposal.target_path),
  );
  updateStatus();
  if (buffer?.mode.kind === "patch-review") {
    const remaining = pendingEditProposalsForPath(proposal.target_path);
    if (remaining.length === 0) exitPatchReviewMode();
  }
}

// status: snapshot-preview-mode
// Mount the snapshot-preview module. Hosted state — `buffer`, the CM6 view,
// the dirty/save flow, render-mode-controls — flow in via the deps; the
// module owns the diff-toggle in-flight guard and orchestrates the open /
// close / toggle / restore lifecycle.
const snapshotPreview: SnapshotPreviewApi = mountSnapshotPreview({
  editor,
  getBuffer: () => buffer,
  setBuffer: (b) => {
    setBufferState({ buffer: b as Buffer | null });
  },
  getHideFrontmatterEnabled: () => hideFrontmatterEnabled,
  renderModeControls: () => editorPane.modeControls.render(),
  // Returning to the activity detail view if it's where the user came from;
  // otherwise fall back to the home overview.
  onClose: () => {
    vaultHome.setVisible(true);
    if (vaultHome.api.activeDetailView()?.kind !== "recent-activity") {
      vaultHome.api.showDetail("recent-activity");
    }
  },
  onRestore: (row) => vaultHome.api.doRestoreSnapshot(row),
  isVaultHomeVisible: () => vaultHome.isVisible(),
  setVaultHomeVisible: (on) => vaultHome.setVisible(on),
  formatError,
});

// Whole-file staging proposals (`write_note` / `set_frontmatter` /
// `apply_tag`) open in the spec-conformant `write-note-review` mode
// below (`openWriteNoteReview`). The older `staging` buffer mode was
// removed; this dispatcher routes the three legacy entry points
// (status-bar version dropdown, tree click, vault-home activity click)
// to the right surface based on the proposal's action.
async function openProposalReview(proposal: { id: string; target_path: string }): Promise<void> {
  const full = pendingProposalsCache.find((p) => p.id === proposal.id);
  if (!full) {
    // Proposal vanished (accepted/rejected concurrently). Surface the
    // live file instead.
    void openFile(proposal.target_path, { preview: true });
    return;
  }
  // Route through the `openFile` wrapper so the
  // `note-open-routes-to-pending-review` auto-routing rule consults
  // `pendingEditProposalsForPath` / `pendingWriteProposalsForPath` and
  // lands the user in patch-review (for `edit_note`) or write-note
  // review (for whole-file proposals). Keeps every staging-row entry
  // point on one path so the contract holds regardless of which
  // surface triggered the open. Fixes
  // `bug-activity-pending-row-skips-patch-review-mode`.
  await openFile(proposal.target_path, { preview: true });
}

// `save` is owned by the EditorHost; main.ts calls `editor.save()`. The
// drift / save-error handlers live in `./app/openFile`.

// Tab coordination glue (activate / close / cycle / jump / snapshot
// + preview-promotion shims) lives in `./app/tabs`. Mounted further
// down once every dep (editor host, vaultHome / settingsPane visibility,
// `save` / `isDirty`) is in scope. The shim functions below capture
// `tabs` / `openFileApi` lexically; they're invoked lazily (event
// listeners, deps callbacks) so the forward references resolve fine
// once the consts initialize.
// status: note-open-routes-to-pending-review
async function openFile(
  rel: string,
  opts?: { preview?: boolean },
): Promise<void> {
  // Auto-routing per `note-open-routes-to-pending-review`:
  // - whole-file proposals (`write_note` / `set_frontmatter` /
  //   `apply_tag`) land in write-note review mode (the buffer-side has
  //   no edit shape to compose against);
  // - paths with pending `edit_note` proposals open as the live file
  //   and enter patch-review mode so the user sees the agent's hunks
  //   inline instead of plain editing.
  // The agent-diff toolbar toggle (`patch-review-agent-diff-toggle`)
  // is still the explicit exit/re-enter affordance once the user
  // chooses to leave review.
  const editProposals = pendingEditProposalsForPath(rel);
  if (editProposals.length === 0) {
    const writeProposals = pendingWriteProposalsForPath(rel);
    if (writeProposals.length > 0) {
      writeProposals.sort((a, b) => b.created_at_ms - a.created_at_ms);
      await openWriteNoteReview(writeProposals[0]);
      return;
    }
    return openFileApi.openFile(rel, opts);
  }
  await openFileApi.openFile(rel, opts);
  if (buffer && buffer.path === rel && buffer.mode.kind === "file") {
    await enterPatchReviewMode(rel);
  }
}
function activateTabInner(rel: string): void {
  tabs.activateTab(rel);
}
async function closeTab(rel: string): Promise<void> {
  // status: cluster-review-tab-discard
  // Close-guard for a cluster-review tab whose in-memory result hasn't
  // been Confirmed yet. The review module owns the confirm modal so the
  // copy stays consistent with the Discard button's prompt; once
  // resolved (confirmed or not), the entry is dropped via `dropTab` and
  // we fall through to the normal close.
  if (clusterReviewTab?.hasUnsavedResult(rel)) {
    const ok = await import("./widgets/confirm").then((m) =>
      m.confirmDanger(
        "Discard the clustering result? You'll need to re-run to get it back.",
        "Discard",
      ),
    );
    if (!ok) return;
  }
  clusterReviewTab?.dropTab(rel);
  await tabs.closeTab(rel);
  // status: autosave-write-tick — closing the tab means the autosave
  // entry for that buffer is no longer relevant (whether the user
  // saved, discarded, or the tab was clean to begin with).
  if (!openBuffers.has(rel)) {
    autosave.clearPath(rel);
  }
  // status: autosave-tab-state-store — tab-shape change.
  autosave.scheduleTabStatePush();
}
function cycleTab(delta: 1 | -1): void {
  tabs.cycleTab(delta);
}
function jumpToTab(n: number): void {
  tabs.jumpToTab(n);
}
function tabSnapshots(): ReturnType<TabsApi["tabSnapshots"]> {
  return tabs.tabSnapshots();
}
function promotePreviewIfActive(): void {
  tabs.promotePreviewIfActive();
}
function promotePreviewByPath(rel: string): void {
  tabs.promotePreviewByPath(rel);
}

// status: tree-* (see ./tree)
// Sidebar tree owns its own state (expanded folders, sort order, debounce,
// index-state cache) inside the module; host wires DOM ids and editor-coupled
// callbacks via deps. The wrapper functions below preserve the old call-site
// shape (`refreshTree`, `revealInTree`, `scheduleTreeRefreshFromWatcher`).
// Shared toast adapter that bridges the `PanelDeps.toast` shape
// (action expressed as `{ actionLabel, onAction }`) to the local
// `showToast` widget (action expressed as `{ label, run }`). Every
// panel's `PanelDeps.toast` slot routes through this so the widget
// API stays internal.
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

const tree: TreeController = mountTree({
  treeEl,
  newNoteBtn,
  sidebarActionsBtn,
  cssEscape,
  // PanelDeps cross-panel uniforms (toast / formatErr / settings /
  // openNote / focusEditor). The tree uses `formatErr` (alert copy on
  // ipc rejection) and `openNote` (row click → open file); the others
  // are wired so future tree affordances don't have to thread new deps.
  toast: panelToast,
  formatErr: formatError,
  settings,
  openNote: (rel, opts) => openFile(rel, opts),
  focusEditor: () => editor.focus(),
  getBuffer: () => buffer,
  isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
  setBufferPath: (newPath) => {
    if (buffer) {
      buffer.path = newPath;
      updateStatus();
    }
  },
  isDirty,
  clearOpenBufferIfWithin: (deletedRel) => {
    // status: editor-tab-strip — drop any tabs whose paths fall under
    // the deleted prefix so they don't linger as broken references.
    const drop = [...openBuffers.keys()].filter(
      (p) => p === deletedRel || p.startsWith(deletedRel + "/"),
    );
    for (const p of drop) openBuffers.delete(p);
    // status: editor-preview-tab — drop preview slot pointer if dropped.
    const clearPreview =
      previewTabPath !== null && drop.includes(previewTabPath);
    const clearActive =
      !!buffer &&
      (buffer.path === deletedRel ||
        buffer.path.startsWith(deletedRel + "/"));
    if (clearPreview || clearActive) {
      setBufferState({
        ...(clearPreview ? { previewTabPath: null } : {}),
        ...(clearActive ? { buffer: null, activePath: null } : {}),
      });
    }
    if (clearActive) {
      editor.dispatch({
        changes: { from: 0, to: editor.getDocLength(), insert: "" },
      });
      updateStatus();
    }
    tabStrip.render();
  },
  refreshTrashBin,
  renderIndexStatus: () => indexStatusView.render(),
  // status: trail-add-to-active-from-tree-verb — explicit panel +
  // membership refresh after the verb's `trailAppendWaypoint`
  // succeeds. Watcher is suppressed for the trail-doc + waypoint-note
  // paths during the append, so the `hiker:file-changed` refresh
  // path can't fire here. See
  // `bug-add-to-trail-verbs-dont-refresh-panel`.
  onWaypointAppended: () => {
    void trailsPanel?.api.refresh();
    void refreshActiveTrailWaypointPaths();
  },
  // status: tree-context-properties
  onOpenProperties: (rel) => openPropertiesTab(rel),
  // status: staging-accept-reject-from-tree
  onOpenStagingProposal: (proposal) => openProposalReview(proposal),
  onAcceptStaging: async (proposal) => {
    const outcome = await Ipc.stagingAccept({ proposalId: proposal.id });
    const alreadyOpen = openBuffers.has(outcome.target_path);
    await openFile(outcome.target_path, { preview: true });
    if (alreadyOpen && buffer?.path === outcome.target_path && buffer.mode.kind === "file") {
      if (isDirty()) {
        showToast(`${outcome.target_path} was updated by accept; save to keep your changes.`);
      } else {
        try {
          const fresh = await Ipc.openForEdit({ rel: outcome.target_path });
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
    }
  },
  onRejectStaging: async (proposal) => {
    await Ipc.stagingReject({ proposalId: proposal.id });
  },
});

// status: status-bar-index-label, status-bar-active-file-index-state
// Status-bar index label + per-path tree-row marker rendering. Pairs
// with `mountIndexStatusBus` (mounted further down) — the bus is the
// data half (listeners + per-event bookkeeping); this view is the
// rendering half (label paint + per-row marker DOM mutation).
const indexStatusView: IndexStatusViewApi = mountIndexStatusView({
  statusIndexEl,
  getBuffer: () => buffer,
  isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
  isVaultOpen: () => vaultIsOpen(),
  getIndexState: (p) => tree.api.getIndexState(p),
  setIndexState: (p, s) => tree.api.setIndexState(p, s),
  fetchIndexState: (p) => tree.api.fetchIndexState(p),
  cssEscape,
});

function refreshTree(): Promise<void> {
  return tree.api.refresh();
}
function revealInTree(rel: string): Promise<void> {
  return tree.api.revealPath(rel);
}
function scheduleTreeRefreshFromWatcher(): void {
  tree.api.notifyWatcher();
}

function formatError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object" && "message" in err) {
    const m = (err as { message: unknown }).message;
    return typeof m === "string" ? m : JSON.stringify(err);
  }
  return JSON.stringify(err);
}

async function applyOpenedVault(path: string): Promise<void> {
  const basename = path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() ?? path;
  vaultPathEl.textContent = basename;
  vaultPathEl.title = path;
  tree.api.setSelectedFolder("");
  // Announce the open on the bus. No production subscriber today —
  // every cross-module wake-up after a vault swap currently rides
  // through the host's direct calls in `applyOpenedVault`. Declared
  // so future panels (and the deferred `vault-closed` counterpart)
  // have the typed seam without further main.ts edits.
  emitBusEvent("vault-opened", { path });
  indexStatusView.setOutstanding(0);
  // status: task-queue-home-widget
  // Tile mounts pre-vault-open; re-fetch settings + snapshot now.
  void taskQueueTile.refresh();
  // Re-seed the queue-detail worker toggles from the now-vault-bound
  // config — the initial seed at module-load may have errored or
  // resolved against a not-yet-vault-bound config.
  void queueDetail.api.refreshFromSettings();

  // status: settings-load-once-at-startup
  // Seed View menu / tree / panel state from the merged settings. Failures
  // here aren't fatal — fall back to whatever the in-memory defaults are.
  try {
    const s = await Ipc.getSettings<Settings>();
    applySettingsToUi(s);
  } catch (err) {
    Logger.error("ui::app", "get_settings failed", { err });
  }
  // Stale per-path state from a prior vault must not leak into the new one
  // (paths can collide across vaults).
  tree.api.clearCaches();
  // status: cluster-editor-sidebar-mode — re-fetch the open-trees list
  // against the freshly-opened vault. Failures self-log inside the
  // module; we don't need to surface them here.
  void clusterEditor?.refresh();
  // status: multi-buffer-in-memory-only — open buffers don't persist
  // across vault swaps; clear them along with the rest of per-vault state.
  openBuffers.clear();
  // status: editor-preview-tab — preview slot doesn't survive vault swap.
  setBufferState({ buffer: null, activePath: null, previewTabPath: null });
  editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: "" } });
  tabStrip.render();
  // Clear the related-notes panel so hits from the prior vault don't linger
  // until the next file open / save populates it for the new vault.
  void discovery.api.refreshRelated(null);
  // status: chat-panel-pinned-bottom — drop transcript and any in-flight
  // turn so the new vault starts clean.
  chatPanel.reset();
  // status: chat-session-resume-latest
  // Re-seed the panel from the most-recent on-disk session (if any).
  // The backend's `resume_latest_at_open` already adopted it as active;
  // we just paint the rendered transcript here.
  try {
    const active = await Ipc.chatSessionActive();
    chatPanel.hydrate(active);
  } catch (err) {
    Logger.error("ui::app", "chat_session_active failed", { err });
  }
  // Likewise, blank the search input/results so prior-vault matches don't
  // surface in the new vault. status: search-discovery-panel
  discovery.api.clear();
  startBackgroundIntervals();
  // status: trail-row-icon — seed the trail-doc set so the first
  // tree paint can decorate trail-doc rows. Awaited before
  // `refreshTree` so the initial paint already includes the icon.
  await tree.api.refreshTrailDocSet();
  // status: staging-accept-reject-from-tree — seed pending proposals
  // so the first tree paint includes synthetic staging rows.
  await tree.api.refreshStagingProposals();
  // status: patch-review-mode — seed the local pending-proposals cache
  // used by the agent-diff toggle + patch-review hunk decorations.
  await refreshPendingProposalsCache();
  await refreshTree();
  await refreshTrashBin();
  // status: trails-mode-body — re-fetch trails-list + active trail
  // detail after the settings snapshot has seeded `activeTrailStore`.
  trailsPanel?.api.onActiveTrailMaybeChanged();
  // status: navigation-history-stack — history is per-vault, so swapping
  // vaults drops the stack along with `openBuffers`. Cleared *before*
  // `vaultHome.setVisible(true)` below so the home page becomes the
  // first checkpoint on the new vault rather than landing on a stale tail.
  nav.reset();
  // status: vault-home-screen — default landing surface on vault open
  // (no auto-resume of last buffer in v1). Opens as a app-page tab per
  // tab-kinds so the editor toolbar + status bar hide on activation.
  void openAppPageTab("home", {});

  // status: autosave-recover-cmd, autosave-recovery-auto-restore,
  // autosave-tab-state-silent-restore
  // Stop any prior vault's tick before swapping; restart against the new
  // vault. Recovery modal first (load any unsaved buffers from the last
  // session); on resolve, silently load the tab-state snapshot and
  // reopen the saved tabs in order.
  autosave.stop();
  await runAutosaveRecoveryAndRestore();
  autosave.start();
}

/// status: autosave-recover-cmd, autosave-recovery-auto-restore,
/// autosave-tab-state-silent-restore
async function runAutosaveRecoveryAndRestore(): Promise<void> {
  let recovered: Awaited<ReturnType<typeof Ipc.autosaveRecover>> = [];
  try {
    recovered = await Ipc.autosaveRecover();
  } catch (err) {
    Logger.error("ui::app", "autosave_recover failed", { err });
  }
  // status: autosave-recovery-auto-restore
  // No prompt — every recovered buffer auto-opens as a sticky tab
  // carrying the autosaved content. For files still on disk the buffer
  // reads dirty (autosaved bytes vs. on-disk loadedText) so the user
  // sees the unsaved work and decides whether to save or revert via the
  // normal save / discard surfaces. For deleted files the autosaved
  // bytes are written back to disk first (so the file exists for the
  // editor to open) and the buffer comes up clean.
  for (const entry of recovered) {
    try {
      if (entry.on_disk_hash === null) {
        await Ipc.writeFile({
          rel: entry.path,
          contents: entry.autosaved_content,
          extraMetadata: null,
        });
        await openFile(entry.path, { preview: false });
      } else {
        await openFile(entry.path, { preview: false });
        if (buffer && buffer.path === entry.path) {
          editor.dispatch({
            changes: {
              from: 0,
              to: editor.getDocLength(),
              insert: entry.autosaved_content,
            },
          });
        }
      }
      // Autosaved copy is now live in memory — drop the sidecar.
      await autosave.discard(entry.path);
    } catch (err) {
      Logger.error("ui::app", "autosave restore failed", {
        path: entry.path,
        err,
      });
    }
  }

  // Tab-state restore — silent, even when the recovery modal had nothing
  // to surface. Reopens saved tabs in order, then activates active_path,
  // then opens preview_path if set and not already in the open set.
  // Failures (paths gone from disk) are dropped silently per spec.
  let tabState: Awaited<ReturnType<typeof Ipc.autosaveLoadTabState>> = null;
  try {
    tabState = await Ipc.autosaveLoadTabState();
  } catch (err) {
    Logger.error("ui::app", "autosave_load_tab_state failed", { err });
  }
  if (!tabState) return;
  const alreadyOpen = new Set(openBuffers.keys());
  const kinds = tabState.open_tab_kinds ?? {};
  for (const path of tabState.open_paths) {
    if (alreadyOpen.has(path)) continue;
    // status: tab-kinds — __hiker:* sentinels are app-page tabs, not
    // files. Restore them via openAppPageTab instead of openFile.
    if (path.startsWith("__hiker:")) {
      const kind = kinds[path] || "";
      if (kind === "home") {
        void openAppPageTab("home", {});
      } else if (kind === "home-detail") {
        void openAppPageTab("home-detail", {});
      } else if (kind === "queue") {
        void openAppPageTab("queue", {});
      } else if (kind === "settings") {
        void openAppPageTab("settings", {});
      } else if (kind === "agent") {
        const sessionId = path.replace(/^__hiker:agent:?/, "") || undefined;
        if (sessionId) {
          void openAgentTab(sessionId);
        }
      } else if (kind === "properties") {
        const rel = path.replace(/^__hiker:properties:/, "");
        if (rel) openPropertiesTab(rel);
      } else if (kind === "cluster-review") {
        // status: cluster-review-tab-no-persistence-until-confirm
        // Re-derive the purpose from the synthetic path key. The
        // in-memory structural result is NOT persisted; the user lands
        // on the configure phase with defaults.
        const rest = path.replace(/^__hiker:cluster-review:/, "");
        if (rest === "new-tree") {
          openClusterReviewTab({ kind: "new-tree" });
        } else if (rest.startsWith("recluster-subtree:")) {
          const [_, treeId, nodeId] = rest.split(":");
          if (treeId && nodeId) {
            openClusterReviewTab({ kind: "recluster-subtree", treeId, nodeId });
          }
        } else if (rest.startsWith("rebuild:")) {
          const treeId = rest.slice("rebuild:".length);
          if (treeId) openClusterReviewTab({ kind: "rebuild", treeId });
        }
      } else if (kind === "cluster-pane") {
        // status: cluster-editor-pane-mode
        // Same shape as the other synthetic tab kinds — re-open the
        // cluster-editor pane for the tree id encoded in the sentinel
        // (`__hiker:cluster-pane:<treeId>`). The pane defaults back to
        // the `cluster-tree` sub-state on restore; the user can re-enter
        // batch-review via Apply or by clicking a tree state pill that's
        // already `applied`. Persisting the live sub-state would require
        // widening the autosave payload — out of scope.
        //
        // Tolerate the legacy `cluster-batch-review:` prefix so autosave
        // state from earlier sessions still restores; can be dropped
        // after the next vault.
        const treeId = path
          .replace(/^__hiker:cluster-pane:/, "")
          .replace(/^__hiker:cluster-batch-review:/, "");
        if (treeId) void openClusterTab(treeId);
      }
      continue;
    }
    try {
      await openFile(path, { preview: false });
      alreadyOpen.add(path);
    } catch (err) {
      Logger.error("ui::app", "autosave tab restore: skipping path", {
        path,
        err,
      });
    }
  }
  if (tabState.active_path && openBuffers.has(tabState.active_path)) {
    activateTabInner(tabState.active_path);
  }
  if (
    tabState.preview_path
    && !openBuffers.has(tabState.preview_path)
  ) {
    try {
      await openFile(tabState.preview_path, { preview: true });
    } catch (err) {
      Logger.error("ui::app", "autosave preview-slot restore failed", {
        path: tabState.preview_path,
        err,
      });
    }
  }
}

// status: settings-default-vault-autoopen
// Vault open / bootstrap-from-default flow lives in `./app/vaultLifecycle`.
// Host owns `applyOpenedVault` (touches every panel API + store reset);
// the lifecycle module owns the picker + boot path + error funneling.
const vaultLifecycle = mountVaultLifecycle({
  applyOpenedVault,
  formatError,
});

pickBtn.addEventListener("click", () => void vaultLifecycle.openVault());

// `vaultLifecycle.bootstrapDefaultVault()` fires at the tail of bootstrap,
// after every UI mount has completed; that way `applyOpenedVault`'s
// closures (which touch every mounted module) all resolve cleanly.

// New-note button, tree-actions menu (Refresh / Reindex / Sort by),
// inline rename, attachContextMenu, deleteFromTree, countNotesIn,
// sortOrderLabel, openSortByMenu — all moved to ./tree.

// status: vault-home-screen, vault-home-button
// Home button in the top strip — opens the vault home as a app-page tab.
homeBtn.addEventListener("click", () => {
  void openAppPageTab("home", {});
});

// status: vault-bar-settings-icon
// Settings button in the top strip — opens the settings as a app-page tab.
settingsBtn.addEventListener("click", () => {
  void openAppPageTab("settings", {});
});

const win = getCurrentWindow();

// Custom window controls (decorations: false in tauri.conf.json — the
// top strip is the title bar, so we provide our own min/max/close +
// drag-to-move). Tauri 2's `data-tauri-drag-region` attribute only
// matches the exact event target, which makes clicks on inner
// containers (vault-path span, leading-cluster wrapper, empty tab-strip
// space) fall through and not initiate a drag. A mousedown listener on
// the whole strip that excludes interactive descendants gives us the
// behavior the OS title bar used to: drag to move, double-click to
// maximize, click on a button to do its action.
const { topStripEl, winMinBtn, winMaxBtn, winCloseBtn } = dom.topStrip;
function isInteractiveTarget(t: EventTarget | null): boolean {
  if (!(t instanceof Element)) return false;
  return !!t.closest(
    "button, input, textarea, a, [role='tab'], [role='button']",
  );
}
topStripEl?.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if (isInteractiveTarget(e.target)) return;
  e.preventDefault();
  void win.startDragging();
});
topStripEl?.addEventListener("dblclick", (e) => {
  if (isInteractiveTarget(e.target)) return;
  void win.toggleMaximize();
});
winMinBtn?.addEventListener("click", () => {
  void win.minimize();
});
winMaxBtn?.addEventListener("click", () => {
  void win.toggleMaximize();
});
winCloseBtn?.addEventListener("click", () => {
  // Routes through the same `onCloseRequested` handler below so the
  // autosave flush + tab-state snapshot run before destroy.
  void win.close();
});
// status: autosave-close-no-modal
// Always preventDefault and drive the close ourselves via `win.destroy()`.
// Returning without preventDefault to "let Tauri default-close" is
// unreliable (X button becomes a no-op), and `win.close()` would re-enter
// this handler — `destroy()` skips the close-requested round-trip.
//
// No dirty-buffer modal: every dirty buffer is flushed through the
// autosave pipeline and the open-tab snapshot is pushed, so next launch
// auto-restores the workspace as it was. Recovered tabs surface as dirty
// (autosaved bytes ≠ on-disk loadedText) and the user can save or revert
// then via the existing dirty-buffer affordances.
void win.onCloseRequested(async (event) => {
  event.preventDefault();
  try {
    await autosave.flushAllAndWait();
  } catch (err) {
    Logger.error("ui::app", "autosave flush (close) failed", { err });
  }
  try {
    await autosave.pushTabStateNow();
  } catch (err) {
    Logger.error("ui::app", "autosave_save_tab_state (close) failed", { err });
  }
  autosave.stop();
  openBuffers.clear();
  setBufferState({ buffer: null, activePath: null, previewTabPath: null });
  await win.destroy();
});

// `updateStatus()` fires at the tail of bootstrap, after every mount
// has completed; that's the linchpin that lets the TDZ-avoidance
// `let X: T | null = null` forward-decl scaffolding go away — every
// module the paint reads (`mutationsMenu`, `dirtyBufferDiff`,
// `modeControls`, `tabStrip`) is in scope as a const by then.

// status: status-bar-path-reveal — click handler lives in
// `./app/statusBar` alongside the path-element paint.

// ---------- vault home view ----------
// Vault-home (overview tiles + recent-activity detail) lives in `./vaultHome`.
// `vaultHome` is defined below, after `settingsPane` (the home/settings
// mutual-exclusion uses `settingsPane.isVisible()` in `onBeforeShow`).
// Forward refs from earlier mounts (e.g. `snapshotPreview.onClose`) reach
// `vaultHome` via closures resolved at call time, after init completes.

// status: settings-pane-mode
// status: vault-bar-settings-icon
// Settings pane sub-mode of the editor pane. Mutually exclusive with the
// vault-home view; opening either drops the other. Dirty-buffer guard is
// the same `confirm3` modal `openFile` uses (file-switch-guard-dirty).
const settingsPane: SettingsPaneApi = mountSettingsPane({
  paneEl: settingsPaneEl,
  settingsBtn,
  vaultPathEl,
  guardDirtyBuffer: async () => {
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
    // Drop home view; the editor's CM6 view is hidden by the
    // `settings-view` CSS class, so no explicit teardown is needed.
    if (vaultHome.isVisible()) vaultHome.setVisible(false);
  },
  onSettingApplied: (cfg) => {
    // Mirror the seeding `applyOpenedVault` does on first vault open so
    // every UI surface that reads from settings stays in sync after a
    // pane-driven flip — same way the View menu's persistSetting calls
    // already do for their specific keys.
    applySettingsToUi(cfg);
  },
});

// status: settings-pane-keybind
// `settings.open` chord: `Mod-,` (Cmd-, on macOS, Ctrl-, elsewhere). Same
// dual-half shape as `search-keybind-ctrl-space` — registered in CM6 so it
// wins inside the editor, plus a window-level handler for everywhere else.
register({
  id: "settings.open",
  keys: "Mod-,",
  label: "Open settings",
  run: () => {
    openAppPageTab("settings", {});
    return true;
  },
});

// Window-level keybinding handlers are installed below in one block,
// after every dep (`settingsPane`, `discovery`, `nav`) is mounted, per
// `./app/keybindings`.

// status: vault-home-screen
// Vault-home view (overview tiles + recent-activity detail) lives in
// `./vaultHome`. This module owns activity rows, recently-restored
// highlight, author filters, and the stats-refresh debounce. Snapshot
// preview / note open route back here via callbacks so the module never
// touches editor state directly.
const vaultHome: VaultHomeController = mountVaultHome({
  // PanelDeps cross-panel uniforms — vault home uses `formatErr`
  // (alert copy on stats / restore failure) and `openNote` (recents
  // row click → open file). `toast` / `settings` / `focusEditor` are
  // wired so future affordances don't need new deps fields.
  toast: panelToast,
  formatErr: formatError,
  settings,
  openNote: (rel, opts) => openFile(rel, opts),
  focusEditor: () => editor.focus(),
  editorPaneEl,
  vaultHomeEl,
  homeBtn,
  vaultPathEl,
  titleEl: dom.vaultHome.titleEl,
  statsBodyEl: dom.vaultHome.statsBodyEl,
  modifiedListEl: dom.vaultHome.modifiedListEl,
  accessedListEl: dom.vaultHome.accessedListEl,
  newNoteBtn: dom.vaultHome.newNoteBtn,
  overviewEl: dom.vaultHome.overviewEl,
  detailEl: dom.vaultHome.detailEl,
  detailTitleEl: dom.vaultHome.detailTitleEl,
  detailCountEl: dom.vaultHome.detailCountEl,
  detailListEl: dom.vaultHome.detailListEl,
  detailFiltersEl: dom.vaultHome.detailFiltersEl,
  activitySectionEl: dom.vaultHome.activitySectionEl,
  activityHeaderEl: dom.vaultHome.activityHeaderEl,
  activityListEl: dom.vaultHome.activityListEl,
  getVaultIsOpen: () => vaultIsOpen(),
  onOpenSnapshot: (row) => snapshotPreview.open(row),
  onBeforeShow: () => {
    if (settingsPane.isVisible()) void settingsPane.setVisible(false);
    if (queueDetail.isVisible()) {
      queueDetail.setVisible(false);
      dom.vaultHome.overviewEl.hidden = false;
    }
  },
  // status: tab-kinds — activity-widget clicks open home-detail tabs.
  onOpenPage: (kind, payload) => {
    if (kind === "home-detail") {
      void openAppPageTab("home-detail", payload);
    }
  },
  // status: staging-review-activity-detail-filter
  onOpenStagingProposal: (proposal) => openProposalReview(proposal),
  // status: staging-accept-navigates-to-preview
  // Activity-detail surface deliberately *does not* navigate on
  // accept: the surface is a list of pending items and jumping to
  // the target file on every click makes bulk-review hostile. The
  // staging list refreshes in place from `hiker:staging-changed`.
  // Fixes `bug-staging-accept-navigates-from-activity-detail`.
  onAcceptStaging: async (proposal) => {
    await Ipc.stagingAccept({ proposalId: proposal.id });
    // If the accepted file is already the active buffer in plain
    // editing mode, reload its contents from disk so the user sees
    // the result of the accept without leaving the page (or warn if
    // their in-flight edits would clobber it).
    const activePath = buffer?.path;
    if (activePath === proposal.target_path && buffer?.mode.kind === "file") {
      if (isDirty()) {
        showToast(`${proposal.target_path} was updated by accept; save to keep your changes.`);
      } else {
        try {
          const fresh = await Ipc.openForEdit({ rel: proposal.target_path });
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
    }
  },
  onRejectStaging: async (proposal) => {
    await Ipc.stagingReject({ proposalId: proposal.id });
  },
});

// status: task-queue-home-detail-view
const queueDetail: QueueDetailController = mountQueueDetail({
  // PanelDeps cross-panel uniforms — queue detail builds its own
  // local `SettingsManager` (with a toggles-tray flash callback) for
  // the in-tray write surface, so the host's `settings` instance
  // passed here is currently unused inside the module. Wired anyway
  // so the deps shape stays uniform.
  toast: panelToast,
  formatErr: formatError,
  settings,
  openNote: (rel, opts) => openFile(rel, opts),
  focusEditor: () => editor.focus(),
  containerEl: dom.vaultHome.queueDetailEl,
});

// status: navigation-history-stack
// status: top-strip-back-button, top-strip-forward-button
// status: navigation-trackpad-swipe, navigation-keybind
// Per-vault back/forward stack across editor-pane content surfaces.
// Mounted after the surfaces it observes (vault home / settings / queue
// detail / snapshot preview / trash) so `inferCurrent()` and `apply()`
// can read/drive them. Reset on vault swap; pruned on tab close + on
// preview-slot replacement.
const { navBackBtn, navForwardBtn } = dom.vaultBar;

function inferNavState(): NavState {
  const buf = buffer;
  // status: tab-kinds — non-buffer ("app-page") tab kinds derive their
  // NavState from the active tab's `(kind, payload)` rather than from
  // legacy DOM `hidden` / class flips. The synthetic tab key
  // `__hiker:<kind>[:<payload>]` carries the payload so we don't have to
  // probe each app-page module for its own active-view state.
  if (buf && buf.kind !== "buffer") {
    if (buf.kind === "home") return { kind: "home" };
    if (buf.kind === "home-detail") {
      // Parse the view discriminator out of the synthetic key
      // (`__hiker:home-detail:<view>`); fall back to the only currently
      // valid view if the prefix is malformed.
      let view: "recent-activity" = "recent-activity";
      const prefix = "__hiker:home-detail:";
      if (buf.path.startsWith(prefix)) {
        const raw = buf.path.slice(prefix.length);
        if (raw === "recent-activity") view = raw;
      }
      return { kind: "home-detail", view };
    }
    if (buf.kind === "queue") return { kind: "queue-detail" };
    if (buf.kind === "settings") return { kind: "settings" };
    if (buf.kind === "properties") {
      const prefix = "__hiker:properties:";
      const rel = buf.path.startsWith(prefix) ? buf.path.slice(prefix.length) : buf.path;
      return { kind: "properties", path: rel };
    }
    // `agent` / `graph` (future) — for now treat the synthetic key as a
    // tab path so applyNavState's `tab` branch handles activation when
    // the tab still exists. Reopening these kinds after eviction is a
    // followup once they have their own restore paths.
    return { kind: "tab", path: buf.path };
  }
  if (buf && buf.mode.kind === "trash") {
    const trashedName = buf.path.replace(/^\.hiker\/trash\//, "");
    return { kind: "trash-preview", trashedName };
  }
  if (buf && buf.mode.kind === "snapshot") {
    return {
      kind: "snapshot-preview",
      changeId: buf.mode.changeId,
      row: buf.mode.row,
    };
  }
  if (buf && buf.mode.kind === "write-note-review") {
    return {
      kind: "staging-preview",
      proposalId: buf.mode.proposal_id,
      targetPath: buf.mode.targetPath,
    };
  }
  if (
    activePath !== null
    && buf
    && (buf.mode.kind === "file" || buf.mode.kind === "patch-review")
  ) {
    return { kind: "tab", path: activePath };
  }
  return { kind: "empty" };
}

async function applyNavState(s: NavState): Promise<boolean> {
  switch (s.kind) {
    case "tab": {
      if (!openBuffers.has(s.path)) return false;
      activateTabInner(s.path);
      return true;
    }
    case "home": {
      openAppPageTab("home", {});
      return true;
    }
    case "home-detail": {
      openAppPageTab("home-detail", { view: s.view });
      return true;
    }
    case "queue-detail": {
      openAppPageTab("queue", {});
      return true;
    }
    case "settings": {
      openAppPageTab("settings", {});
      return true;
    }
    case "properties": {
      openPropertiesTab(s.path);
      return true;
    }
    case "trash-preview": {
      const item = trash.api.items().find((i) => i.trashed_name === s.trashedName);
      if (!item) return false;
      await trash.api.openPreview(item);
      return true;
    }
    case "snapshot-preview": {
      await snapshotPreview.open(s.row);
      return true;
    }
    case "staging-preview": {
      await openProposalReview({ id: s.proposalId, target_path: s.targetPath });
      return true;
    }
    case "empty": {
      return false;
    }
  }
}

function paintNavButtons(): void {
  navBackBtn.disabled = !nav!.canBack();
  navForwardBtn.disabled = !nav!.canForward();
}

const nav: NavApi = mountNavigation({
  inferCurrent: inferNavState,
  apply: applyNavState,
  onChange: paintNavButtons,
});

navBackBtn.addEventListener("click", () => {
  void nav!.back();
});
navForwardBtn.addEventListener("click", () => {
  void nav!.forward();
});

// status: navigation-history-stack
// Snapshot preview replaces the singleton `buffer` without mutating any
// observed DOM attribute, so the MutationObserver above can't detect the
// transition. Wrap its openers to checkpoint after the buffer flip lands.
// Trash gets the same treatment further down (after `trash` is mounted).
// The wrappers also fire on back/forward apply, where the nav module's
// `restoring` flag turns the checkpoint into a no-op.
{
  const _snapOpen = snapshotPreview.open;
  snapshotPreview.open = async (row) => {
    await _snapOpen.call(snapshotPreview, row);
    checkpointNav();
  };
  const _snapClose = snapshotPreview.close;
  snapshotPreview.close = () => {
    _snapClose.call(snapshotPreview);
    checkpointNav();
  };
}

// status: navigation-trackpad-swipe
// Two-finger horizontal trackpad swipe → back/forward. Right-swipe = back,
// left-swipe = forward (browser convention). Threshold ~120px accumulated
// `deltaX`. See `navigation/index.ts` for the wheel-event heuristic.
installNavigationSwipe({
  back: () => void nav!.back(),
  forward: () => void nav!.forward(),
});

// status: navigation-history-stack
// Per `tab-kinds` (editor.md), every content-surface flip — both buffer
// tabs and app-page tabs (home / home-detail / queue / settings /
// properties) — goes through `activateTab` in `app/tabs.ts`, which ends
// with a `checkpointNav()` call. The legacy MutationObserver that
// watched the editor-pane class flips and per-section `hidden` flips
// fired again *after* the canonical checkpoint, could read
// mid-transition DOM state into a wrong NavState, and double-pushed on
// every kind switch — see `bug-nav-history-broken-for-app-page-tabs`.
// The activation-time checkpoint is now the sole entry point.

// status: editor-diff-vs-disk-toggle, note-mutations-menu
// Dirty-buffer diff, status bar, mutations menu, mode controls, and view
// menu are all mounted inside `editorPane` (see `./editorPane`). The
// host accesses them via `editorPane.dirtyBufferDiff`,
// `editorPane.repaintStatusBar()`, `editorPane.mutationsMenu`,
// `editorPane.modeControls`, and `editorPane.viewMenu`.
// The individual mounts that previously lived here were lifted into
// editorPane per S3 of the tab-kinds refactor.

// status: note-mutation-applies-as-buffer-edit, mcp-ui-refresh-on-agent-write
// Both `hiker:note-mutation-applied` (apply mutation result into the
// open buffer / its saved CM6 state) and `hiker:changes-appended`
// (agent-author rows triggering tree refresh + active-buffer reload)
// listeners live in `./app/agentChanges`.
mountAgentChanges({
  editor,
  openBuffers,
  getBuffer: () => buffer,
  getActivePath: () => activePath,
  getPreviewTabPath: () => previewTabPath,
  isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
  isDirty,
  setBufferState,
  setReadOnly,
  updateStatus,
  scheduleChunkBoundariesRefresh,
  renderTabStrip: () => tabStrip.render(),
  scheduleTreeRefreshFromWatcher,
  getTreeSortOrder: () => tree.api.getSortOrder(),
  notifyChangesAppended: () => vaultHome.api.notifyChangesAppended(),
});

// status: task-queue-home-widget
// status: task-queue-home-widget-respects-llm-disable
// status: vault-bar-queue-button
// Wire the home-page Task queue tile + the new vault-bar queue button.
// The tile shows a one-line "X active · Y succeeded · Z failed" summary
// (middots) since vault open; the vault-bar button shows a pulsing blue
// dot when anything is active and a dim red dot if any task has failed
// since the last time the user viewed the queue. Hidden / inert entirely
// when `[llm] enabled = false`.
const taskQueueTile = (() => {
  const { tasksSection, tasksHeader, tasksSummary } = dom.vaultHome;
  const { queueBtnEl, queueIndicatorEl } = dom.vaultBar;

  let activeCount = 0;
  let succeededCount = 0;
  let failedCount = 0;
  let unreadFailure = false;
  let llmEnabled = true;
  let pendingReviewCount = 0;

  const SUMMARY_DOT = " · ";
  function paintSummary(): void {
    if (!tasksSection || !tasksSummary) return;
    if (!llmEnabled) {
      tasksSection.hidden = true;
      return;
    }
    tasksSection.hidden = false;
    if (activeCount + succeededCount + failedCount === 0) {
      tasksSummary.textContent = "No tasks queued";
      return;
    }
    tasksSummary.textContent = [
      `${activeCount} active`,
      `${succeededCount} succeeded`,
      `${failedCount} failed`,
    ].join(SUMMARY_DOT);
  }

  function paintIndicator(): void {
    if (!queueBtnEl || !queueIndicatorEl) return;
    if (!llmEnabled) {
      queueBtnEl.hidden = true;
      queueIndicatorEl.hidden = true;
      return;
    }
    queueBtnEl.hidden = false;

    const total = activeCount + pendingReviewCount;

    if (total > 0) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.textContent = String(total);
      if (activeCount > 0) {
        queueIndicatorEl.classList.add("queue-indicator-active");
      } else {
        queueIndicatorEl.classList.remove("queue-indicator-active");
      }
      return;
    }

    queueIndicatorEl.classList.remove("queue-indicator-active");
    if (unreadFailure) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.textContent = "";
      queueIndicatorEl.classList.add("queue-indicator-failed");
      return;
    }
    queueIndicatorEl.hidden = true;
    queueIndicatorEl.classList.remove("queue-indicator-failed");
  }

  function repaint(): void {
    paintSummary();
    paintIndicator();
  }

  // status: staging-review-top-bar-badge
  async function refreshStaging(): Promise<void> {
    try {
      pendingReviewCount = await Ipc.stagingCount();
    } catch {
      pendingReviewCount = 0;
    }
    repaint();
  }

  let stagingInterval: ReturnType<typeof setInterval> | undefined;

  function startStagingPolling(): void {
    stopStagingPolling();
    stagingInterval = setInterval(() => { void refreshStaging(); }, 30_000);
  }

  function stopStagingPolling(): void {
    if (stagingInterval) { clearInterval(stagingInterval); stagingInterval = undefined; }
  }

  // Listen for staging changes so the badge updates immediately after
  // an accept/reject/accept_all, without waiting for the next poll tick.
  // Refresh the cached `pendingProposals` BEFORE re-rendering the tree —
  // otherwise the render reads stale proposals and leaves the just-accepted
  // file's row marked `staging-new` / `staging-dirty` (see
  // `bug-accept-new-note-from-activity-page-leaves-stale-dirty-marker`).
  void listen("hiker:staging-changed", () => {
    void refreshStaging();
    void (async () => {
      await tree.api.refreshStagingProposals();
      await tree.api.refresh();
    })();
    // status: patch-review-mode
    // Re-sync local pending-proposals cache for the agent-diff toggle
    // grey state and the active patch-review hunk decorations.
    void (async () => {
      await refreshPendingProposalsCache();
      const buf = buffer;
      if (buf && (buf.mode.kind === "patch-review" || buf.mode.kind === "file")) {
        const proposals = pendingEditProposalsForPath(buf.path);
        editorPane.patchReview.setProposals(
          buf.mode.kind === "patch-review" ? proposals : [],
        );
      }
      refreshAgentDiffBtn();
      // status: write-note-pending-banner
      // Staging change may have added or removed a pending write-shape
      // proposal for the active path. Invalidate the existence cache
      // (cheap) so the label re-probes if the target's on-disk presence
      // changed since last paint, then repaint.
      writeNoteTargetExistsCache.clear();
      refreshWriteNotePendingBanner();
    })();
    // bug-home-recent-activity-missing-pending-agent-review:
    // home recent-activity widget consumes the unified feed and must
    // repaint when staging proposals appear/disappear.
    vaultHome.api.notifyStagingChanged();
  });

  function openQueueDetail(): void {
    if (!llmEnabled) return;
    // status: tab-kinds — open the queue as a app-page tab instead of
    // swapping the vaultHome sub-mode.
    void openAppPageTab("queue", {});
    unreadFailure = false;
    paintIndicator();
  }

  if (tasksHeader) {
    tasksHeader.style.cursor = "pointer";
    tasksHeader.addEventListener("click", openQueueDetail);
  }
  if (queueBtnEl) {
    queueBtnEl.addEventListener("click", openQueueDetail);
  }

  void listen<{ event: string }>("hiker:queue-event", (ev) => {
    const k = ev.payload.event;
    if (k === "task_queued") {
      activeCount += 1;
    } else if (k === "task_completed") {
      activeCount = Math.max(0, activeCount - 1);
      succeededCount += 1;
    } else if (k === "task_failed") {
      activeCount = Math.max(0, activeCount - 1);
      failedCount += 1;
      // Only flag the indicator red if the user isn't currently looking
      // at the queue — otherwise the dot would light up under their
      // cursor for no reason.
      if (!queueDetail.isVisible()) unreadFailure = true;
    } else if (k === "task_cancelled") {
      activeCount = Math.max(0, activeCount - 1);
    }
    repaint();
  });

  async function refresh(): Promise<void> {
    try {
      const cfg = await Ipc.getSettings<{ llm: { enabled: boolean } }>();
      llmEnabled = cfg.llm.enabled;
    } catch {
      // No vault open yet — keep the tile + button hidden until
      // refresh() runs again.
      llmEnabled = false;
    }
    try {
      const rows = await Ipc.tasksSnapshot<{ state: string }>();
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      for (const r of rows) {
        if (r.state === "queued" || r.state === "leased") activeCount += 1;
        else if (r.state === "completed") succeededCount += 1;
        else if (r.state === "failed") failedCount += 1;
      }
      // Fresh vault → no unread state to inherit.
      unreadFailure = false;
    } catch {
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      unreadFailure = false;
    }
    repaint();
    void refreshStaging();
    startStagingPolling();
  }
  repaint();
  void refresh();
  return { refresh, stopStagingPolling };
})();

// Snapshot-preview lifecycle: callers go through the `snapshotPreview`
// API directly (e.g. `snapshotPreview.open(row)` from vault-home; the
// mode-controls renderer above wires its toolbar buttons in the same way).

// `hiker:changes-appended` listener (vault-home's recents/activity
// refresh + agent-write tree refresh + active-buffer reload) lives in
// `./app/agentChanges` alongside `hiker:note-mutation-applied`.

// ---------- panel toggles ----------

function syncToggleButtons(): void {
  toggleSidebarBtn.classList.toggle("active", !appEl.classList.contains("sidebar-collapsed"));
  toggleRelatedBtn.classList.toggle("active", !appEl.classList.contains("related-collapsed"));
}

toggleSidebarBtn.addEventListener("click", () => {
  appEl.classList.toggle("sidebar-collapsed");
  syncToggleButtons();
  // Emit on the bus so other modules can react without the toggle
  // handler having to know who cares. The discovery / tree / chat
  // panels don't take direct calls from here anymore — they subscribe
  // if they care about sidebar visibility transitions.
  const open = !appEl.classList.contains("sidebar-collapsed");
  emitBusEvent("sidebar-toggled", { open });
  if (!vaultIsOpen()) return;
  void persistSetting("vault", "vault.sidebar_open", open);
});
toggleRelatedBtn.addEventListener("click", () => {
  appEl.classList.toggle("related-collapsed");
  syncToggleButtons();
  const open = !appEl.classList.contains("related-collapsed");
  // The related panel rides the same bus event as the sidebar — both
  // are "a side column collapsed/uncollapsed" transitions. If a
  // subscriber needs to distinguish, the payload can grow a `which`
  // field; for now no consumer needs the distinction.
  emitBusEvent("sidebar-toggled", { open });
  if (!vaultIsOpen()) return;
  void persistSetting("vault", "vault.related_open", open);
});

// Default: tree open, related collapsed (per editor.md). Overridden once
// `get_settings` lands in `openVault` for vaults that have explicit values.
appEl.classList.add("related-collapsed");
syncToggleButtons();

// status: sidebar-mode-switcher
// Files / Cluster trees / Trails switcher at the top of the sidebar.
// Files-mode body is the existing tree + toolbar; Clusters-mode swaps in a
// placeholder until the cluster editor surface (`cluster-editor-sidebar-mode`)
// lands; Trails is greyed in v1 until trails do. The trash bin
// (`#trash-bin`) is shared across modes per spec. Mode persists per-vault
// under `vault.sidebar_mode`.
type SidebarMode = "files" | "clusters" | "trails";
const sidebarEl = document.getElementById("sidebar");
const sidebarModeFilesBtn = document.getElementById("sidebar-mode-files");
const sidebarModeClustersBtn = document.getElementById("sidebar-mode-clusters");
const sidebarModeTrailsBtn = document.getElementById("sidebar-mode-trails");
let sidebarMode: SidebarMode = "files";
function paintSidebarMode(): void {
  if (!sidebarEl) return;
  sidebarEl.classList.toggle("mode-files", sidebarMode === "files");
  sidebarEl.classList.toggle("mode-clusters", sidebarMode === "clusters");
  sidebarEl.classList.toggle("mode-trails", sidebarMode === "trails");
  for (const [btn, mode] of [
    [sidebarModeFilesBtn, "files"],
    [sidebarModeClustersBtn, "clusters"],
    [sidebarModeTrailsBtn, "trails"],
  ] as const) {
    if (!btn) continue;
    const active = sidebarMode === mode;
    btn.classList.toggle("active", active);
    btn.setAttribute("aria-selected", active ? "true" : "false");
  }
}
function setSidebarMode(mode: SidebarMode, persist: boolean): void {
  if (mode === sidebarMode) return;
  sidebarMode = mode;
  paintSidebarMode();
  if (persist && vaultIsOpen()) {
    void persistSetting("vault", "vault.sidebar_mode", mode);
  }
}
paintSidebarMode();
sidebarModeFilesBtn?.addEventListener("click", () =>
  setSidebarMode("files", true),
);
sidebarModeClustersBtn?.addEventListener("click", () =>
  setSidebarMode("clusters", true),
);
sidebarModeTrailsBtn?.addEventListener("click", () => {
  setSidebarMode("trails", true);
});

// status: trails-default-location
// status: sidebar-new-item-button (Trails-mode left-click branch)
// `+` button is mode-aware. Files mode keeps the existing tree-owned
// create-note path. Trails mode creates a new trail at the configured
// `[trails] new_trail_dir` (suffix-counted by `core::trails::create_trail`),
// auto-activates it, opens the trail-doc, and triggers inline rename
// so the user can name it before submitting. Clusters is a v1 no-op
// until the cluster editor lands. Capture-phase + `stopImmediatePropagation`
// preempts the tree module's own listener for the trails branch only;
// Files mode falls through unchanged.
newNoteBtn.addEventListener(
  "click",
  (e) => {
    if (sidebarMode === "files") return;
    e.stopImmediatePropagation();
    // status: cluster-editor-new-tree-action — `+` in clusters mode opens
    // the New-tree modal (the "Suggest reorganization" entry point).
    if (sidebarMode === "clusters") {
      clusterEditor?.newTree();
      return;
    }
    if (sidebarMode !== "trails") return;
    void (async () => {
      let created: { trail_doc_rel: string; trail_id: string };
      try {
        created = await Ipc.trailCreate({ name: "new-trail" });
      } catch (err) {
        Logger.error("ui::trails", "trail_create failed", { err });
        showToast(`Couldn't create trail: ${formatError(err)}`);
        return;
      }
      try {
        await Ipc.trailSetActive({ trailDocRel: created.trail_doc_rel });
        activeTrailStore.set({ rel: created.trail_doc_rel });
      } catch (err) {
        Logger.error("ui::trails", "trail_set_active failed", { err });
      }
      // Open the trail-doc, refresh the tree so the new row exists,
      // then begin inline rename. The tree refresh is also driven by
      // the watcher event for the create, but we don't want to race
      // — refresh explicitly so `beginInlineRenameByPath` has a row
      // to attach to.
      try {
        await openFile(created.trail_doc_rel, { preview: false });
      } catch (err) {
        Logger.error("ui::trails", "open new trail-doc failed", { err });
      }
      await tree.api.refreshTrailDocSet();
      await tree.api.refresh();
      await tree.api.revealPath(created.trail_doc_rel);
      await tree.api.beginInlineRenameByPath(created.trail_doc_rel);
      // Kick the trails panel so the new trail surfaces in the
      // dropdown / body without waiting for the watcher debounce.
      void trailsPanel?.api.refresh();
    })();
  },
  true, // capture phase — runs before the tree module's bubbled listener
);

// status: cluster-editor-mode-menu
// `…` overflow is mode-aware. In clusters mode it routes to the cluster
// editor's mode menu (New tree / Discard drafts / Refresh) rather than
// the file-tree's Refresh / Reindex entries. Capture phase + stopImmediate
// preempts the tree module's bubbled listener, matching the `+` button's
// pattern above.
sidebarActionsBtn.addEventListener(
  "click",
  (e) => {
    if (sidebarMode !== "clusters") return;
    e.stopImmediatePropagation();
    if (clusterEditor) clusterEditor.openModeMenu(sidebarActionsBtn);
  },
  true,
);

// status: side-panel-resize
// Drag handles on the inner edge of the sidebar / discovery columns.
// Per the spec: 4px handles, `col-resize` cursor, min/max clamped, persisted
// per-vault on pointerup. The CSS grid column-template reads
// `--sidebar-width` / `--discovery-width` from `#app`'s inline style; the
// drag updates those vars live so CM6 reflows for free, and the toggle
// (`sidebar-collapsed` / `related-collapsed`) still hides the column
// wholesale via `grid-template-columns: 0 …` overrides — collapse is
// not "drag width to 0."
const SIDEBAR_MIN_PX = 160;
const DISCOVERY_MIN_PX = 220;
function maxSidePanelPx(): number {
  return Math.max(SIDEBAR_MIN_PX, Math.floor(window.innerWidth * 0.5));
}
function setSidebarWidthVar(px: number): void {
  const clamped = Math.round(
    Math.min(Math.max(px, SIDEBAR_MIN_PX), maxSidePanelPx()),
  );
  appEl.style.setProperty("--sidebar-width", `${clamped}px`);
}
function setDiscoveryWidthVar(px: number): void {
  const clamped = Math.round(
    Math.min(Math.max(px, DISCOVERY_MIN_PX), maxSidePanelPx()),
  );
  appEl.style.setProperty("--discovery-width", `${clamped}px`);
}
function readWidthVar(name: "--sidebar-width" | "--discovery-width"): number {
  const raw = getComputedStyle(appEl).getPropertyValue(name).trim();
  const n = parseFloat(raw);
  return Number.isFinite(n) ? n : name === "--sidebar-width" ? 280 : 320;
}

function wireSidePanelResize(
  handle: HTMLElement,
  edge: "sidebar" | "discovery",
): void {
  let dragStartX = 0;
  let dragStartW = 0;
  handle.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    const collapsedCls =
      edge === "sidebar" ? "sidebar-collapsed" : "related-collapsed";
    if (appEl.classList.contains(collapsedCls)) return;
    ev.preventDefault();
    handle.classList.add("dragging");
    handle.setPointerCapture(ev.pointerId);
    dragStartX = ev.clientX;
    dragStartW = readWidthVar(
      edge === "sidebar" ? "--sidebar-width" : "--discovery-width",
    );
  });
  handle.addEventListener("pointermove", (ev) => {
    if (!handle.classList.contains("dragging")) return;
    const dx = ev.clientX - dragStartX;
    // Sidebar grows when dragging right; discovery grows when dragging left.
    const next = edge === "sidebar" ? dragStartW + dx : dragStartW - dx;
    if (edge === "sidebar") setSidebarWidthVar(next);
    else setDiscoveryWidthVar(next);
  });
  function endDrag(ev: PointerEvent): void {
    if (!handle.classList.contains("dragging")) return;
    handle.classList.remove("dragging");
    try {
      handle.releasePointerCapture(ev.pointerId);
    } catch {}
    if (!vaultIsOpen()) return;
    const px = readWidthVar(
      edge === "sidebar" ? "--sidebar-width" : "--discovery-width",
    );
    const key =
      edge === "sidebar" ? "vault.sidebar_width" : "vault.discovery_width";
    void persistSetting("vault", key, Math.round(px));
  }
  handle.addEventListener("pointerup", endDrag);
  handle.addEventListener("pointercancel", endDrag);
}

const sidebarResizeHandleEl = document.getElementById("sidebar-resize-handle");
const discoveryResizeHandleEl = document.getElementById(
  "discovery-resize-handle",
);
if (sidebarResizeHandleEl) wireSidePanelResize(sidebarResizeHandleEl, "sidebar");
if (discoveryResizeHandleEl)
  wireSidePanelResize(discoveryResizeHandleEl, "discovery");

// View ▾ menu — `buildItems` factory + click-handler wiring extracted to
// `./app/viewMenu`. Reads `viewSettingsStore`; writes via `editor.setX`
// ---------- discovery panel (search + related) ----------
// Search input + mode toggles + lexical/semantic results + related-notes
// panel + collapsible sections + roving-tabindex keyboard nav all live in
// `./discovery`. Host wires DOM ids and the editor-coupled callbacks
// (`onOpenNote`, `onScrollToChunk`).
const discovery: DiscoveryController = mountDiscovery({
  // PanelDeps cross-panel uniforms — discovery uses `settings` (mode
  // / section-collapse persistence) and `openNote` (search/related
  // row click). `toast` / `formatErr` / `focusEditor` are wired so
  // future affordances don't need new deps fields.
  toast: panelToast,
  formatErr: formatError,
  settings,
  openNote: (rel, opts) => openFile(rel, opts ?? {}).then(() => undefined),
  focusEditor: () => editor.focus(),
  appEl,
  inputEl: searchInputEl,
  clearBtn: searchClearBtn,
  toggleSemanticBtn: toggleModeSemanticBtn,
  toggleLexicalBtn: toggleModeLexicalBtn,
  searchSectionEl,
  searchListEl,
  searchCountEl,
  searchSpinnerEl,
  relatedSectionEl,
  relatedListEl,
  relatedCountEl,
  onScrollToChunk: async (rel, chunkIndex) => {
    if (buffer?.path !== rel) return;
    try {
      const bounds = await Ipc.chunksFor({ rel });
      const target = bounds.find((b) => b.chunk_index === chunkIndex);
      if (!target) return;
      const safe = Math.min(target.char_start, editor.getDocLength());
      editor.dispatch({
        selection: { anchor: safe },
        effects: EditorView.scrollIntoView(safe, { y: "start" }),
      });
      editor.focus();
    } catch (err) {
      Logger.error("ui::app", "scroll-to-chunk failed", { err });
    }
  },
  expandPanelIfCollapsed: () => {
    const wasCollapsed = appEl.classList.contains("related-collapsed");
    if (wasCollapsed) {
      appEl.classList.remove("related-collapsed");
      void persistSetting("vault", "vault.related_open", true);
      syncToggleButtons();
      // Programmatic uncollapse rides the same bus event as a manual
      // toggle — subscribers shouldn't care which path triggered it.
      emitBusEvent("sidebar-toggled", { open: true });
    }
    return wasCollapsed;
  },
});

// status: trails-mode-body
// Trails sidebar mode body. Module owns trails-list cache, active-trail
// detail cache, expanded-card state, and refresh epoch; host wires the
// `openNote` callback + the `#sidebar-trails-body` root.
const sidebarTrailsBodyEl = document.getElementById("sidebar-trails-body");
const trailsPanel: TrailsController | null = sidebarTrailsBodyEl
  ? mountTrailsPanel({
      toast: panelToast,
      formatErr: formatError,
      settings,
      openNote: (rel, opts) => openFile(rel, opts ?? {}).then(() => undefined),
      focusEditor: () => editor.focus(),
      rootEl: sidebarTrailsBodyEl,
      // `core::trails::remove_waypoint` cascades through `core::ops::delete`
      // which suppresses the watcher for deleted paths, so the
      // `hiker:file-changed`-driven refresh can't fire. Refresh the trash
      // panel + vault-home activity surface explicitly instead.
      onWaypointRemoved: () => {
        void trash.api.refresh();
        vaultHome.api.notifyChangesAppended();
      },
    })
  : null;
// status: cluster-editor-sidebar-mode
// Cluster-trees sidebar body. Loads the open trees list on mount + on
// every refresh; opens the New-tree modal on the `+` (new-note) button
// hijack below for clusters-mode, and on the per-tree action chip.
const sidebarClustersBodyEl = document.getElementById("sidebar-clusters-body");
let clusterEditor: ClusterEditorApi | null = null;
if (sidebarClustersBodyEl) {
  // Drop the placeholder class so our own CSS layout applies without
  // fighting the centered "Coming soon" stub copy.
  sidebarClustersBodyEl.classList.remove("sidebar-mode-placeholder");
  // The element is `hidden` in the markup so the placeholder stub
  // doesn't show in modes other than clusters. The mode CSS uses
  // `display: block` to reveal it; the `hidden` attribute would
  // override that, so drop it now and let the mode-class do the work.
  sidebarClustersBodyEl.removeAttribute("hidden");
  sidebarClustersBodyEl.replaceChildren();
  clusterEditor = mountClusterEditor({
    rootEl: sidebarClustersBodyEl,
    openNote: (rel, opts) => openFile(rel, opts ?? {}).then(() => undefined),
    openPane: (treeId, treeName) => openClusterTab(treeId, treeName),
    // status: cluster-review-tab-from-new-tree-action
    openNewTreeReview: () => openClusterReviewTab({ kind: "new-tree" }),
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

// status: cluster-editor-pane-mode, cluster-editor-batch-review-pane-mode
const clusterPaneEl = document.getElementById("cluster-editor-pane");
let clusterEditorPane: ClusterEditorPaneApi | null = null;
if (clusterPaneEl) {
  clusterEditorPane = mountClusterEditorPane({
    rootEl: clusterPaneEl,
    openNote: (rel, opts) => openFile(rel, opts ?? {}).then(() => undefined),
    closePane: () => {
      const key = currentClusterTabKey;
      if (key) {
        void closeTab(key);
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
let currentClusterTabKey: string | null = null;

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
    openNote: (rel, opts) => openFile(rel, opts ?? {}).then(() => undefined),
    transitionToPane: async (tabKey, treeId, treeName) => {
      // Tab transitions in place — drop the cluster-review buffer entry,
      // open a cluster-pane tab in its slot. The cluster-pane mount
      // (`openClusterTab`) re-uses the existing key shape, so we close
      // the review tab then open the pane tab; the user's "current tab"
      // visibly shifts to the new one.
      openBuffers.delete(tabKey);
      autosave.scheduleTabStatePush();
      await openClusterTab(treeId, treeName);
      // Sidebar's Cluster trees list reads `cluster_trees_list` on mount
      // and on explicit refresh — kick a refresh so the just-persisted
      // tree (new-tree case) or the reshaped subtree (recluster case)
      // shows up without requiring a vault re-open.
      void clusterEditor?.refresh();
    },
    closeTab: (tabKey) => {
      void closeTab(tabKey);
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
  const key = clusterReviewTab.open(purpose);
  const existing = openBuffers.get(key);
  let label = "Cluster review";
  if (purpose.kind === "new-tree") label = "Cluster review: new tree";
  else if (purpose.kind === "recluster-subtree") {
    label = `Subcluster: ${purpose.nodeName ?? ""}`.trim();
  } else if (purpose.kind === "rebuild") label = "Cluster review: rebuild";
  if (existing) {
    existing.buffer.displayLabel = label;
    activateTabInner(key);
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
  activateTabInner(key);
  autosave.scheduleTabStatePush();
}

// status: cluster-editor-pane-mode
/// Open or activate the cluster-editor pane tab for `treeId`. The tab
/// is sticky (no preview slot) — the pane is heavy enough that a
/// preview-eviction surprise would be jarring. Re-opening for the same
/// tree re-paints the tree view without losing batch-review state.
async function openClusterTab(treeId: string, treeName?: string): Promise<void> {
  const key = appPageTabKey("cluster-pane", treeId);
  currentClusterTabKey = key;
  const existing = openBuffers.get(key);
  if (existing) {
    if (treeName) existing.buffer.displayLabel = treeName;
    activateTabInner(key);
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
  activateTabInner(key);
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
    const entry = openBuffers.get(key);
    if (!entry) return;
    entry.buffer.displayLabel = row.name;
    tabStrip.render();
  } catch (err) {
    Logger.error("ui::clusterEditor", "hydrateClusterTabLabel failed", { err });
  }
}

// Refresh on external active-trail mutations — `applySettingsToUi`
// re-seeds `activeTrailStore` after every `set_setting` round-trip,
// and other surfaces (e.g. `set as active` tree verb, slice U3 +
// button) write through it too.
{
  let lastActiveRel = activeTrailStore.get().rel;
  activeTrailStore.subscribe((s) => {
    if (s.rel === lastActiveRel) return;
    lastActiveRel = s.rel;
    trailsPanel?.api.onActiveTrailMaybeChanged();
  });
}

// Window-level keybinding handlers (settings.open global half,
// search-keybind-ctrl-space global half, tab keybinds, navigation
// keybinds) live in `./app/keybindings`. Installed once now that every
// dep is in scope (`settingsPane`, `discovery`, `nav` is forward-decl
// null until its mount; the module reads it lazily through `getNav`).
installWindowKeybindings({
  toggleSettings: () => settingsPane.toggle(),
  focusSearchInput: () => discovery.api.focusInput(),
  closeActiveTab: () => {
    if (activePath) void closeTab(activePath);
  },
  cycleTab,
  jumpToTab,
  getNav: () => nav,
  getActivePath: () => activePath,
});

let bufferPathInterval: number | null = null;
let lastSeenBufferPath: string | null = null;

// `index_status` is now pushed from the indexer over `hiker:index-status`
// (see the listener below). The 2s poll has been removed; the buffer-path
// watcher stays — that one's a separate UI concern (active-buffer changes
// drive the related-notes refresh).
function startBackgroundIntervals(): void {
  if (bufferPathInterval !== null) window.clearInterval(bufferPathInterval);
  bufferPathInterval = window.setInterval(() => {
    if (!vaultIsOpen()) return;
    const cur = buffer?.path ?? null;
    if (cur !== lastSeenBufferPath) {
      lastSeenBufferPath = cur;
      discovery.api.scheduleRelatedRefresh(cur, 0);
    }
  }, 250);
}

// ---------- index status indicator ----------
// `renderIndexStatus` + `updateIndexStateForPath` + the cached
// `IndexStatus` snapshot all live in `./app/indexStatusView` (mounted
// above, right after `tree`). The bus below is the data half of the
// pairing — listeners for `hiker:index-status` / `hiker:reindex-progress`
// — and pushes status / outstanding / per-path updates into the view.
mountIndexStatusBus({
  onStatusChanged: (next) => indexStatusView.setStatus(next),
  onOutstandingChanged: (count) => indexStatusView.setOutstanding(count),
  updateIndexStateForPath: (path, state) =>
    indexStatusView.updateIndexStateForPath(path, state),
  deleteIndexState: (p) => tree.api.deleteIndexState(p),
  getIndexState: (p) => tree.api.getIndexState(p),
  getActiveBufferPath: () => buffer?.path ?? null,
  scheduleRelatedRefresh: (rel, delayMs) =>
    discovery.api.scheduleRelatedRefresh(rel, delayMs),
  scheduleStatsRefresh: () => vaultHome.api.scheduleStatsRefresh(),
});


// ---------- trash bin ----------
// Trash bin (sidebar collapsible, list rendering, row context menu,
// restore/purge, read-only preview) lives in `./trash`. The host wires it
// to DOM elements and the editor view via the deps below.
const trash: TrashController = mountTrash({
  binEl: trashBinEl,
  headerEl: trashHeaderEl,
  listEl: trashListEl,
  chevronEl: trashChevronEl,
  labelEl: trashLabelEl,
  editor: editor!,
  getBuffer: () => buffer,
  setBuffer: (b) => {
    setBufferState({ buffer: b as Buffer | null });
  },
  cssEscape,
  isVaultIsOpen: () => vaultIsOpen(),
  settings,
  isVaultHomeVisible: () => vaultHome.isVisible(),
  setVaultHomeVisible: (on) => vaultHome.setVisible(on),
  refreshTree,
  formatError,
});
function refreshTrashBin(): Promise<void> {
  return trash.api.refresh();
}

// status: navigation-history-stack
// Mirror of the snapshot wrap above — trash preview also bypasses the
// editor-pane MutationObserver, so checkpoint after open/close.
{
  const _trashOpen = trash.api.openPreview;
  trash.api.openPreview = async (item) => {
    await _trashOpen.call(trash.api, item);
    checkpointNav();
  };
  const _trashClose = trash.api.closePreview;
  trash.api.closePreview = () => {
    _trashClose.call(trash.api);
    checkpointNav();
  };
}
// Mount the openFile slice now that every dep it needs (CM6 view,
// stores, tree reveal, panels, nav, tab strip) is in scope. The
// forward-declared `openFile` / `promotePreview*` shims above pick
// up the implementation here.
const tabs: TabsApi = mountTabs({
  editor: editor!,
  setBufferState,
  getBuffer: () => buffer,
  getActivePath: () => activePath,
  getPreviewTabPath: () => previewTabPath,
  openBuffers,
  bumpActivationCounter,
  inFlightMutationPaths,
  getLivePreviewEnabled: () => livePreviewEnabled,
  getHideFrontmatterEnabled: () => hideFrontmatterEnabled,
  getOpenFileApi: () => openFileApi,
  onShowHome: () => { openAppPageTab("home", {}); },
  save: () => save(),
  isDirty: () => isDirty(),
  revealInTree: (rel) => revealInTree(rel),
  updateStatus,
  refreshChunkBoundaries,
  renderTabStrip: () => tabStrip.render(),
  pruneNavTab: (rel) => nav.pruneTab(rel),
  checkpointNav,
  setReadOnly: (ro) => setReadOnly(ro),
});

const openFileApi = mountOpenFile({
  editor: editor!,
  setBufferState,
  getBuffer: () => buffer,
  getActivePath: () => activePath,
  getPreviewTabPath: () => previewTabPath,
  openBuffers,
  bumpActivationCounter,
  inFlightMutationPaths,
  activateTab: activateTabInner,
  hideVaultHomeIfVisible: () => {
    if (vaultHome.isVisible()) vaultHome.setVisible(false);
  },
  hideSettingsPaneIfVisible: () => {
    if (settingsPane.isVisible()) {
      void settingsPane.setVisible(false);
    }
  },
  revealInTree: (rel) => revealInTree(rel),
  updateStatus,
  refreshChunkBoundaries,
  scheduleChunkBoundariesRefresh,
  renderTabStrip: () => tabStrip.render(),
  pruneNavTab: (rel) => nav.pruneTab(rel),
  checkpointNav,
  // After `resolve_drift` returns `TookTheirs`, swap the editor doc +
  // the buffer's loadedText / token in one place so both the save
  // flow's drift-modal and the watcher-conflict path land on the same
  // code.
  applyTookTheirs: (rel, contents, token) => {
    const buf = buffer;
    if (!buf || buf.path !== rel) return;
    editor.dispatch({
      changes: { from: 0, to: editor.getDocLength(), insert: contents },
    });
    buf.loadedText = editor.getActiveText();
    buf.token = token;
    updateStatus();
  },
});

// status: tab-kinds
/// Open or activate a app-page tab in the preview slot. Evicts other
/// app-page previews. Called from Home / Queue / Settings button handlers.
async function openAppPageTab(
  kind: "home" | "home-detail" | "queue" | "settings",
  payload?: Record<string, string>,
): Promise<void> {
  const key = appPageTabKey(kind, payload?.view);
  if (openBuffers.has(key)) {
    activateTabInner(key);
    return;
  }
  // Evict the current preview tab (any kind) — at most one preview
  // exists at a time per spec. Sticky tabs (preview: false) survive.
  if (previewTabPath) {
    const oldEntry = openBuffers.get(previewTabPath);
    if (oldEntry && oldEntry.buffer.preview) {
      openBuffers.delete(previewTabPath);
      nav.pruneTab(previewTabPath);
    }
  }
  const buf: Buffer = {
    path: key,
    loadedText: "",
    token: null,
    kind: kind as TabKind,
    mode: { kind: "file" },
    pendingChangesMetadata: null,
    preview: true,
  };
  openBuffers.set(key, {
    buffer: buf,
    savedState: null,
    lastActivatedAt: bumpActivationCounter(),
  });
  setBufferState({ previewTabPath: key });
  activateTabInner(key);
}

// status: note-properties-tab
/// Open a properties-kind tab for the given relative path. Reuses an
/// existing properties tab for the same path; otherwise creates a new one.
function openPropertiesTab(rel: string): void {
  const key = appPageTabKey("properties", rel);
  if (openBuffers.has(key)) {
    activateTabInner(key);
    void propertiesPane.update(rel);
    return;
  }
  const buf: Buffer = {
    path: key,
    loadedText: "",
    token: null,
    kind: "properties",
    mode: { kind: "file" },
    pendingChangesMetadata: null,
    preview: false,
  };
  openBuffers.set(key, {
    buffer: buf,
    savedState: null,
    lastActivatedAt: bumpActivationCounter(),
  });
  activateTabInner(key);
  void propertiesPane.update(rel);
}

// status: chat-panel-expand-to-editor
/// Open an agent-kind tab for the given chat session. Creates one
/// tab per session; reopens the existing one if already open.
async function openAgentTab(sessionId: string): Promise<void> {
  const key = appPageTabKey("agent", sessionId);
  if (openBuffers.has(key)) {
    activateTabInner(key);
    return;
  }
  const buf: Buffer = {
    path: key,
    loadedText: "",
    token: null,
    kind: "agent",
    mode: { kind: "file" },
    pendingChangesMetadata: null,
    preview: false,
  };
  openBuffers.set(key, {
    buffer: buf,
    savedState: null,
    lastActivatedAt: bumpActivationCounter(),
  });
  activateTabInner(key);
}

// status: autosave-write-tick, autosave-tab-state-store, autosave-readonly-skipped
// Autosave coordinator. Tick + on-blur flush + per-path clear + tab-state
// debounce. Started inside `applyOpenedVault`; stopped on vault swap and
// on `onCloseRequested` after the dirty-buffer guard resolves.
const autosave: AutosaveApi = mountAutosave({
  editor,
  openBuffers,
  getBuffer: () => buffer,
  getActivePath: () => activePath,
  getPreviewTabPath: () => previewTabPath,
  isActiveDirty: () => isDirty(),
});

// status: autosave-tab-state-store
// Tab-state pushes are event-driven, not on the timer. Every
// `setBufferState` fires on tab open / close / activate / preview-slot
// change — exactly the four triggers the spec calls out — so a single
// store subscription covers all of them. Debounced ~250ms inside
// `autosave` so a burst (e.g. opening multiple files) collapses to one
// IPC call. Skipped while no vault is open (the autosave handle isn't
// running yet — `scheduleTabStatePush` is cheap but the resulting IPC
// would error).
bufferStore.subscribe(() => {
  if (!vaultIsOpen()) return;
  autosave.scheduleTabStatePush();
});

/// Thin host wrapper over `editor.setReadOnly` that also re-renders the
/// mode-controls toolbar slot. The `mode` argument used to drive
/// per-mode banners but `renderModeControls` now reads buffer state
/// directly; the parameter persists for legacy call-site clarity.
function setReadOnly(ro: boolean, _mode: "trash" | "snapshot" | "mutation" | null = null): void {
  editor.setReadOnly(ro);
  editorPane.modeControls.render();
}

// status: trail-add-to-active-from-editor-verb
// Editor toolbar pill: "Add to trail: <name>". Lives in the right-side
// toolbar cluster, just left of Save. The membership cache (installed
// below) drives the idempotency state; the pill subscribes to
// `activeTrailStore` / `bufferStore` / membership-cache changes for
// re-renders.
const addToTrailPill = mountAddToTrailPill({
  // status: trail-add-to-active-from-editor-verb — explicit panel +
  // membership refresh after the pill's `trailAppendWaypoint`
  // succeeds. Watcher is suppressed for the trail-doc + waypoint-note
  // paths during the append, so the `hiker:file-changed` refresh
  // path can't fire here. See
  // `bug-add-to-trail-verbs-dont-refresh-panel`.
  onAppended: () => {
    void trailsPanel?.api.refresh();
    void refreshActiveTrailWaypointPaths();
  },
});
addToTrailPill.setTrailDocPredicate((rel) => tree.api.isTrailDoc(rel));
installMembershipWatchers();

// status: editor-tab-strip
const tabStrip: TabStripApi = mountTabStrip({
  hostEl: dom.editor.tabStripEl,
  getTabs: () => tabSnapshots(),
  getActivePath: () =>
    activePath,
  onActivate: (path) => activateTabInner(path),
  onClose: (path) => void closeTab(path),
  onCloseOthers: (path) => {
    void (async () => {
      const others = [...openBuffers.keys()].filter((p) => p !== path);
      for (const p of others) {
        await closeTab(p);
        // closeTab may have aborted on Cancel — if the tab still exists,
        // bail out of the bulk operation.
        if (openBuffers.has(p)) return;
      }
    })();
  },
  onCloseToRight: (path) => {
    void (async () => {
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
  onRevealInTree: (path) => {
    void revealInTree(path);
  },
  // status: editor-preview-tab-promotion
  onPromote: (path) => promotePreviewByPath(path),
});

// status: write-note-review-surface
// Open a write-note-review session for a proposal: replaces the active
// buffer with a read-only view of the proposed content, with a diff
// toggle against current disk and Accept/Reject in the mode-controls
// slot. Accept blocks if any open buffer for the same target is dirty
// (per `write-note-review-blocks-on-dirty`).
async function openWriteNoteReview(proposal: Proposal): Promise<void> {
  // Per `patch-review.md` (Pane integration): "Write-note review entry
  // never blocks either. The user can be dirty *while reviewing*; accept
  // is what blocks (per `write-note-review-blocks-on-dirty`)."
  let contents: string;
  try {
    contents = await Ipc.stagingContent({ proposalId: proposal.id });
  } catch (err) {
    alert(`Failed to load proposal: ${formatError(err)}`);
    return;
  }
  // Determine whether the target file exists on disk.
  let isCreate = true;
  try {
    await Ipc.readFile({ rel: proposal.target_path });
    isCreate = false;
  } catch {
    isCreate = true;
  }
  // Persist the outgoing file-mode buffer's CM6 state before we clobber
  // the live editor. Otherwise any unsaved edits sitting in the editor
  // (not yet saved into `openBuffers[path].savedState`) vanish for the
  // duration of the review and never come back on exit/reject.
  if (buffer && buffer.mode.kind === "file") {
    const out = openBuffers.get(buffer.path);
    if (out) out.savedState = editor.getState();
  }
  setBufferState({ buffer: null, activePath: null, previewTabPath: null });
  editor.dispatch({
    changes: { from: 0, to: editor.getDocLength(), insert: contents },
    effects: [
      editor.language.reconfigure(editor.languageExtensionForPath(proposal.target_path)),
      editor.livePreviewCompartment.reconfigure(
        editor.livePreviewExtensionForPath(proposal.target_path),
      ),
    ],
  });
  if (vaultHome.isVisible()) vaultHome.setVisible(false);
  setBufferState({
    buffer: {
      path: proposal.target_path,
      loadedText: editor.getActiveText(),
      token: null,
      kind: "buffer",
      mode: {
        kind: "write-note-review",
        proposal_id: proposal.id,
        targetPath: proposal.target_path,
        diffActive: false,
        isCreate,
      },
      pendingChangesMetadata: null,
      preview: false,
    },
    activePath: proposal.target_path,
    previewTabPath: null,
  });
  editor.setReadOnly(true);
  updateStatus();
  checkpointNav();
}

function exitWriteNoteReview(): void {
  if (buffer?.mode.kind !== "write-note-review") return;
  const targetPath = buffer.mode.targetPath;
  editor.resetDiffDecorations();
  editor.setReadOnly(false);
  // If the user was looking at the target file (possibly with unsaved
  // edits) before entering review, restore that tab — `activateTab`
  // re-applies the saved CM6 state we stashed on entry.
  const priorEntry = openBuffers.get(targetPath);
  if (priorEntry && priorEntry.buffer.mode.kind === "file") {
    tabs.activateTab(targetPath);
    return;
  }
  setBufferState({ buffer: null, activePath: null, previewTabPath: null });
  editor.dispatch({
    changes: { from: 0, to: editor.getDocLength(), insert: "" },
  });
  updateStatus();
  checkpointNav();
}

async function toggleWriteNoteReviewDiff(): Promise<void> {
  const buf = buffer;
  if (!buf || buf.mode.kind !== "write-note-review") return;
  const mode = buf.mode;
  if (mode.diffActive) {
    editor.clearDiff(buf.loadedText);
    editor.dispatch({
      effects: [
        editor.livePreviewCompartment.reconfigure(
          editor.livePreviewExtensionForPath(mode.targetPath),
        ),
        editor.hideFrontmatterCompartment.reconfigure(
          hideFrontmatterEnabled ? hideFrontmatter() : [],
        ),
      ],
    });
    mode.diffActive = false;
    editorPane.modeControls.render();
    return;
  }
  let currentContent = "";
  try {
    currentContent = await Ipc.readFile({ rel: mode.targetPath });
  } catch {
    currentContent = ""; // new-note case
  }
  if (buf.mode.kind !== "write-note-review") return;
  editor.dispatch({
    effects: [
      editor.livePreviewCompartment.reconfigure([]),
      editor.hideFrontmatterCompartment.reconfigure([]),
    ],
  });
  await editor.renderDiff({
    before: { label: `${mode.targetPath} · current`, content: currentContent },
    after: { label: `${mode.targetPath} · proposed`, content: buf.loadedText },
  });
  mode.diffActive = true;
  editorPane.modeControls.render();
}

// status: write-note-review-surface
// status: write-note-review-mode-label
// status: write-note-review-blocks-on-dirty
editorPane.modeControls.register("write-note-review", (host) => {
  if (!buffer || buffer.mode.kind !== "write-note-review") return;
  const mode = buffer.mode;
  const proposal = pendingProposalsCache.find((p) => p.id === mode.proposal_id);
  const labelEl = document.createElement("span");
  labelEl.style.cssText = "margin-right:8px;font-size:12px;color:var(--fg-muted);";
  const base = mode.isCreate ? "Review new note" : "Review rewrite";
  // status: write-note-review-mode-label — surface origin suffix
  let origin = "";
  const surface = proposal?.surface ?? "";
  if (surface === "chat") origin = " · chat";
  else if (surface === "trails") origin = " · trail";
  else if (surface === "batch-mutation") origin = " · batch";
  const conflicted = proposal?.state === "conflicted";
  const conflictSuffix = conflicted
    ? ` · conflicted (${proposal?.conflict_reason ?? "unknown"})`
    : "";
  labelEl.textContent = base + origin + conflictSuffix;
  host.appendChild(labelEl);
  host.appendChild(
    iconButton({
      title: mode.diffActive ? "Hide diff" : "Show diff vs current",
      pressed: mode.diffActive,
      svg: Icons.diff(),
      onClick: () => toggleWriteNoteReviewDiff(),
    }),
  );
  const acceptBtn = iconButton({
    title: conflicted
      ? `Cannot accept: ${proposal?.conflict_reason ?? "conflicted"}`
      : "Accept",
    svg: Icons.check(),
    onClick: async () => {
      if (acceptBtn.disabled) return;
      await acceptHandler();
    },
  });
  acceptBtn.classList.add("mode-controls-accept");
  if (conflicted) acceptBtn.disabled = true;
  async function acceptHandler(): Promise<void> {
    // write-note-review-blocks-on-dirty: refuse if any open buffer for
    // the same target path is dirty — active or background.
    const targetPath = mode.targetPath;
    for (const [, entry] of openBuffers) {
      if (
        entry.buffer.path !== targetPath
        || entry.buffer.mode.kind !== "file"
      ) {
        continue;
      }
      const isActive = entry.buffer === buffer;
      const dirty = isActive
        ? editor.isDirty()
        : entry.savedState
          ? entry.buffer.loadedText !== entry.savedState.doc.toString()
          : false;
      if (dirty) {
        alert("Your buffer has unsaved changes. Save or revert before accepting this rewrite.");
        return;
      }
    }
    try {
      await Ipc.stagingAccept({ proposalId: mode.proposal_id });
      // No "Accepted" toast: per `patch-review.md`, the buffer reload
      // is the user-visible confirmation; a transient toast would be
      // redundant chrome. (`bug-write-note-review-redundant-exit-x`)
      await refreshPendingProposalsCache();
      exitWriteNoteReview();
      void openFile(targetPath, { preview: true });
    } catch (err) {
      alert("Accept failed: " + formatError(err));
    }
  }
  host.appendChild(acceptBtn);
  const rejectBtn = iconButton({
    title: "Reject",
    svg: Icons.cross(),
    onClick: async () => {
      if (!confirm("Reject this proposed change?")) return;
      try {
        await Ipc.stagingReject({ proposalId: mode.proposal_id });
        showToast("Rejected");
        await refreshPendingProposalsCache();
        exitWriteNoteReview();
      } catch (err) {
        alert("Reject failed: " + formatError(err));
      }
    },
  });
  rejectBtn.classList.add("mode-controls-reject");
  host.appendChild(rejectBtn);
  // No separate Exit verb in the slot: the agent-diff toolbar toggle
  // already serves as the exit affordance (per `patch-review.md:52`).
  // A second exit `X` next to the Reject `X` was redundant chrome.
  // (`bug-write-note-review-redundant-exit-x`)
});

// status: patch-review-mode-controls
editorPane.modeControls.register("patch-review", (host) => {
  if (buffer?.mode.kind !== "patch-review") return;
  const target = buffer.mode.targetPath;
  const proposals = pendingEditProposalsForPath(target);
  const applyable = proposals.filter((p) => p.state !== "conflicted");
  const conflicted = proposals.length - applyable.length;
  const labelEl = document.createElement("span");
  labelEl.style.cssText = "margin-right:8px;font-size:12px;color:var(--fg-muted);";
  const conflictSuffix = conflicted > 0 ? ` (${conflicted} conflicted)` : "";
  labelEl.textContent = `Review agent edits · ${applyable.length} hunks${conflictSuffix}`;
  host.appendChild(labelEl);
  const acceptAll = iconButton({
    title: `Accept all ${applyable.length} applyable hunks`,
    svg: Icons.check(),
    onClick: async () => {
      if (applyable.length === 0) return;
      if (applyable.length > 5 && !confirm(`Accept ${applyable.length} hunks?`)) return;
      for (const p of applyable) {
        await acceptPatchReviewHunk(p);
      }
    },
  });
  acceptAll.disabled = applyable.length === 0;
  acceptAll.classList.add("mode-controls-accept");
  host.appendChild(acceptAll);
  const rejectAll = iconButton({
    title: `Reject all ${proposals.length} hunks`,
    svg: Icons.cross(),
    onClick: async () => {
      if (proposals.length === 0) return;
      if (!confirm(`Reject ${proposals.length} hunks? Agent work will be discarded.`)) return;
      for (const p of proposals) {
        await rejectPatchReviewHunk(p);
      }
    },
  });
  rejectAll.disabled = proposals.length === 0;
  rejectAll.classList.add("mode-controls-reject");
  host.appendChild(rejectAll);
});

editorPane.modeControls.register("snapshot", (host) => {
  if (buffer?.mode.kind !== "snapshot") return;
  const row = buffer.mode.row;
  const diffActive = buffer.mode.diffActive;
  // status: mode-controls-diff-toggle
  // Hidden for `op = "deleted"` rows — there's no `before` blob to diff
  // against, so the toggle's affordance lies. Other rows always offer it.
  if (row && row.op !== "deleted") {
    host.appendChild(
      iconButton({
        title: diffActive ? "Hide diff" : "Show diff vs current",
        pressed: diffActive,
        svg: Icons.diff(),
        onClick: () => snapshotPreview.toggleDiff(),
      }),
    );
  }
  host.appendChild(
    iconButton({
      title: "Restore this version",
      svg: Icons.restore(),
      onClick: () => snapshotPreview.restore(),
    }),
  );
  host.appendChild(
    iconButton({
      title: "Close preview",
      svg: Icons.close(),
      onClick: () => snapshotPreview.close(),
    }),
  );
});

// status: note-mutation-buffer-ro-while-in-flight
// `#mode-controls` renderer for the regular `file` buffer state.
// The "Reformatting…" pill moved to the status-bar left region; the
// toolbar slot now holds only action buttons.
editorPane.modeControls.register("file", (_host) => {
  // Empty — file-mode chrome (Save / Diff / View / Mutations) lives
  // outside the centered mode-controls slot.
});

editorPane.modeControls.register("trash", (host) => {
  host.appendChild(
    iconButton({
      title: "Close preview",
      svg: Icons.close(),
      onClick: () => trash.api.closePreview(),
    }),
  );
});

// Watcher overflow toast; trash-changed listener lives inside the trash
// module now (it owns the cleanup of a previewed entry that vanished).
void listen("hiker:watcher-overflow", () => {
  showToast("Filesystem watcher fell behind — rescanning…");
});

interface LlmWarningPayload {
  kind: string;
  env?: string;
  message: string;
}
// status: llm-providers-config
// API-key preflight surface (per llm.md §Disable mode): the backend
// emits this on vault open when [llm].enabled = true and the configured
// api_key_env is unset, so the user sees the problem before they try to
// chat. Longer TTL than the default toast so the message is readable.
void listen<LlmWarningPayload>("hiker:llm-warning", (event) => {
  showToast(event.payload.message, undefined, 8000);
});

// External edits to either config.toml fire this event. Reload through
// the same applySettingsToUi path as vault open + set_setting writes so
// every surface that reflects a setting (View menu, tree sort, panels,
// chat) repaints from the live Config.
void listen<Settings>("hiker:config-reloaded", (event) => {
  applySettingsToUi(event.payload);
});

// ---------- watcher → editor integration ----------
// Reacts to external changes to the active buffer's file:
// - clean + modified → silent reload
// - dirty + modified → proactive conflict modal (Keep/Take/Cancel)
// - deleted (clean)  → close buffer + toast
// - deleted (dirty)  → keep buffer + toast ("save to recreate")
// - renamed          → buffer.path follows the new path silently
//
// status: watcher-editor-reload-clean
// status: watcher-editor-conflict-dirty
// status: watcher-editor-deleted-buffer
// status: watcher-editor-renamed-followup

type FileChangedEvent =
  | { kind: "created" | "modified" | "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string };

let watcherConflictPromptOpen = false;

void listen<FileChangedEvent>("hiker:file-changed", async (event) => {
  const ev = event.payload;
  // status: trails-mode-body — refresh the trails panel when a trail
  // doc or any `.hiker/trails/<id>/waypoints/` path is touched. Cheap
  // path-prefix check; the panel's internal epoch counter drops stale
  // fetches if the user is mid-refresh. Also refresh on any `.md`
  // change outside `.hiker/` — a non-active trail-doc could have been
  // created/renamed/deleted (acquiring or losing `hiker.kind: trail`
  // frontmatter), and the trails-list cache must reflect that or the
  // dropdown lists stale entries (and activating a deleted trail
  // errors). Mirrors the conservative posture used by the
  // `trail-row-icon` block below; trails-list is a small per-vault
  // query so the extra calls are acceptable.
  {
    const paths =
      ev.kind === "renamed" ? [ev.from, ev.to] : [ev.path];
    const activeTrailRel = activeTrailStore.get().rel;
    const looksLikeWaypoint = paths.some((p) =>
      p.startsWith(".hiker/trails/"),
    );
    const matchesActiveTrail =
      activeTrailRel !== null && paths.includes(activeTrailRel);
    const isMdOutsideHikerForTrails = paths.some(
      (p) => p.endsWith(".md") && !p.startsWith(".hiker/"),
    );
    if (looksLikeWaypoint || matchesActiveTrail || isMdOutsideHikerForTrails) {
      void trailsPanel?.api.refresh();
    }
    // status: trail-add-to-active-from-editor-verb — keep the
    // membership cache fresh so the editor pill and the tree verb
    // flip to "Already in this trail" without a manual refresh
    // when a new waypoint of the active trail lands (or the active
    // trail-doc itself changes shape via a frontmatter edit).
    // Conservative: any `.hiker/trails/` event refreshes (we don't
    // pre-narrow to the active trail's id since the cost is one
    // `trail_get` call); active-trail-doc edits also refresh.
    if (
      activeTrailRel !== null
      && (matchesActiveTrail || looksLikeWaypoint)
    ) {
      void refreshActiveTrailWaypointPaths();
    }
    // status: trail-row-icon — any `.md` change outside `.hiker/` may
    // have added or removed `hiker.kind: trail` frontmatter, so the
    // tree's cached trail-doc set is potentially stale. Conservative
    // refresh; cheap (single `trails_list` call) and only triggers a
    // tree repaint if the set actually changed.
    {
      const isMdOutsideHiker = paths.some(
        (p) => p.endsWith(".md") && !p.startsWith(".hiker/"),
      );
      const isTrailDoc = paths.some((p) => p.startsWith(".hiker/trails/"));
      if (isMdOutsideHiker || isTrailDoc) {
        void tree.api.refreshTrailDocSet();
      }
    }
  }
  // Tree shape changes don't depend on which buffer (if any) is active.
  // Schedule before buffer mutations so the rebuild reads the post-update
  // `buffer.path` (matters for the renamed branch's silent path follow).
  if (ev.kind === "created" || ev.kind === "deleted" || ev.kind === "renamed") {
    scheduleTreeRefreshFromWatcher();
    // status: vault-home-recent-modified — tree-shape changes can shift
    // which notes are in the top-N; modified-only events update mtimes.
    // External edits don't ride core::changes (deferred per `changes-write-path`
    // notes), so the watcher path keeps refreshing the recents widget directly
    // for that case. Internal saves are covered by `hiker:changes-appended` →
    // `refreshOnChangesAppended` upstream.
    vaultHome.api.notifyRecentModified();
  } else if (
    ev.kind === "modified"
    && (tree.api.getSortOrder() === "mtime-newest" || tree.api.getSortOrder() === "mtime-oldest")
  ) {
    // Tree *shape* doesn't change on Modified, but mtime-based sort orders
    // depend on per-entry mtime — a save reorders rows. Schedule a refresh
    // only when the chosen sort actually consumes mtime; under name sorts
    // we keep the existing no-op behavior.
    scheduleTreeRefreshFromWatcher();
  }
  if (ev.kind === "modified") {
    vaultHome.api.notifyRecentModified();
  }
  // Don't react while previewing a trash entry or a snapshot — both are
  // read-only views; mutating them would corrupt the user's intent. Trash
  // entries live under .hiker/trash/ which the watcher ignores anyway, but
  // snapshot previews share the live file path so this guard is the only
  // thing keeping a watcher event from clobbering the historic content.
  if (!buffer || isReadOnlyBuffer(buffer)) return;

  if (ev.kind === "modified" && ev.path === buffer.path) {
    if (isDirty()) {
      if (watcherConflictPromptOpen) return;
      watcherConflictPromptOpen = true;
      try {
        await openFileApi!.handleWatcherConflictDirty(buffer.path);
      } finally {
        watcherConflictPromptOpen = false;
      }
      return;
    }
    try {
      // Buffer is clean — silent reload via `open_for_edit` reseeds the
      // doc + rotates the token.
      const fresh = await Ipc.openForEdit({ rel: buffer.path });
      editor.dispatch({
        changes: { from: 0, to: editor.getDocLength(), insert: fresh.contents },
      });
      if (buffer) {
        buffer.loadedText = editor.getActiveText();
        buffer.token = fresh.token;
        updateStatus();
        scheduleChunkBoundariesRefresh(500);
      }
    } catch (err) {
      Logger.error("ui::app", "silent reload failed", { err });
    }
    return;
  }

  if (ev.kind === "deleted" && ev.path === buffer.path) {
    const path = buffer.path;
    if (isDirty()) {
      showToast(`${path} was removed on disk; save to recreate.`);
    } else {
      // status: editor-tab-strip — drop the tab for the removed path.
      openBuffers.delete(path);
      setBufferState({
        buffer: null,
        activePath: null,
        ...(previewTabPath === path ? { previewTabPath: null } : {}),
      });
      editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: "" } });
      updateStatus();
      tabStrip.render();
      // status: autosave-write-tick — clean buffer dropped, autosave
      // entry is no longer relevant.
      autosave.clearPath(path);
      showToast(`${path} was removed externally`);
    }
    return;
  }

  if (ev.kind === "renamed" && ev.from === buffer.path) {
    const oldPath = buffer.path;
    buffer.path = ev.to;
    updateStatus();
    // status: autosave-rename-clear-old — drop the autosave entry for
    // the old path; the next tick writes against the new path naturally.
    autosave.onRenamed(oldPath, ev.to);
    return;
  }
});

// status: note-properties-tab
// Read-only note inspector mounted into the `#properties-pane` div.
const propertiesPane: PropertiesPaneApi = mountPropertiesPane({
  containerEl: dom.propertiesPane.paneEl,
});

// status: chat-panel-expand-to-editor
// Expand button in the chat handle area — opens the active chat session
// as an agent-kind tab in the editor pane.
dom.chat.expandBtnEl.addEventListener("click", () => {
  const sid = chatPanel.getActiveSessionId!();
  if (sid) void openAgentTab(sid);
});

// Initial paint — every mount above is now in scope as a const, so
// `updateStatus()` reaches `mutationsMenu` / `dirtyBufferDiff` /
// `modeControls` / `tabStrip` directly without `?.` defensive guards.
updateStatus();

// Default-vault auto-open. Async; `applyOpenedVault` runs once the
// Tauri call resolves, by which point every closure it touches has
// initialized.
void vaultLifecycle.bootstrapDefaultVault();

} // end bootstrap()

void bootstrap();
