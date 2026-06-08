//! Vault-wide note-link graph panel. Directed graph (one node per .md,
//! one edge per resolved `[[wikilink]]`). The pan/zoom, layout, view-options
//! menu, and the node/edge/label/hover/preview rendering all live in the
//! shared `hiker_graph_view` engine; this panel is the vault-specific
//! [`graph_view::Source`] adapter plus the vault walk that builds the graph
//! and the tab-linking (FOLLOW / DRIVE) wiring.
//!
//! Tree layouts need a tree; the vault graph is not one, so we BFS a
//! spanning tree rooted on the active note (when it's in the graph) or the
//! highest-degree node otherwise. Non-tree edges are still drawn — the
//! tree only shapes positions.

use std::collections::HashMap;
use std::time::Instant;

use eframe::egui;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::editor_pane;
use crate::state::AppState;
use hiker_graph_view::graph_view::{
    self, LayoutConfig, NodeDescriptor, NodeShape, Palette, Source, Style,
};
use hiker_core::vault::Vault;
use hiker_graph::{bfs_tree, dfs_tree, LayoutKind, LayoutTree};
use hiker_theme as theme;

/// Re-scan the vault for new/removed files no more often than this. Layout
/// runs in the background, but rebuilds still trigger file I/O so keep this
/// generous; users can hit "Rebuild" for an explicit refresh.
const REBUILD_AFTER_SECS: u64 = 300;
const LAYOUT_BOX: f32 = 1000.0;
const VAULT_CFG: LayoutConfig = LayoutConfig {
    area: LAYOUT_BOX * LAYOUT_BOX,
    seed_box: LAYOUT_BOX,
};

/// The vault graph panel's persistent state: the built graph + the shared
/// render engine + the vault-specific "Orphans" toggle. Lives on
/// `AppState::panels.graph` (a persisted singleton).
pub struct VaultPanel {
    pub data: VaultData,
    pub engine: graph_view::State,
    pub show_orphans: bool,
}

/// The vault link graph itself, rebuilt from disk on demand. Held apart
/// from the engine state so a rebuild swaps the graph without resetting the
/// user's view options or layout.
pub struct VaultData {
    pub graph: DiGraph<NodeData, ()>,
    /// Cached undirected edge list (index pairs) for the layout worker and
    /// for edge drawing.
    pub edges: Vec<(u32, u32)>,
    pub built_at: Instant,
}

pub struct NodeData {
    pub path: String,
    pub degree: u32,
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: crate::tab::TabId) {
    let mut rebuild = false;
    let mut reset_view = false;
    let mut relayout = false;

    // This tab's cross-tab wiring (FOLLOW source / DRIVE target). status: tab-linking
    let link = app.tab_by_id(tab_id).map(|t| t.link).unwrap_or_default();
    let active_path = highlighted_note_path(app, link);

    ui.horizontal(|ui| {
        ui.heading("Graph");
        link_control(ui, app, tab_id);
        if ui.small_button("Rebuild").clicked() {
            rebuild = true;
        }
        if ui.small_button("Reset view").clicked() {
            reset_view = true;
        }
        if let Some(vg) = app.panels.graph.as_mut() {
            relayout = vg.engine.view_options_menu(
                ui,
                crate::icons::ICONS.image(crate::icons::Icon::Eye),
                &mut [("Orphans", &mut vg.show_orphans)],
            );
            let status = match vg.engine.worker.as_ref() {
                Some(w) if w.is_running() => format!("· layout {} iters", w.iters_done()),
                _ => String::new(),
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} · {} notes · {} links · zoom {:.2}x {}",
                    vg.engine.layout_kind.label(),
                    vg.data.graph.node_count(),
                    vg.data.graph.edge_count(),
                    vg.engine.view.zoom,
                    status,
                ))
                .color(theme::muted())
                .small(),
            );
        }
    });
    if reset_view
        && let Some(vg) = app.panels.graph.as_mut()
    {
        vg.engine.needs_fit = true;
    }

    let stale = app
        .panels
        .graph
        .as_ref()
        .map(|vg| vg.data.built_at.elapsed().as_secs() > REBUILD_AFTER_SECS)
        .unwrap_or(true);
    if rebuild || stale {
        let data = Builder { app }.build_data();
        install_and_layout(app, data, active_path.as_deref());
    } else if relayout {
        relayout_vault(app, active_path.as_deref());
    }

    if let Some(path) = render_canvas(ui, app, active_path.as_deref()) {
        // DRIVE: when this graph targets a linked group, open the clicked
        // note there; otherwise the historical self-open. status: tab-linking
        match editor_pane::drive_target_group(app, link.target) {
            Some(group) => editor_pane::open_file_in_group(app, &path, group, false),
            None => editor_pane::open_file(app, &path, false),
        }
    }
}

