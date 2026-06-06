//! Shared force/tree graph rendering engine for the vault link-graph and
//! the cluster-tree graph. Owns pan/zoom, layout (the background force
//! worker plus the inline tree-position math), the eye-icon view-options
//! menu, and the node/edge/label/hover/preview paint loop. Each caller
//! supplies a [`Source`] that turns its own data — a `petgraph` vault graph
//! or a slice of cluster `EditableNode`s — into per-frame [`NodeDescriptor`]s
//! plus the edge and layout-tree topology, so one code path renders both
//! views with different colors and options.

use eframe::egui;
use hiker_graph::{LayoutKind, LayoutParams, LayoutTree};
use hiker_theme as theme;

use crate::widgets::force_graph::{View, ZoomBounds};
use graph_widgets::force_layout::LayoutWorker;
use graph_widgets::{
    horizontal_tree_positions, radial_positions, vertical_tree_positions,
};

const ZOOM_MIN: f32 = 0.005;
const ZOOM_MAX: f32 = 6.0;

/// Persistent per-view engine state: pan/zoom, node positions, the active
/// layout + its background worker, the configurable [`Style`], the common
/// toggles, and the hover-preview cache. The graph's domain data lives on
/// the caller (via its [`Source`]), not here.
pub struct State {
    pub positions: Vec<egui::Vec2>,
    pub layout_kind: LayoutKind,
    /// `Some` only while `layout_kind == ForceDirected` and the worker is
    /// still iterating toward convergence.
    pub worker: Option<LayoutWorker>,
    /// True after a (re)build — `ui()` refits pan/zoom on the next paint so
    /// the user never opens to an off-screen layout.
    pub needs_fit: bool,
    pub view: View,
    pub style: Style,
    pub toggles: Toggles,
    pub preview: PreviewCache,
}

/// View toggles common to every graph. Caller-specific toggles (the vault
/// "Orphans", the cluster "Leaves") live on the caller and are surfaced
/// through the `extra_toggle` argument of [`State::view_options_menu`].
#[derive(Clone, Copy)]
pub struct Toggles {
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_preview: bool,
}

/// Hover-preview card text, refreshed only when the hovered node changes so
/// we don't re-read the note body every frame.
#[derive(Default)]
pub struct PreviewCache {
    hovered_index: Option<usize>,
    title: Option<String>,
    body: Option<String>,
}

/// Per-node draw + hit-test descriptor produced by a [`Source`] each frame.
/// The caller computes `fill`/`radius`/`shape` from its own data and the
/// active [`Style`]; the engine never hardcodes a coloring scheme.
pub struct NodeDescriptor {
    /// Index into `positions` — also the hover/preview identity.
    pub index: usize,
    pub world_pos: egui::Vec2,
    /// Base radius in world units, before `node_scale`/zoom.
    pub radius: f32,
    pub shape: NodeShape,
    pub fill: egui::Color32,
    pub resting_stroke: egui::Stroke,
    pub hover_stroke: egui::Stroke,
    pub label: Option<String>,
    /// Labels draw only at or above this zoom (0.0 = always).
    pub label_min_zoom: f32,
    /// `Some` makes the node clickable; the path is returned from [`State::ui`]
    /// for the caller to open.
    pub click_path: Option<String>,
    /// Hover tooltip text (the cluster graph shows node names; the vault
    /// graph passes `None`).
    pub tooltip: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeShape {
    Circle,
    Square,
}

/// World-space layout sizing. The vault graph and cluster graph settled on
/// different scales (1000² vs 800² boxes), so each caller passes its own.
#[derive(Clone, Copy)]
pub struct LayoutConfig {
    /// Area handed to the tree layouts.
    pub area: f32,
    /// Full width of the random scatter box for the force seed.
    pub seed_box: f32,
}

/// The caller-supplied bridge from domain data to the engine. Vault and
/// cluster panels each implement it over their own storage.
pub trait Source {
    /// Total node count (length of the `positions` vector). Includes nodes
    /// the caller hides in [`Source::nodes`] (orphans / leaves) so edge and
    /// layout indices stay stable.
    fn node_count(&self) -> usize;

