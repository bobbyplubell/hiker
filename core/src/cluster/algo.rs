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

use super::{ClusterAssignment, ClusterError, LeidenParams, OUTLIER_LABEL};

/// HDBSCAN over a slice of pre-normalized embeddings. The crate operates
/// on Euclidean distance by default — we L2-normalize once on entry so
/// Euclidean is monotonic with cosine distance, which is what the spec
/// asks for ("cosine distance via pre-normalized embeddings" —
/// `cluster-hdbscan-crate-petal`).
///
/// `min_samples = None` defaults to `min_cluster_size` per `clustering.md`.
///
/// Returns one `ClusterAssignment` per input point; order matches the
/// input slice. Outliers carry `cluster_label = OUTLIER_LABEL`.
pub fn partition(
    embeddings: &[Vec<f32>],
    min_cluster_size: usize,
    min_samples: Option<usize>,
) -> Result<Vec<ClusterAssignment>, ClusterError> {
    if embeddings.is_empty() {
        return Err(ClusterError::Empty);
    }
    let dim = embeddings[0].len();
    for (i, row) in embeddings.iter().enumerate() {
        if row.len() != dim {
            return Err(ClusterError::DimMismatch {
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
        let mut out: Vec<ClusterAssignment> = Vec::with_capacity(n);
        for i in 0..n {
            out.push(ClusterAssignment {
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
        ClusterAssignment {
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

/// Leiden community detection over a kNN cosine-similarity graph. Per
/// `cluster-leiden` + `cluster-leiden-knn-graph`.
///
/// Construction:
/// 1. L2-normalize the input embeddings (so cosine = dot product).
/// 2. For each point, find its top-`k_nearest` neighbors by cosine
///    similarity (brute-force O(n²); fine at personal-vault scale).
/// 3. Drop neighbor edges with weight < `edge_weight_floor`.
/// 4. Build a `single-clustering` `CSRNetwork` from the deduped edges.
/// 5. Run Leiden over a Reichardt-Bornholdt configuration partition.
///    The `resolution` (γ) parameter lets the caller bias toward finer
///    (γ > 1) or coarser (γ < 1) communities; γ=1.0 is the modularity
///    equivalent.
/// 6. The optimized partition's `membership(node)` gives a community id
///    per node. Communities smaller than `min_cluster_size` are flagged
///    as outliers.
///
/// Returns one `ClusterAssignment` per input point, in input order.
/// `cluster_label = OUTLIER_LABEL` for noise points (small communities
/// or — when the input is below the partition guard — every point).
pub fn partition_leiden(
    embeddings: &[Vec<f32>],
    leiden: &LeidenParams,
) -> Result<Vec<ClusterAssignment>, ClusterError> {
    if embeddings.is_empty() {
        return Err(ClusterError::Empty);
    }
    let dim = embeddings[0].len();
    for (i, row) in embeddings.iter().enumerate() {
        if row.len() != dim {
            return Err(ClusterError::DimMismatch {
                row: i,
                expected: dim,
                got: row.len(),
            });
        }
    }

    let n = embeddings.len();
    let mut out: Vec<ClusterAssignment> = (0..n)
        .map(|i| ClusterAssignment {
            point_index: i,
            cluster_label: OUTLIER_LABEL,
        })
        .collect();

    let min_size = leiden.min_cluster_size.max(1) as usize;
    if n < min_size {
        return Ok(out);
    }

    // L2-normalize once so cosine reduces to dot product.
    let normed: Vec<Vec<f32>> = embeddings.iter().map(|v| l2_normalize(v)).collect();

    // kNN: brute-force for every point. Bound k by n-1 so we don't ask
    // for more neighbors than exist.
    let k = (leiden.k_nearest as usize).min(n.saturating_sub(1)).max(1);
    let floor = leiden.edge_weight_floor;

    // Collect undirected edges with cosine weights. Dedup on (min, max)
    // so each unordered pair appears once with the score from whichever
    // direction's kNN found it.
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

    // Edge case: no kept edges (e.g. floor too high, or n=1) means every
    // node is a singleton → mark them all outliers and return early.
    // `single-clustering`'s Leiden also asserts a non-trivial graph.
    if edges.is_empty() {
        return Ok(out);
    }

    let node_weights: Vec<f64> = vec![1.0; n];
    let network: CSRNetwork<f64, f64> = CSRNetwork::from_edges(&edges, node_weights);

    let config = LeidenConfig {
        max_iterations: (leiden.iterations as usize).max(1),
        seed: Some(0),
        ..LeidenConfig::default()
    };
    let mut optimizer = LeidenOptimizer::new(config);
    let resolution = leiden.resolution.max(0.0) as f64;
    let mut partition: RBConfigurationPartition<f64, VectorGrouping> =
        RBConfigurationPartition::with_resolution(network, resolution);
    optimizer
        .optimize_single_partition(&mut partition, None)
        .map_err(|e| ClusterError::Leiden(e.to_string()))?;

    // Group nodes by membership; communities below the size floor stay
    // OUTLIER_LABEL. Densify the surviving community ids so consumers
    // see contiguous labels 0..num_communities.
    let mut by_comm: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for node in 0..n {
        let comm = partition.membership(node);
        by_comm.entry(comm).or_default().push(node);
    }
    let mut next_label: i32 = 0;
    // Sort by raw community id for stable output across runs given a
    // fixed seed.
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
/// by `Trees::split_cluster`'s `leaf_cohesion_threshold` recursion guard.
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
