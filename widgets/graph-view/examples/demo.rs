//! Standalone integration example — drop `hiker-graph-view` + `hiker-projection`
//! into your own egui app.
//!
//! A real clustered graph rendered through `hiker_graph_view::State`, with a
//! left panel of projection controls (mode / strength / size falloff / edge
//! segments / boundary) driving the live `State::projection` field. The central
//! panel runs `State::ui` exactly as the hiker app does — the projection is
//! invisible until a non-Off mode is selected, then the layout warps around its
//! centroid focus. Pan/zoom with drag + scroll, as in the app.
//!
//! Run: `cargo run -p hiker-graph-view --example demo`

#[path = "shared/mod.rs"]
mod synthetic;

use eframe::egui;
use hiker_graph::LayoutKind;
use hiker_graph_view::graph_view::styling::Style;
use hiker_graph_view::graph_view::State;
use hiker_projection::{Mobius, ProjectionKind};
use synthetic::SyntheticGraph;

struct DemoApp {
    graph: SyntheticGraph,
    state: State,
}

impl Default for DemoApp {
    fn default() -> Self {
        let graph = SyntheticGraph::new();
        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        state.positions = graph.positions();
        state.worker = None;
        state.toggles.show_labels = false;
        state.toggles.show_preview = false;
        Self { graph, state }
    }
}

const fn mode_label(kind: ProjectionKind) -> &'static str {
    match kind {
        ProjectionKind::Affine => "Off (Affine)",
        ProjectionKind::Fisheye => "Fisheye",
        ProjectionKind::Poincare => "Poincaré",
    }
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("controls")
            .resizable(false)
            .default_width(230.0)
            .show(ctx, |ui| {
                ui.heading("graph-view projection");
                ui.label("Lens over a clustered graph.");
                ui.separator();

                ui.label("Mode");
                let prev = self.state.projection.kind;
                for kind in [
                    ProjectionKind::Affine,
                    ProjectionKind::Fisheye,
                    ProjectionKind::Poincare,
                ] {
                    ui.radio_value(&mut self.state.projection.kind, kind, mode_label(kind));
                }
                if self.state.projection.kind != prev {
                    // Reframe when the lens turns on/off so the extent fits.
                    self.state.needs_fit = true;
                }
                ui.separator();

                if self.state.projection.kind != ProjectionKind::Affine {
                    ui.add(
                        egui::Slider::new(&mut self.state.projection.strength, 0.1..=3.0)
                            .text("strength"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.state.projection.size_falloff, 0.0..=1.0)
                            .text("size falloff"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.state.projection.geodesic_segments, 2..=64)
                            .text("edge segments"),
                    );
                    if self.state.projection.kind == ProjectionKind::Poincare {
                        ui.checkbox(&mut self.state.show_boundary, "Show disk boundary");
                        ui.label(
                            egui::RichText::new("drag to recenter · click a node to fly to it")
                                .small(),
                        );
                        if ui.button("Reset view").clicked() {
                            self.state.nav = Mobius::identity();
                            self.state.needs_fit = true;
                        }
                    }
                } else {
                    ui.label("Select Fisheye or Poincaré to warp.");
                }
                ui.separator();
                ui.label("Drag to pan, scroll to zoom.");
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.state
                .ui(ui, &self.graph, |_p, _r, _t, _b, _a| {});
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 680.0]),
        ..Default::default()
    };
    eframe::run_native(
        "graph-view projection demo",
        options,
        Box::new(|_cc| Ok(Box::<DemoApp>::default())),
    )
}