    /// Build the visible node descriptors for this frame. `positions` is the
    /// engine's current layout; the caller reads `positions[i]` for each node
    /// it emits and skips its own hidden nodes.
    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor>;

    /// Edges as `positions`-index pairs. Used both for drawing and as the
    /// force-worker topology.
    fn edges(&self) -> Vec<(u32, u32)>;

    /// Spanning/parent tree for a tree layout. The vault graph BFS/DFS-es a
    /// spanning tree per kind; the cluster graph uses its parent tree for
    /// all. Only called for non-force kinds.
    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree;

    /// `(title, body)` for the hover-preview card of node `index`. Called
    /// once per hover change. Returns `None` to suppress the card.
    fn preview_for(&self, index: usize) -> Option<(String, String)>;
}

impl State {
    /// Fresh engine state with the given style + starting layout.
    pub fn new(style: Style, layout_kind: LayoutKind) -> Self {
        Self {
            positions: Vec::new(),
            layout_kind,
            worker: None,
            needs_fit: true,
            view: View::default(),
            style,
            toggles: Toggles {
                show_labels: true,
                show_edges: true,
                show_preview: false,
            },
            preview: PreviewCache::default(),
        }
    }

    /// (Re)compute positions for the current `layout_kind`. Force-directed
    /// spawns the background worker from a random scatter; the tree layouts
    /// run inline off `source.layout_tree`. Always flags `needs_fit` so
    /// `ui()` reframes on the next paint.
    pub fn recompute_layout(&mut self, source: &dyn Source, cfg: LayoutConfig) {
        self.worker = None;
        self.needs_fit = true;
        let n = source.node_count();
        if n == 0 {
            self.positions.clear();
            return;
        }
        match self.layout_kind {
            LayoutKind::ForceDirected => {
                let seed = scatter(n, cfg.seed_box);
                self.positions = seed.clone();
                // `bound` is only a runaway-force safety belt; keep it well
                // clear of any natural equilibrium for realistic graphs.
                self.worker = Some(LayoutWorker::spawn(
                    seed,
                    source.edges(),
                    LayoutParams {
                        bound: 50_000.0,
                        ..LayoutParams::default()
                    },
                ));
            }
            kind => {
                let tree = source.layout_tree(kind);
                self.positions = match kind {
                    LayoutKind::Radial => radial_positions(&tree, cfg.area),
                    LayoutKind::VerticalTree => vertical_tree_positions(&tree, cfg.area),
                    LayoutKind::HorizontalTree => horizontal_tree_positions(&tree, cfg.area),
                    LayoutKind::ForceDirected => unreachable!(),
                };
            }
        }
    }

    /// Allocate the canvas, run pan/zoom input, and draw the graph from
    /// `source`: background, edges, nodes + labels, hover ring, tooltip, and
    /// (when enabled) the hover-preview card. Returns the path of a clicked
    /// node for the caller to open, if any.
    pub fn ui(&mut self, ui: &mut egui::Ui, source: &dyn Source) -> Option<String> {
        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let bg = self.style.background.unwrap_or(ui.visuals().extreme_bg_color);
        painter.rect_filled(rect, 0.0, bg);

        // Pull fresh positions while the force worker is still settling.
        let worker_running = self.worker.as_ref().is_some_and(LayoutWorker::is_running);
        if worker_running
            && let Some(w) = self.worker.as_ref()
        {
            w.snapshot_into(&mut self.positions);
            ui.ctx().request_repaint();
        }

        // Auto-fit after a (re)build / Reset view; keep refitting while the
        // worker rescales so the framing tracks it.
        if self.needs_fit && !self.positions.is_empty() {
            self.view
                .fit_to_positions(&self.positions, rect, (ZOOM_MIN, ZOOM_MAX));
            if !worker_running {
                self.needs_fit = false;
            }
        }

        self.view.handle_input(
            ui,
            &response,
            rect,
            ZoomBounds {
                min: ZOOM_MIN,
                max: ZOOM_MAX,
            },
        );
        let to_screen = self.view.screen_mapper(rect);
        let zoom = self.view.zoom;
        let node_scale = self.style.node_scale;

        let nodes = source.nodes(&self.positions, &self.style);
        let hovered = response
            .hover_pos()
            .and_then(|hp| hit_test(&nodes, &to_screen, hp, node_scale, zoom));

        if self.toggles.show_edges {
            let stroke = egui::Stroke::new(self.style.edge_width, self.style.edge_color);
            for (a, b) in source.edges() {
                let (a, b) = (a as usize, b as usize);
                if a < self.positions.len() && b < self.positions.len() {
                    let seg = [to_screen(self.positions[a]), to_screen(self.positions[b])];
                    painter.line_segment(seg, stroke);
                }
            }
        }

        let draw = self.draw_nodes(&painter, &nodes, &to_screen, hovered, response.clicked());

        if let Some((pos, text)) = draw.tooltip {
            draw_tooltip(&painter, pos, text);
        }

        if self.toggles.show_preview
            && let Some(idx) = hovered
        {
            self.refresh_preview(source, idx);
            if let (Some(anchor), Some(title)) = (draw.hover_anchor, self.preview.title.as_deref())
            {
                let body = self.preview.body.as_deref().unwrap_or("(unable to read note)");
                crate::panels::graph::paint_preview_card(&painter, rect, title, body, anchor);
            }
        } else {
            self.preview.hovered_index = None;
        }

        draw.clicked
    }

