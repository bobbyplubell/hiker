//! Shared force/tree graph rendering engine for the vault link-graph and
//! the cluster-tree graph. Owns pan/zoom, layout (the background force
//! worker plus the inline tree-position math), the eye-icon view-options
//! menu, and the node/edge/label/hover/preview paint loop. Each caller
//! supplies a [`Source`] that turns its own data — a `petgraph` vault graph
//! or a slice of cluster `EditableNode`s — into per-frame [`NodeDescriptor`]s
//! plus the edge and layout-tree topology, so one code path renders both
//! views with different colors and options.

use std::collections::HashMap;

use hiker_graph::{LayoutKind, LayoutParams, LayoutTree};
use hiker_projection::{forward, Complex, Mobius, ProjectionConfig, ProjectionKind};
use hiker_theme as theme;

use crate::force_graph::View;
use graph_widgets::force_layout::LayoutWorker;
use graph_widgets::{
    horizontal_tree_positions, layered_layout, radial_positions, vertical_tree_positions,
};

mod edges;
mod layout;
mod nav;
mod panes;

use layout::{adaptive_anchor_stiffness, build_warm_seed, change_fraction, scatter};

const ZOOM_MIN: f32 = 0.005;
const ZOOM_MAX: f32 = 6.0;

/// Fraction of the pane's shorter half-dimension the locked Poincaré disk fills,
/// leaving a small margin so the boundary ring isn't flush against the edge.
const DISK_FILL: f32 = 0.92;

/// The locked Poincaré disk frame for a pane: its centre (the pane centre) and
/// radius (`DISK_FILL` of the shorter half-dimension, times `zoom`). Deliberately
/// a pure function of `pane_rect` + `zoom` *only* — it must NOT depend on [`View`]
/// pan/zoom, so the disk stays fixed-CENTERED to the pane as the user navigates
/// (the disk IS the viewport; navigation is Möbius drag + click fly-to, never
/// affine pan/zoom). Scroll-zoom scales the RADIUS only: the centre is
/// zoom-invariant (always the pane centre), so the disk grows/shrinks centred and
/// never drifts. At `zoom > 1` the disk may exceed the pane and clip — the user
/// Möbius-drags to explore; `zoom = 1.0` is fit.
fn poincare_disk(pane_rect: egui::Rect, zoom: f32) -> (egui::Pos2, f32) {
    let radius = 0.5 * pane_rect.size().min_elem() * DISK_FILL * zoom;
    (pane_rect.center(), radius)
}

/// Scroll-zoom clamp for the locked Poincaré disk radius.
const POINCARE_ZOOM_MIN: f32 = 0.3;
const POINCARE_ZOOM_MAX: f32 = 5.0;

/// Persistent per-view engine state: pan/zoom, node positions, the active
/// layout + its background worker, the configurable [`Style`], the common
/// toggles, and the hover-preview cache. The graph's domain data lives on
/// the caller (via its [`Source`]), not here.
pub struct State {
    pub positions: Vec<egui::Vec2>,
    /// Poly-line route for each edge, aligned to [`Source::edges`] order.
    /// Populated only by the [`LayoutKind::Layered`] layout (which routes edges
    /// orthogonally between ranks); empty for every other kind, where edges
    /// draw as straight segments.
    pub edge_routes: Vec<Vec<egui::Vec2>>,
    pub layout_kind: LayoutKind,
    /// Rank direction for the [`LayoutKind::Layered`] layout (Top-Down vs
    /// Left-Right …). Ignored by every other kind. Default
    /// [`RankDir::Tb`](hiker_graph::RankDir::Tb).
    pub layered_rankdir: hiker_graph::RankDir,
    /// `Some` only while `layout_kind == ForceDirected` and the worker is
    /// still iterating toward convergence.
    pub worker: Option<LayoutWorker>,
    /// True after a (re)build — `ui()` refits pan/zoom on the next paint so
    /// the user never opens to an off-screen layout.
    pub needs_fit: bool,
    pub view: View,
    pub style: Style,
    pub toggles: Toggles,
    pub preview: PreviewCache,
    /// Lens applied to world positions before the affine `view` mapping. With
    /// the default [`ProjectionKind::Affine`] the lens is the identity, so the
    /// graph renders byte-identically to a non-projected view; selecting
    /// Fisheye/Poincaré warps the layout around its centroid focus.
    pub projection: ProjectionConfig,
    /// Whether to stroke the Poincaré unit-disk boundary circle.
    pub show_boundary: bool,
    /// Hyperbolic navigation transform applied *after* the Poincaré lens (and
    /// only for Poincaré): drag-to-recentre and click fly-to compose into it.
    /// Identity for a freshly-built / Reset view, and ignored under
    /// Off/Fisheye so those modes stay byte-identical.
    pub nav: Mobius,
    /// Scroll-zoom factor for the locked Poincaré disk RADIUS (the disk centre
    /// stays the pane centre regardless). `1.0` = fit-to-pane (default); larger
    /// grows the disk (content bigger, may clip at the pane edges); smaller
    /// shrinks it. Clamped to `[POINCARE_ZOOM_MIN, POINCARE_ZOOM_MAX]`. Only the
    /// interactive pane reads scroll into this; the read-only overview stays at
    /// `1.0`. Reset alongside `nav`/`flyto`. Poincaré-only — ignored by the
    /// affine regimes.
    pub poincare_zoom: f32,
    /// Whether to paint the always-on corner Poincaré overview minimap after
    /// the main graph. Off by default, so an untouched view is unchanged.
    pub show_minimap: bool,
    /// Which pane corner the minimap occupies.
    pub minimap_corner: Corner,
    /// Side of the minimap as a fraction of the shorter pane dimension.
    pub minimap_size: f32,
    /// Whether the minimap reads as a clipped circle or a filled square.
    pub minimap_shape: MinimapShape,
    /// Click-to-expand swap target: when `true` the Poincaré overview promotes
    /// to fill the pane while the Euclidean main view demotes into the corner;
    /// `false` settles back to the corner overview. The actual layout follows
    /// `swap_t`, which eases toward this each frame.
    pub minimap_expanded: bool,
    /// Eased swap progress in `[0, 1]`: `0` ⇒ Euclidean-in-full +
    /// Poincaré-in-corner (today's layout), `1` ⇒ Poincaré-in-full +
    /// Euclidean-in-corner. Advances toward `minimap_expanded ? 1 : 0`.
    swap_t: f32,
    /// When set (demo/snapshot only), `swap_t` is held fixed and never advances
    /// toward `minimap_expanded`, so a filmstrip can capture intermediate frames.
    swap_pinned: bool,
    /// In-flight click fly-to animation, if any.
    flyto: Option<FlyTo>,
    /// Magnification at or above which a node renders at FULL detail (circle /
    /// square + label). Read clamped so `lod_marker_mag < lod_full_mag`.
    pub lod_full_mag: f32,
    /// Magnification at or above which a node renders as a small DOT (below it a
    /// tiny MARKER point); below `lod_full_mag` and at/above this is the DOT tier.
    pub lod_marker_mag: f32,
    /// Where the interactive pane's lens focus sits each frame.
    pub focus_mode: FocusMode,
    /// The node the lens focuses on under [`FocusMode::Selection`] — set on click.
    focus_node: Option<usize>,
    /// Whether Poincaré/Fisheye edges bow along geodesics/the bulge. When
    /// `false`, edges draw as straight segments even under a lens.
    pub geodesic_edges: bool,
    /// Whether a Poincaré node click animates a fly-to recentre.
    pub flyto_enabled: bool,
    /// Fly-to glide duration in seconds (range 0.1..=2.0).
    pub flyto_duration: f32,
    /// Poincaré boundary-fade onset: disk radii at or below this stay fully
    /// opaque; beyond it the fade ramps in. Range 0.0..=1.0.
    pub fade_start: f32,
    /// Poincaré boundary-fade strength: how much the periphery fades at the rim
    /// (0 = no fade, 1 = fully transparent at the boundary). Range 0.0..=1.0.
    pub fade_strength: f32,
    /// Last force-layout positions keyed by [`Source::node_key`], captured each
    /// frame in `ui()`. On the next same-kind force rebuild these warm-seed and
    /// anchor the retained nodes so the layout morphs instead of reshuffling.
    /// Empty until the source supplies stable keys (the default does not).
    prev_positions: HashMap<String, egui::Vec2>,
    /// Adjacency of the *last* laid-out force graph, keyed by
    /// [`Source::node_key`] (each value the sorted neighbour-key list). Captured
    /// at the end of `recompute_layout`. The next same-kind force rebuild
    /// compares it against the new wiring to measure how *structurally* the
    /// graph changed (the `change_fraction` that scales anchor stiffness down for
    /// big re-clusterings — see [`adaptive_anchor_stiffness`]). Empty until the
    /// source supplies stable keys.
    prev_adjacency: HashMap<String, Vec<String>>,
    /// The layout kind the *last* `recompute_layout` produced, so a same-kind
    /// data rebuild can warm-seed while a kind switch starts fresh. `None`
    /// until the first layout.
    last_layout_kind: Option<LayoutKind>,
    /// Anchor-spring stiffness for warm force rebuilds: `0` = lively/free
    /// re-layout, higher = nodes stay put as the graph changes. Only effective
    /// when the source supplies stable [`node_key`](Source::node_key)s.
    pub anchor_stiffness: f32,
}

