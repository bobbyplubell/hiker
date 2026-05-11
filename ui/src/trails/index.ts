// status: trails-mode-body
// status: trails-mode-active-trail-dropdown
// status: trails-dropdown-ordering
// status: trails-mode-trail-head-icon
// status: trails-mode-waypoint-card
// status: trails-mode-empty-states
// status: trails-mode-orphan-card
// status: trails-mode-sidebar-read-only
// status: trails-mode-remove-waypoint-verb
// status: trails-mode-side-trail-render
// status: trail-append-cursor-indicator
// status: trail-append-from-here-verb
// status: trail-reset-cursor-verb
//
// Trails sidebar mode body. Mirrors the panel-controller pattern from
// `tree/index.ts` and `discovery/index.ts`. The module is read-only for
// editing operations per `trails-mode-sidebar-read-only`: no
// drag-to-reorder, no inline rename, no in-place annotation editing.
// The single editing verb is the row right-click "Remove waypoint"
// context-menu entry. Any deeper edits route through opening the
// trail-doc or the waypoint-note in the editor pane.
//
// State this module owns:
//   - trails list cache (top-level dropdown contents)
//   - active-trail detail cache (currently-rendered waypoints)
//   - per-card expanded state (one card at a time, plus an "Expand all"
//     header toggle)
//   - in-flight epoch counter so stale fetches drop after a refresh
//
// The host wires `openNote` (mirrors the existing tree/discovery shape)
// and calls `onActiveTrailMaybeChanged()` whenever the merged settings
// snapshot's `vault.active_trail` field changes (vault open, settings
// pane, post-`set_setting`).

import { Ipc } from "../ipc";
import {
  type TrailListItem,
  type TrailDetail,
  type ResolvedWaypoint,
  type ResolutionOutcome,
} from "../ipc";
import { Logger } from "../logger";
import {
  createPanelController,
  type PanelController,
  type PanelDeps,
} from "../panels/controller";
import { Icons } from "../icons";
import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";
import { confirmDanger } from "../widgets/confirm";
import { activeTrailStore } from "../app/state";

export interface TrailsDeps extends PanelDeps {
  rootEl: HTMLElement;
  /// Fired after a successful `removeWaypoint` → `refresh()`. The host
  /// uses this to refresh the trash panel + vault-home activity surface,
  /// because `core::trails::remove_waypoint` cascades through
  /// `core::ops::delete` which suppresses the watcher for deleted paths,
  /// so the `hiker:file-changed`-driven refresh path can't fire for
  /// these writes. Mirrors `onWaypointAppended` on `TreeDeps`.
  onWaypointRemoved?: () => void;
}

export interface TrailsApi {
  /// Re-fetch the trails-list and (if there's an active trail) its
  /// detail, then re-render. Safe to call from any host hook.
  refresh(): Promise<void>;
  /// Host hook — called whenever the merged settings snapshot's
  /// `vault.active_trail` field changes. Triggers a refresh.
  onActiveTrailMaybeChanged(): void;
}

export type TrailsController = PanelController<TrailsApi>;

function basenameOf(rel: string): string {
  const last = rel.split("/").pop() ?? rel;
  return last.replace(/\.md$/i, "");
}

function firstNonEmptyLine(s: string): string {
  for (const raw of s.split("\n")) {
    const line = raw.trim();
    if (line.length > 0) return line;
  }
  return "";
}

function isOrphanLike(r: ResolutionOutcome): boolean {
  // v1 simplification: surface `path_conflict` as orphan-styled with a
  // "broken reference" pill until the path-conflict modal lands. Spec
  // calls this out explicitly.
  return r.kind === "orphan" || r.kind === "path_conflict";
}

function sourcePathFor(w: ResolvedWaypoint): string | null {
  // Prefer the self-heal canonical path when applicable; otherwise the
  // recorded `source_ref.path`. Returns `null` for orphan-like
  // resolutions (v1: orphan + path_conflict).
  if (isOrphanLike(w.resolution)) return null;
  if (w.resolution.kind === "self_heal") {
    return w.resolution.canonical_path;
  }
  return w.source_ref.path;
}

