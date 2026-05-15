# Index

The v1 indexer: watcher → chunker → embedder → store, plus the "related notes" query that exposes it. design.md sketches the full multi-axis index model; this doc nails down the concrete v1 slice. Lexical search, structural/temporal/entity/provenance indexes, MCP exposure, and curated-tree placement are out of scope here — they layer on later.

v1 goal: open a note, see a panel listing semantically related notes from the same vault. Nothing else. The store, the chunking, and the embedding pipeline all need to exist for that to work, but they should be the simplest version that supports the goal without painting v2+ into a corner.


## Store: sqlite-vec

Single SQLite database at `vault/.hiker/index.db`. Vectors via the `sqlite-vec` extension, FTS via SQLite's built-in FTS5 (used in v2; schema reserved here so v1 doesn't migrate). One file per vault, regenerable from content — never the source of truth, only a cache. [store-schema-v1]

Why sqlite-vec over LanceDB for v1: one transaction across vectors + future FTS, single-file backup, brute-force search is fine at this scale (10k–500k chunks), small dep footprint. Revisit if a vault ever pushes past ~500k chunks or needs ANN.

Schema (initial):

```sql
-- one row per indexed file
CREATE TABLE notes (
  id            TEXT PRIMARY KEY,           -- ulid; stable across renames via path table
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

-- vec0 virtual table; one row per chunk
CREATE VIRTUAL TABLE chunk_vecs USING vec0(
  chunk_id      TEXT PRIMARY KEY,
  embedding     FLOAT[384]                  -- dimension fixed by chosen model
);

-- ulid stability across renames: path → id
CREATE TABLE path_ids (
  path          TEXT PRIMARY KEY,
  id            TEXT NOT NULL
);
```

Notes:

- `chunks.text` is duplicated from the source file for snippet rendering and to decouple the index from filesystem reads on every query. Cheap; vault sizes don't justify cleverness here.
- `path_ids` is the rename-stability table. On a rename event we look up the old path's id, update `notes.path`, leave the id alone. Trails and links never break.
- Schema version pragma (`PRAGMA user_version = 1`) so future migrations have a hook.

**Static linking.** Both SQLite itself and the sqlite-vec extension are statically linked into the Hiker binary — `rusqlite` with the `bundled` feature compiles the SQLite C source in, and the `sqlite-vec` crate compiles the vec extension's C source in via its `cc` build-script. No system libsqlite3 dependency, no runtime extension load, no separate `vec0.so` to ship. One binary, no surprises across OS/distro versions. [store-sqlite-vec-static]

**Open semantics (`Store::open(vault_root)`).** Creates `vault/.hiker/index.db` if missing, runs idempotent schema setup (every `CREATE TABLE` uses `IF NOT EXISTS`) on a matching `user_version`, and **fails loudly on version mismatch** rather than auto-migrating. v1 ships at version 1; if a future binary opens a v2 db (or vice-versa), the user gets a clear error rather than silent breakage. [store-version-fail-loud]

**Migration policy (pre-real-use).** Until the project crosses into "I'm storing my actual notes in this" territory, no migration code is written. Schema bumps are handled by deleting `.hiker/index.db` and re-indexing — the db is regenerable from content, so the cost is one rescan. Saves a lot of effort writing migrations against a schema that's still moving. When real-data use begins, the policy flips: every schema bump from that point ships a migration alongside it. Worth a one-line note in the changelog when that line is crossed.

**Writer connection ownership.** The single writer connection lives *inside* the indexer task, not behind an `Arc<Mutex<Connection>>`. Only the indexer task ever writes; the design's mpsc job queue already serializes access. No mutex needed, no shared state to reason about. Read connections are entirely independent (fresh per call as described above), so writer/reader concurrency is handled at the SQLite WAL level rather than in Rust locking.


## Store module discipline

