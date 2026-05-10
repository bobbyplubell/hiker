// status: drag-and-drop-move
// status: tree-refresh-manual
// status: tree-refresh-watcher
// status: tree-double-click-rename
// status: tree-context-menu
// status: tree-context-delete
// status: tree-toolbar-actions-menu
// status: tree-sort-options
// status: tree-row-unsupported-marker
// status: tree-row-skipped-marker
// status: tree-row-queued-marker
// status: create-note-button
//
// Sidebar tree panel — rendering, dnd, inline rename, context menu, sort
// menu, watcher reconciliation, index-state markers. The module owns its
// own state (expanded folders, sort order, debounce, index-state cache);
// host wires DOM ids and editor-coupled callbacks through `deps`.

import { Ipc } from "../ipc";
import { Logger } from "../logger";
import {
  createPanelController,
  type PanelController,
  type PanelDeps,
} from "../panels/controller";
import { Classes, IX_STATE_CLASSES } from "../style/classes";

import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { confirmDanger } from "../widgets/confirm";

export type EntryKind = "dir" | "file";
export interface DirEntry {
  name: string;
  rel_path: string;
  kind: EntryKind;
  mtime: number;
}

export type IndexState =
  | { kind: "indexed" }
  | { kind: "unsupported" }
  | { kind: "skipped"; reason: string }
  | { kind: "queued" };

export type TreeSortOrder =
  | "name-asc"
  | "name-desc"
  | "mtime-newest"
  | "mtime-oldest";

type SortByConfig = "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc";

export function sortOrderFromSettings(s: SortByConfig): TreeSortOrder {
  switch (s) {
    case "name_asc": return "name-asc";
    case "name_desc": return "name-desc";
    case "mtime_desc": return "mtime-newest";
    case "mtime_asc": return "mtime-oldest";
  }
}

export function sortOrderToSettings(o: TreeSortOrder): SortByConfig {
  switch (o) {
    case "name-asc": return "name_asc";
    case "name-desc": return "name_desc";
    case "mtime-newest": return "mtime_desc";
    case "mtime-oldest": return "mtime_asc";
  }
}

function sortOrderLabel(order: TreeSortOrder): string {
  switch (order) {
    case "name-asc": return "Name (A→Z)";
    case "name-desc": return "Name (Z→A)";
    case "mtime-newest": return "Modified (newest first)";
    case "mtime-oldest": return "Modified (oldest first)";
  }
}

function parentOf(rel: string): string {
  const idx = rel.lastIndexOf("/");
  return idx >= 0 ? rel.slice(0, idx) : "";
}

// Loose buffer shape — modules only read `path` and `mode.kind`; the host's
// richer Buffer satisfies this structurally without coupling.
type BufferLike = { path: string; mode: { kind: string } };

export interface TreeDeps extends PanelDeps {
  treeEl: HTMLElement;
  newNoteBtn: HTMLButtonElement;
  treeActionsBtn: HTMLButtonElement;
  cssEscape: (s: string) => string;
  getBuffer: () => BufferLike | null;
  /// True when buffer is a read-only preview (snapshot/trash). Used by the
  /// tree-actions menu's "Reindex this file" gate.
  isReadOnlyBuffer: (b: BufferLike | null) => boolean;
  setBufferPath: (newPath: string) => void;
  /// Called when a folder rename's recursive path remap touches the open
  /// buffer's path; host updates `buffer.path` + status.
  isDirty: () => boolean;
  /// Called from `deleteFromTree` to clear the buffer when its path falls
  /// inside the deleted subtree.
  clearOpenBufferIfWithin: (deletedRel: string) => void;
  refreshTrashBin: () => Promise<void>;
  /// Re-render the status-bar index label when the active buffer's index
  /// state resolves lazily.
  renderIndexStatus: () => void;
}

export interface TreeApi {
  refresh(): Promise<void>;
  revealPath(rel: string): Promise<void>;
  setSortOrder(order: TreeSortOrder, persist: boolean): Promise<void>;
  getSortOrder(): TreeSortOrder;
  setSelectedFolder(rel: string): void;
  notifyWatcher(): void;
  getIndexState(rel: string): IndexState | undefined;
  setIndexState(rel: string, state: IndexState): void;
  deleteIndexState(rel: string): void;
  clearCaches(): void;
  fetchIndexState(rel: string): Promise<IndexState>;
}

