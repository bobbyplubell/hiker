// status: cluster-editor-row-primitive
//
// Shared tree-row primitive consumed by the sidebar cluster editor
// (`ui/src/clusterEditor/index.ts`) and the expanded center pane
// (`ui/src/clusterEditorPane/index.ts`). Owns the row-level shape and
// interactions: chevron + icon + name (click-to-edit on clusters,
// click-to-open on leaves) + summary preview (click-to-edit) + members
// count + ↻ staleness badge + policy chip + right-click context menu
// (Move to… / Split / Subcluster… / Merge children up / Drop cluster /
// Send to outliers / Promote out of outliers…) + selection (Shift /
// Cmd / Ctrl-click) + outlier virtual node + multi-select toolbar
// (Merge siblings / Drop / Stage move to / Stage tag with / Clear).
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

import { invoke } from "@tauri-apps/api/core";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { history, defaultKeymap, historyKeymap } from "@codemirror/commands";
import { Logger } from "../logger";
import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";
import { showToast } from "../widgets/toast";

// ── Wire types ──────────────────────────────────────────────────────

export interface ClusterTreeRow {
  id: string;
  name: string;
  source: string;
  state: string;
  scope_json: string;
  method_json: string;
  created_at_ms: number;
  vault_snapshot: string | null;
}

export interface ClusterNodeRow {
  id: string;
  parent: string | null;
  kind: "cluster" | "leaf" | "outlier-bucket";
  note_ref: string | null;
  note_path: string | null;
  note_title: string | null;
  name: string;
  summary: string;
  user_edited_name: boolean;
  user_edited_summary: boolean;
  policy_json: string | null;
  confidence: number;
  summary_membership_churn: number;
}

// ── Tauri shim (shared by both surfaces) ────────────────────────────

export const Api = {
  list(): Promise<ClusterTreeRow[]> {
    return invoke("cluster_trees_list");
  },
  get(treeId: string): Promise<ClusterNodeRow[]> {
    return invoke("cluster_tree_get", { treeId });
  },
  rename(treeId: string, nodeId: string, name: string): Promise<void> {
    return invoke("cluster_node_rename", { treeId, nodeId, name });
  },
  setSummary(treeId: string, nodeId: string, summary: string): Promise<void> {
    return invoke("cluster_node_set_summary", { treeId, nodeId, summary });
  },
  move(treeId: string, nodeId: string, newParent: string | null): Promise<void> {
    return invoke("cluster_node_move", { treeId, nodeId, newParent });
  },
  setPolicy(treeId: string, nodeId: string, policyJson: string | null): Promise<void> {
    return invoke("cluster_node_set_policy", { treeId, nodeId, policyJson });
  },
  mergeSiblings(treeId: string, nodeIds: string[]): Promise<string> {
    return invoke("cluster_op_merge_siblings", { treeId, nodeIds });
  },
  mergeChildrenUp(treeId: string, parentId: string): Promise<void> {
    return invoke("cluster_op_merge_children_up", { treeId, parentId });
  },
  dropCluster(treeId: string, nodeId: string, outlierBucketId: string): Promise<void> {
    return invoke("cluster_op_drop_cluster", { treeId, nodeId, outlierBucketId });
  },
  promoteOutlier(treeId: string, leafId: string, newParent: string | null): Promise<void> {
    return invoke("cluster_op_promote_outlier", { treeId, leafId, newParent });
  },
  split(treeId: string, nodeId: string): Promise<string[]> {
    return invoke("cluster_op_split", { treeId, nodeId });
  },
  regenerateNames(treeId: string): Promise<string[]> {
    return invoke("cluster_regenerate_names", { treeId });
  },
  stageMoves(treeId: string, nodeIds: string[], targetFolder: string): Promise<string[]> {
    return invoke("cluster_stage_moves", { treeId, nodeIds, targetFolder });
  },
  stageTags(treeId: string, nodeIds: string[], tagSlug: string): Promise<string[]> {
    return invoke("cluster_stage_tags", { treeId, nodeIds, tagSlug });
  },
};

// ── Per-surface UI state ────────────────────────────────────────────

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

