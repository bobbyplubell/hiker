# Clustering

How Hiker builds a hierarchical tree of topics from an unorganized vault. This doc covers the *build* side — turning embeddings into a tree of nodes with names. Placement of new notes into the resulting tree, and the reconcile flow that lets users accept/reject changes, live in `design.md` (Curated tree placement, Reconciliation). This doc fills the gap that `design.md` leaves implicit: cluster-level embeddings and summaries are *named* as inputs to placement, but the algorithm that produces them isn't.

Not built in v1. Lands alongside the curated-tree placement feature (post-v1, after the related-notes panel proves the index pipeline). Speccing now because (a) the build algorithm shapes what `hiker reconcile`'s output looks like, (b) it determines the cost model that decides whether reconcile runs in seconds or minutes, and (c) it's the thing the synthetic-corpus eval (`qa.md`) is supposed to validate.


## Approach: recursive cluster + LLM-summarize (RAPTOR-shaped)

Bottom-up tree construction. Repeatedly cluster the current level's embeddings, ask an LLM to summarize each cluster, embed the summary, and recurse. The summary serves three roles at once: it names the node (proposed name in the reconcile UI), it's the embedding seed for the parent level, and it becomes the user-visible "what's in this branch" description.

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

The note embedding is the mean of the note's chunk embeddings, weighted by chunk byte length. Computed lazily on first cluster pass and cached on the `notes` row (new column: `note_embedding BLOB`). Recomputed when any of the note's chunks change. This is cheap — a vector mean over typically <20 chunks — and avoids spending a separate embedder pass on each note.

Why mean-pool rather than embed the full note text directly: `bge-small`'s context window is 512 tokens (~2000 characters). A note longer than that gets silently truncated by the embedder, and personal-vault notes commonly exceed this — anything longer than a couple of paragraphs. Mean-pool over chunks sidesteps the limit entirely (each chunk is ~1200 chars by the chunker's cap, so each chunk fits), and the resulting representation already reflects the chunker's heading-bounded structure. There is no practical max note size for clustering with mean-pool. If we ever swap to a long-context embedder (`bge-m3` does 8k tokens, `nomic-embed-text` does 8k), direct embed becomes viable for most notes; mean-pool stays as a fallback for outliers.

Empty notes (no chunks) get no embedding and are excluded from clustering — they end up in the "inbox" (unplaced) bucket, same as outliers.


## Clustering algorithm: HDBSCAN

