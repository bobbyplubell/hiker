// status: tree-trash-bin
// status: tree-trash-disk-listing
// status: tree-trash-flat-by-deleted
// status: tree-trash-preview
// status: tree-trash-restore-action
// status: tree-trash-empty-action
// status: tree-trash-orphan-recovery
//
// Sidebar trash bin (collapsible row pinned to the bottom of the file tree)
// + per-row context menu (Restore / Delete permanently) + read-only preview
// buffer. The preview reuses the buffer-mode union's `{kind: "trash"}`
// variant so save / dirty / file-switch guards take the read-only path.

import { listen } from "@tauri-apps/api/event";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import type { SettingsManager } from "../settings/manager";
import type { EditorHost } from "../app/editor";

import {
  openContextMenu,
  type CtxMenuItem,
} from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { confirm3, confirmDanger } from "../widgets/confirm";
import {
  createPanelController,
  type PanelController,
} from "../panels/controller";

export interface TrashListItem {
  id: string | null;
  trashed_name: string;
  original_path: string | null;
  deleted_at: number;
  kind: "file" | "folder";
  member_count: number | null;
  orphaned: boolean;
}

interface BufferLike {
  path: string;
  loadedText: string;
  /// Trash previews are read-only (no commits), so the token slot is
  /// always `null`. Kept here so the host's `Buffer` interface and
  /// this module's `BufferLike` shape stay structurally compatible.
  token: unknown | null;
  mode: { kind: string } & Record<string, unknown>;
  kind?: string;
}

export interface TrashDeps {
  binEl: HTMLElement;
  headerEl: HTMLElement;
  listEl: HTMLElement;
  chevronEl: HTMLElement;
  labelEl: HTMLElement;
  editor: EditorHost;
  getBuffer: () => BufferLike | null;
  setBuffer: (b: BufferLike | null) => void;
  cssEscape: (s: string) => string;
  isVaultIsOpen: () => boolean;
  // Routes the `vault.trash_expanded` write through the host's
  // `SettingsManager` instead of a bespoke `persistSetting` closure.
  settings: SettingsManager;
  isVaultHomeVisible: () => boolean;
  setVaultHomeVisible: (on: boolean) => void;
  refreshTree: () => Promise<void>;
  formatError: (err: unknown) => string;
}

export interface TrashApi {
  refresh(): Promise<void>;
  openPreview(item: TrashListItem): Promise<void>;
  closePreview(): void;
  /// Returns whether the active buffer's trash entry is still on disk.
  /// Used by the existing `hiker:trash-changed` cleanup path.
  items(): TrashListItem[];
}

// Trash is the proof-of-shape migration onto the new
// `PanelController<Api>` shape. The controller's `isVisible` /
// `setVisible` map onto the bin's expanded/collapsed state — clicking the
// header (handled inside the module) and the host-driven flip both route
// through the same toggle so the persisted `vault.trash_expanded` setting
// stays in sync. The other four panels (`tree`, `vaultHome`, `discovery`,
// `queueDetail`) keep their bespoke factory APIs for now and migrate one
// at a time per the bug row's "narrow enough to review" guidance.
export type TrashController = PanelController<TrashApi>;

