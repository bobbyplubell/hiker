/// Tab-coordination glue. Owns activate / cycle / jump / close /
/// snapshot for the tab strip + editor pane, plus the
/// preview-promotion shims that route through `openFile`.
///
/// The underlying state — `bufferStore`, `tabStore.openBuffers`,
/// `viewSettingsStore`, `inFlightMutationsStore` — lives in
/// `./state.ts` and main.ts (the `Buffer` registry stays where it
/// is; this module only moves the *coordination* logic, not the
/// data model).
///
/// Step 2 of the main.ts refactor. Pairs with `./editor.ts` (step
/// 1) — the multi-compartment effect bundle that activate-tab
/// previously dispatched directly is now `EditorHost.tabSwitchEffects`,
/// keeping the four per-feature compartments encapsulated in the
/// host.
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { confirm3 } from "../widgets/confirm";
import type { Buffer, OpenBufferEntry } from "./state";
import type { EditorHost } from "./editor";
import type { OpenFileApi } from "./openFile";

export interface TabSnapshot {
  path: string;
  basename: string;
  folder: string;
  dirty: boolean;
  preview: boolean;
  // status: tab-kinds
  kind: string;
}

export interface TabsDeps {
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
  /// Set of paths with an in-flight mutation; the active buffer is
  /// RO while its path is in this set.
  inFlightMutationPaths: Set<string>;

  /// View-toggle reads — the tab-switch compartment bundle re-applies
  /// the user's *current* preferences rather than the snapshotted
  /// ones, so the values are read fresh on every activation.
  getLivePreviewEnabled: () => boolean;
  getHideFrontmatterEnabled: () => boolean;

  /// Lazy `OpenFileApi` accessor — `openFile` mounts after the tabs
  /// module in main.ts (it depends on `activateTab`), so we read it
  /// at call time rather than in deps.
  getOpenFileApi: () => OpenFileApi | null;

  /// Host callback: open or activate a `home`-kind tab. Called when
  /// the last buffer tab is closed or a `home-detail` tab is closed
  /// with no home-overview tab open.
  onShowHome?: () => void;

  /// Save the active buffer (used by the close-dirty modal).
  save: () => Promise<boolean>;
  /// True when the active buffer is dirty.
  isDirty: () => boolean;

  revealInTree: (rel: string) => Promise<void> | void;
  updateStatus: () => void;
  refreshChunkBoundaries: () => void;
  renderTabStrip: () => void;
  pruneNavTab: (rel: string) => void;
  checkpointNav: () => void;
  /// Toggle CM6 read-only after a tab close clears the editor.
  setReadOnly: (ro: boolean) => void;
}

export interface TabsApi {
  /// Activate (focus) a tab by path. Persists the outgoing tab's CM6
  /// state, restores or seeds the target's content, re-applies the
  /// per-tab compartment bundle, and fires every post-activate side
  /// effect (status repaint, tree reveal, nav checkpoint, note-access
  /// tracking).
  activateTab: (rel: string) => void;
  /// Close a tab. Fires the dirty-buffer confirm3 modal if needed.
  closeTab: (rel: string) => Promise<void>;
  /// Cycle ±1 through the tab order (Ctrl-Tab / Ctrl-Shift-Tab).
  cycleTab: (delta: 1 | -1) => void;
  /// Jump to the Nth tab (Cmd/Ctrl-1..9; 9 jumps to last).
  jumpToTab: (n: number) => void;
  /// Snapshot every tab for the strip renderer.
  tabSnapshots: () => TabSnapshot[];
  /// Promote the active preview tab to sticky. Idempotent — safe to
  /// call every doc-change tick.
  promotePreviewIfActive: () => void;
  /// Promote a specific preview tab (by path) to sticky. Used by
  /// double-click and "Keep open" tab-context-menu paths.
  promotePreviewByPath: (rel: string) => void;
}

