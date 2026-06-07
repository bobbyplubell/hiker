//! The regenerable history query-index (`op_history` in `index.db`).
//!
//! The durable history is the per-document `.ops` frames (self-describing:
//! author, op-kind, surface, session/batch ids, durable metadata, timestamp).
//! This module owns a **regenerable** SQLite query-index over those frames so
//! the activity feed, per-file history, author-attribution, and the sync
//! content-hash merge-base get fast indexed lookups without re-reading every
//! `.ops` file. The index is rebuilt by replaying frames — `op-log-no-oplog-db`
//! / `changes-query-api` — and lives in the vault's sole `index.db`
//! (`index.md`), not in a durable side table.
//!
//! Because it's regenerable, dropping the table and replaying every doc's
//! `.ops` frames yields byte-identical rows (same op-id / hash / ordering), so
//! `rm`-ing `index.db` and reopening is a no-op for correctness.
//!
//! Under path-as-identity (`op-log-path-identity`) the `path` column holds the
//! document's vault-relative path — the path IS the id. A small durable
//! `bootstrap_skipped` marker table rides alongside (cheap to regenerate: a
//! re-bootstrap re-reads and re-skips the file).
//!
//! All `rusqlite` use for the op log lives here. WAL + `synchronous=NORMAL`,
//! the same posture as `core::store` (which owns the same `index.db` file
//! through its own connection).
//
// status: op-log-no-oplog-db
// status: changes-query-api
// status: op-log-status-states
// status: op-log-store-layout
// status: op-log-path-identity

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::error::Error;
use super::shapes::Author;
use super::store;

/// Status of a query/projection row. Only accepted ops have `.ops` frames, so
/// the regenerable index only ever holds `Accepted` rows; `Rejected` survives
/// as a typed enum solely so the public DTO and `Filter` stay stable for
/// callers (a rejected filter matches nothing).
///
/// status: op-log-status-states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Accepted,
    Rejected,
}

/// One row of the `op_history` query-index, returned by [`query_metadata`].
/// Plain Rust types only — no rusqlite types cross this boundary. Kept named
/// `OpMetadata` so the activity / history callers stay stable. Every row is an
/// accepted op (the index only indexes `.ops` frames); `status` is always
/// `Accepted`.
///
/// status: changes-query-api
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpMetadata {
    pub doc_id: String,
    pub op_id: String,
    pub author: Author,
    pub op_kind: String,
    pub rename_from: Option<String>,
    pub status: OpStatus,
    pub timestamp_ms: i64,
    /// blake3 hex of `materialize(accepted)` as of this op — computed on replay
    /// from the frame's materialized text. The sync enrollment hash-classification
    /// reads this (`sync-content-hash-column`). Always `Some` for an indexed
    /// (accepted) op.
    pub content_hash: Option<String>,
    pub surface: Option<String>,
    pub session_id: Option<String>,
    pub batch_id: Option<String>,
    pub metadata: serde_json::Value,
}

/// Filter for [`query_metadata`]. An empty filter returns the whole index
/// most-recent-first. `author_class` builds a `LIKE 'class:%'` prefix
/// wildcard; `author_exact` matches the full wire string. A `status` of
/// `Rejected` matches no rows (the index holds only accepted ops).
///
/// status: op-log-author-classes
/// status: op-log-status-states
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub doc_id: Option<String>,
    pub author_class: Option<String>,
    pub author_exact: Option<String>,
    pub status: Option<OpStatus>,
    pub limit: Option<usize>,
}

/// One row appended to the `op_history` index as a frame is retained. Built
/// from the [`HistoryFrame`] metadata + the materialized content hash computed
/// at append time, so the incremental path matches a full replay exactly.
pub(super) struct HistoryRow<'a> {
    pub doc_id: &'a str,
    pub op_id: &'a str,
    pub author_wire: &'a str,
    pub op_kind: &'a str,
    pub rename_from: Option<&'a str>,
    pub timestamp_ms: i64,
    pub content_hash: &'a str,
    pub surface: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub batch_id: Option<&'a str>,
    pub metadata: &'a serde_json::Value,
}

