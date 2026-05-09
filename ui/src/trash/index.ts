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

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { EditorView } from "@codemirror/view";
import type { Compartment, Extension } from "@codemirror/state";

import {
  openContextMenu,
  type CtxMenuItem,
} from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { confirm3, confirmDanger } from "../widgets/confirm";

interface TrashEntry {
  id: string;
  original_path: string;
  trashed_name: string;
  original_mtime: number;
  deleted_at: number;
  kind: "file" | "folder";
  members?: string[] | null;
}

export interface TrashListItem {
  id: string | null;
  trashed_name: string;
  original_path: string | null;
  deleted_at: number;
  kind: "file" | "folder";
  member_count: number | null;
  orphaned: boolean;
}

interface FileWithHash {
  contents: string;
  hash: string;
}

interface BufferLike {
  path: string;
  loadedText: string;
  loadedHash: string;
  mode: { kind: string } & Record<string, unknown>;
}

export interface TrashDeps {
  binEl: HTMLElement;
  headerEl: HTMLElement;
  listEl: HTMLElement;
  chevronEl: HTMLElement;
  labelEl: HTMLElement;
  view: EditorView;
  language: Compartment;
  livePreviewCompartment: Compartment;
  languageExtensionForPath: (rel: string) => Extension;
  livePreviewExtensionForPath: (rel: string) => Extension;
  getBuffer: () => BufferLike | null;
  setBuffer: (b: BufferLike | null) => void;
  setReadOnly: (ro: boolean, mode?: "trash" | "snapshot" | null) => void;
  updateStatus: () => void;
  refreshChunkBoundaries: () => void;
  isDirty: () => boolean;
  save: () => Promise<boolean>;
  cssEscape: (s: string) => string;
  isVaultIsOpen: () => boolean;
  persistSetting: (
    scope: "user" | "vault",
    key: string,
    value: unknown,
  ) => Promise<void>;
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

export function mountTrash(deps: TrashDeps): TrashApi {
  let trashItems: TrashListItem[] = [];

  async function refresh(): Promise<void> {
    if (!deps.isVaultIsOpen()) return;
    try {
      trashItems = await invoke<TrashListItem[]>("list_trash");
    } catch (err) {
      console.error("list_trash failed:", err);
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
            const restored = await invoke<TrashEntry>("restore_trash_entry", {
              id: item.id,
            });
            showToast(`Restored ${restored.original_path}`);
            await deps.refreshTree();
          } catch (err) {
            console.error("restore_trash_entry failed:", err);
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
          await invoke("permanent_delete_trash_entry", {
            trashedName: item.trashed_name,
          });
        } catch (err) {
          console.error("permanent_delete_trash_entry failed:", err);
          alert(`delete failed: ${deps.formatError(err)}`);
        }
      },
    });
    openContextMenu(x, y, items);
  }

  async function openPreview(item: TrashListItem): Promise<void> {
    const buffer = deps.getBuffer();
    if (buffer && deps.isDirty()) {
      const choice = await confirm3(
        `${buffer.path} has unsaved changes.`,
        "Save & switch",
        "Discard & switch",
        "Cancel",
      );
      if (choice === "cancel") return;
      if (choice === "a") {
        const ok = await deps.save();
        if (!ok) return;
      }
    }
    const trashRel = `.hiker/trash/${item.trashed_name}`;
    try {
      const file = await invoke<FileWithHash>("read_file_with_hash", {
        rel: trashRel,
      });
      deps.setBuffer(null);
      deps.view.dispatch({
        changes: {
          from: 0,
          to: deps.view.state.doc.length,
          insert: file.contents,
        },
        effects: [
          deps.language.reconfigure(
            deps.languageExtensionForPath(item.original_path ?? item.trashed_name),
          ),
          deps.livePreviewCompartment.reconfigure(
            deps.livePreviewExtensionForPath(
              item.original_path ?? item.trashed_name,
            ),
          ),
        ],
      });
      if (deps.isVaultHomeVisible()) deps.setVaultHomeVisible(false);
      deps.setBuffer({
        path: trashRel,
        loadedText: deps.view.state.doc.toString(),
        loadedHash: file.hash,
        mode: {
          kind: "trash",
          displayPath: item.original_path ?? item.trashed_name,
        },
      });
      deps.setReadOnly(true, "trash");
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
      deps.updateStatus();
      deps.refreshChunkBoundaries();
    } catch (err) {
      console.error("openTrashPreview failed:", err);
      alert(`preview failed: ${deps.formatError(err)}`);
    }
  }

  function closePreview(): void {
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "trash") return;
    deps.setBuffer(null);
    deps.view.dispatch({
      changes: { from: 0, to: deps.view.state.doc.length, insert: "" },
    });
    deps.setReadOnly(false);
    document
      .querySelectorAll(".trash-row.active")
      .forEach((el) => el.classList.remove("active"));
    deps.updateStatus();
  }

  // Header click → toggle collapsed; persist new state.
  deps.headerEl.addEventListener("click", () => {
    deps.binEl.classList.toggle("collapsed");
    const expanded = !deps.binEl.classList.contains("collapsed");
    deps.chevronEl.textContent = expanded ? "▾" : "▸";
    if (deps.isVaultIsOpen()) {
      void deps.persistSetting("vault", "vault.trash_expanded", expanded);
    }
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
            await invoke("empty_trash");
          } catch (err) {
            console.error("empty_trash failed:", err);
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
        deps.view.dispatch({
          changes: { from: 0, to: deps.view.state.doc.length, insert: "" },
        });
        deps.setReadOnly(false);
        deps.updateStatus();
      }
    }
  });

  return {
    refresh,
    openPreview,
    closePreview,
    items: () => trashItems,
  };
}