All sqlite-vec / rusqlite usage is confined to a single `core::store` module (start as `core/src/store.rs`; split into `core/src/store/sqlite.rs` etc. if/when a second backend lands). Everything outside `store` — ingest, search, related, CLI handlers — interacts via a narrow API of plain Rust types: `upsert_note`, `delete_note`, `rename_note`, `get_note_chunks`, `knn_chunks`, returning owned structs (`ChunkHit`, `NoteRow`, ...) not driver types. [store-module-discipline]

Why: the v1 store choice (sqlite-vec) is a defensible default, not a permanent commitment. LanceDB becomes interesting if vault size pushes past brute-force KNN comfort, or if Lance's native versioning starts pulling weight against the design.md versioned-sources flow. Keeping SQL inside one module makes that swap a 1–2 day rewrite of one file, not a codebase-wide grep. Cost of the discipline is near-zero now; cost of *not* having it scales with every callsite that learns to write SQL directly.

Specifically forbidden outside `store`: importing `rusqlite`, returning `rusqlite::Row` or `sqlite_vec`-specific types, embedding SQL strings in handler code. The trait/struct boundary is the whole point.


## Vault tolerance (v1)

v1 is permissive about what's already in the user's vault. No "init" step rewrites files; no migration mutates content.

**Non-markdown files are silently ignored.** PDFs, images, audio, office docs, code files — all sit in the vault untouched. The indexer doesn't error, doesn't warn, doesn't produce sidecars. They simply aren't searchable until the extractor pipeline lands per design.md:419 (v4+). Users importing an existing mixed-content folder get a working v1 over their markdown subset on day one; non-md content waits its turn.

**Frontmatter is optional and never auto-injected.** Hiker reads `hiker:`-namespaced frontmatter if present (currently unused at v1; reserved for tags, ids, and lifecycle flags as they land), strips frontmatter before chunking either way, and tolerates its complete absence. The indexer never writes to a user's `.md` file as a side effect of opening, viewing, or indexing it. The path→id table in the store is the authoritative id source in v1; the `hiker.id` field in frontmatter only starts being written when an explicit user action requires a stable id in the file itself (creating a wikilink target, pinning a trail waypoint, etc.) — none of which exist in v1. This rule exists because users keep markdown in many tools simultaneously (vim, Obsidian, git, mobile editors), and silently mutating their files would be a hard-to-undo trust violation.


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

`fastembed-rs` with `BAAI/bge-small-en-v1.5` (384 dims, ~30MB model, runs on CPU, English-tuned). Stored as `embedder_version: "bge-small-en-v1.5"` in `notes`. [embedder-fastembed-bge-small, embedder-version-tag]

Choice rationale:

- Small enough to ship in the binary or download once on first run; no GPU required.
- bge-small consistently beats MiniLM-L6 on retrieval benchmarks at similar size.
- 384 dims keeps the vec table compact (~1.5KB per chunk).
- English-only is acceptable for v1 personal use; multilingual is a swap-in later (`bge-m3`).

Bumping `embedder_version` triggers full re-embed naturally — query for stale rows, regenerate. No migration code needed.

Batching: embed in batches of 64 chunks. Run on a dedicated tokio task; never block command handlers. [embedder-batch-64]

**CPU-bound boundary.** fastembed-rs is synchronous and CPU-heavy. Both model load (multi-second on first call) and `embed_batch` must run via `tokio::task::spawn_blocking` (or on a dedicated `std::thread` driven by an mpsc channel). Calling them directly inside an async context will block one of tokio's worker threads and starve other tasks. The indexer task awaits the spawn_blocking handle to keep its mpsc-driven loop async-shaped while the actual work runs on the blocking pool. [embedder-spawn-blocking]

**Module discipline (mirrors the store):** all fastembed-rs usage lives in `core::embed` (single `core/src/embed.rs`, splittable later). Outside the module, code calls `Embedder::embed_batch(&[String]) -> Vec<Vec<f32>>` and treats it as opaque. No fastembed types leak past the boundary. Reasoning: the embedder is at least as likely to be swapped as the store — a future user might want a cloud embedder (Voyage, OpenAI), a different local model (candle + a custom checkpoint), or a multilingual model (`bge-m3`). v1 ships fastembed-only with no fallback; the trait shape is what makes future fallbacks cheap. [embedder-module-discipline]

