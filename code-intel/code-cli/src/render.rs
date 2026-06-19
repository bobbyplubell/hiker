//! Self-contained graph rendering for the `graph` command — the standalone de-risk from
//! `hiker-integration-plan.md`: prove the node/edge → visual path with **zero hiker dependency**.
//!
//! `dot`/graphviz is *not* assumed present, so we ship our own deterministic force layout
//! (Fruchterman-Reingold) and emit a finished **SVG** directly (colored by entity kind, edged by
//! Calls/Implements/…). A `--dot` mode is also provided for when graphviz *is* available. This is
//! the same node/edge data a hiker `graph_view::Source` will consume, so the eventual UI swap is
//! mechanical (same data, different paint target), and it surfaces the legibility issues
//! (whole-repo hairball) that motivate the scoped/neighborhood default.

use std::collections::VecDeque;

use hiker_code::{CodeGraph, GraphNode};
use spec_engine::EdgeKind;

/// A view over a subset of a [`CodeGraph`] (for scoped/neighborhood rendering — item F). `subset`
/// holds node indices into the parent graph; `local_edges` are remapped to 0..subset.len().
pub struct SubGraph<'a> {
    pub parent: &'a CodeGraph,
    pub subset: Vec<usize>,
    pub local_edges: Vec<(usize, usize, EdgeKind)>,
}

impl<'a> SubGraph<'a> {
    /// The whole graph as a subgraph (identity mapping).
    pub fn full(graph: &'a CodeGraph) -> SubGraph<'a> {
        let subset: Vec<usize> = (0..graph.nodes.len()).collect();
        let local_edges = graph.edges.clone();
        SubGraph { parent: graph, subset, local_edges }
    }

    /// The depth-bounded neighborhood of `focus` (undirected BFS), capped at `max` nodes by
    /// breadth order. This is the scoped default that keeps large graphs legible
    /// (`code-graph-scoped-default`): never dump the whole repo.
    pub fn neighborhood(graph: &'a CodeGraph, focus: usize, depth: usize, max: usize) -> SubGraph<'a> {
        // Adjacency (undirected) for traversal.
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
        for &(a, b, _) in &graph.edges {
            adj[a].push(b);
            adj[b].push(a);
        }
        let mut keep: Vec<usize> = Vec::new();
        let mut seen = vec![false; graph.nodes.len()];
        let mut q: VecDeque<(usize, usize)> = VecDeque::new();
        seen[focus] = true;
        q.push_back((focus, 0));
        while let Some((n, d)) = q.pop_front() {
            keep.push(n);
            if keep.len() >= max {
                break;
            }
            if d == depth {
                continue;
            }
            for &m in &adj[n] {
                if !seen[m] {
                    seen[m] = true;
                    q.push_back((m, d + 1));
                }
            }
        }
        Self::from_subset(graph, keep)
    }

    /// Build a subgraph from an explicit node-index set, inducing the edges between them.
    pub fn from_subset(graph: &'a CodeGraph, subset: Vec<usize>) -> SubGraph<'a> {
        let mut remap = vec![usize::MAX; graph.nodes.len()];
        for (local, &g) in subset.iter().enumerate() {
            remap[g] = local;
        }
        let local_edges = graph
            .edges
            .iter()
            .filter(|&&(a, b, _)| remap[a] != usize::MAX && remap[b] != usize::MAX)
            .map(|&(a, b, k)| (remap[a], remap[b], k))
            .collect();
        SubGraph { parent: graph, subset, local_edges }
    }

    pub fn len(&self) -> usize {
        self.subset.len()
    }
    #[allow(dead_code)] // paired with len() for completeness; CLI uses len()/empty-check on parent
    pub fn is_empty(&self) -> bool {
        self.subset.is_empty()
    }
    fn node(&self, local: usize) -> &GraphNode {
        &self.parent.nodes[self.subset[local]]
    }
}

// --- visual mapping (shared by SVG + DOT) -----------------------------------------------------

/// (svg-shape, fill, dot-shape) for an entity kind. Mirrors the item-C intent:
/// type=square/blue, function/method=circle/green(teal), module=diamond/purple, constant/field=small.
fn kind_style(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "code:type" => ("square", "#4f83cc", "box"),
        "code:function" => ("circle", "#4caf72", "ellipse"),
        "code:method" => ("circle", "#3fb6a8", "ellipse"),
        "code:module" => ("diamond", "#9575cd", "diamond"),
        "code:macro" => ("circle", "#c98b3a", "ellipse"),
        "code:constant" => ("circle", "#c75b6d", "ellipse"),
        "code:field" => ("circle", "#b0894a", "ellipse"),
        _ => ("circle", "#9e9e9e", "ellipse"),
    }
}

fn edge_color(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Calls => "#9aa0a6",
        EdgeKind::Implements => "#e0922f",
        EdgeKind::TypeRef => "#5b8def",
        EdgeKind::Imports => "#5aa469",
        EdgeKind::Link => "#9e9e9e",
    }
}

fn edge_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Calls => "calls",
        EdgeKind::Implements => "implements",
        EdgeKind::TypeRef => "type",
        EdgeKind::Imports => "imports",
        EdgeKind::Link => "link",
    }
}

