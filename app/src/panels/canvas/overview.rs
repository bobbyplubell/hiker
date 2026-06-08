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
//! [`CanvasGraphSource`] is the [`hiker_graph_view::graph_view::Source`] adapter;
//! [`Model`] is the egui-free, unit-testable spine (the id↔index map, the
//! edge index pairs, viewport membership, and the focused-node pick) it reads
//! from. The panel sets the graph view's `positions` to the card centers directly
//! (never `recompute_layout`) so the overview shows the canvas's actual layout,
//! projected. status: canvas-minimap

use eframe::egui;

use canvas_view::palette::resolve_node;
use hiker_canvas::geometry::{node_bounds, Rect as CanvasRect};
use hiker_canvas::model::{Canvas, NodeKind};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::{NodeDescriptor, NodeShape, Source, Style};
use hiker_projection::{forward, Complex, Mobius, ProjectionConfig, ProjectionKind};

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

    /// Which cards lie within the canvas viewport's world rect — the viewport
    /// indicator: a `true` flag means the overview should highlight that node so
    /// the disk shows WHERE the canvas viewport currently sits. A card counts as
    /// inside when its center is within `rect`.
    #[must_use]
    pub fn viewport_membership(&self, rect: CanvasRect) -> Vec<bool> {
        self.cards
            .iter()
            .map(|c| rect.contains(hiker_canvas::geometry::Point::new(f64::from(c.center.x), f64::from(c.center.y))))
            .collect()
    }

    /// The card whose projected disk point is nearest the disk origin under the
    /// overview's current `nav` + projection — the node a swap-back should center
    /// the canvas on. `None` for an empty overview. [the swap that moves the canvas]
    #[must_use]
    pub fn focused_card(&self, cfg: ProjectionConfig, nav: Mobius) -> Option<&str> {
        let (focus, scale) = centroid_scale(&self.positions());
        self.cards
            .iter()
            .min_by(|a, b| {
                let da = disk_point(a.center, focus, scale, cfg, nav).abs();
                let db = disk_point(b.center, focus, scale, cfg, nav).abs();
                da.total_cmp(&db)
            })
            .map(|c| c.id.as_str())
    }
}

/// The graph-view [`Source`] over an [`Model`]: emits one coloured dot
/// per card with its title label + click path, brightening cards inside the
/// canvas viewport so the overview marks where the viewport sits. Positions are
/// supplied by the panel (the card centers), never force-laid-out, so
/// `layout_tree` is trivial and `preview_for` is `None`.
pub struct CanvasGraphSource<'a> {
    model: &'a Model,
    /// Per-node viewport-membership flag, aligned to the index order — `true`
    /// brightens the node (the viewport indicator).
    in_viewport: Vec<bool>,
    /// Brighter highlight stroke for in-viewport nodes.
    highlight: egui::Color32,
}

