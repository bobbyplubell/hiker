//! Pane painting on [`State`]: the core `paint_pane` routine that fits a view,
//! runs input on the interactive pane, and draws background/edges/boundary/nodes,
//! plus the locked-Poincaré pane path. A pure `impl State` continuation of the
//! parent module, split out for file length.

use crate::force_graph::{View, ZoomBounds};
use graph_widgets::force_layout::LayoutWorker;
use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};
use hiker_theme as theme;

use super::gpu::{FlowParams, GpuBatch, GpuCacheKey, GraphPaintCallback, ViewXform};
use super::source::Source;
use super::{
    draw_tooltip, hit_test, poincare_disk, EdgeMap, FocusMode, HoverFlow, Lens, NodeDraw,
    NodePaint, PaneInputs, State, ZOOM_MAX, ZOOM_MIN,
};

impl State {

    /// Start a screen-space GPU batch for this pane (the lens/Poincaré path:
    /// positions are baked to final screen-points and rebuilt every frame). The
    /// `view_scale`/`view_offset` uniform is identity. Returns `(batch, reserved
    /// shape idx)`, or `None` when the GPU path is inactive (Painter path runs).
    fn gpu_batch_start(&self, painter: &egui::Painter) -> Option<(GpuBatch, egui::layers::ShapeIdx)> {
        self.gpu_active()
            .then(|| (GpuBatch::default(), painter.add(egui::Shape::Noop)))
    }

    /// Start a **world-space** GPU batch for the Affine path, reserving the
    /// bottom z-slot. Positions push in world space + base radii; the view
    /// transform (`view.zoom` + the pane translate) goes in the uniform so a
    /// pan/zoom needs no rebuild. Compares the geometry key against `slot`'s
    /// last build: a hit produces a `cached` batch (fills dropped — labels/hover
    /// still build) so `prepare` reuses the uploaded buffers. Returns the batch,
    /// the reserved idx, and the [`ViewXform`] to emit with.
    fn gpu_affine_batch_start(
        &mut self,
        painter: &egui::Painter,
        pane_rect: egui::Rect,
        view: View,
        slot: usize,
        content: u64,
    ) -> Option<(GpuBatch, egui::layers::ShapeIdx, ViewXform)> {
        if !self.gpu_active() {
            return None;
        }
        // `screen_mapper` is `center + (w + pan) * zoom == w*zoom + (center +
        // pan*zoom)`, i.e. an affine map with scale `zoom`, offset the second
        // term — exactly the shader's `pos * view_scale + view_offset`.
        let scale = view.zoom;
        let offset = pane_rect.center().to_vec2() + view.pan * view.zoom;
        let key = GpuCacheKey { layout_epoch: self.layout_epoch, content };
        // Cache hit when this pane's GPU buffers already hold this exact key —
        // i.e. a pure pan/zoom at an unchanged layout. Then only the uniform is
        // rewritten; the heavy instance/edge upload is skipped.
        let cached = self.gpu_last_key[slot] == Some(key);
        let flow = self.flow_params();
        let xform = ViewXform {
            scale,
            offset: [offset.x, offset.y],
            edge_width: self.style.edge_width,
            time: flow.time,
            flow: if flow.flow { 1.0 } else { 0.0 },
            flow_color: flow.color,
            flow_size: flow.size,
            flow_alpha: flow.alpha,
            flow_speed: flow.speed,
            flow_density: flow.density,
            cache_key: Some(key),
        };
        Some((GpuBatch::world(cached), painter.add(egui::Shape::Noop), xform))
    }

    /// Pick the right GPU batch for an Affine-regime pane: the world-space
    /// cacheable path when the lens is inactive (pure Affine), or the
    /// screen-space rebuild-every-frame path when it's a Fisheye bulge (the lens
    /// moves every node). Returns `None` when the GPU path is off (Painter runs).
    fn gpu_start_affine_regime(
        &mut self,
        clipped: &egui::Painter,
        pane_rect: egui::Rect,
        view: View,
        lens_active: bool,
        slot: usize,
        source: &dyn Source,
    ) -> Option<(GpuBatch, egui::layers::ShapeIdx, ViewXform)> {
        if lens_active {
            // Fisheye: positions are final screen-points (lens-warped), rebuilt
            // each frame — no caching possible.
            let xform = ViewXform::screen(self.style.edge_width, self.flow_params());
            self.gpu_batch_start(clipped).map(|(b, i)| (b, i, xform))
        } else {
            let content = self.gpu_content_key(source);
            self.gpu_affine_batch_start(clipped, pane_rect, view, slot, content)
        }
    }

