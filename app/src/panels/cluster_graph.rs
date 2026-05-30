//! Cluster-tree graph view (`cluster-editor-graph-view`). Renders a
//! cluster tree under one of four layouts (radial / vertical tree /
//! horizontal tree / force-directed). Force-directed runs in a
//! background thread with Barnes–Hut repulsion + freeze-on-converge
//! (see `widgets::force_layout`); the tree layouts are pure O(n).
//!
//! State lives on `AppState::cluster_graph_states`, keyed by tree id.
//! That's where the (non-Clone) background layout worker hangs, so
//! flipping between tabs doesn't re-do the layout work.

use std::collections::HashMap;

use eframe::egui;

use crate::icons;
use crate::state::AppState;
use crate::theme;
use crate::widgets::force_graph::{View, ZoomBounds};
use graph_widgets::force_layout::{LayoutParams, LayoutWorker};
use graph_widgets::graph_layouts::{
    LayoutKind, LayoutTree, horizontal_tree_positions, radial_positions,
    vertical_tree_positions,
};

/// Persistent per-tree state. Held on `AppState`, not egui memory,
/// because `LayoutWorker` isn't `Clone`.
pub struct ClusterGraph {
    /// Stable id list — `positions[i]` corresponds to `ids[i]`.
    pub ids: Vec<String>,
    pub id_index: HashMap<String, usize>,
    pub parent_of: Vec<Option<usize>>,
    pub positions: Vec<egui::Vec2>,
    pub layout_kind: LayoutKind,
    /// Force-directed layout worker; `Some` only when
    /// `layout_kind == ForceDirected`.
    pub layout_worker: Option<LayoutWorker>,
    pub view: View,
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_leaves: bool,
    /// Note-preview overlay: toggle, selected note path, cached body
    /// snippet. Refreshed only when the selection changes.
    pub show_preview: bool,
    pub selected_path: Option<String>,
    pub selected_preview: Option<String>,
    /// Tracks the node-list shape we last seeded for. When the upstream
    /// tree changes we rebuild positions.
    pub seeded_for: u64,
    /// True after a layout rebuild — `show()` will then fit pan/zoom
    /// to the position bounding box on the next paint so the user
    /// doesn't open the panel to an off-screen layout.
    pub needs_fit: bool,
}

impl Default for ClusterGraph {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            id_index: HashMap::new(),
            parent_of: Vec::new(),
            positions: Vec::new(),
            layout_kind: LayoutKind::Radial,
            layout_worker: None,
            view: View::default(),
            show_labels: true,
            show_edges: true,
            show_leaves: true,
            show_preview: false,
            selected_path: None,
            selected_preview: None,
            seeded_for: 0,
            needs_fit: false,
        }
    }
}

const FR_BOX: f32 = 800.0;

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
        ui.label(
            egui::RichText::new("(tree is empty)")
                .color(theme::muted()),
        );
        return;
    }
    // Persisted-tree entry point: clickable leaves resolve to vault paths
    // via `app.vault_session.services.read_store`.
    show_with_nodes(ui, app, tree_id, &nodes, /*clickable_leaves=*/ true);
}

