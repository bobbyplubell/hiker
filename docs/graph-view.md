# Graph view

One spatial graph engine drives every node-link surface in hiker: the vault-wide `Graph` tab (`design.md` App-shell) and the cluster / cluster-review graphs. The engine (`hiker-graph-view` crate, `widgets/graph-view/`) owns pan/zoom, layout, the eye-icon view menu, and the node/edge/label/hover/preview paint loop; each caller plugs its own data in through a `Source` trait. The layout math is egui-free in `hiker-graph` (`hiker-render/graph/`), wrapped for egui by the `graph-widgets` façade (`widgets/graph-widgets/`).

The headline decisions:

- **One engine, many sources.** `graph_view::State` carries all view state and the paint loop; a caller implements `Source` to turn its domain data (a `petgraph` vault graph, a slice of cluster `EditableNode`s) into per-frame node descriptors, edge pairs, and a layout tree. One code path renders both views with different colors and options. [graph-view-source-trait]
- **Layout math is egui-free.** Force-directed (ForceAtlas2 + Barnes–Hut worker), the radial/vertical/horizontal tree layouts, and the layered (dagre/Sugiyama) port all live in `hiker-graph` with their own `Vec2`; `graph-widgets` is the thin egui façade converting at the boundary. [graph-layout-egui-free]
- **Re-clustering and vault rebuilds morph, they don't reshuffle.** Force layouts carry stable node identity across rebuilds: a retained node keeps and is tethered to its prior position by an anchor spring, so the layout settles into the same shape with the changed parts moved, instead of scattering fresh every time. [force-node-identity, force-anchor-springs]
- **Display-engine controls live in the view/eye menu.** Layout kind, projection mode and config, node/edge/label sizes, and anchor stiffness are surfaced from the surface's eye-icon menu; clustering options stay confined to the clustering engine's own params. [view-menu-display-controls]
- **Projection modes are a lens over the same layout.** Fisheye and Poincaré-disk navigation apply to every graph consumer at once; the seam and its controls are specced in `projection.md`. [proj-graph-mode]


## The Source seam

`Source` is the caller-supplied bridge from domain data to the engine (`graph_view.rs`). The engine never knows whether it's drawing the vault link graph or a cluster tree. [graph-view-source-trait]

| Method | Purpose |
| ------ | ------- |
| `node_count` | total nodes (length of the positions vector); includes hidden nodes so edge/layout indices stay stable |
| `nodes(positions, style)` | visible node descriptors for the frame; reads `positions[i]` per node |
| `edges` | edges as `positions`-index pairs — used both for drawing and as the force-worker topology |
| `layout_tree(kind)` | spanning/parent tree for the tree layouts (vault graph spans per kind; cluster graph uses its parent tree) |
| `preview_for(index)` | `(title, body)` for the hover-preview card |
| `node_key(index)` | stable per-node identity for temporal stability (below); default `None` opts out |

The vault graph (`app/src/panels/graph.rs`) and the cluster graph (`app/src/panels/cluster_graph.rs`) each implement it over their own storage.


## Layout kinds

Selected from the view menu; switching kind triggers a relayout (`recompute_layout`).

- **Force-directed** — ForceAtlas2 over a Barnes–Hut quadtree, run on a background worker (`hiker-graph::LayoutWorker`) so the layout settles live without blocking the UI; positions are snapshotted into the view each frame until convergence. [graph-force-layout]
- **Radial / vertical / horizontal tree** — pure deterministic tree layouts over the `Source`'s `layout_tree`, computed inline (no worker). [graph-tree-layouts]
- **Layered (dagre)** — the Sugiyama/dagre port (`hiker-graph::LayeredEngine`, `layered/`), via the `graph-widgets::layered_layout` façade. Treated as directed; ranks flow per the view menu's rank-direction selector. [graph-layered-layout]
  - **Routed edges.** The layered layout is the only kind that returns poly-line edge routes (orthogonal routing between ranks); the engine stores them in `State::edge_routes` and the paint loop draws them as routes. Every other kind clears the routes and draws edges as straight segments. [graph-routed-edges]


## Temporal layout stability

Force layouts are intrinsically chaotic — a small change to the graph re-randomizes the whole picture. The engine keeps the force layout *stable across rebuilds* so a re-cluster or vault-graph rebuild reads as the same shape with the changed parts moved, not a fresh scatter.

- **Stable node identity.** `Source::node_key(index)` returns a key that survives a rebuild. Each frame the engine captures the live positions into `prev_positions` keyed by that identity, so the next rebuild can map old positions onto the new graph. [force-node-identity]
- **Warm seed + anchor springs.** On a same-kind force rebuild that has captured history, the engine builds a warm seed (`build_warm_seed`): a retained node seeds at its prior position and is tethered there by a weak anchor spring; the solver re-converges from that warm seed (`spawn_anchored` / `force_to_convergence_anchored`), so the layout morphs coherently. The anchor spring's strength is the **anchor stiffness** — `0.0` lets nodes settle freely (the un-anchored behavior), higher values hold them closer to where they were. [force-anchor-springs]
- **Structural-diff spawn for new nodes.** A node whose key is new (no prior position) is *not* anchored; it seeds at the centroid of its already-placed neighbours (plus deterministic jitter), so an added node appears next to where it belongs rather than across the canvas. A node with no placed neighbour falls back to the deterministic scatter. [force-structural-diff]
- **Seeded, deterministic scatter.** The fresh-scatter seed (first build, kind switch, or unplaceable new node) is a deterministic hash-based pseudo-random scatter (`scatter` / `scatter_point`, a splitmix-style mixer over the node index), so a layout is reproducible and headless snapshots are stable. [force-seeded-rng]
- **Live-stable re-clustering.** The cluster-review graph's live preview re-runs the structural pass on a debounced config change; because the cluster `Source` supplies a stable `node_key` (the cluster node id), each debounced rebuild morphs from the prior layout instead of jumping, so tuning a slider reads as the clusters reorganizing smoothly. The camera is preserved across the re-seed. [cluster-config-live-stable]

A fresh scatter and an un-anchored solve remain the fallback whenever no history exists or the source opts out of `node_key` — identical to the pre-anchor behavior.


## Display controls in the view menu

The eye-icon view-options menu (`State::view_options_menu`) is where display-engine controls live — distinct from clustering options, which stay on the clustering engine's own config. [view-menu-display-controls]

- **Layout kind + rank direction** picker, returning whether a relayout is needed.
- **Anchor stiffness** slider (force-directed only) governing the temporal-stability morph: low = lively/free re-settle, high = nodes hold their prior positions tightly. [force-cfg-anchor-stiffness]
- **Node / edge / label size** sliders and the common toggles (labels, edges, hover preview).
- **Projection** mode selector and its live config sub-menu (`projection.md`).


## Projection modes

The graph is the primary target for the shared `Projection` seam: Off (affine) / Fisheye / Poincaré-disk navigation, with geodesic edges, boundary fade, a magnification-coupled LOD ladder, Möbius fly-to, and a corner Poincaré minimap. Because every graph consumer drives this one engine, the modes land for all of them at once. The seam, its math crate (`hiker-projection`), and the full control set are specced in `projection.md` (`proj-graph-mode` and the `proj-*` slugs); this doc does not duplicate them.


## Deferred

- **3D graph mode.** A 2D/3D toggle riding the shared 3D scene substrate, with the cluster hierarchy and the note wikilink graph as alternate edge feeds. Tracked in `ideas.md` (`graph-3d-mode` / `scene3d-shared`).
