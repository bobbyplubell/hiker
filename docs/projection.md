# Projection

Hyperbolic / fisheye view options for the spatial surfaces. Both the vault graph and the `.canvas` board today project an infinite Euclidean plane to screen through one affine pan+zoom chokepoint; this doc generalizes that chokepoint into a swappable `Projection` so a focus+context fisheye and a full Poincaré-disk navigation mode become pure view-state additions — no change to stored coordinates, the `.canvas` format, the op-log, or sync.

The headline decisions:

- **One projection seam, two surfaces.** Generalize the graph's `screen_mapper` (`app/src/widgets/force_graph.rs`) and the canvas's `Camera` (`hiker-canvas/view-core/src/camera.rs`) onto a shared `Projection` over `world_to_screen` / `screen_to_world`; the current affine pan+zoom becomes one impl. Projection is view state, never persisted into coordinates. [proj-seam]
- **Hyperbolic is a lens over Euclidean coords, never a storage model.** Layout/positions stay Euclidean; the disk is computed at paint time around a movable focus. Adopting a `{p,q}` tiling + anchor-identity (as the reference app does) is explicitly rejected — it would fight JSON Canvas, path-identity, and the canonical-JSON op-log. [proj-euclidean-storage]
- **Fisheye is the cheap shared substrate; Poincaré is the showpiece mode on top.** A radial focus+context lens reuses almost all the machinery the disk view needs, with no mode switch. [proj-fisheye, proj-poincare]
- **The graph is the primary target, the canvas a willing second.** A node graph has no flat-card-content problem, so it warps cleanly; the canvas keeps cards as axis-aligned rects scaled by a per-card factor (no glyph shearing). [proj-graph-mode, proj-canvas-mode]
- **A corner Poincaré minimap is both an overview and the entrypoint** — click it to swap the disk into the full pane while the Euclidean view demotes into the corner. [proj-minimap, proj-minimap-expand]
- **Möbius fly-to + drag-to-recenter are the navigation model** — there is no edge to scroll off, so navigation is re-centering the focus, animated on click/open. [proj-mobius-pan, proj-flyto]


## The seam

Both surfaces funnel every world→screen mapping through one place: the canvas `Camera` (`world_to_screen` / `screen_to_world` / `world_rect_to_screen`, `camera.rs`) and the graph `View::screen_mapper` closure (`force_graph.rs`), with the inverse used for hit-testing (`graph_view.rs::hit_test`). Generalize each into a `Projection` (trait or enum) over the same forward + inverse surface; affine pan+zoom is `Projection::Affine` and stays the default. [proj-seam]

