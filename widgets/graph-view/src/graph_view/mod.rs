//! Shared force/tree graph rendering engine for the vault link-graph and
//! the cluster-tree graph. Owns pan/zoom, layout (the background force
//! worker plus the inline tree-position math), the eye-icon view-options
//! menu, and the node/edge/label/hover/preview paint loop. Each caller
//! supplies a [`Source`] that turns its own data — a `petgraph` vault graph
//! or a slice of cluster `EditableNode`s — into per-frame [`NodeDescriptor`]s
//! plus the edge and layout-tree topology, so one code path renders both
//! views with different colors and options.

use std::collections::HashMap;

use hiker_graph::{LayoutKind, LayoutParams};
use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};
use hiker_projection_view::lens_disk;
use hiker_theme as theme;

use crate::force_graph::View;
use graph_widgets::force_layout::LayoutWorker;
use graph_widgets::{
    horizontal_tree_positions, layered_layout, radial_positions, vertical_tree_positions,
};

pub mod edge_paint;
mod edges;
pub mod gpu;
mod layout;
pub mod minimap;
mod nav;
mod panes;
pub mod source;
pub mod styling;
#[cfg(test)]
mod tests;

/// A `puffin` profile span, gated behind this crate's `profiling` feature so it
/// compiles to nothing when off (mirrors the app's `profile_scope!`). Lets the
/// GPU batch build + upload show up in the same capture as the app's spans.
macro_rules! profile_scope {
    ($name:expr) => {
        #[cfg(feature = "profiling")]
        puffin::profile_scope!($name);
    };
}
pub(crate) use profile_scope;

use std::sync::atomic::{AtomicBool, Ordering};

use layout::{adaptive_anchor_stiffness, build_warm_seed, change_fraction, scatter};
use source::{LayoutConfig, NodeDescriptor, PreviewCache, Snapshot, Source, Toggles};
use styling::{color_row, palette_rows, HighlightStyle, Palette, Style};

/// Process-global opt-in for the custom instanced GPU paint path. Off by
/// default so every test / example / snapshot renders through the unchanged
/// egui Painter; the live app turns it on only when it selected the wgpu
/// backend (see `set_gpu_paint`). Combined per-`State` with
/// [`State::gpu_instancing`] before the GPU path activates.
static GPU_PAINT: AtomicBool = AtomicBool::new(false);

/// Enable/disable the custom instanced GPU paint path process-wide. The live app
/// calls `set_gpu_paint(true)` after choosing the wgpu renderer; nothing else
/// does, so committed snapshots keep rendering through the Painter path.
pub fn set_gpu_paint(on: bool) {
    GPU_PAINT.store(on, Ordering::Relaxed);
}

/// Whether the process-global GPU paint opt-in is set.
fn gpu_paint_enabled() -> bool {
    GPU_PAINT.load(Ordering::Relaxed)
}

const ZOOM_MIN: f32 = 0.005;
const ZOOM_MAX: f32 = 6.0;

// The locked Poincaré disk frame, its `DISK_FILL` / `POINCARE_ZOOM_MIN/MAX`
// constants, the `zoom_poincare` scroll law, the `centroid_scale` framing, and
// the `forward`+nav disk operator now live in the shared `hiker_projection_view`
// crate — the single source of truth with the canvas camera (they were
// hand-synced copies). `poincare_disk` stays as a thin engine-internal delegate
// so callers (`panes.rs`) and the in-file tests keep the short name.
// [proj-canvas-mode]

