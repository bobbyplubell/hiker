//! Clustering primitives — HDBSCAN partitioning and online placement
//! against an already-built tree. See `docs/clustering.md` for the full
//! spec. This module owns every `petal-clustering` import; outside
//! callers consume plain Rust types so the algorithm choice is a one-
//! file swap (mirrors `core::store` and `core::embed` discipline — see
//! `cluster-module-discipline`).
//!
//! Submodule layout:
//!
//! - `algo`   — pure clustering math: `partition` (HDBSCAN),
//!              `partition_leiden`, `l2_normalize`, `cosine_similarity`,
//!              `mean_normalize`, `ninetieth_percentile_distance`.
//! - `tree`   — online placement: `place_beam_descent` over `TreeView`.
//! - `build`  — offline build pipeline + persistence: `build_tree`,
//!              `build_and_persist`, `rebuild_and_persist`,
//!              `build_tree_structural`, plus the divisive top-down
//!              recipe and the FromFolders alternative.
//!
//! status: cluster-module-discipline
//! status: cluster-hdbscan-crate-petal
//! status: cluster-place-beam-descent
//! status: cluster-leiden
//! status: cluster-leiden-crate-single-clustering

use serde::{Deserialize, Serialize};

pub mod algo;
pub mod build;
pub mod tree;

#[cfg(test)]
mod tests;

pub use algo::{
    cosine_similarity, l2_normalize, mean_normalize, ninetieth_percentile_distance, partition,
    partition_leiden,
};
pub use build::{
    build_and_persist, build_tree, build_tree_structural, rebuild_and_persist,
    result_to_node_inserts_pub, NoopSummarizer,
};
pub use tree::place_beam_descent;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClusterAlgorithm {
    #[default]
    Hdbscan,
    Gmm,
    Hybrid,
    /// Leiden community detection on a kNN cosine-similarity graph. Per
    /// `cluster-leiden`. Lands as an opt-in alternative to HDBSCAN for
    /// vaults where density-based clustering produces 0-1 cohesive
    /// cluster + everything-as-outliers.
    Leiden,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SummarizeMode {
    #[default]
    Llm,
    None,
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
