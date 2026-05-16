//! Clustering primitives — HDBSCAN partitioning and online placement
//! against an already-built tree. See `docs/clustering.md` for the full
//! spec. This module owns every `petal-clustering` import; outside
//! callers consume plain Rust types so the algorithm choice is a one-
//! file swap (mirrors `core::store` and `core::embed` discipline — see
//! `cluster-module-discipline`).
//!
//! status: cluster-module-discipline
//! status: cluster-hdbscan-crate-petal
//! status: cluster-place-beam-descent
//! status: cluster-leiden
//! status: cluster-leiden-crate-single-clustering

use ndarray::Array2;
use petal_clustering::{Fit, HDbscan};
use serde::{Deserialize, Serialize};
use single_clustering::community_search::leiden::partition::RBConfigurationPartition;
use single_clustering::community_search::leiden::{LeidenConfig, LeidenOptimizer};
use single_clustering::network::CSRNetwork;
use single_clustering::network::grouping::VectorGrouping;

/// Outlier sentinel used by HDBSCAN's flat-cluster output. Matches the
/// classic sklearn `-1` convention so callers reading the spec see what
/// they expect.
pub const OUTLIER_LABEL: i32 = -1;

/// One row from the cluster output: index into the caller's embeddings
/// slice → cluster label. `OUTLIER_LABEL` (-1) flags a noise point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterAssignment {
    pub point_index: usize,
    pub cluster_label: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("embeddings is empty")]
    Empty,
    #[error("inconsistent embedding dimensions: row 0 is {expected}, row {row} is {got}")]
    DimMismatch {
        row: usize,
        expected: usize,
        got: usize,
    },
    #[error("leiden: {0}")]
    Leiden(String),
}

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

// ── RAPTOR sample-and-merge planning ─────────────────────────────────

/// Default threshold above which a cluster's summarization is split
/// into sample batches with a fan-in merge step (per
/// `raptor-summarize-sample-merge`). Configurable downstream; the
/// constant here names the spec default so producers don't have to
/// repeat the number.
pub const SAMPLE_MERGE_BATCH_THRESHOLD: usize = 30;

/// Default target batch size when splitting a large cluster into
/// sibling sample-summary tasks. With 30-member batches a 300-member
/// cluster fans into 10 sibling tasks plus 1 merge — manageable for
/// the queue's priority arbitration.
pub const SAMPLE_MERGE_BATCH_SIZE: usize = 30;

/// Default hard cap on cluster members. Beyond this the fan-in cost
/// outweighs the value of an LLM summary; the producer skips
/// summarization for this cluster.
pub const SAMPLE_MERGE_MEMBER_CAP: usize = 300;

/// Plan the RAPTOR sample-and-merge strategy for one cluster. The
/// planner is pure data-shaping — no IO, no queue calls — so the
/// producer (cluster pipeline) and tests can both reason about it.
///
/// Returns one of three shapes:
/// - `Single`: cluster fits in a single LLM call; submit one
///   `RaptorSummarize` task.
/// - `SampleAndMerge`: cluster has > `batch_threshold` members; split
///   into `batches` (each batch a `RaptorSummarize` task) and aggregate
///   via a `Merge` task that depends on the batches.
/// - `TooLarge`: cluster exceeds `member_cap`; producer skips
///   summarization for this cluster.
///
/// status: raptor-summarize-sample-merge
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleMergePlan {
    Single { members: Vec<String> },
    SampleAndMerge { batches: Vec<Vec<String>> },
    TooLarge { member_count: usize },
}

/// Build a `SampleMergePlan` from a cluster's member list. Member
/// identity stays opaque to the planner — caller hands in strings
/// (note ids or chunk ids, whichever the cluster level is).
///
/// `batch_threshold = 0` is treated as 1 (always sample-and-merge);
/// `batch_size = 0` is normalized to 1 so the planner never produces
/// empty batches.
pub fn plan_sample_merge(
    members: &[String],
    batch_threshold: usize,
    batch_size: usize,
    member_cap: usize,
) -> SampleMergePlan {
    let n = members.len();
    if n > member_cap {
        return SampleMergePlan::TooLarge { member_count: n };
    }
    if n <= batch_threshold.max(1) {
        return SampleMergePlan::Single {
            members: members.to_vec(),
        };
    }
    let bsz = batch_size.max(1);
    let mut batches = Vec::with_capacity(n.div_ceil(bsz));
    for chunk in members.chunks(bsz) {
        batches.push(chunk.to_vec());
    }
    SampleMergePlan::SampleAndMerge { batches }
}

// ── Cluster tree shape (consumed by placement) ────────────────────────

/// Stable per-node id. Per `clustering.md`'s `cluster-tree-output`,
/// cluster ids are ephemeral within a run but stable enough to address
/// nodes inside a saved tree. Trees this module places into are
/// persisted by `core::trees`; we stay agnostic to the storage shape
/// and just take owned strings.
pub type NodeId = String;

/// One cluster-tree node. `members` is the unit of recursion — at
/// non-leaf levels each member references a child `NodeId`; at leaf
/// level each member is a note id. We don't need to know which until
/// the consumer walks the tree, so this module keeps both behind an
/// untyped string list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNode {
    pub id: NodeId,
    /// Mean of member embeddings, L2-normalized so cosine similarity
    /// against a normalized query reduces to a dot product.
    pub centroid: Vec<f32>,
    /// Child node ids, in arbitrary order. Empty for leaves.
    #[serde(default)]
    pub children: Vec<NodeId>,
}

/// Output of `place_beam_descent`. `confidence` is the matched leaf's
/// cosine against the (normalized) query; `margin` is the top-1 / top-2
/// gap across the final beam — used to flag ambiguous matches per
/// `cluster-place-beam-descent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementMatch {
    pub leaf_node_id: NodeId,
    pub confidence: f32,
    pub margin: f32,
}

/// View into a saved cluster tree the placement classifier walks. The
/// caller hands `place_beam_descent` a function from node-id → node so
/// the tree can live behind whatever storage shape the consumer
/// prefers (in-memory map, sqlite row lookup, etc.). Keeps this module
/// free of trees.db assumptions.
pub trait TreeView {
    fn root(&self) -> &NodeId;
    fn get(&self, id: &NodeId) -> Option<&ClusterNode>;
}

/// In-memory `TreeView` impl over a flat `HashMap<NodeId, ClusterNode>`.
/// Convenient for tests and for callers that already have the tree in
/// memory; persistent stores plug their own `TreeView` in.
pub struct InMemoryTree {
    pub root: NodeId,
    pub nodes: std::collections::HashMap<NodeId, ClusterNode>,
}

impl TreeView for InMemoryTree {
    fn root(&self) -> &NodeId {
        &self.root
    }
    fn get(&self, id: &NodeId) -> Option<&ClusterNode> {
        self.nodes.get(id)
    }
}

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

// ── Build types & pipeline ───────────────────────────────────────────
//
// Everything below is the offline batch build side. The placement
// classifier above is the online cheap path; the two share the same
// `ClusterNode` shape for tree traversal but the build pipeline produces
// the richer `BuiltClusterNode` (with names, summaries, confidence)
// described in `clustering.md` §"Output: what suggestions consume".
//
// status: cluster-build-recursive
// status: cluster-tree-output
// status: cluster-build-scope
// status: cluster-build-method
// status: cluster-build-params
// status: cluster-build-cluster-method
// status: cluster-build-from-folders
// status: cluster-build-from-folders-uniform-output
// status: cluster-algorithm-selectable
// status: cluster-hybrid-outlier-recovery
// status: cluster-summarize-llm
// status: cluster-hdbscan

/// Per `cluster-algorithm-selectable`. `Gmm` is reserved for the future
/// linfa-clustering swap; for now the producer falls back to `Hdbscan`
/// with a warning when a vault picks it (see `build_tree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClusterAlgorithm {
    Hdbscan,
    Gmm,
    Hybrid,
    /// Leiden community detection on a kNN cosine-similarity graph. Per
    /// `cluster-leiden`. Lands as an opt-in alternative to HDBSCAN for
    /// vaults where density-based clustering produces 0-1 cohesive
    /// cluster + everything-as-outliers.
    Leiden,
}

impl Default for ClusterAlgorithm {
    fn default() -> Self {
        ClusterAlgorithm::Hdbscan
    }
}

/// Leiden-specific tunables. Per `cluster-leiden-params`.
///
/// The `single-clustering` crate exposes a Reichardt-Bornholdt (RB)
/// configuration partition with a tunable `resolution` parameter (γ),
/// so the standard Leiden quality knob *is* available: γ > 1 biases
/// toward smaller / more communities, γ < 1 toward larger / fewer.
/// γ = 1.0 reduces to standard modularity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeidenParams {
    /// Number of nearest neighbors per node when building the kNN graph.
    /// Defaults to 15; a small enough k keeps the graph sparse, a large
    /// enough k keeps cohesive communities connected.
    pub k_nearest: u32,
    /// Edges with cosine similarity below this floor are dropped before
    /// Leiden runs. Defaults to 0.0 (keep every kNN edge). Raise to
    /// strip weak neighbor links and tighten community boundaries.
    pub edge_weight_floor: f32,
    /// Cap on Leiden refinement iterations. Defaults to 100.
    pub iterations: u32,
    /// Communities with fewer than this many members are flagged as
    /// outliers (mirrors HDBSCAN's `min_cluster_size` posture so the
    /// downstream tree shape stays consistent). Defaults to 2.
    pub min_cluster_size: u32,
    /// Resolution (γ) for the Reichardt-Bornholdt configuration partition.
    /// Defaults to 1.0 (modularity-equivalent). Higher = finer / more
    /// communities; lower = coarser / fewer.
    #[serde(default = "default_leiden_resolution")]
    pub resolution: f32,
    /// Resolution override used **only** on the build-recipe's top-level
    /// (virtual-root) Split call. Per `cluster-op-split`: when Split is
    /// invoked against the virtual root (`target_node_id = None`), the
    /// Leiden partition runs with this γ instead of `resolution`.
    /// Recursive sub-splits and direct user-driven splits against a real
    /// node use the normal `resolution`. Default `0.3` biases the
    /// top-level partition toward coarser / fewer communities so the
    /// initial "broad-strokes" cut isn't over-fragmented; recursive
    /// passes then refine each subtree at γ=1.0. [cluster-op-split]
    #[serde(default = "default_leiden_top_resolution")]
    pub top_level_resolution: f32,
}