/// Install freshly built `data`, preserving the engine view options across
/// rebuilds (only the graph is replaced), then (re)run the layout.
fn install_and_layout(app: &mut AppState, data: VaultData, active_path: Option<&str>) {
    let (engine, show_orphans) = match app.panels.graph.take() {
        Some(vg) => (vg.engine, vg.show_orphans),
        None => (
            graph_view::State::new(Style::flat(), LayoutKind::ForceDirected),
            true,
        ),
    };
    let mut vg = VaultPanel {
        data,
        engine,
        show_orphans,
    };
    let vault = app.vault_session.vault.clone();
    {
        let VaultPanel {
            data,
            engine,
            show_orphans,
        } = &mut vg;
        let source = VaultSource::new(data, vault.as_ref(), active_path, *show_orphans);
        engine.recompute_layout(&source, VAULT_CFG);
    }
    app.panels.graph = Some(vg);
}

/// Recompute positions in place after a layout-kind change (graph unchanged).
fn relayout_vault(app: &mut AppState, active_path: Option<&str>) {
    let vault = app.vault_session.vault.clone();
    let Some(vg) = app.panels.graph.as_mut() else {
        return;
    };
    let VaultPanel {
        data,
        engine,
        show_orphans,
    } = vg;
    let source = VaultSource::new(data, vault.as_ref(), active_path, *show_orphans);
    engine.recompute_layout(&source, VAULT_CFG);
}

/// Drive the shared engine for one frame; returns its click/hover output.
fn render_canvas(
    ui: &mut egui::Ui,
    app: &mut AppState,
    active_path: Option<&str>,
) -> Option<String> {
    let vault = app.vault_session.vault.clone();
    let vg = app.panels.graph.as_mut()?;
    let VaultPanel {
        data,
        engine,
        show_orphans,
    } = vg;
    let source = VaultSource::new(data, vault.as_ref(), active_path, *show_orphans);
    engine.ui(ui, &source, |p: &egui::Painter, r: egui::Rect, t: &str, b: &str, a: egui::Pos2| {
        paint_preview_card(p, r, t, b, a);
    })
}

/// Vault adapter from `VaultData` to the shared graph engine.
struct VaultSource<'a> {
    graph: &'a DiGraph<NodeData, ()>,
    edges: &'a [(u32, u32)],
    vault: &'a Vault,
    active_path: Option<&'a str>,
    show_orphans: bool,
}

impl<'a> VaultSource<'a> {
    fn new(
        data: &'a VaultData,
        vault: &'a Vault,
        active_path: Option<&'a str>,
        show_orphans: bool,
    ) -> Self {
        Self {
            graph: &data.graph,
            edges: &data.edges,
            vault,
            active_path,
            show_orphans,
        }
    }

    /// Tree-layout root: the active note when it's in the graph, else the
    /// highest-degree node.
    fn pick_root(&self) -> usize {
        if let Some(p) = self.active_path {
            for idx in self.graph.node_indices() {
                if self.graph[idx].path == p {
                    return idx.index();
                }
            }
        }
        let mut best_i = 0usize;
        let mut best_d = 0u32;
        for idx in self.graph.node_indices() {
            let d = self.graph[idx].degree;
            if d > best_d {
                best_d = d;
                best_i = idx.index();
            }
        }
        best_i
    }
}

