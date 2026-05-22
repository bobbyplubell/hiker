//! Vault-wide note-link graph panel. Directed graph (one node per .md,
//! one edge per resolved `[[wikilink]]`) with pluggable layouts (radial,
//! vertical/horizontal tree, force-directed via background-thread
//! Barnes–Hut FR with freeze-on-converge — see `widgets::force_layout`).
//! Hand-rolled `Painter` rendering, drag-to-pan, scroll-to-zoom,
//! hover-highlight, click-to-open.
//!
//! Tree layouts need a tree; the vault graph is not one, so we BFS a
//! spanning tree rooted on the active note (when it's in the graph) or
//! the highest-degree node otherwise. Non-tree edges are still drawn —
//! the tree only shapes positions.

use std::collections::HashMap;
use std::time::Instant;

use eframe::egui;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::editor_pane;
use crate::icons;
use crate::state::AppState;
use crate::theme;
use crate::widgets::force_graph::{View, ZoomBounds};
use graph_widgets::force_layout::{LayoutParams, LayoutWorker};
use graph_widgets::graph_layouts::{
    LayoutKind, bfs_tree, dfs_tree, horizontal_tree_positions, radial_positions,
    vertical_tree_positions,
};

/// Cached graph + layout state. Lives on `AppState::graph_state`.
pub struct State {
    pub graph: DiGraph<NodeData, ()>,
    /// Per-node positions, indexed by `NodeIndex::index()`. Refreshed
    /// from `layout` every frame while the worker is running.
    pub positions: Vec<egui::Vec2>,
    /// Cached undirected edge list (u32 index pairs). Used by the
    /// layout worker and by BFS tree-building.
    pub edges: Vec<(u32, u32)>,
    pub layout_kind: LayoutKind,
    /// `Some` only when `layout_kind == ForceDirected`.
    pub layout: Option<LayoutWorker>,
    pub built_at: Instant,
    /// True after a layout rebuild — `show()` refits pan/zoom on the
    /// next paint so the user always sees the layout framed.
    pub needs_fit: bool,
    /// Pan/zoom + shared input handling. Extracted to
    /// `widgets::force_graph` so the cluster-graph panel can reuse it.
    pub view: View,
    /// View options surfaced in the panel header.
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_orphans: bool,
    /// Note-preview overlay: toggle, selected note path, cached body
    /// snippet. Refreshed only when the selection changes — we don't
    /// re-read the file every frame.
    pub show_preview: bool,
    pub selected_path: Option<String>,
    pub selected_preview: Option<String>,
}

pub struct NodeData {
    pub path: String,
    pub degree: u32,
}

