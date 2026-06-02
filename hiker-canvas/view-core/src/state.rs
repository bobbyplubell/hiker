//! The view's mutable, non-document state: selection, the in-progress
//! interaction, the pending-create mode, and the in-session undo/redo stack.
//! None of this is serialized — it is view state distinct from the `Canvas`
//! document. Selection and camera survive a remote edit because they key off
//! stable node/edge ids, never text offsets.
//
// status: canvas-selection
// status: canvas-undo-redo
// status: canvas-node-create

use std::collections::HashSet;

use emath::Pos2;
use hiker_canvas::geometry::Point;
use hiker_canvas::model::{Canvas, Side};
use hiker_canvas::ops::{Endpoint, EditOp};

use crate::handles::Handle;

/// The active interaction tool. **Select** (the default) routes a left-drag by
/// what's under the cursor — empty canvas marquee-selects, a node moves, a
/// handle resizes. **Hand** routes every left-drag to a camera pan. The tool is
/// `CanvasView` view state — never serialized, never in the op-log, the same
/// posture as the camera. status: canvas-tool-mode
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Route a left-drag by what's under the cursor (the default).
    #[default]
    Select,
    /// Route every left-drag to a camera pan.
    Hand,
}

/// What a left-press should begin, decided purely from the active [`Tool`], the
/// universal pan modifiers (Space held / middle mouse button), and whether the
/// press landed on a node or on empty canvas. Pulled out of the egui event
/// handler so the routing table is unit-testable. status: canvas-tool-mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressAction {
    /// Pan the camera (Hand tool, Space held, or a middle-button drag).
    Pan,
    /// Marquee-select (Select tool on empty canvas).
    Marquee,
    /// Select / move the node (Select tool on a node) or fall through to the
    /// normal hit-test routing for handles / edges.
    Select,
}

/// Decide what a left-press begins. Holding Space or using the middle mouse
/// button pans regardless of tool; otherwise the Hand tool always pans, while
/// the Select tool marquees on empty canvas and selects/moves on a node.
/// status: canvas-tool-mode
#[must_use]
pub const fn press_action(tool: Tool, space_held: bool, middle_button: bool, on_node: bool) -> PressAction {
    if space_held || middle_button || matches!(tool, Tool::Hand) {
        return PressAction::Pan;
    }
    if on_node {
        PressAction::Select
    } else {
        PressAction::Marquee
    }
}

/// The kind of node a pending-create gesture will drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateKind {
    /// An empty markdown text node.
    Text,
    /// A file-embed node (host supplies the path).
    File,
    /// A link node (host supplies the URL).
    Link,
    /// A group node (drawn as a drag-rect).
    Group,
}

/// What the user is currently doing with the pointer. `Idle` between gestures.
#[derive(Debug, Default, Clone, PartialEq)]
pub enum Interaction {
    /// No active gesture.
    #[default]
    Idle,
    /// Panning the camera by dragging empty canvas.
    Pan,
    /// Moving the current selection; `accum` is the un-applied sub-pixel canvas
    /// delta carried between frames so integer node coords advance smoothly.
    MoveSelection {
        /// Canvas-space delta accumulated but not yet committed as whole units.
        accum: Point,
        /// Node ids to move, snapshotted at drag start (the selection plus any
        /// selected group's geometric members). Fixed for the whole gesture so
        /// a group dragged over another never re-scoops the other's members.
        /// status: canvas-group-move
        ids: Vec<String>,
    },
    /// Resizing a single node via `handle`; `node` is its id and `orig` its rect
    /// at drag start (x, y, w, h) so each frame rewrites from the original.
    Resize {
        /// Target node id.
        node: String,
        /// Grabbed handle.
        handle: Handle,
        /// Original (x, y, width, height) at drag start.
        orig: (i64, i64, i64, i64),
    },
    /// Marquee-selecting; `origin` is the press point in canvas space.
    Marquee {
        /// Press point (canvas space).
        origin: Point,
    },
    /// Drawing a new group's rectangle; `origin` is the press point (canvas
    /// space). The drag rubber-bands the group's bounds and release creates it.
    /// status: canvas-group-draw
    DrawGroup {
        /// Press point (canvas space).
        origin: Point,
    },
    /// Drawing or redirecting an edge. `from_node`/`from_side` anchor the fixed
    /// end; `existing` names the edge being redirected (else a fresh draw).
    EdgeDrag {
        /// The anchored node id.
        from_node: String,
        /// The anchored side.
        from_side: Side,
        /// Which endpoint moves when redirecting an existing edge.
        endpoint: Endpoint,
        /// The edge id being redirected, or `None` for a new draw.
        existing: Option<String>,
    },
    /// A click-to-connect gesture: an edge is being drawn from
    /// `from_node`/`from_side` and the *next* click on a node attaches it. A
    /// rubber band follows the cursor between the two clicks (vs. `EdgeDrag`,
    /// which connects on a press-drag-release). status: canvas-edge-draw
    Connecting {
        /// The node the connection started from.
        from_node: String,
        /// The side handle the connection started from.
        from_side: Side,
    },
}

