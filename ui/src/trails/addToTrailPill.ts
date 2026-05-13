// status: trail-add-to-active-from-editor-verb
//
// Editor toolbar pill: "Add to trail: <name>". Per `docs/trails.md`'s
// "Building a trail while reading" section, when a regular note is
// open in the editor and a trail is active, a small pill near the
// `#mode-controls` slot appends the open note as a root-level
// waypoint. Doubles as the always-visible active-trail indicator
// outside Trails mode (the trail name lives in the pill).
//
// Visibility — shown iff ALL of:
//   - active trail is set
//   - the open buffer is a regular file-mode buffer (not trash /
//     snapshot / read-only previews)
//   - the buffer's path isn't itself a trail-doc or a waypoint-note
//     (under `.hiker/trails/`)
//   - the buffer's path is `.md` / `.txt` (indexable extension)
//
// Idempotency — when shown, immediately consults the membership
// cache (`./membership`); disables the pill + tooltip "Already in
// this trail" when the path is already a waypoint at any depth.
//
// Click — calls `Ipc.trailAppendWaypoint` directly (NOT
// `captureToActiveTrail`, which swallows errors per its capture-flow
// contract; the editor verb is user-explicit and needs error
// visibility, mirroring the tree-verb pattern).

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { activeTrailStore, bufferStore, getActiveTrailRel } from "../app/state";
import type { Buffer } from "../app/state";
import { showToast } from "../widgets/toast";
import {
  getActiveTrailWaypointPaths,
  subscribeMembership,
} from "./membership";

const INDEXABLE_EXTS = [".md", ".txt"];

export interface AddToTrailPillApi {
  /// Force a re-render. The pill self-renders on store changes; the
  /// host calls this only after explicit refreshes (e.g. a
  /// post-write idempotency flip the watcher path will already
  /// drive).
  render(): void;
  /// Provide a synchronous "is this path a trail-doc?" predicate.
  /// The pill hides itself for trail-doc buffers (a trail can't be
  /// a waypoint of itself). Set by the host once the tree's
  /// `trailDocPaths` cache is reachable.
  setTrailDocPredicate(fn: (path: string) => boolean): void;
}

export function mountAddToTrailPill(opts: {
  /// Fired after a successful `trailAppendWaypoint` from the pill
  /// click. The host uses this to explicitly refresh the trails
  /// panel + membership cache, because `core::trails::append_waypoint`
  /// suppresses the watcher for the trail-doc + waypoint-note paths
  /// (to avoid indexer feedback loops), so the
  /// `hiker:file-changed`-driven refresh path can't fire for these
  /// writes. See `bug-add-to-trail-verbs-dont-refresh-panel`.
  onAppended?: () => void;
}): AddToTrailPillApi {
  const existing = document.getElementById(
    "add-to-trail-pill",
  ) as HTMLButtonElement | null;
  if (!existing) {
    throw new Error("#add-to-trail-pill element not found in DOM");
  }
  const btn = existing;

  function isIndexablePath(path: string): boolean {
    const lower = path.toLowerCase();
    return INDEXABLE_EXTS.some((ext) => lower.endsWith(ext));
  }

  function isWaypointPath(path: string): boolean {
    return path.startsWith(".hiker/trails/");
  }

  // The pill must hide on trail-doc buffers (a trail can't be a
  // waypoint of itself). The shared trail-doc path set lives in
  // `tree/index.ts` (`trailDocPaths`); the host wires it via
  // `setTrailDocPredicate` so we don't take a hard dep on tree's
  // internals. Default predicate is conservative (false) so the pill
  // works before the host wires the predicate; the structural
  // waypoint-path check + backend "self-reference" error are the
  // backstop in that interim.
  let isTrailDocFn: (path: string) => boolean = () => false;

  function compute(): {
    visible: boolean;
    label: string;
    disabled: boolean;
    tooltip: string | undefined;
  } {
    const active = getActiveTrailRel();
    const buf: Buffer | null = bufferStore.get().buffer;
    if (!active || !buf) return hidden();
    if (buf.mode.kind !== "file") return hidden();
    if (!isIndexablePath(buf.path)) return hidden();
    if (isWaypointPath(buf.path)) return hidden();
    if (isTrailDocFn(buf.path)) return hidden();

    const trailBasename = (active.split("/").pop() ?? active).replace(
      /\.md$/i,
      "",
    );
    const label = `Add to trail: ${trailBasename}`;
    const member = getActiveTrailWaypointPaths().has(buf.path);
    return {
      visible: true,
      label,
      disabled: member,
      tooltip: member ? "Already in this trail" : undefined,
    };
  }

  function hidden(): {
    visible: false;
    label: string;
    disabled: boolean;
    tooltip: undefined;
  } {
    return { visible: false, label: "", disabled: false, tooltip: undefined };
  }

  function render(): void {
    const s = compute();
    if (!s.visible) {
      btn.hidden = true;
      return;
    }
    btn.hidden = false;
    btn.disabled = s.disabled;
    if (s.tooltip) {
      btn.title = s.tooltip;
      btn.setAttribute("aria-label", s.tooltip);
    } else {
      btn.title = s.label;
      btn.setAttribute("aria-label", s.label);
    }
  }

  btn.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    const active = getActiveTrailRel();
    const buf = bufferStore.get().buffer;
    if (!active || !buf || buf.mode.kind !== "file") return;
    if (btn.disabled) return;
    const trailBasename = (active.split("/").pop() ?? active).replace(
      /\.md$/i,
      "",
    );
    try {
      await Ipc.trailAppendWaypoint({
        trailDocRel: active,
        sourceRel: buf.path,
        parentWaypointId: null,
        annotation: null,
      });
      showToast(`Added to ${trailBasename}`);
      // `core::trails::append_waypoint` suppresses the watcher for
      // the waypoint-note + trail-doc paths to prevent indexer
      // feedback loops, so the `hiker:file-changed`-driven refresh
      // path can't fire here. Explicitly notify the host to refresh
      // the trails panel + membership cache; the membership cache
      // flip then drives the pill into its disabled+tooltip state.
      // See `bug-add-to-trail-verbs-dont-refresh-panel`.
      opts.onAppended?.();
    } catch (err) {
      Logger.error("ui::trails", "trail append from editor pill failed", {
        error: String(err),
        rel: buf.path,
        trail: active,
      });
      showToast("Failed to add waypoint");
    }
  });

  // Re-render on every relevant store / cache change.
  activeTrailStore.subscribe(() => render());
  bufferStore.subscribe(() => render());
  subscribeMembership(() => render());

  // First paint.
  render();

  return {
    render,
    setTrailDocPredicate(fn: (path: string) => boolean): void {
      isTrailDocFn = fn;
      render();
    },
  };
}
