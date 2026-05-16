//! Index store. SQLite + sqlite-vec, statically linked. See docs/index.md.
//!
//! All rusqlite usage is confined here; callers see only the API in this module
//! and the DTOs it returns. The writer connection is owned by whoever
//! constructs `Store` (in v1, the indexer task); read connections open fresh
//! per call via `Store::open_reader`.

use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::chunker::Chunk;
use crate::error::HikerError;

mod notes;
mod chunks;
mod search;
mod trails;

#[cfg(test)]
mod tests;

/// Bumped only when on-disk schema changes. Pre-real-use policy: a mismatch
/// is an error, not a migration trigger — delete `.hiker/index.db` and
/// re-index. See docs/index.md `store-version-fail-loud`.
///
/// v2 added `notes.skipped` + `notes.skip_reason` so the indexer can
/// distinguish "skipped on purpose" from "never seen" across launches.
///
/// v3 added the `chunks_fts` external-content FTS5 virtual table plus
/// sync triggers on `chunks`, backing the lexical search engine
/// (`search-fts5-schema`). External-content means tokens + offsets only —
/// the chunk text isn't duplicated; FTS5 reads `chunks.text` on demand.
///
/// v4 added `notes.last_accessed_at` (NULL until first user-open) so the
/// vault-home recents widget can sort by user-open time. Tracked
/// independently of `mtime` since the user may open without modifying.
///
/// v5 added the `trail_waypoints` derived index table (one row per
/// waypoint-note) for fast `trails_containing(note_id)` and
/// `waypoints_of(trail_id)` lookups. See `docs/trails.md`
/// §"Indexer integration" — `trail-waypoints-derived-table`.
///
/// v6 reshaped `trail_waypoints` to support side trails
/// (`trail-side-trail-shape`): the `seq` column is dropped in favor of
/// `parent_waypoint_id` (NULL for root) + `tree_path` (materialized
/// depth-first dotted-1-based path, e.g. `"1"`, `"1.2"`, `"1.2.1"`).
/// Lexical ordering on `tree_path` reproduces reading order without a
/// recursive query.
///
/// v7 added `notes.note_embedding BLOB` (NULL until computed; lazily
/// filled by the cluster pipeline as a byte-length-weighted mean-pool
/// of the note's chunk embeddings — see `cluster-note-embeddings` in
/// `clustering.md`). Cleared whenever the note's chunks change so it
/// stays consistent with the live chunk-vecs table.
pub const SCHEMA_VERSION: i32 = 7;

/// Default embedding dimension — matches the v1 default model
/// (`bge-small-en-v1.5`). Used as the initial `chunk_vecs` column width on
/// a brand-new vault and as the dim every test embedder reports. Per-model
/// dim is the actual source of truth at runtime (`Embedder::dim()`); the
/// store rebuilds `chunk_vecs` to match via `ensure_chunk_vecs_dim`.
///
/// status: embedder-dim-from-model
pub const DEFAULT_EMBED_DIM: usize = 384;


#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("embedding dimension mismatch: got {got}, expected {expected}")]
    EmbedDim { got: usize, expected: usize },
    #[error("note not found: {0}")]
    NotFound(String),
}

impl From<StoreError> for HikerError {
    fn from(e: StoreError) -> Self {
        HikerError::Io(e.to_string())
    }
}

/// Row-shaped DTO for a single note. Owned strings so it crosses module
/// boundaries cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    pub id: String,
    pub path: String,
    pub content_hash: String,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: String,
    /// True when the indexer attempted but refused this file (file too large,
    /// non-UTF-8, ...). The row exists so the UI can mark the file as
    /// Skipped across launches; chunks/vecs are not written for skipped rows.
    pub skipped: bool,
    /// Short, stable, human-readable reason — used directly in tooltips and
    /// the status bar (`"file too large"`, `"not UTF-8"`).
    pub skip_reason: Option<String>,
    /// Unix seconds; updated each time the note becomes the active buffer.
    /// `None` until the note is first opened. See `note-access-tracking`.
    pub last_accessed_at: Option<i64>,
}