// ── Row renderer ─────────────────────────────────────────────────────

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

  // Chevron — collapsible iff there are children.
  const chev = document.createElement("span");
  chev.className = "ce-chev";
  chev.textContent = children.length === 0 ? "·" : expanded ? "▾" : "▸";
  if (children.length > 0) {
    chev.addEventListener("click", (e) => {
      e.stopPropagation();
      if (expanded) state.expanded.delete(node.id);
      else state.expanded.add(node.id);
      deps.repaint();
    });
  }
  row.appendChild(chev);

  // Icon — distinct glyphs for cluster / leaf / outlier-bucket.
  const icon = document.createElement("span");
  icon.className = "ce-row-icon";
  icon.textContent =
    node.kind === "leaf" ? "●" : node.kind === "outlier-bucket" ? "◇" : "◉";
  row.appendChild(icon);

  // Name — click-to-edit on clusters; clicking a leaf opens its note.
  const name = document.createElement("span");
  name.className = "ce-row-name";
  if (node.kind === "leaf") {
    name.textContent = node.note_title ?? node.note_ref ?? "(unknown)";
    name.classList.add("ce-row-name-leaf");
    name.addEventListener("click", (e) => {
      if (e.shiftKey || e.metaKey || e.ctrlKey) {
        toggleSelect(state, deps, node.id);
        return;
      }
      if (node.note_path) {
        void deps.openNote(node.note_path, { preview: false });
      }
    });
  } else {
    name.textContent = node.name;
    if (node.user_edited_name) name.classList.add("ce-row-name-edited");
    name.addEventListener("click", (e) => {
      e.stopPropagation();
      if (e.shiftKey || e.metaKey || e.ctrlKey) {
        toggleSelect(state, deps, node.id);
        return;
      }
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
    if (e.shiftKey || e.metaKey || e.ctrlKey) {
      toggleSelect(state, deps, node.id);
      return;
    }
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
    sum.style.paddingLeft = `${depth * 14 + 24}px`;
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

// ── Multi-select toolbar ────────────────────────────────────────────

export function renderMultiSelectToolbar(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
): HTMLElement {
  const bar = document.createElement("span");
  bar.className = "ce-msel-toolbar";
  const lbl = document.createElement("span");
  lbl.textContent = `${state.selection.size} selected`;
  bar.appendChild(lbl);

  const make = (
    label: string,
    onClick: () => Promise<void> | void,
    title?: string,
  ) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "ce-msel-btn";
    b.textContent = label;
    if (title) b.title = title;
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      void onClick();
    });
    bar.appendChild(b);
    return b;
  };
  make("Merge siblings", async () => {
    const ids = Array.from(state.selection);
    try {
      await Api.mergeSiblings(state.tree.id, ids);
      state.selection.clear();
      await deps.refresh();
    } catch (err) {
      showToast(`Merge failed: ${String(err)}`);
    }
  });
  make("Drop", async () => {
    const bucket = state.nodes.find((n) => n.kind === "outlier-bucket");
    if (!bucket) {
      showToast("This tree has no outlier bucket");
      return;
    }
    const ids = Array.from(state.selection);
    for (const id of ids) {
      try {
        await Api.dropCluster(state.tree.id, id, bucket.id);
      } catch (err) {
        showToast(`Drop failed for ${id}: ${String(err)}`);
      }
    }
    state.selection.clear();
    await deps.refresh();
  });
  // status: cluster-editor-multi-select-stage-move
  make("Stage move to…", async () => {
    const folder = window.prompt(
      "Target folder (vault-relative, e.g. research/embeddings):",
      "",
    );
    if (folder === null) return;
    const leafIds = Array.from(state.selection).filter((id) => {
      const n = state.nodes.find((x) => x.id === id);
      return n?.kind === "leaf";
    });
    if (leafIds.length === 0) {
      showToast("Stage move: no leaves selected");
      return;
    }
    try {
      const ids = await Api.stageMoves(state.tree.id, leafIds, folder.trim());
      showToast(`Staged ${ids.length} move${ids.length === 1 ? "" : "s"}`);
      state.selection.clear();
      await deps.refresh();
    } catch (err) {
      showToast(`Stage move failed: ${String(err)}`);
    }
  });
  // status: cluster-editor-multi-select-stage-tag
  make("Stage tag with…", async () => {
    const slug = window.prompt("Tag slug:", "");
    if (!slug) return;
    const leafIds = Array.from(state.selection).filter((id) => {
      const n = state.nodes.find((x) => x.id === id);
      return n?.kind === "leaf";
    });
    if (leafIds.length === 0) {
      showToast("Stage tag: no leaves selected");
      return;
    }
    try {
      const ids = await Api.stageTags(state.tree.id, leafIds, slug.trim());
      showToast(`Staged ${ids.length} tag${ids.length === 1 ? "" : "s"}`);
      state.selection.clear();
      await deps.refresh();
    } catch (err) {
      showToast(`Stage tag failed: ${String(err)}`);
    }
  });
  make("Clear", () => {
    state.selection.clear();
    deps.repaint();
  });
  return bar;
}

