//! Pane layout + painting on [`State`]: the corner-inset geometry, the
//! Poincaré overview projection config, the inset frame stroke, and the core
//! `paint_pane` routine that fits a view, runs input on the interactive pane,
//! and draws background/edges/boundary/nodes. A pure `impl State` continuation
//! of the parent module, split out for file length.

use crate::force_graph::{View, ZoomBounds};
use graph_widgets::force_layout::LayoutWorker;
use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};
use hiker_theme as theme;

use super::{
    draw_tooltip, hit_test, poincare_disk, Corner, EdgeMap, FocusMode, Lens, MinimapShape, NodeDraw,
    NodePaint, PaneInputs, Source, State, ZOOM_MAX, ZOOM_MIN,
};

impl State {
    /// The corner inset rect, honouring `minimap_corner` + `minimap_size`. This
    /// is the small "slot" the overview occupies today and the Euclidean view
    /// demotes into when expanded.
    pub(super) fn corner_rect(&self, rect: egui::Rect) -> egui::Rect {
        const MARGIN: f32 = 8.0;
        let frac = self.minimap_size.clamp(0.12, 0.5);
        let side = rect.width().min(rect.height()) * frac;
        let (min_x, min_y) = match self.minimap_corner {
            Corner::TopLeft => (rect.left() + MARGIN, rect.top() + MARGIN),
            Corner::TopRight => (rect.right() - MARGIN - side, rect.top() + MARGIN),
            Corner::BottomLeft => (rect.left() + MARGIN, rect.bottom() - MARGIN - side),
            Corner::BottomRight => {
                (rect.right() - MARGIN - side, rect.bottom() - MARGIN - side)
            }
        };
        egui::Rect::from_min_size(egui::pos2(min_x, min_y), egui::Vec2::splat(side))
    }

