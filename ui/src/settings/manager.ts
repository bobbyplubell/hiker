// One service for the "flip a toggle, persist via `set_setting`, log on
// failure" pattern that was previously reimplemented in main.ts plus four
// panels. Panels take a `SettingsManager` in deps (or import the host's
// instance directly) instead of bespoke `persistSetting` closures.
//
// Shape:
//   - `setUserSetting(key, value)` / `setVaultSetting(key, value)` — the
//     two scoped write paths; thin wrappers over `Ipc.setSetting` that
//     swallow rejection (logged) so a flip that worked locally never
//     surfaces an error toast just because the disk write failed.
//   - `onSettingsChanged(cb)` — fires after every successful write with
//     the merged `Config` returned by `set_setting`. Per the bug row,
//     this is the seam future cross-panel reactivity rides on (e.g. a
//     panel that wants to react to *any* setting flip, not just the ones
//     it owns the UI for). Currently no production subscriber — the
//     existing per-panel paths already react locally — but the seam is
//     declared so adding one is one `onSettingsChanged(cb)` call.
//
// `optional flash`: queueDetail's prior local `persistSetting`
// also flashed a "Saved" / "Error: ..." status in the toggles tray.
// `createSettingsManager` accepts an optional `flash` callback so that
// surface keeps its existing behavior with no per-panel reimplementation
// of the try/catch shape.
//
// Why a factory rather than a singleton: matches the rest of the UI
// modules (`mountTree`, `mountDiscovery`, `mountTrash` are all
// factory-style) and keeps the test seam explicit. The host constructs
// one manager in `main.ts` and threads it into every panel that needs
// it.

import { Ipc } from "../ipc";
import { Logger, type UiTarget } from "../logger";
import type { SettingsConfig } from "../settings";

export type SettingsScope = "user" | "vault";

export type SettingsChangedListener = (cfg: SettingsConfig) => void;

export interface SettingsManager {
  setUserSetting(key: string, value: unknown): Promise<void>;
  setVaultSetting(key: string, value: unknown): Promise<void>;
  onSettingsChanged(cb: SettingsChangedListener): () => void;
}

export interface CreateSettingsManagerOpts {
  /// Optional status callback. queueDetail uses this to flash
  /// "Saved" / "Error: ..." in its toggles tray; the editor / discovery /
  /// tree / trash panels don't surface a status string and pass nothing.
  flash?: (msg: string, isError: boolean) => void;
  /// Tracing target for `Logger.error` calls. Default `ui::app`;
  /// queueDetail overrides to keep its log lines on `ui::queue-detail`
  /// for grep parity with the prior local implementation.
  logTarget?: UiTarget;
}

export function createSettingsManager(
  opts: CreateSettingsManagerOpts = {},
): SettingsManager {
  const listeners = new Set<SettingsChangedListener>();
  const target: UiTarget = opts.logTarget ?? "ui::app";

  async function persist(
    scope: SettingsScope,
    key: string,
    value: unknown,
  ): Promise<void> {
    try {
      const cfg = await Ipc.setSetting({ scope, key, value });
      // Notify subscribers *before* surfacing the toast so any UI that
      // re-renders from the new config has a chance to repaint before
      // the status string appears (matches the prior local-flash order
      // in queueDetail).
      for (const cb of listeners) {
        try {
          cb(cfg as SettingsConfig);
        } catch (err) {
          Logger.error(target, "settings listener threw", { err });
        }
      }
      opts.flash?.("Saved", false);
    } catch (err) {
      Logger.error(target, "set_setting failed", { scope, key, err });
      opts.flash?.(String(err), true);
    }
  }

  return {
    setUserSetting(key, value) {
      return persist("user", key, value);
    },
    setVaultSetting(key, value) {
      return persist("vault", key, value);
    },
    onSettingsChanged(cb) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
  };
}
