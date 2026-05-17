/// Shared staging-feed cache. Centralizes the `Ipc.stagingList()` fetch
/// path that several surfaces (patch-review, chat, tree, plus anything
/// derivative) used to drive independently — each subscribing to
/// `hiker:staging-changed` and round-tripping its own IPC call.
///
/// Cluster Apply or `staging_accept_all` can fire dozens of
/// `staging-changed` events in tight succession; the cache debounces the
/// burst into a single fetch and broadcasts the result to every
/// subscriber. Subscribers transform the broadcast into whatever local
/// shape they need (a `Set<id>` in chat, a filtered array in
/// patch-review, etc.) — the cache only stores the raw list.
///
/// Constructed in `phase1_runtime.ts` so the IPC + event seams are owned
/// by the lifecycle, not by individual panels. Tests inject `fetch` and
/// `onChange` directly.
import type { Proposal } from "../ipc";

export type StagingFeedSubscriber = (proposals: Proposal[]) => void;

export interface StagingFeedCache {
  /// Register a subscriber. Returns an unsubscribe fn. The subscriber is
  /// NOT called eagerly with the current snapshot — call `current()`
  /// after subscribing if you need to seed initial state synchronously,
  /// or `refresh()` if you need to await a fresh fetch first.
  subscribe(cb: StagingFeedSubscriber): () => void;
  /// Force an immediate fetch (bypasses debounce). Resolves with the
  /// fetched list; broadcasts to subscribers on success.
  refresh(): Promise<Proposal[]>;
  /// Synchronous read of the last successfully fetched snapshot. Empty
  /// array before the first fetch resolves.
  current(): Proposal[];
  /// Tear down the staging-changed subscription. Subsequent events are
  /// ignored; in-flight refreshes still resolve but don't broadcast.
  dispose(): void;
}

export interface StagingFeedCacheDeps {
  /// IPC fetch — typically `() => Ipc.stagingList()`. Injected so tests
  /// can substitute a stub.
  fetch: () => Promise<Proposal[]>;
  /// Event subscription — typically wraps `onHikerEvent("hiker:staging-changed", cb)`.
  /// Returns an unsubscribe (matching the `UnlistenFn` Tauri ships).
  onChange: (cb: () => void) => Promise<() => void> | (() => void);
  /// Debounce window in ms. Defaults to 50ms — large enough to coalesce
  /// the per-row staging events emitted by Cluster Apply, small enough
  /// that an interactive accept/reject still feels live.
  debounceMs?: number;
  /// Error sink for failed fetches. Defaults to a no-op; production
  /// wires this to `Logger.error`.
  onError?: (err: unknown) => void;
}

export function mountStagingFeedCache(
  deps: StagingFeedCacheDeps,
): StagingFeedCache {
  const debounceMs = deps.debounceMs ?? 50;
  const onError = deps.onError ?? (() => {});
  const subscribers = new Set<StagingFeedSubscriber>();
  let snapshot: Proposal[] = [];
  let timer: ReturnType<typeof setTimeout> | null = null;
  let disposed = false;
  let unlisten: (() => void) | null = null;

  // Subscribe immediately. The dep returns a thenable in production
  // (Tauri's `listen` is async) and a sync fn in tests; handle both.
  const ready = deps.onChange(scheduleRefresh);
  if (typeof (ready as Promise<() => void>).then === "function") {
    void (ready as Promise<() => void>).then((u) => {
      if (disposed) u();
      else unlisten = u;
    });
  } else {
    unlisten = ready as () => void;
  }

  async function doFetch(): Promise<Proposal[]> {
    try {
      const next = await deps.fetch();
      if (disposed) return next;
      snapshot = next;
      for (const cb of subscribers) {
        try {
          cb(snapshot);
        } catch (err) {
          onError(err);
        }
      }
      return next;
    } catch (err) {
      onError(err);
      return snapshot;
    }
  }

  function scheduleRefresh(): void {
    if (disposed) return;
    if (timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      void doFetch();
    }, debounceMs);
  }

  return {
    subscribe(cb) {
      subscribers.add(cb);
      return () => subscribers.delete(cb);
    },
    refresh() {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      return doFetch();
    },
    current() {
      return snapshot;
    },
    dispose() {
      disposed = true;
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      if (unlisten) {
        unlisten();
        unlisten = null;
      }
      subscribers.clear();
    },
  };
}
