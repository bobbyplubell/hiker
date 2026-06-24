//! Hyperbolic navigation on [`State`]: resolving the per-frame lens focus,
//! drag-to-recentre (Möbius pan), and the click fly-to animation. A pure
//! `impl State` continuation of the parent module, split out for file length.

use hiker_projection::{clamp_inside_disk, forward, Complex, Mobius, DEFAULT_BOUNDARY_RADIUS};

use super::{centroid_scale, ease_out_cubic, lerp_complex, FlyTo, FocusMode, Lens, State};

/// An affine glide-to-selection in progress: the affine `view.pan` eases from
/// `start_pan` to `target_pan` over `dur` seconds (ease-out-cubic), panning the
/// newly-selected node to the pane centre. Pan only — zoom is left untouched (an
/// auto-zoom on every click is jarring). status: code-graph
#[derive(Clone, Copy)]
pub(super) struct Glide {
    pub(super) start_pan: egui::Vec2,
    pub(super) target_pan: egui::Vec2,
    pub(super) t: f32,
    pub(super) dur: f32,
}

/// Default affine glide duration (seconds).
const GLIDE_DUR: f32 = 0.4;

/// Below this world-distance the glide-to-selection just snaps (no animation) — a
/// tiny move would be an imperceptible animation that only delays the paint.
const GLIDE_MIN_MOVE: f32 = 1.0;

impl State {
    /// The interactive pane's lens focus for this frame, per [`FocusMode`]:
    /// - [`LockedCenter`](FocusMode::LockedCenter): the layout centroid.
    /// - [`Cursor`](FocusMode::Cursor): the world point under the cursor (the
    ///   inverse-affine of the hover position) when the pane is hovered, else the
    ///   centroid. An approximation — `screen_to_affine` inverts the affine map,
    ///   not the lens — but it tracks the cursor closely enough to drive the warp.
    /// - [`Selection`](FocusMode::Selection): the focused node's position if set
    ///   and in range, else the centroid.
    pub(super) fn interactive_focus(
        &self,
        pane_rect: egui::Rect,
        response: Option<&egui::Response>,
    ) -> egui::Vec2 {
        let (centroid, _) = centroid_scale(&self.positions);
        match self.focus_mode {
            FocusMode::LockedCenter => centroid,
            FocusMode::Cursor => response
                .and_then(egui::Response::hover_pos)
                .map(|hp| self.view.screen_to_affine(pane_rect, hp))
                .unwrap_or(centroid),
            FocusMode::Selection => self
                .focus_node
                .and_then(|i| self.positions.get(i).copied())
                .unwrap_or(centroid),
        }
    }

    /// Hyperbolic drag-to-recentre (Poincaré only): the disk point grabbed
    /// under the cursor follows it. Reads the previous + current pointer
    /// positions, maps both into post-nav disk space through the *locked* disk
    /// frame (`disk_center`/`disk_radius`, fixed to the pane — NOT the affine
    /// view), then left-composes the Möbius transform that carries
    /// `p_prev → p_cur` onto `nav`. Any manual drag cancels an in-flight fly-to.
    pub(super) fn handle_mobius_pan(
        &mut self,
        response: &egui::Response,
        disk_center: egui::Pos2,
        disk_radius: f32,
    ) {
        if !response.dragged_by(egui::PointerButton::Primary) {
            return;
        }
        let Some(cur) = response.interact_pointer_pos().or_else(|| response.hover_pos()) else {
            return;
        };
        let prev = cur - response.drag_delta();

        let to_disk = |screen: egui::Pos2| -> Complex {
            let rel = (screen - disk_center) / disk_radius.max(f32::EPSILON);
            clamp_inside_disk(Complex::new(rel.x, rel.y), DEFAULT_BOUNDARY_RADIUS)
        };
        let p_prev = to_disk(prev);
        let p_cur = to_disk(cur);

        let t = Mobius::from_point_pair(p_prev, p_cur);
        self.nav = Mobius::compose(t, self.nav);
        self.flyto = None;
    }

