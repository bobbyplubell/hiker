// status: cluster-editor-renderer-reuse
//
// Shared sigma+graphology renderer adapter. Two consumers planned:
//   - cluster editor's tree-shaped graph view (this sprint).
//   - a future vault-wide graph view of notes + wikilinks.
//
// The adapter is the only file that imports `sigma` / `graphology` —
// callers see a small, surface-agnostic API. Module-discipline
// matches the `Embedder` / `LlmClient` / `LexicalEngine` pattern from
// design.md ("Renderer adapter pattern").

/// Plain DTO for nodes. Consumer-specific fields ride `data`.
export interface RendererNode {
  id: string;
  /// Layout-assigned position. Layouts run in caller code and mutate
  /// the node objects in-place before `mount` / `update`.
  x: number;
  y: number;
  /// Display size in pixels (rendered at zoom = 1). Caller computes
  /// from member-count / log-scale etc.
  size: number;
  /// CSS color string. Caller composes from policy + tint.
  color: string;
  /// Optional label drawn next to the node.
  label?: string;
  /// Optional outline ring color (used for selection highlight). When
  /// unset, no ring is drawn.
  outlineColor?: string;
  /// Opacity 0..1 — used by the policy-filter "dim non-matching"
  /// chrome. Default 1.
  opacity?: number;
  /// Whether the node is visible in the current view. Filtered-out
  /// nodes are dropped from the graph entirely (consumer can choose
  /// between dropping and dimming via `opacity`).
  hidden?: boolean;
  /// Free-form per-consumer payload. Returned in hover/click events
  /// so the consumer can look up its own data without a side index.
  data?: unknown;
}

export interface RendererEdge {
  id: string;
  source: string;
  target: string;
  /// CSS color string; default is a muted edge color.
  color?: string;
  /// Edge thickness in pixels at zoom = 1.
  size?: number;
}

export interface RendererData {
  nodes: RendererNode[];
  edges: RendererEdge[];
}

export interface RendererCallbacks {
  onNodeClick?: (id: string, ev: { shift: boolean }) => void;
  /// Fires when a node is double-clicked. Consumers wire this for
  /// "open the underlying thing" gestures (open a note from a leaf
  /// node, etc.). Sigma's default doubleClick-to-zoom is suppressed
  /// when this fires so the gesture doesn't dual-fire.
  onNodeDoubleClick?: (id: string, ev: { shift: boolean }) => void;
  onNodeHover?: (id: string | null) => void;
  /// Background (canvas) click, no node hit. Useful for "deselect on
  /// blank click".
  onBackgroundClick?: () => void;
  /// Camera pan/zoom changed. Fires for every interaction frame, so
  /// consumers should debounce expensive work themselves.
  onCameraUpdate?: (c: CameraState) => void;
}

export interface RendererCapabilities {
  /// True iff the underlying renderer can patch nodes/edges without
  /// remounting the whole canvas. Sigma supports this.
  inPlaceUpdate: boolean;
  /// True iff the renderer reports camera (pan/zoom) state to the
  /// caller via `getCamera` / `setCamera`. Sigma supports this.
  camera: boolean;
}

export interface CameraState {
  x: number;
  y: number;
  ratio: number;
  angle?: number;
}

export interface GraphRenderer {
  /// Replace the rendered graph with `data`. First call mounts the
  /// canvas; subsequent calls patch in-place when `capabilities
  /// .inPlaceUpdate` is true.
  setGraph(data: RendererData): void;
  /// Reset zoom + pan to fit the current graph in the viewport.
  fit(): void;
  /// Restore the layout's default zoom + pan.
  reset(): void;
  getCamera(): CameraState | null;
  setCamera(c: CameraState): void;
  capabilities: RendererCapabilities;
  /// Tear down the canvas + listeners. Caller should null out the
  /// returned reference.
  destroy(): void;
}

export type CreateRenderer = (
  container: HTMLElement,
  callbacks: RendererCallbacks,
) => GraphRenderer;
