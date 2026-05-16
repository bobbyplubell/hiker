// status: drag-and-drop-move
// status: tree-refresh-manual
// status: tree-refresh-watcher
// status: tree-double-click-rename
// status: tree-context-menu
// status: tree-context-delete
// status: sidebar-toolbar-actions-menu
// status: tree-sort-options
// status: tree-row-unsupported-marker
// status: tree-row-skipped-marker
// status: tree-row-queued-marker
// status: create-note-button
// status: trail-row-icon
// status: trail-row-dropdown-chevron
// status: trail-set-as-active-context-verb
// status: trail-add-to-active-from-tree-verb
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
import { Classes } from "../style/classes";

import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { confirmDanger } from "../widgets/confirm";
import { Icons } from "../icons";
import type { Proposal, ResolvedWaypoint } from "../ipc";
import { getActiveTrailRel } from "../app/state";
import { getActiveTrailWaypointPaths } from "../trails/membership";
import {
  applyIndexMarker,
  parentOf,
  sortOrderLabel,
  sortOrderToSettings,
  sortTreeEntries,
  type DirEntry,
  type IndexState,
  type TreeSortOrder,
} from "./helpers";

export {
  sortOrderFromSettings,
  sortOrderToSettings,
} from "./helpers";
export type {
  DirEntry,
  EntryKind,
  IndexState,
  TreeSortOrder,
} from "./helpers";

// Loose buffer shape — modules only read `path` and `mode.kind`; the host's
// richer Buffer satisfies this structurally without coupling.
type BufferLike = { path: string; mode: { kind: string } };

export interface TreeDeps extends PanelDeps {
  treeEl: HTMLElement;
  newNoteBtn: HTMLButtonElement;
  sidebarActionsBtn: HTMLButtonElement;
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
  /// status: trail-add-to-active-from-tree-verb — fired after a
  /// successful `trailAppendWaypoint` from the tree-row "Add to active
  /// trail" verb. Host uses this to explicitly refresh the trails
  /// panel + membership cache; needed because
  /// `core::trails::append_waypoint` suppresses the watcher for the
  /// trail-doc + waypoint-note paths (to avoid indexer feedback
  /// loops), so the `hiker:file-changed`-driven refresh path can't
  /// fire for these writes. See
  /// `bug-add-to-trail-verbs-dont-refresh-panel`.
  onWaypointAppended?: () => void;
  // status: tree-context-properties
  /// Open the note-properties inspector for a note. Wired by the host
  /// to `openPropertiesTab(rel)`; fires from the Properties context-menu
  /// entry on tree rows.
  onOpenProperties?: (rel: string) => void;
  // status: staging-accept-reject-from-tree
  /// Open a staging proposal as a read-only preview buffer.
  onOpenStagingProposal?: (proposal: Proposal) => void | Promise<void>;
  /// Accept a staging proposal.
  onAcceptStaging?: (proposal: Proposal) => Promise<void>;
  /// Reject a staging proposal.
  onRejectStaging?: (proposal: Proposal) => Promise<void>;
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
  /// Begin inline-rename on the row currently rendered for `rel`. Used
  /// by the sidebar `+`-button create flows (notes, trails) so the new
  /// item lands in rename mode for naming. Awaits the next tree
  /// refresh before resolving the row, so the caller can `await
  /// refresh()` first or rely on `revealPath`. Resolves silently when
  /// no row matches (e.g. the create call failed and the row never
  /// landed).
  beginInlineRenameByPath(rel: string): Promise<void>;
  /// status: trail-row-icon — refresh the cached set of trail-doc rel
  /// paths so the file tree can decorate trail-doc rows with the
  /// squiggly icon. Cheap; called from the host's watcher hook on any
  /// `.md` change outside `.hiker/`.
  refreshTrailDocSet(): Promise<void>;
  /// status: trail-add-to-active-from-editor-verb — synchronous "is
  /// this rel-path a trail-doc?" predicate over the tree's internal
  /// `trailDocPaths` cache. Used by the editor toolbar pill to hide
  /// itself when the open buffer is a trail-doc (a trail can't be a
  /// waypoint of itself).
  isTrailDoc(rel: string): boolean;
  /// status: staging-accept-reject-from-tree — fetch pending proposals
  /// and refresh the tree so synthetic rows appear.
  refreshStagingProposals(): Promise<void>;
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

