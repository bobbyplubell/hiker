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
import { listen } from "@tauri-apps/api/event";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { Icons } from "../icons";
import type {
  ClusterNodeRow,
  ClusterTreeRow,
} from "../clusterEditor";
import {
  renderMultiSelectToolbar,
  renderSiblingsWithOutliers,
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
          paint();
        } catch (err) {
          Logger.error("ui::clusterEditor", "pane refresh failed", { err });
        }
      },
      repaint: () => paint(),
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
    const head = document.createElement("header");
    head.className = "cep-head";
    const title = document.createElement("h2");
    title.textContent = cachedTree?.name ?? "(unknown tree)";
    head.appendChild(title);
    const meta = document.createElement("span");
    meta.className = "cep-state-pill";
    meta.textContent = cachedTree?.state ?? "draft";
    meta.dataset.state = cachedTree?.state ?? "draft";
    head.appendChild(meta);
    const spacer = document.createElement("span");
    spacer.className = "cep-spacer";
    head.appendChild(spacer);

    // Toolbar: build the icon buttons in this order: Save-as-triage,
    // Regenerate names, Rebuild, view toggle, then Apply (✓) + Discard
    // (✕) as the trailing accept/reject pair. `applyBtn` is created
    // here but appended later, just before `discardBtn`, so the pair
    // sits together.
    const applyBtn = document.createElement("button");
    applyBtn.type = "button";
    applyBtn.className = "cep-btn cep-btn-primary cep-icon-btn";
    applyBtn.innerHTML = Icons.check();
    applyBtn.setAttribute("aria-label", "Apply");
    applyBtn.title = "Apply — emit staging rows for every Tag/Move-policied leaf";
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
        showToast(`Apply failed: ${String(err)}`);
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
    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "cep-btn cep-icon-btn";
    saveBtn.innerHTML = Icons.triageStar();
    saveBtn.setAttribute("aria-label", "Save as triage");
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
          showToast(`Save failed: ${String(err)}`);
          saveBtn.disabled = false;
        }
      });
    }
    head.appendChild(saveBtn);

    // status: cluster-editor-regenerate-via-task-queue
    // status: cluster-editor-llm-actions-via-task-queue
    const regenBtn = document.createElement("button");
    regenBtn.type = "button";
    regenBtn.className = "cep-btn cep-icon-btn";
    regenBtn.innerHTML = Icons.restore();
    regenBtn.setAttribute("aria-label", "Regenerate names");
    regenBtn.title =
      "Regenerate names — enqueue one summarize task per non-user-edited cluster row";
    regenBtn.addEventListener("click", async () => {
      regenBtn.disabled = true;
      try {
        const ids: string[] = await invoke("cluster_regenerate_names", {
          treeId,
        });
        showToast(`Queued ${ids.length} regeneration tasks`);
      } catch (err) {
        showToast(`Regenerate failed: ${String(err)}`);
      } finally {
        regenBtn.disabled = false;
      }
    });
    head.appendChild(regenBtn);

    // status: cluster-build-rebuild
    const rebuildBtn = document.createElement("button");
    rebuildBtn.type = "button";
    rebuildBtn.className = "cep-btn cep-icon-btn";
    rebuildBtn.innerHTML = Icons.hammer();
    rebuildBtn.setAttribute("aria-label", "Rebuild");
    rebuildBtn.title =
      "Rebuild — re-run the original build pipeline against the current vault state; user-edited names / summaries / policies on overlapping clusters are preserved";
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
        showToast(`Rebuild failed to submit: ${String(err)}`);
        rebuildBtn.disabled = false;
      }
    });
    head.appendChild(rebuildBtn);

    // Divider between the build/manage cluster (Triage/Regen/Rebuild)
    // and the view-toggle cluster.
    const divider1 = document.createElement("span");
    divider1.className = "cep-divider";
    divider1.setAttribute("aria-hidden", "true");
    head.appendChild(divider1);

    const discardBtn = document.createElement("button");
    discardBtn.type = "button";
    discardBtn.className = "cep-btn cep-icon-btn";
    discardBtn.innerHTML = Icons.close();
    discardBtn.setAttribute("aria-label", "Discard draft");
    discardBtn.title = "Discard draft";
    discardBtn.addEventListener("click", async () => {
      if (!confirm(`Discard draft "${cachedTree?.name ?? treeId}"?`)) return;
      try {
        await Api.discard(treeId);
        deps.closePane();
      } catch (err) {
        showToast(`Discard failed: ${String(err)}`);
      }
    });
    // discardBtn is appended at the end of the toolbar, after the view
    // toggle and apply button — see the trailing `head.appendChild`s.

    // status: cluster-editor-graph-view-toggle
    // View-variant toggle (tree vs graph). Selection survives the
    // switch — shared `sharedSelection` set.
    const variantToggle = document.createElement("div");
    variantToggle.className = "cep-view-toggle";
    for (const v of ["tree", "graph", "markdown"] as const) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "cep-btn cep-icon-btn cep-view-toggle-btn";
      const label = v === "tree" ? "Tree" : v === "graph" ? "Graph" : "Markdown";
      b.innerHTML =
        v === "tree"
          ? Icons.clusterTreeShape()
          : v === "graph"
            ? Icons.graphNodes()
            : Icons.mdLabel();
      b.title = label;
      b.setAttribute("aria-label", label);
      if (treeViewVariant === v) b.classList.add("active");
      b.addEventListener("click", () => {
        if (treeViewVariant === v) return;
        treeViewVariant = v;
        // Destroy the graph view if leaving it — lazy-mount again on
        // re-entry.
        if (v !== "graph" && graphApi) {
          graphApi.destroy();
          graphApi = null;
        }
        paint();
      });
      variantToggle.appendChild(b);
    }
    head.appendChild(variantToggle);

    // Divider between the view-toggle cluster and the accept/reject
    // pair (Apply ✓ / Discard ✕).
    const divider2 = document.createElement("span");
    divider2.className = "cep-divider";
    divider2.setAttribute("aria-hidden", "true");
    head.appendChild(divider2);

    // Trailing accept/reject pair — Apply (✓) immediately left of
    // Discard (✕), matching the left-to-right accept-then-reject reading
    // order used elsewhere in the app.
    head.appendChild(applyBtn);
    head.appendChild(discardBtn);

    // Close (✕) button removed — the tab strip already has its own
    // close affordance for this tab kind.

    root.appendChild(head);

    if (treeViewVariant === "graph") {
      // status: cluster-editor-graph-view
      // status: cluster-editor-graph-view-lazy-load
      const gHost = document.createElement("div");
      gHost.className = "cep-graph-host";
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
        graphApi = api;
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
        showToast(`Graph view failed: ${String(err)}`);
      });
      return;
    }

    if (treeViewVariant === "markdown") {
      // status: cluster-editor-markdown-view-toggle
      const mdHost = document.createElement("div");
      mdHost.className = "cep-markdown-host";
      const pre = document.createElement("pre");
      pre.className = "cep-markdown-body";
      pre.textContent = renderTreeAsMarkdown(
        cachedTree,
        cachedNodes,
      );
      mdHost.appendChild(pre);
      root.appendChild(mdHost);
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
      const mselHost = document.createElement("div");
      mselHost.className = "cep-msel-host";
      mselHost.appendChild(renderMultiSelectToolbar(surface, rowDeps(treeId)));
      head.appendChild(mselHost);
    }
    const body = document.createElement("div");
    body.className = "cep-tree-body";
    const rootNodes = cachedNodes.filter((n) => n.parent === null);
    const els = renderSiblingsWithOutliers(surface, rowDeps(treeId), rootNodes, 0);
    for (const el of els) body.appendChild(el);
    root.appendChild(body);
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
    const head = document.createElement("header");
    head.className = "cep-head";
    const title = document.createElement("h2");
    title.textContent = `Apply review: ${cachedTree?.name ?? treeId}`;
    head.appendChild(title);
    const count = document.createElement("span");
    count.className = "cep-row-count";
    count.textContent = `${cachedRows.length} pending`;
    head.appendChild(count);
    const spacer = document.createElement("span");
    spacer.className = "cep-spacer";
    head.appendChild(spacer);

    const acceptAllBtn = document.createElement("button");
    acceptAllBtn.type = "button";
    acceptAllBtn.className = "cep-btn cep-btn-primary";
    acceptAllBtn.textContent = `Accept all (${cachedRows.length})`;
    acceptAllBtn.disabled = cachedRows.length === 0;
    acceptAllBtn.addEventListener("click", async () => {
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
    });
    head.appendChild(acceptAllBtn);

    const rejectAllBtn = document.createElement("button");
    rejectAllBtn.type = "button";
    rejectAllBtn.className = "cep-btn";
    rejectAllBtn.textContent = "Reject all";
    rejectAllBtn.disabled = cachedRows.length === 0;
    rejectAllBtn.addEventListener("click", async () => {
      if (!confirm(`Reject all ${cachedRows.length} rows?`)) return;
      for (const r of cachedRows) {
        await rejectRow(r);
      }
      await maybeFlipApplied(treeId);
      await showBatchReview(treeId);
    });
    head.appendChild(rejectAllBtn);

    const backBtn = document.createElement("button");
    backBtn.type = "button";
    backBtn.className = "cep-btn";
    backBtn.textContent = "← Back to tree";
    backBtn.addEventListener("click", () => {
      void showTree(treeId);
    });
    head.appendChild(backBtn);

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "cep-btn";
    closeBtn.textContent = "✕";
    closeBtn.addEventListener("click", () => deps.closePane());
    head.appendChild(closeBtn);

    root.appendChild(head);

    if (cachedRows.length === 0) {
      const empty = document.createElement("p");
      empty.className = "cep-empty";
      empty.textContent =
        "All rows resolved. The tree is marked applied.";
      root.appendChild(empty);
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
    const sec = document.createElement("section");
    sec.className = "cep-group";
    const h = document.createElement("h3");
    h.textContent = `${label} (${rows.length})`;
    sec.appendChild(h);
    rows.sort((a, b) => a.target_path.localeCompare(b.target_path));
    for (const r of rows) {
      sec.appendChild(renderProposalRow(r, treeId));
    }
    return sec;
  }

  function renderProposalRow(
    row: StagingProposal,
    treeId: string,
  ): HTMLElement {
    const el = document.createElement("div");
    el.className = "cep-prow";
    const conflicted = row.state === "conflicted";
    if (conflicted) el.classList.add("cep-prow-conflicted");
    const left = document.createElement("span");
    left.className = "cep-prow-left";
    if (row.action === "move_note") {
      left.textContent = `${row.source_path ?? "?"} → ${row.target_path}`;
    } else {
      const md = (row.metadata ?? {}) as Record<string, unknown>;
      const slug = typeof md["tag_slug"] === "string" ? (md["tag_slug"] as string) : "";
      left.textContent = `${row.target_path}   + ${slug}`;
    }
    el.appendChild(left);
    if (conflicted) {
      const warn = document.createElement("span");
      warn.className = "cep-prow-warn";
      warn.textContent = `⚠ ${row.conflict_reason ?? "conflict"}`;
      el.appendChild(warn);
    }
    const acc = document.createElement("button");
    acc.type = "button";
    acc.className = "cep-prow-btn";
    acc.textContent = "✓";
    acc.title = "Accept";
    acc.disabled = conflicted;
    acc.addEventListener("click", async () => {
      try {
        await Api.staging_accept(row.id);
        await maybeFlipApplied(treeId);
        await showBatchReview(treeId);
      } catch (err) {
        showToast(`Accept failed: ${String(err)}`);
      }
    });
    el.appendChild(acc);
    const rej = document.createElement("button");
    rej.type = "button";
    rej.className = "cep-prow-btn";
    rej.textContent = "✗";
    rej.title = "Reject";
    rej.addEventListener("click", async () => {
      try {
        await rejectRow(row);
        await maybeFlipApplied(treeId);
        await showBatchReview(treeId);
      } catch (err) {
        showToast(`Reject failed: ${String(err)}`);
      }
    });
    el.appendChild(rej);
    return el;
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
  void listen<QueueEvt>("hiker:queue-event", (ev) => {
    const p = ev.payload;
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
