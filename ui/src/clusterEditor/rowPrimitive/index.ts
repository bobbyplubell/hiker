// status: cluster-editor-row-primitive
//
// Shared tree-row primitive consumed by the sidebar cluster editor
// (`ui/src/clusterEditor/index.ts`) and the expanded center pane
// (`ui/src/clusterEditorPane/index.ts`). Owns the row-level shape and
// interactions: chevron + icon + name (click-to-edit on clusters,
// click-to-open on leaves) + summary preview (click-to-edit) + members
// count + ↻ staleness badge + policy chip + right-click context menu
// (Move to… / Split / Subcluster… / Merge children up / Summarize /
// Drop cluster / Send to outliers / Promote out of outliers…) +
// selection (Shift / Cmd / Ctrl-click) + outlier virtual node +
// multi-select toolbar (Merge siblings / Drop / Summarize / Stage move
// to / Stage tag with / Clear) + drag-and-drop reparent with promote-
// to-top band.
//
// Per the cluster-editor.md "Reusable row primitive" section, the
// component lives in a shared module so future hierarchical surfaces
// can plug into the same row shape. The home is here in the cluster
// editor module rather than `ui/src/treeRows/` for cross-reference
// tightness — the consumers and the spec contract both live in this
// neighborhood. Both consumers currently emit `.ce-*` class names; the
// pane's CSS extends the sidebar's `.ce-*` selectors to its own
// wrapper rather than carrying a parallel `.cep-row*` prefix for tree
// rows.
//
// The module is split into focused files under `./rowPrimitive/`; this
// index file re-exports the public surface that the two consumer
// modules import.

export { Api } from "./api";
export type {
  ClusterNodeRow,
  ClusterTreeRow,
  SummarizeSweepOutcome,
} from "./api";

export type { TreeRowDeps, TreeRowSurfaceState } from "./state";

export { summarizeOutcomeToast } from "./toasts";

export { renderTreeNode, renderSiblingsWithOutliers } from "./render";
export { renderMultiSelectToolbar } from "./toolbar";

export {
  openNodeMenu,
  openMoveTargetPicker,
  openPolicyMenu,
} from "./menus";

export { beginInlineEdit, beginInlineEditMultiline } from "./inlineEdit";

export {
  handleSelectionClick,
  toggleSelect,
} from "./selection";

export {
  isDragInFlight,
  onDragStateChange,
  renderPromoteBand,
} from "./dnd";

export {
  collectDescendants,
  countMembers,
  renderPolicyLabel,
} from "./helpers";
