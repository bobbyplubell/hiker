// status: navigation-history-stack
// status: top-strip-back-button
// status: top-strip-forward-button
// status: navigation-trackpad-swipe
// status: navigation-keybind
//
// Browser-style back/forward across editor-pane content surfaces. Keeps a
// per-vault in-memory `back` / `forward` stack of `NavState` values. The
// host owns transitions; this module observes them via `checkpoint()` and
// restores prior states via `apply` on back/forward.
//
// Push rule: every user-initiated transition between distinct content
// surfaces appends to the back stack and clears the forward stack
// (browser shape). Restoration via back/forward is itself silent — the
// `restoring` flag suppresses checkpoints fired by `apply`'s side effects.

import type { ChangeRow } from "../snapshotPreview";

export type NavState =
  | { kind: "tab"; path: string }
  | { kind: "home" }
  | { kind: "home-detail"; view: "recent-activity" }
  | { kind: "queue-detail" }
  | { kind: "settings" }
  | { kind: "trash-preview"; trashedName: string }
  | { kind: "snapshot-preview"; changeId: number; row: ChangeRow }
  | { kind: "empty" };

export interface NavDeps {
  /// Read the live UI state and return the current `NavState`. Called by
  /// `checkpoint()` to detect transitions.
  inferCurrent: () => NavState;
  /// Apply a stored state by driving the appropriate UI transition.
  /// Returns `false` when the target is no longer valid (e.g. a closed
  /// tab) so the navigation can skip past it.
  apply: (state: NavState) => Promise<boolean> | boolean;
  /// Fired whenever back/forward availability or current state changes.
  onChange: () => void;
}

export interface NavApi {
  /// Compare `inferCurrent()` to the tracked current state; if changed,
  /// push the prior current onto the back stack and clear forward.
  checkpoint(): void;
  back(): Promise<boolean>;
  forward(): Promise<boolean>;
  canBack(): boolean;
  canForward(): boolean;
  /// Drop both stacks and the tracked current. Called on vault swap per
  /// spec (history is per-vault, not persisted).
  reset(): void;
  /// Drop any stack entries referencing a closed tab path. Called from
  /// the host after `closeTab` and after a preview-slot replacement.
  pruneTab(path: string): void;
}

function sameState(a: NavState | null, b: NavState): boolean {
  if (a === null) return false;
  if (a.kind !== b.kind) return false;
  switch (a.kind) {
    case "tab":
      return a.path === (b as { path: string }).path;
    case "home-detail":
      return a.view === (b as { view: string }).view;
    case "trash-preview":
      return a.trashedName === (b as { trashedName: string }).trashedName;
    case "snapshot-preview":
      return a.changeId === (b as { changeId: number }).changeId;
    default:
      return true;
  }
}

export function mountNavigation(deps: NavDeps): NavApi {
  let current: NavState | null = null;
  const back: NavState[] = [];
  const forward: NavState[] = [];
  let restoring = false;

  function checkpoint(): void {
    if (restoring) return;
    const next = deps.inferCurrent();
    if (sameState(current, next)) return;
    if (current !== null) back.push(current);
    current = next;
    forward.length = 0;
    deps.onChange();
  }

  async function navigate(direction: "back" | "forward"): Promise<boolean> {
    const src = direction === "back" ? back : forward;
    const dst = direction === "back" ? forward : back;
    while (src.length > 0) {
      const target = src.pop()!;
      if (sameState(current, target)) continue;
      restoring = true;
      let ok: boolean;
      try {
        ok = await deps.apply(target);
      } finally {
        restoring = false;
      }
      if (ok) {
        if (current !== null) dst.push(current);
        current = target;
        deps.onChange();
        return true;
      }
      // Target invalid (e.g. closed tab) — try the next prior state.
    }
    deps.onChange();
    return false;
  }

  function pruneTab(path: string): void {
    const filter = (s: NavState) => !(s.kind === "tab" && s.path === path);
    {
      const kept = back.filter(filter);
      back.length = 0;
      back.push(...kept);
    }
    {
      const kept = forward.filter(filter);
      forward.length = 0;
      forward.push(...kept);
    }
    if (current !== null && current.kind === "tab" && current.path === path) {
      // Caller will checkpoint to whatever surface replaced the closed tab;
      // mark current invalid so the next checkpoint pushes nothing for it.
      current = null;
    }
    deps.onChange();
  }

  function reset(): void {
    back.length = 0;
    forward.length = 0;
    current = null;
    deps.onChange();
  }

  return {
    checkpoint,
    back: () => navigate("back"),
    forward: () => navigate("forward"),
    canBack: () => back.length > 0,
    canForward: () => forward.length > 0,
    reset,
    pruneTab,
  };
}

// status: navigation-trackpad-swipe
// Watch wheel events for sustained horizontal trackpad scrolls and fire
// back/forward past `~120px` accumulated `deltaX` (browser convention).
// Right-swipe (negative `deltaX` on macOS-style natural scrolling, where
// fingers move right) → back; left-swipe → forward. Skips events whose
// target sits inside an element that can scroll horizontally (so the tab
// strip's overflow scroll, code-block horizontal scroll, etc. still work).
export interface SwipeDeps {
  back: () => void;
  forward: () => void;
}

export function installNavigationSwipe(deps: SwipeDeps): void {
  const THRESHOLD = 120;
  const RESET_MS = 250;
  const COOLDOWN_MS = 600;
  let acc = 0;
  let lastTs = 0;
  let cooldownUntil = 0;

  function targetIsHorizontallyScrollable(t: EventTarget | null): boolean {
    let el = t instanceof Element ? t : null;
    while (el) {
      if (el.scrollWidth > el.clientWidth) {
        const style = getComputedStyle(el);
        const ox = style.overflowX;
        if (ox === "auto" || ox === "scroll") return true;
      }
      el = el.parentElement;
    }
    return false;
  }

  window.addEventListener(
    "wheel",
    (e) => {
      const now = e.timeStamp;
      if (now < cooldownUntil) {
        // Still locked from a recent firing — keep absorbing related
        // delta so it doesn't bleed into the next gesture.
        lastTs = now;
        return;
      }
      const ax = Math.abs(e.deltaX);
      const ay = Math.abs(e.deltaY);
      // Horizontal-dominant only. The 1.5x ratio matches what Chrome's
      // own back-forward swipe heuristic uses to ignore diagonal scrolls.
      if (ax < ay * 1.5 || ax === 0) {
        if (now - lastTs > RESET_MS) acc = 0;
        return;
      }
      // Don't hijack a real horizontal scroll (tab strip overflow,
      // horizontal code blocks, etc.).
      if (targetIsHorizontallyScrollable(e.target)) {
        acc = 0;
        lastTs = now;
        return;
      }
      if (now - lastTs > RESET_MS) acc = 0;
      acc += e.deltaX;
      lastTs = now;
      if (Math.abs(acc) >= THRESHOLD) {
        e.preventDefault();
        if (acc < 0) deps.back();
        else deps.forward();
        acc = 0;
        cooldownUntil = now + COOLDOWN_MS;
      }
    },
    { passive: false, capture: true },
  );
}