/// The locked Poincaré disk frame for a pane — a pure function of `pane_rect` +
/// `zoom` (centre = pane centre, radius = `DISK_FILL` of the shorter
/// half-dimension × `zoom`). Delegates to [`hiker_projection_view::poincare_disk`].
fn poincare_disk(pane_rect: egui::Rect, zoom: f32) -> (egui::Pos2, f32) {
    hiker_projection_view::poincare_disk(pane_rect, zoom)
}

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
    /// How far the force layout spreads out (user-controllable). Maps to FA2
    /// gravity as `gravity = 1/spread` (less gravity → wider layout) and scales
    /// the runaway safety belt with it, so bigger spread never piles nodes at the
    /// wall. `1.0` = the default weak-gravity shape. Force-directed only.
    pub layout_spread: f32,
    /// Force-layout iteration budget (FA2 `max_iters`). Higher = longer settle for
    /// big graphs that still drift at the default cap; it still stops early on
    /// convergence. User-configurable via the view menu.
    pub settle_iters: u32,
    /// Whether the node-colour palette controls (flat node fill + active-note accent)
    /// are meaningful for this source. `false` when nodes are coloured by some other
    /// rule (e.g. the code graph colours by entity kind), so the view menu hides the
    /// inapplicable vault-style palette pickers. Edge/label/background colours still
    /// apply regardless.
    pub palette_editable: bool,
    /// Per-`State` user toggle for the custom instanced GPU paint path. Only has
    /// an effect when the process-global opt-in ([`set_gpu_paint`]) is also set —
    /// i.e. in the live app under the wgpu backend. Default `true`.
    pub gpu_instancing: bool,
    /// Stable GPU-callback slot ids for this view's panes (main + minimap),
    /// allocated lazily so each pane keeps its own persistent instance buffer.
    gpu_pane_ids: [Option<u64>; 2],
    /// CPU-side mirror of the geometry key each pane's GPU buffers were last
    /// built for (indexed by pane slot, 0 = main, 1 = minimap). Lets the affine
    /// fast-path decide — before building the batch — whether this frame is a
    /// cache hit (skip the fill build) or a rebuild. Kept in sync with the GPU's
    /// own `PaneBuffers.cache_key`.
    gpu_last_key: [Option<gpu::GpuCacheKey>; 2],
    /// Animated "edge flow": toggle-able tracer dots that ride each edge from
    /// caller→callee, drawn by the GPU flow pipeline. Default `false`. GPU path
    /// only — inert under the Painter fallback (flow is a GPU feature). When on,
    /// `ui()` feeds the seconds clock into the callback and requests a repaint
    /// every frame so the dots advance; when off, no extra repaint, zero cost
    /// (the flow pipeline simply isn't drawn).
    pub flow_enabled: bool,
    /// Tracer-dot hue. A vivid amber (`#ff8c1a`) by default so the dots read on
    /// both the white live code-graph background and dark demo backgrounds (the
    /// old white-brightened dots washed out on white — Bug 2). Only meaningful
    /// when `flow_enabled`.
    pub flow_color: egui::Color32,
    /// Tracer-dot radius in screen px (1.0..=6.0). Not view-scaled.
    pub flow_size: f32,
    /// Tracer-dot opacity (0.0..=1.0) — how intrusive the dots are.
    pub flow_alpha: f32,
    /// Dots emitted per edge (1..=8). Multiple evenly-spaced dots keep one in the
    /// on-screen edge span at any zoom (Bug 1).
    pub flow_density: u32,
    /// Tracer-dot speed in cycles/second (0.05..=2.0).
    pub flow_speed: f32,
    /// The seconds clock fed to the flow shader this frame. Normally
    /// `ui.input(|i| i.time)`; a headless demo can pin it via
    /// [`set_flow_for_demo`](Self::set_flow_for_demo).
    flow_time: f32,
    /// `true` once a demo pinned `flow_time`, so `ui()` stops overwriting it with
    /// the live input clock (headless snapshots render at a fixed flow phase).
    flow_time_pinned: bool,
    /// Monotonic "the layout geometry changed" counter for the GPU paint cache.
    /// Bumped on every [`recompute_layout`](Self::recompute_layout) and on every
    /// frame the force worker is still settling (positions move). The affine GPU
    /// path records the epoch it last built its instance/edge buffers for and
    /// skips the rebuild + upload when it's unchanged — so a pure pan/zoom (which
    /// leaves the world positions alone) only rewrites a small view-transform
    /// uniform. See `gpu.rs`.
    layout_epoch: u64,
    /// Hover / selection edge-highlight appearance + toggles. [graph-hover-highlight]
    pub highlight: HighlightStyle,
    /// The "selected" node index the host marks for a persistent edge highlight
    /// (e.g. the code view's drilled-into node). Set by the consumer each frame;
    /// `None` = nothing selected. Honored when `highlight.selected_edges`.
    pub selected_node: Option<usize>,
    /// The node whose hover highlight is currently animating — retained when the
    /// pointer leaves so its edges can FADE OUT (the live `hovered` is already
    /// `None` by then). Internal.
    hover_anim_node: Option<usize>,
    /// In-flight hover-flow transition: when the hover MOVES between two nodes,
    /// the highlight doesn't jump — the old node's glow fades out while the new
    /// one's fades in, and any edge directly connecting them carries a travelling
    /// pulse. Internal. status: graph-hover-flow
    hover_flow: Option<HoverFlow>,
    /// Fluid-highlight energy per node: injected at the hovered node, diffusing
    /// along edges, drifting downhill toward the selected node, decaying.
    /// Internal. status: graph-hover-fluid
    fluid_energy: Vec<f32>,
    /// Hop-distance potential from the selected node (the "gravity" the fluid
    /// runs down); rebuilt when the selection / graph changes. Internal.
    fluid_potential: Vec<f32>,
    fluid_potential_for: Option<usize>,
    fluid_potential_epoch: u64,
    fluid_last_time: f64,
    /// The `click_path` of the node RIGHT-clicked this frame (set during paint,
    /// read+cleared by the host via [`take_secondary_click`](Self::take_secondary_click)).
    /// Lets a consumer attach a context action (e.g. the code view's "open source
    /// file") without the engine knowing the verb.
    secondary_click: Option<String>,
    /// Positions-vector index of the node RIGHT-clicked this frame (set during
    /// paint alongside `secondary_click`, read+cleared via
    /// [`take_secondary_click_node`](Self::take_secondary_click_node)). Unlike
    /// `secondary_click` it is set even for nodes with no `click_path`, so a
    /// host can attach a context menu to non-openable nodes (e.g. cluster
    /// nodes) without giving them a misleading click affordance.
    secondary_click_node: Option<usize>,
}