    /// Split the optional `(batch, idx, xform)` a `gpu_start_*` returns into the
    /// `(batch, idx)` pair the draw loop fills and the [`ViewXform`] the emit
    /// needs — with an identity screen transform when the GPU path is off (the
    /// xform is then unused, since `gpu_batch_emit` early-returns on `None`).
    fn split_started(
        started: Option<(GpuBatch, egui::layers::ShapeIdx, ViewXform)>,
        edge_width: f32,
    ) -> (Option<(GpuBatch, egui::layers::ShapeIdx)>, ViewXform) {
        match started {
            Some((b, i, x)) => (Some((b, i)), x),
            // The GPU path is off here, so this xform is never used (the emit
            // early-returns on the `None` batch). Flow is inert under the Painter
            // fallback, so default (disabled) flow params suffice.
            None => (None, ViewXform::screen(edge_width, FlowParams::default())),
        }
    }

    /// Fill the reserved bottom slot with the finished batch as one egui-wgpu
    /// paint callback clipped to `pane_rect`, using `slot`'s stable buffer id.
    /// `xform` carries the view transform + cache key ([`ViewXform::screen`] for
    /// the lens path). An empty, non-cached batch leaves the reserved `Noop`.
    fn gpu_batch_emit(
        &mut self,
        painter: &egui::Painter,
        pane_rect: egui::Rect,
        slot: usize,
        batch: Option<(GpuBatch, egui::layers::ShapeIdx)>,
        xform: ViewXform,
    ) {
        let Some((batch, idx)) = batch else { return };
        // A cache hit emits with empty geometry but MUST still issue the callback
        // (to refresh the uniform + reissue the draws against the cached buffers).
        if !batch.cached && batch.nodes.is_empty() && batch.edges.is_empty() {
            return;
        }
        // Record what the GPU now holds so the next frame can detect a cache hit.
        if let Some(key) = xform.cache_key {
            self.gpu_last_key[slot] = Some(key);
        }
        let id = self.gpu_pane_id(slot);
        painter.set(
            idx,
            egui_wgpu::Callback::new_paint_callback(
                pane_rect,
                GraphPaintCallback::new(id, pane_rect, xform, batch.nodes, batch.edges),
            ),
        );
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
    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_pane<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
        slot: usize,
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
            self.paint_pane_poincare(ui, &clipped, pane_rect, cfg, response, inputs, slot)
        } else {
            self.paint_pane_affine(ui, &clipped, pane_rect, cfg, response, inputs, slot)
        }
    }

    /// Paint the Affine/Fisheye regime: a free, pannable + zoomable affine view
    /// with the lens (identity under Affine, a bulge under Fisheye) composed
    /// into the world→screen map. Unchanged from the historical path.
    #[allow(clippy::too_many_arguments)]
    fn paint_pane_affine<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        clipped: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
        slot: usize,
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
            let started =
                self.gpu_start_affine_regime(clipped, pane_rect, view, lens.active(), slot, source);
            let (mut batch, xform) = Self::split_started(started, self.style.edge_width);
            if self.toggles.show_edges {
                self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens, batch.as_mut().map(|(b, _)| b));
            }
            self.draw_nodes(
                clipped,
                nodes,
                &to_screen,
                &NodePaint { lens: &lens, zoom: view.zoom, label_zoom: view.zoom, hovered: None, response_clicked: false, label_dim: None },
                batch.as_mut().map(|(b, _)| b),
            );
            self.gpu_batch_emit(clipped, pane_rect, slot, batch, xform);
            return None;
        }

        // Hand control to the user: the settle-time auto-fit re-frames every frame
        // while the layout worker runs, which otherwise yanks the view back as soon
        // as you scroll/pinch/drag to zoom in mid-settle. The first such gesture
        // cancels the auto-fit so your zoom sticks.
        if let Some(response) = response {
            let zoom_gesture = response.hovered()
                && ui.input(|i| i.smooth_scroll_delta.y != 0.0 || i.zoom_delta() != 1.0);
            if zoom_gesture || response.dragged() {
                self.needs_fit = false;
            }
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
        // Right-click a node → surface its click_path + index for the host's
        // context menu (e.g. the code view opens the node's source file).
        if response.is_some_and(egui::Response::secondary_clicked)
            && let Some(idx) = hovered
        {
            self.secondary_click =
                nodes.iter().find(|d| d.index == idx).and_then(|d| d.click_path.clone());
            self.secondary_click_node = Some(idx);
        }

        let started =
            self.gpu_start_affine_regime(clipped, pane_rect, self.view, lens.active(), slot, source);
        let (mut batch, xform) = Self::split_started(started, self.style.edge_width);
        if self.toggles.show_edges {
            self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens, batch.as_mut().map(|(b, _)| b));
        }
        // Reserve the highlight overlay's slot AFTER the base edges (and the GPU
        // callback's slot) but BEFORE the nodes/labels paint: the glow renders
        // above the edges it traces yet under node shapes and labels. (On the GPU
        // path node FILLS share the edge callback below; only the translucent
        // glow washes over them — labels and the hover ring stay on top.)
        // status: graph-hover-highlight
        let highlight_slot = clipped.add(egui::Shape::Noop);
        self.paint_highlight_overlay(
            ui,
            clipped,
            source,
            &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen },
            &lens,
            hovered,
            highlight_slot,
        );

        // Selection label-dimming factors for this frame (None = no dimming).
        // status: graph-label-dim
        let label_dim = self.label_dim_factors(source);
        let clicked_this_frame = response.is_some_and(egui::Response::clicked);
        let draw = self.draw_nodes(
            clipped,
            nodes,
            &to_screen,
            &NodePaint { lens: &lens, zoom, label_zoom: zoom, hovered, response_clicked: clicked_this_frame, label_dim: label_dim.as_deref() },
            batch.as_mut().map(|(b, _)| b),
        );
        self.gpu_batch_emit(clipped, pane_rect, slot, batch, xform);
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
    #[allow(clippy::too_many_arguments)]
    fn paint_pane_poincare<F: Fn(&egui::Painter, egui::Rect, &str, &str, egui::Pos2)>(
        &mut self,
        ui: &mut egui::Ui,
        clipped: &egui::Painter,
        pane_rect: egui::Rect,
        cfg: ProjectionConfig,
        response: Option<&egui::Response>,
        inputs: &PaneInputs<'_, F>,
        slot: usize,
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
            let mut batch = self.gpu_batch_start(clipped);
            if self.toggles.show_edges {
                self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens, batch.as_mut().map(|(b, _)| b));
            }
            if self.show_boundary {
                self.stroke_disk_boundary(clipped, disk_center, disk_radius);
            }
            self.draw_nodes(
                clipped,
                nodes,
                &to_screen,
                &NodePaint { lens: &lens, zoom: 1.0, label_zoom: 1.0, hovered: None, response_clicked: false, label_dim: None },
                batch.as_mut().map(|(b, _)| b),
            );
            self.gpu_batch_emit(clipped, pane_rect, slot, batch, ViewXform::screen(self.style.edge_width, self.flow_params()));
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
                self.poincare_zoom = hiker_projection_view::zoom_poincare(self.poincare_zoom, scroll);
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
        // Right-click a node → surface its click_path + index for the host's context menu.
        if response.is_some_and(egui::Response::secondary_clicked)
            && let Some(idx) = hovered
        {
            self.secondary_click =
                nodes.iter().find(|d| d.index == idx).and_then(|d| d.click_path.clone());
            self.secondary_click_node = Some(idx);
        }

        let mut batch = self.gpu_batch_start(clipped);
        if self.toggles.show_edges {
            self.draw_edges(clipped, source, &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen }, &lens, batch.as_mut().map(|(b, _)| b));
        }
        // Above the base edges, below nodes/labels — see the affine regime.
        // status: graph-hover-highlight
        let highlight_slot = clipped.add(egui::Shape::Noop);
        self.paint_highlight_overlay(
            ui,
            clipped,
            source,
            &EdgeMap { to_screen: &to_screen, disk_to_screen: &disk_to_screen },
            &lens,
            hovered,
            highlight_slot,
        );
        if self.show_boundary {
            self.stroke_disk_boundary(clipped, disk_center, disk_radius);
        }

        // Selection label-dimming factors for this frame (None = no dimming).
        // status: graph-label-dim
        let label_dim = self.label_dim_factors(source);
        let clicked_this_frame = response.is_some_and(egui::Response::clicked);
        let draw = self.draw_nodes(
            clipped,
            nodes,
            &to_screen,
            &NodePaint { lens: &lens, zoom: 1.0, label_zoom: self.poincare_zoom, hovered, response_clicked: clicked_this_frame, label_dim: label_dim.as_deref() },
            batch.as_mut().map(|(b, _)| b),
        );
        self.gpu_batch_emit(clipped, pane_rect, slot, batch, ViewXform::screen(self.style.edge_width, self.flow_params()));
        self.finish_pane(clipped, source, pane_rect, &draw, hovered, draw_preview);
        draw.clicked
    }

    /// Stroke the locked Poincaré disk boundary ring at the pane-fixed frame.
    fn stroke_disk_boundary(&self, painter: &egui::Painter, center: egui::Pos2, radius: f32) {
        painter.circle_stroke(center, radius, egui::Stroke::new(1.0, theme::divider()));
    }

    /// Advance the hover animation state and fill the pane's reserved bottom-most
    /// highlight slot: the selected node's steady glow, the hovered node's faded
    /// glow, the hover-flow cross-fade + travelling pulse when the hover moved
    /// between two nodes — or, when `highlight.fluid`, the fluid energy field's
    /// gradient strokes + node halos instead of the discrete flow.
    /// status: graph-hover-flow
    #[allow(clippy::too_many_arguments)]
    fn paint_highlight_overlay(
        &mut self,
        ui: &egui::Ui,
        painter: &egui::Painter,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        hovered: Option<usize>,
        slot: egui::layers::ShapeIdx,
    ) {
        // Keyframes: a hover MOVE (A → B) starts a flow transition; first hover or
        // re-hovering the same node doesn't. The hovered node is retained so its
        // glow can fade out after the pointer leaves.
        if let Some(new) = hovered {
            if let Some(prev) = self.hover_anim_node
                && prev != new
            {
                self.hover_flow =
                    Some(HoverFlow { from: prev, to: new, start: ui.input(|i| i.time) });
            }
            self.hover_anim_node = hovered;
        }
        let hover_glow = ui.ctx().animate_bool_with_time(
            ui.id().with("graphview_hover_glow"),
            hovered.is_some(),
            self.highlight.fade_secs,
        );
        if !self.toggles.show_edges {
            return;
        }
        let mut shapes = Vec::new();
        if self.highlight.selected_edges {
            self.highlight_edge_shapes(
                &mut shapes,
                source,
                map,
                lens,
                self.selected_node,
                self.highlight.opacity,
            );
        }
        if self.highlight.hover_edges {
            if self.highlight.fluid {
                self.fluid_advance_and_shapes(ui, &mut shapes, source, map, lens, hovered);
            } else {
                let alpha = hover_glow * self.highlight.opacity;
                // An in-flight flow renders both keyframe nodes cross-faded plus the
                // travelling pulse; otherwise the steady single-node glow.
                let flow = self.hover_flow.and_then(|f| {
                    let t = ((ui.input(|i| i.time) - f.start) as f32
                        / self.highlight.flow_secs.max(0.01))
                    .clamp(0.0, 1.0);
                    (t < 1.0 && self.hover_anim_node == Some(f.to))
                        .then_some((f, super::smoothstep(t)))
                });
                match flow {
                    Some((f, t)) => {
                        self.highlight_edge_shapes(&mut shapes, source, map, lens, Some(f.from), alpha * (1.0 - t));
                        self.highlight_edge_shapes(&mut shapes, source, map, lens, Some(f.to), alpha * t);
                        self.hover_flow_shapes(&mut shapes, source, map, lens, f.from, f.to, t, alpha);
                        ui.ctx().request_repaint();
                    }
                    None => {
                        self.hover_flow = None;
                        self.highlight_edge_shapes(&mut shapes, source, map, lens, self.hover_anim_node, alpha);
                    }
                }
            }
        }
        if !shapes.is_empty() {
            painter.set(slot, egui::Shape::Vec(shapes));
        }
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
