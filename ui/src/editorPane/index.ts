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
import { Ipc } from "../ipc";
import {
  mountMutationsMenu,
  type MutationsMenuApi,
} from "../mutations";
import type { CtxMenuItem } from "../widgets/contextMenu";
import { openMenuAtEvent } from "../widgets/contextMenu";
import type { Extension } from "@codemirror/state";
import { getBuffer, viewSettingsStore } from "../app/state";
import { dom } from "../app/dom";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export interface EditorPaneDeps {
  /// Drift-detected fall-through. The host (main.ts) owns the modal +
  /// reseed via `openFileApi`. Forwarded into `mountEditor`.
  handleDriftDetected: (
    rel: string,
    newText: string,
    extraMetadata: Record<string, unknown> | null,
  ) => Promise<boolean>;
  /// Save-time non-drift error fallthrough.
  handleSaveError: (err: unknown) => void;

  /// Keymap built from the host's keybind registry. Genuinely
  /// editor-specific config: the keymap must be threaded into the CM6
  /// extension list at view construction.
  keymap: Extension;

  /// Fired after a successful save — host wires into discovery refresh,
  /// chunk-boundary refresh, autosave clear.
  onAfterSave?: (savedPath: string | null, ok: boolean) => void;

  /// Post-status-paint fan-out. The host wires the updateStatus
  /// coordinator that pushes into preview-promotion, index-status
  /// rendering, tab-strip render, and renderActiveTab.
  onStatusPulse?: () => void;

  /// Mutations-menu host hook — fired when a path enters / leaves the
  /// in-flight mutation set. Host wires RO toggling + preview promotion.
  onMutationInFlightChanged?: (path: string, inFlight: boolean) => void;
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
    handleDriftDetected,
    handleSaveError,
    keymap,
    onAfterSave,
    onStatusPulse,
    onMutationInFlightChanged,
  } = deps;

  // DOM refs from the singleton — captured once at mount time. Status-bar
  // / toolbar elements live inside `dom().editor` and `dom().statusBar`.
  const domRefs = dom();
  const editorDom = domRefs.editor;
  const statusBarDom = domRefs.statusBar;

  // ---- Editor host (CM6 view + compartments + save/dirty/drift) ----

  const editor: EditorHost = mountEditor({
    parent: editorDom.editorEl,
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
    isReadOnlyBuffer: (b) => services.isReadOnlyBuffer(b),
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
    statusPathEl: statusBarDom.statusPathEl,
    statusCursorEl: statusBarDom.statusCursorEl,
    statusWordsEl: statusBarDom.statusWordsEl,
    saveBtn: editorDom.saveBtn,
    diffBtn: editorDom.diffBtn,
    treeEl: domRefs.tree.treeEl,
    editor,
    isReadOnlyBuffer: (b) => services.isReadOnlyBuffer(b),
    isDirtyBufferDiffActive: () => dirtyBufferDiff.isActive(),
    cssEscape: (s) => CSS.escape(s),
    onRevealInFileManager: (rel) => Ipc.revealInFileManager({ rel }),
    onSelectCurrent: (path) => services.openFile(path, { preview: false }),
    onSelectSnapshot: (row) => controllers.snapshotPreview.get().open(row),
    onSelectStaging: (proposal) =>
      controllers.patchReview.get().openProposalReview(proposal),
  });

  // ---- View menu ----

  const viewMenu: ViewMenuApi = mountViewMenu({
    editor,
    settings: controllers.settings.get(),
    syncToggleButtons: () =>
      controllers.sidebarMode.tryGet()?.syncToggleButtons(),
  });

  // ---- Mode controls (toolbar center slot) ----

  const modeControls: ModeControlsApi = mountModeControls({
    hostEl: editorDom.modeControlsEl,
    viewMenuBtn: editorDom.viewMenuBtn,
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
    formatError: (err) => services.formatError(err),
  });

  // ---- Mutations menu (wand button) ----
  //
  // Declared before `mountPatchReview` because mountPatchReview
  // synchronously dispatches an editor transaction during setup, which
  // fan-outs through the editor's `onAfterStatus` callback — that
  // callback closes over `mutationsMenu` and previously hit a TDZ
  // (`Cannot access 'mutationsMenu' before initialization`) when
  // bootstrap ran in this order.

  const mutationsMenu: MutationsMenuApi = mountMutationsMenu(
    {
      buttonEl: editorDom.mutationsMenuBtn,
      getBuffer: () => {
        const buf = getBuffer();
        if (!buf) return null;
        return { path: buf.path, mode: { kind: buf.mode.kind } };
      },
      getActiveBufferText: () => {
        const buf = getBuffer();
        if (!buf || services.isReadOnlyBuffer(buf)) return null;
        return editor.getActiveText();
      },
      formatError: (err) => services.formatError(err),
    },
    {
      onInFlightChanged: (path, inFlight) => {
        onMutationInFlightChanged?.(path, inFlight);
        modeControls.render();
      },
    },
  );

  // ---- Patch-review hunk decorations ----
  // status: patch-review-mode
  const patchReview: PatchReviewApi = mountPatchReview({
    dispatch: editor.dispatch,
    acceptHunk: (p) => controllers.patchReview.get().acceptPatchReviewHunk(p),
    rejectHunk: (p) => controllers.patchReview.get().rejectPatchReviewHunk(p),
  });

  // ---- Buffer-scoped toolbar wiring ----

  editorDom.saveBtn.addEventListener("click", async () => {
    const savedPath = getBuffer()?.path ?? null;
    const ok = await editor.save();
    onAfterSave?.(savedPath, ok);
  });

  editorDom.diffBtn.addEventListener("click", () => {
    if (editorDom.diffBtn.disabled) return;
    // status: patch-review-toggles-mutually-exclusive
    // Exit patch-review mode before activating the user-diff toggle —
    // both decorating the same buffer would conflict.
    const buf = getBuffer();
    if (buf?.mode.kind === "patch-review") {
      controllers.patchReview.tryGet()?.exitPatchReviewMode();
    }
    void dirtyBufferDiff.toggle();
  });
  editorDom.diffBtn.addEventListener("contextmenu", (ev) => {
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
    openMenuAtEvent(ev, items);
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
