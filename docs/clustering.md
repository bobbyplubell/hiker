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

### Split

`Split` partitions a target's leaves and reparents them under new children, preserving the target's own row (name, summary, policy). Inputs: `target_node_id` (or a virtual-root sentinel for the build scope) and `Params` (algorithm + tunables + recursion flags + leaf stop conditions).

Algorithm choice drives the partition (§"Algorithm choices"): Leiden default, HDBSCAN / Hybrid / GMM-stub selectable. Split is flat by default, recursion opt-in. Each level operates on actual note embeddings within the parent's member set, not geometric centroids (§"Note embeddings input"), so re-clustering already-distinct centroids never arises.

Recursion is **two bools** on `Params`: `disable_recursion` and `recurse`. The default is a single flat partition; `recurse` opts into descending every branch until a stop condition trips (per-branch member count below `leaf_min_size` default 5, or cohesion radius below `leaf_cohesion_threshold` default 0.15). The build recipe defaults to flat; the user opts into recursion in the review tab or deepens by hand. [cluster-op-split, cluster-recursion-modes]

PLANNED (`bug-clustering-params-spec-drift`): a `RecursionMode::{Flat, Manual, Auto}` enum replacing the two bools, plus a build-time `max_depth` becoming a tunable `Params` field.

The `split` op's reverse edit snapshots the prior subtree for undo per `cluster-editor-undo-redo`.

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
- **`StaleOrUnfilled`** — every cluster row where `summary_membership_churn > 0 OR summary is empty OR name is a `Cluster N` placeholder`. The staleness counter (`cluster-summary-staleness-counter`) is load-bearing; every reshape op bumps it, so this scope is a no-op once everything is fresh.
- **`Subset { ids }`** — exactly the listed cluster ids; "Summarize this one cluster" (single-element) and "Summarize selected" (multi-select). Out-of-tree ids are silently dropped.

`force` defaults to `false`: a cluster whose name is *not* a `Cluster N` placeholder is left alone — its name came from the user or a prior pass. Set `force = true` for explicit per-node Regenerate. A `Subset` summarize with `force = false` against an already-named cluster is a no-op (returns `SkippedNamed`).