fn default_leiden_resolution() -> f32 {
    1.0
}

fn default_leiden_top_resolution() -> f32 {
    0.3
}

impl Default for LeidenParams {
    fn default() -> Self {
        Self {
            k_nearest: 15,
            edge_weight_floor: 0.0,
            iterations: 100,
            min_cluster_size: 2,
            resolution: 1.0,
            top_level_resolution: 0.3,
        }
    }
}

/// Per `cluster-build-scope`. Caller-resolved into a `Vec<NoteRef>` by
/// the producer before `build_tree` runs; this type is what gets stored
/// on `cluster_trees.scope` so triage knows the eligible set.
///
/// Each variant carries an optional `source_types` filter (per
/// `cluster-build-scope-source-types`): a list of file extensions (canonical
/// lower-case without the leading dot, e.g. `"md"` or `"txt"`) that the
/// build pass + triage classifier accept. `"md"` matches both `.md` and
/// `.markdown` (the indexer treats them identically). Empty vec = no
/// filter = every indexable extension is in scope (legacy behavior; old
/// persisted trees deserialize cleanly via `#[serde(default)]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BuildScope {
    Vault {
        #[serde(default)]
        source_types: Vec<String>,
    },
    Folder {
        rel: String,
        #[serde(default)]
        source_types: Vec<String>,
    },
    Notes {
        ids: Vec<String>,
        #[serde(default)]
        source_types: Vec<String>,
    },
}

impl BuildScope {
    /// Read-only view of this scope's `source_types` filter. Empty slice =
    /// "no filter; accept every indexable extension."
    pub fn source_types(&self) -> &[String] {
        match self {
            BuildScope::Vault { source_types }
            | BuildScope::Folder { source_types, .. }
            | BuildScope::Notes { source_types, .. } => source_types,
        }
    }

    /// True when `path`'s extension is acceptable under this scope's
    /// `source_types` filter. An empty filter accepts everything.
    /// `"md"` matches both `.md` and `.markdown` (canonical-form
    /// equivalence; the indexer's chunker treats them as the same
    /// source type).
    pub fn matches_path(&self, path: &str) -> bool {
        let st = self.source_types();
        if st.is_empty() {
            return true;
        }
        // Lower-case the path's extension once. `rsplit_once('.')` gives
        // `("foo/bar", "md")`; paths without a dot have no extension and
        // are rejected when the filter is non-empty.
        let ext = match path.rsplit_once('.') {
            Some((_, e)) => e.to_ascii_lowercase(),
            None => return false,
        };
        for t in st {
            let want = t.to_ascii_lowercase();
            if ext == want {
                return true;
            }
            // `"md"` is the canonical form covering both `.md` and
            // `.markdown` per the indexer's INDEXABLE_EXTENSIONS list.
            if want == "md" && ext == "markdown" {
                return true;
            }
        }
        false
    }
}

/// Picks llm / skip for the per-cluster naming step. Per
/// `cluster-summarize-llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SummarizeMode {
    Llm,
    None,
}

impl Default for SummarizeMode {
    fn default() -> Self {
        SummarizeMode::Llm
    }
}

/// RAPTOR-shape build parameters. Per `cluster-build-params`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterParams {
    pub algorithm: ClusterAlgorithm,
    pub min_cluster_size: u32,
    /// `None` → defaults to `min_cluster_size` at runtime.
    pub min_samples: Option<u32>,
    /// Legacy termination knob from the pre-ops-framework build pipeline.
    /// Replaced by the per-branch recursion checks on `Trees::split_cluster`
    /// (`recurse` + `leaf_min_size` + `leaf_cohesion_threshold`). Kept
    /// deserializable so persisted `cluster_trees.method` JSON from before
    /// the ops-framework migration round-trips; `skip_serializing` so new
    /// rows don't carry the dead field. Per `cluster-op-split`'s "Surviving
    /// / changed knobs" table.
    #[serde(default, skip_serializing)]
    pub min_clusters_to_recurse: u32,
    pub summary_confidence_threshold: f32,
    pub include_outliers: bool,
    pub summarize: SummarizeMode,
    /// Leiden-specific tunables. Dormant unless `algorithm == Leiden`.
    /// `serde(default)` keeps old persisted `cluster_trees.method` JSON
    /// (which predates this field) deserializing cleanly.
    /// Per `cluster-leiden-params`.
    #[serde(default)]
    pub leiden: LeidenParams,
    /// When `true`, the recursive build loop short-circuits after the
    /// level-0 pass — `build_cluster_tree` returns a single-level tree.
    /// Surfaced as a UI toggle on the clustering review tab's Advanced
    /// disclosure (`cluster-review-tab-disable-recursion`).
    #[serde(default)]
    pub disable_recursion: bool,
    /// When `true`, `Trees::split_cluster` recursively re-splits each
    /// newly-produced child that exceeds `leaf_min_size` and whose
    /// 90th-percentile cohesion radius exceeds `leaf_cohesion_threshold`.
    /// Default `false` — direct user-driven splits stop after one level;
    /// the build recipe sets `true` so its top-down pass refines each
    /// branch until cohesion is reached. Per `cluster-op-split`.
    #[serde(default)]
    pub recurse: bool,
    /// Recursion guard for `Trees::split_cluster`: children with fewer
    /// than this many members are not re-split. Default 5; matches the
    /// HDBSCAN `min_cluster_size` posture.
    #[serde(default = "default_leaf_min_size")]
    pub leaf_min_size: u32,
    /// Recursion guard for `Trees::split_cluster`: children whose
    /// 90th-percentile cosine distance to centroid is at or below this
    /// value are considered tight enough and not re-split. Default 0.15.
    #[serde(default = "default_leaf_cohesion_threshold")]
    pub leaf_cohesion_threshold: f32,
}

fn default_leaf_min_size() -> u32 {
    5
}

fn default_leaf_cohesion_threshold() -> f32 {
    0.15
}

impl Default for ClusterParams {
    fn default() -> Self {
        Self {
            algorithm: ClusterAlgorithm::Hdbscan,
            min_cluster_size: 5,
            min_samples: None,
            min_clusters_to_recurse: 4,
            summary_confidence_threshold: 0.5,
            include_outliers: true,
            summarize: SummarizeMode::Llm,
            leiden: LeidenParams::default(),
            disable_recursion: false,
            recurse: false,
            leaf_min_size: default_leaf_min_size(),
            leaf_cohesion_threshold: default_leaf_cohesion_threshold(),
        }
    }
}

/// FromFolders parameters. Per `cluster-build-params`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderDeriveParams {
    pub summarize: SummarizeMode,
    pub include_outliers: bool,
    pub outlier_threshold: f32,
}

impl Default for FolderDeriveParams {
    fn default() -> Self {
        Self {
            summarize: SummarizeMode::Llm,
            include_outliers: true,
            outlier_threshold: 0.5,
        }
    }
}

/// Per `cluster-build-method`. The two methods produce the same output
/// shape (`BuiltClusterTree`); the cluster editor doesn't distinguish
/// once a tree exists (per `cluster-build-from-folders-uniform-output`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum BuildMethod {
    Cluster { params: ClusterParams },
    FromFolders { params: FolderDeriveParams },
}

/// One member of an input set handed to `build_tree`. Carries the note's
/// id, embedding, and the strings the summarizer feeds the prompt with.
/// The `folder` field is consulted only by the FromFolders method.
#[derive(Debug, Clone)]
pub struct NoteInput {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub folder: String,
    pub embedding: Vec<f32>,
}

/// Rich build-output node, matching the `ClusterNode` shape spec'd in
/// `clustering.md` §"Output". This is the offline batch-build product;
/// the smaller `ClusterNode` above is the placement-classifier view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltClusterNode {
    pub id: NodeId,
    /// Children for non-leaf clusters; note ids for leaf clusters.
    /// Member identity is opaque to consumers downstream of the build
    /// pipeline — the level index tells you which.
    pub members: Vec<String>,
    /// L2-normalized centroid (mean of member embeddings).
    pub centroid: Vec<f32>,
    /// 90th-percentile member distance from centroid (cosine distance).
    pub radius: f32,
    /// LLM-proposed or template name.
    pub name: String,
    /// LLM-generated or template summary.
    pub summary: String,
    /// 0.0-1.0 confidence from the summarizer.
    pub confidence: f32,
}

/// Output of `build_tree`. Per `clustering.md` §"Output: what suggestions
/// consume". `levels[0]` is the leaf clusters (over notes); `levels.last()`
/// is the root level. `outliers` holds unplaced note ids when
/// `include_outliers = true`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltClusterTree {
    pub levels: Vec<Vec<BuiltClusterNode>>,
    pub outliers: Vec<String>,
}

/// Result of the build with the input-resolution stage. Producers store
/// `scope` / `method` on the tree row alongside the tree itself.
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub scope: BuildScope,
    pub method: BuildMethod,
    pub tree: BuiltClusterTree,
}

