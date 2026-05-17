// status: cluster-editor-graph-view
// status: cluster-editor-graph-view-toggle
// status: cluster-editor-graph-view-not-vault-graph
// status: cluster-editor-graph-view-layout
// status: cluster-editor-graph-view-layout-extensible
// status: cluster-editor-graph-view-color-by-policy
// status: cluster-editor-graph-view-size-by-members
// status: cluster-editor-graph-view-summary-staleness-tint
// status: cluster-editor-graph-view-click-to-edit-policy
// status: cluster-editor-graph-view-hover-detail
// status: cluster-editor-graph-view-policy-filter
// status: cluster-editor-graph-view-selection-outline
// status: cluster-editor-graph-view-leaf-click-opens-note
// status: cluster-editor-graph-view-no-reshape
// status: cluster-editor-graph-view-pan-zoom-keybinds
// status: cluster-editor-graph-view-view-menu
// status: cluster-editor-graph-view-leaf-visibility
// status: cluster-editor-graph-view-show-outliers
// status: cluster-editor-graph-view-reset-fit
// status: cluster-editor-graph-view-outlier-disconnected
// status: cluster-editor-graph-view-saved-view-state
// status: cluster-editor-graph-view-lazy-load
//
// Cluster-editor graph view. Sub-mode of the pane's `cluster-tree`
// state — flipped by the toolbar toggle, not a new `BufferMode`.
//
// The renderer is dynamically imported on first mount so the sigma +
// graphology bundle is paid only when the user opens the graph view.
// (`cluster-editor-graph-view-lazy-load`).

import { invoke } from "@tauri-apps/api/core";
import { Logger } from "../../logger";
import { showToast } from "../../widgets/toast";
import { describeErr } from "../../ipc/runCommand";
import { openContextMenu, openMenuAtAnchor, type CtxMenuItem } from "../../widgets/contextMenu";
import type { ClusterNodeRow } from "../../clusterEditor";
import type {
  GraphRenderer,
  RendererData,
  RendererEdge,
  RendererNode,
} from "../../graphRenderer";
import {
  LAYOUTS,
  DEFAULT_LAYOUT_ID,
  type GraphNodeInput,
} from "./layouts";
import {
  resolvePolicy,
  sizeForMembers,
  type PolicyKind,
} from "./encoding";

type LeafVisibility = "hide" | "auto" | "show";

interface SavedViewState {
  layoutId: string;
  leafVisibility: LeafVisibility;
  showOutliers: boolean;
  policyFilter: PolicyKind | "all" | "require-review";
  /// When true, the pinned note-preview card shows the last leaf you
  /// single-clicked. Double-click on a leaf opens it as a full note tab.
  showNotePreview: boolean;
  camera?: { x: number; y: number; ratio: number };
}

const DEFAULT_VIEW_STATE: SavedViewState = {
  layoutId: DEFAULT_LAYOUT_ID,
  leafVisibility: "auto",
  showOutliers: true,
  policyFilter: "all",
  showNotePreview: true,
};

const STORAGE_PREFIX = "hiker.clusterEditor.graphView.";

function loadViewState(treeId: string): SavedViewState {
  // status: cluster-editor-graph-view-saved-view-state
  //
  // We persist per-tree view state in localStorage rather than adding
  // a new `cluster_trees.view_state` column for this sprint. That
  // keeps the schema migration footprint zero and matches the spec's
  // explicit "or in `vault/.hiker/config.toml`" — the spec calls out
  // either as acceptable. localStorage is the simplest of those for
  // the UI-only scope of this state.
  try {
    const raw = localStorage.getItem(STORAGE_PREFIX + treeId);
    if (!raw) return { ...DEFAULT_VIEW_STATE };
    const parsed = JSON.parse(raw) as Partial<SavedViewState>;
    return { ...DEFAULT_VIEW_STATE, ...parsed };
  } catch {
    return { ...DEFAULT_VIEW_STATE };
  }
}

function saveViewState(treeId: string, state: SavedViewState): void {
  try {
    localStorage.setItem(STORAGE_PREFIX + treeId, JSON.stringify(state));
  } catch (err) {
    Logger.error("ui::clusterEditor", "saveViewState failed", {
      err,
    });
  }
}

