//! First-class corner **minimap** for the graph-view engine: a locked-Poincaré
//! overview of a [`Source`], inset in a pane corner, click-to-expand to fill the
//! pane, with a **viewport-location indicator** and a **swap-back focus** the host
//! acts on. It is pure CHROME — placement / expand-swap / indicator / overview
//! navigation — and owns **no engine**: every render borrows one through
//! [`Minimap::ui_for`].
//!
//! It supersedes the old built-in two-pane minimap that lived inline in
//! [`State::ui`] (used by nobody in the app — only examples). The canonical
//! shape is the one the canvas board proved out: a corner disk of dots that
//! expands to full-pane, brightening (or outlining) wherever the host's main
//! viewport currently sits, and reporting the focused node on collapse so the
//! host can recentre its own view.
//!
//! **Standalone by design.** The host's main view need not be a graph-view (the
//! canvas board isn't), so the minimap is driven explicitly — the host hands it a
//! borrowed [`State`] engine, its [`Source`], and (optionally) the world rect its
//! main view currently shows (`viewport_world`). A peer host passes its secondary
//! view's engine; a self-overview host (the canvas, with no peer engine) passes an
//! engine from [`Minimap::overview_engine`] whose `positions` it set to its node
//! positions (the card centers) for the frame. status: canvas-minimap, container-tab

use hiker_graph::{LayoutKind, LayoutTree};
use hiker_projection::{Mobius, ProjectionConfig, ProjectionKind};
use hiker_projection_view::{centroid_scale, disk_to_screen, lens_disk, poincare_disk};

use super::source::{NodeDescriptor, Source};
use super::styling::Style;
use super::State;

/// Interpolate two rects by lerping their min + max corners — `t = 0` ⇒ `a`,
/// `t = 1` ⇒ `b`. Drives the expand swap (full ⇄ corner) animation.
fn lerp_rect(a: egui::Rect, b: egui::Rect, t: f32) -> egui::Rect {
    egui::Rect::from_min_max(a.min.lerp(b.min, t), a.max.lerp(b.max, t))
}

/// Seconds the expand swap animation takes end to end.
const SWAP_DURATION: f32 = 0.35;

/// Pane corner the minimap is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    /// The default — bottom-right, out of a top toolbar's way.
    #[default]
    BottomRight,
}

/// Frame of the inset: a clipped disk or a filled square.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Circle,
    Square,
}

/// How the minimap marks where the host's main viewport currently sits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndicatorMode {
    /// No indicator.
    Off,
    /// Brighten the dots whose world position lies inside the viewport rect (the
    /// historical canvas strategy).
    BrightenVisible,
    /// Project the viewport rect onto the disk and stroke its outline.
    ShowViewport,
}

/// What the host should act on after a [`Minimap::ui_for`] frame.
#[derive(Default)]
pub struct Output {
    /// A clicked node's `click_path` (the canvas's card id) — the host brings it
    /// into view in the main view.
    pub clicked: Option<String>,
    /// On a collapse (expanded → corner), the node nearest the disk centre under
    /// the minimap's current navigation — the host recentres its main view on it.
    pub focused_on_collapse: Option<String>,
}

/// The borrowed engine's main-view chrome fields that [`Minimap::ui_for`] overrides for the
/// corner render, saved so they can be restored after the frame (so the same engine still renders
/// faithfully as the full-size primary). status: container-tab
struct SavedEngineView {
    projection: ProjectionConfig,
    nav: Mobius,
    poincare_zoom: f32,
    needs_fit: bool,
    show_boundary: bool,
    show_labels: bool,
    show_preview: bool,
    background: Option<egui::Color32>,
    label_bg: Option<egui::Color32>,
}

