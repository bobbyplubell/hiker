//! Node + edge painting on [`State`]: the per-frame draw loop that renders
//! each node at its LOD tier, the mode-aware edge routing (straight /
//! geodesic / fisheye-bulge), the hover/selection highlight overlays, and the
//! hover-preview refresh. A pure `impl State` continuation of the parent
//! module, split out for file length; the free painting primitives it draws
//! through live in [`edge_paint`](super::edge_paint).

use hiker_graph::LayoutKind;
use hiker_projection::ProjectionKind;

use super::edge_paint::{
    adaptive_segments, draw_node_fill, emit_edge, gradient_strokes, hop_potential, sub_polyline,
    tint, Lod, NodeFill, CULL_ALPHA,
};
use super::gpu::GpuBatch;
use super::source::{NodeDescriptor, Source};
use super::styling::fade;
use super::{ease_out_reveal, EdgeMap, Lens, NodeDraw, NodePaint, State, REVEAL_DUR};

/// Cap on the live-bundle radius inflation: a container drawn as a bundle this frame grows by
/// `1 + 0.5·√(rolled_count)`, clamped to this multiple of its base radius so even a huge rollup
/// stays a node, not a blob. status: code-graph-bundling
const BUNDLE_RADIUS_MAX: f32 = 4.0;
/// Smallest live rolled-up count that earns a `· N` bundle-count suffix on a container's label — a
/// 1–2 member rollup reads fine by name alone. status: code-graph-bundling
const BUNDLE_COUNT_MIN: u32 = 3;
/// Label-density BUDGET at the fitted overview (fit-relative zoom ≈ 1): the de-confliction places at
/// most this many labels there — the highest-priority (most important) ones — so the structure reads
/// instead of being papered over. Grows with zoom (see the budget formula). status: graph-label-dim
const BASE_LABELS: usize = 14;
/// Hard cap on placed labels at any zoom, so even a deep zoom-in stays readable. status: graph-label-dim
const MAX_LABELS: usize = 140;
/// Node-count threshold below which a graph is treated as SMALL: a filtered Hops subgraph or a
/// single-package SCIP. At/under it the label LOD shows MOST labels — the depth gate is bypassed
/// (every node is a candidate) and the budget is lifted to [`MAX_LABELS`], so all candidates place
/// (subject only to overlap de-confliction). Above it (the ~10k overview) the depth gate + the
/// zoom-scaled budget stay in force. status: code-graph-scope-hops
const SMALL_GRAPH_LABELS: usize = 120;

/// The label BUDGET for this frame's de-confliction pass: a SMALL graph (`small_graph`) lifts it to
/// [`MAX_LABELS`] (≥ its node count, so every candidate places — only overlap can drop one); the
/// large overview keeps the zoom-scaled budget (floored at [`BASE_LABELS`], capped at [`MAX_LABELS`]).
/// status: code-graph-scope-hops, graph-label-dim
fn label_budget(small_graph: bool, label_zoom: f32) -> usize {
    if small_graph {
        MAX_LABELS
    } else {
        ((BASE_LABELS as f32) * label_zoom.max(1.0))
            .round()
            .clamp(BASE_LABELS as f32, MAX_LABELS as f32) as usize
    }
}

/// Target on-screen merge distance (px): SPATIAL bundling collapses nodes whose FA2 positions land
/// within ~this distance on screen into one cluster rep. The world grid is sized so each cell spans
/// at least this many pixels (then rounded UP to a power of two), so a cell's members are within
/// ~`MERGE_PX·√2` of the rep on screen — close enough that revealing them on zoom-in keeps them in
/// view. Tuned via the `bundle-open` graph-harness scenario. status: code-graph-bundling
const MERGE_PX: f32 = 48.0;
/// Clamp on the world-cell power-of-two exponent (`cell = 2^exp`). Bounds keep a degenerate
/// `screen_scale` (a near-zero or huge zoom) from producing an absurd cell size that would either
/// merge the whole graph into one node or never merge anything. The range spans the world extents the
/// FA2 layouts settle into (~1e3 box). status: code-graph-bundling
const MIN_CELL_EXP: f32 = -4.0;
const MAX_CELL_EXP: f32 = 12.0;

/// Per-frame SPATIAL bundling state (marker-cluster / quadtree collapse-on-zoom-out): which nodes
/// are drawn as individuals at this zoom and, for each culled node, the on-screen REPRESENTATIVE it
/// rolls up into. Built once per pane paint by hashing each node's [`NodeDescriptor::world_pos`] into
/// a WORLD-FIXED power-of-2 grid sized so each cell spans ~`MERGE_PX` on screen (see
/// [`State::compute_bundles`]): nodes that land in the same cell are within ~`MERGE_PX` of each other
/// on screen, so they collapse to the cell's highest-[`NodeDescriptor::label_scale`] member. Because a
/// cluster's members are always within one cell of the rep, zooming in (shrinking the cell) splits a
/// cluster into sub-clusters that STAY in view — the whole point of spatial (vs hierarchical)
/// bundling. Indexed by `NodeDescriptor::index` (== positions index). status: code-graph-bundling
pub(super) struct BundleState {
    /// `visible[i]` — node `i` is drawn + hit-tested as an individual at this zoom. A culled node
    /// (`false`) is rolled up into [`Self::representative`]`[i]` and contributes no fill/label/hit.
    visible: Vec<bool>,
    /// `representative[i]` — the VISIBLE rep node `i` rolls up into (the highest-`label_scale` member
    /// of its on-screen cell); `i` itself when visible. Edges roll up to these. Since the rep shares
    /// `i`'s cell, it is within ~`MERGE_PX` of `i` on screen.
    representative: Vec<usize>,
    /// `rolled_count[r]` — the LIVE number of nodes that have `r` as their representative but are NOT
    /// themselves visible this frame (i.e. how many descendants are currently collapsed INTO container
    /// `r`). A visible node with a non-zero count is acting as a bundle this frame; the draw path
    /// reads it to inflate the radius + append a `· N` count that shrinks as members emerge on
    /// zoom-in. Identity (no bundling) is all-zero, so non-bundling sources render unchanged.
    /// status: code-graph-bundling
    rolled_count: Vec<u32>,
}

impl BundleState {
    /// Bundling disabled (every node visible, its own representative) — used when the caller passes a
    /// non-positive `screen_scale` (the read-only / Poincaré panes) or when the source's `bundling`
    /// toggle is off, so canvas / vault-graph / minimap / disk paths are unaffected.
    pub(super) fn identity(n: usize) -> Self {
        Self {
            visible: vec![true; n],
            representative: (0..n).collect(),
            rolled_count: vec![0; n],
        }
    }