// ── Context menus ───────────────────────────────────────────────────

export function openNodeMenu(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  node: ClusterNodeRow,
  x: number,
  y: number,
): void {
  const isCluster = node.kind === "cluster";
  const items: CtxMenuItem[] = [];
  items.push({
    label: "Move to…",
    run: () => openMoveTargetPicker(state, deps, node, x, y),
  });
  if (isCluster) {
    items.push({
      label: "Split",
      run: async () => {
        try {
          await Api.split(state.tree.id, node.id);
          await deps.refresh();
        } catch (err) {
          showToast(`Split failed: ${String(err)}`);
        }
      },
    });
    items.push({
      // status: cluster-review-tab-from-recluster-action
      label: "Subcluster…",
      run: () => deps.openReclusterReview(state.tree.id, node.id, node.name),
    });
    items.push({
      label: "Merge children up",
      run: async () => {
        try {
          await Api.mergeChildrenUp(state.tree.id, node.id);
          await deps.refresh();
        } catch (err) {
          showToast(`Merge-up failed: ${String(err)}`);
        }
      },
    });
    items.push({
      label: "Drop cluster",
      run: async () => {
        const bucket = state.nodes.find((n) => n.kind === "outlier-bucket");
        if (!bucket) {
          showToast("This tree has no outlier bucket");
          return;
        }
        try {
          await Api.dropCluster(state.tree.id, node.id, bucket.id);
          await deps.refresh();
        } catch (err) {
          showToast(`Drop failed: ${String(err)}`);
        }
      },
    });
  }
  if (node.kind === "leaf") {
    const isOutlier = (() => {
      if (!node.parent) return false;
      const p = state.nodes.find((n) => n.id === node.parent);
      return p?.kind === "outlier-bucket";
    })();
    items.push({
      label: isOutlier ? "Promote out of outliers…" : "Send to outliers",
      run: async () => {
        if (isOutlier) {
          return openMoveTargetPicker(state, deps, node, x, y);
        }
        const bucket = state.nodes.find((n) => n.kind === "outlier-bucket");
        if (!bucket) {
          showToast("This tree has no outlier bucket");
          return;
        }
        try {
          await Api.promoteOutlier(state.tree.id, node.id, bucket.id);
          await deps.refresh();
        } catch (err) {
          showToast(`Move failed: ${String(err)}`);
        }
      },
    });
  }
  openContextMenu(x, y, items);
}

export function openMoveTargetPicker(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  node: ClusterNodeRow,
  x: number,
  y: number,
): void {
  const descendants = collectDescendants(state.nodes, node.id);
  const candidates = state.nodes.filter(
    (n) =>
      (n.kind === "cluster" || n.kind === "outlier-bucket") &&
      n.id !== node.id &&
      !descendants.has(n.id),
  );
  const items: CtxMenuItem[] = candidates.map((c) => ({
    label: `${c.kind === "outlier-bucket" ? "◇" : "◉"} ${c.name}`,
    run: async () => {
      try {
        if (node.kind === "leaf") {
          await Api.promoteOutlier(state.tree.id, node.id, c.id);
        } else {
          await Api.move(state.tree.id, node.id, c.id);
        }
        await deps.refresh();
      } catch (err) {
        showToast(`Move failed: ${String(err)}`);
      }
    },
  }));
  if (items.length === 0) {
    showToast("No valid targets for this move");
    return;
  }
  openContextMenu(x, y, items);
}

export function openPolicyMenu(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  node: ClusterNodeRow,
  anchor: HTMLElement,
): void {
  const rect = anchor.getBoundingClientRect();
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
        void Api.setPolicy(state.tree.id, node.id, policy)
          .then(() => deps.refresh())
          .catch((err) => showToast(`Policy failed: ${String(err)}`));
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
        void Api.setPolicy(state.tree.id, node.id, policy)
          .then(() => deps.refresh())
          .catch((err) => showToast(`Policy failed: ${String(err)}`));
      },
    },
    {
      label: "Freeze",
      run: () => {
        const policy = JSON.stringify({ kind: "freeze" });
        void Api.setPolicy(state.tree.id, node.id, policy)
          .then(() => deps.refresh())
          .catch((err) => showToast(`Policy failed: ${String(err)}`));
      },
    },
    {
      label: "Clear policy",
      run: () => {
        void Api.setPolicy(state.tree.id, node.id, null)
          .then(() => deps.refresh())
          .catch((err) => showToast(`Policy failed: ${String(err)}`));
      },
    },
  ];
  openContextMenu(rect.right, rect.bottom, items, anchor);
}

