// status: cluster-editor-dnd-reparent, cluster-editor-dnd-visual-feedback
//
// HTML5 drag-and-drop on tree rows. Picking up a row (or any of the
// rows in the current multi-selection if the dragged row is part of
// it) gives the user a draggable payload they can drop on:
//
//  - another cluster row in the same tree (target = that cluster id),
//  - the outlier-bucket virtual row (target = bucket id),
//  - the synthetic "Drop here to make top-level" band rendered above
//    the tree body during drag (target = null sentinel → root level).
//
// The drop calls `Api.move(treeId, dragged_id, target_parent)` once
// per dragged item (history records one `move` row per item via the
// existing single-move IPC). The DnD payload is held in a
// module-level ref because dataTransfer is hard to read during
// `dragover` (cycle-detection happens there).
//
// Validation refuses cycles (dragged == target or target ⊂ dragged's
// subtree), leaf-onto-leaf drops, and cluster-onto-its-current-parent
// noops. Cross-tree drag is prevented at dragstart by gating on
// `state.tree.id` — each row's dragstart records the tree id and the
// drop site bails if it differs.
//
// Visual feedback:
//
//  - `.ce-row-drop-target` on a row currently a valid drop target
//    (accent box-shadow ring + faint accent tint).
//  - `.ce-row-drop-invalid` on a row currently an invalid drop target
//    (cursor: not-allowed).
//  - `.ce-drop-chip` follows the pointer with the dragged item's
//    glyph + name (or "N items" + first glyph for multi-drag).
//  - `.ce-drop-promote-band` is rendered into the surface body during
//    drag by `renderPromoteBand`; surfaces call it from their own
//    render path.

import { Logger } from "../../logger";
import { showToast } from "../../widgets/toast";
import { Api, type ClusterNodeRow } from "./api";
import { collectDescendants } from "./helpers";
import type { TreeRowDeps, TreeRowSurfaceState } from "./state";

interface DragPayload {
  treeId: string;
  /// All ids being dragged (single element for solo drag; the
  /// selection set for multi-drag). Multi-drag requires the
  /// dragstart row to be part of the current selection.
  ids: string[];
  /// Cached display label for the floating chip.
  chipLabel: string;
  /// Cached icon glyph for the floating chip.
  chipGlyph: string;
}

let dragState: DragPayload | null = null;
let dragChipEl: HTMLDivElement | null = null;
/// Listeners that need to repaint when a drag starts/ends — used by
/// the surface modules (`clusterEditor/index.ts`, `clusterEditorPane/
/// index.ts`) to show/hide the promote-to-top band.
const dragListeners = new Set<() => void>();

export function onDragStateChange(cb: () => void): () => void {
  dragListeners.add(cb);
  return () => dragListeners.delete(cb);
}
function notifyDragListeners(): void {
  for (const cb of dragListeners) cb();
}
export function isDragInFlight(): boolean {
  return dragState !== null;
}

export function attachRowDnD(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  node: ClusterNodeRow,
  row: HTMLElement,
): void {
  row.draggable = true;
  const treeId = state.tree.id;

  row.addEventListener("dragstart", (e) => {
    // If the dragstart row is part of the current selection, the
    // payload is the whole selection; otherwise just this row.
    let ids: string[];
    if (state.selection.has(node.id) && state.selection.size > 0) {
      ids = Array.from(state.selection);
    } else {
      ids = [node.id];
    }
    const firstNode = state.nodes.find((n) => n.id === ids[0]) ?? node;
    dragState = {
      treeId,
      ids,
      chipLabel:
        ids.length > 1
          ? `${ids.length} items`
          : firstNode.kind === "leaf"
            ? firstNode.note_title ?? firstNode.note_ref ?? firstNode.name
            : firstNode.name,
      chipGlyph:
        firstNode.kind === "leaf"
          ? "●"
          : firstNode.kind === "outlier-bucket"
            ? "◇"
            : "◉",
    };
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      // Make the API happy; the actual payload routes via dragState.
      try {
        e.dataTransfer.setData("text/plain", node.id);
      } catch {
        /* some browsers throw if setData fires on a non-text type;
           ignore — the side-channel ref is what we read. */
      }
      // Suppress the default ghost image — we render our own chip.
      const blank = document.createElement("canvas");
      blank.width = 1;
      blank.height = 1;
      try {
        e.dataTransfer.setDragImage(blank, 0, 0);
      } catch {
        /* ignore */
      }
    }
    ensureDragChip();
    updateDragChip(e.clientX, e.clientY);
    notifyDragListeners();
  });

  row.addEventListener("dragover", (e) => {
    if (!dragState || dragState.treeId !== treeId) return;
    const verdict = validateDrop(state, dragState, node.id);
    updateDragChip(e.clientX, e.clientY);
    if (verdict === "valid") {
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
      row.classList.add("ce-row-drop-target");
      row.classList.remove("ce-row-drop-invalid");
    } else {
      // Don't preventDefault — the browser shows the no-drop cursor.
      row.classList.add("ce-row-drop-invalid");
      row.classList.remove("ce-row-drop-target");
    }
  });

  row.addEventListener("dragleave", () => {
    row.classList.remove("ce-row-drop-target", "ce-row-drop-invalid");
  });

  row.addEventListener("drop", async (e) => {
    e.preventDefault();
    row.classList.remove("ce-row-drop-target", "ce-row-drop-invalid");
    if (!dragState || dragState.treeId !== treeId) return;
    const verdict = validateDrop(state, dragState, node.id);
    if (verdict !== "valid") return;
    const targetParent = node.id;
    const ids = dragState.ids.slice();
    // Clear drag state immediately so subsequent renders don't think
    // a drag is still in flight if the API calls take a moment.
    dragState = null;
    teardownDragChip();
    notifyDragListeners();
    await performMultiMove(state, deps, ids, targetParent);
  });

  row.addEventListener("dragend", () => {
    row.classList.remove("ce-row-drop-target", "ce-row-drop-invalid");
    // dragend fires for any reason a drag ends (drop, escape,
    // browser cancel). Always clear state to be safe.
    if (dragState) {
      dragState = null;
      teardownDragChip();
      notifyDragListeners();
    }
  });
}