**Model storage:** downloaded model files live under the platform data dir (Linux `~/.local/share/hiker/models/`, macOS `~/Library/Application Support/hiker/models/`, Windows `%APPDATA%\hiker\models\`). Use the `directories` crate; do not roll path logic by hand. Treated as durable data rather than cache because re-downloading 30MB on a slow or metered connection is a real cost; users may still delete the directory and the app re-downloads on next launch. [embedder-platform-data-dir]

**First-run UX:** model download is non-blocking. Vault opens normally; the indexer starts but defers any embedding work until the model is on disk. Status bar / settings surface the download progress. Search/related queries return empty with a "indexing not yet ready" indicator until the first batch completes. See `settings.md` for tunables (download timing, model selection, batch size, manual reindex triggers). [embedder-first-run-nonblocking]

### Alternative embedder backends (cloud / Ollama)

The `Embedder` trait that hides fastembed-rs also hides any other backend. A second concrete impl, `core::embed::LlmEmbedder`, wraps the [`llm`](https://crates.io/crates/llm) crate's `EmbeddingProvider` trait — providing access to OpenAI, Ollama, Google, Cohere, Mistral, and HuggingFace embedding models. Same trait, same interface, indexer code doesn't change. [embedder-llm-crate-backed]

Why offer this alongside fastembed:

- **Quality ceiling.** OpenAI `text-embedding-3-large` (3072-dim), Voyage v3, Cohere `embed-v4` etc. are noticeably better than bge-small for nuanced retrieval and non-English content.
- **Bigger chunks.** Cloud embedders accept 8k+ tokens vs. fastembed's 512 cap.
- **No model download** for users on metered connections / slow disk who already have an Ollama server or are happy paying for cloud calls.
- **Ollama specifically** lets users who already run Ollama for chat features (basic agent loop or external ACP agent — see `llm.md`) reuse the runtime for embeddings via models like `nomic-embed-text` or `mxbai-embed-large`. One runtime, multiple consumers.

Why fastembed stays the default:

- Volume — embeddings fire on every chunk on every ingest, so cloud bandwidth and per-call cost are real. A 10k-note vault is 50k embedding calls.
- Privacy — sending every chunk's content to a cloud provider is a sharper concern than occasional generative LLM use.
- Offline — fastembed works without network; cloud embedders don't (Ollama works offline if the server runs locally).
- Zero config required for first-run — fastembed downloads its own model, no API key.

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

Per-file pipeline:

```
read file → compute blake3 hash → if hash matches notes.content_hash, no-op
         → else: parse → chunk → embed batch → BEGIN TX
                                                 upsert notes row
                                                 delete old chunks + vecs for note_id
                                                 insert new chunks + vecs
                                                 update path_ids
                                               COMMIT
         → emit `hiker:reindex-progress` event
```
[ingest-tx-upsert, ingest-progress-events]

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

The full algorithm lives behind a single `Store::related_notes(source_note_id, top_k) -> Vec<RelatedHit>` method — keeping the per-chunk KNN loop, exclude-source-note filter, and group-by-note aggregation inside the store module preserves the SQL-stays-in-one-place discipline. Callers (Tauri command handlers, MCP later) hand it a note id and receive note-shaped hits.

Latency budget: the panel updates on file-open and on save (debounced 500ms). Brute-force KNN over a 100k-chunk vault should be <100ms; if it isn't, that's the signal to add an ANN index, not before.


## Tauri command surface (v1 additions)

Existing v0 commands stay unchanged (`open_vault`, `list_dir`, `read_file_with_hash`, `write_file_checked`). v1 adds three:

- `related_notes(path: String) -> Vec<RelatedHit>` — runs the related-notes query above. Empty vec for unindexed or empty notes; never errors on absence. [tauri-cmd-related-notes]
- `index_status() -> IndexStatus` — snapshot of indexer state for the status bar / settings UI. Shape: `{ model_ready: bool, queued: u32, total_notes: u32, last_error: Option<String> }`. [tauri-cmd-index-status]
- `index(scope: IndexScope) -> ()` — enqueue index jobs. `IndexScope::All` triggers a full rescan; `IndexScope::Path(rel)` re-indexes a single file. Same command covers first-time indexing and re-indexing — there's no semantic difference between them, just whether rows existed before. Returns immediately; progress comes via `hiker:reindex-progress` events. [tauri-cmd-index]
- `chunks_for(path: String) -> Vec<ChunkBounds>` — ordered chunk bounds for the note at `path`. `ChunkBounds = { chunk_index: u32, byte_start: u64, byte_end: u64, heading_path: Option<String> }`. Empty vec for unindexed or empty notes; never errors on absence. Backs the chunk-boundary view (`view-show-chunk-boundaries` in `editor.md`). [tauri-cmd-chunks-for-path]

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

The `notes` row already answers "is this file indexed" — presence + non-zero chunks = yes. v1 expands the surface so the UI can also explain *why not* when the answer is no, and render a distinct tree-row marker for each case (see `tree-row-unsupported-marker` / `tree-row-skipped-marker` / `tree-row-queued-marker` in `editor.md`). Three non-indexed states:

- **Unsupported** — the extension has no chunker (`is_indexable_path` returns false). Derivable client-side from the path; no store row required. The indexer never sees the file.
- **Skipped** — a chunker exists but ingest refused: file exceeded the 5MB sanity cap, failed UTF-8 decode, or (future) hit a corrupted-source signal. The indexer records the attempt as a `notes` row with a `skipped` flag set and a short `skip_reason` string. Storing the row (rather than dropping silently) is what lets the UI distinguish "skipped on purpose" from "never seen."
- **Queued** — the file is in the indexer's mpsc queue or actively processing. Transient; not stored — exposed via `hiker:reindex-progress` events.

Schema addition (v1 schema bumps to `user_version = 2`): `notes.skipped` (BOOLEAN, default 0) and `notes.skip_reason` (TEXT, NULL when not skipped). Per the migration policy in `store-version-fail-loud`, the bump is handled by deleting `.hiker/index.db` and re-indexing until real-data use begins.

Surface:

- `index_state_for(path: String) -> IndexState` Tauri command. Returns `Indexed`, `Unsupported`, `Skipped { reason: String }`, or `Queued`. One path lookup; cheap enough for the tree to call lazily on render of visible rows. The skip reason is a stable, short, human-readable string (`"file too large"`, `"not UTF-8"`) used directly in tooltips and the status bar — no translation layer. [tauri-cmd-file-index-state]
- `hiker:reindex-progress` events (per `ingest-progress-events`) carry per-file transitions, so the tree flips rows from Queued → Indexed (or Skipped) without polling.

Indexer logic: when ingest decides to skip a file, write the `notes` row with `skipped = 1` and a reason; do not chunk, do not embed. A subsequent successful re-ingest of the same path clears the flag. Deletes cascade as before (`ingest-delete-cascade`).


## Reindex verbs

`tauri-cmd-index` already covers the mechanics — `IndexScope::All` for full rescan and `IndexScope::Path` for one file. v1 wires two UI verbs to it through the sidebar's `⋯` actions menu in Files mode (see `editor.md`'s `sidebar-toolbar-actions-menu`):

- **Reindex all** — `index(IndexScope::All)`. No confirm modal; re-embedding identical content is non-destructive and the click is the opt-in. [reindex-all-action]
- **Reindex this file** — `index(IndexScope::Path(currentPath))`. Greyed when no file is active. [reindex-current-file-action]

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
