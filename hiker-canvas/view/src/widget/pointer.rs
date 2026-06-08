//! Pointer-gesture state machine for [`super::CanvasView`]: press begins a
//! gesture (select / move / resize / marquee / edge-draw / pan), drag advances
//! it, release commits it. Split from `view.rs` so the `show` loop stays within
//! the cognitive-complexity budget. This is an `impl super::CanvasView`
//! continuation, not a standalone module.

use egui::{Pos2, Rect, Response, Vec2};
use hiker_canvas::geometry::{Point, Rect as CanvasRect};
use hiker_canvas::model::{Canvas, NodeKind, Side};
use hiker_canvas::ops::Endpoint;

use canvas_view_core::handles::Handle;
use canvas_view_core::interaction::{self, Target};
use canvas_view_core::state::{self, CreateKind, Interaction, LabelEdit, PressAction};

use crate::widget::{CanvasResponse, CanvasView};

/// The fixed end and moving endpoint of an in-progress edge drag, bundled so the
/// release handler takes one parameter instead of four.
struct EdgeDragEnd {
    from_node: String,
    from_side: Side,
    endpoint: Endpoint,
    existing: Option<String>,
}

/// A live resize gesture's target node and origin rect, bundled so `drag_resize`
/// stays within the argument budget.
#[derive(Clone, Copy)]
struct ResizeDrag<'a> {
    node: &'a str,
    handle: Handle,
    orig: (i64, i64, i64, i64),
}

