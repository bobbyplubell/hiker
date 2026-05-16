// status: cluster-editor-renderer-reuse
//
// Sigma + graphology adapter. The only file in the project that
// imports `sigma` / `graphology` — everything else talks to this
// module through `./types.ts`. Matches design.md's renderer-adapter
// pattern.
//
// Bundle-size note: we import `sigma` (which pulls graphology under
// the hood) and `graphology` directly. No layout plugins are imported
// here — layouts are caller-owned and assign x/y on the DTO before
// `setGraph`. This keeps the adapter bundle minimal.

import Sigma from "sigma";
import Graph from "graphology";
import type {
  CameraState,
  GraphRenderer,
  RendererCallbacks,
  RendererData,
} from "./types";
import { NodeBorderProgram } from "./nodeBorderProgram";
import { drawLabelWithBackground } from "./labelRenderer";

export function createSigmaRenderer(
  container: HTMLElement,
  callbacks: RendererCallbacks,
): GraphRenderer {
  const graph = new Graph({ multi: false, type: "directed" });
  const sigma = new Sigma(graph, container, {
    renderEdgeLabels: false,
    defaultNodeColor: "#000",
    defaultEdgeColor: "#000",
    nodeProgramClasses: { circle: NodeBorderProgram },
    // Custom label-drawing: rounded white background + thin grey outline
    // behind the node label text so labels read against any node color
    // and any backdrop (the canvas can be light or dark depending on
    // theme).
    defaultDrawNodeLabel: drawLabelWithBackground,
    minCameraRatio: 0.05,
    maxCameraRatio: 8,
    // Tauri/Vite mounts the canvas while its flex parent is still
    // laying out — sigma's resize() throws on 0-width containers
    // unless this is set. Subsequent render() calls re-run resize()
    // and pick up the real size once layout settles.
    allowInvalidContainer: true,
  });

  // Observe the container so we trigger a refresh as soon as it
  // gains real width/height after the initial 0-sized mount. We only
  // care about meaningful size changes — refreshing on every sub-pixel
  // wiggle (which can fire during pan as scrollbars come/go) would
  // stutter the camera.
  let lastW = container.offsetWidth;
  let lastH = container.offsetHeight;
  const ro = new ResizeObserver(() => {
    const w = container.offsetWidth;
    const h = container.offsetHeight;
    if (w === lastW && h === lastH) return;
    lastW = w;
    lastH = h;
    sigma.refresh();
  });
  ro.observe(container);

  // ── Event wiring ──────────────────────────────────────────────────
  if (callbacks.onNodeClick) {
    sigma.on("clickNode", (payload) => {
      const ev = payload.event as unknown as { original?: MouseEvent };
      const shift = !!(ev?.original && ev.original.shiftKey);
      callbacks.onNodeClick?.(payload.node, { shift });
    });
  }
  if (callbacks.onNodeDoubleClick) {
    sigma.on("doubleClickNode", (payload) => {
      // Suppress sigma's default doubleClick-to-zoom on a node — the
      // consumer's "open" gesture is the meaningful action here.
      payload.preventSigmaDefault();
      const ev = payload.event as unknown as { original?: MouseEvent };
      const shift = !!(ev?.original && ev.original.shiftKey);
      callbacks.onNodeDoubleClick?.(payload.node, { shift });
    });
  }
  if (callbacks.onNodeRightClick) {
    sigma.on("rightClickNode", (payload) => {
      // Suppress the browser's native context menu so the consumer
      // can paint its own.
      payload.preventSigmaDefault();
      const ev = payload.event as unknown as {
        original?: MouseEvent;
        x?: number;
        y?: number;
      };
      if (ev?.original) {
        ev.original.preventDefault?.();
      }
      const clientX = ev?.original?.clientX ?? ev?.x ?? 0;
      const clientY = ev?.original?.clientY ?? ev?.y ?? 0;
      callbacks.onNodeRightClick?.(payload.node, { clientX, clientY });
    });
  }
  if (callbacks.onBackgroundClick) {
    sigma.on("clickStage", () => callbacks.onBackgroundClick?.());
  }
  if (callbacks.onNodeHover) {
    sigma.on("enterNode", (payload) => callbacks.onNodeHover?.(payload.node));
    sigma.on("leaveNode", () => callbacks.onNodeHover?.(null));
  }
  if (callbacks.onCameraUpdate) {
    sigma.getCamera().on("updated", (s) => {
      callbacks.onCameraUpdate?.({
        x: s.x,
        y: s.y,
        ratio: s.ratio,
        angle: s.angle,
      });
    });
  }

  function setGraph(data: RendererData): void {
    // In-place diff: add/remove nodes and patch attributes so the
    // camera doesn't snap back on re-render.
    const seenNodes = new Set<string>();
    for (const n of data.nodes) {
      seenNodes.add(n.id);
      const attrs = nodeAttrs(n);
      if (graph.hasNode(n.id)) {
        graph.replaceNodeAttributes(n.id, attrs);
      } else {
        graph.addNode(n.id, attrs);
      }
    }
    for (const id of graph.nodes()) {
      if (!seenNodes.has(id)) graph.dropNode(id);
    }
    const seenEdges = new Set<string>();
    for (const e of data.edges) {
      seenEdges.add(e.id);
      // Edges may reference nodes we just dropped — skip those.
      if (!graph.hasNode(e.source) || !graph.hasNode(e.target)) continue;
      const attrs = {
        color: e.color ?? "rgba(136,136,136,0.5)",
        size: e.size ?? 1,
      };
      if (graph.hasEdge(e.id)) {
        graph.replaceEdgeAttributes(e.id, attrs);
      } else {
        graph.addEdgeWithKey(e.id, e.source, e.target, attrs);
      }
    }
    for (const id of graph.edges()) {
      if (!seenEdges.has(id)) graph.dropEdge(id);
    }
    sigma.refresh();
  }

  function nodeAttrs(n: RendererData["nodes"][number]) {
    // Sigma node attrs — color/size/label/x/y are first-class; we
    // stash outline + opacity + payload on the node for the consumer's
    // custom-reducer to read if it adds one later. For Sprint E we
    // bake outline color into a slightly enlarged "halo" by adjusting
    // the visible size where outlineColor is set; the minimal styling
    // matches the spec's "thin outline ring" requirement at the
    // adapter layer without pulling in custom WebGL programs.
    const base = {
      x: n.x,
      y: n.y,
      size: n.size,
      color: n.color,
      label: n.label ?? null,
      hidden: !!n.hidden,
      // sigma respects these on built-in node program if set, but we
      // also expose them for any custom program later.
      borderColor: n.outlineColor ?? null,
      // Filter dim: blend toward neutral when opacity < 1.
      ...(n.opacity != null && n.opacity < 1
        ? { color: dim(n.color, n.opacity) }
        : {}),
    };
    return base;
  }

  function fit(): void {
    const cam = sigma.getCamera();
    void cam.animatedReset({ duration: 200 });
  }

  function reset(): void {
    const cam = sigma.getCamera();
    cam.setState({ x: 0.5, y: 0.5, ratio: 1, angle: 0 });
  }

  function getCamera(): CameraState | null {
    const s = sigma.getCamera().getState();
    return { x: s.x, y: s.y, ratio: s.ratio, angle: s.angle };
  }

  function setCamera(c: CameraState): void {
    sigma.getCamera().setState({
      x: c.x,
      y: c.y,
      ratio: c.ratio,
      angle: c.angle ?? 0,
    });
  }

  function destroy(): void {
    ro.disconnect();
    sigma.kill();
  }

  return {
    setGraph,
    fit,
    reset,
    getCamera,
    setCamera,
    capabilities: { inPlaceUpdate: true, camera: true },
    destroy,
  };
}

/// Dim a hex/rgb color toward neutral grey by `opacity` factor. Used
/// by the policy-filter "dim non-matching" chrome — keeps structure
/// legible vs. hiding nodes outright.
function dim(color: string, opacity: number): string {
  // Convert "#rrggbb" or "rgb(...)" / "rgba(...)" into an rgba with
  // alpha = opacity. Anything we can't parse falls through unchanged.
  if (color.startsWith("#") && (color.length === 7 || color.length === 4)) {
    let r: number, g: number, b: number;
    if (color.length === 7) {
      r = parseInt(color.slice(1, 3), 16);
      g = parseInt(color.slice(3, 5), 16);
      b = parseInt(color.slice(5, 7), 16);
    } else {
      r = parseInt(color[1] + color[1], 16);
      g = parseInt(color[2] + color[2], 16);
      b = parseInt(color[3] + color[3], 16);
    }
    return `rgba(${r},${g},${b},${opacity})`;
  }
  const m = color.match(/^rgba?\(([^)]+)\)$/);
  if (m) {
    const parts = m[1].split(",").map((s) => s.trim());
    if (parts.length >= 3) {
      return `rgba(${parts[0]},${parts[1]},${parts[2]},${opacity})`;
    }
  }
  return color;
}
