//! Eight-handle resize geometry over a node's screen rect. Pure screen-space
//! math shared by the painter (drawing the handles) and the interaction layer
//! (hit-testing a drag onto one and rewriting the node rect). The handle a drag
//! grabs determines which edges move: a `Left`/`Top` handle moves the node's
//! origin as well as its size.
//
// status: canvas-node-resize

use emath::{Pos2, Rect, Vec2};

/// Side length (screen px) of a square resize handle.
pub const HANDLE_SIZE: f32 = 8.0;

/// How much a handle (resize square or connector circle) grows when the pointer
/// hovers it, so the grab target reads before pressing — a gentle affordance,
/// not a jarring jump. Shared by the resize-handle painter and the connector
/// painter for a consistent feel. status: canvas-handle-hover
pub const HOVER_GROW: f32 = 1.3;

/// Grow `rect` about its own center by `factor` (so the enlarged handle stays
/// centered on the same point). The base case `factor == 1.0` returns `rect`
/// unchanged. Pure screen-space math shared by the painter's hover affordance.
/// status: canvas-handle-hover
#[must_use]
pub fn grown_about_center(rect: Rect, factor: f32) -> Rect {
    Rect::from_center_size(rect.center(), rect.size() * factor)
}

/// Which of the eight resize handles a drag grabbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    /// Top-left corner.
    TopLeft,
    /// Top edge midpoint.
    Top,
    /// Top-right corner.
    TopRight,
    /// Right edge midpoint.
    Right,
    /// Bottom-right corner.
    BottomRight,
    /// Bottom edge midpoint.
    Bottom,
    /// Bottom-left corner.
    BottomLeft,
    /// Left edge midpoint.
    Left,
}

/// All eight handles in a stable order matching [`handle_rects`].
pub const ALL_HANDLES: [Handle; 8] = [
    Handle::TopLeft,
    Handle::Top,
    Handle::TopRight,
    Handle::Right,
    Handle::BottomRight,
    Handle::Bottom,
    Handle::BottomLeft,
    Handle::Left,
];

impl Handle {
    /// The center of this handle on `rect` (screen space).
    #[must_use]
    pub fn center(self, rect: Rect) -> Pos2 {
        let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
        let (cx, cy) = (rect.center().x, rect.center().y);
        match self {
            Self::TopLeft => Pos2::new(l, t),
            Self::Top => Pos2::new(cx, t),
            Self::TopRight => Pos2::new(r, t),
            Self::Right => Pos2::new(r, cy),
            Self::BottomRight => Pos2::new(r, b),
            Self::Bottom => Pos2::new(cx, b),
            Self::BottomLeft => Pos2::new(l, b),
            Self::Left => Pos2::new(l, cy),
        }
    }

    /// Whether this handle drives the left edge (so dragging moves `x`).
    #[must_use]
    pub const fn moves_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::Left | Self::BottomLeft)
    }

    /// Whether this handle drives the right edge.
    #[must_use]
    pub const fn moves_right(self) -> bool {
        matches!(self, Self::TopRight | Self::Right | Self::BottomRight)
    }

    /// Whether this handle drives the top edge (so dragging moves `y`).
    #[must_use]
    pub const fn moves_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::Top | Self::TopRight)
    }

    /// Whether this handle drives the bottom edge.
    #[must_use]
    pub const fn moves_bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::Bottom | Self::BottomRight)
    }
}

/// The screen rects of all eight handles around `rect`, in [`ALL_HANDLES`] order.
#[must_use]
pub fn handle_rects(rect: Rect) -> [Rect; 8] {
    let half = Vec2::splat(HANDLE_SIZE / 2.0);
    ALL_HANDLES.map(|h| Rect::from_center_size(h.center(rect), half * 2.0))
}

/// The handle whose rect contains `pos`, or `None`. A slightly enlarged hit area
/// makes the small handles easier to grab.
#[must_use]
pub fn hit_handle(rect: Rect, pos: Pos2) -> Option<Handle> {
    ALL_HANDLES
        .into_iter()
        .find(|h| Rect::from_center_size(h.center(rect), Vec2::splat(HANDLE_SIZE + 4.0)).contains(pos))
}

#[cfg(test)]
mod tests {
    use super::{grown_about_center, handle_rects, hit_handle, Handle, HOVER_GROW};
    use emath::{Pos2, Rect};

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::new(100.0, 100.0), emath::vec2(200.0, 100.0))
    }

    #[test]
    fn eight_distinct_handles() {
        let rects = handle_rects(rect());
        assert_eq!(rects.len(), 8);
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert_ne!(a.center(), b.center());
            }
        }
    }

    #[test]
    fn corner_handles_hit_test() {
        let r = rect();
        assert_eq!(hit_handle(r, Pos2::new(100.0, 100.0)), Some(Handle::TopLeft));
        assert_eq!(hit_handle(r, Pos2::new(300.0, 200.0)), Some(Handle::BottomRight));
        assert_eq!(hit_handle(r, r.center()), None);
    }

    #[test]
    fn hover_grows_handle_about_its_center() {
        // A hovered handle grows (larger area) but stays centered on the same
        // point, so the affordance reads as "this exact square" rather than
        // shifting the grab target.
        let base = Rect::from_center_size(Pos2::new(50.0, 60.0), emath::vec2(8.0, 8.0));
        let grown = grown_about_center(base, HOVER_GROW);
        assert_eq!(grown.center(), base.center(), "stays centered");
        assert!(grown.area() > base.area(), "grows");
        assert!((grown.width() - base.width() * HOVER_GROW).abs() < 1e-4);
        // The base case (no hover) is identity.
        assert_eq!(grown_about_center(base, 1.0), base);
    }

    #[test]
    fn edge_membership_flags() {
        assert!(Handle::TopLeft.moves_left() && Handle::TopLeft.moves_top());
        assert!(Handle::BottomRight.moves_right() && Handle::BottomRight.moves_bottom());
        assert!(!Handle::Top.moves_left() && !Handle::Top.moves_right());
    }
}
