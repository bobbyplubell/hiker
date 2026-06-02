//! Canvas export: a one-way snapshot of a **trail** or a **cluster tree** into
//! a fresh JSON Canvas document. The structure (which waypoints / clusters,
//! their order and nesting) is frozen at export time; content stays live only
//! because File nodes are pointers at the referenced notes. Re-exporting makes
//! a new file — there is no "update the existing canvas" link to maintain.
//!
//! This module is the egui-free CORE: pure builders that map a source
//! structure to a [`hiker_canvas::model::Canvas`], plus disk-writing entry
//! points that seed a new `.canvas` op-log document with the builder's
//! canonical JSON. Layout is deterministic (ids derived by walk order, never
//! random / time) so the same source serializes byte-identically.
//
// status: canvas-export-builder
// status: canvas-export-trail
// status: canvas-export-tree
// status: canvas-export-tree-force
// status: canvas-export-snapshot
// status: canvas-export-output

use std::collections::BTreeMap;

use hiker_canvas::model::{Canvas, Edge, EndCap, Node, NodeKind};

use crate::errors::HikerError;
use crate::oplog::OpLog;
use crate::store::Store;
use crate::trails::{ResolvedWaypoint, TrailDetail, get_trail};
use crate::trees::types::{self, EditableNode};
use crate::vault::Vault;

/// Default width of an exported card node, in canvas coordinate units.
const NODE_W: i64 = 260;
/// Default height of an exported card node.
const NODE_H: i64 = 140;
/// Gap between sibling nodes along the layout axis.
const GAP: i64 = 60;
/// Perpendicular offset a side trail branches from its parent's row.
const BRANCH_OFFSET: i64 = NODE_H + GAP;
/// Inner padding between a group's frame and the children it contains.
const PAD: i64 = 24;
/// Reserved height at the top of a group for its label (and summary text).
const LABEL_BAND: i64 = 48;
/// Height of a cluster summary's small Text node.
const SUMMARY_H: i64 = 60;
/// Width of a force-layout cluster's small labeled Text node.
const FORCE_LABEL_W: i64 = 160;
/// Height of a force-layout cluster's small labeled Text node.
const FORCE_LABEL_H: i64 = 60;
/// Fixed iteration count for the deterministic force relaxation.
const FORCE_ITERS: usize = 300;
/// Ideal edge length (the `k` of Fruchterman-Reingold), comfortably larger
/// than the node footprint so connected nodes separate rather than overlap.
const FORCE_K: f64 = 360.0;

// ── Trail → canvas ──────────────────────────────────────────────────────

/// Build a canvas from a resolved trail. One [`NodeKind::File`] node per
/// waypoint (pointing at the waypoint-NOTE so its annotation renders live),
/// one [`Edge`] per parent→child link (`from_end = None`, `to_end = Arrow` —
/// the reading direction). Layout is a depth-first layered walk: the main line
/// chains horizontally in trail order, each side trail offset downward from
/// its parent so digressions read as branches.
///
/// status: canvas-export-trail
#[must_use]
pub fn trail_to_canvas(detail: &TrailDetail) -> Canvas {
    let mut walk = TrailWalk::default();
    walk.layout_forest(&detail.waypoints, 0, 0);
    Canvas {
        nodes: walk.nodes,
        edges: walk.edges,
        extra: BTreeMap::new(),
    }
}

/// Accumulator for the depth-first trail layout. `next_id` hands out
/// deterministic `n0`, `n1`, … ids in walk order.
#[derive(Default)]
struct TrailWalk {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    next_id: usize,
}

impl TrailWalk {
    /// Lay out a forest of sibling waypoints whose first card sits at
    /// (`col`, `row`). Returns the next free column after the deepest sibling
    /// chain, so the caller can continue the main line past this subtree.
    fn layout_forest(&mut self, waypoints: &[ResolvedWaypoint], col: i64, row: i64) -> i64 {
        let mut cursor = col;
        for wp in waypoints {
            cursor = self.layout_node(wp, cursor, row, None);
        }
        cursor
    }