/// The current selection, by stable id. A node and an edge are never both
/// selected at once in practice, but the sets are independent for flexibility.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    /// Selected node ids.
    pub nodes: HashSet<String>,
    /// Selected edge ids.
    pub edges: HashSet<String>,
}

impl Selection {
    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    /// Clear both sets.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
    }

    /// Whether `id` is a selected node.
    #[must_use]
    pub fn has_node(&self, id: &str) -> bool {
        self.nodes.contains(id)
    }

    /// Whether `id` is a selected edge.
    #[must_use]
    pub fn has_edge(&self, id: &str) -> bool {
        self.edges.contains(id)
    }
}

/// An in-session undo/redo stack of inverse [`EditOp`]s, distinct from op-log
/// history. Pushing a committed op clears the redo stack (a new edit forks the
/// timeline). Undo/redo return the op to apply, having already recorded its
/// counterpart on the opposite stack.
#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<EditOp>,
    redo: Vec<EditOp>,
}

impl UndoStack {
    /// Record that `op` was just applied to `canvas` (post-apply state), pushing
    /// its inverse for a future undo and clearing the redo branch.
    pub fn record(&mut self, op: &EditOp, canvas_before: &Canvas) {
        self.undo.push(op.invert(canvas_before));
        self.redo.clear();
    }

    /// Whether an undo is available.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a redo is available.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Pop the next undo op and, given the current `canvas` (pre-apply), record
    /// its inverse on the redo stack. The caller applies the returned op.
    pub fn take_undo(&mut self, canvas: &Canvas) -> Option<EditOp> {
        let op = self.undo.pop()?;
        self.redo.push(op.invert(canvas));
        Some(op)
    }

    /// Symmetric to [`UndoStack::take_undo`] for redo.
    pub fn take_redo(&mut self, canvas: &Canvas) -> Option<EditOp> {
        let op = self.redo.pop()?;
        self.undo.push(op.invert(canvas));
        Some(op)
    }
}

/// An in-progress inline edit of an edge's label, opened by double-clicking an
/// edge. `draft` is the live field text; committing emits an
/// [`EditOp::SetEdgeLabel`]. status: canvas-edge-label
#[derive(Debug, Clone, Default)]
pub struct LabelEdit {
    /// The edge whose label is being edited.
    pub edge_id: String,
    /// The current field text.
    pub draft: String,
}

/// A queued edge-redirect target resolved while hovering during an edge drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoverAnchor {
    /// The screen position of the hovered side handle.
    pub pos: Pos2,
    /// The side it represents.
    pub side: Side,
}

impl CreateKind {
    /// Build the [`EditOp::AddNode`] that drops a node of this kind at the world
    /// `at`, with id `id` and a sensible default size. Group/text/link/file all
    /// start empty; the host fills in path/url afterward via `set_*` ops or the
    /// content editor.
    #[must_use]
    pub fn add_op(self, id: String, at: Point) -> EditOp {
        use hiker_canvas::model::{Node, NodeKind};
        let (w, h) = self.default_size();
        let kind = match self {
            Self::Text => NodeKind::Text { text: String::new() },
            Self::File => NodeKind::File { file: String::new(), subpath: None },
            Self::Link => NodeKind::Link { url: String::new() },
            Self::Group => NodeKind::Group { label: None, background: None, background_style: None },
        };
        let node = Node {
            id,
            x: at.x.round() as i64,
            y: at.y.round() as i64,
            width: w,
            height: h,
            color: None,
            kind,
            extra: std::collections::BTreeMap::new(),
        };
        EditOp::AddNode { node: Box::new(node) }
    }

    /// Build the [`EditOp::AddNode`] for a group drawn between two corners in
    /// canvas space, normalizing a negative-direction drag and clamping to a
    /// small minimum size so a tiny drag still yields a usable frame.
    /// status: canvas-group-draw
    #[must_use]
    pub fn add_group_op(id: String, a: Point, b: Point) -> EditOp {
        use hiker_canvas::model::{Node, NodeKind};
        let rect = normalize_draw_rect(a, b);
        let node = Node {
            id,
            x: rect.x.round() as i64,
            y: rect.y.round() as i64,
            width: rect.width.round() as i64,
            height: rect.height.round() as i64,
            color: None,
            kind: NodeKind::Group { label: None, background: None, background_style: None },
            extra: std::collections::BTreeMap::new(),
        };
        EditOp::AddNode { node: Box::new(node) }
    }

    /// A default (width, height) for a freshly created node of this kind.
    #[must_use]
    pub const fn default_size(self) -> (i64, i64) {
        match self {
            Self::Text | Self::Link => (250, 120),
            Self::File => (300, 200),
            Self::Group => (400, 300),
        }
    }
}

