// status: tab-kinds
//
// Buffer-kind tab renderer. Owns the CM6 EditorView construction, the
// EditorHost wrapper, buffer-scoped toolbar wiring (Save / Diff / View /
// Mutations button click handlers), the bottom status bar, the
// mode-controls slot rendering for the regular `file` mode, and the
// save/drift/dirty pipeline. This is the peer of the other kind modules
// (vaultHome, queueDetail, settings, chat) — each exports a
// `mount<Kind>(deps): <Kind>Api` pattern.
//
// The EditorHost module (`ui/src/app/editor.ts`) is constructed here;
// status bar, mode controls, View menu, dirty-buffer diff, and mutations
// menu mounts all live inside this module. What stays in main.ts is tab
// coordination, app-page rendering, vault lifecycle, navigation history,
// sidebar/discovery wiring, autosave coordination, window-level keybinds,
// and event listeners.

import { mountEditor, type EditorHost } from "../app/editor";
import { mountStatusBar, type StatusBarApi } from "../app/statusBar";
import type { ChangeRow } from "../snapshotPreview";
import {
  mountModeControls,
  type ModeControlsApi,
} from "../modeControls";
import { mountViewMenu, type ViewMenuApi } from "../app/viewMenu";
import {
  mountDirtyBufferDiff,
  type DirtyBufferDiffApi,
} from "../dirtyBufferDiff";
import {
  mountPatchReview,
  type PatchReviewApi,
} from "../patchReview";
import type { Proposal } from "../ipc";
import {
  mountMutationsMenu,
  type MutationsMenuApi,
} from "../mutations";
import type { CtxMenuItem } from "../widgets/contextMenu";
import { openContextMenu } from "../widgets/contextMenu";
import type { Extension } from "@codemirror/state";
import type { Buffer } from "../app/state";
import { viewSettingsStore } from "../app/state";
import type { SettingsManager } from "../settings/manager";

export interface EditorPaneDeps {
  /// Where to mount the CM6 view (the `#editor` element).
  parentEl: HTMLElement;

  /// Editor toolbar DOM refs.
  saveBtn: HTMLButtonElement;
  diffBtn: HTMLButtonElement;
  modeControlsEl: HTMLElement;
  viewMenuBtn: HTMLButtonElement;
  mutationsMenuBtn: HTMLButtonElement;

  /// Status bar DOM refs.
  statusPathEl: HTMLElement;
  statusCursorEl: HTMLElement;
  statusWordsEl: HTMLElement;

  /// Tree element (for dirty-tree-dot fan-out in the status bar).
  treeEl: HTMLElement;

  /// Buffer state accessors — main.ts owns the underlying bufferStore.
  getBuffer: () => Buffer | null;
  setBufferState: (
    patch: Partial<{
      buffer: Buffer | null;
      activePath: string | null;
      previewTabPath: string | null;
    }>,
  ) => void;

  /// Drift-detected fall-through. The host (main.ts) owns the modal +
  /// reseed via `openFileApi`. Forwarded into `mountEditor`.
  handleDriftDetected: (
    rel: string,
    newText: string,
    extraMetadata: Record<string, unknown> | null,
  ) => Promise<boolean>;
  /// Save-time non-drift error fallthrough.
  handleSaveError: (err: unknown) => void;
  /// True for any read-only preview buffer (trash / snapshot) or
  /// non-buffer kind tab.
  isReadOnlyBuffer: (b: Buffer | null) => boolean;

  /// Keymap built from the host's keybind registry.
  keymap: Extension;

  /// Fired after a successful save — host wires into discovery refresh,
  /// chunk-boundary refresh, autosave clear.
  onAfterSave?: (savedPath: string | null, ok: boolean) => void;

  /// Post-status-paint fan-out. The host wires the updateStatus
  /// coordinator that pushes into preview-promotion, index-status
  /// rendering, tab-strip render, and renderActiveTab.
  onStatusPulse?: () => void;

  /// Paths with an in-flight NoteMutation task. The active buffer is RO
  /// while its path is in this set.
  inFlightMutationPaths: Set<string>;

  /// Settings manager (for View menu persistence).
  settings: SettingsManager;

  /// Sidebar / related panel toggle button sync.
  syncToggleButtons: () => void;

  /// CSS.escape passthrough (for the dirty-tree-dot per-row marker).
  cssEscape: (s: string) => string;

  /// Error formatting helper.
  formatError: (err: unknown) => string;

  /// Mutations-menu host hook — fired when a path enters / leaves the
  /// in-flight mutation set. Host wires RO toggling + preview promotion.
  onMutationInFlightChanged?: (path: string, inFlight: boolean) => void;