/// Render the cluster graph from a pre-resolved `EditableNode` slice
/// instead of loading from the tree's `.md`. Shared between the persisted
/// cluster-tree tab (`show`) and the un-persisted `BuiltClusterTree`
/// preview in `clusters::panel` (see
/// `built_tree_to_editable_nodes` for the synthesis path).
///
/// `state_key` is used to namespace the per-tree layout cache on
/// `AppState::cluster_graph_states`. Persisted trees pass their tree id;
/// preview callers pass a stable tab-scoped key so the layout survives
/// frame-to-frame without colliding with a persisted tree.
///
/// `clickable_leaves = false` disables the click-to-open-note path: the
/// review preview holds note-id-shaped leaves that aren't necessarily
/// resolvable through `app.vault_session.services.read_store` (the build was synchronous off
/// the vault walk; ids may be vault-relative paths or arbitrary).
///
/// status: cluster-review-tab-result-graph-view
pub fn show_with_nodes(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tree_id: &str,
    nodes: &[hiker_core::trees::types::EditableNode],
    clickable_leaves: bool,
) {
    if nodes.is_empty() {
        ui.label(
            egui::RichText::new("(tree is empty)")
                .color(theme::muted()),
        );
        return;
    }
    // Cheap shape fingerprint over the (id, parent) edges. Changes when
    // nodes are added, removed, or re-parented; doesn't churn on summary/
    // policy edits.
    let shape_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        nodes.len().hash(&mut h);
        for n in nodes {
            n.id.hash(&mut h);
            n.parent.hash(&mut h);
        }
        h.finish()
    };

    // Ensure per-tree state exists and is fresh.
    {
        let entry = app
            .panels.cluster_graph
            .entry(tree_id.to_string())
            .or_default();
        if entry.seeded_for != shape_hash {
            entry.seed(nodes, shape_hash);
            recompute_layout(entry);
        }
    }

    // Toolbar + legend (mutates view toggles via the per-tree state).
    let (mut reset_view, mut relayout) = (false, false);
    {
        let state = app.panels.cluster_graph.get_mut(tree_id).unwrap();
        ui.horizontal_wrapped(|ui| {
            let prev_kind = state.layout_kind;
            state.view_options_menu(ui);
            if state.layout_kind != prev_kind {
                relayout = true;
            }
            if ui.small_button("Reset view").clicked() {
                reset_view = true;
            }
            ui.label(
                egui::RichText::new(format!(
                    "{} · zoom {:.2}x",
                    state.layout_kind.label(),
                    state.view.zoom
                ))
                .color(theme::muted())
                .small(),
            );
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Encoding:")
                    .color(theme::muted())
                    .small(),
            );
            legend_swatch(ui, theme::accent(), "cluster");
            legend_swatch(ui, egui::Color32::from_rgb(0x2f, 0x6f, 0xb9), "move policy");
            legend_swatch(ui, egui::Color32::from_rgb(0xa8, 0x4a, 0xc4), "tag policy");
            legend_swatch(ui, egui::Color32::from_rgb(0x2f, 0x8f, 0x4d), "leaf");
        });
        ui.add_space(4.0);
    }
    if reset_view {
        app.panels.cluster_graph
            .get_mut(tree_id)
            .unwrap()
            .needs_fit = true;
    }
    if relayout {
        recompute_layout(app.panels.cluster_graph.get_mut(tree_id).unwrap());
    }

    // Canvas.
    let size = egui::Vec2::new(ui.available_width(), ui.available_height() - 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

    {
        let state = app.panels.cluster_graph.get_mut(tree_id).unwrap();
        state.view.handle_input(
            ui,
            &resp,
            rect,
            ZoomBounds { min: 0.005, max: 6.0 },
        );

        // Pull latest positions from worker if running.
        if let Some(w) = state.layout_worker.as_ref()
            && w.is_running()
        {
            w.snapshot_into(&mut state.positions);
            ui.ctx().request_repaint();
        }

        // Auto-fit pan/zoom to the layout's bounding box right after a
        // rebuild (or any time the kind changed) so users never open the
        // panel to an off-screen layout. We refit a few times while the
        // force layout settles so the framing tracks the changing scale.
        let still_settling =
            state.layout_worker.as_ref().is_some_and(graph_widgets::force_layout::LayoutWorker::is_running);
        if state.needs_fit && !state.positions.is_empty() {
            state.view.fit_to_positions(&state.positions, rect, (0.005, 6.0));
            if !still_settling {
                state.needs_fit = false;
            }
        }
    }

    // Render. Take an immutable snapshot of state to avoid holding a
    // mutable borrow during painting (we still need &AppState below for
    // the click callback).
    let state_ref = app.panels.cluster_graph.get(tree_id).unwrap();
    let to_screen = state_ref.view.screen_mapper(rect);
    let positions = state_ref.positions.clone();
    let ids = state_ref.ids.clone();
    let parent_of = state_ref.parent_of.clone();
    let show_edges = state_ref.show_edges;
    let show_labels = state_ref.show_labels;
    let show_leaves = state_ref.show_leaves;
    let zoom = state_ref.view.zoom;

    if show_edges {
        for (i, parent) in parent_of.iter().enumerate() {
            if let Some(p) = *parent
                && i < positions.len()
                && p < positions.len()
            {
                painter.line_segment(
                    [to_screen(positions[i]), to_screen(positions[p])],
                    egui::Stroke::new(1.0, theme::divider()),
                );
            }
        }
    }

    let hits = PaintCtx {
        painter: &painter,
        nodes,
        ids: &ids,
        positions: &positions,
        show_labels,
        show_leaves,
        zoom,
        resp: &resp,
        to_screen: &to_screen,
        app,
        clickable_leaves,
    }
    .paint_nodes();
    if let Some(path) = hits.clicked.clone() {
        crate::editor_pane::open_file(app, &path, /*sticky=*/ true);
        update_selection(app, tree_id, path);
    }

    app.render_cluster_preview(&painter, rect, tree_id, &hits);

    ui.add_space(4.0);
    use hiker_core::trees::types::NodeKind;
    ui.label(
        egui::RichText::new(format!(
            "{} nodes · {} clusters · {} leaves",
            nodes.len(),
            nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Cluster))
                .count(),
            nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Leaf))
                .count(),
        ))
        .color(theme::muted())
        .small(),
    );
}