/// Re-scan the vault for new/removed files no more often than this.
/// The previous value (30s) re-walked the whole vault while the panel
/// was open, which compounded with the on-thread layout cost. Layout
/// now runs in the background, but rebuilds still trigger file I/O so
/// keep this generous; users can hit "Rebuild" for an explicit refresh.
const REBUILD_AFTER_SECS: u64 = 300;
const LAYOUT_BOX: f32 = 1000.0;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    let mut rebuild = false;
    let mut reset_view = false;
    let mut relayout = false;

    let active_path = app
        .session.active_tab
        .and_then(|id| app.tab_by_id(id))
        .and_then(|t| t.buffer_path())
        .map(std::string::ToString::to_string);

    // Header row.
    ui.horizontal(|ui| {
        ui.heading("Graph");
        if ui.small_button("Rebuild").clicked() {
            rebuild = true;
        }
        if ui.small_button("Reset view").clicked() {
            reset_view = true;
        }
        if let Some(gs) = app.panels.graph.as_mut() {
            let prev_kind = gs.layout_kind;
            gs.view_options_menu(ui);
            if gs.layout_kind != prev_kind {
                relayout = true;
            }
            let status = match gs.layout.as_ref() {
                Some(w) if w.is_running() => format!("· layout {} iters", w.iters_done()),
                _ => String::new(),
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} · {} notes · {} links · zoom {:.2}x {}",
                    gs.layout_kind.label(),
                    gs.graph.node_count(),
                    gs.graph.edge_count(),
                    gs.view.zoom,
                    status,
                ))
                .color(theme::muted())
                .small(),
            );
        }
    });
    if reset_view
        && let Some(gs) = app.panels.graph.as_mut()
    {
        gs.needs_fit = true;
    }

    // Build / rebuild as needed.
    let stale = app
        .panels.graph
        .as_ref()
        .map(|gs| gs.built_at.elapsed().as_secs() > REBUILD_AFTER_SECS)
        .unwrap_or(true);
    if rebuild || stale {
        app.panels.graph = Some(Builder { app }.build(active_path.as_deref()));
    } else if relayout
        && let Some(gs) = app.panels.graph.as_mut()
    {
        recompute_layout(gs, active_path.as_deref());
    }

    let Some(gs) = app.panels.graph.as_mut() else {
        return;
    };

    // Pull latest positions from the worker (cheap RwLock read). Skip
    // once the worker is done — positions are already final.
    let layout_running = gs.layout.as_ref().is_some_and(graph_widgets::force_layout::LayoutWorker::is_running);
    if layout_running
        && let Some(w) = gs.layout.as_ref()
    {
        w.snapshot_into(&mut gs.positions);
        ui.ctx().request_repaint();
    }

    // Canvas allocation moved a few lines down; do the auto-fit after
    // we know `rect`.

    // Canvas.
    let available = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);

    // Auto-fit pan/zoom on rebuild / "Reset view". Refit while the
    // worker is still settling so framing tracks the changing scale.
    if gs.needs_fit && !gs.positions.is_empty() {
        gs.view.fit_to_positions(&gs.positions, rect, (0.005, 6.0));
        if !layout_running {
            gs.needs_fit = false;
        }
    }

    // Shared pan/zoom input.
    gs.view.handle_input(
        ui,
        &response,
        rect,
        ZoomBounds { min: 0.005, max: 6.0 },
    );
    let to_screen = gs.view.screen_mapper(rect);
    let zoom = gs.view.zoom;

    // Hit-test for hover/click.
    let hover_pos = response.hover_pos();
    let mut hovered: Option<NodeIndex> = None;
    if let Some(hp) = hover_pos {
        let mut best = f32::INFINITY;
        for idx in gs.graph.node_indices() {
            let i = idx.index();
            if i >= gs.positions.len() {
                continue;
            }
            let n = &gs.graph[idx];
            let p = to_screen(gs.positions[i]);
            let r = node_radius(n.degree) * zoom.max(0.4);
            let d2 = (p - hp).length_sq();
            if d2 <= (r + 2.0).powi(2) && d2 < best {
                best = d2;
                hovered = Some(idx);
            }
        }
    }

    // Edges first.
    if gs.show_edges {
        let edge_color = egui::Color32::from_rgba_premultiplied(0x90, 0x96, 0xa0, 0xa0);
        let edge_stroke = egui::Stroke::new(1.0, edge_color);
        for e in gs.graph.edge_indices() {
            let (a, b) = gs.graph.edge_endpoints(e).unwrap();
            let (ai, bi) = (a.index(), b.index());
            if ai >= gs.positions.len() || bi >= gs.positions.len() {
                continue;
            }
            let pa = to_screen(gs.positions[ai]);
            let pb = to_screen(gs.positions[bi]);
            painter.line_segment([pa, pb], edge_stroke);
        }
    }

    // Nodes + labels.
    let accent = theme::accent();
    let node_fill = egui::Color32::from_rgb(0x6b, 0x72, 0x80);
    let label_color = theme::muted();
    let label_font = egui::FontId::proportional(11.0);

    let mut clicked: Option<String> = None;
    for idx in gs.graph.node_indices() {
        let i = idx.index();
        if i >= gs.positions.len() {
            continue;
        }
        let n = &gs.graph[idx];
        // Skip orphans (degree == 0) when the user has hidden them. Keeps
        // the canvas focused on the linked subgraph.
        if !gs.show_orphans && n.degree == 0 {
            continue;
        }
        let p = to_screen(gs.positions[i]);
        let r = node_radius(n.degree) * zoom.max(0.4);
        let is_active = active_path.as_deref() == Some(n.path.as_str());
        let is_hover = hovered == Some(idx);
        let fill = if is_active { accent } else { node_fill };
        let stroke = if is_hover {
            egui::Stroke::new(2.0, accent)
        } else {
            egui::Stroke::new(0.5, theme::divider())
        };
        painter.circle(p, r, fill, stroke);

        if gs.show_labels {
            let name = basename(&n.path);
            painter.text(
                egui::pos2(p.x, p.y + r + 2.0),
                egui::Align2::CENTER_TOP,
                name,
                label_font.clone(),
                label_color,
            );
        }

        if is_hover && response.clicked() {
            clicked = Some(n.path.clone());
        }
    }

    if let Some(path) = clicked {
        editor_pane::open_file(app, &path, false);
        update_selection(app, path);
    }

    // Hover-driven preview: while `show_preview` is on, the card
    // tracks the hovered node and anchors near its screen position.
    let hovered_info: Option<(String, egui::Pos2)> = hovered.and_then(|idx| {
        app.panels.graph.as_ref().and_then(|gs| {
            let n = gs.graph.node_weight(idx)?;
            let i = idx.index();
            let pos = gs.positions.get(i).copied()?;
            Some((n.path.clone(), to_screen(pos)))
        })
    });
    let show_preview = app
        .panels
        .graph
        .as_ref()
        .map(|gs| gs.show_preview)
        .unwrap_or(false);
    if show_preview
        && let Some((path, _)) = hovered_info.as_ref()
    {
        update_selection(app, path.clone());
    }

    // Note-preview overlay (drawn last so it sits on top of nodes).
    // Only when we're actively hovering a node — otherwise the card
    // would linger over the last-clicked node, which is confusing.
    if show_preview
        && let Some((path, anchor)) = hovered_info
        && let Some(gs) = app.panels.graph.as_ref()
    {
        let title = basename(&path);
        let body = gs.selected_preview.as_deref().unwrap_or("(unable to read note)");
        paint_preview_card(&painter, rect, &title, body, anchor);
    }
}

