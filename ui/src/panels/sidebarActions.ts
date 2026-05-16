// status: trails-default-location
// status: sidebar-new-item-button (Trails-mode + Clusters-mode branches)
// status: cluster-editor-mode-menu
//
// Mode-aware `+` button and `…` overflow menu hijack for the sidebar.
// Capture-phase + `stopImmediatePropagation` preempts the tree module's
// own listeners for non-Files modes. Reads singletons directly.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { activeTrailStore } from "../app/state";
import { dom } from "../app/dom";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export function installSidebarActions(): void {
  const newNoteBtn = dom().tree.newNoteBtn;
  const sidebarActionsBtn = dom().tree.sidebarActionsBtn;

  newNoteBtn.addEventListener(
    "click",
    (e) => {
      const sidebarMode = controllers.sidebarMode.get().getSidebarMode();
      if (sidebarMode === "files") return;
      e.stopImmediatePropagation();
      // status: cluster-editor-new-tree-action — `+` in clusters mode opens
      // the New-tree modal (the "Suggest reorganization" entry point).
      if (sidebarMode === "clusters") {
        controllers.clusterEditor.tryGet()?.newTree();
        return;
      }
      if (sidebarMode !== "trails") return;
      void (async () => {
        let created: { trail_doc_rel: string; trail_id: string };
        try {
          created = await Ipc.trailCreate({ name: "new-trail" });
        } catch (err) {
          Logger.error("ui::trails", "trail_create failed", { err });
          showToast(`Couldn't create trail: ${services.formatError(err)}`);
          return;
        }
        try {
          await Ipc.trailSetActive({ trailDocRel: created.trail_doc_rel });
          activeTrailStore.set({ rel: created.trail_doc_rel });
        } catch (err) {
          Logger.error("ui::trails", "trail_set_active failed", { err });
        }
        try {
          await services.openFile(created.trail_doc_rel, { preview: false });
        } catch (err) {
          Logger.error("ui::trails", "open new trail-doc failed", { err });
        }
        const tree = controllers.tree.get();
        await tree.api.refreshTrailDocSet();
        await tree.api.refresh();
        await tree.api.revealPath(created.trail_doc_rel);
        await tree.api.beginInlineRenameByPath(created.trail_doc_rel);
        void controllers.trailsPanel.tryGet()?.api.refresh();
      })();
    },
    true, // capture phase — runs before the tree module's bubbled listener
  );

  // status: cluster-editor-mode-menu
  // `…` overflow is mode-aware. In clusters mode it routes to the cluster
  // editor's mode menu (New tree / Discard drafts / Refresh) rather than
  // the file-tree's Refresh / Reindex entries.
  sidebarActionsBtn.addEventListener(
    "click",
    (e) => {
      if (controllers.sidebarMode.get().getSidebarMode() !== "clusters") return;
      e.stopImmediatePropagation();
      const ce = controllers.clusterEditor.tryGet();
      if (ce) ce.openModeMenu(sidebarActionsBtn);
    },
    true,
  );
}
