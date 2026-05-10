// status: trail-add-to-active-from-editor-verb
//
// Single source of truth for "which paths are waypoints of the
// currently active trail." Per `docs/trails.md`'s "Building a trail
// while reading" section, both the tree right-click verb and the
// editor toolbar pill must idempotency-check against the same
// per-trail membership view ("Already in this trail" tooltip rather
// than a duplicate append).
//
// The cache lives here (not in `tree/index.ts` and not in
// `addToTrailPill.ts`) so both consumers read from the same
// synchronous getter; an async refresh kicks off whenever the active
// trail changes or a watcher event touches `.hiker/trails/<active>/`.
// A subscribe seam lets the pill re-render on cache change without
// polling.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { activeTrailStore, getActiveTrailRel } from "../app/state";
import type { ResolvedWaypoint } from "../ipc";

let cache: Set<string> = new Set();
let cachedTrailRel: string | null = null;
let refreshEpoch = 0;
const subscribers = new Set<() => void>();

function notify(): void {
  for (const cb of subscribers) {
    try {
      cb();
    } catch (err) {
      Logger.error("ui::trails", "subscriber threw", { err });
    }
  }
}

function collectPaths(waypoints: ResolvedWaypoint[], out: Set<string>): void {
  for (const wp of waypoints) {
    if (wp.source_ref?.path) out.add(wp.source_ref.path);
    if (wp.children && wp.children.length > 0) {
      collectPaths(wp.children, out);
    }
  }
}

/// Synchronous getter: paths of every note that's a waypoint (at any
/// depth) of the currently-active trail. Empty when no trail is
/// active. The set may be stale by one async tick after an
/// active-trail change or a waypoint mutation; consumers that need
/// to react should also `subscribe` so they re-render when the
/// async refresh lands.
export function getActiveTrailWaypointPaths(): Set<string> {
  return cache;
}

/// Subscribe to cache changes (every successful refresh that
/// produces a different set, plus active-trail changes that clear
/// the cache). Returns an unsubscribe.
export function subscribeMembership(cb: () => void): () => void {
  subscribers.add(cb);
  return () => {
    subscribers.delete(cb);
  };
}

/// Refresh the cache against the currently-active trail. No-op when
/// no trail is active (clears the cache + notifies if it had
/// entries). Race-safe: each call bumps an epoch and stale
/// responses are dropped.
export async function refreshActiveTrailWaypointPaths(): Promise<void> {
  const activeRel = getActiveTrailRel();
  const epoch = ++refreshEpoch;
  if (activeRel === null) {
    if (cache.size > 0 || cachedTrailRel !== null) {
      cache = new Set();
      cachedTrailRel = null;
      notify();
    }
    return;
  }
  try {
    const detail = await Ipc.trailGet({ trailDocRel: activeRel });
    if (epoch !== refreshEpoch) return;
    const fresh = new Set<string>();
    collectPaths(detail.waypoints, fresh);
    let changed = activeRel !== cachedTrailRel || fresh.size !== cache.size;
    if (!changed) {
      for (const p of fresh) {
        if (!cache.has(p)) {
          changed = true;
          break;
        }
      }
    }
    if (changed) {
      cache = fresh;
      cachedTrailRel = activeRel;
      notify();
    }
  } catch (err) {
    if (epoch !== refreshEpoch) return;
    Logger.error("ui::trails", "trail_get refresh failed", {
      err,
      activeRel,
    });
  }
}

/// Wire the cache to the active-trail store: any change clears the
/// cache immediately (so the UI doesn't render stale "Already in
/// this trail" between trail-switch and the async refresh) and
/// schedules a refresh.
export function installMembershipWatchers(): void {
  let lastActive = getActiveTrailRel();
  // Initial load.
  void refreshActiveTrailWaypointPaths();
  activeTrailStore.subscribe((s) => {
    if (s.rel === lastActive) return;
    lastActive = s.rel;
    // Eagerly clear so the UI never reports membership against the
    // *previous* trail between the activation and the async refresh.
    if (cache.size > 0) {
      cache = new Set();
      cachedTrailRel = null;
      notify();
    }
    void refreshActiveTrailWaypointPaths();
  });
}
