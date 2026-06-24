//! Cluster-tree graph view (`cluster-editor-graph-view`). Renders a cluster
//! tree (radial / vertical / horizontal / force-directed) through the shared
//! `hiker_graph_view` engine. This panel is the cluster-specific
//! [`graph_view::source::Source`] adapter: it maps `EditableNode` rows to colored,
//! sized nodes (color-by-policy, blended toward grey by summary staleness,
//! sized by member count), owns the policy-color legend, and resolves leaf
//! note bodies for the hover preview.
//!
//! State lives on `AppState::panels.cluster_graph`, keyed by tree id, so
//! flipping between tabs keeps each tree's layout (and its non-Clone
//! background worker) warm.

use std::collections::HashMap;

use eframe::egui;

use crate::state::AppState;
use hiker_graph_view::graph_view;
use hiker_graph_view::graph_view::source::{LayoutConfig, NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::{policy_legend, Palette, Style};
use hiker_core::trees::types::{EditableNode, NodeKind, NodePolicy};
use hiker_core::vault::Vault;
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_theme as theme;

const FR_BOX: f32 = 800.0;
const CLUSTER_CFG: LayoutConfig = LayoutConfig {
    area: FR_BOX * FR_BOX,
    seed_box: 80.0,
};

/// Per-tree panel state: the shared render engine + the tree's index data +
/// the cluster-specific "Leaves" toggle + the Ctrl+F find-popup state.
pub struct ClusterView {
    engine: graph_view::State,
    data: ClusterData,
    show_leaves: bool,
    /// "Find / jump to note" popup (Ctrl+F) — sibling parity with the vault
    /// and code graphs (`graph_find.rs`).
    find: crate::widgets::autocomplete_picker::PickerState,
    /// Latched right-click menu: the right-clicked node's id + the pointer
    /// position the popup opens at (the engine owns its pane response, so the
    /// menu is hosted in a popup instead of `Response::context_menu`).
    /// Right-click is a menu, never a direct action (`interaction.md`
    /// [rightclick-menu-always]).
    node_menu: Option<(String, egui::Pos2)>,
}

/// Stable node index for a tree shape. `ids[i]` ↔ position `i`; rebuilt when
/// the upstream tree's `(id, parent)` set changes.
#[derive(Default)]
struct ClusterData {
    ids: Vec<String>,
    id_index: HashMap<String, usize>,
    parent_of: Vec<Option<usize>>,
    seeded_for: u64,
}

impl ClusterView {
    fn new() -> Self {
        Self {
            engine: graph_view::State::new(Style::policy(), LayoutKind::Radial),
            data: ClusterData::default(),
            show_leaves: true,
            find: Default::default(),
            node_menu: None,
        }
    }
}

impl ClusterData {
    /// Rebuild the id list + parent index from a fresh node slice, recording
    /// the shape fingerprint it was seeded for.
    fn seed(&mut self, nodes: &[EditableNode], shape_hash: u64) {
        self.ids = nodes.iter().map(|n| n.id.clone()).collect();
        self.id_index = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect();
        self.parent_of = nodes
            .iter()
            .map(|n| {
                n.parent
                    .as_deref()
                    .and_then(|p| self.id_index.get(p).copied())
            })
            .collect();
        self.seeded_for = shape_hash;
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tree_id: &str) {
    let short = &tree_id[..tree_id.len().min(8)];
    ui.heading(format!("Cluster graph · {short}"));

    let trees = app.vault_session.services.trees.clone();
    let nodes = match trees.list_nodes(tree_id) {
        Ok(ns) => ns,
        Err(err) => {
            ui.colored_label(egui::Color32::RED, format!("list_nodes failed: {err}"));
            return;
        }
    };
    if nodes.is_empty() {
        ui.label(egui::RichText::new("(tree is empty)").color(theme::muted()));
        return;
    }
    // Persisted-tree entry point: clickable leaves resolve to vault paths.
    // The persisted tree is static, so a refit on (rare) shape change is fine.
    // No "Live preview" toggle here — that's a review-tab display control only.
    show_with_nodes(
        ui,
        app,
        tree_id,
        &nodes,
        /*clickable_leaves=*/ true,
        /*preserve_view=*/ false,
        /*live_preview=*/ None,
    );
}

/// Render the cluster graph from a pre-resolved `EditableNode` slice. Shared
/// between the persisted cluster-tree tab (`show`) and the un-persisted
/// `BuiltClusterTree` preview in `clusters::panel`.
///
/// `state_key` namespaces the per-tree state on `AppState::panels.cluster_graph`.
/// `clickable_leaves = false` disables click-to-open for the review preview,
/// whose leaf ids aren't necessarily resolvable through the read store.
///
/// `preserve_view`: when the shape changes on a *re*-seed (i.e. the graph was
/// already seeded once), keep the current pan/zoom instead of refitting to
/// the new content. Used by the review tab's live preview so re-clustering
/// as the user tweaks knobs updates in place rather than yanking the camera
/// back to a fit. The very first seed always fits.
///
/// `live_preview`: when `Some`, a "Live preview" checkbox is appended to the
/// result graph's view/eye menu (a display control), bound to the caller's
/// flag — toggling it gates debounced live re-clustering. The persisted tab
/// passes `None`; only the review tab carries the toggle.
///
/// status: cluster-review-tab-result-graph-view
/// status: cluster-review-tab-live-preview
pub fn show_with_nodes(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tree_id: &str,
    nodes: &[EditableNode],
    clickable_leaves: bool,
    preserve_view: bool,
    live_preview: Option<&mut bool>,
) {
    if nodes.is_empty() {
        ui.label(egui::RichText::new("(tree is empty)").color(theme::muted()));
        return;
    }
    let shape_hash = shape_fingerprint(nodes);

    // Ensure per-tree state exists and is seeded for the current shape.
    {
        let vault = app.vault_session.vault.clone();
        let view = app
            .panels
            .cluster_graph
            .entry(tree_id.to_string())
            .or_insert_with(ClusterView::new);
        if view.data.seeded_for != shape_hash {
            // Preserve the camera across a re-seed (live preview) so the
            // viewport doesn't jump; the first seed (seeded_for == 0) still
            // fits so a fresh graph frames itself.
            let keep_view = (preserve_view && view.data.seeded_for != 0)
                .then_some(view.engine.view);
            view.data.seed(nodes, shape_hash);
            let ClusterView {
                engine,
                data,
                show_leaves,
                ..
            } = view;
            let source = ClusterSource::new(nodes, data, vault.as_ref(), *show_leaves, clickable_leaves);
            engine.recompute_layout(&source, CLUSTER_CFG);
            if let Some(v) = keep_view {
                engine.view = v;
                engine.needs_fit = false;
            }
        }
    }

    // Toolbar + legend.
    let (mut reset_view, mut relayout) = (false, false);
    if let Some(view) = app.panels.cluster_graph.get_mut(tree_id) {
        ui.horizontal_wrapped(|ui| {
            // "Leaves" is the per-tree display toggle; the review tab also
            // appends "Live preview" (a display control over the clustering
            // engine's debounced re-run). The clustering ENGINE params live in
            // the config form, not here.
            let mut extra_toggles: Vec<(&str, &mut bool)> =
                vec![("Leaves", &mut view.show_leaves)];
            if let Some(lp) = live_preview {
                extra_toggles.push(("Live preview", lp));
            }
            relayout = view.engine.view_options_menu(
                ui,
                crate::icons::ICONS.image(crate::icons::Icon::Eye),
                &mut extra_toggles,
            );
            if ui.small_button("Reset view").clicked() {
                reset_view = true;
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} · zoom {:.2}x",
                    view.engine.layout_kind.label(),
                    view.engine.view.zoom
                ))
                .color(theme::muted())
                .small(),
            );
        });
        policy_legend(ui, &view.engine.style.palette);
        ui.add_space(4.0);
    }
    if reset_view
        && let Some(view) = app.panels.cluster_graph.get_mut(tree_id)
    {
        view.engine.needs_fit = true;
    }
    if relayout {
        relayout_cluster(app, tree_id, nodes, clickable_leaves);
    }

    // Ctrl+F opens the "Find / jump to note" popup — sibling parity with the
    // vault/code graphs (interaction.md [keyboard-esc-ladder]). Only where a
    // leaf click opens (a pick navigates exactly like a click), so the
    // review preview's non-clickable leaves don't get a dead popup.
    if clickable_leaves
        && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F))
        && let Some(view) = app.panels.cluster_graph.get_mut(tree_id)
    {
        view.find.open();
    }

    // Canvas (reserve a line at the bottom for the count summary).
    let clicked = render_canvas(ui, app, tree_id, nodes, clickable_leaves);
    node_menu_ui(ui, app, tree_id, nodes);
    // A find-popup pick opens the chosen leaf's note exactly like a node
    // click (same sticky-open routing).
    let jumped = find_popup(ui, app, tree_id, nodes);
    if let Some(path) = clicked.or(jumped) {
        crate::editor_pane::open_file(app, &path, /*sticky=*/ true);
    }

    summary_label(ui, nodes);
}

