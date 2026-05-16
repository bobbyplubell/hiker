// status: cluster-editor-row-primitive
//
// Per-surface UI state and callback contracts for the shared tree-row
// primitive. The surfaces (sidebar + expanded pane) own the state Sets
// and pass them through on every paint; the primitive treats them as
// mutable scratch space but doesn't allocate new ones.

import type { ClusterNodeRow, ClusterTreeRow } from "./api";

export interface TreeRowSurfaceState {
  /// The owning tree's metadata. Only `id` and `name` are consumed by
  /// the primitive; surfaces pass through their full row so the
  /// surface-specific code (toolbars, headers) can read whatever
  /// fields they need.
  tree: ClusterTreeRow;
  /// All `cluster_nodes` rows for this tree. The primitive treats
  /// these as read-only — mutations route through the surface's
  /// `refresh` callback, not through direct array edits.
  nodes: ClusterNodeRow[];
  /// Per-surface expand/collapse state. The caller owns the Set;
  /// re-renders survive because the Set instance is reused.
  expanded: Set<string>;
  /// Per-surface multi-select state. Same lifetime as `expanded`.
  selection: Set<string>;
  /// status: cluster-editor-multi-select-shift-range
  /// Anchor row id for shift-click range extension. Set on every
  /// non-shift click (plain or Cmd/Ctrl); a shift-click computes the
  /// selection as the range from `anchor` through the clicked id in
  /// current display order. `null` until the user has clicked at least
  /// one row.
  anchor?: string | null;
  /// status: cluster-editor-multi-select-shift-range
  /// Flat list of currently-rendered row ids in display order (top-to-
  /// bottom, respecting `expanded`). Re-populated by the renderer on
  /// every paint; consumed by shift-click range computation.
  /// Collapsed clusters' hidden descendants are not in this array.
  displayOrder?: string[];
}

/// Callbacks the primitive needs but doesn't own. Surfaces wire these
/// to their own host (sidebar refresh, pane re-fetch, openNote, etc.).
export interface TreeRowDeps {
  /// Re-paint the surface after a mutation. Most callbacks trigger a
  /// data re-fetch first; the surface's `refresh` does both.
  refresh: () => Promise<void> | void;
  /// Local-only re-paint (no data re-fetch). Used by chevron toggle,
  /// selection-toggle, and the editor's commit/cancel paths so we
  /// don't round-trip to the backend on a pure-UI change.
  repaint: () => void;
  /// Open a note in the editor pane.
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// Open the clustering review tab for a subtree recluster.
  openReclusterReview: (treeId: string, nodeId: string, nodeName: string) => void;
}
