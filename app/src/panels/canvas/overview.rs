//! The canvas board's Poincaré OVERVIEW: a simplified graph of the canvas that
//! drives the corner minimap and the expand-swap.
//!
//! Rather than re-projecting the whole canvas (cards, content) through a second
//! camera — which hairballed and never matched the canvas's own layout — the
//! overview is a [`hiker_graph_view`] instance: each non-group card becomes a
//! coloured node at its canvas-space CENTER, canvas edges become graph edges, and
//! the graph view's locked Poincaré disk renders them as a clean disk of dots.
//! Navigating the overview (Möbius drag + click fly-to) and swapping back to the
//! canvas re-centers the canvas viewport on the focused node.
//!
//! [`CanvasGraphSource`] is the [`hiker_graph_view::graph_view::source::Source`] adapter;
//! [`Model`] is the egui-free, unit-testable spine (the id↔index map, the
//! edge index pairs, viewport membership, and the focused-node pick) it reads
//! from. The panel sets the graph view's `positions` to the card centers directly
//! (never `recompute_layout`) so the overview shows the canvas's actual layout,
//! projected. status: canvas-minimap

use eframe::egui;

use canvas_view::palette::resolve_node;
use hiker_canvas::geometry::node_bounds;
use hiker_canvas::model::{Canvas, NodeKind};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::source::{NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::Style;

/// Radius (world units) every overview dot draws at. Small — the overview reads
/// as dots, never cards.
const DOT_RADIUS: f32 = 4.0;

/// Zoom at or above which an overview dot's label shows. Modest, so labels
/// surface when the user zooms/navigates the disk but don't clutter the corner.
const LABEL_MIN_ZOOM: f32 = 0.9;

/// The egui-free spine of the overview: the canvas's non-group cards mapped to a
/// dense `0..node_count` index space (the order the graph view's `positions`
/// vector follows), the canvas edges as index pairs over that space, and the
/// per-card center / color / title needed to build the descriptors.
///
/// Built once per frame from the live [`Canvas`]; pure so the mapping, edge
/// projection, viewport membership, and focused-node pick are all unit-testable
/// without egui.
pub struct Model {
    /// One entry per non-group card, in index order. The graph view's
    /// `positions[i]` corresponds to `cards[i]`.
    cards: Vec<Card>,
    /// Canvas edges as `(u32, u32)` index pairs over `cards`. Edges touching a
    /// group node or an unknown id are skipped.
    edges: Vec<(u32, u32)>,
}

/// One overview node: a non-group card's identity, world center, resolved color,
/// and title.
struct Card {
    /// The canvas node id — stable across rebuilds, used as the graph view's
    /// `node_key`, `click_path`, and the swap-back focus target.
    id: String,
    /// Card center in canvas coordinates (the graph view's `positions[i]`).
    center: egui::Vec2,
    /// The dot fill — the card's color resolved through the canvas palette, or a
    /// muted default when the card has no color.
    fill: egui::Color32,
    /// The card's title (first non-empty line / basename / link host).
    title: String,
}

impl Model {
    /// Build the overview spine from `canvas`, resolving each card's color for
    /// `visuals`. Non-group cards become indexed nodes; edges between two known
    /// non-group cards become index pairs.
    #[must_use]
    pub fn build(canvas: &Canvas, visuals: &egui::Visuals) -> Self {
        let mut cards = Vec::new();
        let mut index_of = std::collections::HashMap::new();
        for node in &canvas.nodes {
            if matches!(node.kind, NodeKind::Group { .. }) {
                continue;
            }
            let b = node_bounds(node);
            let center = egui::vec2(b.center().x as f32, b.center().y as f32);
            let fill = dot_fill(node.color.as_ref(), visuals);
            index_of.insert(node.id.clone(), cards.len() as u32);
            cards.push(Card { id: node.id.clone(), center, fill, title: card_title(node) });
        }
        let edges = canvas
            .edges
            .iter()
            .filter_map(|e| {
                let u = *index_of.get(&e.from_node)?;
                let v = *index_of.get(&e.to_node)?;
                (u != v).then_some((u, v))
            })
            .collect();
        Self { cards, edges }
    }

    /// The number of overview nodes (non-group cards).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.cards.len()
    }

    /// The card centers in canvas coordinates, aligned to the node index order —
    /// the panel assigns these straight to the graph view's `positions`.
    #[must_use]
    pub fn positions(&self) -> Vec<egui::Vec2> {
        self.cards.iter().map(|c| c.center).collect()
    }

    /// The canvas node id at index `i` (the stable `node_key` / click path).
    #[must_use]
    pub fn id_at(&self, i: usize) -> Option<&str> {
        self.cards.get(i).map(|c| c.id.as_str())
    }
}

/// The graph-view [`Source`] over an [`Model`]: emits one coloured dot per card
/// with its title label + click path. Positions are supplied by the panel (the
/// card centers), never force-laid-out, so `layout_tree` is trivial and
/// `preview_for` is `None`. The viewport-location indicator + focus pick are
/// applied by the engine [`Minimap`](hiker_graph_view::graph_view::minimap::Minimap), so
/// this source stays a plain data provider. status: canvas-minimap
pub struct CanvasGraphSource<'a> {
    model: &'a Model,
    /// Hover-ring colour for the dots (the theme selection accent).
    hover: egui::Color32,
}

impl<'a> CanvasGraphSource<'a> {
    /// Build the source from the overview model + the active visuals (for the
    /// dot hover-ring colour).
    #[must_use]
    pub const fn new(model: &'a Model, visuals: &egui::Visuals) -> Self {
        Self { model, hover: visuals.selection.stroke.color }
    }
}

