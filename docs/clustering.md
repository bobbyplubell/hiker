# Clustering

How Hiker builds a hierarchical tree of topics from an unorganized vault. This doc covers the *build* side only — turning embeddings into a tree of nodes with names. The tree is consumed by `suggestions.md`, which uses it as a recommendation engine (one-shot reorganization proposals + saved-tree inbox triage); it is **not** durable infrastructure that owns the user's organization. Per-note placement provenance, manual-vs-auto stickiness, durable cluster IDs, and a parallel curated-tree-vs-filesystem mental model are explicitly out of scope — see `design.md`'s "Auto-organization suggestions" section.

Not built in v1. Lands alongside the suggestions surface (post-v1, after the related-notes panel proves the index pipeline). Speccing now because (a) the build algorithm shapes what suggestions look like, (b) it determines the cost model that decides whether a one-shot run takes seconds or minutes, and (c) it's the thing the synthetic-corpus eval (`qa.md`) is supposed to validate.


## Operations framework

Tree construction and edits are composed from three primitive operations. The cluster editor surfaces each as a verb; the build recipe (below) is a canonical composition. Every action a user takes on a tree — initial build, manual edits, regenerating names, growing a parent layer — decomposes into a sequence of these three.

| Op | Signature | Effect |
| --- | --- | --- |
| **Split** | `Trees::split_cluster(target, params)` | Partitions a target cluster's leaves into child sub-clusters using `params.algorithm`. The target can be a real cluster node or a virtual root containing every note in the build scope. Recursive sub-splits are governed by `leaf_min_size` / `leaf_cohesion_threshold`. [cluster-op-split] |
| **Summarize** | `Trees::summarize(scope, params)` | Generates `name` + `summary` for every cluster matching `scope`. Decoupled from Split — clusters Split produces have placeholder names until Summarize runs against them. Idempotent on `StaleOrUnfilled`. [cluster-op-summarize-sweep] |
| **Roll-up** | `Trees::rollup(input_node_ids, params)` | Embeds each input cluster's `summary` text via `Embedder::embed_batch`, partitions those summary embeddings, and inserts the resulting groups as a new parent layer above the inputs. Requires every input cluster to have a non-empty `summary` (errors `MissingSummary { node_id }` otherwise). [cluster-op-rollup] |

The headline framing decisions:

- **Split is top-down divisive.** A Split against the virtual root produces a coarse top-level partition; recursive sub-splits descend into each top-level child's members. At each sub-split, the algorithm operates on actual note embeddings within the parent's member set, not on geometric centroids. The pathology of clustering already-distinct cluster centroids (centroids of well-separated clusters are by construction well-separated, so no partition algorithm finds structure at the parent level) doesn't arise — each Split level sees the right granularity of data. [cluster-op-split]
- **Summarize is its own op, never run implicitly by Split.** Split produces unnamed clusters with `name = "Cluster N"` placeholders. The user invokes Summarize explicitly via the cluster editor (per-node, multi-select subset, or scope-driven sweep), or the build recipe composes Split + Summarize as a canned recipe. This separation lets the user defer or skip LLM-naming entirely (run on a `[llm].enabled = false` vault, name clusters by hand, or batch-name later). [cluster-op-summarize-sweep]
- **Roll-up is the way to grow a parent layer.** When the user wants to coarsen an already-named tree's top level, they invoke Roll-up over the top-level cluster ids. Roll-up clusters the summary *embeddings* (not centroids of member embeddings) so the new parent layer reflects semantic similarity between *what the clusters are about*, not geometric similarity between their members. This is the RAPTOR-shaped step, available as an explicit user action rather than a step baked into every build. [cluster-op-rollup]

### Split

`Split` partitions a target cluster's leaves into sub-clusters and reparents the leaves under the new children. The target's own row is preserved (id, name, summary, user-edit flags, policy); only its descendants change.