/// Pane corner the overview minimap is anchored to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Frame style of the overview minimap: a clipped Poincaré disk or a filled
/// square with the disk clipped to it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MinimapShape {
    Circle,
    Square,
}

/// Where the lens focus (the world point that maps to the disk centre) sits each
/// frame, for the interactive pane. The lens *scale* is always the centroid
/// extent — only the focus moves — so a moving focus pans the warp without
/// rescaling the layout.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusMode {
    /// Focus stays on the layout centroid (today's behaviour).
    LockedCenter,
    /// Focus tracks the cursor (the warp follows the pointer); falls back to the
    /// centroid when the pane isn't hovered.
    Cursor,
    /// Focus locks onto the last-clicked node (the disk recentres on it); falls
    /// back to the centroid until a node is clicked.
    Selection,
}

/// A click fly-to in progress: the disk centre glides from `start_center` to
/// `target_center` (both pre-nav disk points) over `dur` seconds, easing out.
/// Each frame rebuilds `nav` as the pure recentre that maps the eased point to
/// the disk origin.
#[derive(Clone, Copy)]
struct FlyTo {
    start_center: Complex,
    target_center: Complex,
    t: f32,
    dur: f32,
}

/// `1 − (1 − t)³` — decelerating ease for the fly-to glide.
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Linear interpolation on the complex plane (component-wise on re/im).
fn lerp_complex(a: Complex, b: Complex, t: f32) -> Complex {
    Complex::new(a.re + (b.re - a.re) * t, a.im + (b.im - a.im) * t)
}

/// Interpolate two rects by lerping their min + max corners — `t = 0` ⇒ `a`,
/// `t = 1` ⇒ `b`. Drives the expand swap (full ⇄ corner) animation.
fn lerp_rect(a: egui::Rect, b: egui::Rect, t: f32) -> egui::Rect {
    egui::Rect::from_min_max(a.min.lerp(b.min, t), a.max.lerp(b.max, t))
}

/// Seconds the expand swap animation takes end to end.
const SWAP_DURATION: f32 = 0.35;

/// Per-frame lens: the `world → lensed-world` step inserted *before* the
/// affine [`View::screen_mapper`]. Holds the focus (the world point that maps
/// to the disk centre) and `lens_scale` (the world distance that normalises
/// the lens so `tanh` doesn't saturate), both cached once per paint. With an
/// [`ProjectionKind::Affine`] config every method is the identity.
#[derive(Clone, Copy)]
struct Lens {
    cfg: ProjectionConfig,
    focus: egui::Vec2,
    /// World radius the lens normalises against; lensed points occupy a disk
    /// of this radius around `focus`.
    scale: f32,
    /// Hyperbolic navigation transform, applied to the disk point *after* the
    /// lens — Poincaré only. Identity (no effect) for Off/Fisheye.
    nav: Mobius,
}

/// The layout centroid and the centroid extent (farthest node distance from the
/// centroid, floored at 1.0). The extent normalises the lens so `tanh` doesn't
/// saturate; computing it from the centroid — not from the lens focus — keeps
/// the layout scale fixed when the focus moves (focus-modes).
fn centroid_scale(positions: &[egui::Vec2]) -> (egui::Vec2, f32) {
    if positions.is_empty() {
        return (egui::Vec2::ZERO, 1.0);
    }
    let mut sum = egui::Vec2::ZERO;
    for &p in positions {
        sum += p;
    }
    let centroid = sum / positions.len() as f32;
    let scale = positions
        .iter()
        .map(|&p| (p - centroid).length())
        .fold(0.0_f32, f32::max)
        .max(1.0);
    (centroid, scale)
}

impl Lens {
    /// Build the per-frame lens. `focus` is the world point that maps to the disk
    /// centre (caller-chosen per focus-mode); `scale` is always the centroid
    /// extent (floored at 1.0) so a moving focus pans the warp without rescaling
    /// the layout. `nav` is the hyperbolic navigation transform (Poincaré only).
    fn new(cfg: ProjectionConfig, nav: Mobius, focus: egui::Vec2, positions: &[egui::Vec2]) -> Self {
        let (_, scale) = centroid_scale(positions);
        Self { cfg, focus, scale, nav }
    }

    /// Build the lens with the centroid as focus — the centred overview used by
    /// the read-only panes and by callers that don't pick an explicit focus.
    fn centred(cfg: ProjectionConfig, nav: Mobius, positions: &[egui::Vec2]) -> Self {
        let (centroid, scale) = centroid_scale(positions);
        Self { cfg, focus: centroid, scale, nav }
    }

    /// Whether the lens warps at all (false ⇒ every method is the identity).
    fn active(&self) -> bool {
        self.cfg.kind != ProjectionKind::Affine
    }

