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

import { invoke } from "@tauri-apps/api/core";

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

interface TrashEntry {
  id: string;
  original_path: string;
  trashed_name: string;
  original_mtime: number;
  deleted_at: number;
  kind: "file" | "folder";
  members?: string[] | null;
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

export interface TreeDeps {
  treeEl: HTMLElement;
  newNoteBtn: HTMLButtonElement;
  treeActionsBtn: HTMLButtonElement;
  cssEscape: (s: string) => string;
  formatError: (err: unknown) => string;
  getBuffer: () => BufferLike | null;
  /// True when buffer is a read-only preview (snapshot/trash). Used by the
  /// tree-actions menu's "Reindex this file" gate.
  isReadOnlyBuffer: (b: BufferLike | null) => boolean;
  setBufferPath: (newPath: string) => void;
  /// Called when a folder rename's recursive path remap touches the open
  /// buffer's path; host updates `buffer.path` + status.
  isDirty: () => boolean;
  openFile: (rel: string, opts?: { preview?: boolean }) => Promise<void>;
  /// Called from `deleteFromTree` to clear the buffer when its path falls
  /// inside the deleted subtree.
  clearOpenBufferIfWithin: (deletedRel: string) => void;
  refreshTrashBin: () => Promise<void>;
  /// Re-render the status-bar index label when the active buffer's index
  /// state resolves lazily.
  renderIndexStatus: () => void;
  persistSetting: (
    scope: "user" | "vault",
    key: string,
    value: unknown,
  ) => Promise<void>;
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

export function mountTree(deps: TreeDeps): TreeApi {
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
    const state = await invoke<IndexState>("index_state_for", { rel });
    indexStateCache.set(rel, state);
    return state;
  }

  function applyIndexMarker(li: HTMLElement, state: IndexState | null): void {
    li.classList.remove("ix-unsupported", "ix-skipped", "ix-queued", "ix-indexed");
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
        li.classList.add("ix-unsupported");
        li.removeAttribute("title");
        break;
      case "skipped":
        li.classList.add("ix-skipped");
        li.dataset.ixReason = state.reason;
        li.title = `Skipped — ${state.reason}`;
        break;
      case "queued":
        li.classList.add("ix-queued");
        li.removeAttribute("title");
        break;
      case "indexed":
        li.classList.add("ix-indexed");
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
        console.error("index_state_for failed:", path, err);
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
      li.classList.add("drop-target");
    });
    li.addEventListener("dragleave", () => li.classList.remove("drop-target"));

    li.addEventListener("drop", async (e) => {
      e.preventDefault();
      e.stopPropagation();
      li.classList.remove("drop-target");
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
    const cmd = fromKind === "dir" ? "move_folder" : "move_note";
    try {
      await invoke(cmd, { from, to });
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
      console.error(`${cmd} failed:`, err);
      alert(`move failed: ${deps.formatError(err)}`);
    }
  }

  async function renderDir(rel: string, container: HTMLElement): Promise<void> {
    const entries = await invoke<DirEntry[]>("list_dir", {
      rel,
      sort: sortOrderToSettings(treeSortOrder),
    });
    const ul = document.createElement("ul");
    const pendingChildren: Promise<void>[] = [];
    for (const entry of entries) {
      const li = document.createElement("li");
      li.dataset.path = entry.rel_path;
      li.dataset.kind = entry.kind;
      li.draggable = true;
      renderTreeRowLabel(li, entry);
      attachDnd(li, entry);
      attachContextMenu(li, entry);
      if (entry.kind === "dir") {
        let expanded = expandedFolders.has(entry.rel_path);
        let childContainer: HTMLElement | null = null;
        if (expanded) {
          renderTreeRowLabel(li, entry, true);
          const path = entry.rel_path;
          pendingChildren.push(
            new Promise<void>((resolve) => {
              queueMicrotask(() => {
                if (!expanded) {
                  resolve();
                  return;
                }
                childContainer = document.createElement("div");
                li.after(childContainer);
                renderDir(path, childContainer).then(resolve, resolve);
              });
            }),
          );
        }
        li.addEventListener("click", async (e) => {
          e.stopPropagation();
          if ((e as MouseEvent).detail >= 2) return;
          selectedFolder = entry.rel_path;
          if (expanded) {
            childContainer?.remove();
            childContainer = null;
            expanded = false;
            expandedFolders.delete(entry.rel_path);
            renderTreeRowLabel(li, entry, false);
          } else {
            childContainer = document.createElement("div");
            li.after(childContainer);
            await renderDir(entry.rel_path, childContainer);
            expanded = true;
            expandedFolders.add(entry.rel_path);
            renderTreeRowLabel(li, entry, true);
          }
        });
      } else {
        li.addEventListener("click", (e) => {
          e.stopPropagation();
          if ((e as MouseEvent).detail >= 2) return;
          selectedFolder = parentOf(entry.rel_path);
          // status: editor-preview-tab-from-open-callsites
          // status: editor-preview-tab-mod-click-sticky
          const me = e as MouseEvent;
          const sticky = me.metaKey || me.ctrlKey;
          void deps.openFile(entry.rel_path, { preview: !sticky });
        });
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
          const cmd = kind === "dir" ? "move_folder" : "move_note";
          try {
            await invoke(cmd, { from: currentPath, to });
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
            console.error("rename failed:", err);
            alert(`rename failed: ${deps.formatError(err)}`);
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
        items.push({ label: "Open", run: () => deps.openFile(entry.rel_path) });
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
    return invoke<number>("count_notes_in", { rel });
  }

  async function deleteFromTree(entry: DirEntry): Promise<void> {
    let memberCount = 0;
    if (entry.kind === "dir") {
      try {
        memberCount = await countNotesIn(entry.rel_path);
      } catch (err) {
        console.error("countNotesIn failed:", err);
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
      const result = await invoke<TrashEntry>("delete_note", {
        rel: entry.rel_path,
      });
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
            const restored = await invoke<TrashEntry>("restore_trash_entry", {
              id: result.id,
            });
            await refresh();
            showToast(`Restored ${restored.original_path}`);
          } catch (err) {
            console.error("restore_trash_entry failed:", err);
            alert(`restore failed: ${deps.formatError(err)}`);
          }
        },
      });
    } catch (err) {
      console.error("delete_note failed:", err);
      alert(`delete failed: ${deps.formatError(err)}`);
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
      void deps.persistSetting(
        "vault",
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
      const created = await invoke<string>("create_note", {
        folder: selectedFolder,
      });
      await refresh();
      await deps.openFile(created);
      const li = document.querySelector(
        `#tree li[data-path="${deps.cssEscape(created)}"]`,
      ) as HTMLLIElement | null;
      if (li) await beginInlineRename(li, created);
    } catch (err) {
      console.error("create_note failed:", err);
      alert(`new note failed: ${deps.formatError(err)}`);
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
            await invoke("index", { scope: { kind: "all" } });
          } catch (err) {
            console.error("reindex all failed:", err);
            alert(`reindex failed: ${deps.formatError(err)}`);
          }
        },
      },
      {
        label: "Reindex this file",
        disabled: activePath === null,
        run: async () => {
          if (!activePath) return;
          try {
            await invoke("index", {
              scope: { kind: "path", rel: activePath },
            });
          } catch (err) {
            console.error("reindex file failed:", err);
            alert(`reindex failed: ${deps.formatError(err)}`);
          }
        },
      },
      {
        label: `Sort by  ▸  ${sortOrderLabel(treeSortOrder)}`,
        run: () => openSortByMenu(rect.right, rect.bottom),
      },
    ]);
  });

  return {
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
}
