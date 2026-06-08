//! Code-graph view panel (`code-graph-view-source`). Renders a project note's **repo source** as
//! a precise entity graph through the shared `hiker_graph_view` engine — a third
//! `graph_view::Source` beside the vault link-graph and the cluster-tree graph.
//!
//! The note (`hiker.kind: project`) is parsed by `hiker-projects`, whose repo source binds the
//! SCIP adapter (`hiker-code`); the adapter's `code_graph()` is mapped to colored/sized nodes
//! (by entity kind) + typed edges (calls / implements), with edge-type toggles, a scoped
//! top-degree default for large repos, and a read-only click→detail (signature location).
//!
//! State lives on `AppState::panels.code_graph`, keyed by the project-note path, so flipping
//! tabs keeps each project's layout (and its non-Clone adapter + background worker) warm.

use eframe::egui;

use crate::state::AppState;
use crate::tab::{Tab, TabId, TabKind};
use hiker_code::{CodeGraph, ScipAdapter};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::{
    self, LayoutConfig, NodeDescriptor, NodeShape, Source, Style,
};
use hiker_projects::{Backend, Project};
use hiker_theme as theme;
use spec_engine::{DerivedNodeSource, EdgeKind, NodeHandle, SourceId};

const FR_BOX: f32 = 1200.0;
const CODE_CFG: LayoutConfig = LayoutConfig { area: FR_BOX * FR_BOX, seed_box: 80.0 };
/// Scoped default: a whole large repo is a hairball, so cap the rendered graph at the highest-degree
/// nodes (`code-graph-scoped-default`) and say so rather than silently dumping everything.
const MAX_NODES: usize = 400;

/// Per-project-note panel state: the shared render engine + the bound adapter + the (possibly
/// scoped) graph + view toggles + the click-selected node.
pub struct CodeGraphView {
    engine: graph_view::State,
    /// `None` only when `error` is set (the note failed to bind); the cached error stops a costly
    /// re-parse/re-bind every frame.
    adapter: Option<ScipAdapter>,
    src: SourceId,
    graph: CodeGraph,
    full_count: usize,
    show_calls: bool,
    show_impls: bool,
    applied: (bool, bool),
    selected: Option<usize>,
    error: Option<String>,
}

