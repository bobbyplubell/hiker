//! Cluster-tree graph view (`cluster-editor-graph-view`). Renders a cluster
//! tree (radial / vertical / horizontal / force-directed) through the shared
//! `widgets::graph_view` engine. This panel is the cluster-specific
//! [`graph_view::Source`] adapter: it maps `EditableNode` rows to colored,
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
use crate::widgets::graph_view::{
    self, policy_legend, LayoutConfig, NodeDescriptor, NodeShape, Palette, Source, Style,
};
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
/// the cluster-specific "Leaves" toggle.
pub struct ClusterView {
    engine: graph_view::State,
    data: ClusterData,
    show_leaves: bool,
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
    show_with_nodes(ui, app, tree_id, &nodes, /*clickable_leaves=*/ true, /*preserve_view=*/ false);
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
/// status: cluster-review-tab-result-graph-view
/// status: cluster-review-tab-live-preview
pub fn show_with_nodes(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tree_id: &str,
    nodes: &[EditableNode],
    clickable_leaves: bool,
    preserve_view: bool,
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
            relayout = view
                .engine
                .view_options_menu(ui, Some(("Leaves", &mut view.show_leaves)));
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

    // Canvas (reserve a line at the bottom for the count summary).
    let clicked = render_canvas(ui, app, tree_id, nodes, clickable_leaves);
    if let Some(path) = clicked {
        crate::editor_pane::open_file(app, &path, /*sticky=*/ true);
    }

    summary_label(ui, nodes);
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
    } = view;
    let source = ClusterSource::new(nodes, data, vault.as_ref(), *show_leaves, clickable);
    let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(50.0));
    ui.allocate_ui(size, |ui| engine.ui(ui, &source)).inner
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
                label: is_cluster.then(|| n.name.clone()),
                label_min_zoom: 0.55,
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
