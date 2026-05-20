//! Pluggable layout algorithms for the two graph panels. Both the vault
//! link graph (`panels/graph.rs`) and the cluster-tree graph
//! (`panels/cluster_graph.rs`) can render their nodes under any of the
//! kinds enumerated here.
//!
//! - **Force-directed** lives in `widgets::force_layout` (runs on a
//!   background thread). The other three are pure, O(n) deterministic
//!   functions over a tree structure.
//! - The vault graph is not a tree, so it's flattened to one by BFS
//!   from a chosen root before any tree layout runs.
//! - Cross-edges (non-tree edges) are still rendered; the tree only
//!   shapes the positions.

use eframe::egui::Vec2;

#[allow(dead_code)] // used by app; also included by graph-snapshot which doesn't construct it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutKind {
    ForceDirected,
    Radial,
    VerticalTree,
    HorizontalTree,
}

impl LayoutKind {
    #[allow(dead_code)] // see LayoutKind
    pub fn label(self) -> &'static str {
        match self {
            LayoutKind::ForceDirected => "Force-directed",
            LayoutKind::Radial => "Radial",
            LayoutKind::VerticalTree => "Vertical tree",
            LayoutKind::HorizontalTree => "Horizontal tree",
        }
    }

    #[allow(dead_code)] // see LayoutKind
    pub fn all() -> [LayoutKind; 4] {
        [
            LayoutKind::ForceDirected,
            LayoutKind::Radial,
            LayoutKind::VerticalTree,
            LayoutKind::HorizontalTree,
        ]
    }
}

/// Tree shape used as input to the radial / vertical / horizontal
/// layouts. Each node has at most one parent; nodes with `None` parent
/// are roots. Disconnected components surface as additional roots.
pub struct LayoutTree {
    pub n: usize,
    pub children: Vec<Vec<usize>>,
    pub roots: Vec<usize>,
    pub depth: Vec<usize>,
    /// Number of leaves under each node (≥1 for leaves themselves).
    /// Drives angular/horizontal sweep weighting so dense subtrees get
    /// proportionally more room.
    pub subtree_leaves: Vec<usize>,
}

impl LayoutTree {
    pub fn from_parents(parent_of: Vec<Option<usize>>) -> Self {
        let n = parent_of.len();
        let mut children = vec![Vec::new(); n];
        let mut roots = Vec::new();
        for (i, p) in parent_of.iter().enumerate() {
            match *p {
                Some(p) if p < n && p != i => children[p].push(i),
                _ => roots.push(i),
            }
        }
        let mut depth = vec![0usize; n];
        let mut stack: Vec<(usize, usize)> = roots.iter().map(|&r| (r, 0)).collect();
        while let Some((u, d)) = stack.pop() {
            depth[u] = d;
            for &c in &children[u] {
                stack.push((c, d + 1));
            }
        }
        // Post-order to compute subtree leaf counts.
        let mut order: Vec<usize> = Vec::with_capacity(n);
        let mut stack: Vec<usize> = roots.clone();
        while let Some(u) = stack.pop() {
            order.push(u);
            for &c in &children[u] {
                stack.push(c);
            }
        }
        let mut subtree_leaves = vec![0usize; n];
        for &u in order.iter().rev() {
            if children[u].is_empty() {
                subtree_leaves[u] = 1;
            } else {
                let s: usize = children[u].iter().map(|&c| subtree_leaves[c]).sum();
                subtree_leaves[u] = s.max(1);
            }
        }
        Self {
            n,
            children,
            roots,
            depth,
            subtree_leaves,
        }
    }
}

/// Build an adjacency list from an undirected edge set, deduplicated.
fn build_adj(n: usize, edges: &[(u32, u32)]) -> Vec<Vec<usize>> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        let (a, b) = (a as usize, b as usize);
        if a >= n || b >= n || a == b {
            continue;
        }
        adj[a].push(b);
        adj[b].push(a);
    }
    adj
}

/// DFS spanning tree of an arbitrary graph. Produces **deep, narrow**
/// trees, which is what vertical/horizontal tree layouts want — BFS on
/// a non-tree graph fans out very shallowly and renders as flat
/// horizontal bands rather than a tree. Each node's neighbours are
/// sorted by degree descending so the highest-connected neighbour is
/// visited first, biasing the trunk through dense regions.
///
/// Disconnected components are reached by additional DFS passes
/// seeded at each unvisited node, producing additional roots.
pub fn dfs_tree(n: usize, edges: &[(u32, u32)], root: usize) -> LayoutTree {
    let mut adj = build_adj(n, edges);
    let degree: Vec<usize> = adj.iter().map(|nbrs| nbrs.len()).collect();
    for nbrs in adj.iter_mut() {
        nbrs.sort_by(|&a, &b| degree[b].cmp(&degree[a]));
    }
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut visited = vec![false; n];
    let seed = if root < n { root } else { 0 };
    for start in std::iter::once(seed).chain(0..n) {
        if start >= n || visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack: Vec<usize> = vec![start];
        while let Some(u) = stack.pop() {
            // Push in reverse so the *first* (highest-degree)
            // neighbour is popped next.
            for &v in adj[u].iter().rev() {
                if !visited[v] {
                    visited[v] = true;
                    parent[v] = Some(u);
                    stack.push(v);
                }
            }
        }
    }
    LayoutTree::from_parents(parent)
}