    /// Whether node `i` is drawn as an individual this frame (defaults to visible for an
    /// out-of-range index, so a node the bundling didn't cover still paints).
    pub(super) fn is_visible(&self, i: usize) -> bool {
        self.visible.get(i).copied().unwrap_or(true)
    }

    /// The full per-node visible set this frame — the un-bundling animation snapshots it to detect
    /// next-frame culled→visible transitions (a member emerging from its bundle). status: code-graph-bundling
    pub(super) fn visible(&self) -> &[bool] {
        &self.visible
    }

    /// The representative (rolled-up container) of node `i` for edge endpoints — itself when out of
    /// range.
    pub(super) fn rep(&self, i: usize) -> usize {
        self.representative.get(i).copied().unwrap_or(i)
    }

    /// How many nodes are currently rolled up INTO node `i` (its live collapsed-descendant count).
    /// `0` for a leaf, a fully-expanded container, or any non-bundling source. A visible node with a
    /// non-zero count presents as a bundle this frame (inflated radius + `· N` label).
    /// status: code-graph-bundling
    pub(super) fn rolled_count(&self, i: usize) -> u32 {
        self.rolled_count.get(i).copied().unwrap_or(0)
    }

    /// The radius MULTIPLIER node `i` is drawn at this frame from its live bundle count — `1.0` for a
    /// non-bundle node, growing `1 + 0.5·√(rolled)` (clamped) for a container acting as a bundle. The
    /// node draw AND the hit-test both apply it, so the inflated bundle square is hoverable where it's
    /// drawn (not just within its small un-inflated circle). status: code-graph-bundling
    pub(super) fn radius_mult(&self, i: usize) -> f32 {
        let rolled = self.rolled_count(i);
        if rolled > 0 {
            (1.0 + 0.5 * (rolled as f32).sqrt()).clamp(1.0, BUNDLE_RADIUS_MAX)
        } else {
            1.0
        }
    }

    /// A cheap fingerprint of the visible SET, folded into the affine GPU fill-cache key so a zoom
    /// that crosses a bundling threshold (changing which nodes are culled) rebuilds the cached fill
    /// buffer, while a pure pan at a fixed zoom (same visible set) still hits the cache. `0` when
    /// bundling is the identity (every node visible) — the historical cache behaviour. status: code-graph-bundling
    pub(super) fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        // Identity (all visible) → 0, so non-bundling sources never perturb the cache key.
        if self.visible.iter().all(|&v| v) {
            return 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.visible.hash(&mut h);
        h.finish()
    }
}

impl State {
    /// Per-node label-alpha factors for selection dimming: the selected node's
    /// label at full strength, its 1-hop neighbours semi-dimmed, everything else
    /// dimmed — `None` (no dimming) when the option is off or nothing is
    /// selected, so an unselected graph renders exactly as before.
    /// status: graph-label-dim
    pub(super) fn label_dim_factors(&self, source: &dyn Source) -> Option<Vec<f32>> {
        const SEMI: f32 = 0.55;
        const DIM: f32 = 0.18;
        if !self.highlight.dim_labels {
            return None;
        }
        let n = self.positions.len();
        let s = self.selected_node.filter(|&s| s < n)?;
        let mut f = vec![DIM; n];
        for (a, b) in source.edges() {
            let (a, b) = (a as usize, b as usize);
            if a == s && b < n {
                f[b] = SEMI;
            } else if b == s && a < n {
                f[a] = SEMI;
            }
        }
        f[s] = 1.0;
        Some(f)
    }

    /// Build this frame's [`BundleState`] by SPATIAL clustering on the FA2 layout positions: nodes
    /// that fall within ~one screen region collapse to a single representative, and zooming in splits
    /// each cluster into on-screen sub-clusters.
    ///
    /// `screen_scale` is the real world→screen pixel scale of the pane (the affine `view.zoom`, with
    /// no lens factor); a **non-positive** `screen_scale` means "no spatial bundling" (the read-only /
    /// Poincaré panes, where bundling is disabled) → returns [`BundleState::identity`] so every node
    /// shows. The `lens` is accepted for signature symmetry but unused: spatial bundling is the affine
    /// path only.
    ///
    /// Algorithm: a world-fixed quadtree grid (origin at world 0). Cell size targets ≥ [`MERGE_PX`] on
    /// screen — `MERGE_PX / screen_scale` in world units — QUANTIZED to the next power of two so the
    /// grid subdivides in stable octaves (a pan never changes membership; zoom only flips a whole
    /// octave at once, so clusters don't flicker per frame). Each node hashes to cell
    /// `(floor(x/cell), floor(y/cell))`; a cell with ≥2 members keeps its highest-`label_scale` member
    /// as the rep (tie-break: lowest index → fully deterministic) and culls the rest into it. A cell's
    /// members are therefore always within one `cell` (≤ ~`MERGE_PX·√2` on screen) of the rep, so when
    /// the cell subdivides on zoom-in the revealed sub-clusters are guaranteed on-screen.
    /// status: code-graph-bundling
    pub(super) fn compute_bundles(
        &self,
        nodes: &[NodeDescriptor],
        lens: &Lens,
        screen_scale: f32,
    ) -> BundleState {
        let _ = lens; // spatial bundling is affine-only; magnification is identity there.
        // Size the arrays to cover every descriptor index (== positions index). Out-of-range lookups
        // fall back to "visible / self", so a partial descriptor set is safe.
        let n = nodes.iter().map(|d| d.index + 1).max().unwrap_or(0).max(self.positions.len());
        // Sentinel: a non-positive screen scale disables spatial bundling (read-only / Poincaré),
        // so those panes render every node (today's behaviour for them).
        if !(screen_scale > 0.0) {
            return BundleState::identity(n);
        }
        // World cell size whose on-screen span is ≥ MERGE_PX, quantized UP to a power of two for
        // cross-frame/zoom stability, then clamped to a sane world range so a degenerate zoom can't
        // blow up the exponent. With cell == 2^k, panning never moves a node across a boundary
        // (origin-fixed) and zoom only steps k by whole octaves.
        let target_world = (MERGE_PX / screen_scale).max(f32::MIN_POSITIVE);
        let exp = target_world.log2().ceil().clamp(MIN_CELL_EXP, MAX_CELL_EXP);
        let cell = 2f32.powf(exp);

        // Group node indices by world cell key. Reserve for the common dense graph.
        let mut cells: std::collections::HashMap<(i32, i32), Vec<usize>> =
            std::collections::HashMap::with_capacity(nodes.len());
        for d in nodes {
            if d.index >= n {
                continue;
            }
            let key = (
                (d.world_pos.x / cell).floor() as i32,
                (d.world_pos.y / cell).floor() as i32,
            );
            cells.entry(key).or_default().push(d.index);
        }

        // `label_scale` per index (the rep-rank), defaulting to 1.0 for an index without a descriptor.
        let mut rank = vec![1.0f32; n];
        for d in nodes {
            if d.index < n {
                rank[d.index] = d.label_scale;
            }
        }

        let mut visible = vec![true; n];
        let mut representative: Vec<usize> = (0..n).collect();
        let mut rolled_count = vec![0u32; n];
        for members in cells.values() {
            if members.len() < 2 {
                continue; // a lone node in its cell stays itself.
            }
            // Rep = highest label_scale, tie-break lowest index (deterministic).
            let rep = *members
                .iter()
                .min_by(|&&a, &&b| {
                    rank[b].partial_cmp(&rank[a]).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
                })
                .expect("non-empty cell");
            for &m in members {
                if m == rep {
                    continue;
                }
                visible[m] = false;
                representative[m] = rep;
            }
            rolled_count[rep] = (members.len() - 1) as u32;
        }
        BundleState { visible, representative, rolled_count }
    }