impl AppState {
    /// Hover-driven preview card for the cluster graph. For leaves we keep
    /// `selected_preview`'s file-content cache up to date (so the body text
    /// follows the cursor across leaves). For clusters there's no file to
    /// read — the card uses the node's in-memory summary directly.
    fn render_cluster_preview(
        &mut self,
        painter: &egui::Painter,
        rect: egui::Rect,
        tree_id: &str,
        hits: &PaintHits,
    ) {
        let show_preview = self
            .panels
            .cluster_graph
            .get(tree_id)
            .map(|s| s.show_preview)
            .unwrap_or(false);
        if !show_preview {
            return;
        }
        let Some(h) = hits.hovered.as_ref() else {
            return;
        };
        if let Some(path) = h.leaf_path.as_deref() {
            update_selection(self, tree_id, path.to_string());
        }

        let cached_body = self
            .panels
            .cluster_graph
            .get(tree_id)
            .and_then(|s| s.selected_preview.clone())
            .filter(|s| !s.is_empty());
        let (title, body): (String, String) = if let Some(path) = h.leaf_path.as_deref() {
            let title = path
                .rsplit('/')
                .next()
                .unwrap_or(path)
                .strip_suffix(".md")
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path))
                .to_string();
            // Body resolution for a leaf:
            //   1. cached file content (post-frontmatter)
            //   2. in-memory summary (rare for leaves but possible)
            //   3. "(unable to read note)" fallback
            let body = cached_body
                .or_else(|| {
                    if h.summary.is_empty() {
                        None
                    } else {
                        Some(h.summary.clone())
                    }
                })
                .unwrap_or_else(|| "(unable to read note)".to_string());
            (title, body)
        } else {
            // Non-leaf: prefer in-memory summary; fall back to a
            // cached body if some earlier hover left one (e.g. the
            // tree's last clicked leaf), since "(no summary)" is
            // less useful than showing real content somewhere in
            // the tree.
            let body = if !h.summary.is_empty() {
                h.summary.clone()
            } else {
                cached_body.unwrap_or_else(|| "(no summary)".to_string())
            };
            (h.name.clone(), body)
        };
        crate::panels::graph::paint_preview_card(painter, rect, &title, &body, h.screen_pos);
    }
}

impl ClusterGraph {
    /// View-options popup. Mirrors the buffer pane's eye-icon menu so all
    /// the per-view toggles live in one consistent place.
    fn view_options_menu(&mut self, ui: &mut egui::Ui) {
        let resp = ui
            .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Eye)))
            .on_hover_text("View options");
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Layout").small().color(theme::muted()));
            for kind in LayoutKind::all() {
                let mut selected = self.layout_kind == kind;
                if ui.checkbox(&mut selected, kind.label()).clicked() && selected {
                    self.layout_kind = kind;
                }
            }
            ui.separator();
            ui.checkbox(&mut self.show_labels, "Labels");
            ui.checkbox(&mut self.show_edges, "Edges");
            ui.checkbox(&mut self.show_leaves, "Leaves");
            ui.checkbox(&mut self.show_preview, "Show note preview");
        });
    }

    /// Rebuild the stable id list, parent index, and zeroed positions
    /// from a fresh `EditableNode` slice, recording the shape fingerprint
    /// it was seeded for.
    fn seed(&mut self, nodes: &[hiker_core::trees::types::EditableNode], shape_hash: u64) {
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
        self.positions = vec![egui::Vec2::ZERO; nodes.len()];
        self.layout_worker = None;
        self.seeded_for = shape_hash;
    }
}