    /// Paint every node + label, returning the click target, the hovered
    /// node's screen anchor (for the preview card), and any tooltip.
    fn draw_nodes(
        &self,
        painter: &egui::Painter,
        nodes: &[NodeDescriptor],
        to_screen: &impl Fn(egui::Vec2) -> egui::Pos2,
        hovered: Option<usize>,
        response_clicked: bool,
    ) -> NodeDraw {
        let zoom = self.view.zoom;
        let node_scale = self.style.node_scale;
        let label_font = egui::FontId::proportional(self.style.label_size);
        let mut draw = NodeDraw::default();
        for d in nodes {
            let p = to_screen(d.world_pos);
            let r = d.radius * node_scale * zoom.max(0.4);
            let is_hover = hovered == Some(d.index);
            match d.shape {
                NodeShape::Circle => {
                    let stroke = if is_hover { d.hover_stroke } else { d.resting_stroke };
                    painter.circle(p, r, d.fill, stroke);
                }
                NodeShape::Square => {
                    let rect = egui::Rect::from_center_size(p, egui::Vec2::splat(r * 2.0));
                    painter.rect_filled(rect, 1.0, d.fill);
                    if is_hover {
                        painter.rect_stroke(
                            rect.expand(2.0),
                            1.0,
                            d.hover_stroke,
                            egui::StrokeKind::Outside,
                        );
                    }
                }
            }
            if self.toggles.show_labels
                && zoom >= d.label_min_zoom
                && let Some(label) = &d.label
            {
                painter.text(
                    egui::pos2(p.x, p.y + r + 2.0),
                    egui::Align2::CENTER_TOP,
                    label,
                    label_font.clone(),
                    self.style.label_color,
                );
            }
            if is_hover {
                draw.hover_anchor = Some(p);
                if let Some(t) = &d.tooltip {
                    draw.tooltip = Some((p + egui::vec2(10.0, -10.0), t.clone()));
                }
                if response_clicked
                    && let Some(path) = &d.click_path
                {
                    draw.clicked = Some(path.clone());
                }
            }
        }
        draw
    }

    /// Re-resolve the preview text when the hovered node changes.
    fn refresh_preview(&mut self, source: &dyn Source, idx: usize) {
        if self.preview.hovered_index == Some(idx) {
            return;
        }
        let resolved = source.preview_for(idx);
        self.preview.hovered_index = Some(idx);
        self.preview.title = resolved.as_ref().map(|(t, _)| t.clone());
        self.preview.body = resolved.map(|(_, b)| b);
    }

