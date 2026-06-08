//! Node + edge painting and pointer hit-testing on [`State`]: the per-frame
//! draw loop that renders each node at its LOD tier, the mode-aware edge
//! routing (straight / geodesic / fisheye-bulge), and the hover-preview
//! refresh. A pure `impl State` continuation of the parent module, split out
//! for file length.

use hiker_graph::LayoutKind;
use hiker_projection::ProjectionKind;

use super::{
    fade, EdgeMap, Lens, Lod, NodeDescriptor, NodeDraw, NodePaint, NodeShape, Source, State,
};

impl State {
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
    ) -> NodeDraw {
        let &NodePaint { lens, zoom, hovered, response_clicked } = paint;
        let node_scale = self.style.node_scale;
        let label_font = egui::FontId::proportional(self.style.label_size);
        let lens_active = lens.active();
        let mut draw = NodeDraw::default();
        for d in nodes {
            let p = to_screen(d.world_pos);
            let mag = lens.magnification(d.world_pos);
            // Center nodes grow, rim nodes shrink (1.0 under Affine).
            let r = d.radius * node_scale * zoom.max(0.4) * mag;
            let alpha = lens.rim_alpha(d.world_pos, self.fade_start, self.fade_strength);
            let fill = fade(d.fill, alpha);
            let is_hover = hovered == Some(d.index);
            let tier = self.lod_tier(mag, lens_active);
            match tier {
                Lod::Full => match d.shape {
                    NodeShape::Circle => {
                        let stroke = if is_hover { d.hover_stroke } else { d.resting_stroke };
                        painter.circle(p, r, fill, stroke);
                    }
                    NodeShape::Square => {
                        let rect = egui::Rect::from_center_size(p, egui::Vec2::splat(r * 2.0));
                        painter.rect_filled(rect, 1.0, fill);
                        if is_hover {
                            painter.rect_stroke(
                                rect.expand(2.0),
                                1.0,
                                d.hover_stroke,
                                egui::StrokeKind::Outside,
                            );
                        }
                    }
                },
                Lod::Dot => {
                    painter.circle_filled(p, r.min(3.0), fill);
                }
                Lod::Marker => {
                    painter.circle_filled(p, 1.5, fill);
                }
            }
            // Labels + hover affordances only on FULL nodes; dots/markers are
            // too small to anchor a label or a hover ring legibly.
            if tier == Lod::Full {
                if self.toggles.show_labels
                    && zoom >= d.label_min_zoom
                    && let Some(label) = &d.label
                {
                    painter.text(
                        egui::pos2(p.x, p.y + r + 2.0),
                        egui::Align2::CENTER_TOP,
                        label,
                        label_font.clone(),
                        self.style.label_color,
                    );
                }
                if is_hover {
                    draw.hover_anchor = Some(p);
                    if let Some(t) = &d.tooltip {
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
    ) {
        let &EdgeMap { to_screen, disk_to_screen } = map;
        let width = self.style.edge_width;
        let color = self.style.edge_color;
        let n = self.positions.len();
        // Layered layout produces poly-line routes (orthogonal between ranks) in
        // the same world space as `positions`. Draw them when not under a lens
        // (the lens path resamples geodesics/bulges from the endpoints instead).
        let routed = self.layout_kind == LayoutKind::Layered
            && !lens.active()
            && !self.edge_routes.is_empty();
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            let (wa, wb) = (self.positions[a], self.positions[b]);
            if routed {
                let stroke = egui::Stroke::new(width, color);
                match self.edge_routes.get(i) {
                    Some(route) if route.len() >= 2 => {
                        let pts: Vec<egui::Pos2> = route.iter().map(|&p| to_screen(p)).collect();
                        painter.add(egui::Shape::line(pts, stroke));
                    }
                    // No usable route for this edge — fall back to a straight
                    // segment between its endpoints.
                    _ => {
                        painter.line_segment([to_screen(wa), to_screen(wb)], stroke);
                    }
                }
                continue;
            }
            if !lens.active() {
                let seg = [to_screen(wa), to_screen(wb)];
                painter.line_segment(seg, egui::Stroke::new(width, color));
                continue;
            }
            let alpha = lens
                .rim_alpha(wa, self.fade_start, self.fade_strength)
                .min(lens.rim_alpha(wb, self.fade_start, self.fade_strength));
            let stroke = egui::Stroke::new(width, fade(color, alpha));
            if !self.geodesic_edges {
                let seg = [to_screen(wa), to_screen(wb)];
                painter.line_segment(seg, stroke);
                continue;
            }
            let pts: Vec<egui::Pos2> = match self.projection.kind {
                ProjectionKind::Poincare => {
                    let (za, zb) = (lens.disk(wa), lens.disk(wb));
                    hiker_projection::sample_geodesic(za, zb, self.projection.geodesic_segments)
                        .into_iter()
                        .map(disk_to_screen)
                        .collect()
                }
                // Fisheye (and any non-Affine fallback): subdivide the world
                // chord, lensing each sample so the edge tracks the distortion.
                _ => {
                    let steps = self.projection.geodesic_segments.max(1);
                    (0..=steps)
                        .map(|i| {
                            let t = i as f32 / steps as f32;
                            to_screen(wa + (wb - wa) * t)
                        })
                        .collect()
                }
            };
            painter.add(egui::Shape::line(pts, stroke));
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