/// One row in the chat `@`-mention autocomplete popover. Vault-relative
/// path with the indexable extension stripped (token format is
/// `@<rel-path-without-extension>`), plus the basename and parent
/// directory for two-line rendering, and `last_accessed_at` so the
/// frontend can format a recency hint if it wants.
///
/// status: chat-input-at-autocomplete-tauri-cmd
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtSuggestion {
    /// Vault-relative path with the file extension stripped — exactly
    /// what gets inserted into the chat input as `@<rel_path>`.
    pub rel_path: String,
    /// Filename minus extension; rendered as the primary label.
    pub basename: String,
    /// Containing folder, vault-relative; empty when the note sits at
    /// the vault root. Rendered as a muted hint to disambiguate notes
    /// with the same basename.
    pub parent_dir: String,
    /// Unix seconds; `None` until the note has been opened at least
    /// once. The popover orders by recency on the backend, so the UI
    /// usually doesn't need to consult this directly.
    pub last_accessed_at: Option<i64>,
}

impl AtSuggestion {
    fn from_path(path: String, last_accessed_at: Option<i64>) -> Self {
        let basename_full = basename_of(&path).to_string();
        let stem = strip_indexable_extension(&basename_full).to_string();
        let parent_dir = match path.rfind('/') {
            Some(i) => path[..i].to_string(),
            None => String::new(),
        };
        let rel_path = if parent_dir.is_empty() {
            stem.clone()
        } else {
            format!("{parent_dir}/{stem}")
        };
        AtSuggestion {
            rel_path,
            basename: stem,
            parent_dir,
            last_accessed_at,
        }
    }
}

fn basename_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Strip a known indexable extension (`.md`, `.markdown`, `.txt`) from the
/// basename. Other extensions stay intact — the autocomplete should never
/// see them since the query filters skipped + non-indexable rows by virtue
/// of the indexer's allowlist, but if one slips through we leave the
/// filename as-is rather than mangling it.
fn strip_indexable_extension(basename: &str) -> &str {
    for ext in crate::indexer::INDEXABLE_EXTENSIONS {
        let dotted = format!(".{ext}");
        if basename.len() > dotted.len()
            && basename[basename.len() - dotted.len()..]
                .eq_ignore_ascii_case(&dotted)
        {
            return &basename[..basename.len() - dotted.len()];
        }
    }
    basename
}

/// Compact note row for the vault-home recents widgets. Same shape for both
/// "recently modified" (sorted by `mtime`) and "recently accessed" (sorted by
/// `last_accessed_at`); the UI picks the relevant timestamp per widget.
///
/// status: vault-home-recent-modified
/// status: vault-home-recent-accessed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentNote {
    pub path: String,
    pub title: String,
    pub mtime: i64,
    pub last_accessed_at: Option<i64>,
}

/// Counts surfaced by the vault-home stats widget. `queued` is filled by the
/// indexer handle, not the store; the rest come straight off the notes /
/// chunks tables.
///
/// status: vault-home-stats-widget
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultStats {
    pub total_notes: u32,
    pub total_chunks: u32,
    pub indexed: u32,
    pub skipped: u32,
}

// status: note-properties-tab-content
/// Read-only snapshot of everything hiker knows about a note across
/// `index.db` and `changes.db`. Consumed by the `properties`-kind tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteProperties {
    pub path: String,
    pub note_id: Option<String>,
    pub path_ids_id: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
    pub content_hash: Option<String>,
    pub extension: Option<String>,
    pub indexed_at: Option<i64>,
    pub embedder_version: Option<String>,
    pub skipped: Option<bool>,
    pub skip_reason: Option<String>,
    pub chunk_count: Option<i64>,
    pub last_accessed_at: Option<i64>,
    pub change_count: Option<i64>,
}

/// Stored chunk metadata (without the embedding — fetch via knn_chunks for
/// scored retrieval).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub id: String,
    pub note_id: String,
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: String,
    pub heading_path: Option<String>,
}

