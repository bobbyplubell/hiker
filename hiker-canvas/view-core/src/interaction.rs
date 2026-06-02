//! Pointer-driven editing: hit-testing, selection, move/group-move, resize,
//! marquee, edge draw/redirect, delete, and pan/zoom input. The [`crate::view`]
//! `show` loop calls these helpers; each returns committed [`EditOp`]s (already
//! applied to the canvas for responsiveness) or mutates view state.
//
// status: canvas-node-move
// status: canvas-group-move
// status: canvas-edge-draw
// status: canvas-edge-redirect
// status: canvas-delete

use emath::{Pos2, Rect};
use hiker_canvas::geometry::{hit_test, node_bounds, Point, Rect as CanvasRect};
use hiker_canvas::model::{Canvas, NodeKind, Side};
use hiker_canvas::ops::{EditOp, Endpoint};

use crate::camera::Camera;
use crate::edges::{anchor_pos, build_geometry, resolve_sides, side_normal, EdgeGeometry};
use crate::handles::{hit_handle, Handle};
use crate::state::{HoverAnchor, Selection};

/// Distance (screen px) within which a pointer hits an edge curve / endpoint.
const EDGE_HIT_TOL: f32 = 6.0;
/// Radius (screen px) of the four side handles that appear on a hovered node.
pub const SIDE_HANDLE_R: f32 = 6.0;
/// How far outside a node's edge the connector handles float (screen px), so
/// they don't collide with the on-rect resize handles. Paint (`paint.rs`) and
/// hit-test ([`side_handle_pos`]) share this offset. status: canvas-edge-draw
pub const CONNECTOR_OFFSET: f32 = 11.0;
/// Height (canvas units) of a group's grabbable header strip along its top edge,
/// where the label sits. A press in this band targets the group (so it can be
/// selected / moved); a press in the body falls through to the framed children.
/// status: canvas-group-grab
pub const GROUP_HEADER_H: f64 = 28.0;

/// The result of resolving what the pointer is over, in priority order:
/// resize handle > side handle > node > edge > empty.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A resize handle of the single selected node.
    ResizeHandle(Handle),
    /// A side handle on a hovered node (start of an edge draw).
    SideHandle {
        /// The node id.
        node: String,
        /// The side.
        side: Side,
    },
    /// A node body, by id.
    Node(String),
    /// An edge curve, by id.
    Edge(String),
    /// Empty canvas.
    Empty,
}

/// Resolve the topmost target under `pointer` (screen space).
#[must_use]
pub fn resolve_target(
    canvas: &Canvas,
    camera: &Camera,
    viewport: Rect,
    selection: &Selection,
    pointer: Pos2,
) -> Target {
    if let Some(h) = single_selected_handle(canvas, camera, viewport, selection, pointer) {
        return Target::ResizeHandle(h);
    }
    if let Some((node, side)) = hovered_side_handle(canvas, camera, viewport, pointer) {
        return Target::SideHandle { node, side };
    }
    let world = camera.screen_to_world(viewport, pointer);
    // A press on a group's header strip targets the group (select + move) before
    // the normal top-most hit-test grabs a framed child. status: canvas-group-grab
    if let Some(id) = group_header_hit(canvas, world) {
        return Target::Node(id);
    }
    if let Some(idx) = hit_test(canvas, world) {
        return Target::Node(canvas.nodes[idx].id.clone());
    }
    if let Some(id) = hit_edge(canvas, camera, viewport, pointer) {
        return Target::Edge(id);
    }
    Target::Empty
}

/// If exactly one node is selected, the resize handle (if any) under `pointer`.
/// Public so the hover-affordance painter (`canvas-view`) can grow whichever
/// handle the press path would grab, reusing this exact hit-test rather than a
/// parallel one. status: canvas-handle-hover
#[must_use]
pub fn single_selected_handle(
    canvas: &Canvas,
    camera: &Camera,
    viewport: Rect,
    selection: &Selection,
    pointer: Pos2,
) -> Option<Handle> {
    if selection.nodes.len() != 1 || !selection.edges.is_empty() {
        return None;
    }
    let id = selection.nodes.iter().next()?;
    let node = canvas.nodes.iter().find(|n| &n.id == id)?;
    let screen = camera.world_rect_to_screen(viewport, node_bounds(node));
    hit_handle(screen, pointer)
}

