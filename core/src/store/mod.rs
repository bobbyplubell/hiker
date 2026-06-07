//! Index store. SQLite + sqlite-vec, statically linked. See docs/index.md.
//!
//! All rusqlite usage is confined here; callers see only the API in this module
//! and the DTOs it returns. The writer connection is owned by whoever
//! constructs `Store` (in v1, the indexer task); read connections open fresh
//! per call via `Store::open_reader`.

use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{params, Connection};

pub mod service;
pub mod error;
pub mod dto;
pub mod vec;

mod notes;
mod boards;
mod centroids;
mod chunks;
mod metadata;
mod search;
mod trails;

#[cfg(test)]
mod tests;

use error::Error;
use vec::read_chunk_vecs_dim;

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
///
/// v8 added the `note_meta` derived metadata index (flattened
/// frontmatter, one row per scalar / list element) backing structured
/// `query_notes` queries over tags / lifecycle / author / arbitrary
/// frontmatter fields. Re-derived from frontmatter on every ingest
/// (mirrors `trail_waypoints`); cleared on skip / delete. See
/// `docs/index.md` — `store-note-metadata-index`.
///
/// v9 added the `board_cards` derived index table (one row per card on a
/// board) for fast `boards_containing_note(note)` and `cards_of(board)`
/// lookups, plus the auto-update-on-move path. Re-derived from each
/// board-doc's `hiker.columns` frontmatter on ingest (clear-by-board +
/// re-insert), cleared on board-doc delete. See `docs/kanban.md`
/// §"Indexer integration" — `board-cards-derived-table`.
///
/// v10 dropped the `path_ids` table. The indexer no longer needs its own
/// parallel path↔id mapping; the note's path is its key. See `docs/index.md`
/// §"Store: sqlite-vec" — `store-path-is-identity`.
///
/// v11 dropped the dead `trail_waypoints.source_id` and
/// `board_cards.card_note_id` columns (+ their indexes). Under path-as-identity
/// the derived-table lookups (`trails_containing_note` /
/// `boards_containing_note`) key purely on the note's rel-path; the parallel
/// ULID columns were populated by the indexer but never queried by production
/// callers, so they're gone.
///
/// v12 made the note's vault path its identity (`op-log-path-identity`):
/// `notes.id` (the minted ULID / op-log doc_id) is dropped — `notes.path` is
/// now the primary key. `chunks.note_id` becomes `chunks.note_path`
/// (FK → `notes(path)` ON DELETE/UPDATE CASCADE), and the vec-join key
/// `chunk_vecs.chunk_id` is `"<note_path>:<idx>"`. The derived-table key
/// columns (`note_meta.note_id`, `trail_waypoints.{waypoint_id,trail_id}`,
/// `board_cards.board_id`) now carry vault paths rather than ULIDs — same
/// shape, path-valued. There is no `doc-index.db` and no separate document id.
pub const SCHEMA_VERSION: i32 = 12;

