//! Pure, egui-free dagre auto-arrange ("Tidy") over a [`Canvas`].
//!
//! [`auto_arrange`] maps the canvas onto the layered (Sugiyama / dagre) engine in
//! [`hiker_graph`] and returns a `Vec<EditOp>` of [`SetNodeRect`](EditOp::SetNodeRect)
//! moves — one per node that actually shifts — so the caller drives them through
//! the same op-log / undo pipeline as any other edit. The function never touches
//! egui or I/O.
//!
//! ## Index space
//! The dagre node space lists every leaf (non-`Group`) first in canvas order
//! `[0..L)`, then every `Group` `[L..L+G)`. A `Group` becomes a dagre *cluster*
//! (the engine sizes and positions it around its members), so its input size is
//! `(0, 0)` and its emitted rect comes from the engine's computed cluster
//! rectangle.
//!
//! ## Membership
//! Membership is geometric (groups are nodes; a leaf "belongs" to a group when
//! the group's bounds contain the leaf's center). For each leaf we pick the
//! *smallest-area* containing group — the tightest nesting. A group whose bounds
//! sit fully inside a larger group gets that larger group as its dagre parent, so
//! nested clusters survive.
//!
//! ## Translation
//! The engine lays the graph out at its own origin (top-left near `(0, 0)`). We
//! translate the whole result so its bounding-box center lands on the original
//! content's bounding-box center, keeping the tidied board roughly where the user
//! was looking instead of teleporting it to the origin.

use std::collections::HashMap;

use hiker_graph::{GraphInput, LayeredEngine, LayoutEngine, RankDir, Vec2};

use crate::geometry::{content_bounds, node_bounds, Rect};
use crate::model::{Canvas, Node, NodeKind};
use crate::ops::EditOp;

/// Which way ranks flow in an [`auto_arrange`] layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankDirection {
    /// Top to bottom (roots at the top). The default.
    TopToBottom,
    /// Bottom to top.
    BottomToTop,
    /// Left to right.
    LeftToRight,
    /// Right to left.
    RightToLeft,
}

impl RankDirection {
    const fn to_dagre(self) -> RankDir {
        match self {
            Self::TopToBottom => RankDir::Tb,
            Self::BottomToTop => RankDir::Bt,
            Self::LeftToRight => RankDir::Lr,
            Self::RightToLeft => RankDir::Rl,
        }
    }
}

/// Tuning for [`auto_arrange`]. Defaults match the dagre engine's own defaults
/// (`ranksep`/`nodesep` 50) with a top-to-bottom flow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArrangeOpts {
    /// Rank flow direction.
    pub rankdir: RankDirection,
    /// Separation between ranks (dagre `ranksep`).
    pub ranksep: f32,
    /// Separation between adjacent nodes in a rank (dagre `nodesep`).
    pub nodesep: f32,
}

impl Default for ArrangeOpts {
    fn default() -> Self {
        Self { rankdir: RankDirection::TopToBottom, ranksep: 50.0, nodesep: 50.0 }
    }
}

/// Compute a tidy hierarchical (dagre layered) arrangement of `canvas` and return
/// the [`SetNodeRect`](EditOp::SetNodeRect) ops that realize it.
///
/// Leaves are re-placed at their dagre centers (keeping their own size); each
/// `Group` frame is resized to the engine-computed cluster rectangle so it wraps
/// its members. A node whose new rect equals its current rect is skipped (no
/// no-op ops). The whole layout is translated so its bounding-box center matches
/// the original content's. Pure: no egui, no I/O.
///
/// Returns an empty vector when the canvas has no nodes.
#[must_use]
pub fn auto_arrange(canvas: &Canvas, opts: ArrangeOpts) -> Vec<EditOp> {
    let Some(plan) = ArrangePlan::build(canvas) else {
        return Vec::new();
    };

    let node_sizes = plan.node_sizes(canvas);
    let parents = plan.node_parents(canvas);
    let engine = LayeredEngine {
        rankdir: opts.rankdir.to_dagre(),
        ranksep: opts.ranksep,
        nodesep: opts.nodesep,
        edgesep: 20.0,
        default_node_size: Vec2::new(50.0, 50.0),
        // Beyond-dagre crossing cleanup (legibility; see order::transpose).
        transpose: true,
    };
    let out = engine.layout(&GraphInput {
        node_count: plan.node_count(),
        edges: &plan.edges,
        node_sizes: Some(&node_sizes),
        node_parents: if plan.groups.is_empty() { None } else { Some(&parents) },
        edge_label_sizes: None,
        directed: true,
    });

    let offset = centroid_offset(canvas, &out);
    let ops = plan.emit_ops(canvas, &out.positions, &out.node_sizes, offset);
    tracing::debug!(
        target: "hiker_canvas::arrange",
        leaves = plan.leaves.len(),
        groups = plan.groups.len(),
        edges = plan.edges.len(),
        ops = ops.len(),
        "auto_arrange produced moves",
    );
    ops
}