/// A first-class corner minimap over a graph [`Source`].
///
/// The minimap is pure CHROME: corner placement, the expand/collapse swap
/// animation, the [`IndicatorMode`], the persistent overview navigation, and the
/// [`Output`]. It owns NO engine — every render borrows one through
/// [`ui_for`](Self::ui_for): a peer host hands its secondary view's engine; a
/// self-overview host (the canvas board, with no peer engine) hands an engine it
/// owns via [`overview_engine`](Self::overview_engine). status: container-tab
pub struct Minimap {
    /// Whether the corner minimap is shown at all.
    pub enabled: bool,
    /// Which corner the inset occupies.
    pub corner: Corner,
    /// Inset side as a fraction of the shorter pane dimension (`0.12..=0.5`).
    pub size: f32,
    /// Inset frame shape.
    pub shape: Shape,
    /// Viewport-location indicator mode.
    pub indicator: IndicatorMode,
    /// When `true` the minimap promotes to fill the pane; a collapse reports the
    /// focused node (see [`Output`]).
    pub expanded: bool,
    /// Eased swap progress in `[0, 1]`: `0` = corner inset, `1` = full pane.
    /// Advances toward `expanded ? 1 : 0` each frame.
    swap_t: f32,
    /// When set (demo/snapshot), `swap_t` is held fixed so a filmstrip can capture
    /// intermediate frames.
    swap_pinned: bool,
    /// Host opt-in to labels. When set, labels are drawn in the corner inset AND when expanded — the
    /// engine's budget LOD (`draw_nodes`) caps the small corner to a readable handful of the top
    /// labels, so a labelled overview (e.g. the code graph's spec minimap, where the specdoc
    /// containers want names) reads even at inset size. Off by default (the canvas overview is a
    /// clean disk of dots). status: graph-label-dim
    labels_when_expanded: bool,
    /// Persistent overview navigation for the BORROWED-engine path ([`ui_for`](Self::ui_for)): the
    /// Poincaré disk pan/zoom the minimap accumulates from drags. Kept HERE, not on the borrowed
    /// engine, so the secondary engine's own main-view nav (it also renders full-size when swapped to
    /// primary) is never corrupted by the corner overview's navigation. status: container-tab
    overview_nav: Mobius,
    /// Overview disk-radius scroll-zoom for the borrowed-engine path (paired with `overview_nav`).
    overview_zoom: f32,
    /// Overview refit flag for the borrowed-engine path: snap the disk back to the whole-graph fit on
    /// the next `ui_for` frame (set on a collapse). status: container-tab
    overview_needs_fit: bool,
}

impl Default for Minimap {
    fn default() -> Self {
        Self::new()
    }
}

impl Minimap {
    /// A fresh minimap chrome (corner placement / swap / indicator / overview
    /// nav). It owns no engine — see [`overview_engine`](Self::overview_engine)
    /// for the self-overview host's engine and [`ui_for`](Self::ui_for) for the
    /// render seam.
    #[must_use]
    pub fn new() -> Self {
        Self {
            enabled: false,
            corner: Corner::default(),
            size: 0.26,
            shape: Shape::Circle,
            indicator: IndicatorMode::BrightenVisible,
            expanded: false,
            swap_t: 0.0,
            swap_pinned: false,
            labels_when_expanded: false,
            overview_nav: Mobius::identity(),
            overview_zoom: 1.0,
            overview_needs_fit: true,
        }
    }

    /// A fresh engine configured for the SelfOverview corner-render seam: the
    /// locked-Poincaré disk-of-dots look (flat style; labels/preview off) a host
    /// with NO peer engine (the canvas board) owns and borrows to [`ui_for`].
    ///
    /// The host sets the engine's `positions` to its supplied node positions each
    /// frame (the canvas hands it the card centers — never force-laid-out), then
    /// passes `&mut engine` to [`ui_for`](Self::ui_for) alongside the host's
    /// `viewport_world`. [`ui_for`] re-asserts the Poincaré projection / labels /
    /// boundary every frame, so this only seeds the starting state.
    /// status: canvas-minimap, container-tab
    #[must_use]
    pub fn overview_engine() -> State {
        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        state.projection.kind = ProjectionKind::Poincare;
        state.projection.strength = 1.0;
        // The disk is locked to the inset — there's no view framing to do.
        state.needs_fit = false;
        state.toggles.show_labels = false;
        state.toggles.show_preview = false;
        state
    }

