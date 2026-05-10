/// File-open + preview-promotion + drift-modal flow. Owns the orchestra-
/// tion of opening a vault-relative path into the editor (preview vs.
/// sticky tab), the at-most-one preview-slot replacement, the
/// preview→sticky promotion sites, and the disk-drift / watcher-conflict
/// modal flows. Hosted by `main.ts`; the underlying state (buffer +
/// open-buffers stores, CM6 view, panel APIs) stays in main.ts and
/// flows in via `deps`.
///
/// Module is a thin orchestration shell: no DOM lookups, no IPC seam
/// beyond what the host already owns; it composes host primitives
/// (open_for_edit, dispatch into CM6, set buffer state, render
/// tab strip).
import { Ipc, type DriftChoice } from "../ipc";
import { Logger } from "../logger";
import { confirm3 } from "../widgets/confirm";
import { EditorState } from "@codemirror/state";
import type { Buffer, OpenBufferEntry } from "./state";
import type { EditorHost } from "./editor";

export interface OpenFileDeps {
  editor: EditorHost;
  /// Buffer-state setter — atomic update of any subset of buffer-state
  /// fields. Implemented by host's `setBufferState` over `bufferStore`.
  setBufferState: (
    patch: Partial<{
      buffer: Buffer | null;
      activePath: string | null;
      previewTabPath: string | null;
    }>,
  ) => void;
  getBuffer: () => Buffer | null;
  getActivePath: () => string | null;
  getPreviewTabPath: () => string | null;
  /// Tab registry shared with main.ts; mutated in place via Map
  /// .set/.delete (see `bug-ui-state-in-mutable-closures` notes).
  openBuffers: Map<string, OpenBufferEntry>;
  bumpActivationCounter: () => number;
  inFlightMutationPaths: Set<string>;
  /// Activate a tab by path (host-owned because it touches every panel
  /// API — vaultHome / settingsPane visibility, tree reveal, tab-strip
  /// render, nav checkpoint, note-access tracking).
  activateTab: (rel: string) => void;
  /// Visibility / reveal hooks the open paths fire post-load.
  hideVaultHomeIfVisible: () => void;
  hideSettingsPaneIfVisible: () => Promise<void> | void;
  revealInTree: (rel: string) => Promise<void>;
  updateStatus: () => void;
  refreshChunkBoundaries: () => void;
  scheduleChunkBoundariesRefresh: (delayMs: number) => void;
  renderTabStrip: () => void;
  pruneNavTab: (rel: string) => void;
  checkpointNav: () => void;
  /// Re-render the editor view's content + the buffer state after
  /// `resolve_drift` returned `TookTheirs`. Owns the CM6 dispatch +
  /// `loadedText` + token swap; the host wires this so dispatch threads
  /// through the same view + buffer-state setter the rest of the app
  /// uses.
  applyTookTheirs: (rel: string, contents: string, token: unknown) => void;
}

export interface OpenFileApi {
  openFile: (rel: string, opts?: { preview?: boolean }) => Promise<void>;
  /// Flip the active preview tab to sticky. Idempotent — safe to call
  /// every doc-change tick. Re-renders the tab strip so the italic
  /// clears.
  promotePreviewIfActive: () => void;
  /// Promote a specific preview tab (by path) to sticky. Used by the
  /// double-click and "Keep open" tab-context-menu paths so the user
  /// can promote a tab they aren't actively editing.
  promotePreviewByPath: (rel: string) => void;
  /// Save-time disk-drift modal. Called by host's `save()` when
  /// `commit_buffer` returns `DriftDetected`. Renders the modal,
  /// dispatches to `resolve_drift` with the user's choice, and reseeds
  /// the buffer / view if the user chose `TakeTheirs`. Returns whether
  /// the save flow should report success to its caller (true → saved
  /// or take-theirs cleanly applied; false → user cancelled).
  handleDriftDetected: (
    rel: string,
    newText: string,
    extraMetadata: Record<string, unknown> | null,
  ) => Promise<boolean>;
  /// Save-time error fallthrough (non-drift IO failures bubble through
  /// here as today). Drift no longer rides this path.
  handleSaveError: (err: unknown) => void;
  /// Watcher-conflict modal — fired when an external write lands on the
  /// active dirty buffer. Owns the prompt + dispatches to `resolve_drift`
  /// for take-theirs / keep-mine; cancel leaves the buffer untouched
  /// (the next save's pre-write drift check will re-prompt).
  handleWatcherConflictDirty: (path: string) => Promise<void>;
}

