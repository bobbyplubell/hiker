// Right-click context menus + policy-chip popover for the shared row
// primitive. Holds the Move to… / Split / Subcluster… / Merge children
// up / Summarize / Drop cluster / Send to outliers / Promote out of
// outliers… menu plus the Tag / Move-to-folder / Freeze / Clear policy
// popover.

import { openContextMenu, type CtxMenuItem } from "../../widgets/contextMenu";
import { showToast } from "../../widgets/toast";
import { Api, type ClusterNodeRow } from "./api";
import { summarizeOutcomeToast } from "./toasts";
import { collectDescendants } from "./helpers";
import type { TreeRowDeps, TreeRowSurfaceState } from "./state";

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
    // status: cluster-editor-summarize-verb
    items.push({
      label: "Summarize",
      run: async () => {
        try {
          const outcome = await Api.summarizeSubset(state.tree.id, [node.id]);
          showToast(summarizeOutcomeToast(outcome, 1));
        } catch (err) {
          showToast(`Summarize failed: ${String(err)}`);
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