impl Source for VaultSource<'_> {
    fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let (node_color, active_color) = match style.palette {
            Palette::Flat { node, active } => (node, active),
            Palette::Policy { cluster, .. } => (cluster, cluster),
        };
        let mut out = Vec::new();
        for idx in self.graph.node_indices() {
            let i = idx.index();
            if i >= positions.len() {
                continue;
            }
            let n = &self.graph[idx];
            // Hide orphans (degree 0) when toggled off; keeps the canvas on
            // the linked subgraph.
            if !self.show_orphans && n.degree == 0 {
                continue;
            }
            let is_active = self.active_path == Some(n.path.as_str());
            out.push(NodeDescriptor {
                index: i,
                world_pos: positions[i],
                radius: node_radius(n.degree),
                shape: NodeShape::Circle,
                fill: if is_active { active_color } else { node_color },
                resting_stroke: egui::Stroke::new(0.5, theme::divider()),
                hover_stroke: egui::Stroke::new(2.0, active_color),
                label: Some(basename(&n.path)),
                label_min_zoom: 0.0,
                click_path: Some(n.path.clone()),
                tooltip: None,
            });
        }
        out
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.edges.to_vec()
    }

    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree {
        let root = self.pick_root();
        let n = self.graph.node_count();
        // Radial wants a shallow tree (one ring per depth → BFS); the
        // vertical/horizontal layouts want depth → DFS.
        match kind {
            LayoutKind::Radial => bfs_tree(n, self.edges, root),
            _ => dfs_tree(n, self.edges, root),
        }
    }

    fn node_key(&self, index: usize) -> Option<String> {
        // The note's rel-path is stable across vault rebuilds, so it carries
        // each node's layout position through a re-walk.
        self.graph.node_weight(NodeIndex::new(index)).map(|n| n.path.clone())
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.node_weight(NodeIndex::new(index))?;
        let title = basename(&n.path);
        let body = self
            .vault
            .read_file(&n.path)
            .ok()
            .map(|s| preview_snippet(skip_frontmatter(&s)))
            .unwrap_or_else(|| "(unable to read note)".to_string());
        Some((title, body))
    }
}

/// The note the graph highlights as "active": the note active in the linked
/// FOLLOW source group when set, else the global active buffer. status: tab-linking
fn highlighted_note_path(app: &AppState, link: crate::tab::TabLink) -> Option<String> {
    editor_pane::followed_note_path(app, link.source).or_else(|| {
        app.session
            .active_tab
            .and_then(|id| app.tab_by_id(id))
            .and_then(|t| t.buffer_path())
            .map(std::string::ToString::to_string)
    })
}

/// Small "Link" control: opens a popup to wire this graph tab to follow /
/// drive another editor group. status: tab-linking
fn link_control(ui: &mut egui::Ui, app: &mut AppState, tab_id: crate::tab::TabId) {
    let linked = app
        .tab_by_id(tab_id)
        .map(|t| t.link.source.is_some() || t.link.target.is_some())
        .unwrap_or(false);
    let label = if linked { "Link *" } else { "Link" };
    let resp = ui
        .small_button(label)
        .on_hover_text("Link this graph to another tab group");
    egui::Popup::menu(&resp).show(|ui| {
        editor_pane::link_menu_ui(ui, app, tab_id);
    });
}

fn node_radius(degree: u32) -> f32 {
    6.0 + ((degree as f32) + 1.0).ln() * 2.0
}

/// File basename without directory or `.md`. `pub(crate)` so the cluster
/// graph panel can reuse it.
pub(crate) fn basename(path: &str) -> String {
    let stem = path.rsplit('/').next().unwrap_or(path);
    stem.strip_suffix(".md").unwrap_or(stem).to_string()
}