  // status: trail-row-icon
  // Cached set of vault-relative trail-doc paths. Populated by
  // `Ipc.trailsList()` and refreshed lazily on any `.md` watcher event
  // outside `.hiker/` (a non-trail-doc could acquire `hiker.kind:
  // trail` frontmatter at any time, and a trail-doc could lose it).
  // Used by `renderTreeRowLabel` to prepend the squiggly-trail icon
  // and by `attachContextMenu` to show the "Set as active trail"
  // verb. Empty until first refresh resolves.
  const trailDocPaths = new Set<string>();

  // status: trail-row-dropdown-chevron
  // Per-trail expansion state in the file tree. NOT persisted —
  // resets on vault open per spec. Each entry is a trail-doc rel path
  // currently expanded inline; the children are waypoint rows
  // rendered after the trail-doc row in `renderDir`. Detail is
  // fetched lazily via `Ipc.trailGet` on first expand and cached in
  // `trailDetailCache`.
  const expandedTrails = new Set<string>();
  const trailDetailCache = new Map<string, ResolvedWaypoint[]>();

  // status: staging-accept-reject-from-tree
  // Cached pending staging proposals. Fetched on vault open and on
  // `hiker:staging-changed`. Used by `renderDir` to inject synthetic
  // rows for new files and dirty markers for existing files.
  let pendingProposals: Proposal[] = [];

  async function refreshStagingProposals(): Promise<void> {
    try {
      pendingProposals = await Ipc.stagingList();
    } catch (err) {
      Logger.error("ui::tree", "staging_list failed", { err });
      pendingProposals = [];
    }
  }

  /// Given a directory rel path, return a map of direct child names
  /// to the proposals that target that child (or deeper inside it).
  function childProposalsForDir(rel: string): Map<string, Proposal[]> {
    const map = new Map<string, Proposal[]>();
    for (const p of pendingProposals) {
      if (rel === "") {
        // Direct children have no slash (file at root) or the first
        // segment before any slash.
        const slashIdx = p.target_path.indexOf("/");
        const childName = slashIdx >= 0 ? p.target_path.slice(0, slashIdx) : p.target_path;
        const arr = map.get(childName) ?? [];
        arr.push(p);
        map.set(childName, arr);
      } else if (p.target_path === rel || p.target_path.startsWith(rel + "/")) {
        const rest = p.target_path === rel ? "" : p.target_path.slice(rel.length + 1);
        const slashIdx = rest.indexOf("/");
        const childName = slashIdx >= 0 ? rest.slice(0, slashIdx) : rest;
        if (!childName) continue;
        const arr = map.get(childName) ?? [];
        arr.push(p);
        map.set(childName, arr);
      }
    }
    return map;
  }

  // status: trails-mode-side-trail-render — render side-trail
  // children recursively under their parent. Each level is wrapped
  // in its own `<ul>`, so the global `#tree ul ul` rules + the
  // `.tree-trail-children` indent step compound naturally per
  // depth.
  function appendWaypointChildren(
    parentUl: HTMLElement,
    waypoints: ResolvedWaypoint[],
  ): void {
    for (const w of waypoints) {
      const childLi = document.createElement("li");
      const sourcePath = w.source_ref?.path ?? w.waypoint_rel;
      childLi.dataset.path = sourcePath;
      childLi.dataset.kind = "file";
      childLi.classList.add("tree-trail-waypoint");
      const name = sourcePath.split("/").pop() ?? sourcePath;
      childLi.append(document.createTextNode(name));
      parentUl.appendChild(childLi);
      const kids = w.children ?? [];
      if (kids.length > 0) {
        const nestedContainer = document.createElement("div");
        nestedContainer.className = "tree-trail-children";
        const nestedUl = document.createElement("ul");
        appendWaypointChildren(nestedUl, kids);
        nestedContainer.appendChild(nestedUl);
        childLi.after(nestedContainer);
      }
    }
  }

  let treeSortOrder: TreeSortOrder = "name-asc";

  // Per-path index-state cache so re-renders don't re-fetch on every paint.
  const indexStateCache = new Map<string, IndexState>();
  const inflightStateFetches = new Set<string>();

