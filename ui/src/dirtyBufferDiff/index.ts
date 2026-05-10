// status: editor-diff-vs-disk-toggle
//
// Diff toggle for any dirty editable buffer. Lives in the editor toolbar's
// `#mode-controls` slot when `buffer.mode.kind === "file"` AND `isDirty()`.
// Mirrors `snapshotPreview/`'s diff-toggle pattern: reconfigures the live-
// preview + hide-frontmatter compartments to no-ops while the diff is
// active so the synthesized diff doc isn't re-decorated as markdown (per
// `diff.md`'s "Markdown-rendering coupling" note), then restores them on
// toggle-off.
//
// Saves selection + viewport on toggle-on so toggling back returns the
// user's editing context — flipping to diff isn't destructive to live
// editor state.

import { Ipc } from "../ipc";
import { hideFrontmatter } from "../editor/hideFrontmatter";
import type { EditorHost } from "../app/editor";

interface BufferLike {
  path: string;
  loadedText: string;
  mode: { kind: string };
}

interface SavedViewState {
  selection: { anchor: number; head: number };
  scrollTop: number;
}

export interface DirtyBufferDiffDeps {
  editor: EditorHost;
  getBuffer: () => BufferLike | null;
  getHideFrontmatterEnabled: () => boolean;
  renderModeControls: () => void;
  formatError: (err: unknown) => string;
}

export interface DirtyBufferDiffApi {
  /// True while the dirty-buffer Diff toggle is on for the active buffer.
  isActive(): boolean;
  /// Flip the toggle.
  toggle(): Promise<void>;
  /// Force the toggle off (e.g. when the buffer becomes clean, or on
  /// buffer swap). Idempotent.
  forceOff(): void;
}

export function mountDirtyBufferDiff(deps: DirtyBufferDiffDeps): DirtyBufferDiffApi {
  let active = false;
  let inFlight = false;
  // Saved live-buffer state so toggling back to editing returns the user
  // to where they were. Captured at toggle-on; consumed at toggle-off.
  let saved: { content: string; view: SavedViewState } | null = null;

  function captureViewState(): SavedViewState {
    const sel = deps.editor.getState().selection.main;
    return {
      selection: { anchor: sel.anchor, head: sel.head },
      scrollTop: deps.editor.scrollDOM.scrollTop,
    };
  }

  function restoreViewState(s: SavedViewState): void {
    const docLen = deps.editor.getDocLength();
    const anchor = Math.min(s.selection.anchor, docLen);
    const head = Math.min(s.selection.head, docLen);
    deps.editor.dispatch({ selection: { anchor, head } });
    deps.editor.scrollDOM.scrollTop = s.scrollTop;
  }

  async function toggle(): Promise<void> {
    if (inFlight) return;
    const buffer = deps.getBuffer();
    if (!buffer || buffer.mode.kind !== "file") return;
    inFlight = true;
    try {
      if (active) {
        // Off: restore the live editable buffer + selection + viewport,
        // restore markdown compartments, clear RO.
        const restore = saved;
        deps.editor.clearDiff(restore?.content ?? buffer.loadedText);
        deps.editor.dispatch({
          effects: [
            deps.editor.livePreviewCompartment.reconfigure(
              deps.editor.livePreviewExtensionForPath(buffer.path),
            ),
            deps.editor.hideFrontmatterCompartment.reconfigure(
              deps.getHideFrontmatterEnabled() ? hideFrontmatter() : [],
            ),
          ],
        });
        if (restore) restoreViewState(restore.view);
        saved = null;
        active = false;
        deps.editor.setReadOnly(false);
        deps.renderModeControls();
        deps.editor.refreshChunkBoundaries();
        return;
      }
      // On: snapshot the live buffer state, compute the diff, render.
      const liveContent = deps.editor.getActiveText();
      saved = { content: liveContent, view: captureViewState() };
      let result: unknown;
      try {
        result = await Ipc.computeDiff({
          before: buffer.loadedText,
          after: liveContent,
        });
      } catch (err) {
        alert(`could not compute diff: ${deps.formatError(err)}`);
        saved = null;
        return;
      }
      // The active buffer must still be the same file.
      const after = deps.getBuffer();
      if (!after || after.mode.kind !== "file" || after.path !== buffer.path) {
        saved = null;
        return;
      }
      deps.editor.dispatch({
        effects: [
          deps.editor.livePreviewCompartment.reconfigure([]),
          deps.editor.hideFrontmatterCompartment.reconfigure([]),
        ],
      });
      // `renderDiff` accepts the full `DiffInput` (re-running compute) or,
      // when invoked with raw inputs, computes its own diff. Mirror the
      // snapshot-preview call shape: the renderer here drives the doc
      // dispatch and decoration application via the IPC result it has
      // already in hand.
      void result; // computed for parity with snapshot's IPC call
      await deps.editor.renderDiff({
        before: {
          label: `${buffer.path} · disk`,
          content: buffer.loadedText,
        },
        after: {
          label: `${buffer.path} · buffer`,
          content: liveContent,
        },
      });
      deps.editor.setReadOnly(true);
      active = true;
      deps.renderModeControls();
      deps.editor.refreshChunkBoundaries();
    } finally {
      inFlight = false;
    }
  }

  function forceOff(): void {
    if (!active) return;
    const buffer = deps.getBuffer();
    const restore = saved;
    deps.editor.clearDiff(restore?.content ?? buffer?.loadedText ?? "");
    if (buffer && buffer.mode.kind === "file") {
      deps.editor.dispatch({
        effects: [
          deps.editor.livePreviewCompartment.reconfigure(
            deps.editor.livePreviewExtensionForPath(buffer.path),
          ),
          deps.editor.hideFrontmatterCompartment.reconfigure(
            deps.getHideFrontmatterEnabled() ? hideFrontmatter() : [],
          ),
        ],
      });
    }
    saved = null;
    active = false;
    deps.editor.setReadOnly(false);
    deps.renderModeControls();
    deps.editor.refreshChunkBoundaries();
  }

  return {
    isActive: () => active,
    toggle,
    forceOff,
  };
}
