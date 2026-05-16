// Row renderer — the heart of the shared tree-row primitive. Renders
// one node (and recursively its children when expanded) into a detached
// DOM fragment, wiring up chevron / name / summary / member-count /
// staleness badge / policy chip / right-click menu / row click + DnD /
// selection handling. The "siblings + outliers" helper sequences regular
// children before the outlier bucket (or a ghost bucket row if none).

import { Logger } from "../../logger";
import { showToast } from "../../widgets/toast";
import { Api, type ClusterNodeRow } from "./api";
import { attachRowDnD } from "./dnd";
import { countMembers, renderPolicyLabel } from "./helpers";
import { beginInlineEdit, beginInlineEditMultiline } from "./inlineEdit";
import { openNodeMenu, openPolicyMenu } from "./menus";
import { handleSelectionClick } from "./selection";
import type { TreeRowDeps, TreeRowSurfaceState } from "./state";

/// Render one node (and recursively its children when expanded) into a
/// detached fragment. Returns a single element — either the row itself
/// (leaf / collapsed cluster) or a wrapper containing the row + summary
/// + children (expanded cluster).
export function renderTreeNode(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  node: ClusterNodeRow,
  depth: number,
): HTMLElement {
  const children = state.nodes.filter((n) => n.parent === node.id);
  const isCluster = node.kind === "cluster" || node.kind === "outlier-bucket";
  const expanded = state.expanded.has(node.id);
  const memberCount = countMembers(state.nodes, node.id);

  // status: cluster-editor-multi-select-shift-range
  // Record this row's position in the surface's display-order array
  // so shift-click range computation can slice it. The array is
  // initialized to [] by `renderSiblingsWithOutliers` at depth 0 and
  // appended-to as nodes render top-to-bottom.
  if (state.displayOrder) state.displayOrder.push(node.id);

  const row = document.createElement("div");
  row.className = "ce-row";
  row.dataset.nodeId = node.id;
  row.dataset.kind = node.kind;
  row.style.paddingLeft = `${depth * 14}px`;
  if (state.selection.has(node.id)) {
    row.classList.add("ce-row-selected");
  }
  if (node.kind === "outlier-bucket") {
    row.classList.add("ce-row-outliers");
  }
  // status: cluster-editor-dnd-reparent
  attachRowDnD(state, deps, node, row);

  // Chevron — collapsible iff there are children.
  const chev = document.createElement("span");
  chev.className = "ce-chev";
  // Leaves / childless rows get an empty placeholder (width preserved by
  // CSS) instead of a `·`, so the row doesn't read as "two dots" next to
  // the leaf's own `●` glyph.
  chev.textContent = children.length === 0 ? "" : expanded ? "▾" : "▸";
  if (children.length > 0) {
    chev.addEventListener("click", (e) => {
      e.stopPropagation();
      if (expanded) state.expanded.delete(node.id);
      else state.expanded.add(node.id);
      deps.repaint();
    });
  }
  row.appendChild(chev);

  // Name — click-to-edit on clusters; clicking a leaf opens its note.
  const name = document.createElement("span");
  name.className = "ce-row-name";
  if (node.kind === "leaf") {
    name.textContent = node.note_title ?? node.note_ref ?? "(unknown)";
    name.classList.add("ce-row-name-leaf");
    name.addEventListener("click", (e) => {
      if (handleSelectionClick(state, deps, node.id, e)) return;
      if (node.note_path) {
        void deps.openNote(node.note_path, { preview: false });
      }
    });
  } else {
    name.textContent = node.name;
    if (node.user_edited_name) name.classList.add("ce-row-name-edited");
    name.addEventListener("click", (e) => {
      e.stopPropagation();
      if (handleSelectionClick(state, deps, node.id, e)) return;
      beginInlineEdit(name, node.name, deps, async (v) => {
        if (v === node.name) return;
        try {
          await Api.rename(state.tree.id, node.id, v);
          await deps.refresh();
        } catch (err) {
          Logger.error("ui::clusterEditor", "rename failed", { err });
          showToast(`Rename failed: ${String(err)}`);
        }
      });
    });
  }
  row.appendChild(name);

  // Members count chip.
  if (isCluster) {
    const cnt = document.createElement("span");
    cnt.className = "ce-row-count";
    cnt.textContent = `(${memberCount})`;
    row.appendChild(cnt);
  }

  // ↻ N staleness badge — appears when membership has churned since
  // the last summarization. Click queues a Regenerate task for the
  // tree (per `cluster-editor-summary-staleness-badge`).
  if (isCluster && node.summary_membership_churn > 0) {
    const badge = document.createElement("button");
    badge.type = "button";
    badge.className = "ce-row-staleness";
    badge.textContent = `↻ ${node.summary_membership_churn}`;
    badge.title = `Summary may be stale — ${node.summary_membership_churn} membership change(s) since last regenerate. Click to regenerate.`;
    badge.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        const ids = await Api.regenerateNames(state.tree.id);
        showToast(`Queued ${ids.length} regeneration tasks`);
      } catch (err) {
        showToast(`Regenerate failed: ${String(err)}`);
      }
    });
    row.appendChild(badge);
  }

  // Policy chip — clicking opens policy editor.
  if (isCluster) {
    const policyChip = document.createElement("button");
    policyChip.type = "button";
    policyChip.className = "ce-row-policy";
    policyChip.textContent = renderPolicyLabel(node.policy_json);
    policyChip.addEventListener("click", (e) => {
      e.stopPropagation();
      openPolicyMenu(state, deps, node, policyChip);
    });
    row.appendChild(policyChip);
  }

  // Trailing spacer so the row fills its container; node actions are
  // accessed via right-click (contextmenu) on the row.
  const spacer = document.createElement("span");
  spacer.style.flex = "1";
  row.appendChild(spacer);
  row.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openNodeMenu(state, deps, node, e.clientX, e.clientY);
  });

  // Row-level click: modifier-click toggles multi-select; plain click
  // on a cluster row toggles expand. Children that intercept clicks
  // (chevron, name span, policy chip, staleness badge) already
  // stopPropagation, so those interactions still work.
  row.addEventListener("click", (e) => {
    if (handleSelectionClick(state, deps, node.id, e)) return;
    if (node.kind === "leaf") return;
    if (children.length === 0) return;
    if (expanded) state.expanded.delete(node.id);
    else state.expanded.add(node.id);
    deps.repaint();
  });

  // Summary — truncated one-liner under the row when the cluster is
  // collapsed; full wrapping textbox when expanded so the user can
  // read + click-to-edit the whole thing without leaving the tree view.
  if (isCluster && node.summary) {
    const sum = document.createElement("div");
    sum.className = expanded
      ? "ce-row-summary ce-row-summary-expanded"
      : "ce-row-summary";
    // Align the box's left edge with the row's name-text column so the
    // per-level indent reads cleanly. Row's content starts at depth*14;
    // chevron (12) + gap (4) = 16px to name text (the leaf/cluster icon
    // glyph was dropped — chevron alone signals the row's kind).
    sum.dataset.depth = String(depth);
    sum.style.marginLeft = `${depth * 14 + 16}px`;
    sum.textContent = node.summary;
    if (node.user_edited_summary) sum.classList.add("ce-row-summary-edited");
    sum.addEventListener("click", (e) => {
      e.stopPropagation();
      beginInlineEditMultiline(sum, node.summary, deps, async (v) => {
        if (v === node.summary) return;
        try {
          await Api.setSummary(state.tree.id, node.id, v);
          await deps.refresh();
        } catch (err) {
          Logger.error("ui::clusterEditor", "set_summary failed", { err });
          showToast(`Edit summary failed: ${String(err)}`);
        }
      });
    });
    const wrap = document.createElement("div");
    wrap.appendChild(row);
    wrap.appendChild(sum);
    if (expanded && node.kind === "cluster") {
      const els = renderSiblingsWithOutliers(state, deps, children, depth + 1);
      for (const el of els) wrap.appendChild(el);
    } else if (expanded) {
      for (const c of children) wrap.appendChild(renderTreeNode(state, deps, c, depth + 1));
    }
    return wrap;
  }

  if (expanded && (children.length > 0 || node.kind === "cluster")) {
    const wrap = document.createElement("div");
    wrap.appendChild(row);
    if (node.kind === "cluster") {
      const els = renderSiblingsWithOutliers(state, deps, children, depth + 1);
      for (const el of els) wrap.appendChild(el);
    } else {
      for (const c of children) wrap.appendChild(renderTreeNode(state, deps, c, depth + 1));
    }
    return wrap;
  }
  return row;
}