export interface GraphViewDeps {
  treeId: string;
  nodes: ClusterNodeRow[];
  /// Where the renderer canvas should mount.
  hostEl: HTMLElement;
  /// Open a note in the editor (leaf-click handler).
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// Refresh data after a policy mutation (re-fetch the tree rows).
  onMutated: () => Promise<void> | void;
  /// Selection mirrored back to the host (e.g. so the bulk-action
  /// toolbar shows the count).
  onSelectionChanged?: (selection: Set<string>) => void;
}

export interface GraphViewApi {
  /// Replace the in-canvas data with fresh rows (after a mutation).
  setNodes(nodes: ClusterNodeRow[]): void;
  /// Open the view-options popover anchored to `anchor`. Exposed so
  /// the host pane can host the trigger button on its own toolbar
  /// rather than inside the graph view's local chrome.
  openViewMenu(anchor: HTMLElement): void;
  /// Returns the graph-specific view-options items (leaves visibility,
  /// layout, show outliers, fit/reset). Exposed so the pane can fold
  /// these into a unified "view" menu that also carries the view-mode
  /// (tree / graph / markdown) selector, without each surface having
  /// to maintain its own menu trigger.
  getViewMenuItems(): import("../../widgets/contextMenu").CtxMenuItem[];
  destroy(): void;
}

/// Mount the graph view inside `hostEl`. Returns the API once the
/// renderer has loaded; throws on import failure.
export async function mountGraphView(
  deps: GraphViewDeps,
): Promise<GraphViewApi> {
  const { graphRendererModule } = await loadRendererModule();
  return mount(deps, graphRendererModule);
}

// ── Lazy loader (cluster-editor-graph-view-lazy-load) ───────────────

let cached: Promise<{ graphRendererModule: typeof import("../../graphRenderer") }> | null =
  null;

function loadRendererModule(): Promise<{
  graphRendererModule: typeof import("../../graphRenderer");
}> {
  if (!cached) {
    cached = import("../../graphRenderer").then((m) => ({
      graphRendererModule: m,
    }));
  }
  return cached;
}

// ── Mount ───────────────────────────────────────────────────────────

