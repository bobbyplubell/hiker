//! Warm-start seeding + adaptive anchoring for the force-directed layout.
//!
//! When a force graph is re-solved after a clustering-parameter change, this
//! module decides how the new layout relates to the old one. The core trade-off:
//!
//! * A **small** change (membership barely moves) should morph smoothly — keep
//!   retained nodes where they were and tether them there (anchor springs), so
//!   the user sees the layout *adjust* rather than reshuffle.
//! * A **big** structural re-clustering must NOT be pinned to the old shape:
//!   tethering retained nodes to positions that fight the new edge structure
//!   leaves the layout badly tangled. Such a change should relax toward a fresh
//!   solve.
//!
//! [`change_fraction`] measures how structurally the graph changed (retained
//! nodes whose neighbour set differs, plus new nodes), and both the anchor
//! stiffness ([`adaptive_anchor_stiffness`]) and the warm seed's grip
//! ([`build_warm_seed`]'s relax) scale down with it — reaching a fully fresh
//! layout at [`RELAX_FULL_AT`]. The tangle this guards against is verified
//! numerically by `hiker_graph::edge_crossings` in the force-layout tests and
//! the `anchor_tangle` example.

use std::collections::HashMap;

use super::Source;

/// Random scatter seed of `n` points in a `box_size`-wide box centered on
/// the origin. Deterministic LCG — the force layout converges from any
/// start, so a fixed seed keeps frames reproducible.
pub(super) fn scatter(n: usize, box_size: f32) -> Vec<egui::Vec2> {
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

/// A single deterministic scatter point for node `i` in a `box_size`-wide box
/// centred on the origin. Hashes `i` through the same LCG `scatter` uses so the
/// result is reproducible per index (an unchanged graph yields identical
/// seeds), without materialising the whole `n`-length scatter.
fn scatter_point(i: usize, box_size: f32) -> egui::Vec2 {
    let mut s = 0x9E37_79B9_7F4A_7C15u64 ^ (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
    let mut rng = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((s >> 33) as u32) as f32 / (u32::MAX as f32)
    };
    egui::vec2((rng() - 0.5) * box_size, (rng() - 0.5) * box_size)
}

/// Change fraction at or above which anchoring is fully off and the warm seed
/// is fully relaxed to a fresh scatter — i.e. a re-clustering this big is
/// treated as a fresh layout. Below it, anchoring/seed-grip scale linearly. At
/// 0.5, "half the nodes restructured" already means "lay it out fresh", which
/// matches the user-reported failure (complex re-clustering tangles) without
/// touching small-scrub coherence.
const RELAX_FULL_AT: f32 = 0.5;

/// Effective anchor stiffness for a warm force rebuild: the user's
/// `baseline` (their slider value = the MAX) scaled down linearly by how much
/// the graph *structurally* changed, hitting 0 at [`RELAX_FULL_AT`]. A small
/// clustering scrub keeps near-full stiffness (smooth morph); a big
/// re-clustering relaxes to ~0 so retained nodes aren't tethered to stale
/// positions that fight the new edges and leave the layout tangled.
pub(super) fn adaptive_anchor_stiffness(baseline: f32, change_fraction: f32) -> f32 {
    let factor = (1.0 - change_fraction / RELAX_FULL_AT).clamp(0.0, 1.0);
    baseline * factor
}

/// Fraction of the new graph's nodes whose local structure changed since the
/// previous force layout. A retained node (its key is in `prev_adjacency`)
/// counts as changed when its current neighbour-key set differs from the
/// recorded one; a new node (key absent) always counts. `0` = identical wiring
/// (pure param scrub that didn't move membership), approaching `1` = almost
/// everything rewired. Returns `0` for an empty graph or when no prior wiring
/// was recorded (first warm rebuild → behave like a small change).
pub(super) fn change_fraction(
    source: &dyn Source,
    prev_adjacency: &HashMap<String, Vec<String>>,
    edges: &[(u32, u32)],
    n: usize,
) -> f32 {
    if n == 0 || prev_adjacency.is_empty() {
        return 0.0;
    }
    // Current neighbour-key set per index (sorted+deduped to match the recorded
    // form), so a retained node's wiring can be compared key-for-key.
    let mut nbr_keys: Vec<Vec<String>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        let (a, b) = (a as usize, b as usize);
        if a >= n || b >= n || a == b {
            continue;
        }
        if let Some(kb) = source.node_key(b) {
            nbr_keys[a].push(kb);
        }
        if let Some(ka) = source.node_key(a) {
            nbr_keys[b].push(ka);
        }
    }
    let mut changed = 0usize;
    for (i, keys) in nbr_keys.iter_mut().enumerate() {
        keys.sort_unstable();
        keys.dedup();
        match source.node_key(i).and_then(|k| prev_adjacency.get(&k)) {
            // Retained: changed iff its neighbour set differs.
            Some(prior) if prior == keys => {}
            // Retained-but-rewired, or brand new (no prior entry).
            _ => changed += 1,
        }
    }
    changed as f32 / n as f32
}