    /// The disk point for a world position: `forward((w − focus) / scale)`,
    /// then the `nav` Möbius transform for Poincaré (identity otherwise). All
    /// the downstream methods (`world_to_lensed`, `magnification`, `rim_alpha`)
    /// route through here, so they pick up navigation automatically. Fisheye
    /// never applies `nav`, keeping its affine pan unchanged.
    fn disk(&self, w: egui::Vec2) -> Complex {
        let rel = (w - self.focus) / self.scale;
        let z = forward(Complex::from([rel.x, rel.y]), self.cfg);
        if self.cfg.kind == ProjectionKind::Poincare {
            self.nav.apply(z)
        } else {
            z
        }
    }

    /// Map a disk point back to lensed-world space (the inverse of the
    /// `(w − focus) / scale` framing, *not* of the lens remap).
    fn disk_to_world(&self, z: Complex) -> egui::Vec2 {
        self.focus + egui::vec2(z.re, z.im) * self.scale
    }

    /// `world → lensed-world`. Identity under Affine; otherwise the lensed
    /// point lives back in world space, in a disk of radius `scale` around
    /// `focus`, ready for the affine `screen_mapper`.
    fn world_to_lensed(&self, w: egui::Vec2) -> egui::Vec2 {
        if !self.active() {
            return w;
        }
        self.disk_to_world(self.disk(w))
    }

    /// Local linear magnification at a world position (1.0 under Affine).
    fn magnification(&self, w: egui::Vec2) -> f32 {
        if !self.active() {
            return 1.0;
        }
        hiker_projection::magnification(self.disk(w), self.cfg)
    }

    /// Rim-fade alpha multiplier (1.0 unless Poincaré, where the periphery
    /// recedes toward the disk boundary). `fade_start` is the disk radius the
    /// fade begins at; `fade_strength` scales how transparent the rim becomes.
    fn rim_alpha(&self, w: egui::Vec2, fade_start: f32, fade_strength: f32) -> f32 {
        if self.cfg.kind != ProjectionKind::Poincare {
            return 1.0;
        }
        let r = self.disk(w).abs();
        if r <= fade_start {
            return 1.0;
        }
        let denom = (1.0 - fade_start).max(f32::EPSILON);
        let t = ((r - fade_start) / denom).clamp(0.0, 1.0);
        (1.0 - fade_strength.clamp(0.0, 1.0) * smoothstep(t)).clamp(0.0, 1.0)
    }
}

/// Smooth Hermite step `t²·(3 − 2t)` on a pre-normalised `t ∈ [0, 1]`.
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// LOD render tier for a node, selected from its magnification.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lod {
    /// Full detail: the descriptor's circle/square + label (zoom rule still
    /// gates the label).
    Full,
    /// A small filled dot, no label, no hover ring.
    Dot,
    /// A tiny point, no label.
    Marker,
}