// status: cluster-editor-outlier-virtual-node
export function renderSiblingsWithOutliers(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  siblings: ClusterNodeRow[],
  depth: number,
): HTMLElement[] {
  // status: cluster-editor-multi-select-shift-range
  // Reset the surface's display-order array at the top-level entry so
  // each paint produces a fresh top-to-bottom walk. Recursive calls
  // from inside `renderTreeNode` (cluster bodies expanding their
  // children) inherit the same array and append in order.
  if (depth === 0) state.displayOrder = [];
  const out: HTMLElement[] = [];
  const regular = siblings.filter((n) => n.kind !== "outlier-bucket");
  const buckets = siblings.filter((n) => n.kind === "outlier-bucket");
  for (const n of regular) out.push(renderTreeNode(state, deps, n, depth));
  if (buckets.length > 0) {
    for (const b of buckets) out.push(renderTreeNode(state, deps, b, depth));
  } else {
    const hasCluster = regular.some((n) => n.kind === "cluster");
    if (hasCluster || depth === 0) {
      const ghost = document.createElement("div");
      ghost.className = "ce-row ce-row-outliers-ghost";
      ghost.style.paddingLeft = `${depth * 14}px`;
      const ic = document.createElement("span");
      ic.className = "ce-row-icon";
      ic.textContent = "◇";
      ghost.appendChild(ic);
      const lbl = document.createElement("span");
      lbl.className = "ce-row-name";
      lbl.textContent = "Outliers (0)";
      ghost.appendChild(lbl);
      out.push(ghost);
    }
  }
  return out;
}
