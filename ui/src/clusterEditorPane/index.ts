// status: cluster-editor-pane-mode
// status: cluster-editor-pane-expand
// status: cluster-editor-pane-back-to-tree
// status: cluster-editor-pane-leaf-click-opens-note
// status: cluster-editor-batch-review-pane
// status: cluster-editor-batch-review-pane-mode
// status: cluster-editor-apply-action
// status: cluster-editor-sapling-evergreen-lifecycle
// status: suggestions-rejection-history
//
// Expanded cluster-editor pane. Two sub-states keyed off the active
// buffer's `mode.kind`:
//
//   - `cluster-tree`         — graphical tree view + Apply / Save-as-triage
//                              toolbar. Leaf click opens the note. For Sprint C
//                              the surface is a structurally simple reuse of
//                              the sidebar row primitive; Sprint D's drag-drop
//                              graph view is a follow-up.
//   - `cluster-batch-review` — post-Apply rows surface grouped by Move /
//                              Tag, with per-row Accept/Reject and
//                              Accept-all / Reject-all batch verbs. Back-to-
//                              tree flips back to `cluster-tree` mode.
//
// Mounting follows the same shape as `editorPane` and `patchReview`:
// host wires DOM refs + buffer-state accessors; the module owns its
// internal render. The pane is a *peer surface* of `#editor`,
// `#vault-home`, `#settings-pane`, `#properties-pane`.

import { invoke } from "@tauri-apps/api/core";
import { onHikerEventAs } from "../events";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { describeErr } from "../ipc/runCommand";
import {
  closeContextMenu,
  isContextMenuOpen,
  openContextMenu,
  type CtxMenuItem,
} from "../widgets/contextMenu";
import { Icons } from "../icons";
import { el } from "../widgets/dom";
import type {
  ClusterNodeRow,
  ClusterTreeRow,
} from "../clusterEditor";
import {
  onDragStateChange,
  renderMultiSelectToolbar,
  renderPromoteBand,
  renderSiblingsWithOutliers,
  summarizeOutcomeToast,
  type SummarizeSweepOutcome,
  type TreeRowDeps,
  type TreeRowSurfaceState,
} from "../clusterEditor/rowPrimitive";
import type { GraphViewApi } from "./graphView";

interface ClusterApplyOutcome {
  tree_id: string;
  staged_ids: string[];
  moves: number;
  tags: number;
  frozen: number;
  unpolicied: number;
  missing: number;
}

interface StagingProposal {
  id: string;
  surface: string;
  action: string;
  target_path: string;
  source_path?: string | null;
  metadata?: Record<string, unknown> | null;
  state?: string;
  conflict_reason?: string | null;
}

export interface ClusterEditorPaneDeps {
  /// Pane root (`#cluster-editor-pane`).
  rootEl: HTMLElement;
  /// Vault-relative open-note hook (shared with the sidebar cluster
  /// editor).
  openNote: (rel: string, opts?: { preview?: boolean }) => Promise<void> | void;
  /// Pop the editor-pane tab back to whatever was active before the
  /// pane opened. Host owns the tab lifecycle.
  closePane: () => void;
  /// status: cluster-review-tab-from-recluster-action
  /// Open (or activate) the clustering review tab for a subtree
  /// recluster. Routed through the row primitive's right-click menu.
  openReclusterReview: (treeId: string, nodeId: string, nodeName: string) => void;
}

export interface ClusterEditorPaneApi {
  /// Show the expanded tree view for `treeId`. Idempotent — re-paints
  /// if already showing the same tree.
  showTree(treeId: string): Promise<void>;
  /// Show the batch-review surface for `treeId`. Re-fetches the rows.
  showBatchReview(treeId: string): Promise<void>;
  /// Force a refresh of whatever sub-state is active.
  refresh(): Promise<void>;
  /// Hide the pane root.
  hide(): void;
}

type SubState =
  | { kind: "tree"; treeId: string }
  | { kind: "batch-review"; treeId: string };

const Api = {
  list(): Promise<ClusterTreeRow[]> {
    return invoke("cluster_trees_list");
  },
  get(treeId: string): Promise<ClusterNodeRow[]> {
    return invoke("cluster_tree_get", { treeId });
  },
  apply(treeId: string): Promise<ClusterApplyOutcome> {
    return invoke("cluster_apply", { treeId });
  },
  setState(treeId: string, newState: string): Promise<void> {
    return invoke("cluster_tree_set_state", { treeId, newState });
  },
  staging_list(filter: {
    surface?: string;
    path?: string | null;
  }): Promise<StagingProposal[]> {
    return invoke("staging_list", { filter });
  },
  staging_accept(proposalId: string): Promise<unknown> {
    return invoke("staging_accept", { proposalId });
  },
  staging_reject(proposalId: string): Promise<void> {
    return invoke("staging_reject", { proposalId });
  },
  record_rejection(
    fingerprint: string,
    notePath: string,
    action: string,
  ): Promise<void> {
    return invoke("cluster_record_rejection", {
      fingerprint,
      notePath,
      action,
    });
  },
  discard(treeId: string): Promise<void> {
    return invoke("cluster_tree_discard", { treeId });
  },
  rebuild(treeId: string, newName: string | null): Promise<string> {
    return invoke("cluster_tree_rebuild", { treeId, newName });
  },
};

