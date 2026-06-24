# Clustering

How Hiker builds a hierarchical tree of topics from an unorganized vault. This doc covers the *build* side only — turning embeddings into a tree of named clusters. The tree is consumed by `cluster-editor.md` as a durable, user-authored organizational layer whose per-cluster policies (Tag / Move / Freeze) drive tagging and moving of notes. The filesystem still holds the notes' bytes — a Move policy really renames the file, a Tag policy really writes frontmatter — but the tree is the organizing structure the user curates over them.

Not built in v1. Lands alongside the cluster editor (post-v1, after the related-notes panel proves the index pipeline).


## Operations framework

Tree construction and edits compose from three primitive operations, each a cluster-editor verb; the build recipe (below) is a canonical composition.

| Op | Signature | Effect |
| --- | --- | --- |
| **Split** | `Trees::split_cluster(target, params)` | Partitions a target cluster's leaves into child sub-clusters using `params.algorithm`. The target can be a real cluster node or a virtual root containing every note in the build scope. Recursive sub-splits are governed by `leaf_min_size` / `leaf_cohesion_threshold`. [cluster-op-split] |
| **Summarize** | `Trees::summarize(scope, params)` | Generates `name` + `summary` for every cluster matching `scope`. Decoupled from Split — clusters Split produces have placeholder names until Summarize runs against them. Idempotent on `StaleOrUnfilled`. [cluster-op-summarize-sweep] |
| **Roll-up** | `Trees::rollup(input_node_ids, params)` | Embeds each input cluster's `summary` text via `Embedder::embed_batch`, partitions those summary embeddings, and inserts the resulting groups as a new parent layer above the inputs. Requires every input cluster to have a non-empty `summary` (errors `MissingSummary { node_id }` otherwise). [cluster-op-rollup] |

