//! Offline build pipeline: cluster → summarize → flatten, plus the
//! FromFolders alternate method. The pipeline returns the neutral
//! `BuiltClusterTree` only — converting that into the tree-storage
//! representation (`Db` rows) and the `persist` / `rebuild_and_persist`
//! wrappers live on the storage side in `crate::trees::build_adapter`,
//! so the clustering algorithm never reaches up into `trees::types`.
//! The placement classifier in `tree.rs` shares the `Node` shape but
//! lives on the online path; this module produces the richer
//! `BuiltClusterNode` described in `clustering.md` §"Output: what
//! suggestions consume".
//!
//! status: cluster-build-recursive
//! status: cluster-tree-output
//! status: cluster-build-from-folders

pub mod stream;

use super::algo::{
    build_leiden_graph, cosine_similarity, l2_normalize, leiden_communities, mean_normalize,
    ninetieth_percentile_distance, partition, partition_leiden, LeidenGraph,
};
use super::{
    BuildError, BuildMethod, BuildResult, BuildScope, BuiltClusterNode, BuiltClusterTree,
    Algorithm, Assignment, Error, Id, Params,
    FolderDeriveParams, MemberInfo, NoteInput, OUTLIER_LABEL, Phase, SummarizeInput, SummarizeMode,
    SummaryOutput, Summarizer,
};

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
pub fn tree(
    scope: BuildScope,
    method: BuildMethod,
    notes: &[NoteInput],
    summarizer: &dyn Summarizer,
) -> Result<BuildResult, BuildError> {
    if notes.is_empty() {
        return Err(BuildError::EmptyScope);
    }
    let tree = match &method {
        BuildMethod::Cluster { params } => {
            let mut sctx = StreamCtx {
                tx: None,
                cancel: Arc::new(AtomicBool::new(false)),
                items_processed: 0,
                clusters_found: 0,
                outliers: 0,
                partition_loop_counter: 0,
                max_partition_level_emitted: -1,
            };
            // Blocking entry: no cross-run graph cache to thread through.
            build_cluster_tree(notes, params, summarizer, &mut sctx, &mut None)?
        }
        BuildMethod::FromFolders { params } => build_from_folders(notes, params, summarizer)?,
    };
    Ok(BuildResult {
        scope,
        method,
        tree,
    })
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

use self::stream::StreamCtx;
use std::cell::RefCell;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Outcome of a partition pass: communities keyed by label, mapped to the
/// member indices in each, plus the outlier indices the partitioner peeled
/// off (folded into the first child after recursion).
type PartitionSplit = (std::collections::BTreeMap<i32, Vec<usize>>, Vec<usize>);

/// Run the structural build. `top_graph` is an in/out cache for the
/// top-level Leiden kNN graph: pass `Some` to reuse a graph from a prior
/// run (skipping the O(n²) kNN sweep when `k_nearest` / `edge_weight_floor`
/// are unchanged), and on return it holds the graph actually used so the
/// caller can cache it for next time. `None`/unchanged for HDBSCAN and
/// FromFolders. Per `cluster-review-tab-live-preview`.
pub(super) fn build_cluster_tree(
    notes: &[NoteInput],
    params: &Params,
    summarizer: &dyn Summarizer,
    sctx: &mut StreamCtx,
    top_graph: &mut Option<Arc<LeidenGraph>>,
) -> Result<BuiltClusterTree, BuildError> {
    let builder = Builder {
        notes,
        params,
        summarizer,
        top_graph: RefCell::new(top_graph.take()),
    };
    let tree = builder.run(sctx)?;
    *top_graph = builder.top_graph.into_inner();
    Ok(tree)
}

/// Borrow-bundle for the build entry point. Splitting `run` into
/// `&self` methods keeps each phase under the cognitive-complexity
/// budget while sharing `notes`, `params`, and the summarizer with no
/// free-helper sprawl.
struct Builder<'a> {
    notes: &'a [NoteInput],
    params: &'a Params,
    summarizer: &'a dyn Summarizer,
    /// In/out cache for the top-level Leiden kNN graph (interior
    /// mutability so the `&self` recursion methods can populate it). Built
    /// lazily by `top_level_leiden_graph` on the first top-level Leiden
    /// cut and reused across the resolution-escalation retries; lifted out
    /// by `build_cluster_tree` for cross-run caching.
    top_graph: RefCell<Option<Arc<LeidenGraph>>>,
}