function mount(
  deps: GraphViewDeps,
  rendererMod: typeof import("../../graphRenderer"),
): GraphViewApi {
  const { treeId, hostEl, openNote, onMutated, onSelectionChanged } = deps;
  let nodes: ClusterNodeRow[] = deps.nodes;
  let viewState = loadViewState(treeId);
  const selection = new Set<string>();
  let hoveredId: string | null = null;
  let hoverStart = 0;
  let extendedHover = false;
  let tooltipEl: HTMLElement | null = null;
  let renderer: GraphRenderer | null = null;
  let canvasHost: HTMLElement | null = null;
  let persistTimer: number | null = null;
  // Latest pointer position in viewport coords — used to anchor the
  // hover tooltip near the cursor rather than at the canvas corner.
  // Updated on every pointermove over the canvas; falls back to the
  // canvas's top-left when no pointer movement has been observed yet.
  let lastMouse: { x: number; y: number } | null = null;

  // Pin-state for the note-preview card. Declared up here (rather than
  // alongside the rendering helpers below) because the initial
  // `refreshNoteOverlayVisibility()` call during chrome setup reads
  // `pinnedLeafId`, and a `let` declared further down would be in TDZ
  // at that point.
  const noteBodyCache = new Map<string, string>();
  const noteFetchInFlight = new Set<string>();
  let pinnedLeafId: string | null = null;

  // Build chrome: filter strip on top, view-menu button, canvas, and
  // a selection-count footer.
  hostEl.replaceChildren();
  hostEl.classList.add("cep-graph");

  const chrome = document.createElement("div");
  chrome.className = "cep-graph-chrome";

  // Filter strip — policy filter.
  // status: cluster-editor-graph-view-policy-filter
  const filterStrip = document.createElement("div");
  filterStrip.className = "cep-graph-filter";
  const filters: Array<{ id: PolicyKind | "all" | "require-review"; label: string }> = [
    { id: "all", label: "All" },
    { id: "move", label: "Move" },
    { id: "tag", label: "Tag" },
    { id: "freeze", label: "Freeze" },
    { id: "none", label: "No policy" },
    { id: "require-review", label: "Require review" },
  ];
  for (const f of filters) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "cep-graph-filter-btn";
    btn.dataset.filterId = f.id;
    btn.textContent = f.label;
    if (viewState.policyFilter === f.id) btn.classList.add("active");
    btn.addEventListener("click", () => {
      viewState.policyFilter = f.id;
      persist();
      refreshFilterUI();
      rebuild();
    });
    filterStrip.appendChild(btn);
  }
  chrome.appendChild(filterStrip);

  function refreshFilterUI(): void {
    for (const child of filterStrip.children) {
      const el = child as HTMLElement;
      el.classList.toggle("active", el.dataset.filterId === viewState.policyFilter);
    }
  }

  // View-menu trigger lives on the parent pane's toolbar (see
  // `clusterEditorPane/index.ts::paintTree` — eye-icon button next to
  // the view toggle). Exposed below via `GraphViewApi.openViewMenu`.
  // status: cluster-editor-graph-view-view-menu

  hostEl.appendChild(chrome);

  // Canvas host.
  canvasHost = document.createElement("div");
  canvasHost.className = "cep-graph-canvas";
  hostEl.appendChild(canvasHost);

  // Track the latest pointer position over the canvas so the tooltip
  // can anchor near the cursor. Sigma's hover event doesn't carry
  // viewport coords on the public type, so we listen directly on the
  // canvas host. `passive: true` since we never preventDefault here.
  canvasHost.addEventListener(
    "pointermove",
    (e) => {
      lastMouse = { x: e.clientX, y: e.clientY };
      if (tooltipEl) positionTooltip();
    },
    { passive: true },
  );
  canvasHost.addEventListener(
    "pointerleave",
    () => {
      lastMouse = null;
    },
    { passive: true },
  );

  // Zoom-in note overlay — a scrollable card pinned to the right edge
  // of the canvas that shows the centered leaf's note text when the
  // user zooms past `ZOOM_NOTE_THRESHOLD`. Hidden otherwise.
  const noteOverlay = document.createElement("div");
  noteOverlay.className = "cep-graph-note-overlay";
  noteOverlay.hidden = true;
  canvasHost.appendChild(noteOverlay);

  // Selection footer.
  const footer = document.createElement("div");
  footer.className = "cep-graph-footer";
  hostEl.appendChild(footer);

  // status: cluster-editor-graph-view-pan-zoom-keybinds
  //
  // Pan/zoom rides sigma's built-in mouse captors (pointer drag for
  // pan, wheel for zoom, pinch for zoom-on-touch). The chord ids
  // reserved in the spec (`cluster-editor.graph-pan` etc.) are
  // documented here for keybind-registry follow-up; defaults map to
  // sigma's built-ins so no extra wiring needed at this layer.

  // status: cluster-editor-graph-view-leaf-visibility
  //
  // LOD: hide/show leaves based on zoom in "auto" mode. We track the
  // last computed visibility so we only rebuild when it actually
  // crosses the threshold — the camera fires "updated" on every pan
  // frame too, and rebuilding on each would be wasteful.
  let lastLeafVisibilityShown: boolean | null = null;
  const LOD_LEAF_THRESHOLD = 1.2;

  renderer = rendererMod.createSigmaRenderer(canvasHost, {
    onNodeClick: (id, { shift }) => handleNodeClick(id, shift),
    onNodeDoubleClick: (id) => handleNodeDoubleClick(id),
    onNodeRightClick: (id, pos) => handleNodeRightClick(id, pos),
    onNodeHover: (id) => handleHover(id),
    onBackgroundClick: () => {
      if (selection.size > 0) {
        selection.clear();
        onSelectionChanged?.(new Set(selection));
        rebuild();
        renderFooter();
      }
      // Clearing the selection on a blank click also dismisses the
      // pinned note-preview overlay.
      unpinLeaf();
      hideTooltip();
    },
    onCameraUpdate: (cam) => {
      persistCameraSoon();
      if (viewState.leafVisibility === "auto") {
        const shouldShow = cam.ratio < LOD_LEAF_THRESHOLD;
        if (lastLeafVisibilityShown !== shouldShow) {
          lastLeafVisibilityShown = shouldShow;
          rebuild();
        }
      }
    },
  });

  // Restore camera if any.
  if (viewState.camera) {
    renderer.setCamera(viewState.camera);
  }

  rebuild();
  renderFooter();
  refreshNoteOverlayVisibility();

  // ── Build cycle ───────────────────────────────────────────────────

  function rebuild(): void {
    if (!renderer) return;
    const data = composeRendererData();
    renderer.setGraph(data);
    // Snapshot camera each rebuild for persistence.
    persistCameraSoon();
  }

  function persistCameraSoon(): void {
    if (!renderer || !renderer.capabilities.camera) return;
    if (persistTimer != null) window.clearTimeout(persistTimer);
    persistTimer = window.setTimeout(() => {
      const cam = renderer?.getCamera();
      if (cam) {
        viewState.camera = { x: cam.x, y: cam.y, ratio: cam.ratio };
        persist();
      }
    }, 400);
  }

  function persist(): void {
    saveViewState(treeId, viewState);
  }

  // ── Pinned note preview ───────────────────────────────────────────
  //
  // A small card pinned to the top-right of the canvas. Single-click a
  // leaf to load its body here (lazy, cached); double-click opens the
  // note as a full editor tab. The `pinnedLeafId` / cache state is
  // hoisted up near `lastMouse` so the early-init call can read it.

  function refreshNoteOverlayVisibility(): void {
    // Only show the overlay once the user has selected a leaf — there's
    // no value in occluding the canvas with an empty "click a note"
    // hint card when no preview is pinned.
    noteOverlay.hidden = !viewState.showNotePreview || pinnedLeafId == null;
  }

  function pinLeaf(leaf: ClusterNodeRow): void {
    if (!leaf.note_path) return;
    pinnedLeafId = leaf.id;
    renderPinnedNote(leaf);
    refreshNoteOverlayVisibility();
  }

  function unpinLeaf(): void {
    if (pinnedLeafId == null) return;
    pinnedLeafId = null;
    noteOverlay.replaceChildren();
    refreshNoteOverlayVisibility();
  }

  function renderPinnedNote(leaf: ClusterNodeRow): void {
    const path = leaf.note_path!;
    noteOverlay.replaceChildren();
    const header = document.createElement("div");
    header.className = "cep-graph-note-header";
    const title = document.createElement("strong");
    title.textContent = leaf.note_title ?? path;
    title.title = path;
    header.appendChild(title);
    const open = document.createElement("button");
    open.type = "button";
    open.className = "cep-graph-note-open";
    open.textContent = "Open ↗";
    open.addEventListener("click", () => {
      void openNote(path, { preview: false });
    });
    header.appendChild(open);
    noteOverlay.appendChild(header);

    const body = document.createElement("pre");
    body.className = "cep-graph-note-body";
    noteOverlay.appendChild(body);

    const cached = noteBodyCache.get(path);
    if (cached != null) {
      body.textContent = cached;
      return;
    }
    body.textContent = "Loading…";
    if (noteFetchInFlight.has(path)) return;
    noteFetchInFlight.add(path);
    invoke<string>("read_file", { rel: path })
      .then((text) => {
        noteBodyCache.set(path, text);
        noteFetchInFlight.delete(path);
        if (pinnedLeafId === leaf.id) body.textContent = text;
      })
      .catch((err) => {
        noteFetchInFlight.delete(path);
        Logger.error("ui::clusterEditor", "note read failed", { err, path });
        if (pinnedLeafId === leaf.id) {
          body.textContent = `Failed to load: ${describeErr(err)}`;
        }
      });
  }

  function composeRendererData(): RendererData {
    const byId = new Map<string, ClusterNodeRow>();
    for (const n of nodes) byId.set(n.id, n);

    // Member-count for each cluster node.
    const memberCountById = new Map<string, number>();
    for (const n of nodes) {
      if (n.kind === "cluster" || n.kind === "outlier-bucket") {
        memberCountById.set(n.id, countMembers(nodes, n.id));
      }
    }

    // status: cluster-editor-graph-view-leaf-visibility
    // status: cluster-editor-graph-view-show-outliers
    //
    // Leaf visibility: "hide" drops all leaves; "auto" hides leaves
    // when zoomed out below threshold; "show" keeps every leaf in the
    // canvas.
    let showLeaves = true;
    if (viewState.leafVisibility === "hide") showLeaves = false;
    else if (viewState.leafVisibility === "auto") {
      const cam = renderer?.getCamera();
      showLeaves = cam ? cam.ratio < 1.2 : true;
    }

    // Show-outliers toggle.
    const showOutliers = viewState.showOutliers;
    const outlierIds = new Set<string>();
    for (const n of nodes) {
      if (n.kind === "outlier-bucket") {
        outlierIds.add(n.id);
        // Mark its descendant leaves so we can hide them too.
        const desc = collectDescendants(nodes, n.id);
        for (const d of desc) outlierIds.add(d);
      }
    }

    // Build input list for the layout.
    const layoutInputs: GraphNodeInput[] = [];
    const renderableIds = new Set<string>();
    for (const n of nodes) {
      if (!showLeaves && n.kind === "leaf") continue;
      if (!showOutliers && outlierIds.has(n.id)) continue;
      renderableIds.add(n.id);
      layoutInputs.push({
        row: n,
        parent: n.parent,
        isOutlier: outlierIds.has(n.id),
        memberCount: memberCountById.get(n.id) ?? 0,
      });
    }

    const layout = LAYOUTS[viewState.layoutId] ?? LAYOUTS[DEFAULT_LAYOUT_ID];
    const positions = layout.assignPositions(layoutInputs);

    const filter = viewState.policyFilter;
    const out: RendererNode[] = [];
    for (const n of nodes) {
      if (!renderableIds.has(n.id)) continue;
      const pos = positions.get(n.id) ?? { x: 0, y: 0 };
      const resolved = resolvePolicy(n, byId);
      const isLeaf = n.kind === "leaf";
      // Nodes render as black outlined rings via NodeBorderProgram —
      // policy/staleness/leaf distinctions live in the row view and
      // the hover tooltip; the graph stays visually quiet.
      const color = "#000";

      // Policy filter dim. "Highlight only X" = full opacity for X,
      // 0.25 for everything else.
      let opacity = 1;
      if (filter !== "all") {
        const matches =
          filter === "require-review"
            ? resolved.requireReview
            : resolved.kind === filter;
        if (!matches) opacity = 0.25;
      }

      // Selection ring.
      const outlineColor = selection.has(n.id)
        ? "var(--accent, #f97316)"
        : undefined;

      const memberCount =
        memberCountById.get(n.id) ?? (isLeaf ? 0 : 0);
      const size = sizeForMembers(memberCount, isLeaf);

      let label: string | undefined;
      if (n.kind === "outlier-bucket") {
        label = `Outliers (${memberCount})`;
      } else if (n.kind === "cluster") {
        const glyph = resolved.requireReview ? " ⏸" : "";
        label = `${n.name}${glyph}`;
      } else {
        label = n.note_title ?? n.note_ref ?? "";
      }

      out.push({
        id: n.id,
        x: pos.x,
        y: pos.y,
        size: selection.has(n.id) ? size + 2 : size,
        color,
        label,
        outlineColor,
        opacity,
        data: { kind: n.kind },
      });
    }

    // Edges: parent → child for the renderable, non-outlier set.
    const edges: RendererEdge[] = [];
    for (const n of nodes) {
      if (!renderableIds.has(n.id)) continue;
      if (!n.parent || !renderableIds.has(n.parent)) continue;
      // Outliers render disconnected — skip their parent edge if the
      // parent is the outlier bucket.
      if (outlierIds.has(n.parent)) continue;
      edges.push({
        id: `${n.parent}->${n.id}`,
        source: n.parent,
        target: n.id,
        color: "#000",
        size: 1,
      });
    }

    return { nodes: out, edges };
  }

  // ── Interaction ───────────────────────────────────────────────────

  function handleNodeClick(id: string, shift: boolean): void {
    const row = nodes.find((n) => n.id === id);
    if (!row) return;
    // status: cluster-editor-graph-view-selection-outline
    // Plain left-click selects the node (single-select, replacing any
    // existing selection). Shift+click extends/toggles the multi-select.
    // The policy popover moved to right-click (`onNodeRightClick`).
    if (shift) {
      if (selection.has(id)) selection.delete(id);
      else selection.add(id);
    } else {
      selection.clear();
      selection.add(id);
    }
    onSelectionChanged?.(new Set(selection));
    rebuild();
    renderFooter();
    // For a leaf, also pin its note into the preview card so a single
    // click acts as "select + show contents" (the preview affordance
    // only fires when the user has the overlay enabled).
    if (
      !shift
      && row.kind === "leaf"
      && row.note_path
      && viewState.showNotePreview
    ) {
      pinLeaf(row);
    }
  }

  function handleNodeRightClick(
    id: string,
    pos: { clientX: number; clientY: number },
  ): void {
    const row = nodes.find((n) => n.id === id);
    if (!row) return;
    // status: cluster-editor-graph-view-click-to-edit-policy
    // Right-click opens the policy menu on cluster / outlier-bucket
    // nodes. Leaves have no per-node menu yet — falling through is
    // intentional (matches the row primitive's right-click behavior).
    if (row.kind === "leaf") return;
    openPolicyMenuFor(row, id, pos);
  }

  function handleNodeDoubleClick(id: string): void {
    const row = nodes.find((n) => n.id === id);
    if (!row) return;
    // Only leaves act on double-click; cluster nodes' single-click
    // opens the policy popover (above) and double-click is a no-op so
    // accidental rapid clicks don't fire twice.
    if (row.kind !== "leaf" || !row.note_path) return;
    // Open as a preview tab (matches sidebar / activity behavior) —
    // the full tab is reachable from the preview's own pin affordance.
    void openNote(row.note_path, { preview: true });
  }

  function openPolicyMenuFor(
    row: ClusterNodeRow,
    sigmaNodeId: string,
    anchor?: { clientX: number; clientY: number },
  ): void {
    // Anchor at the pointer location when called from the right-click
    // path (the natural place for a context menu); fall back to the
    // canvas center for legacy/programmatic call sites.
    const rect = canvasHost!.getBoundingClientRect();
    const items: CtxMenuItem[] = [
      {
        label: "Tag…",
        run: () => {
          const slug = window.prompt("Tag slug:", "");
          if (!slug) return;
          const req = window.confirm("Require review for matches?");
          const policy = JSON.stringify({
            kind: "tag",
            slug,
            require_review: req,
          });
          void applyPolicy(row.id, policy);
        },
      },
      {
        label: "Move to folder…",
        run: () => {
          const folder = window.prompt("Target folder (vault-relative):", "");
          if (!folder) return;
          const req = window.confirm("Require review for matches?");
          const policy = JSON.stringify({
            kind: "move",
            folder,
            require_review: req,
          });
          void applyPolicy(row.id, policy);
        },
      },
      {
        label: "Freeze",
        run: () => {
          const policy = JSON.stringify({ kind: "freeze" });
          void applyPolicy(row.id, policy);
        },
      },
      {
        label: "Clear policy",
        run: () => void applyPolicy(row.id, null),
      },
    ];
    // status: cluster-editor-graph-view-summary-staleness-tint
    // Offer Regenerate-summary if the node carries churn.
    if (row.summary_membership_churn > 0 && row.kind === "cluster") {
      items.push({
        label: `Regenerate summary (↻ ${row.summary_membership_churn})`,
        run: () => {
          void invoke("cluster_summarize_node", {
            treeId,
            nodeId: row.id,
          }).then(
            () => showToast("Summary regeneration queued"),
            (err) => showToast(`Regenerate failed: ${describeErr(err)}`),
          );
        },
      });
    }
    const anchorX = anchor?.clientX ?? rect.left + rect.width / 2;
    const anchorY = anchor?.clientY ?? rect.top + rect.height / 2;
    openContextMenu(anchorX, anchorY, items);
    // Silence unused-var warning for sigmaNodeId — kept in the
    // signature to make future "anchor at exact node coords" work
    // straightforward.
    void sigmaNodeId;
  }

  async function applyPolicy(nodeId: string, policyJson: string | null): Promise<void> {
    try {
      await invoke("cluster_node_set_policy", {
        treeId,
        nodeId,
        policyJson,
      });
      await onMutated();
    } catch (err) {
      showToast(`Policy failed: ${describeErr(err)}`);
    }
  }

  // status: cluster-editor-graph-view-hover-detail
  function handleHover(id: string | null): void {
    hoveredId = id;
    if (id == null) {
      hideTooltip();
      return;
    }
    hoverStart = performance.now();
    extendedHover = false;
    const row = nodes.find((n) => n.id === id);
    if (!row) return;
    showTooltip(row, false);
    // 500ms held-hover expands the tooltip to include member titles.
    window.setTimeout(() => {
      if (hoveredId === id && performance.now() - hoverStart >= 500) {
        extendedHover = true;
        showTooltip(row, true);
      }
    }, 510);
  }

  function showTooltip(row: ClusterNodeRow, expanded: boolean): void {
    hideTooltip();
    const t = document.createElement("div");
    t.className = "cep-graph-tooltip";
    const title = document.createElement("strong");
    title.textContent =
      row.kind === "leaf"
        ? row.note_title ?? row.note_ref ?? "(unknown)"
        : row.name;
    t.appendChild(title);
    if (row.kind !== "leaf") {
      const byId = new Map<string, ClusterNodeRow>();
      for (const n of nodes) byId.set(n.id, n);
      const resolved = resolvePolicy(row, byId);
      const meta = document.createElement("div");
      meta.className = "cep-graph-tooltip-meta";
      const members = countMembers(nodes, row.id);
      const policyLabel =
        resolved.kind === "none"
          ? "no policy"
          : resolved.explicit
            ? resolved.kind
            : `${resolved.kind} (inherited)`;
      meta.textContent = `${members} members · ${policyLabel}${
        row.summary_membership_churn > 0
          ? ` · ↻ ${row.summary_membership_churn}`
          : ""
      }`;
      t.appendChild(meta);
      if (row.summary) {
        const s = document.createElement("div");
        s.className = "cep-graph-tooltip-summary";
        s.textContent = row.summary;
        t.appendChild(s);
      }
      if (expanded) {
        const members = leafTitlesUnder(nodes, row.id, 10);
        if (members.titles.length > 0) {
          const list = document.createElement("ul");
          list.className = "cep-graph-tooltip-members";
          for (const tt of members.titles) {
            const li = document.createElement("li");
            li.textContent = tt;
            list.appendChild(li);
          }
          if (members.more > 0) {
            const li = document.createElement("li");
            li.textContent = `and ${members.more} more`;
            li.className = "cep-graph-tooltip-more";
            list.appendChild(li);
          }
          t.appendChild(list);
        }
      }
    }
    document.body.appendChild(t);
    tooltipEl = t;
    positionTooltip();
    void extendedHover;
  }

  function positionTooltip(): void {
    if (!tooltipEl || !canvasHost) return;
    // Anchor near the cursor with a 12,12 offset so the tooltip sits
    // just below-right of the pointer. Fall back to the canvas's top-
    // left corner when no pointer movement has been observed (e.g. the
    // hover event fires before the first pointermove on slow first-
    // mount frames). Both branches clamp to the viewport so the
    // tooltip never lands off-screen.
    const tipRect = tooltipEl.getBoundingClientRect();
    let x: number;
    let y: number;
    if (lastMouse) {
      x = lastMouse.x + 12;
      y = lastMouse.y + 12;
    } else {
      const rect = canvasHost.getBoundingClientRect();
      x = rect.left + 12;
      y = rect.top + 12;
    }
    // Clamp against the viewport so an edge-hover doesn't slide the
    // tooltip past the screen.
    const maxX = window.innerWidth - tipRect.width - 4;
    const maxY = window.innerHeight - tipRect.height - 4;
    if (x > maxX) x = Math.max(4, maxX);
    if (y > maxY) y = Math.max(4, maxY);
    tooltipEl.style.left = `${x}px`;
    tooltipEl.style.top = `${y}px`;
  }

  function hideTooltip(): void {
    if (tooltipEl) {
      tooltipEl.remove();
      tooltipEl = null;
    }
  }

  // ── View menu ─────────────────────────────────────────────────────

  function openViewMenu(anchor: HTMLElement): void {
    openMenuAtAnchor(anchor, buildViewMenuItems(), { align: "right" });
  }

  function buildViewMenuItems(): CtxMenuItem[] {
    return [
      // status: cluster-editor-graph-view-leaf-visibility
      {
        kind: "radio",
        label: "Leaves",
        value: viewState.leafVisibility,
        options: [
          { label: leafVisibilityLabel("hide"), value: "hide" },
          { label: leafVisibilityLabel("auto"), value: "auto" },
          { label: leafVisibilityLabel("show"), value: "show" },
        ],
        onChange: (v) => {
          viewState.leafVisibility = v as LeafVisibility;
          persist();
          rebuild();
        },
      },
      // status: cluster-editor-graph-view-layout
      // status: cluster-editor-graph-view-layout-extensible
      {
        kind: "radio",
        label: "Layout",
        value: viewState.layoutId,
        options: Object.values(LAYOUTS).map((l) => ({
          label: l.label,
          value: l.id,
        })),
        onChange: (v) => {
          viewState.layoutId = v;
          persist();
          rebuild();
        },
      },
      // status: cluster-editor-graph-view-show-outliers
      {
        label: viewState.showOutliers ? "Hide outliers" : "Show outliers",
        run: () => {
          viewState.showOutliers = !viewState.showOutliers;
          persist();
          rebuild();
        },
      },
      {
        label: viewState.showNotePreview
          ? "Hide note preview"
          : "Show note preview",
        run: () => {
          viewState.showNotePreview = !viewState.showNotePreview;
          persist();
          refreshNoteOverlayVisibility();
        },
      },
      // status: cluster-editor-graph-view-reset-fit
      {
        label: "Fit to view",
        run: () => renderer?.fit(),
      },
      {
        label: "Reset view",
        run: () => {
          renderer?.reset();
          persistCameraSoon();
        },
      },
    ];
  }

  function leafVisibilityLabel(v: LeafVisibility): string {
    if (v === "hide") return "Hide leaves";
    if (v === "show") return "Show all leaves";
    return "Auto (LOD)";
  }

  // ── Footer ────────────────────────────────────────────────────────

  function renderFooter(): void {
    footer.replaceChildren();
    if (selection.size > 0) {
      const span = document.createElement("span");
      span.className = "cep-graph-selcount";
      span.textContent = `Selected: ${selection.size} node${
        selection.size === 1 ? "" : "s"
      }`;
      footer.appendChild(span);
      const note = document.createElement("span");
      note.className = "cep-graph-foot-note";
      // status: cluster-editor-graph-view-no-reshape
      note.textContent =
        "Tree-shape edits stay in the row view — graph view is overview + policy assignment.";
      footer.appendChild(note);
    }
  }

  // ── API ───────────────────────────────────────────────────────────

  function setNodes(next: ClusterNodeRow[]): void {
    nodes = next;
    rebuild();
    renderFooter();
  }

  function destroy(): void {
    hideTooltip();
    if (persistTimer != null) window.clearTimeout(persistTimer);
    renderer?.destroy();
    renderer = null;
  }

  return { setNodes, destroy, openViewMenu, getViewMenuItems: buildViewMenuItems };
}

