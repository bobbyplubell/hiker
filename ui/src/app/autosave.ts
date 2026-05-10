/// Autosave tick + recovery + tab-state coordinator. See docs/autosave.md.
///
/// Frontend's role per the spec: fire a 5s timer when any tab is dirty,
/// push every dirty buffer's current text to `autosave_write`, clear
/// entries for buffers that became clean, flush on window blur, push
/// debounced tab-state snapshots on tab open/close/activate/preview-slot
/// changes, and on vault open auto-restore recovered buffers as sticky
/// tabs (no modal — the dirty-marker on each restored tab is the cue
/// the user can save or revert via the normal surfaces). Backend
/// (`core::autosave`) owns storage, GC, and the hash-vs-disk filtering
/// inside `recover()`.
//
// status: autosave-write-tick
// status: autosave-readonly-skipped
// status: autosave-rename-clear-old
// status: autosave-tab-state-store
// status: autosave-tab-state-silent-restore

import { Ipc, type AutosaveTabState } from "../ipc";
import { Logger } from "../logger";
import type { Buffer, OpenBufferEntry } from "./state";
import type { EditorHost } from "./editor";

const TICK_INTERVAL_MS = 5_000;
const TAB_STATE_DEBOUNCE_MS = 250;

export interface AutosaveDeps {
  editor: EditorHost;
  /// Tab registry shared with main.ts. Mutated in place; we read on
  /// every tick.
  openBuffers: Map<string, OpenBufferEntry>;
  getBuffer: () => Buffer | null;
  getActivePath: () => string | null;
  getPreviewTabPath: () => string | null;
  /// True when the active editable buffer's text differs from its
  /// loadedText. Inactive tabs compute dirty from their `savedState`
  /// (matches the same-shape enumeration in `onCloseRequested`).
  isActiveDirty: () => boolean;
}

export interface AutosaveApi {
  /// Start the per-vault tick loop. Idempotent — calling twice
  /// without `stop()` between is a no-op.
  start: () => void;
  /// Stop the tick + cancel any pending tab-state debounce. Called on
  /// vault swap / window close before the new vault's autosave starts.
  stop: () => void;
  /// Notify the autosave layer that a buffer's path was renamed
  /// externally. Clears the old path's sidecar; the next tick writes
  /// the new path naturally. status: autosave-rename-clear-old
  onRenamed: (oldPath: string, _newPath: string) => void;
  /// Notify on save success / tab close — the autosave entry for the
  /// path is no longer relevant (matches saved on disk, or buffer
  /// dropped). Idempotent.
  clearPath: (path: string) => void;
  /// Explicit immediate flush of every dirty buffer. Window-blur fires
  /// this; spec calls for an extra immediate tick on focus loss to
  /// shorten the worst-case data-loss window.
  flushAllNow: () => void;
  /// Awaitable flush — same shape as `flushAllNow` but waits for every
  /// in-flight `autosave_write` to settle. Used by the window-close
  /// path so dirty buffers land on disk before `win.destroy()`.
  flushAllAndWait: () => Promise<void>;
  /// Schedule a debounced tab-state push. Call from open / close /
  /// activate / preview-slot change.
  scheduleTabStatePush: () => void;
  /// Cancel any pending tab-state debounce and push the current snapshot
  /// synchronously. Used by the close path so the next launch's tab
  /// restore matches what was open at exit.
  pushTabStateNow: () => Promise<void>;
  /// Discard a recovered entry's sidecar (called after auto-restore so
  /// the now-in-memory buffer's stale sidecar doesn't resurface).
  discard: (path: string) => Promise<void>;
}

