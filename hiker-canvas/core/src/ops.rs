//! Pure, egui-free edit operations over a [`Canvas`]. Each verb is an
//! [`EditOp`] variant with [`EditOp::apply`] (mutate in place) and
//! [`EditOp::invert`] (capture prior state, returning the op that undoes it),
//! so a future undo stack just keeps a list of inverses. `remove_nodes`
//! cascades to incident edges, and its inverse restores both the nodes and
//! the dropped edges at their original positions. No verb reads or writes any
//! referenced note.
//
// status: canvas-edit-ops

use crate::color::Color;
use crate::model::{Canvas, Edge, Node, NodeKind, Side};

/// A removed node together with the document index it occupied, so an undo can
/// reinsert it where it was.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovedNode {
    /// The original index of the node in `canvas.nodes`.
    pub index: usize,
    /// The removed node.
    pub node: Node,
}

/// A removed edge together with the document index it occupied.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovedEdge {
    /// The original index of the edge in `canvas.edges`.
    pub index: usize,
    /// The removed edge.
    pub edge: Edge,
}

/// Which endpoint of an edge a redirect targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The `from` endpoint.
    From,
    /// The `to` endpoint.
    To,
}

/// An invertible edit. Apply mutates the canvas; invert returns the op that
/// undoes it (capturing whatever prior state the inverse needs).
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Translate a set of nodes (by id) by a delta.
    MoveNodes {
        /// Node ids to translate.
        ids: Vec<String>,
        /// X delta.
        dx: i64,
        /// Y delta.
        dy: i64,
    },
    /// Set a node's absolute width and height.
    ResizeNode {
        /// Target node id.
        id: String,
        /// New width.
        width: i64,
        /// New height.
        height: i64,
    },
    /// Set a node's absolute top-left position and size in one op (used as the
    /// inverse of a resize that also moved the top-left corner).
    SetNodeRect {
        /// Target node id.
        id: String,
        /// New X.
        x: i64,
        /// New Y.
        y: i64,
        /// New width.
        width: i64,
        /// New height.
        height: i64,
    },
    /// Append a node to the document.
    AddNode {
        /// The node to append.
        node: Box<Node>,
    },
    /// Remove nodes by id, cascading their incident edges.
    RemoveNodes {
        /// Node ids to remove.
        ids: Vec<String>,
    },
    /// Reinsert previously removed nodes and edges at their original indices
    /// (the inverse of `RemoveNodes` and `RemoveEdge`).
    RestoreRemoved {
        /// Nodes to reinsert, ascending by index.
        nodes: Vec<RemovedNode>,
        /// Edges to reinsert, ascending by index.
        edges: Vec<RemovedEdge>,
    },
    /// Append an edge to the document.
    AddEdge {
        /// The edge to append.
        edge: Box<Edge>,
    },
    /// Remove a single edge by id.
    RemoveEdge {
        /// Target edge id.
        id: String,
    },
    /// Re-anchor one endpoint of an edge to a different node and/or side.
    SetEdgeEndpoint {
        /// Target edge id.
        id: String,
        /// Which endpoint to move.
        endpoint: Endpoint,
        /// New target node id.
        node: String,
        /// New side anchor (or `None` to clear it).
        side: Option<Side>,
    },
    /// Replace a text node's body.
    SetText {
        /// Target node id.
        id: String,
        /// New markdown body.
        text: String,
    },
    /// Set (or clear) a node's color.
    SetColor {
        /// Target node id.
        id: String,
        /// New color, or `None` to clear it.
        color: Option<Color>,
    },
    /// Set (or clear) a group node's label.
    SetLabel {
        /// Target group node id.
        id: String,
        /// New label, or `None` to clear it.
        label: Option<String>,
    },
    /// Set (or clear) an edge's label.
    SetEdgeLabel {
        /// Target edge id.
        id: String,
        /// New label, or `None` to clear it.
        label: Option<String>,
    },
}