// ── Helpers ─────────────────────────────────────────────────────────

function countMembers(nodes: ClusterNodeRow[], rootId: string): number {
  let count = 0;
  const childMap = new Map<string, ClusterNodeRow[]>();
  for (const n of nodes) {
    if (!n.parent) continue;
    const arr = childMap.get(n.parent);
    if (arr) arr.push(n);
    else childMap.set(n.parent, [n]);
  }
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop()!;
    const kids = childMap.get(id) ?? [];
    for (const k of kids) {
      if (k.kind === "leaf") count += 1;
      else stack.push(k.id);
    }
  }
  return count;
}

function collectDescendants(
  nodes: ClusterNodeRow[],
  rootId: string,
): Set<string> {
  const out = new Set<string>();
  const childMap = new Map<string, ClusterNodeRow[]>();
  for (const n of nodes) {
    if (!n.parent) continue;
    const arr = childMap.get(n.parent);
    if (arr) arr.push(n);
    else childMap.set(n.parent, [n]);
  }
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop()!;
    const kids = childMap.get(id) ?? [];
    for (const k of kids) {
      out.add(k.id);
      stack.push(k.id);
    }
  }
  return out;
}

function leafTitlesUnder(
  nodes: ClusterNodeRow[],
  rootId: string,
  cap: number,
): { titles: string[]; more: number } {
  const titles: string[] = [];
  const childMap = new Map<string, ClusterNodeRow[]>();
  for (const n of nodes) {
    if (!n.parent) continue;
    const arr = childMap.get(n.parent);
    if (arr) arr.push(n);
    else childMap.set(n.parent, [n]);
  }
  let more = 0;
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop()!;
    const kids = childMap.get(id) ?? [];
    for (const k of kids) {
      if (k.kind === "leaf") {
        const t = k.note_title ?? k.note_ref ?? "(untitled)";
        if (titles.length < cap) titles.push(t);
        else more += 1;
      } else {
        stack.push(k.id);
      }
    }
  }
  return { titles, more };
}