export function mountAutosave(deps: AutosaveDeps): AutosaveApi {
  let tickHandle: ReturnType<typeof setInterval> | null = null;
  let blurHandler: (() => void) | null = null;
  let tabStateDebounce: ReturnType<typeof setTimeout> | null = null;
  /// Last text we wrote per path, so we don't re-push identical content.
  /// Cleared on `clearPath` so the entry is re-asserted after a save.
  const lastWritten: Map<string, string> = new Map();

  function isAutosavablePath(path: string): boolean {
    // status: autosave-readonly-skipped — trash / snapshot previews
    // have no own tab in `openBuffers`; the only way an entry sits in
    // the map is via the `file` mode buffer flow. The check is here
    // for the active-buffer branch below where the user might be
    // looking at a read-only preview while a file buffer is also open.
    const entry = deps.openBuffers.get(path);
    if (!entry) return false;
    return entry.buffer.mode.kind === "file";
  }

  function activeBufferTextFor(path: string): string | null {
    const buf = deps.getBuffer();
    if (
      buf &&
      buf.mode.kind === "file" &&
      buf.path === path &&
      deps.getActivePath() === path
    ) {
      return deps.editor.getActiveText();
    }
    const entry = deps.openBuffers.get(path);
    if (!entry || entry.buffer.mode.kind !== "file") return null;
    // Inactive tab: the saved CM6 state (captured at last
    // tab-deactivate) is the freshest text we have for it.
    return entry.savedState ? entry.savedState.doc.toString() : null;
  }

  function isPathDirty(path: string): boolean {
    const entry = deps.openBuffers.get(path);
    if (!entry || entry.buffer.mode.kind !== "file") return false;
    if (path === deps.getActivePath()) {
      const buf = deps.getBuffer();
      if (buf && buf.mode.kind === "file" && buf.path === path) {
        return deps.isActiveDirty();
      }
    }
    if (!entry.savedState) return false;
    return entry.savedState.doc.toString() !== entry.buffer.loadedText;
  }

  function tick(): Promise<void> {
    let anyDirty = false;
    const writes: Promise<void>[] = [];
    for (const path of deps.openBuffers.keys()) {
      if (!isAutosavablePath(path)) continue;
      const dirty = isPathDirty(path);
      if (dirty) {
        anyDirty = true;
        const text = activeBufferTextFor(path);
        if (text === null) continue;
        if (lastWritten.get(path) === text) continue;
        lastWritten.set(path, text);
        writes.push(
          Ipc.autosaveWrite({
            path,
            contents: text,
          }).catch((err) => {
            Logger.error("ui::app", "autosave_write failed", { path, err });
          }),
        );
      } else if (lastWritten.has(path)) {
        // Transitioned dirty → clean since the prior tick. Drop the
        // sidecar so it can't resurface as a false-positive recovery.
        clearPath(path);
      }
    }
    if (!anyDirty) {
      // status: autosave-write-tick — suspend the timer when nothing is
      // dirty. Reactivated on the next dirty-transition via `start()`
      // being called again, or via the next manual `flushAllNow`.
      stopTick();
    }
    return Promise.all(writes).then(() => undefined);
  }

  function startTick(): void {
    if (tickHandle !== null) return;
    tickHandle = setInterval(() => tick(), TICK_INTERVAL_MS);
  }

  function stopTick(): void {
    if (tickHandle !== null) {
      clearInterval(tickHandle);
      tickHandle = null;
    }
  }

  function flushAllNow(): void {
    // Run a synchronous `tick()` so on-blur gets the immediate write
    // the spec calls for, then make sure the timer is running so the
    // 5s cadence resumes after the blur tick.
    void tick();
    startTick();
  }

  async function flushAllAndWait(): Promise<void> {
    await tick();
  }

  function clearPath(path: string): void {
    lastWritten.delete(path);
    Ipc.autosaveClear({ path }).catch((err) => {
      Logger.error("ui::app", "autosave_clear failed", { path, err });
    });
  }

  function onRenamed(oldPath: string, _newPath: string): void {
    clearPath(oldPath);
  }

  function buildTabState(): AutosaveTabState {
    const open_paths: string[] = [];
    for (const [path, entry] of deps.openBuffers) {
      if (entry.buffer.mode.kind !== "file") continue;
      open_paths.push(path);
    }
    const activePath = deps.getActivePath();
    const buf = deps.getBuffer();
    const active_path =
      buf && buf.mode.kind === "file" && activePath ? activePath : null;
    const preview_path = deps.getPreviewTabPath();
    return {
      open_paths,
      active_path,
      preview_path,
      saved_at_ms: 0,
    };
  }

  function scheduleTabStatePush(): void {
    if (tabStateDebounce !== null) clearTimeout(tabStateDebounce);
    tabStateDebounce = setTimeout(() => {
      tabStateDebounce = null;
      const statePayload = buildTabState();
      Ipc.autosaveSaveTabState({ statePayload }).catch((err) => {
        Logger.error("ui::app", "autosave_save_tab_state failed", {
          err,
        });
      });
    }, TAB_STATE_DEBOUNCE_MS);
  }

  async function pushTabStateNow(): Promise<void> {
    if (tabStateDebounce !== null) {
      clearTimeout(tabStateDebounce);
      tabStateDebounce = null;
    }
    const statePayload = buildTabState();
    try {
      await Ipc.autosaveSaveTabState({ statePayload });
    } catch (err) {
      Logger.error("ui::app", "autosave_save_tab_state failed", { err });
    }
  }

  function start(): void {
    if (blurHandler !== null) return;
    blurHandler = () => flushAllNow();
    window.addEventListener("blur", blurHandler);
    startTick();
  }

  function stop(): void {
    stopTick();
    if (blurHandler !== null) {
      window.removeEventListener("blur", blurHandler);
      blurHandler = null;
    }
    if (tabStateDebounce !== null) {
      clearTimeout(tabStateDebounce);
      tabStateDebounce = null;
    }
    lastWritten.clear();
  }

  async function discard(path: string): Promise<void> {
    lastWritten.delete(path);
    await Ipc.autosaveDiscard({ path });
  }

  return {
    start,
    stop,
    onRenamed,
    clearPath,
    flushAllNow,
    flushAllAndWait,
    scheduleTabStatePush,
    pushTabStateNow,
    discard,
  };
}