/// The id of the group whose header strip (the [`GROUP_HEADER_H`]-tall band along
/// its top edge) contains `world`, top-most group wins. Returns `None` when no
/// group header is under the point, so a press in a group body falls through to
/// the framed child. status: canvas-group-grab
#[must_use]
pub fn group_header_hit(canvas: &Canvas, world: Point) -> Option<String> {
    canvas
        .nodes
        .iter()
        .rev()
        .find(|node| {
            matches!(node.kind, NodeKind::Group { .. })
                && world.x >= node.x as f64
                && world.x <= (node.x + node.width) as f64
                && world.y >= node.y as f64
                && world.y <= node.y as f64 + GROUP_HEADER_H
        })
        .map(|node| node.id.clone())
}

/// The four connector handles appear on a node the pointer is near; return
/// whichever the pointer is on. Checks every node, topmost first.
#[must_use]
pub fn hovered_side_handle(canvas: &Canvas, camera: &Camera, viewport: Rect, pointer: Pos2) -> Option<(String, Side)> {
    for node in canvas.nodes.iter().rev() {
        if matches!(node.kind, NodeKind::Group { .. }) {
            continue;
        }
        let screen = camera.world_rect_to_screen(viewport, node_bounds(node));
        // Widen the cheap bounding-box reject to reach the offset handles.
        if !screen.expand(CONNECTOR_OFFSET + SIDE_HANDLE_R * 2.0).contains(pointer) {
            continue;
        }
        for side in [Side::Top, Side::Right, Side::Bottom, Side::Left] {
            let h = side_handle_pos(node, side, camera, viewport);
            if (h.pos - pointer).length() <= SIDE_HANDLE_R + 2.0 {
                return Some((node.id.clone(), side));
            }
        }
    }
    None
}

/// The screen position of a node's connector handle: its edge anchor pushed
/// [`CONNECTOR_OFFSET`] outward along the side normal, so the clickable handle
/// floats just outside the card (clear of the on-rect resize handles). The edge
/// itself still anchors at the on-edge point.
#[must_use]
pub fn side_handle_pos(
    node: &hiker_canvas::model::Node,
    side: Side,
    camera: &Camera,
    viewport: Rect,
) -> HoverAnchor {
    let a = anchor_pos(node, side);
    let base = camera.world_to_screen(viewport, Point::new(f64::from(a.x), f64::from(a.y)));
    let pos = base + side_normal(side) * CONNECTOR_OFFSET;
    HoverAnchor { pos, side }
}

/// The on-screen center of a node's connector handle on `side`, given the node's
/// screen rect: the midpoint of that edge pushed [`CONNECTOR_OFFSET`] outward
/// along the side normal. The painter (`canvas-view`) draws the circle here; it
/// must match [`side_handle_pos`] so the visible handle is what gets hit-tested.
/// status: canvas-edge-draw
#[must_use]
pub fn connector_handle_center(screen: Rect, side: Side) -> Pos2 {
    let c = screen.center();
    let base = match side {
        Side::Top => Pos2::new(c.x, screen.top()),
        Side::Right => Pos2::new(screen.right(), c.y),
        Side::Bottom => Pos2::new(c.x, screen.bottom()),
        Side::Left => Pos2::new(screen.left(), c.y),
    };
    base + side_normal(side) * CONNECTOR_OFFSET
}

/// The screen-space midpoint of an edge's straight chord (for placing the
/// inline label editor). `None` if the edge or either endpoint is gone.
/// status: canvas-edge-label
#[must_use]
pub fn edge_midpoint_screen(canvas: &Canvas, camera: &Camera, viewport: Rect, edge_id: &str) -> Option<Pos2> {
    let edge = canvas.edges.iter().find(|e| e.id == edge_id)?;
    let from = canvas.nodes.iter().find(|n| n.id == edge.from_node)?;
    let to = canvas.nodes.iter().find(|n| n.id == edge.to_node)?;
    let (fs, ts) = resolve_sides(edge, from, to);
    let a = world_anchor(camera, viewport, anchor_pos(from, fs));
    let b = world_anchor(camera, viewport, anchor_pos(to, ts));
    Some(a + (b - a) * 0.5)
}

