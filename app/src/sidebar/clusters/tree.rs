//! The selected cluster tree's body and the actions that operate on it.
//! This is the single non-`mod.rs` file of the clusters module; it owns
//! everything the user touches once a tree is selected:
//!
//! - **Row rendering** — walks the hydrated node list, paints rows with
//!   expand/collapse, inline rename, and drag-and-drop reparenting.
//! - **Per-row context menu** — rename, edit summary, policy submenu,
//!   split/recluster, summarize, merge, drop, outlier moves, Move-to…
//! - **Action toolbar** — undo/redo, selection-aware actions (summarize
//!   subset, stage moves/tags, clear), regenerate names, params, graph
//!   view, discard tree.
//! - **Undo/redo engine** — pops history rows via `Db::pop_last_history`
//!   and inverts them; the toolbar's undo/redo buttons drive it.
//!
//! These were separate files split purely for the file-length cap; they
//! form one cohesive "operate on the selected tree" surface and share the
//! `ClusterCtx` receiver, so they live together. All mutations route
//! through `hiker_core::trees::types::Db` and mark the surface dirty so
//! the next frame re-reads from disk.

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::types::{Db, EditableNode, Error, NodeInsert, NodeKind, NodePolicy};

use super::{mark_dirty, regenerate_names, summarize_subset, ClusterCtx};
use crate::state::{AppState, ToastLevel};
use crate::theme;

/// Drag-and-drop payload — the node id being dragged.
#[derive(Clone, Debug)]
pub(super) struct DragNode {
    pub node_id: String,
}

impl ClusterCtx<'_> {
pub(super) fn show_tree(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
) {
    // Build a parent_id → children map for one tree walk; nodes are
    // hydrated once per dirty cycle so we don't pay for this every row.
    let mut by_parent: std::collections::HashMap<Option<String>, Vec<EditableNode>> =
        std::collections::HashMap::new();
    for n in self.state.panels.clusters.nodes.iter().cloned() {
        by_parent.entry(n.parent.clone()).or_default().push(n);
    }
    for kids in by_parent.values_mut() {
        // Clusters first, then leaves, then outlier buckets. Within a
        // group, alphabetical by name. Matches the old TS row order well
        // enough for v0 — the spec doesn't pin a precise order.
        kids.sort_by_key(|n| {
            let kind_rank = match n.kind {
                NodeKind::Cluster => 0u8,
                NodeKind::Leaf => 1,
                NodeKind::OutlierBucket => 2,
            };
            (kind_rank, n.name.to_lowercase())
        });
    }

    let roots = by_parent.get(&None).cloned().unwrap_or_default();
    for root in roots {
        self.render_node(ui, tree_id, &by_parent, &root, 0);
    }
}

fn render_node(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    by_parent: &std::collections::HashMap<Option<String>, Vec<EditableNode>>,
    node: &EditableNode,
    depth: usize,
) {
    let has_children = by_parent
        .get(&Some(node.id.clone()))
        .map(|c| !c.is_empty())
        .unwrap_or(false);
    let expanded = self.state.panels.clusters.expanded.contains(&node.id);

    self.paint_row(ui, tree_id, node, depth, has_children, expanded);

    if expanded && has_children {
        let kids = by_parent.get(&Some(node.id.clone())).cloned().unwrap_or_default();
        for child in kids {
            self.render_node(ui, tree_id, by_parent, &child, depth + 1);
        }
    }
}

fn paint_row(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
    depth: usize,
    has_children: bool,
    expanded: bool,
) {
    let indent = (depth as f32) * 12.0;

    let (rect, row_response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 22.0),
        egui::Sense::click_and_drag(),
    );

    // Hover highlight + selection highlight.
    let is_selected = self.state.panels.clusters.selected_nodes.contains(&node.id);
    if is_selected {
        ui.painter().rect_filled(rect, 2.0, theme::active_bg());
    } else if ui.rect_contains_pointer(rect) {
        ui.painter().rect_filled(rect, 2.0, theme::hover_bg());
    }

    // Row contents (chevron, glyph, name) paint into a child UI placed at
    // `rect`. The chevron is its own click target (`row_contents`); the
    // surrounding row body is left to `row_response`, which carries the
    // drag + the body click. Mirrors the Files tree (`files.rs`), where the
    // row uses a plain click/drag response and the drag payload is attached
    // afterward — egui only begins a drag once the pointer moves past its
    // built-in threshold, so a press-release on the row body still reports
    // `clicked()` and a press on the chevron still reaches the chevron.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect);
    child.set_max_width(rect.width());
    child.add_space(indent);
    self.row_contents(&mut child, tree_id, node, has_children, expanded);

    // Drag-source: attach the dragged node id once egui decides a drag has
    // begun (past its motion threshold). A plain click never sets a payload.
    row_response
        .clone()
        .dnd_set_drag_payload::<DragNode>(DragNode { node_id: node.id.clone() });

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
                self.handle_drop(tree_id, &payload.node_id, Some(&node.id));
            }
        }
    }

    // Click handling. A genuine click (no drag past threshold) lands here.
    // Cmd/Ctrl-click toggles multi-select; plain click on a leaf opens the
    // note; plain click on a cluster falls through (the chevron in
    // row_contents handles expand/collapse).
    if row_response.clicked() {
        let multi = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        if multi {
            if self.state.panels.clusters.selected_nodes.contains(&node.id) {
                self.state.panels.clusters.selected_nodes.remove(&node.id);
            } else {
                self.state.panels.clusters.selected_nodes.insert(node.id.clone());
            }
        } else if node.kind == NodeKind::Leaf {
            self.open_leaf(node);
        }
    }

    // Right-click context menu.
    let node_owned = node.clone();
    let tree_id_owned = tree_id.to_string();
    row_response.context_menu(|ui| {
        self.node_context_menu(ui, &tree_id_owned, &node_owned);
    });
}

