// status: navigation-history-stack
// status: top-strip-back-button, top-strip-forward-button
// status: navigation-trackpad-swipe, navigation-keybind
//
// Host wiring for the navigation history stack. Reads singletons directly.

import {
  mountNavigation,
  installNavigationSwipe,
  type NavApi,
  type NavState,
} from "./index";
import { getBuffer, getActivePath, getOpenBuffers } from "../app/state";
import { dom } from "../app/dom";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export interface NavigationSetupApi {
  nav: NavApi;
  checkpointNav(): void;
  paintNavButtons(): void;
  installSnapshotWrappers(): void;
  installTrashWrappers(): void;
}

export function setupNavigation(): NavigationSetupApi {
  const navBackBtn = dom().vaultBar.navBackBtn;
  const navForwardBtn = dom().vaultBar.navForwardBtn;

  function inferNavState(): NavState {
    const buf = getBuffer();
    // status: tab-kinds — non-buffer ("app-page") tab kinds derive their
    // NavState from the active tab's `(kind, payload)` rather than from
    // legacy DOM `hidden` / class flips. The synthetic tab key
    // `__hiker:<kind>[:<payload>]` carries the payload so we don't have to
    // probe each app-page module for its own active-view state.
    if (buf && buf.kind !== "buffer") {
      if (buf.kind === "home") return { kind: "home" };
      if (buf.kind === "home-detail") {
        // Parse the view discriminator out of the synthetic key
        // (`__hiker:home-detail:<view>`); fall back to the only currently
        // valid view if the prefix is malformed.
        let view: "recent-activity" = "recent-activity";
        const prefix = "__hiker:home-detail:";
        if (buf.path.startsWith(prefix)) {
          const raw = buf.path.slice(prefix.length);
          if (raw === "recent-activity") view = raw;
        }
        return { kind: "home-detail", view };
      }
      if (buf.kind === "queue") return { kind: "queue-detail" };
      if (buf.kind === "settings") return { kind: "settings" };
      if (buf.kind === "properties") {
        const prefix = "__hiker:properties:";
        const rel = buf.path.startsWith(prefix) ? buf.path.slice(prefix.length) : buf.path;
        return { kind: "properties", path: rel };
      }
      // `agent` / `graph` (future) — for now treat the synthetic key as a
      // tab path so applyNavState's `tab` branch handles activation when
      // the tab still exists. Reopening these kinds after eviction is a
      // followup once they have their own restore paths.
      return { kind: "tab", path: buf.path };
    }
    if (buf && buf.mode.kind === "trash") {
      const trashedName = buf.path.replace(/^\.hiker\/trash\//, "");
      return { kind: "trash-preview", trashedName };
    }
    if (buf && buf.mode.kind === "snapshot") {
      return {
        kind: "snapshot-preview",
        changeId: buf.mode.changeId,
        row: buf.mode.row,
      };
    }
    if (buf && buf.mode.kind === "write-note-review") {
      return {
        kind: "staging-preview",
        proposalId: buf.mode.proposal_id,
        targetPath: buf.mode.targetPath,
      };
    }
    const activePath = getActivePath();
    if (
      activePath !== null
      && buf
      && (buf.mode.kind === "file" || buf.mode.kind === "patch-review")
    ) {
      return { kind: "tab", path: activePath };
    }
    return { kind: "empty" };
  }

  async function applyNavState(s: NavState): Promise<boolean> {
    switch (s.kind) {
      case "tab": {
        if (!getOpenBuffers().has(s.path)) return false;
        services.activateTabInner(s.path);
        return true;
      }
      case "home": {
        services.openAppPageTab("home", {});
        return true;
      }
      case "home-detail": {
        services.openAppPageTab("home-detail", { view: s.view });
        return true;
      }
      case "queue-detail": {
        services.openAppPageTab("queue", {});
        return true;
      }
      case "settings": {
        services.openAppPageTab("settings", {});
        return true;
      }
      case "properties": {
        services.openPropertiesTab(s.path);
        return true;
      }
      case "trash-preview": {
        const trash = controllers.trash.get();
        const item = trash.api.items().find((i) => i.trashed_name === s.trashedName);
        if (!item) return false;
        await trash.api.openPreview(item);
        return true;
      }
      case "snapshot-preview": {
        await controllers.snapshotPreview.get().open(s.row);
        return true;
      }
      case "staging-preview": {
        await services.openProposalReview({ id: s.proposalId, target_path: s.targetPath });
        return true;
      }
      case "empty": {
        return false;
      }
    }
  }

  function paintNavButtons(): void {
    navBackBtn.disabled = !nav!.canBack();
    navForwardBtn.disabled = !nav!.canForward();
  }

  const nav: NavApi = mountNavigation({
    inferCurrent: inferNavState,
    apply: applyNavState,
    onChange: paintNavButtons,
  });

  navBackBtn.addEventListener("click", () => {
    void nav!.back();
  });
  navForwardBtn.addEventListener("click", () => {
    void nav!.forward();
  });

  function checkpointNav(): void {
    nav.checkpoint();
  }

  // status: navigation-history-stack
  // Snapshot preview replaces the singleton `buffer` without mutating any
  // observed DOM attribute, so the MutationObserver can't detect the
  // transition. Wrap its openers to checkpoint after the buffer flip lands.
  // Trash gets the same treatment. The wrappers also fire on back/forward
  // apply, where the nav module's `restoring` flag turns the checkpoint
  // into a no-op.
  function installSnapshotWrappers(): void {
    const snapshotPreview = controllers.snapshotPreview.get();
    const _snapOpen = snapshotPreview.open;
    snapshotPreview.open = async (row) => {
      await _snapOpen.call(snapshotPreview, row);
      checkpointNav();
    };
    const _snapClose = snapshotPreview.close;
    snapshotPreview.close = () => {
      _snapClose.call(snapshotPreview);
      checkpointNav();
    };
  }

  function installTrashWrappers(): void {
    const trash = controllers.trash.get();
    const _trashOpen = trash.api.openPreview;
    trash.api.openPreview = async (item) => {
      await _trashOpen.call(trash.api, item);
      checkpointNav();
    };
    const _trashClose = trash.api.closePreview;
    trash.api.closePreview = () => {
      _trashClose.call(trash.api);
      checkpointNav();
    };
  }

  // status: navigation-trackpad-swipe
  // Two-finger horizontal trackpad swipe → back/forward. Right-swipe = back,
  // left-swipe = forward (browser convention). Threshold ~120px accumulated
  // `deltaX`. See `navigation/index.ts` for the wheel-event heuristic.
  installNavigationSwipe({
    back: () => void nav!.back(),
    forward: () => void nav!.forward(),
  });

  return {
    nav,
    checkpointNav,
    paintNavButtons,
    installSnapshotWrappers,
    installTrashWrappers,
  };
}
