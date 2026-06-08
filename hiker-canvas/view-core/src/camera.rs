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
use hiker_projection::{
    clamp_inside_disk, forward, inverse, magnification, Complex, Mobius, ProjectionConfig,
    ProjectionKind, DEFAULT_BOUNDARY_RADIUS,
};

/// The smallest and largest zoom factors the camera clamps to.
const MIN_SCALE: f32 = 0.002;
const MAX_SCALE: f32 = 20.0;

/// Scroll-zoom clamp for the locked Poincaré disk radius. [proj-canvas-mode]
const POINCARE_ZOOM_MIN: f32 = 0.3;
const POINCARE_ZOOM_MAX: f32 = 25.0;

/// Fraction of the viewport's shorter half-dimension the locked Poincaré disk
/// fills, leaving a small margin so the boundary ring isn't flush against the
/// edge.
const DISK_FILL: f32 = 0.92;

/// The locked Poincaré disk frame for a viewport: its centre (the viewport
/// centre) and radius (`DISK_FILL` of the shorter half-dimension × `zoom`).
/// Deliberately a pure function of `viewport` + `zoom` *only* — it must NOT
/// depend on the camera's `pan`/`scale`, so the disk stays fixed-CENTERED to the
/// pane as the user navigates (the disk IS the viewport; navigation is Möbius
/// `nav` drag + click fly-to, never affine pan/zoom). Scroll-zoom scales the
/// RADIUS only — the centre is zoom-invariant (always the viewport centre), so
/// the disk grows/shrinks centred and never drifts. Mirrors the graph view's
/// `poincare_disk`. [proj-canvas-mode]
fn poincare_disk(viewport: Rect, zoom: f32) -> (Pos2, f32) {
    let radius = 0.5 * viewport.size().min_elem() * DISK_FILL * zoom;
    (viewport.center(), radius)
}

/// Default lower / upper bounds for the per-card lens scale (`proj-card-scale`).
/// A peripheral card never shrinks below `MIN` (it stays clickable, or collapses
/// to an LOD dot), and a card never grows past `MAX`. The upper bound is well
/// above `1.0` because the neighbor-gap fill (`proj-card-fill`) sizes cards to
/// the on-screen distance to their neighbour — under a lens the disk spreads the
/// board wide, so the fill target routinely exceeds the (small, fit-to-pane)
/// affine card size and the cap is what lets cards actually grow to fill the
/// space instead of floating tiny. Identity (`1.0`) when the lens is Off.
const DEFAULT_CARD_SCALE_MIN: f32 = 0.35;
const DEFAULT_CARD_SCALE_MAX: f32 = 2.5;

/// Default for the neighbor-gap fill factor: a card grows to ~90% of the screen
/// distance to its nearest neighbour, so sparse regions fill out without cards
/// overlapping. [proj-card-fill]
const DEFAULT_CARD_FILL: f32 = 0.9;

/// Per-card uniform-scale clamp for the canvas projection compromise: cards stay
/// axis-aligned rects scaled by a single magnification factor (egui can't shear a
/// glyph), so the factor is clamped to keep rim cards usable and center cards
/// proportionate. `fill` is the neighbor-gap "fill the space" factor consulted
/// only under an active lens (`proj-card-fill`). [proj-cfg-card-scale-clamp]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardScaleClamp {
    /// Smallest per-card scale (rim cards never shrink below this).
    pub min: f32,
    /// Largest per-card scale (center cards never grow past this).
    pub max: f32,
    /// How aggressively a card grows to fill the gap to its nearest neighbour
    /// under a lens: the on-screen target size is `gap * fill`. [proj-card-fill]
    pub fill: f32,
}

impl Default for CardScaleClamp {
    fn default() -> Self {
        Self { min: DEFAULT_CARD_SCALE_MIN, max: DEFAULT_CARD_SCALE_MAX, fill: DEFAULT_CARD_FILL }
    }
}

impl CardScaleClamp {
    /// Clamp a raw magnification into `[min, max]` (with `min <= max` enforced).
    #[must_use]
    pub const fn apply(self, mag: f32) -> f32 {
        mag.clamp(self.min.min(self.max), self.max.max(self.min))
    }
}

/// The per-frame canvas lens: the `world → lensed-world` step inserted *before*
/// the affine [`Camera`] mapping, mirroring the graph view's `Lens`. Holds the
/// projection config plus the framing (`focus` = the world point that maps to the
/// disk center, `scale` = the world radius that normalises the lens so `tanh`
/// doesn't saturate). With [`ProjectionKind::Affine`] every method is the
/// identity, so a non-projected canvas renders byte-identically. Egui-free.
/// [proj-canvas-mode]
#[derive(Debug, Clone, Copy)]
pub struct Lens {
    cfg: ProjectionConfig,
    focus: Point,
    scale: f64,
    /// Hyperbolic navigation transform applied to the disk point *after* the
    /// projection — Poincaré only. Identity (no effect) under Off/Fisheye, so
    /// those modes stay byte-identical. Drag-to-recentre + click fly-to compose
    /// into this. [proj-poincare-nav]
    nav: Mobius,
}