    /// Advance the per-node un-bundling reveal animation one frame against this frame's `bundles`,
    /// returning whether anything is still animating (so the caller can request a repaint). For each
    /// node: a culled→visible transition since last frame (a member emerging from its dissolving
    /// bundle) restarts its `reveal_t` at `0.0`; a currently-visible node steps its `reveal_t` toward
    /// `1.0` by `dt / REVEAL_DUR`; a culled node is left alone (it isn't drawn, and the next reveal
    /// resets it). Snapshots the visible set for next frame's transition detection. ONLY the
    /// interactive affine pane calls this — read-only / Poincaré paths render at settled positions.
    /// status: code-graph-bundling
    pub(super) fn advance_reveal(&mut self, bundles: &BundleState, dt: f32) -> bool {
        // Keep the animation arrays sized to the current node count (a no-op once settled). A relayout
        // already reset them; this only catches a length drift without a full relayout.
        let n = self.positions.len();
        if self.reveal_t.len() != n {
            self.reveal_t.resize(n, 1.0);
        }
        // First interactive frame after a (re)layout has no visibility history: seed it from THIS
        // frame so nothing counts as a fresh reveal (every node stays settled — no spurious whole-graph
        // animation). Real fly-outs only start once a later frame's zoom flips a node culled→visible.
        if self.prev_bundle_visible.len() != n {
            self.prev_bundle_visible = bundles.visible().to_vec();
            self.prev_bundle_rep = (0..n).map(|i| bundles.rep(i)).collect();
            return false;
        }
        // Resize the rep history alongside the visible history (a drift without a relayout).
        if self.prev_bundle_rep.len() != n {
            self.prev_bundle_rep.resize(n, 0);
        }
        if self.reveal_origin.len() != n {
            self.reveal_origin.resize(n, 0);
        }
        let step = if REVEAL_DUR > 0.0 { dt / REVEAL_DUR } else { 1.0 };
        let mut animating = false;
        for i in 0..n {
            let vis = bundles.is_visible(i);
            let was_vis = self.prev_bundle_visible.get(i).copied().unwrap_or(false);
            if vis && !was_vis {
                // Just emerged from its SPATIAL cluster — start the fly-out from the rep it was
                // bundled into LAST frame (its own cell's rep, ≤ ~MERGE_PX away → a short, local
                // "explode open in place"). Capture that origin now (the live `prev_bundle_rep` still
                // holds last frame's rep); `effective_positions` lerps out from it this frame.
                self.reveal_t[i] = 0.0;
                self.reveal_origin[i] = self.prev_bundle_rep.get(i).copied().unwrap_or(i);
            }
            if vis {
                self.reveal_t[i] = (self.reveal_t[i] + step).min(1.0);
                if self.reveal_t[i] < 1.0 {
                    animating = true;
                }
            }
        }
        self.prev_bundle_visible = bundles.visible().to_vec();
        // Snapshot THIS frame's reps for next frame's transition detection (a node culled now has its
        // cluster rep here; the frame it reveals reads this as its fly-out origin).
        self.prev_bundle_rep = (0..n).map(|i| bundles.rep(i)).collect();
        animating
    }

    /// This frame's EFFECTIVE draw position per node (indexed by positions index), used by nodes,
    /// labels, AND edges so everything tracks the un-bundling animation. A settled node (`reveal_t >=
    /// 1.0`, the common case) maps to its own `world_pos`; a mid-flight node lerps from its SPATIAL
    /// cluster rep's `world_pos` toward its own by [`ease_out_reveal`], so members "explode open" out
    /// from the cluster they were collapsed into. The fly-out origin is [`Self::reveal_origin`] —
    /// captured when the node emerged ([`State::advance_reveal`]), the rep of the cell it was bundled
    /// into the frame before. Because that rep shared its cell, the origin is ≤ ~`MERGE_PX` away on
    /// screen → a short, local fly-out. A node whose origin is itself (no cluster) never animates. The
    /// returned vector is `world_pos` byte-for-byte when nothing is animating. status: code-graph-bundling
    pub(super) fn effective_positions(&self, nodes: &[NodeDescriptor]) -> Vec<egui::Vec2> {
        // Index → world_pos lookup from the descriptors (positions index == descriptor index).
        let n = self.positions.len();
        let mut world_pos = self.positions.clone();
        for d in nodes {
            if d.index < n {
                world_pos[d.index] = d.world_pos;
            }
        }
        let mut eff = world_pos.clone();
        for i in 0..n {
            let t = self.reveal_t.get(i).copied().unwrap_or(1.0);
            if t >= 1.0 {
                continue;
            }
            // Start at the cluster rep's FINAL world_pos (don't chase a moving rep — simpler, looks
            // right). A node whose recorded origin is itself has no fly-out, so it stays put.
            let origin = self.reveal_origin.get(i).copied().unwrap_or(i);
            if origin != i && origin < n {
                let e = ease_out_reveal(t);
                eff[i] = world_pos[origin] + (world_pos[i] - world_pos[origin]) * e;
            }
        }
        eff
    }

    /// The LOD tier for a node at lens magnification `mag`. Always [`Lod::Full`]
    /// under Affine (`!lens.active()`); otherwise stepped by the configured
    /// thresholds, read so `lod_marker_mag < lod_full_mag` even if the fields are
    /// set out of order.
    pub(super) fn lod_tier(&self, mag: f32, lens_active: bool) -> Lod {
        if !lens_active {
            return Lod::Full;
        }
        let full = self.lod_full_mag;
        let marker = self.lod_marker_mag.min(full - f32::EPSILON);
        if mag >= full {
            Lod::Full
        } else if mag >= marker {
            Lod::Dot
        } else {
            Lod::Marker
        }
    }