Inputs: `target_node_id` (or a virtual-root sentinel for the build scope), `ClusterParams` (algorithm + per-algorithm tunables, plus the recipe's leaf stop conditions when invoked recursively).

Algorithm choice drives the partition step inside Split (see §"Algorithm choices" below). Leiden is the default; HDBSCAN, Hybrid, and a GMM stub are selectable.

Recursive sub-split — when Split is invoked with `recurse: true`, each newly-produced child whose member count exceeds `leaf_min_size` (default 5) and whose intra-cluster cohesion radius exceeds `leaf_cohesion_threshold` (default 0.15) is itself Split. Branches stop when either threshold trips. The recipe (below) sets `recurse: true`; the cluster editor's row-menu "Split" verb uses `recurse: false` (one level only). [cluster-op-split]

The `split` op's reverse edit snapshots the prior subtree for undo per `cluster-editor-undo-redo`.

### Summarize

`Summarize` generates `name` + `summary` for every cluster matching `scope`. One LLM call per cluster, batched through the task queue.

```rust
enum SummarizeScope {
    All,                                  // every cluster in the (sub)tree
    StaleOrUnfilled,                      // summary_membership_churn > 0 OR summary IS NULL
    Subset { ids: Vec<ClusterId> },       // arbitrary set, typically from multi-select
}

struct SummarizeParams {
    scope: SummarizeScope,
    subtree_root: Option<ClusterId>,      // None = whole tree; constrains All / StaleOrUnfilled
    recursive: bool,                      // default true; only relevant for subtree_root != None
    summarize: SummarizeMode,             // None / Template / Llm
    overwrite_user_edited: bool,          // default false
}
```

Scope semantics:

- **`All`** — every cluster row in the (sub)tree.
- **`StaleOrUnfilled`** — every cluster row where `summary_membership_churn > 0 OR summary IS NULL OR name IS NULL`. The staleness counter (`cluster-summary-staleness-counter`) is the load-bearing infrastructure; every reshape op already bumps it, so this scope is a no-op once everything is fresh.
- **`Subset { ids }`** — exactly the listed cluster ids; useful for "Summarize this one cluster" (single-element subset) and "Summarize selected" (multi-select subset). Out-of-tree ids are silently dropped.

`overwrite_user_edited` defaults to `false`: clusters whose `user_edited_name` or `user_edited_summary` flag is set get skipped. Set to `true` for explicit per-node Regenerate. The flag composes with `scope` — a `Subset` summarize with `overwrite_user_edited = false` against a user-edited node is a no-op (returns `SkippedUserEdited`).

`SummarizeMode::None` short-circuits without invoking the summarizer; the cluster keeps its placeholder name. This is the path used by the build recipe when the user wants Split + Roll-up without naming.

Queue integration: every `Summarize` invocation enqueues one `TaskKind::ClusterSummarize { tree_id, scope_kind, n_targets }` row that the user can watch in the queue. Per-cluster `RaptorSummarize` tasks (already specced as `cluster-editor-regenerate-via-task-queue`) are the fan-out underneath. [cluster-op-summarize-sweep]

Bottom-up submission ordering carries over from `cluster-editor-regenerate-via-task-queue` — when the scope contains parent clusters, those tasks are submitted only after their children's tasks complete so the parent's summary input is the children's freshly-generated names, not placeholders.

### Roll-up

`Roll-up` grows a new parent layer over a set of existing clusters by clustering the *summary embeddings* (not the geometric centroids of member embeddings).

```rust
struct RollupParams {
    input_node_ids: Vec<ClusterId>,
    algorithm: ClusterAlgorithm,          // same as Split
    leiden: LeidenParams,
    min_cluster_size: u32,                // for HDBSCAN
    new_layer_name_pattern: Option<String>,  // default "Group {n}"
}
```

Steps:

1. Validate every `input_node_id` exists in the same tree and carries a non-empty `summary`. Errors `MissingSummary { node_id }` on the first violation. The caller's responsibility to run `Summarize` first.
2. For each input cluster, embed its `summary` text via `Embedder::embed_batch`. The summary embedding's dim is whatever the loaded embedder reports (`embedder-dim-from-model`); the result is stored only in memory for the partition step, not persisted.
3. Run the partition algorithm (`partition` for HDBSCAN, `partition_leiden` for Leiden) over the summary embeddings.
4. For each resulting community, insert a new cluster node at the layer above the inputs. Each new parent's members = the input clusters that landed in its community; their `parent` is updated to point at the new parent. The new parents have placeholder names (`new_layer_name_pattern.unwrap_or("Group {n}")`); a subsequent `Summarize { Subset(new_parent_ids) }` invocation fills them in.
5. The new parents' `centroid` is the L2-normalized mean of their input clusters' summary embeddings. `radius` is the 90th-percentile cosine distance from centroid to inputs. `confidence` is inherited from the partition's confidence metric (HDBSCAN stability or Leiden modularity contribution).

If the partition produces a single community, Roll-up returns `Refused { reason: "all inputs landed in one community" }` without inserting any rows — a single super-parent is uninformative. The user can lower the algorithm's resolution (Leiden) or `min_cluster_size` (HDBSCAN) and re-invoke. Symmetrically, when every input lands in its own singleton community, Roll-up returns `Refused { reason: "no inputs merged" }`.

Roll-up does not recurse on its own — one invocation produces at most one new layer. The user runs Roll-up again over the new parents if they want a deeper hierarchy. [cluster-op-rollup]

The `rollup` op's reverse edit snapshots the prior `parent`s of the inputs + the new parent nodes so undo restores the original top-level structure.


## Build recipe

"Build a tree from scratch" is a composition of the three ops, not a monolithic algorithm. The clustering review tab (`cluster-review-tab` in `cluster-editor.md`) drives the recipe:

```
1. Split { target: virtual_root(scope), params: ClusterParams { recurse: true, ... } }
   → produces a tree of leaf clusters with placeholder names

2. Summarize { scope: All, subtree_root: None, summarize: <user choice> }
   → fills name + summary on every cluster row

3. (Optional, default off) Rollup { input_node_ids: top_level_cluster_ids, ... }
   → grows a parent layer above the initial top level when the user wants
     additional hierarchy depth
```

[cluster-build-recipe]

Steps 1 and 2 are gated separately in the review tab — the structural pass (Run clustering) is step 1 alone; Confirm-and-name is step 1 followed by step 2; Confirm-no-naming is step 1 alone with the result persisted. Step 3 is invoked from the cluster editor's toolbar after the tree is persisted (not from the review tab); doing so during the build is unnecessary since top-down divisive Split already produces a hierarchical shape. The build uses top-down Split rather than recursive Roll-up because Roll-up would force a Summarize at every level (doubling the LLM dependency); Roll-up stays an explicit verb for coarsening an already-named tree.

Top-down divisive parameters that the recipe sets on the Split call:

- **`top_level_resolution`** (Leiden only) — γ override for the *first* Split call against the virtual root. Default `0.3`, lower than the default `1.0` used at sub-splits. Lower γ produces coarser, fewer communities at the top level (target: 3–8 broad clusters). Recursive sub-splits revert to `LeidenParams.resolution` for finer structure. Lives on `LeidenParams` as a new field with `#[serde(default = "default_leiden_top_resolution")]`. [cluster-leiden-params]
- **`leaf_min_size`** — recursive sub-split stops when a child has fewer members than this. Default `5`. On `ClusterParams`.
- **`leaf_cohesion_threshold`** — recursive sub-split stops when a child's intra-cluster cohesion radius (90th-percentile cosine distance from members to centroid) is below this. Default `0.15`. On `ClusterParams`.

The previously-used `min_clusters_to_recurse` knob is removed — the recursion termination criterion is now per-branch (member count / cohesion), not per-level (cardinality). Persisted `method` frontmatter for trees built before the recipe lands deserializes with the field absent and is treated as "use the new defaults" via `#[serde(default)]`. Saved trees do not store `min_clusters_to_recurse` going forward.

Surviving / changed knobs at a glance:

| Knob | Change | Notes |
| --- | --- | --- |
| `min_cluster_size` | survives | HDBSCAN tunable, drives partition at every level |
| `min_samples` | survives | HDBSCAN tunable |
| `min_clusters_to_recurse` | **removed** | replaced by per-branch leaf conditions |
| `summary_confidence_threshold` | survives | marks below-threshold clusters as "uncertain" in the review surface |
| `include_outliers` | survives | controls force-routing of outliers at the top-level Split |
| `disable_recursion` | survives | when `true`, Split runs once and returns; equivalent to `recurse: false` on the top-level Split |
| `leaf_min_size` | new | recursive Split stop condition |
| `leaf_cohesion_threshold` | new | recursive Split stop condition |
| `leiden.top_level_resolution` | new | first-Split γ override; recursive sub-splits use `leiden.resolution` |
| `leiden.resolution` | survives | sub-split γ |
| `leiden.k_nearest`, `edge_weight_floor`, `iterations`, `min_cluster_size` | survive | unchanged |


## Async execution and progress

The structural pass runs on a background task so the UI thread stays responsive. Producers (the cluster review tab is the only one in v1) submit a build request and consume a stream of progress events from `core::cluster`:

- `Phase { phase }` — current pipeline phase; phase variants cover `LoadingEmbeddings`, `PartitioningLevel(u32)`, and `Finalizing`. Emitted at the start of each phase.
- `Counters { items_processed, clusters_found, outliers }` — running totals; emitted as the partitioner advances.
- `ClusterDiscovered { node: BuiltClusterNode, parent: Option<ClusterId> }` — emitted as each new cluster is added to the in-flight tree. Consumers use this to incrementally reveal clusters in the review surface (per `cluster-review-tab-live-cluster-reveal`) rather than waiting for the full tree.
- `Done { tree: BuiltClusterTree }` — terminal: the full tree is ready.
- `Cancelled` — terminal: the producer signalled cancel and the pass aborted cleanly.
- `Failed { error }` — terminal: the pass errored out (partition refused, embeddings missing, etc.).

The stream is owned by `core::cluster` and consumed by the producer through a channel-shaped interface (concrete IPC shape lives in the producer's command layer, not pinned here). Cancellation is cooperative: the producer signals cancel via a shared atomic, the pass checks at every level boundary and on a periodic per-node interval inside the partition loop, drops the in-flight partition results, and emits `Cancelled`. [cluster-build-async-pass, cluster-build-progress-stream]

The LLM summarization pass (Summarize op) is async via the task queue (`cluster-op-summarize-sweep`) and not part of this stream — structural and naming are separate operations with separate progress surfaces.


## Note embeddings input

Every Split operates on note-level embeddings, regardless of where in the tree it runs. A Split against the virtual root sees every note in the build scope; a Split against a real cluster sees that cluster's leaves' embeddings.

The note embedding is the mean of the note's chunk embeddings, weighted by chunk byte length. Computed inline by the indexer's per-file upsert and persisted on the `notes` row (`note_embedding BLOB`) in the same transaction as the chunks. Refreshed on every upsert so the pool tracks the chunk set; notes with no chunks leave the column NULL and are excluded from clustering. Cheap — a vector mean over typically <20 chunks — and avoids spending a separate embedder pass on each note. [cluster-note-embeddings]

Mean-pool rather than embedding the full note text directly: `bge-small`'s 512-token (~2000 char) context silently truncates longer notes, and personal-vault notes routinely exceed that. Mean-pool over chunks (each ~1200 chars, so each fits) sidesteps the limit and reflects the chunker's heading-bounded structure — no practical max note size.

When the user selects a long-context embedder via `embedder-model-selectable` (`bge-m3` at 8k tokens, `embedding-gemma-300m` at 2k), direct full-note embed becomes viable for most notes; mean-pool stays as the fallback for outliers that still exceed the model's context. Implementing the direct-embed path is deferred — the mean-pool path already works for every model and isn't observably wrong; the direct-embed quality win is a follow-up that lands when there's evidence the difference matters. [cluster-note-embeddings-direct-long-context]

Empty notes (no chunks) get no embedding and are excluded from clustering — they end up in the "inbox" (unplaced) bucket, same as outliers.


## Algorithm choices

`ClusterParams.algorithm` selects which partitioner runs inside Split (and inside Roll-up). Four variants: Leiden (default), HDBSCAN, Hybrid, GMM-stubbed.

### Leiden (default)

Modularity-optimization community detection over a kNN cosine-similarity graph. Every node lands in some community; small communities (below `min_cluster_size`) post-flag as outliers. [cluster-leiden]

Default because loose `bge-small` embeddings often give HDBSCAN 0–1 cohesive clusters plus everything-as-outliers, while Leiden produces 5–20 communities reflecting the topical structure. γ is a direct granularity knob for both the coarse top-level pass (`top_level_resolution` `0.3`) and the finer sub-splits (`resolution` `1.0`).

Pipeline: [cluster-leiden-knn-graph]

1. L2-normalize the input embeddings so cosine similarity reduces to a dot product.
2. For each point, find its top-`k_nearest` neighbors by cosine similarity (brute-force O(n²) — same scale ceiling as HDBSCAN's path).
3. Drop neighbor edges whose weight (cosine similarity) is below `edge_weight_floor`.
4. Construct an undirected graph (edges symmetrize on insertion; mutual-kNN is too aggressive at vault scale).
5. Construct a `single_clustering::network::CSRNetwork` from the deduped edges (`from`, `to`, `cosine_weight`) plus a unit `node_weights` vec.
6. Build a `RBConfigurationPartition<f64, VectorGrouping>` with the configured `resolution` (γ — the recipe substitutes `top_level_resolution` on the first call), then run `LeidenOptimizer::optimize_single_partition` over it.
7. Each node's community id comes from `partition.membership(node)`; group nodes by id, densify the surviving ids, emit `ClusterAssignment` per input point.
8. Communities smaller than `min_cluster_size` flag as outliers (post-filter, not algorithmic).

Tunables: [cluster-leiden-params]

- `k_nearest` — number of nearest neighbors per node. Default `15`. Smaller = sparser graph = more, smaller communities; larger = denser graph = fewer, bigger communities. Clamped to `n-1` so it doesn't ask for more neighbors than exist.
- `edge_weight_floor` — minimum cosine similarity for a kNN edge to survive. Default `0.0` (keep every kNN edge). Raise to strip weak neighbor links and tighten community boundaries.
- `resolution` (γ) — Reichardt-Bornholdt resolution parameter on sub-splits. Default `1.0`. γ > 1 biases toward finer / more communities; γ < 1 toward coarser / fewer.
- `top_level_resolution` — γ override for the *first* Split call against the virtual root (when Split is invoked from the build recipe). Default `0.3` — explicitly coarser than `resolution` to produce 3–8 broad top-level clusters that the recursive sub-splits drill into. Ignored when Split is invoked on a non-virtual target.
- `iterations` — cap on Leiden refinement iterations (`LeidenConfig.max_iterations`). Default `100`. Algorithm converges fast; the cap is a safety rail.
- `min_cluster_size` — minimum community size; smaller communities flag as outliers. Default `2`.

**Crate choice: `single-clustering` 0.6.1.** BSD-3-Clause licensed; pure Rust; builds on aarch64-linux (verified locally). Provides both `ModularityPartition` and `RBConfigurationPartition` (used here for the resolution parameter), plus optional HNSW-based kNN primitives we deliberately do not use — our hand-rolled cosine kNN is clear about its weight semantics where the crate's "Gaussian" variants apply `exp(-d²/σ²)` weighting. The crate is marked "under heavy development" in its README, so the workspace dep is **exact-version pinned to `=0.6.1`** — any bump is a deliberate re-test. Public surface used: `CSRNetwork::from_edges`, `LeidenConfig`, `LeidenOptimizer::new` + `optimize_single_partition`, `RBConfigurationPartition::with_resolution`, `VectorGrouping`, `VertexPartition::membership`. Repository: <https://github.com/SingleRust/single-clustering>. [cluster-leiden-crate-single-clustering]

### HDBSCAN

Density-based hierarchical clustering. Selectable via `ClusterParams.algorithm = Hdbscan`. [cluster-hdbscan]

Picks over Leiden when:

- The vault has a long miscellaneous tail the user wants explicitly bucketed as outliers (HDBSCAN labels low-density points as outliers natively; Leiden places every point and post-filters singletons).
- The user wants density gaps respected (two topically-close clusters separated by a thin density bridge stay separate under HDBSCAN; under Leiden they may merge if mutually kNN-connected).

Tunables:
- `min_cluster_size` — smallest cluster the algorithm will form. Default 5; user-overridable per vault in `vault/.hiker/config.toml`.
- `min_samples` — density threshold. Default = `min_cluster_size`. Higher → more outliers.
- Distance metric — cosine, on the note embeddings.

Watch for: small vaults (<50 notes) where HDBSCAN may produce all-outliers and an empty tree. Fallback: if the top-level Split produces fewer than 2 clusters, surface a "vault is too small" message rather than a misleading tree of one node.

**Crate choice: `petal-clustering`.** Rust-native HDBSCAN implementation, MIT-licensed, no C/C++ FFI, used in production by the Petabi suite. Builds clean on the project's target stack (Rust workspace), no extra system deps. The crate exposes `Hdbscan::new(min_cluster_size, min_samples).fit(&data) -> Vec<i32>` plus cluster-stability metadata — exactly the surface `core::cluster::partition` needs. Vector distance is cosine via pre-normalized embeddings (the crate operates on Euclidean by default; we normalize once and pass-through). [cluster-hdbscan-crate-petal]

### Hybrid mode

HDBSCAN runs first; outliers get reassigned to the nearest cohesive cluster's centroid if cosine ≥ 0.6. Selectable via `ClusterParams.algorithm = Hybrid`. Useful when HDBSCAN's default outlier set is too aggressive but the user doesn't want pure Leiden's "every point gets a home" posture either. Soft members tag distinctly in the cluster row so the cluster editor can render them differently from primary members. Not wired under `algorithm = Leiden` (Leiden places every point in a community by construction, so there's no outlier set to recover). [cluster-hybrid-outlier-recovery]

### GMM (stub)

`ClusterParams.algorithm = Gmm` is reserved; the runtime falls back to HDBSCAN with a warning until a GMM crate goes through dep review. Linfa-clustering doesn't ship GMM in the form needed; petal doesn't have GMM at all. Real GMM lands when the dep choice is made. [cluster-algorithm-selectable]

### Algorithm selection

`cluster.algorithm` lives in `vault/.hiker/config.toml` as a per-vault default; the clustering review tab's Advanced disclosure overrides it per build. Per-vault rather than per-user — different vaults have different shapes (a structured reference vault vs. a fleeting-thoughts journal cluster very differently). [cluster-algorithm-selectable]


## Build scope

`core::cluster::build_tree(scope, method, params)` takes a `BuildScope` describing which notes participate in the build pass, a `BuildMethod` selecting how the tree is constructed, and method-specific parameters. The cluster editor's "Suggest reorganization" action picks all three at invocation time (per `cluster-review-tab-config-section`); saved Evergreen trees record them so triage knows which notes the tree classifies and how to rebuild against fresh data. [cluster-build-scope, cluster-build-method]

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

**Source-type filter** [cluster-build-scope-source-types]. Each variant carries an optional `source_types: Vec<String>` of canonical lower-case extensions. The build-pass + triage classifier only see notes whose extension is in the list; `"md"` covers both `.md` and `.markdown` (the indexer treats them as the same source type per `INDEXABLE_EXTENSIONS`). An empty vec is the legacy "every indexable extension" posture and is what every pre-feature persisted tree's `scope` frontmatter deserializes to. Surfaced in the clustering review tab's config section as checkboxes (Markdown / Plain text), default both on. Enforced in two places:

1. `notes_for_scope` (the build-pass resolver) filters resolved paths via `BuildScope::matches_path` before reading embeddings — a tree built with `source_types = ["md"]` simply never sees `.txt` notes.
2. `triage_all_saved_trees` (the on-save classifier) reads each saved tree's `scope` frontmatter and skips trees whose `source_types` filter rejects the saved note's path. A `.txt` note save against a Markdown-only triage tree no-ops; the same note save against a mixed-type tree fires normally.

The clustering algorithm itself is unaware of scope — `BuildScope` is resolved into a `Vec<NoteId>` by `core::cluster::build_tree`'s caller-facing entry, the recursive pass operates on whatever embeddings are handed in. The `min_cluster_size` fallback for small vaults (`<50 notes` → skip with "vault is too small") applies per-scope: a `Folder` scope with three notes gets the same skip message.

Saved triage trees persist their scope; the triage classifier (`cluster-place-beam-descent`) only evaluates new/modified notes whose path falls within the saved scope. Triage's existing safety rule (notes outside the configured triage scope are never moved out of their folder) intersects with the build scope — a saved tree built over `research/` can still only auto-move notes that live in the triage scope (default `inbox/`).


## Build method

`BuildMethod` selects how the tree is constructed from the resolved note set. Two methods, each with its own parameter shape:

```rust
enum BuildMethod {
    Cluster   { params: ClusterParams },      // runs the build recipe (default)
    FromFolders { params: FolderDeriveParams }, // mirror the filesystem hierarchy
}

struct ClusterParams {
    algorithm: ClusterAlgorithm,    // leiden (default) / hdbscan / gmm / hybrid (cluster-algorithm-selectable)
    leiden: LeidenParams,           // Leiden-only knobs incl. top_level_resolution
    min_cluster_size: u32,          // HDBSCAN tunable
    min_samples: Option<u32>,       // HDBSCAN tunable; None → defaults to min_cluster_size
    leaf_min_size: u32,             // recursive Split stops below this member count (default 5)
    leaf_cohesion_threshold: f32,   // recursive Split stops below this radius (default 0.15)
    summary_confidence_threshold: f32, // marks clusters "uncertain" below this
    include_outliers: bool,         // when false, force-routes outliers into nearest cluster
    summarize: SummarizeMode,       // llm / template / none — invoked by Summarize op, not by Split
    disable_recursion: bool,        // when true, Split runs once and stops (per cluster-review-tab-disable-recursion)
}

struct FolderDeriveParams {
    summarize: SummarizeMode,
    include_outliers: bool,         // default true; gates outlier detection at triage time
    outlier_threshold: f32,         // cosine-distance threshold; matches farther than this become outliers
}
```

[cluster-build-method, cluster-build-params]

### `Cluster` method

Default. Runs the build recipe (`cluster-build-recipe`): top-down divisive `Split` from the virtual root, followed by `Summarize { All }` if the user picks Confirm-and-name (else the tree persists with placeholder names and the user runs `Summarize` later). `ClusterParams.include_outliers` defaults to `true`; setting it to `false` runs the existing hybrid-mode outlier-recovery pass with the threshold lowered to absorb every outlier into its closest cluster (no minimum confidence — every note gets a home). [cluster-build-cluster-method]

### `FromFolders` method

Skip clustering entirely. Walk the filesystem under the build scope; produce a `ClusterTree` whose structure mirrors the folder hierarchy:

- One `ClusterNode` per folder (kind `Cluster`).
- One leaf node per note, parented to the folder it lives in.
- Root = the scope's root (`vault/` for `Vault`, `<rel>/` for `Folder(rel)`, a synthetic single-cluster root for `Notes(ids)`).
- Centroids: mean of member embeddings (same as `Cluster` method's leaf-level centroids). Computed lazily on save-as-triage so the placement classifier (`cluster-place-beam-descent`) works against the folder-derived tree the same way it works against a `Cluster`-method tree.
- Outliers at build time: not generated. Every note already in the scope has a folder; the build's leaf-set is exactly the scope's notes.
- Outliers at triage time (Evergreen use): the saved tree's `outlier_threshold` parameter gates whether new notes that don't fit any existing folder go to the outlier bucket. When the placement classifier descends to the nearest folder and finds the final cosine distance exceeds `outlier_threshold` (default `0.5`, configurable per `FolderDeriveParams`), the note is routed to the outlier bucket instead of force-fit into the nearest folder. The outlier bucket node is created lazily on the first such match. The bucket can carry its own policy (per `cluster-editor-outlier-policy`) — typically `Move` to an `inbox/unsorted/` folder. Setting `include_outliers = false` disables this check; every new note is routed to its nearest existing folder regardless of similarity. [cluster-build-from-folders-outliers]
- Confidence: 1.0 on every node (the folder structure is the source of truth, not a probabilistic guess).
- Summaries: per `SummarizeMode` — `llm` runs the same prompt as the clustering pipeline (per `cluster-summarize-llm`) over each folder's member titles + summaries; `none` leaves summary empty. Name defaults to the folder's basename.

[cluster-build-from-folders]

For users with already-organized vaults: build the Evergreen tree on their actual folders rather than the partitioner's guess. Triage then just finds the most similar existing folder for a new note.

The output `ClusterTree` shape is identical to the `Cluster` method's — same `ClusterNode` type, same downstream consumers. The cluster editor doesn't distinguish how a tree was built once it exists; reshape operations (merge / split / move-note) work the same way. Splitting a folder-derived node re-runs HDBSCAN against just that node's members — a folder-derived tree can grow cluster-derived subtrees through user editing without ceremony. The `method` frontmatter records the original build method for reference and re-build. [cluster-build-from-folders-uniform-output]

### FromFolders live-update

A saved FromFolders Evergreen tree tracks the filesystem. When a note moves between folders (user drag-drop in the file tree, accepted `move_note` staging row from any surface, manual `hiker mv`, external rename caught by the watcher), `core::trees` updates the affected nodes in the tree's frontmatter in place — the leaf's `parent` flips to the new folder's node id. Cluster nodes for newly-created folders are added on the fly; emptied folders' nodes are dropped (unless they carry an explicit policy, in which case they're kept as empty placeholders so the user's rule survives a transient empty state).

The trigger is the same watcher file events rename event the indexer already consumes; `core::trees` subscribes alongside it. The update is incremental — no re-build, no re-summarization, no LLM call. Centroids are recomputed for affected clusters (cheap: a vector mean over members). [cluster-build-from-folders-live-update]

**Staleness counter.** Each cluster node carries a `summary_membership_churn` integer (initialized to 0 at summary-generation time). Every leaf insert or remove within a cluster's subtree increments the counter on that cluster and all its ancestors. The counter is the user-visible "your summary may be out of date" signal — surfaced as a `↻ N` badge on the node's row in the cluster editor and as a soft-tinted node color in the graph view. The counter resets to 0 when the user runs Regenerate on that node. [cluster-build-from-folders-summary-staleness]

The counter applies to FromFolders trees primarily, where filesystem moves drive churn. `Cluster`-method trees use the same field for reshape operations (move-note-between-clusters / merge / split via the cluster editor) — same column, same UI treatment. The counter is also the staleness signal consumed by `cluster-op-summarize-sweep`'s `StaleOrUnfilled` scope: a Summarize sweep runs the LLM on exactly the clusters with `summary_membership_churn > 0 OR summary IS NULL`. [cluster-summary-staleness-counter]


### Re-building Evergreen trees

A saved Evergreen tree records both its scope and method in its `.md` frontmatter. The "Re-build" action in the cluster editor (per `cluster-editor-mode-menu`) runs `build_tree(scope, method, params)` again with the saved parameters, producing a fresh tree. The user reviews the diff (deferred per `cluster-editor-tree-diff-view`) or accepts the new tree as the active Evergreen, retiring the previous one to the vault's trash. [cluster-build-rebuild]


## Placement classifier: beam-K=2 descent

Online per-note placement against an already-built tree (triage's classifier, and the `Place` step of the build/place pairing). The same algorithm services both `Cluster` and `FromFolders` trees — they expose the same `ClusterNode` shape with centroids. Pure cosine; no LLM. [cluster-place-beam-descent]

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

Cost: `O(K · branching · depth)` cosines, ≈ a few hundred dot products on a 10k-vault tree. Microseconds; no LLM. The classifier is the per-note path; the full build pass is the rare batch path.

Beam over greedy (`K=1`): greedy can pick an almost-right top cluster while the true target sits in a sibling subtree it barely missed; `K=2` recovers that for trivial cost. Collapsed-tree scoring (compare to every node flat) is more accurate but loses the speedup; deferred.

`hiker mv` and drag-and-drop-move are *not* this. Manual user moves don't re-classify against the tree; they're authoritative. The classifier fires only on new-note-on-save and the modified-rerun pathways.


## Per-note placement (online, cheap)

The build recipe in this doc is the **batch / seed** operation — runs only on `hiker reconcile`, produces a fresh tree, expensive enough to be worth doing rarely. The complementary cheap operation is **per-note placement**: drop a single new note into the existing tree without touching anyone else's placement, no LLM calls, no re-clustering.

Per-note placement is fully specced in `design.md:252-257` (greedy centroid descent over the existing curated tree). Mentioned here to make the pairing explicit:

- **Build / reseed (this doc):** offline, on `hiker reconcile`, batch over the whole vault, produces a `ClusterTree` proposal.
- **Place (`design.md:252`):** online, on note-create or note-edit-saves-significant-content, embedding-only descent over the existing tree, writes a placement to the note's frontmatter.

Most of the time only `Place` runs — every new note gets a home cheaply. `Build` runs when the user decides it's worth re-examining the structure (new corpus shape, the tree feels stale, after a large import).


## What consumes the tree

The build pipeline produces a `ClusterTree` (shape below). `suggestions.md` consumes it as a recommendation engine — *not* as durable infrastructure that owns the user's organization. Two flows downstream:

- **One-shot reorganization** — generate a tree, render it as a markdown proposal with checkboxes, user picks what to apply (folder moves and/or frontmatter tags). Tree is ephemeral; nothing persists except the user's accepted actions.
- **Saved-tree triage** — user saves a generated tree as a classifier; new notes get routed against it via greedy centroid descent (`cluster-place-beam-descent`), with confidence-tiered behavior (auto-apply / queue-for-review / leave-in-inbox).

The build engine is unaware of which flow consumes its output — same algorithm, same `ClusterTree`. See `suggestions.md` for everything downstream.

## Summarization

One LLM call per cluster per level. Input: cluster member titles + per-note summaries (or per-cluster summaries at higher levels). Output: a short cluster summary (1–3 sentences) and a proposed name (3–6 words). [cluster-summarize-llm, cluster-name-from-summary]

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

For a leaf cluster, the per-note summary input comes from the existing `Summary` enrichment (`design.md:293`) — already cached on the note's frontmatter or in the store. For a parent cluster (anything with children that are themselves clusters), the inputs are the child clusters' summaries; same prompt, no special-casing.

Confidence below a threshold (default 0.5) marks the cluster as "uncertain" — the suggestions flow shows it but flags it for explicit review before applying.

**Routing per `llm.md`:** cluster summarization is a *fan-out* feature (one prompt per cluster, scope determined pre-batch by the cluster set). Calls flow through `core::llm` direct — no agent loop, no ACP. The summarizer's pluggable surface (`core::cluster::Summarizer` trait) is a thin layer *on top of* `core::llm`: the trait owns the prompt template, member-formatting, and JSON parsing; the LLM call itself goes through `core::llm`. Same discipline pattern as embedder and store. `LlmSummarizer` is the only LLM-backed production impl; the build pipeline also supports `SummarizeMode::None` as a fully-supported *structural pass* — no `Summarizer` is invoked, names default to `"Cluster N"` placeholders, and the build runs end-to-end without `[llm] enabled`. The clustering review tab (`cluster-editor.md` § Clustering review tab) drives Run on this path and only fires the LLM-backed naming at Confirm time.

Model choice: provider/model are user-configured in `[llm]` per `llm.md`; a small local model via Ollama (e.g. `qwen2.5:3b`) is enough — cluster naming is easier than freeform writing. The Confirm-and-name path requires `[llm] enabled = true` because it submits `RaptorSummarize` tasks that route through `core::llm`; the structural pass (Run, before Confirm) is LLM-optional and works even when `[llm]` is disabled — names land as placeholders and the cluster editor becomes the source of truth for naming.


## Cost model

Dominant cost: LLM calls for summarization. One call per cluster in the `Summarize` scope.

Rough numbers for a small local model (~3B, ~50ms/call on CPU), assuming a fresh build (full recipe: Split + Summarize { All }):

| Vault size | Leaf clusters | Intermediate clusters | Total summaries | Wall time |
| ---------- | ------------- | --------------------- | --------------- | --------- |
| 100 notes  | ~10           | ~3                    | ~13             | <1s       |
| 1k notes   | ~80           | ~15                   | ~95             | ~5s       |
| 10k notes  | ~500          | ~80                   | ~580            | ~30s      |

Embedding cost (one extra call per cluster summary) is negligible relative to summarization.

The structural pass alone (Split, no LLM) is interactive — sub-second on small vaults, low single-digit seconds on 10k notes. It runs on a background task with streaming progress (per `cluster-build-async-pass`) so the UI never blocks even when the pass is slow. The Summarize pass is the part that is *not* interactive: it spends real wall-clock time waiting on LLM calls and runs as a fan-out of queue tasks the user watches in the queue widget. The two passes are decoupled — running structural alone is a fully supported outcome (placeholder names; naming runs later, if ever).


## When it runs

- **`hiker suggest`** (one-shot) — explicit user command, runs the full pipeline once, output goes to `suggestions.md`'s proposal flow.
- **Saved-tree triage** does *not* re-run the build pipeline. Triage uses the cheap greedy-descent classifier (`cluster-place-beam-descent`) against an already-saved tree; only `hiker suggest save` and re-running `hiker suggest` regenerate the tree itself.
- **Never automatic.** No background build pass, no on-save trigger for the full pipeline. Triage *can* run on note save (per `suggestions.md`) but that's the cheap classifier, not a re-cluster.
- **Watcher does not drive the build.** Watcher events drive the *index* (chunk/embed updates); they don't drive the tree.


## Output: what suggestions consume

The cluster pass produces a `ClusterTree`: [cluster-tree-output]

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

`suggestions.md` consumes this shape — rendering the markdown proposal for one-shot review, or persisting a slimmed-down version (centroids + names + target folders/tags) as the saved-tree classifier. Cluster IDs are *not* expected to be stable across runs; the rejection-history bookkeeping in `suggestions.md` keys on member-set fingerprints, not cluster IDs.


## Module discipline

All clustering logic lives in `core::cluster`. Outside the module: `partition()` returns plain assignments, `build_tree()` returns a `ClusterTree` of plain Rust types. No HDBSCAN-crate types leak past the boundary. Same reasoning as `core::store` and `core::embed` — algorithm choice is a defensible default, not a permanent commitment, and the future swap (GMM, agglomerative, leiden) should be a one-file rewrite. [cluster-module-discipline]

Summarization is its own module (`core::summarize`) since it has independent failure modes (LLM unavailable, slow, low-quality) and an independent swap surface (local model vs cloud vs template fallback).


## Out of scope

- Online incremental clustering of the *full* tree. The cheap online path is greedy descent against an already-built saved tree (`cluster-place-beam-descent`, used by triage) — that's not a rebuild, it's a classifier.
- Multi-axis trees (one tree per type — semantic, temporal, entity). Single semantic tree only at first; the multi-axis idea from `design.md:215` is a later concern.
- Durable cluster identity across runs. Each build run produces a fresh tree; rejection-history bookkeeping happens at the per-suggestion level in `suggestions.md`, not at the cluster-id level.
- A parallel "curated tree" mental model alongside the filesystem. The filesystem is the only source of truth for organization; the build engine is a recommendation tool, not a structural overlay.
- Cross-vault clustering. One tree per vault.
- Trail discovery from clusters. Trails are user-authored only by design (see `design.md`); the clustering pipeline never proposes them.
