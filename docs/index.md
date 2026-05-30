# Index

The v1 indexer: watcher → chunker → embedder → store, plus the "related notes" query that exposes it. design.md sketches the full multi-axis index model; this doc nails down the concrete v1 slice. Lexical search, structural/temporal/entity/provenance indexes, MCP exposure, and curated-tree placement are out of scope here — they layer on later.

v1 goal: open a note, see a panel listing semantically related notes from the same vault. Nothing else. The store, the chunking, and the embedding pipeline all need to exist for that to work, but they should be the simplest version that supports the goal without painting v2+ into a corner.


## Store: sqlite-vec

Single SQLite database at `vault/.hiker/index.db`. Vectors via the `sqlite-vec` extension, FTS via SQLite's built-in FTS5 (used in v2; schema reserved here so v1 doesn't migrate). One file per vault, regenerable from content — never the source of truth, only a cache. [store-schema-v1]

Brute-force search over sqlite-vec is fine at this scale (10k–500k chunks); one transaction spans vectors and future FTS, and the whole store is a single-file backup. Revisit ANN if a vault pushes past ~500k chunks.

Schema (initial):

```sql
-- one row per indexed file
CREATE TABLE notes (
  id            TEXT PRIMARY KEY,           -- ulid; THE SAME id as op-log's doc_id for this path
  path          TEXT NOT NULL UNIQUE,       -- vault-relative
  content_hash  TEXT NOT NULL,              -- blake3 of file body; skip re-embed if unchanged
  mtime         INTEGER NOT NULL,           -- unix seconds; cheap pre-check before hashing
  size          INTEGER NOT NULL,
  indexed_at    INTEGER NOT NULL,
  embedder_version TEXT NOT NULL,           -- forces re-embed when the model changes
  note_embedding BLOB                       -- packed f32 mean-pool of chunk embeddings; lazy, per cluster-note-embeddings
);

-- one row per chunk
CREATE TABLE chunks (
  id            TEXT PRIMARY KEY,           -- "<note_id>:<chunk_index>"
  note_id       TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
  chunk_index   INTEGER NOT NULL,           -- 0-based, contiguous within a note
  byte_start    INTEGER NOT NULL,           -- offset into note body
  byte_end      INTEGER NOT NULL,
  text          TEXT NOT NULL,              -- the chunk content (for snippet rendering)
  heading_path  TEXT,                       -- e.g. "Setup > Database" or NULL
  UNIQUE(note_id, chunk_index)
);

-- vec0 virtual table; one row per chunk. The `N` is filled in at CREATE
-- time from the loaded embedder's `dim()`; 384 for bge-small, 768 for
-- embedding-gemma-300m, 1024 for bge-m3. See "Dim-from-model" below.
CREATE VIRTUAL TABLE chunk_vecs USING vec0(
  chunk_id      TEXT PRIMARY KEY,
  embedding     FLOAT[N]
);
```

Notes:

- `chunks.text` is duplicated from the source file for snippet rendering and to decouple the index from filesystem reads on every query. Cheap; vault sizes don't justify cleverness here.
- **`notes.id` is op-log's `doc_id` for this path** — one ULID per document, minted by op-log on first ingest. The indexer reads it from op-log's `doc-index.db` (`oplog::doc_id_for_path`) when upserting a note; it never mints its own. This is why there is no separate `path_ids` table: the authoritative path↔id mapping lives in op-log. Renames update the mapping there; the id never changes. [store-id-from-oplog]
- Schema version pragma (`PRAGMA user_version = N`) so future migrations have a hook.

**Static linking.** Both SQLite itself and the sqlite-vec extension are statically linked into the Hiker binary — `rusqlite` with the `bundled` feature compiles the SQLite C source in, and the `sqlite-vec` crate compiles the vec extension's C source in via its `cc` build-script. No system libsqlite3 dependency, no runtime extension load, no separate `vec0.so` to ship. One binary, no surprises across OS/distro versions. [store-sqlite-vec-static]

**Open semantics (`Store::open(vault_root)`).** Creates `vault/.hiker/index.db` if missing, runs idempotent schema setup (every `CREATE TABLE` uses `IF NOT EXISTS`) on a matching `user_version`, and **fails loudly on version mismatch** rather than auto-migrating. v1 ships at version 1; if a future binary opens a v2 db (or vice-versa), the user gets a clear error rather than silent breakage. [store-version-fail-loud]