/// The id of the edge whose curve passes within tolerance of `pointer`, if any.
/// Samples the *same* cubic-Bézier geometry the painter draws (not the straight
/// chord), so a bowed connector is clickable along its visible path.
fn hit_edge(canvas: &Canvas, camera: &Camera, viewport: Rect, pointer: Pos2) -> Option<String> {
    for edge in canvas.edges.iter().rev() {
        let Some(from) = canvas.nodes.iter().find(|n| n.id == edge.from_node) else { continue };
        let Some(to) = canvas.nodes.iter().find(|n| n.id == edge.to_node) else { continue };
        let (fs, ts) = resolve_sides(edge, from, to);
        let a = world_anchor(camera, viewport, anchor_pos(from, fs));
        let b = world_anchor(camera, viewport, anchor_pos(to, ts));
        let handle = (a - b).length().clamp(40.0, 320.0) * 0.4;
        let geo = build_geometry(a, b, fs, ts, handle);
        if bezier_distance(pointer, &geo) <= EDGE_HIT_TOL {
            return Some(edge.id.clone());
        }
    }
    None
}

/// Minimum distance from `p` to an edge's cubic-Bézier curve, by sampling the
/// curve into short chords. Matches the painter's geometry (`paint::one_edge`).
fn bezier_distance(p: Pos2, geo: &EdgeGeometry) -> f32 {
    const SAMPLES: usize = 18;
    let mut prev = geo.start;
    let mut best = f32::INFINITY;
    for i in 1..=SAMPLES {
        let t = i as f32 / SAMPLES as f32;
        let cur = cubic_point(geo.start, geo.ctrl_a, geo.ctrl_b, geo.end, t);
        best = best.min(dist_point_segment(p, prev, cur));
        prev = cur;
    }
    best
}

/// Evaluate a cubic Bézier at parameter `t`.
fn cubic_point(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let mt = 1.0 - t;
    let (a, b, c, d) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
    Pos2::new(
        a * p0.x + b * p1.x + c * p2.x + d * p3.x,
        a * p0.y + b * p1.y + c * p2.y + d * p3.y,
    )
}

fn world_anchor(camera: &Camera, viewport: Rect, p: Pos2) -> Pos2 {
    camera.world_to_screen(viewport, Point::new(f64::from(p.x), f64::from(p.y)))
}

/// Perpendicular distance from `p` to segment `a`–`b`.
fn dist_point_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    (p - proj).length()
}

/// Apply a click to the selection: select the clicked target, extending with
/// shift, clearing on empty. Returns nothing — selection is pure view state.
pub fn click_select(selection: &mut Selection, target: &Target, shift: bool) {
    if !shift {
        selection.clear();
    }
    match target {
        Target::Node(id) => toggle(&mut selection.nodes, id, shift),
        Target::Edge(id) => toggle(&mut selection.edges, id, shift),
        _ => {}
    }
}

fn toggle(set: &mut std::collections::HashSet<String>, id: &str, shift: bool) {
    if shift && set.contains(id) {
        set.remove(id);
    } else {
        set.insert(id.to_owned());
    }
}

/// Translate exactly the `ids` by a whole-unit canvas delta. The caller passes
/// the frozen move-set (the selection, which already includes any selected
/// group's members — folded in at select-time). Returns the move op (already
/// applied).
#[must_use]
pub fn move_selection(canvas: &mut Canvas, ids: Vec<String>, dx: i64, dy: i64) -> Option<EditOp> {
    if (dx == 0 && dy == 0) || ids.is_empty() {
        return None;
    }
    let op = EditOp::MoveNodes { ids, dx, dy };
    op.apply(canvas);
    Some(op)
}

/// The ids of the nodes that geometrically belong to the group `group_id`: every
/// node whose [`node_bounds`] sit entirely inside the group's rect, excluding the
/// group itself. Used at select-time to fold a group's members into the selection
/// so membership is visible (they paint as selected) and the move-set is decided
/// once, not re-derived by containment mid-drag. Returns an empty vec when the id
/// isn't a group (or isn't found).
#[must_use]
pub fn group_member_ids(canvas: &Canvas, group_id: &str) -> Vec<String> {
    let Some(group) = canvas.nodes.iter().find(|n| n.id == group_id) else {
        return Vec::new();
    };
    if !matches!(group.kind, NodeKind::Group { .. }) {
        return Vec::new();
    }
    let rect = node_bounds(group);
    canvas
        .nodes
        .iter()
        .filter(|member| member.id != group_id && rect_contains(rect, node_bounds(member)))
        .map(|member| member.id.clone())
        .collect()
}

/// Whether `inner` sits entirely within `outer`.
fn rect_contains(outer: CanvasRect, inner: CanvasRect) -> bool {
    inner.x >= outer.x && inner.y >= outer.y && inner.right() <= outer.right() && inner.bottom() <= outer.bottom()
}

