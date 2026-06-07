//! Pure clustering math + algorithm wrappers. HDBSCAN + Leiden
//! partitioners plus the cosine / L2 / mean / radius helpers used by the
//! build pipeline and the placement classifier. No IO, no storage, no
//! tracing — keeps the math swappable per `cluster-module-discipline`.

use ndarray::Array2;
use petal_clustering::{Fit, HDbscan};
use single_clustering::community_search::leiden::partition::RBConfigurationPartition;
use single_clustering::community_search::leiden::{LeidenConfig, LeidenOptimizer};
use single_clustering::network::CSRNetwork;
use single_clustering::network::grouping::VectorGrouping;

use super::{Assignment, Error, LeidenParams, OUTLIER_LABEL};

/// HDBSCAN over a slice of pre-normalized embeddings. The crate operates
/// on Euclidean distance by default — we L2-normalize once on entry so
/// Euclidean is monotonic with cosine distance, which is what the spec
/// asks for ("cosine distance via pre-normalized embeddings" —
/// `cluster-hdbscan-crate-petal`).
///
/// `min_samples = None` defaults to `min_cluster_size` per `clustering.md`.
///
/// Returns one `Assignment` per input point; order matches the
/// input slice. Outliers carry `cluster_label = OUTLIER_LABEL`.
pub fn partition(
    embeddings: &[Vec<f32>],
    min_cluster_size: usize,
    min_samples: Option<usize>,
) -> Result<Vec<Assignment>, Error> {
    if embeddings.is_empty() {
        return Err(Error::Empty);
    }
    let dim = embeddings[0].len();
    for (i, row) in embeddings.iter().enumerate() {
        if row.len() != dim {
            return Err(Error::DimMismatch {
                row: i,
                expected: dim,
                got: row.len(),
            });
        }
    }

    let n = embeddings.len();
    // petal-clustering 0.13's MST panics with a slice-length mismatch
    // when `n` is below `min_samples` (it builds a k-NN graph with
    // k = min_samples). Short-circuit: with too few points to form a
    // cluster of the requested size, label everything as an outlier
    // and let the caller treat the result as "nothing to cluster."
    let effective_min_samples = min_samples.unwrap_or(min_cluster_size);
    if n < min_cluster_size || n < effective_min_samples {
        let mut out: Vec<Assignment> = Vec::with_capacity(n);
        for i in 0..n {
            out.push(Assignment {
                point_index: i,
                cluster_label: OUTLIER_LABEL,
            });
        }
        return Ok(out);
    }

    // L2-normalize into an f64 ndarray. `petal-clustering` is generic
    // over the scalar type; we use f64 because that's its tested path
    // and the cost of the up-cast is negligible at this scale.
    let mut arr = Array2::<f64>::zeros((n, dim));
    for (i, row) in embeddings.iter().enumerate() {
        let mut sumsq = 0.0f64;
        for &v in row {
            sumsq += (v as f64) * (v as f64);
        }
        let norm = sumsq.sqrt().max(f64::EPSILON);
        for (j, &v) in row.iter().enumerate() {
            arr[(i, j)] = (v as f64) / norm;
        }
    }

    let mut hdb: HDbscan<f64, _> = HDbscan {
        min_cluster_size,
        min_samples: min_samples.unwrap_or(min_cluster_size),
        ..Default::default()
    };
    let (clusters, outliers, _outlier_scores) = hdb.fit(&arr, None);

    let mut out = vec![
        Assignment {
            point_index: 0,
            cluster_label: OUTLIER_LABEL,
        };
        n
    ];
    for (i, item) in out.iter_mut().enumerate() {
        item.point_index = i;
    }
    for (label, members) in &clusters {
        let label_i32 = i32::try_from(*label).unwrap_or(i32::MAX);
        for &m in members {
            if m < n {
                out[m].cluster_label = label_i32;
            }
        }
    }
    for &m in &outliers {
        if m < n {
            out[m].cluster_label = OUTLIER_LABEL;
        }
    }
    Ok(out)
}

/// A built kNN cosine-similarity graph, ready for Leiden community
/// detection. Per `cluster-leiden-knn-graph`.
///
/// The split between *building* this graph and *detecting communities* on
/// it (`build_leiden_graph` vs `leiden_communities`) is deliberate: the
/// build is the expensive O(n²) kNN sweep and depends only on the
/// embeddings, `k_nearest`, and `edge_weight_floor`; community detection
/// is comparatively cheap and varies `resolution` (γ) / `iterations` /
/// `min_cluster_size`. Keeping the graph lets a caller re-tune γ over the
/// *same* graph without paying the kNN cost again — used by the build
/// recipe's resolution-escalation retry and by the review tab's
/// live-preview cache (`cluster-review-tab-live-preview`).
///
/// Holds the deduped edge list (not a prebuilt `CSRNetwork`) so it stays
/// trivially `Clone`/`Send`/`Sync` for caching across the async build
/// boundary; rebuilding the CSR from edges is O(E) and negligible next to
/// the kNN sweep.
#[derive(Clone)]
pub struct LeidenGraph {
    n: usize,
    edges: Vec<(usize, usize, f64)>,
    /// The (clamped) `k_nearest` and `edge_weight_floor` this graph was
    /// built with — so a caller reusing a cached graph can confirm it
    /// still matches the requested params (`matches`).
    k_nearest: u32,
    edge_weight_floor: f32,
}