/// Default embedding dimension — matches the v1 default model
/// (`bge-small-en-v1.5`). Used as the initial `chunk_vecs` column width on
/// a brand-new vault and as the dim every test embedder reports. Per-model
/// dim is the actual source of truth at runtime (`Embedder::dim()`); the
/// store rebuilds `chunk_vecs` to match via `ensure_chunk_vecs_dim`.
///
/// status: embedder-dim-from-model
pub const DEFAULT_EMBED_DIM: usize = 384;



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
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        register_vec_extension();
        let db_path = vault_root.join(".hiker").join("index.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        configure_connection(&conn)?;
        let dim_arg = DEFAULT_EMBED_DIM;
        let user_version: i32 =
            conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if user_version != 0 && user_version != SCHEMA_VERSION {
            return Err(Error::VersionMismatch {
                found: user_version,
                expected: SCHEMA_VERSION,
            });
        }

        conn.execute_batch(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS notes (
                path             TEXT PRIMARY KEY,
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

            -- status: store-path-is-identity
            -- A note's identity is its vault path (`op-log-path-identity`):
            -- `chunks.note_path` is the FK to `notes(path)`, cascading on
            -- delete AND update so a rename (a `notes.path` UPDATE) re-keys the
            -- chunks with the note. The vec-join key `chunk_vecs.chunk_id` is
            -- `"<note_path>:<idx>"`; chunks.id holds that same string.
            CREATE TABLE IF NOT EXISTS chunks (
                id           TEXT PRIMARY KEY,
                note_path    TEXT NOT NULL REFERENCES notes(path) ON DELETE CASCADE ON UPDATE CASCADE,
                chunk_index  INTEGER NOT NULL,
                byte_start   INTEGER NOT NULL,
                byte_end     INTEGER NOT NULL,
                text         TEXT NOT NULL,
                heading_path TEXT,
                UNIQUE(note_path, chunk_index)
            );

            CREATE INDEX IF NOT EXISTS chunks_note_path ON chunks(note_path);

            CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vecs USING vec0(
                chunk_id  TEXT PRIMARY KEY,
                embedding float[{dim}]
            );

            -- status: store-path-is-identity
            -- The note's vault path is its identity — `notes.path` is the
            -- primary key, there is no minted id and no `doc-index.db`. The PK
            -- already gives path-keyed lookup; basename queries (for the
            -- wikilink ambiguity resolver) read `notes.path` directly.
            CREATE INDEX IF NOT EXISTS notes_path_basename
              ON notes(path);

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
            -- the ops layer. Keyed on the source note's rel-path
            -- (`source_path`); `trails_containing_note` matches on path only.
            CREATE TABLE IF NOT EXISTS trail_waypoints (
                waypoint_path        TEXT PRIMARY KEY,
                waypoint_id          TEXT NOT NULL,
                trail_id             TEXT NOT NULL,
                source_path          TEXT NOT NULL,
                parent_waypoint_id   TEXT,
                tree_path            TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS trail_waypoints_trail_id        ON trail_waypoints(trail_id);
            CREATE INDEX IF NOT EXISTS trail_waypoints_source_path     ON trail_waypoints(source_path);
            CREATE INDEX IF NOT EXISTS trail_waypoints_parent_waypoint ON trail_waypoints(parent_waypoint_id);

            -- status: board-cards-derived-table
            -- Derived index of board cards. Re-derived from each board-doc's
            -- `hiker.columns` frontmatter on ingest (clear-by-board +
            -- re-insert), cleared on board-doc delete. Keyed on the card
            -- note's rel-path (`card_note_path`); `boards_containing_note`
            -- matches on path only. Rowid PK (no declared key) since a board
            -- re-derive replaces the whole row set atomically.
            CREATE TABLE IF NOT EXISTS board_cards (
                board_id        TEXT NOT NULL,
                board_path      TEXT NOT NULL,
                card_note_path  TEXT NOT NULL,
                column_name     TEXT NOT NULL,
                ordinal         INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS board_cards_board_id   ON board_cards(board_id);
            CREATE INDEX IF NOT EXISTS board_cards_note_path  ON board_cards(card_note_path);
            CREATE INDEX IF NOT EXISTS board_cards_board_path ON board_cards(board_path);

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

            -- status: trees-centroids-index
            -- Derived cluster-tree centroids. Cluster trees are per-tree
            -- `.md` files (`trees-md-store`); their centroids (packed
            -- little-endian f32) are a recomputable index cache kept here
            -- rather than bloating the synced markdown. Keyed by
            -- (tree_id, node_id); read by the placement classifier
            -- (`cluster-place-beam-descent`). Added without a schema-version
            -- bump — `CREATE TABLE IF NOT EXISTS` makes it forward-compatible
            -- with older dbs (a missing centroid is recomputed from members).
            CREATE TABLE IF NOT EXISTS cluster_centroids (
                tree_id  TEXT NOT NULL,
                node_id  TEXT NOT NULL,
                centroid BLOB NOT NULL,
                PRIMARY KEY (tree_id, node_id)
            );

            -- status: store-note-metadata-index
            -- Derived per-note metadata index. Holds the note's frontmatter
            -- flattened to (key, value) rows: nested maps use dotted keys
            -- (`hiker.author`), list elements explode to one row each
            -- (`tags` → N rows). `num` mirrors `value` for YAML numbers /
            -- bools (NULL for strings) so range filters and numeric ordering
            -- work without parsing text at query time. Re-derived from
            -- frontmatter on every ingest (mirrors `trail_waypoints`);
            -- cleared on skip / delete. Backs `query_notes` — structured
            -- retrieval over tags / lifecycle / author / arbitrary fields.
            CREATE TABLE IF NOT EXISTS note_meta (
                note_id TEXT NOT NULL,
                key     TEXT NOT NULL,
                value   TEXT NOT NULL,
                num     REAL
            );
            CREATE INDEX IF NOT EXISTS note_meta_note      ON note_meta(note_id);
            CREATE INDEX IF NOT EXISTS note_meta_key_value ON note_meta(key, value);
            CREATE INDEX IF NOT EXISTS note_meta_key_num   ON note_meta(key, num);
            "#,
            dim = dim_arg,
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
            params![dim_arg.to_string()],
        )?;
        let dim = read_chunk_vecs_dim(&conn)?.unwrap_or(DEFAULT_EMBED_DIM);
        Ok(Self { conn, db_path, dim })
    }

    /// Live `chunk_vecs` embedding dimension. Equal to the loaded
    /// `Embedder::dim()` after `ensure_chunk_vecs_dim` has run.
    pub const fn dim(&self) -> usize {
        self.dim
    }

    /// Open a fresh read-only connection against the same db. Cheap (sub-ms on
    /// warm cache); intended for per-command read paths. Callers drop it on
    /// return.
    pub fn open_reader(&self) -> Result<Connection, Error> {
        register_vec_extension();
        let conn = Connection::open(&self.db_path)?;
        configure_connection(&conn)?;
        Ok(conn)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

fn configure_connection(conn: &Connection) -> Result<(), Error> {
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
