//! The [`CanvasView`] widget and its per-frame [`CanvasView::show`] loop.
//!
//! # Host / renderer contract
//!
//! The host owns the clip rect and allocates the response; this widget paints
//! into the [`egui::Painter`] and hit-tests, the same division `zim.md`
//! describes for the htmlview tab. `show` takes the `Canvas` document and a
//! host-supplied [`crate::content::NodeContentRenderer`], drives one frame of
//! interaction, and returns a [`CanvasResponse`].
//!
//! Every edit this frame is applied to `canvas` immediately (for responsiveness)
//! AND reported as an [`EditOp`] in [`CanvasResponse::committed`] so the host can
//! persist it through the op-log binding. Undo/redo likewise produce ops that
//! are both applied and reported — the host treats them no differently from a
//! direct edit. The view's undo stack is in-memory and distinct from op-log
//! history.
//
// status: canvas-render-widget

use egui::{Key, Rect, Sense};
use hiker_canvas::geometry::{content_bounds, node_bounds, Point};
use hiker_canvas::model::{Canvas, Node};
use hiker_canvas::ops::EditOp;

use std::collections::HashMap;

use hiker_projection::{clamp_inside_disk, Complex, Mobius, DEFAULT_BOUNDARY_RADIUS};

use canvas_view_core::camera::Camera;
use canvas_view_core::edges::anchor_pos;
use canvas_view_core::handles::Handle;
use canvas_view_core::interaction::{self, Target};
use canvas_view_core::state::{CreateKind, Interaction, LabelEdit, ScrollMode, Selection, Tool, UndoStack};
use hiker_canvas::model::Side;

use crate::content::{CardView, NodeContentRenderer};
use crate::menu::{self, CanvasMenuRenderer};
use crate::paint;

/// Spacing of the optional background grid, in canvas units.
const GRID_STEP: f32 = 40.0;

/// Default click fly-to glide duration, in seconds. [proj-poincare-nav]
const FLYTO_DURATION: f32 = 0.5;

/// A Poincaré click fly-to in progress: the disk centre glides from
/// `start_center` to `target_center` (both pre-nav disk points) over `dur`
/// seconds, easing out. Each frame rebuilds the camera's `nav` as the pure
/// recentre that maps the eased point to the disk origin, so the clicked card
/// glides to the centre while the board recentres hyperbolically around it.
/// [proj-poincare-nav]
#[derive(Debug, Clone, Copy)]
struct FlyTo {
    start_center: Complex,
    target_center: Complex,
    t: f32,
    dur: f32,
}

/// `1 − (1 − t)³` — decelerating ease for the fly-to glide. [proj-poincare-nav]
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Linear interpolation on the complex plane (component-wise on re/im).
/// [proj-poincare-nav]
fn lerp_complex(a: Complex, b: Complex, t: f32) -> Complex {
    Complex::new(a.re + (b.re - a.re) * t, a.im + (b.im - a.im) * t)
}

/// What one frame of the canvas view produced. The host persists `committed`
/// through the op-log binding; the ops are already applied to the live `Canvas`.
#[derive(Debug, Default, Clone)]
pub struct CanvasResponse {
    /// Edit ops committed this frame, in apply order. Empty on a no-op frame.
    pub committed: Vec<EditOp>,
    /// Whether the pointer interacted with the canvas this frame (for the host
    /// to decide focus / cursor).
    pub interacted: bool,
    /// A node the user *activated* (double-clicked) this frame, for the host to
    /// act on by kind: open a link node's URL, open a file node in a tab. The
    /// view itself stays content-display-only; activation is the host's job.
    /// status: canvas-link-node-card
    pub activated: Option<String>,
    /// A node the user asked to EDIT this frame WITHOUT a double-click: either a
    /// plain click on a node that was already the sole selection (click-again, a
    /// la Finder rename), or the host's Enter/F2-on-selected. The host enters
    /// inline-edit for an editable node (no activate fallback — a click-again on a
    /// non-editable node does nothing). Distinct from `activated` (double-click)
    /// so the host can treat them differently. status: canvas-inline-edit
    pub edit_requested: Option<String>,
    /// The host should open the inline "+ Link" URL prompt — a canvas
    /// context-menu action that needs host-side UI. status: canvas-node-create
    pub request_link_prompt: bool,
    /// The host should open the "Insert from vault" picker — a context-menu
    /// action that needs host-side UI. status: canvas-insert-from-vault
    pub request_insert_picker: bool,
    /// The host should create a brand-new vault note and drop a File-node pointer
    /// to it on the canvas — a context-menu action whose vault file creation is
    /// host-side. status: canvas-node-create
    pub request_new_note: bool,
}

/// The interactive handles the pointer is over this frame, resolved from the
/// press-path hit-tests so the hover overlays grow / brighten exactly what a
/// press would grab. All fields are `None` when no pointer hover is over the
/// viewport. Purely visual — no field drives an interaction. status: canvas-handle-hover
#[derive(Debug, Default, Clone)]
struct HandleHover {
    /// The resize handle under the cursor (only when a single node is selected).
    resize: Option<Handle>,
    /// The connector `(node id, side)` under the cursor.
    connector: Option<(String, Side)>,
    /// The id of the group whose header grab-strip is under the cursor.
    group_header: Option<String>,
}

/// The stateful canvas editor widget. Holds view state (camera, selection,
/// in-progress interaction, a queued create/insert request) and an in-session
/// undo stack. The `Canvas` document lives with the host and is passed into
/// `show`.
///
/// A create/insert is a one-shot request the host queues (via
/// [`CanvasView::create_centered`] / [`CanvasView::insert_node_centered`]); the
/// next [`CanvasView::show`] consumes it immediately at the viewport center —
/// committing, selecting, and clearing it — so a toolbar click drops a node with
/// no second placement gesture.
#[derive(Debug, Default)]
pub struct CanvasView {
    camera: Camera,
    selection: Selection,
    interaction: Interaction,
    undo: UndoStack,
    /// A queued "create a node of this kind at viewport center" request,
    /// consumed on the next `show`.
    pending_create: Option<CreateKind>,
    /// A queued "insert this fully-built node at viewport center" request,
    /// consumed on the next `show`. The host supplies a node with kind + fields
    /// already filled (e.g. a file-node pointer or a link).
    pending_insert: Option<Node>,
    /// An in-progress inline edit of an edge's label (opened by double-clicking
    /// an edge). The field is rendered above the interaction surface so it can
    /// receive input. status: canvas-edge-label
    label_edit: Option<LabelEdit>,
    /// Per-card content view state (zoom + scroll), keyed by node id. View state
    /// only — never serialized; defaults to 1.0 zoom / 0 scroll. Decoupled from
    /// camera zoom: a card is a readable, scrollable window.
    /// status: canvas-card-zoom, canvas-card-scroll
    card_views: HashMap<String, CardView>,
    /// Screen position of the last right-click, used to anchor the canvas
    /// context menu and decide whether it landed on a card or empty space.
    /// status: canvas-context-menu
    menu_anchor: Option<egui::Pos2>,
    next_id: u64,
    show_grid: bool,
    /// The active interaction tool (Select / Hand). View state only — never
    /// serialized. status: canvas-tool-mode
    tool: Tool,
    /// Armed by the "Add group" verb: the next left-drag on empty canvas draws
    /// the group's rectangle (a bare click drops a default-sized frame instead).
    /// Reverts to normal Select behavior once the group is created.
    /// status: canvas-group-draw
    pending_group_draw: bool,
    /// How a plain (no-modifier) scroll over empty canvas behaves (pan / zoom /
    /// auto-detect by device). Ctrl/Cmd+scroll and pinch always zoom regardless.
    /// Set by the host from `[ui].canvas_scroll_mode` each frame. View state only.
    /// status: canvas-scroll-mode
    scroll_mode: ScrollMode,
    /// In `Auto` mode, the device behind the most recent scroll (`true` = mouse
    /// wheel / line delta, `false` = touchpad / pixel delta). egui emits a tail of
    /// smoothed `smooth_scroll_delta` frames after the raw `MouseWheel` events
    /// stop, so we remember the last source to keep pan-vs-zoom stable across that
    /// tail rather than flipping mid-gesture. status: canvas-scroll-mode
    last_scroll_was_line: bool,
    /// An in-flight Poincaré click fly-to, if any: animates the camera's `nav` so
    /// a clicked card glides to the disk centre. Cleared on completion, a manual
    /// drag-recentre, or a fit/reset. View state only. [proj-poincare-nav]
    flyto: Option<FlyTo>,
}