impl<'a> Builder<'a> {
    fn run(&self, sctx: &mut StreamCtx) -> Result<BuiltClusterTree, BuildError> {
        // GMM isn't wired yet (linfa-clustering doesn't ship HDBSCAN;
        // see `clustering.md` §"Crate choice"). Producers requesting
        // `Gmm` fall back to `Hdbscan` on every Split call.
        //
        // status: cluster-algorithm-selectable (partial — gmm path stubbed)
        if matches!(self.params.algorithm, Algorithm::Gmm) {
            tracing::warn!(
                "cluster: gmm algorithm not yet supported; falling back to hdbscan"
            );
        }
        tracing::info!(
            algorithm = ?self.params.algorithm,
            note_count = self.notes.len(),
            recurse = !self.params.disable_recursion,
            leaf_min_size = self.params.leaf_min_size,
            leaf_cohesion_threshold = self.params.leaf_cohesion_threshold,
            top_level_resolution = self.params.leiden.top_level_resolution,
            resolution = self.params.leiden.resolution,
            include_outliers = self.params.include_outliers,
            "cluster: build recipe entry — top-down divisive Split from virtual root"
        );
        let (top_groups, top_outliers) = self.top_level_split(sctx)?;
        let top_level_nodes = self.build_top_level_nodes(top_groups, sctx)?;
        let outlier_ids: Vec<String> = if self.params.include_outliers {
            top_outliers.iter().map(|&i| self.notes[i].id.clone()).collect()
        } else {
            // By this point every outlier was force-routed into a
            // cluster via the recovery pass; anything still here is a
            // degenerate case (zero centroids, etc.). Drop it so the
            // output doesn't contradict `include_outliers = false`.
            Vec::new()
        };
        sctx.check_cancel()?;
        sctx.emit_phase(Phase::Finalizing);
        let ctx = self.split_branch_ctx();
        let tree = ctx.flatten_split_forest(top_level_nodes, outlier_ids);
        sctx.emit_counters();
        tracing::info!(
            total_levels = tree.levels.len(),
            per_level_counts = ?tree.levels.iter().map(std::vec::Vec::len).collect::<Vec<_>>(),
            outliers = tree.outliers.len(),
            "cluster: build recipe finished"
        );
        Ok(tree)
    }

    /// ── Step 1: top-level Split against the virtual root ─────────
    /// The first Split is special:
    ///   - Uses `top_level_resolution` (Leiden only) for a coarser cut.
    ///   - Handles outliers (Hybrid / `include_outliers = false`) by
    ///     force-routing them into the nearest top-level community.
    ///   - Requires at least 2 cohesive communities (else
    ///     `VaultTooSmall`).
    ///
    /// Sub-splits below use the regular `resolution` and silently
    /// fold outliers into a per-branch outlier list (the spec
    /// doesn't ask for recursive Hybrid recovery).
    fn top_level_split(
        &self,
        sctx: &mut StreamCtx,
    ) -> Result<PartitionSplit, BuildError> {
        sctx.check_cancel()?;
        sctx.emit_partition_phase_if_new(0);
        let indices: Vec<usize> = (0..self.notes.len()).collect();
        let (mut top_groups, mut top_outliers) =
            self.partition_top_level_escalating(&indices, sctx)?;
        if top_groups.len() < 2 {
            return Err(BuildError::VaultTooSmall {
                found: self.notes.len(),
            });
        }
        // Hybrid / force-routing applies only at the top level.
        let hybrid_recovery_for_algo = matches!(self.params.algorithm, Algorithm::Hybrid)
            && !matches!(self.params.algorithm, Algorithm::Leiden);
        if hybrid_recovery_for_algo || !self.params.include_outliers {
            top_outliers = self.recover_outliers(&mut top_groups, &top_outliers);
        }
        tracing::info!(
            top_level_clusters = top_groups.len(),
            outliers = top_outliers.len(),
            "cluster: top-level Split produced communities"
        );
        // Surface the outlier count to the progress stream now that
        // the top-level Split has settled.
        sctx.outliers = top_outliers.len() as u32;
        sctx.emit_counters();
        Ok((top_groups, top_outliers))
    }

