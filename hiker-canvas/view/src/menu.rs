//! The canvas context-menu host seam and its per-target action contract.
//!
//! Right-clickable targets (node, edge, empty space) each have a per-target
//! action enum — plain data, no menu-library types — and the widget applies a
//! chosen action through its own `EditOp` / undo pipeline (and [`CanvasResponse`]
//! request flags), never via an app-side `ctx.defer`. status: ctxmenu-canvas
//!
//! Building and rendering the actual menu rows is the HOST's job, mirroring the
//! [`NodeContentRenderer`](crate::content::NodeContentRenderer) content seam: the
//! host supplies a [`CanvasMenuRenderer`] that draws the menu into the
//! context-menu `ui` and returns the chosen action, which the widget then
//! applies. This keeps the lean widget crate free of any menu-library dependency
//! — the app, which owns the menu primitive, builds the menus.

/// What the user chose from a node card's context menu. Applied by the widget in
/// its `EditOp` / undo pipeline (`Delete`) or by mutating the card's view state
/// (the zoom verbs). status: ctxmenu-canvas
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeMenuAction {
    /// Increase the card's content zoom one step.
    ZoomIn,
    /// Decrease the card's content zoom one step.
    ZoomOut,
    /// Restore the card's content zoom to 1.0.
    ResetZoom,
    /// Remove the node (and its incident edges) via `EditOp::RemoveNode`.
    Delete,
}

/// What the user chose from an edge's context menu. status: ctxmenu-canvas
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeMenuAction {
    /// Open the inline edge-label editor.
    EditLabel,
    /// Remove the edge via `EditOp::RemoveEdge`.
    Delete,
}

/// What the user chose from the empty-canvas context menu — the toolbar's
/// create / insert / fit verbs, reachable without leaving the canvas.
/// status: ctxmenu-canvas
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmptyMenuAction {
    /// Drop a new empty text card at the viewport center.
    AddText,
    /// Ask the host to create a brand-new vault note and drop a pointer to it.
    NewNote,
    /// Ask the host to open the "+ Link" URL prompt.
    AddLink,
    /// Ask the host to open the "Insert from vault" picker.
    InsertFromVault,
    /// Arm the next drag to draw a group rectangle.
    AddGroup,
    /// Fit the camera to the canvas content.
    FitToContent,
}

/// The host seam for the canvas's right-click menus, mirroring
/// [`NodeContentRenderer`](crate::content::NodeContentRenderer).
///
/// The widget resolves which target a right-click landed on, then calls the
/// matching method with the open context-menu `ui`. The host draws the menu rows
/// (with whatever menu primitive it owns) and returns the chosen action, which
/// the widget applies through its existing `EditOp` / undo / request-flag paths.
/// The menus are static (the widget already knows the target id and applies the
/// effect), so `&mut egui::Ui` is all the host needs. status: ctxmenu-canvas
pub trait CanvasMenuRenderer {
    /// Draw the node card's context menu and return the chosen action, if any.
    fn node_menu(&mut self, ui: &mut egui::Ui) -> Option<NodeMenuAction>;
    /// Draw an edge's context menu and return the chosen action, if any.
    fn edge_menu(&mut self, ui: &mut egui::Ui) -> Option<EdgeMenuAction>;
    /// Draw the empty-canvas context menu and return the chosen action, if any.
    fn empty_menu(&mut self, ui: &mut egui::Ui) -> Option<EmptyMenuAction>;
}