    /// Place one waypoint at (`col`, `row`), wire the edge from `parent_id`
    /// when present, then recurse into its children one row below. Returns the
    /// next free column after this waypoint and its descendants.
    fn layout_node(
        &mut self,
        wp: &ResolvedWaypoint,
        col: i64,
        row: i64,
        parent_id: Option<&str>,
    ) -> i64 {
        let id = format!("n{}", self.next_id);
        self.next_id += 1;
        let x = col * (NODE_W + GAP);
        let y = row * BRANCH_OFFSET;
        self.nodes.push(file_node(id.clone(), x, y, &wp.waypoint_rel));
        if let Some(parent) = parent_id {
            self.edges.push(reading_edge(self.edges.len(), parent, &id));
        }

        let mut cursor = col + 1;
        for (i, child) in wp.children.iter().enumerate() {
            // First child continues the main line on the same row; further
            // children branch onto deeper rows so side trails read as forks.
            let child_row = row + i64::from(i > 0);
            cursor = self.layout_node(child, cursor, child_row, Some(&id));
        }
        cursor
    }
}

/// A File node of the default size at (`x`, `y`) pointing at `file`.
fn file_node(id: String, x: i64, y: i64, file: &str) -> Node {
    Node {
        id,
        x,
        y,
        width: NODE_W,
        height: NODE_H,
        color: None,
        kind: NodeKind::File {
            file: file.to_owned(),
            subpath: None,
        },
        extra: BTreeMap::new(),
    }
}

/// A reading-direction edge `from → to`: no source cap, an arrowhead at the
/// destination. `seq` derives the deterministic `e<seq>` id.
fn reading_edge(seq: usize, from: &str, to: &str) -> Edge {
    Edge {
        id: format!("e{seq}"),
        from_node: from.to_owned(),
        from_side: None,
        from_end: Some(EndCap::None),
        to_node: to.to_owned(),
        to_side: None,
        to_end: Some(EndCap::Arrow),
        color: None,
        label: None,
        extra: BTreeMap::new(),
    }
}

// ── Cluster tree → canvas ───────────────────────────────────────────────

/// Which visual style a cluster-tree export produces. `Grouped` is the
/// default: nested [`NodeKind::Group`] containers, hierarchy = spatial
/// nesting. `ForceDirected`: one node per tree node, parent→child
/// [`Edge`] connectors, an organic node-link layout.
///
/// status: canvas-export-tree-force
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeCanvasStyle {
    /// Nested group containers, no edges (the default).
    #[default]
    Grouped,
    /// Node-link graph relaxed by a deterministic force layout.
    ForceDirected,
}

/// Build a canvas from a cluster tree's flat node list in the chosen `style`.
/// `Grouped` packs nested group containers (the default); `ForceDirected`
/// emits a node-link graph relaxed by a deterministic force layout. Either
/// way centroids, policies, and confidence are dropped — a canvas is a
/// presentation snapshot, not the tree's automation program.
///
/// status: canvas-export-tree
#[must_use]
pub fn tree_to_canvas(tree_name: &str, nodes: &[EditableNode], style: TreeCanvasStyle) -> Canvas {
    match style {
        TreeCanvasStyle::Grouped => tree_to_canvas_grouped(tree_name, nodes),
        TreeCanvasStyle::ForceDirected => tree_to_canvas_force(nodes),
    }
}

/// The Grouped builder. Each cluster (and the outlier bucket) becomes a
/// [`NodeKind::Group`] labeled with the cluster name, with a small summary
/// [`NodeKind::Text`] node when the summary is non-empty; each leaf with a
/// `note_path` becomes a [`NodeKind::File`] node. Hierarchy is SPATIAL —
/// child groups and leaf cards are packed inside their parent's rect (no
/// edges). Sizes are computed bottom-up so every parent frames its children.
/// Ids are deterministic in walk order.
fn tree_to_canvas_grouped(tree_name: &str, nodes: &[EditableNode]) -> Canvas {
    let packer = TreePacker::new(nodes);
    let mut emit = TreeEmit::default();
    // A synthetic root frames the whole tree under the tree's own name so a
    // multi-root forest still packs into a single deterministic layout.
    let roots = packer.children(None);
    let boxes: Vec<Placed> = roots.iter().map(|id| packer.measure(id)).collect();
    let root = frame(tree_name, "", &boxes);
    emit.place(&packer, &root, 0, 0);
    Canvas {
        nodes: emit.nodes,
        edges: Vec::new(),
        extra: BTreeMap::new(),
    }
}