export type TreeController = PanelController<TreeApi>;

// The tree panel's visibility is sidebar-managed by the host (the
// `appEl.classList` `sidebar-collapsed` flag), not flipped at the panel
// level. The controller exposes `isVisible: () => true; setVisible: noop`
// per the bug row's guidance — the migration here is purely about moving
// the factory's API under `controller.api` and bundling cross-panel
// uniforms (`PanelDeps`).
export function mountTree(deps: TreeDeps): TreeController {
  // Tracks the folder a "+ New note" click should target.
  let selectedFolder = "";

  // Persists folder expansion state across `refreshTree` calls so a delete /
  // rename / refresh doesn't collapse every open folder.
  const expandedFolders = new Set<string>();

  let treeSortOrder: TreeSortOrder = "name-asc";

  // Per-path index-state cache so re-renders don't re-fetch on every paint.
  const indexStateCache = new Map<string, IndexState>();
  const inflightStateFetches = new Set<string>();

  async function fetchIndexState(rel: string): Promise<IndexState> {
    const state = await Ipc.indexStateFor({ rel });
    indexStateCache.set(rel, state);
    return state;
  }

  function applyIndexMarker(li: HTMLElement, state: IndexState | null): void {
    li.classList.remove(...IX_STATE_CLASSES);
    li.removeAttribute("data-ix-reason");
    let marker = li.querySelector<HTMLSpanElement>(":scope > .ix-marker");
    if (state && state.kind !== "indexed") {
      if (!marker) {
        marker = document.createElement("span");
        marker.className = "ix-marker";
        li.append(marker);
      }
    } else if (marker) {
      marker.remove();
    }
    if (!state) {
      li.removeAttribute("title");
      return;
    }
    switch (state.kind) {
      case "unsupported":
        li.classList.add(Classes.IX_UNSUPPORTED);
        li.removeAttribute("title");
        break;
      case "skipped":
        li.classList.add(Classes.IX_SKIPPED);
        li.dataset.ixReason = state.reason;
        li.title = `Skipped — ${state.reason}`;
        break;
      case "queued":
        li.classList.add(Classes.IX_QUEUED);
        li.removeAttribute("title");
        break;
      case "indexed":
        li.classList.add(Classes.IX_INDEXED);
        li.removeAttribute("title");
        break;
    }
  }

  function renderTreeRowLabel(
    li: HTMLLIElement,
    entry: DirEntry,
    expanded = false,
  ): void {
    li.textContent = "";
    if (entry.kind === "dir") {
      li.append(document.createTextNode((expanded ? "▾ " : "▸ ") + entry.name));
      return;
    }
    li.append(document.createTextNode(entry.name));

    const cached = indexStateCache.get(entry.rel_path);
    if (cached) {
      applyIndexMarker(li, cached);
      return;
    }
    // Always defer to the backend's `index_state_for` — it's the single
    // source of truth for "is this file indexable?" and returns
    // `unsupported` for anything outside `core::indexer::indexable_extensions`.
    const path = entry.rel_path;
    if (inflightStateFetches.has(path)) return;
    inflightStateFetches.add(path);
    void fetchIndexState(path)
      .then((state) => {
        document
          .querySelectorAll(`#tree li[data-path="${deps.cssEscape(path)}"]`)
          .forEach((el) => applyIndexMarker(el as HTMLElement, state));
        const buffer = deps.getBuffer();
        if (
          buffer &&
          !deps.isReadOnlyBuffer(buffer) &&
          buffer.path === path
        ) {
          deps.renderIndexStatus();
        }
      })
      .catch((err) => {
        Logger.error("ui::tree", "index_state_for failed", { path, err });
      })
      .finally(() => {
        inflightStateFetches.delete(path);
      });
  }

  function attachDnd(li: HTMLLIElement, entry: DirEntry): void {
    li.addEventListener("dragstart", (e) => {
      e.dataTransfer?.setData("text/plain", entry.rel_path);
      e.dataTransfer?.setData(
        "application/x-hiker-kind",
        entry.kind === "dir" ? "dir" : "file",
      );
      if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
      li.classList.add("dragging");
    });
    li.addEventListener("dragend", () => li.classList.remove("dragging"));

    li.addEventListener("dragover", (e) => {
      const src = e.dataTransfer?.types.includes("text/plain");
      if (!src) return;
      e.preventDefault();
      e.stopPropagation();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
      li.classList.add(Classes.DROP_TARGET);
    });
    li.addEventListener("dragleave", () => li.classList.remove(Classes.DROP_TARGET));

    li.addEventListener("drop", async (e) => {
      e.preventDefault();
      e.stopPropagation();
      li.classList.remove(Classes.DROP_TARGET);
      const from = e.dataTransfer?.getData("text/plain");
      if (!from) return;
      const fromKind =
        e.dataTransfer?.getData("application/x-hiker-kind") === "dir"
          ? "dir"
          : "file";
      const targetFolder =
        entry.kind === "dir" ? entry.rel_path : parentOf(entry.rel_path);
      await performDrop(from, fromKind, targetFolder);
    });
  }

  async function performDrop(
    from: string,
    fromKind: "dir" | "file",
    targetFolder: string,
  ): Promise<void> {
    if (from === targetFolder) return;
    const fromParent = parentOf(from);
    if (fromParent === targetFolder) return;
    const name = from.split("/").pop()!;
    if (targetFolder === from || targetFolder.startsWith(from + "/")) return;
    const to = targetFolder ? `${targetFolder}/${name}` : name;
    try {
      if (fromKind === "dir") {
        await Ipc.moveFolder({ from, to });
      } else {
        await Ipc.moveNote({ from, to });
      }
      const buffer = deps.getBuffer();
      if (buffer) {
        if (buffer.path === from) {
          deps.setBufferPath(to);
        } else if (fromKind === "dir" && buffer.path.startsWith(from + "/")) {
          deps.setBufferPath(to + buffer.path.slice(from.length));
        }
      }
      await refresh();
    } catch (err) {
      Logger.error("ui::tree", "move failed", { err });
      alert(`move failed: ${deps.formatErr(err)}`);
    }
  }

  // Pure builder for one tree row. Produces the `<li>` with `data-path`
  // and `data-kind` attributes the delegated click handler reads via
  // `closest("[data-path]")`. No click listener attached here — click
  // routes through container-level delegation on `treeEl` (see
  // `onTreeClick`). The inherently per-row dnd / context-menu / dblclick
  // listeners are still attached at row creation in `renderDir` because
  // they depend on per-row data the global delegation can't reconstruct
  // (drag state, inline-rename target, contextmenu items).
  function domForTreeRow(entry: DirEntry, expanded: boolean): HTMLLIElement {
    const li = document.createElement("li");
    li.dataset.path = entry.rel_path;
    li.dataset.kind = entry.kind;
    li.draggable = true;
    renderTreeRowLabel(li, entry, expanded);
    return li;
  }

  async function renderDir(rel: string, container: HTMLElement): Promise<void> {
    const entries = await Ipc.listDir({
      rel,
      sort: sortOrderToSettings(treeSortOrder),
    });
    const ul = document.createElement("ul");
    const pendingChildren: Promise<void>[] = [];
    for (const entry of entries) {
      const expanded = entry.kind === "dir" && expandedFolders.has(entry.rel_path);
      const li = domForTreeRow(entry, expanded);
      attachDnd(li, entry);
      attachContextMenu(li, entry);
      if (entry.kind === "dir" && expanded) {
        const path = entry.rel_path;
        pendingChildren.push(
          new Promise<void>((resolve) => {
            queueMicrotask(() => {
              const childContainer = document.createElement("div");
              li.after(childContainer);
              renderDir(path, childContainer).then(resolve, resolve);
            });
          }),
        );
      }
      li.addEventListener("dblclick", (e) => {
        e.preventDefault();
        e.stopPropagation();
        void beginInlineRename(li, entry.rel_path, entry.kind);
      });
      ul.appendChild(li);
    }
    container.appendChild(ul);
    await Promise.all(pendingChildren);
  }

  // Container-level click delegation. Resolves the clicked row via
  // `closest("[data-path]")`; folder rows toggle expansion through the
  // `expandedFolders` set + an on-demand child-container DOM mutation,
  // file rows route through `deps.openNote`. Double-click is suppressed
  // (the per-row `dblclick` handler owns inline rename).
  async function onTreeClick(e: MouseEvent): Promise<void> {
    if (e.detail >= 2) return;
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const li = target.closest<HTMLLIElement>("li[data-path]");
    if (!li || !deps.treeEl.contains(li)) return;
    e.stopPropagation();
    const rel = li.dataset.path ?? "";
    const kind = li.dataset.kind === "dir" ? "dir" : "file";
    if (kind === "dir") {
      selectedFolder = rel;
      // The child container, when expanded, lives as the next sibling
      // `<div>` after the `<li>` (see `renderDir`'s expansion path).
      const next = li.nextElementSibling;
      const childIsContainer =
        next instanceof HTMLDivElement && next.previousElementSibling === li;
      const isExpanded = expandedFolders.has(rel);
      // Reconstruct an ad-hoc DirEntry for the relabel — only `kind`,
      // `name`, `rel_path` are read by `renderTreeRowLabel`.
      const entry: DirEntry = {
        kind: "dir",
        name: rel.split("/").pop() ?? rel,
        rel_path: rel,
        mtime: 0,
      };
      if (isExpanded) {
        if (childIsContainer) next.remove();
        expandedFolders.delete(rel);
        renderTreeRowLabel(li, entry, false);
      } else {
        const childContainer = document.createElement("div");
        li.after(childContainer);
        await renderDir(rel, childContainer);
        expandedFolders.add(rel);
        renderTreeRowLabel(li, entry, true);
      }
    } else {
      selectedFolder = parentOf(rel);
      const sticky = e.metaKey || e.ctrlKey;
      void deps.openNote(rel, { preview: !sticky });
    }
  }
  deps.treeEl.addEventListener("click", (e) => {
    void onTreeClick(e);
  });

  async function refresh(): Promise<void> {
    deps.treeEl.innerHTML = "";
    await renderDir("", deps.treeEl);
    const buffer = deps.getBuffer();
    if (buffer) {
      document
        .querySelector(
          `#tree li[data-path="${deps.cssEscape(buffer.path)}"]`,
        )
        ?.classList.add("active");
    }
  }

  async function revealPath(rel: string): Promise<void> {
    let added = false;
    let cursor = parentOf(rel);
    while (cursor !== "") {
      if (!expandedFolders.has(cursor)) {
        expandedFolders.add(cursor);
        added = true;
      }
      cursor = parentOf(cursor);
    }
    if (added) {
      await refresh();
    }
    const row = document.querySelector(
      `#tree li[data-path="${deps.cssEscape(rel)}"]`,
    );
    row?.classList.add("active");
    row?.scrollIntoView({ block: "nearest" });
  }

  // status: tree-refresh-watcher — debounce a single tree rebuild across
  // bursts of watcher events. 200ms matches the watcher's own debounce.
  let treeRefreshDebounce: number | null = null;
  function notifyWatcher(): void {
    if (treeRefreshDebounce !== null) window.clearTimeout(treeRefreshDebounce);
    treeRefreshDebounce = window.setTimeout(() => {
      treeRefreshDebounce = null;
      void refresh();
    }, 200);
  }

  async function beginInlineRename(
    li: HTMLLIElement,
    currentPath: string,
    kind: "file" | "dir" = "file",
  ): Promise<void> {
    const name = currentPath.split("/").pop()!;
    const dotIdx = kind === "file" ? name.lastIndexOf(".") : -1;
    const stemEnd = dotIdx > 0 ? dotIdx : name.length;
    const input = document.createElement("input");
    input.type = "text";
    input.className = "tree-rename-input";
    input.value = name;
    li.textContent = "";
    li.appendChild(input);
    input.focus();
    input.setSelectionRange(0, stemEnd);

    await new Promise<void>((resolve) => {
      let done = false;
      const finish = async (commit: boolean) => {
        if (done) return;
        done = true;
        const newName = input.value.trim();
        if (commit && newName && newName !== name) {
          const parent = parentOf(currentPath);
          const to = parent ? `${parent}/${newName}` : newName;
          try {
            if (kind === "dir") {
              await Ipc.moveFolder({ from: currentPath, to });
            } else {
              await Ipc.moveNote({ from: currentPath, to });
            }
            if (kind === "dir") {
              const fromPrefix = currentPath + "/";
              const remapped = new Set<string>();
              for (const p of expandedFolders) {
                if (p === currentPath) {
                  remapped.add(to);
                } else if (p.startsWith(fromPrefix)) {
                  remapped.add(to + p.slice(currentPath.length));
                } else {
                  remapped.add(p);
                }
              }
              expandedFolders.clear();
              for (const p of remapped) expandedFolders.add(p);
            }
            const buffer = deps.getBuffer();
            if (buffer) {
              if (buffer.path === currentPath) {
                deps.setBufferPath(to);
              } else if (
                kind === "dir" &&
                buffer.path.startsWith(currentPath + "/")
              ) {
                deps.setBufferPath(to + buffer.path.slice(currentPath.length));
              }
            }
          } catch (err) {
            Logger.error("ui::tree", "rename failed", { err });
            alert(`rename failed: ${deps.formatErr(err)}`);
          }
        }
        await refresh();
        resolve();
      };
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          void finish(true);
        } else if (e.key === "Escape") {
          e.preventDefault();
          void finish(false);
        }
      });
      input.addEventListener("blur", () => void finish(true));
    });
  }

  function attachContextMenu(li: HTMLLIElement, entry: DirEntry): void {
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const items: CtxMenuItem[] = [];
      if (entry.kind === "file") {
        items.push({ label: "Open", run: () => deps.openNote(entry.rel_path) });
      }
      items.push({
        label: "Rename",
        run: () => beginInlineRename(li, entry.rel_path, entry.kind),
      });
      items.push({
        label: "Delete",
        danger: true,
        run: () => deleteFromTree(entry),
      });
      items.push({ label: "Properties", disabled: true });
      openContextMenu(e.clientX, e.clientY, items);
    });
  }

  async function countNotesIn(rel: string): Promise<number> {
    return Ipc.countNotesIn({ rel });
  }

  async function deleteFromTree(entry: DirEntry): Promise<void> {
    let memberCount = 0;
    if (entry.kind === "dir") {
      try {
        memberCount = await countNotesIn(entry.rel_path);
      } catch (err) {
        Logger.error("ui::tree", "countNotesIn failed", { err });
      }
    }
    const buffer = deps.getBuffer();
    const bufferUnderEntry =
      !!buffer &&
      (buffer.path === entry.rel_path ||
        buffer.path.startsWith(entry.rel_path + "/"));
    const dirtyTail = bufferUnderEntry && deps.isDirty()
      ? " Unsaved changes will be discarded."
      : "";
    const message =
      entry.kind === "dir"
        ? `Move ${entry.rel_path} and ${memberCount} note${memberCount === 1 ? "" : "s"} inside it to trash?${dirtyTail}`
        : `Move ${entry.rel_path} to trash?${dirtyTail}`;

    const ok = await confirmDanger(message, "Move to trash");
    if (!ok) return;

    try {
      const result = await Ipc.deleteNote({ rel: entry.rel_path });
      deps.clearOpenBufferIfWithin(entry.rel_path);
      await refresh();
      const toastMsg =
        result.kind === "folder"
          ? `Moved ${result.original_path} to trash (${result.members?.length ?? 0} notes)`
          : `Moved ${result.original_path} to trash`;
      showToast(toastMsg, {
        label: "Undo",
        run: async () => {
          try {
            const restored = await Ipc.restoreTrashEntry({ id: result.id });
            await refresh();
            showToast(`Restored ${restored.original_path}`);
          } catch (err) {
            Logger.error("ui::tree", "restore_trash_entry failed", { err });
            alert(`restore failed: ${deps.formatErr(err)}`);
          }
        },
      });
    } catch (err) {
      Logger.error("ui::tree", "delete_note failed", { err });
      alert(`delete failed: ${deps.formatErr(err)}`);
    }
  }

  function openSortByMenu(x: number, y: number): void {
    const orders: TreeSortOrder[] = [
      "name-asc",
      "name-desc",
      "mtime-newest",
      "mtime-oldest",
    ];
    openContextMenu(
      x,
      y,
      orders.map((o) => ({
        label: sortOrderLabel(o),
        checked: treeSortOrder === o,
        run: async () => {
          if (treeSortOrder === o) return;
          await setSortOrder(o, true);
        },
      })),
    );
  }

  async function setSortOrder(
    order: TreeSortOrder,
    persist: boolean,
  ): Promise<void> {
    treeSortOrder = order;
    await refresh();
    if (persist) {
      void deps.settings.setVaultSetting(
        "vault.tree.sort_by",
        sortOrderToSettings(order),
      );
    }
  }

  // --- Top-level wiring (root drop, empty-space context, toolbar buttons) ---

  // Tree-root drop zone: dropping on empty space below the tree → vault root.
  deps.treeEl.addEventListener("dragover", (e) => {
    if (!e.dataTransfer?.types.includes("text/plain")) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
  });
  deps.treeEl.addEventListener("drop", async (e) => {
    e.preventDefault();
    const from = e.dataTransfer?.getData("text/plain");
    if (!from) return;
    const fromKind =
      e.dataTransfer?.getData("application/x-hiker-kind") === "dir"
        ? "dir"
        : "file";
    await performDrop(from, fromKind, "");
  });

  // Empty-space right-click → "New note here".
  deps.treeEl.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openContextMenu(e.clientX, e.clientY, [
      {
        label: "New note here",
        run: async () => {
          selectedFolder = "";
          deps.newNoteBtn.click();
        },
      },
    ]);
  });

  // status: create-note-button — toolbar's "+ New note" button.
  deps.newNoteBtn.addEventListener("click", async () => {
    try {
      const created = await Ipc.createNote({ folder: selectedFolder });
      await refresh();
      await deps.openNote(created);
      const li = document.querySelector(
        `#tree li[data-path="${deps.cssEscape(created)}"]`,
      ) as HTMLLIElement | null;
      if (li) await beginInlineRename(li, created);
    } catch (err) {
      Logger.error("ui::tree", "create_note failed", { err });
      alert(`new note failed: ${deps.formatErr(err)}`);
    }
  });

  // status: tree-toolbar-actions-menu — `…` actions menu.
  deps.treeActionsBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    const rect = deps.treeActionsBtn.getBoundingClientRect();
    const buffer = deps.getBuffer();
    const activePath =
      buffer && !deps.isReadOnlyBuffer(buffer) ? buffer.path : null;
    openContextMenu(rect.right, rect.bottom, [
      {
        label: "Refresh tree",
        run: async () => {
          await refresh();
          await deps.refreshTrashBin();
        },
      },
      {
        label: "Reindex all",
        run: async () => {
          try {
            await Ipc.index({ scope: { kind: "all" } });
          } catch (err) {
            Logger.error("ui::tree", "reindex all failed", { err });
            alert(`reindex failed: ${deps.formatErr(err)}`);
          }
        },
      },
      {
        label: "Reindex this file",
        disabled: activePath === null,
        run: async () => {
          if (!activePath) return;
          try {
            await Ipc.index({
              scope: { kind: "path", rel: activePath },
            });
          } catch (err) {
            Logger.error("ui::tree", "reindex file failed", { err });
            alert(`reindex failed: ${deps.formatErr(err)}`);
          }
        },
      },
      {
        label: `Sort by  ▸  ${sortOrderLabel(treeSortOrder)}`,
        run: () => openSortByMenu(rect.right, rect.bottom),
      },
    ]);
  });

  const api: TreeApi = {
    refresh,
    revealPath,
    setSortOrder,
    getSortOrder: () => treeSortOrder,
    setSelectedFolder: (rel: string) => {
      selectedFolder = rel;
    },
    notifyWatcher,
    getIndexState: (rel: string) => indexStateCache.get(rel),
    setIndexState: (rel: string, state: IndexState) => {
      indexStateCache.set(rel, state);
    },
    deleteIndexState: (rel: string) => {
      indexStateCache.delete(rel);
    },
    clearCaches: () => {
      indexStateCache.clear();
      inflightStateFetches.clear();
    },
    fetchIndexState,
  };

  return createPanelController<TreeApi>(api, {
    initialVisible: true,
    applyOnMount: false,
    onSetVisible: () => {
      // Sidebar-managed; no panel-level visibility toggle.
    },
  });
}
