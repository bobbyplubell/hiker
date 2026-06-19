//! HEADLESS per-frame profiling harness for the code graph-view in the
//! **Poincaré** projection at **`Level::Everything`** LOD (labels ON) — the
//! realistic worst case. Produces REAL measured timing numbers, broken into the
//! phases that make up one rendered frame, at increasing node counts, so we can
//! decide whether the bottleneck is CPU-fixable (our Rust loops: geodesic edge
//! sampling, label galley layout, the O(n²) label-overlap de-conflict) or is
//! genuinely egui tessellation/fill (the thing a GPU instancing path offloads).
//!
//! ## Why this re-implements the draw loop instead of calling `State::ui`
//!
//! The interesting phases (`draw_edges`, `draw_nodes`, the geodesic-sampling vs
//! `painter.add` split, the label-galley vs overlap split) all live in
//! `pub(super)` engine code inside `paint_pane_poincare` — unreachable from an
//! example and impossible to time individually through the single `State::ui`
//! entry point. So this harness re-implements the Poincaré draw path
//! **line-for-line** against the engine's own public lens kernel
//! (`hiker_projection::{forward, magnification, sample_geodesic, Mobius}`) and a
//! real `egui::Painter`, with `std::time::Instant` timers around each phase. The
//! lens math (`disk`, `magnification`, `rim_alpha`), the edge routing, the LOD
//! tiering, the label de-confliction loop, and the node-shape draws are copied
//! verbatim from `graph_view/{mod,edges,panes}.rs` (see the inline `MIRRORS:`
//! notes), so the per-phase costs match what the engine actually pays. The only
//! thing not exercised is the engine's hover/preview tail (no cursor headless).
//!
//! Tessellation is measured against the **real** egui context: the closure emits
//! exactly the shapes the engine would, and we time `Context::tessellate(...)`
//! turning those `Shape`s into a `Mesh`. The final wgpu submit is attempted
//! opportunistically; if no GPU/software device initialises we report every CPU
//! phase (the interesting part) and say the submit was skipped.
//!
//! Run (from the hiker repo root):
//!   cargo run --release -p hiker-graph-view --example graph_profile
//!
//! Output: a per-node-count phase table (median + p95 ms) printed to stdout.

use std::time::Instant;

use egui::{Color32, FontId, Pos2, Rect, Shape, Stroke, Vec2};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::source::{NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::Style;
use hiker_graph_view::graph_view::State;
use hiker_projection::{forward, magnification, sample_geodesic, Complex, Mobius, ProjectionConfig, ProjectionKind};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const NODE_COUNTS: &[usize] = &[500, 2000, 8000, 20000];
const FRAMES: usize = 30;
/// Render canvas (square), points. Matches the app's graph pane scale.
const SIZE: f32 = 1200.0;
/// Poincaré spread strength the app renders at (mirrors snapshot.rs `STRENGTH`).
const STRENGTH: f32 = 1.2;
/// `geodesic_segments` default in `ProjectionConfig` (the per-edge sample count).
const GEODESIC_SEGMENTS: u32 = 24;
/// Label font size (mirrors `Style::flat().label_size`).
const LABEL_SIZE: f32 = 11.0;
const EDGE_WIDTH: f32 = 1.0;
const NODE_SCALE: f32 = 1.0;
/// LOD thresholds (mirror `State::new` defaults: full >= 0.5, dot >= 0.15).
const LOD_FULL_MAG: f32 = 0.5;
const LOD_MARKER_MAG: f32 = 0.15;
/// Boundary-fade defaults (mirror `State::new`).
const FADE_START: f32 = 0.6;
const FADE_STRENGTH: f32 = 1.0;

// ---------------------------------------------------------------------------
// Draw-path optimizations (MIRROR `graph_view/edges.rs`).
// ---------------------------------------------------------------------------

/// Alpha at/below which a node or edge contributes no perceptible pixels and is
/// skipped entirely (≈ 3/255). MIRRORS `edges.rs::CULL_ALPHA`.
const CULL_ALPHA: f32 = 0.012;

/// Screen-space pixels per geodesic segment. MIRRORS `edges.rs::SEG_PX`.
const SEG_PX: f32 = 8.0;

/// Segment count for an edge whose on-screen chord is `chord_px` long, clamped to
/// `[2, max]`. MIRRORS `edges.rs::adaptive_segments`.
fn adaptive_segments(chord_px: f32, max: u32) -> u32 {
    let want = (chord_px / SEG_PX).ceil() as u32;
    want.clamp(2, max.max(2))
}

// ---------------------------------------------------------------------------
// Deterministic RNG (xorshift64*) — seeded fixed for reproducibility.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    const fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

// ---------------------------------------------------------------------------
// Synthetic code graph: mixed kinds, avg degree ~5 with a hub/cluster bias so
// `Level::Everything` shows types/functions/methods/fields/etc. all at once.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Node {
    name: String,
    kind: &'static str,
    is_type: bool,
}