/// An in-flight hover-flow transition: the two keyframes (`from` = the previously
/// hovered node, `to` = the newly hovered one) and the wall-clock start, from which
/// each frame derives its progress. status: graph-hover-flow
#[derive(Clone, Copy, Debug)]
struct HoverFlow {
    from: usize,
    to: usize,
    start: f64,
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

/// The layout centroid + centroid extent — delegates to
/// [`hiker_projection_view::centroid_scale`]. Kept as an engine-internal wrapper
/// so `Lens` and `nav.rs` keep the short name. The extent normalises the lens so
/// `tanh` doesn't saturate; computing it from the centroid (not the moving lens
/// focus) keeps the layout scale fixed as the focus moves (focus-modes).
fn centroid_scale(positions: &[egui::Vec2]) -> (egui::Vec2, f32) {
    hiker_projection_view::centroid_scale(positions)
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
        lens_disk((w - self.focus) / self.scale, self.cfg, self.nav)
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

const fn projection_kind_str(kind: ProjectionKind) -> &'static str {
    match kind {
        ProjectionKind::Affine => "affine",
        ProjectionKind::Fisheye => "fisheye",
        ProjectionKind::Poincare => "poincare",
    }
}

fn projection_kind_from_str(s: &str) -> ProjectionKind {
    match s {
        "fisheye" => ProjectionKind::Fisheye,
        "poincare" => ProjectionKind::Poincare,
        _ => ProjectionKind::Affine,
    }
}

const fn focus_mode_str(mode: FocusMode) -> &'static str {
    match mode {
        FocusMode::LockedCenter => "center",
        FocusMode::Cursor => "cursor",
        FocusMode::Selection => "selection",
    }
}

fn focus_mode_from_str(s: &str) -> FocusMode {
    match s {
        "cursor" => FocusMode::Cursor,
        "selection" => FocusMode::Selection,
        _ => FocusMode::LockedCenter,
    }
}

impl State {
    /// Snapshot the persistable view bits into a plain, serde-free
    /// [`Snapshot`] the app converts to its tab-state store. The node
    /// positions come from `prev_positions` (the live layout keyed by stable
    /// [`Source::node_key`], captured every frame in [`State::ui`]) so they
    /// survive a rebuild. Excludes the worker / edge routes / preview / fly-to /
    /// nav / GPU handles. status: graph-view-state-persist
    pub fn view_snapshot(&self) -> Snapshot {
        Snapshot {
            positions: self
                .prev_positions
                .iter()
                .map(|(k, v)| (k.clone(), (v.x, v.y)))
                .collect(),
            pan_x: self.view.pan.x,
            pan_y: self.view.pan.y,
            zoom: self.view.zoom,
            projection_kind: projection_kind_str(self.projection.kind).to_string(),
            projection_strength: self.projection.strength,
            projection_size_falloff: self.projection.size_falloff,
            focus_mode: focus_mode_str(self.focus_mode).to_string(),
            show_labels: self.toggles.show_labels,
            show_edges: self.toggles.show_edges,
            show_preview: self.toggles.show_preview,
            lod_full_mag: self.lod_full_mag,
            lod_marker_mag: self.lod_marker_mag,
        }
    }

