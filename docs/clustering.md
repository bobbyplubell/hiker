# Clustering

How Hiker builds a hierarchical tree of topics from an unorganized vault. This doc covers the *build* side only — turning embeddings into a tree of nodes with names. The tree is consumed by `suggestions.md`, which uses it as a recommendation engine (one-shot reorganization proposals + saved-tree inbox triage); it is **not** durable infrastructure that owns the user's organization. Per-note placement provenance, manual-vs-auto stickiness, durable cluster IDs, and a parallel curated-tree-vs-filesystem mental model are explicitly out of scope — see `design.md`'s "Auto-organization suggestions" section.

Not built in v1. Lands alongside the suggestions surface (post-v1, after the related-notes panel proves the index pipeline). Speccing now because (a) the build algorithm shapes what suggestions look like, (b) it determines the cost model that decides whether a one-shot run takes seconds or minutes, and (c) it's the thing the synthetic-corpus eval (`qa.md`) is supposed to validate.


## Approach: recursive cluster + LLM-summarize (RAPTOR-shaped)

Bottom-up tree construction. Repeatedly cluster the current level's embeddings, ask an LLM to summarize each cluster, embed the summary, and recurse. The summary serves three roles at once: it names the node (proposed name in the reconcile UI), it's the embedding seed for the parent level, and it becomes the user-visible "what's in this branch" description. [cluster-build-recursive]

```
level 0: note embeddings              ─┐
              cluster ─→ summarize     │ recurse
level 1: cluster summaries (embedded)  │
              cluster ─→ summarize     │
level 2: meta-cluster summaries        │
              ...                     ─┘
root: one node summarizing the vault
```

Termination: stop recursing when the level has fewer than `MIN_CLUSTERS_TO_RECURSE` (default 4) nodes, or when a cluster's summary fails to add information beyond its members (signal: cosine similarity between the summary embedding and the mean of its member embeddings exceeds a saturation threshold — the summary is just restating the centroid).

The named technique in the literature is RAPTOR (Sarthi et al., 2024). Borrowed directly because the algorithm is straightforward and matches the data shape. Where Hiker diverges from the paper: clusters notes (not chunks) at level 0, and uses HDBSCAN rather than GMM (rationale below).


## Level 0 input: note embeddings, not chunk embeddings

The curated tree is a tree of notes — placement is per-note, the leaves are notes. So clustering operates on **note-level** embeddings at level 0, not chunk-level.

The note embedding is the mean of the note's chunk embeddings, weighted by chunk byte length. Computed lazily on first cluster pass and cached on the `notes` row (new column: `note_embedding BLOB`). Recomputed when any of the note's chunks change. This is cheap — a vector mean over typically <20 chunks — and avoids spending a separate embedder pass on each note. [cluster-note-embeddings]

Why mean-pool rather than embed the full note text directly: `bge-small`'s context window is 512 tokens (~2000 characters). A note longer than that gets silently truncated by the embedder, and personal-vault notes commonly exceed this — anything longer than a couple of paragraphs. Mean-pool over chunks sidesteps the limit entirely (each chunk is ~1200 chars by the chunker's cap, so each chunk fits), and the resulting representation already reflects the chunker's heading-bounded structure. There is no practical max note size for clustering with mean-pool. If we ever swap to a long-context embedder (`bge-m3` does 8k tokens, `nomic-embed-text` does 8k), direct embed becomes viable for most notes; mean-pool stays as a fallback for outliers.

Empty notes (no chunks) get no embedding and are excluded from clustering — they end up in the "inbox" (unplaced) bucket, same as outliers.


## Clustering algorithm

Two algorithms are supported, selectable per build via `ClusterParams.algorithm`. HDBSCAN is the default; Leiden is opt-in via the clustering review tab's Advanced disclosure.

### HDBSCAN