struct CodeGraphSyn {
    nodes: Vec<Node>,
    edges: Vec<(u32, u32)>,
    degree: Vec<u32>,
    max_degree: f32,
}

/// The kind mix mirrors a real code graph: ~1 type per ~6 members, the rest a
/// spread of functions/methods/fields/constants/modules.
const KINDS: &[&str] = &[
    "code:type",
    "code:function",
    "code:method",
    "code:method",
    "code:field",
    "code:constant",
    "code:module",
];

impl CodeGraphSyn {
    fn new(n: usize, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let mut nodes = Vec::with_capacity(n);
        for i in 0..n {
            let kind = KINDS[i % KINDS.len()];
            nodes.push(Node {
                name: format!("{}_{i}", short_kind(kind)),
                kind,
                is_type: kind == "code:type",
            });
        }
        // Edges: each node gets ~2-3 out-edges to nearby indices (locality, like
        // calls within a module) plus a few long hub edges, giving avg degree ~5.
        let mut edges = Vec::with_capacity(n * 3);
        for i in 0..n {
            let out = 2 + rng.below(2); // 2..=3
            for _ in 0..out {
                // 80% local (within a window), 20% global hub link.
                let j = if rng.f32() < 0.8 {
                    let window = 40usize.min(n.saturating_sub(1).max(1));
                    let off = 1 + rng.below(window);
                    (i + off) % n
                } else {
                    rng.below(n)
                };
                if j != i {
                    edges.push((i as u32, j as u32));
                }
            }
        }
        let mut degree = vec![0u32; n];
        for &(a, b) in &edges {
            degree[a as usize] += 1;
            degree[b as usize] += 1;
        }
        let max_degree = degree.iter().copied().max().unwrap_or(1).max(1) as f32;
        Self { nodes, edges, degree, max_degree }
    }
}

fn short_kind(kind: &str) -> &'static str {
    match kind {
        "code:type" => "Type",
        "code:function" => "fn",
        "code:method" => "m",
        "code:field" => "f",
        "code:constant" => "C",
        "code:module" => "mod",
        _ => "x",
    }
}

/// MIRRORS `code_graph_snapshot.rs::kind_color`.
fn kind_color(kind: &str) -> Color32 {
    match kind {
        "code:type" => Color32::from_rgb(0x4f, 0x83, 0xcc),
        "code:function" => Color32::from_rgb(0x4c, 0xaf, 0x72),
        "code:method" => Color32::from_rgb(0x3f, 0xb6, 0xa8),
        "code:module" => Color32::from_rgb(0x95, 0x75, 0xcd),
        "code:constant" => Color32::from_rgb(0xc7, 0x5b, 0x6d),
        "code:field" => Color32::from_rgb(0xb0, 0x89, 0x4a),
        _ => Color32::from_rgb(0x9e, 0x9e, 0x9e),
    }
}

// ---------------------------------------------------------------------------
// Deterministic layout seed (golden-angle spiral + a fixed force-iteration
// budget). Reproducible; no async worker, no RNG. Mirrors
// `code_graph_snapshot.rs::layout` but with a capped iteration budget so 20k
// nodes settle in bounded time (the O(n²) repulsion is layout, not per-frame —
// done ONCE, not timed in the frame loop).
// ---------------------------------------------------------------------------

fn layout(g: &CodeGraphSyn) -> Vec<Vec2> {
    let n = g.nodes.len();
    if n == 0 {
        return Vec::new();
    }
    let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
    let c = SIZE / 2.0;
    let mut pos: Vec<Vec2> = (0..n)
        .map(|i| {
            let r = SIZE * 0.45 * ((i as f32 + 0.5) / n as f32).sqrt();
            let a = i as f32 * golden;
            Vec2::new(c + r * a.cos(), c + r * a.sin())
        })
        .collect();
    // A few cheap edge-attraction passes to make clusters real (no O(n²)
    // repulsion at large n — we only need a plausible spread for the lens).
    let k = (SIZE * SIZE / n as f32).sqrt();
    let iters = if n > 4000 { 8 } else { 30 };
    for _ in 0..iters {
        let mut disp = vec![Vec2::ZERO; n];
        for &(a, b) in &g.edges {
            let (a, b) = (a as usize, b as usize);
            let d = pos[a] - pos[b];
            let dist = d.length().max(0.01);
            let f = dist * dist / k;
            let u = d / dist;
            disp[a] -= u * f;
            disp[b] += u * f;
        }
        for i in 0..n {
            disp[i] += (Vec2::new(c, c) - pos[i]) * 0.02;
            let len = disp[i].length().max(0.01);
            pos[i] += disp[i] / len * len.min(SIZE * 0.02);
        }
    }
    pos
}

// ---------------------------------------------------------------------------
// Lens — a faithful copy of `graph_view::mod::Lens` for the Poincaré path, built
// from public `hiker_projection` primitives. `disk`/`magnification`/`rim_alpha`
// match the engine byte-for-byte.
// ---------------------------------------------------------------------------