/// Find-or-focus a code-graph tab for `project_path`, opening one if none exists.
pub fn open(app: &mut AppState, project_path: &str) -> TabId {
    if let Some(existing) = app.session.tabs.iter().find(
        |t| matches!(&t.kind, TabKind::CodeGraph { project_path: p } if p == project_path),
    ) {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    let kind = TabKind::CodeGraph { project_path: project_path.to_string() };
    app.session.tabs.push(Tab::new(id, kind, true));
    app.session.active_tab = Some(id);
    id
}

/// True if the `.md` at `rel` is a project note (`hiker.kind: project`). Reads + parses the file —
/// called on click / menu open, never per-frame (mirrors `is_board_doc`).
pub fn is_project_doc(app: &AppState, rel: &str) -> bool {
    if !rel.ends_with(".md") {
        return false;
    }
    app.vault_session
        .vault
        .read_file(rel)
        .ok()
        .map(|src| Project::parse(&src, std::path::Path::new(rel)).is_ok())
        .unwrap_or(false)
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, _tab_id: TabId, project_path: &str) {
    let short = project_path.rsplit('/').next().unwrap_or(project_path);
    ui.heading(format!("Code graph · {short}"));

    if !app.panels.code_graph.contains_key(project_path) {
        let view = build_view(app, project_path);
        app.panels.code_graph.insert(project_path.to_string(), view);
    }

    // Surface a load error and stop.
    if let Some(view) = app.panels.code_graph.get(project_path) {
        if let Some(err) = &view.error {
            ui.colored_label(egui::Color32::RED, err);
            return;
        }
    }

    let relayout = toolbar(ui, app, project_path);
    detail_line(ui, app, project_path);
    if relayout {
        recompute(app, project_path);
    }

    let clicked = render_canvas(ui, app, project_path);
    if let Some(id) = clicked {
        if let Some(view) = app.panels.code_graph.get_mut(project_path) {
            view.selected = view.graph.nodes.iter().position(|n| n.id == id);
        }
    }
    summary(ui, app, project_path);
}

/// Build the view: parse the note → first repo source → bind SCIP → `code_graph()` → scope → seed
/// the engine layout. Errors are stored on the view (rendered by `show`) rather than panicking.
fn build_view(app: &mut AppState, project_path: &str) -> CodeGraphView {
    let mut view = CodeGraphView {
        engine: graph_view::State::new(Style::flat(), LayoutKind::ForceDirected),
        adapter: None,
        src: SourceId(String::new()),
        graph: CodeGraph { nodes: Vec::new(), edges: Vec::new() },
        full_count: 0,
        show_calls: true,
        show_impls: true,
        applied: (true, true),
        selected: None,
        error: None,
    };
    let built = (|| -> Result<(ScipAdapter, SourceId), String> {
        let text = app
            .vault_session
            .vault
            .read_file(project_path)
            .map_err(|e| format!("read note: {e}"))?;
        let project = Project::parse(&text, std::path::Path::new(project_path))
            .map_err(|e| format!("project note: {e}"))?;
        let repo = project
            .repo_sources()
            .next()
            .ok_or_else(|| "project note has no `kind: repo` source".to_string())?;
        // Bind the repo descriptor → SCIP adapter here (the consumer composes; hiker-projects is
        // decoupled from code intelligence). Only the SCIP backend is implemented today.
        if repo.backend != Backend::Scip {
            return Err("only the SCIP backend is supported (LSP is not implemented yet)".to_string());
        }
        let src = SourceId(repo.repo_id.clone());
        let adapter = ScipAdapter::load(&repo.index, &repo.root, src.clone())
            .map_err(|e| format!("load index: {e}"))?;
        Ok((adapter, src))
    })();
    match built {
        Ok((adapter, src)) => {
            let graph = adapter.code_graph();
            view.full_count = graph.nodes.len();
            view.graph = scope_top_degree(graph, MAX_NODES);
            view.adapter = Some(adapter);
            view.src = src;
            // Construct the source from the disjoint `graph` field so `engine` stays mutably borrowable.
            let source = CodeGraphSource {
                graph: &view.graph,
                show_calls: view.show_calls,
                show_impls: view.show_impls,
            };
            view.engine.recompute_layout(&source, CODE_CFG);
        }
        Err(e) => view.error = Some(e),
    }
    view
}

/// Toolbar: edge-type toggles (Calls / Implements) + reset-view. Returns whether a relayout is
/// needed (layout-kind change, or an edge-toggle that alters the force topology).
fn toolbar(ui: &mut egui::Ui, app: &mut AppState, key: &str) -> bool {
    let Some(view) = app.panels.code_graph.get_mut(key) else { return false };
    let mut relayout = false;
    ui.horizontal_wrapped(|ui| {
        let mut extra: Vec<(&str, &mut bool)> =
            vec![("Calls", &mut view.show_calls), ("Implements", &mut view.show_impls)];
        relayout = view.engine.view_options_menu(
            ui,
            crate::icons::ICONS.image(crate::icons::Icon::Eye),
            &mut extra,
        );
        if ui.small_button("Reset view").clicked() {
            view.engine.needs_fit = true;
        }
        ui.label(
            egui::RichText::new(format!("{} · zoom {:.2}x", view.engine.layout_kind.label(), view.engine.view.zoom))
                .color(theme::muted())
                .small(),
        );
    });
    // An edge-type toggle changed the topology → relayout so the force worker + drawn edges agree.
    if view.applied != (view.show_calls, view.show_impls) {
        view.applied = (view.show_calls, view.show_impls);
        relayout = true;
    }
    relayout
}

/// Read-only click→detail (`code-node-detail`): the selected node's kind + definition `file:line`,
/// resolved through the adapter (no new editable tab).
fn detail_line(ui: &mut egui::Ui, app: &AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get(key) else { return };
    let Some(i) = view.selected else { return };
    let Some(node) = view.graph.nodes.get(i) else { return };
    let Some(adapter) = &view.adapter else { return };
    let handle = NodeHandle { source: view.src.clone(), id: node.id.clone() };
    let loc = adapter
        .locate(&handle)
        .map(|l| format!("{}:{}", l.file, l.start_line + 1))
        .unwrap_or_else(|| node.file.clone());
    ui.label(
        egui::RichText::new(format!("▸ {}  ·  {}  @ {}", node.name, node.kind, loc))
            .color(theme::muted())
            .small(),
    );
}

/// Recompute positions after a layout-kind or edge-toggle change.
fn recompute(app: &mut AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get_mut(key) else { return };
    let source = CodeGraphSource {
        graph: &view.graph,
        show_calls: view.show_calls,
        show_impls: view.show_impls,
    };
    view.engine.recompute_layout(&source, CODE_CFG);
}

/// Drive the engine for one frame; returns the clicked node id (its SCIP moniker), if any.
fn render_canvas(ui: &mut egui::Ui, app: &mut AppState, key: &str) -> Option<String> {
    let view = app.panels.code_graph.get_mut(key)?;
    let CodeGraphView { engine, graph, show_calls, show_impls, .. } = view;
    let source = CodeGraphSource { graph: &*graph, show_calls: *show_calls, show_impls: *show_impls };
    let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(50.0));
    ui.allocate_ui(size, |ui| {
        engine.ui(ui, &source, |p, r, t, b, a| {
            crate::panels::graph::paint_preview_card(p, r, t, b, a);
        })
    })
    .inner
}