[cluster-op-split]
status:: done
implements:: [[code:hiker/trees/ops/split/impl#[Db]split_cluster]]
note:: `core/src/trees/ops/split.rs::Trees::split_cluster` is the orchestration entry. Resolves the target (virtual root vs real cluster node) into a leaf set, fetches embeddings via a caller-supplied `Fn(&str) -> Option<Vec<f32>>` resolver (keeps `core::trees` free of `core::store` imports), runs `core::cluster::partition` / `partition_leiden` against the embeddings, inserts one new sub-cluster row per HDBSCAN/Leiden community, and reparents the affected leaves. Recursive sub-split when `params.recurse = true`: each newly-produced child whose member count > `params.leaf_min_size` (default 5) AND whose intra-cluster cohesion radius > `params.leaf_cohesion_threshold` (default 0.15) is recursively split. Hard 16-level cap. Top-level virtual-root Leiden call uses `params.leiden.top_level_resolution` (default 1.0); recursive sub-splits revert to `params.leiden.resolution` (1.0). One `record_split` history row per level. Command `cluster_op_split` lifted to a thin host wrapper. New `ClusterParams` fields: `recurse: bool` (default false, `#[serde(default)]`), `leaf_min_size: u32` (default 5), `leaf_cohesion_threshold: f32` (default 0.15). New `LeidenParams.top_level_resolution: f32` (default 1.0, `#[serde(default)]`). `SplitOutcome { new_clusters, total_levels, outliers }` returned. Helpers `core::cluster::mean_normalize` and `core::cluster::ninetieth_percentile_distance` lifted to `pub` for the radius check. **Note:** virtual-root recursion is deferred until the build recipe lands (the children of a virtual-root split don't yet have leaf rows to walk through); top-level virtual-root invocation is supported, and user-driven real-node recursion works end-to-end

### Split

`Split` partitions a target's leaves and reparents them under new children, preserving the target's own row (name, summary, policy). Inputs: `target_node_id` (or a virtual-root sentinel for the build scope) and `Params` (algorithm + tunables + recursion flags + leaf stop conditions).

Algorithm choice drives the partition (§"Algorithm choices"): Leiden default, HDBSCAN / Hybrid / GMM-stub selectable. Split is flat by default, recursion opt-in. Each level operates on actual note embeddings within the parent's member set, not geometric centroids (§"Note embeddings input"), so re-clustering already-distinct centroids never arises.

Recursion is **two bools** on `Params`: `disable_recursion` and `recurse`. The default is a single flat partition; `recurse` opts into descending every branch until a stop condition trips (per-branch member count below `leaf_min_size` default 5, or cohesion radius below `leaf_cohesion_threshold` default 0.15). The build recipe defaults to flat; the user opts into recursion in the review tab or deepens by hand. [cluster-op-split]
implements:: [[code:hiker/cluster/build/impl#[`SplitBranchCtx<'a>`]recursive_split_branch]], [[code:hiker/cluster/build/impl#[`SplitBranchCtx<'b>`]open_branch]], [[code:hiker/cluster/build/impl#[`BranchFrame<'a, 'b>`]try_stop]], [[code:hiker/cluster/build/impl#[`BranchFrame<'a, 'b>`]sub_split]], [[code:hiker/trees/ops/split/impl#[Db]split_cluster_recursive]]

PLANNED (`bug-clustering-params-spec-drift`): Split takes a `recursion` mode — Flat (default, a single top-level partition), Manual (row-menu Split, one level), or Auto (recurse every branch until `max_depth` / `leaf_min_size` / `leaf_cohesion_threshold` trips). The `RecursionMode::{Flat, Manual, Auto}` enum replaces the two bools (the old `disable_recursion` folds into Flat), and the build-time `max_depth` becomes a tunable `Params` field. [cluster-recursion-modes]
status:: planned

The `split` op's reverse edit snapshots the prior subtree for undo per [[spec:cluster-editor-undo-redo]].

### Summarize

`Summarize` generates `name` + `summary` for every cluster matching `scope`. Runs deterministically (no LLM) by default, or one LLM call per cluster batched through the task queue.

```rust
struct SummarizeParams {
    scope: SummarizeScope,                // All / StaleOrUnfilled / Subset { ids }
    subtree_root: Option<ClusterId>,      // None = whole tree; constrains All / StaleOrUnfilled
    recursive: bool,                      // default true; only relevant for subtree_root != None
    mode: SummarizeMode,                  // Llm (default) / None
    force: bool,                          // default false: skip clusters whose name isn't a placeholder
}
```

Scope semantics:

- **`All`** — every cluster row in the (sub)tree.
- **`StaleOrUnfilled`** — every cluster row where `summary_membership_churn > 0 OR summary is empty OR name is a `Cluster N` placeholder`. The staleness counter ([[spec:cluster-summary-staleness-counter]]) is load-bearing; every reshape op bumps it, so this scope is a no-op once everything is fresh.
- **`Subset { ids }`** — exactly the listed cluster ids; "Summarize this one cluster" (single-element) and "Summarize selected" (multi-select). Out-of-tree ids are silently dropped.

`force` defaults to `false`: a cluster whose name is *not* a `Cluster N` placeholder is left alone — its name came from the user or a prior pass. Set `force = true` for explicit per-node Regenerate. A `Subset` summarize with `force = false` against an already-named cluster is a no-op (returns `SkippedNamed`).

`SummarizeMode` is `Llm` (default) or `None`. `Llm` runs one LLM call per cluster (the prompt in §"Summarization"). `None` short-circuits without invoking the summarizer; the cluster keeps its placeholder name — structure without naming. PLANNED (`bug-clustering-params-spec-drift`): an `Extractive` mode (deterministic TF-IDF / KeyBERT-style naming, most-central note's title as fallback) for a fully model-free loop with `[llm]` disabled.

Queue integration: every `Summarize` enqueues one `TaskKind::ClusterSummarize { tree_id, scope_kind, n_targets }` row the user watches; per-cluster tasks fan out underneath ([[spec:cluster-editor-regenerate-via-task-queue]]). Submission is bottom-up — parent tasks run only after their children complete, so a parent summarizes over the children's fresh names, not placeholders. [cluster-op-summarize-sweep]
status:: done
touches:: [[code:hiker/trees/ops]]
note:: `core/src/trees/ops/summarize.rs::Trees::plan_summarize_sweep` owns the selection + ordering. Selection per `SummarizeScope`: `All` → every cluster row; `StaleOrUnfilled` → `summary_membership_churn > 0 OR summary='' OR name=''`; `Subset { ids }` → listed ids only (missing dropped). Subtree filter via `SummarizeParams.subtree_root` + `recursive`. Skips leaves; skips `user_edited_name OR user_edited_summary` unless `overwrite_user_edited`. Returns `SummarizePlan { tree_id, scope_kind, enqueued, skipped_user_edited, skipped_fresh }` with `enqueued` in deepest-first submission order. Commands `cluster_summarize(tree_id, params_json)` (generic) and `cluster_regenerate_names(tree_id)` (toolbar `All` scope) — both wrap `plan_summarize_sweep` + handle the async queue submission (Trees stays sync per module discipline). Umbrella `TaskKind::ClusterSummarize { tree_id, scope_kind, n_targets }` at `Priority::High` submitted first; per-cluster `RaptorSummarize` tasks at `Priority::Normal` follow in deepest-first order. Returns `SummarizeSweepOutcome { enqueued, skipped_user_edited, skipped_fresh, queue_row_id }` so the UI can render the queue badge. **Note:** the umbrella task uses the coarse fallback called out in the spec — it's submitted into the queue at High priority but has no per-task completion plumbing wired (the supervision "resolve when N children done" path isn't built yet); the orphan-fail sweep clears it after the synthetic expiry. Acceptable per the spec's "coarser but correct" escape clause
implements:: [[code:hiker/trees/ops/impl#[Db]plan_summarize_sweep]], [[code:hiker/trees/ops/edit/impl#[Db]auto_set_name_summary]]

### Roll-up

`Roll-up` grows a new parent layer over a set of existing clusters by clustering their chosen **representation** ([[spec:cluster-representation]]) — centroids by default, summary embeddings or a lexical vector optionally.

```rust
struct RollupParams {
    input_node_ids: Vec<ClusterId>,
    representation: Representation,        // centroid (default) / summary-embedding / lexical (cluster-representation)
    algorithm: ClusterAlgorithm,          // same as Split
    leiden: LeidenParams,
    min_cluster_size: u32,                // for HDBSCAN
    new_layer_name_pattern: Option<String>,  // default "Group {n}"
}
```

Steps:

1. Validate every `input_node_id` exists in the same tree. When `representation = SummaryEmbedding`, additionally require a non-empty `summary` on each input (errors `MissingSummary { node_id }`); centroid/lexical have no such precondition.
2. Build each input's representation vector (centroid / summary embedding / lexical) in memory, not persisted.
3. Run the partition algorithm over those vectors.
4. Insert a new cluster node above the inputs per community; members = inputs that landed in it, their `parent` repointed. New parents get placeholder names (`new_layer_name_pattern.unwrap_or("Group {n}")`), filled by a later `Summarize { Subset }`.
5. Each new parent's `centroid` is the L2-normalized mean of its inputs' representation vectors, `radius` the 90th-percentile distance, `confidence` inherited from the partition's metric.

Roll-up returns `Refused` (no rows inserted) when the partition yields a single community ("all inputs landed in one community") or all singletons ("no inputs merged"); the user lowers resolution / `min_cluster_size` and re-invokes. One invocation produces at most one new layer. [cluster-op-rollup]
status:: done
touches:: [[code:hiker/trees/ops]]
note:: `core/src/trees/ops/rollup.rs::Trees::validate_rollup_inputs` + `Trees::apply_rollup`. Validation step checks every `input_node_id` exists in `tree_id` and has a non-empty `summary` (`MissingSummary` shape returned as a `TreesError` carrying the offending id). The host command `cluster_op_rollup(tree_id, params_json)` embeds each input's summary live via the indexer's `Arc<dyn Embedder>` (`session.indexer.embedder()`; matches the hot-reload posture from [[spec:embedder-hot-reload-on-model-change]]) using `tokio::task::spawn_blocking` so the fastembed call doesn't park a tokio worker. The persistence step runs `partition` (HDBSCAN) or `partition_leiden` against the summary embeddings, refuses with `RollupOutcome::Refused { reason }` when (a) all inputs land in one community or (b) no inputs merged (every group is a singleton). Otherwise inserts one new `cluster_nodes` row per non-singleton community with `parent_id` = the inputs' common former parent (or `NULL` when split), centroid = L2-normalized mean of summary embeddings, confidence = `0.8` flat placeholder (the partitioner doesn't expose per-community confidence cleanly; called out in the spec). New parent's members get reparented via `reparent_many`. History op `rollup` snapshots `prior_parent_ids` per input + `new_parent_ids` + `new_parent_row_snapshots` for undo. Returns `RollupOutcome` so the UI can surface the refusal reason verbatim. Tagged `// status: cluster-op-rollup` in both files
implements:: [[code:hiker/trees/ops/impl#[Db]validate_rollup_inputs]], [[code:hiker/trees/ops/impl#[Db]apply_rollup]], [[code:hiker/trees/ops/impl#[Db]build_rollup_parents]], [[code:hiker/trees/ops/impl#[Db]record_rollup_history]]

The `rollup` op's reverse edit snapshots the inputs' prior `parent`s + the new parent nodes for undo.

**Manual wrap.** Wrapping a selected set under one new parent — a pure structural edit (no algorithm/representation): insert a parent, reparent the selection, placeholder name. The by-hand counterpart to automatic Roll-up. [cluster-op-wrap]
status:: planned
note:: Manual roll-up: wrap a selected set of clusters under one new placeholder-named parent — a pure structural edit (no algorithm, no representation), the by-hand dual of automatic [[spec:cluster-op-rollup]] and the up-counterpart to a manual Split


## Build recipe

"Build a tree from scratch" is a composition of the three ops, not a monolithic algorithm. The clustering review tab ([[spec:cluster-review-tab]] in `cluster-editor.md`) drives the recipe:

```
1. Split { target: virtual_root(scope), params: { recurse: false (default), ... } }
   → a single-level partition of leaf clusters with placeholder names
   (set recurse: true for a deeper tree in one pass, or deepen by hand later)

2. Name { scope: All, mode: Llm (default) | None }
   → fills name + summary on every cluster

3. (Optional) Rollup and/or per-cluster manual Split
   → grow the hierarchy up or down, where the user wants it
```

[cluster-build-recipe]
status:: done
touches:: [[code:hiker/cluster/build]]
note:: `core/src/cluster.rs::build_cluster_tree` drives the top-down divisive Split recipe end-to-end. Step 1: a virtual-root partition over every note's embedding using `LeidenParams.top_level_resolution` (default 1.0) for the top-level cut — `partition_top_level_escalating` retries this cut with geometrically-rising γ (×1.6, ≤4 retries) when it yields <2 communities so a too-low resolution self-corrects instead of aborting with `VaultTooSmall` — then per-top-level-community recursive sub-split via `recursive_split_branch` using the regular `LeidenParams.resolution` (default 1.0) at sub-levels. Stop conditions per-branch: member count `<=` `leaf_min_size` OR cohesion radius `<` `leaf_cohesion_threshold` OR 16-level depth cap OR sub-partition produced <2 communities. Step 2 (Summarize) is invoked per-cluster inline by the build's `summarizer` arg when callers pass `SummarizeMode::Llm`; the review tab's structural pass forces `SummarizeMode::None` so step 2 is deferred to Confirm-and-name. Step 3 (Rollup) is an explicit cluster-editor verb ([[spec:cluster-op-rollup]]), not invoked by the build. Output `BuiltClusterTree` packs each cluster into `levels[max_child_level + 1]` (leaves at level 0, parents above) and adds a synthetic `vault-root` cluster at the top when >1 top-level communities are produced — so `core/src/trees/build_adapter.rs::node_inserts`'s "top.len() == 1 → that's root" path always lands a single root. Top-level outliers get force-routed when `include_outliers = false` (threshold `-1.0`) or hybrid-recovered at ≥0.6 cosine for `algorithm = Hybrid`; sub-split outliers fold back into the first child cluster as plain members. End-to-end consumer: the review tab's Run-clustering / Confirm-and-name / Confirm-no-naming buttons via `cluster_run_structural` → `build_tree_structural` → `build_tree` → `build_cluster_tree`. **Note:** the recipe's algorithmic core is duplicated between `core::cluster::recursive_split_branch` (this op, operates on `Vec<NoteInput>`, returns `BuiltClusterTree`) and `core/src/trees/ops/split.rs::Trees::split_cluster_recursive` (the user-driven verb, operates on persisted leaf rows + DB writes interleaved per recursion level for undo). Sharing more than the `partition_indices` primitive isn't worth the abstraction cost given the two sides' different output shapes; revisited during the `trees.rs` module-directory split and the verdict held — rationale captured in `core/src/trees/ops/split.rs`'s top-of-file comment
implements:: [[code:hiker/cluster/build/impl#[`Builder<'a>`]top_level_split]], [[code:hiker/cluster/build/impl#[`Builder<'a>`]build_top_level_nodes]], [[code:hiker/cluster/build/impl#[`Builder<'a>`]split_branch_ctx]], [[code:hiker/cluster/build/impl#[`SplitBranchCtx<'a>`]split_top_level_groups]]

Steps 1 and 2 are gated separately in the review tab — Run clustering is step 1; Confirm names is step 2 (LLM, or skipped). The result is flat unless the user set `recurse`. Down-recursion uses divisive Split (not recursive Roll-up, which would force a Summarize at every level); Roll-up stays the explicit coarsening verb.

The current `Params` set (`core::cluster::Params`):

- **`disable_recursion` / `recurse`** — the two recursion bools (§"Split"). Default is a single flat partition.
- **`leaf_min_size`** (default `5`) / **`leaf_cohesion_threshold`** (default `0.15`, 90th-percentile cosine distance from members to centroid) — per-branch stop conditions for recursive sub-split.
- **`min_cluster_size`** / **`min_samples`** — HDBSCAN tunables; `min_cluster_size` drives the partition at every level.
- **`summary_confidence_threshold`** — marks below-threshold clusters "uncertain" in the review surface.
- **`include_outliers`** — force-routes outliers at the top-level Split when false.
- **Leiden knobs** (`LeidenParams`) — detailed in §"Leiden"; the recipe's first Split substitutes `top_level_resolution` (default `1.0`) for `resolution` (default `1.0`) on the virtual-root cut. The review-tab γ slider drives `top_level_resolution` directly, and the recipe escalates it automatically if the cut collapses to a single community (§"Leiden"). [cluster-leiden-params]
status:: done
touches:: [[code:hiker/clusters/panel]]
note:: `core/src/cluster.rs::LeidenParams { k_nearest: u32, edge_weight_floor: f32, iterations: u32, min_cluster_size: u32, resolution: f32, top_level_resolution: f32 }` carried on `ClusterParams.leiden` (with `#[serde(default)]` and `default_leiden_resolution() -> 1.0` / `default_leiden_top_resolution() -> 1.0` so old persisted method JSON deserializes cleanly). Defaults: `k_nearest=15`, `edge_weight_floor=0.0`, `iterations=100`, `min_cluster_size=2`, `resolution=1.0`, `top_level_resolution=1.0`. UI surfaces the original five in the cluster review tab's Advanced disclosure when `algorithm == leiden` (`app/src/clusters/panel/mod.rs`); the "Resolution (γ)" slider sets **both** `resolution` and `top_level_resolution` (so the slider drives the decisive top-level cut, not only sub-splits). `top_level_resolution` is consumed by the build recipe's first Split call against the virtual root ([[spec:cluster-build-recipe]]) and by `Trees::split_cluster`'s virtual-root invocation ([[spec:cluster-op-split]]) — recursive sub-splits and real-node user-driven splits revert to `resolution`. γ is the Reichardt-Bornholdt configuration parameter; γ > 1 finer, γ < 1 coarser. The earlier `0.3` top-level default collapsed dense kNN graphs to a single community (`Q_single = m·(1−2γ) > 0` for γ < 0.5) and aborted with `VaultTooSmall`; the `1.0` default plus the recipe's resolution-escalation retry (`partition_top_level_escalating`) fixes it

The review tab's Advanced disclosure toggles its content by the selected method and algorithm. Cluster: an algorithm select (`hdbscan` / `leiden` / `hybrid` / `gmm`); HDBSCAN/Hybrid/GMM show `min_cluster_size` + `min_samples` (blank for auto); Leiden swaps those for `k_nearest` / `edge_weight_floor` / `resolution` (γ) / `iterations` / `min_cluster_size`. Common across algorithms: `summary_confidence_threshold` and the `disable_recursion` checkbox. FromFolders: `outlier_threshold` (only when Include outliers is on, with an explanatory note otherwise). `summarize` is intentionally not exposed — the structural pass forces `none`, the Confirm-and-name path forces `llm`. [cluster-review-tab-advanced-disclosure]
status:: done

The `disable_recursion` toggle is `ClusterParams.disable_recursion: bool` (default `false`, `#[serde(default)]`) on `core/src/cluster.rs`; `build_cluster_tree` short-circuits the recursive merge loop right after level 0 when set. Its UI checkbox lives in `app/src/panels/cluster_review/mod.rs` under the common-tunables block, and the flag is persisted explicitly in `cluster_trees.method` JSON so the saved tree's intent is recoverable. [cluster-review-tab-disable-recursion]
status:: done

PLANNED (`bug-clustering-params-spec-drift`): a `representation` param (centroid / summary / lexical, §"Representation"); a `max_depth` recursion cap promoted from a build-time const (`build/mod.rs`) to a `Params` field; the `RecursionMode` enum and `SummarizeMode::Extractive` (above).


## Async execution and progress

The structural pass runs on a background task so the UI thread stays responsive. Producers (the cluster review tab is the only one in v1) submit a build request and consume a stream of progress events from `core::cluster`:

- `Phase { phase }` — current pipeline phase; phase variants cover `LoadingEmbeddings`, `PartitioningLevel(u32)`, and `Finalizing`. Emitted at the start of each phase.
- `Counters { items_processed, clusters_found, outliers }` — running totals; emitted as the partitioner advances.
- `ClusterDiscovered { node: BuiltClusterNode, parent: Option<ClusterId> }` — emitted as each new cluster is added to the in-flight tree. Consumers use this to incrementally reveal clusters in the review surface (per [[spec:cluster-review-tab-live-cluster-reveal]]) rather than waiting for the full tree.
- `Done { tree: BuiltClusterTree }` — terminal: the full tree is ready.
- `Cancelled` — terminal: the producer signalled cancel and the pass aborted cleanly.
- `Failed { error }` — terminal: the pass errored out (partition refused, embeddings missing, etc.).

These `Phase` / `BuildEvent` enums are emitted by the async structural pass and consumed by the cluster review tab ([[spec:cluster-review-tab-live-cluster-reveal]]); they are not part of the public crate API for non-UI callers. [cluster-build-progress-stream]
status:: done
implements:: [[code:hiker/cluster/build/impl#[`BranchFrame<'a, 'b>`]emit_leaf]], [[code:hiker/cluster/build/impl#[`BranchFrame<'a, 'b>`]finalize_branch]], [[code:hiker/cluster/Id]], [[code:hiker/cluster/Phase]], [[code:hiker/cluster/BuildEvent]]
verifies:: [[code:hiker/cluster/tests/structural_streaming_fixture_notes]]

The stream is owned by `core::cluster` and consumed through a channel-shaped interface. `core::cluster::build_tree_structural_streaming` (`core/src/cluster/build/stream.rs`) runs the structural pass on a tokio task and emits the stream; the blocking `build_tree_structural` stays as a convenience wrapper for tests and non-UI callers. Cancellation is cooperative: the producer signals via a shared atomic, the pass checks at level boundaries and a periodic per-node interval, drops in-flight results, and emits `Cancelled`. [cluster-build-async-pass]
status:: done
implements:: [[code:hiker/cluster/build/stream/impl#[StreamCtx]is_cancelled]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]check_cancel]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]check_cancel_periodic]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]emit]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]emit_phase]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]emit_counters]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]emit_partition_phase_if_new]], [[code:hiker/cluster/build/stream/impl#[StreamCtx]emit_cluster]]
implements:: [[code:hiker/cluster/BuildError#Cancelled]], [[code:hiker/cluster/BuildEvent]]
verifies:: [[code:hiker/cluster/tests/structural_streaming_fixture_notes]]

The LLM summarization pass (Summarize op) is async via the task queue ([[spec:cluster-op-summarize-sweep]]) and not part of this stream — structural and naming are separate operations with separate progress surfaces.

**Live preview.** Once a first Run has loaded the note set, the review tab re-runs the structural pass automatically (debounced) whenever a config knob changes, so tuning is interactive rather than click-Run-and-wait. The on/off toggle is a **display control** and lives in the result graph's view/eye menu (alongside Labels / Edges / Leaves), *not* in the clustering-config knobs — those knobs hold only clustering-engine params (HDBSCAN / Leiden tunables). The principle: display controls live in the view menu; clustering options are engine params. (When the note count exceeds the per-algorithm gate the config form shows a one-line "off — over the live limit; use Run" notice in place of the toggle.) Three caches keep this cheap: the resolved inputs (vault walk + per-note embeddings) are held in memory keyed by scope so tweaks don't re-hit SQLite; the top-level Leiden kNN graph is cached and reused so re-tuning γ / `min_cluster_size` skips the O(n²) sweep (the partitioner is split into `build_leiden_graph` + `leiden_communities` for exactly this — the build returns the graph it used in `BuildEvent::Done { top_graph }`, validated against `k_nearest` / `edge_weight_floor` before reuse); and a per-algorithm note-count gate auto-disables live preview on large vaults (Leiden tolerates a high cap because its γ re-tune is graph-cached; HDBSCAN re-partitions from scratch so it gets a tighter one). [cluster-review-tab-live-preview]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_graph_view]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]render_config_form]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]run_structural_streaming]], [[code:hiker/panels/cluster_graph/show_with_nodes]]
note:: `app/src/clusters/panel/mod.rs::maybe_live_rerun` re-runs the structural pass automatically (debounced 250ms) when any config knob changes, once a first Run has established the note set. Three layers keep it cheap: (1) `resolve_notes` caches the vault-walk + per-note embedding load on the pane keyed by scope signature (`source_types` + semantic-vs-folders), so tweaks don't re-query SQLite; (2) the top-level Leiden kNN graph is cached and handed back to the next build via `BuildEvent::Done { top_graph }` + the `build_tree_structural_streaming(.., prebuilt_top_graph)` param, so γ / min-size re-tunes skip the O(n²) sweep — the build's `top_level_leiden_graph` validates the cached graph against the requested `k_nearest` / `edge_weight_floor` (`LeidenGraph::matches`) and rebuilds only on mismatch; (3) a per-algorithm size gate (`live_preview_max`: Leiden 2000, HDBSCAN/Hybrid/GMM 600, FromFolders effectively unbounded) auto-disables live preview on large vaults, surfaced as a caption under the toggle. Global `AdvancedClusterParams.live_preview` (default on) gates it; `config_signature` (algorithm + scope + all tunables) detects changes and `last_run_sig` avoids re-firing on unchanged config. The core split that enables (2): `core::cluster::algo::{build_leiden_graph, leiden_communities}` (the expensive kNN build vs the cheap RB community detection), composed by `partition_leiden`; the escalation retry (`partition_top_level_escalating`) reuses one graph across every γ instead of rebuilding
implements:: [[code:hiker/cluster/algo/impl#[LeidenGraph]matches]], [[code:hiker/cluster/build/impl#[`Builder<'a>`]top_level_leiden_graph]]


## Note embeddings input

Every Split operates on note-level embeddings, regardless of where in the tree it runs. A Split against the virtual root sees every note in the build scope; a Split against a real cluster sees that cluster's leaves' embeddings.

The note embedding is the mean of the note's chunk embeddings, weighted by chunk byte length. Computed inline by the indexer's per-file upsert and persisted on the `notes` row (`note_embedding BLOB`) in the same transaction as the chunks. Refreshed on every upsert so the pool tracks the chunk set; notes with no chunks leave the column NULL and are excluded from clustering. Cheap — a vector mean over typically <20 chunks — and avoids spending a separate embedder pass on each note. [cluster-note-embeddings]
status:: done
implements:: [[code:hiker/store/chunks/impl#[Store]note_embedding_for_path]], [[code:hiker/store/chunks/impl#[Store]compute_and_store_note_embedding]], [[code:hiker/store/chunks/impl#[Store]clear_note_embedding]]
note:: `core/src/store/chunks.rs::upsert_note` computes the byte-length-weighted mean-pool from the chunks being written and persists it on `notes.note_embedding` in the same transaction (empty-chunk notes stay NULL). `Store::note_embedding_for_path` is the read path used by the cluster-review graph view (`app/src/panels/cluster_review/mod.rs`) and the cluster-tree sidebar leaf-preview. `Store::compute_and_store_note_embedding` and `Store::clear_note_embedding` remain on the surface for tests / one-off recomputes but the production write path is the inline-pool branch. `ensure_chunk_vecs_dim` still clears `note_embedding` when the embedder dim changes so a model swap can't leave stale-dim blobs around

Mean-pool rather than embedding the full note directly because `bge-small`'s 512-token (~2000 char) context silently truncates longer notes; mean-pool over chunks (each ~1200 chars) sidesteps the limit with no practical max note size. With a long-context embedder ([[spec:embedder-model-selectable]]: `bge-m3` 8k, `embedding-gemma-300m` 2k) direct full-note embed becomes viable for most notes, with mean-pool as the fallback for over-context outliers; the direct-embed path is deferred. [cluster-note-embeddings-direct-long-context]
status:: planned
note:: when a long-context embedder is selected (`bge-m3` / `embedding-gemma-300m` per [[spec:embedder-model-selectable]]), embed the full note text directly instead of mean-pooling chunk vectors; mean-pool stays as the fallback for notes exceeding the model's context

Empty notes (no chunks) are excluded from clustering — they land in the "inbox" (unplaced) bucket, same as outliers.


## Algorithm choices

`algorithm` selects which partitioner runs inside Split (and Roll-up). Four variants: Leiden (default), HDBSCAN, Hybrid, GMM-stubbed.

### Leiden (default)

Modularity-optimization community detection over a kNN cosine-similarity graph. Every node lands in some community; small communities (below `min_cluster_size`) post-flag as outliers. Default because loose `bge-small` embeddings often give HDBSCAN 0–1 cohesive clusters plus everything-as-outliers, while Leiden produces 5–20 communities; γ is a direct granularity knob (`resolution` / `top_level_resolution`, both default `1.0`). [cluster-leiden]
status:: done
note:: `core/src/cluster.rs::partition_leiden` — kNN-graph + modularity-optimization community detection over L2-normalized embeddings. Wired end-to-end through `ClusterParams.algorithm = Leiden`: `build_cluster_tree` dispatches `partition` vs `partition_leiden` at level 0, and at each recursion level, with `k_nearest` clamped per-level (`min(k_nearest, prev.len() - 1)`) so a default `k=15` doesn't ask for 15 neighbors among 4 cluster summaries. Communities smaller than `min_cluster_size` flag as outliers; Hybrid mode is intentionally not wired (Leiden places every node — see [[spec:cluster-hybrid-outlier-recovery]])

**Why γ defaults to `1.0`, not a coarser value.** The RB quality of a single all-notes community is `Q_single = m·(1 − 2γ)` (with `m` = total edge weight). For any `γ < 0.5` that is positive, so on a dense kNN graph the one-giant-community partition is the optimum and the top-level Split collapses to a single community — which the recipe then rejects as `VaultTooSmall`. An earlier `top_level_resolution = 0.3` default hit this on essentially every vault. Two guards keep it from recurring: the default sits at the modularity-equivalent `1.0` (where `Q_single = −m < 0`, so any non-trivial split wins), and the recipe **escalates** γ geometrically (×1.6, up to 4 retries) whenever a top-level cut still yields fewer than 2 communities before surfacing the error. [cluster-leiden, cluster-build-recipe]
implements:: [[code:hiker/cluster/build/impl#[`Builder<'a>`]partition_top_level_escalating]]

Pipeline: [cluster-leiden-knn-graph]
status:: done
note:: `partition_leiden` builds the graph inline: L2-normalize embeddings → brute-force top-`k_nearest` neighbors per point by cosine similarity → drop edges below `edge_weight_floor` → dedupe pairs (i,j)/(j,i) via a `HashSet<(usize,usize)>` so each undirected edge is inserted once (mutual-kNN is too aggressive at vault scale, so we keep an edge if it appears in either direction's top-k). Brute-force O(n²) is consistent with the HDBSCAN path's posture; fine at personal-vault sizes <10k notes

1. L2-normalize the input embeddings so cosine similarity reduces to a dot product.
2. For each point, find its top-`k_nearest` neighbors by cosine similarity (brute-force O(n²) — same scale ceiling as HDBSCAN's path).
3. Drop neighbor edges whose weight (cosine similarity) is below `edge_weight_floor`.
4. Construct an undirected graph (edges symmetrize on insertion; mutual-kNN is too aggressive at vault scale).
5. Build a CSR network from the deduped edges, run `RBConfigurationPartition` at the configured `resolution` (the recipe substitutes `top_level_resolution` on the first call), and read each node's community id from the resulting membership.
6. Communities smaller than `min_cluster_size` flag as outliers (post-filter, not algorithmic).

Tunables: [cluster-leiden-params]

- `k_nearest` — number of nearest neighbors per node. Default `15`. Smaller = sparser graph = more, smaller communities; larger = denser graph = fewer, bigger communities. Clamped to `n-1` so it doesn't ask for more neighbors than exist.
- `edge_weight_floor` — minimum cosine similarity for a kNN edge to survive. Default `0.0` (keep every kNN edge). Raise to strip weak neighbor links and tighten community boundaries.
- `resolution` (γ) — Reichardt-Bornholdt resolution parameter on sub-splits. Default `1.0`. γ > 1 biases toward finer / more communities; γ < 1 toward coarser / fewer. The review-tab "Resolution (γ)" slider sets both this and `top_level_resolution`.
- `top_level_resolution` — γ for the *first* Split call against the virtual root (when Split is invoked from the build recipe). Default `1.0`. The build recipe escalates this value automatically (×1.6, up to 4 retries) when a cut produces fewer than 2 communities, so a too-low value self-corrects rather than aborting the build. Ignored when Split is invoked on a non-virtual target.
- `iterations` — cap on Leiden refinement iterations (`LeidenConfig.max_iterations`). Default `100`. Algorithm converges fast; the cap is a safety rail.
- `min_cluster_size` — minimum community size; smaller communities flag as outliers. Default `2`.

**Crate choice: `single-clustering` 0.6.1.** BSD-3-Clause; pure Rust; builds on aarch64-linux (verified locally). Provides `RBConfigurationPartition` (the resolution-parameterized partition used here). Its optional HNSW-based kNN primitives are deliberately unused — our hand-rolled cosine kNN has clearer weight semantics than the crate's `exp(-d²/σ²)` "Gaussian" variants. Marked "under heavy development" in its README, so the workspace dep is **exact-version pinned to `=0.6.1`** — any bump is a deliberate re-test. [cluster-leiden-crate-single-clustering]
status:: done
note:: `single-clustering = "=0.6.1"` added as a workspace dep + `core` dep (exact-version pinned; crate is marked "under heavy development" by its README). Public surface used: `CSRNetwork::from_edges(&edges, node_weights)`, `LeidenConfig { max_iterations, seed, ..Default::default() }`, `LeidenOptimizer::new(config) + optimize_single_partition(&mut partition, None)`, `RBConfigurationPartition::<f64, VectorGrouping>::with_resolution(network, γ)`, `partition.membership(node)`. BSD-3-Clause; builds on aarch64-linux (verified). All `single_clustering::` imports live inside `core/src/cluster.rs` — module-discipline preserved. Replaced `fa-leiden-cd` 0.1.0 to gain a tunable resolution parameter

### HDBSCAN

Density-based hierarchical clustering. Selectable via `ClusterParams.algorithm = Hdbscan`. [cluster-hdbscan]
status:: done
touches:: [[code:hiker/cluster]]
note:: `core/src/cluster.rs::partition` is the HDBSCAN entry; the build pipeline (`cluster-build-recursive`) is the first consumer. Determinism comes from petal-clustering's seeded fit + the stable L2-normalize → ndarray pipeline; outliers carry `OUTLIER_LABEL` and end up in `BuiltClusterTree.outliers` when `include_outliers = true`

Picks over Leiden when:

- The vault has a long miscellaneous tail the user wants explicitly bucketed as outliers (HDBSCAN labels low-density points as outliers natively; Leiden places every point and post-filters singletons).
- The user wants density gaps respected (two topically-close clusters separated by a thin density bridge stay separate under HDBSCAN; under Leiden they may merge if mutually kNN-connected).

Tunables:
- `min_cluster_size` — smallest cluster the algorithm will form. Default 5; user-overridable per vault in `vault/.hiker/config.toml`.
- `min_samples` — density threshold. Default = `min_cluster_size`. Higher → more outliers.
- Distance metric — cosine, on the note embeddings.

Watch for: small vaults (<50 notes) where HDBSCAN may produce all-outliers and an empty tree. Fallback: if the top-level Split produces fewer than 2 clusters, surface a "vault is too small" message rather than a misleading tree of one node.

**Crate choice: `petal-clustering`.** Rust-native HDBSCAN, MIT-licensed, no C/C++ FFI, no extra system deps. Exposes a `fit -> Vec<i32>` plus cluster-stability metadata — the surface `core::cluster::partition` needs. The crate is Euclidean by default; we pre-normalize embeddings once so distance reduces to cosine. [cluster-hdbscan-crate-petal]
status:: partial
note:: `core/src/cluster.rs::partition` — wraps `petal_clustering::HDbscan` over an L2-normalized `ndarray::Array2<f64>` so the crate's default Euclidean metric is monotonic with cosine (matches spec's "cosine distance via pre-normalized embeddings"); returns `Vec<ClusterAssignment>` with `cluster_label = OUTLIER_LABEL (-1)` for noise points. `l2_normalize` + `cosine_similarity` exposed for downstream use. Cargo workspace gains `petal-clustering 0.13` + `ndarray 0.17` (pinned to the version petal-clustering depends on). **Gap:** wired through `core::cluster::partition` but no consumer yet — `core::cluster::build_tree` (per `cluster-build-recursive`) is the first caller

### Hybrid mode

HDBSCAN runs first; outliers get reassigned to the nearest cohesive cluster's centroid if cosine ≥ 0.6. Selectable via `algorithm = Hybrid` — for when HDBSCAN's outlier set is too aggressive but the user doesn't want pure Leiden's "every point gets a home" either. Soft members tag distinctly in the cluster row so the editor renders them differently. Not wired under Leiden (Leiden places every point, so there's no outlier set to recover). [cluster-hybrid-outlier-recovery]
status:: partial
touches:: [[code:hiker/cluster]]
note:: `core/src/cluster.rs::build_cluster_tree` — when `algorithm = Hybrid`, every outlier is re-scored against the interim cluster centroids and joins its nearest cluster if cosine ≥ 0.6 (matches the spec's "GMM places with probability > 0.6" gate). When `include_outliers = false`, the threshold is dropped so every outlier is force-routed. Leiden is intentionally **not** wired into Hybrid — Leiden places every point in a community by construction, so the outlier-recovery reassignment has nothing to do; small-community-flagged outliers under `Leiden` still get force-routed when `include_outliers = false`. **Gap:** approximation — the spec calls for a real GMM-soft-membership pass; we use cosine-to-centroid because GMM isn't wired yet (see [[spec:cluster-algorithm-selectable]]). Soft-member tagging (distinguishing primary vs soft members in the cluster row) is not yet expressed in `BuiltClusterNode`
implements:: [[code:hiker/cluster/build/impl#[`Builder<'a>`]recover_outliers]]

### GMM (stub)

`algorithm = Gmm` is reserved; the runtime falls back to HDBSCAN with a warning until a GMM crate goes through dep review. [cluster-algorithm-selectable]
status:: partial
implements:: [[code:hiker/cluster/build/impl#[`Builder<'a>`]run]]
touches:: [[code:hiker/cluster]]
note:: `core/src/cluster.rs::ClusterAlgorithm` enum (`Hdbscan` / `Gmm` / `Hybrid` / `Leiden`) selectable on `ClusterParams.algorithm`; persisted on the tree row via `cluster_trees.method` JSON. `Hdbscan` runs end-to-end; `Hybrid` runs HDBSCAN + the outlier-recovery reassignment pass below; `Leiden` runs end-to-end via `partition_leiden` over a kNN cosine-similarity graph ([[spec:cluster-leiden]]). **Gap:** `Gmm` falls back to `Hdbscan` with a warning — linfa-clustering doesn't ship GMM in the form we need yet (and petal doesn't have GMM at all). Real GMM support lands when a GMM crate goes through the dep review

### Algorithm selection

`cluster.algorithm` lives in `vault/.hiker/config.toml` as a per-vault default (different vaults cluster differently); the review tab's Advanced disclosure overrides it per build. [cluster-algorithm-selectable]


## Representation

PLANNED (`bug-clustering-params-spec-drift`): `representation` is not yet a `Params` field; today every partition uses the Centroid representation. The model below is the specced target.

`representation` decides what each *unit* is reduced to before the partition algorithm sees it — orthogonal to the algorithm (which decides *how*) and to direction: Split's units are notes, Roll-up's units are clusters. One routine services both. [cluster-representation]
status:: planned
note:: Pluggable `Representation` (Centroid default / Summary-embedding / Lexical-deterministic) deciding what each unit is reduced to before partitioning; orthogonal to algorithm and to direction (Split's units = notes, Roll-up's = clusters). One `cluster(units, representation, method)` routine serves both. Per-action choosable, tree's last choice inherited; Summary-embedding only when units are named

| Representation | Vector | Character |
| --- | --- | --- |
| **Centroid** (default) | L2-normalized mean of the unit's member embeddings | Semantic, geometric, always available, no LLM. The honest default |
| **Summary embedding** | embedding of the unit's name/summary text | Semantic by *what it's about*. Needs naming first → natural for Roll-up (clusters are named), a costly opt-in for Split (notes aren't) |
| **Lexical** | TF-IDF / sparse term vector over the unit's text (document-frequency from the lexical/FTS index) | Literal — groups by shared terms. Reproducible, most explainable, model-free |

Summary-embedding is available only when the units are named. Default is Centroid for both directions; the user overrides per action, with the tree's last choice inherited (mixing within a tree is allowed but costs legibility). Lexical + the planned `SummarizeMode::Extractive` gives a complete model-free path.

Signal *fusion* — a deferred composite representation blending lexical + embedding + link-graph + tags with weights (the "Connections" / hybrid lenses) — slots into this same `representation` param when it lands. [cluster-representation-fusion]
status:: planned


## Build scope

`core::cluster::build_tree(scope, method, params)` takes a `BuildScope` (which notes), a `BuildMethod` (how the tree is built), and method-specific params. The cluster editor's "Suggest reorganization" picks all three; saved Evergreen trees record them so triage knows what the tree classifies and how to rebuild. The `BuildScope` enum (`Vault` / `Folder { rel }` / `Notes { ids }`) is serialized as JSON onto `cluster_trees.scope`; the producer resolves it into `Vec<NoteInput>` before calling `build_tree`, and the scope itself rides through as part of `BuildResult` so triage knows which notes the saved tree classifies. [cluster-build-scope]
status:: done
touches:: [[code:hiker/cluster]]

```rust
enum BuildScope {
    Vault   { source_types: Vec<String> },                        // every indexed note in the vault
    Folder  { rel: VaultRel,    source_types: Vec<String> },      // every note under a folder (recursive)
    Notes   { ids: Vec<NoteId>, source_types: Vec<String> },      // an explicit set
}
```

Resolution rules:

- `Vault` — the default; same set the v0 design assumed.
- `Folder(rel)` — every note whose path is under `rel` at the time of the build. Empty subfolders are ignored; notes added under `rel` after the build do not retroactively join.
- `Notes(ids)` — exactly the listed notes; missing ids are silently dropped (a note may have been deleted between selection and build).

**Source-type filter** [cluster-build-scope-source-types]. Each variant carries an optional `source_types: Vec<String>` of canonical lower-case extensions; only notes whose extension is in the list reach the build pass + triage classifier (`"md"` covers both `.md` and `.markdown`). An empty vec is the legacy "every indexable extension" posture, what pre-feature `scope` frontmatter deserializes to. Surfaced as Markdown / Plain-text checkboxes (default both on). Enforced in two places: the build-pass resolver filters resolved paths before reading embeddings; the on-save classifier skips saved trees whose filter rejects the saved note's path (a `.txt` save against a Markdown-only triage tree no-ops).
status:: done
implements:: [[code:hiker/suggest/triage_all_saved_trees]]
note:: `BuildScope` enum variants each carry `source_types: Vec<String>` with `#[serde(default)]` (pre-feature persisted trees deserialize cleanly as "every indexable extension"). `BuildScope::matches_path(path)` and `BuildScope::source_types()` helpers in `core/src/cluster.rs`. Build-pass enforcement in both `DirectWorkerHandlers::notes_for_scope` and host-side `notes_for_scope_via_session` — paths whose extension fails the filter are dropped before embeddings are read. Triage-time enforcement in `core::suggest::triage_all_saved_trees` — deserializes each saved tree's `scope_json` and skips when the source path's extension doesn't match. `"md"` covers both `.md` and `.markdown` (indexer-canonical equivalence). UI checkboxes in `app/src/panels/cluster_review/mod.rs` Scope section (Markdown / Plain text, default both on); empty selection rejected at Run-time with a toast. Carried through `scope_json` so rebuild-prefill inherits the filter
implements:: [[code:hiker/cluster/impl#[BuildScope]source_types]], [[code:hiker/cluster/impl#[BuildScope]matches_path]]

The clustering algorithm is unaware of scope — `BuildScope` resolves to a `Vec<NoteId>` at the entry, the pass operates on whatever embeddings it's handed. The small-vault skip (`<50 notes` → "vault is too small") applies per-scope.

Saved triage trees persist their scope; the triage classifier ([[spec:cluster-place-beam-descent]]) only evaluates new/modified notes whose path falls within the saved scope. Triage's existing safety rule (notes outside the configured triage scope are never moved out of their folder) intersects with the build scope — a saved tree built over `research/` can still only auto-move notes that live in the triage scope (default `inbox/`).


## Build method

`BuildMethod` selects how the tree is constructed from the resolved note set — the `BuildMethod` enum (`Cluster { params: ClusterParams }` / `FromFolders { params: FolderDeriveParams }`), persisted as JSON on `cluster_trees.method`. `build_tree` dispatches to `build_cluster_tree` vs `build_from_folders` based on the variant; the output shape is identical (`BuiltClusterTree`). Two methods, each with its own parameter shape: [cluster-build-method]
status:: done
touches:: [[code:hiker/cluster]]

```rust
enum BuildMethod {
    Cluster   { params: Params },              // runs the build recipe (default)
    FromFolders { params: FolderDeriveParams }, // mirror the filesystem hierarchy
}

struct FolderDeriveParams {
    summarize: SummarizeMode,
    include_outliers: bool,         // default true; gates outlier detection at triage time
    outlier_threshold: f32,         // cosine-distance threshold; matches farther than this become outliers
}
```

`Cluster`'s `Params` is the field set enumerated in §"Build recipe" — `core/src/cluster.rs::ClusterParams` (algorithm, min_cluster_size, min_samples, summary_confidence_threshold, include_outliers, summarize, **leiden**, **disable_recursion**, **recurse**, **leaf_min_size** default 5, **leaf_cohesion_threshold** default 0.15) and `FolderDeriveParams` (summarize, include_outliers, outlier_threshold); both serde + `Default`. The build recipe ([[spec:cluster-build-recipe]]) terminates recursion per-branch via `leaf_min_size` / `leaf_cohesion_threshold`. Persisted inside the `BuildMethod` JSON on the `cluster_trees` row. Naming is applied by the Name step's `SummarizeMode`, not by `Params`. [cluster-build-params]
status:: done
touches:: [[code:hiker/cluster]]

### `Cluster` method

Default. Runs the build recipe ([[spec:cluster-build-recipe]]). `include_outliers` defaults to `true`; `false` runs the hybrid-mode outlier-recovery pass with the threshold lowered so every outlier lands in its closest cluster (every note gets a home). [cluster-build-cluster-method]
status:: done
touches:: [[code:hiker/cluster/build]], [[code:hiker/cluster]]
note:: `core/src/cluster.rs::build_cluster_tree` — the `BuildMethod::Cluster` branch of `build_tree`. Runs the build recipe ([[spec:cluster-build-recipe]]): top-down divisive Split from the virtual root using `LeidenParams.top_level_resolution` (default 1.0, auto-escalated when a cut yields <2 communities) on the first cut and the regular `resolution` on recursive sub-splits, with per-branch stop conditions via `leaf_min_size` / `leaf_cohesion_threshold` and a hard 16-level safety cap. Summarize is invoked per-cluster inline by the `summarizer` arg (review tab's structural pass forces `SummarizeMode::None` so naming defers to Confirm-and-name). `include_outliers = false` runs the top-level outlier-recovery loop with the cosine threshold dropped to `-1.0` so every outlier force-routes into its nearest cluster; `algorithm = Hybrid` runs the same loop at ≥0.6 cosine. The legacy bottom-up recursive-Roll-up loop has been removed

### `FromFolders` method

Skip clustering entirely. Walk the filesystem under the build scope; produce a `ClusterTree` whose structure mirrors the folder hierarchy:

- One `ClusterNode` per folder (kind `Cluster`).
- One leaf node per note, parented to the folder it lives in.
- Root = the scope's root (`vault/` for `Vault`, `<rel>/` for `Folder(rel)`, a synthetic single-cluster root for `Notes(ids)`).
- Centroids: mean of member embeddings (same as `Cluster` method's leaf-level centroids). Computed lazily on save-as-triage so the placement classifier ([[spec:cluster-place-beam-descent]]) works against the folder-derived tree the same way it works against a `Cluster`-method tree.
- Outliers at build time: not generated. Every note already in the scope has a folder; the build's leaf-set is exactly the scope's notes.
- Outliers at triage time (Evergreen use): a new note whose final cosine distance to its nearest folder exceeds `outlier_threshold` (default `0.5`) routes to the outlier bucket instead of force-fit; the bucket node is created lazily and can carry its own policy ([[spec:cluster-editor-outlier-policy]], typically `Move` to `inbox/unsorted/`). `include_outliers = false` disables the check — every note routes to its nearest folder regardless of similarity. [cluster-build-from-folders-outliers]
status:: partial
note:: Outlier-bucket attachment / lazy creation rides through `core::trees::update_for_folder_rename` (Sprint D) which creates the destination folder cluster on the fly; the triage classifier's `outlier_threshold` gate against `FolderDeriveParams.outlier_threshold` is wired implicitly via [[spec:cluster-place-beam-descent]]'s confidence/margin output (caller inspects them per match). **Partial:** the explicit "route to outlier bucket when cosine > threshold" branch inside the classifier is not yet promoted into a first-class outlier-bucket path; today the row lands under the matched cluster and the user sees the low confidence in the staging metadata
- Confidence: 1.0 on every node (the folder structure is the source of truth, not a probabilistic guess).
- Summaries: per `SummarizeMode` — `llm` runs the same prompt as the clustering pipeline (per [[spec:cluster-summarize-llm]]) over each folder's member titles + summaries; `none` leaves summary empty. Name defaults to the folder's basename.

[cluster-build-from-folders]
status:: done
touches:: [[code:hiker/cluster]]
note:: `core/src/cluster.rs::build_from_folders` — groups `NoteInput`s by their `folder` field, produces one `BuiltClusterNode` per folder. Centroid = L2-normalized mean of member embeddings; radius = 90th-percentile cosine distance; confidence pinned to 1.0 (the folder structure *is* the truth, not a guess); default name = folder basename. `SummarizeMode::None` → name only, no LLM call; `Template` / `Llm` route through the same `run_summarizer` shared with the Cluster method. Build-time outliers: not produced (every note already has a folder); triage-time outliers gated by `FolderDeriveParams.include_outliers` + `outlier_threshold` are the cluster-place-beam-descent's concern, not this function's

For already-organized vaults: build the Evergreen tree on the actual folders rather than the partitioner's guess; triage then finds the most similar existing folder.

The output `ClusterTree` is identical in shape to the `Cluster` method's — same downstream consumers, same reshape ops. Splitting a folder-derived node re-runs HDBSCAN against just that node's members, so a folder-derived tree can grow cluster-derived subtrees. The `method` frontmatter records the original build method for re-build. [cluster-build-from-folders-uniform-output]
status:: done
touches:: [[code:hiker/cluster]]
note:: both `build_cluster_tree` and `build_from_folders` return `BuiltClusterTree` with identical `BuiltClusterNode` rows; persistence flatten (`core/src/trees/build_adapter.rs::node_inserts` → `cluster_nodes`) is method-agnostic. The `cluster_trees.method` JSON records the original build method for re-build, but the cluster editor doesn't need to consult it for editing. **Note:** the split-on-folder-node flow (re-run HDBSCAN against just a folder-derived node's members) lands with [[spec:cluster-editor-split-cluster]] — the underlying `partition()` is already callable on any member subset

### FromFolders live-update

A saved FromFolders Evergreen tree tracks the filesystem. When a note moves between folders (file-tree drag-drop, accepted `move_note` staging row, `hiker mv`, or a watcher-caught external rename), `core::trees` updates the tree's frontmatter in place — the leaf's `parent` flips to the new folder's node id. Cluster nodes for new folders are added on the fly; emptied folders' nodes are dropped unless they carry an explicit policy (kept as empty placeholders so the rule survives). The trigger is the same watcher rename event the indexer consumes; the update is incremental — no re-build, no LLM call — and affected centroids are recomputed (cheap). [cluster-build-from-folders-live-update]
status:: done
implements:: [[code:hiker/trees/ops/folder_rename/impl#[Db]update_for_folder_rename]]
verifies:: [[code:hiker/trees/tests/folder_rename_relocates_leaf_and_drops_empty_folder]]
note:: `core/src/trees/ops/folder_rename.rs::update_for_folder_rename` re-parents the affected leaf to the new folder cluster (lazily inserting the folder cluster when absent), drops emptied folder clusters that lack an explicit policy, and bumps churn on both ancestor chains. Wired to the watcher's Renamed event by the host (spawn after `spawn_staging_recheck`) for every saved-as-triage tree whose `method.kind = "from-folders"`. Host command `cluster_folder_rename_update` exposes the same op for explicit / test paths. Centroid recomputation is deferred — the spec's "no LLM call" guidance applies, and folder centroids drift slowly with single moves

**Staleness counter.** Each cluster node carries a `summary_membership_churn` integer (0 at summary-generation). Every leaf insert/remove in a cluster's subtree increments it on that cluster and all ancestors — the "summary may be out of date" signal, surfaced as a `↻ N` badge / soft-tinted node color. Resets to 0 on Regenerate. [cluster-build-from-folders-summary-staleness]
status:: done
implements:: [[code:hiker/trees/ops/folder_rename/impl#[Db]update_for_folder_rename]]
verifies:: [[code:hiker/trees/tests/move_node_bumps_churn_on_both_chains]]
note:: The five reshape ops (`move_node` / `promote_outlier` / `merge_siblings` / `merge_children_up` / `drop_cluster`) plus the folder-rename live-update all call `Trees::bump_churn_chain` on the affected chain(s); `reset_churn` zeroes a single node when the user runs Regenerate. Coverage proven by `trees::tests::move_node_bumps_churn_on_both_chains`

The counter applies primarily to FromFolders trees (filesystem moves drive churn); `Cluster`-method trees use the same field for reshape ops (move/merge/split). It is the signal [[spec:cluster-op-summarize-sweep]]'s `StaleOrUnfilled` scope consumes. [cluster-summary-staleness-counter]
status:: done
implements:: [[code:hiker/trees/ops/drop/impl#[Db]drop_cluster]], [[code:hiker/trees/ops/merge/impl#[Db]merge_siblings]], [[code:hiker/trees/ops/merge/impl#[Db]merge_children_up]], [[code:hiker/trees/ops/move_node/impl#[Db]move_node]], [[code:hiker/trees/ops/move_node/impl#[Db]reparent_many]], [[code:hiker/trees/ops/move_node/impl#[Db]promote_outlier]], [[code:hiker/trees/store/impl#[Db]delete_node]]
verifies:: [[code:hiker/trees/tests/move_node_bumps_churn_on_both_chains]], [[code:hiker/trees/tests]]
note:: `summary_membership_churn` column on `cluster_nodes` (Sprint A's schema), wired into every leaf insert/remove path (Sprint D). Bump sites across `core/src/trees/`: `ops/move_node.rs::move_node` (both prior + new parent chains), `ops/move_node.rs::promote_outlier` (same shape — distinct op for history), `ops/merge.rs::merge_siblings` (survivor chain), `ops/merge.rs::merge_children_up` (parent chain), `ops/drop.rs::drop_cluster` (outlier-bucket chain), `ops/folder_rename.rs::update_for_folder_rename` (both chains; FromFolders live-update), and `ops/move_node.rs::reparent_many` (both chains, for every move in the batch — this is the split / sub-cluster reparent surface, so `cluster_op_split` is covered without per-call ceremony). `storage.rs::delete_node` does **not** bump (structural drop of an already-empty cluster shell; see in-source comment). Round-trip surface: `storage.rs::Trees::bump_churn_chain` (walks parent chain incrementing each), `storage.rs::Trees::reset_churn` (zero a single node), `types.rs::EditableNode.summary_membership_churn` (read-side). Coverage tests: `move_node_bumps_churn_on_both_chains` and `split_bumps_churn_on_old_parent_and_new_subclusters`. **Load-bearing for** [[spec:cluster-op-summarize-sweep]]'s `StaleOrUnfilled` scope — the predicate is `summary_membership_churn > 0 OR summary IS NULL`. Also consumed by the cluster-editor row badge ([[spec:cluster-editor-summary-staleness-badge]])
implements:: [[code:hiker/trees/store/impl#[TreeDoc]bump_churn_until]], [[code:hiker/trees/store/impl#[TreeDoc]set_churn]], [[code:hiker/trees/store/impl#[Db]bump_churn_chain_until]], [[code:hiker/trees/store/impl#[Db]bump_churn_chain]], [[code:hiker/trees/store/impl#[Db]reset_churn]], [[code:hiker/trees/store/impl#[Db]set_churn]]


### Re-building Evergreen trees

A saved Evergreen tree records scope + method in frontmatter. The "Re-build" action ([[spec:cluster-editor-mode-menu]]) re-runs `build_tree` with the saved params; the user reviews the diff (deferred, [[spec:cluster-editor-tree-diff-view]]) or accepts the fresh tree, retiring the previous one to trash. [cluster-build-rebuild]
status:: partial
implements:: [[code:hiker/trees/build_adapter/rebuild_and_persist]]
note:: `core/src/trees/build_adapter.rs::rebuild_and_persist` re-runs the cluster build against the tree's saved `scope` + `method` (deserialized off the existing `cluster_trees` row); writes a new `draft` tree. User-edited names + summaries + policies are merged forward when a new cluster's note-id member set has Jaccard ≥ 0.5 against any old cluster. `cluster_tree_rebuild` command + `Rebuild` button in `app/src/panels/cluster_review/mod.rs` toolbar. **Partial:** the old tree isn't auto-retired (user reviews + discards manually) — that hinges on the deferred [[spec:cluster-editor-tree-diff-view]]; the Sprint F implementation is the minimal "produce a fresh tree, preserve overlapping clusters' user-edits" shape per the rollout doc's "ship a minimal version" fallback. Folder-rename / centroid drift on `from-folders` trees may produce surprising overlaps; the threshold is conservative


## Placement classifier: beam-K=2 descent

Online per-note placement against an already-built tree (triage's classifier, and the `Place` step of the build/place pairing). The same algorithm services both `Cluster` and `FromFolders` trees — they expose the same `ClusterNode` shape with centroids. Pure cosine; no LLM. [cluster-place-beam-descent]
status:: done
implements:: [[code:hiker/cluster/tree/place_beam_descent]]
note:: `core/src/cluster.rs::place_beam_descent(query, &dyn TreeView, beam_width) -> Option<PlacementMatch { leaf_node_id, confidence, margin }>` — beam-K descent over a renderer-agnostic `TreeView` trait (in-memory `InMemoryTree` impl provided; persistent stores plug their own). Query is L2-normalized on entry; centroids expected pre-normalized at tree-construction time. Top-1/top-2 margin computed across the final beam. The triage producer (`core::suggest::triage_match`) wraps a fresh `LoadedTreeView` over the tree `.md`'s `EditableNode`s and is the first real consumer (Sprint D). `min_confidence` / `min_margin` per-tree thresholds + dynamic beam-width storage on the tree row remain a future tunable; callers pass `beam_width = 2` (spec default) explicitly

```
Inputs:  query_embedding (the new note's embedding), tree (the saved ClusterTree)
Output:  PlacementMatch { leaf_node_id, confidence, margin }

Algorithm:
  candidates = [tree.root]                       // beam at this level
  while candidates contain any non-leaf:
    expanded = []
    for each cluster node in candidates:
      score every child's centroid against query (cosine)
      take top-K children                        // K = 2 by default
      append to `expanded`
    candidates = top-K of expanded by score      // beam stays width-K across levels
  // candidates is now K leaves
  pick the leaf with highest cosine; that is the match
  confidence = that leaf's cosine
  margin     = top-1 cosine − top-2 cosine        // among the final K leaves
```

Tunables (per-saved-tree, in the `.md` frontmatter):

- `beam_width` (`K`) — default `2`. `K=1` is the cheap fallback ("greedy"); `K=3+` is robust but rarely needed at vault scale.
- `min_confidence` — default `0.55` (cosine). Matches below this threshold route to the outlier bucket if `include_outliers = true`; otherwise still apply at low confidence (per-policy `require_review = true` handles the gating).
- `min_margin` — default `0.05`. A match whose top-1 / top-2 margin is below this is "ambiguous" — same as below-confidence; routes to outlier or fires with `require_review` semantics depending on outlier policy.

Cost: `O(K · branching · depth)` cosines, ≈ a few hundred dot products on a 10k-vault tree. Microseconds; no LLM. The classifier is the per-note path; the full build pass is the rare batch path. Beam (`K=2`) over greedy (`K=1`) recovers the case where the true target sits in a sibling subtree the top cluster barely missed. Collapsed-tree scoring (compare to every node flat) is deferred.

`hiker mv` and drag-and-drop-move are *not* this. Manual user moves don't re-classify against the tree; they're authoritative. The classifier fires only on new-note-on-save and the modified-rerun pathways.

The triage producer wrapping this descent is `core/src/suggest.rs::triage_match` — it loads `cluster_nodes` rows, builds a `LoadedTreeView` (the on-disk version of `TreeView`), runs `cluster::place_beam_descent`, walks the matched leaf up via `resolve_effective_policy`, and stages one pending edit per Move/Tag match. `triage_all_saved_trees` iterates every tree where `state == "saved-as-triage"`. [triage-classifier-engine]
status:: done
implements:: [[code:hiker/suggest/triage_match]]
verifies:: [[code:hiker/suggest/tests]]
touches:: [[code:hiker/suggest]]


## Per-note placement (online, cheap)

The build recipe is the **batch / seed** path (offline, on `hiker reconcile`, expensive, rare). The complementary cheap path is **per-note placement** — dropping a single new note into the existing tree with no LLM calls or re-clustering, fully specced in `design.md` (greedy centroid descent over the curated tree, on note-create / significant-edit-save, writes a placement to frontmatter). Most of the time only `Place` runs; `Build` runs when the user wants to re-examine the structure.


## What consumes the tree

`cluster-editor.md` consumes the `ClusterTree` (shape below). Two flows downstream, both unaware to the build engine:

- **One-shot Apply** — review a built tree, set per-cluster policies, apply; each policied leaf emits a pending op (folder move and/or frontmatter tag) the user batch-reviews.
- **Saved-tree triage** — save a tree as the active classifier; new notes route against it via greedy centroid descent ([[spec:cluster-place-beam-descent]]), each match resolved through the matched node's policy.

## Summarization

The LLM naming path (`SummarizeMode::Llm`, the current default) takes cluster member titles + per-note summaries (or per-cluster summaries at higher levels) and produces a short summary + a proposed name via the prompt below. `core/src/cluster.rs::LlmSummarizer` wraps `core::llm::LlmClient` + the `cluster_summarize` bundled prompt (`core/prompts/cluster_summarize.md`, registered in `core::prompts`). `Summarizer::summarize` renders member titles + summaries into the prompt, spins a per-call current-thread tokio runtime to bridge sync→async, calls `chat`, and parses the model's JSON `{name, summary, confidence}` reply. Wired into `cluster_tree_create` / `cluster_tree_rebuild` / `cluster_op_recluster_subtree` via `build_cluster_summarizer` (errors if `[llm].enabled = false` — there is no non-LLM fallback). The Queue carrier (`RaptorSummarize`) stays as the fan-out pathway for batched sample-merge; the in-process direct path here is the simpler shape used during interactive builds. [cluster-summarize-llm]
status:: done
implements:: [[code:hiker/cluster/LlmSummarizer]], [[code:hiker/prompts/bundled_defaults]]
touches:: [[code:hiker/cluster]]

Concretely, the LLM proposes a 3–6 word name + a 1–3 sentence summary + a confidence score. [cluster-name-from-summary]
status:: planned

The PLANNED deterministic path is `SummarizeMode::Extractive` ([[spec:cluster-name-deterministic]], `bug-clustering-params-spec-drift`): top TF-IDF / KeyBERT-style terms over a cluster's members (document-frequency from the lexical/FTS index) + the most-central note's title as fallback, plus a short extractive summary. An `ExtractiveSummarizer` impl behind the `Summarizer` trait alongside `LlmSummarizer`; with a deterministic representation, the whole build + naming loop runs `[llm]`-disabled. [cluster-name-deterministic]
status:: planned

Prompt shape (sketch):

```
You are organizing a personal notes vault. The following N notes have been
grouped together based on semantic similarity. Produce:
- a 3-6 word topical name for the group
- a 1-3 sentence summary of what the group is about
- a confidence score 0.0-1.0 reflecting whether the group is coherent

Members:
- <title>: <summary>
- <title>: <summary>
...

Return strict JSON: {"name": ..., "summary": ..., "confidence": ...}
```

For a leaf cluster, the per-note summary input comes from the existing `Summary` enrichment (`design.md`, Summary enrichment) — cached on the note's frontmatter or in the store. For a parent cluster, the inputs are the child clusters' summaries; same prompt, no special-casing.

Confidence below a threshold (default 0.5) marks the cluster "uncertain" — shown but flagged for explicit review before applying.

**Routing per `llm.md`:** cluster summarization is a *fan-out* feature (one prompt per cluster) — calls flow through `core::llm` direct, no agent loop, no ACP. The summarizer is a pluggable `core::cluster::Summarizer` trait *on top of* `core::llm` (the trait owns prompt template / member-formatting / JSON parsing), the same discipline as embedder and store. `LlmSummarizer` is the production impl; `SummarizeMode::None` invokes no summarizer (names stay `"Cluster N"`, build runs without `[llm] enabled`); the planned `ExtractiveSummarizer` would be the deterministic third option. Provider/model are user-configured in `[llm]`; a small local model (e.g. `qwen2.5:3b` via Ollama) suffices — naming is easier than freeform writing. The clustering review tab drives Run on the structural pass and applies the naming mode at Confirm time.


## Cost model

Dominant cost: LLM calls for summarization. One call per cluster in the `Summarize` scope.

Rough numbers for a small local model (~3B, ~50ms/call on CPU), assuming a fresh build (full recipe: Split + Summarize { All }):

| Vault size | Leaf clusters | Intermediate clusters | Total summaries | Wall time |
| ---------- | ------------- | --------------------- | --------------- | --------- |
| 100 notes  | ~10           | ~3                    | ~13             | <1s       |
| 1k notes   | ~80           | ~15                   | ~95             | ~5s       |
| 10k notes  | ~500          | ~80                   | ~580            | ~30s      |

Embedding cost is negligible relative to summarization. The structural pass alone (Split, no LLM) is interactive — sub-second on small vaults, low single-digit seconds on 10k notes; the Summarize pass is the non-interactive part (LLM wall-clock). Running structural alone is fully supported (placeholder names; naming later, if ever).


## When it runs

- **New tree** (build) — explicit user action from the clustering review tab, runs the full pipeline once.
- **Never automatic.** No background build, no on-save full-pipeline trigger. Triage *can* run on note save but that's the cheap classifier ([[spec:cluster-place-beam-descent]]), not a re-cluster; only saving-as-triage and an explicit Re-build regenerate the tree.
- **Watcher does not drive the build** — its events drive the index (chunk/embed), not the tree.


## Output: the `ClusterTree`

The cluster pass produces a `ClusterTree`: [cluster-tree-output]
status:: done
implements:: [[code:hiker/trees/build_adapter/node_inserts]]
touches:: [[code:hiker/cluster]]
note:: `core/src/cluster.rs::BuiltClusterTree` + `BuiltClusterNode` — matches the spec's `ClusterTree` / `ClusterNode` shape (`id`, `members`, `centroid`, `radius`, `name`, `summary`, `confidence`). `levels[0]` is the leaf clusters (over notes); `levels.last()` is the highest-level set; `outliers` carries unplaced note ids. The placement-classifier's smaller `ClusterNode` (centroid + children only) stays as the traversal view; the build output is renamed `BuiltClusterNode` to avoid the type-name collision
implements:: [[code:hiker/cluster/build/impl#[`SplitBranchCtx<'a>`]flatten_split_forest]]

```rust
struct ClusterTree {
    levels: Vec<Vec<ClusterNode>>,    // levels[0] = leaf clusters, levels.last() = top-level clusters
    outliers: Vec<NoteId>,            // unplaced
}

struct ClusterNode {
    id: ClusterId,                    // ephemeral per-run; not durable across runs
    members: Vec<MemberRef>,          // notes (leaf clusters) or child clusters (parents)
    centroid: Vec<f32>,               // mean of member embeddings
    radius: f32,                      // 90th-percentile member distance from centroid
    name: String,                     // LLM-proposed
    summary: String,                  // LLM-generated
    confidence: f32,                  // 0.0-1.0 from summarizer
}
```

Cluster IDs are *not* stable across runs; durable cluster identity is the tree document's outline position-and-name (`cluster-editor.md`), not the build pass's ephemeral per-run ids.


## Module discipline

All clustering logic lives in `core::cluster`. Outside the module: `partition()` returns plain assignments and the build returns a neutral `BuiltClusterTree` of plain Rust types — no HDBSCAN-crate types leak past the boundary, and `core::cluster` has no dependency on tree *storage* types either. Turning a `BuiltClusterTree` into storage rows is the storage-side adapter `core::trees::build_adapter`, which depends downward on `core::cluster`, so the dependency is one-way (`trees → cluster`). Same swappability posture as `core::store` / `core::embed`: the algorithm choice is a default, and a future swap (GMM, agglomerative) should be a one-file rewrite. [cluster-module-discipline]
status:: done
note:: `core/src/cluster.rs` is the one home for every petal-clustering / ndarray import; build types (`BuildScope`, `BuildMethod`, `ClusterParams`, `FolderDeriveParams`, `BuiltClusterTree`, `BuiltClusterNode`) are plain Rust + serde. The `Summarizer` trait is the swap surface for LLM vs template vs future-cloud naming, living alongside the build pipeline rather than as a separate `core::summarize` module — fewer files, same trait-bounded discipline; can split into its own module if the impl set grows. `core::trees` mirrors the pattern for its `.md` store (see [[spec:trees-module-discipline]])
implements:: [[code:hiker/trees/build_adapter/persist]]

Summarization is its own module (`core::summarize`) since it has independent failure modes (LLM unavailable, slow, low-quality) and an independent swap surface (local model vs cloud vs extractive fallback).


## Out of scope

- Online incremental clustering of the *full* tree. The cheap online path is greedy descent against an already-built saved tree ([[spec:cluster-place-beam-descent]], used by triage) — that's not a rebuild, it's a classifier.
- Multi-axis trees (one tree per type — semantic, temporal, entity). Single semantic tree only at first; the multi-axis idea (`design.md`) is a later concern.
- Durable cluster identity carried by the *build pass*. Each build run produces a fresh tree with ephemeral per-run ids; the durable identity lives in the tree document (its outline position-and-name), owned by `cluster-editor.md`, not in the build output.
- Cross-vault clustering. One tree per vault.
- Trail discovery from clusters. Trails are user-authored only by design (see `design.md`); the clustering pipeline never proposes them.
