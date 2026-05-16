// status: watcher-editor-reload-clean
// status: watcher-editor-conflict-dirty
// status: watcher-editor-deleted-buffer
// status: watcher-editor-renamed-followup
// status: trails-mode-body, trail-row-icon
// status: trail-add-to-active-from-editor-verb
//
// Filesystem watcher → editor integration. Reacts to external changes to
// the active buffer's file (silent reload / conflict modal / deletion /
// rename), kicks tree refreshes, refreshes trail panel / membership /
// trail-doc set. Reads singletons directly.

import { onHikerEvent } from "../events";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import {
  activeTrailStore,
  getBuffer,
  getPreviewTabPath,
  getOpenBuffers,
  setBufferState,
} from "./state";
import { refreshActiveTrailWaypointPaths } from "../trails/membership";
import { controllers } from "./controllers";
import { services } from "./services";

export function installFileWatcher(): void {
  let watcherConflictPromptOpen = false;

  void onHikerEvent("hiker:file-changed", async (payload) => {
    const ev = payload;
    const tree = controllers.tree.get();
    const trailsPanel = controllers.trailsPanel.tryGet();
    const vaultHome = controllers.vaultHome.get();
    const tabStrip = controllers.tabStrip.get();
    const autosave = controllers.autosave.get();
    const editor = controllers.editorPane.get().host;
    // status: trails-mode-body — refresh the trails panel when a trail
    // doc or any `.hiker/trails/<id>/waypoints/` path is touched. Cheap
    // path-prefix check; the panel's internal epoch counter drops stale
    // fetches if the user is mid-refresh. Also refresh on any `.md`
    // change outside `.hiker/` — a non-active trail-doc could have been
    // created/renamed/deleted (acquiring or losing `hiker.kind: trail`
    // frontmatter), and the trails-list cache must reflect that or the
    // dropdown lists stale entries (and activating a deleted trail
    // errors). Mirrors the conservative posture used by the
    // `trail-row-icon` block below; trails-list is a small per-vault
    // query so the extra calls are acceptable.
    {
      const paths =
        ev.kind === "renamed" ? [ev.from, ev.to] : [ev.path];
      const activeTrailRel = activeTrailStore.get().rel;
      const looksLikeWaypoint = paths.some((p) =>
        p.startsWith(".hiker/trails/"),
      );
      const matchesActiveTrail =
        activeTrailRel !== null && paths.includes(activeTrailRel);
      const isMdOutsideHikerForTrails = paths.some(
        (p) => p.endsWith(".md") && !p.startsWith(".hiker/"),
      );
      if (looksLikeWaypoint || matchesActiveTrail || isMdOutsideHikerForTrails) {
        void trailsPanel?.api.refresh();
      }
      // status: trail-add-to-active-from-editor-verb — keep the
      // membership cache fresh so the editor pill and the tree verb
      // flip to "Already in this trail" without a manual refresh
      // when a new waypoint of the active trail lands (or the active
      // trail-doc itself changes shape via a frontmatter edit).
      // Conservative: any `.hiker/trails/` event refreshes (we don't
      // pre-narrow to the active trail's id since the cost is one
      // `trail_get` call); active-trail-doc edits also refresh.
      if (
        activeTrailRel !== null
        && (matchesActiveTrail || looksLikeWaypoint)
      ) {
        void refreshActiveTrailWaypointPaths();
      }
      // status: trail-row-icon — any `.md` change outside `.hiker/` may
      // have added or removed `hiker.kind: trail` frontmatter, so the
      // tree's cached trail-doc set is potentially stale. Conservative
      // refresh; cheap (single `trails_list` call) and only triggers a
      // tree repaint if the set actually changed.
      {
        const isMdOutsideHiker = paths.some(
          (p) => p.endsWith(".md") && !p.startsWith(".hiker/"),
        );
        const isTrailDoc = paths.some((p) => p.startsWith(".hiker/trails/"));
        if (isMdOutsideHiker || isTrailDoc) {
          void tree.api.refreshTrailDocSet();
        }
      }
    }
    // Tree shape changes don't depend on which buffer (if any) is active.
    // Schedule before buffer mutations so the rebuild reads the post-update
    // `buffer.path` (matters for the renamed branch's silent path follow).
    if (ev.kind === "created" || ev.kind === "deleted" || ev.kind === "renamed") {
      services.scheduleTreeRefreshFromWatcher();
      // status: vault-home-recent-modified — tree-shape changes can shift
      // which notes are in the top-N; modified-only events update mtimes.
      // External edits don't ride core::changes (deferred per `changes-write-path`
      // notes), so the watcher path keeps refreshing the recents widget directly
      // for that case. Internal saves are covered by `hiker:changes-appended` →
      // `refreshOnChangesAppended` upstream.
      vaultHome.api.notifyRecentModified();
    } else if (
      ev.kind === "modified"
      && (tree.api.getSortOrder() === "mtime-newest" || tree.api.getSortOrder() === "mtime-oldest")
    ) {
      // Tree *shape* doesn't change on Modified, but mtime-based sort orders
      // depend on per-entry mtime — a save reorders rows. Schedule a refresh
      // only when the chosen sort actually consumes mtime; under name sorts
      // we keep the existing no-op behavior.
      services.scheduleTreeRefreshFromWatcher();
    }
    if (ev.kind === "modified") {
      vaultHome.api.notifyRecentModified();
    }
    // Don't react while previewing a trash entry or a snapshot — both are
    // read-only views; mutating them would corrupt the user's intent. Trash
    // entries live under .hiker/trash/ which the watcher ignores anyway, but
    // snapshot previews share the live file path so this guard is the only
    // thing keeping a watcher event from clobbering the historic content.
    const buffer = getBuffer();
    if (!buffer || services.isReadOnlyBuffer(buffer)) return;

    if (ev.kind === "modified" && ev.path === buffer.path) {
      if (services.isDirty()) {
        if (watcherConflictPromptOpen) return;
        watcherConflictPromptOpen = true;
        try {
          await services.handleWatcherConflictDirty(buffer.path);
        } finally {
          watcherConflictPromptOpen = false;
        }
        return;
      }
      try {
        // Buffer is clean — silent reload via `open_for_edit` reseeds the
        // doc + rotates the token.
        const fresh = await Ipc.openForEdit({ rel: buffer.path });
        editor.dispatch({
          changes: { from: 0, to: editor.getDocLength(), insert: fresh.contents },
        });
        const cur = getBuffer();
        if (cur) {
          cur.loadedText = editor.getActiveText();
          cur.token = fresh.token;
          services.updateStatus();
          services.scheduleChunkBoundariesRefresh(500);
        }
      } catch (err) {
        Logger.error("ui::app", "silent reload failed", { err });
      }
      return;
    }

    if (ev.kind === "deleted" && ev.path === buffer.path) {
      const path = buffer.path;
      if (services.isDirty()) {
        showToast(`${path} was removed on disk; save to recreate.`);
      } else {
        // status: editor-tab-strip — drop the tab for the removed path.
        getOpenBuffers().delete(path);
        const previewTabPath = getPreviewTabPath();
        setBufferState({
          buffer: null,
          activePath: null,
          ...(previewTabPath === path ? { previewTabPath: null } : {}),
        });
        editor.dispatch({ changes: { from: 0, to: editor.getDocLength(), insert: "" } });
        services.updateStatus();
        tabStrip.render();
        // status: autosave-write-tick — clean buffer dropped, autosave
        // entry is no longer relevant.
        autosave.clearPath(path);
        showToast(`${path} was removed externally`);
      }
      return;
    }

    if (ev.kind === "renamed" && ev.from === buffer.path) {
      const oldPath = buffer.path;
      buffer.path = ev.to;
      services.updateStatus();
      // status: autosave-rename-clear-old — drop the autosave entry for
      // the old path; the next tick writes against the new path naturally.
      autosave.onRenamed(oldPath, ev.to);
      return;
    }
  });
}