/// Public chunk-bounds DTO returned by `chunk_bounds_for` — the wire shape
/// for the chunk-boundary editor decoration. Omits `text` and `note_id` to
/// keep the payload small; the UI only needs offsets + heading_path.
///
/// `char_start` / `char_end` are UTF-16 code-unit offsets into the source
/// note, computed in core via `enrich_char_offsets`. The chunker emits
/// byte offsets natively; the UI used to convert them via `TextEncoder`,
/// which made the UI the seam for a representation translation that
/// belongs next to the data. Both are populated here so frontend / CLI /
/// MCP all see the same shape and the conversion only ever happens once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkBounds {
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    /// UTF-16 code unit offset into the note's plain text. JS strings are
    /// UTF-16, so this is what CM6 / the editor consumes directly.
    pub char_start: u64,
    pub char_end: u64,
    pub heading_path: Option<String>,
}

/// Walk `text` once and replace `char_start` / `char_end` on every entry
/// in `bounds` with the UTF-16 offset corresponding to its byte offset.
/// Single linear pass — chunks are already sorted by `chunk_index`, but
/// the byte offsets they report aren't required to be monotonic, so we
/// build a fresh sorted index of unique byte positions and binary-search
/// each into that table. For the typical ~hundreds-of-chunks-per-note
/// shape this stays cheap.
pub fn enrich_char_offsets(text: &str, bounds: &mut [ChunkBounds]) {
    if bounds.is_empty() {
        return;
    }
    // Collect distinct byte targets we need char offsets for.
    let mut targets: Vec<u64> = bounds
        .iter()
        .flat_map(|b| [b.byte_start, b.byte_end])
        .collect();
    targets.sort_unstable();
    targets.dedup();

    // Walk the string once; for each char, record the (byte_pos, utf16_pos).
    // utf16_pos uses `encode_utf16().count()` per char so surrogate pairs
    // contribute 2.
    let mut byte_to_utf16: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let total_bytes = text.len() as u64;
    let mut byte_pos: u64 = 0;
    let mut utf16_pos: u64 = 0;
    let mut targets_iter = targets.iter().peekable();
    // Map any leading targets at position 0 first.
    while let Some(&&t) = targets_iter.peek() {
        if t == 0 {
            byte_to_utf16.insert(0, 0);
            targets_iter.next();
        } else {
            break;
        }
    }
    for ch in text.chars() {
        let ch_bytes = ch.len_utf8() as u64;
        let ch_units = ch.len_utf16() as u64;
        byte_pos += ch_bytes;
        utf16_pos += ch_units;
        while let Some(&&t) = targets_iter.peek() {
            if t <= byte_pos {
                byte_to_utf16.insert(t, utf16_pos);
                targets_iter.next();
            } else {
                break;
            }
        }
        if targets_iter.peek().is_none() {
            break;
        }
    }
    // Anything past the document end clamps to the doc-end utf16 length.
    while let Some(&t) = targets_iter.next() {
        byte_to_utf16.insert(t, utf16_pos);
        let _ = total_bytes;
    }

    for b in bounds.iter_mut() {
        b.char_start = *byte_to_utf16.get(&b.byte_start).unwrap_or(&0);
        b.char_end = *byte_to_utf16.get(&b.byte_end).unwrap_or(&b.char_start);
    }
}

/// One hit from a KNN query. `score` is similarity (higher = closer); we
/// convert from sqlite-vec's distance (L2) so callers can rank uniformly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHit {
    pub chunk_id: String,
    pub note_id: String,
    pub note_path: String,
    pub heading_path: Option<String>,
    pub text: String,
    pub score: f32,
}

/// Note-level hit for the related-notes panel. Aggregates a note's chunk
/// hits into a single row — score is the max similarity across the note's
/// matching chunks, snippet/heading come from that highest-scoring chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedHit {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub score: f32,
    pub best_heading_path: Option<String>,
    pub snippet: String,
}