  /// status: status-bar-path-reveal — host wires the reveal-in-file-manager IPC.
  onRevealInFileManager: (rel: string) => Promise<void>;
  /// status: status-bar-version-dropdown-selection
  /// Selecting "Current" in the version dropdown re-opens the live editable
  /// file, exiting any snapshot / staging preview the buffer is in.
  onSelectCurrentVersion: (path: string) => void | Promise<void>;
  /// Selecting a snapshot row opens it read-only in the editor.
  onSelectSnapshotVersion: (row: ChangeRow) => void | Promise<void>;
  /// Selecting a staging proposal opens it as a read-only preview.
  onSelectStagingVersion: (proposal: { id: string; target_path: string }) => void | Promise<void>;

  /// status: patch-review-per-hunk-accept
  /// Per-hunk accept handler — host owns the transactional disk + buffer
  /// apply (per `patch-review-dirty-buffer-transactional-accept`).
  onPatchReviewAcceptHunk: (proposal: Proposal) => Promise<void>;
  /// status: patch-review-per-hunk-accept
  /// Per-hunk reject handler — host owns the staging-reject IPC.
  onPatchReviewRejectHunk: (proposal: Proposal) => Promise<void>;

  /// status: patch-review-toggles-mutually-exclusive
  /// Called by the user-diff toggle on activation so the host can exit
  /// patch-review mode first.
  onExitPatchReview?: () => void;
}

export interface EditorPaneApi {
  /// The EditorHost — consumable by tabs, openFile, autosave, snapshot,
  /// trash, agent-changes, and any other module that needs the CM6
  /// editor surface.
  host: EditorHost;

  /// Move keyboard focus to the editor.
  focus(): void;

  /// Trigger a save of the active buffer (delegates to EditorHost.save).
  save(): Promise<boolean>;

  /// True when the active editable buffer is dirty.
  isDirty(): boolean;

  /// Toggle CM6 read-only + re-render mode controls.
  setReadOnly(ro: boolean): void;

  /// Repaint the status bar.
  repaintStatusBar(): void;

  /// Refresh chunk-boundary decorations immediately.
  refreshChunkBoundaries(): void;

  /// Debounced chunk-boundary refresh.
  scheduleChunkBoundariesRefresh(delayMs: number): void;

  /// The mode-controls registry. Host uses `register()` to add
  /// per-mode renderers for snapshot, trash, and future modes.
  modeControls: ModeControlsApi;

  /// Dirty-buffer diff toggle state. Host reads `isActive()` for the
  /// status-bar diff-button paint.
  dirtyBufferDiff: DirtyBufferDiffApi;

  /// status: patch-review-mode
  /// Patch-review hunk decoration controller. Host calls `setProposals`
  /// to push the current `edit_note` proposal snapshot for the active
  /// file; empty list clears decorations.
  patchReview: PatchReviewApi;

  /// View menu item builder.
  viewMenu: ViewMenuApi;

  /// Mutations menu API (button-state refresh).
  mutationsMenu: MutationsMenuApi;
}