/// Drive one frame of the "Find / jump to note" popup over the tree's leaf
/// note paths (the clickable nodes). Returns the picked rel-path for the
/// caller to open — the same routing as a leaf click.
fn find_popup(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tree_id: &str,
    nodes: &[EditableNode],
) -> Option<String> {
    use crate::widgets::autocomplete_picker::{self, PickerOutcome};
    let view = app.panels.cluster_graph.get_mut(tree_id)?;
    if !view.find.is_open() {
        return None;
    }
    let paths: Vec<String> = nodes.iter().filter_map(|n| n.note_path.clone()).collect();
    let source = crate::panels::graph_find::VaultNodeFindSource::new(&paths);
    match autocomplete_picker::show(ui, &mut view.find, &source, "Find node") {
        PickerOutcome::Selected(item) => Some(item.insert.to_string()),
        PickerOutcome::Cancelled | PickerOutcome::Open => None,
    }
}

/// Recompute positions in place after a layout-kind change.
fn relayout_cluster(app: &mut AppState, tree_id: &str, nodes: &[EditableNode], clickable: bool) {
    let vault = app.vault_session.vault.clone();
    let Some(view) = app.panels.cluster_graph.get_mut(tree_id) else {
        return;
    };
    let ClusterView {
        engine,
        data,
        show_leaves,
        ..
    } = view;
    let source = ClusterSource::new(nodes, data, vault.as_ref(), *show_leaves, clickable);
    engine.recompute_layout(&source, CLUSTER_CFG);
}