    /// Show (or hide) node labels in the overview. Off by default (the canvas overview is a clean
    /// disk of dots); a host that wants a labelled overview (e.g. the code graph's spec minimap)
    /// opts in. Labels then appear in the corner inset AND when expanded — the engine's budget LOD
    /// caps the small corner to the top handful of labels (the specdoc containers + a few top
    /// specs), so it stays readable. A readable background pill rides along. status: graph-label-dim
    pub const fn set_labels(&mut self, on: bool) {
        self.labels_when_expanded = on;
    }

    /// Whether labels are drawn in the CORNER inset for the current state — `true` once a host has
    /// opted in via [`set_labels`](Self::set_labels), regardless of swap progress (the budget LOD
    /// keeps the corner readable). Exposed for the host's tests; the canvas SelfOverview never opts
    /// in, so this stays `false` for it. status: graph-label-dim
    #[must_use]
    pub const fn corner_labels_enabled(&self) -> bool {
        self.labels_when_expanded
    }

    /// Force the swap progress directly — for headless snapshot/demo filmstrips
    /// that capture intermediate frames without driving real time.
    #[doc(hidden)]
    pub fn set_swap_t_for_demo(&mut self, t: f32) {
        self.swap_t = t.clamp(0.0, 1.0);
        self.swap_pinned = true;
        self.expanded = t >= 0.5;
    }

    /// The corner inset rect for `host`: a clamped fraction of the shorter pane
    /// dimension, inset by a small margin from the chosen corner.
    fn corner_rect(&self, host: egui::Rect) -> egui::Rect {
        const MARGIN: f32 = 8.0;
        let side = host.width().min(host.height()) * self.size.clamp(0.12, 0.5);
        let (min_x, min_y) = match self.corner {
            Corner::TopLeft => (host.left() + MARGIN, host.top() + MARGIN),
            Corner::TopRight => (host.right() - MARGIN - side, host.top() + MARGIN),
            Corner::BottomLeft => (host.left() + MARGIN, host.bottom() - MARGIN - side),
            Corner::BottomRight => (host.right() - MARGIN - side, host.bottom() - MARGIN - side),
        };
        egui::Rect::from_min_size(egui::pos2(min_x, min_y), egui::Vec2::splat(side))
    }