export function mountTabs(deps: TabsDeps): TabsApi {
  // status: multi-buffer-tree-click-switches-tab
  // status: editor-tab-strip
  // Tab activation. Saves the previously-active file buffer's CM6 state
  // (so undo history / selection / scroll persist across tab switches)
  // then restores the target buffer's content + state. If `target` has no
  // saved state yet (freshly opened, never switched away from), we
  // dispatch its loadedText into the live state instead of `setState`.
  function activateTab(rel: string): void {
    const target = deps.openBuffers.get(rel);
    if (!target) return;
    const ed = deps.editor;
    const buffer = deps.getBuffer();
    // Persist the outgoing tab's state.
    if (buffer && buffer.mode.kind === "file") {
      const out = deps.openBuffers.get(buffer.path);
      if (out) out.savedState = ed.getState();
    }
    ed.resetDiffDecorations();
    // status: tab-kinds — only buffer-kind tabs carry CM6 state (save /
    // restore / dispatch). Non-buffer tabs (home, queue, settings, agent,
    // graph, properties) skip the CM6 dance entirely — their renderers
    // manage their own DOM independently.
    if (target.buffer.kind === "buffer") {
      // Clear the outgoing buffer reference *before* the doc dispatch so
      // CM6's synchronous update listeners (notably `statusUpdater` →
      // `updateStatus()` → `isDirty()`) don't compare the incoming doc
      // against the outgoing buffer's `loadedText`. Without this, the
      // mid-switch dirty-check sees a doc/loadedText mismatch (the doc
      // already swapped, the buffer pointer hasn't yet) and fires
      // `promotePreviewIfActive()` against the outgoing preview tab —
      // demoting it to sticky on every tab switch, contradicting the
      // promotion-paths rule in `editor-preview-tab-promotion`.
      deps.setBufferState({ buffer: null });
      // Build the per-tab compartment effect list once via the host's
      // `tabSwitchEffects`. setState restores compartments to whatever the
      // target's saved state had; we re-apply path-dependent + global
      // compartments so toggles (live preview, hide-frontmatter, word wrap,
      // etc.) reflect the user's *current* preferences rather than the
      // snapshotted ones.
      const tabEffects = ed.tabSwitchEffects({
        rel: target.buffer.path,
        livePreviewEnabled: deps.getLivePreviewEnabled(),
        hideFrontmatterEnabled: deps.getHideFrontmatterEnabled(),
        readOnly: deps.inFlightMutationPaths.has(target.buffer.path),
      });
      if (target.savedState) {
        // CM6's `view.setState` is the only path that restores undo history /
        // selection / scroll; `applySavedState` wraps it + the follow-up
        // compartment-reapply dispatch.
        ed.applySavedState(target.savedState, tabEffects);
      } else {
        // First activation — dispatch loadedText into the existing state.
        ed.dispatch({
          changes: { from: 0, to: ed.getDocLength(), insert: target.buffer.loadedText },
          effects: tabEffects,
        });
        // The dispatch normalized loadedText through CM's doc — re-read so
        // isDirty() doesn't immediately flag the buffer dirty after open.
        target.buffer.loadedText = ed.getActiveText();
      }
    }
    deps.setBufferState({ buffer: target.buffer, activePath: rel });
    target.lastActivatedAt = deps.bumpActivationCounter();
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
    // status: tab-kinds — chunkBoundaries refresh and reveal-in-tree
    // are buffer-only concepts. Non-buffer tabs have no CM6 view, no
    // tree path, and no note-access tracking.
    if (target.buffer.kind === "buffer") {
      void deps.revealInTree(rel);
      deps.refreshChunkBoundaries();
      // status: note-access-tracking
      Ipc.noteAccessed({ rel }).catch((err) => {
        Logger.error("ui::app", "note_accessed failed", { err });
      });
    }
    deps.updateStatus();
    deps.renderTabStrip();
    deps.checkpointNav();
  }

  // status: editor-tab-strip
  // Close a tab. If dirty, fires the existing close-time confirm3 modal
  // (file-switch-guard-dirty's surviving entry point per multi-buffer-no-
  // switch-guard). On confirmed close, removes the entry from the
  // registry; if it was active, activates the most-recently-used remaining
  // tab (or clears the editor when none remain).
  async function closeTab(rel: string): Promise<void> {
    const entry = deps.openBuffers.get(rel);
    if (!entry) return;
    const isActive = deps.getActivePath() === rel;
    // status: tab-kinds — only buffer-kind tabs carry a dirty guard.
    // Non-buffer tabs close without prompting.
    const isBufferKind = entry.buffer.kind === "buffer";
    const dirty = isBufferKind &&
      (isActive
        ? deps.isDirty()
        : entry.buffer.loadedText !==
          (entry.savedState?.doc.toString() ?? entry.buffer.loadedText));
    if (dirty) {
      // We need the dirty buffer's edits visible in the modal context —
      // the user wants to see what they're saving/discarding. If it's
      // not the active tab, switch to it first so the editor shows the
      // pending content while the modal is up.
      if (!isActive) activateTab(rel);
      const choice = await confirm3(
        `${rel} has unsaved changes.`,
        "Save & close",
        "Discard & close",
        "Cancel",
      );
      if (choice === "cancel") return;
      if (choice === "a") {
        const ok = await deps.save();
        if (!ok) return;
      }
    }
    deps.openBuffers.delete(rel);
    // status: editor-preview-tab — clear the slot if we just closed it.
    if (deps.getPreviewTabPath() === rel) deps.setBufferState({ previewTabPath: null });
    // status: tab-kinds — when closing a home-detail tab, return to
    // the home overview (same as browser "back from detail to overview").
    if (entry.buffer.kind === "home-detail") {
      if (!deps.openBuffers.has("__hiker:home")) {
        deps.onShowHome?.();
      }
    }
    // status: navigation-history-stack — drop history entries pointing at
    // the closed tab so back/forward never tries to revive a vanished buffer.
    deps.pruneNavTab(rel);
    if (deps.getActivePath() === rel) {
      // Pick the most-recently-used remaining tab.
      let next: string | null = null;
      let bestSeen = -1;
      for (const [p, e] of deps.openBuffers) {
        if (e.lastActivatedAt > bestSeen) {
          bestSeen = e.lastActivatedAt;
          next = p;
        }
      }
      if (next) {
        activateTab(next);
      } else {
        // No tabs left — open the home page.
        deps.onShowHome?.();
      }
    }
    deps.renderTabStrip();
    deps.checkpointNav();
  }

  // status: editor-tab-keybinds
  function cycleTab(delta: 1 | -1): void {
    const order = [...deps.openBuffers.keys()];
    if (order.length === 0) return;
    const activePath = deps.getActivePath();
    const idx = activePath ? order.indexOf(activePath) : -1;
    const next =
      idx < 0
        ? order[0]
        : order[(idx + delta + order.length) % order.length];
    activateTab(next);
  }

  function jumpToTab(n: number): void {
    const order = [...deps.openBuffers.keys()];
    if (order.length === 0) return;
    // Cmd/Ctrl-9 jumps to the last tab regardless of count (browser
    // convention) per editor-tab-keybinds.
    const idx = n === 9 ? order.length - 1 : Math.min(n - 1, order.length - 1);
    if (idx < 0) return;
    activateTab(order[idx]);
  }

  function tabSnapshots(): TabSnapshot[] {
    const out: TabSnapshot[] = [];
    const buffer = deps.getBuffer();
    const activePath = deps.getActivePath();
    for (const [path, entry] of deps.openBuffers) {
      const slash = path.lastIndexOf("/");
      let basename = slash >= 0 ? path.slice(slash + 1) : path;
      // status: tab-kinds — produce human-readable labels for page-kind
      // tabs so the strip says "Home" / "Queue" / etc. instead of the
      // internal `__hiker:*` sentinel key.
      if (path.startsWith("__hiker:home") && !path.includes("detail")) {
        basename = "Home";
      } else if (path.startsWith("__hiker:home-detail")) {
        basename = "Recent activity";
      } else if (path.startsWith("__hiker:queue")) {
        basename = "Queue";
      } else if (path.startsWith("__hiker:settings")) {
        basename = "Settings";
      } else if (path.startsWith("__hiker:agent")) {
        basename = "Chat";
      } else if (path.startsWith("__hiker:properties")) {
        basename = path.replace(/^__hiker:properties:/, "");
        basename = basename.slice(basename.lastIndexOf("/") + 1);
      }
      const folder = slash >= 0 ? path.slice(0, slash) : "";
      const isActive = path === activePath && buffer?.mode.kind === "file";
      // status: tab-kinds — only buffer-kind tabs carry a dirty marker;
      // non-buffer tabs (home, queue, settings, agent, graph, properties)
      // have no dirty concept.
      const isBufferKind = entry.buffer.kind === "buffer";
      const dirty = isBufferKind &&
        (isActive
          ? deps.isDirty()
          : entry.savedState !== null
            ? entry.savedState.doc.toString() !== entry.buffer.loadedText
            : false);
      out.push({
        path,
        basename,
        folder,
        dirty,
        preview: entry.buffer.preview,
        kind: entry.buffer.kind,
      });
    }
    return out;
  }

  // Preview-promotion shims — the actual implementation lives in
  // `./openFile.ts`. Keeping the re-exports here lets main.ts wire
  // them through the tabs module instead of holding duplicate shims.
  function promotePreviewIfActive(): void {
    deps.getOpenFileApi()?.promotePreviewIfActive();
  }
  function promotePreviewByPath(rel: string): void {
    deps.getOpenFileApi()?.promotePreviewByPath(rel);
  }

  return {
    activateTab,
    closeTab,
    cycleTab,
    jumpToTab,
    tabSnapshots,
    promotePreviewIfActive,
    promotePreviewByPath,
  };
}