impl std::fmt::Debug for LeidenGraph {
    /// Concise — the edge list can be large, so summarize rather than dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeidenGraph")
            .field("n", &self.n)
            .field("edges", &self.edges.len())
            .field("k_nearest", &self.k_nearest)
            .field("edge_weight_floor", &self.edge_weight_floor)
            .finish()
    }
}

impl LeidenGraph {
    /// Node count (== input embedding count).
    pub const fn len(&self) -> usize {
        self.n
    }

    pub const fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// True when this graph was built from `n` points at the given
    /// `k_nearest` / `edge_weight_floor`, so it can be reused instead of
    /// rebuilt. `k_nearest` is compared after the same `n-1` clamp the
    /// builder applies, so a pre-clamped caller still matches.
    pub fn matches(&self, n: usize, k_nearest: u32, edge_weight_floor: f32) -> bool {
        self.n == n
            && self.k_nearest == clamp_k(k_nearest, n)
            && self.edge_weight_floor.to_bits() == edge_weight_floor.to_bits()
    }
}

/// Clamp a requested `k_nearest` to `[1, n-1]` so the kNN build never asks
/// for more neighbors than exist. Shared by `build_leiden_graph` and
/// `LeidenGraph::matches` so cache validation lines up with how the graph
/// was actually built.
fn clamp_k(k_nearest: u32, n: usize) -> u32 {
    (k_nearest as usize).min(n.saturating_sub(1)).max(1) as u32
}