struct Lens {
    cfg: ProjectionConfig,
    focus: Vec2,
    scale: f32,
    nav: Mobius,
}

/// MIRRORS `mod::centroid_scale`.
fn centroid_scale(positions: &[Vec2]) -> (Vec2, f32) {
    if positions.is_empty() {
        return (Vec2::ZERO, 1.0);
    }
    let mut sum = Vec2::ZERO;
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
    /// MIRRORS `Lens::centred` (centroid focus, identity nav).
    fn centred(cfg: ProjectionConfig, positions: &[Vec2]) -> Self {
        let (centroid, scale) = centroid_scale(positions);
        Self { cfg, focus: centroid, scale, nav: Mobius::identity() }
    }

    /// Centroid focus + a non-identity `nav` that re-centres the disk onto an
    /// off-centre point (MIRRORS an in-app drag / click fly-to: the engine sets
    /// `nav` to a `from_point_pair` recentre). This pushes the far side of the
    /// graph toward |z|→1, the state where the rim/offscreen cull actually fires.
    fn navigated(cfg: ProjectionConfig, positions: &[Vec2]) -> Self {
        let mut me = Self::centred(cfg, positions);
        // Send the pre-nav disk point of an off-centre node to the origin: this is
        // the same recentre the engine performs when you fly-to / drag a node to
        // the middle. Pick a node well off the centroid so a real rim forms.
        let (centroid, _) = centroid_scale(positions);
        // farthest-from-centroid node = a clear rim target.
        let target = positions
            .iter()
            .copied()
            .max_by(|a, b| {
                (*a - centroid).length().partial_cmp(&(*b - centroid).length()).unwrap()
            })
            .unwrap_or(centroid);
        // pre-nav disk coord of that node:
        let rel = (target - me.focus) / me.scale;
        let p = forward(Complex::from([rel.x, rel.y]), me.cfg);
        // Recentre p → origin (drag it to the middle); origin stays at origin.
        me.nav = Mobius::from_point_pair(p, Complex::from([0.0, 0.0]));
        me
    }

    /// MIRRORS `Lens::disk`.
    fn disk(&self, w: Vec2) -> Complex {
        let rel = (w - self.focus) / self.scale;
        let z = forward(Complex::from([rel.x, rel.y]), self.cfg);
        if self.cfg.kind == ProjectionKind::Poincare {
            self.nav.apply(z)
        } else {
            z
        }
    }

    /// MIRRORS `Lens::magnification`.
    fn magnification(&self, w: Vec2) -> f32 {
        magnification(self.disk(w), self.cfg)
    }

    /// MIRRORS `Lens::rim_alpha`.
    fn rim_alpha(&self, w: Vec2, fade_start: f32, fade_strength: f32) -> f32 {
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

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// MIRRORS `mod::fade`.
fn fade(color: Color32, factor: f32) -> Color32 {
    if factor >= 1.0 {
        return color;
    }
    let a = (color.a() as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lod {
    Full,
    Dot,
    Marker,
}

/// MIRRORS `State::lod_tier` (lens always active here).
fn lod_tier(mag: f32) -> Lod {
    let full = LOD_FULL_MAG;
    let marker = LOD_MARKER_MAG.min(full - f32::EPSILON);
    if mag >= full {
        Lod::Full
    } else if mag >= marker {
        Lod::Dot
    } else {
        Lod::Marker
    }
}

// ---------------------------------------------------------------------------
// Phase timings for one frame (milliseconds).
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Phases {
    nodes_build: f64,
    edges_geodesic: f64,
    edges_add: f64,
    edges_total: f64,
    nodes_shapes: f64,
    nodes_label_galley: f64,
    nodes_overlap: f64,
    nodes_total: f64,
    tessellate: f64,
    total: f64,
    // counts (for context, captured once)
    full_count: usize,
    label_count: usize,
    // optimization stats
    nodes_culled: usize,
    edges_culled: usize,
    edges_drawn: usize,
    segments_total: u64,
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Run one frame against the real egui context, returning per-phase timings.
/// The closure emits exactly the engine's Poincaré shapes; tessellation is timed
/// against the resulting `FullOutput::shapes`.
fn run_frame(ctx: &egui::Context, g: &CodeGraphSyn, positions: &[Vec2], navigated: bool) -> Phases {
    use std::cell::RefCell;
    let phases = RefCell::new(Phases::default());

    let frame_start = Instant::now();

    let raw_input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::splat(SIZE))),
        ..Default::default()
    };

    let full_output = ctx.run(raw_input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = Rect::from_min_size(Pos2::ZERO, Vec2::splat(SIZE));
            let painter = ui.painter_at(rect);
            let mut ph = phases.borrow_mut();

            let cfg = ProjectionConfig {
                kind: ProjectionKind::Poincare,
                strength: STRENGTH,
                size_falloff: 1.0,
                geodesic_segments: GEODESIC_SEGMENTS,
            };
            let lens = if navigated {
                Lens::navigated(cfg, positions)
            } else {
                Lens::centred(cfg, positions)
            };

            // Locked Poincaré disk frame (MIRRORS `poincare_disk(rect, 1.0)` with
            // DISK_FILL = 0.92).
            let disk_radius = 0.5 * rect.size().min_elem() * 0.92;
            let disk_center = rect.center();
            let disk_to_screen = |z: Complex| disk_center + Vec2::new(z.re, z.im) * disk_radius;
            let to_screen = |w: Vec2| disk_to_screen(lens.disk(w));

            // ---- PHASE: source.nodes() build ----
            // MIRRORS `CodeGraphSource::nodes`: degree-scaled radius, kind color,
            // shape, label = name.
            let t = Instant::now();
            struct Desc {
                world_pos: Vec2,
                radius: f32,
                is_type: bool,
                fill: Color32,
                label: String,
            }
            let descs: Vec<Desc> = (0..g.nodes.len())
                .map(|i| Desc {
                    world_pos: positions[i],
                    radius: 4.0 + 7.0 * (g.degree[i] as f32 / g.max_degree),
                    is_type: g.nodes[i].is_type,
                    fill: kind_color(g.nodes[i].kind),
                    label: g.nodes[i].name.clone(),
                })
                .collect();
            ph.nodes_build = ms(t.elapsed());

            // ---- PHASE: draw_edges (Poincaré geodesic) ----
            // MIRRORS `State::draw_edges` Poincaré arm. Split: the geodesic
            // sampling (our Rust loop) vs the `painter.add(Shape::line)` cost.
            let edges_t = Instant::now();
            let mut geo_acc = std::time::Duration::ZERO;
            let mut add_acc = std::time::Duration::ZERO;
            let color = Color32::from_rgba_premultiplied(0x90, 0x96, 0xa0, 0xa0);
            let n = positions.len();
            let mut edges_culled = 0usize;
            let mut edges_drawn = 0usize;
            let mut segments_total = 0u64;
            for &(a, b) in &g.edges {
                let (a, b) = (a as usize, b as usize);
                if a >= n || b >= n {
                    continue;
                }
                let (wa, wb) = (positions[a], positions[b]);
                let alpha = lens
                    .rim_alpha(wa, FADE_START, FADE_STRENGTH)
                    .min(lens.rim_alpha(wb, FADE_START, FADE_STRENGTH));
                // OPT 1: edge rim cull — both endpoints faded out => paints nothing.
                if alpha <= CULL_ALPHA {
                    edges_culled += 1;
                    continue;
                }
                let stroke = Stroke::new(EDGE_WIDTH, fade(color, alpha));

                let gt = Instant::now();
                let (za, zb) = (lens.disk(wa), lens.disk(wb));
                // OPT 2: adaptive geodesic segments — density tracks on-screen chord.
                let chord_px = (disk_to_screen(za) - disk_to_screen(zb)).length();
                let segs = adaptive_segments(chord_px, GEODESIC_SEGMENTS);
                segments_total += segs as u64;
                let pts: Vec<Pos2> = sample_geodesic(za, zb, segs)
                    .into_iter()
                    .map(disk_to_screen)
                    .collect();
                geo_acc += gt.elapsed();

                let at = Instant::now();
                painter.add(Shape::line(pts, stroke));
                add_acc += at.elapsed();
                edges_drawn += 1;
            }
            ph.edges_culled = edges_culled;
            ph.edges_drawn = edges_drawn;
            ph.segments_total = segments_total;
            ph.edges_total = ms(edges_t.elapsed());
            ph.edges_geodesic = ms(geo_acc);
            ph.edges_add = ms(add_acc);

            // ---- PHASE: draw_nodes ----
            // MIRRORS `State::draw_nodes`: shape draws + deferred label list, then
            // the sort + O(n²) overlap de-conflict (galley layout inside it).
            let nodes_t = Instant::now();
            let label_font = FontId::proportional(LABEL_SIZE);
            let label_color = Color32::from_gray(0x9b); // theme::muted()-ish
            let mut labels: Vec<(f32, Pos2, String, f32)> = Vec::new();
            let mut full_count = 0usize;
            let mut nodes_culled = 0usize;
            // OPT 1: node rim/offscreen cull setup (MIRRORS edges.rs).
            let clip = painter.clip_rect();
            let label_pad = LABEL_SIZE + 6.0;

            // -- shapes sub-phase --
            let shapes_t = Instant::now();
            for d in &descs {
                let p = to_screen(d.world_pos);
                let mag = lens.magnification(d.world_pos);
                let r = d.radius * NODE_SCALE * 1.0_f32.max(0.4) * mag;
                let alpha = lens.rim_alpha(d.world_pos, FADE_START, FADE_STRENGTH);
                // OPT 1: skip fully rim-faded or fully-offscreen nodes — they paint
                // no visible pixel, so their shape draw + label shaping are dead work.
                if alpha <= CULL_ALPHA || !clip.expand(r + label_pad).contains(p) {
                    nodes_culled += 1;
                    continue;
                }
                let fill = fade(d.fill, alpha);
                let tier = lod_tier(mag);
                match tier {
                    Lod::Full => {
                        if d.is_type {
                            let rect = Rect::from_center_size(p, Vec2::splat(r * 2.0));
                            painter.rect_filled(rect, 1.0, fill);
                        } else {
                            painter.circle(p, r, fill, Stroke::NONE);
                        }
                    }
                    Lod::Dot => {
                        painter.circle_filled(p, r.min(3.0), fill);
                    }
                    Lod::Marker => {
                        painter.circle_filled(p, 1.5, fill);
                    }
                }
                if tier == Lod::Full {
                    full_count += 1;
                    // show_labels && zoom(1.0) >= label_min_zoom(0.0) — always.
                    labels.push((mag, Pos2::new(p.x, p.y + r + 2.0), d.label.clone(), alpha));
                }
            }
            ph.nodes_shapes = ms(shapes_t.elapsed());
            ph.full_count = full_count;
            ph.label_count = labels.len();
            ph.nodes_culled = nodes_culled;

            // -- label de-confliction: sort + O(n²) overlap, galley layout inside --
            labels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut placed: Vec<Rect> = Vec::new();
            let mut galley_acc = std::time::Duration::ZERO;
            let mut overlap_acc = std::time::Duration::ZERO;
            for (_, anchor, text, alpha) in labels {
                let color = fade(label_color, alpha);
                let gt = Instant::now();
                // OPT 3: lay out with a stable PLACEHOLDER colour so egui's
                // cross-frame galley cache (in `Fonts`) keys on geometry alone; the
                // rim-fade `alpha` is applied at paint via the fallback colour below.
                let galley = painter.layout_no_wrap(text, label_font.clone(), Color32::PLACEHOLDER);
                galley_acc += gt.elapsed();

                let top_left = Pos2::new(anchor.x - galley.size().x / 2.0, anchor.y);
                let rect = Rect::from_min_size(top_left, galley.size()).expand(1.0);

                let ot = Instant::now();
                let collides = placed.iter().any(|r| r.intersects(rect));
                overlap_acc += ot.elapsed();
                if collides {
                    continue;
                }
                placed.push(rect);
                painter.galley(top_left, galley, color);
            }
            ph.nodes_label_galley = ms(galley_acc);
            ph.nodes_overlap = ms(overlap_acc);
            ph.nodes_total = ms(nodes_t.elapsed());

            // Boundary ring (MIRRORS `stroke_disk_boundary`).
            painter.circle_stroke(disk_center, disk_radius, Stroke::new(1.0, Color32::from_gray(0x55)));
        });
    });

    // ---- PHASE: tessellation (Shapes -> Mesh), the real egui step ----
    let ppp = full_output.pixels_per_point;
    let t = Instant::now();
    let _primitives = ctx.tessellate(full_output.shapes, ppp);
    let mut ph = phases.into_inner();
    ph.tessellate = ms(t.elapsed());
    ph.total = ms(frame_start.elapsed());
    ph
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

