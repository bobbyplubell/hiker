//! First-class corner **minimap** for the graph-view engine: a locked-Poincaré
//! overview of a [`Source`], inset in a pane corner, click-to-expand to fill the
//! pane, with a **viewport-location indicator** and a **swap-back focus** the host
//! acts on. It owns a private overview [`State`] plus the placement / expand-swap
//! / indicator settings that consumers used to hand-roll.
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
//! [`Source`], the per-node world `positions`, and (optionally) the world rect its
//! main view currently shows (`viewport_world`). A graph view showing a minimap of
//! *itself* is just the case where those come from its own state. status: canvas-minimap

use hiker_graph::{LayoutKind, LayoutTree};
use hiker_projection::ProjectionKind;
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

/// What the host should act on after a [`Minimap::ui`] frame.
#[derive(Default)]
pub struct Output {
    /// A clicked node's `click_path` (the canvas's card id) — the host brings it
    /// into view in the main view.
    pub clicked: Option<String>,
    /// On a collapse (expanded → corner), the node nearest the disk centre under
    /// the minimap's current navigation — the host recentres its main view on it.
    pub focused_on_collapse: Option<String>,
}

/// A first-class corner minimap over a graph [`Source`].
pub struct Minimap {
    /// The private overview engine: locked Poincaré, labels off, positions set
    /// directly each frame (never force-laid-out).
    state: State,
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
}

impl Default for Minimap {
    fn default() -> Self {
        Self::new()
    }
}

impl Minimap {
    /// A fresh minimap with an overview engine configured for the locked-Poincaré
    /// disk-of-dots look (flat style; per-node colours come from the host's
    /// [`Source`], labels off so a dense graph doesn't hairball in the corner).
    #[must_use]
    pub fn new() -> Self {
        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        state.projection.kind = ProjectionKind::Poincare;
        state.projection.strength = 1.0;
        // The disk is locked to the inset — there's no view framing to do.
        state.needs_fit = false;
        state.toggles.show_labels = false;
        state.toggles.show_preview = false;
        Self {
            state,
            enabled: false,
            corner: Corner::default(),
            size: 0.26,
            shape: Shape::Circle,
            indicator: IndicatorMode::BrightenVisible,
            expanded: false,
            swap_t: 0.0,
            swap_pinned: false,
        }
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

    /// Render the minimap over `host_rect` and run its interaction.
    ///
    /// - `source` / `positions`: the overview graph and its per-node world
    ///   positions (assigned to the engine directly — never laid out).
    /// - `viewport_world`: the world rect the host's main view currently shows, for
    ///   the indicator. `None` (or [`IndicatorMode::Off`]) draws no indicator.
    ///
    /// Returns the clicked node and, on collapse, the focused node for the host to
    /// recentre on.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        host_rect: egui::Rect,
        source: &dyn Source,
        positions: &[egui::Vec2],
        viewport_world: Option<egui::Rect>,
    ) -> Output {
        let mut out = Output::default();
        if !self.enabled && !self.expanded && self.swap_t == 0.0 {
            return out;
        }

        // Advance the expand swap toward its target; repaint while in flight. A
        // pinned `swap_t` (demo/snapshot) is held fixed.
        if !self.swap_pinned {
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

        // The inset eases from the corner (swap_t = 0) to the full pane (1).
        let area = lerp_rect(self.corner_rect(host_rect), host_rect, self.swap_t);
        if area.width() < 2.0 || area.height() < 2.0 {
            return out;
        }
        let full = self.swap_t >= 0.999;

        self.state.positions = positions.to_vec();
        // Chrome: the corner inset reads as a floating disk over the host (the
        // pane fills transparent and we paint a round/square inset bg); the full
        // expanded overview is opaque with its boundary ring.
        self.state.style.background = (!full).then_some(egui::Color32::TRANSPARENT);
        self.state.show_boundary = full;
        if !full {
            let bg = ui.visuals().extreme_bg_color.gamma_multiply(0.9);
            let p = ui.painter().with_clip_rect(area);
            match self.shape {
                Shape::Circle => p.circle_filled(area.center(), 0.5 * area.size().min_elem(), bg),
                Shape::Square => p.rect_filled(area, 4.0, bg),
            };
        }

        // The viewport indicator is applied engine-side so the host's Source stays
        // a plain data provider: brighten the in-viewport dots via a wrapping
        // Source, or stroke the projected outline after the paint.
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
            self.state.ui(&mut child, &indicator_src, |_, _, _, _, _| {})
        };

        if matches!(self.indicator, IndicatorMode::ShowViewport)
            && let Some(vp) = viewport_world
        {
            self.draw_viewport_outline(ui, area, positions, vp, highlight);
        }

        // A clicked dot is the host's to act on; it also suppresses the
        // empty-area expand toggle for this frame.
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
                // Report the focused node so the host recentres its main view, then
                // snap the inset back to the whole-graph overview (drop the
                // expanded session's accumulated pan/zoom/fly-to).
                out.focused_on_collapse = self.focused_node(source);
                self.state.needs_fit = true;
            }
        }
        out
    }

    /// The node whose projected disk point is nearest the disk centre under the
    /// minimap's current projection + navigation — the swap-back focus target.
    /// `None` for an empty graph or a source without stable node keys.
    fn focused_node(&self, source: &dyn Source) -> Option<String> {
        let positions = &self.state.positions;
        if positions.is_empty() {
            return None;
        }
        let (focus, scale) = centroid_scale(positions);
        let cfg = self.state.projection;
        let nav = self.state.nav;
        let disk_abs =
            |i: usize| lens_disk((positions[i] - focus) / scale, cfg, nav).abs();
        let best = (0..positions.len())
            .min_by(|&a, &b| disk_abs(a).total_cmp(&disk_abs(b)))?;
        source.node_key(best)
    }

    /// Stroke the host viewport rect, projected onto the disk through the same
    /// lens the overview rendered with (straight segments between the projected
    /// corners — geodesic bowing is a later polish).
    fn draw_viewport_outline(
        &self,
        ui: &egui::Ui,
        area: egui::Rect,
        positions: &[egui::Vec2],
        vp: egui::Rect,
        color: egui::Color32,
    ) {
        let (focus, scale) = centroid_scale(positions);
        let cfg = self.state.projection;
        let nav = self.state.nav;
        let (center, radius) = poincare_disk(area, self.state.poincare_zoom);
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