  async function fetchIndexState(rel: string): Promise<IndexState> {
    const state = await Ipc.indexStateFor({ rel });
    // Don't overwrite a fresher value that arrived via hiker:reindex-progress
    // while this IPC was in flight — events are the live source of truth.
    if (!indexStateCache.has(rel)) {
      indexStateCache.set(rel, state);
    }
    return indexStateCache.get(rel)!;
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
    // status: trail-row-icon — squiggly-trail glyph prefix on trail-doc
    // file rows. Driven by the cached `trailDocPaths` set; non-trail
    // file rows render unchanged.
    const isTrail = trailDocPaths.has(entry.rel_path);
    if (isTrail) {
      const icon = document.createElement("span");
      icon.className = "tree-trail-icon";
      icon.innerHTML = Icons.trail({ size: 11 });
      li.append(icon);
      // status: trail-row-dropdown-chevron — chevron next to a
      // trail-doc row when the trail has at least one waypoint.
      // Fetches detail lazily on first click; expansion state lives
      // in `expandedTrails` and resets on vault open.
      const cachedWps = trailDetailCache.get(entry.rel_path);
      const wpCount = cachedWps?.length ?? null;
      // Only render chevron when we already know the count is > 0
      // OR we haven't fetched yet (optimistic — toggle on first click
      // hides itself if the trail has zero waypoints). The watcher
      // refresh path repaints the tree on any `.hiker/trails/` event
      // so the chevron appears after the first waypoint capture.
      if (wpCount === null || wpCount > 0) {
        const chev = document.createElement("span");
        chev.className = "tree-trail-chevron";
        chev.dataset.action = "toggle-trail";
        chev.textContent = expandedTrails.has(entry.rel_path) ? "▾" : "▸";
        li.append(chev);
      }
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
    let entries: DirEntry[];
    try {
      entries = await Ipc.listDir({
        rel,
        sort: sortOrderToSettings(treeSortOrder),
      });
    } catch {
      // Directory doesn't exist on disk — typical for synthetic staging
      // directories that haven't been accepted yet.
      entries = [];
    }

    // status: staging-accept-reject-from-tree — merge pending proposals.
    const childMap = childProposalsForDir(rel);
    const realNames = new Set(entries.map((e) => e.name));
    for (const [childName, proposals] of childMap) {
      if (realNames.has(childName)) continue;
      const exactPath = rel ? `${rel}/${childName}` : childName;
      const hasExactFile = proposals.some((p) => p.target_path === exactPath);
      entries.push({
        kind: hasExactFile ? "file" : "dir",
        name: childName,
        rel_path: exactPath,
        mtime: 0,
      });
    }
    entries = sortTreeEntries(entries, treeSortOrder);

    const ul = document.createElement("ul");
    const pendingChildren: Promise<void>[] = [];
    for (const entry of entries) {
      const proposals = childMap.get(entry.name) ?? [];
      const hasStaging = proposals.length > 0;
      const isSynthetic = !realNames.has(entry.name);
      const expanded = entry.kind === "dir" && expandedFolders.has(entry.rel_path);
      const li = domForTreeRow(entry, expanded);
      if (isSynthetic) {
        li.draggable = false;
      }
      if (hasStaging) {
        li.classList.add(isSynthetic ? "staging-new" : "staging-dirty");
      }
      if (!isSynthetic) {
        attachDnd(li, entry);
      }
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
      if (!isSynthetic) {
        li.addEventListener("dblclick", (e) => {
          e.preventDefault();
          e.stopPropagation();
          void beginInlineRename(li, entry.rel_path, entry.kind);
        });
      }
      ul.appendChild(li);
      // status: trail-row-dropdown-chevron — render waypoint children
      // for expanded trail-doc rows, mirroring the folder-expansion
      // shape. Detail comes from `trailDetailCache`; first-paint will
      // be empty if the click handler hasn't fetched yet. Must run
      // AFTER `ul.appendChild(li)` — otherwise `li.after(...)` is a
      // no-op (Element.after needs a parent) and the waypoints never
      // appear in the DOM (fix for bug-filetree-trail-expand-shows-no-waypoints).
      if (
        entry.kind === "file"
        && !isSynthetic
        && trailDocPaths.has(entry.rel_path)
        && expandedTrails.has(entry.rel_path)
      ) {
        const wps = trailDetailCache.get(entry.rel_path) ?? [];
        if (wps.length > 0) {
          const childContainer = document.createElement("div");
          childContainer.className = "tree-trail-children";
          const childUl = document.createElement("ul");
          appendWaypointChildren(childUl, wps);
          childContainer.appendChild(childUl);
          li.after(childContainer);
        }
      }
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
    // status: trail-row-dropdown-chevron — chevron click toggles
    // inline expansion of the trail's waypoints. Resolved before the
    // generic row click so opening a trail-doc still works (clicking
    // the basename, not the chevron).
    const chevTarget = target.closest<HTMLElement>(
      "[data-action='toggle-trail']",
    );
    if (chevTarget && deps.treeEl.contains(chevTarget)) {
      const li = chevTarget.closest<HTMLLIElement>("li[data-path]");
      if (li) {
        e.stopPropagation();
        const trailRel = li.dataset.path ?? "";
        await toggleTrailExpansion(trailRel);
      }
      return;
    }
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
      // status: staging-accept-reject-from-tree — synthetic staging rows
      // open the staging preview instead of the on-disk file.
      if (li.classList.contains("staging-new")) {
        const proposal = pendingProposals.find((p) => p.target_path === rel);
        if (proposal && deps.onOpenStagingProposal) {
          void deps.onOpenStagingProposal(proposal);
        }
      } else {
        const sticky = e.metaKey || e.ctrlKey;
        void deps.openNote(rel, { preview: !sticky });
      }
    }
  }
  deps.treeEl.addEventListener("click", (e) => {
    void onTreeClick(e);
  });

  async function refreshTrailDocSet(): Promise<void> {
    // status: trail-row-icon — reload trail-doc paths so the tree
    // can decorate rows. Failures are logged and leave the set
    // unchanged; the worst-case is a stale icon until the next
    // successful refresh.
    try {
      const list = await Ipc.trailsList();
      const fresh = new Set<string>();
      for (const t of list) fresh.add(t.rel_path);
      // Only mutate if anything changed — avoids redundant refreshes
      // when the watcher fires for non-trail md edits.
      let changed = fresh.size !== trailDocPaths.size;
      if (!changed) {
        for (const p of fresh) {
          if (!trailDocPaths.has(p)) {
            changed = true;
            break;
          }
        }
      }
      if (changed) {
        trailDocPaths.clear();
        for (const p of fresh) trailDocPaths.add(p);
        // Drop expanded state + cached detail for trails that no
        // longer exist.
        for (const k of [...expandedTrails]) {
          if (!trailDocPaths.has(k)) expandedTrails.delete(k);
        }
        for (const k of [...trailDetailCache.keys()]) {
          if (!trailDocPaths.has(k)) trailDetailCache.delete(k);
        }
      }
    } catch (err) {
      Logger.error("ui::tree", "trails_list (decoration) failed", { err });
    }
  }

  async function toggleTrailExpansion(trailRel: string): Promise<void> {
    if (expandedTrails.has(trailRel)) {
      expandedTrails.delete(trailRel);
      await refresh();
      return;
    }
    // First-time expand — fetch detail lazily. Cached for subsequent
    // toggles; the watcher hook invalidates the cache on `.hiker/
    // trails/` events so reopening picks up new waypoints.
    try {
      const detail = await Ipc.trailGet({ trailDocRel: trailRel });
      trailDetailCache.set(trailRel, detail.waypoints);
      // Hide the chevron post-fetch when the trail is empty (the
      // optimistic render above shows it before we know the count).
      if (detail.waypoints.length === 0) {
        await refresh();
        return;
      }
      expandedTrails.add(trailRel);
      await refresh();
    } catch (err) {
      Logger.error("ui::tree", "trail_get (tree expand) failed", {
        err,
        trailRel,
      });
    }
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
        // status: trail-set-as-active-context-verb — only on trail-doc rows.
        if (trailDocPaths.has(entry.rel_path)) {
          items.push({
            label: "Set as active trail",
            run: async () => {
              try {
                await Ipc.trailSetActive({ trailDocRel: entry.rel_path });
                const name = entry.rel_path.split("/").pop() ?? entry.rel_path;
                showToast(`Activated ${name.replace(/\.md$/i, "")}`);
              } catch (err) {
                Logger.error("ui::tree", "trail_set_active failed", {
                  err,
                  rel: entry.rel_path,
                });
                alert(`activate failed: ${deps.formatErr(err)}`);
              }
            },
          });
        }
        items.push({ label: "Open", run: () => deps.openNote(entry.rel_path) });
        // status: trail-add-to-active-from-tree-verb — append the row's
        // note as a waypoint of the currently-active trail. Calls
        // `Ipc.trailAppendWaypoint` directly rather than going through
        // the shared `captureToActiveTrail` helper: the helper swallows
        // errors by contract (capture entry points must not hard-fail
        // when trail routing fails), but this tree verb is a direct
        // user action and the user needs to see when the append fails.
        // Hidden on trail-docs (already a trail), waypoint-notes under
        // `.hiker/trails/`, and unsupported file types (per the cached
        // `IndexState`). Disabled with a tooltip when no active trail
        // is set so the affordance teaches itself.
        const isTrailDoc = trailDocPaths.has(entry.rel_path);
        const isWaypointNote = entry.rel_path.startsWith(".hiker/trails/");
        const ixState = indexStateCache.get(entry.rel_path);
        const isUnsupported = ixState?.kind === "unsupported";
        if (!isTrailDoc && !isWaypointNote && !isUnsupported) {
          const active = getActiveTrailRel();
          const activeBasename = active
            ? (active.split("/").pop() ?? active).replace(/\.md$/i, "")
            : null;
          const label = activeBasename
            ? `Add to active trail (${activeBasename})`
            : "Add to active trail";
          // Per `trails.md` "Building a trail while reading":
          // idempotency check is per-trail, not per-vault. The
          // synchronous membership cache is populated by
          // `./trails/membership` and refreshed on active-trail and
          // waypoint-set changes; reading from it inside the
          // contextmenu builder keeps the menu sync-safe (no async
          // before show).
          const alreadyMember =
            active !== null
            && getActiveTrailWaypointPaths().has(entry.rel_path);
          const disabled = active === null || alreadyMember;
          let tooltip: string | undefined;
          if (active === null) {
            tooltip = "No active trail — pick one in Trails mode";
          } else if (alreadyMember) {
            tooltip = "Already in this trail";
          }
          items.push({
            label,
            disabled,
            tooltip,
            run: async () => {
              const target = getActiveTrailRel();
              if (!target) return;
              const targetBasename =
                (target.split("/").pop() ?? target).replace(/\.md$/i, "");
              try {
                await Ipc.trailAppendWaypoint({
                  trailDocRel: target,
                  sourceRel: entry.rel_path,
                  annotation: null,
                });
                showToast(`Added to ${targetBasename}`);
                // Watcher is suppressed for the trail-doc +
                // waypoint-note paths during the append (indexer
                // feedback-loop prevention), so the `hiker:file-changed`
                // refresh path can't fire here. Explicitly notify the
                // host to refresh the trails panel + membership cache.
                // See `bug-add-to-trail-verbs-dont-refresh-panel`.
                deps.onWaypointAppended?.();
              } catch (err) {
                Logger.error("ui::tree", "trail append from tree verb failed", {
                  error: String(err),
                  rel: entry.rel_path,
                  trail: target,
                });
                showToast("Failed to add waypoint");
              }
            },
          });
        }
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
      // status: tree-context-properties
      items.push({
        label: "Properties",
        run: () => deps.onOpenProperties?.(entry.rel_path),
      });
      // status: staging-accept-reject-from-tree
      const stagingProposals = pendingProposals.filter(
        (p) =>
          p.target_path === entry.rel_path ||
          p.target_path.startsWith(entry.rel_path + "/"),
      );
      if (stagingProposals.length > 0) {
        const first = stagingProposals[0];
        items.push({
          label: "Review pending change",
          run: () => deps.onOpenStagingProposal?.(first),
        });
        items.push({
          label: "Accept change",
          run: async () => {
            try {
              await deps.onAcceptStaging?.(first);
              await refreshStagingProposals();
              await refresh();
            } catch (err) {
              Logger.error("ui::tree", "staging accept failed", { err });
            }
          },
        });
        items.push({
          label: "Reject change",
          danger: true,
          run: async () => {
            try {
              await deps.onRejectStaging?.(first);
              await refreshStagingProposals();
              await refresh();
            } catch (err) {
              Logger.error("ui::tree", "staging reject failed", { err });
            }
          },
        });
      }
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

  // status: sidebar-toolbar-actions-menu — `…` actions menu.
  deps.sidebarActionsBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    const rect = deps.sidebarActionsBtn.getBoundingClientRect();
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
    ], deps.sidebarActionsBtn);
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
      // status: trail-row-dropdown-chevron — expansion resets on vault
      // open per spec. status: trail-row-icon — drop the cached
      // trail-doc set so the new vault re-fetches.
      expandedTrails.clear();
      trailDetailCache.clear();
      trailDocPaths.clear();
      // status: staging-accept-reject-from-tree
      pendingProposals = [];
    },
    fetchIndexState,
    beginInlineRenameByPath: async (rel: string) => {
      const li = document.querySelector(
        `#tree li[data-path="${deps.cssEscape(rel)}"]`,
      ) as HTMLLIElement | null;
      if (!li) return;
      await beginInlineRename(li, rel, "file");
    },
  refreshTrailDocSet,
  isTrailDoc: (rel: string) => trailDocPaths.has(rel),
  refreshStagingProposals,
};

  return createPanelController<TreeApi>(api, {
    initialVisible: true,
    applyOnMount: false,
    onSetVisible: () => {
      // Sidebar-managed; no panel-level visibility toggle.
    },
  });
}