/// Drive the engine for one frame inside a height-reserved sub-canvas;
/// returns the clicked leaf path, if any.
fn render_canvas(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tree_id: &str,
    nodes: &[EditableNode],
    clickable: bool,
) -> Option<String> {
    let vault = app.vault_session.vault.clone();
    let view = app.panels.cluster_graph.get_mut(tree_id)?;
    let ClusterView {
        engine,
        data,
        show_leaves,
        node_menu,
        ..
    } = view;
    let source = ClusterSource::new(nodes, data, vault.as_ref(), *show_leaves, clickable);
    let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(50.0));
    let clicked = ui
        .allocate_ui(size, |ui| {
            engine.ui(ui, &source, |p: &egui::Painter, r: egui::Rect, t: &str, b: &str, a: egui::Pos2| {
                crate::panels::graph::paint_preview_card(p, r, t, b, a);
            })
        })
        .inner;
    // Right-click a node → latch its MENU (never a direct action, per
    // `interaction.md` [rightclick-menu-always]); `node_menu_ui` renders it
    // and applies the picked verb. Index-keyed (not `click_path`-keyed) so
    // cluster nodes — which have no click path — get a menu too. Gated to the
    // clickable-leaves entry point like the find popup: the review preview's
    // leaf ids aren't necessarily resolvable, so it gets no dead menu.
    if clickable
        && let Some(idx) = engine.take_secondary_click_node()
        && let Some(id) = data.ids.get(idx)
    {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or_else(|| ui.min_rect().center());
        *node_menu = Some((id.clone(), pos));
    }
    clicked
}