/// Per-cluster naming output. Shape that the summarizer trait produces;
/// `core::summarize` (or its tf-idf fallback) populates this.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryOutput {
    pub name: String,
    pub summary: String,
    pub confidence: f32,
}

/// Inputs the summarizer receives per cluster. Members carry just
/// title + summary so the summarizer can format the prompt without
/// reaching back into the store.
#[derive(Debug, Clone)]
pub struct SummarizeInput<'a> {
    pub level: usize,
    pub members: Vec<MemberInfo<'a>>,
}

#[derive(Debug, Clone)]
pub struct MemberInfo<'a> {
    pub title: &'a str,
    pub summary: &'a str,
}

/// Pluggable per-cluster naming. Production wires in `LlmSummarizer`
/// (wraps `core::llm` per the trait pattern, `cluster-module-discipline`);
/// tests pass small in-memory mocks.
pub trait Summarizer {
    fn summarize(&self, input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("no notes in the resolved scope")]
    EmptyScope,
    #[error("clustering didn't separate the inputs: {found} notes resolved into fewer than 2 clusters. Try lowering min_cluster_size or include outliers.")]
    VaultTooSmall { found: usize },
    #[error("cluster: {0}")]
    Cluster(#[from] ClusterError),
    #[error("summarizer: {0}")]
    Summarizer(String),
}

/// LLM-backed summarizer. Renders the `cluster_summarize` prompt with the
/// cluster's member titles + summaries, calls `LlmClient::chat`, and
/// parses the JSON response into `SummaryOutput`.
///
/// The `Summarizer` trait is sync; `chat` is async. We bridge by spinning
/// a per-call current-thread runtime — cluster builds make O(clusters)
/// calls and each is dominated by network latency, so the runtime
/// build cost is negligible. The alternative (async trait + async build
/// pipeline) would cascade into every caller.
///
/// status: cluster-summarize-llm
pub struct LlmSummarizer {
    client: std::sync::Arc<dyn crate::llm::LlmClient>,
    prompt_template: String,
}

impl LlmSummarizer {
    pub fn new(
        client: std::sync::Arc<dyn crate::llm::LlmClient>,
        prompt_template: String,
    ) -> Self {
        Self { client, prompt_template }
    }
}

impl Summarizer for LlmSummarizer {
    fn summarize(&self, input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
        let mut members_txt = String::new();
        for m in &input.members {
            let t = m.title.trim();
            let s = m.summary.trim();
            if s.is_empty() {
                members_txt.push_str(&format!("- {t}\n"));
            } else {
                members_txt.push_str(&format!("- {t}: {s}\n"));
            }
        }
        let rendered = self
            .prompt_template
            .replace("{{level}}", &input.level.to_string())
            .replace("{{members}}", members_txt.trim_end());
        let msgs = vec![crate::llm::Message::user(rendered)];
        // Each call spins a dedicated worker thread with its own
        // multi-thread runtime so it stays isolated from whatever
        // runtime context the caller is in (tauri sync commands run on
        // tauri's own threads; tests may have no runtime at all).
        // Building a fresh runtime per call costs ~ms; LLM round-trips
        // are seconds.
        let client = self.client.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("runtime: {e}")));
                    return;
                }
            };
            let res = rt
                .block_on(client.chat(&msgs))
                .map_err(|e| format!("llm: {e}"));
            let _ = tx.send(res);
        });
        let resp = rx
            .recv()
            .map_err(|e| BuildError::Summarizer(format!("llm thread: {e}")))?
            .map_err(BuildError::Summarizer)?;
        parse_summary_json(&resp)
    }
}

fn parse_summary_json(resp: &str) -> Result<SummaryOutput, BuildError> {
    // The model is asked to return a bare JSON object, but providers
    // sometimes wrap it in prose or a ```json fence. Locate the first
    // `{` and the matching final `}` and parse that slice.
    let start = resp
        .find('{')
        .ok_or_else(|| BuildError::Summarizer(format!("no JSON object in response: {resp:?}")))?;
    let end = resp
        .rfind('}')
        .ok_or_else(|| BuildError::Summarizer(format!("unterminated JSON in response: {resp:?}")))?;
    if end < start {
        return Err(BuildError::Summarizer(format!("malformed JSON in response: {resp:?}")));
    }
    let slice = &resp[start..=end];
    #[derive(serde::Deserialize)]
    struct Raw {
        name: String,
        summary: String,
        #[serde(default)]
        confidence: Option<f32>,
    }
    let raw: Raw = serde_json::from_str(slice)
        .map_err(|e| BuildError::Summarizer(format!("parse JSON {slice:?}: {e}")))?;
    let confidence = raw.confidence.unwrap_or(0.7).clamp(0.0, 1.0);
    Ok(SummaryOutput {
        name: raw.name,
        summary: raw.summary,
        confidence,
    })
}