/// Validate a potential drop of `payload.ids` onto `targetId`.
/// Returns "valid" if all of the following hold for every id in
/// `payload.ids`:
///  - the id is not the target (no self-drop),
///  - the target is not a descendant of the id (no cycle),
///  - if the dragged row is a cluster, the target is not its current
///    parent (no-op refused),
///  - the target's kind is not "leaf" (can't drop into a leaf).
function validateDrop(
  state: TreeRowSurfaceState,
  payload: DragPayload,
  targetId: string,
): "valid" | "invalid" {
  const target = state.nodes.find((n) => n.id === targetId);
  if (!target) return "invalid";
  if (target.kind === "leaf") return "invalid";
  for (const id of payload.ids) {
    if (id === targetId) return "invalid";
    const dragged = state.nodes.find((n) => n.id === id);
    if (!dragged) return "invalid";
    // Cluster-onto-its-current-parent noop.
    if (dragged.parent === targetId) return "invalid";
    // Cycle check — target must not be inside dragged's subtree.
    const descendants = collectDescendants(state.nodes, id);
    if (descendants.has(targetId)) return "invalid";
  }
  return "valid";
}

/// Multi-move: loop the single-move IPC so each item gets its own
/// `move` history row (preserving per-item undo granularity). Failures
/// surface a toast per item; successful moves still commit.
async function performMultiMove(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  ids: string[],
  newParent: string | null,
): Promise<void> {
  let failed = 0;
  for (const id of ids) {
    try {
      await Api.move(state.tree.id, id, newParent);
    } catch (err) {
      failed += 1;
      Logger.error("ui::clusterEditor", "dnd move failed", {
        err,
        id,
        newParent,
      });
    }
  }
  state.selection.clear();
  state.anchor = null;
  if (failed > 0) {
    showToast(`Move failed for ${failed} of ${ids.length} item${ids.length === 1 ? "" : "s"}`);
  }
  await deps.refresh();
}

// ── Drag chip (floating pointer-follower) ──────────────────────────

function ensureDragChip(): void {
  if (dragChipEl) return;
  const el = document.createElement("div");
  el.className = "ce-drop-chip";
  document.body.appendChild(el);
  dragChipEl = el;
  document.addEventListener("dragover", chipFollow);
}
function chipFollow(e: DragEvent): void {
  updateDragChip(e.clientX, e.clientY);
}
function updateDragChip(x: number, y: number): void {
  if (!dragChipEl || !dragState) return;
  dragChipEl.style.left = `${x + 12}px`;
  dragChipEl.style.top = `${y + 12}px`;
  dragChipEl.textContent = `${dragState.chipGlyph} ${dragState.chipLabel}`;
}
function teardownDragChip(): void {
  if (dragChipEl) {
    document.removeEventListener("dragover", chipFollow);
    dragChipEl.remove();
    dragChipEl = null;
  }
}

// ── Promote-to-top drop band ───────────────────────────────────────
//
// Surfaces call `renderPromoteBand` during their paint to insert the
// "Drop here to make top-level" affordance above the tree body. It
// only renders when a drag is in flight and the in-flight drag's tree
// matches this surface's tree id; otherwise returns `null`.

export function renderPromoteBand(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
): HTMLElement | null {
  if (!dragState) return null;
  if (dragState.treeId !== state.tree.id) return null;
  const band = document.createElement("div");
  band.className = "ce-drop-promote-band";
  band.textContent = "Drop here to make top-level";
  const treeId = state.tree.id;

  band.addEventListener("dragover", (e) => {
    if (!dragState || dragState.treeId !== treeId) return;
    const verdict = validatePromoteToTop(state, dragState);
    updateDragChip(e.clientX, e.clientY);
    if (verdict === "valid") {
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
      band.classList.add("ce-drop-promote-band-active");
    } else {
      band.classList.add("ce-drop-promote-band-invalid");
    }
  });
  band.addEventListener("dragleave", () => {
    band.classList.remove(
      "ce-drop-promote-band-active",
      "ce-drop-promote-band-invalid",
    );
  });
  band.addEventListener("drop", async (e) => {
    e.preventDefault();
    band.classList.remove(
      "ce-drop-promote-band-active",
      "ce-drop-promote-band-invalid",
    );
    if (!dragState || dragState.treeId !== treeId) return;
    if (validatePromoteToTop(state, dragState) !== "valid") return;
    const ids = dragState.ids.slice();
    dragState = null;
    teardownDragChip();
    notifyDragListeners();
    await performMultiMove(state, deps, ids, null);
  });
  return band;
}

/// Promote-to-top is valid for any non-leaf dragged item whose current
/// parent isn't already null. Leaves can't be promoted to the root
/// (they live under clusters or outlier buckets); we filter them out
/// to keep the affordance honest.
function validatePromoteToTop(
  state: TreeRowSurfaceState,
  payload: DragPayload,
): "valid" | "invalid" {
  for (const id of payload.ids) {
    const dragged = state.nodes.find((n) => n.id === id);
    if (!dragged) return "invalid";
    if (dragged.kind === "leaf") return "invalid";
    if (dragged.parent === null) return "invalid";
  }
  return "valid";
}