impl CanvasView {
    /// A fresh view with the grid enabled.
    #[must_use]
    pub fn new() -> Self {
        Self { show_grid: true, ..Self::default() }
    }

    /// Set how a plain scroll behaves (pan / zoom / auto-detect). The host drives
    /// this from `[ui].canvas_scroll_mode`. status: canvas-scroll-mode
    pub const fn set_scroll_mode(&mut self, mode: ScrollMode) {
        self.scroll_mode = mode;
    }

    /// The current camera (read-only view state).
    #[must_use]
    pub const fn camera(&self) -> &Camera {
        &self.camera
    }

    /// The camera's active projection config (read-only) so the host view menu
    /// can reflect the current kind / strength / size-falloff. [proj-canvas-mode]
    #[must_use]
    pub const fn projection(&self) -> hiker_projection::ProjectionConfig {
        self.camera.projection()
    }

    /// Mutable access to the camera's projection config for the host's view-menu
    /// sliders (kind / strength / size-falloff). [proj-cfg-strength,
    /// proj-cfg-size-falloff]
    pub const fn projection_mut(&mut self) -> &mut hiker_projection::ProjectionConfig {
        self.camera.projection_mut()
    }

    /// Mutable access to the per-card scale clamp for the view-menu min/max
    /// sliders. [proj-cfg-card-scale-clamp]
    pub const fn card_scale_clamp_mut(&mut self) -> &mut canvas_view_core::camera::CardScaleClamp {
        self.camera.card_scale_clamp_mut()
    }

    /// Mutable access to the Poincaré boundary-circle toggle for the view-menu
    /// checkbox (Poincaré only). [proj-canvas-mode]
    pub const fn show_boundary_mut(&mut self) -> &mut bool {
        self.camera.show_boundary_mut()
    }

    /// The current selection (read-only).
    #[must_use]
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The current content scroll offset of a card by node id (0 when the card
    /// has no stored view state). Lets the host seed an inline-edit overlay at
    /// the same scroll the read-only card was showing, so clicking into a note
    /// doesn't jump to the top. status: canvas-inline-edit
    #[must_use]
    pub fn card_scroll(&self, id: &str) -> f32 {
        self.card_views.get(id).map_or(0.0, |c| c.scroll_y)
    }

    /// Toggle the dotted background grid.
    pub const fn set_grid(&mut self, on: bool) {
        self.show_grid = on;
    }

    /// Snapshot the persistable view state: the camera pan + zoom and a clone of
    /// every touched card's [`CardView`] keyed by node id. The host converts this
    /// to a serializable form and rides the tab-state store with it, so camera
    /// pan/zoom + per-card scroll/zoom survive tab close/reopen and restart — the
    /// camera stays view state and never enters the op-log / `.canvas` file.
    /// status: canvas-view-state-persist
    #[must_use]
    pub fn view_snapshot(&self) -> (Point, f32, Vec<(String, CardView)>) {
        let cards = self.card_views.iter().map(|(id, view)| (id.clone(), *view)).collect();
        (self.camera.pan(), self.camera.scale(), cards)
    }

    /// Restore a previously snapshotted view: set the camera to `pan` + `scale`
    /// (the camera clamps `scale` to its zoom bounds) and replace the per-card
    /// view state with `cards`. The inverse of [`CanvasView::view_snapshot`].
    /// status: canvas-view-state-persist
    pub fn restore_view(&mut self, pan: Point, scale: f32, cards: impl IntoIterator<Item = (String, CardView)>) {
        self.camera.set_pan_scale(pan, scale);
        self.card_views = cards.into_iter().collect();
    }

    /// The active interaction tool (read-only). status: canvas-tool-mode
    #[must_use]
    pub const fn tool(&self) -> Tool {
        self.tool
    }

    /// Switch the active interaction tool. status: canvas-tool-mode
    pub const fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
    }

    /// Arm a one-shot group-draw: the next left-drag on empty canvas rubber-bands
    /// the group's rectangle and creates it on release; a bare click drops a
    /// default-sized frame at the click point. status: canvas-group-draw
    pub const fn arm_group_draw(&mut self) {
        self.pending_group_draw = true;
    }

    /// Queue an immediate create: the next [`CanvasView::show`] mints a node of
    /// `kind`, drops it at the viewport center, commits it (reported in
    /// [`CanvasResponse::committed`] for the host to persist), and selects it.
    /// One click, no second placement gesture. Supersedes any pending request.
    pub fn create_centered(&mut self, kind: CreateKind) {
        self.pending_create = Some(kind);
        self.pending_insert = None;
    }

    /// Queue an immediate insert of a FULLY-BUILT `node` (caller supplies the
    /// kind + fields — e.g. a `File { file, subpath }` pointer or a `Link`): the
    /// next [`CanvasView::show`] positions it at the viewport center, commits it
    /// (reported in [`CanvasResponse::committed`]), and selects it. The node's
    /// `id` is reassigned to a canvas-unique one and its `x`/`y` are overwritten
    /// with the center; `width`/`height`/`color`/kind fields are kept as given.
    /// Supersedes any pending request.
    pub fn insert_node_centered(&mut self, node: Node) {
        self.pending_insert = Some(node);
        self.pending_create = None;
    }

    /// The on-screen rect a `node` occupies under the current camera within
    /// `viewport` — the same `camera` + `node_bounds` mapping the painter uses.
    /// The host positions the inline-edit overlay over this rect and tracks it
    /// across pan/zoom. status: canvas-inline-edit
    #[must_use]
    pub fn node_screen_rect(&self, viewport: Rect, node: &Node) -> Rect {
        self.camera.world_rect_to_screen(viewport, node_bounds(node))
    }

    /// Whether `node` renders as a LOD placeholder at the current camera —
    /// i.e. its on-screen rect is below the readable threshold (`paint::is_tiny`).
    /// A placeholder is too small to edit, so the host opens the note in a tab
    /// on double-click instead of entering edit mode.
    /// status: canvas-inline-edit, canvas-lod-placeholder
    #[must_use]
    pub fn is_node_lod(&self, viewport: Rect, node: &Node) -> bool {
        paint::is_tiny(self.node_screen_rect(viewport, node))
    }