HDBSCAN over GMM (RAPTOR's choice) for three reasons specific to personal-vault scale: [cluster-hdbscan]

1. **Outlier handling.** Personal vaults always have a long tail of notes that don't belong to any cohesive topic — fleeting thoughts, one-off snippets, miscellaneous reference. HDBSCAN labels these as outliers natively; they go to the inbox rather than being force-fit into a cluster they don't belong in. GMM's soft-probabilistic assignment can't represent "doesn't belong anywhere."
2. **No K.** Personal vault sizes vary by 100×. Tuning K per vault is annoying; HDBSCAN's `min_cluster_size` is more stable across scales (default 5).
3. **Determinism given seed.** Stable across reconcile runs — important because `reconcile-history.yaml` keys on rejected proposals, and unstable cluster identity makes that bookkeeping useless.

Tunables:
- `min_cluster_size` — smallest cluster the algorithm will form. Default 5; user-overridable per vault in `vault/.hiker/config.toml`.
- `min_samples` — density threshold. Default = `min_cluster_size`. Higher → more outliers.
- Distance metric — cosine, on the note embeddings.

Watch for: small vaults (<50 notes) where HDBSCAN may produce all-outliers and an empty tree. Fallback: if the level-0 pass produces fewer than 2 clusters, skip auto-org and surface a "vault is too small" message rather than a misleading tree of one node.

**Crate choice: `petal-clustering`.** Rust-native HDBSCAN implementation, MIT-licensed, no C/C++ FFI, used in production by the Petabi suite. Builds clean on the project's target stack (Tauri + Rust workspace), no extra system deps. The crate exposes `Hdbscan::new(min_cluster_size, min_samples).fit(&data) -> Vec<i32>` plus cluster-stability metadata — exactly the surface `core::cluster::partition` needs. Vector distance is cosine via pre-normalized embeddings (the crate operates on Euclidean by default; we normalize once and pass-through). [cluster-hdbscan-crate-petal]

Alternatives considered:
- **`linfa-clustering`** — covers KMeans / DBSCAN / GMM but not HDBSCAN as of writing. Would force a custom HDBSCAN port; not worth the cost.
- **PyO3 + sklearn** — adds a Python runtime dependency; rejected for the Tauri-app distribution model.
- **Hand-rolled** — HDBSCAN's mutual-reachability-distance + single-linkage + condensed-tree-extraction is real work; not justified when `petal-clustering` exists.

Possible future swap: GMM behind the same trait if outlier rate proves too aggressive. The clustering call lives in a single `core::cluster::partition(embeddings) -> Vec<ClusterAssignment>` boundary so swaps are local. Same module discipline as `core::store` and `core::embed`.

**User-selectable algorithm.** A `cluster.algorithm` setting in `vault/.hiker/config.toml` picks between `hdbscan` (default), `gmm`, and `hybrid`. Per-vault rather than per-user — different vaults have different shapes (a structured reference vault vs. a fleeting-thoughts journal cluster very differently). [cluster-algorithm-selectable]

**Hybrid mode.** `hdbscan` runs first to extract cohesive clusters and the outlier set. `gmm` then runs only on the outliers, with K = sqrt(outlier_count). Outliers that GMM places with probability > 0.6 join the corresponding HDBSCAN cluster as soft members; the rest stay in the inbox. This recovers the "doesn't fit confidently anywhere but isn't truly miscellaneous" middle ground without forcing every note into a cluster the way pure GMM does. Soft members are tagged in the cluster row so the reconcile UI can show them differently from primary members. [cluster-hybrid-outlier-recovery]

A second hybrid form — HDBSCAN structure with GMM soft-membership across *siblings* (every note gets P(cluster) for its cluster's siblings) — is interesting for surfacing "this note also touches topic Y" and connects naturally to the multi-axis idea in `design.md:215`. Deferred until the simpler hybrid is in production.


### Leiden community detection

Selected via `ClusterParams.algorithm = Leiden`. Lands as an opt-in alternative for vaults where HDBSCAN under-clusters — personal-vault sized corpora with `min_cluster_size = 5` and loose `bge-small` embeddings often produce 0–1 cohesive cluster + everything-as-outliers, which makes the suggestions surface useless. Leiden runs modularity optimization on a kNN cosine-similarity graph; every node lands in some community, and the *community count* falls out of the data rather than out of a density threshold. [cluster-leiden]

How it differs from HDBSCAN, when to pick which:

- **HDBSCAN labels low-density points as outliers.** Good when the vault genuinely has a long miscellaneous tail you want sent to the outlier bucket (the default posture). Bad when *everything* falls below the density floor — the user sees an empty tree.
- **Leiden places every point in a community.** No outliers fall out of the algorithm itself; small communities (below `min_cluster_size`) get post-flagged as outliers so the downstream tree shape stays consistent. Good when the corpus is small or topically tight. Bad when you genuinely want a miscellaneous bucket (you'll get a few singletons but most notes will be placed).
- **HDBSCAN respects density gaps.** Two topically-close clusters separated by a thin density bridge stay separate.
- **Leiden respects modularity.** Two topically-close clusters connected by enough kNN edges may merge; conversely, a small topically-tight subgroup can split off even if the surrounding density is uniform.

Pipeline: [cluster-leiden-knn-graph]

1. L2-normalize the input embeddings so cosine similarity reduces to a dot product.
2. For each point, find its top-`k_nearest` neighbors by cosine similarity (brute-force O(n²) — same scale ceiling as HDBSCAN's path).
3. Drop neighbor edges whose weight (cosine similarity) is below `edge_weight_floor`.
4. Construct an undirected graph (edges symmetrize on insertion; if A is in B's top-k but not vice versa we still keep the edge — mutual-kNN is too aggressive at vault scale).
5. Construct a `single_clustering::network::CSRNetwork` from the deduped edges (`from`, `to`, `cosine_weight`) plus a unit `node_weights` vec.
6. Build a `RBConfigurationPartition<f64, VectorGrouping>` with the configured `resolution` (γ), then run `LeidenOptimizer::optimize_single_partition` over it.
7. Each node's community id comes from `partition.membership(node)`; group nodes by id, densify the surviving ids, and emit `ClusterAssignment` per input point.
8. Communities smaller than `min_cluster_size` are flagged as outliers (post-filter, not algorithmic).

Tunables: [cluster-leiden-params]

- `k_nearest` — number of nearest neighbors per node. Default `15`. Smaller = sparser graph = more, smaller communities; larger = denser graph = fewer, bigger communities. Clamped to `n-1` so it doesn't ask for more neighbors than exist; at recursion levels where the level cardinality is small, it's clamped per-level so a default `k=15` doesn't ask for 15 neighbors among 4 cluster summaries.
- `edge_weight_floor` — minimum cosine similarity for a kNN edge to survive. Default `0.0` (keep every kNN edge). Raise to strip weak neighbor links and tighten community boundaries.
- `resolution` (γ) — Reichardt-Bornholdt resolution parameter on the RB configuration partition. Default `1.0` (modularity-equivalent). γ > 1 biases toward finer / more communities; γ < 1 toward coarser / fewer. This is the standard Leiden quality knob and the primary way to tune cluster granularity at fixed `k_nearest`.
- `iterations` — cap on Leiden refinement iterations (`LeidenConfig.max_iterations`). Default `100`. Algorithm converges fast; the cap is a safety rail.
- `min_cluster_size` — minimum community size; smaller communities flag as outliers. Default `2`.

**Crate choice: `single-clustering` 0.6.1.** BSD-3-Clause licensed; pure Rust; builds on aarch64-linux (verified locally). Replaced `fa-leiden-cd` because the latter exposes only standard modularity with no resolution knob, leaving cluster granularity tunable only indirectly via `k_nearest` and `edge_weight_floor`. `single-clustering` provides both `ModularityPartition` and `RBConfigurationPartition` (used here for the resolution parameter), plus optional HNSW-based kNN primitives we deliberately do not use — our hand-rolled cosine kNN is clear about its weight semantics where the crate's "Gaussian" variants apply `exp(-d²/σ²)` weighting. The crate is marked "under heavy development" in its README, so the workspace dep is **exact-version pinned to `=0.6.1`** — any bump is a deliberate re-test. Public surface used: `CSRNetwork::from_edges`, `LeidenConfig`, `LeidenOptimizer::new` + `optimize_single_partition`, `RBConfigurationPartition::with_resolution`, `VectorGrouping`, `VertexPartition::membership`. Repository: <https://github.com/SingleRust/single-clustering>. [cluster-leiden-crate-single-clustering]

**Hybrid mode does not use Leiden.** The existing hybrid path (`cluster-hybrid-outlier-recovery`) runs HDBSCAN then reassigns outliers to nearest centroid. Leiden places every point in a community by construction, so there's no outlier set to recover. When `algorithm == Leiden` and the user also sets `Hybrid`-style behavior, the hybrid recovery pass is suppressed; small-community-flagged outliers still get force-routed when `include_outliers = false`, which is the same posture HDBSCAN uses. [cluster-hybrid-outlier-recovery]


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

**Source-type filter** [cluster-build-scope-source-types]. Each variant carries an optional `source_types: Vec<String>` of canonical lower-case extensions. The build-pass + triage classifier only see notes whose extension is in the list; `"md"` covers both `.md` and `.markdown` (the indexer treats them as the same source type per `INDEXABLE_EXTENSIONS`). An empty vec is the legacy "every indexable extension" posture and is what every pre-feature persisted tree's `scope_json` deserializes to. Surfaced in the clustering review tab's config section as checkboxes (Markdown / Plain text), default both on. Enforced in two places:

1. `notes_for_scope` (the build-pass resolver) filters resolved paths via `BuildScope::matches_path` before reading embeddings — a tree built with `source_types = ["md"]` simply never sees `.txt` notes.
2. `triage_all_saved_trees` (the on-save classifier) deserializes each saved tree's `scope_json` and skips trees whose `source_types` filter rejects the saved note's path. A `.txt` note save against a Markdown-only triage tree no-ops; the same note save against a mixed-type tree fires normally.

The clustering algorithm itself is unaware of scope — `BuildScope` is resolved into a `Vec<NoteId>` by `core::cluster::build_tree`'s caller-facing entry, the recursive pass operates on whatever embeddings are handed in. The `min_cluster_size` fallback for small vaults (`<50 notes` → skip with "vault is too small") applies per-scope: a `Folder` scope with three notes gets the same skip message.

Saved triage trees persist their scope; the triage classifier (`cluster-place-beam-descent`) only evaluates new/modified notes whose path falls within the saved scope. Triage's existing safety rule (notes outside the configured triage scope are never moved out of their folder) intersects with the build scope — a saved tree built over `research/` can still only auto-move notes that live in the triage scope (default `inbox/`).


## Build method

`BuildMethod` selects how the tree is constructed from the resolved note set. Two methods, each with its own parameter shape:

```rust
enum BuildMethod {
    Cluster   { params: ClusterParams },      // RAPTOR-shaped (default; everything above in this doc)
    FromFolders { params: FolderDeriveParams }, // mirror the filesystem hierarchy
}

struct ClusterParams {
    algorithm: ClusterAlgorithm,    // hdbscan / gmm / hybrid (cluster-algorithm-selectable)
    min_cluster_size: u32,          // HDBSCAN tunable
    min_samples: Option<u32>,       // HDBSCAN tunable; None → defaults to min_cluster_size
    min_clusters_to_recurse: u32,   // termination threshold
    summary_confidence_threshold: f32, // marks clusters "uncertain" below this
    include_outliers: bool,         // when false, force-routes outliers into nearest cluster
    summarize: SummarizeMode,       // llm / template / none
}

struct FolderDeriveParams {
    summarize: SummarizeMode,
    include_outliers: bool,         // default true; gates outlier detection at triage time
    outlier_threshold: f32,         // cosine-distance threshold; matches farther than this become outliers
}
```

[cluster-build-method, cluster-build-params]

### `Cluster` method

The RAPTOR-shaped pipeline described in the rest of this doc. Default. `ClusterParams.include_outliers` defaults to `true`; setting it to `false` runs the existing hybrid mode's outlier-recovery pass with the threshold lowered to absorb every outlier into its closest cluster (no minimum confidence — every note gets a home). [cluster-build-cluster-method]

### `FromFolders` method

Skip clustering entirely. Walk the filesystem under the build scope; produce a `ClusterTree` whose structure mirrors the folder hierarchy:

- One `ClusterNode` per folder (kind `Cluster`).
- One leaf node per note, parented to the folder it lives in.
- Root = the scope's root (`vault/` for `Vault`, `<rel>/` for `Folder(rel)`, a synthetic single-cluster root for `Notes(ids)`).
- Centroids: mean of member embeddings (same as `Cluster` method's level-0 centroids). Computed lazily on save-as-triage so the placement classifier (`cluster-place-beam-descent`) works against the folder-derived tree the same way it works against a RAPTOR tree.
- Outliers at build time: not generated. Every note already in the scope has a folder; the build's leaf-set is exactly the scope's notes.
- Outliers at triage time (Evergreen use): the saved tree's `outlier_threshold` parameter gates whether new notes that don't fit any existing folder go to the outlier bucket. When the placement classifier descends to the nearest folder and finds the final cosine distance exceeds `outlier_threshold` (default `0.5`, configurable per `FolderDeriveParams`), the note is routed to the outlier bucket instead of force-fit into the nearest folder. The outlier bucket node is created lazily on the first such match. The bucket can carry its own policy (per `cluster-editor-outlier-policy`) — typically `Move` to an `inbox/unsorted/` folder. Setting `include_outliers = false` disables this check; every new note is routed to its nearest existing folder regardless of similarity. [cluster-build-from-folders-outliers]
- Confidence: 1.0 on every node (the folder structure is the source of truth, not a probabilistic guess).
- Summaries: per `SummarizeMode` — `llm` runs the same prompt as the clustering pipeline (per `cluster-summarize-llm`) over each folder's member titles + summaries; `none` leaves summary empty. Name defaults to the folder's basename.

[cluster-build-from-folders]

Why it exists: users with already-organized vaults want a saved Evergreen tree built on their actual structure, not on what RAPTOR thinks the structure should be. Triage against a folder-derived tree is just "find the most similar existing folder and put the new note there" — the obvious thing, made explicit. Avoids the "I already organized my vault; why does the AI want to re-organize it" friction.

The output `ClusterTree` shape is identical to the `Cluster` method's — same `ClusterNode` type, same downstream consumers. The cluster editor doesn't distinguish how a tree was built once it exists; reshape operations (merge / split / move-note) work the same way. Splitting a folder-derived node re-runs HDBSCAN against just that node's members — a folder-derived tree can grow cluster-derived subtrees through user editing without ceremony. The `meta.json` (now `cluster_trees.method` column) records the original build method for reference and re-build. [cluster-build-from-folders-uniform-output]

### FromFolders live-update

A saved FromFolders Evergreen tree tracks the filesystem. When a note moves between folders (user drag-drop in the file tree, accepted `move_note` staging row from any surface, manual `hiker mv`, external rename caught by the watcher), `core::trees` updates the affected `cluster_nodes` rows in place — the leaf's `parent_id` flips to the new folder's node id. Cluster nodes for newly-created folders are added on the fly; emptied folders' nodes are dropped (unless they carry an explicit policy, in which case they're kept as empty placeholders so the user's rule survives a transient empty state).

The trigger is the same `hiker:file-changed` rename event the indexer already consumes; `core::trees` subscribes alongside it. The update is incremental — no re-build, no re-summarization, no LLM call. Centroids are recomputed for affected clusters (cheap: a vector mean over members). [cluster-build-from-folders-live-update]

**Staleness counter.** Each cluster node carries a `summary_membership_churn` integer (initialized to 0 at summary-generation time). Every leaf insert or remove within a cluster's subtree increments the counter on that cluster and all its ancestors. The counter is the user-visible "your summary may be out of date" signal — surfaced as a `↻ N` badge on the node's row in the cluster editor and as a soft-tinted node color in the graph view. The counter resets to 0 when the user runs Regenerate on that node. [cluster-build-from-folders-summary-staleness]

Why a counter rather than a stale-bool: a single move barely shifts a 30-note cluster's meaning, but ten moves probably do. The integer lets the user calibrate when to regenerate (`↻ 1` is noise; `↻ 12` is a real drift signal). A bool would force the user to either over-regenerate or ignore real drift. Embedding-distance drift was considered as an alternative metric but rejected as overkill for v1 — the counter ships first, distance-based staleness can replace it later if churn proves too coarse.

The counter applies to FromFolders trees primarily, where filesystem moves drive churn. RAPTOR `Cluster` trees can use the same field for reshape operations (move-note-between-clusters / merge / split via the cluster editor) — same column, same UI treatment. [cluster-summary-staleness-counter]


### Re-building Evergreen trees

A saved Evergreen tree records both its scope and method on the `cluster_trees` row. The "Re-build" action in the cluster editor (per `cluster-editor-mode-menu`) runs `build_tree(scope, method, params)` again with the saved parameters, producing a fresh tree. The user reviews the diff (deferred per `cluster-editor-tree-diff-view`) or accepts the new tree as the active Evergreen, retiring the previous one to the vault's trash. [cluster-build-rebuild]


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

Tunables (per-saved-tree, on the `cluster_trees` row):

- `beam_width` (`K`) — default `2`. `K=1` is the cheap fallback ("greedy"); `K=3+` is robust but rarely needed at vault scale.
- `min_confidence` — default `0.55` (cosine). Matches below this threshold route to the outlier bucket if `include_outliers = true`; otherwise still apply at low confidence (per-policy `require_review = true` handles the gating).
- `min_margin` — default `0.05`. A match whose top-1 / top-2 margin is below this is "ambiguous" — same as below-confidence; routes to outlier or fires with `require_review` semantics depending on outlier policy.

Cost: `O(K · branching · depth)` cosines, ≈ a few hundred dot products on a 10k-vault tree. Microseconds; no LLM. The classifier is the per-note path; the full build pass is the rare batch path.

Why beam over greedy (`K=1`): the failure mode of greedy is "the top cluster at level 1 was *almost* right but the true target sits in a sibling subtree the query barely missed." `K=2` recovers the silent miss for trivial cost. RAPTOR's tree-traversal mode uses this exact pattern; the broader hierarchical-retrieval literature backs the choice. Collapsed-tree scoring (compare to every node flat) is more accurate but loses the speedup; deferred.

`hiker mv` and drag-and-drop-move are *not* this. Manual user moves don't re-classify against the tree; they're authoritative. The classifier fires only on new-note-on-save and the modified-rerun pathways.


## Per-note placement (online, cheap)

The recursive build pass in this doc is the **batch / seed** operation — runs only on `hiker reconcile`, produces a fresh tree, expensive enough to be worth doing rarely. The complementary cheap operation is **per-note placement**: drop a single new note into the existing tree without touching anyone else's placement, no LLM calls, no re-clustering.

Per-note placement is fully specced in `design.md:252-257` (greedy centroid descent over the existing curated tree). Mentioned here to make the pairing explicit:

- **Build / reseed (this doc):** offline, on `hiker reconcile`, batch over the whole vault, produces a `ClusterTree` proposal.
- **Place (`design.md:252`):** online, on note-create or note-edit-saves-significant-content, embedding-only descent over the existing tree, writes a placement to the note's frontmatter.

Most of the time only `Place` runs — every new note gets a home cheaply. `Build` runs when the user decides it's worth re-examining the structure (new corpus shape, the tree feels stale, after a large import).


## What consumes the tree

The build pipeline produces a `ClusterTree` (shape below). `suggestions.md` consumes it as a recommendation engine — *not* as durable infrastructure that owns the user's organization. Two flows downstream:

- **One-shot reorganization** — generate a tree, render it as a markdown proposal with checkboxes, user picks what to apply (folder moves and/or frontmatter tags). Tree is ephemeral; nothing persists except the user's accepted actions.
- **Saved-tree triage** — user saves a generated tree as a classifier; new notes get routed against it via greedy centroid descent (`cluster-place-beam-descent`), with confidence-tiered behavior (auto-apply / queue-for-review / leave-in-inbox).

The build engine is unaware of which flow consumes its output — same algorithm, same `ClusterTree`. See `suggestions.md` for everything downstream.


## Why notes (not chunks) at level 0 — and what chunk-level clustering is good for instead

Note-level clustering at level 0 is the right default because the curated tree's leaves are notes — placement is per-note, navigation is per-note, the user's mental model of "where does this thing live" is per-note. A tree of chunks would index a different abstraction than the one users navigate.

That said, chunk-level clustering has real uses as a *parallel* feature, not a replacement:

- **Cross-note thread surfacing** — chunks from different notes that cluster tightly together are evidence of a thread that crosses several notes. Useful as a "you might be writing about X across these places" hint, not as input to an auto-built trail (trails are user-authored only — see `design.md`). [cluster-chunk-thread-hint]
- **Multi-topic flagging** — a note whose chunks scatter across many distinct clusters is a candidate for splitting. Useful as a soft suggestion, not an auto-action. [cluster-chunk-multitopic-flag]
- **Section reorganization** — chunk clusters *within* a single note suggest heading reorganization.

None of these are in scope for the v1-of-clustering pass. Listed here so the chunk-level signal isn't forgotten when the note-level pipeline is in place.


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

At level 0 the per-note summary input comes from the existing `Summary` enrichment (`design.md:293`) — already cached on the note's frontmatter or in the store. At level 1+ the inputs are child cluster summaries; same prompt, no special-casing.

Confidence below a threshold (default 0.5) marks the cluster as "uncertain" — the suggestions flow shows it but flags it for explicit review before applying.

**Routing per `llm.md`:** cluster summarization is a *fan-out* feature (one prompt per cluster, scope determined pre-batch by the cluster set). Calls flow through `core::llm` direct — no agent loop, no ACP. The summarizer's pluggable surface (`core::cluster::Summarizer` trait) is a thin layer *on top of* `core::llm`: the trait owns the prompt template, member-formatting, and JSON parsing; the LLM call itself goes through `core::llm`. Same discipline pattern as embedder and store. `LlmSummarizer` is the only LLM-backed production impl; the build pipeline also supports `SummarizeMode::None` as a fully-supported *structural pass* — no `Summarizer` is invoked, names default to `"Cluster N"` placeholders, and the build runs end-to-end without `[llm] enabled`. The clustering review tab (`cluster-editor.md` § Clustering review tab) drives Run on this path and only fires the LLM-backed naming at Confirm time.

Model choice: provider/model are user-configured in `[llm]` per `llm.md`; a small local model via Ollama (e.g. `qwen2.5:3b`) is enough — cluster naming is easier than freeform writing. The Confirm-and-name path requires `[llm] enabled = true` because it submits `RaptorSummarize` tasks that route through `core::llm`; the structural pass (Run, before Confirm) is LLM-optional and works even when `[llm]` is disabled — names land as placeholders and the cluster editor becomes the source of truth for naming.


## Cost model

Dominant cost: LLM calls for summarization. One call per cluster per level.

Rough numbers for a small local model (~3B, ~50ms/call on CPU):

| Vault size | Leaf clusters | Mid levels | Total calls | Wall time |
| ---------- | ------------- | ---------- | ----------- | --------- |
| 100 notes  | ~10           | 1          | ~13         | <1s       |
| 1k notes   | ~80           | ~12, ~3    | ~95         | ~5s       |
| 10k notes  | ~500          | ~70, ~10, ~2 | ~580      | ~30s      |

Embedding cost (one extra call per cluster summary) is negligible relative to summarization.

A full build pass is **not** an interactive operation. Run on demand via `hiker suggest` (per `suggestions.md`); results are written as a proposal the user reviews on their own time. No sub-second budgets.


## When it runs

- **`hiker suggest`** (one-shot) — explicit user command, runs the full pipeline once, output goes to `suggestions.md`'s proposal flow.
- **Saved-tree triage** does *not* re-run the build pipeline. Triage uses the cheap greedy-descent classifier (`cluster-place-beam-descent`) against an already-saved tree; only `hiker suggest save` and re-running `hiker suggest` regenerate the tree itself.
- **Never automatic.** No background build pass, no on-save trigger for the full pipeline. Triage *can* run on note save (per `suggestions.md`) but that's the cheap classifier, not a re-cluster.
- **Watcher does not drive the build.** Watcher events drive the *index* (chunk/embed updates); they don't drive the tree.


## Output: what suggestions consume

The cluster pass produces a `ClusterTree`: [cluster-tree-output]

```rust
struct ClusterTree {
    levels: Vec<Vec<ClusterNode>>,    // levels[0] = leaf clusters (notes), levels[N] = root
    outliers: Vec<NoteId>,            // unplaced
}

struct ClusterNode {
    id: ClusterId,                    // ephemeral per-run; not durable across runs
    members: Vec<MemberRef>,          // notes (level 0) or child clusters (higher)
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
