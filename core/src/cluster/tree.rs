//! Online placement against a saved cluster tree. Beam-width-K descent
//! from the root, scoring each candidate node by cosine similarity
//! against the (L2-normalized) query. Per `cluster-place-beam-descent`.

use super::algo::{cosine_similarity, l2_normalize};
use super::{NodeId, PlacementMatch, TreeView};

/// Beam-width-K descent over a saved cluster tree. K=2 by default per
/// `cluster-place-beam-descent`; K=1 reduces to greedy ("the cheap
/// fallback"); K≥3 is robust but rarely needed at vault scale.
///
/// `query_embedding` is L2-normalized on entry; the tree's centroids
/// are expected to be normalized at construction time. The classifier
/// is pure cosine — no LLM, no tool calls — and runs in
/// `O(K · branching · depth)` similarities, which is microseconds at
/// vault scale.
///
/// Returns `None` only when the tree is empty (no root node resolvable).
///
/// status: cluster-place-beam-descent
pub fn place_beam_descent(
    query_embedding: &[f32],
    tree: &dyn TreeView,
    beam_width: usize,
) -> Option<PlacementMatch> {
    let beam_width = beam_width.max(1);
    let q = l2_normalize(query_embedding);
    let root = tree.get(tree.root())?;

    // Beam of (node_id, score). Score is the cosine of the path's last
    // centroid; we use it to keep the top-K across levels.
    let mut beam: Vec<(NodeId, f32)> = vec![(root.id.clone(), cosine_similarity(&q, &root.centroid))];

    loop {
        // If every node in the beam is a leaf, we're done.
        let any_internal = beam.iter().any(|(id, _)| {
            tree.get(id)
                .map(|n| !n.children.is_empty())
                .unwrap_or(false)
        });
        if !any_internal {
            break;
        }

        // Expand: replace each internal node with its top-K children.
        // Leaves stay in the beam as-is so the descent can terminate
        // with a mixed-depth set of candidates (matches RAPTOR's
        // tree-traversal mode).
        let mut expanded: Vec<(NodeId, f32)> = Vec::new();
        for (id, prev_score) in &beam {
            let Some(node) = tree.get(id) else { continue };
            if node.children.is_empty() {
                expanded.push((id.clone(), *prev_score));
                continue;
            }
            let mut child_scores: Vec<(NodeId, f32)> = node
                .children
                .iter()
                .filter_map(|cid| tree.get(cid).map(|c| (cid.clone(), cosine_similarity(&q, &c.centroid))))
                .collect();
            child_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            child_scores.truncate(beam_width);
            expanded.extend(child_scores);
        }
        if expanded.is_empty() {
            break;
        }
        expanded.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        expanded.truncate(beam_width);
        beam = expanded;
    }

    // Final beam → leaves; sort once more so the top-1 vs top-2 margin
    // is meaningful even when the beam picked siblings at different
    // depths. Margin against an empty top-2 falls back to 0.0.
    beam.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (best_id, best_score) = beam.first()?.clone();
    let margin = beam.get(1).map(|(_, s)| best_score - s).unwrap_or(0.0);

    Some(PlacementMatch {
        leaf_node_id: best_id,
        confidence: best_score,
        margin,
    })
}