/// Refresh `selected_path` + `selected_preview` when the user clicks a
/// new node. We read the file once here (rather than every frame) and
/// store a truncated snippet on the state.
fn update_selection(app: &mut AppState, path: String) {
    let needs_load = app
        .panels.graph
        .as_ref()
        .map(|gs| gs.selected_path.as_deref() != Some(path.as_str()))
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
            let body = skip_frontmatter(&s);
            if body.chars().count() <= MAX {
                body.to_string()
            } else {
                let mut out: String = body.chars().take(MAX).collect();
                out.push('…');
                out
            }
        });
    if let Some(gs) = app.panels.graph.as_mut() {
        gs.selected_path = Some(path);
        gs.selected_preview = preview;
    }
}

/// Skip a YAML frontmatter block at the start of a markdown file and
/// also any blank lines that immediately follow it. Without this, the
/// preview body opens on the YAML (`---\nid: …\n---`) instead of the
/// real note content. If the file doesn't open with `---`, returns the
/// input unchanged. `pub(crate)` so the cluster graph panel can reuse
/// it.
pub(crate) fn skip_frontmatter(source: &str) -> &str {
    let trimmed_left = source.trim_start_matches(['\u{feff}']);
    let Some(rest) = trimmed_left
        .strip_prefix("---\n")
        .or_else(|| trimmed_left.strip_prefix("---\r\n"))
    else {
        return source;
    };
    // Find the closing `---` on its own line. Bail back to the original
    // text if the block is unterminated (corrupt frontmatter shouldn't
    // hide the whole note).
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

/// Paint a small preview card anchored near `anchor` (typically the
/// hovered node's screen position). Placement nudges the card to keep
/// it inside `canvas`. Shared between the vault graph and cluster
/// graph panels.
pub(crate) fn paint_preview_card(
    painter: &egui::Painter,
    canvas: egui::Rect,
    title: &str,
    body: &str,
    anchor: egui::Pos2,
) {
    let pad = 8.0;
    let max_size = egui::vec2(320.0, 180.0);
    let card_size = max_size.min(canvas.size() - egui::vec2(pad * 2.0, pad * 2.0));
    if card_size.x < 80.0 || card_size.y < 60.0 {
        return;
    }
    // Try bottom-right of cursor first; flip to other quadrants if it
    // would clip the canvas. 14px offset clears the node circle.
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
    // Lay out the title with a max-width so long basenames (or cluster
    // names) wrap inside the card instead of running off the right
    // edge. Body then starts below however many lines the title used.
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
        return; // title alone filled the card
    }
    let body_rect = egui::Rect::from_min_max(body_top, inner.right_bottom());
    let body_galley = painter.layout(
        body.to_string(),
        egui::FontId::proportional(11.0),
        body_color,
        body_rect.width(),
    );
    let clip_painter = painter.with_clip_rect(body_rect);
    clip_painter.galley(body_rect.left_top(), body_galley, body_color);
}

impl State {
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
            ui.checkbox(&mut self.show_orphans, "Orphans");
            ui.checkbox(&mut self.show_preview, "Show note preview");
        });
    }

    fn pick_root(&self, active_path: Option<&str>) -> usize {
        if let Some(p) = active_path {
            for idx in self.graph.node_indices() {
                if self.graph[idx].path == p {
                    return idx.index();
                }
            }
        }
        // Highest-degree node as a fallback.
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

fn node_radius(degree: u32) -> f32 {
    6.0 + ((degree as f32) + 1.0).ln() * 2.0
}

fn basename(path: &str) -> String {
    let stem = path.rsplit('/').next().unwrap_or(path);
    stem.strip_suffix(".md").unwrap_or(stem).to_string()
}

/// Vault-graph builder. Bundles `&AppState` so the multi-step build
/// (walk → parse wikilinks → resolve targets) is a set of inherent
/// methods rather than single-use free functions.
struct Builder<'a> {
    app: &'a AppState,
}

