//! Headless PNG snapshot of a **real code project** rendered through this engine's [`Source`]
//! trait — the QA proof for `code-graph-view-source` (hiker-integration-plan.md item C).
//!
//! Loads a `.scip` index via `hiker-code`'s `ScipAdapter`, takes its `code_graph()`, adapts it to
//! the engine via [`CodeGraphSource`] (entity-kind → shape/colour, edges → index pairs), lays it
//! out with a small deterministic force pass, and renders it via `egui_kittest`'s wgpu backend —
//! so the code-graph view can be inspected as an image without a display, exactly like
//! `snapshot.rs` does for the synthetic graph.
//!
//! Run (from the hiker repo root):
//!   cargo run -p hiker-graph-view --example code_graph_snapshot -- \
//!       code-intel/fixtures/pyproj.scip code-intel/fixtures/pyproj [out.png]
//! If wgpu can't initialize (no GPU/software device) it prints a message and exits 0.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

use eframe::egui;
use hiker_code::{CodeGraph, ScipAdapter};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::source::{NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::Style;
use hiker_graph_view::graph_view::State;
use hiker_projection::ProjectionKind;
use spec_engine::SourceId;

const SIZE: f32 = 1000.0;

/// Adapts a [`CodeGraph`] to the graph-view [`Source`]. Visual mapping mirrors the standalone
/// `code-cli graph` renderer so the engine view and the CLI SVG agree.
struct CodeGraphSource {
    graph: CodeGraph,
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "code:type" => egui::Color32::from_rgb(0x4f, 0x83, 0xcc),
        "code:function" => egui::Color32::from_rgb(0x4c, 0xaf, 0x72),
        "code:method" => egui::Color32::from_rgb(0x3f, 0xb6, 0xa8),
        "code:module" => egui::Color32::from_rgb(0x95, 0x75, 0xcd),
        "code:macro" => egui::Color32::from_rgb(0xc9, 0x8b, 0x3a),
        "code:constant" => egui::Color32::from_rgb(0xc7, 0x5b, 0x6d),
        "code:field" => egui::Color32::from_rgb(0xb0, 0x89, 0x4a),
        _ => egui::Color32::from_rgb(0x9e, 0x9e, 0x9e),
    }
}

impl Source for CodeGraphSource {
    fn node_count(&self) -> usize {
        self.graph.nodes.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        // Degree drives node size so hubs read as hubs.
        let mut degree = vec![0u32; self.graph.nodes.len()];
        for &(a, b, _) in &self.graph.edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let maxd = degree.iter().copied().max().unwrap_or(1).max(1) as f32;
        self.graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < positions.len())
            .map(|(index, n)| {
                let is_type = n.kind == "code:type";
                NodeDescriptor {
                    index,
                    world_pos: positions[index],
                    radius: 4.0 + 7.0 * (degree[index] as f32 / maxd),
                    shape: if is_type { NodeShape::Square } else { NodeShape::Circle },
                    fill: kind_color(&n.kind),
                    resting_stroke: egui::Stroke::NONE,
                    hover_stroke: egui::Stroke::new(1.5, egui::Color32::WHITE),
                    badge: None,
                    bug_badge: None,
                    label: Some(n.name.clone()),
                    label_min_zoom: 0.0,
                    label_scale: 1.0,
                    click_path: None,
                    tooltip: Some(n.file.clone()),
                }
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.graph.edges.iter().map(|&(a, b, _)| (a as u32, b as u32)).collect()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        // Not called for ForceDirected (we supply positions directly).
        LayoutTree::from_parents(&vec![None; self.graph.nodes.len()])
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.nodes.get(index)?;
        Some((n.name.clone(), format!("{} · {}", n.kind, n.file)))
    }
}

/// Deterministic Fruchterman-Reingold layout (no RNG; golden-angle spiral seed + central gravity),
/// so the snapshot is reproducible. Mirrors the CLI renderer's layout.
/// Bounding extent (w, h) of a set of positions.
fn extent(pos: &[egui::Vec2]) -> (f32, f32) {
    let (mut lo, mut hi) = (egui::vec2(f32::MAX, f32::MAX), egui::vec2(f32::MIN, f32::MIN));
    for &p in pos {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    (hi.x - lo.x, hi.y - lo.y)
}

/// The depth-bounded undirected neighbourhood — a copy of the app's
/// `code_graph::neighborhood`, so the repro matches the drill-down exactly.
fn neighborhood(full: &CodeGraph, focus: usize, depth: usize) -> CodeGraph {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); full.nodes.len()];
    for &(a, b, _) in &full.edges {
        adj[a].push(b);
        adj[b].push(a);
    }
    let mut keep = Vec::new();
    let mut seen = vec![false; full.nodes.len()];
    let mut q = std::collections::VecDeque::from([(focus, 0usize)]);
    seen[focus] = true;
    while let Some((nd, d)) = q.pop_front() {
        keep.push(nd);
        if d == depth { continue; }
        for &m in &adj[nd] {
            if !seen[m] { seen[m] = true; q.push_back((m, d + 1)); }
        }
    }
    let mut remap = vec![usize::MAX; full.nodes.len()];
    for (local, &g) in keep.iter().enumerate() { remap[g] = local; }
    let nodes = keep.iter().map(|&g| { let mut n = full.nodes[g].clone(); n.parent = None; n }).collect();
    let edges = full.edges.iter()
        .filter(|&&(a, b, _)| remap[a] != usize::MAX && remap[b] != usize::MAX)
        .map(|&(a, b, k)| (remap[a], remap[b], k)).collect();
    CodeGraph { nodes, edges }
}