/// A measured subtree: its own outer size plus, for a group, the placed
/// children to recurse into. Leaves carry `children` empty.
#[derive(Clone)]
struct Placed {
    /// Source node id (`""` for the synthetic tree root).
    source_id: String,
    width: i64,
    height: i64,
    /// `true` for a cluster / outlier-bucket group, `false` for a leaf card.
    is_group: bool,
    name: String,
    summary: String,
    note_path: Option<String>,
    /// Children already measured, in grid order. Empty for leaves.
    children: Vec<Placed>,
}

/// Read-only view over the flat node list that resolves parent→child links.
struct TreePacker<'a> {
    nodes: &'a [EditableNode],
}

impl<'a> TreePacker<'a> {
    const fn new(nodes: &'a [EditableNode]) -> Self {
        Self { nodes }
    }

    /// Ids of the direct children of `parent` (`None` = roots), in stored
    /// order — the deterministic source order.
    fn children(&self, parent: Option<&str>) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| n.parent.as_deref() == parent)
            .map(|n| n.id.clone())
            .collect()
    }

    fn node(&self, id: &str) -> Option<&EditableNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Measure the subtree rooted at `id` bottom-up into a [`Placed`].
    fn measure(&self, id: &str) -> Placed {
        let Some(node) = self.node(id) else {
            return frame(id, "", &[]);
        };
        if matches!(node.kind, types::NodeKind::Leaf) {
            return Placed {
                source_id: node.id.clone(),
                width: NODE_W,
                height: NODE_H,
                is_group: false,
                name: node.name.clone(),
                summary: String::new(),
                note_path: node.note_path.clone(),
                children: Vec::new(),
            };
        }
        let kids: Vec<Placed> = self
            .children(Some(&node.id))
            .iter()
            .map(|cid| self.measure(cid))
            .collect();
        let mut framed = frame(&node.name, &node.summary, &kids);
        framed.source_id = node.id.clone();
        framed.note_path = node.note_path.clone();
        framed
    }
}

/// Build a group [`Placed`] sized to contain `children` packed in a grid, plus
/// the label band (and a summary row when `summary` is non-empty).
fn frame(name: &str, summary: &str, children: &[Placed]) -> Placed {
    let (inner_w, inner_h) = grid_extent(children);
    let band = LABEL_BAND + if summary.is_empty() { 0 } else { SUMMARY_H + PAD };
    Placed {
        source_id: String::new(),
        width: inner_w + PAD * 2,
        height: inner_h + band + PAD,
        is_group: true,
        name: name.to_owned(),
        summary: summary.to_owned(),
        note_path: None,
        children: children.to_vec(),
    }
}

/// Grid columns for `n` children: the near-square ceil(sqrt(n)).
const fn grid_cols(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut c = 1;
    while c * c < n {
        c += 1;
    }
    c
}

/// Inner (width, height) a row-major grid of `children` occupies, using the
/// widest / tallest child as the uniform cell so rects never overlap.
fn grid_extent(children: &[Placed]) -> (i64, i64) {
    if children.is_empty() {
        return (NODE_W, NODE_H);
    }
    let cell_w = children.iter().map(|c| c.width).max().unwrap_or(NODE_W);
    let cell_h = children.iter().map(|c| c.height).max().unwrap_or(NODE_H);
    let cols = grid_cols(children.len());
    let rows = children.len().div_ceil(cols);
    let w = cell_w * cols as i64 + GAP * (cols as i64 - 1);
    let h = cell_h * rows as i64 + GAP * (rows as i64 - 1);
    (w, h)
}

/// Emits concrete [`Node`]s from the measured [`Placed`] tree, assigning
/// deterministic `n0`, `n1`, … ids in placement order.
#[derive(Default)]
struct TreeEmit {
    nodes: Vec<Node>,
    next_id: usize,
}

