// Phase 1 — runtime init.
//
// Preconditions: none. Runs before any DOM-dependent mount.
// Outputs:
//  - DOM cached (`initDom()`).
//  - `controllers.settings` populated (`SettingsManager`).
//  - Early services registered: `formatError`, `isReadOnlyBuffer`,
//    `appPageTabKey`, `persistSetting`, `vaultIsOpen`.
//  - `ctx.formatError`, `ctx.isReadOnlyBuffer`, `ctx.appPageTabKey`,
//    `ctx.persistSetting`, `ctx.vaultIsOpen`.
//
// status: bootstrap-phase-split

import { describeErr } from "../../ipc/runCommand";
import { initDom } from "../dom";
import { controllers } from "../controllers";
import { services } from "../services";
import { createSettingsManager } from "../../settings/manager";
import type { Buffer } from "../state";
import { ctx } from "./ctx";

type SettingsScope = "user" | "vault";

export function phase1_initRuntime(): void {
  initDom();

  // status: settings-write-back, bug-persist-setting-duplicated-per-module
  // Single `SettingsManager` for the whole UI. Panels accept this `settings`
  // instance in their deps (or import it from `./settings/manager` if they
  // don't need a test seam) instead of bespoke `persistSetting` closures.
  const settings = createSettingsManager({ logTarget: "ui::app" });
  controllers.settings.set(settings);

  // status: tab-kinds
  function appPageTabKey(kind: string, view?: string): string {
    return view ? `__hiker:${kind}:${view}` : `__hiker:${kind}`;
  }

  // Canonical error stringifier. Kept as a local alias of the new
  // `describeErr` from `ipc/runCommand.ts` so the many `formatError` /
  // `formatErr` Deps-bag entries downstream don't need wholesale renaming.
  const formatError = describeErr;

  /// True for any read-only preview buffer (trash / snapshot) or any
  /// non-buffer-kind tab (home, queue, settings, agent, graph, properties).
  function isReadOnlyBuffer(b: Buffer | null): boolean {
    if (!b) return true;
    if (b.kind !== "buffer") return true;
    return b.mode.kind !== "file";
  }

  function vaultIsOpen(): boolean {
    const kind = controllers.vaultLifecycle.tryGet()?.getState().kind;
    return kind === "open" || kind === "opening";
  }

  async function persistSetting(
    scope: SettingsScope,
    key: string,
    value: unknown,
  ): Promise<void> {
    if (scope === "user") return settings.setUserSetting(key, value);
    return settings.setVaultSetting(key, value);
  }

  // Wire up early-needed services so that modules mounted below can call them.
  services.formatError.set(formatError);
  services.isReadOnlyBuffer.set(isReadOnlyBuffer);
  services.appPageTabKey.set(appPageTabKey);
  services.persistSetting.set(persistSetting);
  services.vaultIsOpen.set(vaultIsOpen);

  ctx.formatError = formatError;
  ctx.isReadOnlyBuffer = isReadOnlyBuffer;
  ctx.appPageTabKey = appPageTabKey;
  ctx.persistSetting = persistSetting;
  ctx.vaultIsOpen = vaultIsOpen;
}