    /// Eye-icon view-options popup: layout selector, common toggles plus an
    /// optional caller toggle, palette-specific + common color pickers, and
    /// the size sliders. Returns `true` when the layout kind changed so the
    /// caller can trigger a relayout.
    pub fn view_options_menu(
        &mut self,
        ui: &mut egui::Ui,
        extra_toggle: Option<(&str, &mut bool)>,
    ) -> bool {
        let resp = ui.add(egui::Button::image(eye_icon())).on_hover_text("View options");
        let prev_kind = self.layout_kind;
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Layout").small().color(theme::muted()));
            for kind in LayoutKind::all() {
                let mut selected = self.layout_kind == kind;
                if ui.checkbox(&mut selected, kind.label()).clicked() && selected {
                    self.layout_kind = kind;
                }
            }
            ui.separator();
            ui.checkbox(&mut self.toggles.show_labels, "Labels");
            ui.checkbox(&mut self.toggles.show_edges, "Edges");
            if let Some((label, flag)) = extra_toggle {
                ui.checkbox(flag, label);
            }
            ui.checkbox(&mut self.toggles.show_preview, "Show note preview");

            ui.separator();
            ui.label(egui::RichText::new("Colors").small().color(theme::muted()));
            palette_rows(ui, &mut self.style.palette);
            color_row(ui, "Edges", &mut self.style.edge_color);
            color_row(ui, "Labels", &mut self.style.label_color);
            let theme_bg = ui.visuals().extreme_bg_color;
            let mut bg = self.style.background.unwrap_or(theme_bg);
            ui.horizontal(|ui| {
                if ui.color_edit_button_srgba(&mut bg).changed() {
                    self.style.background = Some(bg);
                }
                ui.label("Background");
            });

            ui.separator();
            ui.label(egui::RichText::new("Size").small().color(theme::muted()));
            ui.add(egui::Slider::new(&mut self.style.node_scale, 0.3..=3.0).text("Nodes"));
            ui.add(egui::Slider::new(&mut self.style.edge_width, 0.25..=4.0).text("Edges"));
            ui.add(egui::Slider::new(&mut self.style.label_size, 7.0..=20.0).text("Labels"));

            ui.separator();
            if ui.button("Reset style").clicked() {
                self.style = match self.style.palette {
                    Palette::Flat { .. } => Style::flat(),
                    Palette::Policy { .. } => Style::policy(),
                };
            }
        });
        self.layout_kind != prev_kind
    }
}

/// Configurable colors + sizes for a graph view. The [`Palette`] varies the
/// per-node coloring controls (flat vault fill + active accent vs. the
/// cluster color-by-policy set); every other control is common to both.
#[derive(Clone, Copy)]
pub struct Style {
    pub edge_color: egui::Color32,
    pub label_color: egui::Color32,
    /// `None` follows the theme's `extreme_bg_color`.
    pub background: Option<egui::Color32>,
    /// Multiplier on each node's base radius.
    pub node_scale: f32,
    pub edge_width: f32,
    pub label_size: f32,
    pub palette: Palette,
}

/// The per-node color scheme, which differs between the two graphs.
#[derive(Clone, Copy)]
pub enum Palette {
    /// Vault graph: one flat fill + an accent for the active note.
    Flat {
        node: egui::Color32,
        active: egui::Color32,
    },
    /// Cluster graph: color by node kind / policy, blended toward `stale` by
    /// summary churn.
    Policy {
        cluster: egui::Color32,
        move_policy: egui::Color32,
        tag_policy: egui::Color32,
        leaf: egui::Color32,
        stale: egui::Color32,
    },
}

impl Style {
    /// Vault-graph defaults: flat `#6b7280` nodes, active note in accent,
    /// translucent grey edges. Defaults mirror the historical hard-coded
    /// render values so an untouched graph looks unchanged.
    pub const fn flat() -> Self {
        Self {
            edge_color: egui::Color32::from_rgba_premultiplied(0x90, 0x96, 0xa0, 0xa0),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Flat {
                node: egui::Color32::from_rgb(0x6b, 0x72, 0x80),
                active: theme::accent(),
            },
        }
    }