    /// Paint every node + label, returning the click target, the hovered
    /// node's screen anchor (for the preview card), and any tooltip.
    ///
    /// Under an active lens each node renders at its [`Lod`] tier: FULL (the
    /// descriptor's shape + label), DOT (a small filled circle, no label), or
    /// MARKER (a tiny point). Rim fade applies to every tier; FULL is always
    /// used under Affine, so a non-projected view is unchanged.
    pub(super) fn draw_nodes(
        &self,
        painter: &egui::Painter,
        nodes: &[NodeDescriptor],
        to_screen: &dyn Fn(egui::Vec2) -> egui::Pos2,
        paint: &NodePaint<'_>,
        bundles: &BundleState,
        eff_pos: &[egui::Vec2],
        mut gpu: Option<&mut GpuBatch>,
    ) -> NodeDraw {
        // Spans the GPU node-fill batch build + the per-frame label/hover work.
        // On an affine cache hit the fill pushes are dropped (the batch is
        // `cached`), so this scope then measures only labels/hover.
        super::profile_scope!("graph_view::draw_nodes (gpu batch build + labels)");
        let &NodePaint { lens, zoom, label_zoom, hovered, response_clicked, label_dim } = paint;
        let node_scale = self.style.node_scale;
        let label_size = self.style.label_size;
        let lens_active = lens.active();
        let mut draw = NodeDraw::default();
        // Label candidates, de-conflicted after the node pass: (priority, top-center anchor, text,
        // alpha, node-index). Priority is importance-biased (`label_scale`×1000 + magnification
        // tiebreak), so the most significant labels win an overlap contest. Font size is UNIFORM at
        // paint time — priority controls only WHICH labels show, not their size.
        let mut labels: Vec<(f32, egui::Pos2, String, f32, usize)> = Vec::new();
        // A SMALL graph (a filtered Hops subgraph or a single-package SCIP) shows MOST labels: the
        // depth gate is bypassed (every node is a candidate) and the budget is lifted to MAX_LABELS,
        // so all candidates place subject only to overlap de-confliction. The huge overview
        // (`nodes.len() > SMALL_GRAPH_LABELS`) is unchanged. status: code-graph-scope-hops
        let small_graph = nodes.len() <= SMALL_GRAPH_LABELS;
        // Cull against the pane's clip; pad by a node radius + a label line so a
        // node hugging the edge (whose label hangs below it) is still drawn.
        let clip = painter.clip_rect();
        let label_pad = self.style.label_size + 6.0;
        // The world-space (Affine, cacheable) GPU fill buffer is built ONCE and the
        // GPU scissor clips it every frame, so its fills must NOT be viewport-culled
        // here — else nodes off-screen at build time would be permanently missing
        // when a later pan/zoom on the cached buffer brings them into view.
        let world_fill = gpu.as_deref().is_some_and(|b| b.world_space);
        // The node's EFFECTIVE world position this frame: its own settled `world_pos`, OR — mid
        // un-bundling — a point lerped out from its dissolving bundle's centre (see
        // `effective_positions`). Equal to `world_pos` byte-for-byte when nothing is animating, so a
        // settled / non-affine view is unchanged. status: code-graph-bundling
        let wp = |d: &NodeDescriptor| eff_pos.get(d.index).copied().unwrap_or(d.world_pos);
        for d in nodes {
            // Spatial bundling: a node culled into a denser on-screen cluster is dropped — not emitted
            // to the GPU instance buffer, not painted, not hit-tested (its edges roll up to its cluster
            // rep, and a hover lands on the rep, not the hidden member). status: code-graph-bundling
            if !bundles.is_visible(d.index) {
                continue;
            }
            let wpos = wp(d);
            let p = to_screen(wpos);
            let mag = lens.magnification(wpos);
            // Live bundle count: how many descendants are still collapsed into this node THIS frame.
            // Non-zero → the node is acting as a bundle right now (a container with culled members);
            // it drives a √-scaled radius bump + a `· N` label suffix, both of which shrink toward
            // nothing as members reveal on zoom-in, so the bundle visibly DISSOLVES.
            // status: code-graph-bundling
            let rolled = bundles.rolled_count(d.index);
            // Center nodes grow, rim nodes shrink (1.0 under Affine). A container acting as a bundle
            // this frame is inflated by the live count (the same multiplier the hit-test applies, so
            // the drawn square is hoverable where it's shown): a 200-member bundle reads bigger than a
            // 5-member one, clamped to ~4× so it never dominates. As members emerge `rolled` drops and
            // the node shrinks back. status: code-graph-bundling
            let r = d.radius * node_scale * zoom.max(0.4) * mag * bundles.radius_mult(d.index);
            let alpha = lens.rim_alpha(wpos, self.fade_start, self.fade_strength);
            // Alpha cull (Poincaré rim fade) is viewport-independent — always safe.
            if alpha <= CULL_ALPHA {
                continue;
            }
            // Viewport cull gates the per-frame label/hover work and the screen-space
            // fill path; the cached world-space fill ignores it (see above).
            let on_screen = clip.expand(r + label_pad).contains(p);
            if !on_screen && !world_fill {
                continue;
            }
            let fill = fade(d.fill, alpha);
            let is_hover = hovered == Some(d.index);
            let tier = self.lod_tier(mag, lens_active);
            // Base (un-zoomed) radius for the world-space GPU batch: the shader
            // re-applies `max(view_scale, 0.4)` (== `zoom.max(0.4)`), so this must
            // exclude that factor but keep `node_scale`·`mag` (mag == 1 affine).
            let base_r = d.radius * node_scale * mag;
            let fill_pos = NodeFill {
                world: wpos.to_pos2(),
                base_r,
                screen: p,
                screen_r: r,
                color: fill,
            };
            draw_node_fill(painter, gpu.as_deref_mut(), d, tier, fill_pos, is_hover);
            // Labels + hover affordances only on on-screen FULL nodes; dots/markers
            // are too small to anchor a label or a hover ring legibly, and off-screen
            // nodes (kept for the cached world-space fill) get neither.
            if on_screen && tier == Lod::Full {
                // Status badge: a small dot riding the node's top-right shoulder, on
                // the Painter so it draws above the GPU fills (like labels / hover
                // ring) and fades with the node's rim alpha. Sized off the node
                // radius with a floor so it stays a visible mark when zoomed out.
                if let Some(badge) = d.badge {
                    let br = (r * 0.4).clamp(1.5, 4.0);
                    let bp = p + egui::vec2(r * 0.8, -r * 0.8);
                    painter.circle_filled(bp, br, fade(badge, alpha));
                }
                // Its top-left twin (open-bug mark) — an independent channel, so
                // a node can carry both marks. status: code-graph-bug-badge
                if let Some(badge) = d.bug_badge {
                    let br = (r * 0.4).clamp(1.5, 4.0);
                    let bp = p + egui::vec2(-r * 0.8, -r * 0.8);
                    painter.circle_filled(bp, br, fade(badge, alpha));
                }
                // A node is a label CANDIDATE once its depth tier reveals (`label_min_zoom`, the
                // structural LOD floor); the actual COUNT is then bounded by a zoom-scaled BUDGET in
                // the de-confliction pass below (top-priority-first, so the most important labels
                // always survive). The budget replaces the old per-node rank gate, which was fragile —
                // it multiplied importance-radius × fit-relative-zoom against a fixed threshold, so a
                // mis-calibrated fit or importance scale could drop EVERY label (the "no labels at all"
                // bug). The budget instead guarantees a floor of labels at the overview and grows on
                // zoom-in. status: graph-label-dim
                if self.toggles.show_labels
                    && (small_graph || label_zoom * mag >= d.label_min_zoom)
                    && let Some(label) = &d.label
                {
                    // Defer the draw to the de-confliction pass below. The placement priority is
                    // STRUCTURAL (`label_scale`, which the caller biases by node importance), NOT
                    // spatial: importance dominates (×1000) so the most significant labels win the
                    // overlap contest, with magnification only a within-tier tiebreak. Sorting by
                    // magnification alone let a peripheral leaf in open space beat a big central
                    // container — the bug this fixes. Selection dimming scales alpha per node.
                    // status: graph-label-dim
                    let dim = label_dim
                        .and_then(|f| f.get(d.index).copied())
                        .unwrap_or(1.0);
                    // The engine owns the LIVE bundle count: append `· N` when this node is acting as
                    // a bundle (≥ `BUNDLE_COUNT_MIN` descendants still rolled up). The source bakes
                    // only the plain name, so the suffix tracks the dissolving rollup, not a frozen
                    // total. status: code-graph-bundling
                    let text = if rolled >= BUNDLE_COUNT_MIN {
                        format!("{label} · {rolled}")
                    } else {
                        label.clone()
                    };
                    // Placement priority is STRUCTURAL only (importance-biased `label_scale`, ×1000,
                    // magnification a tiebreak). Hover does NOT change priority — emphasis is purely
                    // a paint-time colour/outline (below), so hovering never re-places or drops other
                    // labels. status: graph-label-hit
                    let priority = d.label_scale * 1000.0 + mag;
                    labels.push((priority, egui::pos2(p.x, p.y + r + 2.0), text, alpha * dim, d.index));
                }
                if is_hover {
                    draw.hover_anchor = Some(p);
                    // Suppress the floating tooltip when this node ALREADY has a drawn label on
                    // screen: the label (the node NAME) is conveying identity, so a second floating
                    // box is redundant — instead the de-confliction paint below EMPHASIZES that
                    // existing label on hover. We check LAST frame's placed label set (`label_hits`,
                    // 1-frame lagged like the label hit-test). A node with NO drawn label (small /
                    // culled-label) still gets the floating tooltip. status: graph-label-hit
                    let has_drawn_label =
                        self.label_hits.iter().any(|&(_, i)| i == d.index);
                    if let Some(t) = &d.tooltip
                        && !has_drawn_label
                    {
                        draw.tooltip = Some((p + egui::vec2(10.0, -10.0), t.clone()));
                    }
                    if response_clicked
                        && let Some(path) = &d.click_path
                    {
                        draw.clicked = Some(path.clone());
                    }
                }
            }
        }
        // De-conflict labels: draw highest-priority (most structurally important) first, and skip
        // any whose box would overlap an already-placed one — so a dense centre (esp. under
        // Poincaré) stays readable instead of piling text. Labels fade with their node's rim alpha.
        labels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut placed: Vec<egui::Rect> = Vec::new();
        // Label BUDGET: the density control. Placed top-priority-first, we stop after `budget` labels
        // — so the overview shows a readable handful (the most important) and the count GROWS as you
        // zoom in (`label_zoom` rises), revealing more. `.max(1.0)` floors it at `BASE_LABELS` so some
        // labels ALWAYS show however the fit/zoom is calibrated (the robustness the rank gate lacked).
        // status: graph-label-dim
        // A SMALL graph lifts the budget to MAX_LABELS (≥ its node count), so every label candidate
        // places — only overlap de-confliction can drop one. The large overview keeps the zoom-scaled
        // budget. status: code-graph-scope-hops
        let budget = label_budget(small_graph, label_zoom);
        // Show each distinct label text at most once: code graphs are full of
        // generic repeats (`tests`, `crate`, `mod`), and a field of identical
        // words reads as noise. Labels are priority-sorted (importance desc),
        // so the most significant instance of a name wins and the rest are dropped.
        let mut seen_text: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (_, anchor, text, alpha, node_idx) in &labels {
            if placed.len() >= budget {
                break;
            }
            if !seen_text.insert(text.as_str()) {
                continue;
            }
            // The hovered node's existing label is EMPHASIZED by COLOUR/OUTLINE, not size — full
            // accent text + an accent pill outline — so it pops without changing its geometry (a size
            // bump would reflow neighbouring labels via de-confliction). Only this label's paint
            // diverges; layout is identical to no-hover. status: graph-label-hit
            let hovered_label = hovered == Some(*node_idx);
            let color = if hovered_label {
                self.highlight.color // accent, full opacity — the hover emphasis
            } else {
                fade(self.style.label_color, *alpha)
            };
            // CONSTANT font size: uniform across labels AND independent of zoom — labels are a
            // fixed-size screen-space overlay, so zooming never grows/shrinks them. Zoom feedback comes
            // from the label-DENSITY ramp (more labels appear as you zoom in), not from size. Laid out
            // with a placeholder colour so egui's cross-frame galley cache keys on geometry alone (the
            // rim-fade alpha is applied at paint time below, not baked into the layout job).
            // status: graph-label-dim
            let font = egui::FontId::proportional(label_size.round());
            let galley = painter.layout_no_wrap(text.clone(), font, egui::Color32::PLACEHOLDER);
            let top_left = egui::pos2(anchor.x - galley.size().x / 2.0, anchor.y);
            let rect = egui::Rect::from_min_size(top_left, galley.size()).expand(1.0);
            if placed.iter().any(|r| r.intersects(rect)) {
                continue;
            }
            placed.push(rect);
            // Register the label's rect → its node, so the pane can hit-test labels like nodes.
            // status: graph-label-hit
            draw.label_hits.push((rect, *node_idx));
            // Background pill (keeps text legible over a busy graph). SAME geometry whether hovered or
            // not — the hovered pill is just full-opacity + an accent outline (the colour/outline
            // emphasis that replaces both the old size bump and the suppressed tooltip).
            // status: graph-label-hit
            if let Some(bg) = self.style.label_bg {
                let pill = rect.expand(2.0);
                if hovered_label {
                    painter.rect_filled(pill, 3.0, bg);
                    painter.rect_stroke(
                        pill,
                        3.0,
                        egui::Stroke::new(1.5, self.highlight.color),
                        egui::StrokeKind::Outside,
                    );
                } else {
                    painter.rect_filled(pill, 3.0, fade(bg, *alpha));
                }
            }
            painter.galley(top_left, galley, color);
        }
        draw
    }