/// The index mapping from canvas nodes onto the dagre node space, plus the mapped
/// edge list. Built once, then reused to size, parent, and read back the layout.
struct ArrangePlan {
    /// Canvas indices of the leaf (non-`Group`) nodes, in dagre order `[0..L)`.
    leaves: Vec<usize>,
    /// Canvas indices of the `Group` nodes, in dagre order `[L..L+G)`.
    groups: Vec<usize>,
    /// Mapped leaf→leaf edges as dagre `(u32, u32)` index pairs.
    edges: Vec<(u32, u32)>,
}

impl ArrangePlan {
    /// Partition the canvas into leaves and groups and map the edges. Returns
    /// `None` when there are no nodes (nothing to arrange).
    fn build(canvas: &Canvas) -> Option<Self> {
        if canvas.nodes.is_empty() {
            return None;
        }
        let mut leaves = Vec::new();
        let mut groups = Vec::new();
        for (i, node) in canvas.nodes.iter().enumerate() {
            if matches!(node.kind, NodeKind::Group { .. }) {
                groups.push(i);
            } else {
                leaves.push(i);
            }
        }

        // id -> dagre leaf index, for mapping edges (groups are never edge ends).
        let mut leaf_of_id: HashMap<&str, u32> = HashMap::new();
        for (dagre_idx, &canvas_idx) in leaves.iter().enumerate() {
            leaf_of_id.insert(canvas.nodes[canvas_idx].id.as_str(), dagre_idx as u32);
        }
        let edges = canvas
            .edges
            .iter()
            .filter_map(|e| {
                let from = *leaf_of_id.get(e.from_node.as_str())?;
                let to = *leaf_of_id.get(e.to_node.as_str())?;
                Some((from, to))
            })
            .collect();

        Some(Self { leaves, groups, edges })
    }

    /// Total dagre node count `L + G`.
    fn node_count(&self) -> usize {
        self.leaves.len() + self.groups.len()
    }

    /// Per-dagre-node input sizes: leaves carry their own size; groups carry
    /// `(0, 0)` so the engine sizes the cluster from its children.
    fn node_sizes(&self, canvas: &Canvas) -> Vec<Vec2> {
        let mut sizes = vec![Vec2::ZERO; self.node_count()];
        for (dagre_idx, &canvas_idx) in self.leaves.iter().enumerate() {
            let node = &canvas.nodes[canvas_idx];
            sizes[dagre_idx] = Vec2::new(node.width as f32, node.height as f32);
        }
        sizes
    }