fn row_contents(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
    has_children: bool,
    expanded: bool,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    // Chevron / spacer. Folders that can expand get a clickable SVG
    // chevron; leaf nodes get a same-width spacer so siblings align.
    let chev_size = egui::vec2(14.0, 14.0);
    if has_children {
        let icon = if expanded {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
        } else {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
        };
        let chev_btn = egui::ImageButton::new(icon).frame(false);
        let chev_resp = ui.add_sized(chev_size, chev_btn);
        if chev_resp.clicked() {
            let expanded_set = &mut state.panels.clusters.expanded;
            if expanded_set.contains(&node.id) {
                expanded_set.remove(&node.id);
            } else {
                expanded_set.insert(node.id.clone());
            }
        }
    } else {
        let (_, _) = ui.allocate_exact_size(chev_size, egui::Sense::hover());
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
            let trimmed = draft.trim();
            state.panels.clusters.renaming = None;
            if !trimmed.is_empty() {
                match trees.rename(tree_id, &node.id, trimmed) {
                    Ok(()) => super::mark_dirty(state),
                    Err(err) => state.push_toast(
                        format!("Rename failed: {}", err),
                        crate::state::ToastLevel::Error,
                    ),
                }
            }
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

fn handle_drop(
    &mut self,
    tree_id: &str,
    dragged_id: &str,
    new_parent: Option<&str>,
) {
    // Guard: don't move a node into itself or its own subtree. Walk the
    // parent chain starting from `new_parent` and bail if we hit the
    // dragged node.
    if let Some(np) = new_parent {
        if self.is_descendant_of(np, dragged_id) || np == dragged_id {
            self.state.push_toast(
                "Cannot move a node into its own subtree",
                crate::state::ToastLevel::Warn,
            );
            return;
        }
    }
    let trees = self.trees;
    match trees.move_node(tree_id, dragged_id, new_parent) {
        Ok(()) => super::mark_dirty(self.state),
        Err(err) => self.state.push_toast(
            format!("Move failed: {}", err),
            crate::state::ToastLevel::Error,
        ),
    }
}

/// True if `candidate` is in the ancestor chain of `descendant_root` (or
/// equal). Used to prevent dropping a parent onto its own child.
fn is_descendant_of(
    &self,
    descendant_root: &str,
    candidate: &str,
) -> bool {
    let nodes = &self.state.panels.clusters.nodes;
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

fn open_leaf(&mut self, node: &EditableNode) {
    let state = &mut *self.state;
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

// ── Per-row right-click context menu ──────────────────────────────────
// Surfaces rename, edit summary, the policy submenu, split/recluster,
// summarize, merge children up, drop cluster, promote-out-of-outliers,
// send-to-outliers, and the Move-to… picker. Multi-select stage-moves /
// stage-tags live in the toolbar above; here we operate on a single node.

pub(super) fn node_context_menu(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
) {
    let is_cluster = node.kind == NodeKind::Cluster;
    let is_leaf = node.kind == NodeKind::Leaf;

    if !is_leaf && ui.button("Rename").clicked() {
        self.state.panels.clusters.renaming = Some((node.id.clone(), node.name.clone()));
        ui.close();
    }
    if !is_leaf && ui.button("Edit summary…").clicked() {
        self.state.panels.clusters.editing_summary = Some((node.id.clone(), node.summary.clone()));
        ui.close();
    }
    if is_cluster {
        ui.menu_button("Policy", |ui| {
            self.policy_submenu(ui, tree_id, node);
        });
        ui.separator();
        if ui
            .button("Split / Recluster")
            .on_hover_text("Re-run clustering on this subtree using stored note embeddings")
            .clicked()
        {
            self.recluster_subtree(tree_id, Some(&node.id));
            ui.close();
        }
        if ui
            .button("Summarize (LLM)")
            .on_hover_text("Generate a new summary for this cluster via the LLM")
            .clicked()
        {
            let ids = vec![node.id.clone()];
            super::summarize_subset(self.state, self.trees, tree_id, &ids);
            ui.close();
        }
        if ui.button("Merge children up").on_hover_text(
            "Flatten one level: each child cluster's children move up to this node.",
        ).clicked() {
            self.merge_children_up(tree_id, node);
            ui.close();
        }
        if ui.button("Drop cluster").clicked() {
            self.drop_cluster(tree_id, node);
            ui.close();
        }
    }
    if is_leaf {
        let parent_is_outlier_bucket = node
            .parent
            .as_ref()
            .and_then(|pid| self.state.panels.clusters.nodes.iter().find(|n| &n.id == pid))
            .is_some_and(|p| p.kind == NodeKind::OutlierBucket);
        if parent_is_outlier_bucket && ui.button("Promote out of outliers…").clicked() {
            // v0: route through the Move to… picker.
            self.show_move_targets(ui, tree_id, node);
            ui.close();
        }
        if !parent_is_outlier_bucket && ui.button("Send to outliers").clicked() {
            self.promote_to_outlier_bucket(tree_id, node);
            ui.close();
        }
    }
    ui.menu_button("Move to…", |ui| {
        self.show_move_targets(ui, tree_id, node);
    });
    if !is_leaf {
        ui.menu_button("Merge with sibling…", |ui| {
            self.show_merge_siblings(ui, tree_id, node);
        });
    }
    if ui.button("Collapse all").clicked() {
        self.state.panels.clusters.expanded.clear();
        ui.close();
    }
}

fn policy_submenu(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    let current = node.policy.clone();
    let is_freeze = matches!(current, Some(NodePolicy::Freeze));
    let is_tag = matches!(current, Some(NodePolicy::Tag { .. }));
    let is_move = matches!(current, Some(NodePolicy::Move { .. }));
    if ui
        .selectable_label(current.is_none(), "Default (no policy)")
        .on_hover_text("Clear any policy from this cluster")
        .clicked()
    {
        apply_policy(state, trees, tree_id, &node.id, None);
        ui.close();
    }
    if ui
        .selectable_label(is_freeze, "Freeze")
        .on_hover_text("Reclustering won't touch this subtree")
        .clicked()
    {
        apply_policy(state, trees, tree_id, &node.id, Some(&NodePolicy::Freeze));
        ui.close();
    }
    let (existing_tag_slug, existing_tag_req) = match &node.policy {
        Some(NodePolicy::Tag { slug, require_review }) => (slug.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui
        .selectable_label(is_tag, "Set Tag policy…")
        .clicked()
    {
        state.panels.clusters.editing_tag_policy =
            Some((node.id.clone(), existing_tag_slug, existing_tag_req));
        ui.close();
    }
    let (existing_move_folder, existing_move_req) = match &node.policy {
        Some(NodePolicy::Move { folder, require_review }) => (folder.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui
        .selectable_label(is_move, "Set Move policy…")
        .clicked()
    {
        state.panels.clusters.editing_move_policy =
            Some((node.id.clone(), existing_move_folder, existing_move_req));
        ui.close();
    }
}

fn merge_children_up(
    &mut self,
    tree_id: &str,
    node: &EditableNode,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    match trees.merge_children_up(tree_id, &node.id) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Merge failed: {}", err), ToastLevel::Error),
    }
}

fn show_merge_siblings(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    let parent = node.parent.clone();
    let siblings: Vec<EditableNode> = state
        .panels.clusters
        .nodes
        .iter()
        .filter(|n| {
            n.id != node.id
                && n.parent == parent
                && matches!(n.kind, NodeKind::Cluster)
        })
        .cloned()
        .collect();
    if siblings.is_empty() {
        ui.label("(no sibling clusters)");
        return;
    }
    for sib in siblings {
        let label = format!("* {}", sib.name);
        if ui.button(label).clicked() {
            let ids = vec![node.id.clone(), sib.id.clone()];
            match trees.merge_siblings(tree_id, &ids) {
                Ok(_) => super::mark_dirty(state),
                Err(err) => state.push_toast(
                    format!("Merge siblings failed: {}", err),
                    ToastLevel::Error,
                ),
            }
            ui.close();
        }
    }
}

fn drop_cluster(
    &mut self,
    tree_id: &str,
    node: &EditableNode,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    let Some(bucket) = state
        .panels.clusters
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::OutlierBucket)
        .cloned()
    else {
        state.push_toast("This tree has no outlier bucket", ToastLevel::Warn);
        return;
    };
    match trees.drop_cluster(tree_id, &node.id, &bucket.id) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Drop failed: {}", err), ToastLevel::Error),
    }
}

fn promote_to_outlier_bucket(
    &mut self,
    tree_id: &str,
    node: &EditableNode,
) {
    let state = &mut *self.state;
    let trees = self.trees;
    let Some(bucket) = state
        .panels.clusters
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::OutlierBucket)
        .cloned()
    else {
        state.push_toast("This tree has no outlier bucket", ToastLevel::Warn);
        return;
    };
    match trees.promote_outlier(tree_id, &node.id, Some(&bucket.id)) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Move failed: {}", err), ToastLevel::Error),
    }
}

fn show_move_targets(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
    node: &EditableNode,
) {
    let descendants = self.collect_descendants(&node.id);
    let state = &mut *self.state;
    let trees = self.trees;
    let candidates: Vec<EditableNode> = state
        .panels.clusters
        .nodes
        .iter()
        .filter(|n| {
            matches!(n.kind, NodeKind::Cluster | NodeKind::OutlierBucket)
                && n.id != node.id
                && !descendants.contains(&n.id)
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        ui.label("(no valid targets)");
        return;
    }
    for c in candidates {
        let glyph = if c.kind == NodeKind::OutlierBucket { "?" } else { "*" };
        let label = format!("{} {}", glyph, c.name);
        if ui.button(label).clicked() {
            let result = if node.kind == NodeKind::Leaf {
                trees.promote_outlier(tree_id, &node.id, Some(&c.id))
            } else {
                trees.move_node(tree_id, &node.id, Some(&c.id))
            };
            match result {
                Ok(()) => super::mark_dirty(state),
                Err(err) => state.push_toast(
                    format!("Move failed: {}", err),
                    ToastLevel::Error,
                ),
            }
            ui.close();
        }
    }
}

/// Collect all descendant ids of `root` (exclusive of `root`). Used to
/// keep the Move-to picker from offering a node's own subtree.
fn collect_descendants(
    &self,
    root: &str,
) -> std::collections::HashSet<String> {
    let nodes = &self.state.panels.clusters.nodes;
    let mut out = std::collections::HashSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(id) = stack.pop() {
        for n in nodes.iter().filter(|n| n.parent.as_deref() == Some(id.as_str())) {
            if out.insert(n.id.clone()) {
                stack.push(n.id.clone());
            }
        }
    }
    out
}
}

fn apply_policy(
    state: &mut AppState,
    trees: &Arc<Db>,
    tree_id: &str,
    node_id: &str,
    policy: Option<&NodePolicy>,
) {
    match trees.set_policy(tree_id, node_id, policy) {
        Ok(()) => super::mark_dirty(state),
        Err(err) => state.push_toast(format!("Set policy failed: {}", err), ToastLevel::Error),
    }
}

// ── Cluster-tree action toolbar ───────────────────────────────────────
// Undo/redo, selection-aware actions (summarize subset, stage moves,
// stage tags, clear selection), regenerate names, advanced-params toggle,
// graph view, discard tree. Per-button click handlers delegate to helpers
// in `super` (`regenerate_names`, `summarize_subset`,
// `advanced_params_popover`) and to the undo/redo engine below, which owns
// the redo-stack bookkeeping.

/// Heuristic for "this tree's clusters still carry placeholder names" per
/// `cluster-editor-pane-name-clusters-cta`. Recomputed live per repaint
/// from the already-hydrated `state.panels.clusters.nodes` (no extra cache; the
/// node list is small and a name/regex check is cheap).
///
/// Placeholder ≡ cluster node whose `name` matches `^Cluster \d+$`, whose
/// `summary` is empty, and which has not been user-edited. The tree is
/// considered to be in placeholder-name state if it has at least one
/// cluster and *every* cluster matches the placeholder shape.
impl ClusterCtx<'_> {
fn tree_has_placeholder_names(&self) -> bool {
    // Placeholder ≡ cluster node whose name matches `^Cluster \d+$`,
    // whose summary is empty, and which has not been user-edited.
    let is_placeholder_name = |name: &str| -> bool {
        // Matches `^Cluster \d+$` without pulling in a regex dep.
        match name.strip_prefix("Cluster ") {
            Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
            None => false,
        }
    };
    let mut saw_cluster = false;
    for n in &self.state.panels.clusters.nodes {
        if !matches!(n.kind, NodeKind::Cluster) {
            continue;
        }
        saw_cluster = true;
        let placeholder = !n.user_edited_name
            && n.summary.is_empty()
            && is_placeholder_name(&n.name);
        if !placeholder {
            return false;
        }
    }
    saw_cluster
}

pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) {
    let Some(tree_id) = self.state.panels.clusters.selected_tree.clone() else {
        return;
    };
    let placeholder_state = self.tree_has_placeholder_names();
    // Undo/redo are applied after the layout closure so their redo-stack
    // bookkeeping can be `&mut self` methods rather than reaching into the
    // closure's reborrowed `state`/`trees`.
    let mut undo_clicked = false;
    let mut redo_clicked = false;
    let state = &mut *self.state;
    let trees = self.trees;
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Undo), "Undo").small())
            .on_hover_text("Undo the last edit on this tree")
            .clicked()
        {
            undo_clicked = true;
        }
        let redo_has = state
            .panels.clusters
            .redo_stacks
            .get(&tree_id)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if ui
            .add_enabled(
                redo_has,
                egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Redo), "Redo").small(),
            )
            .on_hover_text("Re-apply the last undone edit")
            .clicked()
        {
            redo_clicked = true;
        }
        ui.separator();
        // Selection-aware actions.
        let selected_count = state.panels.clusters.selected_nodes.len();
        if selected_count > 0 {
            ui.label(
                egui::RichText::new(format!("{selected_count} selected"))
                    .small()
                    .color(theme::muted()),
            );
            let llm_busy = state.panels.clusters.llm_job_in_flight;
            if ui
                .add_enabled(!llm_busy, egui::Button::new("Summarize subset").small())
                .on_hover_text(if llm_busy { "LLM naming in flight" } else { "Summarize the selected clusters via the LLM" })
                .clicked()
            {
                let ids: Vec<String> =
                    state.panels.clusters.selected_nodes.iter().cloned().collect();
                summarize_subset(state, trees, &tree_id, &ids);
            }
            let resp_stage_move = ui.small_button("Stage moves…");
            if resp_stage_move.clicked() {
                state.panels.clusters.editing_stage_move_target = Some(String::new());
            }
            let resp_stage_tag = ui.small_button("Stage tags…");
            if resp_stage_tag.clicked() {
                state.panels.clusters.editing_stage_tag_slug = Some(String::new());
            }
            if ui.small_button("Clear selection").clicked() {
                state.panels.clusters.selected_nodes.clear();
            }
            ui.separator();
        }
        // status: cluster-editor-pane-name-clusters-cta
        // Contextual rename: when every cluster still carries a
        // placeholder name (`Cluster N`, no summary, not user-edited),
        // surface the verb as a primary CTA — "Name clusters with LLM".
        // Otherwise behave as the regular "Regenerate names" button.
        // Both labels invoke the same `regenerate_names` flow.
        let (label, hover) = if placeholder_state {
            (
                "Name clusters with LLM",
                "LLM-name every cluster in this tree (placeholder names \
                 detected). Same task-queue flow as regenerate.",
            )
        } else {
            (
                "Regenerate names",
                "LLM-rename every cluster not user-edited",
            )
        };
        let llm_busy = state.panels.clusters.llm_job_in_flight;
        let busy_hover = "LLM naming in flight";
        let effective_hover = if llm_busy { busy_hover } else { hover };
        let clicked = if placeholder_state {
            // Primary-CTA styling: accent fill + white text. No existing
            // primary-button convention in this codebase, so we synthesise
            // one inline from `theme::accent()`.
            let btn = egui::Button::new(
                egui::RichText::new(label)
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(theme::accent())
            .small();
            ui.add_enabled(!llm_busy, btn).on_hover_text(effective_hover).clicked()
        } else {
            ui.add_enabled(!llm_busy, egui::Button::new(label).small())
                .on_hover_text(effective_hover)
                .clicked()
        };
        if clicked {
            regenerate_names(state, trees, &tree_id);
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Settings), "Params").small())
            .on_hover_text("Advanced clustering parameters")
            .clicked()
        {
            state.panels.clusters.showing_advanced_params = !state.panels.clusters.showing_advanced_params;
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Graph), "Graph view").small())
            .on_hover_text("Open a radial graph of this cluster tree")
            .clicked()
        {
            use crate::tab::TabKind;
            // Singleton-per-tree: focus an existing graph tab if one's
            // open. Otherwise spawn a fresh ClusterGraph tab.
            let tid = tree_id.clone();
            let tid_for_build = tid.clone();
            state.find_or_open_tab(
                |k| matches!(k, TabKind::ClusterGraph { tree_id: x } if x == &tid),
                || TabKind::ClusterGraph { tree_id: tid_for_build },
            );
        }
        if ui
            .add(egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Trash), "Discard tree").small())
            .on_hover_text("Delete this tree from the registry")
            .clicked()
        {
            match trees.delete_tree(&tree_id) {
                Ok(()) => {
                    state.push_toast("Tree discarded", crate::state::ToastLevel::Info);
                    state.panels.clusters.selected_tree = None;
                    state.panels.clusters.nodes.clear();
                    state.panels.clusters.selected_nodes.clear();
                    state.panels.clusters.redo_stacks.remove(&tree_id);
                    state.panels.clusters.loaded = false;
                    state.panels.clusters.dirty = true;
                }
                Err(err) => state.push_toast(
                    format!("Discard failed: {err}"),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
    });
    if undo_clicked {
        self.perform_undo(&tree_id);
    }
    if redo_clicked {
        self.perform_redo(&tree_id);
    }
    if self.state.panels.clusters.showing_advanced_params {
        self.advanced_params_popover(ui);
    }
}
}

