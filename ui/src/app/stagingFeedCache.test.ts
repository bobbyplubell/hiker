import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mountStagingFeedCache } from "./stagingFeedCache";
import type { Proposal } from "../ipc";

function proposal(id: string, target = "a.md"): Proposal {
  return {
    id,
    surface: "mcp-tool-call",
    action: "edit_note",
    target_path: target,
    created_at_ms: 0,
  };
}

describe("StagingFeedCache", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("debounces a burst of staging-changed events into one fetch", async () => {
    let fire: () => void = () => {};
    const fetch = vi.fn().mockResolvedValue([proposal("p1")]);
    const cache = mountStagingFeedCache({
      fetch,
      onChange: (cb) => {
        fire = cb;
        return () => {};
      },
      debounceMs: 50,
    });

    // 30 events in a tight burst.
    for (let i = 0; i < 30; i++) fire();
    expect(fetch).toHaveBeenCalledTimes(0);

    await vi.advanceTimersByTimeAsync(50);
    expect(fetch).toHaveBeenCalledTimes(1);

    cache.dispose();
  });

  it("broadcasts the fetched snapshot to all subscribers", async () => {
    let fire: () => void = () => {};
    const snapshot = [proposal("p1"), proposal("p2", "b.md")];
    const cache = mountStagingFeedCache({
      fetch: vi.fn().mockResolvedValue(snapshot),
      onChange: (cb) => {
        fire = cb;
        return () => {};
      },
      debounceMs: 10,
    });

    const cb1 = vi.fn();
    const cb2 = vi.fn();
    cache.subscribe(cb1);
    cache.subscribe(cb2);

    fire();
    await vi.advanceTimersByTimeAsync(10);

    expect(cb1).toHaveBeenCalledTimes(1);
    expect(cb1).toHaveBeenCalledWith(snapshot);
    expect(cb2).toHaveBeenCalledWith(snapshot);
    expect(cache.current()).toEqual(snapshot);

    cache.dispose();
  });

  it("refresh() bypasses the debounce and resolves with the result", async () => {
    const fetch = vi.fn().mockResolvedValue([proposal("p1")]);
    const cache = mountStagingFeedCache({
      fetch,
      onChange: () => () => {},
      debounceMs: 1000,
    });

    const cb = vi.fn();
    cache.subscribe(cb);

    const result = await cache.refresh();
    expect(result).toEqual([proposal("p1")]);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(cb).toHaveBeenCalledWith([proposal("p1")]);

    cache.dispose();
  });

  it("keeps the last-good snapshot when fetch rejects", async () => {
    let fire: () => void = () => {};
    const fetch = vi
      .fn()
      .mockResolvedValueOnce([proposal("p1")])
      .mockRejectedValueOnce(new Error("boom"));
    const onError = vi.fn();
    const cache = mountStagingFeedCache({
      fetch,
      onChange: (cb) => {
        fire = cb;
        return () => {};
      },
      debounceMs: 5,
      onError,
    });

    await cache.refresh();
    expect(cache.current()).toEqual([proposal("p1")]);

    fire();
    await vi.advanceTimersByTimeAsync(5);
    // Failed fetch — snapshot preserved.
    expect(cache.current()).toEqual([proposal("p1")]);
    expect(onError).toHaveBeenCalledTimes(1);

    cache.dispose();
  });

  it("unsubscribes one subscriber without affecting the others", async () => {
    let fire: () => void = () => {};
    const cache = mountStagingFeedCache({
      fetch: vi.fn().mockResolvedValue([proposal("p1")]),
      onChange: (cb) => {
        fire = cb;
        return () => {};
      },
      debounceMs: 5,
    });

    const cb1 = vi.fn();
    const cb2 = vi.fn();
    const unsub1 = cache.subscribe(cb1);
    cache.subscribe(cb2);

    unsub1();
    fire();
    await vi.advanceTimersByTimeAsync(5);

    expect(cb1).not.toHaveBeenCalled();
    expect(cb2).toHaveBeenCalledTimes(1);

    cache.dispose();
  });

  it("dispose() tears down the staging-changed subscription", () => {
    const unlisten = vi.fn();
    const cache = mountStagingFeedCache({
      fetch: vi.fn().mockResolvedValue([]),
      onChange: () => unlisten,
      debounceMs: 5,
    });

    cache.dispose();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("dispose() suppresses broadcasts from in-flight refreshes", async () => {
    let resolve: (p: Proposal[]) => void = () => {};
    const cache = mountStagingFeedCache({
      fetch: () => new Promise<Proposal[]>((r) => (resolve = r)),
      onChange: () => () => {},
      debounceMs: 5,
    });

    const cb = vi.fn();
    cache.subscribe(cb);
    const pending = cache.refresh();
    cache.dispose();
    resolve([proposal("p1")]);
    await pending;
    expect(cb).not.toHaveBeenCalled();
  });

  it("accepts a Promise-shaped onChange (Tauri listen())", async () => {
    const unlisten = vi.fn();
    let fire: () => void = () => {};
    const cache = mountStagingFeedCache({
      fetch: vi.fn().mockResolvedValue([proposal("p1")]),
      onChange: async (cb) => {
        fire = cb;
        return unlisten;
      },
      debounceMs: 5,
    });

    // Flush the microtask that resolves the Promise-shaped onChange.
    await Promise.resolve();
    cache.dispose();
    expect(unlisten).toHaveBeenCalled();
    // Late events after dispose are no-ops.
    fire();
  });
});
