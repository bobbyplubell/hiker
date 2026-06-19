# Index

The v1 indexer: watcher → chunker → embedder → store, plus the "related notes" query that exposes it. design.md sketches the full multi-axis index model; this doc nails down the concrete v1 slice. Lexical search, structural/temporal/entity/provenance indexes, MCP exposure, and curated-tree placement are out of scope here — they layer on later.

v1 goal: open a note, see a panel listing semantically related notes from the same vault. Nothing else. The store, the chunking, and the embedding pipeline all need to exist for that to work, but they should be the simplest version that supports the goal without painting v2+ into a corner.


## Store: sqlite-vec

Single SQLite database at `vault/.hiker/index.db`. Vectors via the `sqlite-vec` extension, FTS via SQLite's built-in FTS5 (used in v2; schema reserved here so v1 doesn't migrate). One file per vault, regenerable from content — never the source of truth, only a cache. [store-schema-v1]
status:: done
note:: notes / chunks / chunk_vecs (+ trail_waypoints, board_cards, note_meta, meta, cluster_centroids); `path_ids` retired in v10 under [[spec:store-path-is-identity]]. Slug name kept for stability; the schema-version constant tracks the actual on-disk version · evidence: `core/src/store/mod.rs::SCHEMA_VERSION = 10`

Brute-force search over sqlite-vec is fine at this scale (10k–500k chunks); one transaction spans vectors and future FTS. Revisit ANN if a vault pushes past ~500k chunks.

Schema (initial) — three tables:

- **`notes`** (one row per indexed file): `path` PK (vault-relative — the note's identity), `content_hash` (blake3 of body, skip re-embed if unchanged), `mtime` (cheap pre-check before hashing), `size`, `indexed_at`, `embedder_version` (forces re-embed on model change), `note_embedding` (packed-f32 mean-pool of chunk embeddings, lazy, per [[spec:cluster-note-embeddings]]).
- **`chunks`** (one row per chunk): `note_path` FK → `notes(path)` ON DELETE CASCADE ON UPDATE CASCADE (a rename re-keys the chunks with the note), `chunk_index` (0-based, contiguous), `byte_start` / `byte_end`, `text` (chunk content, for snippets), `heading_path` (e.g. `"Setup > Database"` or NULL), `PRIMARY KEY (note_path, chunk_index)`. The vec-join key is `"<note_path>:<chunk_index>"`.
- **`chunk_vecs`** (vec0 virtual table, one row per chunk): `chunk_id` PK (= `"<note_path>:<chunk_index>"`), `embedding FLOAT[N]` where `N` is filled at CREATE time from the loaded embedder's `dim()` (384 bge-small / 768 embedding-gemma-300m / 1024 bge-m3 — see "Dim-from-model").

Notes:

- `chunks.text` is duplicated from the source file for snippet rendering and to decouple the index from filesystem reads on every query.
- **A note's identity is its vault path** — `notes.path` is the primary key; there is no minted document id, no `doc_id`, no `doc-index.db`, and no `path_ids` table ([[spec:op-log-path-identity]]). A rename is a path update — an observed content-preserving move ([[spec:op-log-observed-move]]) — handled by `rename_note` (Renames, below), which updates the `notes.path` PK and cascades to `chunks.note_path`; because `content_hash` is unchanged across a pure rename, the move never triggers re-embed. [store-path-is-identity]
status:: done
implements:: [[code:hiker/boards/list]], [[code:hiker/boards/get_board]], [[code:hiker/boards/ops/create_board]], [[code:hiker/indexer/jobs/impl#[`JobCtx<'a>`]handle_rename]], [[code:hiker/indexer/jobs/process_upsert]], [[code:hiker/indexer/jobs/impl#[`WaypointIngest<'a>`]upsert_waypoint_row]], [[code:hiker/indexer/jobs/impl#[`WaypointIngest<'a>`]rebuild_trail_doc_rows]], [[code:hiker/indexer/jobs/update_board_cards_if_relevant]], [[code:hiker/indexer/jobs/process_delete]], [[code:hiker/search/query]], [[code:hiker/store/chunks/impl#[Store]chunk_bounds_for]], [[code:hiker/store/chunks/impl#[Store]compute_and_store_note_embedding]], [[code:hiker/store/chunks/impl#[Store]get_note_chunks]], [[code:hiker/store/notes/impl#[Store]note_exists]], [[code:hiker/store/notes/impl#[Store]find_notes_by_basename]], [[code:hiker/store/notes/impl#[Store]delete_note_by_path]], [[code:hiker/store/notes/impl#[Store]rename_note_by_path]], [[code:hiker/store/notes/impl#[Store]rekey_note_in_tx]], [[code:hiker/store/vec/knn_chunks_on]], [[code:hiker/trails/list]], [[code:hiker/trails/get_trail]], [[code:hiker/trails/ops/append_waypoint]], [[code:hiker/trails/ops/on_note_moved]], [[code:hiker/vault/rollback_companion]], [[code:hiker/vault/restore_note]], [[code:hiker/related/refresh_cache]]
verifies:: [[code:hiker/indexer/tests/ingesting_trail_doc_and_waypoint_populates_derived_table]]
touches:: [[code:hiker/oplog/store]]
note:: identity is the vault path — the index keys notes by path and mints no identity of its own; a rename is a path update ([[spec:op-log-observed-move]]), content_hash short-circuits re-embed. The op-log doc id IS the path (no ULID document id, no `doc-index.db` map); the per-frame op-id stays a ULID. `doc_id_for_path` / `path_for_doc` survive as thin path-existence shims. (Renamed from `store-id-from-oplog`: "from-oplog" was a misnomer once path became the sole identity.) · evidence: `core/src/store/{mod,notes,chunks}.rs` (schema v10 dropped the `path_ids` table; `note_exists` + `find_notes_by_basename` replace `id_for_path` / `path_for_id`); op-log per-doc files keyed by path (`core/src/oplog/store.rs`), `doc-index.db` + ULID `doc_id` gone (`core/src/oplog/{meta,lifecycle,mod}.rs`)
- Schema version pragma (`PRAGMA user_version = N`) so future migrations have a hook.

**Static linking.** Both SQLite itself and the sqlite-vec extension are statically linked into the Hiker binary — `rusqlite` with the `bundled` feature compiles the SQLite C source in, and the `sqlite-vec` crate compiles the vec extension's C source in via its `cc` build-script. No system libsqlite3 dependency, no runtime extension load, no separate `vec0.so` to ship. One binary, no surprises across OS/distro versions. [store-sqlite-vec-static]
status:: done
note:: bundled rusqlite + sqlite-vec
implements:: [[code:hiker/store/register_vec_extension]]

**Open semantics (`Store::open(vault_root)`).** Creates `vault/.hiker/index.db` if missing, runs idempotent schema setup (`CREATE TABLE IF NOT EXISTS`) on a matching `user_version`, and **fails loudly on version mismatch** rather than auto-migrating — a future binary opening a mismatched db gets a clear error, not silent breakage. [store-version-fail-loud]
status:: done
note:: no auto-migrate
implements:: [[code:hiker/store/impl#[Store]open]]

**Migration policy (pre-real-use).** Until the project crosses into real-data use, no migration code is written: schema bumps are handled by deleting `.hiker/index.db` and re-indexing (the db is regenerable from content). When real-data use begins, the policy flips — every schema bump from that point ships a migration, noted in the changelog when the line is crossed.

**Writer connection ownership.** The single writer connection lives *inside* the indexer task, not behind an `Arc<Mutex<Connection>>` — only the indexer writes, and the mpsc job queue already serializes access. Read connections are fresh per call (see above), so writer/reader concurrency is handled at the SQLite WAL level, not in Rust locking.


## Store module discipline

All sqlite-vec / rusqlite usage is confined to a single `core::store` module (start as `core/src/store.rs`; split into `core/src/store/sqlite.rs` etc. if/when a second backend lands). Everything outside `store` — ingest, search, related, CLI handlers — interacts via a narrow API of plain Rust types: `upsert_note`, `delete_note`, `rename_note`, `get_note_chunks`, `knn_chunks`, returning owned structs (`ChunkHit`, `NoteRow`, ...) not driver types. [store-module-discipline]
status:: done
note:: rusqlite confined to one module
implements:: [[code:hiker/store/Store]], [[code:hiker/store/service/IndexerQueryApi]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]get_note_by_path]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]note_properties]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]related_notes]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]query_notes]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]trails_containing_note]], [[code:hiker/store/service/impl#[Store][IndexerQueryApi]boards_containing_note]]

Keeping all SQL inside one module makes swapping the store backend a one-file rewrite, not a codebase-wide grep.

Specifically forbidden outside `store`: importing `rusqlite`, returning `rusqlite::Row` or `sqlite_vec`-specific types, embedding SQL strings in handler code. The trait/struct boundary is the whole point.


## Vault tolerance (v1)

v1 is permissive about what's already in the user's vault. No "init" step rewrites files; no migration mutates content.

**Non-markdown files are silently ignored.** PDFs, images, audio, office docs, code files — all sit in the vault untouched. The indexer doesn't error, doesn't warn, doesn't produce sidecars. They aren't searchable until the extractor pipeline lands (`design.md` extractor section, v4+). A mixed-content folder gets a working v1 over its markdown subset on day one.

**Frontmatter is optional and never auto-injected.** Hiker reads `hiker:`-namespaced frontmatter if present (currently unused at v1; reserved for tags, ids, and lifecycle flags as they land), strips frontmatter before chunking either way, and tolerates its complete absence. The indexer never writes to a user's `.md` file as a side effect of opening, viewing, or indexing it. Identity is the vault path ([[spec:op-log-path-identity]]), so the index needs no id written into the file; the `hiker.id` field in frontmatter only starts being written if an explicit user action ever requires a stable in-file id — none of which exist in v1. This rule exists because users keep markdown in many tools simultaneously (vim, other markdown apps, git, mobile editors), and silently mutating their files would be a hard-to-undo trust violation.


## Chunking (v1)

Heading-bounded splits over the markdown AST, with a soft size cap.

- Parse the note with pulldown-cmark. Walk top-level blocks; treat each `H1`–`H6` as a chunk boundary. [chunker-heading-bounded]
status:: done
touches:: [[code:hiker/chunker]]
note:: pulldown-cmark walk
- Within a heading section, accumulate blocks (paragraphs, lists, code, blockquotes) into a chunk until reaching ~1200 chars; then start a new chunk *within the same heading*, carrying the heading_path forward. [chunker-soft-size-1200]
status:: done
touches:: [[code:hiker/chunker]]
- Frontmatter is stripped before chunking (separate handling later for tag/entity extraction; v1 ignores it). [chunker-frontmatter-strip]
status:: done
touches:: [[code:hiker/chunker]]
- Code blocks are kept whole — never split mid-block, even if it busts the size cap. [chunker-code-blocks-whole]
status:: done
touches:: [[code:hiker/chunker]]
- Empty notes produce zero chunks (no embedding, but the `notes` row still exists so renames/deletes are tracked).

`heading_path` is the breadcrumb (`"Section > Subsection"`) of the heading whose body the chunk falls under, or `NULL` for content above any heading. Not used for v1 ranking; stored for future use (snippet rendering, structural index). [chunker-heading-path]
status:: done
touches:: [[code:hiker/chunker]]
note:: breadcrumb stored, not yet used in ranking

`chunk_index` is contiguous and 0-based per note. The chunk id `<note_path>:<idx>` is stable as long as a note's chunk count and order don't change. Edits invalidate ids — fine for v1; agent stable-reference concerns (`design.md` MCP section) are an MCP-era problem.


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
status:: partial
note:: batching exists; the `[indexing].batch_size` config key (declared in [[spec:settings-section-indexing]]) is not yet plumbed into the indexer task — the value is loaded but not consumed

**CPU-bound boundary.** fastembed-rs is synchronous and CPU-heavy. Both model load (multi-second on first call) and `embed_batch` must run via `tokio::task::spawn_blocking` (or on a dedicated `std::thread` driven by an mpsc channel). Calling them directly inside an async context will block one of tokio's worker threads and starve other tasks. The indexer task awaits the spawn_blocking handle to keep its mpsc-driven loop async-shaped while the actual work runs on the blocking pool. [embedder-spawn-blocking]
status:: done
note:: model load + embed off async pool

**Module discipline (mirrors the store):** all fastembed-rs usage lives in `core::embed` (single `core/src/embed.rs`, splittable later). Outside the module, code calls `Embedder::embed_batch(&[String]) -> Vec<Vec<f32>>` and treats it as opaque. No fastembed types leak past the boundary. The embedder is at least as likely to be swapped as the store (cloud, a different local model, multilingual), and the trait shape is what makes future backends cheap. v1 ships fastembed-only, no fallback. [embedder-module-discipline]
status:: done
touches:: [[code:hiker/embed]]
note:: trait Embedder; no fastembed leakage; same boundary applies to `llm` crate when [[spec:embedder-llm-crate-backed]] lands

**Model storage:** downloaded model files live under the platform data dir (Linux `~/.local/share/hiker/models/`, macOS `~/Library/Application Support/hiker/models/`, Windows `%APPDATA%\hiker\models\`). Use the `directories` crate; do not roll path logic by hand. Treated as durable data rather than cache because re-downloading 30MB on a slow or metered connection is a real cost; users may still delete the directory and the app re-downloads on next launch. [embedder-platform-data-dir]
status:: done
touches:: [[code:hiker/embed]]
note:: `directories` crate

**First-run UX:** model download is non-blocking. Vault opens normally; the indexer starts but defers any embedding work until the model is on disk. Status bar / settings surface the download progress. Search/related queries return empty with a "indexing not yet ready" indicator until the first batch completes. See `settings.md` for tunables (download timing, model selection, batch size, manual reindex triggers). [embedder-first-run-nonblocking]
status:: done
note:: vault opens; embed defers until model ready

**Model load as a queue task.** First-run download and hot-swap ([[spec:embedder-hot-reload-on-model-change]]) can take seconds (cached) to minutes (cold `bge-m3` ~1.2GB). The indexer wraps every `FastembedEmbedder::load_id` in a task-queue row — `TaskKind::EmbedderModelLoad { model_id }` enqueued just before the `spawn_blocking` load, marked complete/failed on return. The row is **indeterminate** (no byte-progress — fastembed v5 exposes no download-progress callback). Queue badge counts it like any task; detail page shows model id + elapsed time; failed loads stay as a failed-task row with the error. [embedder-model-load-as-task]
status:: done
implements:: [[code:hiker/indexer/jobs/impl#[`ReloadCtx<'a>`]run]], [[code:hiker/indexer/scheduler/impl#[`LoopState<'a>`]load_and_resolve]], [[code:hiker/indexer/submit_embedder_load_task]], [[code:hiker/indexer/EmbedderLoadTaskPlumbing]], [[code:hiker/tasks/queue/impl#[Queue]submit_self_managed]], [[code:hiker/tasks/queue/impl#[Queue]tick_maintenance]], [[code:hiker/tasks/types/TaskKind#EmbedderModelLoad#model_id]], [[code:hiker/tasks/types/WorkerKind#Indexer]]
touches:: [[code:hiker/panels/queue]]
note:: `core/src/tasks.rs` (`TaskKind::EmbedderModelLoad { model_id }`, `WorkerKind::Indexer`, `Queue::submit_self_managed` — inserts a slot in `Leased` state owned by `WorkerKind::Indexer` so `checkout_*` workers never offer to take it; resolved via the existing `submit_result` / `fail` from the indexer); `core/src/indexer.rs` (`submit_embedder_load_task` helper, `EmbedderLoadTaskPlumbing` + `start_indexer_with_tasks` so the host threads its `Queue` + initial model id in, plus the resolve block around the startup `spawn_blocking` load and inside `handle_reload_embedder` — same-id short-circuit happens *before* the submit so a redundant reload doesn't create an empty row); the host constructs the queue before `start_indexer_with_tasks` and passes plumbing in; `app/src/panels/queue.rs` (kind summary handles `embedder_model_load → "Loading embedder model: <id>"`, WorkerKind extended with `indexer`). Indeterminate row (no byte-progress; fastembed v5 has no callback hook). `Priority::High` / `Shape::Direct`; submitted with a 3600s synthetic lease — `tick_maintenance` orphan-fails an expired `WorkerKind::Indexer` lease rather than requeuing it (a self-managed lease can't be re-leased by any other worker). Surfaces in the queue badge + queue detail page like every other task. Queue tile / badge stay hidden under `[llm] enabled = false` per [[spec:task-queue-respects-llm-disable]] — consistent with the rest of the queue UI; the row still exists in the queue, just isn't surfaced in that mode

### Dim-from-model and schema rebuild

The `chunk_vecs` vec0 virtual table fixes its embedding column width at `CREATE` time — `embedding float[N]` is baked into the table definition. Different models have different dims (384 / 768 / 1024), so v1's single-`const EMBED_DIM = 384` posture no longer holds. [embedder-dim-from-model]
status:: done
implements:: [[code:hiker/store/DEFAULT_EMBED_DIM]], [[code:hiker/store/Store#dim]]
touches:: [[code:hiker/embed]]
note:: `Embedder::dim()` is the source of truth at runtime; `Store::dim` mirrors it after `ensure_chunk_vecs_dim` runs. `knn_chunks_on` reads dim from on-disk meta on every call so search validates against the live dim, not a const · evidence: `core/src/store.rs` (`DEFAULT_EMBED_DIM` const + `Store::dim` field; `EMBED_DIM` removed); `core/src/embed.rs` (`Embedder::dim()` per-model from registry)
implements:: [[code:hiker/store/impl#[Store]dim]]

- `EmbedDim` (a small `usize`-newtype wrapper) is reported by the loaded `Embedder` via `Embedder::dim()`. It's the single source of truth at runtime.
- `Store::open` reads the on-disk vec0 column dim once at startup. If it matches the loaded embedder's `dim()`, nothing to do. If it differs (or the table doesn't exist yet), the store creates `chunk_vecs` with the new dim before any ingest runs. [store-rebuild-chunk-vecs-on-dim-change]
status:: done
implements:: [[code:hiker/indexer/scheduler/impl#[`LoopState<'a>`]publish_embedder_ready]], [[code:hiker/store/chunks/impl#[Store]ensure_chunk_vecs_dim]], [[code:hiker/store/vec/read_chunk_vecs_dim]]
note:: **spec deviation**: vec0 (sqlite-vec 0.1.x) doesn't surface the dim through `PRAGMA table_info(chunk_vecs)` — the embedding column is declared with an empty type slot (`vec0_init` in sqlite-vec.c). We persist the dim in a tiny `meta(key, value)` sidecar table written at `ensure_schema` / rebuild time; functionally equivalent guarantee. Added `meta` without bumping `SCHEMA_VERSION` (uses `CREATE TABLE IF NOT EXISTS`, seeds the dim row via `INSERT OR IGNORE` so pre-existing v7 dbs default to 384 until a model switch forces a rebuild) · evidence: `core/src/store.rs` (`Store::ensure_chunk_vecs_dim`, `read_chunk_vecs_dim`, new `meta` table); `core/src/indexer.rs` (calls `store.ensure_chunk_vecs_dim(embedder.dim())` immediately after embedder load, before any ingest)
- The rebuild path: `DROP TABLE chunk_vecs` → recreate with the new `float[N]` → clear `notes.note_embedding` (different-dim packed f32s are garbage at the new dim) → clear `notes.embedder_version` so the existing per-note re-embed trigger ([[spec:embedder-version-tag]]) picks them all up on the next ingest pass. Schema-version bump *not* required — the rebuild is observable through the dim mismatch itself, and the `notes` / `chunks` shapes don't change.
- Reading the on-disk dim: sqlite-vec doesn't expose the vec0 column dim through `PRAGMA table_info` (the declared type slot comes back empty). The store keeps a small `meta(key, value)` sidecar table and writes the active dim into a `chunk_vecs_dim` row whenever `chunk_vecs` is created or recreated. `Store::open` reads it from there; missing row + existing `chunk_vecs` is treated as the legacy 384 case and migrated.

Drop-and-recreate rather than ALTER: vec0 has no column-type change, and every chunk re-embeds anyway (dim-incompatible), so there's nothing worth migrating.

### Alternative embedder backends (cloud / Ollama)

The `Embedder` trait that hides fastembed-rs also hides any other backend. A second concrete impl, `core::embed::LlmEmbedder`, wraps the [`llm`](https://crates.io/crates/llm) crate's `EmbeddingProvider` trait — providing access to OpenAI, Ollama, Google, Cohere, Mistral, and HuggingFace embedding models. Same trait, same interface, indexer code doesn't change. [embedder-llm-crate-backed]
status:: planned
note:: `core::embed::LlmEmbedder` impl wrapping graniet/`llm`'s `EmbeddingProvider`; supports OpenAI / Ollama / Google / Cohere / Mistral / HuggingFace

fastembed stays the default (zero-config first run, no per-call cost, offline, no cloud egress of every chunk); cloud / Ollama backends are opt-in for quality, longer chunks, or reusing an existing Ollama runtime.

Config in `[embedder]` in user/vault TOML, same shape as `[llm]`: `provider` (`fastembed` default / `openai` / `ollama` / …), `model`, and provider-specific keys (`api_key_env`, `base_url`). API keys live in environment variables (`api_key_env` names the variable), never in TOML. Same posture as `[llm]`'s provider config. [embedder-config-section]
status:: planned
note:: `[embedder]` config: `provider`, `model`, `api_key_env`, `base_url`; user/vault scoped, same shape as `[llm]`

The existing `embedder_version` column on `notes` ([[spec:embedder-version-tag]]) keys off provider + model, not just model name — so switching provider naturally triggers re-embed via the existing fail-loud machinery. No new migration code; the schema-version contract already covers it. [embedder-version-tag-includes-provider]
status:: planned
note:: `embedder_version` column keys off provider + model so switching provider triggers re-embed via existing fail-loud machinery

**Module discipline** unchanged — `fastembed-rs` and the `llm` crate both confined to `core::embed`. The `Embedder` trait surface stays narrow; no fastembed *or* `llm` types leak past it. The same crate boundary discipline used in `core::llm` (the generative LLM module — see `llm.md`) applies here.

**ToS posture:** embeddings are always automation-shaped (every chunk on every ingest). Cloud embedding APIs (OpenAI, Cohere, Voyage, etc.) are explicitly priced for automation, so no ACP grey area applies; this is firmly a pay-per-call use case. The interactive-vs-background distinction from `llm.md` is about *generative* LLM access and doesn't carry over.


## Ingest pipeline

Triggered three ways, all funnel into the same upsert path:

1. **Startup scan** — walk vault, compare `mtime`/`size` against `notes` rows, queue any new/changed/missing. [ingest-startup-scan]
status:: done
note:: mtime/size precheck
implements:: [[code:hiker/indexer/run_full_scan]], [[code:hiker/indexer/scheduler/impl#[`LoopState<'a>`]handle_full_scan]], [[code:hiker/indexer/impl#[`ScanState<'a>`]classify_entry]], [[code:hiker/store/notes/impl#[Store]all_note_paths]]
verifies:: [[code:hiker/indexer/tests/full_scan_finds_md_files_and_skips_hiker_dir]], [[code:hiker/indexer/tests/full_scan_emits_delete_for_missing_files]]

2. **Watcher event** — single file changed/created/deleted/renamed (see `watcher.md`). [ingest-watcher-driven]
status:: done
note:: broadcast → IndexJob
3. **Manual** — `hiker reindex [path]` CLI subcommand and a future "reindex" UI button. [ingest-manual-cli]
status:: done
note:: manual reindex surface is the `index` command — wired into the tree-actions menu's Reindex-all / Reindex-this-file entries ([[spec:reindex-all-action]], [[spec:reindex-current-file-action]]). The `hiker reindex` CLI binding rides under the planned [[spec:cli-reindex]] slug in the CLI section · evidence: `core/src/indexer.rs` (`IndexScope::All` / `IndexScope::Path`)

Per-file pipeline:

```
read file → compute blake3 hash → if hash matches notes.content_hash AND
                                     embedder_version matches AND force=false → no-op
         → else: parse → chunk → embed batch → BEGIN TX
                                                 compute byte-weighted mean-pool
                                                   of the new chunk embeddings →
                                                   notes.note_embedding
                                                 upsert notes row
                                                 delete old chunks + vecs for note_path
                                                 insert new chunks + vecs
                                               COMMIT
         → emit indexer-progress events
```
[ingest-tx-upsert, ingest-progress-events, cluster-note-embeddings]
implements:: [[code:hiker/indexer/jobs/impl#[`JobCtx<'a>`]handle_upsert]], [[code:hiker/indexer/jobs/handle_inline_upsert]], [[code:hiker/indexer/ProgressEvent]], [[code:hiker/store/chunks/impl#[Store]upsert_note]], [[code:hiker/store/chunks/impl#[Store]collect_weighted_chunk_embeddings]], [[code:hiker/store/vec/byte_weighted_mean_pool]]
verifies:: [[code:hiker/indexer/tests/indexer_indexes_a_markdown_file]], [[code:hiker/indexer/tests/unchanged_file_is_skipped_on_second_index]]

The note-level pool is computed and persisted in the same transaction as the chunks, so every successful upsert leaves the cached pool in sync with the chunk set the cluster pipeline consumes. Notes with no chunks leave `note_embedding` NULL.

Deletes: cascade via the FK on `chunks`; vec rows cleaned up explicitly (vec0 has no FK enforcement). [ingest-delete-cascade]
status:: done
implements:: [[code:hiker/store/notes/impl#[Store]delete_note_by_path]]
note:: chunks + vec rows cascade explicitly; `path_ids` cleanup retired with the table · evidence: `core/src/store/notes.rs::delete_note` / `delete_note_by_path`
implements:: [[code:hiker/store/notes/impl#[Store]delete_notes_by_paths]]
verifies:: [[code:hiker/indexer/tests/deleting_a_note_removes_it_from_the_index]], [[code:hiker/indexer/tests/missing_file_during_upsert_is_treated_as_delete]]

Renames: a rename is a path update, since identity is the path ([[spec:op-log-path-identity]]). `rename_note` rewrites `notes.path` (and the derived `chunks` rows) for an observed content-preserving move ([[spec:op-log-observed-move]]); `content_hash` is unchanged, so no chunk is re-embedded. Detection follows op-log: the watcher's rename event when available, otherwise inferred from a delete + create within a small window with matching content hash. When events arrive non-paired and similarity can't bridge it, the index simply drops the old path and indexes the new one — no identity is lost because there was none to preserve, only the cached embeddings are recomputed. [ingest-rename-preserve-id]
status:: done
touches:: [[code:hiker/vault]]
note:: path is identity in op-log ([[spec:op-log-path-identity]]), so the index rename is a path-keyed `UPDATE notes SET path`; content_hash short-circuits any re-embed · evidence: `core/src/indexer/jobs.rs::handle_rename` (`store.rename_note_by_path`); `core/src/vault.rs::move_note` (same path-by-path call)
implements:: [[code:hiker/store/notes/impl#[Store]rename_notes_by_paths]]
verifies:: [[code:hiker/indexer/tests/renaming_preserves_id]]

Concurrency:

- Indexer owns a single tokio task with an mpsc channel of `IndexJob` messages. Watcher and command handlers send jobs; the task drains them serially.
- Serial processing is fine for v1 — embedding throughput is the bottleneck and batching across files is a v2 optimization.
- The indexer task holds the only sqlite connection for writes. Reads (search, related) open a fresh connection per call and drop it on return — sub-millisecond cost on a warm cache, zero shared-state concerns. Database is opened in WAL mode (`PRAGMA journal_mode = WAL`) so readers don't block on the writer's transactions. If the sqlite-vec extension load turns out to be slow per-open, switch to an `r2d2` reader pool — one-day change since all SQL lives in `core::store`. [store-wal-mode]
status:: done
note:: `pragma_update(None, "journal_mode", "WAL")` plus `synchronous=NORMAL`
implements:: [[code:hiker/store/configure_connection]], [[code:hiker/store/impl#[Store]open_reader]]

### Indexer task loop [indexer-task-loop]

`start` spawns the single long-lived tokio task that owns the writer `Store`, the loaded `Arc<dyn Embedder>`, and the `IndexJob` mpsc inbox; callers hold a `Handle` for job submission and status/progress subscriptions. The task first drives the (slow, fallible) embedder load on `spawn_blocking`, then loops on `rx.recv()` and dispatches each `IndexJob` serially through the job handlers. A model-load failure makes the task drain remaining jobs as errors instead of processing them.
implements:: [[code:hiker/indexer/start]], [[code:hiker/indexer/scheduler/impl#[`IndexerLoop<F>`]run]], [[code:hiker/indexer/IndexJob]], [[code:hiker/indexer/Handle]]


## Related-notes query (v1)

The v1 panel: open a note, list the top N notes whose chunks are most similar.

Algorithm:

1. Look up the note by path. If it has no chunks (empty or unindexed), panel is empty.
2. Fetch all of this note's chunk embeddings.
3. For each one, KNN search the `chunk_vecs` table for top 20 nearest *excluding* chunks belonging to the same note.
4. Group hits by their `note_path`; score each candidate note as `max(similarity)` across its hit chunks.
5. Return top 10 notes by score, with: title (filename stem until frontmatter parsing lands), path, score, best-matching chunk's `heading_path` and a short snippet. [related-notes-query, related-notes-snippet]

No reranking, no rank fusion, no entity boosting — good enough to validate the pipeline; the full query pipeline (`design.md` query-pipeline section) arrives in v2 alongside lexical search.

The full algorithm lives behind a single `Store::related_notes(source_path, top_k) -> Vec<RelatedHit>` method — keeping the per-chunk KNN loop, exclude-source-note filter, and group-by-note aggregation inside the store module preserves the SQL-stays-in-one-place discipline. Callers (host command handlers, MCP later) hand it a note path and receive note-shaped hits.

Latency budget: the panel updates on file-open and on save (debounced 500ms). Brute-force KNN over a 100k-chunk vault should be <100ms; if it isn't, that's the signal to add an ANN index, not before.


## Structured metadata index

A queryable index over each note's frontmatter, backing *structured* retrieval — "notes tagged `project` with `status: active`, newest first" — distinct from the semantic / lexical content indexes. Powers [[spec:search-tag-scope]].

- **`note_meta` table.** The note's frontmatter flattened to `(note_path, key, value, num)` rows: nested maps use dotted keys (`hiker.author`), list elements explode to one row each (`tags: [a, b]` → two rows), null values are skipped. `num` mirrors `value` for YAML numbers / bools / ISO dates (epoch seconds via `iso_date_epoch`) so range filters and numeric ordering need no parse at query time. Re-derived from frontmatter on every ingest (mirrors `trail_waypoints`), cleared on skip / delete. Entries are capped per note to bound pathological frontmatter. [store-note-metadata-index]
status:: done
implements:: [[code:hiker/frontmatter/flatten]], [[code:hiker/indexer/jobs/process_upsert]], [[code:hiker/store/dto/MetaEntry]], [[code:hiker/store/metadata/impl#[Store]replace_note_metadata]], [[code:hiker/store/metadata/impl#[Store]delete_note_metadata]]
touches:: [[code:hiker/store/metadata]]
note:: Derived per-note frontmatter index: dotted keys, list elements one-per-row, `num` numeric mirror. Re-derived on ingest like `trail_waypoints`; cleared on skip/delete. Backs [[spec:store-note-query]] + [[spec:search-tag-scope]] · evidence: `core/src/store/mod.rs` (`note_meta` table + indexes, `SCHEMA_VERSION = 12`); `core/src/store/metadata.rs` (`replace_note_metadata` / `delete_note_metadata` / `notes_with_meta` — the latter projects `hiker.parent`/`author`/`provenance`/`kind` for the Vault-view lens); `core/src/frontmatter.rs` (`flatten`); `core/src/indexer/jobs.rs` (`note_metadata_entries` + both ingest branches); `core/src/store/notes.rs` (skip/delete cleanup)
implements:: [[code:hiker/indexer/jobs/note_metadata_entries]]
- **`query_notes(NoteQuery)`.** Structured query: AND-ed filters (`Equals` (value-list: any-of) / `Exists` / `NumRange` / `Board` (board_cards membership) + `path_glob`), a `folder` subtree restriction, `order` (mtime / path / a meta key's numeric or text value), `limit`, and a `select` projection that packs chosen keys into each row's `fields`. Each filter compiles to an EXISTS subquery against `note_meta`; every user-supplied string is a bound parameter, never interpolated. Skipped notes are excluded. [store-note-query]
status:: done
implements:: [[code:hiker/store/dto/MetaFilter]], [[code:hiker/store/dto/OrderDir]], [[code:hiker/store/dto/NoteOrder]], [[code:hiker/store/dto/NoteQuery]], [[code:hiker/store/dto/NoteQueryRow]], [[code:hiker/store/metadata/impl#[Store]query_notes]]
note:: Structured query over `note_meta`: AND-ed Equals/Exists/NumRange filters + folder subtree + order (mtime/path/meta key) + limit + select projection. Each filter compiles to an EXISTS subquery, all user strings bound. · evidence: `core/src/store/metadata.rs` (`query_notes`); DTOs in `core/src/store/dto.rs` (`NoteQuery` / `MetaFilter` / `NoteOrder` / `OrderDir` / `NoteQueryRow`)
implements:: [[code:hiker/store/metadata/impl#[Store]fill_selected_fields]]

Tags ride this index as `Equals { key: "tags", value: "<tag>" }` — there is no separate tag table; list-valued frontmatter is simply multiple rows under one key. Lifecycle (`hiker.archived` …), authorship (`hiker.author`), and source type (`hiker.type`) are likewise plain frontmatter keys, so the lifecycle / authorship / source-type filters from `design.md` fall out of the same query surface with no new structure.

Schema bumps to v8 (the `note_meta` table); handled per the [[spec:store-version-fail-loud]] migration policy.


## Spec-anchor index

A derived index of spec-slug anchors — every bare `[slug]` token a note defines — so `[[spec:slug]]` wikilinks (`wikilinks.md` [[spec:wikilink-spec-links]]) resolve with one indexed store lookup instead of walking the vault per click.

- **`spec_anchors` table.** `(slug, note_path)` rows, primary key on the pair (a slug anchored twice in one note collapses to one row); a path index serves the lifecycle sweeps. Re-derived from content on every ingest — deliberately *before* the unchanged short-circuit: the derive is a pure text scan (no embedding, microseconds per note), and running it unconditionally lets dbs created before this table backfill on their next full scan (the table ships `CREATE TABLE IF NOT EXISTS`, no schema-version bump). Cleared on skip / delete; re-keyed on rename — the `note_meta` lifecycle exactly. The token rule (`scan_spec_anchors`: kebab-case bracket token, at least one dash, outside fenced code, not a markdown-link label) matches what the spec engine's reconcile recognizes as an anchor, so the index and the spec tooling agree on what an anchor is. [spec-anchor-index]
status:: done
implements:: [[code:hiker/wikilink/scan_spec_anchors]], [[code:hiker/store/spec_anchors/impl#[Store]replace_spec_anchors]], [[code:hiker/store/spec_anchors/impl#[Store]spec_anchor_paths]]
verifies:: [[code:hiker/store/tests/spec_anchors_replace_query_and_lifecycle]]
touches:: [[code:hiker/store/spec_anchors]]


## Command surface (v1 additions)

Existing v0 commands stay unchanged (`open_vault`, `list_dir`, `read_file_with_hash`, `write_file_checked`). v1 adds three:

- `related_notes(path: String) -> Vec<RelatedHit>` — runs the related-notes query above. Empty vec for unindexed or empty notes; never errors on absence. [cmd-related-notes]
status:: done
note:: evidence: `core/src/store/search.rs` (`related_notes`)
- `index_status() -> IndexStatus` — indexer-state snapshot (`{ model_ready, queued, total_notes, last_error }`) for the status bar / settings UI. `queued` is the mpsc-channel depth, ~1 during a `FullScan` (that handler processes per-file Upserts inline). The indexer-detail panel instead surfaces work-remaining via `IndexerHandle::pending_count()` — the in-flight `pending` paths set, pre-populated with every Upsert path at FullScan start — so the user sees a count down from N to 0. [cmd-index-status, indexer-detail-pending-counter]
implements:: [[code:hiker/indexer/IndexStatus]], [[code:hiker/indexer/impl#[Handle]pending_count]], [[code:hiker/indexer/impl#[Handle]pending_paths]], [[code:hiker/indexer/impl#[Handle]is_pending]]
- `index(scope: IndexScope) -> ()` — enqueue index jobs. `IndexScope::All` triggers a full rescan; `IndexScope::Path(rel)` re-indexes a single file. Same command covers first-time and re-indexing (only difference is whether rows existed before). Returns immediately; progress via indexer-progress events. [cmd-index]
status:: done
note:: All / Path scopes
- `chunks_for(path: String) -> Vec<ChunkBounds>` — ordered chunk bounds (`{ chunk_index, byte_start, byte_end, heading_path: Option<String> }`) for the note at `path`. Empty vec for unindexed/empty notes; never errors on absence. Backs the chunk-boundary view ([[spec:view-show-chunk-boundaries]] in `editor.md`). [cmd-chunks-for-path]
status:: done
implements:: [[code:hiker/store/chunks/impl#[Store]chunk_bounds_for]]
note:: empty vec for unindexed / never-indexed paths; SELECT omits chunk text so the wire payload stays small · evidence: `core/src/store.rs` (`ChunkBounds`, `Store::chunk_bounds_for`)
implements:: [[code:hiker/store/dto/enrich_char_offsets]]

`RelatedHit` (note-level): `path`, `title` (filename stem until frontmatter parsing lands), `score`, `best_heading_path: Option<String>`, `snippet` (text from the highest-scoring chunk).

DTOs live in `core::dto` and are auto-exported to TS via `ts-rs` (per `design.md`'s DTO/ts-rs convention).


## Per-file index state

The `notes` row already answers "is this file indexed" — presence + non-zero chunks = yes. v1 expands the surface so the UI can also explain *why not* when the answer is no, and render a distinct tree-row marker for each case (see [[spec:tree-row-unsupported-marker]] / [[spec:tree-row-skipped-marker]] / [[spec:tree-row-queued-marker]] in `files.md`). Three non-indexed states:

- **Unsupported** — the extension has no chunker (`is_indexable_path` returns false). Derivable client-side from the path; no store row required. The indexer never sees the file.
- **Skipped** — a chunker exists but ingest refused: file exceeded the 5MB sanity cap, failed UTF-8 decode, or (future) hit a corrupted-source signal. The indexer records the attempt as a `notes` row with a `skipped` flag set and a short `skip_reason` string. Storing the row (rather than dropping silently) is what lets the UI distinguish "skipped on purpose" from "never seen."
- **Queued** — the file is in the indexer's mpsc queue or actively processing. Transient; not stored — exposed via indexer-progress events.

Schema addition (bumps to `user_version = 2`): `notes.skipped` (BOOLEAN, default 0) and `notes.skip_reason` (TEXT, NULL when not skipped). Handled per the [[spec:store-version-fail-loud]] migration policy.

Surface:

- `index_state_for(path: String) -> IndexState` command. Returns `Indexed`, `Unsupported`, `Skipped { reason: String }`, or `Queued`. One path lookup; cheap enough for the tree to call lazily on render of visible rows. The skip reason is a stable, short, human-readable string (`"file too large"`, `"not UTF-8"`) used directly in tooltips and the status bar — no translation layer. [cmd-file-index-state]
status:: done
note:: Unsupported via `is_indexable_path`; Queued from indexer's pending-paths set; Skipped + Indexed from `notes` row. Schema bumped to v2 to add `notes.skipped` + `notes.skip_reason` ([[spec:store-schema-v1]] row covers the v1 baseline; the v2 columns + persistence ride on this slug). Indexer now persists Skipped rows for "file too large" and "not UTF-8" branches in `process_upsert`; `Store::upsert_skipped` handles the row + chunk cleanup · evidence: `core/src/indexer.rs` (`IndexerHandle::is_pending`)
implements:: [[code:hiker/indexer/is_indexable_path]], [[code:hiker/indexer/path_extension]], [[code:hiker/store/notes/impl#[Store]get_note_by_path]], [[code:hiker/store/notes/impl#[Store]list_skipped_paths]], [[code:hiker/store/notes/impl#[Store]upsert_skipped]]
verifies:: [[code:hiker/indexer/tests/unsupported_extensions_are_skipped]]
- indexer-progress events (per [[spec:ingest-progress-events]]) carry per-file transitions, so the tree flips rows from Queued → Indexed (or Skipped) without polling.

Indexer logic: when ingest decides to skip a file, write the `notes` row with `skipped = 1` and a reason; do not chunk, do not embed. A subsequent successful re-ingest of the same path clears the flag. Deletes cascade as before ([[spec:ingest-delete-cascade]]).


## Reindex verbs

[[spec:cmd-index]] covers the mechanics; v1 wires two UI verbs to it through the sidebar's `⋯` actions menu in Files mode (see `files.md`'s [[spec:sidebar-toolbar-actions-menu]]):

- **Reindex all** — `index(IndexScope::All)` with the `force` flag set: bypasses the content-hash + embedder-version short-circuit so every note re-embeds even when nothing changed. The button is the user's explicit "redo all the work" verb; without `force` the click would be a no-op on a clean vault. The first-launch / vault-open startup scan and watcher-driven Upserts still default to `force=false` so the cheap-when-nothing-changed path keeps applying to ambient ingest. [reindex-all-action]
status:: done
touches:: [[code:hiker/bootstrap]], [[code:hiker/panels/indexer_detail]]
note:: `app/src/panels/indexer_detail.rs` ("Reindex everything" button → `IndexerHandle::full_scan(true)`); `core/src/indexer/mod.rs::full_scan(force: bool)` enqueues `FullScan { force }`; `run_full_scan` propagates `force` into every per-file `Upsert`, bypassing the content-hash + embedder-version short-circuit in `process_upsert`. The startup scan in `app/src/bootstrap.rs` keeps `force = false` so first-launch ingest skips work for unchanged notes
verifies:: [[code:hiker/indexer/tests/force_reindex_bypasses_unchanged_short_circuit]]
- **Reindex this file** — `index(IndexScope::Path(currentPath))` with `force=true` for the same reason. Greyed when no file is active. [reindex-current-file-action]
status:: done
note:: `app/src/sidebar/files.rs` ("Reindex this file" context-menu) sends `IndexJob::Upsert { rel_path, force: true }` directly on the indexer job sender

A third verb, **Reindex (rebuild)** — drops + recreates the schema, then a full reindex — is deferred to the settings page per `settings.md`'s indexing section. The CLI counterpart [[spec:cli-reindex-rebuild]] covers the operational case in the meantime, so v1 ships without the in-app rebuild button. [reindex-rebuild-action]
status:: planned
note:: destructive UI rebuild (drop + recreate schema then reindex); deferred to settings page per [[spec:settings-section-indexing]]


## Walker symlink handling

The startup-scan walker (`walkdir` or `ignore` crate) must match the watcher's symlink policy: **don't follow symlinks** in v1. Both walker and watcher will pick this up consistently; the goal is no surprise where the indexer sees content the watcher can't notify on, or vice versa. External-path ingestion in v2+ revisits this with an explicit allowlist. [walker-symlink-policy]
status:: done
touches:: [[code:hiker/vault]]
note:: every `walkdir::WalkDir` call uses `.follow_links(false)` · evidence: `core/src/vault.rs:163`, `core/src/indexer.rs:792`, `core/src/trash.rs:159`
implements:: [[code:hiker/indexer/impl#[`ScanState<'a>`]walk_vault]]


## Failure modes

- Corrupt `index.db` — delete it, full rescan rebuilds. Provide a `hiker reindex --rebuild` command that drops and recreates the schema.
- Embedder model download fails on first run — surface the error, don't silently disable indexing. User can retry.
- File grows past a sanity cap (say 5MB of markdown) — log and skip; don't OOM the embedder. Likely an accidentally-committed binary.
- `.hiker/index.db` itself: never indexed (skip the `.hiker/` prefix in the walker — see watcher.md).


## Out of scope for v1 (lands later)

- FTS5 lexical index and hybrid query fusion (v2)
- MCP exposure of search/related (v3 per `design.md`'s MCP section)
- Chunk stability under edits (chunk_id needs to survive small edits before MCP agents pin to them — needs a content-addressed scheme)
- Entity / tag / structural / temporal / provenance indexes
- Curated-tree placement and the reconcile flow
- External-file ingestion and source-derived notes
- Reindex throttling / priority (currently strict FIFO)
- Multi-vault routing (the route → recall → fuse pipeline assumes vault/folder embeddings that don't exist yet)

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **op-log-bootstraps-first** — op-log seeds documents (by path) on first open, before the indexer starts; the indexer's `JobCtx` reads the seeded substrate so every ingest resolves by path [op-log-bootstraps-first]
  status:: done
  touches:: [[code:hiker/bootstrap]]
  note:: evidence: `app/src/bootstrap.rs::open_and_seed_oplog` (called before `start_indexer`); `core/src/indexer/jobs.rs` (`UpsertCtx.oplog_cell` read in every upsert path)
- **embedder-fastembed-bge-small** — bge-small-en-v1.5, 384 dims — the default model variant under [[spec:embedder-model-selectable]] [embedder-fastembed-bge-small]
  status:: done
  touches:: [[code:hiker/embed]]
- **embedder-fastembed-v5** — v5 deprecates `InitOptions` in favor of `TextInitOptions`; `OutputKey` is read off the model registry automatically by `TextEmbedding::try_new` (Gemma's `Some(OutputKey::ByName("sentence_embedding"))` is baked into fastembed's `MODEL_MAP`, so no explicit per-model branch needed) [embedder-fastembed-v5]
  status:: done
  touches:: [[code:hiker/embed]]
  note:: evidence: `Cargo.toml` (workspace dep `fastembed = "5"`); `core/src/embed.rs` (`TextInitOptions` constructor, `TextEmbedding` wrapped in `Mutex` because v5's `embed` takes `&mut self`)
- **embedder-model-selectable** — the three v1 models map to `BGESmallENV15` / `BGEM3` / `EmbeddingGemma300M`; settings-UI dropdown landed under [[spec:settings-embedder-model-change-warning]] [embedder-model-selectable]
  status:: done
  implements:: [[code:hiker/config/patch/ValueType#EmbedderModel]], [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/patch/ELIGIBLE_USER]]
  touches:: [[code:hiker/embed]], [[code:hiker/panels/settings]]
  note:: evidence: `core/src/embed.rs` (`KNOWN_MODELS`, `resolve_model`, `FastembedEmbedder::load_id`); `core/src/config.rs` (`ValueType::EmbedderModel`, `[indexing].model` in `ELIGIBLE_VAULT` + `ELIGIBLE_USER`, strict-load validator delegates to `is_known_model`); `app/src/panels/settings/mod.rs` (embedder-model `enum_combo()`)
- **embedder-version-per-model** — switching `[indexing].model` flips the loader's model id, which becomes the `notes.embedder_version` stamp on the next ingest — the existing [[spec:embedder-version-tag]] re-embed trigger picks every row up automatically. [embedder-version-per-model]
  status:: done
  touches:: [[code:hiker/embed]]
  note:: evidence: `core/src/embed.rs` (`FastembedEmbedder::version()` returns `&self.model_id` verbatim, set from the loader's `model_id` arg)
- **embedder-version-tag** — embedder_version on notes row [embedder-version-tag]
  status:: done
  touches:: [[code:hiker/embed]]
- **ingest-tx-upsert** — atomic chunks+vecs [ingest-tx-upsert]
  status:: done
- **ingest-progress-events** — indexer-progress events [ingest-progress-events]
  status:: done
- **related-notes-query** — per-chunk KNN, exclude source, group by note [related-notes-query]
  status:: done
  implements:: [[code:hiker/store/search/impl#[Store]related_notes]], [[code:hiker/store/search/impl#[Store]knn_chunks]]
- **related-notes-snippet** — snippet + heading_path [related-notes-snippet]
  status:: done
- **canvas-appears-in** — "Appears in" view lists the canvases (File node), boards (note card), trails (waypoint), and cluster-trees (leaf) that reference the active note, grouped by type, each row click-to-open via `open_file`. Cached by active path (re-scans on note switch), the backlinks posture. Boards + trails use indexed derived tables; canvases + trees are on-demand scans — the trees derived-table optimization stays the planned [[spec:tree-leaves-derived-table]]. **Clicking a canvas row snaps the view to the referencing file-node** (selected, centered): `canvas::open_focused` sets a one-shot `Pane::focus_note_pending`, consumed by `render.rs::apply_pending_focus` (reuses `CanvasView::focus_node`, the `apply_follow` machinery) and takes precedence over the initial fit. Snap-to-node for cluster-trees / the vault graph is deferred — neither view has a programmatic center-on-node API yet. Test: `canvases_referencing_returns_only_canvases_with_a_matching_file_node` [canvas-appears-in]
  status:: done
  implements:: [[code:hiker/trees/types/TreeContainingHit]], [[code:hiker/trees/store/impl#[Db]trees_containing_note]], [[code:hiker/panels/canvas/Pane#focus_note_pending]], [[code:hiker/panels/canvas/render/persist_text_edit]]
  verifies:: [[code:hiker/canvas/tests/canvases_referencing_returns_only_canvases_with_a_matching_file_node]]
  touches:: [[code:hiker/appears_in]], [[code:hiker/context]]
  note:: evidence: `app/src/appears_in/mod.rs` (`AppearsInSidebar` `View` + cached `State`), third view in the `context` container (`app/src/context/mod.rs`, dispatch arm in `activity/mod.rs`, `AppState::appears_in_state`); core lookups: `core/src/canvas.rs::canvases_referencing` (scan) + `core/src/trees/store.rs::trees_containing_note` (scan, returns `TreeContainingHit`) + existing `Store::boards_containing_note` / `trails_containing_note`; section titles title-cased in `workbench_host.rs::side_bar_title`
- **cmd-index-status** — (imported without notes — spec text TBD) [cmd-index-status]
  status:: done
- **note-id-stamping** — obsolete under path-as-identity. `IdStampingMode` enum + `IndexingConfig.id_stamping` field + `ensure_note_id_stamped` helper + `IdStamper` + `frontmatter_hiker_id` removed in slice 1. Notes carry no `hiker.id` frontmatter; trail-docs and board-docs likewise dropped their `hiker.id` (identity is the vault path per [[spec:op-log-path-identity]]) [note-id-stamping]
  status:: retired
- **indexer-detail-pending-counter** — `app/src/panels/indexer_detail.rs` (Pending row in the status grid reads `IndexerHandle::pending_count()`); `core/src/indexer/mod.rs::pending_count` returns the size of the in-flight `pending` paths set; `core/src/indexer/scheduler.rs` FullScan handler pre-populates the set with every Upsert path before draining so the count counts down from N to 0 across the scan. `IndexStatus.queued` (mpsc depth) still exists for callers that want the channel-level number [indexer-detail-pending-counter]
  status:: done
  touches:: [[code:hiker/panels/indexer_detail]]