impl EditOp {
    /// Apply the edit in place.
    pub fn apply(&self, canvas: &mut Canvas) {
        match self {
            Self::MoveNodes { ids, dx, dy } => move_nodes(canvas, ids, *dx, *dy),
            Self::ResizeNode { id, width, height } => resize_node(canvas, id, *width, *height),
            Self::SetNodeRect { id, x, y, width, height } => {
                set_node_rect(canvas, id, *x, *y, *width, *height);
            }
            Self::AddNode { node } => canvas.nodes.push((**node).clone()),
            Self::RemoveNodes { ids } => {
                remove_nodes(canvas, ids);
            }
            Self::RestoreRemoved { nodes, edges } => restore_removed(canvas, nodes, edges),
            Self::AddEdge { edge } => canvas.edges.push((**edge).clone()),
            Self::RemoveEdge { id } => {
                remove_edge(canvas, id);
            }
            Self::SetEdgeEndpoint { id, endpoint, node, side } => {
                set_edge_endpoint(canvas, id, *endpoint, node, *side);
            }
            Self::SetText { id, text } => set_text(canvas, id, text),
            Self::SetColor { id, color } => set_color(canvas, id, color.clone()),
            Self::SetLabel { id, label } => set_label(canvas, id, label.clone()),
            Self::SetEdgeLabel { id, label } => set_edge_label(canvas, id, label.clone()),
        }
    }

    /// Return the op that undoes this one against the CURRENT `canvas` state
    /// (call before `apply`). Captures prior values where needed.
    #[must_use]
    pub fn invert(&self, canvas: &Canvas) -> Self {
        match self {
            Self::MoveNodes { ids, dx, dy } => Self::MoveNodes { ids: ids.clone(), dx: -dx, dy: -dy },
            Self::ResizeNode { id, .. } => invert_resize(canvas, id),
            Self::SetNodeRect { id, .. } => invert_resize(canvas, id),
            Self::AddNode { node } => Self::RemoveNodes { ids: vec![node.id.clone()] },
            Self::RemoveNodes { ids } => invert_remove_nodes(canvas, ids),
            Self::RestoreRemoved { nodes, edges } => invert_restore(nodes, edges),
            Self::AddEdge { edge } => Self::RemoveEdge { id: edge.id.clone() },
            Self::RemoveEdge { id } => invert_remove_edge(canvas, id),
            Self::SetEdgeEndpoint { id, endpoint, .. } => invert_set_endpoint(canvas, id, *endpoint),
            Self::SetText { id, .. } => Self::SetText { id: id.clone(), text: prior_text(canvas, id) },
            Self::SetColor { id, .. } => Self::SetColor { id: id.clone(), color: prior_color(canvas, id) },
            Self::SetLabel { id, .. } => Self::SetLabel { id: id.clone(), label: prior_label(canvas, id) },
            Self::SetEdgeLabel { id, .. } => {
                Self::SetEdgeLabel { id: id.clone(), label: prior_edge_label(canvas, id) }
            }
        }
    }
}

fn node_mut<'a>(canvas: &'a mut Canvas, id: &str) -> Option<&'a mut Node> {
    canvas.nodes.iter_mut().find(|n| n.id == id)
}

fn node_ref<'a>(canvas: &'a Canvas, id: &str) -> Option<&'a Node> {
    canvas.nodes.iter().find(|n| n.id == id)
}

fn edge_mut<'a>(canvas: &'a mut Canvas, id: &str) -> Option<&'a mut Edge> {
    canvas.edges.iter_mut().find(|e| e.id == id)
}

fn edge_ref<'a>(canvas: &'a Canvas, id: &str) -> Option<&'a Edge> {
    canvas.edges.iter().find(|e| e.id == id)
}

fn move_nodes(canvas: &mut Canvas, ids: &[String], dx: i64, dy: i64) {
    for node in canvas.nodes.iter_mut().filter(|n| ids.contains(&n.id)) {
        node.x += dx;
        node.y += dy;
    }
}

