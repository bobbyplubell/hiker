// Tiny typed event bus for cross-module UI events. Emitters publish a
// named event with a typed payload; subscribers register a callback for
// the events they care about. The bus is the seam that decouples the
// "something happened" producer (a sidebar toggle handler, a settings
// write, a vault swap) from the N panels that want to react.
//
// Pure: no DOM, no Tauri, no IPC. Listeners run synchronously on the
// emitter's thread; a throwing listener is logged and the remaining
// listeners still fire (one bad subscriber doesn't break sibling panels).
//
// `EventMap` is the canonical typed surface — adding an event is one row
// here plus call sites. The compiler then enforces matching payload
// shapes on every `emit` / `on` pair.
//
// Pairs with `bug-persist-setting-duplicated-per-module`: `SettingsManager`
// is the canonical emitter for `settings-changed` — the host wires its
// `onSettingsChanged` callback to `emit("settings-changed", cfg)` so any
// panel can subscribe via the bus rather than holding a direct reference
// to the manager.

import { Logger } from "../logger";
import type { Settings } from "../app/settingsApply";

/// Typed event surface. Add a row here, then `emit` / `on` are typed for
/// it everywhere.
export interface EventMap {
  /// Fires after the sidebar collapse state flips. `open === true` means
  /// the sidebar is now visible; `false` means collapsed.
  "sidebar-toggled": { open: boolean };
  /// Fires after a vault has been opened and the host has finished
  /// applying its settings / mounted state. `path` is the vault root.
  "vault-opened": { path: string };
  /// Fires when the active vault is being closed (no current consumer in
  /// v1; declared so future close paths have a typed seam).
  "vault-closed": Record<string, never>;
  /// Fires after a successful `set_setting` write, carrying the merged
  /// `Settings` snapshot returned by core. Wired from
  /// `SettingsManager.onSettingsChanged` in the host.
  "settings-changed": Settings;
}

export type EventName = keyof EventMap;
export type Unsubscribe = () => void;

type Listener<E extends EventName> = (payload: EventMap[E]) => void;

// Internal storage erases the per-event type — the public `emit`/`on`
// surface preserves it. Casts at the boundary are safe because every
// insertion goes through `on<E>` which only writes a `Listener<E>` into
// the bucket keyed by `E`.
type AnyListener = (payload: unknown) => void;
const listeners = new Map<EventName, Set<AnyListener>>();

export function emit<E extends EventName>(name: E, payload: EventMap[E]): void {
  const set = listeners.get(name);
  if (!set || set.size === 0) return;
  // Snapshot so a listener that unsubscribes mid-dispatch doesn't perturb
  // the iteration. Listener throws are logged and swallowed — sibling
  // listeners still fire.
  for (const cb of [...set]) {
    try {
      cb(payload);
    } catch (err) {
      Logger.error("ui::app", "event-bus listener threw", { event: name, err });
    }
  }
}

export function on<E extends EventName>(
  name: E,
  cb: Listener<E>,
): Unsubscribe {
  let set = listeners.get(name);
  if (!set) {
    set = new Set();
    listeners.set(name, set);
  }
  const erased = cb as unknown as AnyListener;
  set.add(erased);
  return () => {
    set!.delete(erased);
  };
}