    /// Force a Poincaré navigation recentre directly — for headless snapshot /
    /// demo filmstrips that need to capture intermediate fly-to frames without
    /// driving the animation over real time. `e` is the eased glide fraction in
    /// `[0, 1]`; the disk centre lerps from the disk origin to `target` (a card's
    /// pre-nav disk point, e.g. from [`Camera::disk_point`]). Not part of the
    /// interactive flow. [proj-poincare-nav]
    #[doc(hidden)]
    pub fn set_nav_flyto_for_demo(&mut self, target: Complex, e: f32) {
        let c = clamp_inside_disk(
            lerp_complex(Complex::ORIGIN, target, e.clamp(0.0, 1.0)),
            DEFAULT_BOUNDARY_RADIUS,
        );
        self.camera.set_nav(Mobius::from_point_pair(c, Complex::ORIGIN));
    }

    /// The pre-nav disk point of a world card center — for the demo filmstrip to
    /// pick a peripheral card's fly-to target. [proj-poincare-nav]
    #[doc(hidden)]
    #[must_use]
    pub fn disk_point_for_demo(&self, center: Point) -> Complex {
        self.camera.disk_point(center)
    }

    /// Refresh the lens framing from a canvas without painting — for the demo
    /// filmstrip to resolve a card's disk point before rendering. [proj-poincare-nav]
    #[doc(hidden)]
    pub fn update_lens_for_demo(&mut self, canvas: &Canvas) {
        self.camera.update_lens(content_bounds(canvas));
    }

    /// Frame all content (zoom-to-fit) within `viewport`. Also recentres the
    /// Poincaré disk: a fit/reset drops any accumulated hyperbolic navigation
    /// (drag-recentre / fly-to) so the disk re-centres on the content.
    /// [proj-poincare-nav]
    pub fn fit(&mut self, viewport: Rect, canvas: &Canvas) {
        if let Some(b) = content_bounds(canvas) {
            self.camera.zoom_to_fit(viewport, b, 0.08);
        }
        self.camera.reset_nav();
        self.flyto = None;
    }

    /// Single-select the node `id` and center the camera on it (keeping the
    /// current zoom), if it exists. The FOLLOW seam a host uses to highlight
    /// and bring into view the node matching the active note of a linked tab
    /// group. Returns `true` when the node was found and focused; `false`
    /// (no-op) otherwise. status: tab-linking
    pub fn focus_node(&mut self, viewport: Rect, canvas: &Canvas, id: &str) -> bool {
        let Some(node) = canvas.nodes.iter().find(|n| n.id == id) else {
            return false;
        };
        self.selection.clear();
        self.selection.nodes.insert(id.to_string());
        let center = Point::new(
            node.x as f64 + node.width as f64 / 2.0,
            node.y as f64 + node.height as f64 / 2.0,
        );
        self.camera.center_on_point(viewport, center);
        true
    }

    /// Center the camera on a canvas-space point `p` within `viewport`, keeping
    /// the current zoom — a thin accessor over [`Camera::center_on_point`]. The
    /// host's overview minimap calls this with a focused card's center so
    /// navigating the overview (and swapping back) re-aims the canvas viewport.
    /// status: canvas-minimap
    pub fn center_camera_on(&mut self, viewport: Rect, p: Point) {
        self.camera.center_on_point(viewport, p);
    }

    /// Mint a fresh, canvas-unique node/edge id.
    fn mint_id(&mut self, canvas: &Canvas) -> String {
        loop {
            self.next_id += 1;
            let id = format!("n{}", self.next_id);
            let taken = canvas.nodes.iter().any(|n| n.id == id) || canvas.edges.iter().any(|e| e.id == id);
            if !taken {
                return id;
            }
        }
    }

    /// The canvas-space point at the center of `viewport` under the current
    /// camera — where a queued create/insert drops its node.
    fn viewport_center(&self, viewport: Rect) -> Point {
        self.camera.screen_to_world(viewport, viewport.center())
    }

    /// Consume a queued create/insert (if any) at the viewport center: build the
    /// node, position it so its center sits at the viewport center, commit the
    /// `AddNode`, and select it. Runs at the top of `show` so a toolbar click
    /// lands without a second placement gesture.
    fn consume_pending(&mut self, canvas: &mut Canvas, viewport: Rect, out: &mut CanvasResponse) {
        let center = self.viewport_center(viewport);
        if let Some(kind) = self.pending_create.take() {
            let id = self.mint_id(canvas);
            let (w, h) = kind.default_size();
            let top_left = Point::new(center.x - w as f64 / 2.0, center.y - h as f64 / 2.0);
            self.commit_new_node(canvas, kind.add_op(id.clone(), top_left), id, out);
        }
        if let Some(mut node) = self.pending_insert.take() {
            let id = self.mint_id(canvas);
            node.id = id.clone();
            node.x = (center.x - node.width as f64 / 2.0).round() as i64;
            node.y = (center.y - node.height as f64 / 2.0).round() as i64;
            let op = EditOp::AddNode { node: Box::new(node) };
            self.commit_new_node(canvas, op, id, out);
        }
    }

    /// Apply a freshly-built `AddNode` op, record it for undo + report it, and
    /// make the new node `id` the sole selection so the user can drag it.
    fn commit_new_node(&mut self, canvas: &mut Canvas, op: EditOp, id: String, out: &mut CanvasResponse) {
        let pre = canvas.clone();
        op.apply(canvas);
        self.commit_one(op, &pre, out);
        self.selection.clear();
        self.selection.nodes.insert(id);
    }

    /// Begin a Poincaré click fly-to toward `node`: glide the disk centre from the
    /// currently-centred pre-nav point to the node's pre-nav disk point, so the
    /// card glides to the disk centre and the board recentres around it. Overwrites
    /// any accumulated drag-recentre — the glide ends cleanly centred on the card.
    /// [proj-poincare-nav]
    fn start_flyto(&mut self, node: &Node) {
        let center = Point::new(
            node.x as f64 + node.width as f64 / 2.0,
            node.y as f64 + node.height as f64 / 2.0,
        );
        // The card's resting (pre-nav) disk point is where the glide ends; the
        // start is the point the current nav has at the disk centre.
        let target_center = self.camera.disk_point(center);
        let start_center = self.camera.nav().invert().apply(Complex::ORIGIN);
        self.flyto = Some(FlyTo { start_center, target_center, t: 0.0, dur: FLYTO_DURATION });
    }

    /// Advance an in-flight fly-to by one frame, rebuilding the camera's `nav` as
    /// the pure recentre that maps the eased disk point to the origin. Requests a
    /// repaint while animating; clears the fly-to once `t` reaches 1.
    /// [proj-poincare-nav]
    fn advance_flyto(&mut self, ui: &egui::Ui) {
        let Some(mut fly) = self.flyto else { return };
        let dt = ui.input(|i| i.stable_dt);
        fly.t += dt / fly.dur;
        let e = ease_out_cubic(fly.t.min(1.0));
        let c = clamp_inside_disk(
            lerp_complex(fly.start_center, fly.target_center, e),
            DEFAULT_BOUNDARY_RADIUS,
        );
        self.camera.set_nav(Mobius::from_point_pair(c, Complex::ORIGIN));
        if fly.t >= 1.0 {
            self.flyto = None;
        } else {
            self.flyto = Some(fly);
            ui.ctx().request_repaint();
        }
    }

    /// Cancel any in-flight fly-to (a manual drag-recentre takes over).
    /// [proj-poincare-nav]
    const fn cancel_flyto(&mut self) {
        self.flyto = None;
    }

