//! Index store. SQLite + sqlite-vec, statically linked. See docs/index.md.
//!
//! All rusqlite usage is confined here; callers see only the API in this module
//! and the DTOs it returns. The writer connection is owned by whoever
//! constructs `Store` (in v1, the indexer task); read connections open fresh
//! per call via `Store::open_reader`.

use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{params, Connection};

pub mod error;
pub mod dto;
pub mod vec;

mod notes;
mod chunks;
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
pub const SCHEMA_VERSION: i32 = 7;

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