fn layout(graph: &CodeGraph) -> Vec<egui::Vec2> {
    let n = graph.nodes.len();
    if n == 0 {
        return Vec::new();
    }
    let area = SIZE * SIZE;
    let k = (area / n as f32).sqrt();
    let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());
    let c = SIZE / 2.0;
    let mut pos: Vec<egui::Vec2> = (0..n)
        .map(|i| {
            let r = SIZE * 0.45 * ((i as f32 + 0.5) / n as f32).sqrt();
            let a = i as f32 * golden;
            egui::vec2(c + r * a.cos(), c + r * a.sin())
        })
        .collect();
    let iters = if n > 500 { 120 } else { 300 };
    let mut temp = SIZE * 0.12;
    let cool = temp / (iters as f32 + 1.0);
    for _ in 0..iters {
        let mut disp = vec![egui::Vec2::ZERO; n];
        for i in 0..n {
            for j in (i + 1)..n {
                let d = pos[i] - pos[j];
                let dist = d.length().max(0.01);
                let f = k * k / dist;
                let u = d / dist;
                disp[i] += u * f;
                disp[j] -= u * f;
            }
        }
        for &(a, b, _) in &graph.edges {
            let d = pos[a] - pos[b];
            let dist = d.length().max(0.01);
            let f = dist * dist / k;
            let u = d / dist;
            disp[a] -= u * f;
            disp[b] += u * f;
        }
        for i in 0..n {
            disp[i] += (egui::vec2(c, c) - pos[i]) * 0.04;
            let len = disp[i].length().max(0.01);
            pos[i] += disp[i] / len * len.min(temp);
        }
        temp -= cool;
    }
    pos
}

