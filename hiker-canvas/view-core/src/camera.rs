//! Pure view-state viewport transform between the infinite canvas coordinate
//! space ([`hiker_canvas::geometry::Point`]) and on-screen pixels
//! ([`emath::Pos2`]). The [`Camera`] holds a pan offset and a zoom scale and
//! converts at the boundary in both directions. It is never serialized and
//! never enters the op-log — camera state is view state, document state is the
//! `Canvas`.
//
// status: canvas-pan-zoom

use emath::{Pos2, Rect, Vec2};
use hiker_canvas::geometry::{Point, Rect as CanvasRect};

/// The smallest and largest zoom factors the camera clamps to.
const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 20.0;

/// A pan + zoom viewport over the infinite canvas.
///
/// World-to-screen maps a canvas point `p` to
/// `viewport_origin + (p - pan) * scale`, where `pan` is the canvas point that
/// sits at the top-left (`viewport_origin`) of the on-screen rect. The screen
/// rect is supplied per call so the camera carries no pixel geometry of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// The canvas-space point pinned to the top-left of the viewport.
    pan: Point,
    /// Pixels per canvas unit.
    scale: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self { pan: Point::new(0.0, 0.0), scale: 1.0 }
    }
}

impl Camera {
    /// Current zoom factor (pixels per canvas unit).
    #[must_use]
    pub const fn scale(&self) -> f32 {
        self.scale
    }

    /// The canvas point pinned to the viewport's top-left corner.
    #[must_use]
    pub const fn pan(&self) -> Point {
        self.pan
    }

    /// Restore a saved pan + zoom directly (used when reloading persisted view
    /// state — `canvas-view-state-persist`). `scale` is clamped to the same
    /// `MIN_SCALE..MAX_SCALE` bounds the zoom gestures honor, so a stale or
    /// hand-edited snapshot can't push the camera outside its range.
    pub const fn set_pan_scale(&mut self, pan: Point, scale: f32) {
        self.pan = pan;
        self.scale = scale.clamp(MIN_SCALE, MAX_SCALE);
    }

    /// Map a canvas-space point to screen pixels within `viewport`.
    #[must_use]
    pub fn world_to_screen(&self, viewport: Rect, p: Point) -> Pos2 {
        let dx = (p.x - self.pan.x) as f32 * self.scale;
        let dy = (p.y - self.pan.y) as f32 * self.scale;
        viewport.min + Vec2::new(dx, dy)
    }

    /// Map a screen-pixel position within `viewport` back to canvas space.
    #[must_use]
    pub fn screen_to_world(&self, viewport: Rect, pos: Pos2) -> Point {
        let off = pos - viewport.min;
        Point::new(
            f64::from(off.x / self.scale) + self.pan.x,
            f64::from(off.y / self.scale) + self.pan.y,
        )
    }

    /// Map a canvas-space rectangle to its on-screen rectangle.
    #[must_use]
    pub fn world_rect_to_screen(&self, viewport: Rect, r: CanvasRect) -> Rect {
        let min = self.world_to_screen(viewport, Point::new(r.x, r.y));
        let max = self.world_to_screen(viewport, Point::new(r.right(), r.bottom()));
        Rect::from_min_max(min, max)
    }

    /// Pan by a screen-pixel delta (the gesture for dragging empty canvas).
    /// Dragging the content right (+dx) moves the pinned canvas point left.
    pub fn pan_by_screen(&mut self, delta: Vec2) {
        self.pan.x -= f64::from(delta.x / self.scale);
        self.pan.y -= f64::from(delta.y / self.scale);
    }

    /// Zoom by `factor` while keeping the canvas point under `cursor` fixed on
    /// screen (scroll/pinch toward the cursor). `factor > 1` zooms in.
    pub fn zoom_to_cursor(&mut self, viewport: Rect, cursor: Pos2, factor: f32) {
        let anchor = self.screen_to_world(viewport, cursor);
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        // Re-pin: choose pan so `anchor` maps back to the same screen `cursor`.
        let off = cursor - viewport.min;
        self.pan.x = anchor.x - f64::from(off.x / self.scale);
        self.pan.y = anchor.y - f64::from(off.y / self.scale);
    }

    /// Frame `content` within `viewport`, leaving a fractional `margin` of
    /// padding on each side. A degenerate (zero-area) content rect centers at
    /// scale 1.
    ///
    /// The fit scale is capped above by `MAX_SCALE` (framing a single tiny node
    /// shouldn't zoom in absurdly) but is **not** floored at the gesture
    /// `MIN_SCALE`: oversized content — a folder-derived tree or cluster tree
    /// spanning far more world units than `viewport / MIN_SCALE` — must be free
    /// to zoom out past the gesture floor, or "fit all" would clamp and overflow
    /// the viewport instead of framing everything. A later zoom gesture re-clamps
    /// into the gesture range. `sx`/`sy` are always positive here (the degenerate
    /// case returned above), so no lower floor is needed.
    pub fn zoom_to_fit(&mut self, viewport: Rect, content: CanvasRect, margin: f32) {
        let vw = viewport.width();
        let vh = viewport.height();
        if content.width <= 0.0 || content.height <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            self.scale = 1.0;
            self.center_on(viewport, content);
            return;
        }
        let pad = 1.0 + 2.0 * margin.max(0.0);
        let sx = vw / (content.width as f32 * pad);
        let sy = vh / (content.height as f32 * pad);
        self.scale = sx.min(sy).min(MAX_SCALE);
        self.center_on(viewport, content);
    }

    /// Pin pan so canvas-space `point` sits at the center of `viewport`,
    /// keeping the current zoom. Used to bring a followed node into view
    /// without disturbing the user's scale. status: tab-linking
    pub fn center_on_point(&mut self, viewport: Rect, point: Point) {
        let half_w = f64::from(viewport.width() / 2.0 / self.scale);
        let half_h = f64::from(viewport.height() / 2.0 / self.scale);
        self.pan = Point::new(point.x - half_w, point.y - half_h);
    }

    /// Pin pan so the center of `content` sits at the center of `viewport`.
    fn center_on(&mut self, viewport: Rect, content: CanvasRect) {
        let center = content.center();
        let half_w = f64::from(viewport.width() / 2.0 / self.scale);
        let half_h = f64::from(viewport.height() / 2.0 / self.scale);
        self.pan = Point::new(center.x - half_w, center.y - half_h);
    }
}