/// Build the right-click menu for a cluster-graph node (`interaction.md`
/// [rightclick-menu-always]). A leaf is a note ref, so it gets the shared
/// note-item base (Open · Reveal in file tree · Open in graph · Copy path ·
/// Properties). A
/// cluster node maps to frontmatter rows of the tree's own doc — and opening
/// that doc routes straight back to this graph view (`cluster-tree-open-routing`)
/// — so there is no openable note behind it; its menu is the minimal Copy name.
fn build_node_menu(
    node: &EditableNode,
) -> egui_workbench::menu::Menu<crate::item_menu::ItemAction> {
    use crate::item_menu::{note_item_base, BaseOpts};
    if let Some(path) = node.note_path.as_deref() {
        return note_item_base(path, BaseOpts { reveal: true }, |a| a);
    }
    let name = node.name.clone();
    egui_workbench::menu::Menu::new().custom(move |ui| {
        if ui.button("Copy name").clicked() {
            ui.ctx().copy_text(name.clone());
            ui.close();
        }
        None
    })
}

/// Render the latched node context menu and apply the picked verb (leaf base
/// verbs dispatch through the shared `apply_item_action`; the cluster node's
/// Copy name copies at render time and yields no action).
fn node_menu_ui(ui: &mut egui::Ui, app: &mut AppState, tree_id: &str, nodes: &[EditableNode]) {
    use crate::item_menu;
    // Render under a short view borrow, then apply with `app` free.
    let picked = {
        let Some(view) = app.panels.cluster_graph.get_mut(tree_id) else { return };
        let Some((node_id, _)) = view.node_menu.clone() else { return };
        let Some(node) = nodes.iter().find(|n| n.id == node_id) else {
            // The node vanished under the latch (re-cluster); drop the menu.
            view.node_menu = None;
            return;
        };
        let path = node.note_path.clone();
        item_menu::latched_menu_popup(
            ui,
            egui::Id::new(("cluster-graph-node-menu", tree_id)),
            &mut view.node_menu,
            build_node_menu(node),
        )
        .zip(path)
    };
    if let Some((action, path)) = picked {
        item_menu::apply_item_action(app, action, &path);
    }
}

/// Cheap shape fingerprint over the `(id, parent)` edges. Changes on
/// add/remove/re-parent; stable across summary/policy edits.
fn shape_fingerprint(nodes: &[EditableNode]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    nodes.len().hash(&mut h);
    for n in nodes {
        n.id.hash(&mut h);
        n.parent.hash(&mut h);
    }
    h.finish()
}

fn summary_label(ui: &mut egui::Ui, nodes: &[EditableNode]) {
    ui.add_space(4.0);
    let clusters = nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Cluster))
        .count();
    let leaves = nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Leaf))
        .count();
    ui.label(
        egui::RichText::new(format!(
            "{} nodes · {} clusters · {} leaves",
            nodes.len(),
            clusters,
            leaves
        ))
        .color(theme::muted())
        .small(),
    );
}

/// Cluster adapter from an `EditableNode` slice to the shared graph engine.
struct ClusterSource<'a> {
    nodes: &'a [EditableNode],
    data: &'a ClusterData,
    vault: &'a Vault,
    members: HashMap<String, usize>,
    max_members: f32,
    show_leaves: bool,
    clickable_leaves: bool,
}