HDBSCAN over GMM (RAPTOR's choice) for three reasons specific to personal-vault scale:

1. **Outlier handling.** Personal vaults always have a long tail of notes that don't belong to any cohesive topic — fleeting thoughts, one-off snippets, miscellaneous reference. HDBSCAN labels these as outliers natively; they go to the inbox rather than being force-fit into a cluster they don't belong in. GMM's soft-probabilistic assignment can't represent "doesn't belong anywhere."
2. **No K.** Personal vault sizes vary by 100×. Tuning K per vault is annoying; HDBSCAN's `min_cluster_size` is more stable across scales (default 5).
3. **Determinism given seed.** Stable across reconcile runs — important because `reconcile-history.yaml` keys on rejected proposals, and unstable cluster identity makes that bookkeeping useless.

Tunables:
- `min_cluster_size` — smallest cluster the algorithm will form. Default 5; user-overridable per vault in `vault/.hiker/config.toml`.
- `min_samples` — density threshold. Default = `min_cluster_size`. Higher → more outliers.
- Distance metric — cosine, on the note embeddings.

Watch for: small vaults (<50 notes) where HDBSCAN may produce all-outliers and an empty tree. Fallback: if the level-0 pass produces fewer than 2 clusters, skip auto-org and surface a "vault is too small" message rather than a misleading tree of one node.

Possible future swap: GMM behind the same trait if outlier rate proves too aggressive. The clustering call lives in a single `core::cluster::partition(embeddings) -> Vec<ClusterAssignment>` boundary so swaps are local. Same module discipline as `core::store` and `core::embed`.

**User-selectable algorithm.** A `cluster.algorithm` setting in `vault/.hiker/config.toml` picks between `hdbscan` (default), `gmm`, and `hybrid`. Per-vault rather than per-user — different vaults have different shapes (a structured reference vault vs. a fleeting-thoughts journal cluster very differently).

**Hybrid mode.** `hdbscan` runs first to extract cohesive clusters and the outlier set. `gmm` then runs only on the outliers, with K = sqrt(outlier_count). Outliers that GMM places with probability > 0.6 join the corresponding HDBSCAN cluster as soft members; the rest stay in the inbox. This recovers the "doesn't fit confidently anywhere but isn't truly miscellaneous" middle ground without forcing every note into a cluster the way pure GMM does. Soft members are tagged in the cluster row so the reconcile UI can show them differently from primary members.

A second hybrid form — HDBSCAN structure with GMM soft-membership across *siblings* (every note gets P(cluster) for its cluster's siblings) — is interesting for surfacing "this note also touches topic Y" and connects naturally to the multi-axis idea in `design.md:215`. Deferred until the simpler hybrid is in production.


## Per-note placement (online, cheap)

The recursive build pass in this doc is the **batch / seed** operation — runs only on `hiker reconcile`, produces a fresh tree, expensive enough to be worth doing rarely. The complementary cheap operation is **per-note placement**: drop a single new note into the existing tree without touching anyone else's placement, no LLM calls, no re-clustering.

Per-note placement is fully specced in `design.md:252-257` (greedy centroid descent over the existing curated tree). Mentioned here to make the pairing explicit:

- **Build / reseed (this doc):** offline, on `hiker reconcile`, batch over the whole vault, produces a `ClusterTree` proposal.
- **Place (`design.md:252`):** online, on note-create or note-edit-saves-significant-content, embedding-only descent over the existing tree, writes a placement to the note's frontmatter.

Most of the time only `Place` runs — every new note gets a home cheaply. `Build` runs when the user decides it's worth re-examining the structure (new corpus shape, the tree feels stale, after a large import).


## User override

Auto-placement is a starting point, not a verdict. Users move notes in the file tree (the `drag-and-drop-move` flow) when they disagree with the placer. The provenance system in `design.md:259` makes this stick:

- A drag-move writes `hiker.placement: manual` to the note's frontmatter (only field Hiker writes to user files outside an explicit user action — the move *is* that action).
- `Place` and `Build` both treat `manual` and `confirmed` as immovable: the per-note placer skips notes already manually placed; the reconcile pass surfaces conflicts (where auto-clustering disagrees with manual placement) as **Conflict** deltas (`design.md:269`) for the user to review, never auto-overrides.
- A user can promote `auto:vN` → `confirmed` from the UI ("yes this is correct, lock it in"). Future reconciles treat confirmed placements like manual.

Net effect: the auto-org pass seeds an organization, the user reshapes it via normal tree drags, and successive reconciles preserve every manual decision. The system gets out of the user's way once they care enough to organize a region themselves.


## Why notes (not chunks) at level 0 — and what chunk-level clustering is good for instead

Note-level clustering at level 0 is the right default because the curated tree's leaves are notes — placement is per-note, navigation is per-note, the user's mental model of "where does this thing live" is per-note. A tree of chunks would index a different abstraction than the one users navigate.

That said, chunk-level clustering has real uses as a *parallel* feature, not a replacement:

- **Cross-note thread surfacing** — chunks from different notes that cluster tightly together are evidence of a thread that crosses several notes. Useful as a "you might be writing about X across these places" hint, not as input to an auto-built trail (trails are user-authored only — see `design.md`).
- **Multi-topic flagging** — a note whose chunks scatter across many distinct clusters is a candidate for splitting. Useful as a soft suggestion, not an auto-action.
- **Section reorganization** — chunk clusters *within* a single note suggest heading reorganization.

None of these are in scope for the v1-of-clustering pass. Listed here so the chunk-level signal isn't forgotten when the note-level pipeline is in place.


## Summarization

One LLM call per cluster per level. Input: cluster member titles + per-note summaries (or per-cluster summaries at higher levels). Output: a short cluster summary (1–3 sentences) and a proposed name (3–6 words).

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

Confidence below a threshold (default 0.5) marks the cluster as "uncertain" — the reconcile flow shows it but recommends the user review before accepting.

Model choice: a small local LLM is sufficient. Cluster naming is a much easier task than freeform writing — `Llama-3.2-3B` or similar runs on CPU and produces fine names. The expensive part is summarization quality; for v1-of-clustering, even template-based names ("Notes about X, Y, Z" using top-tf-idf terms) are a reasonable fallback if the LLM dependency is undesirable. Make the summarizer pluggable behind a `core::summarize` trait — same discipline pattern as embedder and store.


## Cluster identity across runs

Cluster IDs from HDBSCAN are inherently unstable — re-running on slightly different data produces clusters with shuffled integer ids, even when membership is essentially unchanged. The reconcile flow needs stable identity (so a previously-rejected cluster proposal isn't re-proposed every run).

Identity rule: a cluster on run N matches a cluster on run N-1 if their **member Jaccard similarity is ≥ 0.7**. Stable id is carried forward; new cluster id is minted only on first appearance or when match fails. Stored in `vault/.hiker/cluster-history.yaml`:

```yaml
clusters:
  - id: cl_01HXYZ...   # ulid, minted once
    first_seen: 2026-04-12T...
    last_seen: 2026-05-04T...
    members_last: [note_id_1, note_id_2, ...]
    name: "Embedding research"
```

Centroid drift is *not* used for identity — content shifts faster than membership in personal vaults. Membership is the durable signal.


## Cost model

Dominant cost: LLM calls for summarization. One call per cluster per level.

Rough numbers for a small local model (~3B, ~50ms/call on CPU):

| Vault size | Leaf clusters | Mid levels | Total calls | Wall time |
| ---------- | ------------- | ---------- | ----------- | --------- |
| 100 notes  | ~10           | 1          | ~13         | <1s       |
| 1k notes   | ~80           | ~12, ~3    | ~95         | ~5s       |
| 10k notes  | ~500          | ~70, ~10, ~2 | ~580      | ~30s      |

Embedding cost (one extra call per cluster summary) is negligible relative to summarization.

Reconcile is **not** an interactive operation. Run on a user-triggered `hiker reconcile` command, results go to a proposal file, user reviews on their own time. No need for sub-second budgets.


## When it runs

- **`hiker reconcile`** — explicit user command. Reads current vault state, runs the full pipeline, writes a proposal to `vault/.hiker/proposals/<timestamp>.yaml`. The flow described in `design.md:264-273` consumes that proposal.
- **Never automatically.** No background reconcile, no on-save trigger. The clustering pass is offline by design — re-clustering touches every cluster's identity, and surprising the user with reorganized topics is exactly the trust violation `design.md` warns against for frontmatter writes.
- **Watcher does not drive it.** Watcher events drive the *index* (chunk/embed updates); they do not drive the *tree*.

Future: a `--watch` flag that re-runs reconcile on a schedule (daily?), still writing to a proposal file the user reviews — never auto-applying. Keep the human in the loop until trust is established.


## Output: what reconcile consumes

The cluster pass produces a `ClusterTree`:

```rust
struct ClusterTree {
    levels: Vec<Vec<ClusterNode>>,    // levels[0] = leaf clusters (notes), levels[N] = root
    outliers: Vec<NoteId>,            // unplaced
}

struct ClusterNode {
    id: ClusterId,                    // stable across runs via Jaccard match
    members: Vec<MemberRef>,          // notes (level 0) or child clusters (higher)
    centroid: Vec<f32>,               // mean of member embeddings
    radius: f32,                      // 90th-percentile member distance from centroid
    name: String,                     // LLM-proposed
    summary: String,                  // LLM-generated
    confidence: f32,                  // 0.0-1.0 from summarizer
}
```

`hiker reconcile` walks this tree against the existing curated tree (in `vault/.hiker/tree.yaml` per `design.md:242`) and emits the four delta types from `design.md:266-269`. The shape above is everything that flow needs.


## Module discipline

All clustering logic lives in `core::cluster`. Outside the module: `partition()` returns plain assignments, `build_tree()` returns a `ClusterTree` of plain Rust types. No HDBSCAN-crate types leak past the boundary. Same reasoning as `core::store` and `core::embed` — algorithm choice is a defensible default, not a permanent commitment, and the future swap (GMM, agglomerative, leiden) should be a one-file rewrite.

Summarization is its own module (`core::summarize`) since it has independent failure modes (LLM unavailable, slow, low-quality) and an independent swap surface (local model vs cloud vs template fallback).


## Out of scope

- Online incremental clustering (clusters update as notes change). v1 of this feature is batch-only on `hiker reconcile`.
- Multi-axis trees (one tree per type — semantic, temporal, entity). Single semantic tree only at first; the multi-axis idea from `design.md:215` is a later concern.
- User-guided cluster splits/merges in the reconcile UI beyond accept/reject of full proposals.
- Cross-vault clustering. One tree per vault.
- Trail discovery from clusters. Trails are user-authored only by design (see `design.md`); the clustering pipeline never proposes them.