    /// The minimap settings menu (engine-owned, so consumers don't hand-roll it):
    /// the enable toggle, corner, size, and indicator-mode selector.
    pub fn options_menu(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.enabled, "Show overview");
        if !self.enabled {
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Corner");
            for (corner, label) in [
                (Corner::TopLeft, "\u{2196}"),
                (Corner::TopRight, "\u{2197}"),
                (Corner::BottomLeft, "\u{2199}"),
                (Corner::BottomRight, "\u{2198}"),
            ] {
                ui.selectable_value(&mut self.corner, corner, label);
            }
        });
        ui.add(egui::Slider::new(&mut self.size, 0.12..=0.5).text("Size"));
        ui.horizontal(|ui| {
            ui.label("Indicator");
            ui.selectable_value(&mut self.indicator, IndicatorMode::Off, "None");
            ui.selectable_value(&mut self.indicator, IndicatorMode::BrightenVisible, "Brighten visible");
            ui.selectable_value(&mut self.indicator, IndicatorMode::ShowViewport, "Show viewport");
        });
    }

    /// Render the minimap chrome through a **BORROWED** engine. The sole render
    /// path — the corner-render seam that keeps the minimap engine-free: the host
    /// owns the engine (the secondary lens-view's, or — for a self-overview host
    /// like the canvas — the engine from [`overview_engine`](Self::overview_engine)),
    /// and the minimap only supplies the corner placement, the Poincaré projection,
    /// the expand/collapse swap animation (~0.35s), the nav, the [`IndicatorMode`],
    /// and the [`Output`]. For the peer case the secondary engine already holds a
    /// force layout + positions; for the self-overview case the host SETS the
    /// engine's positions to its supplied node positions (the canvas card centers)
    /// before the call — either way the positions ARE the overview, never re-laid
    /// out here.
    ///
    /// - `engine`: the borrowed engine to render through; its existing
    ///   `engine.positions` ARE the overview positions (never re-laid-out here).
    /// - `source`: the engine's data source (same one the host renders it with).
    /// - `viewport_world`: the world rect the host's main view shows, for the
    ///   indicator. `None` (the code-graph peer case) draws no indicator.
    ///
    /// The engine's own view-affecting fields (projection / nav / disk-zoom / fit /
    /// boundary / labels / preview / background / label-pill) are SAVED, swapped for
    /// the minimap's locked-Poincaré overview chrome + this minimap's persistent
    /// `overview_nav`/`overview_zoom`, then RESTORED after the frame — so the same
    /// engine still renders byte-faithfully when it's the full-size primary (post
    /// swap). The minimap's overview navigation persists across frames on `self`,
    /// not the engine. status: container-tab
    pub fn ui_for(
        &mut self,
        ui: &mut egui::Ui,
        host_rect: egui::Rect,
        engine: &mut State,
        source: &dyn Source,
        viewport_world: Option<egui::Rect>,
    ) -> Output {
        let mut out = Output::default();
        if !self.enabled && !self.expanded && self.swap_t == 0.0 {
            return out;
        }
        self.advance_swap(ui);

        // The inset eases from the corner (swap_t = 0) to the full pane (1).
        let area = lerp_rect(self.corner_rect(host_rect), host_rect, self.swap_t);
        if area.width() < 2.0 || area.height() < 2.0 {
            return out;
        }
        let full = self.swap_t >= 0.999;
        // A host that opted into labels (the code-graph spec minimap) gets them in the CORNER inset
        // too, not only when expanded: the engine's own budget LOD (`draw_nodes`) caps the corner to
        // a readable handful of the top labels — the specdoc containers and a few top specs — exactly
        // like the crate labels in the main view. The canvas SelfOverview never opts in
        // (`labels_when_expanded == false`), so its disk-of-dots is unaffected. status: graph-label-dim
        let labelled = self.labels_when_expanded;

        // The borrowed engine's positions ARE the overview — clone them out for the
        // indicator / outline / focus math (and so the restore below leaves the
        // engine untouched).
        let positions = engine.positions.clone();

        // SAVE the engine's main-view chrome, then install the locked-Poincaré
        // overview look + THIS minimap's persistent overview nav.
        let saved = SavedEngineView {
            projection: engine.projection,
            nav: engine.nav,
            poincare_zoom: engine.poincare_zoom,
            needs_fit: engine.needs_fit,
            show_boundary: engine.show_boundary,
            show_labels: engine.toggles.show_labels,
            show_preview: engine.toggles.show_preview,
            background: engine.style.background,
            label_bg: engine.style.label_bg,
        };
        engine.projection.kind = ProjectionKind::Poincare;
        engine.projection.strength = 1.0;
        engine.nav = self.overview_nav;
        engine.poincare_zoom = self.overview_zoom;
        engine.needs_fit = self.overview_needs_fit;
        engine.show_boundary = full;
        engine.toggles.show_labels = labelled;
        engine.toggles.show_preview = false;
        engine.style.background = (!full).then_some(egui::Color32::TRANSPARENT);
        engine.style.label_bg = labelled.then_some(super::styling::LABEL_PILL);

        if !full {
            let bg = ui.visuals().extreme_bg_color.gamma_multiply(0.9);
            let p = ui.painter().with_clip_rect(area);
            match self.shape {
                Shape::Circle => p.circle_filled(area.center(), 0.5 * area.size().min_elem(), bg),
                Shape::Square => p.rect_filled(area, 4.0, bg),
            };
        }

        let highlight = ui.visuals().selection.stroke.color;
        let in_viewport = match (self.indicator, viewport_world) {
            (IndicatorMode::BrightenVisible, Some(vp)) => {
                positions.iter().map(|p| vp.contains(p.to_pos2())).collect()
            }
            _ => Vec::new(),
        };
        let indicator_src = IndicatorSource { inner: source, in_viewport, highlight };

        let clicked = {
            let mut child =
                ui.new_child(egui::UiBuilder::new().max_rect(area).layout(*ui.layout()));
            engine.ui(&mut child, &indicator_src, |_, _, _, _, _| {})
        };

        if matches!(self.indicator, IndicatorMode::ShowViewport)
            && let Some(vp) = viewport_world
        {
            self.draw_viewport_outline(
                ui,
                area,
                &positions,
                vp,
                highlight,
                engine.projection,
                engine.nav,
                engine.poincare_zoom,
            );
        }

        // Read THIS minimap's overview nav back out (so a drag persists across
        // frames), then RESTORE the engine's main-view chrome.
        self.overview_nav = engine.nav;
        self.overview_zoom = engine.poincare_zoom;
        self.overview_needs_fit = engine.needs_fit;
        engine.projection = saved.projection;
        engine.nav = saved.nav;
        engine.poincare_zoom = saved.poincare_zoom;
        engine.needs_fit = saved.needs_fit;
        engine.show_boundary = saved.show_boundary;
        engine.toggles.show_labels = saved.show_labels;
        engine.toggles.show_preview = saved.show_preview;
        engine.style.background = saved.background;
        engine.style.label_bg = saved.label_bg;

        if let Some(id) = clicked {
            out.clicked = Some(id);
            return out;
        }

        let resp = ui.interact(area, ui.id().with("graphview_minimap_swap"), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            let collapsing = self.expanded;
            self.expanded = !self.expanded;
            if collapsing {
                // The render used the locked Poincaré overview projection (strength 1)
                // + this minimap's overview nav — match it for the focus pick.
                let overview_cfg = ProjectionConfig {
                    kind: ProjectionKind::Poincare,
                    strength: 1.0,
                    ..ProjectionConfig::default()
                };
                out.focused_on_collapse =
                    Self::focused_node_for(source, &positions, overview_cfg, self.overview_nav);
                // Snap the overview disk back to the whole-graph fit next frame.
                self.overview_needs_fit = true;
            }
        }
        out
    }

    /// Advance the expand-swap animation toward its `expanded` target (eased over
    /// [`SWAP_DURATION`]); requests a repaint while in flight. A pinned `swap_t`
    /// (demo/snapshot) is held fixed. Driven by [`ui_for`](Self::ui_for).
    /// status: container-tab
    fn advance_swap(&mut self, ui: &egui::Ui) {
        if self.swap_pinned {
            return;
        }
        let dt = ui.input(|i| i.stable_dt);
        let target = if self.expanded { 1.0 } else { 0.0 };
        if self.swap_t != target && dt > 0.0 {
            let step = dt / SWAP_DURATION;
            if (target - self.swap_t).abs() <= step {
                self.swap_t = target;
            } else {
                self.swap_t += step * (target - self.swap_t).signum();
            }
        }
        if self.swap_t > 0.0 && self.swap_t < 1.0 {
            ui.ctx().request_repaint();
        }
    }

    /// The node whose projected disk point is nearest the disk centre under an
    /// explicit projection + nav — the swap-back focus target the borrowed-engine
    /// [`ui_for`](Self::ui_for) reports on collapse. `None` for an empty graph or a
    /// source without stable node keys.
    fn focused_node_for(
        source: &dyn Source,
        positions: &[egui::Vec2],
        cfg: ProjectionConfig,
        nav: Mobius,
    ) -> Option<String> {
        if positions.is_empty() {
            return None;
        }
        let (focus, scale) = centroid_scale(positions);
        let disk_abs =
            |i: usize| lens_disk((positions[i] - focus) / scale, cfg, nav).abs();
        let best = (0..positions.len())
            .min_by(|&a, &b| disk_abs(a).total_cmp(&disk_abs(b)))?;
        source.node_key(best)
    }

    /// Stroke the host viewport rect, projected onto the disk through the same
    /// lens the overview rendered with (straight segments between the projected
    /// corners — geodesic bowing is a later polish).
    #[allow(clippy::too_many_arguments)]
    fn draw_viewport_outline(
        &self,
        ui: &egui::Ui,
        area: egui::Rect,
        positions: &[egui::Vec2],
        vp: egui::Rect,
        color: egui::Color32,
        cfg: ProjectionConfig,
        nav: Mobius,
        poincare_zoom: f32,
    ) {
        let (focus, scale) = centroid_scale(positions);
        let (center, radius) = poincare_disk(area, poincare_zoom);
        let to_screen = |w: egui::Vec2| disk_to_screen(lens_disk((w - focus) / scale, cfg, nav), center, radius);
        let pts: Vec<egui::Pos2> = [
            vp.min,
            egui::pos2(vp.max.x, vp.min.y),
            vp.max,
            egui::pos2(vp.min.x, vp.max.y),
        ]
        .iter()
        .map(|c| to_screen(c.to_vec2()))
        .collect();
        let painter = ui.painter().with_clip_rect(area);
        let stroke = egui::Stroke::new(1.5, color);
        for i in 0..pts.len() {
            painter.line_segment([pts[i], pts[(i + 1) % pts.len()]], stroke);
        }
    }
}