export function mountClusterEditorPane(
  deps: ClusterEditorPaneDeps,
): ClusterEditorPaneApi {
  const root = deps.rootEl;
  root.classList.add("cluster-editor-pane");

  let sub: SubState | null = null;
  let cachedTree: ClusterTreeRow | null = null;
  let cachedNodes: ClusterNodeRow[] = [];
  let cachedRows: StagingProposal[] = [];
  // status: cluster-editor-row-primitive
  //
  // Per-tree expand/collapse + multi-select sets. These survive
  // `refresh()` (the queue-event listener re-fetches `cachedNodes` but
  // we keep the sets keyed off `treeId` so the user's collapse state
  // and selection ride through). First time a tree opens we seed
  // `expanded` with its root nodes so the user sees the top level
  // unfolded — matches the sidebar's "first tree open by default"
  // behavior.
  const expandedByTree = new Map<string, Set<string>>();
  const selectionByTree = new Map<string, Set<string>>();
  // status: cluster-editor-graph-view-toggle
  //
  // Sub-sub-mode of `cluster-tree` — tree (row) vs graph. NOT a new
  // BufferMode; same buffer, same `cluster-tree` mode, just a view
  // toggle inside the pane.
  // status: cluster-editor-markdown-view-toggle
  //
  // Third variant alongside "tree" / "graph": an export-shaped markdown
  // rendering of the tree, rendered on demand from the cached
  // `ClusterNodeRow` set + the cached `cachedTree` metadata. Matches the
  // `suggestions-proposal-md` format documented in `docs/suggestions.md`.
  // Read-only for now — the spec calls for an editable CodeMirror buffer
  // that's parsed back on save; that's a follow-up. Bundle-size choice
  // (no markdown library dep) since this is an export-style view, not
  // rendered HTML.
  let treeViewVariant: "tree" | "graph" | "markdown" = "tree";
  // status: cluster-editor-graph-view-lazy-load
  let graphApi: GraphViewApi | null = null;
  // Set by `paintTree` so the radio's `onChange` can swap just the body
  // (no toolbar rebuild). Keeping the toolbar stable across view-mode
  // switches is what lets the view-options menu refresh in place.
  let paintBodyHook: (() => void) | null = null;
  // Selection survives the row/graph switch (`cluster-editor-graph-view-toggle`).
  const sharedSelection = new Set<string>();

  async function fetchTreeMeta(treeId: string): Promise<void> {
    const list = await Api.list();
    cachedTree = list.find((t) => t.id === treeId) ?? null;
    cachedNodes = await Api.get(treeId);
    // Seed expanded set with root cluster nodes the first time this
    // tree opens in the pane — sensible default per the spec
    // ("expand the root level only" on first open).
    if (!expandedByTree.has(treeId)) {
      const initial = new Set<string>();
      for (const n of cachedNodes) {
        if (n.parent === null && (n.kind === "cluster" || n.kind === "outlier-bucket")) {
          initial.add(n.id);
        }
      }
      expandedByTree.set(treeId, initial);
    }
    if (!selectionByTree.has(treeId)) {
      selectionByTree.set(treeId, new Set<string>());
    }
  }

  /// Build the row-primitive surface state for the active tree. The
  /// `nodes` array is the live `cachedNodes` reference; the primitive
  /// treats it as read-only.
  function buildSurfaceState(treeId: string): TreeRowSurfaceState | null {
    if (!cachedTree || cachedTree.id !== treeId) return null;
    const expanded = expandedByTree.get(treeId) ?? new Set<string>();
    const selection = selectionByTree.get(treeId) ?? new Set<string>();
    return {
      tree: cachedTree,
      nodes: cachedNodes,
      expanded,
      selection,
    };
  }

  function rowDeps(treeId: string): TreeRowDeps {
    return {
      refresh: async () => {
        try {
          cachedNodes = await Api.get(treeId);
          // Body-only repaint keeps the toolbar (and the view-menu's
          // anchor) stable. Falls back to full `paint()` only on the
          // pre-first-paint path.
          if (paintBodyHook) paintBodyHook();
          else paint();
        } catch (err) {
          Logger.error("ui::clusterEditor", "pane refresh failed", { err });
        }
      },
      repaint: () => {
        if (paintBodyHook) paintBodyHook();
        else paint();
      },
      openNote: deps.openNote,
      openReclusterReview: deps.openReclusterReview,
    };
  }

  async function fetchBatchRows(treeId: string): Promise<void> {
    const rows = await Api.staging_list({ surface: "cluster-editor" });
    cachedRows = rows.filter((r) => {
      const md = r.metadata as Record<string, unknown> | null | undefined;
      const tid =
        md && typeof md["tree_id"] === "string" ? (md["tree_id"] as string) : null;
      return tid === treeId;
    });
  }

  async function showTree(treeId: string): Promise<void> {
    sub = { kind: "tree", treeId };
    try {
      await fetchTreeMeta(treeId);
    } catch (err) {
      Logger.error("ui::clusterEditor", "fetchTreeMeta failed", { err });
    }
    paint();
  }

  async function showBatchReview(treeId: string): Promise<void> {
    sub = { kind: "batch-review", treeId };
    try {
      await Promise.all([fetchTreeMeta(treeId), fetchBatchRows(treeId)]);
    } catch (err) {
      Logger.error("ui::clusterEditor", "showBatchReview fetch failed", {
        err,
      });
    }
    paint();
  }

  async function refresh(): Promise<void> {
    if (!sub) return;
    if (sub.kind === "tree") await showTree(sub.treeId);
    else await showBatchReview(sub.treeId);
  }

  function hide(): void {
    if (graphApi) {
      graphApi.destroy();
      graphApi = null;
    }
    root.hidden = true;
  }

  function paint(): void {
    // Sigma owns its canvas + listeners; tear down before
    // `replaceChildren` orphans the DOM refs. paintTree will lazily
    // re-mount the graph view if treeViewVariant === "graph".
    if (graphApi) {
      graphApi.destroy();
      graphApi = null;
    }
    root.replaceChildren();
    // Visibility is owned by the host's `renderActiveTab` (which toggles
    // `#cluster-editor-pane.hidden` based on the active tab's kind).
    // paint() previously force-unhid the pane, which made the queue-
    // event-driven refresh fire while the user was on a different tab
    // (e.g. background tasks), occluding it. Just paint the contents.
    if (!sub) return;
    if (sub.kind === "tree") {
      paintTree(sub.treeId);
    } else {
      paintBatchReview(sub.treeId);
    }
  }

  // ── Tree view ────────────────────────────────────────────────────

  function paintTree(treeId: string): void {
    const head = el("header", { class: "cep-head" }, [
      el("h2", { text: cachedTree?.name ?? "(unknown tree)" }),
      el("span", {
        class: "cep-state-pill",
        text: cachedTree?.state ?? "draft",
        data: { state: cachedTree?.state ?? "draft" },
      }),
      el("span", { class: "cep-spacer" }),
    ]);

    // Toolbar: build the icon buttons in this order: Save-as-triage,
    // Regenerate names, Rebuild, view toggle, then Apply (✓) + Discard
    // (✕) as the trailing accept/reject pair. `applyBtn` is created
    // here but appended later, just before `discardBtn`, so the pair
    // sits together.
    const applyBtn = el("button", {
      class: "cep-btn cep-btn-primary cep-icon-btn",
      html: Icons.check(),
      title: "Apply — emit staging rows for every Tag/Move-policied leaf",
      attrs: { type: "button", "aria-label": "Apply" },
    });
    applyBtn.addEventListener("click", async () => {
      applyBtn.disabled = true;
      try {
        const outcome = await Api.apply(treeId);
        // status: cluster-editor-sapling-evergreen-lifecycle
        // Sapling lifecycle: state stays `draft` until the user resolves
        // every row in the batch-review pane (auto-flip to `applied` on
        // completion). If Apply emitted zero rows, flip immediately so
        // the tree shows as completed.
        if (outcome.staged_ids.length === 0) {
          showToast(
            `Apply: 0 rows produced (${outcome.unpolicied} unpolicied, ${outcome.frozen} frozen, ${outcome.missing} missing)`,
          );
          await Api.setState(treeId, "applied").catch(() => {});
        }
        await showBatchReview(treeId);
      } catch (err) {
        Logger.error("ui::clusterEditor", "apply failed", { err });
        showToast(`Apply failed: ${describeErr(err)}`);
        applyBtn.disabled = false;
      }
    });
    // Append later — see the comment above and the explicit append below.

    // status: cluster-editor-save-as-triage
    // status: cluster-editor-sapling-evergreen-lifecycle (Evergreen branch)
    //
    // Save-as-triage persists the tree's policies as the active triage
    // classifier — new note saves get routed against it via
    // `cluster_triage_enqueue`. The tree's `state` flips to
    // `saved-as-triage`; subsequent re-renders show the new state pill.
    const saveBtn = el("button", {
      class: "cep-btn cep-icon-btn",
      html: Icons.triageStar(),
      attrs: { type: "button", "aria-label": "Save as triage" },
    });
    const alreadySaved = cachedTree?.state === "saved-as-triage";
    if (alreadySaved) {
      saveBtn.disabled = true;
      saveBtn.title = "This tree is already the active triage classifier";
    } else {
      saveBtn.title = "Save as triage — persist this tree's policies as the active triage classifier";
      saveBtn.addEventListener("click", async () => {
        const confirmMsg =
          "Save this tree as the active triage classifier? It will fire on new note saves.";
        if (!confirm(confirmMsg)) return;
        saveBtn.disabled = true;
        try {
          await Api.setState(treeId, "saved-as-triage");
          showToast("Tree saved as triage classifier");
          await showTree(treeId);
        } catch (err) {
          Logger.error("ui::clusterEditor", "save-as-triage failed", { err });
          showToast(`Save failed: ${describeErr(err)}`);
          saveBtn.disabled = false;
        }
      });
    }
    head.appendChild(saveBtn);

    // status: cluster-editor-regenerate-via-task-queue
    // status: cluster-editor-llm-actions-via-task-queue
    const regenBtn = el("button", {
      class: "cep-btn cep-icon-btn",
      html: Icons.restore(),
      title: "Regenerate names — enqueue one summarize task per non-user-edited cluster row",
      attrs: { type: "button", "aria-label": "Regenerate names" },
    });
    regenBtn.addEventListener("click", async () => {
      regenBtn.disabled = true;
      try {
        const ids: string[] = await invoke("cluster_regenerate_names", {
          treeId,
        });
        showToast(`Queued ${ids.length} regeneration tasks`);
      } catch (err) {
        showToast(`Regenerate failed: ${describeErr(err)}`);
      } finally {
        regenBtn.disabled = false;
      }
    });
    head.appendChild(regenBtn);

    // status: cluster-editor-summarize-stale-action
    //
    // "Summarize new / changed (N)" — fan-out Summarize over every
    // cluster whose name/summary is empty or whose membership churn
    // counter is non-zero. Predicate mirrors `SummarizeScope::StaleOrUnfilled`
    // server-side; computed client-side off `cachedNodes` (the pane
    // already re-fetches on `cluster_nodes` row changes and on queue
    // completion events, so the count refreshes naturally without a
    // dedicated subscription).
    const staleClusterIds = cachedNodes
      .filter(
        (n) =>
          n.kind === "cluster" &&
          (n.summary_membership_churn > 0 ||
            n.summary === "" ||
            n.name === ""),
      )
      .map((n) => n.id);
    const stalecount = staleClusterIds.length;
    const summarizeStaleBtn = el("button", {
      class: "cep-btn",
      text: stalecount > 0
        ? `Summarize new / changed (${stalecount})`
        : "Summarize new / changed",
      attrs: { type: "button", "aria-label": "Summarize new / changed clusters" },
    });
    if (stalecount === 0) {
      summarizeStaleBtn.disabled = true;
      summarizeStaleBtn.title =
        "Everything is fresh — nothing to summarize";
    } else {
      summarizeStaleBtn.title =
        "Summarize new / changed — enqueue a summarize task for every cluster whose membership shifted or whose name/summary is empty";
      summarizeStaleBtn.addEventListener("click", async () => {
        summarizeStaleBtn.disabled = true;
        try {
          const params = {
            scope: { kind: "stale-or-unfilled" },
            subtree_root: null,
            recursive: true,
            summarize_mode: "llm",
            overwrite_user_edited: false,
          };
          const outcome: SummarizeSweepOutcome = await invoke(
            "cluster_summarize",
            { treeId, paramsJson: JSON.stringify(params) },
          );
          showToast(summarizeOutcomeToast(outcome, stalecount));
        } catch (err) {
          showToast(`Summarize failed: ${describeErr(err)}`);
          summarizeStaleBtn.disabled = false;
        }
      });
    }
    head.appendChild(summarizeStaleBtn);

    // status: cluster-build-rebuild
    const rebuildBtn = el("button", {
      class: "cep-btn cep-icon-btn",
      html: Icons.hammer(),
      title: "Rebuild — re-run the original build pipeline against the current vault state; user-edited names / summaries / policies on overlapping clusters are preserved",
      attrs: { type: "button", "aria-label": "Rebuild" },
    });
    rebuildBtn.addEventListener("click", async () => {
      const confirmMsg = `Rebuild "${cachedTree?.name ?? treeId}" against the current vault? A new draft tree will be created — this one stays put.`;
      if (!confirm(confirmMsg)) return;
      rebuildBtn.disabled = true;
      try {
        // Submission returns the queue task id, not the new tree id.
        // The new draft appears in the sidebar when the direct worker
        // finishes; the user can watch the queue page meanwhile.
        await Api.rebuild(treeId, null);
        showToast("Rebuild queued. Track progress on the queue page.");
        rebuildBtn.disabled = false;
      } catch (err) {
        Logger.error("ui::clusterEditor", "rebuild submit failed", { err });
        showToast(`Rebuild failed to submit: ${describeErr(err)}`);
        rebuildBtn.disabled = false;
      }
    });
    head.appendChild(rebuildBtn);

    // Divider between the build/manage cluster (Triage/Regen/Rebuild)
    // and the view-toggle cluster.
    head.appendChild(el("span", {
      class: "cep-divider",
      attrs: { "aria-hidden": "true" },
    }));

    const discardBtn = el("button", {
      class: "cep-btn cep-icon-btn",
      html: Icons.close(),
      title: "Discard draft",
      attrs: { type: "button", "aria-label": "Discard draft" },
    });
    discardBtn.addEventListener("click", async () => {
      if (!confirm(`Discard draft "${cachedTree?.name ?? treeId}"?`)) return;
      try {
        await Api.discard(treeId);
        deps.closePane();
      } catch (err) {
        showToast(`Discard failed: ${describeErr(err)}`);
      }
    });
    // discardBtn is appended at the end of the toolbar, after the view
    // toggle and apply button — see the trailing `head.appendChild`s.

    // status: cluster-editor-graph-view-toggle
    // status: cluster-editor-graph-view-view-menu
    //
    // Unified "view options" menu (eye icon, matching the editor
    // toolbar's `#view-menu-btn`). Always painted in the pane toolbar.
    // The menu carries a "View as" radio (Tree / Graph / Markdown) and,
    // when the graph variant is active, the graph-specific options
    // (leaves visibility, layout, show outliers, fit/reset, note
    // preview toggle) folded in from `graphApi.getViewMenuItems()`.
    // Previously these lived as a separate 3-button strip on the
    // toolbar; consolidating them keeps the pinned bar tidier.
    const viewMenuBtn = el("button", {
      class: "cep-btn cep-icon-btn",
      html: Icons.eye(),
      title: "View options",
      attrs: { type: "button", "aria-label": "View options" },
      onClick: (e) => {
        e.stopPropagation();
        openViewMenuFor(viewMenuBtn);
      },
    });
    head.appendChild(viewMenuBtn);

    function openViewMenuFor(anchor: HTMLElement): void {
      // Guard against the menu being reopened against a stale anchor.
      // Can happen when `paint()` (rather than `paintBody()`) ran since
      // the menu items were built — the old eye button is detached,
      // and anchoring the popover to it gives a useless 0,0 position.
      // Fall back to the current eye button.
      if (!anchor.isConnected) {
        const live = root.querySelector<HTMLButtonElement>(
          ".cep-head .cep-icon-btn[aria-label='View options']",
        );
        if (!live) return;
        anchor = live;
      }
      const items: CtxMenuItem[] = [
        {
          kind: "radio",
          label: "View as",
          value: treeViewVariant,
          options: [
            { label: "Tree", value: "tree" },
            { label: "Graph", value: "graph" },
            { label: "Markdown", value: "markdown" },
          ],
          onChange: (v) => {
            const next = v as "tree" | "graph" | "markdown";
            if (treeViewVariant === next) return;
            treeViewVariant = next;
            // Swap *only* the body. The toolbar (and our anchor button)
            // stays in place across the view-mode switch — that's the
            // whole point of factoring `paintBody` out from `paintTree`.
            // graphApi teardown is owned by `paintBody` (it knows when
            // it's about to draw a non-graph variant).
            if (paintBodyHook) paintBodyHook();
            else paint();
            // Refresh the menu in place against the still-mounted
            // anchor. `openContextMenu` has a same-trigger toggle-close
            // short-circuit; bypass it by explicitly closing first.
            closeContextMenu();
            openViewMenuFor(viewMenuBtn);
          },
        },
      ];
      if (treeViewVariant === "tree") {
        items.push({
          label: "Expand all",
          run: () => {
            // status: cluster-editor-row-primitive
            // Seed the pane-local `expanded` set with every cluster /
            // outlier-bucket node so the entire tree opens at once.
            // Leaves don't have children to expand; skip them. Use
            // `paintBodyHook` rather than `paint()` so the toolbar
            // (and the menu's anchor) survives the repaint.
            const ex = expandedByTree.get(treeId);
            if (!ex) return;
            for (const n of cachedNodes) {
              if (
                n.kind === "cluster"
                || n.kind === "outlier-bucket"
              ) {
                ex.add(n.id);
              }
            }
            if (paintBodyHook) paintBodyHook();
            else paint();
          },
        });
        items.push({
          label: "Collapse all",
          run: () => {
            const ex = expandedByTree.get(treeId);
            if (!ex) return;
            ex.clear();
            if (paintBodyHook) paintBodyHook();
            else paint();
          },
        });
      }
      if (treeViewVariant === "graph" && graphApi) {
        items.push(...graphApi.getViewMenuItems());
      }
      const rect = anchor.getBoundingClientRect();
      openContextMenu(rect.right, rect.bottom, items, anchor);
    }

    // Divider between the view-toggle cluster and the accept/reject
    // pair (Apply ✓ / Discard ✕).
    head.appendChild(el("span", {
      class: "cep-divider",
      attrs: { "aria-hidden": "true" },
    }));

    // Trailing accept/reject pair — Apply (✓) immediately left of
    // Discard (✕), matching the left-to-right accept-then-reject reading
    // order used elsewhere in the app.
    head.appendChild(applyBtn);
    head.appendChild(discardBtn);

    // Close (✕) button removed — the tab strip already has its own
    // close affordance for this tab kind.

    root.appendChild(head);

    // Body rendering is factored out so view-mode switches can re-render
    // just the body without rebuilding the toolbar. Keeping the toolbar
    // (and the eye button on it) stable across switches is what lets the
    // view-options menu refresh in place: the menu's anchor element
    // doesn't disappear under it.
    paintBody();

    function paintBody(): void {
      // Tear down any prior body content. Multi-select toolbar lives in
      // the head — leave it alone; remove only the children added by
      // earlier `paintBody()` calls.
      for (const el of Array.from(
        root.querySelectorAll(":scope > .cep-graph-host, :scope > .cep-markdown-host, :scope > .cep-tree-body"),
      )) {
        el.remove();
      }
      // Destroy any live graph view before we paint a non-graph body.
      if (graphApi && treeViewVariant !== "graph") {
        graphApi.destroy();
        graphApi = null;
      }

    if (treeViewVariant === "graph") {
      // status: cluster-editor-graph-view
      // status: cluster-editor-graph-view-lazy-load
      const gHost = el("div", { class: "cep-graph-host" });
      root.appendChild(gHost);
      // Dynamic import — sigma + graphology bundle paid on first open.
      import("./graphView").then((mod) =>
        mod.mountGraphView({
          treeId,
          nodes: cachedNodes,
          hostEl: gHost,
          openNote: deps.openNote,
          onMutated: async () => {
            await fetchTreeMeta(treeId);
            graphApi?.setNodes(cachedNodes);
          },
          onSelectionChanged: (s) => {
            sharedSelection.clear();
            for (const id of s) sharedSelection.add(id);
          },
        }),
      ).then((api) => {
        // If the user switched away (or to a *different* graph mount)
        // before this async mount resolved, this `api` is attached to
        // an orphaned `gHost`. Destroy it here — without this guard
        // sigma's document-level listeners (mousemove / mouseup /
        // touchend / touchmove + window resize) accumulate one set per
        // orphaned mount and the pane gradually stops responding.
        //
        // The decisive check is `gHost.isConnected`: paintBody removes
        // the prior gHost when it re-renders, so any in-flight mount
        // whose gHost is no longer in the DOM is stale by definition.
        if (!gHost.isConnected || treeViewVariant !== "graph") {
          api.destroy();
          return;
        }
        // Newer mount already won — also destroy this stale one.
        if (graphApi !== null) {
          api.destroy();
          return;
        }
        graphApi = api;
        // If the user already opened the view-options menu (anchored
        // to the still-mounted, stable eye button), refresh its items
        // now that `graphApi.getViewMenuItems()` can return the graph-
        // specific switches.
        if (isContextMenuOpen(viewMenuBtn)) {
          closeContextMenu();
          openViewMenuFor(viewMenuBtn);
        }
        // Selection survives the row/graph switch — re-applying any
        // prior selection isn't supported by the current graph API
        // (no `setSelection`), but the shared set is preserved here
        // for the row view to pick up on the back-switch. We read it
        // below to keep TS happy and to document intent.
        if (sharedSelection.size > 0) {
          Logger.info("ui::clusterEditor", "graph view: prior selection", {
            count: sharedSelection.size,
          });
        }
      }).catch((err) => {
        Logger.error("ui::clusterEditor", "graph view mount failed", { err });
        showToast(`Graph view failed: ${describeErr(err)}`);
      });
      return;
    }

    if (treeViewVariant === "markdown") {
      // status: cluster-editor-markdown-view-toggle
      root.appendChild(el("div", { class: "cep-markdown-host" }, [
        el("pre", {
          class: "cep-markdown-body",
          text: renderTreeAsMarkdown(cachedTree, cachedNodes),
        }),
      ]));
      return;
    }

    // Body: full row-primitive tree render. The pane now shares the
    // sidebar's expand/collapse + click-to-edit + right-click + policy
    // chip + multi-select interactions (per `cluster-editor-row-
    // primitive`). The surface emits `.ce-*` class names; the pane's
    // CSS extends those selectors to its own wrapper.
    const surface = buildSurfaceState(treeId);
    if (!surface) {
      // No tree loaded yet — `fetchTreeMeta` either failed or hasn't
      // resolved. The header is already painted; nothing else to do.
      return;
    }
    // Multi-select toolbar lives in the header next to the existing
    // toolbar buttons when there's an active selection. Re-uses the
    // shared `.ce-msel-toolbar` chrome.
    if (surface.selection.size > 0) {
      head.appendChild(el("div", { class: "cep-msel-host" }, [
        renderMultiSelectToolbar(surface, rowDeps(treeId)),
      ]));
    }
    const body = el("div", { class: "cep-tree-body" });
    // status: cluster-editor-dnd-visual-feedback
    const band = renderPromoteBand(surface, rowDeps(treeId));
    if (band) body.appendChild(band);
    const rootNodes = cachedNodes.filter((n) => n.parent === null);
    const els = renderSiblingsWithOutliers(surface, rowDeps(treeId), rootNodes, 0);
    for (const el of els) body.appendChild(el);
    root.appendChild(body);
    }

    // Expose the body-only repaint to the radio onChange so view-mode
    // switches stay decoupled from the toolbar (so the eye button —
    // the menu's anchor — survives the switch).
    paintBodyHook = paintBody;
  }

  // status: cluster-editor-markdown-view-toggle
  //
  // Render the tree as the export-style markdown format documented in
  // `docs/suggestions.md` (`suggestions-proposal-md`). Read-only; the
  // editable CodeMirror buffer + parse-back-on-save path is a follow-up.
  function renderTreeAsMarkdown(
    tree: ClusterTreeRow | null,
    nodes: ClusterNodeRow[],
  ): string {
    const id = tree?.id ?? "(unknown)";
    const state = tree?.state ?? "draft";
    const name = tree?.name ?? "(unnamed)";
    let scopeLabel = "?";
    let methodLabel = "?";
    try {
      const s = tree?.scope_json ? JSON.parse(tree.scope_json) : null;
      if (s && typeof s === "object") {
        const kind = (s as Record<string, unknown>)["kind"];
        if (kind === "vault") scopeLabel = "Vault";
        else if (kind === "folder")
          scopeLabel = `Folder: ${(s as Record<string, unknown>)["rel"] ?? "?"}`;
        else if (kind === "notes") scopeLabel = "Selected notes";
      }
    } catch {}
    try {
      const m = tree?.method_json ? JSON.parse(tree.method_json) : null;
      if (m && typeof m === "object") {
        const kind = (m as Record<string, unknown>)["kind"];
        if (kind === "cluster") methodLabel = "Cluster (hdbscan)";
        else if (kind === "from-folders") methodLabel = "FromFolders";
      }
    } catch {}

    // Group leaves under their parent cluster id. The exported format
    // lists one section per cluster that has direct leaf members;
    // outliers go into their own section.
    const childMap = new Map<string, ClusterNodeRow[]>();
    for (const n of nodes) {
      if (!n.parent) continue;
      const arr = childMap.get(n.parent);
      if (arr) arr.push(n);
      else childMap.set(n.parent, [n]);
    }
    const clusterRows = nodes.filter(
      (n) => n.kind === "cluster" || n.kind === "outlier-bucket",
    );
    let clusterCount = 0;
    let leafCount = 0;
    let outlierCount = 0;
    for (const n of nodes) {
      if (n.kind === "cluster") clusterCount += 1;
      else if (n.kind === "leaf") {
        // Count as outlier if any ancestor is an outlier bucket.
        let p: ClusterNodeRow | undefined = nodes.find((x) => x.id === n.parent);
        let isOutlier = false;
        const guard = new Set<string>();
        while (p && !guard.has(p.id)) {
          guard.add(p.id);
          if (p.kind === "outlier-bucket") {
            isOutlier = true;
            break;
          }
          p = nodes.find((x) => x.id === p?.parent);
        }
        if (isOutlier) outlierCount += 1;
        else leafCount += 1;
      }
    }

    const lines: string[] = [];
    lines.push(`# Cluster tree — ${id}  ·  ${state}  ·  ${name}`);
    lines.push("");
    lines.push(
      `Scope: ${scopeLabel}   Method: ${methodLabel}   ${clusterCount} clusters · ${leafCount} leaves · ${outlierCount} outliers`,
    );
    lines.push("");

    function leafPath(leaf: ClusterNodeRow): string {
      return leaf.note_path ?? leaf.note_ref ?? leaf.name;
    }
    function policySuffix(node: ClusterNodeRow): string {
      if (!node.policy_json) return "policy: none";
      try {
        const p = JSON.parse(node.policy_json);
        if (p.kind === "tag")
          return `policy: tag \`${p.slug}\`${p.require_review ? " ⏸" : ""}`;
        if (p.kind === "move")
          return `policy: move → \`${p.folder}\`${p.require_review ? " ⏸" : ""}`;
        if (p.kind === "freeze") return "policy: freeze";
      } catch {}
      return "policy: none";
    }

    // Emit one ## section per cluster / outlier-bucket that has at least
    // one direct leaf child (the export format is leaf-centric — deep
    // cluster trees collapse to their leaf-bearing clusters here).
    for (const c of clusterRows) {
      const directLeaves = (childMap.get(c.id) ?? []).filter(
        (n) => n.kind === "leaf",
      );
      if (directLeaves.length === 0) continue;
      const conf = c.confidence.toFixed(2);
      lines.push(
        `## ${c.name}  ·  confidence ${conf}  ·  ${policySuffix(c)}`,
      );
      lines.push("");
      for (const l of directLeaves) {
        lines.push(`- \`${leafPath(l)}\``);
      }
      lines.push("");
    }
    return lines.join("\n");
  }

  // ── Batch review ─────────────────────────────────────────────────

  function paintBatchReview(treeId: string): void {
    root.appendChild(el("header", { class: "cep-head" }, [
      el("h2", { text: `Apply review: ${cachedTree?.name ?? treeId}` }),
      el("span", { class: "cep-row-count", text: `${cachedRows.length} pending` }),
      el("span", { class: "cep-spacer" }),
      el("button", {
        class: "cep-btn cep-btn-primary",
        text: `Accept all (${cachedRows.length})`,
        attrs: { type: "button" },
        disabled: cachedRows.length === 0,
        onClick: async () => {
          if (cachedRows.length > 5) {
            if (!confirm(`Accept all ${cachedRows.length} rows?`)) return;
          }
          let n = 0;
          for (const r of cachedRows) {
            try {
              await Api.staging_accept(r.id);
              n += 1;
            } catch (err) {
              Logger.error("ui::clusterEditor", "accept failed", {
                err,
                id: r.id,
              });
            }
          }
          showToast(`Accepted ${n} of ${cachedRows.length} rows`);
          await maybeFlipApplied(treeId);
          await showBatchReview(treeId);
        },
      }),
      el("button", {
        class: "cep-btn",
        text: "Reject all",
        attrs: { type: "button" },
        disabled: cachedRows.length === 0,
        onClick: async () => {
          if (!confirm(`Reject all ${cachedRows.length} rows?`)) return;
          for (const r of cachedRows) {
            await rejectRow(r);
          }
          await maybeFlipApplied(treeId);
          await showBatchReview(treeId);
        },
      }),
      el("button", {
        class: "cep-btn",
        text: "← Back to tree",
        attrs: { type: "button" },
        onClick: () => { void showTree(treeId); },
      }),
      el("button", {
        class: "cep-btn",
        text: "✕",
        attrs: { type: "button" },
        onClick: () => deps.closePane(),
      }),
    ]));

    if (cachedRows.length === 0) {
      root.appendChild(el("p", {
        class: "cep-empty",
        text: "All rows resolved. The tree is marked applied.",
      }));
      return;
    }

    const moves = cachedRows.filter((r) => r.action === "move_note");
    const tags = cachedRows.filter((r) => r.action === "apply_tag");
    if (moves.length > 0) root.appendChild(renderGroup("Move", moves, treeId));
    if (tags.length > 0) root.appendChild(renderGroup("Tag", tags, treeId));
  }

  function renderGroup(
    label: string,
    rows: StagingProposal[],
    treeId: string,
  ): HTMLElement {
    rows.sort((a, b) => a.target_path.localeCompare(b.target_path));
    return el("section", { class: "cep-group" }, [
      el("h3", { text: `${label} (${rows.length})` }),
      ...rows.map((r) => renderProposalRow(r, treeId)),
    ]);
  }

  function renderProposalRow(
    row: StagingProposal,
    treeId: string,
  ): HTMLElement {
    const conflicted = row.state === "conflicted";
    let leftText: string;
    if (row.action === "move_note") {
      leftText = `${row.source_path ?? "?"} → ${row.target_path}`;
    } else {
      const md = (row.metadata ?? {}) as Record<string, unknown>;
      const slug = typeof md["tag_slug"] === "string" ? (md["tag_slug"] as string) : "";
      leftText = `${row.target_path}   + ${slug}`;
    }
    return el("div", {
      class: conflicted ? "cep-prow cep-prow-conflicted" : "cep-prow",
    }, [
      el("span", { class: "cep-prow-left", text: leftText }),
      conflicted
        ? el("span", {
            class: "cep-prow-warn",
            text: `⚠ ${row.conflict_reason ?? "conflict"}`,
          })
        : null,
      el("button", {
        class: "cep-prow-btn",
        text: "✓",
        title: "Accept",
        attrs: { type: "button" },
        disabled: conflicted,
        onClick: async () => {
          try {
            await Api.staging_accept(row.id);
            await maybeFlipApplied(treeId);
            await showBatchReview(treeId);
          } catch (err) {
            showToast(`Accept failed: ${describeErr(err)}`);
          }
        },
      }),
      el("button", {
        class: "cep-prow-btn",
        text: "✗",
        title: "Reject",
        attrs: { type: "button" },
        onClick: async () => {
          try {
            await rejectRow(row);
            await maybeFlipApplied(treeId);
            await showBatchReview(treeId);
          } catch (err) {
            showToast(`Reject failed: ${describeErr(err)}`);
          }
        },
      }),
    ]);
  }

  async function rejectRow(row: StagingProposal): Promise<void> {
    // status: suggestions-rejection-history — record before reject so a
    // crash mid-flight leaves the history truthful.
    const md = (row.metadata ?? {}) as Record<string, unknown>;
    const fp =
      typeof md["tree_member_fingerprint"] === "string"
        ? (md["tree_member_fingerprint"] as string)
        : null;
    const notePath = row.source_path ?? row.target_path;
    if (fp) {
      try {
        await Api.record_rejection(fp, notePath, row.action);
      } catch (err) {
        Logger.error("ui::clusterEditor", "record_rejection failed", {
          err,
        });
      }
    }
    await Api.staging_reject(row.id);
  }

  async function maybeFlipApplied(treeId: string): Promise<void> {
    try {
      await fetchBatchRows(treeId);
      if (cachedRows.length === 0) {
        await Api.setState(treeId, "applied").catch(() => {});
        showToast("All cluster-editor rows resolved — tree marked applied");
      }
    } catch (err) {
      Logger.error("ui::clusterEditor", "maybeFlipApplied failed", {
        err,
      });
    }
  }

  // status: cluster-editor-dnd-visual-feedback
  // Re-paint when a drag starts or ends so the promote-to-top band
  // surfaces and tears down without polling.
  onDragStateChange(() => {
    if (sub?.kind === "tree") paint();
  });

  // Auto-refresh the open tree view when a `raptor_summarize` task
  // completes — the worker writes new auto-generated names + summaries
  // back to `trees.db` via `Trees::auto_set_name_summary`, and without a
  // refresh the pane keeps showing the placeholder names.
  const pendingRaptor = new Set<string>();
  type QueueEvt = {
    event: string;
    id?: string;
    kind?: { type?: string };
  };
  void onHikerEventAs<QueueEvt>("hiker:queue-event", (payload) => {
    const p = payload;
    if (p.event === "task_queued") {
      if (p.id && p.kind?.type === "raptor_summarize") {
        pendingRaptor.add(p.id);
      }
      return;
    }
    if (
      p.id &&
      (p.event === "task_completed"
        || p.event === "task_failed"
        || p.event === "task_cancelled")
    ) {
      if (pendingRaptor.delete(p.id) && sub?.kind === "tree") {
        void refresh();
      }
    }
  });

  return {
    showTree,
    showBatchReview,
    refresh,
    hide,
  };
}
