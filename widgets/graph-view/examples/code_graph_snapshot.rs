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
use hiker_graph_view::graph_view::{NodeDescriptor, NodeShape, Source, State, Style};
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
                    label: Some(n.name.clone()),
                    label_min_zoom: 0.0,
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

fn main() {
    let mut args = std::env::args().skip(1);
    let index = args.next().unwrap_or_else(|| "code-intel/fixtures/pyproj.scip".to_string());
    let repo = args.next().unwrap_or_else(|| "code-intel/fixtures/pyproj".to_string());
    let out: PathBuf = args.next().unwrap_or_else(|| "code_graph.png".to_string()).into();

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
    let positions = layout(&graph);
    let source = CodeGraphSource { graph };

    let renderer = match std::panic::catch_unwind(AssertUnwindSafe(egui_kittest::wgpu::WgpuTestRenderer::new)) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("wgpu backend failed to initialize (no GPU/software device) — skipping snapshot");
            return;
        }
    };

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        .renderer(renderer)
        .build_ui(move |ui| {
            let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
            state.positions = positions.clone();
            state.worker = None; // positions are precomputed ⇒ no background layout
            state.toggles.show_labels = true;
            state.toggles.show_edges = true;
            state.toggles.show_preview = false;
            state.ui(ui, &source, |_p, _r, _t, _b, _a| {});
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