impl CanvasView {
    /// Route press / drag / release / click for one frame.
    pub(super) fn handle_pointer(
        &mut self,
        ui: &egui::Ui,
        canvas: &mut Canvas,
        viewport: Rect,
        response: &Response,
        out: &mut CanvasResponse,
    ) {
        let (shift, space) = ui.input(|i| (i.modifiers.shift, i.key_down(egui::Key::Space)));
        // Record where a right-click landed so the context menu can anchor there
        // and tell whether it hit a card or empty space. status: canvas-context-menu
        if response.secondary_clicked() {
            self.menu_anchor = response.interact_pointer_pos();
        }
        if response.drag_started() {
            // Resolve the press TARGET at the press-DOWN position (`press_origin`),
            // NOT the current pointer (`interact_pointer_pos`). By the time
            // `drag_started` fires, egui's drag threshold has already nudged the
            // pointer a few px off where the button went down — enough to slip off
            // a small resize handle, so a resize the user pressed ON would resolve
            // to the node body and become a move. The drag delta below still
            // tracks the live pointer; only the initial hit-test uses the origin.
            // status: canvas-node-resize
            let press_at = ui
                .input(|i| i.pointer.press_origin())
                .or_else(|| response.interact_pointer_pos());
            if let Some(p) = press_at {
                self.on_press(canvas, viewport, p, shift, space);
            }
        }
        if response.dragged() {
            if let Some(p) = response.interact_pointer_pos() {
                self.on_drag(canvas, viewport, p, response.drag_delta(), out);
            }
        }
        if response.drag_stopped() {
            self.on_release(canvas, viewport, response.interact_pointer_pos(), out);
        }
        // A double-click is reported by egui as a click on the first press and a
        // double-click on the second; handle the double first so the activation /
        // label-edit wins over a redundant single-click select.
        if response.double_clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                self.on_double_click(canvas, viewport, p, out);
            }
        } else if response.clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                self.on_click(canvas, viewport, p, shift, out);
            }
        }
    }

    /// A plain click (no drag). If a click-to-connect gesture is in progress,
    /// this click attaches (or cancels) the edge. Otherwise clicking a connector
    /// handle *starts* a click-to-connect; any other target selects / extends /
    /// clears. (Create/insert is consumed at the top of `show`, not on a click.)
    /// status: canvas-edge-draw
    fn on_click(&mut self, canvas: &mut Canvas, viewport: Rect, p: Pos2, shift: bool, out: &mut CanvasResponse) {
        // A bare click while group-draw is armed drops a default-sized frame at
        // the click point (the one-click path). status: canvas-group-draw
        if self.pending_group_draw {
            self.pending_group_draw = false;
            let world = self.camera.screen_to_world(viewport, p);
            let id = self.mint_id(canvas);
            let (w, h) = CreateKind::Group.default_size();
            let top_left = Point::new(world.x - w as f64 / 2.0, world.y - h as f64 / 2.0);
            self.commit_new_node(canvas, CreateKind::Group.add_op(id.clone(), top_left), id, out);
            return;
        }
        let target = interaction::resolve_target(canvas, &self.camera, viewport, &self.selection, p);
        if let Interaction::Connecting { from_node, from_side } = &self.interaction {
            let (from_node, from_side) = (from_node.clone(), *from_side);
            self.interaction = Interaction::Idle;
            self.finish_connect(canvas, &from_node, from_side, &target, out);
            return;
        }
        // Navigate-only under a projection lens (`proj-poincare-mode`): a click
        // never starts an edge connect or asks the host for inline-edit — it only
        // selects (for navigation). Off = full editing.
        if self.camera.lens_active() {
            interaction::click_select(&mut self.selection, &target, shift);
            // Under Poincaré, clicking a card flies the disk so that card glides
            // to the centre. [proj-poincare-nav]
            if self.camera.projection().kind == hiker_projection::ProjectionKind::Poincare
                && let Target::Node(id) = &target
                && let Some(node) = canvas.nodes.iter().find(|n| &n.id == id)
            {
                self.start_flyto(node);
            }
            return;
        }
        if let Target::SideHandle { node, side } = target {
            self.interaction = Interaction::Connecting { from_node: node, from_side: side };
            return;
        }
        // Click-again-to-edit: a plain (no-shift) click on a node that is ALREADY
        // the sole selection asks the host to enter inline-edit — the Finder
        // rename gesture (first click selects, second click edits). The host
        // enters edit only for an editable node; a click-again elsewhere is just a
        // redundant select. status: canvas-inline-edit
        if let Target::Node(id) = &target {
            let already_sole = !shift
                && self.selection.edges.is_empty()
                && self.selection.nodes.len() == 1
                && self.selection.nodes.contains(id);
            if already_sole {
                out.edit_requested = Some(id.clone());
            }
        }
        interaction::click_select(&mut self.selection, &target, shift);
    }

    /// A double-click: activate a node (host opens a link / file) or open the
    /// inline label editor on an edge. status: canvas-edge-label, canvas-link-node-card
    fn on_double_click(&mut self, canvas: &Canvas, viewport: Rect, p: Pos2, out: &mut CanvasResponse) {
        match interaction::resolve_target(canvas, &self.camera, viewport, &self.selection, p) {
            Target::Node(id) => out.activated = Some(id),
            Target::Edge(id) => {
                let draft = canvas.edges.iter().find(|e| e.id == id).and_then(|e| e.label.clone()).unwrap_or_default();
                self.label_edit = Some(LabelEdit { edge_id: id, draft });
            }
            _ => {}
        }
    }

    /// Complete a click-to-connect: add an edge from the origin to the clicked
    /// node (or re-targeted side handle), select it, and report the op. A drop
    /// on empty / the same node is a no-op (already reset to idle by the caller).
    /// status: canvas-edge-draw
    fn finish_connect(&mut self, canvas: &mut Canvas, from_node: &str, from_side: Side, target: &Target, out: &mut CanvasResponse) {
        let new_id = self.mint_id(canvas);
        let pre = canvas.clone();
        let op = interaction::finish_edge_drag(canvas, from_node, from_side, Endpoint::To, None, target, &new_id);
        if let Some(op) = op {
            self.commit_one(op, &pre, out);
            self.selection.clear();
            self.selection.edges.insert(new_id);
        }
    }

    /// Begin a gesture based on what's under the press point and the active tool.
    /// Under the Hand tool (or with Space held), any left-press pans — overriding
    /// select / move / resize / marquee. status: canvas-tool-mode
    fn on_press(&mut self, canvas: &Canvas, viewport: Rect, p: Pos2, shift: bool, space: bool) {
        let target = interaction::resolve_target(canvas, &self.camera, viewport, &self.selection, p);
        // Group-draw is armed by the "Add group" verb; the next empty drag draws
        // the group regardless of tool. status: canvas-group-draw
        if self.pending_group_draw && matches!(target, Target::Empty) {
            self.interaction = Interaction::DrawGroup { origin: self.camera.screen_to_world(viewport, p) };
            return;
        }
        let on_node = !matches!(target, Target::Empty);
        if let PressAction::Pan = state::press_action(self.tool, space, false, on_node) {
            self.interaction = Interaction::Pan;
            return;
        }
        // Navigate-only under a projection lens (`proj-poincare-mode`): editing in
        // warped space (drag-move, resize, edge-draw) isn't supported in v1, so
        // when a projection mode is active a left-press on a node only navigates —
        // it pans the canvas instead of starting an edit gesture. Pan + zoom are
        // handled above / elsewhere and stay live; Off = full editing.
        //
        // Under Poincaré specifically, ANY background/card drag drives the
        // hyperbolic drag-to-recentre (`on_drag` turns a `Pan` gesture into a
        // Möbius recentre), so an empty-canvas press also begins a `Pan` rather
        // than a marquee — the whole disk is the navigation surface.
        // [proj-poincare-nav]
        if self.camera.lens_active() {
            let poincare = self.camera.projection().kind == hiker_projection::ProjectionKind::Poincare;
            if poincare || !matches!(target, Target::Empty) {
                self.interaction = Interaction::Pan;
                return;
            }
        }
        match target {
            Target::ResizeHandle(handle) => self.begin_resize(canvas, handle),
            Target::SideHandle { node, side } => {
                self.interaction = Interaction::EdgeDrag { from_node: node, from_side: side, endpoint: Endpoint::To, existing: None };
            }
            Target::Node(id) => self.begin_node_drag(canvas, &id, shift),
            Target::Edge(id) => self.begin_edge_press(canvas, viewport, &id, p, shift),
            Target::Empty => self.begin_empty_press(viewport, p),
        }
    }

    fn begin_resize(&mut self, canvas: &Canvas, handle: Handle) {
        let Some(id) = self.selection.nodes.iter().next().cloned() else { return };
        let Some(node) = canvas.nodes.iter().find(|n| n.id == id) else { return };
        self.interaction = Interaction::Resize { node: id, handle, orig: (node.x, node.y, node.width, node.height) };
    }

    fn begin_node_drag(&mut self, canvas: &Canvas, id: &str, shift: bool) {
        if !self.selection.has_node(id) {
            if !shift {
                self.selection.clear();
            }
            self.selection.nodes.insert(id.to_owned());
            // Newly selecting a group also selects its geometric members, so the
            // user SEES exactly what will move (members paint as selected) and the
            // move-set is decided once here — not re-derived by containment each
            // frame, which let a group dragged over another silently grab its
            // cards. status: canvas-group-move
            if matches!(canvas.nodes.iter().find(|n| n.id == id).map(|n| &n.kind), Some(NodeKind::Group { .. })) {
                for member in interaction::group_member_ids(canvas, id) {
                    self.selection.nodes.insert(member);
                }
            }
        }
        // Snapshot the move set once, at drag start, as exactly the current
        // selection (which already includes any selected group's members). It
        // stays fixed for the whole gesture, so a group dragged over another never
        // steals its cards — those cards aren't selected. status: canvas-group-move
        let ids: Vec<String> = self.selection.nodes.iter().cloned().collect();
        self.interaction = Interaction::MoveSelection { accum: Point::new(0.0, 0.0), ids };
    }

    fn begin_edge_press(&mut self, canvas: &Canvas, viewport: Rect, id: &str, p: Pos2, shift: bool) {
        if let Some((endpoint, from_node, from_side)) = interaction::nearest_endpoint(canvas, &self.camera, viewport, id, p) {
            self.interaction = Interaction::EdgeDrag { from_node, from_side, endpoint, existing: Some(id.to_owned()) };
            return;
        }
        if !shift {
            self.selection.clear();
        }
        self.selection.edges.insert(id.to_owned());
    }

    /// An empty-canvas press under the Select tool (pan / group-draw were already
    /// handled in `on_press`): marquee-select. status: canvas-tool-mode
    fn begin_empty_press(&mut self, viewport: Rect, p: Pos2) {
        self.interaction = Interaction::Marquee { origin: self.camera.screen_to_world(viewport, p) };
    }

    /// Advance the active gesture by this frame's pointer delta.
    fn on_drag(&mut self, canvas: &mut Canvas, viewport: Rect, p: Pos2, delta: Vec2, out: &mut CanvasResponse) {
        match std::mem::replace(&mut self.interaction, Interaction::Idle) {
            Interaction::Pan => {
                // Under Poincaré a background/card drag recentres the disk
                // hyperbolically (the grabbed point follows the cursor) rather
                // than affine-panning; every other mode pans affinely.
                // [proj-poincare-nav]
                if self.camera.projection().kind == hiker_projection::ProjectionKind::Poincare {
                    self.drag_recenter(viewport, p, delta);
                } else {
                    self.camera.pan_by_screen(delta);
                }
                self.interaction = Interaction::Pan;
            }
            Interaction::MoveSelection { accum, ids } => self.drag_move(canvas, accum, ids, delta, out),
            Interaction::Resize { node, handle, orig } => {
                let drag = ResizeDrag { node: &node, handle, orig };
                self.drag_resize(canvas, viewport, p, drag, out);
            }
            other => self.interaction = other,
        }
    }

    /// Hyperbolic drag-to-recentre (Poincaré only): the disk point grabbed under
    /// the cursor follows it. Maps the previous + current pointer positions into
    /// post-nav disk space (via [`Camera::disk_under_screen`]), then left-composes
    /// the Möbius transform that carries `p_prev → p_cur` onto the camera's `nav`,
    /// so the grabbed point tracks the cursor. A manual drag cancels any in-flight
    /// fly-to. [proj-poincare-nav]
    fn drag_recenter(&mut self, viewport: Rect, cur: Pos2, delta: Vec2) {
        let prev = cur - delta;
        let p_prev = self.camera.disk_under_screen(viewport, prev);
        let p_cur = self.camera.disk_under_screen(viewport, cur);
        let recentre = hiker_projection::Mobius::from_point_pair(p_prev, p_cur);
        let nav = hiker_projection::Mobius::compose(recentre, self.camera.nav());
        self.camera.set_nav(nav);
        self.cancel_flyto();
    }

    fn drag_move(&mut self, canvas: &mut Canvas, accum: Point, ids: Vec<String>, delta: Vec2, out: &mut CanvasResponse) {
        let scale = f64::from(self.camera.scale());
        let total = Point::new(accum.x + f64::from(delta.x) / scale, accum.y + f64::from(delta.y) / scale);
        let dx = total.x.trunc() as i64;
        let dy = total.y.trunc() as i64;
        let pre = canvas.clone();
        if let Some(op) = interaction::move_selection(canvas, ids.clone(), dx, dy) {
            self.commit_one(op, &pre, out);
        }
        self.interaction = Interaction::MoveSelection { accum: Point::new(total.x - dx as f64, total.y - dy as f64), ids };
    }

    fn drag_resize(&mut self, canvas: &mut Canvas, viewport: Rect, p: Pos2, drag: ResizeDrag, out: &mut CanvasResponse) {
        let world = self.camera.screen_to_world(viewport, p);
        let pre = canvas.clone();
        if let Some(op) = interaction::resize_node(canvas, drag.node, drag.handle, drag.orig, world) {
            self.commit_one(op, &pre, out);
        }
        self.interaction = Interaction::Resize { node: drag.node.to_owned(), handle: drag.handle, orig: drag.orig };
    }

    /// Finish the active gesture on pointer release.
    fn on_release(&mut self, canvas: &mut Canvas, viewport: Rect, p: Option<Pos2>, out: &mut CanvasResponse) {
        match std::mem::replace(&mut self.interaction, Interaction::Idle) {
            Interaction::Marquee { origin } => self.finish_marquee(canvas, viewport, origin, p),
            Interaction::DrawGroup { origin } => self.finish_group_draw(canvas, viewport, origin, p, out),
            Interaction::EdgeDrag { from_node, from_side, endpoint, existing } => {
                let end = EdgeDragEnd { from_node, from_side, endpoint, existing };
                self.finish_edge(canvas, viewport, p, &end, out);
            }
            _ => {}
        }
    }

    /// Create the drawn group on release: build the `AddNode` at the rubber-banded
    /// bounds (normalized for negative drags, min-size clamped), commit + select
    /// it, and disarm the one-shot draw. status: canvas-group-draw
    fn finish_group_draw(&mut self, canvas: &mut Canvas, viewport: Rect, origin: Point, p: Option<Pos2>, out: &mut CanvasResponse) {
        self.pending_group_draw = false;
        let Some(p) = p else { return };
        let cur = self.camera.screen_to_world(viewport, p);
        let id = self.mint_id(canvas);
        let op = CreateKind::add_group_op(id.clone(), origin, cur);
        self.commit_new_node(canvas, op, id, out);
    }

    fn finish_marquee(&mut self, canvas: &Canvas, viewport: Rect, origin: Point, p: Option<Pos2>) {
        let Some(p) = p else { return };
        let cur = self.camera.screen_to_world(viewport, p);
        let rect = CanvasRect::new(
            origin.x.min(cur.x),
            origin.y.min(cur.y),
            (origin.x - cur.x).abs(),
            (origin.y - cur.y).abs(),
        );
        interaction::marquee_select(canvas, &mut self.selection, rect, false);
    }

    fn finish_edge(&mut self, canvas: &mut Canvas, viewport: Rect, p: Option<Pos2>, end: &EdgeDragEnd, out: &mut CanvasResponse) {
        let Some(p) = p else { return };
        let target = interaction::resolve_target(canvas, &self.camera, viewport, &self.selection, p);
        let new_id = self.mint_id(canvas);
        let pre = canvas.clone();
        let op = interaction::finish_edge_drag(
            canvas,
            &end.from_node,
            end.from_side,
            end.endpoint,
            end.existing.as_deref(),
            &target,
            &new_id,
        );
        if let Some(op) = op {
            self.commit_one(op, &pre, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CanvasView;
    use canvas_view_core::state::Interaction;
    use hiker_canvas::model::{Canvas, Node, NodeKind};
    use std::collections::BTreeMap;

    fn node(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node { id: id.to_owned(), x, y, width: w, height: h, color: None, kind: NodeKind::Text { text: String::new() }, extra: BTreeMap::new() }
    }

    fn group(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node { kind: NodeKind::Group { label: None, background: None, background_style: None }, ..node(id, x, y, w, h) }
    }

    /// Selecting a group at drag-start folds its geometric members into the
    /// selection (so they paint as selected) and freezes that exact set as the
    /// move-set. A card inside another, unselected group is never picked up.
    #[test]
    fn selecting_a_group_selects_its_members_and_freezes_that_set() {
        let mut view = CanvasView::new();
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("a", 0, 0, 200, 200));
        canvas.nodes.push(node("m", 20, 20, 50, 50));
        canvas.nodes.push(group("b", 600, 0, 200, 200));
        canvas.nodes.push(node("b_card", 620, 20, 50, 50));

        view.begin_node_drag(&canvas, "a", false);

        // Group A and its member are selected (so the outline paints for both);
        // group B and its card are not.
        assert!(view.selection.has_node("a"));
        assert!(view.selection.has_node("m"));
        assert!(!view.selection.has_node("b_card"));
        assert!(!view.selection.has_node("b"));

        // The frozen move-set equals the selection (a + m), excluding b_card.
        let Interaction::MoveSelection { ids, .. } = &view.interaction else {
            panic!("expected a MoveSelection gesture");
        };
        assert!(ids.contains(&"a".to_string()) && ids.contains(&"m".to_string()));
        assert!(!ids.contains(&"b_card".to_string()), "another group's card is not in the move-set");
    }

    /// A group dragged past another group's card does not move that card — it's
    /// not in the frozen selection, even once A's rect slides over it.
    #[test]
    fn dragging_a_group_past_a_card_does_not_move_it() {
        let mut view = CanvasView::new();
        let mut out = crate::widget::CanvasResponse::default();
        let mut canvas = Canvas::default();
        canvas.nodes.push(group("a", 0, 0, 200, 200));
        canvas.nodes.push(node("m", 20, 20, 50, 50));
        canvas.nodes.push(node("b_card", 620, 20, 50, 50));

        view.begin_node_drag(&canvas, "a", false);
        let Interaction::MoveSelection { accum, ids } = std::mem::replace(&mut view.interaction, Interaction::Idle) else {
            panic!("expected a MoveSelection gesture");
        };
        // Drag A far right (camera default scale 1) so its rect now overlaps b_card.
        let before = canvas.nodes.iter().find(|n| n.id == "b_card").map(|n| (n.x, n.y));
        view.drag_move(&mut canvas, accum, ids, egui::vec2(600.0, 0.0), &mut out);
        let after = canvas.nodes.iter().find(|n| n.id == "b_card").map(|n| (n.x, n.y));
        assert_eq!(before, after, "an unselected card must not be carried along");
        assert_eq!(canvas.nodes.iter().find(|n| n.id == "a").map(|n| n.x), Some(600));
        assert_eq!(canvas.nodes.iter().find(|n| n.id == "m").map(|n| n.x), Some(620));
    }
}