struct Summary {
    median: Phases,
    p95: Phases,
    full_count: usize,
    label_count: usize,
    nodes_culled: usize,
    edges_culled: usize,
    edges_drawn: usize,
    segments_total: u64,
    /// Galley layout-phase ms on a brand-new Context (cache empty).
    cold_galley: f64,
    /// Galley layout-phase ms on the second frame of that Context (cache warm).
    warm_galley: f64,
}

fn summarize(frames: &[Phases]) -> Summary {
    macro_rules! col {
        ($field:ident) => {{
            let mut v: Vec<f64> = frames.iter().map(|f| f.$field).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            (percentile(&v, 50.0), percentile(&v, 95.0))
        }};
    }
    let mut median = Phases::default();
    let mut p95 = Phases::default();
    macro_rules! set {
        ($field:ident) => {{
            let (m, p) = col!($field);
            median.$field = m;
            p95.$field = p;
        }};
    }
    set!(nodes_build);
    set!(edges_geodesic);
    set!(edges_add);
    set!(edges_total);
    set!(nodes_shapes);
    set!(nodes_label_galley);
    set!(nodes_overlap);
    set!(nodes_total);
    set!(tessellate);
    set!(total);
    Summary {
        median,
        p95,
        full_count: frames[0].full_count,
        label_count: frames[0].label_count,
        nodes_culled: frames[0].nodes_culled,
        edges_culled: frames[0].edges_culled,
        edges_drawn: frames[0].edges_drawn,
        segments_total: frames[0].segments_total,
        cold_galley: 0.0,
        warm_galley: 0.0,
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        // SAFETY: set at program start before any thread reads the env.
        unsafe { std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1") };
    }

    println!("# Headless graph-view profiling — Poincaré projection, Level::Everything (labels ON)");
    println!("# Canvas {SIZE}x{SIZE}, strength {STRENGTH}, geodesic_segments {GEODESIC_SEGMENTS}, {FRAMES} frames/size");
    println!("# Synthetic code graph: mixed kinds, avg degree ~5; deterministic (seeded) layout.");
    println!();

    if std::env::var_os("DIAG").is_some() {
        for &n in NODE_COUNTS {
            let g = CodeGraphSyn::new(n, 0xC0DE_F00D ^ n as u64);
            let positions = layout(&g);
            let cfg = ProjectionConfig {
                kind: ProjectionKind::Poincare,
                strength: STRENGTH,
                size_falloff: 1.0,
                geodesic_segments: GEODESIC_SEGMENTS,
            };
            for (tag, lens) in [
                ("centred  ", Lens::centred(cfg, &positions)),
                ("navigated", Lens::navigated(cfg, &positions)),
            ] {
                let mut rs: Vec<f32> = positions.iter().map(|&w| lens.disk(w).abs()).collect();
                rs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let alphas: Vec<f32> =
                    positions.iter().map(|&w| lens.rim_alpha(w, FADE_START, FADE_STRENGTH)).collect();
                let culled = alphas.iter().filter(|&&a| a <= CULL_ALPHA).count();
                eprintln!(
                    "DIAG n={n} {tag}: disk |z| p50={:.3} p95={:.3} max={:.4}  alpha<=CULL: {}/{} ({:.1}%)",
                    rs[rs.len()/2], rs[(rs.len()*95/100).min(rs.len()-1)], rs[rs.len()-1],
                    culled, n, 100.0*culled as f64/n as f64,
                );
            }
        }
        return;
    }

    let mut summaries: Vec<(usize, usize, usize, Summary)> = Vec::new();
    let mut nav_summaries: Vec<(usize, usize, usize, Summary)> = Vec::new();

    for &n in NODE_COUNTS {
        let g = CodeGraphSyn::new(n, 0xC0DE_F00D ^ n as u64);
        let positions = layout(&g);
        let edge_count = g.edges.len();

        // Two views per size: the default CENTRED frame (the original baseline) and
        // a NAVIGATED frame (lens recentred on an off-centre node, mirroring a
        // drag / fly-to) — the realistic interactive state where the rim/offscreen
        // cull actually fires. Each reuses ONE persistent Context so frame≥2 hits
        // egui's galley cache.
        for (navigated, sink) in [(false, &mut summaries), (true, &mut nav_summaries)] {
            let ctx = egui::Context::default();
            for _ in 0..3 {
                let _ = run_frame(&ctx, &g, &positions, navigated);
            }
            let mut frames = Vec::with_capacity(FRAMES);
            for _ in 0..FRAMES {
                frames.push(run_frame(&ctx, &g, &positions, navigated));
            }
            let mut s = summarize(&frames);

            // ---- Galley cache cold-vs-warm (OPT 3) ----
            // A *fresh* Context has an empty galley cache, so its first frame shapes
            // every distinct label from scratch (COLD); identical labels on its
            // second frame hit egui's persistent Fonts galley cache (WARM). Prime
            // the font atlas once first so the atlas build doesn't pollute the cold
            // number — we want the cache effect, not lazy font loading.
            {
                let cold_ctx = egui::Context::default();
                let _ = cold_ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let _ = ui.painter().layout_no_wrap(
                            "warmup".into(),
                            FontId::proportional(LABEL_SIZE),
                            Color32::PLACEHOLDER,
                        );
                    });
                });
                let f_cold = run_frame(&cold_ctx, &g, &positions, navigated);
                let f_warm = run_frame(&cold_ctx, &g, &positions, navigated);
                s.cold_galley = f_cold.nodes_label_galley;
                s.warm_galley = f_warm.nodes_label_galley;
            }

            eprintln!(
                "n={n:6} {}  edges={edge_count:7}  labels={:6}  nodes_cull={:6}  edges_cull={:6}  total median={:.2}ms",
                if navigated { "nav " } else { "cent" },
                s.label_count, s.nodes_culled, s.edges_culled, s.median.total
            );
            sink.push((n, edge_count, s.full_count, s));
        }
    }

    println!("# ============================================================");
    println!("# CENTRED (default un-navigated view) — the original baseline frame");
    println!("# ============================================================");
    print_table(&summaries);
    print_opt_stats(&summaries);

    println!();
    println!("# ============================================================");
    println!("# NAVIGATED (lens recentred on a rim node, mirrors drag/fly-to)");
    println!("# — the interactive state where the rim/offscreen cull fires");
    println!("# ============================================================");
    print_table(&nav_summaries);
    print_opt_stats(&nav_summaries);

    wgpu_end_to_end();
}

