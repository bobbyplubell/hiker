# Canvas export

A one-way export that snapshots a **trail** or a **cluster tree** into a new `.canvas` document. The exported canvas is an ordinary JSON Canvas file (`canvas.md`) — it opens in the canvas view, edits ride the op-log, it syncs — and from the moment it lands it is independent of the source it came from. The action is a right-click **Export to canvas** verb on a trail or a tree.

The builder lives in `core::canvas`, on top of the egui-free `hiker-canvas` model: it walks a trail's waypoint tree or a `ClusterTree` and emits a `hiker_canvas::Canvas`, serialized through [[spec:canvas-canonical-json]]. It needs hiker domain types so it stays in `core`, keeping `hiker-canvas` hiker-agnostic (the extractable-as-a-standalone-repo property from [[spec:canvas-crate-split]]); generic domain-free layout helpers (layered placement, box-packing) may live in `hiker-canvas` geometry, the trail/tree→node mapping stays in `core`. [canvas-export-builder]
status:: planned
touches:: [[code:hiker/canvas/export]]
note:: `core::canvas` builder: walks a trail's waypoint tree or a `ClusterTree`, emits a `hiker_canvas::Canvas` via [[spec:canvas-canonical-json]]; `hiker-canvas` stays hiker-agnostic, generic layout helpers may live in its geometry

Export only — canvas→trail / canvas→tree are out of scope (a canvas has no inherent traversal order and no embeddings).


## What gets built

The builder is a pure function from a source structure to a `hiker_canvas::Canvas`; it reads the source, never mutates it, and emits canonical JSON the output file is seeded with. It is a **snapshot, not a synced projection**: the *structure* (which waypoints/clusters, order, nesting) is frozen at export time — later trail/tree edits never propagate and canvas edits never write back; only content stays live, because file nodes are *pointers* ([[spec:canvas-insert-from-vault]]). Re-exporting makes a new file; there is no back-sync to the source trail/tree. [canvas-export-snapshot]
status:: planned
touches:: [[code:hiker/canvas/export]]

### Trail → canvas

[canvas-export-trail]
status:: planned
implements:: [[code:hiker/canvas/export/trail_to_canvas]]
touches:: [[code:hiker/canvas/export]]
note:: trail → canvas: waypoint → `File` node @ waypoint-note, parent→child link → arrow edge, depth-first layered layout (main line chain + side-trail branches)

- **One `File` node per waypoint**, `file` = the waypoint-note's vault path, so the card renders the user's annotation live (an empty annotation renders an empty card, [[spec:trail-empty-waypoint-body]]). The note's frontmatter still records its source note, one hop away.
- **One edge per parent→child link** in the `hiker.waypoints` tree ([[spec:trail-side-trail-shape]]), `from_end = none`, `to_end = arrow` — the main line is a chain of arrows, a side trail a branch off its parent.
- **Layout** is a depth-first walk: the main line in trail order (`tree_path`, [[spec:trail-waypoints-derived-table]]), each side trail offset perpendicular from its parent. Fixed default node sizes; the user rearranges after export. Domain-free, unit-testable.

The source note as a *second* node per waypoint is a deferred enrichment ([[spec:canvas-export-trail-source-card]]); v1 is one card per waypoint.

### Cluster tree → canvas

A cluster tree exports in one of two styles, chosen at the verb. Both map a leaf to a `File` node pointing at its note ([[spec:tree-leaf-path-ref]]; freeform/text leaves → `Text` nodes, broken leaves → broken-reference cards, [[spec:canvas-file-node-embed]]) and both **drop** centroids, per-node policies, confidence, and staleness — that automation/identity machinery has no place on a presentation snapshot.

**Grouped (default).** [canvas-export-tree]
status:: done
implements:: [[code:hiker/canvas/export/tree_to_canvas]]
touches:: [[code:hiker/canvas/export]]
note:: cluster tree → canvas, Grouped style: cluster → `Group` (label = name) + summary text node, leaf → `File` node, hierarchy = spatial nesting (no edges), bottom-up box-packing; centroids/policies/confidence dropped · evidence: `core/src/canvas/export.rs:200` `tree_to_canvas_grouped`

- **One `Group` node per cluster**, `label` = the cluster's name; the one-line summary becomes a small `Text` node near its label (omitted when empty). The outlier bucket exports as a group like any cluster.
- **Hierarchy is spatial, not edge-drawn.** Child groups and leaf cards lay out inside their parent group's rect, so nesting *is* the structure — no edges. Group membership is "contained by the group's rect" ([[spec:canvas-group-move]]), so the export behaves like hand-drawn groups: dragging a parent moves its members.
- **Layout** is a recursive bottom-up box-packing: leaves and child-group rects flow in a grid, each cluster's group rect sized to contain its children plus label/summary padding, parents sized to fit. Computed without a UI.

**Force-directed.** [canvas-export-tree-force]
status:: done
implements:: [[code:hiker/canvas/export/TreeCanvasStyle]], [[code:hiker/canvas/export/tree_to_canvas_force]]
touches:: [[code:hiker/canvas/export]]
note:: cluster tree → canvas, Force-directed style: cluster → labeled `Text` node, leaf → `File` node, parent→child `Edge` connectors, hand-rolled deterministic Fruchterman-Reingold layout (k=360, 300 iters, circle seed); chosen at the export-verb menu · evidence: `core/src/canvas/export.rs:431` `tree_to_canvas_force`