#[cfg(test)]
mod tests {
    use super::Camera;
    use emath::{Pos2, Rect, Vec2};
    use hiker_canvas::geometry::{Point, Rect as CanvasRect};

    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(800.0, 600.0))
    }

    fn assert_point_near(a: Point, b: Point) {
        assert!((a.x - b.x).abs() < 1e-3, "x: {} vs {}", a.x, b.x);
        assert!((a.y - b.y).abs() < 1e-3, "y: {} vs {}", a.y, b.y);
    }

    #[test]
    fn world_screen_round_trips_at_unit_scale() {
        let cam = Camera::default();
        let vp = viewport();
        for p in [Point::new(0.0, 0.0), Point::new(123.5, -42.0), Point::new(-900.0, 700.0)] {
            let screen = cam.world_to_screen(vp, p);
            let back = cam.screen_to_world(vp, screen);
            assert_point_near(p, back);
        }
    }

    #[test]
    fn world_screen_round_trips_when_panned_and_zoomed() {
        let mut cam = Camera::default();
        let vp = viewport();
        cam.pan_by_screen(Vec2::new(40.0, -25.0));
        cam.zoom_to_cursor(vp, Pos2::new(300.0, 200.0), 2.5);
        let p = Point::new(64.0, 96.0);
        let back = cam.screen_to_world(vp, cam.world_to_screen(vp, p));
        assert_point_near(p, back);
    }

    #[test]
    fn zoom_keeps_point_under_cursor_fixed() {
        let mut cam = Camera::default();
        let vp = viewport();
        let cursor = Pos2::new(420.0, 333.0);
        let world_before = cam.screen_to_world(vp, cursor);
        cam.zoom_to_cursor(vp, cursor, 3.0);
        let world_after = cam.screen_to_world(vp, cursor);
        // The canvas point under the cursor must not move when zooming.
        assert_point_near(world_before, world_after);
        assert!((cam.scale() - 3.0).abs() < 1e-4);
    }

    #[test]
    fn scale_clamps_to_bounds() {
        let mut cam = Camera::default();
        let vp = viewport();
        for _ in 0..200 {
            cam.zoom_to_cursor(vp, vp.center(), 2.0);
        }
        assert!(cam.scale() <= 20.0 + 1e-3);
        for _ in 0..400 {
            cam.zoom_to_cursor(vp, vp.center(), 0.5);
        }
        assert!(cam.scale() >= 0.05 - 1e-4);
    }

    #[test]
    fn set_pan_scale_restores_and_clamps() {
        let mut cam = Camera::default();
        cam.set_pan_scale(Point::new(-120.5, 33.0), 0.5);
        assert_point_near(cam.pan(), Point::new(-120.5, 33.0));
        assert!((cam.scale() - 0.5).abs() < 1e-6);
        // Out-of-range scales clamp to the same bounds the gestures use.
        cam.set_pan_scale(Point::new(0.0, 0.0), 1000.0);
        assert!((cam.scale() - 20.0).abs() < 1e-6);
        cam.set_pan_scale(Point::new(0.0, 0.0), 0.0001);
        assert!((cam.scale() - 0.05).abs() < 1e-6);
    }

    #[test]
    fn zoom_to_fit_frames_content_centered() {
        let mut cam = Camera::default();
        let vp = viewport();
        let content = CanvasRect::new(-100.0, -100.0, 200.0, 200.0);
        cam.zoom_to_fit(vp, content, 0.1);
        // The content center maps to the viewport center.
        let screen_center = cam.world_to_screen(vp, content.center());
        assert!((screen_center.x - vp.center().x).abs() < 1.0);
        assert!((screen_center.y - vp.center().y).abs() < 1.0);
        // Content fits with margin: its screen extent is within the viewport.
        let r = cam.world_rect_to_screen(vp, content);
        assert!(r.width() <= vp.width() + 1.0 && r.height() <= vp.height() + 1.0);
    }

    #[test]
    fn zoom_to_fit_frames_oversized_content_below_gesture_floor() {
        // A folder-derived tree / cluster tree can span far more world units than
        // `viewport / MIN_SCALE`. Fitting it must zoom out past the gesture floor
        // so everything lands inside the viewport (regression: it used to clamp to
        // MIN_SCALE and overflow, so the preview looked un-fit).
        let mut cam = Camera::default();
        let vp = viewport(); // 800x600
        let content = CanvasRect::new(0.0, 0.0, 60_000.0, 40_000.0);
        cam.zoom_to_fit(vp, content, 0.1);
        // The required fit scale is well below the gesture floor.
        assert!(cam.scale() < 0.05, "fit zoomed out past MIN_SCALE, got {}", cam.scale());
        // And the whole content actually fits inside the viewport.
        let r = cam.world_rect_to_screen(vp, content);
        assert!(
            r.width() <= vp.width() + 1.0 && r.height() <= vp.height() + 1.0,
            "oversized content framed within viewport: {:?}",
            r
        );
    }
}