    /// Apply a previously-captured [`Snapshot`] (projection / focus /
    /// toggles / LOD / pan-zoom + the warm-seed positions). The positions are
    /// seeded into `prev_positions` so the NEXT same-kind force [`recompute_layout`](Self::recompute_layout)
    /// warm-seeds + anchors the retained nodes onto their saved spots (robust when
    /// the node set changed: only matching keys are re-used, new nodes settle
    /// fresh). The caller suppresses the fresh-build auto-fit so the restored
    /// pan/zoom sticks. status: graph-view-state-persist
    pub fn restore_view(&mut self, snap: &Snapshot) {
        self.view.pan = egui::vec2(snap.pan_x, snap.pan_y);
        if snap.zoom > 0.0 {
            self.view.zoom = snap.zoom;
        }
        self.projection.kind = projection_kind_from_str(&snap.projection_kind);
        self.projection.strength = snap.projection_strength;
        self.projection.size_falloff = snap.projection_size_falloff;
        self.focus_mode = focus_mode_from_str(&snap.focus_mode);
        self.toggles.show_labels = snap.show_labels;
        self.toggles.show_edges = snap.show_edges;
        self.toggles.show_preview = snap.show_preview;
        self.lod_full_mag = snap.lod_full_mag;
        self.lod_marker_mag = snap.lod_marker_mag;
        // Seed the warm-layout history so the next force rebuild morphs onto the
        // saved shape. `last_layout_kind` is left untouched — the caller drives
        // the rebuild; an empty `prev_positions` simply means a fresh scatter.
        self.prev_positions = snap
            .positions
            .iter()
            .map(|(k, &(x, y))| (k.clone(), egui::vec2(x, y)))
            .collect();
    }

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
            layout_spread: 1.0,
            settle_iters: 800,
            palette_editable: true,
            gpu_instancing: true,
            gpu_pane_ids: [None, None],
            gpu_last_key: [None, None],
            flow_enabled: false,
            // Defaults that look good out of the box: vivid amber, ~3px, ~0.9
            // alpha, density 3, speed 0.35.
            flow_color: egui::Color32::from_rgb(0xff, 0x8c, 0x1a),
            flow_size: 3.0,
            flow_alpha: 0.9,
            flow_density: 3,
            flow_speed: 0.35,
            flow_time: 0.0,
            flow_time_pinned: false,
            layout_epoch: 0,
            highlight: HighlightStyle::default(),
            selected_node: None,
            hover_anim_node: None,
            hover_flow: None,
            fluid_energy: Vec::new(),
            fluid_potential: Vec::new(),
            fluid_potential_for: None,
            fluid_potential_epoch: 0,
            fluid_last_time: 0.0,
            secondary_click: None,
            secondary_click_node: None,
        }
    }

    /// Take the `click_path` of the node RIGHT-clicked this frame (clearing it).
    /// The host calls this right after [`ui`](Self::ui) to run a context action —
    /// e.g. the code view opens the node's source file. `None` if no node was
    /// right-clicked.
    pub const fn take_secondary_click(&mut self) -> Option<String> {
        self.secondary_click.take()
    }

    /// Take the positions-vector index of the node RIGHT-clicked this frame
    /// (clearing it). The index-keyed sibling of
    /// [`take_secondary_click`](Self::take_secondary_click) for hosts whose
    /// menu targets include nodes without a `click_path` (cluster nodes, group
    /// nodes): the host maps the index back to its own node identity.
    pub const fn take_secondary_click_node(&mut self) -> Option<usize> {
        self.secondary_click_node.take()
    }

    /// Inject full fluid-highlight energy at `indices` (positions-vector indices),
    /// as if each were hovered to saturation at once — the host-side entry into the
    /// hover fluid (`graph-hover-fluid`): the energy then diffuses along edges,
    /// drifts toward the selection, and decays exactly like a hover wake. Lets a
    /// consumer *light up a set of nodes* (e.g. a spec's implementing entities) with
    /// one call and no new render machinery. Out-of-range indices are ignored; a
    /// no-op while the layout is empty or the fluid highlight is toggled off
    /// (the field only renders under `highlight.fluid`).
    /// status: code-graph-spec-lighting
    pub fn pulse_nodes(&mut self, indices: &[usize]) {
        let n = self.positions.len();
        // The fluid field only advances (and decays) while `highlight.fluid` is
        // on — injecting with it off would park stale energy that pops in later.
        if n == 0 || !(self.highlight.fluid && self.highlight.hover_edges) {
            return;
        }
        // Mirror the advance step's resize: a stale-length field is replaced (and
        // its selection potential invalidated) rather than indexed out of bounds.
        if self.fluid_energy.len() != n {
            self.fluid_energy = vec![0.0; n];
            self.fluid_potential_for = None;
        }
        for &i in indices {
            if i < n {
                self.fluid_energy[i] = 1.0;
            }
        }
    }

    /// Force the GPU paint cache to rebuild its instance/edge buffers next frame
    /// even though the layout geometry is unchanged. Hosts call this when a
    /// *coloring input* outside the engine changes (e.g. the code graph switching
    /// its fill overlay): the cached affine batch bakes node fills, so a pure
    /// recolor would otherwise keep stale colors on the GPU path.
    pub const fn invalidate_paint_cache(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
    }

    /// Whether the GPU instanced paint path should run this frame: the process
    /// opt-in *and* this view's user toggle.
    fn gpu_active(&self) -> bool {
        gpu_paint_enabled() && self.gpu_instancing
    }

    /// The edge-flow animation inputs to thread onto this frame's GPU callbacks:
    /// the current seconds clock + whether flow is enabled. A no-op (`flow: false`)
    /// unless `flow_enabled` is set; the dots animate purely off the moving
    /// `flow_time`, so the affine edge buffer is never rebuilt.
    fn flow_params(&self) -> gpu::FlowParams {
        // The colour picker stores a premultiplied `Color32`; the shader wants
        // the STRAIGHT (un-premultiplied) sRGB hue in 0..1 and applies the
        // opacity itself, so pass the un-multiplied components.
        let [r, g, b, _] = self.flow_color.to_srgba_unmultiplied();
        gpu::FlowParams {
            time: self.flow_time,
            flow: self.flow_enabled,
            color: [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0],
            size: self.flow_size,
            alpha: self.flow_alpha,
            speed: self.flow_speed,
            density: self.flow_density as f32,
        }
    }

    /// Pin the edge-flow clock to a fixed value and enable flow — for headless
    /// snapshot/demo frames that render the tracer dots at a deterministic phase
    /// without driving real time. Not part of the interactive flow.
    #[doc(hidden)]
    pub const fn set_flow_for_demo(&mut self, time: f32) {
        self.flow_enabled = true;
        self.flow_time = time;
        self.flow_time_pinned = true;
    }

    /// The stable GPU-callback slot id for pane `slot` (0 = main, 1 = minimap),
    /// allocating it on first use so the pane's instance buffer persists.
    fn gpu_pane_id(&mut self, slot: usize) -> u64 {
        *self.gpu_pane_ids[slot].get_or_insert_with(gpu::GraphPaintCallback::next_id)
    }

    /// A cheap structural fingerprint of what the affine GPU batch *emits* this
    /// frame, beyond the `layout_epoch`. Two frames that share an epoch but
    /// differ here (a toggled edge layer, a changed node/edge count, a layout
    /// kind switch, a node-scale/edge-width edit that resizes geometry) must
    /// rebuild; a pure pan/zoom leaves all of these alone, so the key matches and
    /// the rebuild is skipped. `node_count`/`edges` come from the [`Source`].
    fn gpu_content_key(&self, source: &dyn Source) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        source.node_count().hash(&mut h);
        source.edges().len().hash(&mut h);
        self.toggles.show_edges.hash(&mut h);
        self.toggles.show_labels.hash(&mut h);
        (self.layout_kind as u8).hash(&mut h);
        // Node scale + edge width resize the baked geometry, so fold them in.
        self.style.node_scale.to_bits().hash(&mut h);
        self.style.edge_width.to_bits().hash(&mut h);
        h.finish()
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

    /// Force the affine pan/zoom view directly — for headless snapshot/demo
    /// frames that need to render at a panned + zoomed view without driving real
    /// scroll/drag input. `zoom` is the affine view zoom; `pan` is the world-space
    /// pan offset (`screen = center + (w + pan) * zoom`). Cancels the settle-time
    /// auto-fit so the view sticks. Not part of the interactive flow.
    #[doc(hidden)]
    pub const fn set_view_for_demo(&mut self, zoom: f32, pan: egui::Vec2) {
        self.view.zoom = zoom;
        self.view.pan = pan;
        self.needs_fit = false;
    }

    /// Drop the warm-seed history so the NEXT [`recompute_layout`](Self::recompute_layout)
    /// lays out fresh (compact scatter) instead of morphing from the prior layout.
    /// Call this when the node set changes so drastically that warm-seeding from the
    /// old positions would scatter the new graph — e.g. drilling from a huge overview
    /// into a small focus neighbourhood, where inheriting the overview's wide spread
    /// would fling the few nodes across an empty area and the fit would zoom past them.
    pub fn reset_layout_history(&mut self) {
        self.prev_positions.clear();
        self.last_layout_kind = None;
    }

    /// (Re)compute positions for the current `layout_kind`. Force-directed
    /// spawns the background worker from a random scatter; the tree layouts
    /// run inline off `source.layout_tree`. Always flags `needs_fit` so
    /// `ui()` reframes on the next paint.
    pub fn recompute_layout(&mut self, source: &dyn Source, cfg: LayoutConfig) {
        self.worker = None;
        self.needs_fit = true;
        // New geometry: invalidate the GPU paint cache so the affine fast-path
        // rebuilds its instance/edge buffers this frame (see `layout_epoch`).
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
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
                // Spread → repulsion strength: the settled radius scales ~√scaling_ratio
                // (repulsion balanced against the edge springs; weak gravity, being
                // ∝1/dist like repulsion, barely moves the extent — measured). So map
                // `layout_spread` to `scaling_ratio = 100·spread²`, giving radius ∝ spread
                // (spread 1 = the default scaling_ratio 100). `bound` is only a runaway
                // safety belt, and a FIXED belt becomes a wall as graphs grow (a 20k-node
                // graph settles at radius ~95k, so the old 50k belt pinned 8% of nodes
                // into a square wall during settle). Scale it with the graph's mass
                // (≈ n + 2·edges) AND the spread so the layout keeps its spread-out shape
                // with room to stretch (measured: 0% at the wall) while still catching
                // true runaways. Small graphs keep the 50k floor.
                let spread = self.layout_spread.clamp(0.25, 5.0);
                let scaling_ratio = 100.0 * spread * spread;
                let bound = (2.0 * spread * (n + 2 * edges.len()) as f32).max(50_000.0);
                // Iteration budget: a big graph often still drifts when it hits the
                // default cap, so let the user extend the settle. FA2 still stops
                // early once its swinging metric converges, so a high cap just means
                // "keep going until actually settled" rather than always running it.
                let max_iters = self.settle_iters.max(50);
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
                            bound,
                            scaling_ratio,
                            max_iters,
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
                            bound,
                            scaling_ratio,
                            max_iters,
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
            // Positions moved this frame: invalidate the GPU paint cache so the
            // affine fast-path rebuilds against the fresh layout while settling.
            self.layout_epoch = self.layout_epoch.wrapping_add(1);
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

        // Edge-flow animation clock: feed the egui monotonic seconds clock into
        // the flow shader and keep repainting so the dots advance. A demo can pin
        // the clock (`flow_time_pinned`) to render a fixed phase headlessly. When
        // flow is off, no extra repaint + zero cost (the pipeline isn't drawn).
        if self.flow_enabled {
            if self.flow_time_pinned {
                // Headless demo: the clock is fixed, so the dots are static — no
                // repaint needed (and the kittest harness would loop forever).
            } else {
                self.flow_time = ui.input(|i| i.time) as f32;
                ui.ctx().request_repaint();
            }
        }

        let nodes = source.nodes(&self.positions, &self.style);
        let inputs = PaneInputs { source, nodes: &nodes, draw_preview: &draw_preview };
        // Cleared each frame; `paint_pane` sets them when a node is right-clicked.
        self.secondary_click = None;
        self.secondary_click_node = None;

        // A single interactive pane. The corner overview is now a first-class,
        // standalone [`minimap::Minimap`] the host composes over this pane (so it
        // works even when the host's main view isn't a graph-view, like the canvas
        // board) — no longer an inline swap branch here.
        self.paint_pane(ui, &painter, rect, self.projection, Some(&response), &inputs, 0)
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
        // Cap the menu height to the viewport so a long option list scrolls instead
        // of running off the bottom of the screen.
        let max_h = (ui.ctx().screen_rect().height() - 96.0).max(240.0);
        let mut spread_relayout = false;
        // Stay open while the user toggles checkboxes / drags sliders inside —
        // only an OUTSIDE click dismisses it (a sticky settings menu).
        egui::Popup::menu(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
          egui::ScrollArea::vertical().max_height(max_h).show(ui, |ui| {
            // Accordion: each section is a collapsible header so the (long) option
            // list stays scannable. Layout + Display open by default; the rest
            // collapse. egui persists each header's open state by id.
            use egui::CollapsingHeader;

            CollapsingHeader::new("Layout").default_open(true).show(ui, |ui| {
                for kind in LayoutKind::all() {
                    let mut selected = self.layout_kind == kind;
                    if ui.checkbox(&mut selected, kind.label()).clicked() && selected {
                        self.layout_kind = kind;
                    }
                }
                if self.layout_kind == LayoutKind::Layered {
                    ui.horizontal(|ui| {
                        ui.label("Direction");
                        ui.selectable_value(&mut self.layered_rankdir, hiker_graph::RankDir::Tb, "Top-Down");
                        ui.selectable_value(&mut self.layered_rankdir, hiker_graph::RankDir::Lr, "Left-Right");
                    });
                }
                // Force-directed morph controls. [force-cfg-anchor-stiffness]
                if self.layout_kind == LayoutKind::ForceDirected {
                    ui.add(egui::Slider::new(&mut self.anchor_stiffness, 0.0..=1.0).text("Anchor stiffness"))
                        .on_hover_text("0 = lively/free re-layout, higher = stays put as the graph changes");
                    // Relayout on release only — dragging would respawn the worker every frame.
                    let sr = ui.add(egui::Slider::new(&mut self.layout_spread, 0.5..=4.0).text("Spread"))
                        .on_hover_text("How far the graph spreads out before settling");
                    let it = ui.add(egui::Slider::new(&mut self.settle_iters, 200..=8000).text("Settle iters").logarithmic(true))
                        .on_hover_text("Force-layout iteration budget (stops early if it converges)");
                    if sr.drag_stopped() || (sr.changed() && !sr.dragged()) || it.drag_stopped() || (it.changed() && !it.dragged()) {
                        spread_relayout = true;
                    }
                }
            });

            CollapsingHeader::new("Display").default_open(true).show(ui, |ui| {
                ui.checkbox(&mut self.toggles.show_labels, "Labels");
                ui.checkbox(&mut self.toggles.show_edges, "Edges");
                for (label, flag) in extra_toggles.iter_mut() {
                    ui.checkbox(flag, *label);
                }
                ui.checkbox(&mut self.toggles.show_preview, "Show note preview");
                ui.checkbox(&mut self.gpu_instancing, "GPU instancing")
                    .on_hover_text("Draw node fills + edge lines via a custom wgpu pipeline (wgpu backend only)");
            });

            CollapsingHeader::new("Highlight").default_open(false).show(ui, |ui| {
                ui.checkbox(&mut self.highlight.hover_edges, "Edges on hover")
                    .on_hover_text("Light up a node's connected edges while hovering it");
                ui.checkbox(&mut self.highlight.fluid, "Fluid highlight")
                    .on_hover_text(
                        "Hover energy flows through edges like a fluid, drawn toward the \
                         selected node; off = a discrete cross-fade between hovered nodes",
                    );
                ui.checkbox(&mut self.highlight.selected_edges, "Edges of selected node")
                    .on_hover_text("Keep the drilled-into / selected node's edges highlighted");
                ui.checkbox(&mut self.highlight.dim_labels, "Dim labels to selection")
                    .on_hover_text(
                        "With a node selected: its label full strength, 1-hop neighbours \
                         semi-dimmed, everything else dimmed",
                    );
                color_row(ui, "Color", &mut self.highlight.color);
                ui.add(egui::Slider::new(&mut self.highlight.width, 0.5..=6.0).text("Width"));
                ui.add(egui::Slider::new(&mut self.highlight.opacity, 0.0..=1.0).text("Opacity"));
                ui.add(egui::Slider::new(&mut self.highlight.softness, 0.0..=1.0).text("Softness"))
                    .on_hover_text("Soft glow halo around the highlighted edges");
                ui.add(egui::Slider::new(&mut self.highlight.fade_secs, 0.0..=0.6).text("Fade (s)"))
                    .on_hover_text("Hover fade in/out duration");
            });

            CollapsingHeader::new("Edge flow").default_open(false).show(ui, |ui| {
                ui.checkbox(&mut self.flow_enabled, "Edge flow").on_hover_text(
                    "Animate tracer dots along each edge from caller to callee (GPU paint path only)",
                );
                if self.flow_enabled {
                    color_row(ui, "Flow color", &mut self.flow_color);
                    ui.add(egui::Slider::new(&mut self.flow_size, 1.0..=6.0).text("Size"));
                    ui.add(egui::Slider::new(&mut self.flow_alpha, 0.0..=1.0).text("Opacity"));
                    ui.add(egui::Slider::new(&mut self.flow_density, 1..=8).text("Density"));
                    ui.add(egui::Slider::new(&mut self.flow_speed, 0.05..=2.0).text("Speed"));
                }
            });

            CollapsingHeader::new("Colors").default_open(false).show(ui, |ui| {
                // Node palette is a vault-graph concept; hidden where nodes are
                // coloured by another rule (e.g. code-graph kinds).
                if self.palette_editable {
                    palette_rows(ui, &mut self.style.palette);
                }
                color_row(ui, "Edges", &mut self.style.edge_color);
                color_row(ui, "Labels", &mut self.style.label_color);
                // Optional translucent pill behind labels (legibility at low LOD).
                let mut label_bg_on = self.style.label_bg.is_some();
                if ui.checkbox(&mut label_bg_on, "Label background").changed() {
                    self.style.label_bg =
                        label_bg_on.then(|| egui::Color32::from_rgba_unmultiplied(0, 0, 0, 160));
                }
                if let Some(mut bg) = self.style.label_bg
                    && color_row(ui, "Label background color", &mut bg)
                {
                    self.style.label_bg = Some(bg);
                }
                let theme_bg = ui.visuals().extreme_bg_color;
                let mut bg = self.style.background.unwrap_or(theme_bg);
                if color_row(ui, "Background", &mut bg) {
                    self.style.background = Some(bg);
                }
            });

            CollapsingHeader::new("Size").default_open(false).show(ui, |ui| {
                ui.add(egui::Slider::new(&mut self.style.node_scale, 0.3..=3.0).text("Nodes"));
                ui.add(egui::Slider::new(&mut self.style.edge_width, 0.25..=4.0).text("Edges"));
                ui.add(egui::Slider::new(&mut self.style.label_size, 7.0..=20.0).text("Labels"));
            });

            CollapsingHeader::new("Projection").default_open(false).show(ui, |ui| {
                for (kind, label) in [
                    (ProjectionKind::Affine, "Off"),
                    (ProjectionKind::Fisheye, "Fisheye"),
                    (ProjectionKind::Poincare, "Poincaré"),
                ] {
                    let mut selected = self.projection.kind == kind;
                    if ui.checkbox(&mut selected, label).clicked() && selected {
                        self.projection.kind = kind;
                        self.needs_fit = true;
                    }
                }
                if self.projection.kind != ProjectionKind::Affine {
                    ui.add(egui::Slider::new(&mut self.projection.strength, 0.1..=3.0).text("Strength"));
                    ui.add(egui::Slider::new(&mut self.projection.size_falloff, 0.0..=1.0).text("Size falloff"));
                    ui.label(egui::RichText::new("Focus").small().color(theme::muted()));
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.focus_mode, FocusMode::LockedCenter, "Center");
                        ui.selectable_value(&mut self.focus_mode, FocusMode::Cursor, "Cursor");
                        ui.selectable_value(&mut self.focus_mode, FocusMode::Selection, "Selection");
                    });
                    ui.label(egui::RichText::new("Detail (LOD)").small().color(theme::muted()));
                    ui.add(egui::Slider::new(&mut self.lod_full_mag, 0.0..=1.0).text("Full above"));
                    ui.add(egui::Slider::new(&mut self.lod_marker_mag, 0.0..=1.0).text("Dot above"));
                    ui.label(egui::RichText::new("Edges").small().color(theme::muted()));
                    ui.checkbox(&mut self.geodesic_edges, "Curved (geodesic)");
                    ui.add(egui::Slider::new(&mut self.projection.geodesic_segments, 2..=64).text("Segments"));
                    if self.projection.kind == ProjectionKind::Poincare {
                        ui.label(egui::RichText::new("Fly-to").small().color(theme::muted()));
                        ui.checkbox(&mut self.flyto_enabled, "Click to fly-to");
                        ui.add(egui::Slider::new(&mut self.flyto_duration, 0.1..=2.0).text("Duration (s)"));
                        ui.label(egui::RichText::new("Boundary fade").small().color(theme::muted()));
                        ui.add(egui::Slider::new(&mut self.fade_start, 0.0..=1.0).text("Start"));
                        ui.add(egui::Slider::new(&mut self.fade_strength, 0.0..=1.0).text("Strength"));
                        ui.checkbox(&mut self.show_boundary, "Boundary ring");
                    }
                }
            });

            ui.separator();
            if ui.button("Reset style").clicked() {
                self.style = match self.style.palette {
                    Palette::Flat { .. } => Style::flat(),
                    Palette::Policy { .. } => Style::policy(),
                };
            }
          });
        });
        // A layout change is a kind switch, a layered rank-direction switch, or a
        // spread change — all need a relayout.
        self.layout_kind != prev_kind
            || self.layered_rankdir != prev_rankdir
            || spread_relayout
    }
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

/// Inputs to one node-paint pass: the active lens, the view zoom (node radius),
/// the label-LOD zoom (the semantic label-reveal gate — `view.zoom` under Affine,
/// `poincare_zoom` under Poincaré, so it's decoupled from radius scaling), and
/// the interaction state (hover/click — both empty for a read-only pane).
struct NodePaint<'a> {
    lens: &'a Lens,
    zoom: f32,
    label_zoom: f32,
    hovered: Option<usize>,
    response_clicked: bool,
    /// Per-node label alpha factors (selection dimming, `graph-label-dim`);
    /// `None` = no dimming this frame.
    label_dim: Option<&'a [f32]>,
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