/// One row in the derived `trail_waypoints` table, surfacing the link
/// between a waypoint-note and its trail/source. Populated by the indexer
/// when it ingests waypoint-notes or trail-docs.
///
/// status: trail-waypoints-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointRow {
    pub waypoint_path: String,
    pub waypoint_id: String,
    pub trail_id: String,
    /// `None` when the source note hasn't been ingested (or had its ULID
    /// stamped) yet. Filled in on a later indexing pass.
    pub source_id: Option<String>,
    pub source_path: String,
    /// `None` for root-level waypoints; otherwise the ULID of the
    /// parent waypoint.
    pub parent_waypoint_id: Option<String>,
    /// Materialized depth-first 1-based dotted path —
    /// `"1"`, `"1.2"`, `"1.2.1"`. Empty when the row was written via
    /// the per-waypoint ingest path before the parent trail-doc has
    /// been re-ingested (next trail-doc ingest fills the canonical
    /// value).
    pub tree_path: String,
}

/// Hit returned by `Store::trails_containing_note`. Holds enough to point
/// the UI at both the trail and the specific waypoint inside it.
///
/// status: trail-waypoints-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailContainingHit {
    pub trail_id: String,
    pub waypoint_path: String,
    pub waypoint_id: String,
    pub tree_path: String,
}

/// Bundle of everything needed to upsert a note in one transaction. Caller
/// (the indexer task) builds this after chunking + embedding.
pub struct NoteUpsert<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub content_hash: &'a str,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: &'a str,
    pub chunks: Vec<(Chunk, Vec<f32>)>,
}

/// Owned writer connection. Construct once per vault; live for the lifetime
/// of the indexer task.
pub struct Store {
    conn: Connection,
    db_path: PathBuf,
    /// Live embedding dimension this store enforces — initialized from the
    /// on-disk `chunk_vecs` column width (or `DEFAULT_EMBED_DIM` on a
    /// brand-new vault) and reseated by `ensure_chunk_vecs_dim` if the
    /// loaded embedder reports a different dim. Source of truth for every
    /// dim check inside this module; `DEFAULT_EMBED_DIM` is no more.
    ///
    /// status: embedder-dim-from-model
    dim: usize,
}