/// BFS spanning tree of an arbitrary graph. Produces **shallow, wide**
/// trees, which radial layout wants — radial places nodes on rings by
/// depth, so a deep DFS tree spreads thin across many rings while a
/// shallow BFS tree gives a balanced fan.
pub fn bfs_tree(n: usize, edges: &[(u32, u32)], root: usize) -> LayoutTree {
    let adj = build_adj(n, edges);
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut visited = vec![false; n];
    let seed = if root < n { root } else { 0 };
    for start in std::iter::once(seed).chain(0..n) {
        if start >= n || visited[start] {
            continue;
        }
        visited[start] = true;
        let mut q = std::collections::VecDeque::new();
        q.push_back(start);
        while let Some(u) = q.pop_front() {
            for &v in &adj[u] {
                if !visited[v] {
                    visited[v] = true;
                    parent[v] = Some(u);
                    q.push_back(v);
                }
            }
        }
    }
    LayoutTree::from_parents(parent)
}

/// Radial layout: root at origin (or roots spaced around a small ring
/// for forests), children fan outward by depth. Subtree sweep is
/// weighted by leaf count so dense subtrees get proportionally more
/// angular room.
///
/// Sizing uses fixed world-unit minimums (not the `area` parameter):
/// rings are at least `MIN_RING_STEP` apart, and the outermost ring
/// stretches large enough that each leaf gets at least `MIN_LEAF_ARC`
/// of arc length. This avoids the dense-tree overlap problem where
/// node radii were larger than the per-leaf spacing.
pub fn radial_positions(tree: &LayoutTree, _area: f32) -> Vec<Vec2> {
    const MIN_RING_STEP: f32 = 90.0;
    const MIN_LEAF_ARC: f32 = 60.0;

    let max_depth = tree.depth.iter().copied().max().unwrap_or(0).max(1) as f32;
    let total_leaves: f32 = tree
        .roots
        .iter()
        .map(|&r| tree.subtree_leaves[r] as f32)
        .sum::<f32>()
        .max(1.0);
    let r_for_arc = total_leaves * MIN_LEAF_ARC / std::f32::consts::TAU;
    let r_for_depth = max_depth * MIN_RING_STEP;
    let r_max = r_for_arc.max(r_for_depth).max(MIN_RING_STEP);

    let mut out = vec![Vec2::ZERO; tree.n];
    let n_roots = tree.roots.len().max(1);
    let root_step = std::f32::consts::TAU / n_roots as f32;
    for (i, &root) in tree.roots.iter().enumerate() {
        let start = i as f32 * root_step;
        let end = start + root_step;
        radial_subtree(tree, root, start, end, r_max, max_depth, &mut out);
    }
    if tree.roots.len() == 1 {
        out[tree.roots[0]] = Vec2::ZERO;
    }
    out
}

fn radial_subtree(
    tree: &LayoutTree,
    node: usize,
    angle_start: f32,
    angle_end: f32,
    r_max: f32,
    max_depth: f32,
    out: &mut [Vec2],
) {
    let angle = (angle_start + angle_end) * 0.5;
    let radius = (tree.depth[node] as f32 / max_depth) * r_max;
    out[node] = Vec2::new(angle.cos() * radius, angle.sin() * radius);

    let children = &tree.children[node];
    if children.is_empty() {
        return;
    }
    let total: usize = children.iter().map(|&c| tree.subtree_leaves[c]).sum();
    if total == 0 {
        return;
    }
    let span = angle_end - angle_start;
    let mut cursor = angle_start;
    for &c in children {
        let w = tree.subtree_leaves[c] as f32 / total as f32;
        let child_span = span * w;
        radial_subtree(tree, c, cursor, cursor + child_span, r_max, max_depth, out);
        cursor += child_span;
    }
}

/// Vertical tidy tree: root at the top (y=0), depth growing downward,
/// x-position by post-order leaf placement (Reingold–Tilford-lite).
///
/// Uses fixed world-unit step sizes so node radii (8–20px) don't
/// overlap their neighbours. The total extent grows with the tree;
/// the canvas pans/zooms.
pub fn vertical_tree_positions(tree: &LayoutTree, _area: f32) -> Vec<Vec2> {
    const X_STEP: f32 = 60.0;
    const Y_STEP: f32 = 110.0;

    if tree.n == 0 {
        return Vec::new();
    }
    let x_step = X_STEP;
    let y_step = Y_STEP;

    let mut out = vec![Vec2::ZERO; tree.n];
    let mut cursor = 0.0f32;
    for &r in &tree.roots {
        cursor = vertical_subtree(tree, r, cursor, x_step, y_step, &mut out);
    }
    // Center horizontally around origin.
    let (lo, hi) = out
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v.x), hi.max(v.x))
        });
    let mid = if lo.is_finite() && hi.is_finite() {
        (lo + hi) * 0.5
    } else {
        0.0
    };
    for v in out.iter_mut() {
        v.x -= mid;
    }
    out
}

fn vertical_subtree(
    tree: &LayoutTree,
    node: usize,
    x_cursor: f32,
    x_step: f32,
    y_step: f32,
    out: &mut [Vec2],
) -> f32 {
    let y = tree.depth[node] as f32 * y_step;
    let children = &tree.children[node];
    if children.is_empty() {
        out[node] = Vec2::new(x_cursor, y);
        return x_cursor + x_step;
    }
    let start_x = x_cursor;
    let mut cur = x_cursor;
    for &c in children {
        cur = vertical_subtree(tree, c, cur, x_step, y_step, out);
    }
    let end_x = cur - x_step;
    let mid = (start_x + end_x) * 0.5;
    out[node] = Vec2::new(mid, y);
    cur
}

/// Horizontal tidy tree: rotate the vertical layout 90° so the root
/// sits on the left and depth grows rightward.
pub fn horizontal_tree_positions(tree: &LayoutTree, area: f32) -> Vec<Vec2> {
    vertical_tree_positions(tree, area)
        .into_iter()
        .map(|v| Vec2::new(v.y, v.x))
        .collect()
}