impl Builder<'_> {
    /// Walk the vault, collect nodes, parse wikilinks, build petgraph,
    /// and run the initial layout.
    fn build(&self, active_path: Option<&str>) -> State {
    let app = self.app;
    let paths: Vec<String> = app
        .vault_session.vault
        .walk_indexable_files("")
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.ends_with(".md"))
        .collect();

    let mut graph: DiGraph<NodeData, ()> = DiGraph::with_capacity(paths.len(), paths.len() * 2);
    let mut by_path: HashMap<String, NodeIndex> = HashMap::with_capacity(paths.len());
    // basename (lowercase, no extension) → rel path. Last writer wins on
    // collisions; good enough for v0 and matches wikilink intuition.
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
            let resolved = self.resolve_target(&target, &by_path, &by_basename);
            if let Some(rel) = resolved {
                if let Some(&dst) = by_path.get(&rel) {
                    if dst != src {
                        graph.add_edge(src, dst, ());
                        graph[src].degree += 1;
                        graph[dst].degree += 1;
                    }
                }
            }
        }
    }

    let edges: Vec<(u32, u32)> = graph
        .edge_indices()
        .filter_map(|e| graph.edge_endpoints(e))
        .map(|(a, b)| (a.index() as u32, b.index() as u32))
        .collect();

    let mut state = State {
        graph,
        positions: vec![egui::Vec2::ZERO; paths.len()],
        edges,
        layout_kind: LayoutKind::ForceDirected,
        layout: None,
        built_at: Instant::now(),
        view: View::default(),
        show_labels: true,
        show_edges: true,
        show_orphans: true,
        show_preview: false,
        selected_path: None,
        selected_preview: None,
        needs_fit: true,
    };
    recompute_layout(&mut state, active_path);
    state
    }
}

/// Spawn the worker (force-directed) or compute pure positions
/// (radial / tree). Picks a root for tree layouts: prefers the active
/// note when it's in the graph, else the highest-degree node.
fn recompute_layout(state: &mut State, active_path: Option<&str>) {
    state.layout = None;
    state.needs_fit = true;
    let n = state.graph.node_count();
    if n == 0 {
        state.positions.clear();
        return;
    }
    let area = LAYOUT_BOX * LAYOUT_BOX;

    match state.layout_kind {
        LayoutKind::ForceDirected => {
            // Random scatter seed (force layout converges from any
            // start; cheap to regenerate).
            let mut seed = Vec::with_capacity(n);
            let mut rng_state: u64 = 0x9E3779B97F4A7C15;
            let mut rng = || {
                rng_state = rng_state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((rng_state >> 33) as u32) as f32 / (u32::MAX as f32)
            };
            for _ in 0..n {
                let x = (rng() - 0.5) * LAYOUT_BOX;
                let y = (rng() - 0.5) * LAYOUT_BOX;
                seed.push(egui::vec2(x, y));
            }
            state.positions = seed.clone();
            state.layout = Some(LayoutWorker::spawn(
                seed,
                state.edges.clone(),
                LayoutParams {
                    bound: 50_000.0,
                    ..LayoutParams::default()
                },
            ));
        }
        LayoutKind::Radial | LayoutKind::VerticalTree | LayoutKind::HorizontalTree => {
            let root = state.pick_root(active_path);
            // Radial wants a shallow tree (one ring per depth → BFS).
            // Vertical/horizontal want a deep tree so dense clusters
            // don't collapse into flat horizontal bands → DFS.
            let tree = match state.layout_kind {
                LayoutKind::Radial => bfs_tree(n, &state.edges, root),
                _ => dfs_tree(n, &state.edges, root),
            };
            state.positions = match state.layout_kind {
                LayoutKind::Radial => radial_positions(&tree, area),
                LayoutKind::VerticalTree => vertical_tree_positions(&tree, area),
                LayoutKind::HorizontalTree => horizontal_tree_positions(&tree, area),
                LayoutKind::ForceDirected => unreachable!(),
            };
        }
    }
}

impl Builder<'_> {
    /// Scan `body` for `[[Target]]` / `[[Target|Alias]]`, returning targets.
    /// `\n` terminates a search so we don't span paragraphs.
    fn scan_wikilinks(&self, body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find closing ]] on the same line.
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
                // Strip alias.
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
