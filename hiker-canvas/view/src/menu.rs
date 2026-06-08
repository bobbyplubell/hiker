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
/// its `EditOp` / undo pipeline (`Delete`), by mutating the card's view state
/// (the zoom verbs), or reported as a host request (`OpenInNewTab`).
/// status: ctxmenu-canvas
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeMenuAction {
    /// Increase the card's content zoom one step.
    ZoomIn,
    /// Decrease the card's content zoom one step.
    ZoomOut,
    /// Restore the card's content zoom to 1.0.
    ResetZoom,
    /// Open the node's target (a File node's referenced file, a Link node's URL)
    /// in a NEW host tab, reported as [`CanvasResponse::request_open_in_new_tab`].
    /// The widget itself stays content-display-only; opening is the host's job.
    /// Only offered (enabled) when the node has an openable target — see
    /// [`NodeOpenTarget`]. status: canvas-open-in-new-tab
    OpenInNewTab,
    /// Remove the node (and its incident edges) via `EditOp::RemoveNode`.
    Delete,
}

/// Whether a node has a target the host can open, used to enable / disable the
/// "Open in new tab" menu item per node kind. The widget resolves this from the
/// right-clicked node's kind and hands it to [`CanvasMenuRenderer::node_menu`];
/// the host greys the item (with a reason) when [`NodeOpenTarget::None`].
/// status: canvas-open-in-new-tab
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeOpenTarget {
    /// A File node referencing a non-empty path, or a Link node with a non-empty
    /// URL: "Open in new tab" is enabled.
    Openable,
    /// A Text or Group node (or a File/Link node with an empty target): nothing
    /// to open, so the item is disabled.
    None,
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
    /// Tidy the board with a dagre auto-arrange (hierarchical ranked layout).
    AutoArrange,
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
    /// `target` describes whether the right-clicked node has an openable target,
    /// so the host can enable / disable the "Open in new tab" item accordingly.
    fn node_menu(&mut self, ui: &mut egui::Ui, target: NodeOpenTarget) -> Option<NodeMenuAction>;
    /// Draw an edge's context menu and return the chosen action, if any.
    fn edge_menu(&mut self, ui: &mut egui::Ui) -> Option<EdgeMenuAction>;
    /// Draw the empty-canvas context menu and return the chosen action, if any.
    fn empty_menu(&mut self, ui: &mut egui::Ui) -> Option<EmptyMenuAction>;
}