    /// Per-dagre-node parent indices (cluster membership), `None` at top level.
    ///
    /// A leaf's parent is the smallest group whose bounds contain the leaf's
    /// center. A group's parent is the smallest *other* group that fully contains
    /// it (a larger enclosing frame), so nested clusters survive.
    fn node_parents(&self, canvas: &Canvas) -> Vec<Option<usize>> {
        let mut parents = vec![None; self.node_count()];
        if self.groups.is_empty() {
            return parents;
        }
        // group canvas index -> dagre index, for the smallest-container lookups.
        let dagre_of_group: HashMap<usize, usize> = self
            .groups
            .iter()
            .enumerate()
            .map(|(pos, &ci)| (ci, self.group_dagre_idx(pos)))
            .collect();

        for (dagre_idx, &canvas_idx) in self.leaves.iter().enumerate() {
            let leaf = &canvas.nodes[canvas_idx];
            if let Some(group_ci) = smallest_container(canvas, &self.groups, leaf, false) {
                parents[dagre_idx] = Some(dagre_of_group[&group_ci]);
            }
        }
        for (pos, &canvas_idx) in self.groups.iter().enumerate() {
            let group = &canvas.nodes[canvas_idx];
            let others: Vec<usize> = self.groups.iter().copied().filter(|&ci| ci != canvas_idx).collect();
            if let Some(parent_ci) = smallest_container(canvas, &others, group, true) {
                parents[self.group_dagre_idx(pos)] = Some(dagre_of_group[&parent_ci]);
            }
        }
        parents
    }

    /// The dagre index of a group given its position in `self.groups`.
    fn group_dagre_idx(&self, group_pos: usize) -> usize {
        self.leaves.len() + group_pos
    }

    /// Emit one [`SetNodeRect`](EditOp::SetNodeRect) per node whose rect changes:
    /// leaves keep their own size and move to their dagre center; groups take the
    /// engine-computed cluster rect (center ± size/2). Order is leaves then
    /// groups; `SetNodeRect`s are independent so any order is safe.
    fn emit_ops(&self, canvas: &Canvas, positions: &[Vec2], node_sizes: &[Vec2], offset: Vec2) -> Vec<EditOp> {
        let mut ops = Vec::new();
        for (dagre_idx, &canvas_idx) in self.leaves.iter().enumerate() {
            let node = &canvas.nodes[canvas_idx];
            let Some(&center) = positions.get(dagre_idx) else { continue };
            let x = round_i64(center.x + offset.x - node.width as f32 / 2.0);
            let y = round_i64(center.y + offset.y - node.height as f32 / 2.0);
            push_if_moved(&mut ops, node, x, y, node.width, node.height);
        }
        for (pos, &canvas_idx) in self.groups.iter().enumerate() {
            let node = &canvas.nodes[canvas_idx];
            let dagre_idx = self.group_dagre_idx(pos);
            let (Some(&center), Some(&size)) = (positions.get(dagre_idx), node_sizes.get(dagre_idx))
            else {
                continue;
            };
            if size.x <= 0.0 || size.y <= 0.0 {
                // Empty group: dagre couldn't size it; leave its frame untouched.
                continue;
            }
            let x = round_i64(center.x + offset.x - size.x / 2.0);
            let y = round_i64(center.y + offset.y - size.y / 2.0);
            push_if_moved(&mut ops, node, x, y, round_i64(size.x), round_i64(size.y));
        }
        ops
    }
}

/// Append a `SetNodeRect` for `node` only when the target rect differs from its
/// current one (skip no-ops).
fn push_if_moved(ops: &mut Vec<EditOp>, node: &Node, x: i64, y: i64, width: i64, height: i64) {
    if node.x == x && node.y == y && node.width == width && node.height == height {
        return;
    }
    ops.push(EditOp::SetNodeRect { id: node.id.clone(), x, y, width, height });
}

/// Round an `f32` engine coordinate to the model's `i64` space.
fn round_i64(v: f32) -> i64 {
    v.round() as i64
}

/// The signed offset mapping the engine layout's bounding-box center onto the
/// original content's bounding-box center. Returns [`Vec2::ZERO`] when either is
/// undefined (no nodes / no positions).
fn centroid_offset(canvas: &Canvas, out: &hiker_graph::LayoutOutput) -> Vec2 {
    let Some(content) = content_bounds(canvas) else {
        return Vec2::ZERO;
    };
    if out.positions.is_empty() {
        return Vec2::ZERO;
    }
    // The engine lays out at its own origin; its content center is size/2.
    let target = content.center();
    Vec2::new(target.x as f32 - out.size.x / 2.0, target.y as f32 - out.size.y / 2.0)
}

/// Whether `outer` fully contains `inner` (edges inclusive).
fn rect_contains_rect(outer: &Rect, inner: &Rect) -> bool {
    inner.x >= outer.x && inner.y >= outer.y && inner.right() <= outer.right() && inner.bottom() <= outer.bottom()
}