/// Per-size optimization stats: cull counts/%, avg segments/edge (flat vs
/// adaptive), and galley cold-vs-warm medians.
fn print_opt_stats(summaries: &[(usize, usize, usize, Summary)]) {
    println!();
    println!("## OPT 1 — rim/offscreen cull (nodes & edges skipped before any draw work)");
    println!();
    let h = ["nodes", "nodes_cull", "node_cull%", "edges", "edges_cull", "edge_cull%", "edges_drawn"];
    let w = [7, 11, 11, 8, 11, 11, 12];
    print_row(&h, &w);
    let sep: Vec<String> = w.iter().map(|x| "-".repeat(*x)).collect();
    println!("{}", sep.join(" "));
    for (n, edges, _full, s) in summaries {
        let node_pct = 100.0 * s.nodes_culled as f64 / (*n as f64).max(1.0);
        let edge_pct = 100.0 * s.edges_culled as f64 / (*edges as f64).max(1.0);
        let cells = [
            n.to_string(),
            s.nodes_culled.to_string(),
            format!("{node_pct:.1}%"),
            edges.to_string(),
            s.edges_culled.to_string(),
            format!("{edge_pct:.1}%"),
            s.edges_drawn.to_string(),
        ];
        let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        print_row(&refs, &w);
    }

    println!();
    println!("## OPT 2 — adaptive geodesic segments (avg segments per DRAWN edge)");
    println!();
    let h2 = ["nodes", "edges_drawn", "flat_seg/edge", "adapt_seg/edge", "seg_saved%"];
    let w2 = [7, 12, 14, 15, 11];
    print_row(&h2, &w2);
    let sep2: Vec<String> = w2.iter().map(|x| "-".repeat(*x)).collect();
    println!("{}", sep2.join(" "));
    for (n, _edges, _full, s) in summaries {
        let drawn = s.edges_drawn.max(1) as f64;
        let adapt = s.segments_total as f64 / drawn;
        let flat = GEODESIC_SEGMENTS as f64;
        let saved = 100.0 * (1.0 - adapt / flat);
        let cells = [
            n.to_string(),
            s.edges_drawn.to_string(),
            format!("{flat:.1}"),
            format!("{adapt:.2}"),
            format!("{saved:.1}%"),
        ];
        let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        print_row(&refs, &w2);
    }

    println!();
    println!("## OPT 3 — galley layout phase: COLD (fresh Context, empty cache) vs WARM (frame 2, cache hit)");
    println!();
    let h3 = ["nodes", "labels", "cold_galley_ms", "warm_galley_ms", "speedup"];
    let w3 = [7, 8, 15, 15, 8];
    print_row(&h3, &w3);
    let sep3: Vec<String> = w3.iter().map(|x| "-".repeat(*x)).collect();
    println!("{}", sep3.join(" "));
    for (n, _edges, _full, s) in summaries {
        let sp = if s.warm_galley > 0.0 { s.cold_galley / s.warm_galley } else { 0.0 };
        let cells = [
            n.to_string(),
            s.label_count.to_string(),
            format!("{:.3}", s.cold_galley),
            format!("{:.3}", s.warm_galley),
            format!("{sp:.1}x"),
        ];
        let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        print_row(&refs, &w3);
    }
    println!();
    println!("Note: the main per-phase table's galley column is WARM (the persistent");
    println!("Context's cache is primed by warmups), matching the in-app steady state.");
}

