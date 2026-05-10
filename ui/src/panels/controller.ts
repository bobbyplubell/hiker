// Tiny shared shape for sidebar / pane panels (`tree`, `vaultHome`,
// `discovery`, `queueDetail`, `trash`). Each `mount*` factory currently
// re-implements the same surface — visibility toggle, an `Api` for
// host-driven actions, a destroy hook (mostly missing today). The bug row
// names a `PanelController<Api>` that pins the shape so future panels and
// follow-up migrations have one thing to conform to instead of N
// near-duplicates.
//
// Migration is incremental — see `bug-panels-duplicate-mount-pattern` for
// rationale. v1 lands the interface + a small `createPanelController`
// helper + one panel (`trash`) on the new shape as proof-of-shape. The
// other four panels (`tree`, `vaultHome`, `discovery`, `queueDetail`) keep
// their existing factory APIs and will migrate one at a time so each
// review stays narrow.

import type { SettingsManager } from "../settings/manager";

/// Common host-supplied dependencies every panel takes. Pairs with the
/// per-panel `Deps` shapes — those still carry their panel-specific DOM
/// handles + callbacks; these are the seams that were copy-pasted across
/// every panel today.
///
/// `settings` references the shared `SettingsManager` (per
/// `bug-persist-setting-duplicated-per-module`) rather than re-declaring a
/// `persistSetting(scope, key, value)` closure shape — that bug already
/// landed the canonical surface.
export interface PanelDeps {
  /// Non-blocking user-visible message. Backed by the existing widget
  /// `showToast` in production; a no-op in tests.
  toast: (msg: string, opts?: { actionLabel?: string; onAction?: () => void }) => void;
  /// Error → human-readable string. Same shape main.ts threads as
  /// `formatError` today.
  formatErr: (err: unknown) => string;
  /// Persisted-settings facade. Panels that need to flip `vault.*` /
  /// `search.*` etc. settings call through here.
  settings: SettingsManager;
  /// Open a note in the editor (preview-tab-aware). Mirrors the existing
  /// per-panel `onOpenNote` callbacks.
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// Move keyboard focus into the editor's CM6 view. Currently used by
  /// surfaces that take focus (search input, tree row click) — declared
  /// here so panels don't each carry a `focusEditor` callback ad-hoc.
  focusEditor: () => void;
}

/// Shared shape for panel modules. Each `mount*` factory returns one of
/// these instead of an ad-hoc `{...Api}` object. The `api` field carries
/// the panel-specific surface (refresh / setSortOrder / scheduleRefresh /
/// etc.); the four sibling fields are the cross-panel uniform.
///
/// `destroy()` is currently a no-op for every panel (none unmount during
/// app lifetime), but declaring it now means the seam exists when a
/// future hot-reload / multi-vault path needs it.
export interface PanelController<Api> {
  isVisible(): boolean;
  setVisible(on: boolean): void;
  api: Api;
  destroy(): void;
}

/// Options for `createPanelController`. `containerEl` is the DOM root the
/// helper toggles via the `hidden` attribute when no `onSetVisible` is
/// provided. `onSetVisible` lets a panel override the default toggle —
/// e.g. trash flips a `.collapsed` class on its bin instead of using
/// `hidden`, and queueDetail seeds a snapshot on first show.
export interface CreatePanelControllerOpts {
  containerEl?: HTMLElement;
  /// Initial visibility. Defaults to whatever the container's current
  /// `hidden` state implies (when a `containerEl` is supplied) or
  /// `false` otherwise.
  initialVisible?: boolean;
  /// Custom visibility hook. When provided, it owns the DOM side-effect;
  /// the helper just tracks the boolean and routes `isVisible()` /
  /// `setVisible(on)` through it.
  onSetVisible?: (on: boolean) => void;
  /// Optional teardown hook. Called from the controller's `destroy()`.
  /// Default no-op.
  onDestroy?: () => void;
  /// When true (default), the helper invokes the visibility hook once at
  /// mount with the initial value so the DOM matches the tracked state.
  /// Set false when the hook has side effects beyond DOM updates (e.g.
  /// persisting a setting) and the caller has already arranged the DOM
  /// to match `initialVisible`.
  applyOnMount?: boolean;
}

export function createPanelController<Api>(
  api: Api,
  opts: CreatePanelControllerOpts = {},
): PanelController<Api> {
  let visible: boolean;
  if (opts.initialVisible !== undefined) {
    visible = opts.initialVisible;
  } else if (opts.containerEl) {
    visible = !opts.containerEl.hidden;
  } else {
    visible = false;
  }

  function applyVisibility(on: boolean): void {
    if (opts.onSetVisible) {
      opts.onSetVisible(on);
    } else if (opts.containerEl) {
      opts.containerEl.hidden = !on;
    }
  }

  // Sync the DOM with the initial state so callers don't have to
  // double-call setVisible() on mount. Skippable for hooks with
  // non-idempotent side effects (e.g. persisted-setting writes); those
  // pass `applyOnMount: false` and arrange the DOM themselves.
  if (opts.applyOnMount !== false) {
    applyVisibility(visible);
  }

  return {
    isVisible: () => visible,
    setVisible(on: boolean) {
      visible = on;
      applyVisibility(on);
    },
    api,
    destroy() {
      opts.onDestroy?.();
    },
  };
}