impl<'a> ClusterSource<'a> {
    fn new(
        nodes: &'a [EditableNode],
        data: &'a ClusterData,
        vault: &'a Vault,
        show_leaves: bool,
        clickable_leaves: bool,
    ) -> Self {
        let members = compute_member_counts(nodes);
        let max_members = members.values().copied().max().unwrap_or(1) as f32;
        Self {
            nodes,
            data,
            vault,
            members,
            max_members,
            show_leaves,
            clickable_leaves,
        }
    }
}

impl Source for ClusterSource<'_> {
    fn node_count(&self) -> usize {
        self.data.ids.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let palette = PolicyColors::from(style.palette);
        let mut out = Vec::new();
        for n in self.nodes {
            let Some(&idx) = self.data.id_index.get(n.id.as_str()) else {
                continue;
            };
            if idx >= positions.len() {
                continue;
            }
            let is_cluster = matches!(n.kind, NodeKind::Cluster);
            if !is_cluster && !self.show_leaves {
                continue;
            }
            let members = self.members.get(n.id.as_str()).copied().unwrap_or(0) as f32;
            let base_size = if is_cluster {
                let frac = (members / self.max_members.max(1.0)).clamp(0.0, 1.0);
                8.0 + 12.0 * frac
            } else {
                5.0
            };
            let stale_t = (n.summary_membership_churn as f32 / 10.0).clamp(0.0, 0.7);
            let fill = blend(palette.base_for(n), palette.stale, stale_t);
            out.push(NodeDescriptor {
                index: idx,
                world_pos: positions[idx],
                radius: base_size,
                shape: if is_cluster {
                    NodeShape::Circle
                } else {
                    NodeShape::Square
                },
                fill,
                resting_stroke: egui::Stroke::NONE,
                hover_stroke: egui::Stroke::new(1.5, egui::Color32::WHITE),
                badge: None,
                bug_badge: None,
                label: is_cluster.then(|| n.name.clone()),
                label_min_zoom: 0.55,
                label_scale: 1.0,
                click_path: if self.clickable_leaves {
                    n.note_path.clone()
                } else {
                    None
                },
                tooltip: Some(n.name.clone()),
            });
        }
        out
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.data
            .parent_of
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.map(|pp| (pp as u32, i as u32)))
            .collect()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        LayoutTree::from_parents(&self.data.parent_of)
    }

    fn node_key(&self, index: usize) -> Option<String> {
        // A cluster/leaf id is the stable identity across re-clustering; a node
        // whose id changes is simply treated as new and settles in fresh.
        self.data.ids.get(index).cloned()
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let id = self.data.ids.get(index)?;
        let n = self.nodes.iter().find(|n| &n.id == id)?;
        // Leaf: file body (post-frontmatter), else its summary, else a
        // fallback. Cluster: in-memory summary, else "(no summary)".
        if let Some(path) = n.note_path.as_deref() {
            let body = self
                .vault
                .read_file(path)
                .ok()
                .map(|s| crate::panels::graph::preview_snippet(crate::panels::graph::skip_frontmatter(&s)))
                .or_else(|| (!n.summary.is_empty()).then(|| n.summary.clone()))
                .unwrap_or_else(|| "(unable to read note)".to_string());
            Some((crate::panels::graph::basename(path), body))
        } else {
            let body = if n.summary.is_empty() {
                "(no summary)".to_string()
            } else {
                n.summary.clone()
            };
            Some((n.name.clone(), body))
        }
    }
}

/// The five cluster encoding colors pulled from the active palette.
struct PolicyColors {
    cluster: egui::Color32,
    move_policy: egui::Color32,
    tag_policy: egui::Color32,
    leaf: egui::Color32,
    stale: egui::Color32,
}