fn print_table(summaries: &[(usize, usize, usize, Summary)]) {
    println!("## Median per-phase (ms), then total + implied FPS");
    println!();
    let header = [
        "nodes", "edges", "nodes_build", "edges_total", "  geo", "  add", "nodes_total",
        " shapes", " galley", " overlap", "tessellate", "TOTAL", "FPS",
    ];
    let widths = [7, 8, 11, 11, 7, 7, 11, 8, 8, 8, 10, 8, 7];
    print_row(&header, &widths);
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join(" "));
    for (n, edges, _full, s) in summaries {
        let m = &s.median;
        let fps = if m.total > 0.0 { 1000.0 / m.total } else { 0.0 };
        let cells = [
            n.to_string(),
            edges.to_string(),
            fmt(m.nodes_build),
            fmt(m.edges_total),
            fmt(m.edges_geodesic),
            fmt(m.edges_add),
            fmt(m.nodes_total),
            fmt(m.nodes_shapes),
            fmt(m.nodes_label_galley),
            fmt(m.nodes_overlap),
            fmt(m.tessellate),
            fmt(m.total),
            format!("{fps:.1}"),
        ];
        let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        print_row(&refs, &widths);
    }

    println!();
    println!("## p95 per-phase (ms)");
    println!();
    print_row(&header[..header.len() - 1], &widths[..widths.len() - 1]);
    let sep2: Vec<String> = widths[..widths.len() - 1].iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep2.join(" "));
    for (n, edges, _full, s) in summaries {
        let p = &s.p95;
        let cells = [
            n.to_string(),
            edges.to_string(),
            fmt(p.nodes_build),
            fmt(p.edges_total),
            fmt(p.edges_geodesic),
            fmt(p.edges_add),
            fmt(p.nodes_total),
            fmt(p.nodes_shapes),
            fmt(p.nodes_label_galley),
            fmt(p.nodes_overlap),
            fmt(p.tessellate),
            fmt(p.total),
        ];
        let refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        print_row(&refs, &widths[..widths.len() - 1]);
    }
    println!();
    println!("Note: hit-test is NOT exercised (no cursor in a headless frame); it is an");
    println!("O(n) screen-space nearest-node scan run only when the pane is hovered.");
}