/// Truncate a note body to a preview snippet (≤500 chars post-frontmatter).
/// `pub(crate)` — shared with the cluster graph preview.
pub(crate) fn preview_snippet(body: &str) -> String {
    const MAX: usize = 500;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let mut out: String = body.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// Skip a YAML frontmatter block (and trailing blank lines) at the start of
/// a markdown file so previews open on real content. Returns the input
/// unchanged when it doesn't open with `---`. `pub(crate)` so the cluster
/// graph panel can reuse it.
pub(crate) fn skip_frontmatter(source: &str) -> &str {
    let trimmed_left = source.trim_start_matches(['\u{feff}']);
    let Some(rest) = trimmed_left
        .strip_prefix("---\n")
        .or_else(|| trimmed_left.strip_prefix("---\r\n"))
    else {
        return source;
    };
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find("\n---") {
        let start = search_from + idx + 1; // line start of the closing fence
        let after_fence = start + 3; // past the three dashes
        let tail = &rest[after_fence..];
        if tail.starts_with('\n') || tail.starts_with("\r\n") || tail.is_empty() {
            let skip = if tail.starts_with("\r\n") { 2 } else { 1 };
            return rest[after_fence + skip..].trim_start_matches(['\n', '\r']);
        }
        search_from = after_fence;
    }
    source
}

/// Paint a small preview card anchored near `anchor`. Shared between the
/// vault graph, cluster graph, and wikilink-hover panels.
pub(crate) fn paint_preview_card(
    painter: &egui::Painter,
    canvas: egui::Rect,
    title: &str,
    body: &str,
    anchor: egui::Pos2,
) -> Option<egui::Rect> {
    paint_preview_card_with(painter, canvas, title, body, anchor, 0.0).map(|p| p.card_rect)
}

/// Returned geometry from [`paint_preview_card_with`]. Lets a caller
/// implement scrollable bodies and hit-test the pointer against `card_rect`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviewCardGeometry {
    pub card_rect: egui::Rect,
    pub max_scroll_y: f32,
}

/// Variant of [`paint_preview_card`] supporting a vertical scroll offset on
/// the body. Callers managing their own hover lifecycle feed in a clamped
/// `scroll_y` per frame.
pub(crate) fn paint_preview_card_with(
    painter: &egui::Painter,
    canvas: egui::Rect,
    title: &str,
    body: &str,
    anchor: egui::Pos2,
    scroll_y: f32,
) -> Option<PreviewCardGeometry> {
    let pad = 8.0;
    let max_size = egui::vec2(320.0, 180.0);
    let card_size = max_size.min(canvas.size() - egui::vec2(pad * 2.0, pad * 2.0));
    if card_size.x < 80.0 || card_size.y < 60.0 {
        return None;
    }
    // Try bottom-right of cursor first; flip quadrants to avoid clipping.
    let offset = egui::vec2(14.0, 14.0);
    let mut min = anchor + offset;
    if min.x + card_size.x > canvas.right() - pad {
        min.x = anchor.x - offset.x - card_size.x;
    }
    if min.y + card_size.y > canvas.bottom() - pad {
        min.y = anchor.y - offset.y - card_size.y;
    }
    min.x = min.x.clamp(canvas.left() + pad, canvas.right() - pad - card_size.x);
    min.y = min.y.clamp(canvas.top() + pad, canvas.bottom() - pad - card_size.y);
    let card_rect = egui::Rect::from_min_size(min, card_size);

    let bg = egui::Color32::from_rgb(0xfa, 0xfa, 0xfa);
    let border = egui::Color32::from_rgb(0xc8, 0xcd, 0xd4);
    let title_color = egui::Color32::from_rgb(0x1a, 0x1e, 0x24);
    let body_color = egui::Color32::from_rgb(0x4a, 0x52, 0x5c);

    painter.rect_filled(card_rect, 4.0, bg);
    painter.rect_stroke(
        card_rect,
        4.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let inner = card_rect.shrink(8.0);
    let title_galley = painter.layout(
        title.to_string(),
        egui::FontId::proportional(13.0),
        title_color,
        inner.width(),
    );
    let title_size = title_galley.size();
    painter.galley(inner.left_top(), title_galley, title_color);

    let body_top = inner.left_top() + egui::vec2(0.0, title_size.y + 6.0);
    if body_top.y >= inner.bottom() {
        return Some(PreviewCardGeometry {
            card_rect,
            max_scroll_y: 0.0,
        });
    }
    let body_rect = egui::Rect::from_min_max(body_top, inner.right_bottom());
    let body_galley = painter.layout(
        body.to_string(),
        egui::FontId::proportional(11.0),
        body_color,
        body_rect.width(),
    );
    let body_h = body_galley.size().y;
    let max_scroll_y = (body_h - body_rect.height()).max(0.0);
    let scroll_clamped = scroll_y.clamp(0.0, max_scroll_y);
    let clip_painter = painter.with_clip_rect(body_rect);
    clip_painter.galley(
        body_rect.left_top() - egui::vec2(0.0, scroll_clamped),
        body_galley,
        body_color,
    );
    Some(PreviewCardGeometry {
        card_rect,
        max_scroll_y,
    })
}

/// Vault-graph builder. Bundles `&AppState` so the multi-step build
/// (walk → parse wikilinks → resolve targets) is a set of inherent methods.
struct Builder<'a> {
    app: &'a AppState,
}