export function mountOpenFile(deps: OpenFileDeps): OpenFileApi {
  // status: editor-preview-tab
  // Swap the currently-previewed tab's buffer in place. Same tab DOM
  // node, same activation order; only the path / contents / token
  // change. The previously-previewed buffer drops from `openBuffers`
  // under its old key (no other tab references it).
  async function replacePreviewWith(newRel: string): Promise<void> {
    const oldPath = deps.getPreviewTabPath()!;
    const file = await Ipc.openForEdit({ rel: newRel });
    const entry = deps.openBuffers.get(oldPath);
    if (!entry) {
      // Stale state — fall back to the normal open path.
      deps.setBufferState({ previewTabPath: null });
      return;
    }
    deps.editor.resetDiffDecorations();
    deps.setBufferState({ buffer: null });
    deps.editor.dispatch({
      changes: { from: 0, to: deps.editor.getDocLength(), insert: file.contents },
      effects: [
        deps.editor.language.reconfigure(deps.editor.languageExtensionForPath(newRel)),
        deps.editor.livePreviewCompartment.reconfigure(
          deps.editor.livePreviewExtensionForPath(newRel),
        ),
        deps.editor.readOnlyCompartment.reconfigure(
          EditorState.readOnly.of(deps.inFlightMutationPaths.has(newRel)),
        ),
      ],
    });
    const replaced: Buffer = {
      path: newRel,
      loadedText: deps.editor.getActiveText(),
      token: file.token,
      mode: { kind: "file" },
      pendingChangesMetadata: null,
      preview: true,
    };
    deps.openBuffers.delete(oldPath);
    deps.openBuffers.set(newRel, {
      buffer: replaced,
      // Discard the prior buffer's savedState — it belongs to a
      // different file's content and would clobber the new doc on
      // activation.
      savedState: null,
      lastActivatedAt: deps.bumpActivationCounter(),
    });
    deps.setBufferState({
      buffer: replaced,
      activePath: newRel,
      previewTabPath: newRel,
    });
    deps.hideVaultHomeIfVisible();
    void deps.hideSettingsPaneIfVisible();
    document
      .querySelectorAll("#tree li.active")
      .forEach((el) => el.classList.remove("active"));
    document
      .querySelectorAll(".trash-row.active")
      .forEach((el) => el.classList.remove("active"));
    await deps.revealInTree(newRel);
    deps.updateStatus();
    deps.refreshChunkBoundaries();
    deps.renderTabStrip();
    // status: navigation-history-stack — preview-replace prunes the
    // displaced path from history so back/forward never tries to revive
    // it.
    deps.pruneNavTab(oldPath);
    deps.checkpointNav();
    Ipc.noteAccessed({ rel: newRel }).catch((err) => {
      Logger.error("ui::app", "note_accessed failed", { err });
    });
  }

  // status: editor-preview-tab-promotion
  function promotePreviewIfActive(): void {
    const buffer = deps.getBuffer();
    if (!buffer || buffer.mode.kind !== "file" || !buffer.preview) return;
    buffer.preview = false;
    if (deps.getPreviewTabPath() === buffer.path) {
      deps.setBufferState({ previewTabPath: null });
    }
    deps.renderTabStrip();
  }

  // status: editor-preview-tab-promotion
  function promotePreviewByPath(rel: string): void {
    const entry = deps.openBuffers.get(rel);
    if (!entry || !entry.buffer.preview) return;
    entry.buffer.preview = false;
    if (deps.getPreviewTabPath() === rel) {
      deps.setBufferState({ previewTabPath: null });
    }
    deps.renderTabStrip();
  }

  async function openFile(
    rel: string,
    opts?: { preview?: boolean },
  ): Promise<void> {
    const wantPreview = opts?.preview === true;
    // status: multi-buffer-tree-click-switches-tab
    // If a tab for this path is already open, switch to it — never
    // reload from disk (would clobber any unsaved edits or in-buffer
    // mutation).
    if (deps.openBuffers.has(rel)) {
      deps.activateTab(rel);
      return;
    }
    // status: editor-preview-tab
    // If a preview tab is open and the caller wants the preview slot,
    // replace the existing preview's buffer in place rather than
    // spawning a new tab. The tab DOM node + tab key in `openBuffers`
    // persists (after a path remap) so the slot reads as the same tab
    // to the user. No dirty guard: preview tabs are never dirty by
    // construction.
    const previewTabPath = deps.getPreviewTabPath();
    if (wantPreview && previewTabPath !== null && previewTabPath !== rel) {
      try {
        await replacePreviewWith(rel);
      } catch (err) {
        Logger.error("ui::app", "openFile (preview replace) failed", { rel, err });
        alert(`open failed: ${err}`);
      }
      return;
    }
    // status: multi-buffer-no-switch-guard — no dirty-modal on tab
    // open. Switching tabs leaves the prior buffer dirty in memory.
    // The guard only fires on explicit close (× / Cmd-W) or
    // window-close.
    try {
      const file = await Ipc.openForEdit({ rel });
      // Persist outgoing tab's state before we dispatch into the view.
      const outgoing = deps.getBuffer();
      if (outgoing && outgoing.mode.kind === "file") {
        const out = deps.openBuffers.get(outgoing.path);
        if (out) out.savedState = deps.editor.getState();
      }
      deps.editor.resetDiffDecorations();
      deps.setBufferState({ buffer: null });
      deps.editor.dispatch({
        changes: { from: 0, to: deps.editor.getDocLength(), insert: file.contents },
        effects: [
          deps.editor.language.reconfigure(deps.editor.languageExtensionForPath(rel)),
          deps.editor.livePreviewCompartment.reconfigure(
            deps.editor.livePreviewExtensionForPath(rel),
          ),
          deps.editor.readOnlyCompartment.reconfigure(
            EditorState.readOnly.of(deps.inFlightMutationPaths.has(rel)),
          ),
        ],
      });
      // Compare against CM's canonical doc representation (CRLF
      // normalized), not the raw file string, or the buffer reads
      // dirty on open.
      const newBuf: Buffer = {
        path: rel,
        loadedText: deps.editor.getActiveText(),
        token: file.token,
        mode: { kind: "file" },
        pendingChangesMetadata: null,
        preview: wantPreview,
      };
      deps.openBuffers.set(rel, {
        buffer: newBuf,
        savedState: null,
        lastActivatedAt: deps.bumpActivationCounter(),
      });
      deps.setBufferState({
        buffer: newBuf,
        activePath: rel,
        ...(wantPreview ? { previewTabPath: rel } : {}),
      });
      deps.hideVaultHomeIfVisible();
      void deps.hideSettingsPaneIfVisible();
      document
        .querySelectorAll("#tree li.active")
        .forEach((el) => el.classList.remove("active"));
      document
        .querySelectorAll(".trash-row.active")
        .forEach((el) => el.classList.remove("active"));
      await deps.revealInTree(rel);
      deps.updateStatus();
      deps.refreshChunkBoundaries();
      deps.renderTabStrip();
      deps.checkpointNav();
      Ipc.noteAccessed({ rel }).catch((err) => {
        Logger.error("ui::app", "note_accessed failed", { err });
      });
    } catch (err) {
      Logger.error("ui::app", "openFile failed", { rel, err });
      alert(`open failed: ${err}`);
    }
  }

  // Map the modal's a/b/cancel result to a typed `DriftChoice` the core
  // op consumes. Modal copy + default focus stay here (presentation);
  // the policy itself rides through `core::ops::resolve_drift`.
  function choiceFromModal(c: "a" | "b" | "cancel"): DriftChoice {
    if (c === "a") return "keep_mine";
    if (c === "b") return "take_theirs";
    return "cancel";
  }

  async function handleDriftDetected(
    rel: string,
    newText: string,
    extraMetadata: Record<string, unknown> | null,
  ): Promise<boolean> {
    const choice = await confirm3(
      `${rel} has changed on disk since you opened it.`,
      "Keep mine (overwrite disk)",
      "Take theirs (discard my edits)",
      "Cancel",
    );
    const dispatch = choiceFromModal(choice);
    try {
      const result = await Ipc.resolveDrift({
        rel,
        choice: dispatch,
        newText,
        extraMetadata,
      });
      // Buffer may have closed / swapped while the modal was up. Apply
      // the resolution to the live buffer only if it still points at
      // the same path.
      const after = deps.getBuffer();
      if (!after || after.path !== rel) return false;
      if (result.kind === "written") {
        // KeepMine succeeded — rotate token, mirror loadedText.
        after.token = result.token;
        after.loadedText = newText;
        after.pendingChangesMetadata = null;
        deps.updateStatus();
        return true;
      }
      if (result.kind === "took_theirs") {
        deps.applyTookTheirs(rel, result.contents, result.token);
        // Discarded edits also discard any pending mutation tag — the
        // bytes now in the buffer are disk's, not the mutation's.
        const after2 = deps.getBuffer();
        if (after2) after2.pendingChangesMetadata = null;
        return true;
      }
      // Cancelled: leave the buffer dirty so the next save re-prompts.
      return false;
    } catch (err) {
      Logger.error("ui::app", "resolve_drift failed", { err });
      alert(`drift resolution failed: ${err}`);
      return false;
    }
  }

  function handleSaveError(err: unknown): void {
    Logger.error("ui::app", "save failed", { err });
    const e = err as { kind?: string; message?: unknown } | string;
    alert(`save failed: ${typeof e === "string" ? e : JSON.stringify(e)}`);
  }

  async function handleWatcherConflictDirty(path: string): Promise<void> {
    const choice = await confirm3(
      `${path} has been modified on disk while you have unsaved changes.`,
      "Keep mine",
      "Take theirs (reload from disk)",
      "Cancel",
    );
    // The buffer may have switched files (or closed) while the modal was
    // up.
    let buffer = deps.getBuffer();
    if (!buffer || buffer.path !== path) return;
    const dispatch = choiceFromModal(choice);
    if (dispatch === "cancel") return;
    // Route both branches through the typed core op so the conflict
    // path stays uniform (KeepMine = unconditional write + token; TakeTheirs
    // = read disk + token).
    try {
      const newText = deps.editor.getActiveText();
      const result = await Ipc.resolveDrift({
        rel: path,
        choice: dispatch,
        newText,
        extraMetadata: buffer.pendingChangesMetadata,
      });
      buffer = deps.getBuffer();
      if (!buffer || buffer.path !== path) return;
      if (result.kind === "written") {
        buffer.token = result.token;
        buffer.loadedText = newText;
        buffer.pendingChangesMetadata = null;
        deps.updateStatus();
        deps.scheduleChunkBoundariesRefresh(500);
        return;
      }
      if (result.kind === "took_theirs") {
        deps.applyTookTheirs(path, result.contents, result.token);
        const after = deps.getBuffer();
        if (after) after.pendingChangesMetadata = null;
        deps.scheduleChunkBoundariesRefresh(500);
        return;
      }
      // Cancelled — fall through.
    } catch (err) {
      Logger.error("ui::app", "watcher conflict resolve failed", { err });
    }
  }

  return {
    openFile,
    promotePreviewIfActive,
    promotePreviewByPath,
    handleDriftDetected,
    handleSaveError,
    handleWatcherConflictDirty,
  };
}
