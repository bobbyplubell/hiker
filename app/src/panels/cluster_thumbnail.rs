//! Cluster-tree preview thumbnail: a force-directed dots-and-lines sketch of a
//! cluster tree, rendered to a flat SVG and rasterized through the shared
//! `render::rasterize_svg`.
//!
//! Honors the "force directed graph view" requirement, but runs a
//! DETERMINISTIC, seeded, FIXED-iteration *synchronous* pass — not the live
//! async converging `LayoutWorker` the cluster-graph panel uses — so the
//! thumbnail is stable frame-to-frame and its cache key is reproducible. A
//! deterministic PRNG seeds the scatter; `hiker_graph::force_layout` settles it
//! over a bounded iteration count; the bounding box is fit into the output
//! square. Node count is capped so a huge tree still renders cheaply.
//!
//! status: preview-tree-thumbnail

use hiker_core::trees::types::{EditableNode, NodeKind};

use crate::widgets::preview::{content_hash, PreviewKey, PreviewKind, ThumbnailProvider};

/// Cap on nodes laid out + drawn. A few hundred dots is plenty for a
/// recognizable thumbnail; beyond this the layout cost and visual clutter both
/// stop paying off. Deterministic: the first `MAX_NODES` in tree order.
const MAX_NODES: usize = 400;

/// Fixed iteration count for the synchronous layout pass — enough to settle a
/// few-hundred-node tree into a readable shape, bounded so the render stays
/// snappy. Determinism comes from this cap plus the seeded scatter.
const LAYOUT_ITERS: u32 = 300;

/// A cluster-tree thumbnail provider over a tree's resolved nodes. The nodes are
/// pre-loaded at the wiring site (from `trees.list_nodes`) so the trait's
/// `render` stays a pure pixel producer. The content hash is over the node
/// *shape* (id + parent, in order), so it churns when nodes are added / removed
/// / re-parented but not on summary / policy edits.
pub struct TreeThumbnail {
    /// `(node_index, parent_index, is_cluster)` triples in tree order.
    nodes: Vec<TreeNode>,
    raw_hash: u64,
}

struct TreeNode {
    parent: Option<usize>,
    cluster: bool,
}

impl TreeThumbnail {
    /// Build a provider from a tree's `EditableNode` slice. Resolves each
    /// node's parent to an index and records the shape hash.
    pub fn new(nodes: &[EditableNode]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        nodes.len().hash(&mut h);

        let take = nodes.len().min(MAX_NODES);
        let index_of: std::collections::HashMap<&str, usize> = nodes
            .iter()
            .take(take)
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();

        let mut out = Vec::with_capacity(take);
        for n in nodes.iter().take(take) {
            n.id.hash(&mut h);
            n.parent.hash(&mut h);
            let parent = n.parent.as_deref().and_then(|p| index_of.get(p).copied());
            out.push(TreeNode {
                parent,
                cluster: matches!(n.kind, NodeKind::Cluster | NodeKind::OutlierBucket),
            });
        }
        Self { nodes: out, raw_hash: h.finish() }
    }
}

impl ThumbnailProvider for TreeThumbnail {
    fn cache_key(&self) -> PreviewKey {
        PreviewKey {
            content_hash: content_hash(PreviewKind::Tree, self.raw_hash),
            kind: PreviewKind::Tree,
            size: 0,
        }
    }

    fn render(&self, px: u32) -> Option<image::RgbaImage> {
        if self.nodes.is_empty() {
            return None;
        }
        let positions = layout(&self.nodes);
        let svg = tree_svg(&self.nodes, &positions, px);
        let (rgba, w, h) = crate::panels::buffer::widgets::render::rasterize_svg(svg.as_bytes(), 1.0)?;
        image::RgbaImage::from_raw(w, h, rgba)
    }
}

/// Run the deterministic, seeded, fixed-iteration force layout. Returns one
/// settled position per node.
fn layout(nodes: &[TreeNode]) -> Vec<hiker_graph::Vec2> {
    // Deterministic scatter seed (same SplitMix-style PRNG the cluster-graph
    // panel uses), so the layout is reproducible across runs / devices.
    let mut rng_state: u64 = 0x517C_C1B7_2722_0A95;
    let mut rng = || {
        rng_state = rng_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((rng_state >> 33) as u32) as f32 / (u32::MAX as f32)
    };
    let seed: Vec<hiker_graph::Vec2> = (0..nodes.len())
        .map(|_| hiker_graph::Vec2::new((rng() - 0.5) * 80.0, (rng() - 0.5) * 80.0))
        .collect();
    let edges: Vec<(u32, u32)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.parent.map(|p| (p as u32, i as u32)))
        .collect();
    let params = hiker_graph::LayoutParams {
        max_iters: LAYOUT_ITERS,
        // Disable convergence early-out so the pass is a fixed iteration count
        // regardless of how fast it settles — keeps the result deterministic.
        convergence_eps: 0.0,
        bound: 50_000.0,
        ..hiker_graph::LayoutParams::default()
    };
    hiker_graph::force_layout(seed, &edges, &params)
}

