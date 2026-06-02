# Canvas export

A one-way export that snapshots a **trail** or a **cluster tree** into a new `.canvas` document. The exported canvas is an ordinary JSON Canvas file (`canvas.md`) — it opens in the canvas view, edits ride the op-log, it syncs — and from the moment it lands it is independent of the source it came from. The action is a right-click **Export to canvas** verb on a trail or a tree.

The headline decisions:

- **Snapshot, not a synced projection.** Export reads the source structure once and writes a fresh `.canvas` file. The *structure* (which waypoints/clusters, their order and nesting) is a frozen copy — later edits to the trail or tree never propagate to the canvas, and canvas edits never write back. Content stays live only because file nodes are *pointers* (`canvas-insert-from-vault`): a node renders the referenced note's current body, but the set of nodes and where they sit is fixed at export time. Re-exporting produces a new file; there is no "update the existing canvas" link to maintain. [canvas-export-snapshot]
- **The builder lives in `core`, on top of the egui-free `hiker-canvas` model.** `core::canvas` gains a builder that walks a trail's waypoint tree or a `ClusterTree` and emits a `hiker_canvas::Canvas`, serialized through `canvas-canonical-json`. The builder needs hiker domain types (trails, cluster trees), so it stays in `core`; `hiker-canvas` keeps its hiker-agnostic posture (the extractable-as-a-standalone-repo property from `canvas-crate-split`). Generic, domain-free layout helpers (layered placement, box-packing) may live in `hiker-canvas` geometry; the trail/tree→node mapping stays in `core`. [canvas-export-builder]
- **Trail → a connected chain of waypoint cards.** Each waypoint becomes a `File` node pointing at the *waypoint-note* (so the user's annotation renders live), and each parent→child link becomes an edge. A depth-first layered layout draws the main line as a chain with side trails branching off, mirroring the Bush memex shape the trail already encodes. [canvas-export-trail]
- **Cluster tree → one of two styles.** **Grouped** (the default): each cluster becomes a `Group` node (label = cluster name), each leaf a `File` node, and the hierarchy is *spatial nesting* — child groups and leaf cards inside their parent group's rect, no edges. **Force-directed**: clusters and leaves all become nodes connected by `Edge` connectors (parent→child), positioned by a force-directed layout into an organic node-link cluster shape. The export verb offers both. Either way centroids, policies, and confidence are dropped — a canvas is a presentation snapshot, not the tree's automation program. [canvas-export-tree, canvas-export-tree-force]
- **Right-click the source.** A trail-doc's context menu and a cluster-tree row's context menu each gain **Export to canvas**; the cluster editor pane also surfaces it as a toolbar button. The new canvas opens framed-to-fit in the canvas view. [canvas-export-trail-verb, canvas-export-tree-verb, canvas-export-output]

This is export only. Canvas→trail and canvas→tree are out of scope (see below): a canvas has no inherent traversal order and no embeddings, so neither reverse mapping is meaningful.


## What gets built

The builder is a pure function from a source structure to a `hiker_canvas::Canvas`; it reads the source, never mutates it, and emits canonical JSON the output file is seeded with. [canvas-export-builder]

### Trail → canvas

[canvas-export-trail]

- **One `File` node per waypoint**, with `file` set to the waypoint-note's vault path. The waypoint-note's body is the user's annotation, so the card renders the annotation live (an empty annotation renders an empty card — matching the trail's own "clean canvas until written" shape, `trail-empty-waypoint-body`). The waypoint-note's frontmatter still records its source note, so the source is one hop away even though it isn't a node in v1.
- **One edge per parent→child link** in the `hiker.waypoints` tree (`trail-side-trail-shape`), `from_end = none`, `to_end = arrow` — the reading direction. The main line is a chain of arrows; a side trail is a branch off its parent.
- **Layout** is a depth-first walk of the waypoint tree: the main line lays out along one axis in trail order (the `tree_path` order, `trail-waypoints-derived-table`), and each side trail offsets perpendicular from its parent so digressions read as branches. Node sizes are a fixed default; the user rearranges freely after export. Layout math is domain-free and unit-testable without a UI.

The source note as a *second* node per waypoint (a source card linked to its annotation card) is a deferred enrichment, not v1 — v1 is the one-card-per-waypoint mapping the user asked for.

### Cluster tree → canvas

A cluster tree exports in one of two styles, chosen at the verb. Both map a leaf to a `File` node pointing at its note (`tree-leaf-path-ref`; freeform/text leaves → `Text` nodes, broken leaves → file pointers that render as the canvas broken-reference card, `canvas-file-node-embed`) and both **drop** centroids, per-node policies, confidence, and staleness — that automation/identity machinery (`cluster-editor-policy-any-level`, `trees-centroids-index`) has no place on a presentation snapshot; the tree stays its source of truth.

**Grouped (default).** [canvas-export-tree]