// ── Helpers ─────────────────────────────────────────────────────────

export function toggleSelect(
  state: TreeRowSurfaceState,
  deps: TreeRowDeps,
  id: string,
): void {
  if (state.selection.has(id)) state.selection.delete(id);
  else state.selection.add(id);
  deps.repaint();
}

export function beginInlineEdit(
  el: HTMLElement,
  initial: string,
  deps: TreeRowDeps,
  commit: (v: string) => Promise<void> | void,
): void {
  const input = document.createElement("input");
  input.type = "text";
  input.value = initial;
  input.className = "ce-inline-edit";
  const parent = el.parentElement;
  if (!parent) return;
  parent.replaceChild(input, el);
  input.focus();
  input.select();
  const finish = (save: boolean) => {
    if (save) void commit(input.value);
    else deps.repaint();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(true));
}

/// Multiline variant of `beginInlineEdit` — swaps the target element for
/// a small CM6 editor with line-wrapping + history. Used for the cluster
/// summary edit, where the single-line `<input>` swap collapses a
/// multi-paragraph summary down to a single row at click time. Save on
/// Cmd/Ctrl-Enter or blur; cancel on Escape.
export function beginInlineEditMultiline(
  el: HTMLElement,
  initial: string,
  deps: TreeRowDeps,
  commit: (v: string) => Promise<void> | void,
): void {
  const host = document.createElement("div");
  host.className = "ce-inline-edit-multiline";
  const parent = el.parentElement;
  if (!parent) return;
  parent.replaceChild(host, el);

  // Re-paint cancel path needs to fire only once. We also guard against
  // the post-blur dispatch from CM6 destroying the view while the
  // commit's async dispatch is still running.
  let done = false;
  let view: EditorView | null = null;
  const finish = (save: boolean) => {
    if (done) return;
    done = true;
    const text = view ? view.state.doc.toString() : initial;
    if (view) {
      view.destroy();
      view = null;
    }
    if (save) void commit(text);
    else deps.repaint();
  };

  view = new EditorView({
    parent: host,
    state: EditorState.create({
      doc: initial,
      extensions: [
        history(),
        EditorView.lineWrapping,
        keymap.of([
          {
            key: "Mod-Enter",
            run: () => {
              finish(true);
              return true;
            },
          },
          {
            key: "Escape",
            run: () => {
              finish(false);
              return true;
            },
          },
          ...defaultKeymap,
          ...historyKeymap,
        ]),
        EditorView.domEventHandlers({
          blur: () => {
            // Defer so a click inside the editor that loses focus
            // momentarily (e.g. the user dragging to select text and
            // releasing outside) doesn't terminate the edit. CM6 fires
            // blur synchronously on focus loss; a microtask is enough
            // to let any incoming click re-focus.
            setTimeout(() => finish(true), 0);
          },
        }),
      ],
    }),
  });
  view.focus();
  // Place caret at end so the user can keep typing.
  view.dispatch({ selection: { anchor: view.state.doc.length } });
}

export function countMembers(nodes: ClusterNodeRow[], rootId: string): number {
  let count = 0;
  const stack = [rootId];
  const childMap = new Map<string, ClusterNodeRow[]>();
  for (const n of nodes) {
    if (!n.parent) continue;
    const arr = childMap.get(n.parent);
    if (arr) arr.push(n);
    else childMap.set(n.parent, [n]);
  }
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

export function collectDescendants(
  nodes: ClusterNodeRow[],
  rootId: string,
): Set<string> {
  const out = new Set<string>();
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop()!;
    for (const n of nodes) {
      if (n.parent === id && !out.has(n.id)) {
        out.add(n.id);
        stack.push(n.id);
      }
    }
  }
  return out;
}

export function renderPolicyLabel(policyJson: string | null): string {
  if (!policyJson) return "policy…";
  try {
    const p = JSON.parse(policyJson);
    if (p.kind === "tag") return `tag: ${p.slug}${p.require_review ? " ⏸" : ""}`;
    if (p.kind === "move") return `move: ${p.folder}${p.require_review ? " ⏸" : ""}`;
    if (p.kind === "freeze") return "freeze";
  } catch {}
  return "policy…";
}
