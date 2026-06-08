//! Headless PNG snapshots of the graph view in each projection mode, via
//! `egui_kittest`'s wgpu backend — so the lens integration can be inspected as
//! an image without a display.
//!
//! Renders the same synthetic clustered graph under Off (Affine) / Fisheye /
//! Poincaré plus a 3-up comparison, into `widgets/graph-view/target/`. If wgpu
//! cannot initialize (no Vulkan/GL software backend) it prints a clear message
//! and exits 0 rather than failing the build, mirroring the htmlview snapshot.

#[path = "shared/mod.rs"]
mod synthetic;

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use eframe::egui;
use hiker_graph::LayoutKind;
use hiker_graph_view::graph_view::{
    Corner, MinimapShape, NodeDescriptor, NodeShape, Source, State, Style,
};
use hiker_projection::{Complex, Mobius, ProjectionKind};
use synthetic::{LayeredGraph, SyntheticGraph};

const SIZE: f32 = 600.0;

/// Strength all snapshots render the lens at.
const STRENGTH: f32 = 1.2;

/// Centroid focus + normalising scale, mirroring the engine's private `Lens`
/// math so the filmstrip can target a node's pre-nav disk point.
fn focus_scale(positions: &[egui::Vec2]) -> (egui::Vec2, f32) {
    let mut sum = egui::Vec2::ZERO;
    for &p in positions {
        sum += p;
    }
    let focus = sum / positions.len() as f32;
    let scale = positions
        .iter()
        .map(|&p| (p - focus).length())
        .fold(0.0_f32, f32::max)
        .max(1.0);
    (focus, scale)
}

/// The pre-nav Poincaré disk point of a world position (no Möbius nav applied):
/// `forward((w − focus) / scale)` at the snapshot strength.
fn disk_point(w: egui::Vec2, focus: egui::Vec2, scale: f32) -> Complex {
    let rel = (w - focus) / scale;
    let cfg = hiker_projection::ProjectionConfig {
        kind: ProjectionKind::Poincare,
        strength: STRENGTH,
        ..Default::default()
    };
    hiker_projection::forward(Complex::from([rel.x, rel.y]), cfg)
}

/// Render the synthetic graph in `kind` to a `SIZE`×`SIZE` PNG at `out_path`.
fn render_mode(kind: ProjectionKind, out_path: &PathBuf) -> Result<(u32, u32), String> {
    render_with(kind, Mobius::identity(), out_path)
}

