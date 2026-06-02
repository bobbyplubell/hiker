//! Egui-free geometry over the canvas model: lightweight [`Point`] / [`Rect`]
//! types (no egui dependency), node bounds, point-in-node hit testing that
//! returns the top-most node (later array index = higher z-order), content
//! bounds over all nodes for zoom-to-fit, and per-side edge anchor points.
//
// status: canvas-geometry

use crate::model::{Canvas, Node, Side};

/// A point in the infinite canvas coordinate space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

impl Point {
    /// Construct a point.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle: top-left corner plus size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge X.
    pub x: f64,
    /// Top edge Y.
    pub y: f64,
    /// Width (non-negative).
    pub width: f64,
    /// Height (non-negative).
    pub height: f64,
}

impl Rect {
    /// Construct a rectangle from a corner and size.
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }

    /// Right edge X.
    #[must_use]
    pub const fn right(&self) -> f64 {
        self.x + self.width
    }

    /// Bottom edge Y.
    #[must_use]
    pub const fn bottom(&self) -> f64 {
        self.y + self.height
    }

    /// The center point.
    #[must_use]
    pub const fn center(&self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Whether the point lies within the rectangle (edges inclusive).
    #[must_use]
    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    /// The anchor point on the given side (the midpoint of that edge).
    #[must_use]
    pub const fn anchor(&self, side: Side) -> Point {
        match side {
            Side::Top => Point::new(self.x + self.width / 2.0, self.y),
            Side::Right => Point::new(self.right(), self.y + self.height / 2.0),
            Side::Bottom => Point::new(self.x + self.width / 2.0, self.bottom()),
            Side::Left => Point::new(self.x, self.y + self.height / 2.0),
        }
    }

    /// The smallest rectangle covering both inputs.
    #[must_use]
    pub fn union(&self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self::new(x, y, right - x, bottom - y)
    }
}

/// The bounds of a single node.
#[must_use]
pub const fn node_bounds(node: &Node) -> Rect {
    Rect::new(node.x as f64, node.y as f64, node.width as f64, node.height as f64)
}

/// The anchor point on a node's side that an edge attaches to.
#[must_use]
pub const fn node_anchor(node: &Node, side: Side) -> Point {
    node_bounds(node).anchor(side)
}

/// The index of the top-most node containing `p`, or `None` if `p` hits no
/// node. Later array index wins (higher z-order paints on top).
#[must_use]
pub fn hit_test(canvas: &Canvas, p: Point) -> Option<usize> {
    canvas
        .nodes
        .iter()
        .enumerate()
        .rev()
        .find(|(_, node)| node_bounds(node).contains(p))
        .map(|(index, _)| index)
}

/// The bounding rectangle of all nodes, for zoom-to-fit. `None` when the canvas
/// has no nodes.
#[must_use]
pub fn content_bounds(canvas: &Canvas) -> Option<Rect> {
    let mut nodes = canvas.nodes.iter();
    let first = node_bounds(nodes.next()?);
    Some(nodes.fold(first, |acc, node| acc.union(node_bounds(node))))
}

#[cfg(test)]
mod tests {
    use super::{content_bounds, hit_test, node_anchor, Point, Rect};
    use crate::model::{Canvas, Node, NodeKind, Side};

    fn text_node(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: w,
            height: h,
            color: None,
            kind: NodeKind::Text { text: String::new() },
            extra: Default::default(),
        }
    }

    #[test]
    fn hit_test_returns_topmost_overlapping_node() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(text_node("under", 0, 0, 100, 100));
        canvas.nodes.push(text_node("over", 50, 50, 100, 100));
        // Point in the overlap region: later node wins.
        assert_eq!(hit_test(&canvas, Point::new(60.0, 60.0)), Some(1));
        // Point only in the first node.
        assert_eq!(hit_test(&canvas, Point::new(10.0, 10.0)), Some(0));
        // Point outside both.
        assert_eq!(hit_test(&canvas, Point::new(500.0, 500.0)), None);
    }

    #[test]
    fn content_bounds_covers_all_nodes() {
        let mut canvas = Canvas::default();
        assert_eq!(content_bounds(&canvas), None);
        canvas.nodes.push(text_node("a", 0, 0, 100, 100));
        canvas.nodes.push(text_node("b", 200, 300, 50, 50));
        let bounds = content_bounds(&canvas).unwrap();
        assert_eq!(bounds, Rect::new(0.0, 0.0, 250.0, 350.0));
    }

    #[test]
    fn anchors_sit_at_edge_midpoints() {
        let node = text_node("a", 0, 0, 100, 200);
        assert_eq!(node_anchor(&node, Side::Top), Point::new(50.0, 0.0));
        assert_eq!(node_anchor(&node, Side::Right), Point::new(100.0, 100.0));
        assert_eq!(node_anchor(&node, Side::Bottom), Point::new(50.0, 200.0));
        assert_eq!(node_anchor(&node, Side::Left), Point::new(0.0, 100.0));
    }
}
