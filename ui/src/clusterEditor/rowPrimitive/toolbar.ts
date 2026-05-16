// Multi-select toolbar rendered above the tree body when 1+ rows are
// selected. Carries the Merge siblings / Drop / Summarize / Stage move
// to / Stage tag with / Clear actions; the Summarize button only
// appears when the selection contains at least one cluster row.

import { showToast } from "../../widgets/toast";
import { describeErr } from "../../ipc/runCommand";
import { Api } from "./api";
import { summarizeOutcomeToast } from "./toasts";
import type { TreeRowDeps, TreeRowSurfaceState } from "./state";

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
      showToast(`Merge failed: ${describeErr(err)}`);
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
        showToast(`Drop failed for ${id}: ${describeErr(err)}`);
      }
    }
    state.selection.clear();
    await deps.refresh();
  });
  // status: cluster-editor-summarize-verb
  //
  // Subset Summarize over the cluster rows in the current selection.
  // Leaves are filtered out (they don't carry a name/summary). The
  // button is omitted entirely when no cluster is selected — matches
  // the spec's "visible when at least one selected node is a cluster".
  const selectedClusterIds = Array.from(state.selection).filter((id) => {
    const n = state.nodes.find((x) => x.id === id);
    return n?.kind === "cluster";
  });
  if (selectedClusterIds.length > 0) {
    make(
      "Summarize",
      async () => {
        try {
          const outcome = await Api.summarizeSubset(
            state.tree.id,
            selectedClusterIds,
          );
          showToast(
            summarizeOutcomeToast(outcome, selectedClusterIds.length),
          );
        } catch (err) {
          showToast(`Summarize failed: ${describeErr(err)}`);
        }
      },
      `Summarize the ${selectedClusterIds.length} selected cluster${selectedClusterIds.length === 1 ? "" : "s"}`,
    );
  }
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
      showToast(`Stage move failed: ${describeErr(err)}`);
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
      showToast(`Stage tag failed: ${describeErr(err)}`);
    }
  });
  make("Clear", () => {
    state.selection.clear();
    deps.repaint();
  });
  return bar;
}
