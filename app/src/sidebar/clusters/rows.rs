//! Row renderer for the cluster-trees sidebar. Walks the hydrated node
//! list from `ClusterUiState`, paints rows with expand/collapse +
//! inline rename + context menu + drag-and-drop. All mutations route
//! through `hiker_core::trees::Trees` and mark the surface dirty so the
//! next frame re-reads from disk.

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::{EditableNode, NodeKind, Trees};

use super::menus;
use crate::state::AppState;
use crate::theme;

/// Drag-and-drop payload — the node id being dragged.
#[derive(Clone, Debug)]
pub(super) struct DragNode {
    pub node_id: String,
}

pub(super) fn show_tree(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
) {
    // Build a parent_id → children map for one tree walk; nodes are
    // hydrated once per dirty cycle so we don't pay for this every row.
    let mut by_parent: std::collections::HashMap<Option<String>, Vec<EditableNode>> =
        std::collections::HashMap::new();
    for n in state.panels.clusters.nodes.iter().cloned() {
        by_parent.entry(n.parent.clone()).or_default().push(n);
    }
    for kids in by_parent.values_mut() {
        kids.sort_by_key(sort_key);
    }

    let roots = by_parent.get(&None).cloned().unwrap_or_default();
    for root in roots {
        render_node(ui, state, trees, tree_id, &by_parent, &root, 0);
    }
}

fn sort_key(n: &EditableNode) -> (u8, String) {
    // Clusters first, then leaves, then outlier buckets. Within a group,
    // alphabetical by name. Matches the old TS row order well enough for
    // v0 — the spec doesn't pin a precise order.
    let kind_rank = match n.kind {
        NodeKind::Cluster => 0,
        NodeKind::Leaf => 1,
        NodeKind::OutlierBucket => 2,
    };
    (kind_rank, n.name.to_lowercase())
}

#[allow(clippy::too_many_arguments)]
fn render_node(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    by_parent: &std::collections::HashMap<Option<String>, Vec<EditableNode>>,
    node: &EditableNode,
    depth: usize,
) {
    let has_children = by_parent
        .get(&Some(node.id.clone()))
        .map(|c| !c.is_empty())
        .unwrap_or(false);
    let expanded = state.panels.clusters.expanded.contains(&node.id);

    paint_row(ui, state, trees, tree_id, node, depth, has_children, expanded);

    if expanded && has_children {
        let kids = by_parent.get(&Some(node.id.clone())).cloned().unwrap_or_default();
        for child in kids {
            render_node(ui, state, trees, tree_id, by_parent, &child, depth + 1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
    depth: usize,
    has_children: bool,
    expanded: bool,
) {
    let indent = (depth as f32) * 12.0;
    let row_id = egui::Id::new(("cluster-row", tree_id, &node.id));

    let (rect, _resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 22.0),
        egui::Sense::click_and_drag(),
    );

    // Hover highlight + selection highlight.
    let is_selected = state.panels.clusters.selected_nodes.contains(&node.id);
    if is_selected {
        ui.painter().rect_filled(rect, 2.0, theme::active_bg());
    } else if ui.rect_contains_pointer(rect) {
        ui.painter().rect_filled(rect, 2.0, theme::hover_bg());
    }

    // Drag-source: wrap the visual paint in dnd_drag_source so egui
    // tracks the drag + paints a ghost. We re-allocate the same area
    // via a child UI placed at `rect`.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);
    child.set_max_width(rect.width());
    child.add_space(indent);

    let drag_payload = DragNode { node_id: node.id.clone() };
    let inner = child.dnd_drag_source(row_id, drag_payload, |ui| {
        row_contents(ui, state, trees, tree_id, node, has_children, expanded)
    });
    let row_response = inner.response;

    // Drop-zone: clusters and outlier-buckets accept drops; leaves don't.
    let accepts_drop = matches!(node.kind, NodeKind::Cluster | NodeKind::OutlierBucket);
    if accepts_drop {
        if ui.rect_contains_pointer(rect) && egui::DragAndDrop::has_payload_of_type::<DragNode>(ui.ctx()) {
            ui.painter().rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(1.5, theme::accent()),
                egui::StrokeKind::Outside,
            );
        }
        if let Some(payload) = row_response.dnd_release_payload::<DragNode>() {
            if payload.node_id != node.id {
                handle_drop(state, trees, tree_id, &payload.node_id, Some(&node.id));
            }
        }
    }

    // Click handling. Cmd/Ctrl-click toggles multi-select; plain click on a
    // leaf opens the note; plain click on a cluster falls through (the
    // chevron in row_contents handles expand/collapse).
    if row_response.clicked() {
        let multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        if multi {
            if state.panels.clusters.selected_nodes.contains(&node.id) {
                state.panels.clusters.selected_nodes.remove(&node.id);
            } else {
                state.panels.clusters.selected_nodes.insert(node.id.clone());
            }
        } else if node.kind == NodeKind::Leaf {
            open_leaf(state, node);
        }
    }

    // Right-click context menu.
    let node_owned = node.clone();
    let trees_arc = trees.clone();
    let tree_id_owned = tree_id.to_string();
    row_response.context_menu(|ui| {
        menus::node_context_menu(ui, state, &trees_arc, &tree_id_owned, &node_owned);
    });
}

fn row_contents(
    ui: &mut egui::Ui,
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node: &EditableNode,
    has_children: bool,
    expanded: bool,
) {
    // Chevron / spacer.
    let chev = if has_children {
        if expanded { "v" } else { ">" }
    } else {
        "  "
    };
    let chev_btn = egui::Label::new(egui::RichText::new(chev).monospace().size(12.0))
        .sense(egui::Sense::click());
    let chev_resp = ui.add(chev_btn);
    if chev_resp.clicked() && has_children {
        toggle_expand(state, &node.id);
    }

    // Glyph by kind.
    let glyph = match node.kind {
        NodeKind::Cluster => "*",
        NodeKind::Leaf => "-",
        NodeKind::OutlierBucket => "?",
    };
    ui.label(egui::RichText::new(glyph).color(theme::muted()).size(11.0));

    // Name (inline-editable for clusters, read-only for leaves).
    let is_renaming = state
        .panels.clusters
        .renaming
        .as_ref()
        .is_some_and(|(id, _)| id == &node.id);
    if is_renaming {
        let mut draft = state
            .panels.clusters
            .renaming
            .as_ref()
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .desired_width(ui.available_width() - 4.0)
                .font(egui::TextStyle::Body),
        );
        resp.request_focus();
        // Persist back.
        state.panels.clusters.renaming = Some((node.id.clone(), draft.clone()));
        let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let cancel = resp.lost_focus() && !ui.input(|i| i.key_pressed(egui::Key::Enter));
        if commit {
            commit_rename(state, trees, tree_id, &node.id, &draft);
        } else if cancel {
            state.panels.clusters.renaming = None;
        }
    } else {
        let mut text = egui::RichText::new(&node.name).size(13.0);
        if node.kind == NodeKind::OutlierBucket {
            text = text.italics().color(theme::muted());
        }
        let label = egui::Label::new(text)
            .truncate()
            .sense(egui::Sense::click());
        let resp = ui.add(label);
        if resp.double_clicked() && node.kind != NodeKind::Leaf {
            state.panels.clusters.renaming = Some((node.id.clone(), node.name.clone()));
        }
    }
}