- **Invertible both ways.** Every projection supplies `world_to_screen` and `screen_to_world` so hit-testing, dragging, and marquee keep working — fisheye and Poincaré both have closed-form inverses, so picking stays exact, not raster-sampled. [proj-seam]
- **Local magnification.** A projection also reports a scalar magnification at a point — `(1 − |z|²)` for the Poincaré disk (the conformal factor, matching the reference app's GPU point-size falloff), the radial derivative for the fisheye. Node radius, card size, label visibility, and the LOD ladder all read this one number rather than the raw zoom. [proj-magnification]
- **Egui-free geometry.** The complex-arithmetic / Möbius / geodesic-sampling math (~200 lines, ported from the reference Poincaré app) lives in a dedicated pure crate, `hiker-projection`, beside `hiker-graph` in the egui-free render-math layer — testable headless, wasm-clean, and reusable by both spatial surfaces (the canvas `Camera` and the graph view's transform both depend on it). [proj-geometry]

Projection is view state, exactly like the camera is today (`canvas-pan-zoom`): it never enters the op-log, the `.canvas` file, or stored node coordinates. [proj-seam]


## Fisheye lens

A `Projection` impl that applies a radial focus+context distortion around a focus point: detail under the focus stays full-size while the periphery shrinks gracefully toward the edge instead of scrolling off. The 80%-of-the-value, ~30%-of-the-work option — no mode switch, no learning curve, and it is most of the machinery the disk view reuses. [proj-fisheye]

- **Distortion.** A radially-symmetric remap of distance-from-focus — a Poincaré-style `r' = tanh(k·r)` or a graphical (Sarkar–Brown) fisheye — applied after the affine map, around a focus that defaults to the viewport center and follows the cursor / selection. [proj-fisheye]
- **Objects scale, never shear.** Nodes scale by `proj-magnification`; canvas cards stay flat axis-aligned rects placed at their projected center with a per-card uniform scale (egui can't bend a glyph — the same compromise the reference app makes; it reads fine because the map is locally conformal). [proj-card-scale]
- **Cards fill the space.** Under a lens a card sizes to the on-screen distance to its nearest neighbour (`gap · fill`, `fill ≈ 0.9`, clamped) rather than to the affine card size — so sparse regions of the disk fill out (no tiny floaters) while dense regions stay compact. A `Fill` slider tunes how aggressively. [proj-card-fill]
- **Periphery degrades for free.** Far objects shrink below the existing LOD threshold and become the dot placeholders (`proj-lod-ladder`); edges already sample and curve, so they distort with no extra work.


## Poincaré disk mode

The full hyperbolic look on the same seam: project around the focus into the unit disk, draw the boundary, scale objects by the `(1 − |z|²)` conformal factor, draw edges as geodesic arcs, and navigate by Möbius re-centering. [proj-poincare]

- **Navigation ViewMode, not an editing surface.** Editing in warped space (resize handles, hit-testing through a Möbius transform) isn't worth fighting on the canvas, so Poincaré is a mode you toggle into to read/navigate and back out of to edit — the same pattern as the existing Canvas/JSON toggle (`canvas-view-toggle`). The graph view is read-navigate already, so there it is simply another view option. [proj-poincare-mode]
- **Möbius pan.** Dragging re-centers the disk (the drag point is pushed toward the rim via a `transformFromPointPair`-style Möbius transform) rather than scrolling — there is no edge to fall off. Re-centering the focus each frame also sidesteps the boundary-precision problem that forced the reference app to re-anchor against a tiling, so no anchor-identity model is needed. [proj-mobius-pan]
- **Fly-to focus.** Clicking a node (or opening one elsewhere, via `nav-stack`) animates a Möbius fly-to that glides that node to the disk center with a cubic ease-out — the single most legible navigation gesture, and pure view state. [proj-flyto]
- **Geodesic edges.** Under any curved projection, edges draw as geodesic arcs (sampled into short polylines) instead of straight segments; the canvas edge router (`canvas-edge-routing`) and the graph edge pass both subdivide. Arc curvature encodes hyperbolic distance, so a link to a far node visibly bows — free information. [proj-geodesic-edges]
- **Boundary + rim fade.** Draw the unit-disk boundary, and fade objects toward the rim (alpha falling off near `|z| → 1`, mirroring the reference shader) so the compressed periphery suggests rather than clutters. [proj-boundary]
- **LOD ladder.** Magnification (`proj-magnification`) drives a ladder that reuses the existing LOD path (`canvas-lod-placeholder`; the graph's label-min-zoom gate): full card/node near the focus → title-only dot mid-disk → bare edge-endpoint marker near the rim. This is what makes the "cards can't warp" problem vanish — at the only place it would bite (the squished periphery) the card has already collapsed to a dot, which projects perfectly. [proj-lod-ladder]


## Minimap entrypoint

A corner Poincaré minimap is the overview and the on-ramp into full disk mode. [proj-minimap]

- **Always-on overview.** A small disk in a pane corner renders the whole surface projected around the current focus — even while the main pane stays Euclidean — for orientation on large graphs/boards. Read-only paint of data already in hand; reuses the `show_static` / `NoContentRenderer` paint-only path (`canvas-static-paint`) so it never invokes the heavy content engines. Distinct from the texture-backed overview minimap (`canvas-minimap`), which is an affine thumbnail; this one is the hyperbolic projection. [proj-minimap]
- **Click to expand (the swap).** Clicking the minimap promotes the disk to fill the pane and demotes the Euclidean view into the corner circle — an inversion, not a separate tab, so the two views trade places and clicking the (now-Euclidean) corner swaps back. Toggling is animated. [proj-minimap-expand]
- **Circle ↔ square framing.** The disk/minimap frame can render as a circle (true to the model, with rim fade) or a square (fills a rectangular pane corner-to-corner, clipping the disk) — a per-view option. [proj-minimap-shape]


## Surface applicability

- **Graph (primary).** The vault-wide `Graph` tab and the cluster/cluster-review graph views all drive `graph_view`, so the projection modes land for every graph consumer at once. Nodes are circles and edges are lines — no flat-card-content problem — making this the cleanest target and the high-value case: a focus+context browser where the centered node is large and its whole neighborhood stays on screen. [proj-graph-mode]
- **Canvas (second).** The `.canvas` board adopts the same seam with the `proj-card-scale` compromise for card content. Euclidean coordinates and the canonical-JSON op-log binding are untouched — the projection is a lens over `x/y`, applied at paint/hit-test time only. [proj-canvas-mode]


## Toggle and configuration

Every projection feature is opt-in and trivially reversible from the surface's existing eye / **View** menu (`canvas-view-toggle`; the graph view's view-options wrench), and each surface owns its own settings — turning the whole thing off is one click. [proj-view-toggle]

- **Mode selector.** The View menu carries a projection selector — **Off (Euclidean)** / **Fisheye** / **Poincaré** — defaulting to Off. Off restores the exact affine behavior (`proj-seam`'s `Affine` impl), so the feature is invisible until chosen. Per surface, persisted as view state alongside the camera (`canvas-view-state-persist`); never in the op-log or the `.canvas`. [proj-view-toggle]
- **Live config sub-menu.** Selecting a non-Off mode reveals its controls inline in the same menu; every control applies next frame and is view state. Controls show only for the parameters the active mode actually uses (fisheye hides disk-only options and vice versa). Each slider below is wired to a real render parameter — none are cosmetic. [proj-config-live]

| Control | Affects | Slug |
| ------- | ------- | ---- |
| **Strength** `k` | the distance→radius mapping (`tanh(k·r)` / disk spread) — how aggressively the periphery compresses | [proj-cfg-strength] |
| **Focus source** | cursor-follow / locked-center / follow-selection — what point the lens centers on | [proj-cfg-focus-mode] |
| **Size falloff** | how strongly node/card size tracks magnification toward the rim (0 = uniform size, 1 = full `(1−|z|²)` conformal) | [proj-cfg-size-falloff] |
| **Card scale clamp** | min/max per-card scale so rim cards stay clickable and center cards don't explode (canvas; `proj-card-scale`) | [proj-cfg-card-scale-clamp] |
| **Card fill** | how aggressively a card grows to fill the screen gap to its nearest neighbour under the lens (canvas; `proj-card-fill`) | [proj-card-fill] |
| **Geodesic edges** | on/off + segment count — straight chords vs smoothly-sampled arcs (`proj-geodesic-edges`) | [proj-cfg-geodesic] |
| **Boundary fade** | boundary-circle on/off + fade start radius + fade strength (`proj-boundary`) | [proj-cfg-boundary-fade] |
| **LOD thresholds** | the magnification cutoffs for full → dot → edge-marker (`proj-lod-ladder`) | [proj-cfg-lod-thresholds] |
| **Fly-to** | on/off + animation duration (`proj-flyto`) | [proj-cfg-flyto] |
| **Minimap** | on/off + corner + size + circle/square shape (`proj-minimap`, `proj-minimap-shape`) | [proj-cfg-minimap] |

Sensible defaults ship so a user who only flips the mode on gets a good-looking result without touching a slider; the controls are for taste and for dense-vs-sparse vaults. [proj-config-live]


## Deferred

- **3D hyperbolic / H3.** A 3D analogue (project into the unit ball) rides the shared 3D scene substrate (`ideas.md` `[scene3d-shared]`), not this 2D seam. [proj-h3-3d]
- **Klein / half-plane models.** The Poincaré disk is the shipped model; the Beltrami–Klein (straight geodesics) or upper-half-plane models are alternate projections on the same seam if a use case appears. [proj-alt-models]
- **Focus-pinning multiple nodes.** Pinning two foci for an elliptical/bipolar lens (compare two neighborhoods at once) — a later refinement of the single-focus lens. [proj-multi-focus]


## Out of scope

- **A native hyperbolic storage model.** No `{p,q}` tiling, no combinatorial anchor-identity, no Möbius-composed chart graph (the reference app's model). Hiker stays Euclidean-on-disk; hyperbolic is strictly a view (`proj-euclidean-storage`).
- **Editing in warped space.** Resize/connect/marquee stay in the Euclidean (or fisheye-lite) surface; the disk is navigate-only on the canvas (`proj-poincare-mode`).
- **GPU compute requirement.** The reference app pushes the Möbius transform to a WebGL vertex shader for a 28k-point tiling; hiker projects only the on-screen (viewport-culled) node/card set on the CPU, so no new render backend is required. A glow paint-callback fast path is an optimization, not a prerequisite.
</content>
</invoke>