/// (Re)compute positions for the current `layout_kind`. Force-directed
/// spawns the background worker; tree layouts run inline (O(n)).
fn recompute_layout(state: &mut ClusterGraph) {
    state.layout_worker = None;
    state.needs_fit = true;
    if state.ids.is_empty() {
        return;
    }
    let tree = LayoutTree::from_parents(&state.parent_of);
    let area = FR_BOX * FR_BOX;
    match state.layout_kind {
        LayoutKind::Radial => {
            state.positions = radial_positions(&tree, area);
        }
        LayoutKind::VerticalTree => {
            state.positions = vertical_tree_positions(&tree, area);
        }
        LayoutKind::HorizontalTree => {
            state.positions = horizontal_tree_positions(&tree, area);
        }
        LayoutKind::ForceDirected => {
            // Small random scatter seed. The radial seed used to live
            // here, but radial sizing scales with total leaf count
            // (~ leaves · MIN_LEAF_ARC / TAU) — on a cluster tree with
            // a few hundred leaves the outermost ring sits at radius
            // ~4000+ world units, which exceeded the layout `bound`
            // clamp and pinned all the leaves to the perimeter of the
            // [-bound, bound]² box. That looks like a literal square.
            // FA2 converges fine from a tight random scatter, so just
            // do that and let the algorithm find the natural scale.
            let mut rng_state: u64 = 0x517C_C1B7_2722_0A95;
            let mut rng = || {
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((rng_state >> 33) as u32) as f32 / (u32::MAX as f32)
            };
            let seed: Vec<egui::Vec2> = (0..state.ids.len())
                .map(|_| egui::vec2((rng() - 0.5) * 80.0, (rng() - 0.5) * 80.0))
                .collect();
            state.positions = seed.clone();
            let edges: Vec<(u32, u32)> = state
                .parent_of
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.map(|pp| (pp as u32, i as u32)))
                .collect();
            state.layout_worker = Some(LayoutWorker::spawn(
                seed,
                edges,
                LayoutParams {
                    // Bound only acts as a safety belt against runaway
                    // forces. Make it large enough that the natural
                    // FA2 equilibrium never touches it for any vault
                    // size we'd realistically see.
                    bound: 50_000.0,
                    ..LayoutParams::default()
                },
            ));
        }
    }
}

/// All the borrows [`PaintCtx::paint_nodes`] needs for one paint pass.
/// Bundling them keeps the painter a single inherent method rather than
/// a 11-argument free function.
struct PaintCtx<'a> {
    painter: &'a egui::Painter,
    nodes: &'a [hiker_core::trees::types::EditableNode],
    ids: &'a [String],
    positions: &'a [egui::Vec2],
    show_labels: bool,
    show_leaves: bool,
    zoom: f32,
    resp: &'a egui::Response,
    to_screen: &'a dyn Fn(egui::Vec2) -> egui::Pos2,
    app: &'a AppState,
    clickable_leaves: bool,
}