**Migration policy (pre-real-use).** Until the project crosses into "I'm storing my actual notes in this" territory, no migration code is written. Schema bumps are handled by deleting `.hiker/index.db` and re-indexing — the db is regenerable from content, so the cost is one rescan. Saves a lot of effort writing migrations against a schema that's still moving. When real-data use begins, the policy flips: every schema bump from that point ships a migration alongside it. Worth a one-line note in the changelog when that line is crossed.

**Writer connection ownership.** The single writer connection lives *inside* the indexer task, not behind an `Arc<Mutex<Connection>>`. Only the indexer task ever writes; the design's mpsc job queue already serializes access. No mutex needed, no shared state to reason about. Read connections are entirely independent (fresh per call as described above), so writer/reader concurrency is handled at the SQLite WAL level rather than in Rust locking.


## Store module discipline

All sqlite-vec / rusqlite usage is confined to a single `core::store` module (start as `core/src/store.rs`; split into `core/src/store/sqlite.rs` etc. if/when a second backend lands). Everything outside `store` — ingest, search, related, CLI handlers — interacts via a narrow API of plain Rust types: `upsert_note`, `delete_note`, `rename_note`, `get_note_chunks`, `knn_chunks`, returning owned structs (`ChunkHit`, `NoteRow`, ...) not driver types. [store-module-discipline]

Keeping all SQL inside one module makes swapping the store backend a one-file rewrite, not a codebase-wide grep. The cost of the discipline is near-zero now; not having it scales with every callsite that learns to write SQL directly.

Specifically forbidden outside `store`: importing `rusqlite`, returning `rusqlite::Row` or `sqlite_vec`-specific types, embedding SQL strings in handler code. The trait/struct boundary is the whole point.


## Vault tolerance (v1)

v1 is permissive about what's already in the user's vault. No "init" step rewrites files; no migration mutates content.

**Non-markdown files are silently ignored.** PDFs, images, audio, office docs, code files — all sit in the vault untouched. The indexer doesn't error, doesn't warn, doesn't produce sidecars. They simply aren't searchable until the extractor pipeline lands per design.md:419 (v4+). Users importing an existing mixed-content folder get a working v1 over their markdown subset on day one; non-md content waits its turn.

**Frontmatter is optional and never auto-injected.** Hiker reads `hiker:`-namespaced frontmatter if present (currently unused at v1; reserved for tags, ids, and lifecycle flags as they land), strips frontmatter before chunking either way, and tolerates its complete absence. The indexer never writes to a user's `.md` file as a side effect of opening, viewing, or indexing it. The path→id table in the store is the authoritative id source in v1; the `hiker.id` field in frontmatter only starts being written when an explicit user action requires a stable id in the file itself (creating a wikilink target, pinning a trail waypoint, etc.) — none of which exist in v1. This rule exists because users keep markdown in many tools simultaneously (vim, other markdown apps, git, mobile editors), and silently mutating their files would be a hard-to-undo trust violation.


## Chunking (v1)

Heading-bounded splits over the markdown AST, with a soft size cap.

- Parse the note with pulldown-cmark. Walk top-level blocks; treat each `H1`–`H6` as a chunk boundary. [chunker-heading-bounded]
- Within a heading section, accumulate blocks (paragraphs, lists, code, blockquotes) into a chunk until reaching ~1200 chars; then start a new chunk *within the same heading*, carrying the heading_path forward. [chunker-soft-size-1200]
- Frontmatter is stripped before chunking (separate handling later for tag/entity extraction; v1 ignores it). [chunker-frontmatter-strip]
- Code blocks are kept whole — never split mid-block, even if it busts the size cap. [chunker-code-blocks-whole]
- Empty notes produce zero chunks (no embedding, but the `notes` row still exists so renames/deletes are tracked).

`heading_path` is the breadcrumb (`"Section > Subsection"`) of the heading whose body the chunk falls under, or `NULL` for content above any heading. Not used for v1 ranking; stored for future use (snippet rendering, structural index). [chunker-heading-path]