/// Wraps a host [`Source`] to apply the [`IndicatorMode::BrightenVisible`]
/// indicator engine-side: every method delegates to the inner source, except
/// `nodes`, which brightens the descriptors whose node index is inside the
/// viewport. This keeps consumers' sources plain data providers.
struct IndicatorSource<'a> {
    inner: &'a dyn Source,
    in_viewport: Vec<bool>,
    highlight: egui::Color32,
}

impl Source for IndicatorSource<'_> {
    fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let mut descs = self.inner.nodes(positions, style);
        for d in &mut descs {
            if self.in_viewport.get(d.index).copied().unwrap_or(false) {
                d.fill = brighten(d.fill);
                d.radius *= 1.4;
                if d.resting_stroke == egui::Stroke::NONE {
                    d.resting_stroke = egui::Stroke::new(2.0, self.highlight);
                }
            }
        }
        descs
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.inner.edges()
    }

    fn edge_color(&self, index: usize) -> Option<egui::Color32> {
        self.inner.edge_color(index)
    }

    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree {
        self.inner.layout_tree(kind)
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        self.inner.preview_for(index)
    }

    fn node_key(&self, index: usize) -> Option<String> {
        self.inner.node_key(index)
    }
}

/// A brighter variant of `c` for the in-viewport indicator: blend toward white.
fn brighten(c: egui::Color32) -> egui::Color32 {
    let mix = |v: u8| (u16::from(v) + (255 - u16::from(v)) * 6 / 10) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh minimap draws NO corner labels (the canvas SelfOverview disk-of-dots default); a host
    /// that opts in (the code graph's spec minimap) gets corner labels — the engine's budget LOD then
    /// caps the small corner to the top handful (the specdoc containers + a few top specs).
    /// status: graph-label-dim
    #[test]
    fn corner_labels_off_by_default_on_when_opted_in() {
        let mut m = Minimap::new();
        assert!(!m.corner_labels_enabled(), "canvas overview: no corner labels by default");
        m.set_labels(true);
        assert!(m.corner_labels_enabled(), "opted-in spec minimap shows corner labels");
        m.set_labels(false);
        assert!(!m.corner_labels_enabled(), "labels can be turned back off");
    }
}
