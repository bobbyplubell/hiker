//! The hiker app's canvas context menus, built on `egui_workbench::menu`.
//!
//! The lean `canvas-view` widget owns no menu library; it exposes a
//! [`CanvasMenuRenderer`](canvas_view::menu::CanvasMenuRenderer) host seam and
//! applies the returned action through its own `EditOp` / undo / request-flag
//! pipeline. This module is the app-side implementation of that seam: one pure
//! builder per target (returning a [`Menu`](egui_workbench::menu::Menu) over the
//! widget's plain action enum), plus a [`CanvasMenus`] that renders them with the
//! shared menu renderer. Same items, order, and `extend` composition as before —
//! the node menu is the canvas-only zoom section spliced onto a shared `Delete`
//! base. status: ctxmenu-canvas
//!
//! The zoom controls are three discrete actions, so they stay plain `action`
//! entries; the `Custom` live-widget escape hatch isn't needed here.

use canvas_view::menu::{CanvasMenuRenderer, EdgeMenuAction, EmptyMenuAction, NodeMenuAction};
use egui_workbench::menu::Menu;

/// The shared node base: the options any surface showing a node offers. Today
/// just `Delete`; the canvas splices its own section onto this with
/// [`Menu::extend`]. status: ctxmenu-canvas
fn node_base_menu() -> Menu<NodeMenuAction> {
    Menu::new().action("Delete", NodeMenuAction::Delete)
}

/// The canvas-only contextual section spliced onto [`node_base_menu`]: per-card
/// content zoom. status: ctxmenu-canvas
fn node_zoom_section() -> Menu<NodeMenuAction> {
    Menu::new()
        .action("Zoom in", NodeMenuAction::ZoomIn)
        .action("Zoom out", NodeMenuAction::ZoomOut)
        .action("Reset zoom", NodeMenuAction::ResetZoom)
}

/// The full node menu: the canvas-only zoom section, then a separator, then the
/// shared `Delete` base spliced in with [`Menu::extend`] — the contextual
/// composition the spec describes (base + host section), ordered to match the
/// canvas's existing layout. `extend` keeps the base as its own group, so the
/// renderer draws the separator between them. status: ctxmenu-canvas
fn build_node_menu() -> Menu<NodeMenuAction> {
    node_zoom_section().extend(node_base_menu())
}

/// The edge menu: edit the label, or delete the edge. status: ctxmenu-canvas
fn build_edge_menu() -> Menu<EdgeMenuAction> {
    Menu::new()
        .action("Edit label…", EdgeMenuAction::EditLabel)
        .action("Delete", EdgeMenuAction::Delete)
}

/// The empty-canvas menu: the toolbar's create / insert verbs, then a separator,
/// then fit-to-content. status: ctxmenu-canvas
fn build_empty_menu() -> Menu<EmptyMenuAction> {
    Menu::new()
        .action("Add text", EmptyMenuAction::AddText)
        .action("New note", EmptyMenuAction::NewNote)
        .action("Add link…", EmptyMenuAction::AddLink)
        .action("Insert from vault…", EmptyMenuAction::InsertFromVault)
        .action("Add group", EmptyMenuAction::AddGroup)
        .section()
        .action("Auto-arrange", EmptyMenuAction::AutoArrange)
        .action("Fit to content", EmptyMenuAction::FitToContent)
}

/// The app's [`CanvasMenuRenderer`]: it renders the canvas's right-click menus
/// with the shared `egui_workbench::menu` renderer and hands the chosen action
/// back to the widget, which applies it. Stateless — the widget already knows the
/// target id and applies the effect. status: ctxmenu-canvas
pub(crate) struct CanvasMenus;

impl CanvasMenuRenderer for CanvasMenus {
    fn node_menu(&mut self, ui: &mut egui::Ui) -> Option<NodeMenuAction> {
        egui_workbench::menu::show(ui, build_node_menu())
    }

    fn edge_menu(&mut self, ui: &mut egui::Ui) -> Option<EdgeMenuAction> {
        egui_workbench::menu::show(ui, build_edge_menu())
    }

    fn empty_menu(&mut self, ui: &mut egui::Ui) -> Option<EmptyMenuAction> {
        egui_workbench::menu::show(ui, build_empty_menu())
    }
}

#[cfg(test)]
mod tests {
    use super::{build_edge_menu, build_empty_menu, build_node_menu, node_base_menu};
    use canvas_view::menu::{EdgeMenuAction, EmptyMenuAction, NodeMenuAction};
    use egui_workbench::menu::Entry;

    /// Pull the action out of a plain `Action` entry, asserting its label so the
    /// menu's visible order is locked in.
    fn action_of<A: Copy>(entry: &Entry<A>, expect_label: &str) -> A {
        match entry {
            Entry::Action { label, action, .. } => {
                assert_eq!(label, expect_label, "entry label");
                *action
            }
            _ => panic!("expected an Action entry for {expect_label}"),
        }
    }

    #[test]
    fn node_menu_is_zoom_section_then_delete_base() {
        let menu = build_node_menu();
        let sections = menu.sections();
        // Two separator-delimited groups: zoom controls, then the shared base.
        assert_eq!(sections.len(), 2, "zoom section + base section");
        assert_eq!(action_of(&sections[0][0], "Zoom in"), NodeMenuAction::ZoomIn);
        assert_eq!(action_of(&sections[0][1], "Zoom out"), NodeMenuAction::ZoomOut);
        assert_eq!(action_of(&sections[0][2], "Reset zoom"), NodeMenuAction::ResetZoom);
        assert_eq!(sections[0].len(), 3, "exactly the three zoom verbs");
        assert_eq!(action_of(&sections[1][0], "Delete"), NodeMenuAction::Delete);
        assert_eq!(sections[1].len(), 1, "base is just Delete");
    }

    #[test]
    fn node_base_is_just_delete() {
        let base = node_base_menu();
        assert_eq!(base.sections().len(), 1);
        assert_eq!(action_of(&base.sections()[0][0], "Delete"), NodeMenuAction::Delete);
    }

    #[test]
    fn edge_menu_is_edit_then_delete() {
        let menu = build_edge_menu();
        let sections = menu.sections();
        assert_eq!(sections.len(), 1, "one group");
        assert_eq!(action_of(&sections[0][0], "Edit label…"), EdgeMenuAction::EditLabel);
        assert_eq!(action_of(&sections[0][1], "Delete"), EdgeMenuAction::Delete);
        assert_eq!(sections[0].len(), 2);
    }

    #[test]
    fn empty_menu_is_create_verbs_then_fit() {
        let menu = build_empty_menu();
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "create group + fit group");
        assert_eq!(action_of(&sections[0][0], "Add text"), EmptyMenuAction::AddText);
        assert_eq!(action_of(&sections[0][1], "New note"), EmptyMenuAction::NewNote);
        assert_eq!(action_of(&sections[0][2], "Add link…"), EmptyMenuAction::AddLink);
        assert_eq!(
            action_of(&sections[0][3], "Insert from vault…"),
            EmptyMenuAction::InsertFromVault
        );
        assert_eq!(action_of(&sections[0][4], "Add group"), EmptyMenuAction::AddGroup);
        assert_eq!(sections[0].len(), 5);
        assert_eq!(action_of(&sections[1][0], "Auto-arrange"), EmptyMenuAction::AutoArrange);
        assert_eq!(action_of(&sections[1][1], "Fit to content"), EmptyMenuAction::FitToContent);
        assert_eq!(sections[1].len(), 2);
    }
}