`chunk_index` is contiguous and 0-based per note. The chunk id `<note_id>:<idx>` is stable as long as a note's chunk count and order don't change. Edits invalidate ids — that's fine for v1; agent stable-reference concerns from design.md:402 are an MCP-era problem.


## Embedder

`fastembed-rs` v5 with one of three selectable models, picked via `[indexing].model` in settings: [embedder-fastembed-v5, embedder-model-selectable]

| `model` id | fastembed variant | Dim | Ctx | Notes |
| --- | --- | --- | --- | --- |
| `bge-small-en-v1.5` (default) | `EmbeddingModel::BGESmallENV15` | 384 | 512 | English-tuned, ~30MB, CPU-friendly |
| `bge-m3` | `EmbeddingModel::BGEM3` | 1024 | 8192 | Multilingual (100+ languages), long context, ~1.2GB |
| `embedding-gemma-300m` | `EmbeddingModel::EmbeddingGemma300M` | 768 | 2048 | Google's 300M-param embedder; ONNX from `onnx-community/embeddinggemma-300m-ONNX` |

`bge-small-en-v1.5` stays the default — smallest, fastest, no download surprise on first run. The other two are opt-in for users who want multilingual, longer chunks, or a quality bump. Stored as the model id verbatim in `notes.embedder_version`. [embedder-version-tag, embedder-version-per-model]

Bumping `[indexing].model` triggers full re-embed naturally — the `embedder_version` column on every existing row goes stale, and the indexer re-embeds. No migration code needed for the embedding bytes themselves. Dim changes additionally require a schema rebuild — see "Dim-from-model and schema rebuild" below.

Batching: embed in batches of 64 chunks. Run on a dedicated tokio task; never block command handlers. [embedder-batch-64]

**CPU-bound boundary.** fastembed-rs is synchronous and CPU-heavy. Both model load (multi-second on first call) and `embed_batch` must run via `tokio::task::spawn_blocking` (or on a dedicated `std::thread` driven by an mpsc channel). Calling them directly inside an async context will block one of tokio's worker threads and starve other tasks. The indexer task awaits the spawn_blocking handle to keep its mpsc-driven loop async-shaped while the actual work runs on the blocking pool. [embedder-spawn-blocking]

**Module discipline (mirrors the store):** all fastembed-rs usage lives in `core::embed` (single `core/src/embed.rs`, splittable later). Outside the module, code calls `Embedder::embed_batch(&[String]) -> Vec<Vec<f32>>` and treats it as opaque. No fastembed types leak past the boundary. The embedder is at least as likely to be swapped as the store (cloud, a different local model, multilingual), and the trait shape is what makes future backends cheap. v1 ships fastembed-only, no fallback. [embedder-module-discipline]

