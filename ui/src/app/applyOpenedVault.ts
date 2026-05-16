// status: settings-load-once-at-startup, vault-home-screen
// status: autosave-recover-cmd, autosave-recovery-auto-restore
// status: autosave-tab-state-silent-restore
// status: chat-session-resume-latest
//
// Host wiring for vault-open application + autosave recovery / tab-state
// restore. Reads singletons (`controllers` / `services`) directly.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { emit as emitBusEvent } from "../events/bus";
import type { Settings } from "./settingsApply";
import { dom } from "./dom";
import { getBuffer, setBufferState, getOpenBuffers } from "./state";
import { controllers } from "./controllers";
import { services } from "./services";

export function createApplyOpenedVault(): (path: string) => Promise<void> {
  async function applyOpenedVault(path: string): Promise<void> {
    const vaultPathEl = dom().vaultBar.vaultPathEl;
    const tree = controllers.tree.get();
    const indexStatusView = controllers.indexStatusView.get();
    const taskQueueTile = controllers.taskQueueTile.get();
    const queueDetail = controllers.queueDetail.get();
    const tabStrip = controllers.tabStrip.get();
    const discovery = controllers.discovery.get();
    const chatPanel = controllers.chatPanel.get();
    const trailsPanel = controllers.trailsPanel.tryGet();
    const editor = controllers.editorPane.get().host;
    const openBuffers = getOpenBuffers();

    const basename = path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() ?? path;
    vaultPathEl.textContent = basename;
    vaultPathEl.title = path;
    tree.api.setSelectedFolder("");
    // Announce the open on the bus. No production subscriber today —
    // every cross-module wake-up after a vault swap currently rides
    // through the host's direct calls in `applyOpenedVault`. Declared
    // so future panels (and the deferred `vault-closed` counterpart)
    // have the typed seam without further main.ts edits.
    emitBusEvent("vault-opened", { path });
    indexStatusView.setOutstanding(0);
    // status: task-queue-home-widget
    // Tile mounts pre-vault-open; re-fetch settings + snapshot now.
    void taskQueueTile.refresh();
    // Re-seed the queue-detail worker toggles from the now-vault-bound
    // config — the initial seed at module-load may have errored or
    // resolved against a not-yet-vault-bound config.
    void queueDetail.api.refreshFromSettings();

    // status: settings-load-once-at-startup
    // Seed View menu / tree / panel state from the merged settings. Failures
    // here aren't fatal — fall back to whatever the in-memory defaults are.
    try {
      const s = await Ipc.getSettings<Settings>();
      services.applySettingsToUi(s);
    } catch (err) {
      Logger.error("ui::app", "get_settings failed", { err });
    }
    // Stale per-path state from a prior vault must not leak into the new one
    // (paths can collide across vaults).
    tree.api.clearCaches();
    // status: cluster-editor-sidebar-mode — re-fetch the open-trees list
    // against the freshly-opened vault. Failures self-log inside the
    // module; we don't need to surface them here.
    void controllers.clusterEditor.tryGet()?.refresh();
    // status: multi-buffer-in-memory-only — open buffers don't persist
    // across vault swaps; clear them along with the rest of per-vault state.
    openBuffers.clear();
    // status: editor-preview-tab — preview slot doesn't survive vault swap.
    setBufferState({ buffer: null, activePath: null, previewTabPath: null });
    editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: "" } });
    tabStrip.render();
    // Clear the related-notes panel so hits from the prior vault don't linger
    // until the next file open / save populates it for the new vault.
    void discovery.api.refreshRelated(null);
    // status: chat-panel-pinned-bottom — drop transcript and any in-flight
    // turn so the new vault starts clean.
    chatPanel.reset();
    // status: chat-session-resume-latest
    // Re-seed the panel from the most-recent on-disk session (if any).
    // The backend's `resume_latest_at_open` already adopted it as active;
    // we just paint the rendered transcript here.
    try {
      const active = await Ipc.chatSessionActive();
      chatPanel.hydrate(active);
    } catch (err) {
      Logger.error("ui::app", "chat_session_active failed", { err });
    }
    // Likewise, blank the search input/results so prior-vault matches don't
    // surface in the new vault. status: search-discovery-panel
    discovery.api.clear();
    services.startBackgroundIntervals();
    // status: trail-row-icon — seed the trail-doc set so the first
    // tree paint can decorate trail-doc rows. Awaited before
    // `refreshTree` so the initial paint already includes the icon.
    await tree.api.refreshTrailDocSet();
    // status: staging-accept-reject-from-tree — seed pending proposals
    // so the first tree paint includes synthetic staging rows.
    await tree.api.refreshStagingProposals();
    // status: patch-review-mode — seed the local pending-proposals cache
    // used by the agent-diff toggle + patch-review hunk decorations.
    await services.refreshPendingProposalsCache();
    await services.refreshTree();
    await services.refreshTrashBin();
    // status: trails-mode-body — re-fetch trails-list + active trail
    // detail after the settings snapshot has seeded `activeTrailStore`.
    trailsPanel?.api.onActiveTrailMaybeChanged();
    // status: navigation-history-stack — history is per-vault, so swapping
    // vaults drops the stack along with `openBuffers`. Cleared *before*
    // `vaultHome.setVisible(true)` below so the home page becomes the
    // first checkpoint on the new vault rather than landing on a stale tail.
    controllers.nav.get().nav.reset();
    // status: vault-home-screen — default landing surface on vault open
    // (no auto-resume of last buffer in v1). Opens as a app-page tab per
    // tab-kinds so the editor toolbar + status bar hide on activation.
    void services.openAppPageTab("home", {});

    // status: autosave-recover-cmd, autosave-recovery-auto-restore,
    // autosave-tab-state-silent-restore
    // Stop any prior vault's tick before swapping; restart against the new
    // vault. Recovery modal first (load any unsaved buffers from the last
    // session); on resolve, silently load the tab-state snapshot and
    // reopen the saved tabs in order.
    const autosave = controllers.autosave.get();
    autosave.stop();
    await runAutosaveRecoveryAndRestore();
    autosave.start();
  }

  /// status: autosave-recover-cmd, autosave-recovery-auto-restore,
  /// autosave-tab-state-silent-restore
  async function runAutosaveRecoveryAndRestore(): Promise<void> {
    const editor = controllers.editorPane.get().host;
    const openBuffers = getOpenBuffers();
    const autosave = controllers.autosave.get();
    let recovered: Awaited<ReturnType<typeof Ipc.autosaveRecover>> = [];
    try {
      recovered = await Ipc.autosaveRecover();
    } catch (err) {
      Logger.error("ui::app", "autosave_recover failed", { err });
    }
    // status: autosave-recovery-auto-restore
    // No prompt — every recovered buffer auto-opens as a sticky tab
    // carrying the autosaved content. For files still on disk the buffer
    // reads dirty (autosaved bytes vs. on-disk loadedText) so the user
    // sees the unsaved work and decides whether to save or revert via the
    // normal save / discard surfaces. For deleted files the autosaved
    // bytes are written back to disk first (so the file exists for the
    // editor to open) and the buffer comes up clean.
    for (const entry of recovered) {
      try {
        if (entry.on_disk_hash === null) {
          await Ipc.writeFile({
            rel: entry.path,
            contents: entry.autosaved_content,
            extraMetadata: null,
          });
          await services.openFile(entry.path, { preview: false });
        } else {
          await services.openFile(entry.path, { preview: false });
          const buffer = getBuffer();
          if (buffer && buffer.path === entry.path) {
            editor.dispatch({
              changes: {
                from: 0,
                to: editor.getDocLength(),
                insert: entry.autosaved_content,
              },
            });
          }
        }
        // Autosaved copy is now live in memory — drop the sidecar.
        await autosave.discard(entry.path);
      } catch (err) {
        Logger.error("ui::app", "autosave restore failed", {
          path: entry.path,
          err,
        });
      }
    }

    // Tab-state restore — silent, even when the recovery modal had nothing
    // to surface. Reopens saved tabs in order, then activates active_path,
    // then opens preview_path if set and not already in the open set.
    // Failures (paths gone from disk) are dropped silently per spec.
    let tabState: Awaited<ReturnType<typeof Ipc.autosaveLoadTabState>> = null;
    try {
      tabState = await Ipc.autosaveLoadTabState();
    } catch (err) {
      Logger.error("ui::app", "autosave_load_tab_state failed", { err });
    }
    if (!tabState) return;
    const alreadyOpen = new Set(openBuffers.keys());
    const kinds = tabState.open_tab_kinds ?? {};
    for (const path of tabState.open_paths) {
      if (alreadyOpen.has(path)) continue;
      // status: tab-kinds — __hiker:* sentinels are app-page tabs, not
      // files. Restore them via openAppPageTab instead of openFile.
      if (path.startsWith("__hiker:")) {
        const kind = kinds[path] || "";
        if (kind === "home") {
          void services.openAppPageTab("home", {});
        } else if (kind === "home-detail") {
          void services.openAppPageTab("home-detail", {});
        } else if (kind === "queue") {
          void services.openAppPageTab("queue", {});
        } else if (kind === "settings") {
          void services.openAppPageTab("settings", {});
        } else if (kind === "agent") {
          const sessionId = path.replace(/^__hiker:agent:?/, "") || undefined;
          if (sessionId) {
            void services.openAgentTab(sessionId);
          }
        } else if (kind === "properties") {
          const rel = path.replace(/^__hiker:properties:/, "");
          if (rel) services.openPropertiesTab(rel);
        } else if (kind === "cluster-review") {
          // status: cluster-review-tab-no-persistence-until-confirm
          // Re-derive the purpose from the synthetic path key. The
          // in-memory structural result is NOT persisted; the user lands
          // on the configure phase with defaults.
          const rest = path.replace(/^__hiker:cluster-review:/, "");
          if (rest === "new-tree") {
            services.openClusterReviewTab({ kind: "new-tree" });
          } else if (rest.startsWith("recluster-subtree:")) {
            const [_, treeId, nodeId] = rest.split(":");
            if (treeId && nodeId) {
              services.openClusterReviewTab({ kind: "recluster-subtree", treeId, nodeId });
            }
          } else if (rest.startsWith("rebuild:")) {
            const treeId = rest.slice("rebuild:".length);
            if (treeId) services.openClusterReviewTab({ kind: "rebuild", treeId });
          }
        } else if (kind === "cluster-pane") {
          // status: cluster-editor-pane-mode
          // Same shape as the other synthetic tab kinds — re-open the
          // cluster-editor pane for the tree id encoded in the sentinel
          // (`__hiker:cluster-pane:<treeId>`). The pane defaults back to
          // the `cluster-tree` sub-state on restore; the user can re-enter
          // batch-review via Apply or by clicking a tree state pill that's
          // already `applied`. Persisting the live sub-state would require
          // widening the autosave payload — out of scope.
          //
          // Tolerate the legacy `cluster-batch-review:` prefix so autosave
          // state from earlier sessions still restores; can be dropped
          // after the next vault.
          const treeId = path
            .replace(/^__hiker:cluster-pane:/, "")
            .replace(/^__hiker:cluster-batch-review:/, "");
          if (treeId) void services.openClusterTab(treeId);
        }
        continue;
      }
      try {
        await services.openFile(path, { preview: false });
        alreadyOpen.add(path);
      } catch (err) {
        Logger.error("ui::app", "autosave tab restore: skipping path", {
          path,
          err,
        });
      }
    }
    if (tabState.active_path && openBuffers.has(tabState.active_path)) {
      services.activateTabInner(tabState.active_path);
    }
    if (
      tabState.preview_path
      && !openBuffers.has(tabState.preview_path)
    ) {
      try {
        await services.openFile(tabState.preview_path, { preview: true });
      } catch (err) {
        Logger.error("ui::app", "autosave preview-slot restore failed", {
          path: tabState.preview_path,
          err,
        });
      }
    }
  }

  return applyOpenedVault;
}
