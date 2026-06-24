//! Pan/zoom + input primitive for the graph panels: drag-to-pan,
//! scroll-to-zoom-anchored-on-cursor, world→screen mapping, and
//! fit-to-content framing.
//!
//! The full rendering engine that builds on this — layout, node/edge/label
//! drawing, the view-options menu, hover preview — lives in
//! `widgets::graph_view`, which both the vault link-graph
//! (`panels/graph.rs`) and the cluster-tree graph (`panels/cluster_graph.rs`)
//! drive through a `graph_view::Source` adapter. This module stays the
//! lowest layer: the pan/zoom transform a `graph_view::State` embeds.


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
    ///
    /// `allow_pan` gates only the drag-to-pan block; scroll-to-zoom always
    /// runs. The Poincaré view turns affine pan off (`allow_pan = false`) so
    /// primary-drag drives a Möbius recentre instead, while scroll still zooms
    /// the disk.
    pub fn handle_input(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        zoom_bounds: ZoomBounds,
        allow_pan: bool,
    ) {
        if allow_pan && response.dragged_by(egui::PointerButton::Primary) {
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

    /// Invert the affine `screen_mapper`: recover the lensed-world point that
    /// maps to `screen`. The exact inverse of `center + (w + pan) * zoom`, used
    /// by the Poincaré Möbius recentre to read drag endpoints back into disk
    /// space.
    pub fn screen_to_affine(self, rect: egui::Rect, screen: egui::Pos2) -> egui::Vec2 {
        (screen - rect.center()) / self.zoom - self.pan
    }

    /// Set pan/zoom so the bounding box of `positions` fits inside
    /// `canvas` with a margin. Used by both graph panels for "fit to
    /// content" on rebuild and on the Reset view button — necessary
    /// because the force layout's natural scale can vary from ~100 to
    /// ~10,000 world units depending on graph size, so a fixed default
    /// zoom always loses for some vault sizes.
    ///
    /// Returns the **unclamped** ideal fit zoom (`min(avail_w/span_x,
    /// avail_h/span_y)`), guarded to a tiny positive floor — the TRUE
    /// fitted-overview scale even when the actual `self.zoom` is clamped to
    /// `zoom_range`. The code-graph view records this as its
    /// `last_fit_zoom` so its LOD gate can be expressed as a ratio over the
    /// fitted overview (`view.zoom / last_fit_zoom`), making the gate
    /// independent of the graph's world extent. status: code-graph-bundling
    pub fn fit_to_positions(
        &mut self,
        positions: &[egui::Vec2],
        canvas: egui::Rect,
        zoom_range: (f32, f32),
    ) -> f32 {
        if positions.is_empty() {
            return 1.0;
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
        // The ideal (pre-clamp) fit — the genuine fitted-overview scale. A huge
        // graph's ideal can fall below `zoom_range.0`; we still return the ideal
        // so the LOD gate's ratio stays extent-independent (floored away from
        // 0/NaN).
        let ideal = (avail_w / span_x).min(avail_h / span_y).max(1e-6);
        let zoom = ideal.clamp(zoom_range.0, zoom_range.1);
        let centre = (lo + hi) * 0.5;
        self.pan = -centre;
        self.zoom = zoom;
        ideal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fit_to_positions` returns the UNCLAMPED ideal fit even when the actual `self.zoom` is floored
    /// at the range minimum — the true fitted-overview scale the LOD gate's ratio needs. status: code-graph-bundling
    #[test]
    fn fit_returns_unclamped_ideal_when_zoom_clamped() {
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 1000.0));
        // A very wide layout: span ~1e6, so the ideal fit (~9.2e-4) sits BELOW the 0.005 floor.
        let positions = [egui::vec2(-500_000.0, -500_000.0), egui::vec2(500_000.0, 500_000.0)];
        let mut v = View::default();
        let ideal = v.fit_to_positions(&positions, canvas, (0.005, 6.0));
        // The written zoom is clamped to the floor, but the returned ideal is the genuine (smaller)
        // fit — so a ratio gate (view.zoom / ideal) stays extent-independent.
        assert!((v.zoom - 0.005).abs() < 1e-9, "actual zoom floored to the range minimum");
        assert!(ideal < 0.005, "returned ideal {ideal} must be the unclamped (smaller) fit");
        assert!(ideal > 0.0, "ideal must be positive");
    }

    /// When the ideal fit is comfortably within the range, the returned ideal EQUALS the written
    /// zoom — so a small graph's overview reads at ratio exactly 1.0.
    #[test]
    fn fit_returns_equal_when_unclamped() {
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 1000.0));
        let positions = [egui::vec2(-100.0, -100.0), egui::vec2(100.0, 100.0)];
        let mut v = View::default();
        let ideal = v.fit_to_positions(&positions, canvas, (0.005, 6.0));
        assert!((ideal - v.zoom).abs() < 1e-6, "unclamped fit: ideal == written zoom");
    }
}
