//! Edge routing: facing-side selection, cubic-Bézier connector geometry, and
//! arrowhead construction. The pure geometry (no egui painter) lives here and is
//! unit-tested; [`crate::paint`] turns it into shapes.
//
// status: canvas-edge-routing

use emath::{Pos2, Vec2};
use hiker_canvas::geometry::{node_anchor, Rect as CanvasRect};
use hiker_canvas::model::{Edge, Node, Side};

/// The on-screen control polygon of an edge: the four cubic-Bézier points
/// (start, two controls, end) plus the start/end sides, all in screen pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeGeometry {
    /// Start point (the `from` anchor).
    pub start: Pos2,
    /// First control point, pushed out along the `from` side normal.
    pub ctrl_a: Pos2,
    /// Second control point, pushed out along the `to` side normal.
    pub ctrl_b: Pos2,
    /// End point (the `to` anchor).
    pub end: Pos2,
    /// Resolved `from` side.
    pub from_side: Side,
    /// Resolved `to` side.
    pub to_side: Side,
}

/// Pick the side of `a` that faces `b`, comparing their centers. Whichever axis
/// has the larger separation decides between left/right and top/bottom.
#[must_use]
pub fn facing_side(a: CanvasRect, b: CanvasRect) -> Side {
    let ca = a.center();
    let cb = b.center();
    let dx = cb.x - ca.x;
    let dy = cb.y - ca.y;
    if dx.abs() >= dy.abs() {
        if dx >= 0.0 { Side::Right } else { Side::Left }
    } else if dy >= 0.0 {
        Side::Bottom
    } else {
        Side::Top
    }
}

/// The outward unit normal of a side, in screen space (y grows downward).
#[must_use]
pub const fn side_normal(side: Side) -> Vec2 {
    match side {
        Side::Top => Vec2::new(0.0, -1.0),
        Side::Right => Vec2::new(1.0, 0.0),
        Side::Bottom => Vec2::new(0.0, 1.0),
        Side::Left => Vec2::new(-1.0, 0.0),
    }
}

/// Resolve an edge's sides, falling back to the facing side when unspecified.
#[must_use]
pub fn resolve_sides(edge: &Edge, from: &Node, to: &Node) -> (Side, Side) {
    let fb = node_bounds_rect(from);
    let tb = node_bounds_rect(to);
    let from_side = edge.from_side.unwrap_or_else(|| facing_side(fb, tb));
    let to_side = edge.to_side.unwrap_or_else(|| facing_side(tb, fb));
    (from_side, to_side)
}

/// Build the screen-space cubic-Bézier geometry for an edge between two anchor
/// points. `world_to_screen` converts a canvas anchor to pixels; `handle` is the
/// control-point pushout in pixels (typically a function of node distance).
#[must_use]
pub fn build_geometry(
    start: Pos2,
    end: Pos2,
    from_side: Side,
    to_side: Side,
    handle: f32,
) -> EdgeGeometry {
    let ctrl_a = start + side_normal(from_side) * handle;
    let ctrl_b = end + side_normal(to_side) * handle;
    EdgeGeometry { start, ctrl_a, ctrl_b, end, from_side, to_side }
}

/// The three points of a filled arrowhead at `tip`, opening back along
/// `direction` (a unit vector pointing toward the tip). `len` is the barb
/// length, `half` the half-width at the base.
#[must_use]
pub fn arrowhead(tip: Pos2, direction: Vec2, len: f32, half: f32) -> [Pos2; 3] {
    let dir = if direction.length() > f32::EPSILON {
        direction.normalized()
    } else {
        Vec2::new(1.0, 0.0)
    };
    let base = tip - dir * len;
    let perp = Vec2::new(-dir.y, dir.x);
    [tip, base + perp * half, base - perp * half]
}

/// The canvas anchor point on a node's resolved side, as a pure `Pos2` helper.
#[must_use]
pub const fn anchor_pos(node: &Node, side: Side) -> Pos2 {
    let p = node_anchor(node, side);
    Pos2::new(p.x as f32, p.y as f32)
}

const fn node_bounds_rect(node: &Node) -> CanvasRect {
    CanvasRect::new(node.x as f64, node.y as f64, node.width as f64, node.height as f64)
}

#[cfg(test)]
mod tests {
    use super::{arrowhead, facing_side, resolve_sides};
    use emath::{Pos2, Vec2};
    use hiker_canvas::geometry::Rect as CanvasRect;
    use hiker_canvas::model::{Edge, Node, NodeKind, Side};
    use std::collections::BTreeMap;

    fn node(id: &str, x: i64, y: i64) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: 100,
            height: 100,
            color: None,
            kind: NodeKind::Text { text: String::new() },
            extra: BTreeMap::new(),
        }
    }

    fn edge_no_sides() -> Edge {
        Edge {
            id: "e".to_owned(),
            from_node: "a".to_owned(),
            from_side: None,
            from_end: None,
            to_node: "b".to_owned(),
            to_side: None,
            to_end: None,
            color: None,
            label: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn facing_side_picks_dominant_axis() {
        let a = CanvasRect::new(0.0, 0.0, 100.0, 100.0);
        let right = CanvasRect::new(400.0, 10.0, 100.0, 100.0);
        assert_eq!(facing_side(a, right), Side::Right);
        assert_eq!(facing_side(right, a), Side::Left);

        let below = CanvasRect::new(10.0, 400.0, 100.0, 100.0);
        assert_eq!(facing_side(a, below), Side::Bottom);
        assert_eq!(facing_side(below, a), Side::Top);
    }

    #[test]
    fn resolve_sides_uses_facing_when_unspecified_and_honors_explicit() {
        let a = node("a", 0, 0);
        let b = node("b", 400, 0);
        let (from, to) = resolve_sides(&edge_no_sides(), &a, &b);
        assert_eq!((from, to), (Side::Right, Side::Left));

        let mut explicit = edge_no_sides();
        explicit.from_side = Some(Side::Top);
        explicit.to_side = Some(Side::Bottom);
        let (from, to) = resolve_sides(&explicit, &a, &b);
        assert_eq!((from, to), (Side::Top, Side::Bottom));
    }

    #[test]
    fn arrowhead_points_back_from_tip() {
        // Pointing right: the two barb points sit left of the tip, symmetric
        // about the x-axis.
        let tri = arrowhead(Pos2::new(100.0, 50.0), Vec2::new(1.0, 0.0), 10.0, 4.0);
        assert_eq!(tri[0], Pos2::new(100.0, 50.0));
        assert!((tri[1].x - 90.0).abs() < 1e-4 && (tri[2].x - 90.0).abs() < 1e-4);
        // The two barb points straddle the axis symmetrically (one at +half,
        // one at -half), 8px apart on y.
        assert!(((tri[1].y - tri[2].y).abs() - 8.0).abs() < 1e-4);
        assert!((tri[1].y + tri[2].y - 100.0).abs() < 1e-4);
    }

    #[test]
    fn arrowhead_handles_zero_direction() {
        let tri = arrowhead(Pos2::new(0.0, 0.0), Vec2::ZERO, 10.0, 4.0);
        // Falls back to pointing right; no NaNs.
        assert!(tri.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
    }
}
