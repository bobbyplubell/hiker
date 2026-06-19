//! Node + edge painting on [`State`]: the per-frame draw loop that renders
//! each node at its LOD tier, the mode-aware edge routing (straight /
//! geodesic / fisheye-bulge), the hover/selection highlight overlays, and the
//! hover-preview refresh. A pure `impl State` continuation of the parent
//! module, split out for file length; the free painting primitives it draws
//! through live in [`edge_paint`](super::edge_paint).

use hiker_graph::LayoutKind;
use hiker_projection::ProjectionKind;

use super::edge_paint::{
    adaptive_segments, draw_node_fill, emit_edge, gradient_strokes, hop_potential, sub_polyline,
    tint, Lod, NodeFill, CULL_ALPHA,
};
use super::gpu::GpuBatch;
use super::source::{NodeDescriptor, Source};
use super::styling::fade;
use super::{EdgeMap, Lens, NodeDraw, NodePaint, State};

impl State {
    /// Per-node label-alpha factors for selection dimming: the selected node's
    /// label at full strength, its 1-hop neighbours semi-dimmed, everything else
    /// dimmed — `None` (no dimming) when the option is off or nothing is
    /// selected, so an unselected graph renders exactly as before.
    /// status: graph-label-dim
    pub(super) fn label_dim_factors(&self, source: &dyn Source) -> Option<Vec<f32>> {
        const SEMI: f32 = 0.55;
        const DIM: f32 = 0.18;
        if !self.highlight.dim_labels {
            return None;
        }
        let n = self.positions.len();
        let s = self.selected_node.filter(|&s| s < n)?;
        let mut f = vec![DIM; n];
        for (a, b) in source.edges() {
            let (a, b) = (a as usize, b as usize);
            if a == s && b < n {
                f[b] = SEMI;
            } else if b == s && a < n {
                f[a] = SEMI;
            }
        }
        f[s] = 1.0;
        Some(f)
    }

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
        mut gpu: Option<&mut GpuBatch>,
    ) -> NodeDraw {
        // Spans the GPU node-fill batch build + the per-frame label/hover work.
        // On an affine cache hit the fill pushes are dropped (the batch is
        // `cached`), so this scope then measures only labels/hover.
        super::profile_scope!("graph_view::draw_nodes (gpu batch build + labels)");
        let &NodePaint { lens, zoom, label_zoom, hovered, response_clicked, label_dim } = paint;
        let node_scale = self.style.node_scale;
        let label_size = self.style.label_size;
        let lens_active = lens.active();
        let mut draw = NodeDraw::default();
        // Label candidates, de-conflicted after the node pass: (priority, top-center anchor, text,
        // alpha, font-scale). Priority = magnification, so the most central/focal nodes win an
        // overlap contest — exactly the focus+context behaviour Poincaré wants (readable centre,
        // decluttered rim). The font-scale lets high-level nodes (crates/modules) render larger.
        let mut labels: Vec<(f32, egui::Pos2, String, f32, f32)> = Vec::new();
        // Cull against the pane's clip; pad by a node radius + a label line so a
        // node hugging the edge (whose label hangs below it) is still drawn.
        let clip = painter.clip_rect();
        let label_pad = self.style.label_size + 6.0;
        // The world-space (Affine, cacheable) GPU fill buffer is built ONCE and the
        // GPU scissor clips it every frame, so its fills must NOT be viewport-culled
        // here — else nodes off-screen at build time would be permanently missing
        // when a later pan/zoom on the cached buffer brings them into view.
        let world_fill = gpu.as_deref().is_some_and(|b| b.world_space);
        for d in nodes {
            let p = to_screen(d.world_pos);
            let mag = lens.magnification(d.world_pos);
            // Center nodes grow, rim nodes shrink (1.0 under Affine).
            let r = d.radius * node_scale * zoom.max(0.4) * mag;
            let alpha = lens.rim_alpha(d.world_pos, self.fade_start, self.fade_strength);
            // Alpha cull (Poincaré rim fade) is viewport-independent — always safe.
            if alpha <= CULL_ALPHA {
                continue;
            }
            // Viewport cull gates the per-frame label/hover work and the screen-space
            // fill path; the cached world-space fill ignores it (see above).
            let on_screen = clip.expand(r + label_pad).contains(p);
            if !on_screen && !world_fill {
                continue;
            }
            let fill = fade(d.fill, alpha);
            let is_hover = hovered == Some(d.index);
            let tier = self.lod_tier(mag, lens_active);
            // Base (un-zoomed) radius for the world-space GPU batch: the shader
            // re-applies `max(view_scale, 0.4)` (== `zoom.max(0.4)`), so this must
            // exclude that factor but keep `node_scale`·`mag` (mag == 1 affine).
            let base_r = d.radius * node_scale * mag;
            let fill_pos = NodeFill {
                world: d.world_pos.to_pos2(),
                base_r,
                screen: p,
                screen_r: r,
                color: fill,
            };
            draw_node_fill(painter, gpu.as_deref_mut(), d, tier, fill_pos, is_hover);
            // Labels + hover affordances only on on-screen FULL nodes; dots/markers
            // are too small to anchor a label or a hover ring legibly, and off-screen
            // nodes (kept for the cached world-space fill) get neither.
            if on_screen && tier == Lod::Full {
                // Status badge: a small dot riding the node's top-right shoulder, on
                // the Painter so it draws above the GPU fills (like labels / hover
                // ring) and fades with the node's rim alpha. Sized off the node
                // radius with a floor so it stays a visible mark when zoomed out.
                if let Some(badge) = d.badge {
                    let br = (r * 0.4).clamp(1.5, 4.0);
                    let bp = p + egui::vec2(r * 0.8, -r * 0.8);
                    painter.circle_filled(bp, br, fade(badge, alpha));
                }
                // Its top-left twin (open-bug mark) — an independent channel, so
                // a node can carry both marks. status: code-graph-bug-badge
                if let Some(badge) = d.bug_badge {
                    let br = (r * 0.4).clamp(1.5, 4.0);
                    let bp = p + egui::vec2(-r * 0.8, -r * 0.8);
                    painter.circle_filled(bp, br, fade(badge, alpha));
                }
                // Label-LOD gate, magnification-aware: the effective zoom is
                // `label_zoom * mag`, so under Poincaré (label_zoom = poincare_zoom)
                // central nodes — higher mag — reveal finer-grained labels than the
                // rim. Under Affine off-lens mag is 1.0, so this is exactly the old
                // `zoom >= label_min_zoom` rule.
                if self.toggles.show_labels
                    && label_zoom * mag >= d.label_min_zoom
                    && let Some(label) = &d.label
                {
                    // Defer the draw to the de-confliction pass below. Priority is
                    // biased by label_scale so larger (higher-level) labels win the
                    // overlap contest over small leaf labels nearby. Selection
                    // dimming scales the alpha per node. status: graph-label-dim
                    let dim = label_dim
                        .and_then(|f| f.get(d.index).copied())
                        .unwrap_or(1.0);
                    labels.push((
                        mag * d.label_scale,
                        egui::pos2(p.x, p.y + r + 2.0),
                        label.clone(),
                        alpha * dim,
                        d.label_scale,
                    ));
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
        // De-conflict labels: draw highest-priority (most magnified / central) first, and skip any
        // whose box would overlap an already-placed one — so a dense centre (esp. under Poincaré)
        // stays readable instead of piling text. Labels fade with their node's rim alpha.
        labels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut placed: Vec<egui::Rect> = Vec::new();
        // Show each distinct label text at most once: code graphs are full of
        // generic repeats (`tests`, `crate`, `mod`), and a field of identical
        // words reads as noise. Labels are priority-sorted (magnification desc),
        // so the most prominent instance of a name wins and the rest are dropped.
        let mut seen_text: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (_, anchor, text, alpha, scale) in &labels {
            if !seen_text.insert(text.as_str()) {
                continue;
            }
            let color = fade(self.style.label_color, *alpha);
            // Per-node font size (high-level nodes render larger). Lay out with a
            // stable placeholder colour so egui's cross-frame galley cache keys on
            // geometry alone — the rim-fade `alpha` (which shifts as the lens
            // navigates) is applied at paint time via the fallback colour below,
            // instead of baking into the layout job and busting the cache.
            let font = egui::FontId::proportional(label_size * scale);
            let galley = painter.layout_no_wrap(text.clone(), font, egui::Color32::PLACEHOLDER);
            let top_left = egui::pos2(anchor.x - galley.size().x / 2.0, anchor.y);
            let rect = egui::Rect::from_min_size(top_left, galley.size()).expand(1.0);
            if placed.iter().any(|r| r.intersects(rect)) {
                continue;
            }
            placed.push(rect);
            // Optional label background pill (keeps text legible over a busy graph
            // / at low LOD). Faded with the label so it tracks the rim fade.
            if let Some(bg) = self.style.label_bg {
                painter.rect_filled(rect.expand(2.0), 3.0, fade(bg, *alpha));
            }
            painter.galley(top_left, galley, color);
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
        mut gpu: Option<&mut GpuBatch>,
    ) {
        // Spans the GPU edge batch build (the old pan-lag hotspot). On an affine
        // cache hit the segment pushes are dropped (`cached` batch), so panning
        // pays only the cheap loop, not the upload.
        super::profile_scope!("graph_view::draw_edges (gpu batch build)");
        let &EdgeMap { to_screen, disk_to_screen } = map;
        let width = self.style.edge_width;
        let n = self.positions.len();
        // The affine GPU batch stores WORLD positions (the shader applies the view
        // transform), so a pan/zoom needs no edge rebuild. The lens batch + the
        // Painter path bake the final screen point. This only diverges on the
        // affine (`!lens.active()`) branches below, which is the only place
        // `world_space` is ever set.
        let world_space = gpu.as_deref().is_some_and(|b| b.world_space);
        let pt = |w: egui::Vec2| -> egui::Pos2 {
            if world_space { w.to_pos2() } else { to_screen(w) }
        };
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
            // Per-edge color override (typed edge kinds); the style's single
            // edge color remains the default. status: vault-graph-edge-toggles
            let color = source.edge_color(i).unwrap_or(self.style.edge_color);
            let (wa, wb) = (self.positions[a], self.positions[b]);
            if routed {
                let pts: Vec<egui::Pos2> = match self.edge_routes.get(i) {
                    Some(route) if route.len() >= 2 => {
                        route.iter().map(|&p| pt(p)).collect()
                    }
                    // No usable route for this edge — fall back to a straight
                    // segment between its endpoints.
                    _ => vec![pt(wa), pt(wb)],
                };
                emit_edge(painter, gpu.as_deref_mut(), &pts, width, color);
                continue;
            }
            if !lens.active() {
                let seg = [pt(wa), pt(wb)];
                emit_edge(painter, gpu.as_deref_mut(), &seg, width, color);
                continue;
            }
            let alpha = lens
                .rim_alpha(wa, self.fade_start, self.fade_strength)
                .min(lens.rim_alpha(wb, self.fade_start, self.fade_strength));
            // An edge both of whose endpoints have faded out paints nothing — skip
            // its sampling + stroke (mirrors the node cull).
            if alpha <= CULL_ALPHA {
                continue;
            }
            let faded = fade(color, alpha);
            if !self.geodesic_edges {
                let seg = [to_screen(wa), to_screen(wb)];
                emit_edge(painter, gpu.as_deref_mut(), &seg, width, faded);
                continue;
            }
            let pts: Vec<egui::Pos2> = match self.projection.kind {
                ProjectionKind::Poincare => {
                    let (za, zb) = (lens.disk(wa), lens.disk(wb));
                    // Sample density tracks the on-screen chord, not a flat count:
                    // short edges (the bulk of a dense graph) get 2 points.
                    let chord_px = (disk_to_screen(za) - disk_to_screen(zb)).length();
                    let segs = adaptive_segments(chord_px, self.projection.geodesic_segments);
                    hiker_projection::sample_geodesic(za, zb, segs)
                        .into_iter()
                        .map(disk_to_screen)
                        .collect()
                }
                // Fisheye (and any non-Affine fallback): subdivide the world
                // chord, lensing each sample so the edge tracks the distortion.
                _ => {
                    let chord_px = (to_screen(wa) - to_screen(wb)).length();
                    let steps = adaptive_segments(chord_px, self.projection.geodesic_segments).max(1);
                    (0..=steps)
                        .map(|i| {
                            let t = i as f32 / steps as f32;
                            to_screen(wa + (wb - wa) * t)
                        })
                        .collect()
                }
            };
            emit_edge(painter, gpu.as_deref_mut(), &pts, width, faded);
        }
    }

    /// The screen-space polyline of edge `i` `(a, b)` — routed (layered), geodesic
    /// (lensed), or straight — exactly the geometry [`draw_edges`](Self::draw_edges)
    /// uses, shared by the highlight + hover-flow overlays.
    fn edge_screen_polyline(
        &self,
        i: usize,
        a: usize,
        b: usize,
        map: &EdgeMap<'_>,
        lens: &Lens,
    ) -> Vec<egui::Pos2> {
        let &EdgeMap { to_screen, disk_to_screen } = map;
        let (wa, wb) = (self.positions[a], self.positions[b]);
        let routed = self.layout_kind == LayoutKind::Layered
            && !lens.active()
            && !self.edge_routes.is_empty();
        if routed {
            match self.edge_routes.get(i) {
                Some(route) if route.len() >= 2 => route.iter().map(|&p| to_screen(p)).collect(),
                _ => vec![to_screen(wa), to_screen(wb)],
            }
        } else if !lens.active() || !self.geodesic_edges {
            vec![to_screen(wa), to_screen(wb)]
        } else {
            match self.projection.kind {
                ProjectionKind::Poincare => {
                    let (za, zb) = (lens.disk(wa), lens.disk(wb));
                    let chord_px = (disk_to_screen(za) - disk_to_screen(zb)).length();
                    let segs = adaptive_segments(chord_px, self.projection.geodesic_segments);
                    hiker_projection::sample_geodesic(za, zb, segs)
                        .into_iter()
                        .map(disk_to_screen)
                        .collect()
                }
                _ => {
                    let chord_px = (to_screen(wa) - to_screen(wb)).length();
                    let steps =
                        adaptive_segments(chord_px, self.projection.geodesic_segments).max(1);
                    (0..=steps)
                        .map(|s| {
                            let t = s as f32 / steps as f32;
                            to_screen(wa + (wb - wa) * t)
                        })
                        .collect()
                }
            }
        }
    }

    /// Re-stroke the edges incident to `node`, so a hovered / selected node's
    /// connections light up. The shapes are PUSHED into `out` rather than painted
    /// directly: the caller reserves a shape slot at the very start of the pane
    /// paint and fills it afterwards, so the glow renders BELOW the base edges,
    /// node shapes, and labels (bottom-most) while still being recomputed per
    /// frame outside any cached GPU batch. Mirrors [`draw_edges`](Self::draw_edges)'
    /// geometry in screen space, styled by [`HighlightStyle`](super::HighlightStyle):
    /// a soft multi-pass glow scaled by `softness`, at the given `alpha` (the
    /// caller fades it for hover). A no-op when `node` is `None` or `alpha` is ~0
    /// (the caller also gates on the per-mode toggle + `show_edges`).
    /// status: graph-hover-highlight
    pub(super) fn highlight_edge_shapes(
        &self,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        node: Option<usize>,
        alpha: f32,
    ) {
        let Some(h) = node else { return };
        if alpha <= 0.003 {
            return;
        }
        let n = self.positions.len();
        let hs = self.highlight;
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        let tinted = tint(hs.color);
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            // Only edges incident to the highlighted node.
            if a >= n || b >= n || (a != h && b != h) {
                continue;
            }
            let pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            // Soft glow: two translucent wider passes under the core stroke, so the
            // line reads as a gentle halo rather than a hard wire.
            if soft > 0.0 {
                out.push(egui::Shape::line(
                    pts.clone(),
                    egui::Stroke::new(core_w * (1.0 + 3.0 * soft), tinted(alpha * 0.16 * soft)),
                ));
                out.push(egui::Shape::line(
                    pts.clone(),
                    egui::Stroke::new(core_w * (1.0 + 1.4 * soft), tinted(alpha * 0.30)),
                ));
            }
            out.push(egui::Shape::line(pts, egui::Stroke::new(core_w, tinted(alpha))));
        }
    }

    /// The travelling pulse of a hover-flow transition: on every edge directly
    /// connecting the two hover keyframes, a short bright window slides from the
    /// old node's end to the new one's as `t` goes 0→1 — the *positional* half of
    /// the cross-fade (the endpoint glows are the keyframes; this is the
    /// in-between). Keyframes that aren't adjacent simply cross-fade — no path
    /// search. Shapes are pushed into `out` (same bottom-most slot as the glow).
    /// status: graph-hover-flow
    #[allow(clippy::too_many_arguments)]
    pub(super) fn hover_flow_shapes(
        &self,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        from: usize,
        to: usize,
        t: f32,
        alpha: f32,
    ) {
        if alpha <= 0.003 {
            return;
        }
        let n = self.positions.len();
        if from >= n || to >= n {
            return;
        }
        let hs = self.highlight;
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        let tinted = tint(hs.color);
        const WINDOW: f32 = 0.18; // pulse half-length, as a fraction of the edge
        for (i, (a, b)) in source.edges().into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            let forward = a == from && b == to;
            let backward = a == to && b == from;
            if !forward && !backward {
                continue;
            }
            let mut pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            if !forward {
                pts.reverse(); // parameterize 0 at the `from` end
            }
            let window = sub_polyline(&pts, t - WINDOW, t + WINDOW);
            if window.len() < 2 {
                continue;
            }
            // Brighter and slightly wider than the steady glow, so the pulse
            // reads as motion riding the edge.
            out.push(egui::Shape::line(
                window.clone(),
                egui::Stroke::new(core_w * (1.0 + 2.0 * soft), tinted(alpha * 0.35)),
            ));
            out.push(egui::Shape::line(window, egui::Stroke::new(core_w * 1.3, tinted(alpha))));
        }
    }

    /// Advance the fluid-highlight field one frame and push its shapes: energy is
    /// injected at the hovered node, diffuses across edges, drifts *downhill* in
    /// the hop-distance potential of the selected node (the highlight gravitates
    /// toward it), and decays everywhere — so dragging the pointer across the
    /// graph leaves a glowing wake that drains toward the selection. Rendered as
    /// per-edge gradient strokes (alpha lerped between the endpoint energies)
    /// plus a soft halo under each energized node, all in the caller's
    /// bottom-most slot. O(V+E) per frame; repaints are requested only while any
    /// energy remains. status: graph-hover-fluid
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fluid_advance_and_shapes(
        &mut self,
        ui: &egui::Ui,
        out: &mut Vec<egui::Shape>,
        source: &dyn Source,
        map: &EdgeMap<'_>,
        lens: &Lens,
        hovered: Option<usize>,
    ) {
        let n = self.positions.len();
        if n == 0 {
            return;
        }
        if self.fluid_energy.len() != n {
            self.fluid_energy = vec![0.0; n];
            self.fluid_potential_for = None;
        }
        let edges = source.edges();
        // The gravity field: hop distance from the selected node. Flat (no drift)
        // when nothing is selected. Rebuilt on selection/graph change.
        if self.fluid_potential.len() != n
            || self.fluid_potential_for != self.selected_node
            || self.fluid_potential_epoch != self.layout_epoch
        {
            self.fluid_potential = hop_potential(n, &edges, self.selected_node);
            self.fluid_potential_for = self.selected_node;
            self.fluid_potential_epoch = self.layout_epoch;
        }
        let now = ui.input(|i| i.time);
        let dt = ((now - self.fluid_last_time) as f32).clamp(0.0, 1.0 / 30.0);
        self.fluid_last_time = now;

        // Source: the hovered node is the faucet — and it RAMPS rather than
        // snapping to full, so hovering a new node fades its glow up (~150ms to
        // saturation) instead of hard-activating while the old wake drains.
        const INJECT: f32 = 9.0; // 1/s
        if let Some(h) = hovered
            && h < n
        {
            let e = &mut self.fluid_energy[h];
            *e += (1.0 - *e) * (INJECT * dt).min(1.0);
        }
        // Transfer along edges: symmetric diffusion + directional drift downhill.
        // Per-step fractions are clamped well below 0.5 so the explicit update
        // can't oscillate regardless of frame rate.
        const DIFFUSE: f32 = 3.0; // 1/s
        const DRIFT: f32 = 6.0; // 1/s
        const TAU: f32 = 0.6; // decay time constant, s
        let k_diff = (DIFFUSE * dt).min(0.4);
        let k_drift = (DRIFT * dt).min(0.4);
        for &(a, b) in &edges {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            let d = (self.fluid_energy[a] - self.fluid_energy[b]) * 0.5 * k_diff;
            self.fluid_energy[a] -= d;
            self.fluid_energy[b] += d;
            let (hi, lo) = match self.fluid_potential[a]
                .partial_cmp(&self.fluid_potential[b])
                .unwrap_or(std::cmp::Ordering::Equal)
            {
                std::cmp::Ordering::Greater => (a, b),
                std::cmp::Ordering::Less => (b, a),
                std::cmp::Ordering::Equal => continue,
            };
            let drift = self.fluid_energy[hi] * k_drift;
            self.fluid_energy[hi] -= drift;
            self.fluid_energy[lo] += drift;
        }
        let decay = (-dt / TAU).exp();
        let mut any = false;
        for v in &mut self.fluid_energy {
            *v = (*v * decay).clamp(0.0, 1.0);
            if *v < 0.004 {
                *v = 0.0;
            } else {
                any = true;
            }
        }
        if !any {
            return;
        }
        ui.ctx().request_repaint();

        // Render: gradient strokes on energized edges, halos under energized nodes.
        let hs = self.highlight;
        let tinted = tint(hs.color);
        let core_w = hs.width.max(0.5);
        let soft = hs.softness.clamp(0.0, 1.0);
        for (i, (a, b)) in edges.into_iter().enumerate() {
            let (a, b) = (a as usize, b as usize);
            if a >= n || b >= n {
                continue;
            }
            let (ea, eb) = (self.fluid_energy[a], self.fluid_energy[b]);
            if ea.max(eb) < 0.01 {
                continue;
            }
            let pts = self.edge_screen_polyline(i, a, b, map, lens);
            if pts.len() < 2 {
                continue;
            }
            gradient_strokes(out, &pts, ea * hs.opacity, eb * hs.opacity, core_w, soft, &tinted);
        }
        for (i, &v) in self.fluid_energy.iter().enumerate() {
            if v < 0.01 {
                continue;
            }
            let c = (map.to_screen)(self.positions[i]);
            let r = core_w * (1.5 + 3.5 * v);
            out.push(egui::Shape::circle_filled(c, r * 1.8, tinted(v * hs.opacity * 0.18)));
            out.push(egui::Shape::circle_filled(c, r, tinted(v * hs.opacity * 0.45)));
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
