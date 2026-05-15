// status: cluster-editor-renderer-reuse
//
// Shared sigma + graphology renderer adapter. Consumed by the cluster
// editor's graph view (Sprint E); designed to be reused by a future
// vault-wide graph view of notes + wikilinks.
//
// The adapter is the only file that imports `sigma` / `graphology` —
// everything else uses the surface defined in `./types.ts`. Same
// pattern as design.md's "Renderer adapter pattern" bullet.
//
// Usage: consumers should `await import("./graphRenderer")` lazily so
// the sigma bundle is paid only when a graph view actually opens
// (cluster-editor-graph-view-lazy-load + the analogous bullet on the
// future vault graph view).

export type {
  CameraState,
  CreateRenderer,
  GraphRenderer,
  RendererCallbacks,
  RendererCapabilities,
  RendererData,
  RendererEdge,
  RendererNode,
} from "./types";

export { createSigmaRenderer } from "./sigmaRenderer";