    /// Run the top-level partition and group the assignments into
    /// `(communities, outliers)`.
    ///
    /// For Leiden, the kNN graph plus a low γ can make a single all-notes
    /// community the RB-quality optimum (the classic "γ too low → one
    /// giant community" collapse that surfaced as a `VaultTooSmall`
    /// abort). When the cut yields fewer than 2 communities we escalate
    /// `top_level_resolution` geometrically and retry, up to
    /// `MAX_ESCALATIONS` times, before letting the caller raise the error.
    /// Crucially the kNN graph is built (or reused from cache) **once** and
    /// every γ retry runs `leiden_communities` over that same graph — the
    /// escalation costs near-nothing, and a cached graph from a prior run
    /// skips the O(n²) sweep entirely.
    ///
    /// HDBSCAN (and any non-Leiden algorithm) runs exactly once — its
    /// density model has no resolution knob to bump.
    fn partition_top_level_escalating(
        &self,
        indices: &[usize],
        sctx: &mut StreamCtx,
    ) -> Result<PartitionSplit, BuildError> {
        const MAX_ESCALATIONS: u32 = 4;
        const ESCALATION_FACTOR: f32 = 1.6;

        if !matches!(self.params.algorithm, Algorithm::Leiden) {
            sctx.check_cancel()?;
            let assignments =
                partition_indices(self.notes, indices, self.params, /* top_level */ true)?;
            sctx.check_cancel()?;
            return Ok(group_assignments(&assignments, indices));
        }

        let lp = &self.params.leiden;
        let graph = self.top_level_leiden_graph(indices)?;
        let mut gamma = lp.top_level_resolution;
        let mut attempt: u32 = 0;
        loop {
            sctx.check_cancel()?;
            let assignments = leiden_communities(&graph, gamma, lp.iterations, lp.min_cluster_size)
                .map_err(|e| BuildError::Compute(format!("leiden: {e}")))?;
            let (groups, outliers) = group_assignments(&assignments, indices);
            if groups.len() >= 2 || attempt >= MAX_ESCALATIONS {
                return Ok((groups, outliers));
            }
            attempt += 1;
            let next = gamma.max(0.1) * ESCALATION_FACTOR;
            tracing::info!(
                attempt,
                from = gamma,
                to = next,
                communities = groups.len(),
                "cluster: top-level Leiden cut produced <2 communities — escalating resolution (graph reused)"
            );
            gamma = next;
        }
    }

    /// The top-level Leiden kNN graph: reuse the cached one if it was built
    /// from the same node set and `k_nearest` / `edge_weight_floor`,
    /// otherwise build it (the O(n²) sweep) and cache it back for the next
    /// run. Per `cluster-review-tab-live-preview`.
    fn top_level_leiden_graph(&self, indices: &[usize]) -> Result<Arc<LeidenGraph>, BuildError> {
        let lp = &self.params.leiden;
        if let Some(g) = self.top_graph.borrow().as_ref()
            && g.matches(indices.len(), lp.k_nearest, lp.edge_weight_floor)
        {
            return Ok(g.clone());
        }
        let embeddings: Vec<Vec<f32>> =
            indices.iter().map(|&i| self.notes[i].embedding.clone()).collect();
        let g = Arc::new(build_leiden_graph(&embeddings, lp.k_nearest, lp.edge_weight_floor)?);
        *self.top_graph.borrow_mut() = Some(g.clone());
        Ok(g)
    }