impl Lens {
    /// Whether the lens warps at all (false ⇒ every method is the identity).
    #[must_use]
    pub fn active(&self) -> bool {
        self.cfg.kind != ProjectionKind::Affine
    }

    /// The active projection config (read-only).
    #[must_use]
    pub const fn cfg(&self) -> ProjectionConfig {
        self.cfg
    }

    /// The disk point for a world position: `forward((w − focus) / scale)`. The
    /// hiker-projection boundary is `f32`/[`Complex`], so convert here. Public
    /// mirror of the private `disk` so the paint layer can sample geodesics
    /// between two world anchors directly in disk space.
    #[must_use]
    pub fn disk_point(&self, p: Point) -> Complex {
        self.disk(p)
    }

    /// Map a disk point back to lensed-world space — the public mirror of the
    /// private `disk_to_world`, so the paint layer can map geodesic samples back
    /// to lensed-world and then through the *affine-only* screen mapping (avoiding
    /// the double-lensing that the full `world_to_screen` would introduce).
    #[must_use]
    pub fn disk_world(&self, z: Complex) -> Point {
        self.disk_to_world(z)
    }

    /// The disk point for a world position: `forward((w − focus) / scale)`, then
    /// the `nav` Möbius recentre for Poincaré (identity otherwise). The
    /// hiker-projection boundary is `f32`/[`Complex`], so convert here. Every
    /// downstream method (`world_to_lensed`, `magnification`, `disk_point`) routes
    /// through here, so they pick up navigation automatically; Off/Fisheye never
    /// apply `nav`, keeping them byte-identical. [proj-poincare-nav]
    fn disk(&self, p: Point) -> Complex {
        let rel = [
            ((p.x - self.focus.x) / self.scale) as f32,
            ((p.y - self.focus.y) / self.scale) as f32,
        ];
        let z = forward(Complex::from(rel), self.cfg);
        if self.cfg.kind == ProjectionKind::Poincare {
            self.nav.apply(z)
        } else {
            z
        }
    }

    /// Map a disk point back to lensed-world space (the inverse of the
    /// `(w − focus) / scale` framing only — not of the lens remap).
    fn disk_to_world(&self, z: Complex) -> Point {
        Point::new(
            self.focus.x + f64::from(z.re) * self.scale,
            self.focus.y + f64::from(z.im) * self.scale,
        )
    }

    /// `world → lensed-world`. Identity under Affine; otherwise the lensed point
    /// lives back in world space in a disk of radius `scale` around `focus`,
    /// ready for the affine mapping.
    #[must_use]
    pub fn world_to_lensed(&self, p: Point) -> Point {
        if !self.active() {
            return p;
        }
        self.disk_to_world(self.disk(p))
    }

    /// `lensed-world → world`. The closed-form inverse of [`Lens::world_to_lensed`];
    /// identity under Affine. Re-frames the lensed point into the unit disk,
    /// applies the projection inverse, then un-frames back to world space.
    #[must_use]
    pub fn lensed_to_world(&self, p: Point) -> Point {
        if !self.active() {
            return p;
        }
        let z = Complex::from([
            ((p.x - self.focus.x) / self.scale) as f32,
            ((p.y - self.focus.y) / self.scale) as f32,
        ]);
        // Under Poincaré the lensed point lives in the *post-nav* disk, so undo
        // the navigation recentre before the projection inverse so the round-trip
        // stays exact. [proj-poincare-nav]
        let z = if self.cfg.kind == ProjectionKind::Poincare {
            self.nav.invert().apply(z)
        } else {
            z
        };
        self.disk_to_world(inverse(z, self.cfg))
    }