**Model storage:** downloaded model files live under the platform data dir (Linux `~/.local/share/hiker/models/`, macOS `~/Library/Application Support/hiker/models/`, Windows `%APPDATA%\hiker\models\`). Use the `directories` crate; do not roll path logic by hand. Treated as durable data rather than cache because re-downloading 30MB on a slow or metered connection is a real cost; users may still delete the directory and the app re-downloads on next launch. [embedder-platform-data-dir]

**First-run UX:** model download is non-blocking. Vault opens normally; the indexer starts but defers any embedding work until the model is on disk. Status bar / settings surface the download progress. Search/related queries return empty with a "indexing not yet ready" indicator until the first batch completes. See `settings.md` for tunables (download timing, model selection, batch size, manual reindex triggers). [embedder-first-run-nonblocking]

**Model load as a queue task.** Both first-run download and hot-swap (`embedder-hot-reload-on-model-change`) can take from seconds (cached) to minutes (cold download of `bge-m3`'s ~1.2GB). To make the work visible, the indexer wraps every `FastembedEmbedder::load_id` call in a task queue row — a new `TaskKind::EmbedderModelLoad { model_id }` enqueued just before the `spawn_blocking` load and marked complete (or failed) when the load returns. The row is **indeterminate**: no byte-progress, no percentage — fastembed v5 doesn't expose a download-progress callback, and the v1 goal is just "the user can see something is happening." Queue badge counts the row like any other task; queue detail page shows the model id and elapsed time. Failed loads stay as a failed-task row with the error so the user can see why the swap didn't take. [embedder-model-load-as-task]

### Dim-from-model and schema rebuild

The `chunk_vecs` vec0 virtual table fixes its embedding column width at `CREATE` time — `embedding float[N]` is baked into the table definition. Different models have different dims (384 / 768 / 1024), so v1's single-`const EMBED_DIM = 384` posture no longer holds. [embedder-dim-from-model]

- `EmbedDim` (a small `usize`-newtype wrapper) is reported by the loaded `Embedder` via `Embedder::dim()`. It's the single source of truth at runtime.
- `Store::open` reads the on-disk vec0 column dim once at startup. If it matches the loaded embedder's `dim()`, nothing to do. If it differs (or the table doesn't exist yet), the store creates `chunk_vecs` with the new dim before any ingest runs. [store-rebuild-chunk-vecs-on-dim-change]
- The rebuild path: `DROP TABLE chunk_vecs` → recreate with the new `float[N]` → clear `notes.note_embedding` (different-dim packed f32s are garbage at the new dim) → clear `notes.embedder_version` so the existing per-note re-embed trigger (`embedder-version-tag`) picks them all up on the next ingest pass. Schema-version bump *not* required — the rebuild is observable through the dim mismatch itself, and the `notes` / `chunks` shapes don't change.
- Reading the on-disk dim: sqlite-vec doesn't expose the vec0 column dim through `PRAGMA table_info` (the declared type slot comes back empty). The store keeps a small `meta(key, value)` sidecar table and writes the active dim into a `chunk_vecs_dim` row whenever `chunk_vecs` is created or recreated. `Store::open` reads it from there; missing row + existing `chunk_vecs` is treated as the legacy 384 case and migrated.

Drop-and-recreate rather than ALTER: vec0 has no column-type change, and every chunk re-embeds anyway (dim-incompatible), so there's nothing worth migrating.

### Alternative embedder backends (cloud / Ollama)

The `Embedder` trait that hides fastembed-rs also hides any other backend. A second concrete impl, `core::embed::LlmEmbedder`, wraps the [`llm`](https://crates.io/crates/llm) crate's `EmbeddingProvider` trait — providing access to OpenAI, Ollama, Google, Cohere, Mistral, and HuggingFace embedding models. Same trait, same interface, indexer code doesn't change. [embedder-llm-crate-backed]

fastembed stays the default (zero-config first run, no per-call cost, offline, no cloud egress of every chunk); cloud / Ollama backends are opt-in for quality, longer chunks, or reusing an existing Ollama runtime.

Config in `[embedder]` in user/vault TOML, same shape as `[llm]`: [embedder-config-section]

```toml
[embedder]
provider = "fastembed"          # default
model = "bge-small-en-v1.5"

# or:
# provider = "openai"
# model = "text-embedding-3-large"
# api_key_env = "OPENAI_API_KEY"

# or:
# provider = "ollama"
# model = "nomic-embed-text"
# base_url = "http://localhost:11434"
```

API keys live in environment variables (`api_key_env` names the variable), never in TOML. Same posture as `[llm]`'s provider config.

The existing `embedder_version` column on `notes` (`embedder-version-tag`) keys off provider + model, not just model name — so switching provider naturally triggers re-embed via the existing fail-loud machinery. No new migration code; the schema-version contract already covers it. [embedder-version-tag-includes-provider]

**Module discipline** unchanged — `fastembed-rs` and the `llm` crate both confined to `core::embed`. The `Embedder` trait surface stays narrow; no fastembed *or* `llm` types leak past it. The same crate boundary discipline used in `core::llm` (the generative LLM module — see `llm.md`) applies here.

**ToS posture:** embeddings are always automation-shaped (every chunk on every ingest). Cloud embedding APIs (OpenAI, Cohere, Voyage, etc.) are explicitly priced for automation, so no ACP grey area applies; this is firmly a pay-per-call use case. The interactive-vs-background distinction from `llm.md` is about *generative* LLM access and doesn't carry over.


## Ingest pipeline

Triggered three ways, all funnel into the same upsert path:

1. **Startup scan** — walk vault, compare `mtime`/`size` against `notes` rows, queue any new/changed/missing. [ingest-startup-scan]
2. **Watcher event** — single file changed/created/deleted/renamed (see `watcher.md`). [ingest-watcher-driven]
3. **Manual** — `hiker reindex [path]` CLI subcommand and a future "reindex" UI button. [ingest-manual-cli]

The startup scan and watcher additionally enqueue *extract* jobs for non-md sources per `extract.md`'s trigger model (`extract-trigger-auto-glob` / `extract-trigger-on-demand`); those produce sidecar `.md` files that re-enter this same upsert path.

Per-file pipeline:

```
read file → compute blake3 hash → if hash matches notes.content_hash AND
                                     embedder_version matches AND force=false → no-op
         → else: parse → chunk → embed batch → BEGIN TX
                                                 compute byte-weighted mean-pool
                                                   of the new chunk embeddings →
                                                   notes.note_embedding
                                                 upsert notes row
                                                 delete old chunks + vecs for note_id
                                                 insert new chunks + vecs
                                               COMMIT
         → emit indexer-progress events
```
[ingest-tx-upsert, ingest-progress-events, cluster-note-embeddings]

The note-level pool is computed and persisted in the same transaction as the chunks, so every successful upsert leaves the cached pool in sync with the chunk set the cluster pipeline consumes. Notes with no chunks leave `note_embedding` NULL.

Deletes: cascade via the FK on `chunks`; vec rows cleaned up explicitly (vec0 has no FK enforcement). [ingest-delete-cascade]

Renames: detected by the watcher's rename event when available; otherwise inferred (delete + create within a small window with matching content hash → rename, preserve id). v1 starts with the simple version: rely on notify's rename events on Linux/macOS, fall back to "treat as delete + create with new ulid" on Windows or when events arrive non-paired. [ingest-rename-preserve-id]

Concurrency:

- Indexer owns a single tokio task with an mpsc channel of `IndexJob` messages. Watcher and command handlers send jobs; the task drains them serially.
- Serial processing is fine for v1 — embedding throughput is the bottleneck and batching across files is a v2 optimization.
- The indexer task holds the only sqlite connection for writes. Reads (search, related) open a fresh connection per call and drop it on return — sub-millisecond cost on a warm cache, zero shared-state concerns. Database is opened in WAL mode (`PRAGMA journal_mode = WAL`) so readers don't block on the writer's transactions. If the sqlite-vec extension load turns out to be slow per-open, switch to an `r2d2` reader pool — one-day change since all SQL lives in `core::store`. [store-wal-mode]


## Related-notes query (v1)

The v1 panel: open a note, list the top N notes whose chunks are most similar.

Algorithm:

1. Look up `note_id` from path. If the note has no chunks (empty or unindexed), panel is empty.
2. Fetch all of this note's chunk embeddings.
3. For each one, KNN search the `chunk_vecs` table for top 20 nearest *excluding* chunks belonging to the same note.
4. Group hits by their `note_id`; score each candidate note as `max(similarity)` across its hit chunks.
5. Return top 10 notes by score, with: title (filename stem until frontmatter parsing lands), path, score, best-matching chunk's `heading_path` and a short snippet. [related-notes-query, related-notes-snippet]

This is intentionally crude. No reranking, no rank fusion, no entity boosting. Good enough to validate the pipeline; the design.md:227 query pipeline arrives in v2 alongside lexical search.

The full algorithm lives behind a single `Store::related_notes(source_note_id, top_k) -> Vec<RelatedHit>` method — keeping the per-chunk KNN loop, exclude-source-note filter, and group-by-note aggregation inside the store module preserves the SQL-stays-in-one-place discipline. Callers (host command handlers, MCP later) hand it a note id and receive note-shaped hits.

Latency budget: the panel updates on file-open and on save (debounced 500ms). Brute-force KNN over a 100k-chunk vault should be <100ms; if it isn't, that's the signal to add an ANN index, not before.


## Structured metadata index

A queryable index over each note's frontmatter, backing *structured* retrieval — "notes tagged `project` with `status: active`, newest first" — distinct from the semantic / lexical content indexes. Powers `search-tag-scope` and the plugin query archetype (`plugins.md`'s `notes.query` host call).

- **`note_meta` table.** The note's frontmatter flattened to `(note_id, key, value, num)` rows: nested maps use dotted keys (`hiker.author`), list elements explode to one row each (`tags: [a, b]` → two rows), null values are skipped. `num` mirrors `value` for YAML numbers / bools so range filters and numeric ordering need no parse at query time. Re-derived from frontmatter on every ingest (mirrors `trail_waypoints`), cleared on skip / delete. Entries are capped per note to bound pathological frontmatter. [store-note-metadata-index]
- **`query_notes(NoteQuery)`.** Structured query: AND-ed filters (`Equals` / `Exists` / `NumRange`), a `folder` subtree restriction, `order` (mtime / path / a meta key's numeric or text value), `limit`, and a `select` projection that packs chosen keys into each row's `fields`. Each filter compiles to an EXISTS subquery against `note_meta`; every user-supplied string is a bound parameter, never interpolated. Skipped notes are excluded. [store-note-query]

Tags ride this index as `Equals { key: "tags", value: "<tag>" }` — there is no separate tag table; list-valued frontmatter is simply multiple rows under one key. Lifecycle (`hiker.archived` …), authorship (`hiker.author`), and source type (`hiker.type`) are likewise plain frontmatter keys, so the lifecycle / authorship / source-type filters from `design.md` fall out of the same query surface with no new structure.

Schema bumps to v8 (the `note_meta` table); per `store-version-fail-loud` the bump is handled by deleting `.hiker/index.db` and re-indexing until real-data use begins.


## Command surface (v1 additions)

Existing v0 commands stay unchanged (`open_vault`, `list_dir`, `read_file_with_hash`, `write_file_checked`). v1 adds three:

- `related_notes(path: String) -> Vec<RelatedHit>` — runs the related-notes query above. Empty vec for unindexed or empty notes; never errors on absence. [cmd-related-notes]
- `index_status() -> IndexStatus` — snapshot of indexer state for the status bar / settings UI. Shape: `{ model_ready: bool, queued: u32, total_notes: u32, last_error: Option<String> }`. `queued` here is the mpsc-channel depth, which sits at ~1 during a `FullScan` because that handler processes per-file Upserts inline. The indexer-detail panel surfaces work-remaining via `IndexerHandle::pending_count()` instead (the size of the in-flight `pending` paths set, pre-populated with every Upsert path at FullScan start) so the user sees a number that counts down from N to 0 across the scan. [cmd-index-status, indexer-detail-pending-counter]
- `index(scope: IndexScope) -> ()` — enqueue index jobs. `IndexScope::All` triggers a full rescan; `IndexScope::Path(rel)` re-indexes a single file. Same command covers first-time indexing and re-indexing — there's no semantic difference between them, just whether rows existed before. Returns immediately; progress comes via indexer-progress events. [cmd-index]
- `chunks_for(path: String) -> Vec<ChunkBounds>` — ordered chunk bounds for the note at `path`. `ChunkBounds = { chunk_index: u32, byte_start: u64, byte_end: u64, heading_path: Option<String> }`. Empty vec for unindexed or empty notes; never errors on absence. Backs the chunk-boundary view (`view-show-chunk-boundaries` in `editor.md`). [cmd-chunks-for-path]

`RelatedHit` shape (note-level, since the v1 panel renders by note):

```rust
struct RelatedHit {
    note_id: String,
    path: String,
    title: String,                 // filename stem until frontmatter parsing lands
    score: f32,
    best_heading_path: Option<String>,
    snippet: String,               // text from the highest-scoring chunk
}
```

DTOs live in `core::dto` and are auto-exported to TS via `ts-rs` per design.md:371.


## Per-file index state

The `notes` row already answers "is this file indexed" — presence + non-zero chunks = yes. v1 expands the surface so the UI can also explain *why not* when the answer is no, and render a distinct tree-row marker for each case (see `tree-row-unsupported-marker` / `tree-row-skipped-marker` / `tree-row-queued-marker` in `files.md`). Three non-indexed states:

- **Unsupported** — the extension has no chunker (`is_indexable_path` returns false). Derivable client-side from the path; no store row required. The indexer never sees the file.
- **Skipped** — a chunker exists but ingest refused: file exceeded the 5MB sanity cap, failed UTF-8 decode, or (future) hit a corrupted-source signal. The indexer records the attempt as a `notes` row with a `skipped` flag set and a short `skip_reason` string. Storing the row (rather than dropping silently) is what lets the UI distinguish "skipped on purpose" from "never seen."
- **Queued** — the file is in the indexer's mpsc queue or actively processing. Transient; not stored — exposed via indexer-progress events.

Schema addition (v1 schema bumps to `user_version = 2`): `notes.skipped` (BOOLEAN, default 0) and `notes.skip_reason` (TEXT, NULL when not skipped). Per the migration policy in `store-version-fail-loud`, the bump is handled by deleting `.hiker/index.db` and re-indexing until real-data use begins.

Surface:

- `index_state_for(path: String) -> IndexState` command. Returns `Indexed`, `Unsupported`, `Skipped { reason: String }`, or `Queued`. One path lookup; cheap enough for the tree to call lazily on render of visible rows. The skip reason is a stable, short, human-readable string (`"file too large"`, `"not UTF-8"`) used directly in tooltips and the status bar — no translation layer. [cmd-file-index-state]
- indexer-progress events (per `ingest-progress-events`) carry per-file transitions, so the tree flips rows from Queued → Indexed (or Skipped) without polling.

Indexer logic: when ingest decides to skip a file, write the `notes` row with `skipped = 1` and a reason; do not chunk, do not embed. A subsequent successful re-ingest of the same path clears the flag. Deletes cascade as before (`ingest-delete-cascade`).


## Reindex verbs

`cmd-index` covers the mechanics; v1 wires two UI verbs to it through the sidebar's `⋯` actions menu in Files mode (see `files.md`'s `sidebar-toolbar-actions-menu`):

- **Reindex all** — `index(IndexScope::All)` with the `force` flag set: bypasses the content-hash + embedder-version short-circuit so every note re-embeds even when nothing changed. The button is the user's explicit "redo all the work" verb; without `force` the click would be a no-op on a clean vault. The first-launch / vault-open startup scan and watcher-driven Upserts still default to `force=false` so the cheap-when-nothing-changed path keeps applying to ambient ingest. [reindex-all-action]
- **Reindex this file** — `index(IndexScope::Path(currentPath))` with `force=true` for the same reason. Greyed when no file is active. [reindex-current-file-action]

A third verb, **Reindex (rebuild)** — drops + recreates the schema, then a full reindex — is deferred to the settings page per `settings.md`'s indexing section. The CLI counterpart `cli-reindex-rebuild` covers the operational case in the meantime, so v1 ships without the in-app rebuild button. [reindex-rebuild-action]


## Walker symlink handling

The startup-scan walker (`walkdir` or `ignore` crate) must match the watcher's symlink policy: **don't follow symlinks** in v1. Both walker and watcher will pick this up consistently; the goal is no surprise where the indexer sees content the watcher can't notify on, or vice versa. External-path ingestion in v2+ revisits this with an explicit allowlist. [walker-symlink-policy]


## Failure modes

- Corrupt `index.db` — delete it, full rescan rebuilds. Provide a `hiker reindex --rebuild` command that drops and recreates the schema.
- Embedder model download fails on first run — surface the error, don't silently disable indexing. User can retry.
- File grows past a sanity cap (say 5MB of markdown) — log and skip; don't OOM the embedder. Likely an accidentally-committed binary.
- `.hiker/index.db` itself: never indexed (skip the `.hiker/` prefix in the walker — see watcher.md).


## Out of scope for v1 (lands later)

- FTS5 lexical index and hybrid query fusion (v2)
- MCP exposure of search/related (v3 per design.md:413)
- Chunk stability under edits (chunk_id needs to survive small edits before MCP agents pin to them — needs a content-addressed scheme)
- Entity / tag / structural / temporal / provenance indexes
- Curated-tree placement and the reconcile flow
- External-file ingestion and source-derived notes
- Reindex throttling / priority (currently strict FIFO)
- Multi-vault routing (the route → recall → fuse pipeline assumes vault/folder embeddings that don't exist yet)