    /// `include_outliers = false` → force-route every outlier into
    /// its nearest cluster (threshold `-1.0` admits everything). Per
    /// `cluster-build-cluster-method`'s "outlier recovery loop with
    /// cosine threshold dropped to -1.0" requirement.
    fn recover_outliers(
        &self,
        top_groups: &mut std::collections::BTreeMap<i32, Vec<usize>>,
        outliers: &[usize],
    ) -> Vec<usize> {
        let interim_centroids: std::collections::BTreeMap<i32, Vec<f32>> = top_groups
            .iter()
            .map(|(label, idxs)| {
                let refs: Vec<&[f32]> = idxs
                    .iter()
                    .map(|&i| self.notes[i].embedding.as_slice())
                    .collect();
                (*label, mean_normalize(&refs))
            })
            .collect();
        let threshold: f32 = if !self.params.include_outliers { -1.0 } else { 0.6 };
        let mut still_outliers: Vec<usize> = Vec::new();
        for &i in outliers {
            let q = l2_normalize(&self.notes[i].embedding);
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
        still_outliers
    }

    fn split_branch_ctx(&self) -> SplitBranchCtx<'a> {
        // Build a `SplitNode` per top-level community. Recursively
        // sub-split unless `disable_recursion` is set; the recursion
        // stops per-branch on `leaf_min_size` /
        // `leaf_cohesion_threshold` / 16-level cap.
        const MAX_DEPTH: u8 = 16;
        SplitBranchCtx {
            notes: self.notes,
            params: self.params,
            summarizer: self.summarizer,
            recurse: !self.params.disable_recursion,
            max_depth: MAX_DEPTH,
        }
    }

    fn build_top_level_nodes(
        &self,
        top_groups: std::collections::BTreeMap<i32, Vec<usize>>,
        sctx: &mut StreamCtx,
    ) -> Result<Vec<SplitNode>, BuildError> {
        let ctx = self.split_branch_ctx();
        ctx.split_top_level_groups(top_groups, sctx)
    }
}

/// Group a partition's `Assignment`s into `(communities, outliers)`,
/// translating each local `point_index` back to the global `notes` index
/// via `indices`. Communities keyed by label in a `BTreeMap` for stable
/// ordering; the `OUTLIER_LABEL` bucket is split out separately.
fn group_assignments(assignments: &[Assignment], indices: &[usize]) -> PartitionSplit {
    let mut groups: std::collections::BTreeMap<i32, Vec<usize>> =
        std::collections::BTreeMap::new();
    for a in assignments {
        groups
            .entry(a.cluster_label)
            .or_default()
            .push(indices[a.point_index]);
    }
    let outliers = groups.remove(&OUTLIER_LABEL).unwrap_or_default();
    (groups, outliers)
}