    /// Advance an in-flight click fly-to by one frame, rebuilding `nav` as the
    /// pure recentre that maps the eased disk point to the origin. Requests a
    /// repaint while animating; clears the fly-to once `t` reaches 1.
    pub(super) fn advance_flyto(&mut self, ui: &egui::Ui) {
        let Some(mut fly) = self.flyto else {
            return;
        };
        let dt = ui.input(|i| i.stable_dt);
        fly.t += dt / fly.dur;
        let e = ease_out_cubic(fly.t.min(1.0));
        let c = clamp_inside_disk(
            lerp_complex(fly.start_center, fly.target_center, e),
            DEFAULT_BOUNDARY_RADIUS,
        );
        self.nav = Mobius::from_point_pair(c, Complex::ORIGIN);
        if fly.t >= 1.0 {
            self.flyto = None;
        } else {
            self.flyto = Some(fly);
            ui.ctx().request_repaint();
        }
    }

    /// Begin a click fly-to toward node world position `w_n`: glide the disk
    /// centre from the currently-centred pre-nav point to `w_n`'s pre-nav disk
    /// point. Overwrites any accumulated rotation — fly-to ends cleanly centred.
    pub(super) fn start_flyto(&mut self, w_n: egui::Vec2, lens: &Lens) {
        let rel = (w_n - lens.focus) / lens.scale;
        let target_center = forward(Complex::from([rel.x, rel.y]), self.projection);
        let start_center = self.nav.invert().apply(Complex::ORIGIN);
        self.flyto = Some(FlyTo {
            start_center,
            target_center,
            t: 0.0,
            dur: self.flyto_duration.clamp(0.1, 2.0),
        });
    }

    /// Start an affine glide that pans `target_world` to the pane centre, easing
    /// out over [`GLIDE_DUR`]. Centring a world point means `view.pan == -w` (the
    /// `center_on` law), so the glide eases `view.pan` from its current value to
    /// `-target_world`. Pan only — zoom is left untouched. A tiny move snaps
    /// (sets the pan directly, no animation). status: code-graph
    pub fn glide_to(&mut self, target_world: egui::Vec2) {
        let start_pan = self.view.pan;
        let target_pan = -target_world;
        if (target_pan - start_pan).length() < GLIDE_MIN_MOVE {
            self.view.pan = target_pan;
            self.glide = None;
            return;
        }
        self.glide = Some(Glide { start_pan, target_pan, t: 0.0, dur: GLIDE_DUR });
    }

    /// Advance an in-flight affine glide by `dt` seconds: step `t`, ease-out-cubic,
    /// lerp `view.pan` from `start_pan` to `target_pan`. Clears the glide (landing
    /// exactly on `target_pan`) once `t` reaches 1. Returns `true` while still
    /// animating so the caller can request a repaint. status: code-graph
    pub(super) fn advance_glide(&mut self, dt: f32) -> bool {
        let Some(mut g) = self.glide else {
            return false;
        };
        g.t += dt / g.dur;
        if g.t >= 1.0 {
            self.view.pan = g.target_pan;
            self.glide = None;
            return false;
        }
        let e = ease_out_cubic(g.t);
        self.view.pan = g.start_pan + (g.target_pan - g.start_pan) * e;
        self.glide = Some(g);
        true
    }

    /// The positions-vector index of the node whose last-drawn LABEL centre is nearest `screen`
    /// (companion to [`nearest_label_center`](State::nearest_label_center)). The harness uses it to
    /// drive `selected_node` from a pane-centre click so the affine glide-to-selection is exercised.
    /// status: code-graph
    pub fn nearest_label_node(&self, screen: egui::Pos2) -> Option<usize> {
        self.label_hits
            .iter()
            .min_by(|(a, _), (b, _)| {
                (a.center() - screen)
                    .length_sq()
                    .partial_cmp(&(b.center() - screen).length_sq())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(_, i)| *i)
    }
}