/// Multiply an egui colour's alpha by `factor` (clamped). Used for rim fade.
fn fade(color: egui::Color32, factor: f32) -> egui::Color32 {
    if factor >= 1.0 {
        return color;
    }
    let a = (color.a() as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

/// View toggles common to every graph. Caller-specific toggles (the vault
/// "Orphans", the cluster "Leaves", the review tab's "Live preview") live on
/// the caller and are surfaced through the `extra_toggles` argument of
/// [`State::view_options_menu`].
#[derive(Clone, Copy)]
pub struct Toggles {
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_preview: bool,
}

/// Hover-preview card text, refreshed only when the hovered node changes so
/// we don't re-read the note body every frame.
#[derive(Default)]
pub struct PreviewCache {
    hovered_index: Option<usize>,
    title: Option<String>,
    body: Option<String>,
}

/// Per-node draw + hit-test descriptor produced by a [`Source`] each frame.
/// The caller computes `fill`/`radius`/`shape` from its own data and the
/// active [`Style`]; the engine never hardcodes a coloring scheme.
pub struct NodeDescriptor {
    /// Index into `positions` — also the hover/preview identity.
    pub index: usize,
    pub world_pos: egui::Vec2,
    /// Base radius in world units, before `node_scale`/zoom.
    pub radius: f32,
    pub shape: NodeShape,
    pub fill: egui::Color32,
    pub resting_stroke: egui::Stroke,
    pub hover_stroke: egui::Stroke,
    pub label: Option<String>,
    /// Labels draw only at or above this zoom (0.0 = always).
    pub label_min_zoom: f32,
    /// `Some` makes the node clickable; the path is returned from [`State::ui`]
    /// for the caller to open.
    pub click_path: Option<String>,
    /// Hover tooltip text (the cluster graph shows node names; the vault
    /// graph passes `None`).
    pub tooltip: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeShape {
    Circle,
    Square,
}

/// World-space layout sizing. The vault graph and cluster graph settled on
/// different scales (1000² vs 800² boxes), so each caller passes its own.
#[derive(Clone, Copy)]
pub struct LayoutConfig {
    /// Area handed to the tree layouts.
    pub area: f32,
    /// Full width of the random scatter box for the force seed.
    pub seed_box: f32,
}

/// The caller-supplied bridge from domain data to the engine. Vault and
/// cluster panels each implement it over their own storage.
pub trait Source {
    /// Total node count (length of the `positions` vector). Includes nodes
    /// the caller hides in [`Source::nodes`] (orphans / leaves) so edge and
    /// layout indices stay stable.
    fn node_count(&self) -> usize;

    /// Build the visible node descriptors for this frame. `positions` is the
    /// engine's current layout; the caller reads `positions[i]` for each node
    /// it emits and skips its own hidden nodes.
    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor>;

    /// Edges as `positions`-index pairs. Used both for drawing and as the
    /// force-worker topology.
    fn edges(&self) -> Vec<(u32, u32)>;

    /// Spanning/parent tree for a tree layout. The vault graph BFS/DFS-es a
    /// spanning tree per kind; the cluster graph uses its parent tree for
    /// all. Only called for non-force kinds.
    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree;

    /// `(title, body)` for the hover-preview card of node `index`. Called
    /// once per hover change. Returns `None` to suppress the card.
    fn preview_for(&self, index: usize) -> Option<(String, String)>;

    /// A stable per-node identity that survives a rebuild, used by the
    /// force-directed layout to map old node positions onto the new graph so a
    /// re-cluster / vault-rebuild *morphs* smoothly instead of reshuffling: a
    /// retained key keeps (and is anchored toward) its prior position; a node
    /// whose key is new settles in fresh.
    ///
    /// The default returns `None` for every node, which opts out entirely —
    /// the layout then falls back to today's fresh random scatter on every
    /// rebuild, byte-identical to the pre-anchor behaviour.
    fn node_key(&self, index: usize) -> Option<String> {
        let _ = index;
        None
    }
}

impl State {
    /// Fresh engine state with the given style + starting layout.
    pub fn new(style: Style, layout_kind: LayoutKind) -> Self {
        Self {
            positions: Vec::new(),
            edge_routes: Vec::new(),
            layout_kind,
            layered_rankdir: hiker_graph::RankDir::Tb,
            worker: None,
            needs_fit: true,
            view: View::default(),
            style,
            toggles: Toggles {
                show_labels: true,
                show_edges: true,
                show_preview: false,
            },
            preview: PreviewCache::default(),
            projection: ProjectionConfig::default(),
            show_boundary: true,
            nav: Mobius::identity(),
            poincare_zoom: 1.0,
            show_minimap: false,
            minimap_corner: Corner::BottomRight,
            minimap_size: 0.28,
            minimap_shape: MinimapShape::Circle,
            minimap_expanded: false,
            swap_t: 0.0,
            swap_pinned: false,
            flyto: None,
            lod_full_mag: 0.5,
            lod_marker_mag: 0.15,
            focus_mode: FocusMode::LockedCenter,
            focus_node: None,
            geodesic_edges: true,
            flyto_enabled: true,
            flyto_duration: 0.6,
            fade_start: 0.6,
            fade_strength: 1.0,
            prev_positions: HashMap::new(),
            prev_adjacency: HashMap::new(),
            last_layout_kind: None,
            anchor_stiffness: 0.2,
        }
    }

    /// Force the lens focus onto a specific node — for demo/snapshot frames that
    /// need a peripheral focus without a real click. Switches to
    /// [`FocusMode::Selection`] so the focus actually takes effect. Not part of
    /// the interactive flow.
    #[doc(hidden)]
    pub const fn set_focus_node_for_demo(&mut self, index: usize) {
        self.focus_mode = FocusMode::Selection;
        self.focus_node = Some(index);
    }

    /// Force the swap progress directly — for headless snapshot/demo filmstrips
    /// that need to capture intermediate frames without driving the animation
    /// over real time. Not part of the interactive flow.
    #[doc(hidden)]
    pub const fn set_swap_t_for_demo(&mut self, t: f32) {
        self.swap_t = t.clamp(0.0, 1.0);
        self.swap_pinned = true;
    }

    /// (Re)compute positions for the current `layout_kind`. Force-directed
    /// spawns the background worker from a random scatter; the tree layouts
    /// run inline off `source.layout_tree`. Always flags `needs_fit` so
    /// `ui()` reframes on the next paint.
    pub fn recompute_layout(&mut self, source: &dyn Source, cfg: LayoutConfig) {
        self.worker = None;
        self.needs_fit = true;
        // Routed edges only exist for the Layered layout; clear any stale routes
        // up front, and the Layered arm below repopulates them.
        self.edge_routes.clear();
        let n = source.node_count();
        if n == 0 {
            self.positions.clear();
            return;
        }
        match self.layout_kind {
            LayoutKind::Layered => {
                let result = layered_layout(n, &source.edges(), None, self.layered_rankdir);
                self.positions = result.positions;
                self.edge_routes = result.edge_routes;
            }
            LayoutKind::ForceDirected => {
                let edges = source.edges();
                // Warm + anchored only when the *previous* layout was also
                // force-directed AND we have captured history to map onto — so a
                // kind switch or the very first build starts fresh, but a
                // same-kind data rebuild morphs from the prior layout.
                let same_kind = self.last_layout_kind == Some(LayoutKind::ForceDirected);
                let have_history = !self.prev_positions.is_empty();
                // `bound` is only a runaway-force safety belt; keep it well
                // clear of any natural equilibrium for realistic graphs.
                if same_kind && have_history {
                    // Adaptive anchoring: a small clustering scrub (membership
                    // barely moves) keeps the full `self.anchor_stiffness` and a
                    // tight warm seed so the layout morphs coherently; a big
                    // structural re-clustering scales BOTH the anchor stiffness
                    // and the warm-seed's grip toward 0, so retained nodes aren't
                    // pinned to stale spots that fight the new edge structure
                    // (which would otherwise leave the layout tangled — see
                    // `adaptive_anchor_stiffness`).
                    let change_fraction =
                        change_fraction(source, &self.prev_adjacency, &edges, n);
                    let effective =
                        adaptive_anchor_stiffness(self.anchor_stiffness, change_fraction);
                    let (seed, anchors) = build_warm_seed(
                        source,
                        &self.prev_positions,
                        &edges,
                        n,
                        cfg.seed_box,
                        change_fraction,
                    );
                    self.positions = seed.clone();
                    self.worker = Some(LayoutWorker::spawn_anchored(
                        seed,
                        edges,
                        LayoutParams {
                            bound: 50_000.0,
                            anchor_stiffness: effective,
                            ..LayoutParams::default()
                        },
                        anchors,
                    ));
                    // Refresh the wiring snapshot for the NEXT rebuild's
                    // structural-change measurement (after `change_fraction`
                    // consumed the prior one).
                    self.capture_adjacency(source, n);
                } else {
                    let seed = scatter(n, cfg.seed_box);
                    self.positions = seed.clone();
                    self.worker = Some(LayoutWorker::spawn(
                        seed,
                        edges,
                        LayoutParams {
                            bound: 50_000.0,
                            ..LayoutParams::default()
                        },
                    ));
                    // Seed the wiring snapshot so the next same-kind rebuild can
                    // measure structural change against this fresh layout.
                    self.capture_adjacency(source, n);
                }
            }
            kind => {
                let tree = source.layout_tree(kind);
                self.positions = match kind {
                    LayoutKind::Radial => radial_positions(&tree, cfg.area),
                    LayoutKind::VerticalTree => vertical_tree_positions(&tree, cfg.area),
                    LayoutKind::HorizontalTree => horizontal_tree_positions(&tree, cfg.area),
                    // ForceDirected and Layered are handled by their own arms above.
                    LayoutKind::ForceDirected | LayoutKind::Layered => unreachable!(),
                };
            }
        }
        // Record what we just laid out so the next rebuild can tell a same-kind
        // data change (→ warm + anchored) from a kind switch (→ fresh). Keep
        // `prev_positions` intact for the warm path.
        self.last_layout_kind = Some(self.layout_kind);
    }

    /// Snapshot the current force graph's wiring into `prev_adjacency`, keyed by
    /// stable [`Source::node_key`]: each retained node maps to its sorted list of
    /// neighbour keys. The next same-kind force rebuild compares the new wiring
    /// against this to compute how *structurally* the clustering changed. Nodes
    /// without a key are skipped (no stable identity to track). A no-op-ish
    /// `clear` keeps the map from accumulating keys of long-gone nodes.
    fn capture_adjacency(&mut self, source: &dyn Source, n: usize) {
        self.prev_adjacency.clear();
        let edges = source.edges();
        // Gather neighbour keys per index first, then key the whole thing by the
        // node's own key.
        let mut nbr_keys: Vec<Vec<String>> = vec![Vec::new(); n];
        for &(a, b) in &edges {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n || a == b {
                continue;
            }
            if let Some(kb) = source.node_key(b) {
                nbr_keys[a].push(kb);
            }
            if let Some(ka) = source.node_key(a) {
                nbr_keys[b].push(ka);
            }
        }
        for (i, mut keys) in nbr_keys.into_iter().enumerate() {
            if let Some(k) = source.node_key(i) {
                keys.sort_unstable();
                keys.dedup();
                self.prev_adjacency.insert(k, keys);
            }
        }
    }

    /// Allocate the canvas, run pan/zoom input, and draw the graph from
    /// `source`: background, edges, nodes + labels, hover ring, tooltip, and
    /// (when enabled) the hover-preview card. Returns the path of a clicked
    /// node for the caller to open, if any. The host supplies `draw_preview`
    /// so the preview card's painter stays an app concern.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        source: &dyn Source,
        draw_preview: impl Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2),
    ) -> Option<String> {
        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        // Pull fresh positions while the force worker is still settling.
        let worker_running = self.worker.as_ref().is_some_and(LayoutWorker::is_running);
        if worker_running
            && let Some(w) = self.worker.as_ref()
        {
            w.snapshot_into(&mut self.positions);
            ui.ctx().request_repaint();
        }

        // Capture the live positions keyed by stable identity so the next
        // same-kind force rebuild can warm-seed + anchor from them. Done every
        // frame (cheap at our graph sizes) so the latest layout is always
        // available; a no-op when the source supplies no `node_key`s.
        for i in 0..self.positions.len() {
            if let Some(k) = source.node_key(i) {
                self.prev_positions.insert(k, self.positions[i]);
            }
        }

        // Advance the expand swap toward its target; repaint while in flight.
        // A pinned `swap_t` (demo/snapshot) is held fixed.
        if !self.swap_pinned {
            let dt = ui.input(|i| i.stable_dt);
            let target = if self.minimap_expanded { 1.0 } else { 0.0 };
            if self.swap_t != target && dt > 0.0 {
                let step = dt / SWAP_DURATION;
                if (target - self.swap_t).abs() <= step {
                    self.swap_t = target;
                } else {
                    self.swap_t += step * (target - self.swap_t).signum();
                }
            }
            if self.swap_t > 0.0 && self.swap_t < 1.0 {
                ui.ctx().request_repaint();
            }
        }

        let nodes = source.nodes(&self.positions, &self.style);
        let inputs = PaneInputs { source, nodes: &nodes, draw_preview: &draw_preview };

        // FAST PATH — the common case is byte-identical to the historical single
        // interactive pane: no minimap, no expansion, no swap in flight.
        if !self.show_minimap && !self.minimap_expanded && self.swap_t == 0.0 {
            return self.paint_pane(ui, &painter, rect, self.projection, Some(&response), &inputs);
        }

        // Two contents (Euclidean = `self.projection`, Poincaré = overview) move
        // between two slots (full pane ⇄ corner) as `swap_t` eases 0 → 1.
        let corner = self.corner_rect(rect);
        let euclid_rect = lerp_rect(rect, corner, self.swap_t);
        let poincare_rect = lerp_rect(corner, rect, self.swap_t);
        let euclid_is_main = self.swap_t < 0.5;
        // Mid-flight: neither pane is interactive (avoids jank); only the settled
        // full-slot content takes input.
        let settled = self.swap_t == 0.0 || self.swap_t == 1.0;
        let euclid_interactive = settled && euclid_is_main;
        let poincare_interactive = settled && !euclid_is_main;
        let euclid_resp = euclid_interactive.then_some(&response);
        let poincare_resp = poincare_interactive.then_some(&response);

        let euclid_cfg = self.projection;
        let poincare_cfg = self.overview_cfg();

        // Paint the full-slot content first so the corner inset sits above it.
        let clicked = if euclid_is_main {
            // Euclidean fills (or nearly fills) the pane; Poincaré is the inset.
            let c = self.paint_pane(ui, &painter, euclid_rect, euclid_cfg, euclid_resp, &inputs);
            self.frame_corner(&painter, poincare_rect);
            self.paint_pane(ui, &painter, poincare_rect, poincare_cfg, poincare_resp, &inputs);
            c
        } else {
            // Poincaré fills the pane; Euclidean is the inset.
            let c = self.paint_pane(ui, &painter, poincare_rect, poincare_cfg, poincare_resp, &inputs);
            self.frame_corner(&painter, euclid_rect);
            let c2 = self.paint_pane(ui, &painter, euclid_rect, euclid_cfg, euclid_resp, &inputs);
            if poincare_interactive { c } else { c2 }
        };

        // Click the corner-slot content to toggle the expand swap.
        let corner_slot_rect = if euclid_is_main { poincare_rect } else { euclid_rect };
        let swap_resp = ui.interact(
            corner_slot_rect,
            ui.id().with("graphview_minimap_swap"),
            egui::Sense::click(),
        );
        if swap_resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if swap_resp.clicked() {
            self.minimap_expanded = !self.minimap_expanded;
        }

        clicked
    }

    /// Eye-icon view-options popup: layout selector, common toggles plus any
    /// caller-supplied display toggles, palette-specific + common color
    /// pickers, and the size sliders. Returns `true` when the layout kind
    /// changed so the caller can trigger a relayout. The host passes the eye
    /// `egui::Image` so the engine stays free of the app's icon registry.
    ///
    /// `extra_toggles` are rendered (in order) as checkboxes after the common
    /// Labels/Edges toggles — these are caller display controls (the vault
    /// "Orphans", the cluster "Leaves", the review tab's "Live preview"). Pass
    /// an empty slice for none.
    pub fn view_options_menu(
        &mut self,
        ui: &mut egui::Ui,
        eye_icon: egui::Image<'static>,
        extra_toggles: &mut [(&str, &mut bool)],
    ) -> bool {
        let resp = ui.add(egui::Button::image(eye_icon)).on_hover_text("View options");
        let prev_kind = self.layout_kind;
        let prev_rankdir = self.layered_rankdir;
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Layout").small().color(theme::muted()));
            for kind in LayoutKind::all() {
                let mut selected = self.layout_kind == kind;
                if ui.checkbox(&mut selected, kind.label()).clicked() && selected {
                    self.layout_kind = kind;
                }
            }
            // Rank direction for the layered layout (relayout on change).
            if self.layout_kind == LayoutKind::Layered {
                ui.horizontal(|ui| {
                    ui.label("Direction");
                    ui.selectable_value(
                        &mut self.layered_rankdir,
                        hiker_graph::RankDir::Tb,
                        "Top-Down",
                    );
                    ui.selectable_value(
                        &mut self.layered_rankdir,
                        hiker_graph::RankDir::Lr,
                        "Left-Right",
                    );
                });
            }
            // Anchor stiffness governs how strongly retained nodes hold their
            // prior spot across a rebuild — the display-engine control for the
            // morph. Force-directed only. [force-cfg-anchor-stiffness]
            if self.layout_kind == LayoutKind::ForceDirected {
                ui.add(
                    egui::Slider::new(&mut self.anchor_stiffness, 0.0..=1.0)
                        .text("Anchor stiffness"),
                )
                .on_hover_text(
                    "0 = lively/free re-layout, higher = stays put as the graph changes",
                );
            }
            ui.separator();
            ui.checkbox(&mut self.toggles.show_labels, "Labels");
            ui.checkbox(&mut self.toggles.show_edges, "Edges");
            for (label, flag) in extra_toggles.iter_mut() {
                ui.checkbox(flag, *label);
            }
            ui.checkbox(&mut self.toggles.show_preview, "Show note preview");

            ui.separator();
            ui.label(egui::RichText::new("Colors").small().color(theme::muted()));
            palette_rows(ui, &mut self.style.palette);
            color_row(ui, "Edges", &mut self.style.edge_color);
            color_row(ui, "Labels", &mut self.style.label_color);
            let theme_bg = ui.visuals().extreme_bg_color;
            let mut bg = self.style.background.unwrap_or(theme_bg);
            ui.horizontal(|ui| {
                if ui.color_edit_button_srgba(&mut bg).changed() {
                    self.style.background = Some(bg);
                }
                ui.label("Background");
            });

            ui.separator();
            ui.label(egui::RichText::new("Size").small().color(theme::muted()));
            ui.add(egui::Slider::new(&mut self.style.node_scale, 0.3..=3.0).text("Nodes"));
            ui.add(egui::Slider::new(&mut self.style.edge_width, 0.25..=4.0).text("Edges"));
            ui.add(egui::Slider::new(&mut self.style.label_size, 7.0..=20.0).text("Labels"));

            ui.separator();
            ui.label(egui::RichText::new("Projection").small().color(theme::muted()));
            for (kind, label) in [
                (ProjectionKind::Affine, "Off"),
                (ProjectionKind::Fisheye, "Fisheye"),
                (ProjectionKind::Poincare, "Poincaré"),
            ] {
                let mut selected = self.projection.kind == kind;
                if ui.checkbox(&mut selected, label).clicked() && selected {
                    self.projection.kind = kind;
                    // Reframe so the (newly) lensed extent fills the view.
                    self.needs_fit = true;
                }
            }
            if self.projection.kind != ProjectionKind::Affine {
                ui.add(
                    egui::Slider::new(&mut self.projection.strength, 0.1..=3.0).text("Strength"),
                );
                ui.add(
                    egui::Slider::new(&mut self.projection.size_falloff, 0.0..=1.0)
                        .text("Size falloff"),
                );

                ui.label(egui::RichText::new("Focus").small().color(theme::muted()));
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.focus_mode,
                        FocusMode::LockedCenter,
                        "Center",
                    );
                    ui.selectable_value(&mut self.focus_mode, FocusMode::Cursor, "Cursor");
                    ui.selectable_value(
                        &mut self.focus_mode,
                        FocusMode::Selection,
                        "Selection",
                    );
                });

                ui.label(egui::RichText::new("Detail (LOD)").small().color(theme::muted()));
                ui.add(
                    egui::Slider::new(&mut self.lod_full_mag, 0.0..=1.0).text("Full above"),
                );
                ui.add(
                    egui::Slider::new(&mut self.lod_marker_mag, 0.0..=1.0).text("Dot above"),
                );

                ui.label(egui::RichText::new("Edges").small().color(theme::muted()));
                ui.checkbox(&mut self.geodesic_edges, "Curved (geodesic)");
                ui.add(
                    egui::Slider::new(&mut self.projection.geodesic_segments, 2..=64)
                        .text("Segments"),
                );

                if self.projection.kind == ProjectionKind::Poincare {
                    ui.label(egui::RichText::new("Fly-to").small().color(theme::muted()));
                    ui.checkbox(&mut self.flyto_enabled, "Click to fly-to");
                    ui.add(
                        egui::Slider::new(&mut self.flyto_duration, 0.1..=2.0)
                            .text("Duration (s)"),
                    );

                    ui.label(
                        egui::RichText::new("Boundary fade").small().color(theme::muted()),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.fade_start, 0.0..=1.0).text("Start"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.fade_strength, 0.0..=1.0).text("Strength"),
                    );
                    ui.checkbox(&mut self.show_boundary, "Boundary ring");
                }
            }

            ui.separator();
            ui.label(egui::RichText::new("Minimap").small().color(theme::muted()));
            ui.checkbox(&mut self.show_minimap, "Show minimap");
            if self.show_minimap || self.minimap_expanded {
                ui.checkbox(&mut self.minimap_expanded, "Expanded");
            }
            if self.show_minimap {
                ui.horizontal(|ui| {
                    for (corner, label) in [
                        (Corner::TopLeft, "TL"),
                        (Corner::TopRight, "TR"),
                        (Corner::BottomLeft, "BL"),
                        (Corner::BottomRight, "BR"),
                    ] {
                        ui.selectable_value(&mut self.minimap_corner, corner, label);
                    }
                });
                ui.add(egui::Slider::new(&mut self.minimap_size, 0.12..=0.5).text("Size"));
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut self.minimap_shape,
                        MinimapShape::Circle,
                        "Circle",
                    );
                    ui.selectable_value(
                        &mut self.minimap_shape,
                        MinimapShape::Square,
                        "Square",
                    );
                });
            }

            ui.separator();
            if ui.button("Reset style").clicked() {
                self.style = match self.style.palette {
                    Palette::Flat { .. } => Style::flat(),
                    Palette::Policy { .. } => Style::policy(),
                };
            }
        });
        // A layout change is either a kind switch or a layered rank-direction
        // switch — both need a relayout.
        self.layout_kind != prev_kind || self.layered_rankdir != prev_rankdir
    }
}

