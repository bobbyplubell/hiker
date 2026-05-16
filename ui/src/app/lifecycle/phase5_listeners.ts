// Phase 5 — bind every event-listener / interval that depends on
// already-mounted controllers + registered services.
//
// Preconditions: phases 2–4 (every controller this listener fan-out
// reads via `controllers.X.tryGet()` and every service it calls via
// `services.Y.get()` must be set; `ctx.refreshTree` / `ctx.openFile` /
// `ctx.closeTab` set).
// Outputs (services): startBackgroundIntervals.
// Side effects: window-level keybinds, index-status bus subscription,
// misc listeners (watcher-overflow / llm-warning / config-reloaded),
// editor keybinds, file watcher.
//
// status: bootstrap-phase-split

import { installWindowKeybindings } from "../keybindings";
import { mountIndexStatusBus } from "../indexStatusBus";
import { installMiscListeners } from "../miscListeners";
import { registerEditorKeybinds } from "../keybindRegistrations";
import { installFileWatcher } from "../fileWatcher";
import { getBuffer, getActivePath } from "../state";
import { controllers } from "../controllers";
import { services } from "../services";
import { need } from "./ctx";

export function phase5_wireEventListeners(): void {
  const vaultIsOpen = need("vaultIsOpen");
  const closeTab = need("closeTab");
  const settingsPane = controllers.settingsPane.get();
  const discovery = controllers.discovery.get();
  const navSetup = controllers.nav.get();
  const indexStatusView = controllers.indexStatusView.get();
  const tree = controllers.tree.get();
  const vaultHome = controllers.vaultHome.get();
  const tabs = controllers.tabs.get();

  // Window-level keybinding handlers.
  installWindowKeybindings({
    toggleSettings: () => settingsPane.toggle(),
    focusSearchInput: () => discovery.api.focusInput(),
    closeActiveTab: () => { const ap = getActivePath(); if (ap) void closeTab(ap); },
    cycleTab: (delta) => tabs.cycleTab(delta),
    jumpToTab: (n) => tabs.jumpToTab(n),
    getNav: () => navSetup.nav,
    getActivePath: () => getActivePath(),
  });

  let bufferPathInterval: number | null = null;
  let lastSeenBufferPath: string | null = null;
  function startBackgroundIntervals(): void {
    if (bufferPathInterval !== null) window.clearInterval(bufferPathInterval);
    bufferPathInterval = window.setInterval(() => {
      if (!vaultIsOpen()) return;
      const buffer = getBuffer();
      const cur = buffer?.path ?? null;
      if (cur !== lastSeenBufferPath) {
        lastSeenBufferPath = cur;
        discovery.api.scheduleRelatedRefresh(cur, 0);
      }
    }, 250);
  }
  services.startBackgroundIntervals.set(startBackgroundIntervals);

  mountIndexStatusBus({
    onStatusChanged: (next) => indexStatusView.setStatus(next),
    onOutstandingChanged: (count) => indexStatusView.setOutstanding(count),
    updateIndexStateForPath: (path, state) =>
      indexStatusView.updateIndexStateForPath(path, state),
    deleteIndexState: (p) => tree.api.deleteIndexState(p),
    getIndexState: (p) => tree.api.getIndexState(p),
    getActiveBufferPath: () => getBuffer()?.path ?? null,
    scheduleRelatedRefresh: (rel, delayMs) =>
      discovery.api.scheduleRelatedRefresh(rel, delayMs),
    scheduleStatsRefresh: () => vaultHome.api.scheduleStatsRefresh(),
  });

  // Snapshot / file / trash mode-controls registrations + misc listeners
  // (watcher-overflow toast, llm-warning toast, config-reloaded reload).
  installMiscListeners();

  registerEditorKeybinds();

  // Filesystem watcher → editor integration.
  installFileWatcher();
}