// --- force-directed layout (Fruchterman-Reingold) ---------------------------------------------

const CANVAS: f32 = 1400.0;

/// Deterministic FR layout over the subgraph's local edges. Seeds positions on a golden-angle
/// spiral (reproducible — no RNG) and relaxes. O(n²) per iteration, fine for scoped subgraphs.
fn layout(sub: &SubGraph) -> Vec<(f32, f32)> {
    let n = sub.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![(CANVAS / 2.0, CANVAS / 2.0)];
    }
    let area = CANVAS * CANVAS;
    let k = (area / n as f32).sqrt(); // ideal edge length
    let golden = std::f32::consts::PI * (3.0 - 5.0_f32.sqrt());

    // Spiral seed centered on the canvas.
    let mut pos: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let r = CANVAS * 0.45 * ((i as f32 + 0.5) / n as f32).sqrt();
            let a = i as f32 * golden;
            (CANVAS / 2.0 + r * a.cos(), CANVAS / 2.0 + r * a.sin())
        })
        .collect();

    let iterations = if n > 500 { 120 } else { 300 };
    let mut temp = CANVAS * 0.12;
    let cool = temp / (iterations as f32 + 1.0);
    let center = CANVAS / 2.0;
    // Central spring: a weak pull toward the centroid so disconnected nodes (e.g. methods with no
    // in-scope edges) drift back rather than fly off. Combined with the per-iteration clamp below
    // and the percentile-based `fit`, this keeps multi-component graphs legible.
    let gravity = 0.04;

    for _ in 0..iterations {
        let mut disp = vec![(0.0f32, 0.0f32); n];
        // Repulsion between every pair.
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                let force = k * k / dist;
                let (ux, uy) = (dx / dist, dy / dist);
                disp[i].0 += ux * force;
                disp[i].1 += uy * force;
                disp[j].0 -= ux * force;
                disp[j].1 -= uy * force;
            }
        }
        // Attraction along edges.
        for &(a, b, _) in &sub.local_edges {
            let dx = pos[a].0 - pos[b].0;
            let dy = pos[a].1 - pos[b].1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = dist * dist / k;
            let (ux, uy) = (dx / dist, dy / dist);
            disp[a].0 -= ux * force;
            disp[a].1 -= uy * force;
            disp[b].0 += ux * force;
            disp[b].1 += uy * force;
        }
        // Central gravity + apply (capped by temperature) + clamp inside the canvas so stragglers
        // pin to the frame edge instead of escaping to infinity.
        for i in 0..n {
            disp[i].0 += (center - pos[i].0) * gravity;
            disp[i].1 += (center - pos[i].1) * gravity;
            let len = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1).sqrt().max(0.01);
            pos[i].0 = (pos[i].0 + disp[i].0 / len * len.min(temp)).clamp(0.0, CANVAS);
            pos[i].1 = (pos[i].1 + disp[i].1 / len * len.min(temp)).clamp(0.0, CANVAS);
        }
        temp -= cool;
    }
    pos
}

/// Rescale positions to fit a `[margin, size-margin]` box. Bounds are taken from the 3rd/97th
/// **percentile** of coordinates (not min/max) so a few disconnected outliers don't dominate the
/// scale and squash the connected core; outliers then clamp onto the frame.
fn fit(pos: &[(f32, f32)], size: f32, margin: f32) -> Vec<(f32, f32)> {
    let pct = |vals: &mut Vec<f32>, p: f32| -> f32 {
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((vals.len().saturating_sub(1)) as f32 * p).round() as usize;
        vals[idx.min(vals.len() - 1)]
    };
    let mut xs: Vec<f32> = pos.iter().map(|p| p.0).collect();
    let mut ys: Vec<f32> = pos.iter().map(|p| p.1).collect();
    let (minx, maxx) = (pct(&mut xs, 0.03), pct(&mut xs, 0.97));
    let (miny, maxy) = (pct(&mut ys, 0.03), pct(&mut ys, 0.97));
    let span = (maxx - minx).max(maxy - miny).max(1.0);
    let scale = (size - 2.0 * margin) / span;
    let lo = margin;
    let hi = size - margin;
    pos.iter()
        .map(|&(x, y)| {
            (
                (margin + (x - minx) * scale).clamp(lo, hi),
                (margin + (y - miny) * scale).clamp(lo, hi),
            )
        })
        .collect()
}

