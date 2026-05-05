# Index

The v1 indexer: watcher → chunker → embedder → store, plus the "related notes" query that exposes it. design.md sketches the full multi-axis index model; this doc nails down the concrete v1 slice. Lexical search, structural/temporal/entity/provenance indexes, MCP exposure, and curated-tree placement are out of scope here — they layer on later.

v1 goal: open a note, see a panel listing semantically related notes from the same vault. Nothing else. The store, the chunking, and the embedding pipeline all need to exist for that to work, but they should be the simplest version that supports the goal without painting v2+ into a corner.


## Store: sqlite-vec

Single SQLite database at `vault/.hiker/index.db`. Vectors via the `sqlite-vec` extension, FTS via SQLite's built-in FTS5 (used in v2; schema reserved here so v1 doesn't migrate). One file per vault, regenerable from content — never the source of truth, only a cache.

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
  embedder_version TEXT NOT NULL            -- forces re-embed when the model changes
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


## Store module discipline

All sqlite-vec / rusqlite usage is confined to a single `core::store` module (start as `core/src/store.rs`; split into `core/src/store/sqlite.rs` etc. if/when a second backend lands). Everything outside `store` — ingest, search, related, CLI handlers — interacts via a narrow API of plain Rust types: `upsert_note`, `delete_note`, `rename_note`, `get_note_chunks`, `knn_chunks`, returning owned structs (`ChunkHit`, `NoteRow`, ...) not driver types.

Why: the v1 store choice (sqlite-vec) is a defensible default, not a permanent commitment. LanceDB becomes interesting if vault size pushes past brute-force KNN comfort, or if Lance's native versioning starts pulling weight against the design.md versioned-sources flow. Keeping SQL inside one module makes that swap a 1–2 day rewrite of one file, not a codebase-wide grep. Cost of the discipline is near-zero now; cost of *not* having it scales with every callsite that learns to write SQL directly.

Specifically forbidden outside `store`: importing `rusqlite`, returning `rusqlite::Row` or `sqlite_vec`-specific types, embedding SQL strings in handler code. The trait/struct boundary is the whole point.


## Vault tolerance (v1)

v1 is permissive about what's already in the user's vault. No "init" step rewrites files; no migration mutates content.

**Non-markdown files are silently ignored.** PDFs, images, audio, office docs, code files — all sit in the vault untouched. The indexer doesn't error, doesn't warn, doesn't produce sidecars. They simply aren't searchable until the extractor pipeline lands per design.md:419 (v4+). Users importing an existing mixed-content folder get a working v1 over their markdown subset on day one; non-md content waits its turn.

**Frontmatter is optional and never auto-injected.** Hiker reads `hiker:`-namespaced frontmatter if present (currently unused at v1; reserved for tags, ids, and lifecycle flags as they land), strips frontmatter before chunking either way, and tolerates its complete absence. The indexer never writes to a user's `.md` file as a side effect of opening, viewing, or indexing it. The path→id table in the store is the authoritative id source in v1; the `hiker.id` field in frontmatter only starts being written when an explicit user action requires a stable id in the file itself (creating a wikilink target, pinning a trail waypoint, etc.) — none of which exist in v1. This rule exists because users keep markdown in many tools simultaneously (vim, Obsidian, git, mobile editors), and silently mutating their files would be a hard-to-undo trust violation.


## Chunking (v1)

Heading-bounded splits over the markdown AST, with a soft size cap.

- Parse the note with pulldown-cmark. Walk top-level blocks; treat each `H1`–`H6` as a chunk boundary.
- Within a heading section, accumulate blocks (paragraphs, lists, code, blockquotes) into a chunk until reaching ~1200 chars; then start a new chunk *within the same heading*, carrying the heading_path forward.
- Frontmatter is stripped before chunking (separate handling later for tag/entity extraction; v1 ignores it).
- Code blocks are kept whole — never split mid-block, even if it busts the size cap.
- Empty notes produce zero chunks (no embedding, but the `notes` row still exists so renames/deletes are tracked).

`heading_path` is the breadcrumb (`"Section > Subsection"`) of the heading whose body the chunk falls under, or `NULL` for content above any heading. Not used for v1 ranking; stored for future use (snippet rendering, structural index).

`chunk_index` is contiguous and 0-based per note. The chunk id `<note_id>:<idx>` is stable as long as a note's chunk count and order don't change. Edits invalidate ids — that's fine for v1; agent stable-reference concerns from design.md:402 are an MCP-era problem.