/// Minimum width/height (canvas units) a drawn group is clamped to, so a stray
/// tiny drag still produces a usable frame. status: canvas-group-draw
const MIN_GROUP_SIZE: f64 = 80.0;

/// Normalize two drag corners into a top-left-anchored [`hiker_canvas::geometry::Rect`],
/// handling a negative-direction drag and enforcing [`MIN_GROUP_SIZE`].
/// status: canvas-group-draw
#[must_use]
pub fn normalize_draw_rect(a: Point, b: Point) -> hiker_canvas::geometry::Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let width = (a.x - b.x).abs().max(MIN_GROUP_SIZE);
    let height = (a.y - b.y).abs().max(MIN_GROUP_SIZE);
    hiker_canvas::geometry::Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::{press_action, normalize_draw_rect, CreateKind, PressAction, Selection, Tool, UndoStack};
    use hiker_canvas::geometry::Point as GeoPoint;
    use hiker_canvas::geometry::Point;
    use hiker_canvas::model::Canvas;
    use hiker_canvas::ops::EditOp;

    #[test]
    fn selection_empty_and_clear() {
        let mut sel = Selection::default();
        assert!(sel.is_empty());
        sel.nodes.insert("a".into());
        assert!(!sel.is_empty());
        sel.clear();
        assert!(sel.is_empty());
    }

    #[test]
    fn create_op_places_node_at_point() {
        let op = CreateKind::Text.add_op("n1".into(), Point::new(40.0, 60.0));
        let mut canvas = Canvas::default();
        op.apply(&mut canvas);
        assert_eq!(canvas.nodes.len(), 1);
        assert_eq!((canvas.nodes[0].x, canvas.nodes[0].y), (40, 60));
    }

    #[test]
    fn undo_redo_round_trips() {
        let mut canvas = Canvas::default();
        let mut stack = UndoStack::default();
        let op = CreateKind::Text.add_op("n1".into(), Point::new(0.0, 0.0));
        let before = canvas.clone();
        op.apply(&mut canvas);
        stack.record(&op, &before);
        assert!(stack.can_undo());

        let undo = stack.take_undo(&canvas).unwrap();
        undo.apply(&mut canvas);
        assert!(canvas.nodes.is_empty(), "undo removed the created node");
        assert!(stack.can_redo());

        let redo = stack.take_redo(&canvas).unwrap();
        redo.apply(&mut canvas);
        assert_eq!(canvas.nodes.len(), 1, "redo re-created the node");
    }

    #[test]
    fn press_action_routes_pan_marquee_and_select() {
        // Select tool: empty marquees, a node selects/moves.
        assert_eq!(press_action(Tool::Select, false, false, false), PressAction::Marquee);
        assert_eq!(press_action(Tool::Select, false, false, true), PressAction::Select);
        // Hand tool always pans, node or empty.
        assert_eq!(press_action(Tool::Hand, false, false, false), PressAction::Pan);
        assert_eq!(press_action(Tool::Hand, false, false, true), PressAction::Pan);
        // Space held pans regardless of tool / target.
        assert_eq!(press_action(Tool::Select, true, false, true), PressAction::Pan);
        assert_eq!(press_action(Tool::Select, true, false, false), PressAction::Pan);
        // Middle button pans regardless of tool / target.
        assert_eq!(press_action(Tool::Select, false, true, true), PressAction::Pan);
        assert_eq!(press_action(Tool::Hand, false, true, false), PressAction::Pan);
    }

    #[test]
    fn normalize_draw_rect_handles_negative_and_min_size() {
        // Negative-direction drag normalizes to a top-left-anchored rect.
        let r = normalize_draw_rect(GeoPoint::new(300.0, 200.0), GeoPoint::new(100.0, 50.0));
        assert_eq!((r.x, r.y), (100.0, 50.0));
        assert_eq!((r.width, r.height), (200.0, 150.0));
        // A tiny drag clamps to the minimum size.
        let r = normalize_draw_rect(GeoPoint::new(10.0, 10.0), GeoPoint::new(12.0, 9.0));
        assert!(r.width >= 80.0 && r.height >= 80.0);
        assert_eq!((r.x, r.y), (10.0, 9.0));
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut canvas = Canvas::default();
        let mut stack = UndoStack::default();
        let op = EditOp::AddNode { node: Box::new({
            let mut c = Canvas::default();
            CreateKind::Text.add_op("a".into(), Point::new(0.0, 0.0)).apply(&mut c);
            c.nodes.remove(0)
        }) };
        let before = canvas.clone();
        op.apply(&mut canvas);
        stack.record(&op, &before);
        let _ = stack.take_undo(&canvas);
        assert!(stack.can_redo());
        // A fresh edit forks the timeline: redo is dropped.
        let op2 = CreateKind::Group.add_op("g".into(), Point::new(0.0, 0.0));
        let before2 = canvas.clone();
        op2.apply(&mut canvas);
        stack.record(&op2, &before2);
        assert!(!stack.can_redo());
    }
}