// ── Per-tree undo/redo engine ─────────────────────────────────────────
// Strategy: pop the most-recent history row via `Db::pop_last_history`,
// parse its `op` + `undo_args` JSON, and invert the change. Simple ops
// (rename, edit-summary, set-policy, move, promote-outlier) just call the
// corresponding forward setter with the prior value, then pop the
// resulting forward history row so undo stays idempotent. Reshape ops
// (merge-*, drop-cluster, split-cluster) replay snapshotted node rows and
// reparent leaves directly. The toolbar's Undo/Redo buttons drive
// `perform_undo` / `perform_redo`, which own the redo-stack bookkeeping.

/// Cap per-tree redo stack so cluster history can't grow without bound.
/// Each entry holds JSON blobs (prior_subtree, absorbed_clusters); leaving
/// them uncapped means a long cluster-editing session accumulates
/// indefinitely. 32 entries is more than any user will redo through
/// interactively.
const REDO_STACK_CAP: usize = 32;

impl ClusterCtx<'_> {
/// Toolbar "Undo" click: pop+invert the last history row, toast the
/// outcome, and push the popped entry onto the bounded per-tree redo
/// stack. A `&mut self` method (the toolbar's only caller) so the
/// redo-stack bookkeeping stays out of the button-layout closure.
fn perform_undo(&mut self, tree_id: &str) {
    let trees = self.trees;
    let state = &mut *self.state;
    match (TreeUndo { trees, tree_id }).undo() {
        Ok((op, entry)) => {
            state.push_toast(format!("Undid '{op}'"), ToastLevel::Info);
            let stack = state
                .panels
                .clusters
                .redo_stacks
                .entry(tree_id.to_string())
                .or_default();
            if stack.len() >= REDO_STACK_CAP {
                // Drop the oldest (front) so the most recent undos stay
                // redoable.
                stack.remove(0);
            }
            stack.push(entry);
            mark_dirty(state);
        }
        Err(UndoError::NothingToUndo) => {
            state.push_toast("Nothing to undo", ToastLevel::Info);
        }
        Err(err) => {
            state.push_toast(format!("Undo failed: {err}"), ToastLevel::Error);
        }
    }
}