- **One `Group` node per cluster**, `label` = the cluster's name; the one-line summary becomes a small `Text` node near its label (omitted when empty). The outlier bucket exports as a group like any cluster.
- **Hierarchy is spatial, not edge-drawn.** Child groups and leaf cards lay out inside their parent group's rect, so nesting *is* the structure — no edges. Group membership is "contained by the group's rect" (`canvas-group-move`), so the export behaves like hand-drawn groups: dragging a parent moves its members.
- **Layout** is a recursive bottom-up box-packing: leaves and child-group rects flow in a grid, each cluster's group rect sized to contain its children plus label/summary padding, parents sized to fit. Computed without a UI.

**Force-directed.** [canvas-export-tree-force]

- **Every cluster and leaf is a node**: a cluster becomes a small labeled `Text` node (its name), a leaf its `File` pointer. No groups.
- **One `Edge` per parent→child link** (cluster→child cluster, cluster→leaf) — the line connectors that make the result read as an organic node-link cluster, not a box hierarchy. Edges are undirected-looking (`to_end = none`) since the tree's containment isn't a reading order.
- **Layout** is a force-directed pass over the node-link graph (spring attraction along edges, repulsion between nodes), run in `core` as a compact deterministic Fruchterman-Reingold-style relaxation — deterministic initial placement (e.g. nodes seeded on a circle by index) and a fixed iteration count, so the same tree always yields the same canvas (no randomness, unit-testable). Tight clusters pull together; the whole tree spreads into a readable graph. Hand-rolled in the export module (no new `core` dependency); the existing `hiker-graph` layout may be reused later if the two should share an engine.


## The output document

[canvas-export-output]

Export writes a new `.canvas` file through the normal create path (`core::ops::file::create_at`, the `canvas-create` shape) seeded with the builder's canonical JSON, then opens it in the canvas view framed to fit (`canvas-pan-zoom` zoom-to-fit).

- **Name** is derived from the source — `<trail-basename>.canvas` / `<tree-name>.canvas` — suffix-counted to avoid collision (`create_with_suffix`), so repeated exports of the same source produce `…-2.canvas`, `…-3.canvas` rather than clobbering.
- **Location** defaults to the source's own folder for a trail (beside the trail-doc); for a cluster tree the source lives under `.hiker/trees/` (hidden), so a tree export lands at vault root. A `[canvas] export_dir` config (vault-scope eligible, default empty = the rule above) overrides the destination.
- The output is an ordinary canvas doc from then on: it is an op-log document (`canvas-doc-kind`), syncs, versions, and its file-node paths rewrite on note rename through `canvas-file-ref-rewrite` — exactly like a hand-authored canvas. This is what "snapshot, not synced" means concretely: the canvas tracks *renames of the notes it points at* (because every canvas does), but not *structural changes to the trail/tree it was built from*.


## Action surfaces

- **Trail.** The trail-doc's file-tree row context menu gains an **Export to canvas** entry, alongside `trail-set-as-active-context-verb`. The Trails sidebar mode header also exposes it for the active trail. Hidden on non-trail rows. [canvas-export-trail-verb]
- **Cluster tree.** The cluster editor pane toolbar's **Export to canvas** control is a menu offering the two styles — **Grouped** (`canvas-export-tree`) and **Force-directed** (`canvas-export-tree-force`) — and exports the tree in its current edited state (the in-memory outline), not a stale on-disk copy. [canvas-export-tree-verb]


## Out of scope

- **Canvas → trail.** A canvas is a spatial DAG with no inherent traversal order; deriving a strict waypoint tree would require cycle-breaking and a root-and-order heuristic, and `Text`/`Link` nodes have no source note to become waypoints. Not useful enough to build.
- **Canvas → cluster tree.** Cluster trees are semantic artifacts built from embeddings and carrying centroids/policies; a canvas has none, so a canvas-shaped tree would have no semantic backing and would bypass the clustering pipeline entirely. Explicitly not built.
- **Live re-sync / "update existing canvas."** Export is a one-shot snapshot; re-export makes a new file. A canvas that stays in lockstep with its source trail/tree is a different feature (a live projection), deliberately not this one.
- **Exporting policies/centroids onto the canvas.** The canvas is presentation; the tree owns automation. Not represented.


## Deferred

- **Source-note card per waypoint.** A second `File` node per waypoint pointing at the *source* note, linked to its annotation card, so a trail export shows both the source content and the commentary. The v1 mapping is one card (the waypoint-note) per waypoint. [canvas-export-trail-source-card]
- **Export a subtree.** Right-click a single cluster (not the whole tree) → export just that subtree as a canvas. Rides the same builder against a `node_id` root. [canvas-export-subtree]
- **MCP export verb.** A headless `canvas_export(source)` tool so an agent can snapshot a trail/tree it assembled, riding the deferred `canvas-mcp-tools` surface. [canvas-export-mcp]