    /// Run one frame: paint the canvas, handle input, and report committed ops.
    ///
    /// The viewport is the space *below* the host's header (the available rect),
    /// never the full clip rect — otherwise the interaction surface would cover
    /// (and steal clicks from) the toolbar.
    ///
    /// Painting happens in two passes around the interaction surface: the scene
    /// (grid, edges, node cards + their content) paints FIRST so node content
    /// registers *below* the surface, then the surface is allocated on top so
    /// the canvas — not the content widgets — owns pointer drag/select/resize/
    /// connect (egui gives the topmost widget pointer priority). Overlays
    /// (selection, handles, rubber bands) and the inline label editor paint
    /// last, on top of the surface. Node content is therefore display-only.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &mut Canvas,
        content: &mut dyn NodeContentRenderer,
        menu: &mut dyn CanvasMenuRenderer,
    ) -> CanvasResponse {
        let viewport = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let mut out = CanvasResponse::default();

        self.consume_pending(canvas, viewport, &mut out);
        // Resolve the hovered interactive handles once per frame from the same
        // hit-tests the press path uses, so the overlays grow / brighten the
        // exact target a press would grab — purely visual. status: canvas-handle-hover
        let hover = self.handle_hover(ui, viewport, canvas);
        self.paint_scene(ui, viewport, canvas, content, hover.group_header.as_deref());

        // The interaction surface, registered AFTER content so it sits on top.
        // A stable id (not the auto allocate_rect id) keeps the context menu open
        // across frames. status: canvas-context-menu
        let response = ui.interact(viewport, ui.id().with("canvas-surface"), Sense::click_and_drag());
        // The canvas reads keyboard via global input (`handle_keys`), never
        // through widget focus — so the surface must not HOLD keyboard focus.
        // egui grants focus to a clicked click/drag widget; left held, it steals
        // focus from an inline-edit overlay editor (whose typing / Backspace then
        // never register). Surrender it each frame. status: canvas-inline-edit
        if response.has_focus() {
            response.surrender_focus();
        }
        self.handle_zoom(ui, viewport, canvas);
        self.handle_keys(ui, canvas, &mut out);
        self.handle_tool_keys(ui);
        self.handle_middle_pan(ui, viewport);
        self.handle_pointer(ui, canvas, viewport, &response, &mut out);
        // Advance any in-flight Poincaré click fly-to (a no-op otherwise), driving
        // the camera's `nav` toward the clicked card and requesting repaints until
        // the glide finishes. [proj-poincare-nav]
        self.advance_flyto(ui);
        self.apply_pan_cursor(ui, viewport, &response);

        self.paint_overlays(ui, viewport, canvas, &hover);
        self.draw_label_editor(ui, viewport, canvas, &mut out);
        self.show_context_menu(&response, viewport, canvas, menu, &mut out);

        out.interacted = response.clicked() || response.dragged() || response.hovered();
        out
    }

    /// Paint the canvas SCENE only — grid, group backgrounds, edges, and node
    /// cards / LOD placeholders at the current camera — with NO interaction:
    /// no interaction surface is allocated, no input is read (zoom / keys /
    /// pointer), no overlays / handles / context menu, and nothing is committed.
    /// A display-only render for previews and thumbnails, safe to call inside a
    /// non-interactable `egui::Area` (it registers no interactive widget, so it
    /// can't steal pointer hover from the row beneath it).
    ///
    /// Shares the exact scene-paint path with [`CanvasView::show`] via
    /// [`CanvasView::paint_scene`]; the only difference is everything around it
    /// is skipped. Hand it [`crate::content::NoContentRenderer`] when only frames / LOD
    /// are wanted — at fit/thumbnail zoom every node is a LOD placeholder and the
    /// content renderer is never invoked. status: canvas-static-paint
    pub fn show_static(&mut self, ui: &mut egui::Ui, canvas: &Canvas, content: &mut dyn NodeContentRenderer) {
        let viewport = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        self.paint_scene(ui, viewport, canvas, content, None);
    }

    /// Wheel / pinch input. A pinch always zooms the camera. A wheel over a
    /// full-detail card scrolls that card's content (Ctrl/Cmd+wheel zooms the
    /// card's content); a wheel over empty canvas — or over a group or a LOD
    /// placeholder, neither of which has scrollable content — zooms the camera
    /// toward the cursor.
    /// status: canvas-card-scroll, canvas-card-zoom, canvas-pan-zoom, canvas-lod-placeholder
    fn handle_zoom(&mut self, ui: &egui::Ui, viewport: Rect, canvas: &Canvas) {
        // Under Poincaré the disk is LOCKED to the viewport centre. Affine pan
        // stays OFF (zoom-to-cursor would mutate `pan` and drift the fixed disk —
        // the viewport-lock bug); instead the wheel scales the disk RADIUS about
        // the centre, so it zooms without drifting. Möbius drag-recentre +
        // click-fly-to remain the navigation. [proj-poincare-nav, proj-canvas-mode]
        if self.camera.projection().kind == hiker_projection::ProjectionKind::Poincare {
            let (scroll, cursor) = ui.input(|i| (i.smooth_scroll_delta.y, i.pointer.hover_pos()));
            if scroll != 0.0
                && let Some(cursor) = cursor.filter(|p| viewport.contains(*p))
            {
                // Over a readable (non-group, non-LOD) card → scroll its content,
                // so a zoomed-in note can be read; otherwise scale the locked disk
                // radius about its centre (zoom without drift).
                let world = self.camera.screen_to_world(viewport, cursor);
                let on_card = hiker_canvas::geometry::hit_test(canvas, world).is_some_and(|idx| {
                    let node = &canvas.nodes[idx];
                    let is_group = matches!(node.kind, hiker_canvas::model::NodeKind::Group { .. });
                    let screen =
                        self.camera.world_rect_to_screen(viewport, hiker_canvas::geometry::node_bounds(node));
                    if !is_group && !crate::paint::is_tiny(screen) {
                        let entry = self.card_views.entry(node.id.clone()).or_default();
                        entry.scroll_y = (entry.scroll_y - scroll).max(0.0);
                        true
                    } else {
                        false
                    }
                });
                if !on_card {
                    self.camera.zoom_poincare(scroll);
                }
                ui.ctx().request_repaint();
            }
            return;
        }
        // Keyboard zoom: plain `+`/`=` in, `-` out, `0` fit-to-content. No
        // modifier, so it never fights egui's built-in Cmd/Ctrl+/- whole-UI zoom
        // — and it's the ergonomic way to zoom in scroll-to-pan mode where a
        // plain scroll pans. Gated on no focused editor (inline-edit / edge
        // label) so it can't fire while typing. Zooms around the cursor when it's
        // over the canvas, else the viewport center. status: canvas-scroll-mode
        if self.label_edit.is_none() && ui.ctx().memory(egui::Memory::focused).is_none() {
            let (zin, zout, fit) = ui.input(|i| {
                (
                    i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals),
                    i.key_pressed(Key::Minus),
                    i.key_pressed(Key::Num0),
                )
            });
            if zin || zout || fit {
                let anchor = ui
                    .input(|i| i.pointer.hover_pos())
                    .filter(|p| viewport.contains(*p))
                    .unwrap_or_else(|| viewport.center());
                if fit {
                    self.fit(viewport, canvas);
                } else {
                    let factor = if zin { 1.2 } else { 1.0 / 1.2 };
                    self.camera.zoom_to_cursor(viewport, anchor, factor);
                }
            }
        }
        // Read the source device of this frame's scroll, if any: egui tags each
        // `MouseWheel` event with a unit — `Line` (mouse-wheel notches) vs `Point`
        // (touchpad pixel deltas). winit's Wayland backend keys this off
        // `has_discrete_scroll`, so it's the libinput mouse-vs-touchpad split. We
        // take the latest event's unit and remember it, because egui's scroll
        // smoothing keeps feeding `smooth_scroll_delta` for a few frames after the
        // raw events stop (no `MouseWheel` event on those tail frames).
        // status: canvas-scroll-mode
        let (scroll_xy, scroll, zoom, pointer, ctrl, wheel_unit) = ui.input(|i| {
            let unit = i.events.iter().rev().find_map(|e| match e {
                egui::Event::MouseWheel { unit, .. } => Some(*unit),
                _ => None,
            });
            (i.smooth_scroll_delta, i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos(), i.modifiers.command, unit)
        });
        if let Some(unit) = wheel_unit {
            // `Point` is a touchpad; `Line`/`Page` are a wheel.
            self.last_scroll_was_line = !matches!(unit, egui::MouseWheelUnit::Point);
        }
        let Some(cursor) = pointer.filter(|p| viewport.contains(*p)) else { return };
        // Pinch and Ctrl/Cmd+scroll always zoom to the cursor (egui folds
        // ctrl+scroll into `zoom_delta`). status: canvas-scroll-mode
        if (zoom - 1.0).abs() > f32::EPSILON {
            self.camera.zoom_to_cursor(viewport, cursor, zoom);
            return;
        }
        if scroll_xy.length() <= 0.5 {
            return;
        }
        let world = self.camera.screen_to_world(viewport, cursor);
        if let Some(idx) = hiker_canvas::geometry::hit_test(canvas, world) {
            let node = &canvas.nodes[idx];
            let is_group = matches!(node.kind, hiker_canvas::model::NodeKind::Group { .. });
            let screen = self.camera.world_rect_to_screen(viewport, hiker_canvas::geometry::node_bounds(node));
            // A group or a LOD placeholder (`canvas-lod-placeholder`) has no
            // scrollable content, so the wheel zooms the camera rather than being
            // captured as card scroll. Otherwise a tiny card sliding under the
            // cursor mid-zoom-out would swallow the wheel and stall the zoom.
            if !is_group && !crate::paint::is_tiny(screen) {
                let entry = self.card_views.entry(node.id.clone()).or_default();
                if ctrl {
                    entry.zoom = (entry.zoom * (scroll * 0.0015).exp()).clamp(0.3, 4.0);
                } else {
                    entry.scroll_y = (entry.scroll_y - scroll).max(0.0);
                }
                return;
            }
        }
        // Over empty canvas / a group / a LOD placeholder (nothing with its own
        // scrollable content): pan or zoom per the host's mode. In `Auto`, the
        // remembered source decides — a wheel zooms, a touchpad pans.
        // status: canvas-scroll-mode
        let zooms = match self.scroll_mode {
            ScrollMode::Zoom => true,
            ScrollMode::Pan => false,
            ScrollMode::Auto => self.last_scroll_was_line,
        };
        if zooms {
            let factor = (scroll * 0.0015).exp();
            self.camera.zoom_to_cursor(viewport, cursor, factor);
        } else {
            // Natural two-finger pan — move the camera with the scroll (both
            // axes). `smooth_scroll_delta` already honors the OS scroll direction.
            self.camera.pan_by_screen(scroll_xy);
        }
    }

    /// Keyboard: delete, undo/redo, escape.
    fn handle_keys(&mut self, ui: &egui::Ui, canvas: &mut Canvas, out: &mut CanvasResponse) {
        // Canvas-level shortcuts (Delete/Backspace deletes the selected nodes,
        // Ctrl-Z/-Shift-Z undo/redo, Esc clears) belong to the canvas only when
        // no text editor holds keyboard focus. While a card is in inline-edit
        // mode (or the edge-label editor is open) the focused editor owns these
        // keys — so Backspace deletes text, not the node. status: canvas-delete
        if self.label_edit.is_some() || ui.ctx().memory(egui::Memory::focused).is_some() {
            return;
        }
        let (del, undo, redo, esc) = ui.input(|i| {
            let cmd = i.modifiers.command;
            (
                i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
                cmd && !i.modifiers.shift && i.key_pressed(Key::Z),
                cmd && i.modifiers.shift && i.key_pressed(Key::Z),
                i.key_pressed(Key::Escape),
            )
        });
        if esc {
            self.selection.clear();
            self.interaction = Interaction::Idle;
            self.pending_create = None;
            self.pending_insert = None;
            self.pending_group_draw = false;
            self.label_edit = None;
        }
        if del && !self.selection.is_empty() {
            let pre = canvas.clone();
            let ops = interaction::delete_selection(canvas, &self.selection);
            self.commit_each(&pre, ops, out);
            self.selection.clear();
        }
        if undo {
            if let Some(op) = self.undo.take_undo(canvas) {
                op.apply(canvas);
                out.committed.push(op);
            }
        }
        if redo {
            if let Some(op) = self.undo.take_redo(canvas) {
                op.apply(canvas);
                out.committed.push(op);
            }
        }
    }

    /// Keyboard tool switch: `V` selects the Select tool, `H` the Hand tool.
    /// Guarded so it never hijacks typing — honored only when egui reports no
    /// focused widget (a text node / JSON editor / `TextEdit` would hold focus)
    /// and no inline edge-label editor is open. status: canvas-tool-mode
    fn handle_tool_keys(&mut self, ui: &egui::Ui) {
        if self.label_edit.is_some() || ui.ctx().memory(egui::Memory::focused).is_some() {
            return;
        }
        let (v, h) = ui.input(|i| (i.key_pressed(Key::V), i.key_pressed(Key::H)));
        if v {
            self.tool = Tool::Select;
        } else if h {
            self.tool = Tool::Hand;
        }
    }

    /// Universal pan via the middle mouse button. egui's drag `Sense` reports the
    /// PRIMARY button only, so a middle-button drag never arrives through the
    /// normal drag response — read it from raw input here: while the middle
    /// button is down and the pointer is over the viewport, pan by its delta.
    /// status: canvas-tool-mode
    fn handle_middle_pan(&mut self, ui: &egui::Ui, viewport: Rect) {
        let (middle, delta, hover) = ui.input(|i| {
            (i.pointer.middle_down(), i.pointer.delta(), i.pointer.hover_pos())
        });
        if middle && hover.is_some_and(|p| viewport.contains(p)) && delta != egui::Vec2::ZERO {
            self.camera.pan_by_screen(delta);
        }
    }

    /// While panning is available (Hand tool, Space held, or a middle-button
    /// drag) show the grab cursor over the viewport — `Grabbing` while a pan is
    /// actually in progress, `Grab` when merely available. status: canvas-tool-mode
    fn apply_pan_cursor(&self, ui: &egui::Ui, viewport: Rect, response: &egui::Response) {
        let (space, middle, hover) = ui.input(|i| {
            (i.key_down(Key::Space), i.pointer.middle_down(), i.pointer.hover_pos())
        });
        if !hover.is_some_and(|p| viewport.contains(p)) {
            return;
        }
        let pan_available = matches!(self.tool, Tool::Hand) || space || middle;
        if !pan_available {
            return;
        }
        let panning = matches!(self.interaction, Interaction::Pan) || (middle && response.is_pointer_button_down_on());
        let icon = if panning || (response.dragged() && matches!(self.interaction, Interaction::Pan)) {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        };
        ui.ctx().set_cursor_icon(icon);
    }

    /// Record `ops` (already applied to the live canvas) on the undo stack and
    /// report them. `pre` is the canvas state before the first op; we re-walk it
    /// so each op's inverse is captured against its own pre-state.
    fn commit_each(&mut self, pre: &Canvas, ops: Vec<EditOp>, out: &mut CanvasResponse) {
        let mut walk = pre.clone();
        for op in ops {
            self.undo.record(&op, &walk);
            op.apply(&mut walk);
            out.committed.push(op);
        }
    }

    /// Record a single just-applied op (pre-state supplied) and report it.
    fn commit_one(&mut self, op: EditOp, pre: &Canvas, out: &mut CanvasResponse) {
        self.undo.record(&op, pre);
        out.committed.push(op);
    }

    /// Resolve which interactive handles the pointer is over, reusing the exact
    /// hit-tests the press path uses (no parallel logic): the resize handle of a
    /// single selection, the connector handle on any node, and a group's header
    /// grab-strip. Returns an all-`None` bundle when no hover is over the
    /// viewport, while in a drag (so a gesture isn't cluttered), or while typing
    /// a label. status: canvas-handle-hover
    fn handle_hover(&self, ui: &egui::Ui, viewport: Rect, canvas: &Canvas) -> HandleHover {
        if !matches!(self.interaction, Interaction::Idle | Interaction::Connecting { .. }) {
            return HandleHover::default();
        }
        let Some(p) = ui.input(|i| i.pointer.hover_pos()).filter(|p| viewport.contains(*p)) else {
            return HandleHover::default();
        };
        let world = self.camera.screen_to_world(viewport, p);
        HandleHover {
            resize: interaction::single_selected_handle(canvas, &self.camera, viewport, &self.selection, p),
            connector: interaction::hovered_side_handle(canvas, &self.camera, viewport, p),
            group_header: interaction::group_header_hit(canvas, world),
        }
    }

    /// First pass: paint grid, group backgrounds, edges, and node cards with
    /// their (display-only) content. Registers BEFORE the interaction surface.
    /// Each card renders per its [`CardView`] (zoom + scroll); the effective
    /// (clamped) scroll the body settled on is stored back as the card's state.
    fn paint_scene(&mut self, ui: &mut egui::Ui, viewport: Rect, canvas: &Canvas, content: &mut dyn NodeContentRenderer, header_hover: Option<&str>) {
        // Refresh the projection lens framing once per frame from the canvas
        // content bounds (focus = bounds center, scale = half the diagonal),
        // before any paint or hit-test reads the lens. A no-op for the Off lens
        // and for an empty canvas. [proj-canvas-mode]
        self.camera.update_lens(content_bounds(canvas));
        let dark = ui.visuals().dark_mode;
        let visuals = ui.visuals().clone();
        let bg = ui.painter().with_clip_rect(viewport);
        if self.show_grid {
            paint::grid(&bg, viewport, &self.camera, GRID_STEP, dark);
        }
        paint::group_backgrounds(&bg, viewport, &self.camera, canvas, &visuals, header_hover);
        paint::edges(&bg, viewport, &self.camera, canvas, &visuals, &|id| self.selection.has_edge(id));
        paint::poincare_boundary(&bg, viewport, &self.camera, &visuals);
        let camera = self.camera;
        // Pre-pass: under a lens, size each card to fill the gap to its nearest
        // on-screen neighbour so sparse regions of the disk fill out instead of
        // floating tiny cards. `None` per node with the lens Off (affine sizing).
        // [proj-card-fill]
        let fills = paint::lens_fill_scales(&camera, viewport, canvas);
        for (node, fill) in canvas.nodes.iter().zip(fills) {
            let view = self.card_view(&node.id);
            let effective_scroll = paint::node_card_filled(ui, viewport, &camera, node, content, view, fill);
            if (effective_scroll - view.scroll_y).abs() > f32::EPSILON {
                self.card_views.entry(node.id.clone()).or_default().scroll_y = effective_scroll;
            }
        }
    }

    /// The per-card content view (zoom + scroll) for `id`, defaulting to 1.0
    /// zoom / 0 scroll for a card not yet touched.
    fn card_view(&self, id: &str) -> CardView {
        self.card_views.get(id).copied().unwrap_or_default()
    }

    /// Second pass (after input): selection outlines + resize handles, the
    /// connector handles on the hovered/selected node, and any in-progress
    /// rubber band. Painted on top of the interaction surface, reflecting the
    /// input just handled this frame.
    fn paint_overlays(&self, ui: &egui::Ui, viewport: Rect, canvas: &Canvas, hover: &HandleHover) {
        let visuals = ui.visuals().clone();
        let accent = visuals.selection.stroke.color;
        let p = ui.painter().with_clip_rect(viewport);
        let single = self.selection.nodes.len() == 1 && self.selection.edges.is_empty();
        for node in &canvas.nodes {
            if !self.selection.has_node(&node.id) {
                continue;
            }
            let screen = self.camera.world_rect_to_screen(viewport, node_bounds(node));
            paint::selection_outline(&p, screen, &self.camera, accent);
            // Resize handles paint for any singly-selected node, groups included.
            // The handle under the cursor grows as a hover affordance.
            // status: canvas-group-resize, canvas-handle-hover
            if single {
                paint::resize_handles(&p, screen, accent, hover.resize);
            }
        }
        self.paint_connectors(ui, viewport, canvas, &p, accent, hover.connector.as_ref());
        self.paint_drag_overlay(ui, viewport, canvas, &p, accent);
    }

    /// Paint the four connector handles on the node the pointer is over (and on
    /// any selected node), and on the connection's origin node while drawing.
    /// Suppressed mid-move/resize/marquee/pan so it doesn't clutter a drag. `hot`
    /// is the pre-resolved connector `(node, side)` under the cursor; the circle
    /// on that side grows as a hover affordance. status: canvas-edge-draw, canvas-handle-hover
    fn paint_connectors(
        &self,
        ui: &egui::Ui,
        viewport: Rect,
        canvas: &Canvas,
        p: &egui::Painter,
        accent: egui::Color32,
        hot: Option<&(String, Side)>,
    ) {
        if !matches!(self.interaction, Interaction::Idle | Interaction::Connecting { .. }) {
            return;
        }
        let hovered_id = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|h| viewport.contains(*h))
            .and_then(|h| {
                let w = self.camera.screen_to_world(viewport, h);
                hiker_canvas::geometry::hit_test(canvas, w).map(|i| canvas.nodes[i].id.clone())
            });
        for node in &canvas.nodes {
            if matches!(node.kind, hiker_canvas::model::NodeKind::Group { .. }) {
                continue;
            }
            let connecting_from = matches!(&self.interaction, Interaction::Connecting { from_node, .. } if from_node == &node.id);
            let show = connecting_from
                || hovered_id.as_deref() == Some(node.id.as_str())
                || self.selection.has_node(&node.id);
            if !show {
                continue;
            }
            let screen = self.camera.world_rect_to_screen(viewport, node_bounds(node));
            let hot_side = hot.filter(|(id, _)| id == &node.id).map(|(_, s)| *s);
            paint::connector_handles(p, screen, accent, interaction::SIDE_HANDLE_R, hot_side);
        }
    }

    /// Paint the in-progress marquee rect, the edge-draw / click-to-connect
    /// rubber band.
    fn paint_drag_overlay(&self, ui: &egui::Ui, viewport: Rect, canvas: &Canvas, p: &egui::Painter, accent: egui::Color32) {
        let pointer = ui.input(|i| i.pointer.hover_pos());
        match &self.interaction {
            Interaction::Marquee { origin } => {
                let Some(cur) = pointer else { return };
                let a = self.camera.world_to_screen(viewport, *origin);
                let r = Rect::from_two_pos(a, cur);
                p.rect_filled(r, 0.0, accent.gamma_multiply(0.08));
                p.rect_stroke(r, 0.0, egui::Stroke::new(1.0, accent), egui::StrokeKind::Inside);
            }
            // The live group-draw preview: a dashed-feel outline of the rect the
            // release will create. status: canvas-group-draw
            Interaction::DrawGroup { origin } => {
                let Some(cur) = pointer else { return };
                let a = self.camera.world_to_screen(viewport, *origin);
                let r = Rect::from_two_pos(a, cur);
                p.rect_filled(r, 4.0, accent.gamma_multiply(0.05));
                p.rect_stroke(r, 4.0, egui::Stroke::new(1.5, accent), egui::StrokeKind::Inside);
            }
            Interaction::EdgeDrag { from_node, from_side, .. }
            | Interaction::Connecting { from_node, from_side } => {
                let Some(cur) = pointer else { return };
                let Some(node) = canvas.nodes.iter().find(|n| &n.id == from_node) else { return };
                let a = anchor_pos(node, *from_side);
                let start = self.camera.world_to_screen(viewport, Point::new(f64::from(a.x), f64::from(a.y)));
                p.line_segment([start, cur], egui::Stroke::new(2.0, accent));
            }
            _ => {}
        }
    }

    /// Render the inline edge-label editor (when open) as a small popup at the
    /// edge's midpoint, registered last so its field sits above the interaction
    /// surface. Enter / click-outside commits an [`EditOp::SetEdgeLabel`]; Esc
    /// cancels. status: canvas-edge-label
    fn draw_label_editor(&mut self, ui: &mut egui::Ui, viewport: Rect, canvas: &mut Canvas, out: &mut CanvasResponse) {
        let Some(mut edit) = self.label_edit.take() else { return };
        let Some(mid) = interaction::edge_midpoint_screen(canvas, &self.camera, viewport, &edit.edge_id) else {
            return; // edge vanished (e.g. a node it touched was deleted) — drop it.
        };
        let mut commit = false;
        let mut cancel = false;
        let area = egui::Area::new(ui.id().with(("canvas-edge-label", edit.edge_id.clone())))
            .order(egui::Order::Foreground)
            .fixed_pos(mid - egui::vec2(75.0, 14.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut edit.draft)
                            .desired_width(140.0)
                            .hint_text("edge label"),
                    );
                    field.request_focus();
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        commit = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        cancel = true;
                    }
                });
            });
        if area.response.clicked_elsewhere() {
            commit = true;
        }
        if commit {
            let label = edit.draft.trim();
            let label = (!label.is_empty()).then(|| label.to_owned());
            let pre = canvas.clone();
            let op = EditOp::SetEdgeLabel { id: edit.edge_id.clone(), label };
            op.apply(canvas);
            self.commit_one(op, &pre, out);
        } else if !cancel {
            self.label_edit = Some(edit);
        }
    }

    /// Right-click context menu on the interaction surface, dispatched by what
    /// the click landed on: a node card (zoom + delete), an edge (edit label +
    /// delete), or empty canvas (the toolbar's create / insert / fit verbs, so
    /// the toolbar is reachable without leaving the canvas). status: canvas-context-menu
    fn show_context_menu(
        &mut self,
        response: &egui::Response,
        viewport: Rect,
        canvas: &mut Canvas,
        menu: &mut dyn CanvasMenuRenderer,
        out: &mut CanvasResponse,
    ) {
        let anchor = self.menu_anchor;
        response.context_menu(|ui| {
            let target = anchor.map(|p| interaction::resolve_target(canvas, &self.camera, viewport, &self.selection, p));
            match target {
                Some(Target::Node(id) | Target::SideHandle { node: id, .. }) => {
                    if let Some(action) = menu.node_menu(ui) {
                        self.apply_node_menu(action, &id, canvas, out);
                    }
                }
                Some(Target::ResizeHandle(_)) => {
                    if let Some(id) = self.selection.nodes.iter().next().cloned() {
                        if let Some(action) = menu.node_menu(ui) {
                            self.apply_node_menu(action, &id, canvas, out);
                        }
                    }
                }
                Some(Target::Edge(id)) => {
                    if let Some(action) = menu.edge_menu(ui) {
                        self.apply_edge_menu(action, &id, canvas, out);
                    }
                }
                _ => {
                    if let Some(action) = menu.empty_menu(ui) {
                        self.apply_empty_menu(action, viewport, canvas, out);
                    }
                }
            }
        });
    }

    /// Apply a chosen node-menu verb through the widget's existing pipeline:
    /// zoom verbs mutate the card's view state; `Delete` runs the same selection
    /// delete + undo recording as the toolbar. status: ctxmenu-canvas
    fn apply_node_menu(&mut self, action: menu::NodeMenuAction, id: &str, canvas: &mut Canvas, out: &mut CanvasResponse) {
        use crate::menu::NodeMenuAction::{Delete, ResetZoom, ZoomIn, ZoomOut};
        match action {
            ZoomIn | ZoomOut | ResetZoom => {
                let entry = self.card_views.entry(id.to_owned()).or_default();
                entry.zoom = match action {
                    ZoomIn => (entry.zoom * 1.25).clamp(0.3, 4.0),
                    ZoomOut => (entry.zoom / 1.25).clamp(0.3, 4.0),
                    _ => 1.0,
                };
            }
            Delete => {
                let mut sel = Selection::default();
                sel.nodes.insert(id.to_owned());
                let pre = canvas.clone();
                let ops = interaction::delete_selection(canvas, &sel);
                self.commit_each(&pre, ops, out);
                self.selection.nodes.remove(id);
            }
        }
    }

    /// Apply a chosen edge-menu verb: open the inline label editor, or delete the
    /// edge through the `EditOp` + undo pipeline.
    /// status: canvas-edge-label, canvas-delete, ctxmenu-canvas
    fn apply_edge_menu(&mut self, action: menu::EdgeMenuAction, id: &str, canvas: &mut Canvas, out: &mut CanvasResponse) {
        match action {
            menu::EdgeMenuAction::EditLabel => {
                let draft = canvas.edges.iter().find(|e| e.id == id).and_then(|e| e.label.clone()).unwrap_or_default();
                self.label_edit = Some(LabelEdit { edge_id: id.to_owned(), draft });
            }
            menu::EdgeMenuAction::Delete => self.delete_edge(id, canvas, out),
        }
    }

    /// Remove an edge (if it still exists) through the `EditOp` + undo pipeline.
    /// status: canvas-delete
    fn delete_edge(&mut self, id: &str, canvas: &mut Canvas, out: &mut CanvasResponse) {
        if !canvas.edges.iter().any(|e| e.id == id) {
            return;
        }
        let pre = canvas.clone();
        let op = EditOp::RemoveEdge { id: id.to_owned() };
        op.apply(canvas);
        self.commit_one(op, &pre, out);
        self.selection.edges.remove(id);
    }

    /// Apply a chosen empty-space verb: the toolbar's create / insert / fit verbs.
    /// Text and group create immediately (queued for the next `show`); link /
    /// vault-insert need host-side UI, so they're reported as requests in
    /// [`CanvasResponse`]. status: canvas-node-create, canvas-context-menu, ctxmenu-canvas
    fn apply_empty_menu(&mut self, action: menu::EmptyMenuAction, viewport: Rect, canvas: &mut Canvas, out: &mut CanvasResponse) {
        use crate::menu::EmptyMenuAction::{
            AddGroup, AddLink, AddText, AutoArrange, FitToContent, InsertFromVault, NewNote,
        };
        match action {
            AddText => self.create_centered(CreateKind::Text),
            NewNote => out.request_new_note = true,
            AddLink => out.request_link_prompt = true,
            InsertFromVault => out.request_insert_picker = true,
            AddGroup => self.arm_group_draw(),
            AutoArrange => self.auto_arrange(canvas, out),
            FitToContent => self.fit(viewport, canvas),
        }
    }

    /// Tidy the whole board with a dagre auto-arrange: compute the pure
    /// `SetNodeRect` ops in the core, apply each to the live canvas, and record
    /// them on the undo stack / report them (so the op-log captures the tidy and
    /// a single undo reverts the move). status: canvas-auto-arrange
    fn auto_arrange(&mut self, canvas: &mut Canvas, out: &mut CanvasResponse) {
        use hiker_canvas::tidy::{auto_arrange, ArrangeOpts};
        let ops = auto_arrange(canvas, ArrangeOpts::default());
        if ops.is_empty() {
            return;
        }
        let pre = canvas.clone();
        for op in &ops {
            op.apply(canvas);
        }
        self.commit_each(&pre, ops, out);
    }
}