    /// Cluster-graph defaults: color-by-policy with the spec's four encoding
    /// colors plus a staleness grey, divider-colored edges.
    pub const fn policy() -> Self {
        Self {
            edge_color: theme::divider(),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Policy {
                cluster: theme::accent(),
                move_policy: egui::Color32::from_rgb(0x2f, 0x6f, 0xb9),
                tag_policy: egui::Color32::from_rgb(0xa8, 0x4a, 0xc4),
                leaf: egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                stale: egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
            },
        }
    }
}

/// Policy-color legend row (cluster graph only). No-op for a flat palette.
/// Reads the configured colors so the legend tracks any user edits.
pub fn policy_legend(ui: &mut egui::Ui, palette: &Palette) {
    let Palette::Policy {
        cluster,
        move_policy,
        tag_policy,
        leaf,
        ..
    } = palette
    else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Encoding:").color(theme::muted()).small());
        legend_swatch(ui, *cluster, "cluster");
        legend_swatch(ui, *move_policy, "move policy");
        legend_swatch(ui, *tag_policy, "tag policy");
        legend_swatch(ui, *leaf, "leaf");
    });
}

/// Scratch results from one node-paint pass.
#[derive(Default)]
struct NodeDraw {
    clicked: Option<String>,
    hover_anchor: Option<egui::Pos2>,
    tooltip: Option<(egui::Pos2, String)>,
}

/// Nearest node within its (scaled) radius of the cursor, if any.
fn hit_test(
    nodes: &[NodeDescriptor],
    to_screen: &impl Fn(egui::Vec2) -> egui::Pos2,
    hover: egui::Pos2,
    node_scale: f32,
    zoom: f32,
) -> Option<usize> {
    let mut best = f32::INFINITY;
    let mut hit = None;
    for d in nodes {
        let p = to_screen(d.world_pos);
        let r = d.radius * node_scale * zoom.max(0.4);
        let d2 = (p - hover).length_sq();
        if d2 <= (r + 4.0).powi(2) && d2 < best {
            best = d2;
            hit = Some(d.index);
        }
    }
    hit
}

/// White-background name tooltip (cluster graph). Mirrors the box the
/// cluster panel drew inline before the engine extraction.
fn draw_tooltip(painter: &egui::Painter, pos: egui::Pos2, text: String) {
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), egui::Color32::BLACK);
    let bg = egui::Rect::from_min_size(pos, galley.size()).expand(4.0);
    painter.rect_filled(bg, 2.0, egui::Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 230));
    painter.galley(pos, galley, egui::Color32::BLACK);
}

/// The palette-specific color rows — flat node/active, or the five policy
/// colors.
fn palette_rows(ui: &mut egui::Ui, palette: &mut Palette) {
    match palette {
        Palette::Flat { node, active } => {
            color_row(ui, "Nodes", node);
            color_row(ui, "Active note", active);
        }
        Palette::Policy {
            cluster,
            move_policy,
            tag_policy,
            leaf,
            stale,
        } => {
            color_row(ui, "Cluster", cluster);
            color_row(ui, "Move policy", move_policy);
            color_row(ui, "Tag policy", tag_policy);
            color_row(ui, "Leaf", leaf);
            color_row(ui, "Stale", stale);
        }
    }
}

/// One labeled color swatch row.
fn color_row(ui: &mut egui::Ui, label: &str, color: &mut egui::Color32) {
    ui.horizontal(|ui| {
        ui.color_edit_button_srgba(color);
        ui.label(label);
    });
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(egui::RichText::new(label).small().color(theme::muted()));
}

fn eye_icon() -> egui::Image<'static> {
    crate::icons::ICONS.image(crate::icons::Icon::Eye)
}

/// Random scatter seed of `n` points in a `box_size`-wide box centered on
/// the origin. Deterministic LCG — the force layout converges from any
/// start, so a fixed seed keeps frames reproducible.
fn scatter(n: usize, box_size: f32) -> Vec<egui::Vec2> {
    let mut rng_state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut rng = || {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((rng_state >> 33) as u32) as f32 / (u32::MAX as f32)
    };
    (0..n)
        .map(|_| egui::vec2((rng() - 0.5) * box_size, (rng() - 0.5) * box_size))
        .collect()
}