    /// Local linear magnification at a world position (1.0 under Affine). Couples
    /// the per-card scale (`proj-card-scale`) and the LOD ladder
    /// (`proj-lod-ladder`).
    #[must_use]
    pub fn magnification(&self, p: Point) -> f32 {
        if !self.active() {
            return 1.0;
        }
        magnification(self.disk(p), self.cfg)
    }
}

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
    /// The projection lens applied to world coordinates *before* the affine
    /// pan/zoom. Default [`ProjectionKind::Affine`] makes the lens the identity,
    /// so the canvas renders byte-identically to a non-projected board.
    /// [proj-canvas-mode]
    projection: ProjectionConfig,
    /// The per-card uniform-scale clamp used by the card-scale compromise.
    /// [proj-cfg-card-scale-clamp]
    card_scale: CardScaleClamp,
    /// The world point that maps to the disk center — the lens focus. Recomputed
    /// each frame by [`Camera::update_lens`] from the canvas content bounds.
    lens_focus: Point,
    /// The world radius that normalises the lens so `tanh` doesn't saturate;
    /// lensed points occupy a disk of this radius around `lens_focus`. Floored at
    /// 1.0. Recomputed each frame by [`Camera::update_lens`].
    lens_scale: f64,
    /// Whether to stroke the Poincaré unit-disk boundary circle. Only consulted
    /// under [`ProjectionKind::Poincare`]; ignored (and never drawn) under
    /// Off/Fisheye, so those modes stay byte-identical. [proj-canvas-mode]
    show_boundary: bool,
    /// Hyperbolic navigation transform applied *after* the Poincaré lens (and
    /// only for Poincaré): drag-to-recentre + click fly-to compose into it.
    /// Identity for a freshly-built / Reset view, and ignored under Off/Fisheye
    /// so those modes stay byte-identical. [proj-poincare-nav]
    nav: Mobius,
    /// Scroll-zoom factor for the locked Poincaré disk RADIUS (the disk centre
    /// stays the viewport centre regardless). `1.0` = fit-to-pane (default);
    /// larger grows the disk (content bigger, may clip the viewport edges),
    /// smaller shrinks it. Clamped to `[POINCARE_ZOOM_MIN, POINCARE_ZOOM_MAX]`.
    /// Reset to `1.0` by `fit`/reset alongside `nav`. Poincaré-only — affine
    /// `pan`/`scale` are untouched, so Off/Fisheye stay byte-identical.
    /// [proj-canvas-mode]
    poincare_zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: Point::new(0.0, 0.0),
            scale: 1.0,
            projection: ProjectionConfig::default(),
            card_scale: CardScaleClamp::default(),
            lens_focus: Point::new(0.0, 0.0),
            lens_scale: 1.0,
            show_boundary: true,
            nav: Mobius::identity(),
            poincare_zoom: 1.0,
        }
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

    /// The active projection config (read-only). [proj-canvas-mode]
    #[must_use]
    pub const fn projection(&self) -> ProjectionConfig {
        self.projection
    }

    /// Whether a non-identity projection lens is active. When `false`,
    /// `world_to_screen` / `screen_to_world` and card scaling are exactly today's
    /// affine behaviour.
    #[must_use]
    pub fn lens_active(&self) -> bool {
        self.projection.kind != ProjectionKind::Affine
    }

    /// Mutable access to the projection config so the host's view menu can drive
    /// the kind / strength / size-falloff sliders directly. [proj-cfg-strength,
    /// proj-cfg-size-falloff]
    pub const fn projection_mut(&mut self) -> &mut ProjectionConfig {
        &mut self.projection
    }

    /// Set the whole projection config at once. [proj-canvas-mode]
    pub const fn set_projection(&mut self, cfg: ProjectionConfig) {
        self.projection = cfg;
    }

    /// The per-card scale clamp (read-only). [proj-cfg-card-scale-clamp]
    #[must_use]
    pub const fn card_scale_clamp(&self) -> CardScaleClamp {
        self.card_scale
    }

    /// Mutable access to the per-card scale clamp for the view menu sliders.
    /// [proj-cfg-card-scale-clamp]
    pub const fn card_scale_clamp_mut(&mut self) -> &mut CardScaleClamp {
        &mut self.card_scale
    }

    /// Whether the Poincaré boundary circle should be stroked (read-only).
    /// [proj-canvas-mode]
    #[must_use]
    pub const fn show_boundary(&self) -> bool {
        self.show_boundary
    }

    /// Mutable access to the boundary-circle toggle for the view menu checkbox.
    /// [proj-canvas-mode]
    pub const fn show_boundary_mut(&mut self) -> &mut bool {
        &mut self.show_boundary
    }

    /// The current lens focus — the world point that maps to the disk centre.
    /// Under the locked Poincaré disk the focus maps to the disk centre (the
    /// viewport centre) regardless of `pan`/`scale`.
    #[must_use]
    pub const fn lens_focus(&self) -> Point {
        self.lens_focus
    }

    /// The current lens scale — the world radius the lens normalises against.
    #[must_use]
    pub const fn lens_scale(&self) -> f64 {
        self.lens_scale
    }

    /// The pane-locked Poincaré disk frame for `viewport`: its centre (the
    /// viewport centre) and radius. A pure function of the viewport — independent
    /// of `pan`/`scale` — so the paint layer can stroke the boundary ring and map
    /// disk points against the exact frame [`Camera::world_to_screen`] uses.
    /// [proj-canvas-mode]
    #[must_use]
    pub fn poincare_disk_frame(&self, viewport: Rect) -> (Pos2, f32) {
        poincare_disk(viewport, self.poincare_zoom)
    }

    /// The per-frame lens, composed from the projection config and the current
    /// framing (`lens_focus` / `lens_scale`). The view calls
    /// [`Camera::update_lens`] once per frame to refresh the framing before
    /// painting. [proj-canvas-mode]
    #[must_use]
    pub const fn lens(&self) -> Lens {
        Lens {
            cfg: self.projection,
            focus: self.lens_focus,
            scale: self.lens_scale,
            nav: self.nav,
        }
    }

    /// The current hyperbolic navigation transform (read-only). Identity unless
    /// a Poincaré drag-recentre / fly-to has accumulated into it.
    /// [proj-poincare-nav]
    #[must_use]
    pub const fn nav(&self) -> Mobius {
        self.nav
    }

    /// Replace the hyperbolic navigation transform — drag-to-recentre + fly-to
    /// compose their result and set it here. Only consulted under Poincaré.
    /// [proj-poincare-nav]
    pub const fn set_nav(&mut self, nav: Mobius) {
        self.nav = nav;
    }

    /// Reset navigation to identity — the Reset / fit path recentres the disk by
    /// dropping any accumulated drag-recentre / fly-to. [proj-poincare-nav]
    pub const fn reset_nav(&mut self) {
        self.nav = Mobius::identity();
        self.poincare_zoom = 1.0;
    }

    /// Scroll-zoom the locked Poincaré disk RADIUS (centre stays the viewport
    /// centre, so it zooms without drifting); clamped. Off/Fisheye use the affine
    /// zoom instead. [proj-canvas-mode]
    pub fn zoom_poincare(&mut self, scroll_y: f32) {
        self.poincare_zoom = (self.poincare_zoom * (scroll_y * 0.005).exp())
            .clamp(POINCARE_ZOOM_MIN, POINCARE_ZOOM_MAX);
    }

    /// The pre-nav disk point of a world position — `forward((w − focus) / scale)`
    /// WITHOUT the navigation recentre. The fly-to target: a card's resting disk
    /// point, which the glide carries to the disk centre. [proj-poincare-nav]
    #[must_use]
    pub fn disk_point(&self, p: Point) -> Complex {
        let rel = [
            ((p.x - self.lens_focus.x) / self.lens_scale) as f32,
            ((p.y - self.lens_focus.y) / self.lens_scale) as f32,
        ];
        forward(Complex::from(rel), self.projection)
    }

    /// The post-nav disk point under a screen position, for the drag-recentre
    /// math. Under the LOCKED Poincaré disk the grabbed point is read straight
    /// off the pane-fixed disk frame — `(screen − disk_center) / disk_radius`,
    /// clamped inside the boundary — NOT through the affine view (which the disk no
    /// longer rides). This is the grabbed point in the same post-nav disk space the
    /// next [`Camera::set_nav`] composes against, so the point under the cursor
    /// follows it. Mirrors the graph view's `handle_mobius_pan`. [proj-poincare-nav]
    #[must_use]
    pub fn disk_under_screen(&self, viewport: Rect, screen_pos: Pos2) -> Complex {
        let (center, radius) = poincare_disk(viewport, self.poincare_zoom);
        let rel = (screen_pos - center) / radius.max(f32::EPSILON);
        clamp_inside_disk(Complex::new(rel.x, rel.y), DEFAULT_BOUNDARY_RADIUS)
    }

    /// Refresh the per-frame lens framing from the canvas content bounds: the
    /// focus is the bounds center, and the scale is half the bounding-box diagonal
    /// (floored at 1.0) so the whole board occupies the lens disk without `tanh`
    /// saturating. A no-op for an empty canvas (callers pass `None`). The view
    /// calls this once per frame before painting. [proj-canvas-mode]
    pub fn update_lens(&mut self, content_bounds: Option<CanvasRect>) {
        let Some(b) = content_bounds else { return };
        self.lens_focus = b.center();
        let diag = (b.width * b.width + b.height * b.height).sqrt();
        self.lens_scale = (diag * 0.5).max(1.0);
    }

    /// The clamped per-card uniform scale at world point `center`, for the
    /// card-scale compromise. `1.0` when the lens is Off (so cards keep their
    /// affine size). [proj-card-scale]
    #[must_use]
    pub fn card_scale_at(&self, center: Point) -> f32 {
        if !self.lens_active() {
            return 1.0;
        }
        self.card_scale.apply(self.lens().magnification(center))
    }

    /// The raw (unclamped) lens magnification at world point `center` — drives the
    /// LOD coupling so rim cards collapse to dots. `1.0` when the lens is Off.
    /// [proj-lod-ladder]
    #[must_use]
    pub fn magnification_at(&self, center: Point) -> f32 {
        self.lens().magnification(center)
    }

    /// The Poincaré rim-fade alpha multiplier at world point `p`: peripheral
    /// content recedes toward the disk boundary, so we fade its fill/stroke alpha
    /// by the local magnification (clamped to `[0, 1]`). `1.0` (no fade) under
    /// Off/Fisheye, so those modes stay byte-identical. [proj-canvas-mode]
    #[must_use]
    pub fn rim_alpha_at(&self, p: Point) -> f32 {
        if self.projection.kind != ProjectionKind::Poincare {
            return 1.0;
        }
        self.lens().magnification(p).clamp(0.0, 1.0)
    }

    /// Restore a saved pan + zoom directly (used when reloading persisted view
    /// state — `canvas-view-state-persist`). `scale` is clamped to the same
    /// `MIN_SCALE..MAX_SCALE` bounds the zoom gestures honor, so a stale or
    /// hand-edited snapshot can't push the camera outside its range.
    pub const fn set_pan_scale(&mut self, pan: Point, scale: f32) {
        self.pan = pan;
        self.scale = scale.clamp(MIN_SCALE, MAX_SCALE);
    }

    /// The affine half of the transform: map a *lensed-world* point to screen.
    /// The lens (`world → lensed-world`) is applied by [`Camera::world_to_screen`]
    /// before this; pan/zoom gestures pin against this affine layer directly so
    /// they stay linear under the lens. Identity-composed when the lens is Off.
    fn lensed_to_screen(&self, viewport: Rect, p: Point) -> Pos2 {
        let dx = (p.x - self.pan.x) as f32 * self.scale;
        let dy = (p.y - self.pan.y) as f32 * self.scale;
        viewport.min + Vec2::new(dx, dy)
    }

    /// The affine inverse: map a screen position to *lensed-world*.
    fn screen_to_lensed(&self, viewport: Rect, pos: Pos2) -> Point {
        let off = pos - viewport.min;
        Point::new(
            f64::from(off.x / self.scale) + self.pan.x,
            f64::from(off.y / self.scale) + self.pan.y,
        )
    }

    /// Map a canvas-space point to screen pixels within `viewport`.
    ///
    /// Under [`ProjectionKind::Poincare`] the disk is LOCKED to the viewport (see
    /// [`poincare_disk`]): the point's unit-disk coordinate `z = lens.disk(p)`
    /// (forward-normalised + `nav`) is placed directly as
    /// `disk_center + vec2(z.re, z.im) * disk_radius`, with NO affine `pan`/`scale`
    /// — so the disk can't drift or rescale as the user navigates (navigation is
    /// the Möbius `nav`). Otherwise (Affine/Fisheye) it applies the projection lens
    /// (`world → lensed-world`) then the affine pan/zoom; under Affine the lens is
    /// the identity, so this is exactly the historical `pan + (p − pan) * scale`.
    /// [proj-canvas-mode]
    #[must_use]
    pub fn world_to_screen(&self, viewport: Rect, p: Point) -> Pos2 {
        if self.projection.kind == ProjectionKind::Poincare {
            let (center, radius) = poincare_disk(viewport, self.poincare_zoom);
            let z = self.lens().disk_point(p);
            return center + Vec2::new(z.re, z.im) * radius;
        }
        self.lensed_to_screen(viewport, self.lens().world_to_lensed(p))
    }

    /// Map a screen-pixel position within `viewport` back to canvas space.
    ///
    /// Under [`ProjectionKind::Poincare`] this inverts the LOCKED disk map:
    /// `z = (pos − disk_center) / disk_radius` (clamped inside the disk), undo the
    /// `nav` recentre, the projection inverse, and the focus/scale framing — the
    /// exact inverse of [`Camera::world_to_screen`]'s Poincaré branch, with no
    /// affine `pan`/`scale`. Otherwise (Affine/Fisheye) it inverts the affine then
    /// the lens (closed-form); under Affine this is exactly today's inverse.
    /// [proj-canvas-mode]
    #[must_use]
    pub fn screen_to_world(&self, viewport: Rect, pos: Pos2) -> Point {
        if self.projection.kind == ProjectionKind::Poincare {
            let (center, radius) = poincare_disk(viewport, self.poincare_zoom);
            let rel = (pos - center) / radius.max(f32::EPSILON);
            let z = clamp_inside_disk(Complex::new(rel.x, rel.y), DEFAULT_BOUNDARY_RADIUS);
            // Undo nav, then the projection inverse, then the focus/scale framing —
            // mirroring `Lens::lensed_to_world` but starting from a disk point.
            let z = self.nav.invert().apply(z);
            let world_rel = inverse(z, self.projection);
            return Point::new(
                self.lens_focus.x + f64::from(world_rel.re) * self.lens_scale,
                self.lens_focus.y + f64::from(world_rel.im) * self.lens_scale,
            );
        }
        self.lens().lensed_to_world(self.screen_to_lensed(viewport, pos))
    }

    /// Map an *already-lensed* world point to screen via the affine half ONLY —
    /// no second lens pass. The paint layer samples a geodesic/bulge in disk or
    /// world space, maps each sample back to lensed-world (e.g. via
    /// [`Camera::disk_world`]), and routes it through this to avoid the
    /// double-lensing that the full [`Camera::world_to_screen`] would introduce.
    /// Under Affine this is identical to `world_to_screen`. [proj-canvas-mode]
    #[must_use]
    pub fn lensed_world_to_screen(&self, viewport: Rect, lensed: Point) -> Pos2 {
        self.lensed_to_screen(viewport, lensed)
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
        // Pin against the affine (lensed-world) layer: pan/zoom always operate on
        // the affine half so they stay linear regardless of the lens. The
        // lensed-world point under the cursor stays fixed on screen.
        let anchor = self.screen_to_lensed(viewport, cursor);
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
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
        assert!(cam.scale() <= super::MAX_SCALE + 1e-3);
        for _ in 0..400 {
            cam.zoom_to_cursor(vp, vp.center(), 0.5);
        }
        assert!(cam.scale() >= super::MIN_SCALE - 1e-4);
    }

    #[test]
    fn set_pan_scale_restores_and_clamps() {
        let mut cam = Camera::default();
        cam.set_pan_scale(Point::new(-120.5, 33.0), 0.5);
        assert_point_near(cam.pan(), Point::new(-120.5, 33.0));
        assert!((cam.scale() - 0.5).abs() < 1e-6);
        // Out-of-range scales clamp to the same bounds the gestures use.
        cam.set_pan_scale(Point::new(0.0, 0.0), 1000.0);
        assert!((cam.scale() - super::MAX_SCALE).abs() < 1e-6);
        cam.set_pan_scale(Point::new(0.0, 0.0), 0.00001);
        assert!((cam.scale() - super::MIN_SCALE).abs() < 1e-6);
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
    fn affine_default_is_byte_identical_to_pre_lens_transform() {
        // With the default (Affine) projection the composed transform must equal
        // the historical `pan + (p − pan) * scale` exactly — the lens is identity.
        let mut cam = Camera::default();
        cam.set_pan_scale(Point::new(-30.0, 17.0), 0.8);
        cam.update_lens(Some(CanvasRect::new(-100.0, -100.0, 200.0, 200.0)));
        let vp = viewport();
        for p in [Point::new(0.0, 0.0), Point::new(123.5, -42.0), Point::new(-900.0, 700.0)] {
            let screen = cam.world_to_screen(vp, p);
            let dx = (p.x - cam.pan().x) as f32 * cam.scale();
            let dy = (p.y - cam.pan().y) as f32 * cam.scale();
            let expected = vp.min + Vec2::new(dx, dy);
            assert!((screen.x - expected.x).abs() < 1e-4 && (screen.y - expected.y).abs() < 1e-4);
            // And the round-trip is exact.
            assert_point_near(cam.screen_to_world(vp, screen), p);
        }
        // Off: card scale is exactly 1.0 and the magnification is 1.0.
        assert!((cam.card_scale_at(Point::new(50.0, 50.0)) - 1.0).abs() < 1e-6);
        assert!((cam.magnification_at(Point::new(50.0, 50.0)) - 1.0).abs() < 1e-6);
        assert!(!cam.lens_active());
    }

    #[test]
    fn active_lens_round_trips_and_warps_card_scale() {
        // An active fisheye lens: world↔screen still round-trips (closed-form
        // inverse), the focus magnifies (card_scale near max), and a far point
        // shrinks (card_scale toward min) — all while staying axis-aligned (the
        // paint layer's concern; here we only check the scalar factors).
        let mut cam = Camera::default();
        cam.set_projection(hiker_projection::ProjectionConfig {
            kind: hiker_projection::ProjectionKind::Fisheye,
            strength: 1.0,
            size_falloff: 1.0,
            geodesic_segments: 16,
        });
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        cam.update_lens(Some(bounds));
        let vp = viewport();
        assert!(cam.lens_active());
        // Round-trip a handful of points through the composed lens+affine.
        for p in [Point::new(0.0, 0.0), Point::new(120.0, -80.0), Point::new(-300.0, 250.0)] {
            let back = cam.screen_to_world(vp, cam.world_to_screen(vp, p));
            assert!((back.x - p.x).abs() < 1.0 && (back.y - p.y).abs() < 1.0, "{p:?} -> {back:?}");
        }
        // Focus (bounds center) magnifies the most; a rim point magnifies less.
        let center = bounds.center();
        let rim = Point::new(bounds.right(), bounds.bottom());
        let mag_center = cam.card_scale_at(center);
        let mag_rim = cam.card_scale_at(rim);
        assert!(mag_center > mag_rim, "center {mag_center} should exceed rim {mag_rim}");
        // Both stay within the clamp.
        let clamp = cam.card_scale_clamp();
        assert!(mag_rim >= clamp.min - 1e-6 && mag_center <= clamp.max + 1e-6);
    }

    #[test]
    fn nav_recenters_poincare_and_round_trips() {
        // A non-identity nav under Poincaré moves the disk centre: the world point
        // that now maps to the disk origin is the one whose pre-nav disk point the
        // nav sends to the origin. world↔screen still round-trips (the lens inverse
        // undoes nav), and the disk is LOCKED to the viewport, so the disk centre
        // is the viewport centre (not the affine focus).
        use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};
        let mut cam = Camera::default();
        cam.set_projection(ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.2,
            size_falloff: 1.0,
            geodesic_segments: 16,
        });
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        cam.update_lens(Some(bounds));
        let vp = viewport();

        // Pick a peripheral world card, recentre the disk on it.
        let card = Point::new(bounds.right(), bounds.bottom());
        let target = cam.disk_point(card);
        cam.set_nav(Mobius::from_point_pair(target, Complex::ORIGIN));

        // That card now sits at the disk centre → at the LOCKED disk centre, which
        // is the viewport centre (the disk no longer rides the affine focus).
        let (center, _) = cam.poincare_disk_frame(vp);
        let card_screen = cam.world_to_screen(vp, card);
        assert!(
            (card_screen.x - center.x).abs() < 1.0 && (card_screen.y - center.y).abs() < 1.0,
            "recentred card lands at the disk centre: {card_screen:?} vs {center:?}"
        );
        // Round-trip survives the nav.
        for p in [Point::new(0.0, 0.0), Point::new(120.0, -80.0), card] {
            let back = cam.screen_to_world(vp, cam.world_to_screen(vp, p));
            assert!((back.x - p.x).abs() < 1.0 && (back.y - p.y).abs() < 1.0, "{p:?} -> {back:?}");
        }
    }

    #[test]
    fn nav_is_ignored_under_off_and_fisheye() {
        // The nav transform is Poincaré-only: under Off (Affine) and Fisheye the
        // composed transform must be byte-identical with or without a non-identity
        // nav set — those modes never apply it.
        use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        let vp = viewport();
        let probes = [Point::new(0.0, 0.0), Point::new(120.0, -80.0), Point::new(-300.0, 250.0)];
        let wild = Mobius::from_point_pair(Complex::new(0.4, -0.3), Complex::ORIGIN);
        for kind in [ProjectionKind::Affine, ProjectionKind::Fisheye] {
            let mut base = Camera::default();
            base.set_projection(ProjectionConfig { kind, strength: 1.2, size_falloff: 1.0, geodesic_segments: 16 });
            base.update_lens(Some(bounds));
            let mut navd = base;
            navd.set_nav(wild);
            for p in probes {
                let a = base.world_to_screen(vp, p);
                let b = navd.world_to_screen(vp, p);
                assert!(
                    (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6,
                    "{kind:?}: nav must not affect {p:?} ({a:?} vs {b:?})"
                );
            }
        }
    }

    /// A small helper to build a Poincaré camera framed to `bounds`.
    fn poincare_cam(bounds: CanvasRect, strength: f32) -> Camera {
        use hiker_projection::{ProjectionConfig, ProjectionKind};
        let mut cam = Camera::default();
        cam.set_projection(ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength,
            size_falloff: 1.0,
            geodesic_segments: 16,
        });
        cam.update_lens(Some(bounds));
        cam
    }

    #[test]
    fn poincare_disk_is_viewport_locked() {
        // The locked disk frame is a pure function of the viewport: centre =
        // viewport centre, radius = 0.5 * min-dim * DISK_FILL. And it is invariant
        // to `pan`/`scale` — two Poincaré cameras with wildly different pan/zoom map
        // the focus to the SAME (viewport-centre) screen point, proving the affine
        // view can't move the disk.
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        for vp in [
            viewport(),
            Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 400.0)),
            Rect::from_min_size(Pos2::new(-30.0, 12.0), Vec2::new(1024.0, 300.0)),
        ] {
            let mut cam = poincare_cam(bounds, 1.0);
            let (center, radius) = cam.poincare_disk_frame(vp);
            assert!((center.x - vp.center().x).abs() < 1e-4 && (center.y - vp.center().y).abs() < 1e-4);
            let expected = 0.5 * vp.size().min_elem() * super::DISK_FILL;
            assert!((radius - expected).abs() < 1e-4, "radius {radius} vs {expected}");

            // The focus (centroid) lands at the disk centre regardless of pan/zoom.
            cam.set_pan_scale(Point::new(-9000.0, 4000.0), 0.05);
            let a = cam.world_to_screen(vp, cam.lens_focus());
            cam.set_pan_scale(Point::new(7777.0, -2222.0), 20.0);
            let b = cam.world_to_screen(vp, cam.lens_focus());
            assert!(
                (a.x - center.x).abs() < 1e-3 && (a.y - center.y).abs() < 1e-3,
                "focus must land at disk centre under pan A: {a:?} vs {center:?}"
            );
            assert!(
                (b.x - center.x).abs() < 1e-3 && (b.y - center.y).abs() < 1e-3,
                "focus must land at disk centre under pan B: {b:?} vs {center:?}"
            );
            // And the disk frame itself never moved with pan/scale.
            let (center2, radius2) = cam.poincare_disk_frame(vp);
            assert!((center2.x - center.x).abs() < 1e-6 && (radius2 - radius).abs() < 1e-6);
        }
    }

    #[test]
    fn poincare_origin_to_center_rim_to_radius() {
        // The lens focus (the layout centroid) maps to the disk centre; a world
        // point out at the layout extent maps near the rim (|screen − centre| ≈
        // radius, within the tanh falloff).
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        let cam = poincare_cam(bounds, 3.0);
        let vp = viewport();
        let (center, radius) = cam.poincare_disk_frame(vp);

        let focus_screen = cam.world_to_screen(vp, cam.lens_focus());
        assert!(
            (focus_screen.x - center.x).abs() < 1e-3 && (focus_screen.y - center.y).abs() < 1e-3,
            "focus maps to disk centre: {focus_screen:?} vs {center:?}"
        );

        // A point far past the bounds: tanh saturates, so it sits very near the rim.
        let far = Point::new(50_000.0, 50_000.0);
        let far_screen = cam.world_to_screen(vp, far);
        let dist = (far_screen - center).length();
        assert!(dist <= radius + 1e-3, "rim point inside the disk: {dist} <= {radius}");
        assert!(dist > radius * 0.9, "rim point near the boundary: {dist} vs {radius}");
    }

    #[test]
    fn poincare_screen_world_roundtrips() {
        // For interior points the locked-disk screen↔world map round-trips: the
        // inverse undoes (disk-frame, nav, projection, focus/scale framing) exactly.
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        let cam = poincare_cam(bounds, 1.2);
        let vp = viewport();
        for p in [Point::new(0.0, 0.0), Point::new(120.0, -80.0), Point::new(-300.0, 250.0)] {
            let back = cam.screen_to_world(vp, cam.world_to_screen(vp, p));
            assert!((back.x - p.x).abs() < 1.0 && (back.y - p.y).abs() < 1.0, "{p:?} -> {back:?}");
        }
    }

    #[test]
    fn mobius_nav_moves_content_not_disk() {
        // Setting a non-identity nav moves a point's screen position (the content
        // pans within the disk) but leaves the disk frame (centre + radius) from
        // `poincare_disk_frame` unchanged — the disk is the viewport, not the
        // content.
        use hiker_projection::{Complex, Mobius};
        let bounds = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        let mut cam = poincare_cam(bounds, 1.2);
        let vp = viewport();
        let (center0, radius0) = cam.poincare_disk_frame(vp);

        let probe = Point::new(120.0, -80.0);
        let before = cam.world_to_screen(vp, probe);

        // Recentre the disk on a peripheral card — the content shifts.
        let card = Point::new(bounds.right(), bounds.bottom());
        let target = cam.disk_point(card);
        cam.set_nav(Mobius::from_point_pair(target, Complex::ORIGIN));
        let after = cam.world_to_screen(vp, probe);
        assert!(
            (after - before).length() > 1.0,
            "nav must move the probe's screen position: {before:?} -> {after:?}"
        );

        // The disk frame itself is unchanged by nav.
        let (center1, radius1) = cam.poincare_disk_frame(vp);
        assert!((center1.x - center0.x).abs() < 1e-6 && (center1.y - center0.y).abs() < 1e-6);
        assert!((radius1 - radius0).abs() < 1e-6);
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