- **Every cluster and leaf is a node**: a cluster becomes a small labeled `Text` node (its name), a leaf its `File` pointer. No groups.
- **One `Edge` per parent→child link** (cluster→child cluster, cluster→leaf) — the line connectors that make the result read as an organic node-link cluster, not a box hierarchy. Edges are undirected-looking (`to_end = none`) since the tree's containment isn't a reading order.
- **Layout** is a deterministic Fruchterman-Reingold-style force pass (circle-seeded initial placement, fixed iteration count) so the same tree always yields the same canvas — hand-rolled in the export module, no new `core` dep (the existing `hiker-graph` layout may be shared later).


## The output document

[canvas-export-output]
status:: planned
implements:: [[code:hiker/canvas/export/write_trail_canvas]], [[code:hiker/canvas/export/write_tree_canvas]]
touches:: [[code:hiker/canvas/export]]
note:: writes the new `.canvas` via `create_at` ([[spec:canvas-create]] shape) seeded with canonical JSON, opens framed-to-fit; name derived + suffix-counted; `[canvas] export_dir` config

Export writes a new `.canvas` file through the normal create path (`core::ops::file::create_at`, the [[spec:canvas-create]] shape) seeded with the builder's canonical JSON, then opens it in the canvas view framed to fit ([[spec:canvas-pan-zoom]] zoom-to-fit).

- **Name** is derived from the source — `<trail-basename>.canvas` / `<tree-name>.canvas` — suffix-counted to avoid collision (`create_with_suffix`), so repeated exports of the same source produce `…-2.canvas`, `…-3.canvas` rather than clobbering.
- **Location** defaults to the source's own folder for a trail (beside the trail-doc); for a cluster tree the source lives under `.hiker/trees/` (hidden), so a tree export lands at vault root. A `[canvas] export_dir` config (vault-scope eligible, default empty = the rule above) overrides the destination.
- The output is an ordinary canvas doc from then on (op-log document, syncs, versions, file-node paths rewrite on note rename via [[spec:canvas-file-ref-rewrite]]). Concretely the snapshot tracks *renames of the notes it points at* but not *structural changes to the source trail/tree*.


## Action surfaces

- **Trail.** The trail-doc's file-tree row context menu gains an **Export to canvas** entry, alongside [[spec:trail-set-as-active-context-verb]]. The Trails sidebar mode header also exposes it for the active trail. Hidden on non-trail rows. [canvas-export-trail-verb]
status:: planned
implements:: [[code:hiker/files/sidebar/set_active_trail]]
note:: right-click "Export to canvas" on a trail-doc row (file tree) + Trails-mode header action for the active trail
- **Cluster tree.** The cluster editor pane toolbar's **Export to canvas** control is a menu offering the two styles — **Grouped** ([[spec:canvas-export-tree]]) and **Force-directed** ([[spec:canvas-export-tree-force]]) — and exports the tree in its current edited state (the in-memory outline), not a stale on-disk copy. [canvas-export-tree-verb]
status:: done
implements:: [[code:hiker/clusters/export_tree_to_canvas]], [[code:hiker/clusters/sidebar/tree/impl#[`ClusterCtx<'_, '_>`]toolbar]]
note:: cluster editor pane toolbar "Export to canvas" menu (Grouped / Force-directed), exports current edited state; sidebar tree-row right-click entry still pending · evidence: `app/src/clusters/sidebar/tree.rs:825` menu_button; `app/src/clusters/mod.rs:59` `export_tree_to_canvas`


## Out of scope

- **Canvas → trail.** A canvas is a spatial DAG with no inherent traversal order, and `Text`/`Link` nodes have no source note to become waypoints. Not built.
- **Canvas → cluster tree.** A canvas has no embeddings/centroids, so a canvas-shaped tree would have no semantic backing and bypass the clustering pipeline. Not built.
- **Live re-sync / "update existing canvas."** Export is a one-shot snapshot; re-export makes a new file. A canvas that stays in lockstep with its source trail/tree is a different feature (a live projection), deliberately not this one.
- **Exporting policies/centroids onto the canvas.** The canvas is presentation; the tree owns automation. Not represented.


## Deferred

- **Source-note card per waypoint.** A second `File` node per waypoint pointing at the *source* note, linked to its annotation card, so a trail export shows both the source content and the commentary. The v1 mapping is one card (the waypoint-note) per waypoint. [canvas-export-trail-source-card]
status:: planned
note:: deferred: a second `File` node per waypoint pointing at the source note, linked to its annotation card
- **Export a subtree.** Right-click a single cluster (not the whole tree) → export just that subtree as a canvas. Rides the same builder against a `node_id` root. [canvas-export-subtree]
status:: planned
note:: deferred: right-click a single cluster → export just that subtree, same builder against a `node_id` root
- **MCP export verb.** A headless `canvas_export(source)` tool so an agent can snapshot a trail/tree it assembled, riding the deferred [[spec:canvas-mcp-tools]] surface. [canvas-export-mcp]
status:: planned
note:: deferred: headless `canvas_export(source)` MCP tool, riding the [[spec:canvas-mcp-tools]] surface