fn resize_node(canvas: &mut Canvas, id: &str, width: i64, height: i64) {
    if let Some(node) = node_mut(canvas, id) {
        node.width = width;
        node.height = height;
    }
}

fn set_node_rect(canvas: &mut Canvas, id: &str, x: i64, y: i64, width: i64, height: i64) {
    if let Some(node) = node_mut(canvas, id) {
        node.x = x;
        node.y = y;
        node.width = width;
        node.height = height;
    }
}

/// Remove nodes by id and cascade their incident edges. Returns the removed
/// nodes and edges with their original indices (for undo).
fn remove_nodes(canvas: &mut Canvas, ids: &[String]) -> (Vec<RemovedNode>, Vec<RemovedEdge>) {
    let mut removed_nodes = Vec::new();
    let mut kept_nodes = Vec::with_capacity(canvas.nodes.len());
    for (index, node) in std::mem::take(&mut canvas.nodes).into_iter().enumerate() {
        if ids.contains(&node.id) {
            removed_nodes.push(RemovedNode { index, node });
        } else {
            kept_nodes.push(node);
        }
    }
    canvas.nodes = kept_nodes;

    let mut removed_edges = Vec::new();
    let mut kept_edges = Vec::with_capacity(canvas.edges.len());
    for (index, edge) in std::mem::take(&mut canvas.edges).into_iter().enumerate() {
        if ids.contains(&edge.from_node) || ids.contains(&edge.to_node) {
            removed_edges.push(RemovedEdge { index, edge });
        } else {
            kept_edges.push(edge);
        }
    }
    canvas.edges = kept_edges;

    (removed_nodes, removed_edges)
}

fn remove_edge(canvas: &mut Canvas, id: &str) -> Option<RemovedEdge> {
    let index = canvas.edges.iter().position(|e| e.id == id)?;
    let edge = canvas.edges.remove(index);
    Some(RemovedEdge { index, edge })
}

/// Reinsert removed nodes and edges at their original indices. Inputs must be
/// ascending by index so each insert lands correctly as the list grows back.
fn restore_removed(canvas: &mut Canvas, nodes: &[RemovedNode], edges: &[RemovedEdge]) {
    for removed in nodes {
        let at = removed.index.min(canvas.nodes.len());
        canvas.nodes.insert(at, removed.node.clone());
    }
    for removed in edges {
        let at = removed.index.min(canvas.edges.len());
        canvas.edges.insert(at, removed.edge.clone());
    }
}

fn set_edge_endpoint(canvas: &mut Canvas, id: &str, endpoint: Endpoint, node: &str, side: Option<Side>) {
    if let Some(edge) = edge_mut(canvas, id) {
        match endpoint {
            Endpoint::From => {
                edge.from_node = node.to_owned();
                edge.from_side = side;
            }
            Endpoint::To => {
                edge.to_node = node.to_owned();
                edge.to_side = side;
            }
        }
    }
}

fn set_text(canvas: &mut Canvas, id: &str, text: &str) {
    if let Some(node) = node_mut(canvas, id) {
        if let NodeKind::Text { text: body } = &mut node.kind {
            *body = text.to_owned();
        }
    }
}

fn set_color(canvas: &mut Canvas, id: &str, color: Option<Color>) {
    if let Some(node) = node_mut(canvas, id) {
        node.color = color;
    }
}

fn set_label(canvas: &mut Canvas, id: &str, label: Option<String>) {
    if let Some(node) = node_mut(canvas, id) {
        if let NodeKind::Group { label: group_label, .. } = &mut node.kind {
            *group_label = label;
        }
    }
}

fn set_edge_label(canvas: &mut Canvas, id: &str, label: Option<String>) {
    if let Some(edge) = edge_mut(canvas, id) {
        edge.label = label;
    }
}

