// UI entry point. The bootstrap orchestrator below documents the fixed
// phase order of UI initialization. Each phase lives in its own module
// under `./app/lifecycle/`:
//
//   1. initRuntime           — DOM cache + settings manager + early services
//   2. mountEditorCore       — chat, editor pane, patch-review, snapshot,
//                              index-status, settings-pane, updateStatus
//   3. mountPanels           — vault lifecycle, top strip, tree, vault home,
//                              queue detail, nav, sidebars, discovery,
//                              trails, clusters, properties, app-page tabs,
//                              agent-changes
//   4. mountTabsAndOpenFile  — trash, tabs, openFile/openFileApi, autosave,
//                              addToTrailPill, tab strip, write-note review,
//                              task-queue tile
//   5. wireEventListeners    — window keybinds, index-status bus,
//                              misc listeners, editor keybinds, file watcher
//   6. startBootstrap        — initial paint + default-vault auto-open
//
// Forward references between phases flow through two existing
// registries (`./app/controllers`, `./app/services`) and a tiny shared
// context (`./app/lifecycle/ctx`). A phase that registers `services.X`
// must run before any phase whose callback fires `services.X.get()`
// synchronously; the order encoded below respects that.
//
// status: bootstrap-phase-split

import { phase1_initRuntime } from "./app/lifecycle/phase1_runtime";
import { phase2_mountEditorCore } from "./app/lifecycle/phase2_editor";
import { phase3_mountPanels } from "./app/lifecycle/phase3_panels";
import { phase4_mountTabsAndOpenFile } from "./app/lifecycle/phase4_tabsAndOpenFile";
import { phase5_wireEventListeners } from "./app/lifecycle/phase5_listeners";
import { controllers } from "./app/controllers";
import { need } from "./app/lifecycle/ctx";

async function bootstrap(): Promise<void> {
  phase1_initRuntime();
  phase2_mountEditorCore();
  phase3_mountPanels();
  phase4_mountTabsAndOpenFile();
  phase5_wireEventListeners();

  // Initial paint — every mount above is now in scope.
  need("updateStatus")();

  void controllers.vaultLifecycle.get().bootstrapDefaultVault();
}

void bootstrap();