fn fmt(v: f64) -> String {
    format!("{v:.2}")
}

fn print_row(cells: &[&str], widths: &[usize]) {
    let row: Vec<String> = cells
        .iter()
        .zip(widths)
        .map(|(c, w)| format!("{c:>width$}", width = w))
        .collect();
    println!("{}", row.join(" "));
}

/// A real engine [`Source`] over the synthetic graph, so the wgpu cross-check
/// drives the ACTUAL `State::ui` Poincaré path (not the replicated loop). Mirrors
/// `code_graph_snapshot.rs::CodeGraphSource`.
struct ProfileSource<'a> {
    g: &'a CodeGraphSyn,
}

impl Source for ProfileSource<'_> {
    fn node_count(&self) -> usize {
        self.g.nodes.len()
    }
    fn nodes(&self, positions: &[Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        (0..self.g.nodes.len())
            .filter(|i| *i < positions.len())
            .map(|i| NodeDescriptor {
                index: i,
                world_pos: positions[i],
                radius: 4.0 + 7.0 * (self.g.degree[i] as f32 / self.g.max_degree),
                shape: if self.g.nodes[i].is_type { NodeShape::Square } else { NodeShape::Circle },
                fill: kind_color(self.g.nodes[i].kind),
                resting_stroke: Stroke::NONE,
                hover_stroke: Stroke::new(1.5, Color32::WHITE),
                badge: None,
                bug_badge: None,
                label: Some(self.g.nodes[i].name.clone()),
                label_min_zoom: 0.0,
                label_scale: 1.0,
                click_path: None,
                tooltip: None,
            })
            .collect()
    }
    fn edges(&self) -> Vec<(u32, u32)> {
        self.g.edges.clone()
    }
    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        LayoutTree::from_parents(&vec![None; self.g.nodes.len()])
    }
    fn preview_for(&self, _index: usize) -> Option<(String, String)> {
        None
    }
}

