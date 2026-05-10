/// Status bar paint — window title, `#status-path` (basename + tooltip
/// + reveal click + snapshot id badge), `#status-cursor` (line:col),
/// `#status-words` (word count), Save button enable, dirty-tree-dot
/// fan-out, and the diff button's enable / pressed state.
///
/// Pre-refactor, every paint rode through a 90-line `updateStatus()`
/// in `main.ts` that also dispatched into the mode controls / tab
/// strip / mutations menu / dirty-buffer-diff toggles. Step 5c of the
/// main.ts refactor splits the *pure status-bar paint* out into this
/// module; the secondary fan-outs (mode-controls render, tab-strip
/// render, mutations-menu enable, dirty-buffer-diff force-off,
/// preview-tab promotion) stay in a small `updateStatus()`
/// coordinator in `main.ts` because they're peer concerns that just
/// happened to share the same trigger.
///
/// The status-bar mount subscribes to `bufferStore` so it repaints on
/// every active-buffer transition automatically. CM6 doc / selection
/// updates still flow through the editor host's `onAfterStatus`
/// pulse — `repaint()` is called from there. The status-bar paint is
/// idempotent; multiple calls per tick are cheap.
///
/// status: status-bar-path-basename-tooltip, status-bar-path-reveal,
/// status-bar-layout, dirty-tree-dot, editor-tab-dirty-marker,
/// editor-diff-vs-disk-toggle (diff button enable), window-title
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { bufferStore, type Buffer } from "./state";
import type { EditorHost } from "./editor";

export interface StatusBarDeps {
  /// DOM refs for the status bar region. Captured via `domRefs.ts`.
  statusPathEl: HTMLElement;
  statusCursorEl: HTMLElement;
  statusWordsEl: HTMLElement;
  /// Save button — paint clears `disabled` when the active buffer is
  /// editable + dirty.
  saveBtn: HTMLButtonElement;
  /// Diff (vs on-disk) button — paint flips `disabled` / `.active` /
  /// tooltip based on the dirty-buffer diff toggle's state.
  diffBtn: HTMLButtonElement;
  /// Tree element — used for the dirty-tree-dot fan-out (only the
  /// active buffer's row gets the `.dirty` class).
  treeEl: HTMLElement;
  editor: EditorHost;
  isReadOnlyBuffer: (b: Buffer | null) => boolean;
  /// Dirty-buffer diff toggle's "is the editor currently showing the
  /// diff" predicate. The status-bar paint reads this for the diff
  /// button's `pressed` class.
  isDirtyBufferDiffActive: () => boolean;
  /// CSS.escape passthrough for the dirty-tree-dot per-row marker.
  cssEscape: (s: string) => string;
}

export interface StatusBarApi {
  /// Repaint everything the status-bar owns. Idempotent. Called from
  /// the editor host's `onAfterStatus` pulse and from the bufferStore
  /// subscriber on active-buffer swap.
  repaint: () => void;
}