/// Open the vault's `index.db` and ensure the regenerable `op_history`
/// query-index (+ the durable `bootstrap_skipped` marker) exist.
///
/// The op log opens its **own** connection to the same `index.db` the search
/// store owns (`op-log-no-oplog-db`: one `index.db` per vault). The two
/// connections coordinate at the SQLite WAL level — the store owns the `notes`
/// / `chunks` / `chunk_vecs` tables and its own `user_version`; the op log owns
/// `op_history` here. `op_history` is created `IF NOT EXISTS` and carries no
/// schema-version gate (it's regenerable from `.ops`, so a shape change is a
/// drop-and-replay, never a migration).
///
/// status: op-log-no-oplog-db
/// status: changes-query-api
pub(super) fn open_index(vault_root: &Path) -> Result<Connection, Error> {
    let hiker_dir = vault_root.join(".hiker");
    std::fs::create_dir_all(&hiker_dir)?;
    let conn = Connection::open(hiker_dir.join("index.db"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the op-log's tables in
/// `index.db`. Separate from `core::store`'s schema so each owner manages its
/// own tables on the shared file.
fn ensure_schema(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS op_history (
            path          TEXT NOT NULL,
            op_id         TEXT NOT NULL,
            timestamp_ms  INTEGER NOT NULL,
            author        TEXT NOT NULL,
            op_kind       TEXT NOT NULL,
            rename_from   TEXT,
            content_hash  TEXT NOT NULL,
            surface       TEXT,
            session_id    TEXT,
            batch_id      TEXT,
            metadata      TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS op_history_path_ts ON op_history(path, timestamp_ms DESC);
        CREATE INDEX IF NOT EXISTS op_history_author_ts ON op_history(author, timestamp_ms DESC);
        CREATE INDEX IF NOT EXISTS op_history_path_hash ON op_history(path, content_hash);
        CREATE TABLE IF NOT EXISTS bootstrap_skipped (
            path        TEXT PRIMARY KEY,
            skip_reason TEXT NOT NULL,
            skipped_at  INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Rebuild the `op_history` index from scratch by replaying every document's
/// `.ops` frames. Wipes the table first, then walks every `.ops` file under
/// `oplog_dir`, reconstructs each frame's materialized text in order (so the
/// `content_hash` matches `materialize_at`), and appends one row per frame.
///
/// This is the regeneration the design promises (`changes-query-api`): the
/// table is a cache over the durable `.ops` history, so dropping it and
/// replaying yields identical rows. Run on [`super::OpLog::open`]; the
/// incremental [`insert_history`] keeps it current per commit thereafter.
///
/// status: changes-query-api
/// status: op-log-no-oplog-db
pub(super) fn rebuild_from_ops(conn: &Connection, oplog_dir: &Path) -> Result<(), Error> {
    conn.execute("DELETE FROM op_history", [])?;
    let doc_ids = store::scan_doc_ids(oplog_dir, "ops")?;
    for doc_id in doc_ids {
        replay_doc(conn, oplog_dir, &doc_id)?;
    }
    Ok(())
}

/// Replay one document's `.ops` frames into `op_history` (rows newest-last in
/// append order). Reconstructs each frame's materialized text from the running
/// chain so `content_hash` equals `materialize_at(op_id)`.
fn replay_doc(conn: &Connection, oplog_dir: &Path, doc_id: &str) -> Result<(), Error> {
    let frames = store::load_ops(oplog_dir, doc_id)?;
    let mut running = String::new();
    for frame in &frames {
        // Decode against the running text (keyframes ignore it, deltas use it
        // as the dictionary) — the same forward walk `materialize_at` does.
        let text = frame.decode(&running)?;
        let hash = super::content_hash(&text);
        let meta = frame.metadata_value();
        insert_history(
            conn,
            &HistoryRow {
                doc_id,
                op_id: &frame.op_id,
                author_wire: &frame.author,
                op_kind: &frame.op_kind,
                rename_from: frame.rename_from.as_deref(),
                timestamp_ms: frame.timestamp_ms,
                content_hash: &hash,
                surface: frame.surface.as_deref(),
                session_id: frame.session_id.as_deref(),
                batch_id: frame.batch_id.as_deref(),
                metadata: &meta,
            },
        )?;
        running = text;
    }
    Ok(())
}

/// Insert one `op_history` row (the incremental append on a retained frame, and
/// the per-frame step of a full replay).
///
/// status: changes-query-api
pub(super) fn insert_history(conn: &Connection, row: &HistoryRow<'_>) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO op_history (
            path, op_id, timestamp_ms, author, op_kind, rename_from,
            content_hash, surface, session_id, batch_id, metadata
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            row.doc_id,
            row.op_id,
            row.timestamp_ms,
            row.author_wire,
            row.op_kind,
            row.rename_from,
            row.content_hash,
            row.surface,
            row.session_id,
            row.batch_id,
            serde_json::to_string(row.metadata).unwrap_or_else(|_| "{}".to_string()),
        ],
    )?;
    Ok(())
}

/// Query the `op_history` index. Builds a dynamic `WHERE` from the filter;
/// results are most-recent-first. A `status = Rejected` filter short-circuits
/// to an empty result (the index holds only accepted ops — rejected pending
/// edits are transient and leave no frame).
///
/// status: changes-query-api
/// status: op-log-author-classes
pub(super) fn query_metadata(
    conn: &Connection,
    filter: &Filter,
) -> Result<Vec<OpMetadata>, Error> {
    if filter.status == Some(OpStatus::Rejected) {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT path, op_id, timestamp_ms, author, op_kind, rename_from, \
         content_hash, surface, session_id, batch_id, metadata FROM op_history",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(doc_id) = &filter.doc_id {
        clauses.push(format!("path = ?{}", binds.len() + 1));
        binds.push(Box::new(doc_id.clone()));
    }
    if let Some(class) = &filter.author_class {
        // Class authors come in two wire shapes: bare (`user`, `external`) and
        // `class:identifier` (`agent:claude-code`). Match both so a class
        // filter catches the identifier-less authors too.
        clauses.push(format!(
            "(author = ?{eq} OR author LIKE ?{like})",
            eq = binds.len() + 1,
            like = binds.len() + 2,
        ));
        binds.push(Box::new(class.clone()));
        binds.push(Box::new(format!("{class}:%")));
    }
    if let Some(exact) = &filter.author_exact {
        clauses.push(format!("author = ?{}", binds.len() + 1));
        binds.push(Box::new(exact.clone()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    // `rowid` (insertion order — append order during replay/commit) is the
    // deterministic newest-first tiebreak when several ops share a millisecond
    // (a create + an immediate save, rapid autosaves). `op_id` can't serve here
    // — `ulid::new()` isn't monotonic within a millisecond. Without a tiebreak
    // the newest-first contract — which the version dropdown's "current" and
    // `previous_accepted_content` rely on — is non-deterministic.
    sql.push_str(" ORDER BY timestamp_ms DESC, rowid DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(AsRef::as_ref).collect();
    let rows = stmt
        .query_map(bind_refs.as_slice(), map_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OpMetadata> {
    let author_wire: String = row.get(3)?;
    let metadata_str: String = row.get(10)?;
    Ok(OpMetadata {
        doc_id: row.get(0)?,
        op_id: row.get(1)?,
        timestamp_ms: row.get(2)?,
        author: Author::parse(&author_wire),
        op_kind: row.get(4)?,
        rename_from: row.get(5)?,
        // Every indexed op is accepted (only `.ops` frames are indexed).
        status: OpStatus::Accepted,
        content_hash: row.get(6)?,
        surface: row.get(7)?,
        session_id: row.get(8)?,
        batch_id: row.get(9)?,
        metadata: serde_json::from_str(&metadata_str).unwrap_or(serde_json::Value::Null),
    })
}

/// The set of `content_hash` values across a doc's accepted ops — the "content
/// was once this" history set the sync enrollment classification tests a peer's
/// current hash against (`sync-content-hash-column`). Every indexed op is
/// accepted and carries a hash.
///
/// status: changes-query-api
pub(super) fn doc_content_hashes(
    conn: &Connection,
    doc_id: &str,
) -> Result<std::collections::HashSet<String>, Error> {
    let mut stmt = conn.prepare("SELECT content_hash FROM op_history WHERE path = ?1")?;
    let rows = stmt
        .query_map(params![doc_id], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<String>, _>>()?;
    Ok(rows)
}

/// The most-recent `limit` distinct `content_hash` values for a doc's accepted
/// ops, ordered by `timestamp_ms DESC, rowid DESC` — the bounded recent-history
/// window the sync manifest carries. Returning an ordered `Vec` (not a
/// `HashSet`) keeps the truncation deterministic
/// (`bug-sync-history-hashset-truncation-nondet`).
///
/// status: changes-query-api
pub(super) fn doc_recent_content_hashes(
    conn: &Connection,
    doc_id: &str,
    limit: usize,
) -> Result<Vec<String>, Error> {
    let mut stmt = conn.prepare(
        "SELECT content_hash, MAX(timestamp_ms) AS ts, MAX(rowid) AS rid \
         FROM op_history \
         WHERE path = ?1 \
         GROUP BY content_hash \
         ORDER BY ts DESC, rid DESC \
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![doc_id, limit as i64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(rows)
}

/// The `op_id` of the most-recent accepted op whose `content_hash` is one of
/// `shared` — the op at which this doc's content last matched a content the
/// peer also knows (its `recent_history_hashes`). That op's
/// [`super::OpLog::materialize_at`] reconstruction is the common base for the
/// same-region 3-way overlap test (`sync-conflict-detect-same-region`):
/// "the most recent content whose hash appears in BOTH histories." `None` when
/// no accepted op of this doc carries a shared hash (no common base — the fork
/// path, left to enrollment classification). Newest-first by
/// `timestamp_ms DESC, rowid DESC`, matching [`query_metadata`]'s order.
///
/// status: changes-query-api
pub(super) fn most_recent_shared_op_id(
    conn: &Connection,
    doc_id: &str,
    shared: &std::collections::HashSet<String>,
) -> Result<Option<String>, Error> {
    if shared.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare(
        "SELECT op_id, content_hash FROM op_history \
         WHERE path = ?1 \
         ORDER BY timestamp_ms DESC, rowid DESC",
    )?;
    let mut rows = stmt.query(params![doc_id])?;
    while let Some(row) = rows.next()? {
        let op_id: String = row.get(0)?;
        let hash: String = row.get(1)?;
        if shared.contains(&hash) {
            return Ok(Some(op_id));
        }
    }
    Ok(None)
}

/// GC: delete `op_history` rows older than `cutoff_ms`. Only `Accepted` rows
/// exist in the index, so an `Accepted` status GCs the activity-retention
/// window; a `Rejected` status is a no-op (no rejected rows are stored — a
/// rejected pending edit leaves no frame). Returns the deleted count. Note the
/// `.ops` frames themselves are not pruned here — GC trims the query-index
/// (the regenerable cache), which a later replay would re-populate from any
/// still-present frames.
///
/// status: op-log-status-states
pub(super) fn gc_status(
    conn: &Connection,
    status: OpStatus,
    cutoff_ms: i64,
) -> Result<usize, Error> {
    if status == OpStatus::Rejected {
        return Ok(0);
    }
    let n = conn.execute(
        "DELETE FROM op_history WHERE timestamp_ms < ?1",
        params![cutoff_ms],
    )?;
    Ok(n)
}

/// Repoint the `op_history` rows of a renamed document from its old path to its
/// new path: under path-as-identity the `path` column holds the path, so a
/// rename rewrites those rows to the new key. Idempotent; a document with no
/// history rows is a silent no-op. (The `.ops` frames are relocated separately;
/// this keeps the regenerable index consistent without a full rebuild.)
///
/// status: op-log-observed-move
pub(super) fn repoint_metadata(conn: &Connection, from: &str, to: &str) -> Result<(), Error> {
    conn.execute(
        "UPDATE op_history SET path = ?1 WHERE path = ?2",
        params![to, from],
    )?;
    Ok(())
}

// ── bootstrap_skipped helpers ──────────────────────────────────────────────

/// Record a path that the bootstrap failed to seed (e.g. non-UTF-8). Stored
/// persistently so subsequent bootstraps skip it silently without re-reading
/// the file or re-emitting the WARN. Idempotent (upsert on the primary key).
pub(super) fn put_bootstrap_skip(
    conn: &Connection,
    path: &str,
    reason: &str,
) -> Result<(), Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO bootstrap_skipped (path, skip_reason, skipped_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET
             skip_reason = excluded.skip_reason,
             skipped_at  = excluded.skipped_at",
        params![path, reason, now],
    )?;
    Ok(())
}

/// Returns `true` when `path` has a persistent bootstrap-skip marker so
/// subsequent bootstrap runs can skip silently without re-reading the file.
pub(super) fn is_bootstrap_skipped(conn: &Connection, path: &str) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM bootstrap_skipped WHERE path = ?1",
            params![path],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}