/// Configurable colors + sizes for a graph view. The [`Palette`] varies the
/// per-node coloring controls (flat vault fill + active accent vs. the
/// cluster color-by-policy set); every other control is common to both.
#[derive(Clone, Copy)]
pub struct Style {
    pub edge_color: egui::Color32,
    pub label_color: egui::Color32,
    /// `None` follows the theme's `extreme_bg_color`.
    pub background: Option<egui::Color32>,
    /// Multiplier on each node's base radius.
    pub node_scale: f32,
    pub edge_width: f32,
    pub label_size: f32,
    pub palette: Palette,
}

/// The per-node color scheme, which differs between the two graphs.
#[derive(Clone, Copy)]
pub enum Palette {
    /// Vault graph: one flat fill + an accent for the active note.
    Flat {
        node: egui::Color32,
        active: egui::Color32,
    },
    /// Cluster graph: color by node kind / policy, blended toward `stale` by
    /// summary churn.
    Policy {
        cluster: egui::Color32,
        move_policy: egui::Color32,
        tag_policy: egui::Color32,
        leaf: egui::Color32,
        stale: egui::Color32,
    },
}

impl Style {
    /// Vault-graph defaults: flat `#6b7280` nodes, active note in accent,
    /// translucent grey edges. Defaults mirror the historical hard-coded
    /// render values so an untouched graph looks unchanged.
    pub const fn flat() -> Self {
        Self {
            edge_color: egui::Color32::from_rgba_premultiplied(0x90, 0x96, 0xa0, 0xa0),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Flat {
                node: egui::Color32::from_rgb(0x6b, 0x72, 0x80),
                active: theme::accent(),
            },
        }
    }