impl<'a> CanvasGraphSource<'a> {
    /// Build the source from the overview model + the canvas viewport world rect
    /// (for the indicator) + the active visuals (for the highlight color).
    #[must_use]
    pub fn new(model: &'a Model, viewport_world: CanvasRect, visuals: &egui::Visuals) -> Self {
        Self {
            model,
            in_viewport: model.viewport_membership(viewport_world),
            highlight: visuals.selection.stroke.color,
        }
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
            .map(|(index, card)| {
                let hot = self.in_viewport.get(index).copied().unwrap_or(false);
                // In-viewport cards read brighter (the indicator); the world
                // position comes from the panel-supplied `positions`.
                let fill = if hot { brighten(card.fill) } else { card.fill };
                let resting = if hot {
                    egui::Stroke::new(2.0, self.highlight)
                } else {
                    egui::Stroke::NONE
                };
                NodeDescriptor {
                    index,
                    world_pos: positions.get(index).copied().unwrap_or(card.center),
                    radius: if hot { DOT_RADIUS * 1.4 } else { DOT_RADIUS },
                    shape: NodeShape::Circle,
                    fill,
                    resting_stroke: resting,
                    hover_stroke: egui::Stroke::new(2.0, self.highlight),
                    label: (!card.title.is_empty()).then(|| card.title.clone()),
                    label_min_zoom: LABEL_MIN_ZOOM,
                    click_path: Some(card.id.clone()),
                    tooltip: (!card.title.is_empty()).then(|| card.title.clone()),
                }
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

/// A brighter variant of `c` for the in-viewport indicator: blend toward white.
fn brighten(c: egui::Color32) -> egui::Color32 {
    let mix = |v: u8| (u16::from(v) + (255 - u16::from(v)) * 6 / 10) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
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

/// The centroid + normalising scale of a set of world positions, mirroring the
/// graph view's private `Lens` framing so [`focused_card`](Model::focused_card)
/// projects exactly as the rendered overview does.
fn centroid_scale(positions: &[egui::Vec2]) -> (egui::Vec2, f32) {
    if positions.is_empty() {
        return (egui::Vec2::ZERO, 1.0);
    }
    let mut sum = egui::Vec2::ZERO;
    for &p in positions {
        sum += p;
    }
    let centroid = sum / positions.len() as f32;
    let scale = positions
        .iter()
        .map(|&p| (p - centroid).length())
        .fold(0.0_f32, f32::max)
        .max(1.0);
    (centroid, scale)
}

/// The post-nav Poincaré disk point of a world position, mirroring the graph
/// view's `Lens::disk`: `forward((w − focus) / scale)` then the Möbius `nav`.
fn disk_point(w: egui::Vec2, focus: egui::Vec2, scale: f32, cfg: ProjectionConfig, nav: Mobius) -> Complex {
    let rel = (w - focus) / scale;
    let z = forward(Complex::from([rel.x, rel.y]), cfg);
    if cfg.kind == ProjectionKind::Poincare {
        nav.apply(z)
    } else {
        z
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
        let src = CanvasGraphSource::new(&model, CanvasRect::new(-1.0, -1.0, 1.0, 1.0), &visuals);
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

    #[test]
    fn viewport_membership_marks_visible_cards() {
        let canvas = sample();
        let model = Model::build(&canvas, &egui::Visuals::dark());
        // A viewport rect covering only card `a`'s center (50, 50).
        let rect = CanvasRect::new(0.0, 0.0, 200.0, 200.0);
        let flags = model.viewport_membership(rect);
        assert_eq!(flags, vec![true, false, false], "only `a` is inside the rect");
        // A rect covering b's center (1050, 50) but not a or c.
        let rect2 = CanvasRect::new(1000.0, 0.0, 200.0, 200.0);
        assert_eq!(model.viewport_membership(rect2), vec![false, true, false]);
    }

    #[test]
    fn focused_card_is_nearest_disk_center() {
        let canvas = sample();
        let model = Model::build(&canvas, &egui::Visuals::dark());
        let cfg = ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.2,
            ..Default::default()
        };
        // With identity nav the focus is the centroid; the card nearest the
        // centroid projects nearest the origin.
        let (focus, scale) = centroid_scale(&model.positions());
        let nearest = (0..model.node_count())
            .min_by(|&a, &b| {
                disk_point(model.positions()[a], focus, scale, cfg, Mobius::identity())
                    .abs()
                    .total_cmp(&disk_point(model.positions()[b], focus, scale, cfg, Mobius::identity()).abs())
            })
            .unwrap();
        assert_eq!(
            model.focused_card(cfg, Mobius::identity()),
            model.id_at(nearest),
            "identity-nav focus is the centroid-nearest card"
        );
        // Recenter the disk on card `c`'s pre-nav disk point: a swap-back must
        // then pick `c`.
        let c_idx = 2;
        let target = disk_point(model.positions()[c_idx], focus, scale, cfg, Mobius::identity());
        let nav = Mobius::from_point_pair(target, Complex::ORIGIN);
        assert_eq!(
            model.focused_card(cfg, nav),
            Some("c"),
            "navigating to c's disk point makes c the focus"
        );
    }
}