/// Build the warm-start `(seed, anchors)` for a force rebuild from the prior
/// positions, mapping old → new by [`Source::node_key`]:
///
/// - A **retained** node (its key is in `prev`) seeds at, and is anchored to,
///   its prior position — the anchor spring holds it roughly in place.
/// - A **new** node (no key, or a key absent from `prev`) gets no anchor
///   (`None`, so it settles freely) and seeds at the centroid of its
///   already-placed neighbours (neighbours from `edges`, located in `prev` via
///   *their* key) plus a small deterministic jitter; with no placed neighbour
///   it falls back to a deterministic [`scatter_point`].
///
/// Deterministic throughout: identical inputs yield identical seeds.
pub(super) fn build_warm_seed(
    source: &dyn Source,
    prev: &HashMap<String, egui::Vec2>,
    edges: &[(u32, u32)],
    n: usize,
    seed_box: f32,
    change_fraction: f32,
) -> (Vec<egui::Vec2>, Vec<Option<egui::Vec2>>) {
    // How far to relax retained seeds toward a fresh scatter, by structural
    // change: 0 (none) for a small scrub, ramping to 1 (fully fresh) at
    // `RELAX_FULL_AT`. A big re-clustering needs its retained nodes free to find
    // the untangled equilibrium, not just un-tethered from a stale start — the
    // warm seed alone, even with no anchor, biases FA2 into the old (now
    // tangled) basin. (Anchors are dropped to 0 in lock-step by
    // `adaptive_anchor_stiffness`, so a fully relaxed seed gets no spring either.)
    let relax = (change_fraction / RELAX_FULL_AT).clamp(0.0, 1.0);
    // Prior position of a node by index, via its stable key — the lookup new
    // nodes use to find their already-placed neighbours.
    let prior = |i: usize| -> Option<egui::Vec2> {
        source.node_key(i).and_then(|k| prev.get(&k).copied())
    };

    // Undirected neighbour lists, so a new node can average whichever of its
    // neighbours already carry a prior position.
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        let (a, b) = (a as usize, b as usize);
        if a < n && b < n && a != b {
            neighbours[a].push(b);
            neighbours[b].push(a);
        }
    }

    let mut seed = Vec::with_capacity(n);
    let mut anchors = Vec::with_capacity(n);
    for (i, nbrs) in neighbours.iter().enumerate() {
        if let Some(p) = prior(i) {
            // Retained: seed at its prior position (blended toward a fresh
            // scatter by `relax` for big changes) and tether to it. The anchor
            // target stays the true prior position — its *strength* is what the
            // adaptive policy scales — so a small change still homes exactly
            // there, while a big change both starts free and is barely pulled.
            let seeded = if relax > 0.0 {
                let s = scatter_point(i, seed_box);
                p + (s - p) * relax
            } else {
                p
            };
            seed.push(seeded);
            anchors.push(Some(p));
            continue;
        }
        // New node: seed at its placed-neighbour centroid + deterministic
        // jitter, or a deterministic scatter point if it has none.
        let mut sum = egui::Vec2::ZERO;
        let mut count = 0u32;
        for &nb in nbrs {
            if let Some(p) = prior(nb) {
                sum += p;
                count += 1;
            }
        }
        let pos = if count > 0 {
            // Jitter scaled small relative to the seed box so neighbours don't
            // stack exactly; `scatter_point` keeps it index-deterministic.
            sum / count as f32 + scatter_point(i, seed_box * 0.05)
        } else {
            scatter_point(i, seed_box)
        };
        seed.push(pos);
        anchors.push(None);
    }
    (seed, anchors)
}