impl TreeEmit {
    fn fresh_id(&mut self) -> String {
        let id = format!("n{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Place `node` at absolute (`x`, `y`) and, for a group, lay its children
    /// in a grid inside the label/summary band.
    fn place(&mut self, packer: &TreePacker, node: &Placed, x: i64, y: i64) {
        if !node.is_group {
            if let Some(file) = &node.note_path {
                let id = self.fresh_id();
                self.nodes.push(file_node(id, x, y, file));
            }
            return;
        }
        let id = self.fresh_id();
        self.nodes.push(group_node(id, x, y, node));
        let mut band = LABEL_BAND;
        if !node.summary.is_empty() {
            let sid = self.fresh_id();
            let w = node.width - PAD * 2;
            self.nodes
                .push(text_node(sid, x + PAD, y + band, w, &node.summary));
            band += SUMMARY_H + PAD;
        }
        self.place_children(packer, &node.children, x + PAD, y + band);
    }

    /// Lay `children` in a row-major grid whose origin is (`ox`, `oy`).
    fn place_children(&mut self, packer: &TreePacker, children: &[Placed], ox: i64, oy: i64) {
        if children.is_empty() {
            return;
        }
        let cell_w = children.iter().map(|c| c.width).max().unwrap_or(NODE_W);
        let cell_h = children.iter().map(|c| c.height).max().unwrap_or(NODE_H);
        let cols = grid_cols(children.len());
        for (i, child) in children.iter().enumerate() {
            let r = i / cols;
            let c = i % cols;
            let cx = ox + c as i64 * (cell_w + GAP);
            let cy = oy + r as i64 * (cell_h + GAP);
            self.place(packer, child, cx, cy);
        }
    }
}

/// A Group node of `node`'s measured size at (`x`, `y`), labeled with its name.
fn group_node(id: String, x: i64, y: i64, node: &Placed) -> Node {
    Node {
        id,
        x,
        y,
        width: node.width,
        height: node.height,
        color: None,
        kind: NodeKind::Group {
            label: (!node.name.is_empty()).then(|| node.name.clone()),
            background: None,
            background_style: None,
        },
        extra: BTreeMap::new(),
    }
}

/// A small Text node carrying a cluster summary.
fn text_node(id: String, x: i64, y: i64, width: i64, text: &str) -> Node {
    Node {
        id,
        x,
        y,
        width,
        height: SUMMARY_H,
        color: None,
        kind: NodeKind::Text {
            text: text.to_owned(),
        },
        extra: BTreeMap::new(),
    }
}

// ── Force-directed cluster tree → canvas ────────────────────────────────

/// Build a node-link canvas from a cluster tree's flat node list. Every
/// cluster / outlier bucket becomes a small labeled [`NodeKind::Text`] node
/// and every leaf with a `note_path` a [`NodeKind::File`] node (path-less
/// leaves are rendered as a `Text` node of their name); each parent→child
/// link becomes an undirected-looking [`Edge`]. Positions come from a
/// deterministic Fruchterman-Reingold relaxation, so the same tree always
/// serializes byte-identically.
///
/// status: canvas-export-tree-force
fn tree_to_canvas_force(nodes: &[EditableNode]) -> Canvas {
    let graph = ForceGraph::build(nodes);
    let centers = graph.relax();
    graph.emit(&centers)
}

/// A node-link graph distilled from the tree: one entry per tree node (in
/// stored order, so ids stay deterministic), its size, the canvas node it
/// becomes, and the parent→child edges by index.
struct ForceGraph {
    sizes: Vec<(i64, i64)>,
    kinds: Vec<NodeKind>,
    edges: Vec<(usize, usize)>,
}

impl ForceGraph {
    /// Map each tree node to a graph node and each parent link to an edge.
    /// A leaf with no path falls back to a `Text` node of its name so the
    /// node count matches the tree (only the size differs by kind).
    fn build(nodes: &[EditableNode]) -> Self {
        let index: BTreeMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let mut sizes = Vec::with_capacity(nodes.len());
        let mut kinds = Vec::with_capacity(nodes.len());
        for n in nodes {
            let (size, kind) = force_node(n);
            sizes.push(size);
            kinds.push(kind);
        }
        let edges = nodes
            .iter()
            .enumerate()
            .filter_map(|(child, n)| {
                let pid = n.parent.as_deref()?;
                index.get(pid).map(|&parent| (parent, child))
            })
            .collect();
        Self { sizes, kinds, edges }
    }

    /// Seed positions deterministically on a circle by index, then run a
    /// fixed number of Fruchterman-Reingold steps (repulsion between all
    /// pairs, attraction along edges, displacement clamped by a cooling
    /// temperature). Returns each node's relaxed center.
    fn relax(&self) -> Vec<(f64, f64)> {
        let n = self.sizes.len();
        let mut pos = seed_circle(n);
        let mut temp = FORCE_K;
        let cooling = if FORCE_ITERS > 0 {
            FORCE_K / FORCE_ITERS as f64
        } else {
            0.0
        };
        for _ in 0..FORCE_ITERS {
            let mut disp = vec![(0.0_f64, 0.0_f64); n];
            apply_repulsion(&pos, &mut disp);
            self.apply_attraction(&pos, &mut disp);
            apply_displacement(&mut pos, &disp, temp);
            temp = (temp - cooling).max(0.0);
        }
        pos
    }

    /// Accumulate edge spring attraction (`d^2 / k`) into `disp`, pulling a
    /// connected pair toward each other.
    fn apply_attraction(&self, pos: &[(f64, f64)], disp: &mut [(f64, f64)]) {
        for &(a, b) in &self.edges {
            let (dx, dy) = (pos[a].0 - pos[b].0, pos[a].1 - pos[b].1);
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = dist * dist / FORCE_K;
            let (ux, uy) = (dx / dist * force, dy / dist * force);
            disp[a].0 -= ux;
            disp[a].1 -= uy;
            disp[b].0 += ux;
            disp[b].1 += uy;
        }
    }

    /// Emit the canvas: each node placed top-left = relaxed center − half
    /// size (rounded to integer canvas units), each tree edge a connector.
    fn emit(&self, centers: &[(f64, f64)]) -> Canvas {
        let ids: Vec<String> = (0..self.sizes.len()).map(|i| format!("n{i}")).collect();
        let nodes = self
            .kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                let (w, h) = self.sizes[i];
                let x = centers[i].0.round() as i64 - w / 2;
                let y = centers[i].1.round() as i64 - h / 2;
                Node {
                    id: ids[i].clone(),
                    x,
                    y,
                    width: w,
                    height: h,
                    color: None,
                    kind: kind.clone(),
                    extra: BTreeMap::new(),
                }
            })
            .collect();
        let edges = self
            .edges
            .iter()
            .enumerate()
            .map(|(seq, &(a, b))| link_edge(seq, &ids[a], &ids[b]))
            .collect();
        Canvas { nodes, edges, extra: BTreeMap::new() }
    }
}