export function mountTrailsPanel(deps: TrailsDeps): TrailsController {
  let listCache: TrailListItem[] = [];
  let detailCache: TrailDetail | null = null;
  // One-card-at-a-time expansion (per spec); when the "Expand all"
  // toggle is on, every card renders expanded regardless of which
  // single card was last clicked.
  let expandedWaypointId: string | null = null;
  let expandAll = false;
  // status: trails-mode-side-trail-render — per-waypoint side-trail
  // collapse state. Default: every side trail expanded on first
  // render (per spec). Entries here are waypoint ids whose children
  // block is currently hidden.
  const sideTrailCollapsed = new Set<string>();
  // Epoch counter — every async fetch tags itself with the current
  // value; when the response lands, it's dropped if a newer refresh
  // has bumped the epoch in the meantime.
  let fetchEpoch = 0;

  function activeTrailRel(): string | null {
    return activeTrailStore.get().rel;
  }

  function sortedTrails(): TrailListItem[] {
    // status: trails-dropdown-ordering — most-recent activation first;
    // nulls last (alphabetical fallback inside the null group keeps the
    // list deterministic).
    const copy = [...listCache];
    copy.sort((a, b) => {
      const ta = a.last_activated_at;
      const tb = b.last_activated_at;
      if (ta === null && tb === null) return a.title.localeCompare(b.title);
      if (ta === null) return 1;
      if (tb === null) return -1;
      // ISO-8601 strings compare correctly lexicographically.
      if (ta < tb) return 1;
      if (ta > tb) return -1;
      return a.title.localeCompare(b.title);
    });
    return copy;
  }

  async function setActiveTrail(rel: string | null): Promise<void> {
    try {
      await Ipc.trailSetActive({ trailDocRel: rel });
      // The Tauri command stamps last_activated_at + writes the
      // setting; the merged-settings refresh in `applySettingsToUi`
      // will re-seed `activeTrailStore`. Refresh now so the UI reacts
      // immediately even before the host's settings round-trip.
      activeTrailStore.set({ rel });
      await refresh();
    } catch (err) {
      Logger.error("ui::trails", "trail_set_active failed", { err });
      showToast(`Couldn't activate trail: ${deps.formatErr(err)}`);
    }
  }

  async function createNewTrailAndActivate(): Promise<void> {
    try {
      const created = await Ipc.trailCreate({ name: "new-trail" });
      await setActiveTrail(created.trail_doc_rel);
    } catch (err) {
      Logger.error("ui::trails", "trail_create failed", { err });
      showToast(`Couldn't create trail: ${deps.formatErr(err)}`);
    }
  }

  function openTrailDropdown(anchor: HTMLElement): void {
    // status: trails-mode-active-trail-dropdown
    // Implemented as a popover (`openContextMenu`) rather than a real
    // `<select>` to match the existing dropdown idiom (tree-actions,
    // sort-by, View menu) and so we can render disabled / "All trails…"
    // sub-picker entries the same way.
    const rect = anchor.getBoundingClientRect();
    const trails = sortedTrails();
    const items: CtxMenuItem[] = [];
    items.push({
      label: "None",
      checked: activeTrailRel() === null,
      run: () => void setActiveTrail(null),
    });
    for (const t of trails) {
      items.push({
        label: t.title,
        checked: activeTrailRel() === t.rel_path,
        run: () => void setActiveTrail(t.rel_path),
      });
    }
    items.push({
      label: "All trails…",
      disabled: trails.length === 0,
      run: () => openAllTrailsPicker(rect.left, rect.bottom),
    });
    openContextMenu(rect.left, rect.bottom, items, anchor);
  }

  function openAllTrailsPicker(x: number, y: number): void {
    // Flat picker over the full vault list. Re-fetches so the picker is
    // up-to-date even if the cached list has drifted.
    void (async () => {
      let fresh: TrailListItem[] = [];
      try {
        fresh = await Ipc.trailsList();
      } catch (err) {
        Logger.error("ui::trails", "trails_list (picker) failed", { err });
        showToast(`Couldn't list trails: ${deps.formatErr(err)}`);
        return;
      }
      const items: CtxMenuItem[] = fresh.map((t) => ({
        label: t.title,
        checked: activeTrailRel() === t.rel_path,
        run: () => void setActiveTrail(t.rel_path),
      }));
      if (items.length === 0) {
        items.push({
          label: "(no trails)",
          disabled: true,
        });
      }
      openContextMenu(x, y, items);
    })();
  }

  function dropdownLabel(): string {
    const rel = activeTrailRel();
    if (rel === null) return "None";
    const found = listCache.find((t) => t.rel_path === rel);
    if (found) return found.title;
    return basenameOf(rel);
  }

  // status: trail-append-cursor-indicator — find a waypoint anywhere in
  // the resolved tree by id. Used to label the header hint and to
  // disable "Append from here" when the right-clicked card is already
  // the cursor.
  function findWaypointById(
    id: string,
    roots: ResolvedWaypoint[] = detailCache?.waypoints ?? [],
  ): ResolvedWaypoint | null {
    for (const w of roots) {
      if (w.waypoint_id === id) return w;
      const inChildren = findWaypointById(id, w.children ?? []);
      if (inChildren !== null) return inChildren;
    }
    return null;
  }

  function cursorBasename(): string | null {
    const id = detailCache?.append_under ?? null;
    if (id === null) return null;
    const w = findWaypointById(id);
    if (w === null) return null;
    // Source-note basename, falling back to the waypoint-note basename
    // for orphan-like resolutions (orphan-safe).
    const src = sourcePathFor(w);
    if (src !== null) return basenameOf(src);
    return basenameOf(w.waypoint_rel);
  }

  async function resetCursor(): Promise<void> {
    const trailRel = activeTrailRel();
    if (trailRel === null) return;
    // status: trail-reset-cursor-verb
    try {
      await Ipc.trailSetAppendCursor({
        trailDocRel: trailRel,
        waypointId: null,
      });
      await refresh();
      showToast("Appending to main line");
    } catch (err) {
      Logger.error("ui::trails", "reset append cursor failed", {
        error: String(err),
      });
      showToast("Failed to reset cursor");
    }
  }

  async function setAppendCursorTo(w: ResolvedWaypoint): Promise<void> {
    const trailRel = activeTrailRel();
    if (trailRel === null) return;
    // status: trail-append-from-here-verb
    try {
      await Ipc.trailSetAppendCursor({
        trailDocRel: trailRel,
        waypointId: w.waypoint_id,
      });
      await refresh();
      const src = sourcePathFor(w);
      const label = src !== null ? basenameOf(src) : basenameOf(w.waypoint_rel);
      showToast(`Appending under ${label}`);
    } catch (err) {
      Logger.error("ui::trails", "set append cursor failed", {
        error: String(err),
      });
      showToast("Failed to set append cursor");
    }
  }

  function renderHeader(): HTMLElement {
    const header = document.createElement("div");
    header.className = "trails-header";

    const dropdownBtn = document.createElement("button");
    dropdownBtn.type = "button";
    dropdownBtn.className = "trails-dropdown-btn";
    dropdownBtn.textContent = `${dropdownLabel()} ▾`;
    dropdownBtn.title = "Active trail";
    dropdownBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      openTrailDropdown(dropdownBtn);
    });
    header.appendChild(dropdownBtn);

    // status: trails-mode-trail-head-icon — squiggly-trail icon button.
    const headBtn = document.createElement("button");
    headBtn.type = "button";
    headBtn.className = "trails-head-btn";
    headBtn.title = "Open trail-doc";
    headBtn.setAttribute("aria-label", "Open trail-doc");
    headBtn.innerHTML = Icons.trail({ size: 14 });
    const activeRel = activeTrailRel();
    headBtn.disabled = activeRel === null;
    headBtn.addEventListener("click", () => {
      const rel = activeTrailRel();
      if (rel === null) return;
      void deps.openNote(rel, { preview: true });
    });
    header.appendChild(headBtn);

    // "Expand all" toggle.
    const expandBtn = document.createElement("button");
    expandBtn.type = "button";
    expandBtn.className = "trails-expand-all-btn";
    expandBtn.textContent = expandAll ? "▾" : "▸";
    expandBtn.title = expandAll ? "Collapse all" : "Expand all";
    expandBtn.setAttribute(
      "aria-label",
      expandAll ? "Collapse all" : "Expand all",
    );
    const hasWaypoints =
      detailCache !== null && detailCache.waypoints.length > 0;
    expandBtn.disabled = !hasWaypoints;
    expandBtn.addEventListener("click", () => {
      expandAll = !expandAll;
      if (!expandAll) expandedWaypointId = null;
      paint();
    });
    header.appendChild(expandBtn);

    return header;
  }

  // status: trail-append-cursor-indicator — header hint row beneath
  // the dropdown / trail-head / expand-all row. Returns null when no
  // active trail is set (empty-state branch already covers that).
  function renderCursorHint(): HTMLElement | null {
    const activeRel = activeTrailRel();
    if (activeRel === null) return null;
    if (detailCache === null) return null;
    const row = document.createElement("div");
    row.className = "trails-cursor-hint";
    const cursorId = detailCache.append_under;
    const basename = cursorId !== null ? cursorBasename() : null;
    if (cursorId === null || basename === null) {
      // Cursor null OR stale (id doesn't resolve in current tree); the
      // next append self-heals so treat as main-line visually.
      row.textContent = "Appending to main line";
      return row;
    }
    const label = document.createElement("span");
    label.textContent = `Appending under ${basename}`;
    row.appendChild(label);
    const resetBtn = document.createElement("button");
    resetBtn.type = "button";
    resetBtn.className = "trails-cursor-reset-btn";
    resetBtn.textContent = "Reset to main line";
    resetBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void resetCursor();
    });
    row.appendChild(resetBtn);
    return row;
  }

  function renderEmptyState(): HTMLElement {
    // status: trails-mode-empty-states — three branches: no trails in
    // the vault, no active trail (but trails exist), active trail with
    // zero waypoints. Orphan-only / partial-orphan trails just render
    // their cards (orphan ones styled per `trails-mode-orphan-card`).
    const wrap = document.createElement("div");
    wrap.className = "trails-empty";
    const activeRel = activeTrailRel();
    if (listCache.length === 0) {
      const p = document.createElement("p");
      p.textContent = "No trails in this vault yet.";
      wrap.appendChild(p);
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = "Create a trail";
      btn.addEventListener("click", () => void createNewTrailAndActivate());
      wrap.appendChild(btn);
      return wrap;
    }
    if (activeRel === null) {
      const p = document.createElement("p");
      p.textContent = "Pick a trail to walk from the dropdown above.";
      wrap.appendChild(p);
      return wrap;
    }
    if (detailCache !== null && detailCache.waypoints.length === 0) {
      const p = document.createElement("p");
      p.textContent =
        "Empty trail — capture into it or use + to add the first waypoint.";
      wrap.appendChild(p);
      return wrap;
    }
    return wrap;
  }

  function renderWaypointCard(w: ResolvedWaypoint): HTMLElement {
    const card = document.createElement("div");
    card.className = "waypoint-card";
    card.dataset.waypointId = w.waypoint_id;
    const orphan = isOrphanLike(w.resolution);
    if (orphan) card.classList.add("orphan");
    const expanded = expandAll || expandedWaypointId === w.waypoint_id;
    if (expanded) card.classList.add("expanded");

    const head = document.createElement("div");
    head.className = "waypoint-card-head";

    const hasChildren = (w.children?.length ?? 0) > 0;
    const sideCollapsed = sideTrailCollapsed.has(w.waypoint_id);

    // status: trails-mode-side-trail-render — side-trail collapse
    // chevron, only shown when this waypoint has child waypoints.
    // Placed at the left edge of the card head (before the ordinal)
    // so it's visually distinct from the body-expand chevron on the
    // right edge of the card.
    if (hasChildren) {
      const sideChev = document.createElement("button");
      sideChev.type = "button";
      sideChev.className = "waypoint-card-side-chevron";
      sideChev.textContent = sideCollapsed ? "▸" : "▾";
      sideChev.title = sideCollapsed ? "Expand side trail" : "Collapse side trail";
      sideChev.setAttribute(
        "aria-label",
        sideCollapsed ? "Expand side trail" : "Collapse side trail",
      );
      sideChev.addEventListener("click", (e) => {
        e.stopPropagation();
        if (sideCollapsed) {
          sideTrailCollapsed.delete(w.waypoint_id);
        } else {
          sideTrailCollapsed.add(w.waypoint_id);
        }
        paint();
      });
      head.appendChild(sideChev);
    }

    const seq = document.createElement("span");
    seq.className = "waypoint-card-seq";
    // tree_path is the dotted 1-based ordinal ("1", "1.2", "1.2.1").
    seq.textContent = w.tree_path;
    head.appendChild(seq);

    // status: trail-append-cursor-indicator — little-person glyph in
    // the card head when this waypoint is the append cursor. Same
    // `Icons.user()` SVG as the recent-activity author pill; accent
    // color via `.waypoint-card-cursor-indicator`.
    const isCursor =
      detailCache !== null && detailCache.append_under === w.waypoint_id;
    if (isCursor) {
      const cursorGlyph = document.createElement("span");
      cursorGlyph.className = "waypoint-card-cursor-indicator";
      cursorGlyph.setAttribute("aria-label", "append cursor");
      cursorGlyph.title = "Next append lands here";
      cursorGlyph.innerHTML = Icons.user({ size: 12 });
      head.appendChild(cursorGlyph);
    }

    const basenameBtn = document.createElement("button");
    basenameBtn.type = "button";
    basenameBtn.className = "waypoint-card-basename";
    basenameBtn.dataset.action = "open-source";
    const sourceRel = sourcePathFor(w);
    basenameBtn.textContent = basenameOf(w.source_ref.path);
    if (orphan) {
      const pill = document.createElement("span");
      pill.className = "waypoint-card-pill";
      pill.textContent = "broken reference";
      basenameBtn.appendChild(document.createTextNode(" "));
      basenameBtn.appendChild(pill);
    }
    basenameBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (orphan) {
        showToast(
          "Reference broken — edit the waypoint-note to fix the link.",
        );
        return;
      }
      if (sourceRel === null) return;
      void deps.openNote(sourceRel, { preview: true });
    });
    head.appendChild(basenameBtn);

    const chevron = document.createElement("span");
    chevron.className = "waypoint-card-chevron";
    chevron.textContent = expanded ? "▾" : "▸";
    head.appendChild(chevron);

    card.appendChild(head);

    if (!expanded) {
      const snippet = firstNonEmptyLine(w.annotation_body);
      if (snippet.length > 0) {
        const snip = document.createElement("div");
        snip.className = "waypoint-card-snippet";
        snip.textContent = snippet;
        card.appendChild(snip);
      }
    } else {
      const body = document.createElement("div");
      body.className = "waypoint-card-body";
      // Plain-text rendering — markdown live-preview is out of scope
      // for this slice. Newlines preserved via `white-space: pre-wrap`
      // in CSS so users see the shape of their annotation.
      body.textContent = w.annotation_body;
      card.appendChild(body);

      const editBtn = document.createElement("button");
      editBtn.type = "button";
      editBtn.className = "waypoint-card-edit";
      editBtn.dataset.action = "open-waypoint";
      editBtn.textContent = "edit annotation";
      editBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        void deps.openNote(w.waypoint_rel, { preview: true });
      });
      card.appendChild(editBtn);
    }

    // Card-body click toggles expansion (excluding clicks on inner
    // buttons which `stopPropagation()` above). Orphan cards are
    // expandable too — the user may still want to read whatever
    // annotation they wrote.
    card.addEventListener("click", () => {
      if (expandAll) {
        // With "Expand all" on, individual clicks have no effect; the
        // header toggle owns the global expansion state.
        return;
      }
      if (expandedWaypointId === w.waypoint_id) {
        expandedWaypointId = null;
      } else {
        expandedWaypointId = w.waypoint_id;
      }
      paint();
    });

    // status: trails-mode-remove-waypoint-verb
    // status: trail-append-from-here-verb
    card.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const alreadyCursor =
        detailCache !== null && detailCache.append_under === w.waypoint_id;
      openContextMenu(e.clientX, e.clientY, [
        {
          label: "Append from here",
          disabled: alreadyCursor,
          tooltip: alreadyCursor ? "Already the cursor" : undefined,
          run: () => void setAppendCursorTo(w),
        },
        {
          label: "Remove waypoint",
          danger: true,
          run: () => removeWaypoint(w),
        },
      ]);
    });

    return card;
  }

  async function removeWaypoint(w: ResolvedWaypoint): Promise<void> {
    const trailRel = activeTrailRel();
    if (trailRel === null) return;
    // status: trails-mode-remove-waypoint-verb — fetch the cascade
    // size before showing the confirm modal so the dialog can name
    // the child count per spec. `descendant_count` returns the total
    // including the target itself; the spec example phrases it as
    // "N side-trail waypoints" (excluding the target), so we
    // subtract one for the user-facing copy.
    let totalNodes = 1;
    try {
      totalNodes = await Ipc.trailDescendantCount({
        trailDocRel: trailRel,
        waypointId: w.waypoint_id,
      });
    } catch (err) {
      Logger.error("ui::trails", "trail_descendant_count failed", { err });
      // Fall back to the no-cascade copy — the underlying remove call
      // will still cascade on the backend regardless.
      totalNodes = 1;
    }
    const sideCount = totalNodes > 1 ? totalNodes - 1 : 0;
    const message =
      sideCount > 0
        ? `Remove this waypoint and ${sideCount} side-trail waypoint${sideCount === 1 ? "" : "s"}? Each waypoint-note moves to trash, restorable.`
        : "Remove this waypoint? The annotation moves to trash, restorable.";
    const ok = await confirmDanger(message, "Remove waypoint");
    if (!ok) return;
    try {
      const outcome = await Ipc.trailRemoveWaypoint({
        trailDocRel: trailRel,
        waypointId: w.waypoint_id,
      });
      await refresh();
      // Notify the host so the trash panel + vault-home activity
      // surface pick up the cascaded deletes. `core::ops::delete`
      // suppresses the watcher for deleted paths, so the
      // `hiker:file-changed`-driven refresh path can't fire here.
      deps.onWaypointRemoved?.();
      const removed = outcome.removed_count;
      if (removed > 1) {
        const sides = removed - 1;
        showToast(
          `Waypoint and ${sides} side-trail waypoint${sides === 1 ? "" : "s"} removed`,
        );
      } else {
        showToast("Waypoint removed");
      }
    } catch (err) {
      Logger.error("ui::trails", "trail_remove_waypoint failed", { err });
      showToast(`Remove failed: ${deps.formatErr(err)}`);
    }
  }

  function paint(): void {
    deps.rootEl.replaceChildren();
    deps.rootEl.appendChild(renderHeader());
    const hint = renderCursorHint();
    if (hint !== null) deps.rootEl.appendChild(hint);

    const empty = renderEmptyState();
    if (empty.children.length > 0) {
      deps.rootEl.appendChild(empty);
      return;
    }

    if (detailCache !== null && detailCache.waypoints.length > 0) {
      const cards = document.createElement("div");
      cards.className = "trails-cards";
      // status: trails-mode-side-trail-render — render the recursive
      // waypoint tree directly; nested children render in a
      // `.trails-side-trail` block under their parent (one indent
      // step + thin left rule per nesting level).
      for (const w of detailCache.waypoints) {
        cards.appendChild(renderWaypointBlock(w));
      }
      deps.rootEl.appendChild(cards);
    }
  }

  // status: trails-mode-side-trail-render — render a waypoint plus
  // (when it has children and the side trail isn't collapsed) a
  // nested side-trail block holding each child rendered the same way.
  function renderWaypointBlock(w: ResolvedWaypoint): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "waypoint-block";
    wrap.appendChild(renderWaypointCard(w));
    const children = w.children ?? [];
    if (children.length > 0 && !sideTrailCollapsed.has(w.waypoint_id)) {
      const nested = document.createElement("div");
      nested.className = "trails-side-trail";
      for (const c of children) {
        nested.appendChild(renderWaypointBlock(c));
      }
      wrap.appendChild(nested);
    }
    return wrap;
  }

  // Walk the recursive `ResolvedWaypoint` tree depth-first; used by
  // `refresh` to drop stale per-waypoint state when the tree shape
  // changes.
  function walkTree(roots: ResolvedWaypoint[]): ResolvedWaypoint[] {
    const out: ResolvedWaypoint[] = [];
    function walk(w: ResolvedWaypoint): void {
      out.push(w);
      for (const c of w.children ?? []) walk(c);
    }
    for (const r of roots) walk(r);
    return out;
  }

  async function refresh(): Promise<void> {
    const myEpoch = ++fetchEpoch;
    let list: TrailListItem[] = [];
    try {
      list = await Ipc.trailsList();
    } catch (err) {
      Logger.error("ui::trails", "trails_list failed", { err });
    }
    if (myEpoch !== fetchEpoch) return;
    listCache = list;

    const activeRel = activeTrailRel();
    if (activeRel === null) {
      detailCache = null;
      expandedWaypointId = null;
      paint();
      return;
    }
    // Active-trail-deleted fallback: if the previously-active trail-doc
    // is no longer in the vault's trails list, clear the active-trail
    // setting so the dropdown falls back to "None" cleanly. Without
    // this, the dropdown shows the deleted trail's basename and any
    // re-activation attempt errors. Surgical: only fires when the rel
    // genuinely vanished from `trails_list`.
    if (!list.some((t) => t.rel_path === activeRel)) {
      try {
        await Ipc.trailSetActive({ trailDocRel: null });
      } catch (err) {
        Logger.error("ui::trails", "trail_set_active(null) fallback failed", {
          err,
          rel: activeRel,
        });
      }
      if (myEpoch !== fetchEpoch) return;
      activeTrailStore.set({ rel: null });
      detailCache = null;
      expandedWaypointId = null;
      paint();
      return;
    }
    let detail: TrailDetail | null = null;
    try {
      detail = await Ipc.trailGet({ trailDocRel: activeRel });
    } catch (err) {
      Logger.error("ui::trails", "trail_get failed", { err, rel: activeRel });
    }
    if (myEpoch !== fetchEpoch) return;
    detailCache = detail;
    // If the previously-expanded card disappeared, drop the pin so the
    // chevron state stays sane.
    if (detail !== null) {
      const allIds = new Set(walkTree(detail.waypoints).map((w) => w.waypoint_id));
      if (
        expandedWaypointId !== null
        && !allIds.has(expandedWaypointId)
      ) {
        expandedWaypointId = null;
      }
      // Drop side-trail-collapse entries for waypoints that have
      // disappeared so the Set doesn't accumulate stale ids.
      for (const id of [...sideTrailCollapsed]) {
        if (!allIds.has(id)) sideTrailCollapsed.delete(id);
      }
    }
    paint();
  }

  // Initial paint — empty state until first refresh resolves.
  paint();

  const api: TrailsApi = {
    refresh,
    onActiveTrailMaybeChanged: () => {
      void refresh();
    },
  };

  return createPanelController<TrailsApi>(api, {
    initialVisible: true,
    applyOnMount: false,
    onSetVisible: () => {
      // Sidebar-managed; no panel-level visibility toggle.
    },
  });
}