impl PaintCtx<'_> {
    /// Paint all cluster/leaf nodes with color-by-policy, size-by-members, and
    /// staleness encodings. Surfaces hover tooltips and detects clicks on leaf
    /// nodes carrying a `note_ref`, returning the resolved vault path so the
    /// caller can open it.
    fn paint_nodes(&self) -> PaintHits {
        use hiker_core::trees::types::{NodeKind, NodePolicy};

        let painter = self.painter;
        let nodes = self.nodes;
        let positions = self.positions;
        let show_labels = self.show_labels;
        let show_leaves = self.show_leaves;
        let zoom = self.zoom;
        let resp = self.resp;
        let to_screen = self.to_screen;
        let app = self.app;
        let clickable_leaves = self.clickable_leaves;

        let members = self.compute_member_counts();
        let max_members = members.values().copied().max().unwrap_or(1) as f32;

        // Build id→index lookup from `ids`.
        let id_index: HashMap<&str, usize> = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();

    let hover = resp.hover_pos();
    let mut tooltip: Option<(egui::Pos2, String)> = None;
    let mut clicked_path: Option<String> = None;
    let mut hovered_node: Option<HoveredNode> = None;
    for n in nodes {
        let Some(&idx) = id_index.get(n.id.as_str()) else {
            continue;
        };
        if idx >= positions.len() {
            continue;
        }
        let world_p = positions[idx];
        let is_cluster = matches!(n.kind, NodeKind::Cluster);
        if !is_cluster && !show_leaves {
            continue;
        }
        let p = to_screen(world_p);
        let member_count = members.get(n.id.as_str()).copied().unwrap_or(0) as f32;
        let base_size = if is_cluster {
            let frac = (member_count / max_members.max(1.0)).clamp(0.0, 1.0);
            8.0 + 12.0 * frac
        } else {
            5.0
        };
        let size = base_size * zoom.max(0.4);
        let base_fill = match (&n.kind, n.policy.as_ref()) {
            (NodeKind::Leaf, _) => egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
            (_, Some(NodePolicy::Move { .. })) => {
                egui::Color32::from_rgb(0x2f, 0x6f, 0xb9)
            }
            (_, Some(NodePolicy::Tag { .. })) => {
                egui::Color32::from_rgb(0xa8, 0x4a, 0xc4)
            }
            _ => theme::accent(),
        };
        let churn = n.summary_membership_churn as f32;
        let stale_t = (churn / 10.0).clamp(0.0, 0.7);
        let grey = egui::Color32::from_rgb(0xa0, 0xa0, 0xa0);
        let fill = self.blend(base_fill, grey, stale_t);
        let hovered = hover.map(|h| h.distance(p) < size + 4.0).unwrap_or(false);
        if is_cluster {
            painter.circle_filled(p, size, fill);
            if hovered {
                painter.circle_stroke(
                    p,
                    size + 2.0,
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                );
            }
        } else {
            let r = egui::Rect::from_center_size(p, egui::Vec2::splat(size * 2.0));
            painter.rect_filled(r, 1.0, fill);
            if hovered {
                painter.rect_stroke(
                    r.expand(2.0),
                    1.0,
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
            }
        }
        if show_labels && is_cluster && zoom > 0.55 {
            painter.text(
                p + egui::Vec2::new(0.0, size + 2.0),
                egui::Align2::CENTER_TOP,
                &n.name,
                egui::FontId::proportional(11.0),
                theme::muted(),
            );
        }
        if hovered {
            tooltip = Some((p + egui::Vec2::new(10.0, -10.0), n.name.clone()));
            // Resolve the leaf path (if any) while we have a fresh
            // store lock. For non-leaf nodes there is no file to
            // preview — the card will render the in-memory summary
            // instead. Either way we record the screen position so
            // the preview can anchor near the hovered node.
            //
            // `note_ref` semantics differ between the persisted
            // cluster-tree tab (a real `NoteId` looked up through the
            // store) and the cluster-review embed (a vault-relative
            // path stuffed into the same field, since the build runs
            // off `NoteInput { id: rel_path, … }`). Try the store
            // first; fall back to treating `note_ref` as a path so
            // the review preview also gets a working preview card.
            let leaf_path = if let Some(note_id) = &n.note_ref {
                // status: store-id-from-oplog
                let resolved = app
                    .vault_session
                    .services
                    .oplog
                    .path_for_doc(note_id)
                    .ok()
                    .flatten();
                let path = resolved.unwrap_or_else(|| note_id.clone());
                if clickable_leaves && resp.clicked() {
                    clicked_path = Some(path.clone());
                }
                Some(path)
            } else {
                None
            };
            hovered_node = Some(HoveredNode {
                name: n.name.clone(),
                summary: n.summary.clone(),
                leaf_path,
                screen_pos: p,
            });
        }
    }
    if let Some((p, txt)) = tooltip {
        let galley = painter.layout_no_wrap(
            txt,
            egui::FontId::proportional(12.0),
            egui::Color32::BLACK,
        );
        let bg_rect = egui::Rect::from_min_size(p, galley.size())
            .expand(4.0);
        painter.rect_filled(
            bg_rect,
            2.0,
            egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 230),
        );
        painter.galley(p, galley, egui::Color32::BLACK);
    }
    PaintHits {
        clicked: clicked_path,
        hovered: hovered_node,
    }
    }

    /// Walk every node and count its Leaf descendants. Returns a map keyed
    /// by node id; cluster nodes carry the count, leaves carry 1 (themselves).
    fn compute_member_counts(&self) -> HashMap<String, usize> {
        use hiker_core::trees::types::NodeKind;
        let nodes = self.nodes;
        let mut out: HashMap<String, usize> = HashMap::new();
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in nodes {
            if let Some(p) = n.parent.as_deref() {
                children.entry(p).or_default().push(n.id.as_str());
            }
        }
        let by_id: HashMap<&str, &hiker_core::trees::types::EditableNode> =
            nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        fn count(
            id: &str,
            by_id: &HashMap<&str, &hiker_core::trees::types::EditableNode>,
            children: &HashMap<&str, Vec<&str>>,
            memo: &mut HashMap<String, usize>,
        ) -> usize {
            if let Some(v) = memo.get(id) {
                return *v;
            }
            let n = match by_id.get(id) {
                Some(n) => n,
                None => return 0,
            };
            let v = if matches!(n.kind, NodeKind::Leaf) {
                1
            } else {
                children
                    .get(id)
                    .map(|cs| cs.iter().map(|c| count(c, by_id, children, memo)).sum())
                    .unwrap_or(0)
            };
            memo.insert(id.to_string(), v);
            v
        }
        for n in nodes {
            let v = count(n.id.as_str(), &by_id, &children, &mut out);
            out.insert(n.id.clone(), v);
        }
        out
    }

    /// Linear-interpolate two colors. `t=0` returns `a`, `t=1` returns `b`.
    fn blend(&self, a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
        let t = t.clamp(0.0, 1.0);
        let lerp = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
        egui::Color32::from_rgba_unmultiplied(
            lerp(a.r(), b.r()),
            lerp(a.g(), b.g()),
            lerp(a.b(), b.b()),
            a.a(),
        )
    }
}