#[cfg(test)]
mod warm_seed_tests {
    use super::super::{LayoutKind, LayoutTree, NodeDescriptor, Style};
    use super::*;

    /// Minimal [`Source`] for the warm-seed helper: each node's key is its
    /// index as a string, so `prev` lookups are trivial to set up. Only
    /// `node_key` / `node_count` matter here.
    struct KeyedSource(usize);

    impl Source for KeyedSource {
        fn node_count(&self) -> usize {
            self.0
        }
        fn nodes(&self, _positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
            Vec::new()
        }
        fn edges(&self) -> Vec<(u32, u32)> {
            Vec::new()
        }
        fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
            LayoutTree::from_parents(&[])
        }
        fn preview_for(&self, _index: usize) -> Option<(String, String)> {
            None
        }
        fn node_key(&self, index: usize) -> Option<String> {
            Some(index.to_string())
        }
    }

    /// A retained node (key present in `prev`) seeds exactly at its prior
    /// position and is anchored there.
    #[test]
    fn retained_node_seeds_and_anchors_at_prior() {
        let src = KeyedSource(1);
        let mut prev = HashMap::new();
        let p = egui::vec2(12.0, -7.0);
        prev.insert("0".to_string(), p);
        let (seed, anchors) = build_warm_seed(&src, &prev, &[], 1, 1000.0, 0.0);
        assert_eq!(seed[0], p);
        assert_eq!(anchors[0], Some(p));
    }

    /// A new node (key absent) with one placed neighbour gets no anchor and
    /// seeds near that neighbour's prior position (within the small jitter).
    #[test]
    fn new_node_seeds_near_placed_neighbour() {
        // Two nodes, edge 0—1. Node 0 retained, node 1 new.
        let src = KeyedSource(2);
        let mut prev = HashMap::new();
        let p0 = egui::vec2(100.0, 200.0);
        prev.insert("0".to_string(), p0);
        let edges = [(0u32, 1u32)];
        let seed_box = 1000.0;
        let (seed, anchors) = build_warm_seed(&src, &prev, &edges, 2, seed_box, 0.0);
        assert_eq!(anchors[0], Some(p0));
        assert_eq!(anchors[1], None, "new node must not be anchored");
        // Jitter is bounded by seed_box * 0.05 * 0.5 per axis.
        let jitter = seed_box * 0.05 * 0.5;
        assert!(
            (seed[1] - p0).length() <= (jitter * std::f32::consts::SQRT_2) + 1e-3,
            "new node {:?} not near neighbour {:?}",
            seed[1],
            p0
        );
    }

    /// A new node with no placed neighbour gets no anchor and a finite seed
    /// inside the scatter box.
    #[test]
    fn new_node_no_neighbour_falls_back_in_range() {
        let src = KeyedSource(1);
        let prev = HashMap::new(); // nothing placed
        let seed_box = 1000.0;
        let (seed, anchors) = build_warm_seed(&src, &prev, &[], 1, seed_box, 0.0);
        assert_eq!(anchors[0], None);
        assert!(seed[0].x.is_finite() && seed[0].y.is_finite());
        let half = seed_box * 0.5;
        assert!(seed[0].x.abs() <= half && seed[0].y.abs() <= half);
    }

    /// Identical inputs yield identical seeds + anchors — an unchanged graph
    /// reproduces its layout exactly.
    #[test]
    fn build_warm_seed_is_deterministic() {
        let src = KeyedSource(4);
        let mut prev = HashMap::new();
        prev.insert("0".to_string(), egui::vec2(10.0, 10.0));
        prev.insert("2".to_string(), egui::vec2(-30.0, 5.0));
        let edges = [(0u32, 1u32), (2u32, 3u32)];
        let a = build_warm_seed(&src, &prev, &edges, 4, 800.0, 0.0);
        let b = build_warm_seed(&src, &prev, &edges, 4, 800.0, 0.0);
        assert_eq!(a.0, b.0, "seeds diverged");
        assert_eq!(a.1, b.1, "anchors diverged");
    }

    /// A retained node's seed is blended toward a fresh scatter as
    /// `change_fraction` rises: at 0 it stays exactly at its prior position, and
    /// at `RELAX_FULL_AT` it sits exactly on its scatter point.
    #[test]
    fn retained_seed_relaxes_with_change_fraction() {
        let src = KeyedSource(1);
        let mut prev = HashMap::new();
        let p = egui::vec2(40.0, -10.0);
        prev.insert("0".to_string(), p);
        let box_size = 1000.0;

        // No change → no relax → exactly the prior position.
        let (seed0, _) = build_warm_seed(&src, &prev, &[], 1, box_size, 0.0);
        assert_eq!(seed0[0], p);

        // Full change → fully relaxed → exactly the scatter point. The anchor
        // target stays the true prior position regardless.
        let (seed1, anchors1) = build_warm_seed(&src, &prev, &[], 1, box_size, RELAX_FULL_AT);
        assert_eq!(seed1[0], scatter_point(0, box_size));
        assert_eq!(anchors1[0], Some(p), "anchor target must stay the prior pos");
    }
}