/// A rough fit (zoom, pan) for the Affine view so a panned/zoomed demo render
/// still lands content on screen: zoom frames the positions' extent into `rect`,
/// pan centres their centroid. Mirrors `View::screen_mapper`'s
/// `screen = center + (w + pan) * zoom`, so `pan = -centroid`.
fn base_view(positions: &[egui::Vec2], rect: egui::Rect) -> (f32, egui::Vec2) {
    if positions.is_empty() {
        return (1.0, egui::Vec2::ZERO);
    }
    let (mut lo, mut hi) = (positions[0], positions[0]);
    for &p in positions {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let span = (hi - lo).max(egui::vec2(1.0, 1.0));
    let zoom = ((rect.width() - 80.0) / span.x).min((rect.height() - 80.0) / span.y).clamp(0.005, 6.0);
    (zoom, -(lo + hi) * 0.5)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let index = args.next().unwrap_or_else(|| "code-intel/fixtures/pyproj.scip".to_string());
    let repo = args.next().unwrap_or_else(|| "code-intel/fixtures/pyproj".to_string());
    let out: PathBuf = args.next().unwrap_or_else(|| "code_graph.png".to_string()).into();
    // Projection mode for QA: off (affine) | fisheye | poincare.
    let kind = match args.next().as_deref() {
        Some("fisheye") => ProjectionKind::Fisheye,
        Some("poincare") => ProjectionKind::Poincare,
        _ => ProjectionKind::Affine,
    };

    let adapter = match ScipAdapter::load(index.as_ref(), repo.as_ref(), SourceId(repo.clone())) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("load {index}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[{}] {} entities | impl-edges: {}", adapter.tool(), adapter.node_count(), adapter.impl_source());
    let graph = adapter.code_graph();
    eprintln!("code_graph: {} nodes, {} edges", graph.nodes.len(), graph.edges.len());
    // QA knob for the custom instanced GPU paint path: HIKER_GPU_SNAPSHOT=1 turns
    // it on for this render (the live app does this only under the wgpu backend).
    // Left unset, the committed-snapshot path is untouched (Painter path).
    if std::env::var_os("HIKER_GPU_SNAPSHOT").is_some() {
        hiker_graph_view::graph_view::set_gpu_paint(true);
        eprintln!("GPU instanced paint path: ON");
    }

    // HIKER_FOCUS=<name> [HIKER_DEPTH=n]: render only that node's n-hop
    // neighbourhood (mirrors the app's drill-down), to repro "1 hop has no nodes".
    let graph = if let Ok(name) = std::env::var("HIKER_FOCUS") {
        let depth: usize = std::env::var("HIKER_DEPTH").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        let fi = graph.nodes.iter().position(|n| n.name == name)
            .unwrap_or_else(|| { eprintln!("focus '{name}' not found"); std::process::exit(1) });
        let sub = neighborhood(&graph, fi, depth);
        eprintln!("focus '{name}' depth={depth}: {} nodes, {} edges", sub.nodes.len(), sub.edges.len());
        sub
    } else {
        graph
    };

    let positions = layout(&graph);
    eprintln!("positions: {} | extent {:?}", positions.len(), extent(&positions));
    let source = CodeGraphSource { graph };

    let renderer = match std::panic::catch_unwind(AssertUnwindSafe(egui_kittest::wgpu::WgpuTestRenderer::new)) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("wgpu backend failed to initialize (no GPU/software device) — skipping snapshot");
            return;
        }
    };

    // QA knob: HIKER_PPP=2 renders at a HiDPI pixels-per-point to exercise the
    // GPU callback's points↔pixels transform (the Painter path is the oracle).
    let ppp: f32 = std::env::var("HIKER_PPP").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    // HIKER_WH=1600x1000 sets a non-square pane to exercise the aspect transform.
    let size = std::env::var("HIKER_WH").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some(egui::vec2(w.trim().parse().ok()?, h.trim().parse().ok()?))
    }).unwrap_or(egui::Vec2::splat(SIZE));
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .with_pixels_per_point(ppp)
        .renderer(renderer)
        .build_ui(move |ui| {
            let mut paint = |ui: &mut egui::Ui| {
                let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
                state.positions = positions.clone();
                state.worker = None; // positions are precomputed ⇒ no background layout
                state.toggles.show_labels = true;
                state.toggles.show_edges = true;
                state.toggles.show_preview = false;
                state.projection.kind = kind;
                state.projection.strength = 1.2;
                if kind == ProjectionKind::Poincare {
                    state.needs_fit = false; // the Poincaré disk is locked to the pane
                }
                // HIKER_PAN=dx,dy and/or HIKER_ZOOM=z drive the Affine view to a
                // non-default panned/zoomed state (proving the GPU view-transform
                // uniform matches the Painter at an off-fit view, not just at fit).
                // Setting either cancels the auto-fit so the explicit view sticks.
                let pan = std::env::var("HIKER_PAN").ok().and_then(|s| {
                    let (x, y) = s.split_once(',')?;
                    Some(egui::vec2(x.trim().parse().ok()?, y.trim().parse().ok()?))
                });
                let zoom = std::env::var("HIKER_ZOOM").ok().and_then(|s| s.trim().parse::<f32>().ok());
                if pan.is_some() || zoom.is_some() {
                    // Fit once to get a sane base zoom/pan, then offset by the envs.
                    let base = base_view(&positions, ui.max_rect());
                    state.set_view_for_demo(
                        zoom.unwrap_or(base.0),
                        pan.unwrap_or(egui::Vec2::ZERO) + base.1,
                    );
                }
                // HIKER_FLOW=<seconds> enables the animated edge-flow tracer dots
                // and pins the clock to that value, so the GPU flow pipeline
                // renders at a deterministic phase headlessly (proves the dots
                // land ON the edges, biased toward caller→callee at small t).
                if let Some(t) = std::env::var("HIKER_FLOW").ok().and_then(|s| s.trim().parse::<f32>().ok()) {
                    state.set_flow_for_demo(t);
                    // HIKER_FLOW_DENSITY=<n> overrides dots-per-edge so a QA render
                    // can compare multi-dot vs single-dot zoomed-in visibility.
                    if let Some(d) = std::env::var("HIKER_FLOW_DENSITY").ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                        state.flow_density = d.clamp(1, 8);
                    }
                }
                state.ui(ui, &source, |_p, _r, _t, _b, _a| {});
            };
            // HIKER_INSET=1 paints into an OFFSET sub-rect (asymmetric margins, like
            // real hiker's sidebars/panels around the graph) so the GPU callback's
            // viewport-relative transform is exercised — the bug a full-window pane hides.
            if std::env::var_os("HIKER_INSET").is_some() {
                let f = ui.max_rect();
                let inset = egui::Rect::from_min_max(
                    f.min + egui::vec2(360.0, 110.0),
                    f.max - egui::vec2(300.0, 40.0),
                );
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(inset), |ui| paint(ui));
            } else {
                paint(ui);
            }
        });
    harness.run();

    match std::panic::catch_unwind(AssertUnwindSafe(|| harness.render())) {
        Ok(Ok(image)) => {
            if let Err(e) = image.save(&out) {
                eprintln!("save {}: {e}", out.display());
                std::process::exit(1);
            }
            println!("wrote {} ({}x{})", out.display(), image.width(), image.height());
        }
        Ok(Err(e)) => eprintln!("wgpu render failed: {e}"),
        Err(_) => eprintln!("wgpu render panicked"),
    }
}