impl Store {
    /// Open or create the index db at `<vault_root>/.hiker/index.db`. Runs
    /// idempotent schema setup on a matching version, fails loud on mismatch.
    /// On a fresh vault the `chunk_vecs` table is created at
    /// `DEFAULT_EMBED_DIM`; the indexer task reseats it via
    /// `ensure_chunk_vecs_dim(embedder.dim())` before processing any jobs.
    pub fn open(vault_root: &Path) -> Result<Self, StoreError> {
        register_vec_extension();
        let db_path = vault_root.join(".hiker").join("index.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        configure_connection(&conn)?;
        ensure_schema(&conn, DEFAULT_EMBED_DIM)?;
        let dim = read_chunk_vecs_dim(&conn)?.unwrap_or(DEFAULT_EMBED_DIM);
        Ok(Self { conn, db_path, dim })
    }

    /// Live `chunk_vecs` embedding dimension. Equal to the loaded
    /// `Embedder::dim()` after `ensure_chunk_vecs_dim` has run.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Open a fresh read-only connection against the same db. Cheap (sub-ms on
    /// warm cache); intended for per-command read paths. Callers drop it on
    /// return.
    pub fn open_reader(&self) -> Result<Connection, StoreError> {
        register_vec_extension();
        let conn = Connection::open(&self.db_path)?;
        configure_connection(&conn)?;
        Ok(conn)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Standalone KNN against any connection (so read connections from
/// `open_reader` can reuse the implementation).
pub fn knn_chunks_on(
    conn: &Connection,
    query: &[f32],
    top_k: usize,
    exclude_note_id: Option<&str>,
) -> Result<Vec<ChunkHit>, StoreError> {
    let expected = read_chunk_vecs_dim(conn)?.unwrap_or(DEFAULT_EMBED_DIM);
    if query.len() != expected {
        return Err(StoreError::EmbedDim {
            got: query.len(),
            expected,
        });
    }
    // Pull a wider candidate set than top_k since we filter post-hoc; vec0
    // doesn't support arbitrary WHERE filters in the MATCH clause.
    let candidate_k = top_k.saturating_mul(4).max(top_k + 16);
    let mut stmt = conn.prepare(
        "SELECT v.chunk_id, c.note_id, n.path, c.heading_path, c.text, v.distance
         FROM chunk_vecs v
         JOIN chunks c ON c.id = v.chunk_id
         JOIN notes n ON n.id = c.note_id
         WHERE v.embedding MATCH ?1 AND k = ?2
         ORDER BY v.distance",
    )?;
    let blob = embedding_to_blob(query);
    let rows = stmt
        .query_map(params![blob, candidate_k as i64], |row| {
            let distance: f32 = row.get(5)?;
            Ok(ChunkHit {
                chunk_id: row.get(0)?,
                note_id: row.get(1)?,
                note_path: row.get(2)?,
                heading_path: row.get(3)?,
                text: row.get(4)?,
                // sqlite-vec returns L2 distance; convert to a similarity-ish
                // score so higher = closer. 1 / (1 + d) is bounded (0, 1].
                score: 1.0 / (1.0 + distance),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let filtered: Vec<ChunkHit> = match exclude_note_id {
        Some(id) => rows.into_iter().filter(|h| h.note_id != id).collect(),
        None => rows,
    };
    Ok(filtered.into_iter().take(top_k).collect())
}

/// Generate a fresh ulid as a string. Used by the indexer when assigning ids
/// to newly-seen notes.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Extract a display title from a vault-relative path. Filename without
/// extension; "Untitled" for an empty stem.
fn title_from_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let stem = last.strip_suffix(".md").unwrap_or(last);
    if stem.is_empty() {
        "Untitled".into()
    } else {
        stem.to_string()
    }
}

/// Trim and clip a chunk text into a short snippet for the panel. ~200 chars,
/// collapsing whitespace.
fn snippet_from(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= 200 {
        collapsed
    } else {
        // Char-boundary safe truncation.
        let cutoff = collapsed
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
        format!("{}…", &collapsed[..cutoff])
    }
}

fn embedding_to_blob(emb: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(emb.len() * 4);
    for f in emb {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Inverse of `embedding_to_blob`. Trailing partial-f32 bytes (impossible
/// on a well-formed blob) are dropped.
fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().expect("chunks_exact yields 4 bytes");
        out.push(f32::from_le_bytes(arr));
    }
    out
}

/// Byte-length-weighted mean-pool over a note's chunk embeddings. Empty
/// input is rejected; zero total weight (every chunk reported zero-length,
/// which shouldn't happen but is cheap to guard against) falls back to an
/// unweighted mean. See `cluster-note-embeddings` in `clustering.md`.
fn byte_weighted_mean_pool(
    weighted: &[(Vec<f32>, u64)],
    expected_dim: usize,
) -> Result<Vec<f32>, StoreError> {
    debug_assert!(!weighted.is_empty());
    let dim = weighted[0].0.len();
    if dim != expected_dim {
        return Err(StoreError::EmbedDim {
            got: dim,
            expected: expected_dim,
        });
    }
    let mut acc = vec![0.0f32; dim];
    let mut total: u64 = 0;
    for (vec, w) in weighted {
        if vec.len() != dim {
            return Err(StoreError::EmbedDim {
                got: vec.len(),
                expected: dim,
            });
        }
        // Treat a zero-length chunk as weight 1 so it still contributes —
        // dropping it would silently make empty-headed chunks invisible to
        // the pool.
        let effective = (*w).max(1);
        total = total.saturating_add(effective);
        let wf = effective as f32;
        for (a, &v) in acc.iter_mut().zip(vec.iter()) {
            *a += v * wf;
        }
    }
    let denom = (total.max(1)) as f32;
    for a in acc.iter_mut() {
        *a /= denom;
    }
    Ok(acc)
}

fn map_note_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        id: row.get(0)?,
        path: row.get(1)?,
        content_hash: row.get(2)?,
        mtime: row.get(3)?,
        size: row.get(4)?,
        indexed_at: row.get(5)?,
        embedder_version: row.get(6)?,
        skipped: row.get::<_, i64>(7)? != 0,
        skip_reason: row.get(8)?,
        last_accessed_at: row.get(9)?,
    })
}

fn map_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        note_id: row.get(1)?,
        chunk_index: row.get::<_, i64>(2)? as u32,
        byte_start: row.get::<_, i64>(3)? as u64,
        byte_end: row.get::<_, i64>(4)? as u64,
        text: row.get(5)?,
        heading_path: row.get(6)?,
    })
}

fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    // WAL mode lets readers run concurrently with the writer's transactions.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // FK enforcement is off by default in SQLite; we rely on the cascade.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Sensible durability: WAL with synchronous=NORMAL is safe and faster
    // than the FULL default.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Move/rename ops open a short-lived writer that may briefly contend
    // with the indexer's owned writer. busy_timeout makes the second writer
    // wait rather than fail loudly with SQLITE_BUSY.
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

/// Read the on-disk `chunk_vecs` embedding column dim. Returns `None` when
/// the dim hasn't been recorded yet (the meta row is missing — fresh db
/// state before the first `ensure_schema` run).
///
/// Implementation note: `PRAGMA table_info(chunk_vecs)` does *not* report
/// the bracketed `float[N]` type the spec describes — sqlite-vec
/// `declare_vtab`s the embedding column with an empty type slot (see
/// sqlite-vec 0.1.x `vec0_init`, which builds `CREATE TABLE x(...
/// "embedding", distance hidden, k hidden)`). So we persist the dim
/// ourselves in a tiny `meta` key/value table at `ensure_schema` /
/// rebuild time, and read it back here. The PRAGMA-based approach in the
/// original spec would only work against a vec0 release that surfaces
/// the dim in `table_info`; this is the equivalent guarantee until then.
///
/// status: store-rebuild-chunk-vecs-on-dim-change
fn read_chunk_vecs_dim(conn: &Connection) -> Result<Option<usize>, StoreError> {
    let row: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'chunk_vecs_dim'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.and_then(|s| s.parse().ok()))
}


fn ensure_schema(conn: &Connection, dim: usize) -> Result<(), StoreError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(StoreError::VersionMismatch {
            found: user_version,
            expected: SCHEMA_VERSION,
        });
    }

    conn.execute_batch(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS notes (
            id               TEXT PRIMARY KEY,
            path             TEXT NOT NULL UNIQUE,
            content_hash     TEXT NOT NULL,
            mtime            INTEGER NOT NULL,
            size             INTEGER NOT NULL,
            indexed_at       INTEGER NOT NULL,
            embedder_version TEXT NOT NULL,
            skipped          INTEGER NOT NULL DEFAULT 0,
            skip_reason      TEXT,
            last_accessed_at INTEGER,
            -- status: cluster-note-embeddings
            -- Lazily-computed byte-length-weighted mean-pool of the note's
            -- chunk embeddings. NULL until the cluster pipeline first asks
            -- for the note; cleared on any chunk change (see
            -- `clear_note_embedding`) so the pool stays consistent with
            -- the live chunk-vecs.
            note_embedding   BLOB
        );

        CREATE TABLE IF NOT EXISTS chunks (
            id           TEXT PRIMARY KEY,
            note_id      TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
            chunk_index  INTEGER NOT NULL,
            byte_start   INTEGER NOT NULL,
            byte_end     INTEGER NOT NULL,
            text         TEXT NOT NULL,
            heading_path TEXT,
            UNIQUE(note_id, chunk_index)
        );

        CREATE INDEX IF NOT EXISTS chunks_note_id ON chunks(note_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vecs USING vec0(
            chunk_id  TEXT PRIMARY KEY,
            embedding float[{dim}]
        );

        CREATE TABLE IF NOT EXISTS path_ids (
            path TEXT PRIMARY KEY,
            id   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS path_ids_id ON path_ids(id);

        -- status: search-fts5-schema
        -- External-content FTS5: tokens + offsets only, no duplicated text;
        -- snippet()/match read chunks.text on demand via the contentless
        -- linkage. Sync triggers below keep this in lockstep with chunks.
        CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
            text,
            content='chunks',
            content_rowid='rowid',
            tokenize='unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
            INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
        END;
        CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
        END;
        CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
            INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
            INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
        END;

        -- status: trail-waypoints-derived-table
        -- Derived index of trail waypoints. Re-derived from frontmatter on
        -- every ingest of a trail-doc or waypoint-note; deletes cascade via
        -- the ops layer. `source_id` is nullable: a waypoint may reference
        -- a source note that hasn't been ingested (or stamped) yet.
        CREATE TABLE IF NOT EXISTS trail_waypoints (
            waypoint_path        TEXT PRIMARY KEY,
            waypoint_id          TEXT NOT NULL,
            trail_id             TEXT NOT NULL,
            source_id            TEXT,
            source_path          TEXT NOT NULL,
            parent_waypoint_id   TEXT,
            tree_path            TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS trail_waypoints_trail_id        ON trail_waypoints(trail_id);
        CREATE INDEX IF NOT EXISTS trail_waypoints_source_id       ON trail_waypoints(source_id);
        CREATE INDEX IF NOT EXISTS trail_waypoints_source_path     ON trail_waypoints(source_path);
        CREATE INDEX IF NOT EXISTS trail_waypoints_parent_waypoint ON trail_waypoints(parent_waypoint_id);

        -- status: store-rebuild-chunk-vecs-on-dim-change
        -- Tiny key/value sidecar for store-wide metadata. Today the only
        -- key is `chunk_vecs_dim` (the live embedding dim, set by
        -- ensure_chunk_vecs_dim). vec0 doesn't surface the dim in
        -- PRAGMA table_info, so we persist it here ourselves. Added
        -- without a schema-version bump — `CREATE TABLE IF NOT EXISTS`
        -- makes it forward-compatible with older v7 dbs (they get the
        -- table on first open and the dim falls back to
        -- DEFAULT_EMBED_DIM until a model switch forces a rebuild).
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
        dim = dim,
    ))?;

    if user_version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = SCHEMA_VERSION,
            "store: created index db schema",
        );
    }
    // Seed the chunk_vecs_dim meta row on first open if it's not already
    // there. Idempotent: an existing row (any value) is left alone — the
    // live writer's `ensure_chunk_vecs_dim` is the only path that bumps
    // it.
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('chunk_vecs_dim', ?1)",
        params![dim.to_string()],
    )?;
    Ok(())
}

/// Register sqlite-vec as an auto-extension. Idempotent: subsequent calls are
/// no-ops (guarded by `Once`). Affects every `Connection` opened after this
/// call in the process.
///
/// `sqlite-vec` declares its init symbol as `extern "C" fn()` (no parameters)
/// because the real C signature `(sqlite3*, char**, sqlite3_api_routines*)`
/// can't be expressed cleanly across the FFI boundary the crate uses. We
/// transmute to rusqlite's typed `RawAutoExtension` alias rather than to a
/// raw pointer so the destination signature is documented at the call site.
fn register_vec_extension() {
    use rusqlite::auto_extension::RawAutoExtension;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let init: RawAutoExtension =
            unsafe { std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ()) };
        if let Err(e) = unsafe { rusqlite::auto_extension::register_auto_extension(init) } {
            // Auto-extension registration is process-wide and only fails on
            // OOM or a pathologically broken sqlite build; if it does fail,
            // every subsequent Store::open will surface a clearer "vec_*
            // function not found" error, so log and continue.
            tracing::error!(
                error = %e,
                "sqlite-vec auto-extension register failed",
            );
        }
    });
}