/// Rewrite a node's rect for a resize drag from its original rect, given the
/// grabbed `handle` and the current pointer in canvas space. Enforces a minimum
/// size. Returns the op (already applied).
#[must_use]
pub fn resize_node(canvas: &mut Canvas, id: &str, handle: Handle, orig: (i64, i64, i64, i64), pointer: Point) -> Option<EditOp> {
    const MIN: i64 = 20;
    let (ox, oy, ow, oh) = orig;
    let (mut x, mut y, mut right, mut bottom) = (ox, oy, ox + ow, oy + oh);
    let px = pointer.x.round() as i64;
    let py = pointer.y.round() as i64;
    if handle.moves_left() {
        x = px.min(right - MIN);
    }
    if handle.moves_right() {
        right = px.max(x + MIN);
    }
    if handle.moves_top() {
        y = py.min(bottom - MIN);
    }
    if handle.moves_bottom() {
        bottom = py.max(y + MIN);
    }
    let op = EditOp::SetNodeRect { id: id.to_owned(), x, y, width: right - x, height: bottom - y };
    op.apply(canvas);
    Some(op)
}

/// Select every node whose bounds intersect the marquee rect (canvas space).
pub fn marquee_select(canvas: &Canvas, selection: &mut Selection, rect: CanvasRect, additive: bool) {
    if !additive {
        selection.clear();
    }
    for node in &canvas.nodes {
        if rects_intersect(rect, node_bounds(node)) {
            selection.nodes.insert(node.id.clone());
        }
    }
}