/// Build the kNN cosine-similarity graph (the expensive half of Leiden).
/// Per `cluster-leiden-knn-graph`:
///
/// 1. L2-normalize the input embeddings (so cosine = dot product).
/// 2. For each point, find its top-`k_nearest` neighbors by cosine
///    similarity (brute-force O(n²); fine at personal-vault scale).
/// 3. Drop neighbor edges with weight < `edge_weight_floor`.
/// 4. Symmetrize on insertion (dedup on `(min, max)`), keeping the score
///    from whichever direction's kNN found the pair.
pub fn build_leiden_graph(
    embeddings: &[Vec<f32>],
    k_nearest: u32,
    edge_weight_floor: f32,
) -> Result<LeidenGraph, Error> {
    if embeddings.is_empty() {
        return Err(Error::Empty);
    }
    let dim = embeddings[0].len();
    for (i, row) in embeddings.iter().enumerate() {
        if row.len() != dim {
            return Err(Error::DimMismatch {
                row: i,
                expected: dim,
                got: row.len(),
            });
        }
    }

    let n = embeddings.len();
    let k = clamp_k(k_nearest, n) as usize;
    let floor = edge_weight_floor;

    // L2-normalize once so cosine reduces to dot product.
    let normed: Vec<Vec<f32>> = embeddings.iter().map(|v| l2_normalize(v)).collect();

    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    for i in 0..n {
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(n.saturating_sub(1));
        for j in 0..n {
            if i == j {
                continue;
            }
            let s = cosine_similarity(&normed[i], &normed[j]);
            scored.push((j, s));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        for (j, w) in scored {
            if w < floor {
                continue;
            }
            let key = if i < j { (i, j) } else { (j, i) };
            if seen.insert(key) {
                edges.push((i, j, w as f64));
            }
        }
    }

    Ok(LeidenGraph {
        n,
        edges,
        k_nearest: clamp_k(k_nearest, n),
        edge_weight_floor: floor,
    })
}

/// Detect communities on a prebuilt `LeidenGraph` (the cheap half). Runs
/// Leiden over a Reichardt-Bornholdt configuration partition: `resolution`
/// (γ) biases toward finer (γ > 1) or coarser (γ < 1) communities, γ=1.0
/// being the modularity equivalent. The optimized partition's
/// `membership(node)` gives a community id per node; communities smaller
/// than `min_cluster_size` are flagged as outliers and the survivors get
/// densified labels `0..num_communities`.
///
/// Returns one `Assignment` per node, in node order.
/// `cluster_label = OUTLIER_LABEL` for noise (small communities, or — when
/// the graph has no edges or fewer nodes than `min_cluster_size` — every
/// node). Cheap enough to call repeatedly over one graph while sweeping γ.
pub fn leiden_communities(
    graph: &LeidenGraph,
    resolution: f32,
    iterations: u32,
    min_cluster_size: u32,
) -> Result<Vec<Assignment>, Error> {
    let n = graph.n;
    let mut out: Vec<Assignment> = (0..n)
        .map(|i| Assignment {
            point_index: i,
            cluster_label: OUTLIER_LABEL,
        })
        .collect();

    let min_size = min_cluster_size.max(1) as usize;
    // Below the size floor, or with no edges (every node a singleton —
    // `single-clustering`'s Leiden also asserts a non-trivial graph),
    // everything is an outlier.
    if n < min_size || graph.edges.is_empty() {
        return Ok(out);
    }

    let node_weights: Vec<f64> = vec![1.0; n];
    let network: CSRNetwork<f64, f64> = CSRNetwork::from_edges(&graph.edges, node_weights);

    let config = LeidenConfig {
        max_iterations: (iterations as usize).max(1),
        seed: Some(0),
        ..LeidenConfig::default()
    };
    let mut optimizer = LeidenOptimizer::new(config);
    let resolution = resolution.max(0.0) as f64;
    let mut partition: RBConfigurationPartition<f64, VectorGrouping> =
        RBConfigurationPartition::with_resolution(network, resolution);
    optimizer
        .optimize_single_partition(&mut partition, None)
        .map_err(|e| Error::Leiden(e.to_string()))?;

    // Group nodes by membership; communities below the size floor stay
    // OUTLIER_LABEL. Densify the surviving community ids so consumers
    // see contiguous labels 0..num_communities. Sort by raw community id
    // for stable output across runs given the fixed seed.
    let mut by_comm: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for node in 0..n {
        let comm = partition.membership(node);
        by_comm.entry(comm).or_default().push(node);
    }
    let mut next_label: i32 = 0;
    let mut comm_ids: Vec<usize> = by_comm.keys().copied().collect();
    comm_ids.sort_unstable();
    for cid in comm_ids {
        let members = by_comm.remove(&cid).expect("present");
        if members.len() < min_size {
            continue;
        }
        for m in members {
            if m < n {
                out[m].cluster_label = next_label;
            }
        }
        next_label += 1;
    }

    Ok(out)
}

/// Leiden community detection over a freshly-built kNN cosine-similarity
/// graph. Convenience wrapper composing `build_leiden_graph` +
/// `leiden_communities` for callers that don't reuse the graph. Per
/// `cluster-leiden`.
pub fn partition_leiden(
    embeddings: &[Vec<f32>],
    leiden: &LeidenParams,
) -> Result<Vec<Assignment>, Error> {
    let graph = build_leiden_graph(embeddings, leiden.k_nearest, leiden.edge_weight_floor)?;
    leiden_communities(
        &graph,
        leiden.resolution,
        leiden.iterations,
        leiden.min_cluster_size,
    )
}

/// L2-normalize a single embedding in place, returning the normalized
/// copy. Public so the placement classifier (which compares an in-flight
/// query against pre-normalized centroids on disk) can apply the same
/// transform before scoring.
pub fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let mut sumsq = 0.0f64;
    for &x in v {
        sumsq += (x as f64) * (x as f64);
    }
    let norm = sumsq.sqrt().max(f64::EPSILON) as f32;
    v.iter().map(|&x| x / norm).collect()
}

/// Cosine similarity. Assumes neither argument is zero-length; for
/// zero-norm vectors returns 0.0. Public so consumers can score their
/// own pairs (the beam-descent classifier and the outlier-threshold
/// check both call it).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine_similarity dim mismatch");
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

// ── Math helpers ─────────────────────────────────────────────────────

/// Mean across `rows`, then L2-normalized. Used as the centroid for a
/// cluster across the build pipeline + the ops-framework Split/Rollup.
pub fn mean_normalize(rows: &[&[f32]]) -> Vec<f32> {
    if rows.is_empty() {
        return Vec::new();
    }
    let dim = rows[0].len();
    let mut sum = vec![0.0f32; dim];
    for r in rows {
        for (i, &v) in r.iter().enumerate() {
            sum[i] += v;
        }
    }
    let n = rows.len() as f32;
    for v in sum.iter_mut() {
        *v /= n;
    }
    l2_normalize(&sum)
}

/// 90th-percentile cosine distance from `centroid` to each row in `rows`.
/// Used as the cohesion / "radius" signal across the build pipeline and
/// by `Db::split_cluster`'s `leaf_cohesion_threshold` recursion guard.
pub fn ninetieth_percentile_distance(centroid: &[f32], rows: &[&[f32]]) -> f32 {
    if rows.is_empty() {
        return 0.0;
    }
    let mut dists: Vec<f32> = rows
        .iter()
        .map(|r| 1.0 - cosine_similarity(centroid, r))
        .collect();
    dists.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((dists.len() as f32) * 0.9).floor() as usize;
    dists[idx.min(dists.len() - 1)]
}