#[cfg(test)]
mod adaptive_anchor_tests {
    use super::super::{LayoutKind, LayoutTree, NodeDescriptor, Style};
    use super::*;

    /// A [`Source`] with a fixed node count and an explicit edge list, keyed by
    /// index-as-string, so `change_fraction` / `capture_adjacency` can be
    /// exercised over a controllable wiring.
    struct WiredSource {
        n: usize,
        edges: Vec<(u32, u32)>,
    }

    impl Source for WiredSource {
        fn node_count(&self) -> usize {
            self.n
        }
        fn nodes(&self, _positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
            Vec::new()
        }
        fn edges(&self) -> Vec<(u32, u32)> {
            self.edges.clone()
        }
        fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
            LayoutTree::from_parents(&[])
        }
        fn preview_for(&self, _index: usize) -> Option<(String, String)> {
            None
        }
        fn node_key(&self, index: usize) -> Option<String> {
            Some(index.to_string())
        }
    }

    /// `adaptive_anchor_stiffness` keeps near-full stiffness for a small change,
    /// reaches 0 at `RELAX_FULL_AT`, and stays clamped at 0 beyond it.
    #[test]
    fn adaptive_stiffness_scales_and_clamps() {
        let base = 0.2;
        assert!((adaptive_anchor_stiffness(base, 0.0) - base).abs() < 1e-6);
        // Halfway to the cutoff → half stiffness.
        let mid = adaptive_anchor_stiffness(base, RELAX_FULL_AT * 0.5);
        assert!((mid - base * 0.5).abs() < 1e-6, "mid {mid}");
        assert!(adaptive_anchor_stiffness(base, RELAX_FULL_AT) < 1e-6);
        assert!(adaptive_anchor_stiffness(base, 0.9) < 1e-6, "clamped at 0");
    }

    /// `change_fraction` is 0 for identical wiring, ~1 when (almost) every
    /// node's neighbour set differs, and counts brand-new nodes as changed.
    #[test]
    fn change_fraction_measures_rewiring() {
        // Path graph 0-1-2-3.
        let edges_a = vec![(0u32, 1u32), (1, 2), (2, 3)];
        let src_a = WiredSource { n: 4, edges: edges_a.clone() };

        // Capture A's adjacency by hand (same logic as capture_adjacency).
        let key_neighbours = |edges: &[(u32, u32)], n: usize| -> HashMap<String, Vec<String>> {
            let mut nbr: Vec<Vec<String>> = vec![Vec::new(); n];
            for &(a, b) in edges {
                let (a, b) = (a as usize, b as usize);
                if a < n && b < n && a != b {
                    nbr[a].push(b.to_string());
                    nbr[b].push(a.to_string());
                }
            }
            let mut map = HashMap::new();
            for (i, mut v) in nbr.into_iter().enumerate() {
                v.sort_unstable();
                v.dedup();
                map.insert(i.to_string(), v);
            }
            map
        };
        let prev = key_neighbours(&edges_a, 4);

        // Identical wiring → zero change.
        let cf_same = change_fraction(&src_a, &prev, &edges_a, 4);
        assert!(cf_same.abs() < 1e-6, "identical wiring should be 0, got {cf_same}");

        // Fully rewire into a different path 0-2-1-3 (every interior node's
        // neighbour set changes) and add a 5th node → big change.
        let edges_b = vec![(0u32, 2u32), (2, 1), (1, 3), (4, 0)];
        let src_b = WiredSource { n: 5, edges: edges_b.clone() };
        let cf_big = change_fraction(&src_b, &prev, &edges_b, 5);
        assert!(cf_big >= 0.5, "expected a big change fraction, got {cf_big}");
    }

    /// Empty graph and missing-prior cases degrade to 0 (treat as small).
    #[test]
    fn change_fraction_degenerate_is_zero() {
        let src = WiredSource { n: 0, edges: vec![] };
        assert_eq!(change_fraction(&src, &HashMap::new(), &[], 0), 0.0);
        let src2 = WiredSource { n: 3, edges: vec![(0, 1)] };
        // No prior adjacency recorded → 0 (first warm rebuild behaves small).
        assert_eq!(change_fraction(&src2, &HashMap::new(), &src2.edges, 3), 0.0);
    }

    /// Small-change coherence (don't regress the 8a win): when only a couple of
    /// nodes change, the adaptive policy keeps near-full stiffness AND a tight
    /// (un-relaxed) warm seed, so retained nodes' seeds stay essentially at
    /// their prior positions. This is the property that makes a param scrub a
    /// smooth morph rather than a reshuffle.
    #[test]
    fn small_change_preserves_coherence() {
        // 10 retained nodes at known prior positions; a tiny change fraction.
        let n = 10usize;
        let src = WiredSource { n, edges: vec![] };
        let mut prev = HashMap::new();
        let priors: Vec<egui::Vec2> = (0..n)
            .map(|i| egui::vec2(i as f32 * 30.0, (i as f32).sin() * 50.0))
            .collect();
        for (i, p) in priors.iter().enumerate() {
            prev.insert(i.to_string(), *p);
        }

        // change_fraction = 0.1 (one in ten) → small. Stiffness stays high.
        let change = 0.1f32;
        let stiffness = adaptive_anchor_stiffness(0.2, change);
        assert!(stiffness > 0.15, "small change must keep high stiffness, got {stiffness}");

        // Warm seed: retained-node seeds must barely drift from their priors.
        let (seed, anchors) = build_warm_seed(&src, &prev, &[], n, 1000.0, change);
        let mean_drift: f32 = seed
            .iter()
            .zip(&priors)
            .map(|(s, p)| (*s - *p).length())
            .sum::<f32>()
            / n as f32;
        // relax = 0.1/0.5 = 0.2; with priors spanning ~270 units and scatter in
        // a 1000-box, keep the bound generous but well under a reshuffle.
        assert!(
            mean_drift < 120.0,
            "small-change retained-seed drift {mean_drift} too large (coherence lost)"
        );
        // Anchors still target the exact priors.
        for (i, a) in anchors.iter().enumerate() {
            assert_eq!(*a, Some(priors[i]));
        }
    }
}