fn invert_resize(canvas: &Canvas, id: &str) -> EditOp {
    let node = node_ref(canvas, id);
    let (x, y, width, height) = node.map_or((0, 0, 0, 0), |n| (n.x, n.y, n.width, n.height));
    EditOp::SetNodeRect { id: id.to_owned(), x, y, width, height }
}

fn invert_remove_nodes(canvas: &Canvas, ids: &[String]) -> EditOp {
    let mut nodes = Vec::new();
    for (index, node) in canvas.nodes.iter().enumerate() {
        if ids.contains(&node.id) {
            nodes.push(RemovedNode { index, node: node.clone() });
        }
    }
    let mut edges = Vec::new();
    for (index, edge) in canvas.edges.iter().enumerate() {
        if ids.contains(&edge.from_node) || ids.contains(&edge.to_node) {
            edges.push(RemovedEdge { index, edge: edge.clone() });
        }
    }
    EditOp::RestoreRemoved { nodes, edges }
}

/// Invert a restore. A node restore undoes via a cascading `RemoveNodes` (which
/// re-drops the same incident edges). An edge-only restore (the inverse of a
/// `RemoveEdge`) undoes by removing that edge directly.
fn invert_restore(nodes: &[RemovedNode], edges: &[RemovedEdge]) -> EditOp {
    if nodes.is_empty() {
        return edges.first().map_or_else(
            || EditOp::RemoveNodes { ids: Vec::new() },
            |first| EditOp::RemoveEdge { id: first.edge.id.clone() },
        );
    }
    EditOp::RemoveNodes { ids: nodes.iter().map(|r| r.node.id.clone()).collect() }
}

fn invert_remove_edge(canvas: &Canvas, id: &str) -> EditOp {
    edge_ref(canvas, id).map_or_else(
        || EditOp::RemoveEdge { id: id.to_owned() },
        |edge| {
            let index = canvas.edges.iter().position(|e| e.id == id).unwrap_or(canvas.edges.len());
            EditOp::RestoreRemoved {
                nodes: Vec::new(),
                edges: vec![RemovedEdge { index, edge: edge.clone() }],
            }
        },
    )
}

fn invert_set_endpoint(canvas: &Canvas, id: &str, endpoint: Endpoint) -> EditOp {
    let edge = edge_ref(canvas, id);
    let (node, side) = match endpoint {
        Endpoint::From => edge.map_or_else(
            || (String::new(), None),
            |e| (e.from_node.clone(), e.from_side),
        ),
        Endpoint::To => edge.map_or_else(
            || (String::new(), None),
            |e| (e.to_node.clone(), e.to_side),
        ),
    };
    EditOp::SetEdgeEndpoint { id: id.to_owned(), endpoint, node, side }
}

fn prior_text(canvas: &Canvas, id: &str) -> String {
    match node_ref(canvas, id).map(|n| &n.kind) {
        Some(NodeKind::Text { text }) => text.clone(),
        _ => String::new(),
    }
}

fn prior_color(canvas: &Canvas, id: &str) -> Option<Color> {
    node_ref(canvas, id).and_then(|n| n.color.clone())
}

fn prior_label(canvas: &Canvas, id: &str) -> Option<String> {
    match node_ref(canvas, id).map(|n| &n.kind) {
        Some(NodeKind::Group { label, .. }) => label.clone(),
        _ => None,
    }
}

fn prior_edge_label(canvas: &Canvas, id: &str) -> Option<String> {
    edge_ref(canvas, id).and_then(|e| e.label.clone())
}

#[cfg(test)]
mod tests {
    use super::{EditOp, Endpoint};
    use crate::color::Color;
    use crate::model::{Canvas, Edge, Node, NodeKind};