/// Toolbar "Redo" click: pop the most-recent entry off the per-tree redo
/// stack and re-apply it, toasting the outcome. No-op when the stack is
/// empty (the button is disabled in that case anyway).
fn perform_redo(&mut self, tree_id: &str) {
    let trees = self.trees;
    let state = &mut *self.state;
    let Some(entry) = state
        .panels
        .clusters
        .redo_stacks
        .get_mut(tree_id)
        .and_then(Vec::pop)
    else {
        return;
    };
    match (TreeUndo { trees, tree_id }).redo(&entry) {
        Ok(op) => {
            state.push_toast(format!("Redid '{op}'"), ToastLevel::Info);
            mark_dirty(state);
        }
        Err(err) => state.push_toast(format!("Redo failed: {err}"), ToastLevel::Error),
    }
}
}

#[derive(Debug)]
enum UndoError {
    Db(Error),
    Parse(String),
    Unsupported(String),
    NothingToUndo,
}

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoError::Db(e) => write!(f, "trees: {e}"),
            UndoError::Parse(e) => write!(f, "parse: {e}"),
            UndoError::Unsupported(s) => write!(f, "undo for `{s}` not implemented yet"),
            UndoError::NothingToUndo => write!(f, "nothing to undo"),
        }
    }
}

impl From<Error> for UndoError {
    fn from(e: Error) -> Self {
        UndoError::Db(e)
    }
}