`SummarizeMode` is `Llm` (default) or `None`. `Llm` runs one LLM call per cluster (the prompt in §"Summarization"). `None` short-circuits without invoking the summarizer; the cluster keeps its placeholder name — structure without naming. PLANNED (`bug-clustering-params-spec-drift`): an `Extractive` mode (deterministic TF-IDF / KeyBERT-style naming, most-central note's title as fallback) for a fully model-free loop with `[llm]` disabled.

Queue integration: every `Summarize` enqueues one `TaskKind::ClusterSummarize { tree_id, scope_kind, n_targets }` row the user watches; per-cluster tasks fan out underneath (`cluster-editor-regenerate-via-task-queue`). Submission is bottom-up — parent tasks run only after their children complete, so a parent summarizes over the children's fresh names, not placeholders. [cluster-op-summarize-sweep]

### Roll-up

`Roll-up` grows a new parent layer over a set of existing clusters by clustering their chosen **representation** (`cluster-representation`) — centroids by default, summary embeddings or a lexical vector optionally.

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

The `rollup` op's reverse edit snapshots the inputs' prior `parent`s + the new parent nodes for undo.

**Manual wrap.** Wrapping a selected set under one new parent — a pure structural edit (no algorithm/representation): insert a parent, reparent the selection, placeholder name. The by-hand counterpart to automatic Roll-up. [cluster-op-wrap]


## Build recipe

"Build a tree from scratch" is a composition of the three ops, not a monolithic algorithm. The clustering review tab (`cluster-review-tab` in `cluster-editor.md`) drives the recipe:

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

Steps 1 and 2 are gated separately in the review tab — Run clustering is step 1; Confirm names is step 2 (LLM, or skipped). The result is flat unless the user set `recurse`. Down-recursion uses divisive Split (not recursive Roll-up, which would force a Summarize at every level); Roll-up stays the explicit coarsening verb.

The current `Params` set (`core::cluster::Params`):

- **`disable_recursion` / `recurse`** — the two recursion bools (§"Split"). Default is a single flat partition.
- **`leaf_min_size`** (default `5`) / **`leaf_cohesion_threshold`** (default `0.15`, 90th-percentile cosine distance from members to centroid) — per-branch stop conditions for recursive sub-split.
- **`min_cluster_size`** / **`min_samples`** — HDBSCAN tunables; `min_cluster_size` drives the partition at every level.
- **`summary_confidence_threshold`** — marks below-threshold clusters "uncertain" in the review surface.
- **`include_outliers`** — force-routes outliers at the top-level Split when false.
- **`min_clusters_to_recurse`** — a dead field, retained as `#[serde(skip_serializing)]` (no longer read; superseded by the per-branch leaf conditions). Persisted trees that carry it deserialize fine and the value is ignored; new saves don't write it.
- **Leiden knobs** (`LeidenParams`) — detailed in §"Leiden"; the recipe's first Split substitutes `top_level_resolution` (default `0.3`) for `resolution` (default `1.0`) to get 3–8 broad top-level clusters. [cluster-leiden-params]

PLANNED (`bug-clustering-params-spec-drift`): a `representation` param (centroid / summary / lexical, §"Representation"); a `max_depth` recursion cap promoted from a build-time const (`build/mod.rs`) to a `Params` field; the `RecursionMode` enum and `SummarizeMode::Extractive` (above).


## Async execution and progress

The structural pass runs on a background task so the UI thread stays responsive. Producers (the cluster review tab is the only one in v1) submit a build request and consume a stream of progress events from `core::cluster`:

- `Phase { phase }` — current pipeline phase; phase variants cover `LoadingEmbeddings`, `PartitioningLevel(u32)`, and `Finalizing`. Emitted at the start of each phase.
- `Counters { items_processed, clusters_found, outliers }` — running totals; emitted as the partitioner advances.
- `ClusterDiscovered { node: BuiltClusterNode, parent: Option<ClusterId> }` — emitted as each new cluster is added to the in-flight tree. Consumers use this to incrementally reveal clusters in the review surface (per `cluster-review-tab-live-cluster-reveal`) rather than waiting for the full tree.
- `Done { tree: BuiltClusterTree }` — terminal: the full tree is ready.
- `Cancelled` — terminal: the producer signalled cancel and the pass aborted cleanly.
- `Failed { error }` — terminal: the pass errored out (partition refused, embeddings missing, etc.).

The stream is owned by `core::cluster` and consumed through a channel-shaped interface. Cancellation is cooperative: the producer signals via a shared atomic, the pass checks at level boundaries and a periodic per-node interval, drops in-flight results, and emits `Cancelled`. [cluster-build-async-pass, cluster-build-progress-stream]

The LLM summarization pass (Summarize op) is async via the task queue (`cluster-op-summarize-sweep`) and not part of this stream — structural and naming are separate operations with separate progress surfaces.


## Note embeddings input

Every Split operates on note-level embeddings, regardless of where in the tree it runs. A Split against the virtual root sees every note in the build scope; a Split against a real cluster sees that cluster's leaves' embeddings.

The note embedding is the mean of the note's chunk embeddings, weighted by chunk byte length. Computed inline by the indexer's per-file upsert and persisted on the `notes` row (`note_embedding BLOB`) in the same transaction as the chunks. Refreshed on every upsert so the pool tracks the chunk set; notes with no chunks leave the column NULL and are excluded from clustering. Cheap — a vector mean over typically <20 chunks — and avoids spending a separate embedder pass on each note. [cluster-note-embeddings]

Mean-pool rather than embedding the full note directly because `bge-small`'s 512-token (~2000 char) context silently truncates longer notes; mean-pool over chunks (each ~1200 chars) sidesteps the limit with no practical max note size. With a long-context embedder (`embedder-model-selectable`: `bge-m3` 8k, `embedding-gemma-300m` 2k) direct full-note embed becomes viable for most notes, with mean-pool as the fallback for over-context outliers; the direct-embed path is deferred. [cluster-note-embeddings-direct-long-context]

Empty notes (no chunks) are excluded from clustering — they land in the "inbox" (unplaced) bucket, same as outliers.


## Algorithm choices

`algorithm` selects which partitioner runs inside Split (and Roll-up). Four variants: Leiden (default), HDBSCAN, Hybrid, GMM-stubbed.

### Leiden (default)

Modularity-optimization community detection over a kNN cosine-similarity graph. Every node lands in some community; small communities (below `min_cluster_size`) post-flag as outliers. Default because loose `bge-small` embeddings often give HDBSCAN 0–1 cohesive clusters plus everything-as-outliers, while Leiden produces 5–20 communities; γ is a direct granularity knob (coarse `top_level_resolution` `0.3` / finer `resolution` `1.0`). [cluster-leiden]

Pipeline: [cluster-leiden-knn-graph]

1. L2-normalize the input embeddings so cosine similarity reduces to a dot product.
2. For each point, find its top-`k_nearest` neighbors by cosine similarity (brute-force O(n²) — same scale ceiling as HDBSCAN's path).
3. Drop neighbor edges whose weight (cosine similarity) is below `edge_weight_floor`.
4. Construct an undirected graph (edges symmetrize on insertion; mutual-kNN is too aggressive at vault scale).
5. Build a CSR network from the deduped edges, run `RBConfigurationPartition` at the configured `resolution` (the recipe substitutes `top_level_resolution` on the first call), and read each node's community id from the resulting membership.
6. Communities smaller than `min_cluster_size` flag as outliers (post-filter, not algorithmic).

Tunables: [cluster-leiden-params]

- `k_nearest` — number of nearest neighbors per node. Default `15`. Smaller = sparser graph = more, smaller communities; larger = denser graph = fewer, bigger communities. Clamped to `n-1` so it doesn't ask for more neighbors than exist.
- `edge_weight_floor` — minimum cosine similarity for a kNN edge to survive. Default `0.0` (keep every kNN edge). Raise to strip weak neighbor links and tighten community boundaries.
- `resolution` (γ) — Reichardt-Bornholdt resolution parameter on sub-splits. Default `1.0`. γ > 1 biases toward finer / more communities; γ < 1 toward coarser / fewer.
- `top_level_resolution` — γ override for the *first* Split call against the virtual root (when Split is invoked from the build recipe). Default `0.3` — explicitly coarser than `resolution` to produce 3–8 broad top-level clusters that the recursive sub-splits drill into. Ignored when Split is invoked on a non-virtual target.
- `iterations` — cap on Leiden refinement iterations (`LeidenConfig.max_iterations`). Default `100`. Algorithm converges fast; the cap is a safety rail.
- `min_cluster_size` — minimum community size; smaller communities flag as outliers. Default `2`.

**Crate choice: `single-clustering` 0.6.1.** BSD-3-Clause; pure Rust; builds on aarch64-linux (verified locally). Provides `RBConfigurationPartition` (the resolution-parameterized partition used here). Its optional HNSW-based kNN primitives are deliberately unused — our hand-rolled cosine kNN has clearer weight semantics than the crate's `exp(-d²/σ²)` "Gaussian" variants. Marked "under heavy development" in its README, so the workspace dep is **exact-version pinned to `=0.6.1`** — any bump is a deliberate re-test. [cluster-leiden-crate-single-clustering]

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

**Crate choice: `petal-clustering`.** Rust-native HDBSCAN, MIT-licensed, no C/C++ FFI, no extra system deps. Exposes a `fit -> Vec<i32>` plus cluster-stability metadata — the surface `core::cluster::partition` needs. The crate is Euclidean by default; we pre-normalize embeddings once so distance reduces to cosine. [cluster-hdbscan-crate-petal]

### Hybrid mode

HDBSCAN runs first; outliers get reassigned to the nearest cohesive cluster's centroid if cosine ≥ 0.6. Selectable via `algorithm = Hybrid` — for when HDBSCAN's outlier set is too aggressive but the user doesn't want pure Leiden's "every point gets a home" either. Soft members tag distinctly in the cluster row so the editor renders them differently. Not wired under Leiden (Leiden places every point, so there's no outlier set to recover). [cluster-hybrid-outlier-recovery]

### GMM (stub)

`algorithm = Gmm` is reserved; the runtime falls back to HDBSCAN with a warning until a GMM crate goes through dep review. [cluster-algorithm-selectable]

### Algorithm selection

`cluster.algorithm` lives in `vault/.hiker/config.toml` as a per-vault default (different vaults cluster differently); the review tab's Advanced disclosure overrides it per build. [cluster-algorithm-selectable]


## Representation

PLANNED (`bug-clustering-params-spec-drift`): `representation` is not yet a `Params` field; today every partition uses the Centroid representation. The model below is the specced target.

`representation` decides what each *unit* is reduced to before the partition algorithm sees it — orthogonal to the algorithm (which decides *how*) and to direction: Split's units are notes, Roll-up's units are clusters. One routine services both. [cluster-representation]

| Representation | Vector | Character |
| --- | --- | --- |
| **Centroid** (default) | L2-normalized mean of the unit's member embeddings | Semantic, geometric, always available, no LLM. The honest default |
| **Summary embedding** | embedding of the unit's name/summary text | Semantic by *what it's about*. Needs naming first → natural for Roll-up (clusters are named), a costly opt-in for Split (notes aren't) |
| **Lexical** | TF-IDF / sparse term vector over the unit's text (document-frequency from the lexical/FTS index) | Literal — groups by shared terms. Reproducible, most explainable, model-free |

Summary-embedding is available only when the units are named. Default is Centroid for both directions; the user overrides per action, with the tree's last choice inherited (mixing within a tree is allowed but costs legibility). Lexical + the planned `SummarizeMode::Extractive` gives a complete model-free path. Signal *fusion* (a weighted blend of lexical + embedding + link-graph + tags) slots into this same param when it lands. [cluster-representation, cluster-representation-fusion]


## Build scope

`core::cluster::build_tree(scope, method, params)` takes a `BuildScope` (which notes), a `BuildMethod` (how the tree is built), and method-specific params. The cluster editor's "Suggest reorganization" picks all three; saved Evergreen trees record them so triage knows what the tree classifies and how to rebuild. [cluster-build-scope, cluster-build-method]

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

The clustering algorithm is unaware of scope — `BuildScope` resolves to a `Vec<NoteId>` at the entry, the pass operates on whatever embeddings it's handed. The small-vault skip (`<50 notes` → "vault is too small") applies per-scope.

Saved triage trees persist their scope; the triage classifier (`cluster-place-beam-descent`) only evaluates new/modified notes whose path falls within the saved scope. Triage's existing safety rule (notes outside the configured triage scope are never moved out of their folder) intersects with the build scope — a saved tree built over `research/` can still only auto-move notes that live in the triage scope (default `inbox/`).


## Build method

`BuildMethod` selects how the tree is constructed from the resolved note set. Two methods, each with its own parameter shape:

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

`Cluster`'s `Params` is the field set enumerated in §"Build recipe" (algorithm, the recursion bools + leaf stop conditions, HDBSCAN/Leiden tunables, `summary_confidence_threshold`, `include_outliers`, the dead `min_clusters_to_recurse`); naming is applied by the Name step's `SummarizeMode`, not by `Params`. [cluster-build-method, cluster-build-params]

### `Cluster` method

Default. Runs the build recipe (`cluster-build-recipe`). `include_outliers` defaults to `true`; `false` runs the hybrid-mode outlier-recovery pass with the threshold lowered so every outlier lands in its closest cluster (every note gets a home). [cluster-build-cluster-method]

### `FromFolders` method

Skip clustering entirely. Walk the filesystem under the build scope; produce a `ClusterTree` whose structure mirrors the folder hierarchy:

- One `ClusterNode` per folder (kind `Cluster`).
- One leaf node per note, parented to the folder it lives in.
- Root = the scope's root (`vault/` for `Vault`, `<rel>/` for `Folder(rel)`, a synthetic single-cluster root for `Notes(ids)`).
- Centroids: mean of member embeddings (same as `Cluster` method's leaf-level centroids). Computed lazily on save-as-triage so the placement classifier (`cluster-place-beam-descent`) works against the folder-derived tree the same way it works against a `Cluster`-method tree.
- Outliers at build time: not generated. Every note already in the scope has a folder; the build's leaf-set is exactly the scope's notes.
- Outliers at triage time (Evergreen use): a new note whose final cosine distance to its nearest folder exceeds `outlier_threshold` (default `0.5`) routes to the outlier bucket instead of force-fit; the bucket node is created lazily and can carry its own policy (`cluster-editor-outlier-policy`, typically `Move` to `inbox/unsorted/`). `include_outliers = false` disables the check — every note routes to its nearest folder regardless of similarity. [cluster-build-from-folders-outliers]
- Confidence: 1.0 on every node (the folder structure is the source of truth, not a probabilistic guess).
- Summaries: per `SummarizeMode` — `llm` runs the same prompt as the clustering pipeline (per `cluster-summarize-llm`) over each folder's member titles + summaries; `none` leaves summary empty. Name defaults to the folder's basename.

[cluster-build-from-folders]

For already-organized vaults: build the Evergreen tree on the actual folders rather than the partitioner's guess; triage then finds the most similar existing folder.

The output `ClusterTree` is identical in shape to the `Cluster` method's — same downstream consumers, same reshape ops. Splitting a folder-derived node re-runs HDBSCAN against just that node's members, so a folder-derived tree can grow cluster-derived subtrees. The `method` frontmatter records the original build method for re-build. [cluster-build-from-folders-uniform-output]

### FromFolders live-update

A saved FromFolders Evergreen tree tracks the filesystem. When a note moves between folders (file-tree drag-drop, accepted `move_note` staging row, `hiker mv`, or a watcher-caught external rename), `core::trees` updates the tree's frontmatter in place — the leaf's `parent` flips to the new folder's node id. Cluster nodes for new folders are added on the fly; emptied folders' nodes are dropped unless they carry an explicit policy (kept as empty placeholders so the rule survives). The trigger is the same watcher rename event the indexer consumes; the update is incremental — no re-build, no LLM call — and affected centroids are recomputed (cheap). [cluster-build-from-folders-live-update]

**Staleness counter.** Each cluster node carries a `summary_membership_churn` integer (0 at summary-generation). Every leaf insert/remove in a cluster's subtree increments it on that cluster and all ancestors — the "summary may be out of date" signal, surfaced as a `↻ N` badge / soft-tinted node color. Resets to 0 on Regenerate. [cluster-build-from-folders-summary-staleness]

The counter applies primarily to FromFolders trees (filesystem moves drive churn); `Cluster`-method trees use the same field for reshape ops (move/merge/split). It is the signal `cluster-op-summarize-sweep`'s `StaleOrUnfilled` scope consumes. [cluster-summary-staleness-counter]


### Re-building Evergreen trees

A saved Evergreen tree records scope + method in frontmatter. The "Re-build" action (`cluster-editor-mode-menu`) re-runs `build_tree` with the saved params; the user reviews the diff (deferred, `cluster-editor-tree-diff-view`) or accepts the fresh tree, retiring the previous one to trash. [cluster-build-rebuild]


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

Cost: `O(K · branching · depth)` cosines, ≈ a few hundred dot products on a 10k-vault tree. Microseconds; no LLM. The classifier is the per-note path; the full build pass is the rare batch path. Beam (`K=2`) over greedy (`K=1`) recovers the case where the true target sits in a sibling subtree the top cluster barely missed. Collapsed-tree scoring (compare to every node flat) is deferred.

`hiker mv` and drag-and-drop-move are *not* this. Manual user moves don't re-classify against the tree; they're authoritative. The classifier fires only on new-note-on-save and the modified-rerun pathways.


## Per-note placement (online, cheap)

The build recipe is the **batch / seed** path (offline, on `hiker reconcile`, expensive, rare). The complementary cheap path is **per-note placement** — dropping a single new note into the existing tree with no LLM calls or re-clustering, fully specced in `design.md` (greedy centroid descent over the curated tree, on note-create / significant-edit-save, writes a placement to frontmatter). Most of the time only `Place` runs; `Build` runs when the user wants to re-examine the structure.


## What consumes the tree

`cluster-editor.md` consumes the `ClusterTree` (shape below). Two flows downstream, both unaware to the build engine:

- **One-shot Apply** — review a built tree, set per-cluster policies, apply; each policied leaf emits a pending op (folder move and/or frontmatter tag) the user batch-reviews.
- **Saved-tree triage** — save a tree as the active classifier; new notes route against it via greedy centroid descent (`cluster-place-beam-descent`), each match resolved through the matched node's policy.

## Summarization

The LLM naming path (`SummarizeMode::Llm`, the current default) takes cluster member titles + per-note summaries (or per-cluster summaries at higher levels) and produces a short summary (1–3 sentences) + a proposed name (3–6 words) via the prompt below. The PLANNED deterministic path (`SummarizeMode::Extractive`, `cluster-name-deterministic`, `bug-clustering-params-spec-drift`) would derive the name from top TF-IDF / KeyBERT-style terms with the most-central note's title as fallback. [cluster-summarize-llm, cluster-name-from-summary, cluster-name-deterministic]

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
- **Never automatic.** No background build, no on-save full-pipeline trigger. Triage *can* run on note save but that's the cheap classifier (`cluster-place-beam-descent`), not a re-cluster; only saving-as-triage and an explicit Re-build regenerate the tree.
- **Watcher does not drive the build** — its events drive the index (chunk/embed), not the tree.


## Output: the `ClusterTree`

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

Cluster IDs are *not* stable across runs; durable cluster identity is the tree document's outline position-and-name (`cluster-editor.md`), not the build pass's ephemeral per-run ids.


## Module discipline

All clustering logic lives in `core::cluster`. Outside the module: `partition()` returns plain assignments and the build returns a neutral `BuiltClusterTree` of plain Rust types — no HDBSCAN-crate types leak past the boundary, and `core::cluster` has no dependency on tree *storage* types either. Turning a `BuiltClusterTree` into storage rows is the storage-side adapter `core::trees::build_adapter`, which depends downward on `core::cluster`, so the dependency is one-way (`trees → cluster`). Same swappability posture as `core::store` / `core::embed`: the algorithm choice is a default, and a future swap (GMM, agglomerative) should be a one-file rewrite. [cluster-module-discipline]

Summarization is its own module (`core::summarize`) since it has independent failure modes (LLM unavailable, slow, low-quality) and an independent swap surface (local model vs cloud vs extractive fallback).


## Out of scope

- Online incremental clustering of the *full* tree. The cheap online path is greedy descent against an already-built saved tree (`cluster-place-beam-descent`, used by triage) — that's not a rebuild, it's a classifier.
- Multi-axis trees (one tree per type — semantic, temporal, entity). Single semantic tree only at first; the multi-axis idea (`design.md`) is a later concern.
- Durable cluster identity carried by the *build pass*. Each build run produces a fresh tree with ephemeral per-run ids; the durable identity lives in the tree document (its outline position-and-name), owned by `cluster-editor.md`, not in the build output.
- Cross-vault clustering. One tree per vault.
- Trail discovery from clusters. Trails are user-authored only by design (see `design.md`); the clustering pipeline never proposes them.
