/// Editor host — owns the CM6 `EditorView`, every per-feature compartment,
/// and the core editor lifecycle (save / dirty / read-only / language /
/// chunk-boundary refresh / status repaint).
///
/// Pre-refactor, ~15 modules reached into `main.ts` directly through a
/// shared `view: EditorView` dep + a soup of compartments + free functions
/// (`save`, `setReadOnly`, `isDirty`, `languageExtensionForPath`, etc.).
/// `EditorHost` collapses that surface into a single object that consumers
/// can take in their `deps`. The CM6 view itself is private — callers go
/// through `dispatch` / `getState` / `setLanguage` / `setReadOnly` / etc.
///
/// Diff helpers (`renderDiff` / `clearDiff` / `resetDiff`) are wrapped
/// here so consumers (snapshot preview / dirty-buffer diff / openFile)
/// don't need to import the `diff/` module *and* hold a CM6 view.
///
/// Step 1 of the main.ts refactor (see refactor notes). Tab management,
/// view-menu wiring, chat panel, vault-home and the rest stay put.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { EditorState, Compartment, type Extension } from "@codemirror/state";
import {
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  highlightWhitespace,
} from "@codemirror/view";
import { basicSetup } from "codemirror";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { livePreview } from "../editor/livePreview";
import {
  chunkBoundaries,
  chunkBoundariesHintState,
  chunkBoundsToState,
  clearChunkBoundariesState,
  setChunkBoundaries,
} from "../editor/chunkBoundaries";
import { hideFrontmatter } from "../editor/hideFrontmatter";
import {
  diffExtensions,
  resetDiffDecorations,
  renderDiff as renderDiffImpl,
  clearDiff as clearDiffImpl,
  type DiffInput,
} from "../diff";
import { viewSettingsStore, type Buffer } from "./state";

/// Public host the rest of the UI consumes. Methods are intentionally
/// minimal — anything callers used to do via `view.state.doc.toString()`
/// has a named accessor here so the CM6 view doesn't need to leak past
/// this module.
export interface EditorHost {
  /// Live document text. Equivalent to `view.state.doc.toString()`.
  getActiveText(): string;
  /// Document length in CM6 chars.
  getDocLength(): number;
  /// Current editor state — gives callers selection / sliceDoc / lineAt
  /// access without exposing the view itself.
  getState(): EditorState;
  /// CM6 transaction sink. Mirrors `view.dispatch`.
  dispatch: EditorView["dispatch"];
  /// Scroll container. Used by the dirty-buffer diff toggle to save and
  /// restore viewport.
  readonly scrollDOM: HTMLElement;
  /// CM6 dom node — exposed for class-toggle helpers (e.g.
  /// `hide-line-numbers`) that operate on the editor root.
  readonly dom: HTMLElement;
  /// Move keyboard focus to the editor.
  focus(): void;

  /// Reconfigure the language compartment for the buffer at `rel`.
  /// Markdown for `.md` / `.markdown` (and `.txt` when the user opted
  /// in via the View menu); empty extension otherwise.
  setLanguage(rel: string): void;
  /// Reconfigure the live-preview compartment for the buffer at `rel`.
  setLivePreview(rel: string): void;
  /// Toggle CM6 read-only.
  setReadOnly(ro: boolean): void;
  /// Toggle the hide-frontmatter compartment. Off → no extension;
  /// on → the hide-frontmatter view plugin.
  setHideFrontmatter(on: boolean): void;
  /// Toggle the chunk-boundaries compartment + refresh decorations.
  setChunkBoundariesEnabled(on: boolean): void;
  /// Toggle whitespace rendering compartment.
  setWhitespaceEnabled(on: boolean): void;
  /// Toggle word-wrap compartment.
  setWordWrapEnabled(on: boolean): void;
  /// Toggle line-number visibility (operates via the `hide-line-numbers`
  /// CSS class on the editor root, not a compartment swap, since
  /// `basicSetup`'s gutter wiring fights compartment-driven removal).
  setLineNumbersVisible(on: boolean): void;
  /// Session-scope override of `editor.render_txt_as_markdown`. Reconfigures
  /// the language + live-preview compartments for the *currently open*
  /// buffer so the user sees the change immediately; future opens pick
  /// the new mode up via `setLanguage` / `setLivePreview`.
  setRenderTxtAsMarkdown(on: boolean): void;
  /// Force live-preview-on flag (View menu's toggle). Current buffer's
  /// md-ness still gates whether the extension actually applies.
  setLivePreviewEnabled(on: boolean): void;

  /// Path-extension helpers — exposed because tab-management code
  /// in `app/tabs.ts` and the file-load path in `app/openFile.ts` need
  /// to reconfigure the same compartments directly during tab switches
  /// / file loads.
  languageExtensionForPath(rel: string): Extension;
  livePreviewExtensionForPath(rel: string): Extension;
  isMarkdownPath(rel: string): boolean;