mod pointer;

#[cfg(test)]
mod tests {
    use super::{CanvasResponse, CanvasView};
    use canvas_view_core::state::CreateKind;
    use egui::{Pos2, Rect, Vec2};
    use hiker_canvas::model::{Canvas, Node, NodeKind};
    use hiker_canvas::ops::EditOp;
    use std::collections::BTreeMap;

    fn viewport() -> Rect {
        // Default camera: pan (0,0), scale 1 — so a screen point maps to the same
        // world point offset from the viewport origin.
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0))
    }

    #[test]
    fn create_centered_commits_add_node_at_center_and_selects() {
        let mut view = CanvasView::new();
        let mut canvas = Canvas::default();
        let mut out = CanvasResponse::default();
        view.create_centered(CreateKind::Text);
        view.consume_pending(&mut canvas, viewport(), &mut out);

        assert_eq!(out.committed.len(), 1, "one AddNode committed");
        assert_eq!(canvas.nodes.len(), 1, "node applied to the live canvas");
        let node = &canvas.nodes[0];
        let (w, h) = CreateKind::Text.default_size();
        // Viewport center is (400, 300); top-left = center - half-size.
        assert_eq!((node.x, node.y), (400 - w / 2, 300 - h / 2));
        assert!(matches!(out.committed[0], EditOp::AddNode { .. }));
        assert!(view.selection().has_node(&node.id), "new node is selected");
        assert_eq!(view.selection().nodes.len(), 1, "sole selection");
    }

    #[test]
    fn insert_node_centered_positions_built_node_and_selects() {
        let mut view = CanvasView::new();
        let mut canvas = Canvas::default();
        let mut out = CanvasResponse::default();
        let node = Node {
            id: "caller-id".into(),
            x: -999,
            y: -999,
            width: 300,
            height: 200,
            color: None,
            kind: NodeKind::File { file: "notes/a.md".into(), subpath: None },
            extra: BTreeMap::new(),
        };
        view.insert_node_centered(node);
        view.consume_pending(&mut canvas, viewport(), &mut out);

        assert_eq!(canvas.nodes.len(), 1);
        let placed = &canvas.nodes[0];
        // Centered: top-left = (400 - 150, 300 - 100); caller id replaced.
        assert_eq!((placed.x, placed.y), (250, 200));
        assert_ne!(placed.id, "caller-id", "id reassigned to a canvas-unique one");
        assert!(matches!(placed.kind, NodeKind::File { .. }), "kind + fields preserved");
        assert!(view.selection().has_node(&placed.id));
        assert_eq!(out.committed.len(), 1);
    }

    #[test]
    fn view_snapshot_round_trips_camera_and_cards() {
        use canvas_view_core::camera::Camera;
        use crate::content::CardView;
        use hiker_canvas::geometry::Point;

        let mut view = CanvasView::new();
        // Pan + zoom the camera (zoom toward a cursor so pan moves too), then
        // give one card a non-default scroll/zoom via the wheel path's store.
        view.camera = {
            let mut cam = Camera::default();
            cam.set_pan_scale(Point::new(-50.0, 12.5), 0.6);
            cam
        };
        view.card_views.insert("n1".into(), CardView { zoom: 1.5, scroll_y: 42.0 });

        let (pan, scale, cards) = view.view_snapshot();
        assert!((scale - 0.6).abs() < 1e-6);

        let mut restored = CanvasView::new();
        restored.restore_view(pan, scale, cards);
        assert!((restored.camera().scale() - 0.6).abs() < 1e-6);
        assert!((restored.camera().pan().x - (-50.0)).abs() < 1e-9);
        assert_eq!(restored.card_scroll("n1"), 42.0);
    }

    #[test]
    fn consume_pending_is_a_no_op_without_a_request() {
        let mut view = CanvasView::new();
        let mut canvas = Canvas::default();
        let mut out = CanvasResponse::default();
        view.consume_pending(&mut canvas, viewport(), &mut out);
        assert!(out.committed.is_empty());
        assert!(canvas.nodes.is_empty());
    }

    fn sample_canvas() -> Canvas {
        let mut canvas = Canvas::default();
        canvas.nodes.push(Node {
            id: "a".into(),
            x: 0,
            y: 0,
            width: 200,
            height: 120,
            color: None,
            kind: NodeKind::Text { text: "hello".into() },
            extra: BTreeMap::new(),
        });
        canvas.nodes.push(Node {
            id: "b".into(),
            x: 400,
            y: 240,
            width: 200,
            height: 120,
            color: None,
            kind: NodeKind::File { file: "notes/x.md".into(), subpath: None },
            extra: BTreeMap::new(),
        });
        canvas
    }

    /// `show_static` drives one display-only paint through a real `Ui` without
    /// panicking and — crucially — registers NO interactive widget, so it can
    /// live inside a non-interactable `Area` without stealing pointer hover.
    /// status: canvas-static-paint
    #[test]
    fn show_static_paints_without_registering_interaction() {
        use crate::content::NoContentRenderer;
        let canvas = sample_canvas();
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let mut view = CanvasView::new();
            view.fit(ui.available_rect_before_wrap(), &canvas);
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });
        harness.run();
        // A non-interactable scene: no widget claimed pointer interest, so the
        // ctx reports nothing wants pointer / keyboard input.
        assert!(!harness.ctx.wants_pointer_input(), "static paint must not sense the pointer");
        assert!(!harness.ctx.wants_keyboard_input(), "static paint must not grab keyboard focus");
    }

    /// A `NoContentRenderer` renderer paints nothing and echoes the card's scroll back
    /// unchanged. status: canvas-static-paint
    #[test]
    fn no_content_echoes_scroll() {
        use crate::content::{CardView, NoContentRenderer};
        use crate::content::NodeContentRenderer;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let node = Node {
                id: "a".into(),
                x: 0,
                y: 0,
                width: 100,
                height: 60,
                color: None,
                kind: NodeKind::Text { text: String::new() },
                extra: BTreeMap::new(),
            };
            let view = CardView { zoom: 1.0, scroll_y: 17.0 };
            let echoed = NoContentRenderer.render(ui, &node, ui.max_rect(), view);
            assert_eq!(echoed, 17.0);
        });
        harness.run();
    }
}