/// Output of [`PaintCtx::paint_nodes`]: which leaf was clicked this frame (used
/// to open the note) and which node the cursor is currently over (used
/// to drive the live preview card).
struct PaintHits {
    clicked: Option<String>,
    hovered: Option<HoveredNode>,
}

/// Snapshot of the cluster/leaf node currently under the cursor.
/// Carries everything the preview card needs to render — including
/// the on-screen position so the card can anchor next to the node
/// rather than parking in a fixed corner.
struct HoveredNode {
    name: String,
    summary: String,
    /// Resolved vault path when this is a leaf with a `note_ref`. None
    /// for clusters (and for leaves whose note_ref can't be resolved
    /// through the store) — the card falls back to the in-memory
    /// `summary` for those.
    leaf_path: Option<String>,
    screen_pos: egui::Pos2,
}

/// Refresh `selected_path` + `selected_preview` for the given tree on
/// click. Mirrors `panels::graph::update_selection`.
fn update_selection(app: &mut AppState, tree_id: &str, path: String) {
    let needs_load = app
        .panels.cluster_graph
        .get(tree_id)
        .map(|s| s.selected_path.as_deref() != Some(path.as_str()))
        .unwrap_or(false);
    if !needs_load {
        return;
    }
    // Body preview, capped at 500 chars (post-frontmatter).
    const MAX: usize = 500;
    let preview = app
        .vault_session
        .vault
        .read_file(&path)
        .ok()
        .map(|s| {
            let body = crate::panels::graph::skip_frontmatter(&s);
            if body.chars().count() <= MAX {
                body.to_string()
            } else {
                let mut out: String = body.chars().take(MAX).collect();
                out.push('…');
                out
            }
        });
    if let Some(s) = app.panels.cluster_graph.get_mut(tree_id) {
        s.selected_path = Some(path);
        s.selected_preview = preview;
    }
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(egui::RichText::new(label).small().color(theme::muted()));
}
