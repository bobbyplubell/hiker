// status: cluster-editor-graph-view-layout
// status: cluster-editor-graph-view-layout-extensible
//
// Pluggable layout registry. Each layout is a pure function that
// mutates the per-node `x` / `y` of a working set of nodes. The
// renderer (`sigmaRenderer.ts`) is layout-agnostic — it reads
// whatever x/y the layout assigned.
//
// Adding a layout = drop a function below + add it to `LAYOUTS`.

import type { ClusterNodeRow } from "../../clusterEditor";

export interface GraphNodeInput {
  row: ClusterNodeRow;
  /// Parent node id, or null for roots / disconnected (outlier).
  parent: string | null;
  /// True for the synthetic outlier-bucket node — laid out separately.
  isOutlier: boolean;
  /// Number of leaf descendants.
  memberCount: number;
}

export interface LaidOutNode {
  id: string;
  x: number;
  y: number;
}

export interface GraphLayout {
  id: string;
  label: string;
  assignPositions(nodes: GraphNodeInput[]): Map<string, { x: number; y: number }>;
}

// ── Tree structure helpers ──────────────────────────────────────────

interface TreeNode {
  input: GraphNodeInput;
  children: TreeNode[];
  depth: number;
}

function buildForest(nodes: GraphNodeInput[]): {
  roots: TreeNode[];
  outliers: TreeNode[];
} {
  const byId = new Map<string, TreeNode>();
  for (const n of nodes) {
    byId.set(n.row.id, { input: n, children: [], depth: 0 });
  }
  const roots: TreeNode[] = [];
  const outliers: TreeNode[] = [];
  for (const tn of byId.values()) {
    if (tn.input.isOutlier) {
      outliers.push(tn);
      continue;
    }
    const pid = tn.input.parent;
    if (pid && byId.has(pid)) {
      const parent = byId.get(pid)!;
      // Skip parenting under the outlier bucket — outliers render
      // disconnected per `cluster-editor-graph-view-outlier-disconnected`.
      if (parent.input.isOutlier) {
        outliers.push(tn);
      } else {
        parent.children.push(tn);
      }
    } else {
      roots.push(tn);
    }
  }
  // Compute depth.
  const stack: Array<{ n: TreeNode; d: number }> = roots.map((r) => ({
    n: r,
    d: 0,
  }));
  while (stack.length) {
    const { n, d } = stack.pop()!;
    n.depth = d;
    for (const c of n.children) stack.push({ n: c, d: d + 1 });
  }
  return { roots, outliers };
}

function maxDepth(nodes: TreeNode[]): number {
  let max = 0;
  const stack = [...nodes];
  while (stack.length) {
    const n = stack.pop()!;
    if (n.depth > max) max = n.depth;
    for (const c of n.children) stack.push(c);
  }
  return max;
}

// ── Radial layout (default) ─────────────────────────────────────────

function radialLayout(
  nodes: GraphNodeInput[],
): Map<string, { x: number; y: number }> {
  const { roots, outliers } = buildForest(nodes);
  const out = new Map<string, { x: number; y: number }>();
  const depth = Math.max(1, maxDepth(roots));
  // Single synthetic super-root if there are multiple roots — splits
  // the angular sweep evenly.
  const rootSlices = roots.length > 0 ? roots : [];
  let nextAngle = 0;
  const angleStep = (Math.PI * 2) / Math.max(1, rootSlices.length);
  for (let i = 0; i < rootSlices.length; i++) {
    const start = i * angleStep;
    const end = start + angleStep;
    layoutSubtree(rootSlices[i], start, end, depth, out);
  }
  if (rootSlices.length === 1) {
    // Put the single root at origin instead of pinned to a ray.
    const r = rootSlices[0];
    out.set(r.input.row.id, { x: 0, y: 0 });
  }
  // Outliers float in the lower-right.
  layoutOutliers(outliers, out);
  // Suppress unused-warning for nextAngle (kept for readability).
  void nextAngle;
  return out;
}

function layoutSubtree(
  node: TreeNode,
  angleStart: number,
  angleEnd: number,
  maxD: number,
  out: Map<string, { x: number; y: number }>,
): void {
  const angle = (angleStart + angleEnd) / 2;
  const radius = node.depth / Math.max(1, maxD);
  out.set(node.input.row.id, {
    x: Math.cos(angle) * radius,
    y: Math.sin(angle) * radius,
  });
  if (node.children.length === 0) return;
  // Weight each child's sweep by its leaf-count so denser subtrees get
  // more angular room.
  const weights = node.children.map((c) => Math.max(1, c.input.memberCount));
  const total = weights.reduce((a, b) => a + b, 0);
  let cursor = angleStart;
  for (let i = 0; i < node.children.length; i++) {
    const span = ((angleEnd - angleStart) * weights[i]) / total;
    layoutSubtree(node.children[i], cursor, cursor + span, maxD, out);
    cursor += span;
  }
}

function layoutOutliers(
  outliers: TreeNode[],
  out: Map<string, { x: number; y: number }>,
): void {
  // Place the outlier bucket at (1.3, -1.3) and any disconnected
  // leaves in a small cluster near it.
  let i = 0;
  for (const o of outliers) {
    out.set(o.input.row.id, {
      x: 1.3 + (i % 4) * 0.08,
      y: -1.3 - Math.floor(i / 4) * 0.08,
    });
    i += 1;
  }
}

// ── Vertical tree ───────────────────────────────────────────────────