    /// Draw edges, mode-aware:
    /// - **Affine**: straight world segment (today's behaviour).
    /// - **Poincaré**: the geodesic between the two disk points, sampled and
    ///   mapped back through the lens → bowed arcs that fade toward the rim.
    /// - **Fisheye**: the straight *world* segment subdivided and each sample
    ///   pushed through the lens, so the edge follows the bulge.
    ///
    /// When `geodesic_edges` is off, a lensed edge draws as a single straight
    /// segment between its two lensed endpoints (no geodesic/bulge sampling),
    /// while still picking up the rim-fade alpha.
    pub(super) fn draw_edges(
        &self,
        painter: &egui::Painter,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        bundles: &BundleState,
        eff_pos: &[egui::Vec2],
        mut gpu: Option<&mut GpuBatch>,
    ) {
        // Spans the GPU edge batch build (the old pan-lag hotspot). On an affine
        // cache hit the segment pushes are dropped (`cached` batch), so panning
        // pays only the cheap loop, not the upload.
        super::profile_scope!("graph_view::draw_edges (gpu batch build)");
        let &EdgeMap { to_screen, disk_to_screen } = map;
        let width = self.style.edge_width;
        let n = self.positions.len();
        // The affine GPU batch stores WORLD positions (the shader applies the view
        // transform), so a pan/zoom needs no edge rebuild. The lens batch + the
        // Painter path bake the final screen point. This only diverges on the
        // affine (`!lens.active()`) branches below, which is the only place
        // `world_space` is ever set.
        let world_space = gpu.as_deref().is_some_and(|b| b.world_space);
        let pt = |w: egui::Vec2| -> egui::Pos2 {
            if world_space { w.to_pos2() } else { to_screen(w) }
        };
        // Layered layout produces poly-line routes (orthogonal between ranks) in
        // the same world space as `positions`. Draw them when not under a lens
        // (the lens path resamples geodesics/bulges from the endpoints instead).
        let routed = self.layout_kind == LayoutKind::Layered
            && !lens.active()
            && !self.edge_routes.is_empty();
        // Zoom-driven auto-bundling: at low zoom many leaf-edges between the same two modules collapse
        // to a single line between their bundle representatives. `seen` dedups those rolled-up pairs
        // (normalized order) so 200 leaf-edges draw once, not 200 overlapping. Skipped entirely when
        // bundling is identity (the common case + every non-code source) — the HashSet stays empty and
        // the per-edge remap is a no-op. status: code-graph-bundling
        let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (mut a, mut b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            // Roll each endpoint up to its visible representative; a self-loop (both ends in the same
            // bundle) draws nothing, and a duplicate rolled-up pair is dropped.
            if !bundles.is_visible(a) || !bundles.is_visible(b) {
                a = bundles.rep(a);
                b = bundles.rep(b);
                if a == b || a >= n || b >= n {
                    continue;
                }
                let key = if a <= b { (a as u32, b as u32) } else { (b as u32, a as u32) };
                if !seen.insert(key) {
                    continue;
                }
            }
            // Per-edge color override (typed edge kinds); the style's single
            // edge color remains the default. status: vault-graph-edge-toggles
            let color = source.edge_color(i).unwrap_or(self.style.edge_color);
            // Effective endpoints follow the un-bundling animation (== `self.positions` when settled),
            // so an edge to a mid-flight node tracks it out of the bundle. status: code-graph-bundling
            let (wa, wb) = (
                eff_pos.get(a).copied().unwrap_or(self.positions[a]),
                eff_pos.get(b).copied().unwrap_or(self.positions[b]),
            );
            if routed {
                let pts: Vec<egui::Pos2> = match self.edge_routes.get(i) {
                    Some(route) if route.len() >= 2 => {
                        route.iter().map(|&p| pt(p)).collect()
                    }
                    // No usable route for this edge — fall back to a straight
                    // segment between its endpoints.
                    _ => vec![pt(wa), pt(wb)],
                };
                emit_edge(painter, gpu.as_deref_mut(), &pts, width, color);
                continue;
            }
            if !lens.active() {
                let seg = [pt(wa), pt(wb)];
                emit_edge(painter, gpu.as_deref_mut(), &seg, width, color);
                continue;
            }
            let alpha = lens
                .rim_alpha(wa, self.fade_start, self.fade_strength)
                .min(lens.rim_alpha(wb, self.fade_start, self.fade_strength));
            // An edge both of whose endpoints have faded out paints nothing — skip
            // its sampling + stroke (mirrors the node cull).
            if alpha <= CULL_ALPHA {
                continue;
            }
            let faded = fade(color, alpha);
            if !self.geodesic_edges {
                let seg = [to_screen(wa), to_screen(wb)];
                emit_edge(painter, gpu.as_deref_mut(), &seg, width, faded);
                continue;
            }
            let pts: Vec<egui::Pos2> = match self.projection.kind {
                ProjectionKind::Poincare => {
                    let (za, zb) = (lens.disk(wa), lens.disk(wb));
                    // Sample density tracks the on-screen chord, not a flat count:
                    // short edges (the bulk of a dense graph) get 2 points.
                    let chord_px = (disk_to_screen(za) - disk_to_screen(zb)).length();
                    let segs = adaptive_segments(chord_px, self.projection.geodesic_segments);
                    hiker_projection::sample_geodesic(za, zb, segs)
                        .into_iter()
                        .map(disk_to_screen)
                        .collect()
                }
                // Fisheye (and any non-Affine fallback): subdivide the world
                // chord, lensing each sample so the edge tracks the distortion.
                _ => {
                    let chord_px = (to_screen(wa) - to_screen(wb)).length();
                    let steps = adaptive_segments(chord_px, self.projection.geodesic_segments).max(1);
                    (0..=steps)
                        .map(|i| {
                            let t = i as f32 / steps as f32;
                            to_screen(wa + (wb - wa) * t)
                        })
                        .collect()
                }
            };
            emit_edge(painter, gpu.as_deref_mut(), &pts, width, faded);
        }
    }