/// A single tree's undo/redo surface — bundles the `Db` handle with the
/// `tree_id` so the apply/restore helpers can be `&self` methods on one
/// receiver instead of repeating both args everywhere.
struct TreeUndo<'a> {
    trees: &'a Arc<Db>,
    tree_id: &'a str,
}

impl TreeUndo<'_> {
/// Pop and invert the most-recent history row for `tree_id`. The
/// popped entry is returned so the caller can push it onto a per-tree
/// redo stack.
fn undo(
    &self,
) -> Result<(String, hiker_core::trees::types::HistoryEntry), UndoError> {
    let Some(entry) = self.trees.pop_last_history(self.tree_id)? else {
        return Err(UndoError::NothingToUndo);
    };
    self.apply_undo(&entry.op, &entry.undo_args_json)?;
    Ok((entry.op.clone(), entry))
}

/// Re-apply a previously-undone history entry. The entry is the value
/// returned by `undo`; on success, history gets a fresh row matching the
/// re-applied op.
fn redo(
    &self,
    entry: &hiker_core::trees::types::HistoryEntry,
) -> Result<String, UndoError> {
    self.apply_redo(&entry.op, &entry.args_json, &entry.undo_args_json)?;
    Ok(entry.op.clone())
}

fn apply_redo(
    &self,
    op: &str,
    args_json: &str,
    _undo_args_json: &str,
) -> Result<(), UndoError> {
    let trees = self.trees;
    let tree_id = self.tree_id;
    match op {
        "rename" => {
            let a: RenameArgs = parse(args_json)?;
            trees.rename(tree_id, &a.node_id, &a.name)?;
        }
        "edit-summary" => {
            let a: EditSummaryArgs = parse(args_json)?;
            trees.set_summary(tree_id, &a.node_id, &a.summary)?;
        }
        "set-policy" => {
            let a: SetPolicyArgs = parse(args_json)?;
            trees.set_policy(tree_id, &a.node_id, a.policy.as_ref())?;
        }
        "move" | "promote-outlier" => {
            let a: MoveArgs = parse(args_json)?;
            trees.move_node(tree_id, a.pick_id(), a.parent_id.as_deref())?;
        }
        "merge-siblings" => {
            let a: MergeSiblingsArgs = parse(args_json)?;
            let mut ids = vec![a.survivor];
            ids.extend(a.absorbed);
            trees.merge_siblings(tree_id, &ids)?;
        }
        "merge-children-up" => {
            let a: MergeChildrenUpArgs = parse(args_json)?;
            trees.merge_children_up(tree_id, &a.parent_id)?;
        }
        "drop-cluster" => {
            let a: DropClusterArgs = parse(args_json)?;
            trees.drop_cluster(tree_id, &a.node_id, &a.outlier_bucket_id)?;
        }
        other => {
            return Err(UndoError::Unsupported(format!("redo {other}")));
        }
    }
    Ok(())
}

