/// Listeners for `hiker:index-status` and `hiker:reindex-progress`. Owns
/// the `indexStatus` snapshot + `outstandingCount` running tally that
/// `renderIndexStatus` (host-owned) reads from, plus the per-path index
/// state cache mutation triggered by terminal progress events.
///
/// Status snapshots arrive over `hiker:index-status` whenever the
/// indexer's `watch::Sender<IndexStatus>` changes. Progress events
/// (`hiker:reindex-progress`) own the per-path marker + outstanding-count
/// bookkeeping; the full `IndexStatus` snapshot rides the status event.
import { listen } from "@tauri-apps/api/event";
import type { IndexState } from "../tree";

export interface IndexStatus {
  model_ready: boolean;
  queued: number;
  total_notes: number;
  last_error: string | null;
}

export type ProgressEvent =
  | { kind: "model_loaded" }
  | { kind: "started"; path: string }
  | { kind: "finished"; path: string }
  | { kind: "skipped"; path: string; reason: string }
  | { kind: "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string }
  | { kind: "scan_complete"; scanned: number; queued: number }
  | { kind: "error"; path: string | null; message: string };

export interface IndexStatusBusDeps {
  /// Called every time `indexStatus` is mutated (replaced or fields
  /// updated). Host paints the status-bar label off this.
  onStatusChanged: (next: IndexStatus) => void;
  /// Called every time `outstandingCount` changes. Host repaints the
  /// status label and may schedule downstream refreshes.
  onOutstandingChanged: (count: number) => void;
  /// Mutate the per-path index-state cache + DOM marker. Implemented by
  /// host's `updateIndexStateForPath`.
  updateIndexStateForPath: (path: string, state: IndexState) => void;
  deleteIndexState: (path: string) => void;
  getIndexState: (path: string) => IndexState | undefined;
  /// Active buffer's path or null. Used to schedule a related-notes
  /// refresh after an indexed-event for the open file.
  getActiveBufferPath: () => string | null;
  scheduleRelatedRefresh: (rel: string | null, delayMs: number) => void;
  /// Vault-home stats refresh hook — counts shift on every terminal
  /// event.
  scheduleStatsRefresh: () => void;
}

export interface IndexStatusBus {
  getStatus: () => IndexStatus;
  getOutstanding: () => number;
}

export function mountIndexStatusBus(deps: IndexStatusBusDeps): IndexStatusBus {
  let indexStatus: IndexStatus = {
    model_ready: false,
    queued: 0,
    total_notes: 0,
    last_error: null,
  };
  // scan_complete adds, every terminal event subtracts, Started is a no-op.
  let outstandingCount = 0;

  void listen<IndexStatus>("hiker:index-status", (event) => {
    indexStatus = event.payload;
    deps.onStatusChanged(indexStatus);
    // Stats counts shift with model_ready / total_notes / last_error too.
    deps.scheduleStatsRefresh();
  });

  void listen<ProgressEvent>("hiker:reindex-progress", (event) => {
    const ev = event.payload;
    switch (ev.kind) {
      case "model_loaded":
        indexStatus = { ...indexStatus, model_ready: true, last_error: null };
        deps.onStatusChanged(indexStatus);
        break;
      case "started":
        // No counter change — Started just marks "queued → processing",
        // same job, still outstanding. Marker stays Queued until terminal.
        deps.updateIndexStateForPath(ev.path, { kind: "queued" });
        break;
      case "finished":
      case "skipped":
      case "deleted":
      case "renamed":
      case "error":
        // Any terminal event ends one outstanding job, regardless of
        // whether a prior Started fired (Delete and Rename don't emit
        // Started).
        outstandingCount = Math.max(0, outstandingCount - 1);
        deps.onOutstandingChanged(outstandingCount);
        if (ev.kind === "error") {
          indexStatus = { ...indexStatus, last_error: ev.message };
        } else {
          indexStatus = { ...indexStatus, last_error: null };
        }
        deps.onStatusChanged(indexStatus);
        if (ev.kind === "finished") {
          deps.updateIndexStateForPath(ev.path, { kind: "indexed" });
          const active = deps.getActiveBufferPath();
          if (active && ev.path === active) {
            deps.scheduleRelatedRefresh(active, 100);
          }
        } else if (ev.kind === "skipped") {
          // "unchanged" is a no-op skip (file already indexed); only
          // persist the Skipped state for genuine refusals.
          if (ev.reason === "unchanged") {
            deps.updateIndexStateForPath(ev.path, { kind: "indexed" });
          } else {
            deps.updateIndexStateForPath(ev.path, {
              kind: "skipped",
              reason: ev.reason,
            });
          }
        } else if (ev.kind === "deleted") {
          deps.deleteIndexState(ev.path);
        } else if (ev.kind === "renamed") {
          const prior = deps.getIndexState(ev.from);
          deps.deleteIndexState(ev.from);
          if (prior) deps.updateIndexStateForPath(ev.to, prior);
        } else if (ev.kind === "error" && ev.path) {
          // Refetch on next render — error state isn't itself a marker.
          deps.deleteIndexState(ev.path);
        }
        break;
      case "scan_complete":
        outstandingCount += ev.queued;
        deps.onOutstandingChanged(outstandingCount);
        break;
    }
    // status: vault-home-stats-widget — counts shift on every terminal
    // event; debounced so a flurry of progress events fires one stats
    // fetch. The full IndexStatus snapshot rides `hiker:index-status`
    // per `bug-index-status-polled-not-pushed` (fixed); progress events
    // only own the per-path marker + outstanding-count bookkeeping.
    deps.scheduleStatsRefresh();
  });

  return {
    getStatus: () => indexStatus,
    getOutstanding: () => outstandingCount,
  };
}