    /// The screen-space polyline of edge `i` `(a, b)` — routed (layered), geodesic
    /// (lensed), or straight — exactly the geometry [`draw_edges`](Self::draw_edges)
    /// uses, shared by the highlight + hover-flow overlays.
    fn edge_screen_polyline(
        &self,
        i: usize,
        a: usize,
        b: usize,
        map: &EdgeMap<'_>,
        lens: &Lens,
    ) -> Vec<egui::Pos2> {
        let &EdgeMap { to_screen, disk_to_screen } = map;
        let (wa, wb) = (self.positions[a], self.positions[b]);
        let routed = self.layout_kind == LayoutKind::Layered
            && !lens.active()
            && !self.edge_routes.is_empty();
        if routed {
            match self.edge_routes.get(i) {
                Some(route) if route.len() >= 2 => route.iter().map(|&p| to_screen(p)).collect(),
                _ => vec![to_screen(wa), to_screen(wb)],
            }
        } else if !lens.active() || !self.geodesic_edges {
            vec![to_screen(wa), to_screen(wb)]
        } else {
            match self.projection.kind {
                ProjectionKind::Poincare => {
                    let (za, zb) = (lens.disk(wa), lens.disk(wb));
                    let chord_px = (disk_to_screen(za) - disk_to_screen(zb)).length();
                    let segs = adaptive_segments(chord_px, self.projection.geodesic_segments);
                    hiker_projection::sample_geodesic(za, zb, segs)
                        .into_iter()
                        .map(disk_to_screen)
                        .collect()
                }
                _ => {
                    let chord_px = (to_screen(wa) - to_screen(wb)).length();
                    let steps =
                        adaptive_segments(chord_px, self.projection.geodesic_segments).max(1);
                    (0..=steps)
                        .map(|s| {
                            let t = s as f32 / steps as f32;
                            to_screen(wa + (wb - wa) * t)
                        })
                        .collect()
                }
            }
        }
    }