/// Build a cluster tree from a resolved set of notes. Per
/// `cluster-build-recursive` + `cluster-build-cluster-method` (and
/// `cluster-build-from-folders` for the folder-derived branch).
///
/// The producer is responsible for resolving `scope` → `Vec<NoteInput>`
/// (the embeddings + per-note summary the level-0 pass needs). This
/// function then runs the recursive cluster → summarize → embed pipeline,
/// or — for `BuildMethod::FromFolders` — walks the per-note `folder`
/// strings to mirror the filesystem.
///
/// `summarizer` provides the per-cluster naming. Producers in production
/// hand in `LlmSummarizer`; tests pass small in-memory mocks.
pub fn build_tree(
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<BuildResult, BuildError> {
    if notes.is_empty() {
        return Err(BuildError::EmptyScope);
    }
    let tree = match &method {
        BuildMethod::Cluster { params } => build_cluster_tree(notes, params, summarizer)?,
        BuildMethod::FromFolders { params } => build_from_folders(notes, params, summarizer)?,
    };
    Ok(BuildResult {
        scope,
        method,
        tree,
    })
}

/// Convenience: build a fresh tree and persist it into `trees.db`. The
/// resulting `cluster_trees` row + `cluster_nodes` rows are written
/// under one transaction (per `cluster-editor-draft-persistence` —
/// every node is editable from the moment it lands). Returns the new
/// `tree_id`.
pub fn build_and_persist(
    trees: &crate::trees::Trees,
    name: &str,
    source: &str,
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<String, BuildError> {
    let result = build_tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    let tree_id = trees
        .insert_tree(crate::trees::TreeInsert {
            id: None,
            name: name.to_string(),
            source: source.to_string(),
            state: "draft".to_string(),
            scope_json,
            method_json,
            vault_snapshot: None,
        })
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;
    let inserts = result_to_node_inserts(&result.tree);
    trees
        .insert_nodes(&tree_id, &inserts)
        .map_err(|e| BuildError::Summarizer(format!("insert_nodes: {e}")))?;
    Ok(tree_id)
}

/// Re-build an existing tree against the current vault state. Re-uses
/// the tree's saved `scope` + `method` (from `cluster_trees.scope` /
/// `.method`), re-runs `build_tree`, and persists a *new* tree row —
/// the original tree is left intact so the user can compare / discard.
/// User-edited fields (`user_edited_name`, `user_edited_summary`,
/// `policy`) on the old tree are preserved onto new clusters whose
/// member-set Jaccard against the old cluster exceeds `merge_threshold`
/// (0.5 by default — matches the spec's "preserve where membership
/// overlaps significantly" wording in the rollout doc).
///
/// Returns the new tree id.
///
/// Per `cluster-build-rebuild`.
///
/// status: cluster-build-rebuild
pub fn rebuild_and_persist(
    trees: &crate::trees::Trees,
    old_tree_id: &str,
    new_name: &str,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
    merge_threshold: f32,
) -> Result<String, BuildError> {
    let old_row = trees
        .get_tree(old_tree_id)
        .map_err(|e| BuildError::Summarizer(format!("get_tree: {e}")))?
        .ok_or_else(|| BuildError::Summarizer(format!("tree not found: {old_tree_id}")))?;
    let scope: BuildScope = serde_json::from_str(&old_row.scope_json)
        .map_err(|e| BuildError::Summarizer(format!("scope deserialize: {e}")))?;
    let method: BuildMethod = serde_json::from_str(&old_row.method_json)
        .map_err(|e| BuildError::Summarizer(format!("method deserialize: {e}")))?;
    let old_nodes = trees
        .list_nodes(old_tree_id)
        .map_err(|e| BuildError::Summarizer(format!("list_nodes: {e}")))?;

    let result = build_tree(scope.clone(), method.clone(), notes, summarizer)?;
    let scope_json = serde_json::to_string(&result.scope)
        .map_err(|e| BuildError::Summarizer(format!("scope serialize: {e}")))?;
    let method_json = serde_json::to_string(&result.method)
        .map_err(|e| BuildError::Summarizer(format!("method serialize: {e}")))?;
    let new_tree_id = trees
        .insert_tree(crate::trees::TreeInsert {
            id: None,
            name: new_name.to_string(),
            source: old_row.source.clone(),
            state: "draft".to_string(),
            scope_json,
            method_json,
            vault_snapshot: None,
        })
        .map_err(|e| BuildError::Summarizer(format!("insert_tree: {e}")))?;

    let mut inserts = result_to_node_inserts(&result.tree);

    // Compute per-old-cluster note-id member sets. Walk old_nodes once
    // to build child→parent map + leaf note ids per cluster.
    use std::collections::{HashMap, HashSet};
    let mut old_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut old_node_by_id: HashMap<String, &crate::trees::EditableNode> = HashMap::new();
    for n in &old_nodes {
        old_node_by_id.insert(n.id.clone(), n);
        if let Some(p) = &n.parent {
            old_children.entry(p.clone()).or_default().push(n.id.clone());
        }
    }
    fn collect_old_notes(
        id: &str,
        old_children: &HashMap<String, Vec<String>>,
        old_node_by_id: &HashMap<String, &crate::trees::EditableNode>,
        acc: &mut HashSet<String>,
    ) {
        if let Some(kids) = old_children.get(id) {
            for k in kids {
                if let Some(node) = old_node_by_id.get(k) {
                    if matches!(node.kind, crate::trees::NodeKind::Leaf) {
                        if let Some(nid) = &node.note_ref {
                            acc.insert(nid.clone());
                        }
                    } else {
                        collect_old_notes(k, old_children, old_node_by_id, acc);
                    }
                }
            }
        }
    }
    let mut old_cluster_members: HashMap<String, HashSet<String>> = HashMap::new();
    for n in &old_nodes {
        if matches!(n.kind, crate::trees::NodeKind::Cluster) {
            let mut s = HashSet::new();
            collect_old_notes(&n.id, &old_children, &old_node_by_id, &mut s);
            old_cluster_members.insert(n.id.clone(), s);
        }
    }

    // Build new clusters' note-id member sets from `inserts`.
    let mut new_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut new_node_kind: HashMap<String, crate::trees::NodeKind> = HashMap::new();
    let mut new_note_ref: HashMap<String, Option<String>> = HashMap::new();
    for n in &inserts {
        if let Some(p) = &n.parent_id {
            new_children.entry(p.clone()).or_default().push(n.node_id.clone());
        }
        new_node_kind.insert(n.node_id.clone(), n.kind);
        new_note_ref.insert(n.node_id.clone(), n.note_id.clone());
    }
    fn collect_new_notes(
        id: &str,
        new_children: &HashMap<String, Vec<String>>,
        new_node_kind: &HashMap<String, crate::trees::NodeKind>,
        new_note_ref: &HashMap<String, Option<String>>,
        acc: &mut HashSet<String>,
    ) {
        if let Some(kids) = new_children.get(id) {
            for k in kids {
                if let Some(kind) = new_node_kind.get(k) {
                    if matches!(kind, crate::trees::NodeKind::Leaf) {
                        if let Some(Some(nid)) = new_note_ref.get(k) {
                            acc.insert(nid.clone());
                        }
                    } else {
                        collect_new_notes(k, new_children, new_node_kind, new_note_ref, acc);
                    }
                }
            }
        }
    }

    // For each new cluster, find the old cluster with the highest
    // Jaccard. If above the threshold, transfer user-edited name /
    // summary + policy.
    for ins in inserts.iter_mut() {
        if !matches!(ins.kind, crate::trees::NodeKind::Cluster) {
            continue;
        }
        let mut new_members: HashSet<String> = HashSet::new();
        collect_new_notes(
            &ins.node_id,
            &new_children,
            &new_node_kind,
            &new_note_ref,
            &mut new_members,
        );
        if new_members.is_empty() {
            continue;
        }
        let mut best_id: Option<&String> = None;
        let mut best_jaccard: f32 = 0.0;
        for (old_id, old_members) in &old_cluster_members {
            if old_members.is_empty() {
                continue;
            }
            let inter = new_members.intersection(old_members).count() as f32;
            let union = new_members.union(old_members).count() as f32;
            if union <= 0.0 {
                continue;
            }
            let j = inter / union;
            if j > best_jaccard {
                best_jaccard = j;
                best_id = Some(old_id);
            }
        }
        if best_jaccard >= merge_threshold {
            if let Some(old_id) = best_id {
                if let Some(old_node) = old_node_by_id.get(old_id) {
                    if old_node.user_edited_name {
                        ins.name = old_node.name.clone();
                        ins.user_edited_name = true;
                    }
                    if old_node.user_edited_summary {
                        ins.summary = old_node.summary.clone();
                        ins.user_edited_summary = true;
                    }
                    if old_node.policy.is_some() {
                        ins.policy = old_node.policy.clone();
                    }
                }
            }
        }
    }

    trees
        .insert_nodes(&new_tree_id, &inserts)
        .map_err(|e| BuildError::Summarizer(format!("insert_nodes: {e}")))?;
    Ok(new_tree_id)
}

/// Flatten a `BuiltClusterTree` into the row shape `core::trees`
/// consumes. Top of the tree is the highest-level cluster (root); levels
/// descend with cluster-kind rows; the leaf level produces `leaf`-kind
/// rows under their parent clusters. Outliers attach as `leaf`-kind rows
/// under a dedicated `outlier-bucket` node parented at the root.
/// Public view onto `result_to_node_inserts` for callers outside this
/// module (e.g. the Tauri `cluster_persist_built_tree` command that
/// drives the clustering review tab's Confirm-and-name step).
///
/// status: cluster-review-tab-confirm-and-name
pub fn result_to_node_inserts_pub(tree: &BuiltClusterTree) -> Vec<crate::trees::NodeInsert> {
    result_to_node_inserts(tree)
}

fn result_to_node_inserts(tree: &BuiltClusterTree) -> Vec<crate::trees::NodeInsert> {
    use crate::trees::{NodeInsert, NodeKind};
    let mut out: Vec<NodeInsert> = Vec::new();
    if tree.levels.is_empty() {
        return out;
    }
    // Determine the root. If the top level has exactly one node, that's
    // root. Otherwise synthesize a root that owns the top-level nodes.
    let top_level = tree.levels.len() - 1;
    let top = &tree.levels[top_level];
    let (root_id, synthesized_root) = if top.len() == 1 {
        (top[0].id.clone(), false)
    } else {
        ("root".to_string(), true)
    };

    // Build a parent lookup: for each child id, who's its parent?
    // The build process records `members` on each `BuiltClusterNode`:
    // - cluster levels (1..N): members are child cluster ids
    // - level 0: members are note ids
    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for level in tree.levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    if synthesized_root {
        for n in top {
            parent_of.insert(n.id.clone(), root_id.clone());
        }
    }

    // Write the synthesized root, if any.
    if synthesized_root {
        // Centroid for the synthesized root = mean of top-level
        // centroids, L2-normalized.
        let top_centroids: Vec<&[f32]> = top.iter().map(|n| n.centroid.as_slice()).collect();
        let centroid = mean_normalize(&top_centroids);
        out.push(NodeInsert {
            node_id: root_id.clone(),
            parent_id: None,
            kind: NodeKind::Cluster,
            note_id: None,
            name: "Vault root".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: Some(centroid),
            confidence: 1.0,
            summary_membership_churn: 0,
        });
    }

    // Emit cluster nodes for every level.
    for (level_idx, level) in tree.levels.iter().enumerate() {
        for node in level {
            let parent = if level_idx == top_level && !synthesized_root {
                None
            } else {
                parent_of.get(&node.id).cloned()
            };
            out.push(NodeInsert {
                node_id: node.id.clone(),
                parent_id: parent,
                kind: NodeKind::Cluster,
                note_id: None,
                name: node.name.clone(),
                summary: node.summary.clone(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            });
        }
    }

    // Emit leaf nodes under their level-0 cluster.
    if let Some(leaf_level) = tree.levels.first() {
        for cluster in leaf_level {
            for note_id in &cluster.members {
                let leaf_id = format!("leaf-{}", note_id);
                out.push(NodeInsert {
                    node_id: leaf_id,
                    parent_id: Some(cluster.id.clone()),
                    kind: NodeKind::Leaf,
                    note_id: Some(note_id.clone()),
                    name: note_id.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: cluster.confidence,
                    summary_membership_churn: 0,
                });
            }
        }
    }

    // Outliers bucket, parented at root.
    if !tree.outliers.is_empty() {
        let bucket_id = "outliers".to_string();
        out.push(NodeInsert {
            node_id: bucket_id.clone(),
            parent_id: Some(root_id.clone()),
            kind: NodeKind::OutlierBucket,
            note_id: None,
            name: "Outliers".to_string(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 0.0,
            summary_membership_churn: 0,
        });
        for note_id in &tree.outliers {
            out.push(NodeInsert {
                node_id: format!("leaf-{}", note_id),
                parent_id: Some(bucket_id.clone()),
                kind: NodeKind::Leaf,
                note_id: Some(note_id.clone()),
                name: note_id.clone(),
                summary: String::new(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: None,
                confidence: 0.0,
                summary_membership_churn: 0,
            });
        }
    }

    out
}

// ── Build recipe: top-down divisive Split ────────────────────────────
//
// status: cluster-build-recipe
// status: cluster-build-cluster-method
//
// The `Cluster` build method composes the ops-framework primitives per
// `clustering.md` §"Build recipe":
//
//   1. Split { target: virtual_root(scope), params: { recurse: true, ... } }
//      → produces a tree of leaf clusters with placeholder names.
//   2. Summarize { scope: All } (gated separately by Confirm-and-name).
//   3. (Optional, default off) Rollup over the top layer.
//
// `build_cluster_tree` runs step 1 only; summarization is invoked either
// (a) per-cluster by the recipe's inline `summarizer` argument when the
// user picks Confirm-and-name in the review tab, or (b) deferred when
// `SummarizeMode::None` is forced by the structural pass. Step 3 is an
// explicit cluster-editor verb and is not invoked here.
//
// **Algorithm shape**: top-down divisive. The first Split partitions the
// virtual root's note embeddings using `LeidenParams.top_level_resolution`
// (default 0.3) so the coarse top-level cut produces 3–8 broad clusters.
// Each top-level child is then recursively re-split using the regular
// `LeidenParams.resolution` (default 1.0) — at every sub-split the
// algorithm runs on *actual note embeddings within that branch's member
// set*, not on centroids-of-centroids. Sub-splits stop per branch when
// either child member count `<=` `leaf_min_size` OR child cohesion
// radius `<` `leaf_cohesion_threshold`. Hard 16-level safety cap.
//
// The legacy `min_clusters_to_recurse` knob is deserialized for
// backwards-compat (`#[serde(default, skip_serializing)]`) and ignored
// at runtime per the spec.

/// One node in the in-memory divisive tree the recipe builds before
/// flattening into `BuiltClusterTree`. A `branch` carries child nodes
/// (sub-clusters); a `leaf` carries note-id members directly. The
/// distinction maps onto `BuiltClusterNode.members` content (cluster
/// ids vs note ids) at flatten time.
enum SplitNode {
    /// Cluster that was further split into sub-clusters.
    Branch {
        id: String,
        centroid: Vec<f32>,
        radius: f32,
        name: String,
        summary: String,
        confidence: f32,
        children: Vec<SplitNode>,
    },
    /// Cluster that was not further split — its members are note ids.
    Leaf {
        id: String,
        centroid: Vec<f32>,
        radius: f32,
        name: String,
        summary: String,
        confidence: f32,
        note_ids: Vec<String>,
    },
}

fn build_cluster_tree(
    notes: &[NoteInput],
    params: &ClusterParams,
    summarizer: &dyn Summarizer,
) -> Result<BuiltClusterTree, BuildError> {
    // GMM isn't wired yet (linfa-clustering doesn't ship HDBSCAN; see
    // `clustering.md` §"Crate choice"). Producers requesting `Gmm` fall
    // back to `Hdbscan` on every Split call.
    //
    // status: cluster-algorithm-selectable (partial — gmm path stubbed)
    if matches!(params.algorithm, ClusterAlgorithm::Gmm) {
        tracing::warn!("cluster: gmm algorithm not yet supported; falling back to hdbscan");
    }

    tracing::info!(
        algorithm = ?params.algorithm,
        note_count = notes.len(),
        recurse = !params.disable_recursion,
        leaf_min_size = params.leaf_min_size,
        leaf_cohesion_threshold = params.leaf_cohesion_threshold,
        top_level_resolution = params.leiden.top_level_resolution,
        resolution = params.leiden.resolution,
        include_outliers = params.include_outliers,
        "cluster: build recipe entry — top-down divisive Split from virtual root"
    );

    // ── Step 1: top-level Split against the virtual root ─────────────
    //
    // The first Split is special:
    //   - Uses `top_level_resolution` (Leiden only) for a coarser cut.
    //   - Handles outliers (Hybrid / `include_outliers = false`) by
    //     force-routing them into the nearest top-level community.
    //   - Requires at least 2 cohesive communities (else VaultTooSmall).
    //
    // Sub-splits below use the regular `resolution` and silently fold
    // outliers into a per-branch outlier list (the spec doesn't ask for
    // recursive Hybrid recovery).
    let indices: Vec<usize> = (0..notes.len()).collect();
    let top_assignments =
        partition_indices(notes, &indices, params, /* top_level */ true)?;
    let mut top_groups: std::collections::BTreeMap<i32, Vec<usize>> =
        std::collections::BTreeMap::new();
    for a in &top_assignments {
        top_groups
            .entry(a.cluster_label)
            .or_default()
            .push(indices[a.point_index]);
    }
    let mut top_outliers: Vec<usize> = top_groups.remove(&OUTLIER_LABEL).unwrap_or_default();

    if top_groups.len() < 2 {
        return Err(BuildError::VaultTooSmall { found: notes.len() });
    }

    // Hybrid / force-routing applies only at the top level.
    let hybrid_recovery_for_algo = matches!(params.algorithm, ClusterAlgorithm::Hybrid)
        && !matches!(params.algorithm, ClusterAlgorithm::Leiden);
    if hybrid_recovery_for_algo || !params.include_outliers {
        let interim_centroids: std::collections::BTreeMap<i32, Vec<f32>> = top_groups
            .iter()
            .map(|(label, idxs)| {
                let refs: Vec<&[f32]> =
                    idxs.iter().map(|&i| notes[i].embedding.as_slice()).collect();
                (*label, mean_normalize(&refs))
            })
            .collect();
        // `include_outliers = false` → force-route every outlier into
        // its nearest cluster (threshold `-1.0` admits everything). Per
        // `cluster-build-cluster-method`'s "outlier recovery loop with
        // cosine threshold dropped to -1.0" requirement.
        let threshold: f32 = if !params.include_outliers { -1.0 } else { 0.6 };
        let mut still_outliers: Vec<usize> = Vec::new();
        for &i in &top_outliers {
            let q = l2_normalize(&notes[i].embedding);
            let mut best: Option<(i32, f32)> = None;
            for (label, centroid) in &interim_centroids {
                let s = cosine_similarity(&q, centroid);
                match best {
                    Some((_, bs)) if s <= bs => {}
                    _ => best = Some((*label, s)),
                }
            }
            match best {
                Some((label, score)) if score >= threshold => {
                    top_groups.entry(label).or_default().push(i);
                }
                _ => still_outliers.push(i),
            }
        }
        top_outliers = still_outliers;
    }

    tracing::info!(
        top_level_clusters = top_groups.len(),
        outliers = top_outliers.len(),
        "cluster: top-level Split produced communities"
    );

    // Build a `SplitNode` per top-level community. Recursively sub-split
    // unless `disable_recursion` is set; the recursion stops per-branch
    // on `leaf_min_size` / `leaf_cohesion_threshold` / 16-level cap.
    const MAX_DEPTH: u8 = 16;
    let recurse = !params.disable_recursion;
    let mut top_level_nodes: Vec<SplitNode> = Vec::new();
    for (label, idxs) in top_groups.into_iter() {
        let id = format!("c0-{label}");
        let node = recursive_split_branch(
            id,
            &idxs,
            notes,
            params,
            summarizer,
            recurse,
            /* depth */ 1,
            MAX_DEPTH,
        )?;
        top_level_nodes.push(node);
    }

    let outlier_ids: Vec<String> = if params.include_outliers {
        top_outliers.iter().map(|&i| notes[i].id.clone()).collect()
    } else {
        // By this point every outlier was force-routed into a cluster
        // via the recovery pass above; anything still here is a
        // degenerate case (zero centroids, etc.). Drop it so the output
        // doesn't contradict `include_outliers = false`.
        Vec::new()
    };

    let tree = flatten_split_forest(top_level_nodes, outlier_ids);
    tracing::info!(
        total_levels = tree.levels.len(),
        per_level_counts = ?tree.levels.iter().map(|l| l.len()).collect::<Vec<_>>(),
        outliers = tree.outliers.len(),
        "cluster: build recipe finished"
    );
    Ok(tree)
}

/// Partition the `indices` subset of `notes` by their embeddings. The
/// `top_level` flag swaps in `LeidenParams.top_level_resolution` for
/// `resolution`; sub-splits get the normal `resolution`. Returns the
/// partitioner's `ClusterAssignment`s with `point_index` indexing into
/// the *local* `indices` slice (i.e. 0..indices.len()) — callers
/// translate back to global `notes` indices themselves.
fn partition_indices(
    notes: &[NoteInput],
    indices: &[usize],
    params: &ClusterParams,
    top_level: bool,
) -> Result<Vec<ClusterAssignment>, ClusterError> {
    let embeddings: Vec<Vec<f32>> =
        indices.iter().map(|&i| notes[i].embedding.clone()).collect();
    match params.algorithm {
        ClusterAlgorithm::Leiden => {
            let mut leiden = params.leiden.clone();
            if top_level {
                leiden.resolution = params.leiden.top_level_resolution;
            }
            // Clamp k_nearest to n-1 so the kNN graph build doesn't ask
            // for more neighbors than exist in the local subset.
            let upper = indices.len().saturating_sub(1).max(1) as u32;
            leiden.k_nearest = leiden.k_nearest.min(upper);
            partition_leiden(&embeddings, &leiden)
        }
        _ => partition(
            &embeddings,
            params.min_cluster_size as usize,
            params.min_samples.map(|x| x as usize),
        ),
    }
}

/// Build a `SplitNode` for one branch. Computes centroid + radius +
/// summary for this cluster, then either:
///   - emits a `Leaf` when stop conditions trip (member count
///     `<=` `leaf_min_size`, OR cohesion radius `<` `leaf_cohesion_threshold`,
///     OR recursion disabled, OR depth cap reached, OR sub-split fails
///     to produce >= 2 communities), or
///   - emits a `Branch` containing recursively-split children.
///
/// `member_idxs` are indices into the outer `notes` slice.
#[allow(clippy::too_many_arguments)]
fn recursive_split_branch(
    id: String,
    member_idxs: &[usize],
    notes: &[NoteInput],
    params: &ClusterParams,
    summarizer: &dyn Summarizer,
    recurse: bool,
    depth: u8,
    max_depth: u8,
) -> Result<SplitNode, BuildError> {
    let refs: Vec<&[f32]> = member_idxs
        .iter()
        .map(|&i| notes[i].embedding.as_slice())
        .collect();
    let centroid = mean_normalize(&refs);
    let radius = ninetieth_percentile_distance(&centroid, &refs);
    let infos: Vec<MemberInfo<'_>> = member_idxs
        .iter()
        .map(|&i| MemberInfo {
            title: &notes[i].title,
            summary: &notes[i].summary,
        })
        .collect();
    // Summarize at this cluster's level (depth from top, 0-indexed for
    // the summarizer's `level` field — keeps the LLM prompt shape
    // consistent with prior pipeline).
    let SummaryOutput {
        name,
        summary,
        confidence,
    } = run_summarizer(params.summarize, depth as usize - 1, infos, summarizer)?;

    // Per-branch stop conditions.
    let too_small = member_idxs.len() <= params.leaf_min_size as usize;
    let too_tight = radius < params.leaf_cohesion_threshold;
    let at_cap = depth >= max_depth;
    if !recurse || too_small || too_tight || at_cap {
        let reason = if !recurse {
            "disable_recursion"
        } else if at_cap {
            "16-level cap"
        } else if too_small {
            "member_count <= leaf_min_size"
        } else {
            "radius < leaf_cohesion_threshold"
        };
        tracing::debug!(
            id = %id,
            depth,
            members = member_idxs.len(),
            radius,
            reason,
            "cluster: branch stopped — emitting leaf cluster"
        );
        let note_ids: Vec<String> = member_idxs.iter().map(|&i| notes[i].id.clone()).collect();
        return Ok(SplitNode::Leaf {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            note_ids,
        });
    }

    // Recursive sub-split using the normal `resolution`. If the
    // partitioner produces fewer than 2 cohesive communities (or
    // errors), the branch can't be refined further — emit a leaf
    // cluster instead. We don't propagate the partition error: a
    // sub-split is allowed to fail to refine without aborting the whole
    // build (the per-branch outcome is "this stays a leaf cluster").
    let sub_assignments = match partition_indices(notes, member_idxs, params, /* top_level */ false)
    {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(
                id = %id,
                depth,
                error = %e,
                "cluster: sub-split partition errored — emitting leaf cluster"
            );
            let note_ids: Vec<String> = member_idxs.iter().map(|&i| notes[i].id.clone()).collect();
            return Ok(SplitNode::Leaf {
                id,
                centroid,
                radius,
                name,
                summary,
                confidence,
                note_ids,
            });
        }
    };
    let mut sub_groups: std::collections::BTreeMap<i32, Vec<usize>> =
        std::collections::BTreeMap::new();
    for a in &sub_assignments {
        if a.cluster_label == OUTLIER_LABEL {
            // Per spec, sub-splits don't run a Hybrid-style recovery;
            // outliers at this level fold back into the *parent*
            // cluster as plain members (we treat the parent as the
            // settled home when sub-splitting fails to assign them).
            // Concretely: keep them in member_idxs implicitly by
            // routing them into a "remainder" bucket below.
            continue;
        }
        sub_groups
            .entry(a.cluster_label)
            .or_default()
            .push(member_idxs[a.point_index]);
    }
    let sub_outlier_local: Vec<usize> = sub_assignments
        .iter()
        .filter(|a| a.cluster_label == OUTLIER_LABEL)
        .map(|a| member_idxs[a.point_index])
        .collect();

    if sub_groups.len() < 2 {
        tracing::debug!(
            id = %id,
            depth,
            members = member_idxs.len(),
            sub_communities = sub_groups.len(),
            "cluster: sub-split produced <2 communities — emitting leaf cluster"
        );
        let note_ids: Vec<String> = member_idxs.iter().map(|&i| notes[i].id.clone()).collect();
        return Ok(SplitNode::Leaf {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            note_ids,
        });
    }

    tracing::debug!(
        id = %id,
        depth,
        sub_communities = sub_groups.len(),
        sub_outliers = sub_outlier_local.len(),
        "cluster: sub-split accepted"
    );

    let mut children: Vec<SplitNode> = Vec::new();
    for (label, child_idxs) in sub_groups.into_iter() {
        let child_id = format!("{id}-s{label}");
        let child = recursive_split_branch(
            child_id,
            &child_idxs,
            notes,
            params,
            summarizer,
            recurse,
            depth + 1,
            max_depth,
        )?;
        children.push(child);
    }
    // Sub-level outliers are folded into the first child cluster as
    // plain members so they remain reachable in the persisted tree.
    // This matches the build recipe's "every note gets a home under the
    // top-level community" intent (Hybrid / force-routing decided what
    // counted as "outlier" at the top level; below that we never
    // discard a note that already passed the top-level gate).
    if !sub_outlier_local.is_empty() {
        if let Some(first) = children.first_mut() {
            fold_into_first_leaf(first, &sub_outlier_local, notes);
        }
    }

    Ok(SplitNode::Branch {
        id,
        centroid,
        radius,
        name,
        summary,
        confidence,
        children,
    })
}

/// Fold extra note indices into the first leaf descendant of `node`,
/// recomputing centroid + radius locally. Used to absorb sub-level
/// partition outliers (see `recursive_split_branch`).
fn fold_into_first_leaf(node: &mut SplitNode, extra_idxs: &[usize], notes: &[NoteInput]) {
    match node {
        SplitNode::Leaf {
            centroid,
            radius,
            note_ids,
            ..
        } => {
            for &i in extra_idxs {
                note_ids.push(notes[i].id.clone());
            }
            // Recompute centroid + radius over the full member set.
            // Need to rebuild the embedding refs from `note_ids`; we
            // don't have a id→idx map handy, but we can recompute from
            // the new combined set: walk `notes` for ids that match.
            // Cheaper: append `extra_idxs` embeddings to the prior
            // mean by re-deriving from a built set.
            let by_id: std::collections::HashMap<&str, &NoteInput> =
                notes.iter().map(|n| (n.id.as_str(), n)).collect();
            let mut refs: Vec<&[f32]> = Vec::with_capacity(note_ids.len());
            for nid in note_ids.iter() {
                if let Some(n) = by_id.get(nid.as_str()) {
                    refs.push(n.embedding.as_slice());
                }
            }
            *centroid = mean_normalize(&refs);
            *radius = ninetieth_percentile_distance(centroid, &refs);
        }
        SplitNode::Branch { children, .. } => {
            if let Some(first) = children.first_mut() {
                fold_into_first_leaf(first, extra_idxs, notes);
            }
        }
    }
}

/// Flatten the top-down divisive forest into a `BuiltClusterTree`. The
/// `levels` contract per `cluster-tree-output`:
///
/// - `levels[0]` = leaf clusters (`members` are note ids).
/// - `levels[k>0]` = parent clusters (`members` are child cluster ids).
/// - `levels.last()` = top-level (root candidates).
///
/// Because the divisive build produces uneven branch depths, we pack
/// each cluster at `level = max_descendant_depth + 1` (a leaf cluster
/// sits at level 0; its parent at level 1; etc., taking the *max* of
/// each child's level so parents always sit above all their children).
///
/// To keep `result_to_node_inserts`'s "synthesize a root iff top.len() != 1"
/// machinery happy when the virtual-root Split produces top-level
/// clusters at different levels (which happens when some branches went
/// deeper than others), we **always** add a synthetic vault root to
/// `levels` when there is more than one top-level cluster. The root's
/// `members` are the top-level cluster ids. When there is exactly one
/// top-level cluster (theoretically impossible since we error
/// `VaultTooSmall` below 2 communities, but defended for safety), it
/// becomes the natural root.
fn flatten_split_forest(top_level: Vec<SplitNode>, outliers: Vec<String>) -> BuiltClusterTree {
    let mut levels: Vec<Vec<BuiltClusterNode>> = Vec::new();
    let mut top_ids: Vec<String> = Vec::new();
    let mut top_centroids: Vec<Vec<f32>> = Vec::new();

    for node in top_level {
        let (_lvl, id, centroid) = place_in_levels(node, &mut levels);
        top_ids.push(id);
        top_centroids.push(centroid);
    }

    // Add a synthetic vault root when there's more than one top-level
    // cluster. With exactly one, the persistence flatten
    // (`result_to_node_inserts`) treats it as root naturally.
    if top_ids.len() > 1 {
        let refs: Vec<&[f32]> = top_centroids.iter().map(|v| v.as_slice()).collect();
        let centroid = mean_normalize(&refs);
        // Place above every other level.
        let target_level = levels.len();
        while levels.len() <= target_level {
            levels.push(Vec::new());
        }
        levels[target_level].push(BuiltClusterNode {
            id: "vault-root".to_string(),
            members: top_ids,
            centroid,
            radius: 0.0,
            name: String::new(),
            summary: String::new(),
            confidence: 1.0,
        });
    }

    BuiltClusterTree { levels, outliers }
}

/// Recursively place a `SplitNode` into `levels`. Returns the level
/// index, id, and centroid of the placed node. A leaf cluster lands at
/// level 0; a branch lands at `1 + max(child levels)`.
fn place_in_levels(
    node: SplitNode,
    levels: &mut Vec<Vec<BuiltClusterNode>>,
) -> (usize, String, Vec<f32>) {
    match node {
        SplitNode::Leaf {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            note_ids,
        } => {
            while levels.is_empty() {
                levels.push(Vec::new());
            }
            let built = BuiltClusterNode {
                id: id.clone(),
                members: note_ids,
                centroid: centroid.clone(),
                radius,
                name,
                summary,
                confidence,
            };
            levels[0].push(built);
            (0, id, centroid)
        }
        SplitNode::Branch {
            id,
            centroid,
            radius,
            name,
            summary,
            confidence,
            children,
        } => {
            let mut child_ids: Vec<String> = Vec::with_capacity(children.len());
            let mut max_child_level: usize = 0;
            for child in children {
                let (lvl, child_id, _c) = place_in_levels(child, levels);
                child_ids.push(child_id);
                max_child_level = max_child_level.max(lvl);
            }
            let level_idx = max_child_level + 1;
            while levels.len() <= level_idx {
                levels.push(Vec::new());
            }
            let built = BuiltClusterNode {
                id: id.clone(),
                members: child_ids,
                centroid: centroid.clone(),
                radius,
                name,
                summary,
                confidence,
            };
            levels[level_idx].push(built);
            (level_idx, id, centroid)
        }
    }
}

fn run_summarizer(
    mode: SummarizeMode,
    level: usize,
    members: Vec<MemberInfo<'_>>,
    summarizer: &dyn Summarizer,
) -> Result<SummaryOutput, BuildError> {
    match mode {
        // status: cluster-review-tab-structural-pass-no-llm
        // `SummarizeMode::None` short-circuits the summarizer call
        // entirely so the structural pass requires no LLM client. Names
        // are left blank here; the caller (`build_tree_structural`)
        // assigns placeholder `"Cluster N"` names ordered by
        // member-count-descending so the result panel has something
        // human-meaningful to show before Confirm-and-name fires.
        SummarizeMode::None => {
            let _ = members;
            Ok(SummaryOutput {
                name: String::new(),
                summary: String::new(),
                confidence: 0.0,
            })
        }
        SummarizeMode::Llm => summarizer.summarize(SummarizeInput { level, members }),
    }
}

/// No-op summarizer used by the structural-only build path
/// (`build_tree_structural`). Cannot actually be invoked because the
/// structural path forces `SummarizeMode::None` on every method param;
/// returns an error loudly if it ever is, so an accidental misuse is
/// observable rather than silent.
///
/// status: cluster-review-tab-structural-pass-no-llm
pub struct NoopSummarizer;

impl Summarizer for NoopSummarizer {
    fn summarize(&self, _input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
        Err(BuildError::Summarizer(
            "NoopSummarizer cannot summarize — structural pass should have set SummarizeMode::None".into(),
        ))
    }
}

/// Run a structural-only cluster build: no LLM calls, no `Summarizer`
/// dependency. Forces `SummarizeMode::None` on the method's params (so a
/// caller that passes `Llm` doesn't accidentally hit the summarizer) and
/// assigns placeholder names (`"Cluster 1"`, `"Cluster 2"`, …) to the
/// resulting leaf-level clusters in member-count-descending order.
/// Recursive levels above the leaf level are left with empty names —
/// the user only ever sees the leaf level in the clustering review
/// panel; higher levels exist only as parents for the persisted tree
/// shape.
///
/// status: cluster-review-tab-run-clustering
/// status: cluster-review-tab-structural-pass-no-llm
pub fn build_tree_structural(
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
) -> Result<BuildResult, BuildError> {
    let forced_method = match method {
        BuildMethod::Cluster { mut params } => {
            params.summarize = SummarizeMode::None;
            BuildMethod::Cluster { params }
        }
        BuildMethod::FromFolders { mut params } => {
            params.summarize = SummarizeMode::None;
            BuildMethod::FromFolders { params }
        }
    };
    let noop = NoopSummarizer;
    let mut result = build_tree(scope, forced_method, notes, &noop)?;
    // Walk only level 0 (leaf-level clusters) for placeholder naming.
    // FromFolders already sets the folder basename as the name in
    // `SummarizeMode::None`, so we leave those alone; the heuristic
    // below treats any cluster whose `name` is empty as needing a
    // placeholder.
    if let Some(leaf_level) = result.tree.levels.get_mut(0) {
        let mut order: Vec<usize> = (0..leaf_level.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(leaf_level[i].members.len()));
        let mut next_n: usize = 1;
        for &i in &order {
            if leaf_level[i].name.is_empty() {
                leaf_level[i].name = format!("Cluster {}", next_n);
                next_n += 1;
            }
        }
    }
    Ok(result)
}

// ── FromFolders method ───────────────────────────────────────────────

fn build_from_folders(
    notes: &[NoteInput],
    params: &FolderDeriveParams,
    summarizer: &dyn Summarizer,
) -> Result<BuiltClusterTree, BuildError> {
    // Group notes by folder. Each unique folder is one leaf-level cluster.
    let mut by_folder: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, n) in notes.iter().enumerate() {
        by_folder.entry(n.folder.clone()).or_default().push(i);
    }
    if by_folder.is_empty() {
        return Err(BuildError::EmptyScope);
    }

    let mut level0: Vec<BuiltClusterNode> = Vec::new();
    for (folder, idxs) in &by_folder {
        let safe_folder = folder.replace('/', "-");
        let id = format!("f-{}", if safe_folder.is_empty() { "_root" } else { &safe_folder });
        let refs: Vec<&[f32]> = idxs
            .iter()
            .map(|&i| notes[i].embedding.as_slice())
            .collect();
        let centroid = mean_normalize(&refs);
        let radius = ninetieth_percentile_distance(&centroid, &refs);
        let members: Vec<String> = idxs.iter().map(|&i| notes[i].id.clone()).collect();
        // Default name is the folder basename per spec.
        let basename = folder.rsplit('/').next().unwrap_or("");
        let default_name = if basename.is_empty() {
            "vault root".to_string()
        } else {
            basename.to_string()
        };
        let SummaryOutput { name, summary, confidence } = match params.summarize {
            SummarizeMode::Llm => {
                let infos: Vec<MemberInfo<'_>> = idxs
                    .iter()
                    .map(|&i| MemberInfo {
                        title: &notes[i].title,
                        summary: &notes[i].summary,
                    })
                    .collect();
                let mut out = run_summarizer(params.summarize, 0, infos, summarizer)?;
                if out.name.is_empty() {
                    out.name = default_name.clone();
                }
                out
            }
            SummarizeMode::None => SummaryOutput {
                name: default_name.clone(),
                summary: String::new(),
                confidence: 1.0,
            },
        };
        level0.push(BuiltClusterNode {
            id,
            members,
            centroid,
            radius,
            // FromFolders trees have confidence 1.0 per the spec: the
            // folder structure is the source of truth, not a guess.
            name,
            summary,
            confidence: confidence.max(1.0),
        });
    }

    // FromFolders is a single-level tree (root synthesized at flatten
    // time per `result_to_node_inserts` when level0.len() > 1).
    Ok(BuiltClusterTree {
        levels: vec![level0],
        outliers: Vec::new(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test-only stand-in for the production `LlmSummarizer`. Returns a
    /// deterministic name derived from member titles so build tests can
    /// assert on tree shape without spinning up an LLM client.
    struct MockSummarizer;

    impl Summarizer for MockSummarizer {
        fn summarize(&self, input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
            let name = if let Some(first) = input.members.first() {
                format!("cluster: {}", first.title)
            } else {
                "empty cluster".to_string()
            };
            Ok(SummaryOutput {
                name,
                summary: format!("{} members", input.members.len()),
                confidence: 0.7,
            })
        }
    }

    fn vec_at(n: usize, dim: usize, base: f32) -> Vec<f32> {
        // Deterministic synthetic embeddings: each "cluster" lives near a
        // distinct corner of the unit hypercube so HDBSCAN separates them.
        let mut v = vec![0.0; dim];
        for i in 0..dim {
            v[i] = base + (n as f32) * 0.01 + (i as f32) * 0.001;
        }
        v
    }

    #[test]
    fn partition_separates_two_obvious_clusters() {
        // 10 points around `base=0.0` and 10 around `base=10.0` — well
        // beyond HDBSCAN's density threshold.
        let mut pts: Vec<Vec<f32>> = (0..10).map(|i| vec_at(i, 8, 0.0)).collect();
        pts.extend((0..10).map(|i| vec_at(i, 8, 10.0)));

        let labels = partition(&pts, 3, None).unwrap();
        assert_eq!(labels.len(), 20);
        // First 10 should share a label; last 10 should share one;
        // the two labels differ. Outliers are tolerated (HDBSCAN can
        // flag edge points) — we only assert majority cohesion.
        let head_label = labels[0..10]
            .iter()
            .map(|a| a.cluster_label)
            .filter(|&l| l != OUTLIER_LABEL)
            .next()
            .expect("first half not all outliers");
        let tail_label = labels[10..20]
            .iter()
            .map(|a| a.cluster_label)
            .filter(|&l| l != OUTLIER_LABEL)
            .next()
            .expect("second half not all outliers");
        assert_ne!(head_label, tail_label, "two halves got the same cluster");
    }

    #[test]
    fn partition_rejects_empty() {
        let r = partition(&Vec::<Vec<f32>>::new(), 5, None);
        assert!(matches!(r, Err(ClusterError::Empty)));
    }

    #[test]
    fn partition_rejects_dim_mismatch() {
        let pts = vec![vec![1.0, 2.0], vec![3.0]];
        let r = partition(&pts, 2, None);
        assert!(matches!(r, Err(ClusterError::DimMismatch { row: 1, .. })));
    }

    #[test]
    fn l2_normalize_unit_length() {
        let n = l2_normalize(&[3.0, 4.0]);
        let norm: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-5);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-5);
        // Zero norm → 0.0, not NaN.
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    fn mk_tree() -> InMemoryTree {
        // Root → two clusters; each cluster has two leaves. Centroids
        // are normalized in 2D for easy reasoning.
        let mut nodes = HashMap::new();
        nodes.insert(
            "root".to_string(),
            ClusterNode {
                id: "root".into(),
                centroid: l2_normalize(&[1.0, 1.0]),
                children: vec!["A".into(), "B".into()],
            },
        );
        nodes.insert(
            "A".into(),
            ClusterNode {
                id: "A".into(),
                centroid: l2_normalize(&[1.0, 0.0]),
                children: vec!["A1".into(), "A2".into()],
            },
        );
        nodes.insert(
            "B".into(),
            ClusterNode {
                id: "B".into(),
                centroid: l2_normalize(&[0.0, 1.0]),
                children: vec!["B1".into(), "B2".into()],
            },
        );
        nodes.insert(
            "A1".into(),
            ClusterNode {
                id: "A1".into(),
                centroid: l2_normalize(&[1.0, 0.1]),
                children: vec![],
            },
        );
        nodes.insert(
            "A2".into(),
            ClusterNode {
                id: "A2".into(),
                centroid: l2_normalize(&[1.0, -0.1]),
                children: vec![],
            },
        );
        nodes.insert(
            "B1".into(),
            ClusterNode {
                id: "B1".into(),
                centroid: l2_normalize(&[0.1, 1.0]),
                children: vec![],
            },
        );
        nodes.insert(
            "B2".into(),
            ClusterNode {
                id: "B2".into(),
                centroid: l2_normalize(&[-0.1, 1.0]),
                children: vec![],
            },
        );
        InMemoryTree {
            root: "root".into(),
            nodes,
        }
    }

    #[test]
    fn beam_descent_picks_nearest_leaf() {
        let tree = mk_tree();
        let query = [1.0, 0.05]; // closest to A1
        let m = place_beam_descent(&query, &tree, 2).expect("non-empty tree");
        assert_eq!(m.leaf_node_id, "A1");
        assert!(m.confidence > 0.9, "confidence={}", m.confidence);
        assert!(m.margin >= 0.0);
    }

    #[test]
    fn beam_descent_finds_other_subtree() {
        let tree = mk_tree();
        let query = [-0.05, 1.0]; // B2 territory
        let m = place_beam_descent(&query, &tree, 2).expect("non-empty tree");
        assert_eq!(m.leaf_node_id, "B2");
    }

    #[test]
    fn sample_merge_plan_single_under_threshold() {
        let members: Vec<String> = (0..15).map(|i| i.to_string()).collect();
        let plan = plan_sample_merge(
            &members,
            SAMPLE_MERGE_BATCH_THRESHOLD,
            SAMPLE_MERGE_BATCH_SIZE,
            SAMPLE_MERGE_MEMBER_CAP,
        );
        assert!(matches!(plan, SampleMergePlan::Single { .. }));
    }

    #[test]
    fn sample_merge_plan_fans_out_above_threshold() {
        let members: Vec<String> = (0..75).map(|i| i.to_string()).collect();
        let plan = plan_sample_merge(&members, 30, 30, 300);
        let SampleMergePlan::SampleAndMerge { batches } = plan else {
            panic!("expected SampleAndMerge, got {plan:?}");
        };
        // 75 / 30 = 3 batches (last partial).
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 30);
        assert_eq!(batches[2].len(), 15);
        let flat_count: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(flat_count, 75);
    }

    #[test]
    fn sample_merge_plan_marks_too_large_above_cap() {
        let members: Vec<String> = (0..400).map(|i| i.to_string()).collect();
        let plan = plan_sample_merge(&members, 30, 30, 300);
        assert!(matches!(
            plan,
            SampleMergePlan::TooLarge { member_count: 400 }
        ));
    }

    fn mk_note(id: &str, folder: &str, base: f32) -> NoteInput {
        // Embedding lives near a corner of the unit hypercube so HDBSCAN
        // can separate them; title/summary feed the summarizer mock.
        NoteInput {
            id: id.into(),
            title: format!("Note {id}"),
            summary: format!("notes about {folder}"),
            folder: folder.into(),
            embedding: vec![base, base + 0.01, base - 0.01, base + 0.005],
        }
    }

    #[test]
    fn build_tree_cluster_method_produces_levels_and_leaves() {
        // Two well-separated clusters of 6 notes each, plus 2 stragglers.
        let mut notes: Vec<NoteInput> = Vec::new();
        for i in 0..6 {
            notes.push(mk_note(&format!("a{i}"), "research", 0.0 + (i as f32) * 0.001));
        }
        for i in 0..6 {
            notes.push(mk_note(&format!("b{i}"), "cooking", 10.0 + (i as f32) * 0.001));
        }
        let params = ClusterParams {
            min_cluster_size: 3,
            // Top-down divisive Split would otherwise re-split each
            // top-level community further; this test asserts on the
            // single-level shape, so disable recursion explicitly.
            disable_recursion: true,
            ..Default::default()
        };
        let summarizer = MockSummarizer;
        let result = build_tree(
            BuildScope::Vault { source_types: Vec::new() },
            BuildMethod::Cluster {
                params: params.clone(),
            },
            &notes,
            &summarizer,
        )
        .unwrap();
        // Top-down build: when there's >1 top-level cluster we add a
        // synthetic vault root at level 1, so the tree has 2 levels:
        // level 0 (leaf clusters) and level 1 (the synthetic root).
        assert!(result.tree.levels.len() >= 1);
        assert!(result.tree.levels[0].len() >= 2);
    }

    #[test]
    fn build_tree_from_folders_one_cluster_per_folder() {
        let notes = vec![
            mk_note("a", "research", 0.0),
            mk_note("b", "research", 0.01),
            mk_note("c", "cooking", 10.0),
            mk_note("d", "cooking", 10.01),
        ];
        let result = build_tree(
            BuildScope::Vault { source_types: Vec::new() },
            BuildMethod::FromFolders {
                params: FolderDeriveParams {
                    summarize: SummarizeMode::None,
                    ..Default::default()
                },
            },
            &notes,
            &MockSummarizer,
        )
        .unwrap();
        // Exactly two folders → two leaf clusters, no outliers.
        assert_eq!(result.tree.levels.len(), 1);
        assert_eq!(result.tree.levels[0].len(), 2);
        assert!(result.tree.outliers.is_empty());
        for n in &result.tree.levels[0] {
            assert!(n.confidence >= 1.0, "FromFolders nodes carry confidence 1.0");
        }
    }

    #[test]
    fn build_and_persist_writes_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let trees = crate::trees::Trees::open(dir.path()).unwrap();
        let mut notes: Vec<NoteInput> = Vec::new();
        for i in 0..6 {
            notes.push(mk_note(&format!("a{i}"), "research", 0.0 + (i as f32) * 0.001));
        }
        for i in 0..6 {
            notes.push(mk_note(&format!("b{i}"), "cooking", 10.0 + (i as f32) * 0.001));
        }
        let params = ClusterParams {
            min_cluster_size: 3,
            disable_recursion: true,
            ..Default::default()
        };
        let summarizer = MockSummarizer;
        let tree_id = build_and_persist(
            &trees,
            "test build",
            "one-shot",
            BuildScope::Vault { source_types: Vec::new() },
            BuildMethod::Cluster { params },
            &notes,
            &summarizer,
        )
        .unwrap();
        let row = trees.get_tree(&tree_id).unwrap().unwrap();
        assert_eq!(row.state, "draft");
        let nodes = trees.list_nodes(&tree_id).unwrap();
        assert!(nodes.iter().any(|n| matches!(n.kind, crate::trees::NodeKind::Cluster)));
        assert!(nodes.iter().any(|n| matches!(n.kind, crate::trees::NodeKind::Leaf)));
    }

    #[test]
    fn partition_leiden_separates_two_obvious_clusters() {
        // Two cosine-orthogonal groups of 10 points each. Leiden runs
        // over L2-normalized embeddings, so cluster separation has to
        // be expressed in direction rather than magnitude — the
        // HDBSCAN fixture's `vec_at` puts every point in the same
        // direction after normalization, which doesn't exercise Leiden
        // meaningfully.
        let mut pts: Vec<Vec<f32>> = Vec::new();
        for i in 0..10 {
            // Cluster A — direction (1, 0, ...) with small noise.
            let mut v = vec![0.0_f32; 8];
            v[0] = 1.0;
            v[1] = (i as f32) * 0.005;
            pts.push(v);
        }
        for i in 0..10 {
            // Cluster B — direction (0, 1, ...) with small noise.
            let mut v = vec![0.0_f32; 8];
            v[1] = 1.0;
            v[0] = (i as f32) * 0.005;
            pts.push(v);
        }

        let leiden = LeidenParams {
            k_nearest: 5,
            edge_weight_floor: 0.0,
            iterations: 100,
            min_cluster_size: 2,
            resolution: 1.0,
            top_level_resolution: 0.3,
        };
        let labels = partition_leiden(&pts, &leiden).unwrap();
        assert_eq!(labels.len(), 20);
        // Majority cohesion: the most common non-outlier label in the
        // first half should differ from the most common in the second
        // half. The grouping itself comes out of modularity
        // optimization, not density estimates, so every point should
        // be placed.
        fn majority_label(slice: &[ClusterAssignment]) -> i32 {
            use std::collections::HashMap;
            let mut counts: HashMap<i32, usize> = HashMap::new();
            for a in slice {
                if a.cluster_label == OUTLIER_LABEL {
                    continue;
                }
                *counts.entry(a.cluster_label).or_default() += 1;
            }
            counts
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(l, _)| l)
                .expect("at least one non-outlier in slice")
        }
        let head_label = majority_label(&labels[0..10]);
        let tail_label = majority_label(&labels[10..20]);
        assert_ne!(
            head_label, tail_label,
            "leiden put both halves into the same community"
        );
    }

    #[test]
    fn partition_leiden_rejects_empty() {
        let r = partition_leiden(&Vec::<Vec<f32>>::new(), &LeidenParams::default());
        assert!(matches!(r, Err(ClusterError::Empty)));
    }

    #[test]
    fn beam_descent_k1_is_greedy() {
        // With beam_width=1 the descent locks into the top child at
        // each level. For our tree the query is unambiguous so it lands
        // at the same leaf as K=2; the test really just guards against
        // panics when K=1 forces a single-element beam.
        let tree = mk_tree();
        let m = place_beam_descent(&[1.0, 0.0], &tree, 1).unwrap();
        assert!(m.leaf_node_id == "A1" || m.leaf_node_id == "A2");
    }
}