/// Emit a flat SVG of the settled graph: a line per parent→child edge, a dot
/// per node (clusters larger + accented, leaves small), fit into a `px` square.
fn tree_svg(nodes: &[TreeNode], positions: &[hiker_graph::Vec2], px: u32) -> String {
    let px = px.max(1) as f32;
    let (lo, hi) = bounds(positions);
    let span_x = (hi.x - lo.x).max(1.0);
    let span_y = (hi.y - lo.y).max(1.0);
    // Leave a small margin so dots near the edge aren't clipped.
    let margin = px * 0.08;
    let usable = (px - margin * 2.0).max(1.0);
    let scale = (usable / span_x).min(usable / span_y);
    let out_w = (span_x * scale + margin * 2.0).round().max(1.0);
    let out_h = (span_y * scale + margin * 2.0).round().max(1.0);
    let map = |p: hiker_graph::Vec2| {
        ((p.x - lo.x) * scale + margin, (p.y - lo.y) * scale + margin)
    };

    let mut svg = String::new();
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{out_w}" height="{out_h}" viewBox="0 0 {out_w} {out_h}">"#,
    ));

    // Edges.
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = n.parent {
            if i < positions.len() && p < positions.len() {
                let (x1, y1) = map(positions[i]);
                let (x2, y2) = map(positions[p]);
                svg.push_str(&format!(
                    r##"<line x1="{x1:.1}" y1="{y1:.1}" x2="{x2:.1}" y2="{y2:.1}" stroke="#666" stroke-width="0.8"/>"##,
                ));
            }
        }
    }

    // Nodes.
    for (i, n) in nodes.iter().enumerate() {
        if i >= positions.len() {
            break;
        }
        let (cx, cy) = map(positions[i]);
        let (r, fill) = if n.cluster {
            (2.6_f32, "#7c9cff")
        } else {
            (1.6_f32, "#6cc674")
        };
        svg.push_str(&format!(
            r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="{r:.1}" fill="{fill}"/>"#,
        ));
    }

    svg.push_str("</svg>");
    svg
}

/// Axis-aligned bounds of the settled positions.
fn bounds(positions: &[hiker_graph::Vec2]) -> (hiker_graph::Vec2, hiker_graph::Vec2) {
    let mut lo = positions[0];
    let mut hi = positions[0];
    for &p in positions.iter().skip(1) {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, kind: NodeKind) -> EditableNode {
        EditableNode {
            id: id.into(),
            parent: parent.map(str::to_string),
            kind,
            note_path: None,
            name: String::new(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 0.0,
            summary_membership_churn: 0,
        }
    }

    fn sample() -> Vec<EditableNode> {
        vec![
            node("root", None, NodeKind::Cluster),
            node("c1", Some("root"), NodeKind::Cluster),
            node("l1", Some("c1"), NodeKind::Leaf),
            node("l2", Some("c1"), NodeKind::Leaf),
        ]
    }

    #[test]
    fn renders_non_empty_image() {
        let t = TreeThumbnail::new(&sample());
        let img = t.render(64).expect("renders");
        assert!(img.width() > 0 && img.height() > 0);
        assert!(img.width().max(img.height()) <= 64);
    }

    #[test]
    fn empty_tree_renders_none() {
        let t = TreeThumbnail::new(&[]);
        assert!(t.render(64).is_none());
    }

    #[test]
    fn layout_is_deterministic() {
        // Same nodes → identical settled positions (seeded + fixed iters).
        let a = layout(&TreeThumbnail::new(&sample()).nodes);
        let b = layout(&TreeThumbnail::new(&sample()).nodes);
        assert_eq!(a.len(), b.len());
        for (pa, pb) in a.iter().zip(&b) {
            assert!((pa.x - pb.x).abs() < 1e-4 && (pa.y - pb.y).abs() < 1e-4);
        }
    }

    #[test]
    fn shape_hash_ignores_summary_but_tracks_reparent() {
        let base = TreeThumbnail::new(&sample()).cache_key().content_hash;
        // Summary edit: same shape → same hash.
        let mut summ = sample();
        summ[2].summary = "changed".into();
        assert_eq!(TreeThumbnail::new(&summ).cache_key().content_hash, base);
        // Re-parent l2 under root: shape changes → hash changes.
        let mut moved = sample();
        moved[3].parent = Some("root".into());
        assert_ne!(TreeThumbnail::new(&moved).cache_key().content_hash, base);
    }

    #[test]
    fn caps_node_count() {
        let many: Vec<EditableNode> = (0..MAX_NODES + 50)
            .map(|i| node(&format!("n{i}"), Some("n0").filter(|_| i != 0), NodeKind::Leaf))
            .collect();
        // n0 has no parent (root); the rest hang off it. Capped to MAX_NODES.
        let t = TreeThumbnail::new(&many);
        assert_eq!(t.nodes.len(), MAX_NODES);
    }
}