    /// Cluster-graph defaults: color-by-policy with the spec's four encoding
    /// colors plus a staleness grey, divider-colored edges.
    pub const fn policy() -> Self {
        Self {
            edge_color: theme::divider(),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Policy {
                cluster: theme::accent(),
                move_policy: egui::Color32::from_rgb(0x2f, 0x6f, 0xb9),
                tag_policy: egui::Color32::from_rgb(0xa8, 0x4a, 0xc4),
                leaf: egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                stale: egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
            },
        }
    }
}

/// Policy-color legend row (cluster graph only). No-op for a flat palette.
/// Reads the configured colors so the legend tracks any user edits.
pub fn policy_legend(ui: &mut egui::Ui, palette: &Palette) {
    let Palette::Policy {
        cluster,
        move_policy,
        tag_policy,
        leaf,
        ..
    } = palette
    else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Encoding:").color(theme::muted()).small());
        legend_swatch(ui, *cluster, "cluster");
        legend_swatch(ui, *move_policy, "move policy");
        legend_swatch(ui, *tag_policy, "tag policy");
        legend_swatch(ui, *leaf, "leaf");
    });
}

/// The per-frame, pane-independent inputs every [`State::paint_pane`] call
/// shares: the domain [`Source`], the descriptors built once from it, and the
/// host preview-card painter. Bundled so each pane call stays a few arguments.
#[derive(Clone, Copy)]
struct PaneInputs<'a, F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)> {
    source: &'a dyn Source,
    nodes: &'a [NodeDescriptor],
    draw_preview: &'a F,
}

