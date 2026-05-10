// status: trail-capture-flow
//
// Generic capture-into-active-trail helper. Per `docs/trails.md`'s
// "Capturing into a trail" section, every capture entry point — the
// browser-extension Save-to-Hiker, drag-URL, the MCP scrape tool, the
// share sheet — should route through here once the source-derived note
// has landed at its normal location. With an active trail set, the
// helper appends a waypoint linking to the source; with no active
// trail, the call is a silent no-op (the active-trail mode adds
// routing, never forces it).
//
// v1 has **no real entry points** wired yet — drag-URL doesn't exist,
// the browser extension isn't built, MCP scrape is `planned`. This
// helper lands as a stub so the *next* slug that adds a capture entry
// point drops in without re-deriving the routing logic. The slug stays
// `partial` in `status.md` until at least one entry point is wired.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { getActiveTrailRel } from "../app/state";

/// Append `sourceRel` to the active trail as a waypoint. No-op when no
/// trail is active. Errors are logged but not thrown — capture entry
/// points should not fail because trail routing failed; the source
/// note has already landed in its normal location.
export async function captureToActiveTrail(sourceRel: string): Promise<void> {
  const active = getActiveTrailRel();
  if (active === null) return;
  try {
    await Ipc.trailAppendWaypoint({
      trailDocRel: active,
      sourceRel,
      annotation: null,
    });
  } catch (err) {
    Logger.error("ui::trails", "captureToActiveTrail failed", {
      err,
      sourceRel,
      trail: active,
    });
  }
}
