//! Shared pan/zoom + input plumbing for force-directed graph panels.
//!
//! The vault link-graph (`panels/graph.rs`) and cluster-tree graph
//! (`panels/cluster_graph.rs`) both render a Fruchterman–Reingold layout
//! on a hand-rolled `Painter` canvas, with identical pan-by-drag,
//! scroll-to-zoom-anchored-on-cursor, and world→screen mapping. This
//! widget hosts that shared input + transform layer.
//!
//! Why not unify the FR step too? The two panels store node positions
//! in materially different shapes: the vault graph uses
//! `petgraph::DiGraph<NodeData, ()>` with `pos` embedded in each node,
//! while the cluster panel keeps an external `HashMap<String, Vec2>`.
//! Sharing the math would force one panel to copy positions in/out
//! every frame — not worth the loss in clarity. Node drawing also
//! stays per-panel (different colouring, sizing, hover semantics).

use eframe::egui;

/// Persistent pan/zoom state. Both consumers store an instance per
/// canvas (the vault graph holds it on `State`; the cluster graph
/// stores it in egui memory keyed by tree id).
#[derive(Clone, Copy)]
pub struct View {
    pub pan: egui::Vec2,
    pub zoom: f32,
}

impl Default for View {
    fn default() -> Self {
        Self {
            pan: egui::Vec2::ZERO,
            zoom: 1.0,
        }
    }
}

/// Min/max zoom bounds. The two callers had slightly different ranges
/// (0.05..5.0 vs 0.1..6.0) — the widget takes them as params so we
/// don't lose either behaviour.
#[derive(Clone, Copy)]
pub struct ZoomBounds {
    pub min: f32,
    pub max: f32,
}

impl View {
    /// Apply drag-to-pan + scroll-to-zoom (anchored on the cursor when
    /// possible). Mirrors the inline blocks the two panels had before
    /// extraction.
    pub fn handle_input(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        zoom_bounds: ZoomBounds,
    ) {
        if response.dragged_by(egui::PointerButton::Primary) {
            self.pan += response.drag_delta() / self.zoom;
        }
        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let factor = (scroll * 0.005).exp();
                let new_zoom = (self.zoom * factor).clamp(zoom_bounds.min, zoom_bounds.max);
                if let Some(hover) = response.hover_pos() {
                    // Keep the world-point under the cursor pinned across
                    // the zoom change.
                    let center = rect.center();
                    let world_under = (hover - center) / self.zoom - self.pan;
                    self.zoom = new_zoom;
                    self.pan = (hover - center) / self.zoom - world_under;
                } else {
                    self.zoom = new_zoom;
                }
            }
        }
    }

    /// World→screen point. Returns a closure so callers can capture it
    /// in tight paint loops without re-deriving `rect.center()` each
    /// iteration. Captures pan/zoom by value so it doesn't keep a
    /// borrow on the view (callers often move the view into egui
    /// memory after painting).
    pub fn screen_mapper(self, rect: egui::Rect) -> impl Fn(egui::Vec2) -> egui::Pos2 {
        let center = rect.center();
        let pan = self.pan;
        let zoom = self.zoom;
        move |w: egui::Vec2| center + (w + pan) * zoom
    }

    /// Set pan/zoom so the bounding box of `positions` fits inside
    /// `canvas` with a margin. Used by both graph panels for "fit to
    /// content" on rebuild and on the Reset view button — necessary
    /// because the force layout's natural scale can vary from ~100 to
    /// ~10,000 world units depending on graph size, so a fixed default
    /// zoom always loses for some vault sizes.
    pub fn fit_to_positions(
        &mut self,
        positions: &[egui::Vec2],
        canvas: egui::Rect,
        zoom_range: (f32, f32),
    ) {
        if positions.is_empty() {
            return;
        }
        let mut lo = positions[0];
        let mut hi = positions[0];
        for &p in positions.iter().skip(1) {
            lo.x = lo.x.min(p.x);
            lo.y = lo.y.min(p.y);
            hi.x = hi.x.max(p.x);
            hi.y = hi.y.max(p.y);
        }
        let span_x = (hi.x - lo.x).max(1.0);
        let span_y = (hi.y - lo.y).max(1.0);
        let margin = 40.0;
        let avail_w = (canvas.width() - margin * 2.0).max(50.0);
        let avail_h = (canvas.height() - margin * 2.0).max(50.0);
        let zoom = (avail_w / span_x).min(avail_h / span_y).clamp(zoom_range.0, zoom_range.1);
        let centre = (lo + hi) * 0.5;
        self.pan = -centre;
        self.zoom = zoom;
    }
}