// --- SVG -------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Render the subgraph to a finished, self-contained SVG document. `title` is shown in the corner.
pub fn to_svg(sub: &SubGraph, title: &str) -> String {
    let size = CANVAS;
    let margin = 60.0;
    let raw = layout(sub);
    let pos = fit(&raw, size, margin);
    let n = sub.len();

    // Degree → node radius + which labels to draw (label everything when small, else hubs only).
    let mut degree = vec![0usize; n];
    for &(a, b, _) in &sub.local_edges {
        degree[a] += 1;
        degree[b] += 1;
    }
    let label_all = n <= 140;
    let max_deg = degree.iter().copied().max().unwrap_or(0).max(1);

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\" \
         viewBox=\"0 0 {size} {size}\" font-family=\"-apple-system,Segoe UI,sans-serif\">\n"
    ));
    s.push_str(&format!("<rect width=\"{size}\" height=\"{size}\" fill=\"#fbfbfd\"/>\n"));

    // Edges first (under nodes).
    s.push_str("<g stroke-width=\"1\" stroke-opacity=\"0.55\" fill=\"none\">\n");
    for &(a, b, kind) in &sub.local_edges {
        let (x1, y1) = pos[a];
        let (x2, y2) = pos[b];
        s.push_str(&format!(
            "<line x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\" stroke=\"{}\"/>\n",
            edge_color(kind)
        ));
    }
    s.push_str("</g>\n");

    // Nodes + labels.
    for i in 0..n {
        let node = sub.node(i);
        let (x, y) = pos[i];
        let (shape, fill, _) = kind_style(&node.kind);
        let r = 4.0 + 8.0 * (degree[i] as f32 / max_deg as f32);
        match shape {
            "square" => s.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"{w:.1}\" rx=\"1.5\" \
                 fill=\"{fill}\" stroke=\"#33333322\"/>\n",
                x - r,
                y - r,
                w = r * 2.0
            )),
            "diamond" => {
                let p = format!(
                    "{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}",
                    x, y - r, x + r, y, x, y + r, x - r, y
                );
                s.push_str(&format!("<polygon points=\"{p}\" fill=\"{fill}\" stroke=\"#33333322\"/>\n"));
            }
            _ => s.push_str(&format!(
                "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"{r:.1}\" fill=\"{fill}\" stroke=\"#33333322\"/>\n"
            )),
        }
        if label_all || degree[i] * 4 >= max_deg {
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"9\" fill=\"#222\">{}</text>\n",
                x + r + 2.0,
                y + 3.0,
                esc(&node.name)
            ));
        }
    }

    // Title + legend.
    s.push_str(&format!(
        "<text x=\"16\" y=\"26\" font-size=\"15\" font-weight=\"600\" fill=\"#111\">{}</text>\n",
        esc(title)
    ));
    s.push_str(&format!(
        "<text x=\"16\" y=\"44\" font-size=\"11\" fill=\"#666\">{} nodes · {} edges</text>\n",
        n,
        sub.local_edges.len()
    ));
    let legend = [
        ("type", "#4f83cc"),
        ("function", "#4caf72"),
        ("method", "#3fb6a8"),
        ("module", "#9575cd"),
        ("constant", "#c75b6d"),
    ];
    let mut ly = size - 16.0 - legend.len() as f32 * 16.0;
    for (name, color) in legend {
        s.push_str(&format!("<circle cx=\"24\" cy=\"{ly:.0}\" r=\"5\" fill=\"{color}\"/>\n"));
        s.push_str(&format!("<text x=\"36\" y=\"{:.0}\" font-size=\"11\" fill=\"#444\">{name}</text>\n", ly + 4.0));
        ly += 16.0;
    }
    s.push_str("</svg>\n");
    s
}

// --- DOT (for when graphviz IS available) -----------------------------------------------------

/// Render the subgraph as Graphviz DOT. `dot -Tsvg out.dot > out.svg` when graphviz is installed.
pub fn to_dot(sub: &SubGraph, title: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("digraph code {{\n  label=\"{}\";\n  node [style=filled,fontname=\"sans\"];\n", esc(title)));
    for (local, &g) in sub.subset.iter().enumerate() {
        let node = &sub.parent.nodes[g];
        let (_, fill, dshape) = kind_style(&node.kind);
        s.push_str(&format!(
            "  n{local} [label=\"{}\",shape={dshape},fillcolor=\"{fill}\"];\n",
            esc(&node.name)
        ));
    }
    for &(a, b, kind) in &sub.local_edges {
        s.push_str(&format!(
            "  n{a} -> n{b} [color=\"{}\",tooltip=\"{}\"];\n",
            edge_color(kind),
            edge_label(kind)
        ));
    }
    s.push_str("}\n");
    s
}