/// Render the synthetic graph in `kind` with an explicit Poincaré navigation
/// transform `nav` to a PNG at `out_path`. `nav` is ignored by the engine for
/// non-Poincaré modes (so Off/Fisheye stay byte-identical).
fn render_with(
    kind: ProjectionKind,
    nav: Mobius,
    out_path: &PathBuf,
) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            // A fresh State each frame: `State::ui` auto-fits (on `needs_fit`)
            // and draws in the same pass, so the framing is correct without
            // cross-frame persistence. Deterministic positions ⇒ no worker.
            let graph = SyntheticGraph::new();
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = graph.positions();
            state.worker = None;
            state.projection.kind = kind;
            state.projection.strength = STRENGTH;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            // The Poincaré disk is locked to the pane (fit-to-pane by
            // construction), so there's no view to frame — just drop `needs_fit`
            // (which would otherwise reset `nav`) and install the navigation.
            if kind == ProjectionKind::Poincare {
                state.needs_fit = false;
                state.nav = nav;
            }
            state.ui(ui, &graph, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the synthetic graph under Poincaré with a caller-supplied tweak to the
/// `State` (LOD thresholds, focus mode, fade, …) applied after the standard
/// framing but before the draw. Mirrors `render_mode`'s framing so the result is
/// directly comparable to the baseline Poincaré snapshot.
fn render_poincare_tuned(
    tweak: impl Fn(&mut State) + Send + Sync + 'static,
    out_path: &PathBuf,
) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let graph = SyntheticGraph::new();
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = graph.positions();
            state.worker = None;
            state.projection.kind = ProjectionKind::Poincare;
            state.projection.strength = STRENGTH;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            // Locked disk: no view framing needed (fit-to-pane by construction).
            state.needs_fit = false;
            tweak(&mut state);
            state.ui(ui, &graph, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the synthetic graph with the main pane in `kind` plus an always-on
/// corner Poincaré minimap of the given `shape`, to a PNG at `out_path`.
fn render_minimap(
    kind: ProjectionKind,
    shape: MinimapShape,
    out_path: &PathBuf,
) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let graph = SyntheticGraph::new();
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = graph.positions();
            state.worker = None;
            state.projection.kind = kind;
            state.projection.strength = STRENGTH;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            state.show_minimap = true;
            state.minimap_corner = Corner::BottomRight;
            state.minimap_shape = shape;
            state.ui(ui, &graph, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render one frame of the click-to-expand swap at a forced `swap_t`, with the
/// main (Euclidean) content as Off (Affine), to a PNG at `out_path`.
fn render_expand(swap_t: f32, out_path: &PathBuf) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let graph = SyntheticGraph::new();
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = graph.positions();
            state.worker = None;
            state.projection.kind = ProjectionKind::Affine;
            state.projection.strength = STRENGTH;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            state.show_minimap = true;
            state.minimap_corner = Corner::BottomRight;
            state.minimap_shape = MinimapShape::Circle;
            state.set_swap_t_for_demo(swap_t);
            state.ui(ui, &graph, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the hardcoded layered DAG with [`LayoutKind::Layered`] (Top-Down) to a
/// PNG at `out_path`. Drives the real `recompute_layout`, so the node positions
/// and orthogonal edge routes come straight from the dagre layered engine — the
/// result should show nodes in horizontal ranks with poly-line edges between
/// them (not a force-directed scatter with straight diagonals).
fn render_layered(out_path: &PathBuf) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let graph = LayeredGraph::new();
            let mut state = State::new(Style::flat(), LayoutKind::Layered);
            state.layered_rankdir = hiker_graph::RankDir::Tb;
            // Run the real layered layout: this fills `positions` + `edge_routes`.
            state.recompute_layout(&graph, synthetic::layout_config());
            state.worker = None;
            state.projection.kind = ProjectionKind::Affine;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            state.ui(ui, &graph, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// A synthetic colored Source mirroring the canvas OVERVIEW's look (the app's
/// `CanvasGraphSource`): a handful of positioned, COLORED dots with edges, a
/// couple flagged as in-viewport (brighter fill + highlight stroke). Proves the
/// simplified colored+highlighted overview render without the graph-view example
/// depending on the canvas crates. status: canvas-minimap
struct ColoredOverviewSource {
    positions: Vec<egui::Vec2>,
    edges: Vec<(u32, u32)>,
    fills: Vec<egui::Color32>,
    in_viewport: Vec<bool>,
}

impl ColoredOverviewSource {
    fn new() -> Self {
        // Card centers spread like a real board (a cluster + outliers).
        let positions = vec![
            egui::vec2(0.0, 0.0),
            egui::vec2(260.0, 40.0),
            egui::vec2(120.0, 240.0),
            egui::vec2(-220.0, 160.0),
            egui::vec2(-80.0, -240.0),
            egui::vec2(360.0, -180.0),
            egui::vec2(-360.0, -120.0),
        ];
        // Distinct preset-style hues so the dots read as colored, not uniform.
        let fills = vec![
            egui::Color32::from_rgb(0xff, 0x7b, 0x72),
            egui::Color32::from_rgb(0x6c, 0xc6, 0x74),
            egui::Color32::from_rgb(0x56, 0xc2, 0xd6),
            egui::Color32::from_rgb(0xc4, 0x8b, 0xf0),
            egui::Color32::from_rgb(0xff, 0xa6, 0x57),
            egui::Color32::from_rgb(0xe3, 0xc5, 0x4a),
            egui::Color32::from_gray(0x80),
        ];
        let edges = vec![(0, 1), (0, 2), (0, 3), (1, 5), (3, 6), (2, 1), (0, 4)];
        // The central cluster (nodes 0..=2) is "in viewport" → highlighted.
        let in_viewport = (0..positions.len()).map(|i| i < 3).collect();
        Self { positions, edges, fills, in_viewport }
    }
}

impl Source for ColoredOverviewSource {
    fn node_count(&self) -> usize {
        self.positions.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        let highlight = egui::Color32::from_rgb(0x5b, 0x9d, 0xff);
        positions
            .iter()
            .enumerate()
            .map(|(index, &world_pos)| {
                let hot = self.in_viewport[index];
                let base = self.fills[index];
                let fill = if hot { brighten(base) } else { base };
                NodeDescriptor {
                    index,
                    world_pos,
                    radius: if hot { 8.4 } else { 6.0 },
                    shape: NodeShape::Circle,
                    fill,
                    resting_stroke: if hot {
                        egui::Stroke::new(2.0, highlight)
                    } else {
                        egui::Stroke::NONE
                    },
                    hover_stroke: egui::Stroke::new(2.0, highlight),
                    label: None,
                    label_min_zoom: 0.9,
                    click_path: Some(format!("card-{index}")),
                    tooltip: None,
                }
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.edges.clone()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> hiker_graph::LayoutTree {
        hiker_graph::LayoutTree {
            n: 0,
            children: Vec::new(),
            roots: Vec::new(),
            depth: Vec::new(),
            subtree_leaves: Vec::new(),
        }
    }

    fn preview_for(&self, _index: usize) -> Option<(String, String)> {
        None
    }
}

/// Blend a color toward white (the in-viewport indicator), mirroring the app's
/// `overview::brighten`.
fn brighten(c: egui::Color32) -> egui::Color32 {
    let mix = |v: u8| (u16::from(v) + (255 - u16::from(v)) * 6 / 10) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// Render the canvas-overview look: a locked Poincaré disk of COLORED dots (the
/// simplified canvas graph), edges as thin geodesics, in-viewport nodes
/// highlighted — saved to `canvas-overview.png`. status: canvas-minimap
fn render_canvas_overview(out_path: &PathBuf) -> Result<(u32, u32), String> {
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let source = ColoredOverviewSource::new();
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = source.positions.clone();
            state.worker = None;
            state.projection.kind = ProjectionKind::Poincare;
            state.projection.strength = 1.0;
            state.toggles.show_labels = false;
            state.toggles.show_preview = false;
            // Locked disk: fit-to-pane by construction, so drop `needs_fit`.
            state.needs_fit = false;
            state.ui(ui, &source, |_p, _r, _t, _b, _a| {});
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            let (w, h) = (image.width(), image.height());
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok((w, h))
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Tracks whether any frame rendered and the first failure, so the suite can
/// print a single clear "no GPU backend" message at the end and still exit 0.
#[derive(Default)]
struct Recorder {
    any_ok: bool,
    first_err: Option<String>,
}

impl Recorder {
    /// Record one render outcome, printing an `OK`/`SKIP` line. Returns the
    /// loaded image on success so callers can collect filmstrip frames.
    fn record(&mut self, label: &str, path: &Path, r: Result<(u32, u32), String>) -> Option<image::RgbaImage> {
        match r {
            Ok((w, h)) => {
                self.any_ok = true;
                println!("OK {label} -> {} ({w}x{h})", display(path));
                image::open(path).ok().map(|img| img.to_rgba8())
            }
            Err(e) => {
                println!("SKIP {label}: {e}");
                self.first_err.get_or_insert(e);
                None
            }
        }
    }
}

/// Render a horizontal strip from `frames` (only if all `expected` are present)
/// and report the save.
fn save_strip(frames: &[image::RgbaImage], expected: usize, label: &str, path: &Path) {
    if frames.len() != expected {
        return;
    }
    match hstrip(frames).save(path) {
        Ok(()) => println!("OK {label} -> {}", display(path)),
        Err(e) => println!("SKIP {label}: {e}"),
    }
}

/// The three base projection modes plus their 3-up comparison strip.
fn render_modes(target: &Path, rec: &mut Recorder) {
    let jobs = [
        ("off", ProjectionKind::Affine, target.join("proj-graph-off.png")),
        ("fisheye", ProjectionKind::Fisheye, target.join("proj-graph-fisheye.png")),
        ("poincare", ProjectionKind::Poincare, target.join("proj-graph-poincare.png")),
    ];
    let mut images: Vec<image::RgbaImage> = Vec::new();
    for (label, kind, path) in &jobs {
        if let Some(img) = rec.record(label, path, render_mode(*kind, path)) {
            images.push(img);
        }
    }
    save_strip(&images, 3, "compare", &target.join("proj-graph-compare.png"));
}

/// Corner minimap: main pane Off (Affine) with a Poincaré overview disk in the
/// corner, in both frame shapes, plus one over a Fisheye main pane.
fn render_minimaps(target: &Path, rec: &mut Recorder) {
    let minimap_jobs = [
        ("minimap-circle", ProjectionKind::Affine, MinimapShape::Circle,
         target.join("proj-graph-minimap-circle.png")),
        ("minimap-square", ProjectionKind::Affine, MinimapShape::Square,
         target.join("proj-graph-minimap-square.png")),
        ("minimap-fisheye", ProjectionKind::Fisheye, MinimapShape::Circle,
         target.join("proj-graph-minimap-fisheye.png")),
    ];
    for (label, kind, shape, path) in &minimap_jobs {
        rec.record(label, path, render_minimap(*kind, *shape, path));
    }
}

/// The three single-frame tuned Poincaré snapshots: LOD ladder, focus mode, and
/// boundary fade.
fn render_tuned(target: &Path, rec: &mut Recorder) {
    // LOD ladder: central nodes render FULL, the mid ring as small DOTS, and
    // the outermost nodes as tiny MARKERS. A higher strength pushes the ring
    // clusters far enough toward the rim that their magnification drops through
    // both LOD thresholds.
    {
        let path = target.join("proj-graph-lod.png");
        let tweak = |state: &mut State| {
            // A moderate lens spreads the clusters across the disk; soften the
            // boundary fade so the rim MARKER tier stays visible rather than
            // fading to transparent before it can be seen.
            state.projection.strength = 1.4;
            state.fade_strength = 0.45;
            // Refit for the tweaked lens so the disk still frames cleanly.
            state.needs_fit = true;
        };
        rec.record("lod", &path, render_poincare_tuned(tweak, &path));
    }

    // Focus mode: lock the lens focus onto a peripheral node so the disk
    // recentres on it (its cluster sits at disk centre, the rest swings around)
    // instead of on the centroid.
    {
        let graph = SyntheticGraph::new();
        let positions = graph.positions();
        let peripheral = most_peripheral(&positions);
        let path = target.join("proj-graph-focus.png");
        let tweak = move |state: &mut State| {
            // Selection focus recentres the lens on the peripheral node, so its
            // cluster lands at the fixed disk centre. The disk frame stays
            // locked to the pane — no view framing needed.
            state.set_focus_node_for_demo(peripheral);
        };
        rec.record("focus", &path, render_poincare_tuned(tweak, &path));
    }

    // Boundary fade: a low `fade_start` + full `fade_strength` so the periphery
    // recedes much more than the default fade.
    {
        let path = target.join("proj-graph-fade.png");
        let tweak = |state: &mut State| {
            state.fade_start = 0.15;
            state.fade_strength = 1.0;
        };
        rec.record("fade", &path, render_poincare_tuned(tweak, &path));
    }
}

/// Fly-to filmstrip: glide a peripheral node from rim to disk centre under
/// Poincaré. The disk centre moves along the path from the centroid (ORIGIN,
/// pre-nav) to the target node's pre-nav disk point; each frame's nav is the
/// recentre mapping that eased point to the origin, so the node slides in and
/// the rest of the graph swings hyperbolically around it.
fn render_flyto_strip(target: &Path, rec: &mut Recorder) {
    let graph = SyntheticGraph::new();
    let positions = graph.positions();
    let (focus, scale) = focus_scale(&positions);
    let target_idx = most_peripheral(&positions);
    let target_pt = disk_point(positions[target_idx], focus, scale);
    let start = Complex::ORIGIN;
    let fractions = [0.0_f32, 0.33, 0.66, 1.0];
    let mut frames: Vec<image::RgbaImage> = Vec::new();
    for (i, &e) in fractions.iter().enumerate() {
        let c = Complex::new(
            start.re + (target_pt.re - start.re) * e,
            start.im + (target_pt.im - start.im) * e,
        );
        let nav = Mobius::from_point_pair(c, Complex::ORIGIN);
        let path = target.join(format!("proj-graph-flyto-{i}.png"));
        if let Some(img) = rec.record(&format!("flyto-{i}"), &path, render_with(ProjectionKind::Poincare, nav, &path)) {
            frames.push(img);
        }
    }
    save_strip(&frames, 4, "flyto-strip", &target.join("proj-graph-flyto-strip.png"));
}

/// Expand filmstrip: the click-to-expand swap at four forced `swap_t` stops.
/// Main content is Off (Affine); as `swap_t` runs 0 → 1 the Poincaré disk grows
/// from the corner to fill the pane while the Euclidean graph shrinks from full
/// into the corner (an inversion).
fn render_expand_strip(target: &Path, rec: &mut Recorder) {
    let mut expand_frames: Vec<image::RgbaImage> = Vec::new();
    let swap_stops = [0.0_f32, 0.33, 0.66, 1.0];
    for (i, &t) in swap_stops.iter().enumerate() {
        let path = target.join(format!("proj-graph-expand-{i}.png"));
        if let Some(img) = rec.record(&format!("expand-{i}"), &path, render_expand(t, &path)) {
            expand_frames.push(img);
        }
    }
    save_strip(&expand_frames, 4, "expand-strip", &target.join("proj-graph-expand-strip.png"));
}

/// The index of the node with the largest pre-nav disk radius (the most
/// peripheral cluster member), used to pick a node that spans the whole disk.
fn most_peripheral(positions: &[egui::Vec2]) -> usize {
    let (focus, scale) = focus_scale(positions);
    (0..positions.len())
        .max_by(|&a, &b| {
            disk_point(positions[a], focus, scale)
                .abs()
                .total_cmp(&disk_point(positions[b], focus, scale).abs())
        })
        .unwrap_or(0)
}

fn main() {
    if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        // SAFETY: set at program start, before any thread that reads the
        // environment is spawned (the wgpu backend reads it on first init).
        unsafe { std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1") };
    }

    let target = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target"));
    let _ = std::fs::create_dir_all(&target);

    let mut rec = Recorder::default();
    {
        let path = target.join("proj-graph-layered.png");
        rec.record("layered", &path, render_layered(&path));
    }
    render_modes(&target, &mut rec);
    {
        let path = target.join("canvas-overview.png");
        rec.record("canvas-overview", &path, render_canvas_overview(&path));
    }
    render_minimaps(&target, &mut rec);
    render_tuned(&target, &mut rec);
    render_flyto_strip(&target, &mut rec);
    render_expand_strip(&target, &mut rec);

    if !rec.any_ok {
        println!();
        println!(
            "Headless snapshot could not render: {}",
            rec.first_err.unwrap_or_default()
        );
        println!("This environment appears to lack a usable GPU/software (Vulkan/GL) backend.");
    }
}

/// Lay images out left-to-right with an 8px dark gutter between them.
fn hstrip(images: &[image::RgbaImage]) -> image::RgbaImage {
    let (w, h) = images[0].dimensions();
    let gap = 8u32;
    let n = images.len() as u32;
    let mut strip = image::RgbaImage::from_pixel(
        w * n + gap * (n - 1),
        h,
        image::Rgba([0x12, 0x12, 0x14, 0xff]),
    );
    for (i, img) in images.iter().enumerate() {
        let x0 = i as u32 * (w + gap);
        for y in 0..h {
            for x in 0..w {
                strip.put_pixel(x0 + x, y, *img.get_pixel(x, y));
            }
        }
    }
    strip
}

fn display(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}