## Embedder

`fastembed-rs` with `BAAI/bge-small-en-v1.5` (384 dims, ~30MB model, runs on CPU, English-tuned). Stored as `embedder_version: "bge-small-en-v1.5"` in `notes`.

Choice rationale:

- Small enough to ship in the binary or download once on first run; no GPU required.
- bge-small consistently beats MiniLM-L6 on retrieval benchmarks at similar size.
- 384 dims keeps the vec table compact (~1.5KB per chunk).
- English-only is acceptable for v1 personal use; multilingual is a swap-in later (`bge-m3`).

Bumping `embedder_version` triggers full re-embed naturally — query for stale rows, regenerate. No migration code needed.

Batching: embed in batches of 64 chunks. Run on a dedicated tokio task; never block command handlers.

**Module discipline (mirrors the store):** all fastembed-rs usage lives in `core::embed` (single `core/src/embed.rs`, splittable later). Outside the module, code calls `Embedder::embed_batch(&[String]) -> Vec<Vec<f32>>` and treats it as opaque. No fastembed types leak past the boundary. Reasoning: the embedder is at least as likely to be swapped as the store — a future user might want a cloud embedder (Voyage, OpenAI), a different local model (candle + a custom checkpoint), or a multilingual model (`bge-m3`). v1 ships fastembed-only with no fallback; the trait shape is what makes future fallbacks cheap.

**Model storage:** downloaded model files live under the platform data dir (Linux `~/.local/share/hiker/models/`, macOS `~/Library/Application Support/hiker/models/`, Windows `%APPDATA%\hiker\models\`). Use the `directories` crate; do not roll path logic by hand. Treated as durable data rather than cache because re-downloading 30MB on a slow or metered connection is a real cost; users may still delete the directory and the app re-downloads on next launch.

**First-run UX:** model download is non-blocking. Vault opens normally; the indexer starts but defers any embedding work until the model is on disk. Status bar / settings surface the download progress. Search/related queries return empty with a "indexing not yet ready" indicator until the first batch completes. See `settings.md` for tunables (download timing, model selection, batch size, manual reindex triggers).


## Ingest pipeline

Triggered three ways, all funnel into the same upsert path:

1. **Startup scan** — walk vault, compare `mtime`/`size` against `notes` rows, queue any new/changed/missing.
2. **Watcher event** — single file changed/created/deleted/renamed (see `watcher.md`).
3. **Manual** — `hiker reindex [path]` CLI subcommand and a future "reindex" UI button.

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

Deletes: cascade via the FK on `chunks`; vec rows cleaned up explicitly (vec0 has no FK enforcement).

Renames: detected by the watcher's rename event when available; otherwise inferred (delete + create within a small window with matching content hash → rename, preserve id). v1 starts with the simple version: rely on notify's rename events on Linux/macOS, fall back to "treat as delete + create with new ulid" on Windows or when events arrive non-paired.

Concurrency:

- Indexer owns a single tokio task with an mpsc channel of `IndexJob` messages. Watcher and command handlers send jobs; the task drains them serially.
- Serial processing is fine for v1 — embedding throughput is the bottleneck and batching across files is a v2 optimization.
- The indexer task holds the only sqlite connection for writes. Reads (search, related) use a separate read-only connection pool.


## Related-notes query (v1)

The v1 panel: open a note, list the top N notes whose chunks are most similar.

Algorithm:

1. Look up `note_id` from path. If the note has no chunks (empty or unindexed), panel is empty.
2. Fetch all of this note's chunk embeddings.
3. For each one, KNN search the `chunk_vecs` table for top 20 nearest *excluding* chunks belonging to the same note.
4. Group hits by their `note_id`; score each candidate note as `max(similarity)` across its hit chunks.
5. Return top 10 notes by score, with: title (filename stem until frontmatter parsing lands), path, score, best-matching chunk's `heading_path` and a short snippet.

This is intentionally crude. No reranking, no rank fusion, no entity boosting. Good enough to validate the pipeline; the design.md:227 query pipeline arrives in v2 alongside lexical search.

Latency budget: the panel updates on file-open and on save (debounced 500ms). Brute-force KNN over a 100k-chunk vault should be <100ms; if it isn't, that's the signal to add an ANN index, not before.


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