/// Cross-check: drive the REAL engine `State::ui` (Poincaré, labels on) through
/// egui_kittest's wgpu backend and time the full headless render (CPU draw +
/// tessellation + GPU paint/submit + readback). Reported as an upper-bound
/// end-to-end number that includes the wgpu submit the per-phase table omits, and
/// as a sanity check that the replicated loop tracks the engine. Prints SKIP if
/// no headless device initialises.
fn wgpu_end_to_end() {
    println!();
    println!("## wgpu end-to-end cross-check (REAL State::ui, full render incl. GPU submit + readback)");
    let renderer_probe = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        egui_kittest::wgpu::WgpuTestRenderer::new,
    ));
    if renderer_probe.is_err() {
        println!("SKIP: no headless GPU/software device — final paint/submit NOT measured.");
        println!("All CPU phases above (our Rust loops + egui tessellation) are unaffected.");
        return;
    }
    drop(renderer_probe);

    for &n in NODE_COUNTS {
        let g = CodeGraphSyn::new(n, 0xC0DE_F00D ^ n as u64);
        let positions = layout(&g);
        let src = ProfileSource { g: &g };
        let pos = positions.clone();

        let renderer = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            egui_kittest::wgpu::WgpuTestRenderer::new,
        )) {
            Ok(r) => r,
            Err(_) => {
                println!("n={n}: SKIP (device init failed)");
                continue;
            }
        };
        let mut harness = egui_kittest::Harness::builder()
            .with_size(Vec2::splat(SIZE))
            .renderer(renderer)
            .build_ui(move |ui| {
                let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
                state.positions = pos.clone();
                state.worker = None;
                state.toggles.show_labels = true;
                state.toggles.show_edges = true;
                state.toggles.show_preview = false;
                state.projection.kind = ProjectionKind::Poincare;
                state.projection.strength = STRENGTH;
                state.needs_fit = false;
                state.ui(ui, &src, |_p, _r, _t, _b, _a| {});
            });
        harness.run();
        // Time several full renders; report median.
        let mut samples = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| harness.render()));
            match r {
                Ok(Ok(_img)) => samples.push(ms(t.elapsed())),
                _ => break,
            }
        }
        if samples.is_empty() {
            println!("n={n:6}: render failed/skipped");
        } else {
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = samples[samples.len() / 2];
            println!("n={n:6}: full render (incl. GPU submit + RGBA readback) median = {med:.2} ms");
        }
    }
    println!("(This number includes a full-frame RGBA texture readback that interactive");
    println!(" rendering would NOT pay, so treat it as a loose upper bound, not the live cost.)");
}