fn apply_undo(
    &self,
    op: &str,
    undo_args_json: &str,
) -> Result<(), UndoError> {
    let trees = self.trees;
    let tree_id = self.tree_id;
    match op {
        "rename" => {
            let u: RenameArgs = parse(undo_args_json)?;
            trees.rename(tree_id, &u.node_id, &u.name)?;
            let _ = trees.pop_last_history(tree_id);
        }
        "edit-summary" => {
            let u: EditSummaryArgs = parse(undo_args_json)?;
            trees.set_summary(tree_id, &u.node_id, &u.summary)?;
            let _ = trees.pop_last_history(tree_id);
        }
        "set-policy" => {
            let u: SetPolicyArgs = parse(undo_args_json)?;
            trees.set_policy(tree_id, &u.node_id, u.policy.as_ref())?;
            let _ = trees.pop_last_history(tree_id);
        }
        "move" | "promote-outlier" => {
            let u: MoveArgs = parse(undo_args_json)?;
            let node_id = u.pick_id();
            trees.move_node(tree_id, node_id, u.parent_id.as_deref())?;
            let _ = trees.pop_last_history(tree_id);
        }
        "merge-siblings" => {
            let u: MergeSiblingsUndo = parse(undo_args_json)?;
            for abs in &u.absorbed {
                self.restore_node_row(&abs.id, &abs.row)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .child_moves
                .into_iter()
                .map(|m| (m.child_id, Some(m.from)))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "merge-children-up" => {
            let u: MergeChildrenUpUndo = parse(undo_args_json)?;
            for abs in &u.absorbed {
                self.restore_node_row(&abs.id, &abs.row)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .grandchild_moves
                .into_iter()
                .map(|m| (m.child_id, Some(m.from)))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "drop-cluster" => {
            let u: DropClusterUndo = parse(undo_args_json)?;
            for c in &u.absorbed_clusters {
                let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.restore_node_row(&id, c)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .leaf_moves
                .into_iter()
                .map(|m| (m.leaf_id, m.prior_parent))
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "split-cluster" => {
            let u: SplitClusterUndo = parse(undo_args_json)?;
            let mut moves: Vec<(String, Option<String>)> = Vec::new();
            for mv in &u.leaf_moves {
                if let Some(arr) = mv.as_array()
                    && let Some(leaf) = arr.first()
                    && let Some(s) = leaf.as_str()
                {
                    moves.push((s.to_string(), Some(u.parent_id.clone())));
                }
            }
            trees.reparent_many(tree_id, &moves)?;
            for nc in &u.new_cluster_ids {
                trees.delete_node(tree_id, nc)?;
            }
        }
        "recluster-subtree" => {
            let u: ReclusterUndo = parse(undo_args_json)?;
            for id in &u.new_node_ids {
                trees.delete_node(tree_id, id)?;
            }
            for snap in &u.prior_subtree {
                let id = snap
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                self.restore_node_row(&id, snap)?;
            }
            let moves: Vec<(String, Option<String>)> = u
                .prior_leaf_parents
                .iter()
                .filter_map(|mv| {
                    let arr = mv.as_array()?;
                    let leaf = arr.first()?.as_str()?.to_string();
                    let parent = arr.get(1).and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        _ => None,
                    });
                    Some((leaf, parent))
                })
                .collect();
            trees.reparent_many(tree_id, &moves)?;
        }
        "raptor-summarize" => {
            let u: RaptorSummarizeUndo = parse(undo_args_json)?;
            trees.set_summary(tree_id, &u.node_id, &u.summary)?;
            let _ = trees.pop_last_history(tree_id);
            // Name restore: rename also stamps user_edited_name, but the
            // raptor-summarize undo carries the prior name explicitly.
            // We rename through the public method to keep history sane,
            // then pop again.
            trees.rename(tree_id, &u.node_id, &u.name)?;
            let _ = trees.pop_last_history(tree_id);
        }
        other => {
            return Err(UndoError::Unsupported(other.to_string()));
        }
    }
    Ok(())
}

fn restore_node_row(
    &self,
    id: &str,
    row: &serde_json::Value,
) -> Result<(), UndoError> {
    let trees = self.trees;
    let tree_id = self.tree_id;
    let parent = row.get("parent_id").and_then(|v| match v {
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    });
    let kind = match row.get("kind").and_then(|v| v.as_str()).unwrap_or("cluster") {
        "leaf" => NodeKind::Leaf,
        "outlier-bucket" => NodeKind::OutlierBucket,
        _ => NodeKind::Cluster,
    };
    let note_id = row
        .get("note_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let summary = row
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let policy: Option<NodePolicy> = row.get("policy").and_then(|v| match v {
        serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
        _ => None,
    });
    trees.insert_single_node(
        tree_id,
        &NodeInsert {
            node_id: id.to_string(),
            parent_id: parent,
            kind,
            note_id,
            name,
            summary,
            user_edited_name: row
                .get("user_edited_name")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            user_edited_summary: row
                .get("user_edited_summary")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            policy,
            centroid: None,
            confidence: row
                .get("confidence")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0) as f32,
            summary_membership_churn: row
                .get("summary_membership_churn")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                .max(0) as u32,
        },
    )?;
    Ok(())
}
}

fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, UndoError> {
    serde_json::from_str(s).map_err(|e| UndoError::Parse(e.to_string()))
}

// ── Per-op arg structs ────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RenameArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize)]
struct EditSummaryArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    summary: String,
}

#[derive(serde::Deserialize)]
struct SetPolicyArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    policy: Option<NodePolicy>,
}

#[derive(serde::Deserialize)]
struct MoveArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    leaf_id: String,
    #[serde(default)]
    parent_id: Option<String>,
}