fn toggle_expand(state: &mut AppState, node_id: &str) {
    if state.panels.clusters.expanded.contains(node_id) {
        state.panels.clusters.expanded.remove(node_id);
    } else {
        state.panels.clusters.expanded.insert(node_id.to_string());
    }
}

fn commit_rename(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    node_id: &str,
    new_name: &str,
) {
    let trimmed = new_name.trim();
    state.panels.clusters.renaming = None;
    if trimmed.is_empty() {
        return;
    }
    match trees.rename(tree_id, node_id, trimmed) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(
            format!("Rename failed: {}", err),
            crate::state::ToastLevel::Error,
        ),
    }
}

fn handle_drop(
    state: &mut AppState,
    trees: &Arc<Trees>,
    tree_id: &str,
    dragged_id: &str,
    new_parent: Option<&str>,
) {
    // Guard: don't move a node into itself or its own subtree. Walk the
    // parent chain starting from `new_parent` and bail if we hit the
    // dragged node.
    if let Some(np) = new_parent {
        if is_descendant_of(&state.panels.clusters.nodes, np, dragged_id) || np == dragged_id {
            state.push_toast(
                "Cannot move a node into its own subtree",
                crate::state::ToastLevel::Warn,
            );
            return;
        }
    }
    match trees.move_node(tree_id, dragged_id, new_parent) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(
            format!("Move failed: {}", err),
            crate::state::ToastLevel::Error,
        ),
    }
}

/// True if `candidate` is in the ancestor chain of `descendant` (or
/// equal). Used to prevent dropping a parent onto its own child.
fn is_descendant_of(
    nodes: &[EditableNode],
    descendant_root: &str,
    candidate: &str,
) -> bool {
    // Walk up from `descendant_root` to root. If we ever see `candidate`,
    // we'd be dropping `candidate` inside its own subtree.
    let mut cur = Some(descendant_root.to_string());
    while let Some(id) = cur {
        if id == candidate {
            return true;
        }
        cur = nodes.iter().find(|n| n.id == id).and_then(|n| n.parent.clone());
    }
    false
}

fn open_leaf(state: &mut AppState, node: &EditableNode) {
    let Some(note_id) = node.note_ref.as_deref() else {
        return;
    };
    // Resolve note_id → vault-relative path via the read store. Clone
    // the Arc so we can release the immutable borrow on `state` before
    // calling `push_toast` / `open_file` (both need `&mut state`).
    let store_mutex = state.vault_session.services.read_store.clone();
    let lookup: Result<Option<String>, String> = (|| {
        let guard = store_mutex.lock().map_err(|_| "Store mutex poisoned".to_string())?;
        guard.path_for_id(note_id).map_err(|e| e.to_string())
    })();
    match lookup {
        Ok(Some(rel)) => crate::editor_pane::open_file(state, &rel, /* sticky */ false),
        Ok(None) => state.push_toast(
            format!("Note {} no longer in index", note_id),
            crate::state::ToastLevel::Warn,
        ),
        Err(err) => state.push_toast(format!("Lookup failed: {}", err), crate::state::ToastLevel::Error),
    }
}