    /// The Poincaré overview projection config: a disk centred on the centroid,
    /// independent of the main pane's mode. Inherits the user's strength when
    /// they're already in Poincaré, else a neutral 1.0.
    pub(super) fn overview_cfg(&self) -> ProjectionConfig {
        let strength = if self.projection.kind == ProjectionKind::Poincare {
            self.projection.strength
        } else {
            1.0
        };
        ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength,
            size_falloff: 1.0,
            geodesic_segments: 16,
        }
    }

    /// Stroke the inset frame for a corner-slot pane (the disk ring or a square),
    /// plus the subtle inset panel behind it. Drawn between the full-slot pane
    /// and the corner-slot pane so it reads as a framed inset.
    pub(super) fn frame_corner(&self, painter: &egui::Painter, slot_rect: egui::Rect) {
        painter.rect_filled(
            slot_rect,
            4.0,
            egui::Color32::from_rgba_unmultiplied(0x10, 0x10, 0x14, 0xc8),
        );
        let stroke = egui::Stroke::new(1.0, theme::divider());
        match self.minimap_shape {
            MinimapShape::Circle => {
                let r = slot_rect.width().min(slot_rect.height()) * 0.5;
                painter.circle_stroke(slot_rect.center(), r, stroke);
            }
            MinimapShape::Square => {
                painter.rect_stroke(slot_rect, 4.0, stroke, egui::StrokeKind::Inside);
            }
        }
    }

    /// Paint + (optionally) interact one pane: build the lens for `cfg`, fit a
    /// view to `pane_rect`, then draw bg/edges/boundary/nodes within a clip to
    /// `pane_rect`. Returns the clicked node path (only meaningful when
    /// `interactive`).
    ///
    /// Interactivity gating is the delicate part: only the interactive pane
    /// touches `self.view`/`self.nav`/`self.flyto` and runs input/needs_fit. A
    /// read-only pane uses a *fresh* local `View` fitted to its lensed extent and
    /// an identity nav (a centred overview), so it never disturbs the persistent
    /// interactive view.
    pub(super) fn paint_pane<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
    ) -> Option<String> {
        let clipped = painter.with_clip_rect(pane_rect);
        // The interactive pane is the one wired to a `Response`; read-only panes
        // (the corner overview / mid-flight content) get `None`.
        if response.is_some() {
            let bg = self.style.background.unwrap_or(ui.visuals().extreme_bg_color);
            clipped.rect_filled(pane_rect, 0.0, bg);
        } else if self.positions.is_empty() {
            return None;
        }

        // Poincaré locks the disk to the pane (the disk IS the viewport); every
        // other mode runs the free affine pan/zoom view.
        if cfg.kind == ProjectionKind::Poincare {
            self.paint_pane_poincare(ui, &clipped, pane_rect, cfg, response, inputs)
        } else {
            self.paint_pane_affine(ui, &clipped, pane_rect, cfg, response, inputs)
        }
    }

    /// Paint the Affine/Fisheye regime: a free, pannable + zoomable affine view
    /// with the lens (identity under Affine, a bulge under Fisheye) composed
    /// into the world→screen map. Unchanged from the historical path.
    fn paint_pane_affine<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        clipped: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
    ) -> Option<String> {
        let PaneInputs { source, nodes, draw_preview } = *inputs;
        if response.is_none() {
            // Read-only pane: a centred overview with a fresh local view; never
            // touch `self.view`/`self.nav`/`self.flyto`.
            let lens = Lens::centred(cfg, Mobius::identity(), &self.positions);
            let lensed: Vec<egui::Vec2> =
                self.positions.iter().map(|&p| lens.world_to_lensed(p)).collect();
            let mut view = View::default();
            // Generous bounds so small/large graphs both frame well in any slot.
            view.fit_to_positions(&lensed, pane_rect, (0.001, 50.0));
            let affine = view.screen_mapper(pane_rect);
            let to_screen = |w: egui::Vec2| affine(lens.world_to_lensed(w));
            let disk_to_screen = |z: Complex| affine(lens.disk_to_world(z));
            if self.toggles.show_edges {
                self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens);
            }
            self.draw_nodes(
                clipped,
                nodes,
                &to_screen,
                &NodePaint { lens: &lens, zoom: view.zoom, hovered: None, response_clicked: false },
            );
            return None;
        }

        // Fit always frames the centred extent so a moving focus never
        // re-fits the view; the focus-mode warp pans within that frame.
        let lens = Lens::centred(cfg, self.nav, &self.positions);
        if self.needs_fit && !self.positions.is_empty() {
            if lens.active() {
                let lensed: Vec<egui::Vec2> =
                    self.positions.iter().map(|&p| lens.world_to_lensed(p)).collect();
                self.view.fit_to_positions(&lensed, pane_rect, (ZOOM_MIN, ZOOM_MAX));
            } else {
                self.view.fit_to_positions(&self.positions, pane_rect, (ZOOM_MIN, ZOOM_MAX));
            }
            let worker_running = self.worker.as_ref().is_some_and(LayoutWorker::is_running);
            if !worker_running {
                self.needs_fit = false;
            }
        }

        if let Some(response) = response {
            self.view.handle_input(
                ui,
                response,
                pane_rect,
                ZoomBounds { min: ZOOM_MIN, max: ZOOM_MAX },
                true,
            );
        }
        // Resolve the focus per focus-mode (after input so Cursor reads the
        // freshly-zoomed view). Lens scale stays the centroid extent.
        let focus = self.interactive_focus(pane_rect, response);
        let lens = Lens::new(cfg, self.nav, focus, &self.positions);
        let affine = self.view.screen_mapper(pane_rect);
        let to_screen = |w: egui::Vec2| affine(lens.world_to_lensed(w));
        let disk_to_screen = |z: Complex| affine(lens.disk_to_world(z));
        let zoom = self.view.zoom;
        let node_scale = self.style.node_scale;

        let hovered = response
            .and_then(egui::Response::hover_pos)
            .and_then(|hp| hit_test(nodes, &to_screen, &lens, hp, node_scale, zoom));

        // A click in Selection focus sets the focus node (the lens recentres on
        // it). Fly-to is Poincaré-only, so there's nothing else to do here.
        if let Some(response) = response
            && response.clicked()
            && let Some(idx) = hovered
            && self.focus_mode == FocusMode::Selection
        {
            self.focus_node = Some(idx);
        }

        if self.toggles.show_edges {
            self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens);
        }

        let clicked_this_frame = response.is_some_and(egui::Response::clicked);
        let draw = self.draw_nodes(
            clipped,
            nodes,
            &to_screen,
            &NodePaint { lens: &lens, zoom, hovered, response_clicked: clicked_this_frame },
        );
        self.finish_pane(clipped, source, pane_rect, &draw, hovered, draw_preview);
        draw.clicked
    }

    /// Paint the Poincaré regime: the disk is locked-CENTERED to the pane (centre
    /// = pane centre, radius = `DISK_FILL` of the shorter half-dimension ×
    /// `poincare_zoom`) and is INDEPENDENT of `self.view` pan/zoom — so the whole
    /// graph stays pressed into a centred disk. Scroll-zoom scales the disk RADIUS
    /// (centre fixed, no drift): zoom in → larger disk (content bigger, may clip
    /// the pane), zoom out → smaller, `1.0` = fit. The remaining navigation is
    /// Möbius drag + click fly-to. The read-only overview pane stays at zoom 1.0
    /// (fit) and never reads scroll.
    fn paint_pane_poincare<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        clipped: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
    ) -> Option<String> {
        let PaneInputs { source, nodes, draw_preview } = *inputs;

        if response.is_none() {
            // Read-only pane: fit (zoom 1.0); never read scroll or touch
            // `self.poincare_zoom`/`self.view`/`self.nav`/`self.flyto`.
            let (disk_center, disk_radius) = poincare_disk(pane_rect, 1.0);
            let disk_to_screen = |z: Complex| disk_center + egui::vec2(z.re, z.im) * disk_radius;
            // A centred overview with an identity nav.
            let lens = Lens::centred(cfg, Mobius::identity(), &self.positions);
            let to_screen = |w: egui::Vec2| disk_to_screen(lens.disk(w));
            if self.toggles.show_edges {
                self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens);
            }
            if self.show_boundary {
                self.stroke_disk_boundary(clipped, disk_center, disk_radius);
            }
            self.draw_nodes(
                clipped,
                nodes,
                &to_screen,
                &NodePaint { lens: &lens, zoom: 1.0, hovered: None, response_clicked: false },
            );
            return None;
        }

        // The disk is fit-to-pane by construction, so there's no `view` to
        // fit; a (re)build / Reset (callers set `needs_fit`) still recentres by
        // dropping accumulated navigation, cancelling a fly-to, and restoring the
        // fit zoom.
        if self.needs_fit {
            self.nav = Mobius::identity();
            self.flyto = None;
            self.poincare_zoom = 1.0;
            if !self.positions.is_empty() {
                self.needs_fit = false;
            }
        }

        // Scroll-zoom scales the disk RADIUS (centre stays the pane centre, so the
        // disk grows/shrinks centred and never drifts). Read only when hovered;
        // exponential so each notch is a constant ratio.
        if let Some(response) = response {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if response.hovered() && scroll != 0.0 {
                self.poincare_zoom = (self.poincare_zoom * (scroll * 0.005).exp())
                    .clamp(super::POINCARE_ZOOM_MIN, super::POINCARE_ZOOM_MAX);
                ui.ctx().request_repaint();
            }
        }

        // The locked disk frame for this frame's zoom — centre = pane centre,
        // radius scaled by `poincare_zoom`. Möbius drag, hit-test, and the
        // disk→screen map all derive from these.
        let (disk_center, disk_radius) = poincare_disk(pane_rect, self.poincare_zoom);
        let disk_to_screen = |z: Complex| disk_center + egui::vec2(z.re, z.im) * disk_radius;

        let focus = self.interactive_focus(pane_rect, response);
        if let Some(response) = response {
            self.handle_mobius_pan(response, disk_center, disk_radius);
            self.advance_flyto(ui);
        }
        // Build the lens after navigation so this frame's draw + hit-test see
        // the freshly-navigated `nav`.
        let lens = Lens::new(cfg, self.nav, focus, &self.positions);
        let to_screen = |w: egui::Vec2| disk_to_screen(lens.disk(w));
        let node_scale = self.style.node_scale;

        let hovered = response
            .and_then(egui::Response::hover_pos)
            .and_then(|hp| hit_test(nodes, &to_screen, &lens, hp, node_scale, 1.0));

        // Node click: Selection focus recentres the lens on the clicked node;
        // otherwise fly-to glides it to the fixed disk centre (when enabled).
        if let Some(response) = response
            && response.clicked()
            && let Some(idx) = hovered
        {
            if self.focus_mode == FocusMode::Selection {
                self.focus_node = Some(idx);
            } else if self.flyto_enabled
                && let Some(&w_n) = self.positions.get(idx)
            {
                self.start_flyto(w_n, &lens);
            }
        }

        if self.toggles.show_edges {
            self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens);
        }
        if self.show_boundary {
            self.stroke_disk_boundary(clipped, disk_center, disk_radius);
        }

        let clicked_this_frame = response.is_some_and(egui::Response::clicked);
        let draw = self.draw_nodes(
            clipped,
            nodes,
            &to_screen,
            &NodePaint { lens: &lens, zoom: 1.0, hovered, response_clicked: clicked_this_frame },
        );
        self.finish_pane(clipped, source, pane_rect, &draw, hovered, draw_preview);
        draw.clicked
    }

    /// Stroke the locked Poincaré disk boundary ring at the pane-fixed frame.
    fn stroke_disk_boundary(&self, painter: &egui::Painter, center: egui::Pos2, radius: f32) {
        painter.circle_stroke(center, radius, egui::Stroke::new(1.0, theme::divider()));
    }

    /// Shared tooltip + hover-preview tail for the interactive panes.
    fn finish_pane<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        clipped: &egui::Painter,
        source: &dyn Source,
        pane_rect: egui::Rect,
        draw: &NodeDraw,
        hovered: Option<usize>,
        draw_preview: &F,
    ) {
        if let Some((pos, text)) = &draw.tooltip {
            draw_tooltip(clipped, *pos, text.clone());
        }
        if self.toggles.show_preview
            && let Some(idx) = hovered
        {
            self.refresh_preview(source, idx);
            if let (Some(anchor), Some(title)) =
                (draw.hover_anchor, self.preview.title.as_deref())
            {
                let body = self.preview.body.as_deref().unwrap_or("(unable to read note)");
                draw_preview(clipped, pane_rect, title, body, anchor);
            }
        } else {
            self.preview.hovered_index = None;
        }
    }
}
