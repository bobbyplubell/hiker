// status: note-mutations-menu
// status: note-mutations-menu-task-shape
// status: note-mutation-reformat-as-markdown
// status: note-mutation-one-in-flight-per-path
//
// Wand-icon top-bar button on the editor toolbar. Click opens a popover
// listing the mutations applicable to the active buffer. v1 ships with
// one entry: Reformat as markdown.
//
// The button's enable/disable rules come from a single piece of derived
// state maintained here:
//
// - `inFlight: Set<source_path>` — paths with a NoteMutation task in
//   `queued` or `leased` state. Updated from `hiker:queue-event`.
//
// On submit the module renders the popover and calls `submit_note_mutation`.
// The host (main.ts) owns the result-handling — `hiker:note-mutation-applied`
// is subscribed there so it can dispatch a single CM6 transaction into the
// active buffer (or hold the result in a `pendingMutations` map keyed by
// source_path when the buffer has been closed, per
// `note-mutation-pending-apply-toast`).

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Ipc } from "../ipc";
import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";

interface MutationFailedEvent {
  task_id: string;
  source_path: string;
  mutation: string;
  error: string;
}

interface QueueEventLite {
  event: string;
  id?: string;
  // Only `task_queued` carries `kind` (per `core::tasks::QueueEvent`).
  // We capture the source_path keyed by task id at queued time so the
  // terminal events (which only carry `id`) can clear in-flight state.
  kind?: { type?: string; source_path?: string };
}

interface BufferLike {
  path: string;
  mode: { kind: string };
}

export interface MutationsMenuDeps {
  buttonEl: HTMLButtonElement;
  /// Returns the active buffer (or null) so the menu can read path + mode.
  getBuffer: () => BufferLike | null;
  /// Returns the buffer's *live* text (live, not last-saved — same rule
  /// as `chat-active-note-context-injection`).
  getActiveBufferText: () => string | null;
  formatError: (err: unknown) => string;
}

export interface MutationsMenuApi {
  /// Re-evaluate the button's enabled state. Call after buffer swap or
  /// after the in-flight set changes.
  refreshButtonState(): void;
  /// True when an in-flight `NoteMutation` task targets `path`.
  isInFlight(path: string): boolean;
  /// Tear down event subscriptions.
  destroy(): Promise<void>;
}

export interface MutationsMenuHostHooks {
  /// Called when a `NoteMutation` task transitions to leased / completes
  /// / fails / cancels. Lets the host maintain its own
  /// `inFlightMutationPaths` set for the read-only-while-in-flight rule
  /// (`note-mutation-buffer-ro-while-in-flight`).
  onInFlightChanged(path: string, inFlight: boolean): void;
}

export function mountMutationsMenu(
  deps: MutationsMenuDeps,
  hooks: MutationsMenuHostHooks,
): MutationsMenuApi {
  const inFlight = new Set<string>();
  // Map from task_id → source_path for in-flight NoteMutation tasks.
  // Populated on `task_queued`; consulted on terminal events (which
  // don't carry `kind`) to find the right `inFlight` entry to clear.
  const taskIdToSourcePath = new Map<string, string>();
  let unlistenQueue: UnlistenFn | null = null;
  let unlistenFailed: UnlistenFn | null = null;

  const SUPPORTED_EXTS = new Set(["md", "markdown", "txt"]);

  function pathExtension(path: string): string {
    const i = path.lastIndexOf(".");
    if (i < 0) return "";
    return path.slice(i + 1).toLowerCase();
  }

  function describeDisableReason(): string | null {
    const buf = deps.getBuffer();
    if (!buf) return "Open a note to apply a mutation";
    if (buf.mode.kind !== "file") {
      return "Mutations not available in preview modes";
    }
    if (!SUPPORTED_EXTS.has(pathExtension(buf.path))) {
      return "Mutations apply to .md / .markdown / .txt files";
    }
    if ((deps.getActiveBufferText() ?? "").length === 0) {
      return "Note is empty";
    }
    if (inFlight.has(buf.path)) return "Mutation in progress…";
    return null;
  }

  function refreshButtonState(): void {
    // Spec says the button can be disabled with a tooltip, but a disabled
    // button is silent — clicks don't fire, so users have no way to see
    // *why* it's disabled. Keep the button itself clickable; the popover
    // items below carry the disable + explanation. Tooltip on the button
    // mirrors the would-be reason so hovering still surfaces it.
    const reason = describeDisableReason();
    deps.buttonEl.disabled = false;
    deps.buttonEl.title = reason ?? "Note mutations";
  }

  function isInFlight(path: string): boolean {
    return inFlight.has(path);
  }

  function buildItems(): CtxMenuItem[] {
    const buf = deps.getBuffer();
    const reason = describeDisableReason();
    const disabled = reason !== null;
    return [
      {
        label: "Reformat as markdown",
        disabled,
        tooltip: reason ?? undefined,
        run: () => {
          if (disabled || !buf) return;
          void submit("reformat-as-markdown", buf.path);
        },
      },
    ];
  }

  async function submit(mutation: string, rel: string): Promise<void> {
    const text = deps.getActiveBufferText();
    if (text === null) return;
    const ext = pathExtension(rel) || "md";
    try {
      await Ipc.submitNoteMutation({
        rel,
        mutation,
        sourceExtension: ext,
        content: text,
      });
      // Optimistically mark in-flight so the button greys out immediately;
      // the queue event will reaffirm.
      if (!inFlight.has(rel)) {
        inFlight.add(rel);
        hooks.onInFlightChanged(rel, true);
      }
      refreshButtonState();
    } catch (err) {
      showToast(`Mutation failed to submit: ${deps.formatError(err)}`, undefined, 6000);
    }
  }

  deps.buttonEl.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const rect = deps.buttonEl.getBoundingClientRect();
    openContextMenu(rect.left, rect.bottom + 2, buildItems(), deps.buttonEl);
  });

  void (async () => {
    unlistenQueue = await listen<QueueEventLite>("hiker:queue-event", (ev) => {
      const p = ev.payload;
      if (p.event === "task_queued") {
        if (!p.kind || p.kind.type !== "note_mutation" || !p.id) return;
        const sourcePath = p.kind.source_path;
        if (!sourcePath) return;
        taskIdToSourcePath.set(p.id, sourcePath);
        if (!inFlight.has(sourcePath)) {
          inFlight.add(sourcePath);
          hooks.onInFlightChanged(sourcePath, true);
        }
        refreshButtonState();
        return;
      }
      // Subsequent events: dispatch by tracked id only.
      if (!p.id) return;
      const sourcePath = taskIdToSourcePath.get(p.id);
      if (!sourcePath) return;
      if (
        p.event === "task_completed"
        || p.event === "task_failed"
        || p.event === "task_cancelled"
      ) {
        if (inFlight.delete(sourcePath)) {
          hooks.onInFlightChanged(sourcePath, false);
        }
        taskIdToSourcePath.delete(p.id);
        refreshButtonState();
      }
    });
    unlistenFailed = await listen<MutationFailedEvent>(
      "hiker:note-mutation-failed",
      (ev) => {
        showToast(
          `Mutation failed (${ev.payload.mutation}): ${ev.payload.error}`,
          undefined,
          8000,
        );
        refreshButtonState();
      },
    );
  })();

  refreshButtonState();

  return {
    refreshButtonState,
    isInFlight,
    destroy: async () => {
      unlistenQueue?.();
      unlistenFailed?.();
    },
  };
}