  /// Compartments — still exposed for `app/openFile.ts`'s file-load
  /// dispatch (which bundles language / live-preview / read-only
  /// compartment reconfigures alongside a doc replace into a single
  /// transaction). Step 2 of the refactor absorbed the tab-switch
  /// caller via `applyTabSwitch` below; a future step can fold the
  /// file-load caller into a similar host-owned method and these
  /// escape hatches can go private.
  readonly language: Compartment;
  readonly livePreviewCompartment: Compartment;
  readonly hideFrontmatterCompartment: Compartment;
  readonly readOnlyCompartment: Compartment;

  /// Tab-switch compartment bundle. Reconfigures language /
  /// live-preview / hide-frontmatter / read-only compartments for
  /// the target tab in one effect list; returns the effects so the
  /// caller can pair them with a `view.setState` (via
  /// `applySavedState`) or fold them into its own dispatch when no
  /// saved state exists yet.
  tabSwitchEffects(opts: {
    rel: string;
    livePreviewEnabled: boolean;
    hideFrontmatterEnabled: boolean;
    readOnly: boolean;
  }): import("@codemirror/state").StateEffect<unknown>[];

  /// True when the active editable buffer's doc differs from
  /// `loadedText`. False for read-only previews and when no buffer
  /// is open.
  isDirty(): boolean;
  /// Save → `commit_buffer`. Returns true on a written outcome (or a
  /// successfully-resolved `KeepMine` drift). False on cancel /
  /// failure / no-buffer.
  save(): Promise<boolean>;
  /// Repaint the status bar / window title / dirty-tree-dot. Idempotent.
  updateStatus(): void;
  /// Repaint the diff-button enable / pressed state.
  refreshDiffButton(): void;
  /// True when the editor toolbar's diff button has a meaningful target
  /// (buffer is editable, on disk, and dirty). The button itself is
  /// rendered by main.ts; this accessor is the source of truth for
  /// "should it be enabled."
  diffButtonAvailable(): boolean;

  /// Refresh chunk-boundary decorations for the active buffer (immediately).
  refreshChunkBoundaries(): void;
  /// Debounced version. Used after save / file open to give the indexer
  /// a chance to compute the new bounds.
  scheduleChunkBoundariesRefresh(delayMs: number): void;

  /// Diff renderer adapters — pass-through to `diff/` so consumers
  /// (snapshot preview / dirty-buffer diff) don't have to thread the
  /// view through alongside the host.
  renderDiff(input: DiffInput): Promise<void>;
  clearDiff(plainText: string): void;
  resetDiffDecorations(): void;

  /// Tab-activation escape hatch. Restores a previously-captured CM6
  /// `EditorState` (selection / scroll / undo history) via
  /// `view.setState`, then dispatches a follow-up effect bundle to
  /// re-apply per-tab compartments. The two-step shape mirrors the
  /// pre-refactor code in main.ts; consolidating it here keeps the
  /// host's setState surface narrow.
  applySavedState(state: EditorState, effects: import("@codemirror/state").StateEffect<unknown>[]): void;
}

export interface EditorHostDeps {
  /// Where to mount the CM6 view.
  parent: HTMLElement;
  /// Live-buffer accessor. The host reads this for `isDirty` / `save`
  /// / status-repaint; main.ts owns the underlying `bufferStore`.
  getBuffer: () => Buffer | null;
  /// Setter for the few fields the host updates after a successful
  /// save (`loadedText`, `token`, `pendingChangesMetadata`). Keeps the
  /// store as the single source of truth.
  applyCommit: (patch: {
    loadedText: string;
    token: import("../ipc").BufferToken;
    pendingChangesMetadata: null;
  }) => void;
  /// Drift-detected fall-through. Host owns the modal + reseed.
  handleDriftDetected: (
    rel: string,
    newText: string,
    extraMetadata: Record<string, unknown> | null,
  ) => Promise<boolean>;
  /// Save-time non-drift error fallthrough.
  handleSaveError: (err: unknown) => void;
  /// True for any read-only preview buffer (trash / snapshot).
  isReadOnlyBuffer: (b: Buffer | null) => boolean;
  /// Side effects fired after every `updateStatus` call. Wired here
  /// because the post-paint cascade touches host-owned panels (tab
  /// strip, mode controls, mutations menu) that don't belong in the
  /// editor module proper.
  onAfterStatus: () => void;
  /// Keymap built from the host's keybind registry. Threaded as an
  /// extension so the host can register save / search / tab / nav
  /// bindings before the editor mounts.
  keymap: Extension;
}