export function mountStatusBar(deps: StatusBarDeps): StatusBarApi {
  const {
    statusPathEl,
    statusCursorEl,
    statusWordsEl,
    saveBtn,
    diffBtn,
    treeEl,
    editor,
    isReadOnlyBuffer,
    isDirtyBufferDiffActive,
    cssEscape,
  } = deps;

  function paintTitleAndPath(buffer: Buffer | null, dirty: boolean): void {
    const isTrash = buffer?.mode.kind === "trash";
    const isSnap = buffer?.mode.kind === "snapshot";
    const path =
      buffer?.mode.kind === "trash"
        ? buffer.mode.displayPath
        : (buffer?.path ?? "");
    const titleSuffix = isTrash ? " (in trash)" : isSnap ? " (snapshot)" : "";
    document.title =
      (dirty ? "• " : "") + (path ? `Hiker — ${path}${titleSuffix}` : "Hiker");
    // status: status-bar-path-basename-tooltip
    let basename = path ? (path.split("/").pop() ?? path) : "";
    if (isTrash) basename += " (in trash)";
    else if (isSnap) basename += " (snapshot)";
    statusPathEl.replaceChildren(document.createTextNode(basename));
    if (buffer?.mode.kind === "snapshot") {
      const idEl = document.createElement("span");
      idEl.className = "status-snapshot-id";
      idEl.textContent = `#${buffer.mode.changeId}`;
      idEl.title = `Snapshot id ${buffer.mode.changeId}`;
      statusPathEl.appendChild(idEl);
    }
    statusPathEl.title = isTrash ? buffer!.path : path;
    // status: status-bar-path-reveal — clickable when a real (non-trash)
    // file is open. Trash-preview paths live under `.hiker/trash/`;
    // revealing them would expose internal state. Snapshot previews
    // share the live file's path, so reveal stays sensible.
    const revealable = !!buffer && !isTrash;
    statusPathEl.classList.toggle("clickable", revealable);
    statusPathEl.style.cursor = revealable ? "pointer" : "";
  }

  function paintCursorAndWords(): void {
    const state = editor.getState();
    const sel = state.selection.main;
    const line = state.doc.lineAt(sel.head);
    const col = sel.head - line.from + 1;
    statusCursorEl.textContent = `${line.number}:${col}`;
    const text = editor.getActiveText();
    const words = text.trim() === "" ? 0 : text.trim().split(/\s+/).length;
    statusWordsEl.textContent = `${words} word${words === 1 ? "" : "s"}`;
  }

  function paintDirtyTreeDot(buffer: Buffer | null, dirty: boolean): void {
    // status: dirty-tree-dot
    // Only the *active* buffer's LI carries the dirty class. Drop it
    // from any other LIs first — switching tabs / opening another
    // note can leave a stale `.dirty` class on the outgoing tab's row
    // (the status repaint fires while `buffer` still points at the
    // outgoing tab, so `isDirty()` is briefly true against the live
    // doc containing the *target's* content vs the outgoing
    // `loadedText` — that one tick stamps the wrong row).
    treeEl.querySelectorAll("li.dirty").forEach((el) => {
      if (!buffer || el.getAttribute("data-path") !== buffer.path) {
        el.classList.remove("dirty");
      }
    });
    if (buffer) {
      const li = treeEl.querySelector(`li[data-path="${cssEscape(buffer.path)}"]`);
      li?.classList.toggle("dirty", dirty);
    }
  }

  function paintDiffButton(): void {
    const available = editor.diffButtonAvailable();
    const active = isDirtyBufferDiffActive();
    diffBtn.disabled = !available && !active;
    diffBtn.classList.toggle("active", active);
    diffBtn.title = active
      ? "Hide diff"
      : available
        ? "Diff vs on-disk"
        : "Nothing to diff";
  }

  function repaint(): void {
    const buffer = bufferStore.get().buffer;
    const dirty = editor.isDirty();
    paintTitleAndPath(buffer, dirty);
    paintCursorAndWords();
    saveBtn.disabled = !buffer || !dirty || isReadOnlyBuffer(buffer);
    paintDiffButton();
    paintDirtyTreeDot(buffer, dirty);
  }

  // Active-buffer swaps repaint automatically. CM6 doc / selection
  // pulses come through `editor.onAfterStatus` (wired host-side).
  bufferStore.subscribe(() => repaint());

  // status: status-bar-path-reveal — click on a non-trash buffer's
  // path opens its location in the OS file manager.
  statusPathEl.addEventListener("click", async () => {
    const buffer = bufferStore.get().buffer;
    if (!buffer || isReadOnlyBuffer(buffer)) return;
    try {
      await Ipc.revealInFileManager({ rel: buffer.path });
    } catch (err) {
      Logger.error("ui::app", "reveal_in_file_manager failed", { err });
    }
  });

  return { repaint };
}