/// The smallest-area node among `candidates` (by canvas index) that contains
/// `target` — by full-rect containment when `require_full`, else by center
/// containment. Ties broken by the smaller canvas index for determinism.
fn smallest_container(canvas: &Canvas, candidates: &[usize], target: &Node, require_full: bool) -> Option<usize> {
    let target_rect = node_bounds(target);
    let center = target_rect.center();
    candidates
        .iter()
        .copied()
        .filter(|&ci| {
            let r = node_bounds(&canvas.nodes[ci]);
            if require_full { rect_contains_rect(&r, &target_rect) } else { r.contains(center) }
        })
        .map(|ci| {
            let r = node_bounds(&canvas.nodes[ci]);
            (ci, r.width * r.height)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)))
        .map(|(ci, _)| ci)
}

#[cfg(test)]
mod tests {
    use super::{auto_arrange, ArrangeOpts, ArrangePlan, RankDirection};
    use crate::model::{Canvas, Edge, Node, NodeKind};
    use crate::ops::EditOp;
    use std::collections::BTreeMap;

    fn leaf(id: &str, x: i64, y: i64) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: 100,
            height: 60,
            color: None,
            kind: NodeKind::Text { text: String::new() },
            extra: BTreeMap::new(),
        }
    }

    fn group(id: &str, x: i64, y: i64, w: i64, h: i64) -> Node {
        Node {
            id: id.to_owned(),
            x,
            y,
            width: w,
            height: h,
            color: None,
            kind: NodeKind::Group { label: None, background: None, background_style: None },
            extra: BTreeMap::new(),
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
            extra: BTreeMap::new(),
        }
    }

    /// The y top-left a node would have after applying `ops` (else its current y).
    fn new_y(ops: &[EditOp], canvas: &Canvas, id: &str) -> i64 {
        ops.iter()
            .find_map(|op| match op {
                EditOp::SetNodeRect { id: oid, y, .. } if oid == id => Some(*y),
                _ => None,
            })
            .unwrap_or_else(|| canvas.nodes.iter().find(|n| n.id == id).unwrap().y)
    }

    fn rect_after(ops: &[EditOp], canvas: &Canvas, id: &str) -> (i64, i64, i64, i64) {
        for op in ops {
            if let EditOp::SetNodeRect { id: oid, x, y, width, height } = op
                && oid == id
            {
                return (*x, *y, *width, *height);
            }
        }
        let n = canvas.nodes.iter().find(|n| n.id == id).unwrap();
        (n.x, n.y, n.width, n.height)
    }

    #[test]
    fn empty_canvas_yields_no_ops() {
        assert!(auto_arrange(&Canvas::default(), ArrangeOpts::default()).is_empty());
    }

    #[test]
    fn diamond_dag_lands_in_distinct_ranks() {
        // 1->2, 1->3, 2->4, 3->4: a diamond. Scattered start positions.
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("1", 500, 90));
        canvas.nodes.push(leaf("2", -300, 800));
        canvas.nodes.push(leaf("3", 900, 40));
        canvas.nodes.push(leaf("4", 10, -500));
        canvas.edges.push(edge("e1", "1", "2"));
        canvas.edges.push(edge("e2", "1", "3"));
        canvas.edges.push(edge("e3", "2", "4"));
        canvas.edges.push(edge("e4", "3", "4"));

        let ops = auto_arrange(&canvas, ArrangeOpts::default());
        assert!(!ops.is_empty(), "a scattered diamond must produce moves");

        // TB hierarchy: source above the middles above the sink.
        let y1 = new_y(&ops, &canvas, "1");
        let y2 = new_y(&ops, &canvas, "2");
        let y3 = new_y(&ops, &canvas, "3");
        let y4 = new_y(&ops, &canvas, "4");
        assert!(y1 < y2, "source above middle 2: {y1} !< {y2}");
        assert!(y1 < y3, "source above middle 3: {y1} !< {y3}");
        assert!(y2 < y4, "middle 2 above sink: {y2} !< {y4}");
        assert!(y3 < y4, "middle 3 above sink: {y3} !< {y4}");

        // Leaves keep their own size.
        let (_, _, w, h) = rect_after(&ops, &canvas, "1");
        assert_eq!((w, h), (100, 60), "leaf size preserved");
    }

    #[test]
    fn left_to_right_ranks_increase_in_x() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("a", 0, 0));
        canvas.nodes.push(leaf("b", 0, 0));
        canvas.nodes.push(leaf("c", 0, 0));
        canvas.edges.push(edge("e1", "a", "b"));
        canvas.edges.push(edge("e2", "b", "c"));
        let opts = ArrangeOpts { rankdir: RankDirection::LeftToRight, ..ArrangeOpts::default() };
        let ops = auto_arrange(&canvas, opts);
        let xa = rect_after(&ops, &canvas, "a").0;
        let xb = rect_after(&ops, &canvas, "b").0;
        let xc = rect_after(&ops, &canvas, "c").0;
        assert!(xa < xb && xb < xc, "LR chain x must increase: {xa} {xb} {xc}");
    }

    #[test]
    fn group_membership_maps_children_to_parent_cluster() {
        // A group framing two leaves; the leaves sit inside its bounds.
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("a", 60, 60));
        canvas.nodes.push(leaf("b", 60, 200));
        canvas.nodes.push(group("g", 0, 0, 400, 400));
        canvas.edges.push(edge("e1", "a", "b"));

        let plan = ArrangePlan::build(&canvas).expect("non-empty");
        // leaves [a=0, b=1]; groups [g -> dagre 2].
        let parents = plan.node_parents(&canvas);
        assert_eq!(parents[0], Some(2), "leaf a parented to group g");
        assert_eq!(parents[1], Some(2), "leaf b parented to group g");
        assert_eq!(parents[2], None, "top-level group has no parent");
    }

    #[test]
    fn group_frame_wraps_its_members() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("a", 60, 60));
        canvas.nodes.push(leaf("b", 60, 200));
        canvas.nodes.push(group("g", 0, 0, 400, 400));
        canvas.edges.push(edge("e1", "a", "b"));

        let ops = auto_arrange(&canvas, ArrangeOpts::default());
        let (gx, gy, gw, gh) = rect_after(&ops, &canvas, "g");
        for id in ["a", "b"] {
            let (x, y, w, h) = rect_after(&ops, &canvas, id);
            assert!(x >= gx && y >= gy, "{id} top-left inside group frame");
            assert!(x + w <= gx + gw && y + h <= gy + gh, "{id} bottom-right inside group frame");
        }
    }

    #[test]
    fn deterministic_same_canvas_same_ops() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("1", 500, 90));
        canvas.nodes.push(leaf("2", -300, 800));
        canvas.nodes.push(leaf("3", 900, 40));
        canvas.nodes.push(leaf("4", 10, -500));
        canvas.edges.push(edge("e1", "1", "2"));
        canvas.edges.push(edge("e2", "1", "3"));
        canvas.edges.push(edge("e3", "2", "4"));
        canvas.edges.push(edge("e4", "3", "4"));
        let a = auto_arrange(&canvas, ArrangeOpts::default());
        let b = auto_arrange(&canvas, ArrangeOpts::default());
        assert_eq!(a, b, "auto_arrange must be deterministic");
    }

    #[test]
    fn edges_referencing_groups_or_unknowns_are_skipped() {
        let mut canvas = Canvas::default();
        canvas.nodes.push(leaf("a", 0, 0));
        canvas.nodes.push(leaf("b", 0, 0));
        canvas.nodes.push(group("g", -50, -50, 500, 500));
        canvas.edges.push(edge("e1", "a", "b")); // kept
        canvas.edges.push(edge("e2", "a", "g")); // group end -> skipped
        canvas.edges.push(edge("e3", "a", "ghost")); // unknown -> skipped
        let plan = ArrangePlan::build(&canvas).expect("non-empty");
        assert_eq!(plan.edges, vec![(0, 1)], "only the leaf->leaf edge survives mapping");
    }
}