fn summary(ui: &mut egui::Ui, app: &AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get(key) else { return };
    let Some(adapter) = &view.adapter else { return };
    let scoped = if view.full_count > view.graph.nodes.len() {
        format!(" (top {} of {} by degree)", view.graph.nodes.len(), view.full_count)
    } else {
        String::new()
    };
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!(
            "[{}] {} entities{} · {} edges · impl-edges: {}",
            adapter.tool(),
            view.graph.nodes.len(),
            scoped,
            view.graph.edges.len(),
            adapter.impl_source(),
        ))
        .color(theme::muted())
        .small(),
    );
}

/// Keep the `max` highest-degree nodes (in+out) and induce their edges, remapping indices. A no-op
/// when the graph already fits.
fn scope_top_degree(graph: CodeGraph, max: usize) -> CodeGraph {
    if graph.nodes.len() <= max {
        return graph;
    }
    let mut degree = vec![0usize; graph.nodes.len()];
    for &(a, b, _) in &graph.edges {
        degree[a] += 1;
        degree[b] += 1;
    }
    let mut idx: Vec<usize> = (0..graph.nodes.len()).collect();
    idx.sort_by(|&a, &b| degree[b].cmp(&degree[a]).then(a.cmp(&b)));
    idx.truncate(max);
    let mut remap = vec![usize::MAX; graph.nodes.len()];
    for (local, &g) in idx.iter().enumerate() {
        remap[g] = local;
    }
    let nodes = idx.iter().map(|&g| graph.nodes[g].clone()).collect();
    let edges = graph
        .edges
        .iter()
        .filter(|&&(a, b, _)| remap[a] != usize::MAX && remap[b] != usize::MAX)
        .map(|&(a, b, k)| (remap[a], remap[b], k))
        .collect();
    CodeGraph { nodes, edges }
}

/// The code adapter from a [`CodeGraph`] to the shared graph engine. Maps entity kind → shape/color
/// and filters edges by the active toggles.
struct CodeGraphSource<'a> {
    graph: &'a CodeGraph,
    show_calls: bool,
    show_impls: bool,
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "code:type" => egui::Color32::from_rgb(0x4f, 0x83, 0xcc),
        "code:function" => egui::Color32::from_rgb(0x4c, 0xaf, 0x72),
        "code:method" => egui::Color32::from_rgb(0x3f, 0xb6, 0xa8),
        "code:module" => egui::Color32::from_rgb(0x95, 0x75, 0xcd),
        "code:macro" => egui::Color32::from_rgb(0xc9, 0x8b, 0x3a),
        "code:constant" => egui::Color32::from_rgb(0xc7, 0x5b, 0x6d),
        "code:field" => egui::Color32::from_rgb(0xb0, 0x89, 0x4a),
        _ => egui::Color32::from_rgb(0x9e, 0x9e, 0x9e),
    }
}

fn edge_kept(kind: EdgeKind, show_calls: bool, show_impls: bool) -> bool {
    match kind {
        EdgeKind::Implements => show_impls,
        _ => show_calls, // Calls / TypeRef / Imports ride the "Calls" toggle for v1
    }
}

impl Source for CodeGraphSource<'_> {
    fn node_count(&self) -> usize {
        self.graph.nodes.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        let mut degree = vec![0u32; self.graph.nodes.len()];
        for &(a, b, _) in &self.graph.edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let maxd = degree.iter().copied().max().unwrap_or(1).max(1) as f32;
        self.graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < positions.len())
            .map(|(index, n)| NodeDescriptor {
                index,
                world_pos: positions[index],
                radius: 4.0 + 7.0 * (degree[index] as f32 / maxd),
                shape: if n.kind == "code:type" { NodeShape::Square } else { NodeShape::Circle },
                fill: kind_color(&n.kind),
                resting_stroke: egui::Stroke::NONE,
                hover_stroke: egui::Stroke::new(1.5, egui::Color32::WHITE),
                label: Some(n.name.clone()),
                label_min_zoom: 0.45,
                click_path: Some(n.id.clone()),
                tooltip: Some(n.file.clone()),
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.graph
            .edges
            .iter()
            .filter(|&&(_, _, k)| edge_kept(k, self.show_calls, self.show_impls))
            .map(|&(a, b, _)| (a as u32, b as u32))
            .collect()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        LayoutTree::from_parents(&vec![None; self.graph.nodes.len()])
    }

    fn node_key(&self, index: usize) -> Option<String> {
        self.graph.nodes.get(index).map(|n| n.id.clone())
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.nodes.get(index)?;
        Some((n.name.clone(), format!("{} · {}", n.kind, n.file)))
    }
}
