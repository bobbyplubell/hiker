/// Tiny store-with-listeners primitive. Single source of truth for a
/// piece of state; consumers `get()` a snapshot, `set()` a new value, or
/// `subscribe(cb)` to be notified after every set. Pure — no DOM, no
/// Tauri. ~30 lines on purpose: this is observer plumbing, not Redux.
///
/// Notification semantics: subscribers fire synchronously after `set()`
/// returns. Equality-checking is *not* done — every `set` notifies, even
/// if the next value is `===` the previous one. Callers that care about
/// equality should check before calling `set`.
///
/// Used to hoist UI state out of `let`-bindings inside module closures
/// per `bug-ui-state-in-mutable-closures`. Cross-module observers (e.g.
/// the chat panel reading the active note via the `BufferApi` shim)
/// subscribe instead of being passed bespoke deps closures.
export type Unsubscribe = () => void;

export interface Store<T> {
  get(): T;
  set(next: T): void;
  /// Convenience: call `update(prev => next)` instead of
  /// `set({ ...store.get(), ... })`. Identical notification semantics.
  update(updater: (prev: T) => T): void;
  /// Register a listener; returns the un-subscribe handle. Listeners do
  /// *not* fire on registration — call `get()` if you need an initial
  /// paint.
  subscribe(listener: (value: T) => void): Unsubscribe;
}

export function createStore<T>(initial: T): Store<T> {
  let value = initial;
  const listeners = new Set<(v: T) => void>();
  return {
    get: () => value,
    set: (next: T) => {
      value = next;
      for (const l of listeners) l(value);
    },
    update: (updater: (prev: T) => T) => {
      value = updater(value);
      for (const l of listeners) l(value);
    },
    subscribe: (listener: (v: T) => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}