/// Partition the `indices` subset of `notes` by their embeddings. The
/// `top_level` flag swaps in `LeidenParams.top_level_resolution` for
/// `resolution`; sub-splits get the normal `resolution`. Returns the
/// partitioner's `Assignment`s with `point_index` indexing into
/// the *local* `indices` slice (i.e. 0..indices.len()) — callers
/// translate back to global `notes` indices themselves.
fn partition_indices(
    notes: &[NoteInput],
    indices: &[usize],
    params: &Params,
    top_level: bool,
) -> Result<Vec<Assignment>, Error> {
    let embeddings: Vec<Vec<f32>> =
        indices.iter().map(|&i| notes[i].embedding.clone()).collect();
    match params.algorithm {
        Algorithm::Leiden => {
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
///
/// Borrow-bundle of the invariants that stay fixed across every
/// recursion frame of `recursive_split_branch`: the notes slice,
/// cluster params, summarizer trait object, whether sub-splits run,
/// and the recursion cap. Only `id`, `member_idxs`, and `depth`
/// differ per frame.
struct SplitBranchCtx<'a> {
    notes: &'a [NoteInput],
    params: &'a Params,
    summarizer: &'a dyn Summarizer,
    recurse: bool,
    max_depth: u8,
}

impl<'a> SplitBranchCtx<'a> {
    fn split_top_level_groups(
        &self,
        top_groups: std::collections::BTreeMap<i32, Vec<usize>>,
        sctx: &mut StreamCtx,
    ) -> Result<Vec<SplitNode>, BuildError> {
        let mut out: Vec<SplitNode> = Vec::new();
        for (label, idxs) in top_groups.into_iter() {
            // Per-cluster cancellation check at the top level — `idxs`
            // may be large and `recursive_split_branch` may not return
            // for a while if the sub-split is deep.
            sctx.check_cancel()?;
            let id = format!("c0-{label}");
            // Top-level clusters have `parent = None` per
            // `cluster-build-progress-stream`.
            let node = self.recursive_split_branch(id, &idxs, /* depth */ 1, &None, sctx)?;
            out.push(node);
        }
        Ok(out)
    }

    /// Flatten the top-down divisive forest into a `BuiltClusterTree`. The
    /// `levels` contract per `cluster-tree-output`:
    ///
    /// - `levels[0]` = leaf clusters (`members` are note ids).
    /// - `levels[k>0]` = parent clusters (`members` are child cluster ids).
    /// - `levels.last()` = top-level (root candidates).
    fn flatten_split_forest(
        &self,
        top_level: Vec<SplitNode>,
        outliers: Vec<String>,
    ) -> BuiltClusterTree {
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
        // (`trees::build_adapter::node_inserts`) treats it as root naturally.
        if top_ids.len() > 1 {
            let refs: Vec<&[f32]> =
                top_centroids.iter().map(std::vec::Vec::as_slice).collect();
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
}

/// Per-frame state for one `recursive_split_branch` invocation.
/// Centroid / radius / summary are computed once at frame entry; the
/// `emit_leaf` and child-handling methods read them off `self` so the
/// recursion body stays focussed on control flow instead of plumbing.
struct BranchFrame<'a, 'b> {
    ctx: &'a SplitBranchCtx<'b>,
    id: String,
    member_idxs: &'a [usize],
    depth: u8,
    parent_id: Option<Id>,
    centroid: Vec<f32>,
    radius: f32,
    name: String,
    summary: String,
    confidence: f32,
}

impl<'b> SplitBranchCtx<'b> {
    /// Build a `BranchFrame` for this recursion level. Pre-computes
    /// centroid / radius / summary so the recursion body in
    /// `recursive_split_branch` can stay focussed on control flow.
    fn open_branch<'a>(
        &'a self,
        id: String,
        member_idxs: &'a [usize],
        depth: u8,
        parent_id: Option<Id>,
    ) -> Result<BranchFrame<'a, 'b>, BuildError> {
        let ctx = self;
        let notes = ctx.notes;
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
        // Summarize at this cluster's level (depth from top, 0-indexed
        // for the summarizer's `level` field — keeps the LLM prompt
        // shape consistent with prior pipeline).
        let SummaryOutput {
            name,
            summary,
            confidence,
        } = run_summarizer(
            ctx.params.summarize,
            depth as usize - 1,
            infos,
            ctx.summarizer,
        )?;
        Ok(BranchFrame {
            ctx,
            id,
            member_idxs,
            depth,
            parent_id,
            centroid,
            radius,
            name,
            summary,
            confidence,
        })
    }
}

impl<'a, 'b> BranchFrame<'a, 'b> {
    /// Emit the leaf cluster event + return the `Leaf` node. Called
    /// from every "branch decided to be a leaf" exit point.
    /// status: cluster-build-progress-stream
    fn emit_leaf(&self, sctx: &mut StreamCtx) -> SplitNode {
        let notes = self.ctx.notes;
        let note_ids: Vec<String> =
            self.member_idxs.iter().map(|&i| notes[i].id.clone()).collect();
        sctx.items_processed = sctx.items_processed.saturating_add(note_ids.len() as u32);
        sctx.emit_cluster(
            BuiltClusterNode {
                id: self.id.clone(),
                members: note_ids.clone(),
                centroid: self.centroid.clone(),
                radius: self.radius,
                name: self.name.clone(),
                summary: self.summary.clone(),
                confidence: self.confidence,
            },
            self.parent_id.clone(),
        );
        sctx.emit_counters();
        SplitNode::Leaf {
            id: self.id.clone(),
            centroid: self.centroid.clone(),
            radius: self.radius,
            name: self.name.clone(),
            summary: self.summary.clone(),
            confidence: self.confidence,
            note_ids,
        }
    }

    /// Stop-condition check. Returns `Some(leaf)` if this frame should
    /// emit a leaf rather than sub-split.
    fn try_stop(&self, sctx: &mut StreamCtx) -> Option<SplitNode> {
        let params = self.ctx.params;
        let too_small = self.member_idxs.len() <= params.leaf_min_size as usize;
        let too_tight = self.radius < params.leaf_cohesion_threshold;
        let at_cap = self.depth >= self.ctx.max_depth;
        if !self.ctx.recurse || too_small || too_tight || at_cap {
            let reason = if !self.ctx.recurse {
                "disable_recursion"
            } else if at_cap {
                "16-level cap"
            } else if too_small {
                "member_count <= leaf_min_size"
            } else {
                "radius < leaf_cohesion_threshold"
            };
            tracing::debug!(
                id = %self.id,
                depth = self.depth,
                members = self.member_idxs.len(),
                radius = self.radius,
                reason,
                "cluster: branch stopped — emitting leaf cluster"
            );
            return Some(self.emit_leaf(sctx));
        }
        None
    }

    /// Run the sub-split partition. `Err` here means the partitioner
    /// itself errored; a `Ok` with `<2` communities still means "leaf"
    /// — the caller checks. Outliers are split out so the caller can
    /// fold them into the first child after recursion.
    fn sub_split(
        &self,
        sctx: &mut StreamCtx,
    ) -> Result<PartitionSplit, Error> {
        let sub_assignments = partition_indices(
            self.ctx.notes,
            self.member_idxs,
            self.ctx.params,
            /* top_level */ false,
        )?;
        let mut sub_groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for a in &sub_assignments {
            // Periodic per-node cancellation check inside the
            // partition assignment loop, per
            // `cluster-build-async-pass`. Cheap atomic load amortized
            // via `PARTITION_CHECK_INTERVAL`.
            if sctx.check_cancel_periodic().is_err() {
                // Cancellation surfaces through `recursive_split_branch`
                // directly; here we just bail.
                break;
            }
            if a.cluster_label == OUTLIER_LABEL {
                // Per spec, sub-splits don't run a Hybrid-style
                // recovery; outliers at this level fold back into the
                // *parent* cluster as plain members.
                continue;
            }
            sub_groups
                .entry(a.cluster_label)
                .or_default()
                .push(self.member_idxs[a.point_index]);
        }
        let sub_outlier_local: Vec<usize> = sub_assignments
            .iter()
            .filter(|a| a.cluster_label == OUTLIER_LABEL)
            .map(|a| self.member_idxs[a.point_index])
            .collect();
        Ok((sub_groups, sub_outlier_local))
    }

    /// Finalize a branch with `children` already built and any
    /// sub-level outliers folded in. Emits the `ClusterDiscovered`
    /// event in child-first order.
    fn finalize_branch(self, children: Vec<SplitNode>, sctx: &mut StreamCtx) -> SplitNode {
        // status: cluster-build-progress-stream
        let child_ids: Vec<String> = children
            .iter()
            .map(|node| match node {
                SplitNode::Leaf { id, .. } | SplitNode::Branch { id, .. } => id.clone(),
            })
            .collect();
        sctx.emit_cluster(
            BuiltClusterNode {
                id: self.id.clone(),
                members: child_ids,
                centroid: self.centroid.clone(),
                radius: self.radius,
                name: self.name.clone(),
                summary: self.summary.clone(),
                confidence: self.confidence,
            },
            self.parent_id.clone(),
        );
        sctx.emit_counters();
        SplitNode::Branch {
            id: self.id,
            centroid: self.centroid,
            radius: self.radius,
            name: self.name,
            summary: self.summary,
            confidence: self.confidence,
            children,
        }
    }
}

impl<'a> SplitBranchCtx<'a> {
    fn recursive_split_branch(
        &self,
        id: String,
        member_idxs: &[usize],
        depth: u8,
        parent_id: &Option<Id>,
        sctx: &mut StreamCtx,
    ) -> Result<SplitNode, BuildError> {
        // Level-boundary cancellation check on every recursion frame
        // entry.
        sctx.check_cancel()?;
        sctx.emit_partition_phase_if_new(depth as u32);
        let frame = self.open_branch(id, member_idxs, depth, parent_id.clone())?;
        if let Some(leaf) = frame.try_stop(sctx) {
            return Ok(leaf);
        }
        // Recursive sub-split using the normal `resolution`. If the
        // partitioner errors, the branch can't be refined further —
        // emit a leaf cluster instead. We don't propagate the
        // partition error: a sub-split is allowed to fail to refine
        // without aborting the whole build (the per-branch outcome is
        // "this stays a leaf cluster").
        let (sub_groups, sub_outlier_local) = match frame.sub_split(sctx) {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(
                    id = %frame.id,
                    depth = frame.depth,
                    error = %e,
                    "cluster: sub-split partition errored — emitting leaf cluster"
                );
                return Ok(frame.emit_leaf(sctx));
            }
        };
        if sub_groups.len() < 2 {
            tracing::debug!(
                id = %frame.id,
                depth = frame.depth,
                members = frame.member_idxs.len(),
                sub_communities = sub_groups.len(),
                "cluster: sub-split produced <2 communities — emitting leaf cluster"
            );
            return Ok(frame.emit_leaf(sctx));
        }
        tracing::debug!(
            id = %frame.id,
            depth = frame.depth,
            sub_communities = sub_groups.len(),
            sub_outliers = sub_outlier_local.len(),
            "cluster: sub-split accepted"
        );
        let mut children: Vec<SplitNode> = Vec::new();
        for (label, child_idxs) in sub_groups.into_iter() {
            sctx.check_cancel()?;
            let child_id = format!("{}-s{label}", frame.id);
            let child = self.recursive_split_branch(
                child_id,
                &child_idxs,
                frame.depth + 1,
                &Some(frame.id.clone()),
                sctx,
            )?;
            children.push(child);
        }
        // Sub-level outliers are folded into the first child cluster
        // as plain members so they remain reachable in the persisted
        // tree. Matches the build recipe's "every note gets a home
        // under the top-level community" intent.
        if !sub_outlier_local.is_empty()
            && let Some(first) = children.first_mut()
        {
            fold_into_first_leaf(first, &sub_outlier_local, self.notes);
        }
        Ok(frame.finalize_branch(children, sctx))
    }
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
/// To keep `trees::build_adapter::node_inserts`'s "synthesize a root iff top.len() != 1"
/// machinery happy when the virtual-root Split produces top-level
/// clusters at different levels (which happens when some branches went
/// deeper than others), we **always** add a synthetic vault root to
/// `levels` when there is more than one top-level cluster. The root's
/// `members` are the top-level cluster ids. When there is exactly one
/// top-level cluster (theoretically impossible since we error
/// `VaultTooSmall` below 2 communities, but defended for safety), it
/// becomes the natural root.
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
        // are left blank here; the caller (`tree_structural`)
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
/// (`tree_structural`). Cannot actually be invoked because the
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
pub fn tree_structural(
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
    let mut result = tree(scope, forced_method, notes, &noop)?;
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

pub(super) fn build_from_folders(
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
    // time per `trees::build_adapter::node_inserts` when level0.len() > 1).
    Ok(BuiltClusterTree {
        levels: vec![level0],
        outliers: Vec::new(),
    })
}