export function mountEditor(deps: EditorHostDeps): EditorHost {
  const language = new Compartment();
  const readOnlyCompartment = new Compartment();
  const livePreviewCompartment = new Compartment();
  const chunkBoundariesCompartment = new Compartment();
  const hideFrontmatterCompartment = new Compartment();
  const whitespaceCompartment = new Compartment();
  const wordWrapCompartment = new Compartment();

  let chunkBoundariesRequestSeq = 0;
  let chunkBoundariesDebounce: number | null = null;

  function isMarkdownPath(rel: string): boolean {
    const ext = rel.split(".").pop()?.toLowerCase() ?? "";
    if (ext === "md" || ext === "markdown") return true;
    if (ext === "txt" && viewSettingsStore.get().renderTxtAsMarkdown) return true;
    return false;
  }
  function languageExtensionForPath(rel: string): Extension {
    return isMarkdownPath(rel) ? markdown({ base: markdownLanguage }) : [];
  }
  function livePreviewExtensionForPath(rel: string): Extension {
    if (!isMarkdownPath(rel)) return [];
    return viewSettingsStore.get().livePreviewEnabled ? livePreview() : [];
  }

  const statusUpdater = ViewPlugin.fromClass(
    class {
      update(u: ViewUpdate) {
        void u;
        updateStatus();
      }
    },
  );

  const view = new EditorView({
    parent: deps.parent,
    state: EditorState.create({
      doc: "",
      extensions: [
        basicSetup,
        wordWrapCompartment.of(EditorView.lineWrapping),
        language.of(markdown()),
        livePreviewCompartment.of(livePreview()),
        chunkBoundariesCompartment.of([]),
        hideFrontmatterCompartment.of([]),
        whitespaceCompartment.of([]),
        readOnlyCompartment.of(EditorState.readOnly.of(false)),
        ...diffExtensions(),
        statusUpdater,
        deps.keymap,
      ],
    }),
  });

  function isDirty(): boolean {
    const buffer = deps.getBuffer();
    if (!buffer || deps.isReadOnlyBuffer(buffer)) return false;
    return view.state.doc.toString() !== buffer.loadedText;
  }

  function diffButtonAvailable(): boolean {
    const buffer = deps.getBuffer();
    if (!buffer || buffer.mode.kind !== "file") return false;
    if (!buffer.token) return false;
    return isDirty();
  }

  function updateStatus(): void {
    deps.onAfterStatus();
  }

  function refreshDiffButton(): void {
    deps.onAfterStatus();
  }

  async function save(): Promise<boolean> {
    const buffer = deps.getBuffer();
    if (!buffer) return false;
    if (!buffer.token) return false;
    const contents = view.state.doc.toString();
    // status: note-mutation-stash-changes-tag — one-shot consume of the
    // stash on a successful save. Drift fall-through into the host's
    // resolve_drift consumes the same stash so a "Keep mine" still tags
    // the resulting row.
    const stash = buffer.pendingChangesMetadata;
    try {
      const outcome = await Ipc.commitBuffer({
        token: buffer.token,
        newText: contents,
        extraMetadata: stash,
      });
      if (outcome.kind === "written") {
        deps.applyCommit({
          loadedText: contents,
          token: outcome.token,
          pendingChangesMetadata: null,
        });
        updateStatus();
        return true;
      }
      // DriftDetected — host owns the modal + reseed.
      return await deps.handleDriftDetected(buffer.path, contents, stash);
    } catch (err) {
      deps.handleSaveError(err);
      return false;
    }
  }

  async function fetchAndApplyChunkBounds(rel: string): Promise<void> {
    const seq = ++chunkBoundariesRequestSeq;
    let state;
    try {
      const ix = await Ipc.indexStateFor({ rel });
      if (seq !== chunkBoundariesRequestSeq) return;
      if (ix.kind === "unsupported") {
        state = chunkBoundariesHintState("unsupported file type");
      } else if (ix.kind === "skipped") {
        state = chunkBoundariesHintState(`skipped — ${ix.reason}`);
      } else if (ix.kind === "queued") {
        state = chunkBoundariesHintState("queued for indexing");
      } else {
        const bounds = await Ipc.chunksFor({ rel });
        if (seq !== chunkBoundariesRequestSeq) return;
        state =
          bounds.length === 0
            ? chunkBoundariesHintState("no chunks yet")
            : chunkBoundsToState(view, bounds);
      }
    } catch (err) {
      Logger.error("ui::app", "chunk_boundaries fetch failed", { err });
      state = chunkBoundariesHintState("error loading chunks");
    }
    if (seq !== chunkBoundariesRequestSeq) return;
    view.dispatch({ effects: setChunkBoundaries.of(state) });
  }

  function refreshChunkBoundaries(): void {
    if (!viewSettingsStore.get().chunkBoundariesEnabled) {
      view.dispatch({
        effects: setChunkBoundaries.of(clearChunkBoundariesState()),
      });
      return;
    }
    const buffer = deps.getBuffer();
    const rel = buffer?.path;
    if (!rel || deps.isReadOnlyBuffer(buffer)) {
      view.dispatch({
        effects: setChunkBoundaries.of(clearChunkBoundariesState()),
      });
      return;
    }
    void fetchAndApplyChunkBounds(rel);
  }

  function scheduleChunkBoundariesRefresh(delayMs: number): void {
    if (chunkBoundariesDebounce !== null) {
      window.clearTimeout(chunkBoundariesDebounce);
    }
    chunkBoundariesDebounce = window.setTimeout(() => {
      chunkBoundariesDebounce = null;
      refreshChunkBoundaries();
    }, delayMs);
  }

  function setLanguage(rel: string): void {
    view.dispatch({
      effects: language.reconfigure(languageExtensionForPath(rel)),
    });
  }
  function setLivePreview(rel: string): void {
    view.dispatch({
      effects: livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
    });
  }
  function setReadOnly(ro: boolean): void {
    view.dispatch({
      effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(ro)),
    });
  }
  function setHideFrontmatter(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, hideFrontmatterEnabled: on }));
    view.dispatch({
      effects: hideFrontmatterCompartment.reconfigure(on ? hideFrontmatter() : []),
    });
  }
  function setChunkBoundariesEnabled(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, chunkBoundariesEnabled: on }));
    view.dispatch({
      effects: chunkBoundariesCompartment.reconfigure(on ? chunkBoundaries() : []),
    });
    refreshChunkBoundaries();
  }
  function setWhitespaceEnabled(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, whitespaceEnabled: on }));
    view.dispatch({
      effects: whitespaceCompartment.reconfigure(on ? highlightWhitespace() : []),
    });
  }
  function setWordWrapEnabled(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, wordWrapEnabled: on }));
    view.dispatch({
      effects: wordWrapCompartment.reconfigure(on ? EditorView.lineWrapping : []),
    });
  }
  function setLineNumbersVisible(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, lineNumbersVisible: on }));
    view.dom.classList.toggle("hide-line-numbers", !on);
  }
  function setRenderTxtAsMarkdown(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, renderTxtAsMarkdown: on }));
    const buffer = deps.getBuffer();
    const rel = buffer?.path;
    if (!rel) return;
    view.dispatch({
      effects: [
        language.reconfigure(languageExtensionForPath(rel)),
        livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
      ],
    });
  }
  function tabSwitchEffects(opts: {
    rel: string;
    livePreviewEnabled: boolean;
    hideFrontmatterEnabled: boolean;
    readOnly: boolean;
  }): import("@codemirror/state").StateEffect<unknown>[] {
    return [
      language.reconfigure(languageExtensionForPath(opts.rel)),
      livePreviewCompartment.reconfigure(
        opts.livePreviewEnabled ? livePreviewExtensionForPath(opts.rel) : [],
      ),
      hideFrontmatterCompartment.reconfigure(
        opts.hideFrontmatterEnabled ? hideFrontmatter() : [],
      ),
      readOnlyCompartment.reconfigure(EditorState.readOnly.of(opts.readOnly)),
    ];
  }

  function setLivePreviewEnabled(on: boolean): void {
    viewSettingsStore.update((s) => ({ ...s, livePreviewEnabled: on }));
    const buffer = deps.getBuffer();
    const rel = buffer?.path;
    if (!rel) return;
    view.dispatch({
      effects: livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
    });
  }

  return {
    getActiveText: () => view.state.doc.toString(),
    getDocLength: () => view.state.doc.length,
    getState: () => view.state,
    dispatch: view.dispatch.bind(view),
    get scrollDOM() {
      return view.scrollDOM;
    },
    get dom() {
      return view.dom;
    },
    focus: () => view.focus(),
    setLanguage,
    setLivePreview,
    setReadOnly,
    setHideFrontmatter,
    setChunkBoundariesEnabled,
    setWhitespaceEnabled,
    setWordWrapEnabled,
    setLineNumbersVisible,
    setRenderTxtAsMarkdown,
    setLivePreviewEnabled,
    languageExtensionForPath,
    livePreviewExtensionForPath,
    isMarkdownPath,
    language,
    livePreviewCompartment,
    hideFrontmatterCompartment,
    readOnlyCompartment,
    tabSwitchEffects,
    isDirty,
    save,
    updateStatus,
    refreshDiffButton,
    diffButtonAvailable,
    refreshChunkBoundaries,
    scheduleChunkBoundariesRefresh,
    renderDiff: (input) => renderDiffImpl(view, input),
    clearDiff: (plainText) => clearDiffImpl(view, plainText),
    resetDiffDecorations: () => resetDiffDecorations(view),
    applySavedState: (state, effects) => {
      view.setState(state);
      view.dispatch({ effects });
    },
  };
}