impl Builder<'_> {
    /// Walk the vault, collect nodes, parse wikilinks, and build the graph.
    fn build_data(&self) -> VaultData {
        let app = self.app;
        let paths: Vec<String> = app
            .vault_session
            .vault
            .walk_indexable_files("")
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.ends_with(".md"))
            .collect();

        let mut graph: DiGraph<NodeData, ()> =
            DiGraph::with_capacity(paths.len(), paths.len() * 2);
        let mut by_path: HashMap<String, NodeIndex> = HashMap::with_capacity(paths.len());
        // basename (lowercase, no extension) → rel path. Last writer wins.
        let mut by_basename: HashMap<String, String> = HashMap::with_capacity(paths.len());

        for p in &paths {
            let idx = graph.add_node(NodeData {
                path: p.clone(),
                degree: 0,
            });
            by_path.insert(p.clone(), idx);
            by_basename.insert(basename(p).to_lowercase(), p.clone());
        }

        for p in &paths {
            let body = match app.vault_session.vault.read_file(p) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let src = by_path[p];
            for target in self.scan_wikilinks(&body) {
                if let Some(rel) = self.resolve_target(&target, &by_path, &by_basename)
                    && let Some(&dst) = by_path.get(&rel)
                    && dst != src
                {
                    graph.add_edge(src, dst, ());
                    graph[src].degree += 1;
                    graph[dst].degree += 1;
                }
            }
        }

        let edges: Vec<(u32, u32)> = graph
            .edge_indices()
            .filter_map(|e| graph.edge_endpoints(e))
            .map(|(a, b)| (a.index() as u32, b.index() as u32))
            .collect();

        VaultData {
            graph,
            edges,
            built_at: Instant::now(),
        }
    }

    /// Scan `body` for `[[Target]]` / `[[Target|Alias]]`, returning targets.
    fn scan_wikilinks(&self, body: &str) -> Vec<String> {
        let bytes = body.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'[' && bytes[i + 1] == b'[' {
                let start = i + 2;
                let mut j = start;
                let mut found = None;
                while j + 1 < bytes.len() {
                    let c = bytes[j];
                    if c == b'\n' || c == b']' && bytes[j + 1] == b']' {
                        if c == b']' {
                            found = Some(j);
                        }
                        break;
                    }
                    j += 1;
                }
                if let Some(end) = found {
                    let span = &body[start..end];
                    let target = match span.find('|') {
                        Some(p) => &span[..p],
                        None => span,
                    };
                    let trimmed = target.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                    i = end + 2;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// Map a wikilink target to an existing vault rel-path.
    fn resolve_target(
        &self,
        target: &str,
        by_path: &HashMap<String, NodeIndex>,
        by_basename: &HashMap<String, String>,
    ) -> Option<String> {
        if by_path.contains_key(target) {
            return Some(target.to_string());
        }
        let with_md = format!("{target}.md");
        if by_path.contains_key(&with_md) {
            return Some(with_md);
        }
        let leaf = target.rsplit('/').next().unwrap_or(target);
        let key = leaf.strip_suffix(".md").unwrap_or(leaf).to_lowercase();
        by_basename.get(&key).cloned()
    }
}