impl Source for CanvasGraphSource<'_> {
    fn node_count(&self) -> usize {
        self.model.node_count()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        self.model
            .cards
            .iter()
            .enumerate()
            .map(|(index, card)| NodeDescriptor {
                index,
                world_pos: positions.get(index).copied().unwrap_or(card.center),
                radius: DOT_RADIUS,
                shape: NodeShape::Circle,
                fill: card.fill,
                resting_stroke: egui::Stroke::NONE,
                hover_stroke: egui::Stroke::new(2.0, self.hover),
                badge: None,
                bug_badge: None,
                label: (!card.title.is_empty()).then(|| card.title.clone()),
                label_min_zoom: LABEL_MIN_ZOOM,
                label_scale: 1.0,
                click_path: Some(card.id.clone()),
                tooltip: (!card.title.is_empty()).then(|| card.title.clone()),
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.model.edges.clone()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        // Positions are set directly by the panel (the card centers), so the
        // graph view never force-/tree-lays-out the overview.
        LayoutTree {
            n: 0,
            children: Vec::new(),
            roots: Vec::new(),
            depth: Vec::new(),
            subtree_leaves: Vec::new(),
        }
    }

    fn preview_for(&self, _index: usize) -> Option<(String, String)> {
        None
    }

    fn node_key(&self, index: usize) -> Option<String> {
        self.model.id_at(index).map(ToString::to_string)
    }
}

/// The dot fill for a card color: the resolved accent STROKE (the vivid hue, not
/// the translucent card tint), or a muted default when the card has no color.
fn dot_fill(color: Option<&hiker_canvas::color::Color>, visuals: &egui::Visuals) -> egui::Color32 {
    match color {
        Some(c) => resolve_node(Some(c), visuals).stroke,
        None => visuals.weak_text_color(),
    }
}

/// A cheap one-line title for a card, mirroring the canvas paint layer's LOD
/// title: a File yields its basename (no dir, no `.md`), a Text its first
/// non-empty line, a Link its host. Groups never reach the overview.
fn card_title(node: &hiker_canvas::model::Node) -> String {
    match &node.kind {
        NodeKind::File { file, .. } => {
            let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
            base.strip_suffix(".md").unwrap_or(base).to_string()
        }
        NodeKind::Text { text } => text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string(),
        NodeKind::Link { url } => {
            let after = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
            after.split(['/', '?', '#']).next().unwrap_or(after).to_string()
        }
        NodeKind::Group { .. } => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiker_canvas::model::{Edge, Node};

    /// A non-group card at `(x, y)` sized `w×h` with optional color.
    fn card(id: &str, x: i64, y: i64, w: i64, h: i64, color: Option<hiker_canvas::color::Color>) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: w,
            height: h,
            color,
            kind: NodeKind::Text { text: format!("title {id}") },
            extra: Default::default(),
        }
    }

    /// A group node (excluded from the overview).
    fn group(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: w,
            height: h,
            color: None,
            kind: NodeKind::Group { label: None, background: None, background_style: None },
            extra: Default::default(),
        }
    }

    fn edge(id: &str, from: &str, to: &str) -> Edge {
        Edge {
            id: id.to_owned(),
            from_node: from.to_owned(),
            from_side: None,
            from_end: None,
            to_node: to.to_owned(),
            to_side: None,
            to_end: None,
            color: None,
            label: None,
            extra: Default::default(),
        }
    }

    /// A small canvas: 3 cards, 2 edges, 1 group — and one edge touching the
    /// group (which must be skipped).
    fn sample() -> Canvas {
        let mut c = Canvas::default();
        c.nodes.push(card("a", 0, 0, 100, 100, Some(hiker_canvas::color::Color::Preset(4))));
        c.nodes.push(card("b", 1000, 0, 100, 100, None));
        c.nodes.push(card("c", 0, 1000, 100, 100, Some(hiker_canvas::color::Color::Hex("#112233".to_owned()))));
        c.nodes.push(group("g", -50, -50, 2000, 2000));
        c.edges.push(edge("e1", "a", "b"));
        c.edges.push(edge("e2", "b", "c"));
        // Touches the group → skipped.
        c.edges.push(edge("e3", "a", "g"));
        c
    }

    #[test]
    fn canvas_graph_source_maps_cards_and_edges() {
        let canvas = sample();
        let model = Model::build(&canvas, &egui::Visuals::dark());
        // node_count is the non-group card count (group excluded).
        assert_eq!(model.node_count(), 3, "3 cards, group excluded");
        // node_key == card id, in document order.
        assert_eq!(model.id_at(0), Some("a"));
        assert_eq!(model.id_at(1), Some("b"));
        assert_eq!(model.id_at(2), Some("c"));
        // Edges as index pairs; the group-touching edge is skipped.
        assert_eq!(model.edges, vec![(0, 1), (1, 2)], "group edge skipped");
        // Positions are card centers.
        let pos = model.positions();
        assert_eq!(pos[0], egui::vec2(50.0, 50.0));
        assert_eq!(pos[1], egui::vec2(1050.0, 50.0));
        // Colors resolve: the colored card gets a vivid (non-muted) dot, the
        // uncolored one falls back to muted.
        let visuals = egui::Visuals::dark();
        let src = CanvasGraphSource::new(&model, &visuals);
        let descs = src.nodes(&pos, &Style::flat());
        assert_eq!(descs.len(), 3);
        let muted = visuals.weak_text_color();
        assert_ne!(descs[0].fill, muted, "preset-colored card resolves a vivid dot");
        assert_eq!(descs[1].fill, muted, "uncolored card defaults to muted");
        // node_key round-trips the id.
        assert_eq!(src.node_key(2).as_deref(), Some("c"));
        // click_path carries the id for navigation.
        assert_eq!(descs[2].click_path.as_deref(), Some("c"));
    }
}