export function mountEditorPane(deps: EditorPaneDeps): EditorPaneApi {
  const {
    parentEl,
    saveBtn,
    diffBtn,
    modeControlsEl,
    viewMenuBtn,
    mutationsMenuBtn,
    statusPathEl,
    statusCursorEl,
    statusWordsEl,
    treeEl,
    getBuffer,
    handleDriftDetected,
    handleSaveError,
    isReadOnlyBuffer,
    keymap,
    onAfterSave,
    onStatusPulse,
    settings,
    syncToggleButtons,
    cssEscape,
    formatError,
    onMutationInFlightChanged,
    onRevealInFileManager,
    onSelectCurrentVersion,
    onSelectSnapshotVersion,
    onSelectStagingVersion,
    onPatchReviewAcceptHunk,
    onPatchReviewRejectHunk,
    onExitPatchReview,
  } = deps;

  // ---- Editor host (CM6 view + compartments + save/dirty/drift) ----

  const editor: EditorHost = mountEditor({
    parent: parentEl,
    getBuffer,
    applyCommit: ({ loadedText, token, pendingChangesMetadata }) => {
      const buf = getBuffer();
      if (!buf) return;
      buf.loadedText = loadedText;
      buf.token = token;
      buf.pendingChangesMetadata = pendingChangesMetadata;
    },
    handleDriftDetected,
    handleSaveError,
    isReadOnlyBuffer,
    onAfterStatus: () => {
      // Internal buffer-scoped fan-out.
      statusBar.repaint();
      mutationsMenu.refreshButtonState();
      modeControls.render();
      // Host fan-out: preview-promotion, index-status, dirty-buffer-diff
      // force-off, tab-strip render, renderActiveTab.
      onStatusPulse?.();
    },
    keymap,
  });

  // ---- Status bar (path / cursor / words / dirty-tree-dot) ----

  const statusBar: StatusBarApi = mountStatusBar({
    statusPathEl,
    statusCursorEl,
    statusWordsEl,
    saveBtn,
    diffBtn,
    treeEl,
    editor,
    isReadOnlyBuffer,
    isDirtyBufferDiffActive: () => dirtyBufferDiff.isActive(),
    cssEscape,
    onRevealInFileManager,
    onSelectCurrent: onSelectCurrentVersion,
    onSelectSnapshot: onSelectSnapshotVersion,
    onSelectStaging: onSelectStagingVersion,
  });

  // ---- View menu ----

  const viewMenu: ViewMenuApi = mountViewMenu({
    editor,
    settings,
    syncToggleButtons,
  });

  // ---- Mode controls (toolbar center slot) ----

  const modeControls: ModeControlsApi = mountModeControls({
    hostEl: modeControlsEl,
    viewMenuBtn,
    buildViewMenuItems: viewMenu.buildItems,
    getActiveMode: () => {
      const buf = getBuffer();
      return buf?.mode.kind ?? null;
    },
  });

  // ---- Dirty-buffer diff toggle ----

  const dirtyBufferDiff: DirtyBufferDiffApi = mountDirtyBufferDiff({
    editor,
    getBuffer: () => {
      const buf = getBuffer();
      if (!buf) return null;
      return {
        path: buf.path,
        loadedText: buf.loadedText,
        mode: { kind: buf.mode.kind },
      };
    },
    getHideFrontmatterEnabled: () =>
      viewSettingsStore.get().hideFrontmatterEnabled,
    renderModeControls: () => modeControls.render(),
    formatError,
  });

  // ---- Patch-review hunk decorations ----
  // status: patch-review-mode
  const patchReview: PatchReviewApi = mountPatchReview({
    dispatch: editor.dispatch,
    acceptHunk: onPatchReviewAcceptHunk,
    rejectHunk: onPatchReviewRejectHunk,
  });

  // ---- Mutations menu (wand button) ----

  const mutationsMenu: MutationsMenuApi = mountMutationsMenu(
    {
      buttonEl: mutationsMenuBtn,
      getBuffer: () => {
        const buf = getBuffer();
        if (!buf) return null;
        return { path: buf.path, mode: { kind: buf.mode.kind } };
      },
      getActiveBufferText: () => {
        const buf = getBuffer();
        if (!buf || isReadOnlyBuffer(buf)) return null;
        return editor.getActiveText();
      },
      formatError,
    },
    {
      onInFlightChanged: (path, inFlight) => {
        onMutationInFlightChanged?.(path, inFlight);
        modeControls.render();
      },
    },
  );

  // ---- Buffer-scoped toolbar wiring ----

  saveBtn.addEventListener("click", async () => {
    const savedPath = getBuffer()?.path ?? null;
    const ok = await editor.save();
    onAfterSave?.(savedPath, ok);
  });

  diffBtn.addEventListener("click", () => {
    if (diffBtn.disabled) return;
    // status: patch-review-toggles-mutually-exclusive
    // Exit patch-review mode before activating the user-diff toggle —
    // both decorating the same buffer would conflict.
    const buf = getBuffer();
    if (buf?.mode.kind === "patch-review") {
      onExitPatchReview?.();
    }
    void dirtyBufferDiff.toggle();
  });
  diffBtn.addEventListener("contextmenu", (ev) => {
    ev.preventDefault();
    const available = editor.diffButtonAvailable();
    const active = dirtyBufferDiff.isActive();
    const items: CtxMenuItem[] = [
      {
        label: active ? "Hide diff" : "Diff against on-disk",
        disabled: !available && !active,
        run: () => void dirtyBufferDiff.toggle(),
      },
    ];
    openContextMenu(ev.clientX, ev.clientY, items);
  });

  // ---- File-mode controls ----
  //
  // The "Reformatting…" pill that previously lived here moved to the
  // status-bar left region (see `status-bar-path-basename-tooltip` in
  // `statusBar.ts`). The mode-controls toolbar slot now holds only
  // action buttons so the toolbar stays compact.
  modeControls.register("file", (_host) => {
    // Empty — all file-mode chrome (Save, Diff, View, Mutations) lives
    // outside the centered mode-controls slot.
  });

  // ---- Read-only wrapper that also re-renders mode controls ----

  function setReadOnly(ro: boolean): void {
    editor.setReadOnly(ro);
    modeControls.render();
  }

  // ---- API ----

  return {
    host: editor,
    focus: () => editor.focus(),
    save: () => editor.save(),
    isDirty: () => editor.isDirty(),
    setReadOnly,
    repaintStatusBar: () => statusBar.repaint(),
    refreshChunkBoundaries: () => editor.refreshChunkBoundaries(),
    scheduleChunkBoundariesRefresh: (delayMs) =>
      editor.scheduleChunkBoundariesRefresh(delayMs),
    modeControls,
    dirtyBufferDiff,
    patchReview,
    viewMenu,
    mutationsMenu,
  };
}