impl MoveArgs {
    fn pick_id(&self) -> &str {
        if !self.node_id.is_empty() {
            &self.node_id
        } else {
            &self.leaf_id
        }
    }
}

#[derive(serde::Deserialize)]
struct MergeSiblingsUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    child_moves: Vec<ChildMove>,
}

#[derive(serde::Deserialize)]
struct MergeChildrenUpUndo {
    #[serde(default)]
    absorbed: Vec<AbsorbedNode>,
    #[serde(default)]
    grandchild_moves: Vec<ChildMove>,
}

#[derive(serde::Deserialize)]
struct AbsorbedNode {
    #[serde(default)]
    id: String,
    #[serde(default)]
    row: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct ChildMove {
    #[serde(default)]
    child_id: String,
    #[serde(default)]
    from: String,
}

#[derive(serde::Deserialize)]
struct DropClusterUndo {
    #[serde(default)]
    absorbed_clusters: Vec<serde_json::Value>,
    #[serde(default)]
    leaf_moves: Vec<LeafMoveRow>,
}

#[derive(serde::Deserialize)]
struct LeafMoveRow {
    #[serde(default)]
    leaf_id: String,
    #[serde(default)]
    prior_parent: Option<String>,
}

#[derive(serde::Deserialize)]
struct SplitClusterUndo {
    #[serde(default)]
    parent_id: String,
    #[serde(default)]
    leaf_moves: Vec<serde_json::Value>,
    #[serde(default)]
    new_cluster_ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct MergeSiblingsArgs {
    #[serde(default)]
    survivor: String,
    #[serde(default)]
    absorbed: Vec<String>,
}

#[derive(serde::Deserialize)]
struct MergeChildrenUpArgs {
    #[serde(default)]
    parent_id: String,
}

#[derive(serde::Deserialize)]
struct DropClusterArgs {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    outlier_bucket_id: String,
}

#[derive(serde::Deserialize)]
struct ReclusterUndo {
    #[serde(default)]
    prior_subtree: Vec<serde_json::Value>,
    #[serde(default)]
    prior_leaf_parents: Vec<serde_json::Value>,
    #[serde(default)]
    new_node_ids: Vec<String>,
}

#[derive(serde::Deserialize)]
struct RaptorSummarizeUndo {
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: String,
}