    /// Re-stroke the edges incident to `node`, so a hovered / selected node's
    /// connections light up. The shapes are PUSHED into `out` rather than painted
    /// directly: the caller reserves a shape slot at the very start of the pane
    /// paint and fills it afterwards, so the glow renders BELOW the base edges,
    /// node shapes, and labels (bottom-most) while still being recomputed per
    /// frame outside any cached GPU batch. Mirrors [`draw_edges`](Self::draw_edges)'
    /// geometry in screen space, styled by [`HighlightStyle`](super::HighlightStyle):
    /// a soft multi-pass glow scaled by `softness`, at the given `alpha` (the
    /// caller fades it for hover). A no-op when `node` is `None` or `alpha` is ~0
    /// (the caller also gates on the per-mode toggle + `show_edges`).
    /// status: graph-hover-highlight
    pub(super) fn highlight_edge_shapes(
        &self,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        node: Option<usize>,
        alpha: f32,
    ) {
        let Some(h) = node else { return };
        if alpha <= 0.003 {
            return;
        }
        let n = self.positions.len();
        let hs = self.highlight;
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        let tinted = tint(hs.color);
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            // Only edges incident to the highlighted node.
            if a >= n || b >= n || (a != h && b != h) {
                continue;
            }
            let pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            // Soft glow: two translucent wider passes under the core stroke, so the
            // line reads as a gentle halo rather than a hard wire.
            if soft > 0.0 {
                out.push(egui::Shape::line(
                    pts.clone(),
                    egui::Stroke::new(core_w * (1.0 + 3.0 * soft), tinted(alpha * 0.16 * soft)),
                ));
                out.push(egui::Shape::line(
                    pts.clone(),
                    egui::Stroke::new(core_w * (1.0 + 1.4 * soft), tinted(alpha * 0.30)),
                ));
            }
            out.push(egui::Shape::line(pts, egui::Stroke::new(core_w, tinted(alpha))));
        }
    }

    /// The travelling pulse of a hover-flow transition: on every edge directly
    /// connecting the two hover keyframes, a short bright window slides from the
    /// old node's end to the new one's as `t` goes 0→1 — the *positional* half of
    /// the cross-fade (the endpoint glows are the keyframes; this is the
    /// in-between). Keyframes that aren't adjacent simply cross-fade — no path
    /// search. Shapes are pushed into `out` (same bottom-most slot as the glow).
    /// status: graph-hover-flow
    #[allow(clippy::too_many_arguments)]
    pub(super) fn hover_flow_shapes(
        &self,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        from: usize,
        to: usize,
        t: f32,
        alpha: f32,
    ) {
        if alpha <= 0.003 {
            return;
        }
        let n = self.positions.len();
        if from >= n || to >= n {
            return;
        }
        let hs = self.highlight;
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        let tinted = tint(hs.color);
        const WINDOW: f32 = 0.18; // pulse half-length, as a fraction of the edge
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            let forward = a == from && b == to;
            let backward = a == to && b == from;
            if !forward && !backward {
                continue;
            }
            let mut pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            if !forward {
                pts.reverse(); // parameterize 0 at the `from` end
            }
            let window = sub_polyline(&pts, t - WINDOW, t + WINDOW);
            if window.len() < 2 {
                continue;
            }
            // Brighter and slightly wider than the steady glow, so the pulse
            // reads as motion riding the edge.
            out.push(egui::Shape::line(
                window.clone(),
                egui::Stroke::new(core_w * (1.0 + 2.0 * soft), tinted(alpha * 0.35)),
            ));
            out.push(egui::Shape::line(window, egui::Stroke::new(core_w * 1.3, tinted(alpha))));
        }
    }

    /// Advance the fluid-highlight field one frame and push its shapes: energy is
    /// injected at the hovered node, diffuses across edges, drifts *downhill* in
    /// the hop-distance potential of the selected node (the highlight gravitates
    /// toward it), and decays everywhere — so dragging the pointer across the
    /// graph leaves a glowing wake that drains toward the selection. Rendered as
    /// per-edge gradient strokes (alpha lerped between the endpoint energies)
    /// plus a soft halo under each energized node, all in the caller's
    /// bottom-most slot. O(V+E) per frame; repaints are requested only while any
    /// energy remains. status: graph-hover-fluid
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fluid_advance_and_shapes(
        &mut self,
        ui: &egui::Ui,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        hovered: Option<usize>,
    ) {
        let n = self.positions.len();
        if n == 0 {
            return;
        }
        if self.fluid_energy.len() != n {
            self.fluid_energy = vec![0.0; n];
            self.fluid_potential_for = None;
        }
        let edges = source.edges();
        // The gravity field: hop distance from the selected node. Flat (no drift)
        // when nothing is selected. Rebuilt on selection/graph change.
        if self.fluid_potential.len() != n
            || self.fluid_potential_for != self.selected_node
            || self.fluid_potential_epoch != self.layout_epoch
        {
            self.fluid_potential = hop_potential(n, &edges, self.selected_node);
            self.fluid_potential_for = self.selected_node;
            self.fluid_potential_epoch = self.layout_epoch;
        }
        let now = ui.input(|i| i.time);
        let dt = ((now - self.fluid_last_time) as f32).clamp(0.0, 1.0 / 30.0);
        self.fluid_last_time = now;

        // Source: the hovered node is the faucet — and it RAMPS rather than
        // snapping to full, so hovering a new node fades its glow up (~150ms to
        // saturation) instead of hard-activating while the old wake drains.
        const INJECT: f32 = 9.0; // 1/s
        if let Some(h) = hovered
            && h < n
        {
            let e = &mut self.fluid_energy[h];
            *e += (1.0 - *e) * (INJECT * dt).min(1.0);
        }
        // Transfer along edges: symmetric diffusion + directional drift downhill.
        // Per-step fractions are clamped well below 0.5 so the explicit update
        // can't oscillate regardless of frame rate.
        const DIFFUSE: f32 = 3.0; // 1/s
        const DRIFT: f32 = 6.0; // 1/s
        const TAU: f32 = 0.6; // decay time constant, s
        let k_diff = (DIFFUSE * dt).min(0.4);
        let k_drift = (DRIFT * dt).min(0.4);
        for &(a, b) in &edges {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            let d = (self.fluid_energy[a] - self.fluid_energy[b]) * 0.5 * k_diff;
            self.fluid_energy[a] -= d;
            self.fluid_energy[b] += d;
            let (hi, lo) = match self.fluid_potential[a]
                .partial_cmp(&self.fluid_potential[b])
                .unwrap_or(std::cmp::Ordering::Equal)
            {
                std::cmp::Ordering::Greater => (a, b),
                std::cmp::Ordering::Less => (b, a),
                std::cmp::Ordering::Equal => continue,
            };
            let drift = self.fluid_energy[hi] * k_drift;
            self.fluid_energy[hi] -= drift;
            self.fluid_energy[lo] += drift;
        }
        let decay = (-dt / TAU).exp();
        let mut any = false;
        for v in &mut self.fluid_energy {
            *v = (*v * decay).clamp(0.0, 1.0);
            if *v < 0.004 {
                *v = 0.0;
            } else {
                any = true;
            }
        }
        if !any {
            return;
        }
        ui.ctx().request_repaint();

        // Render: gradient strokes on energized edges, halos under energized nodes.
        let hs = self.highlight;
        let tinted = tint(hs.color);
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        for (i, (a, b)) in edges.into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            let (ea, eb) = (self.fluid_energy[a], self.fluid_energy[b]);
            if ea.max(eb) < 0.01 {
                continue;
            }
            let pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            gradient_strokes(out, &pts, ea * hs.opacity, eb * hs.opacity, core_w, soft, &tinted);
        }
        for (i, &v) in self.fluid_energy.iter().enumerate() {
            if v < 0.01 {
                continue;
            }
            let c = (map.to_screen)(self.positions[i]);
            let r = core_w * (1.5 + 3.5 * v);
            out.push(egui::Shape::circle_filled(c, r * 1.8, tinted(v * hs.opacity * 0.18)));
            out.push(egui::Shape::circle_filled(c, r, tinted(v * hs.opacity * 0.45)));
        }
    }

    /// Re-resolve the preview text when the hovered node changes.
    pub(super) fn refresh_preview(&mut self, source: &dyn Source, idx: usize) {
        if self.preview.hovered_index == Some(idx) {
            return;
        }
        let resolved = source.preview_for(idx);
        self.preview.hovered_index = Some(idx);
        self.preview.title = resolved.as_ref().map(|(t, _)| t.clone());
        self.preview.body = resolved.map(|(_, b)| b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 1 (code-graph-scope-hops): a SMALL graph (≤ `SMALL_GRAPH_LABELS`) — a filtered Hops
    /// subgraph or a single-package SCIP — bypasses the depth gate and lifts the budget to
    /// `MAX_LABELS`, so every node is a label candidate and the budget never caps below the node
    /// count. The huge overview (> threshold) is unchanged: depth gate in force + the zoom-scaled
    /// budget.
    #[test]
    fn small_graph_threshold_and_budget() {
        // The small/large split is purely the node count vs the threshold.
        let small = |n: usize| n <= SMALL_GRAPH_LABELS;
        assert!(small(1), "a single node is small");
        assert!(small(40), "a few-dozen-node Hops subgraph is small");
        assert!(small(SMALL_GRAPH_LABELS), "exactly the threshold is still small (≤)");
        assert!(!small(SMALL_GRAPH_LABELS + 1), "one over the threshold is the large overview");
        assert!(!small(10_000), "the ~10k overview is large");

        // SMALL graph → budget is lifted to MAX_LABELS at any zoom (≥ the node count for any small
        // graph, since SMALL_GRAPH_LABELS < MAX_LABELS), so all candidates can place.
        assert!(SMALL_GRAPH_LABELS < MAX_LABELS, "the small budget covers every small-graph node");
        assert_eq!(label_budget(true, 1.0), MAX_LABELS, "small graph at overview zoom = full budget");
        assert_eq!(label_budget(true, 5.0), MAX_LABELS, "small graph zoomed-in = full budget");

        // LARGE graph → the historical zoom-scaled budget: floored at BASE_LABELS, capped at
        // MAX_LABELS, growing with zoom (UNCHANGED by this fix).
        assert_eq!(label_budget(false, 1.0), BASE_LABELS, "overview floors at BASE_LABELS");
        assert_eq!(label_budget(false, 0.5), BASE_LABELS, "below-fit zoom still floors at BASE_LABELS");
        assert!(
            label_budget(false, 2.0) > BASE_LABELS && label_budget(false, 2.0) <= MAX_LABELS,
            "zooming in grows the large-graph budget toward the cap"
        );
        assert_eq!(label_budget(false, 1000.0), MAX_LABELS, "deep zoom caps at MAX_LABELS");
    }
}