impl From<Palette> for PolicyColors {
    fn from(palette: Palette) -> Self {
        match palette {
            Palette::Policy {
                cluster,
                move_policy,
                tag_policy,
                leaf,
                stale,
            } => Self {
                cluster,
                move_policy,
                tag_policy,
                leaf,
                stale,
            },
            // The cluster view always carries a policy palette; fall back to
            // the flat node color so a misconfig is visible, not blank.
            Palette::Flat { node, .. } => Self {
                cluster: node,
                move_policy: node,
                tag_policy: node,
                leaf: node,
                stale: egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
            },
        }
    }
}

impl PolicyColors {
    const fn base_for(&self, n: &EditableNode) -> egui::Color32 {
        match (&n.kind, n.policy.as_ref()) {
            (NodeKind::Leaf, _) => self.leaf,
            (_, Some(NodePolicy::Move { .. })) => self.move_policy,
            (_, Some(NodePolicy::Tag { .. })) => self.tag_policy,
            _ => self.cluster,
        }
    }
}

/// Walk every node and count its Leaf descendants. Cluster nodes carry the
/// count; leaves carry 1.
fn compute_member_counts(nodes: &[EditableNode]) -> HashMap<String, usize> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes {
        if let Some(p) = n.parent.as_deref() {
            children.entry(p).or_default().push(n.id.as_str());
        }
    }
    let by_id: HashMap<&str, &EditableNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        let v = count_members(n.id.as_str(), &by_id, &children, &mut out);
        out.insert(n.id.clone(), v);
    }
    out
}

fn count_members(
    id: &str,
    by_id: &HashMap<&str, &EditableNode>,
    children: &HashMap<&str, Vec<&str>>,
    memo: &mut HashMap<String, usize>,
) -> usize {
    if let Some(v) = memo.get(id) {
        return *v;
    }
    let Some(n) = by_id.get(id) else {
        return 0;
    };
    let v = if matches!(n.kind, NodeKind::Leaf) {
        1
    } else {
        children
            .get(id)
            .map(|cs| {
                cs.iter()
                    .map(|c| count_members(c, by_id, children, memo))
                    .sum()
            })
            .unwrap_or(0)
    };
    memo.insert(id.to_string(), v);
    v
}

/// Linear-interpolate two colors. `t=0` returns `a`, `t=1` returns `b`.
fn blend(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    egui::Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        a.a(),
    )
}

#[cfg(test)]
mod tests {
    use egui_workbench::menu::Entry;
    use hiker_core::trees::types::{EditableNode, NodeKind};

    use super::build_node_menu;

    fn node(kind: NodeKind, note_path: Option<&str>) -> EditableNode {
        EditableNode {
            id: "n1".to_string(),
            parent: None,
            kind,
            note_path: note_path.map(str::to_string),
            name: "Cluster name".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        }
    }

    /// Menu composition: a leaf is a note ref and gets the full shared
    /// note-item base; a cluster node has no openable note behind it (its doc
    /// routes back to this graph view) so it gets the minimal Copy-name menu.
    #[test]
    fn leaf_gets_note_base_and_cluster_gets_copy_name() {
        let leaf = build_node_menu(&node(NodeKind::Leaf, Some("notes/a.md")));
        let sections = leaf.sections();
        assert_eq!(sections.len(), 1, "base verbs in one section");
        assert_eq!(sections[0].len(), 5, "Open · Reveal · Open in graph · Copy path · Properties");
        let labels: Vec<&str> = sections[0]
            .iter()
            .map(|e| match e {
                Entry::Action { label, .. } => label.as_ref(),
                Entry::Custom(_) => "(custom)",
                _ => panic!("unexpected entry kind"),
            })
            .collect();
        assert_eq!(
            labels,
            ["Open", "Reveal in file tree", "Open in graph", "(custom)", "Properties"]
        );

        let cluster = build_node_menu(&node(NodeKind::Cluster, None));
        let sections = cluster.sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].len(), 1, "Copy name only");
        assert!(matches!(sections[0][0], Entry::Custom(_)), "Copy name is a Custom entry");
    }
}