    fn text_node(id: &str) -> Node {
        Node {
            id: id.to_owned(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            color: None,
            kind: NodeKind::Text { text: String::new() },
            extra: Default::default(),
        }
    }

    fn edge(id: &str, from: &str, to: &str) -> Edge {
        Edge {
            id: id.to_owned(),
            from_node: from.to_owned(),
            from_side: None,
            from_end: None,
            to_node: to.to_owned(),
            to_side: None,
            to_end: None,
            color: None,
            label: None,
            extra: Default::default(),
        }
    }

    /// Apply an op, then its inverse, and assert we are back to the start.
    fn assert_round_trips(canvas: &Canvas, op: &EditOp) {
        let mut after = canvas.clone();
        let inverse = op.invert(canvas);
        op.apply(&mut after);
        inverse.apply(&mut after);
        assert_eq!(&after, canvas, "apply+invert did not round-trip for {op:?}");
    }

    fn sample() -> Canvas {
        let mut canvas = Canvas::default();
        canvas.nodes.push(text_node("a"));
        canvas.nodes.push(text_node("b"));
        canvas.edges.push(edge("e1", "a", "b"));
        canvas
    }

    #[test]
    fn move_invert_round_trips() {
        assert_round_trips(&sample(), &EditOp::MoveNodes { ids: vec!["a".into()], dx: 10, dy: -5 });
    }

    #[test]
    fn resize_invert_round_trips() {
        assert_round_trips(&sample(), &EditOp::ResizeNode { id: "a".into(), width: 250, height: 80 });
    }

    #[test]
    fn add_and_remove_node_round_trip() {
        assert_round_trips(&sample(), &EditOp::AddNode { node: Box::new(text_node("c")) });
        assert_round_trips(&sample(), &EditOp::RemoveNodes { ids: vec!["a".into()] });
    }

    #[test]
    fn set_text_color_label_round_trip() {
        let mut canvas = sample();
        canvas.nodes.push(Node {
            kind: NodeKind::Group { label: Some("g".into()), background: None, background_style: None },
            ..text_node("grp")
        });
        assert_round_trips(&canvas, &EditOp::SetText { id: "a".into(), text: "hello".into() });
        assert_round_trips(&canvas, &EditOp::SetColor { id: "a".into(), color: Some(Color::Preset(3)) });
        assert_round_trips(&canvas, &EditOp::SetLabel { id: "grp".into(), label: Some("renamed".into()) });
    }

    #[test]
    fn edge_ops_round_trip() {
        assert_round_trips(&sample(), &EditOp::AddEdge { edge: Box::new(edge("e2", "a", "b")) });
        assert_round_trips(
            &sample(),
            &EditOp::SetEdgeEndpoint {
                id: "e1".into(),
                endpoint: Endpoint::To,
                node: "a".into(),
                side: None,
            },
        );
    }

    #[test]
    fn set_edge_label_round_trips() {
        assert_round_trips(&sample(), &EditOp::SetEdgeLabel { id: "e1".into(), label: Some("depends on".into()) });
        // And clearing a present label round-trips too.
        let mut canvas = sample();
        canvas.edges[0].label = Some("old".into());
        assert_round_trips(&canvas, &EditOp::SetEdgeLabel { id: "e1".into(), label: None });
    }

    #[test]
    fn remove_node_cascades_incident_edges() {
        let mut canvas = sample();
        let op = EditOp::RemoveNodes { ids: vec!["a".into()] };
        op.apply(&mut canvas);
        assert_eq!(canvas.nodes.len(), 1);
        assert!(canvas.edges.is_empty(), "edge e1 incident to removed node 'a' must cascade");
    }

    #[test]
    fn remove_node_cascade_undo_restores_edges() {
        let original = sample();
        let mut canvas = original.clone();
        let op = EditOp::RemoveNodes { ids: vec!["a".into()] };
        let inverse = op.invert(&canvas);
        op.apply(&mut canvas);
        inverse.apply(&mut canvas);
        assert_eq!(canvas, original, "undo of a cascading delete must restore nodes AND edges");
    }
}
