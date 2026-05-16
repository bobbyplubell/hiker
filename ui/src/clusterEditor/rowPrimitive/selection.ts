// status: cluster-editor-multi-select-shift-range
//
// Centralised click-handler logic for row selection. Splits the legacy
// "any modifier toggles" path into three distinct gestures:
//  - plain click: clear any multi-selection and re-anchor (default
//    activation fires),
//  - Cmd/Ctrl-click: toggle the clicked id in selection + re-anchor,
//  - Shift-click: extend selection from `state.anchor` through the
//    clicked id in current display order.

import type { TreeRowDeps, TreeRowSurfaceState } from "./state";

export function toggleSelect(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  id: string,
): void {
  if (state.selection.has(id)) state.selection.delete(id);
  else state.selection.add(id);
  state.anchor = id;
  deps.repaint();
}

// status: cluster-editor-multi-select-shift-range
//
// Centralised click-handler logic for selection. Returns `true` if the
// click was consumed as a selection gesture (anchor set / selection
// mutated / range extended); the caller should skip its default action
// (open-note / inline-edit / expand-toggle). Returns `false` to let the
// caller proceed with the default action.
//
// Semantics:
//
//  - Shift-click (no Cmd/Ctrl): extend selection from `state.anchor`
//    through the clicked id in current display order, replacing the
//    selection with that range. Anchor stays where it was. If there's
//    no anchor yet, the clicked id is treated as a single-row range
//    (and becomes the anchor for next time).
//  - Cmd/Ctrl-click (any combination with Shift treated as Cmd/Ctrl
//    when Cmd/Ctrl is present): toggle the clicked id in selection,
//    set anchor = clicked id. Toggling-off does not clear anchor — a
//    subsequent shift-click still extends from the just-toggled row.
//  - Plain click: clear an existing multi-selection (if any), set
//    anchor = clicked id. Does NOT add the clicked id to selection —
//    callers' default activations (open note, inline-edit name,
//    expand cluster) own the row's primary affordance. Returns
//    `false` so the default action fires.
export function handleSelectionClick(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  id: string,
  e: MouseEvent | KeyboardEvent,
): boolean {
  const hasCmd = e.metaKey || e.ctrlKey;
  const hasShift = e.shiftKey;
  if (hasCmd) {
    if (state.selection.has(id)) state.selection.delete(id);
    else state.selection.add(id);
    state.anchor = id;
    deps.repaint();
    return true;
  }
  if (hasShift) {
    extendSelectionToAnchor(state, id);
    deps.repaint();
    return true;
  }
  // Plain click — clear any multi-selection and re-anchor. The default
  // action (open / edit / expand) still fires.
  let changed = false;
  if (state.selection.size > 0) {
    state.selection.clear();
    changed = true;
  }
  state.anchor = id;
  if (changed) deps.repaint();
  return false;
}

function extendSelectionToAnchor(
  state: TreeRowSurfaceState,
  clickedId: string,
): void {
  const order = state.displayOrder ?? [];
  const anchor = state.anchor;
  // No anchor yet, or anchor is no longer rendered: treat the click
  // as a single-row range and re-anchor.
  if (!anchor || !order.includes(anchor)) {
    state.selection.clear();
    state.selection.add(clickedId);
    state.anchor = clickedId;
    return;
  }
  const iA = order.indexOf(anchor);
  const iC = order.indexOf(clickedId);
  if (iC === -1) {
    // Clicked row not in current display order (shouldn't happen in
    // practice — every rendered row is registered). Fall back to
    // single-row range.
    state.selection.clear();
    state.selection.add(clickedId);
    state.anchor = clickedId;
    return;
  }
  const lo = Math.min(iA, iC);
  const hi = Math.max(iA, iC);
  state.selection.clear();
  for (let i = lo; i <= hi; i += 1) {
    state.selection.add(order[i]);
  }
  // Anchor stays put so subsequent shift-clicks pivot off the same
  // origin — file-manager convention.
}