/// The two screen-mapping closures [`State::draw_edges`] needs, bundled to keep
/// its signature small. `to_screen` is the regime-appropriate world→screen map
/// (it already composes the lens); `disk_to_screen` maps a unit-disk
/// [`Complex`] to screen, used for Poincaré geodesic samples. For the affine
/// regimes these are `|w| affine(lens.world_to_lensed(w))` and
/// `|z| affine(lens.disk_to_world(z))`; for the locked Poincaré disk they map
/// against the pane-fixed disk frame instead.
#[derive(Clone, Copy)]
struct EdgeMap<'a> {
    to_screen: &'a dyn Fn(egui::Vec2) -> egui::Pos2,
    disk_to_screen: &'a dyn Fn(Complex) -> egui::Pos2,
}

/// Inputs to one node-paint pass: the active lens + view zoom (for radius +
/// label gating) and the interaction state (hover/click — both empty for a
/// read-only pane).
struct NodePaint<'a> {
    lens: &'a Lens,
    zoom: f32,
    hovered: Option<usize>,
    response_clicked: bool,
}

/// Scratch results from one node-paint pass.
#[derive(Default)]
struct NodeDraw {
    clicked: Option<String>,
    hover_anchor: Option<egui::Pos2>,
    tooltip: Option<(egui::Pos2, String)>,
}

/// Nearest node within its (scaled) radius of the cursor, if any.
///
/// Picking is done in screen space against each node's already-projected
/// screen position (`to_screen` composes lens + affine) and its
/// magnification-scaled radius, so it stays exact under any projection without
/// a separate inverse pass. The `Lens` is still passed so the radius matches
/// the drawn size (centre nodes hit larger, rim nodes smaller).
fn hit_test(
    nodes: &[NodeDescriptor],
    to_screen: &dyn Fn(egui::Vec2) -> egui::Pos2,
    lens: &Lens,
    hover: egui::Pos2,
    node_scale: f32,
    zoom: f32,
) -> Option<usize> {
    let mut best = f32::INFINITY;
    let mut hit = None;
    for d in nodes {
        let p = to_screen(d.world_pos);
        let r = d.radius * node_scale * zoom.max(0.4) * lens.magnification(d.world_pos);
        let d2 = (p - hover).length_sq();
        if d2 <= (r + 4.0).powi(2) && d2 < best {
            best = d2;
            hit = Some(d.index);
        }
    }
    hit
}

/// White-background name tooltip (cluster graph). Mirrors the box the
/// cluster panel drew inline before the engine extraction.
fn draw_tooltip(painter: &egui::Painter, pos: egui::Pos2, text: String) {
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), egui::Color32::BLACK);
    let bg = egui::Rect::from_min_size(pos, galley.size()).expand(4.0);
    painter.rect_filled(bg, 2.0, egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 230));
    painter.galley(pos, galley, egui::Color32::BLACK);
}

/// The palette-specific color rows — flat node/active, or the five policy
/// colors.
fn palette_rows(ui: &mut egui::Ui, palette: &mut Palette) {
    match palette {
        Palette::Flat { node, active } => {
            color_row(ui, "Nodes", node);
            color_row(ui, "Active note", active);
        }
        Palette::Policy {
            cluster,
            move_policy,
            tag_policy,
            leaf,
            stale,
        } => {
            color_row(ui, "Cluster", cluster);
            color_row(ui, "Move policy", move_policy);
            color_row(ui, "Tag policy", tag_policy);
            color_row(ui, "Leaf", leaf);
            color_row(ui, "Stale", stale);
        }
    }
}

/// One labeled color swatch row.
fn color_row(ui: &mut egui::Ui, label: &str, color: &mut egui::Color32) {
    ui.horizontal(|ui| {
        ui.color_edit_button_srgba(color);
        ui.label(label);
    });
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(egui::RichText::new(label).small().color(theme::muted()));
}

#[cfg(test)]
mod poincare_disk_tests {
    use super::*;