fn rects_intersect(a: CanvasRect, b: CanvasRect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// Delete the current selection (nodes cascade their edges; selected edges drop
/// directly). Returns the ops applied, in apply order.
#[must_use]
pub fn delete_selection(canvas: &mut Canvas, selection: &Selection) -> Vec<EditOp> {
    let mut ops = Vec::new();
    if !selection.nodes.is_empty() {
        let ids: Vec<String> = selection.nodes.iter().cloned().collect();
        let op = EditOp::RemoveNodes { ids };
        op.apply(canvas);
        ops.push(op);
    }
    for id in &selection.edges {
        if canvas.edges.iter().any(|e| &e.id == id) {
            let op = EditOp::RemoveEdge { id: id.clone() };
            op.apply(canvas);
            ops.push(op);
        }
    }
    ops
}

/// Commit an edge draw/redirect when the pointer is released over `target`.
/// A fresh draw adds an edge; a redirect re-anchors the moving endpoint. Drop on
/// empty (no node target) cancels and returns `None`.
#[must_use]
pub fn finish_edge_drag(
    canvas: &mut Canvas,
    from_node: &str,
    from_side: Side,
    endpoint: Endpoint,
    existing: Option<&str>,
    target: &Target,
    new_id: &str,
) -> Option<EditOp> {
    let (to_node, to_side) = match target {
        Target::Node(id) => (id.clone(), None),
        Target::SideHandle { node, side } => (node.clone(), Some(*side)),
        _ => return None,
    };
    if to_node == from_node {
        return None;
    }
    let op = match existing {
        Some(edge_id) => EditOp::SetEdgeEndpoint {
            id: edge_id.to_owned(),
            endpoint,
            node: to_node,
            side: to_side,
        },
        None => EditOp::AddEdge {
            edge: Box::new(new_edge(new_id, from_node, from_side, &to_node, to_side)),
        },
    };
    op.apply(canvas);
    Some(op)
}

fn new_edge(id: &str, from: &str, from_side: Side, to: &str, to_side: Option<Side>) -> hiker_canvas::model::Edge {
    hiker_canvas::model::Edge {
        id: id.to_owned(),
        from_node: from.to_owned(),
        from_side: Some(from_side),
        from_end: None,
        to_node: to.to_owned(),
        to_side,
        to_end: None,
        color: None,
        label: None,
        extra: std::collections::BTreeMap::new(),
    }
}

/// Which endpoint of an existing edge `pointer` is nearest, for redirect. Picks
/// whichever anchor is closer; returns the side it currently uses too.
#[must_use]
pub fn nearest_endpoint(
    canvas: &Canvas,
    camera: &Camera,
    viewport: Rect,
    edge_id: &str,
    pointer: Pos2,
) -> Option<(Endpoint, String, Side)> {
    let edge = canvas.edges.iter().find(|e| e.id == edge_id)?;
    let from = canvas.nodes.iter().find(|n| n.id == edge.from_node)?;
    let to = canvas.nodes.iter().find(|n| n.id == edge.to_node)?;
    let (fs, ts) = resolve_sides(edge, from, to);
    let a = world_anchor(camera, viewport, anchor_pos(from, fs));
    let b = world_anchor(camera, viewport, anchor_pos(to, ts));
    if (pointer - a).length() <= (pointer - b).length() {
        Some((Endpoint::From, edge.to_node.clone(), ts))
    } else {
        Some((Endpoint::To, edge.from_node.clone(), fs))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delete_selection, dist_point_segment, finish_edge_drag, group_header_hit, group_member_ids,
        marquee_select, move_selection, resize_node, single_selected_handle, Target,
    };
    use crate::camera::Camera;
    use crate::handles::Handle;
    use crate::state::Selection;
    use emath::Pos2;
    use hiker_canvas::geometry::{Point, Rect as CanvasRect};
    use hiker_canvas::model::{Canvas, Edge, Node, NodeKind, Side};
    use hiker_canvas::ops::Endpoint;
    use std::collections::BTreeMap;

    fn node(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node { id: id.to_owned(), x, y, width: w, height: h, color: None, kind: NodeKind::Text { text: String::new() }, extra: BTreeMap::new() }
    }

    fn group(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node { kind: NodeKind::Group { label: None, background: None, background_style: None }, ..node(id, x, y, w, h) }
    }

    #[test]
    fn group_member_ids_returns_contained_nodes_only() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("g", 0, 0, 400, 400));
        canvas.nodes.push(node("inside", 50, 50, 100, 100));
        canvas.nodes.push(node("outside", 500, 500, 100, 100));
        let members = group_member_ids(&canvas, "g");
        assert!(members.contains(&"inside".to_string()));
        assert!(!members.contains(&"outside".to_string()));
        // The group itself is excluded from its own membership.
        assert!(!members.contains(&"g".to_string()));
    }

    #[test]
    fn group_member_ids_is_empty_for_non_group_or_missing() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("n", 0, 0, 100, 100));
        assert!(group_member_ids(&canvas, "n").is_empty(), "a plain node has no members");
        assert!(group_member_ids(&canvas, "ghost").is_empty(), "a missing id has no members");
    }

    #[test]
    fn moving_a_frozen_set_does_not_touch_unselected_nodes() {
        // The move-set is the (frozen) selection only. A node that is NOT in the
        // set must never move even if a moved group's rect slides over it —
        // membership is decided at select-time, not by mid-drag containment.
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("a", 0, 0, 200, 200));
        canvas.nodes.push(node("m", 20, 20, 50, 50));
        canvas.nodes.push(node("b_card", 620, 20, 50, 50));
        // The frozen set: group A and its member m. b_card is excluded.
        let ids = vec!["a".to_string(), "m".to_string()];
        let before = canvas.nodes.iter().find(|n| n.id == "b_card").map(|n| (n.x, n.y));
        let _ = move_selection(&mut canvas, ids, 600, 0);
        let after = canvas.nodes.iter().find(|n| n.id == "b_card").map(|n| (n.x, n.y));
        assert_eq!(before, after, "an unselected card must not be carried along");
        assert_eq!(canvas.nodes.iter().find(|n| n.id == "a").map(|n| n.x), Some(600));
        assert_eq!(canvas.nodes.iter().find(|n| n.id == "m").map(|n| n.x), Some(620));
    }

    #[test]
    fn group_header_hit_targets_header_not_body() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("g", 0, 0, 400, 300));
        // A point in the top header band hits the group.
        assert_eq!(group_header_hit(&canvas, Point::new(200.0, 10.0)).as_deref(), Some("g"));
        // A point in the body (below the header) falls through.
        assert!(group_header_hit(&canvas, Point::new(200.0, 150.0)).is_none());
        // A point outside the group entirely is no hit.
        assert!(group_header_hit(&canvas, Point::new(500.0, 10.0)).is_none());
    }

    #[test]
    fn group_header_hit_topmost_group_wins() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("under", 0, 0, 400, 300));
        canvas.nodes.push(group("over", 0, 0, 400, 300));
        assert_eq!(group_header_hit(&canvas, Point::new(50.0, 5.0)).as_deref(), Some("over"));
    }

    #[test]
    fn single_selected_group_gets_resize_handles() {
        // Regression: groups are no longer excluded from resize handles.
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("g", 100, 100, 200, 100));
        let mut sel = Selection::default();
        sel.nodes.insert("g".into());
        let camera = Camera::default();
        let viewport = emath::Rect::from_min_size(Pos2::new(0.0, 0.0), emath::vec2(800.0, 600.0));
        // The top-left corner of the group screen rect lands on a handle.
        let handle = single_selected_handle(&canvas, &camera, viewport, &sel, Pos2::new(100.0, 100.0));
        assert_eq!(handle, Some(Handle::TopLeft));
    }

    #[test]
    fn resize_from_bottom_right_grows_size() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("n", 0, 0, 100, 100));
        let op = resize_node(&mut canvas, "n", Handle::BottomRight, (0, 0, 100, 100), Point::new(250.0, 180.0));
        assert!(op.is_some());
        let n = &canvas.nodes[0];
        assert_eq!((n.x, n.y, n.width, n.height), (0, 0, 250, 180));
    }

    #[test]
    fn resize_from_top_left_moves_origin() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("n", 100, 100, 100, 100));
        let _ = resize_node(&mut canvas, "n", Handle::TopLeft, (100, 100, 100, 100), Point::new(60.0, 70.0));
        let n = &canvas.nodes[0];
        assert_eq!((n.x, n.y), (60, 70));
        assert_eq!((n.width, n.height), (140, 130));
    }

    #[test]
    fn marquee_selects_intersecting_nodes() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("a", 0, 0, 50, 50));
        canvas.nodes.push(node("b", 1000, 1000, 50, 50));
        let mut sel = Selection::default();
        marquee_select(&canvas, &mut sel, CanvasRect::new(-10.0, -10.0, 100.0, 100.0), false);
        assert!(sel.has_node("a") && !sel.has_node("b"));
    }

    #[test]
    fn delete_selection_cascades_edges() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("a", 0, 0, 50, 50));
        canvas.nodes.push(node("b", 100, 0, 50, 50));
        canvas.edges.push(Edge {
            id: "e".into(), from_node: "a".into(), from_side: None, from_end: None,
            to_node: "b".into(), to_side: None, to_end: None, color: None, label: None, extra: BTreeMap::new(),
        });
        let mut sel = Selection::default();
        sel.nodes.insert("a".into());
        let ops = delete_selection(&mut canvas, &sel);
        assert_eq!(ops.len(), 1);
        assert!(canvas.edges.is_empty(), "incident edge cascaded");
    }

    #[test]
    fn finish_edge_drag_creates_and_cancels() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(node("a", 0, 0, 50, 50));
        canvas.nodes.push(node("b", 200, 0, 50, 50));
        // Drop on empty cancels.
        assert!(finish_edge_drag(&mut canvas, "a", Side::Right, Endpoint::To, None, &Target::Empty, "e1").is_none());
        // Drop on a node creates an edge.
        let op = finish_edge_drag(&mut canvas, "a", Side::Right, Endpoint::To, None, &Target::Node("b".into()), "e1");
        assert!(op.is_some());
        assert_eq!(canvas.edges.len(), 1);
        assert_eq!(canvas.edges[0].from_node, "a");
    }

    #[test]
    fn point_segment_distance() {
        let d = dist_point_segment(Pos2::new(5.0, 5.0), Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0));
        assert!((d - 5.0).abs() < 1e-4);
    }

    #[test]
    fn connector_handle_paint_matches_hit_test() {
        use crate::camera::Camera;
        use hiker_canvas::geometry::node_bounds;
        // The painted handle center and the hit-tested handle center
        // (side_handle_pos) must coincide, or the visible circle wouldn't be
        // clickable. Both push the edge anchor outward by CONNECTOR_OFFSET.
        let n = node("n", 10, 20, 100, 60);
        let camera = Camera::default();
        let viewport = emath::Rect::from_min_size(Pos2::new(0.0, 0.0), emath::vec2(500.0, 500.0));
        let screen = camera.world_rect_to_screen(viewport, node_bounds(&n));
        for side in [Side::Top, Side::Right, Side::Bottom, Side::Left] {
            let hit = super::side_handle_pos(&n, side, &camera, viewport).pos;
            let painted = super::connector_handle_center(screen, side);
            assert!((hit - painted).length() < 1e-3, "{side:?}: {hit:?} vs {painted:?}");
        }
    }
}