/// The (size, kind) a tree node maps to in the force graph: a leaf with a
/// path → a `File` card at the default node size; a cluster / outlier bucket
/// (or a path-less leaf) → a small labeled `Text` node.
fn force_node(node: &EditableNode) -> ((i64, i64), NodeKind) {
    if matches!(node.kind, types::NodeKind::Leaf)
        && let Some(file) = &node.note_path
    {
        return (
            (NODE_W, NODE_H),
            NodeKind::File { file: file.clone(), subpath: None },
        );
    }
    (
        (FORCE_LABEL_W, FORCE_LABEL_H),
        NodeKind::Text { text: node.name.clone() },
    )
}

/// Seed `n` node centers evenly on a circle whose radius scales with the node
/// count, deterministic by index so the relaxation has no random input.
fn seed_circle(n: usize) -> Vec<(f64, f64)> {
    if n == 0 {
        return Vec::new();
    }
    let radius = FORCE_K * (n as f64).sqrt();
    (0..n)
        .map(|i| {
            let theta = std::f64::consts::TAU * i as f64 / n as f64;
            (radius * theta.cos(), radius * theta.sin())
        })
        .collect()
}

/// Accumulate all-pairs repulsion (`k^2 / d`) into `disp`, pushing every pair
/// of nodes apart along the line between their centers.
fn apply_repulsion(pos: &[(f64, f64)], disp: &mut [(f64, f64)]) {
    let n = pos.len();
    let k2 = FORCE_K * FORCE_K;
    for i in 0..n {
        for j in (i + 1)..n {
            let (dx, dy) = (pos[i].0 - pos[j].0, pos[i].1 - pos[j].1);
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = k2 / dist;
            let (ux, uy) = (dx / dist * force, dy / dist * force);
            disp[i].0 += ux;
            disp[i].1 += uy;
            disp[j].0 -= ux;
            disp[j].1 -= uy;
        }
    }
}

/// Move each node by its accumulated displacement, clamped to the cooling
/// `temp` so late iterations only fine-tune.
fn apply_displacement(pos: &mut [(f64, f64)], disp: &[(f64, f64)], temp: f64) {
    for (p, d) in pos.iter_mut().zip(disp) {
        let len = (d.0 * d.0 + d.1 * d.1).sqrt().max(0.01);
        let step = len.min(temp);
        p.0 += d.0 / len * step;
        p.1 += d.1 / len * step;
    }
}