    /// The Poincaré projection config the locked-disk tests render at. A
    /// strength that visibly spreads the clusters across the disk.
    fn poincare_cfg() -> ProjectionConfig {
        ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.4,
            ..Default::default()
        }
    }

    /// A small clustered layout: a central blob at the origin plus a ring of
    /// off-centre clusters, mirroring the synthetic snapshot graph's shape so
    /// the periphery has real spread for the lens to compress toward the rim.
    fn clustered_positions() -> Vec<egui::Vec2> {
        let mut pos = Vec::new();
        // Central cluster.
        for i in 0..6 {
            let a = i as f32;
            pos.push(egui::vec2(a * 7.0 - 17.0, a * -5.0 + 12.0));
        }
        // Ring clusters.
        let radius = 320.0;
        for c in 0..5 {
            let ang = c as f32 / 5.0 * std::f32::consts::TAU;
            let (cx, cy) = (ang.cos() * radius, ang.sin() * radius);
            for i in 0..6 {
                let a = i as f32;
                pos.push(egui::vec2(cx + a * 9.0 - 22.0, cy + a * -6.0 + 15.0));
            }
        }
        pos
    }

    /// The locked-disk frame is a pure function of the pane rect + zoom: centre =
    /// pane centre (ZOOM-INVARIANT), radius = `DISK_FILL` of the shorter
    /// half-dimension × zoom — and it does NOT (cannot) depend on any
    /// `View`/pan/zoom. This is the core regression guard for "the disk drifts":
    /// the signature itself forbids the bug. The centre must be the pane centre
    /// for ANY zoom; only the radius scales.
    #[test]
    fn poincare_disk_is_pane_locked() {
        let rects = [
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0)),
            egui::Rect::from_min_size(egui::pos2(40.0, 90.0), egui::vec2(800.0, 400.0)),
            egui::Rect::from_min_size(egui::pos2(-120.0, 30.0), egui::vec2(300.0, 900.0)),
        ];
        for r in rects {
            for zoom in [0.5_f32, 1.0, 3.0] {
                let (center, radius) = poincare_disk(r, zoom);
                assert_eq!(
                    center,
                    r.center(),
                    "disk centre must be the pane centre at zoom {zoom}"
                );
                let expected = 0.5 * r.size().min_elem() * DISK_FILL * zoom;
                assert!(
                    (radius - expected).abs() < 1e-4,
                    "disk radius {radius} != {expected} for {r:?} at zoom {zoom}"
                );
            }
        }
    }

    /// Scroll-zoom scales the RADIUS while leaving the CENTRE fixed: two zoom
    /// values yield the same centre, and the radius ratio equals the zoom ratio
    /// (the disk grows/shrinks centred, never drifting).
    #[test]
    fn poincare_zoom_scales_radius_keeps_center() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(640.0, 480.0));
        let (c_a, r_a) = poincare_disk(rect, 1.0);
        let (c_b, r_b) = poincare_disk(rect, 2.5);
        assert_eq!(c_a, c_b, "centre must be zoom-invariant");
        assert_eq!(c_a, rect.center(), "centre must be the pane centre");
        // radius ratio == zoom ratio (2.5 / 1.0).
        assert!(
            (r_b / r_a - 2.5).abs() < 1e-4,
            "radius ratio {} != zoom ratio 2.5",
            r_b / r_a
        );
    }

    /// The disk→screen map sends the unit-disk origin to the disk centre and a
    /// rim point (|z| = 1) to the disk centre + radius — the disk fills exactly
    /// the locked frame.
    #[test]
    fn poincare_maps_origin_to_center_and_rim_to_radius() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(640.0, 480.0));
        let (center, radius) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| center + egui::vec2(z.re, z.im) * radius;
        let o = disk_to_screen(Complex::ORIGIN);
        assert!((o - center).length() < 1e-4, "origin {o:?} != centre {center:?}");
        let rim = disk_to_screen(Complex::new(1.0, 0.0));
        let expected = center + egui::vec2(radius, 0.0);
        assert!(
            (rim - expected).length() < 1e-4,
            "rim {rim:?} != centre+radius {expected:?}"
        );
    }

    /// Every node, projected through the locked Poincaré map at fit (zoom 1.0),
    /// lands inside the fixed disk (within 1px of the boundary) — the whole graph
    /// is pressed into the disk, never panned off-screen. (At higher zoom the disk
    /// may exceed the pane and nodes legitimately fall outside it, so this is a
    /// zoom-1.0 / relative-to-disk-radius invariant.)
    #[test]
    fn poincare_keeps_all_nodes_inside_disk() {
        let pos = clustered_positions();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0));
        let (center, radius) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| center + egui::vec2(z.re, z.im) * radius;
        let lens = Lens::centred(poincare_cfg(), Mobius::identity(), &pos);
        for &w in &pos {
            let p = disk_to_screen(lens.disk(w));
            let dist = (p - center).length();
            assert!(
                dist <= radius + 1.0,
                "node at {dist}px from centre exceeds disk radius {radius}"
            );
        }
    }

    /// Möbius navigation moves the projected content but leaves the disk frame
    /// invariant: the disk centre/radius are unchanged and at least one node's
    /// screen position changes. The disk is a fixed viewport; nav re-aims the
    /// graph within it.
    #[test]
    fn mobius_nav_moves_content_but_not_disk_geometry() {
        let pos = clustered_positions();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0));
        let cfg = poincare_cfg();
        // The disk frame depends only on the pane (+ a fixed zoom) — recomputing
        // it after nav must yield the identical centre/radius.
        let (c0, r0) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| c0 + egui::vec2(z.re, z.im) * r0;

        let plain = Lens::centred(cfg, Mobius::identity(), &pos);
        // Recentre on a peripheral node, as a drag/fly-to would.
        let target = plain.disk(pos[pos.len() - 1]);
        let nav = Mobius::from_point_pair(target, Complex::ORIGIN);
        let navd = Lens::centred(cfg, nav, &pos);

        let (c1, r1) = poincare_disk(rect, 1.0);
        assert_eq!(c0, c1, "nav must not move the disk centre");
        assert!((r0 - r1).abs() < 1e-6, "nav must not rescale the disk");

        let mut moved = false;
        for &w in &pos {
            let before = disk_to_screen(plain.disk(w));
            let after = disk_to_screen(navd.disk(w));
            if (before - after).length() > 1.0 {
                moved = true;
            }
            // Content stays inside the (unchanged) disk under navigation too.
            assert!((after - c1).length() <= r1 + 1.0, "navigated node left the disk");
        }
        assert!(moved, "navigation did not move any content");
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    fn positions() -> Vec<egui::Vec2> {
        vec![
            egui::vec2(0.0, 0.0),
            egui::vec2(100.0, 0.0),
            egui::vec2(-100.0, 50.0),
            egui::vec2(40.0, -90.0),
        ]
    }

    /// A non-identity nav must leave Off and Fisheye disk points untouched —
    /// navigation is Poincaré-only, so those modes stay byte-identical.
    #[test]
    fn nav_ignored_off_and_fisheye() {
        let pos = positions();
        let nav = Mobius::from_point_pair(Complex::new(0.3, -0.2), Complex::ORIGIN);
        for kind in [ProjectionKind::Affine, ProjectionKind::Fisheye] {
            let cfg = ProjectionConfig { kind, strength: 1.2, ..Default::default() };
            let plain = Lens::centred(cfg, Mobius::identity(), &pos);
            let navd = Lens::centred(cfg, nav, &pos);
            for &w in &pos {
                let a = plain.disk(w);
                let b = navd.disk(w);
                assert!(
                    (a.re - b.re).abs() < 1e-6 && (a.im - b.im).abs() < 1e-6,
                    "{kind:?} disk changed under nav: {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Under Poincaré the recentre `from_point_pair(z_node, ORIGIN)` must drive
    /// the chosen node's disk point exactly to the origin.
    #[test]
    fn poincare_flyto_centres_target() {
        let pos = positions();
        let cfg = ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.2,
            ..Default::default()
        };
        let base = Lens::centred(cfg, Mobius::identity(), &pos);
        let target = base.disk(pos[2]); // a peripheral node, pre-nav
        let nav = Mobius::from_point_pair(target, Complex::ORIGIN);
        let navd = Lens::centred(cfg, nav, &pos);
        let centred = navd.disk(pos[2]);
        assert!(
            centred.abs() < 1e-4,
            "fly-to did not centre target: {centred:?}"
        );
    }
}