function relativeTime(unixSecs: number): string {
  const now = Math.floor(Date.now() / 1000);
  const d = now - unixSecs;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  if (d < 86400 * 2) return "yesterday";
  if (d < 86400 * 7) return `${Math.floor(d / 86400)}d ago`;
  const date = new Date(unixSecs * 1000);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function mountTrash(deps: TrashDeps): TrashController {
  let trashItems: TrashListItem[] = [];

  async function refresh(): Promise<void> {
    if (!deps.isVaultIsOpen()) return;
    try {
      trashItems = await Ipc.listTrash();
    } catch (err) {
      Logger.error("ui::trash", "list_trash failed", { err });
      trashItems = [];
    }
    render();
  }

  function render(): void {
    const n = trashItems.length;
    deps.labelEl.textContent = n === 0 ? "Trash" : `Trash (${n})`;
    deps.listEl.innerHTML = "";
    for (const item of trashItems) {
      const row = document.createElement("div");
      row.className = "trash-row";
      if (item.orphaned) row.classList.add("orphaned");
      row.dataset.trashedName = item.trashed_name;

      const main = document.createElement("div");
      main.className = "trash-row-main";
      const glyph = item.kind === "folder" ? "▸ " : "  ";
      const display = item.original_path
        ? (item.original_path.split("/").pop() ?? item.original_path)
        : item.trashed_name;
      let label = `${glyph}${display}`;
      if (item.kind === "folder") {
        const count =
          item.member_count === null ? "?" : String(item.member_count);
        label += ` (${count} note${item.member_count === 1 ? "" : "s"})`;
      }
      main.textContent = label;
      row.appendChild(main);

      const meta = document.createElement("div");
      meta.className = "trash-row-meta";
      const when = relativeTime(item.deleted_at);
      const orig = item.original_path ?? "(no original location recorded)";
      meta.textContent = `${when} · ${orig}`;
      row.appendChild(meta);

      if (item.kind === "file") {
        row.addEventListener("click", () => void openPreview(item));
      }
      row.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        e.stopPropagation();
        openRowMenu(e.clientX, e.clientY, item);
      });

      deps.listEl.appendChild(row);
    }
  }

  function openRowMenu(x: number, y: number, item: TrashListItem): void {
    const items: CtxMenuItem[] = [];
    if (item.id) {
      items.push({
        label: "Restore",
        run: async () => {
          try {
            const restored = await Ipc.restoreTrashEntry({ id: item.id });
            showToast(`Restored ${restored.original_path}`);
            await deps.refreshTree();
          } catch (err) {
            Logger.error("ui::trash", "restore_trash_entry failed", { err });
            alert(`restore failed: ${deps.formatError(err)}`);
          }
        },
      });
    } else {
      items.push({
        label: "Restore (no original location recorded)",
        disabled: true,
      });
    }
    items.push({
      label: "Delete permanently",
      danger: true,
      run: async () => {
        const target = item.original_path ?? item.trashed_name;
        const ok = await confirmDanger(
          `Permanently delete ${target}? This cannot be undone.`,
          "Delete permanently",
        );
        if (!ok) return;
        try {
          await Ipc.permanentDeleteTrashEntry({
            trashedName: item.trashed_name,
          });
        } catch (err) {
          Logger.error("ui::trash", "permanent_delete_trash_entry failed", {
            err,
          });
          alert(`delete failed: ${deps.formatError(err)}`);
        }
      },
    });
    openContextMenu(x, y, items);
  }

  async function openPreview(item: TrashListItem): Promise<void> {
    const buffer = deps.getBuffer();
    if (buffer && deps.editor.isDirty()) {
      const choice = await confirm3(
        `${buffer.path} has unsaved changes.`,
        "Save & switch",
        "Discard & switch",
        "Cancel",
      );
      if (choice === "cancel") return;
      if (choice === "a") {
        const ok = await deps.editor.save();
        if (!ok) return;
      }
    }
    const trashRel = `.hiker/trash/${item.trashed_name}`;
    try {
      // Trash entries are read-only previews. We just need the bytes;
      // the file lives at a fixed `.hiker/trash/...` path, so a plain
      // read suffices and no token is minted (the buffer's token is
      // always `null` for non-file buffer modes).
      const contents = await Ipc.readFile({ rel: trashRel });
      deps.setBuffer(null);
      deps.editor.dispatch({
        changes: {
          from: 0,
          to: deps.editor.getDocLength(),
          insert: contents,
        },
        effects: [
          deps.editor.language.reconfigure(
            deps.editor.languageExtensionForPath(item.original_path ?? item.trashed_name),
          ),
          deps.editor.livePreviewCompartment.reconfigure(
            deps.editor.livePreviewExtensionForPath(
              item.original_path ?? item.trashed_name,
            ),
          ),
        ],
      });
      if (deps.isVaultHomeVisible()) deps.setVaultHomeVisible(false);
      deps.setBuffer({
        path: trashRel,
        loadedText: deps.editor.getActiveText(),
        token: null,
        kind: "buffer",
        mode: {
          kind: "trash",
          displayPath: item.original_path ?? item.trashed_name,
        },
      });
      deps.editor.setReadOnly(true);
      document
        .querySelectorAll("#tree li.active")
        .forEach((el) => el.classList.remove("active"));
      document
        .querySelectorAll(".trash-row.active")
        .forEach((el) => el.classList.remove("active"));
      const row = deps.listEl.querySelector(
        `.trash-row[data-trashed-name="${deps.cssEscape(item.trashed_name)}"]`,
      );
      row?.classList.add("active");
      deps.editor.updateStatus();
      deps.editor.refreshChunkBoundaries();
    } catch (err) {
      Logger.error("ui::trash", "openTrashPreview failed", { err });
      alert(`preview failed: ${deps.formatError(err)}`);
    }
  }

  function closePreview(): void {
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "trash") return;
    deps.setBuffer(null);
    deps.editor.dispatch({
      changes: { from: 0, to: deps.editor.getDocLength(), insert: "" },
    });
    deps.editor.setReadOnly(false);
    document
      .querySelectorAll(".trash-row.active")
      .forEach((el) => el.classList.remove("active"));
    deps.editor.updateStatus();
  }

  // Header click → flip the controller's visibility, which owns the
  // `.collapsed` toggle + chevron + persisted `vault.trash_expanded`
  // write through the shared setter below. One toggle path for both the
  // user click and any future host-driven flip (e.g. a "show trash" verb
  // restoring previous state on vault open).
  deps.headerEl.addEventListener("click", () => {
    controller.setVisible(!controller.isVisible());
  });

  // Header right-click → Empty trash.
  deps.headerEl.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const n = trashItems.length;
    openContextMenu(e.clientX, e.clientY, [
      {
        label: n === 0 ? "Empty trash" : `Empty trash (${n} entries)`,
        danger: true,
        disabled: n === 0,
        run: async () => {
          const ok = await confirmDanger(
            `Permanently delete ${n} trash ${n === 1 ? "entry" : "entries"}? This cannot be undone.`,
            "Empty trash",
          );
          if (!ok) return;
          try {
            await Ipc.emptyTrash();
          } catch (err) {
            Logger.error("ui::trash", "empty_trash failed", { err });
            alert(`empty failed: ${deps.formatError(err)}`);
          }
        },
      },
    ]);
  });

  // Listen for any trash-changing op and re-render. Also clear the preview
  // buffer if the entry being previewed was emptied/restored under us.
  void listen("hiker:trash-changed", async () => {
    await refresh();
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind === "trash") {
      const stillThere = trashItems.some(
        (i) => `.hiker/trash/${i.trashed_name}` === buffer.path,
      );
      if (!stillThere) {
        deps.setBuffer(null);
        deps.editor.dispatch({
          changes: { from: 0, to: deps.editor.getDocLength(), insert: "" },
        });
        deps.editor.setReadOnly(false);
        deps.editor.updateStatus();
      }
    }
  });

  const api: TrashApi = {
    refresh,
    openPreview,
    closePreview,
    items: () => trashItems,
  };

  // Visibility = "trash bin row expanded" (vs collapsed). The controller's
  // `setVisible` owns the DOM class flip + chevron + persisted setting so
  // both header-click and any host-driven flip share one path.
  const controller = createPanelController<TrashApi>(api, {
    initialVisible: !deps.binEl.classList.contains("collapsed"),
    // DOM already reflects `initialVisible` (host's startup pass set
    // `.collapsed` from the persisted `vault.trash_expanded` value), and
    // the persist write below should fire only on real user toggles —
    // not on mount.
    applyOnMount: false,
    onSetVisible: (on) => {
      deps.binEl.classList.toggle("collapsed", !on);
      deps.chevronEl.textContent = on ? "▾" : "▸";
      if (deps.isVaultIsOpen()) {
        void deps.settings.setVaultSetting("vault.trash_expanded", on);
      }
    },
  });

  return controller;
}