/// An undirected-looking connector `from ↔ to`: no caps at either end, since
/// the tree's containment isn't a reading order. `seq` derives the
/// deterministic `e<seq>` id.
fn link_edge(seq: usize, from: &str, to: &str) -> Edge {
    Edge {
        id: format!("e{seq}"),
        from_node: from.to_owned(),
        from_side: None,
        from_end: None,
        to_node: to.to_owned(),
        to_side: None,
        to_end: None,
        color: None,
        label: None,
        extra: BTreeMap::new(),
    }
}

// ── Disk-writing entry points (canvas-export-output) ────────────────────

/// Export a trail to a fresh `.canvas` document beside the trail-doc. Loads
/// via [`get_trail`], builds via [`trail_to_canvas`], seeds the new file with
/// canonical JSON through the op-log path, and returns its vault-relative
/// path. The name is `<trail-basename>.canvas`, suffix-counted on collision.
///
/// # Errors
/// Propagates trail-load, path, or op-log write failures.
///
/// status: canvas-export-output
pub fn write_trail_canvas(
    vault: &Vault,
    store: &Store,
    log: &OpLog,
    trail_doc_rel: &str,
) -> Result<String, HikerError> {
    let detail = get_trail(vault, store, log, trail_doc_rel)?;
    let canvas = trail_to_canvas(&detail);
    let (dir, stem) = split_dir_stem(trail_doc_rel);
    let rel = pick_free_path(vault, dir, stem)?;
    write_canvas(log, vault, &rel, &canvas)?;
    Ok(rel)
}

/// Export a cluster tree (in its on-disk state) to a fresh `.canvas` document
/// at the vault root in the chosen `style`. Loads the tree's name + nodes via
/// `trees`, builds via [`tree_to_canvas`], seeds the new file through the
/// op-log path, and returns its vault-relative path. The name is
/// `<tree-name>.canvas`, suffix-counted.
///
/// # Errors
/// Propagates tree-not-found, path, or op-log write failures.
///
/// status: canvas-export-output
pub fn write_tree_canvas(
    trees: &types::Db,
    vault: &Vault,
    log: &OpLog,
    tree_id: &str,
    style: TreeCanvasStyle,
) -> Result<String, HikerError> {
    let row = trees
        .get_tree(tree_id)?
        .ok_or_else(|| HikerError::NotFound(format!("cluster tree: {tree_id}")))?;
    let nodes = trees.list_nodes(tree_id)?;
    let canvas = tree_to_canvas(&row.name, &nodes, style);
    let stem = sanitize_stem(&row.name);
    let rel = pick_free_path(vault, "", &stem)?;
    write_canvas(log, vault, &rel, &canvas)?;
    Ok(rel)
}

/// Split a vault-relative file path into its folder (`""` at root) and the
/// basename without extension.
fn split_dir_stem(rel: &str) -> (&str, &str) {
    let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let base = rel.rsplit_once('/').map_or(rel, |(_, b)| b);
    let stem = base.rsplit_once('.').map_or(base, |(s, _)| s);
    (dir, stem)
}

/// Make a tree name safe to use as a filename stem: path separators become
/// dashes, and an empty result falls back to `tree`.
fn sanitize_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "tree".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Pick the first free `<dir>/<stem>.canvas`, then `…-2.canvas`, … so a repeat
/// export never clobbers an earlier one.
fn pick_free_path(vault: &Vault, dir: &str, stem: &str) -> Result<String, HikerError> {
    for n in 1..10_000 {
        let name = if n == 1 {
            format!("{stem}.canvas")
        } else {
            format!("{stem}-{n}.canvas")
        };
        let rel = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        let abs = vault.abs_path(&rel)?;
        if !abs.exists() {
            return Ok(rel);
        }
    }
    Err(HikerError::AlreadyExists(format!(
        "ran out of {stem}-N.canvas candidates"
    )))
}

/// Seed a new `.canvas` document at `rel` with the canvas's canonical JSON
/// through the op-log user-save path, mirroring the rename-rewrite write so
/// the file is a first-class op-log document (`canvas-doc-kind`).
fn write_canvas(log: &OpLog, vault: &Vault, rel: &str, canvas: &Canvas) -> Result<(), HikerError> {
    let json = canvas.to_canonical_json();
    crate::ops::op_writes::user_save(log, vault, rel, &json)
}

#[cfg(test)]
mod tests;