function verticalTreeLayout(
  nodes: GraphNodeInput[],
): Map<string, { x: number; y: number }> {
  const { roots, outliers } = buildForest(nodes);
  const out = new Map<string, { x: number; y: number }>();
  let xCursor = 0;
  const xStep = 0.05;
  for (const r of roots) {
    xCursor = layoutVertical(r, xCursor, xStep, out);
  }
  layoutOutliers(outliers, out);
  return out;
}

function layoutVertical(
  node: TreeNode,
  xCursor: number,
  xStep: number,
  out: Map<string, { x: number; y: number }>,
): number {
  if (node.children.length === 0) {
    out.set(node.input.row.id, { x: xCursor, y: -node.depth * 0.2 });
    return xCursor + xStep;
  }
  const startX = xCursor;
  let cur = xCursor;
  for (const c of node.children) {
    cur = layoutVertical(c, cur, xStep, out);
  }
  const endX = cur - xStep;
  const mid = (startX + endX) / 2;
  out.set(node.input.row.id, { x: mid, y: -node.depth * 0.2 });
  return cur;
}

// ── Horizontal tree ─────────────────────────────────────────────────

function horizontalTreeLayout(
  nodes: GraphNodeInput[],
): Map<string, { x: number; y: number }> {
  const vert = verticalTreeLayout(nodes);
  const out = new Map<string, { x: number; y: number }>();
  for (const [id, p] of vert) {
    // 90° rotate: (x,y) -> (-y, x). Visually this lays the tree out
    // left-to-right with the root on the left.
    out.set(id, { x: -p.y, y: p.x });
  }
  return out;
}

// ── Force-directed (simple, layout-pkg-free) ────────────────────────
//
// We deliberately do NOT pull in `graphology-layout-forceatlas2` to
// keep the bundle small (see the bundle-size constraint). The
// force-directed alternative below is a tiny in-line spring-embedder
// — good enough for the "alternative layout" affordance without
// bloating the JS bundle. If users want a beefier FA2, we add it
// later behind another feature flag.

function forceLayout(
  nodes: GraphNodeInput[],
): Map<string, { x: number; y: number }> {
  // Start from the radial layout so we converge fast.
  const positions = radialLayout(nodes);
  const ids = nodes.map((n) => n.row.id);
  // Build edge list (parent -> child).
  const edges: Array<[string, string]> = [];
  for (const n of nodes) {
    if (n.parent && positions.has(n.parent) && !n.isOutlier) {
      edges.push([n.parent, n.row.id]);
    }
  }
  const iters = 80;
  const k = 0.05; // ideal edge length
  for (let step = 0; step < iters; step++) {
    const forces = new Map<string, { fx: number; fy: number }>();
    for (const id of ids) forces.set(id, { fx: 0, fy: 0 });
    // Repulsion (O(n^2) — fine for cluster trees of low-hundreds).
    for (let i = 0; i < ids.length; i++) {
      for (let j = i + 1; j < ids.length; j++) {
        const pa = positions.get(ids[i])!;
        const pb = positions.get(ids[j])!;
        const dx = pa.x - pb.x;
        const dy = pa.y - pb.y;
        const d2 = dx * dx + dy * dy + 0.001;
        const force = (k * k) / d2;
        const ux = dx / Math.sqrt(d2);
        const uy = dy / Math.sqrt(d2);
        const fa = forces.get(ids[i])!;
        const fb = forces.get(ids[j])!;
        fa.fx += ux * force;
        fa.fy += uy * force;
        fb.fx -= ux * force;
        fb.fy -= uy * force;
      }
    }
    // Attraction along edges.
    for (const [a, b] of edges) {
      const pa = positions.get(a)!;
      const pb = positions.get(b)!;
      const dx = pb.x - pa.x;
      const dy = pb.y - pa.y;
      const d = Math.sqrt(dx * dx + dy * dy) + 0.001;
      const force = (d * d) / k;
      const ux = dx / d;
      const uy = dy / d;
      const fa = forces.get(a)!;
      const fb = forces.get(b)!;
      fa.fx += ux * force * 0.5;
      fa.fy += uy * force * 0.5;
      fb.fx -= ux * force * 0.5;
      fb.fy -= uy * force * 0.5;
    }
    // Apply with decaying step.
    const temp = 0.05 * (1 - step / iters);
    for (const id of ids) {
      const p = positions.get(id)!;
      const f = forces.get(id)!;
      const mag = Math.sqrt(f.fx * f.fx + f.fy * f.fy) + 0.001;
      const dx = (f.fx / mag) * Math.min(mag, temp);
      const dy = (f.fy / mag) * Math.min(mag, temp);
      positions.set(id, { x: p.x + dx, y: p.y + dy });
    }
  }
  return positions;
}

// ── Registry ────────────────────────────────────────────────────────

export const LAYOUTS: Record<string, GraphLayout> = {
  radial: {
    id: "radial",
    label: "Radial (default)",
    assignPositions: radialLayout,
  },
  "vertical-tree": {
    id: "vertical-tree",
    label: "Vertical tree",
    assignPositions: verticalTreeLayout,
  },
  "horizontal-tree": {
    id: "horizontal-tree",
    label: "Horizontal tree",
    assignPositions: horizontalTreeLayout,
  },
  "force-directed": {
    id: "force-directed",
    label: "Force-directed",
    assignPositions: forceLayout,
  },
};

export const DEFAULT_LAYOUT_ID = "radial";
