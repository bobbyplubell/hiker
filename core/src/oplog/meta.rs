//! The editorial-metadata side table (`oplog_meta.db`) and the path→doc_id
//! index (`doc-index.db`). Yrs ops carry no author/status/surface, so the
//! op log layers that in one vault-wide SQLite database keyed by
//! `(doc_id, op_id)` plus the Yrs clock range the row describes.
//!
//! All `rusqlite` use for the op log lives here. WAL + `synchronous=NORMAL`,
//! fail-loud on schema-version mismatch — the same posture as `core::store`.
//
// status: op-log-side-table
// status: op-log-status-states
// status: op-log-store-layout

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::error::Error;
use super::shapes::{Author, OpKind};

/// Bumped only when the on-disk schema changes. Pre-1.0 policy mirrors
/// `store-version-fail-loud`: a mismatch is an error, not a migration —
/// the legacy-cutover phase deletes and rebuilds.
///
/// v2 adds the `content_hash` column the sync enrollment classification reads
/// (`sync-content-hash-column`).
pub const SCHEMA_VERSION: i32 = 2;

/// Schema version for the separate `doc-index.db` file (`open_index`). Tracked
/// independently of `SCHEMA_VERSION` because the two SQLite files evolve on
/// their own cadence. v1 is the current schema: `doc_index` plus the
/// `bootstrap_skipped` table (`bug-oplog-bootstrap-nonutf8-warn-spam`). Same
/// fail-loud posture as `open_meta` / `core::store` — a mismatch is an error,
/// not a migration. Existing on-disk files predate version tracking
/// (`user_version == 0`) and are stamped to v1 on next open since their schema
/// already matches.
pub const INDEX_SCHEMA_VERSION: i32 = 1;

/// Status of an accepted/rejected op in the side table. `pending` is *not*
/// a side-table state — pending ops live only in `<doc-id>.pending` and have
/// no Yrs client_id range until they land in `accepted`.
///
/// status: op-log-status-states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Accepted,
    Rejected,
}

impl OpStatus {
    const fn as_str(self) -> &'static str {
        match self {
            OpStatus::Accepted => "accepted",
            OpStatus::Rejected => "rejected",
        }
    }
}

/// One row of `op_metadata`, returned by [`query_metadata`]. Plain Rust
/// types only — no rusqlite or Yrs types cross this boundary.
///
/// status: op-log-side-table
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpMetadata {
    pub doc_id: String,
    pub op_id: String,
    pub yrs_client_id: i64,
    pub yrs_clock_lo: i64,
    pub yrs_clock_hi: i64,
    pub author: Author,
    pub op_kind: String,
    pub rename_from: Option<String>,
    pub status: OpStatus,
    pub timestamp_ms: i64,
    /// blake3 hex of `materialize(accepted)` as of this op, for accepted ops
    /// that changed content; `None` for rejected (never-applied) rows. The
    /// sync enrollment hash-classification reads this (`sync-content-hash-column`).
    pub content_hash: Option<String>,
    pub surface: Option<String>,
    pub session_id: Option<String>,
    pub batch_id: Option<String>,
    pub metadata: serde_json::Value,
}

/// Fields for one metadata insert. Borrowed to avoid extra allocations on
/// the accept/reject path.
pub(super) struct MetadataInsert<'a> {
    pub doc_id: &'a str,
    pub op_id: &'a str,
    pub yrs_client_id: i64,
    pub yrs_clock_lo: i64,
    pub yrs_clock_hi: i64,
    pub author: &'a Author,
    pub op_kind: &'a OpKind,
    pub status: OpStatus,
    pub timestamp_ms: i64,
    /// blake3 hex of `materialize(accepted)` as of this op (`Some` on accepted
    /// content ops, `None` on rejected rows) — `sync-content-hash-column`.
    pub content_hash: Option<&'a str>,
    pub surface: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub batch_id: Option<&'a str>,
    pub metadata: &'a serde_json::Value,
}

/// Filter for [`query_metadata`]. An empty filter returns the whole table
/// most-recent-first. `author_class` builds a `LIKE 'class:%'` prefix
/// wildcard; `author_exact` matches the full wire string.
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

/// Open/create the side table and the path index, run the idempotent schema
/// bootstrap, and fail loud on a version mismatch.
///
/// status: op-log-side-table
/// status: op-log-store-layout
pub(super) fn open_meta(oplog_dir: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(oplog_dir.join("oplog_meta.db"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;

    let user_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(Error::VersionMismatch {
            found: user_version,
            expected: SCHEMA_VERSION,
        });
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS op_metadata (
            doc_id        TEXT NOT NULL,
            op_id         TEXT NOT NULL,
            yrs_client_id INTEGER NOT NULL,
            yrs_clock_lo  INTEGER NOT NULL,
            yrs_clock_hi  INTEGER NOT NULL,
            author        TEXT NOT NULL,
            op_kind       TEXT NOT NULL,
            rename_from   TEXT,
            status        TEXT NOT NULL,
            timestamp_ms  INTEGER NOT NULL,
            content_hash  TEXT,
            surface       TEXT,
            session_id    TEXT,
            batch_id      TEXT,
            metadata      TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS op_metadata_doc_ts ON op_metadata(doc_id, timestamp_ms DESC);
        CREATE INDEX IF NOT EXISTS op_metadata_author_ts ON op_metadata(author, timestamp_ms DESC);
        CREATE INDEX IF NOT EXISTS op_metadata_status ON op_metadata(status, timestamp_ms DESC);
        CREATE INDEX IF NOT EXISTS op_metadata_doc_hash ON op_metadata(doc_id, content_hash);
        "#,
    )?;
    if user_version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = SCHEMA_VERSION,
            "oplog: created op_metadata db schema",
        );
    }
    Ok(conn)
}

/// Open/create the `doc-index.db` path→doc_id map. Separate connection so
/// the two SQLite files stay independent (the index is regenerable by
/// rescanning each Doc's `meta.path`).
///
/// status: op-log-store-layout
pub(super) fn open_index(oplog_dir: &Path) -> Result<Connection, Error> {
    let conn = Connection::open(oplog_dir.join("doc-index.db"))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;

    let user_version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != INDEX_SCHEMA_VERSION {
        return Err(Error::VersionMismatch {
            found: user_version,
            expected: INDEX_SCHEMA_VERSION,
        });
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS doc_index (
            path   TEXT PRIMARY KEY,
            doc_id TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS doc_index_doc ON doc_index(doc_id);
        CREATE TABLE IF NOT EXISTS bootstrap_skipped (
            path        TEXT PRIMARY KEY,
            skip_reason TEXT NOT NULL,
            skipped_at  INTEGER NOT NULL
        );
        "#,
    )?;
    if user_version == 0 {
        conn.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = INDEX_SCHEMA_VERSION,
            "oplog: created doc-index db schema",
        );
    }
    Ok(conn)
}

/// Insert one `op_metadata` row.
///
/// status: op-log-side-table
pub(super) fn insert_metadata(conn: &Connection, row: &MetadataInsert<'_>) -> Result<(), Error> {
    let rename_from = match row.op_kind {
        OpKind::Rename { from } => Some(from.as_str()),
        _ => None,
    };
    conn.execute(
        "INSERT INTO op_metadata (
            doc_id, op_id, yrs_client_id, yrs_clock_lo, yrs_clock_hi,
            author, op_kind, rename_from, status, timestamp_ms,
            content_hash, surface, session_id, batch_id, metadata
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            row.doc_id,
            row.op_id,
            row.yrs_client_id,
            row.yrs_clock_lo,
            row.yrs_clock_hi,
            row.author.as_wire(),
            row.op_kind.as_str(),
            rename_from,
            row.status.as_str(),
            row.timestamp_ms,
            row.content_hash,
            row.surface,
            row.session_id,
            row.batch_id,
            serde_json::to_string(row.metadata).unwrap_or_else(|_| "{}".to_string()),
        ],
    )?;
    Ok(())
}

/// Query the side table. Builds a dynamic `WHERE` from the filter; results
/// are most-recent-first.
///
/// status: op-log-side-table
/// status: op-log-author-classes
pub(super) fn query_metadata(
    conn: &Connection,
    filter: &Filter,
) -> Result<Vec<OpMetadata>, Error> {
    let mut sql = String::from(
        "SELECT doc_id, op_id, yrs_client_id, yrs_clock_lo, yrs_clock_hi, \
         author, op_kind, rename_from, status, timestamp_ms, \
         content_hash, surface, session_id, batch_id, metadata FROM op_metadata",
    );
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(doc_id) = &filter.doc_id {
        clauses.push(format!("doc_id = ?{}", binds.len() + 1));
        binds.push(Box::new(doc_id.clone()));
    }
    if let Some(class) = &filter.author_class {
        // Class authors come in two wire shapes: bare (`user`, `external`)
        // and `class:identifier` (`agent:claude-code`). Match both so a
        // class filter catches the identifier-less authors too.
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
    if let Some(status) = filter.status {
        clauses.push(format!("status = ?{}", binds.len() + 1));
        binds.push(Box::new(status.as_str().to_string()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    // `rowid` (insertion order) is the deterministic newest-first tiebreak when
    // several ops share a millisecond (a create + an immediate save, rapid
    // autosaves). `op_id` can't serve here — `ulid::new()` isn't monotonic
    // within a millisecond. Without a tiebreak the newest-first contract — which
    // the version dropdown's "current" and `previous_accepted_content` rely on —
    // is non-deterministic.
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
    let author_wire: String = row.get(5)?;
    let status_str: String = row.get(8)?;
    let metadata_str: String = row.get(14)?;
    Ok(OpMetadata {
        doc_id: row.get(0)?,
        op_id: row.get(1)?,
        yrs_client_id: row.get(2)?,
        yrs_clock_lo: row.get(3)?,
        yrs_clock_hi: row.get(4)?,
        author: Author::parse(&author_wire),
        op_kind: row.get(6)?,
        rename_from: row.get(7)?,
        status: if status_str == "rejected" {
            OpStatus::Rejected
        } else {
            OpStatus::Accepted
        },
        timestamp_ms: row.get(9)?,
        content_hash: row.get(10)?,
        surface: row.get(11)?,
        session_id: row.get(12)?,
        batch_id: row.get(13)?,
        metadata: serde_json::from_str(&metadata_str).unwrap_or(serde_json::Value::Null),
    })
}

/// The set of non-null `content_hash` values across a doc's *accepted* ops —
/// the "content was once this" history set the sync enrollment classification
/// tests a peer's current hash against (`sync-content-hash-column`). Rejected
/// rows carry no hash, so the `status` filter is implicit in the `NOT NULL`.
///
/// status: op-log-side-table
pub(super) fn doc_content_hashes(
    conn: &Connection,
    doc_id: &str,
) -> Result<std::collections::HashSet<String>, Error> {
    let mut stmt = conn.prepare(
        "SELECT content_hash FROM op_metadata \
         WHERE doc_id = ?1 AND content_hash IS NOT NULL",
    )?;
    let rows = stmt
        .query_map(params![doc_id], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<String>, _>>()?;
    Ok(rows)
}

/// The most-recent `limit` distinct non-null `content_hash` values for a doc's
/// *accepted* ops, ordered by `timestamp_ms DESC, rowid DESC` — the bounded
/// recent-history window the sync manifest carries. Returning an ordered `Vec`
/// (not a `HashSet`) keeps the truncation deterministic: peers will classify
/// the same window the same way every time (`bug-sync-history-hashset-truncation-nondet`).
///
/// status: op-log-side-table
pub(super) fn doc_recent_content_hashes(
    conn: &Connection,
    doc_id: &str,
    limit: usize,
) -> Result<Vec<String>, Error> {
    let mut stmt = conn.prepare(
        "SELECT content_hash, MAX(timestamp_ms) AS ts, MAX(rowid) AS rid \
         FROM op_metadata \
         WHERE doc_id = ?1 AND status = 'accepted' AND content_hash IS NOT NULL \
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

/// GC: delete `op_metadata` rows of `status` older than `cutoff_ms`. Pending
/// ops never auto-GC (they aren't in this table). Returns the deleted count.
///
/// status: op-log-status-states
pub(super) fn gc_status(
    conn: &Connection,
    status: OpStatus,
    cutoff_ms: i64,
) -> Result<usize, Error> {
    let n = conn.execute(
        "DELETE FROM op_metadata WHERE status = ?1 AND timestamp_ms < ?2",
        params![status.as_str(), cutoff_ms],
    )?;
    Ok(n)
}

// ── doc-index.db helpers ───────────────────────────────────────────────

/// Upsert a path→doc_id mapping.
pub(super) fn put_doc_id(conn: &Connection, path: &str, doc_id: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO doc_index (path, doc_id) VALUES (?1, ?2)
         ON CONFLICT(path) DO UPDATE SET doc_id = excluded.doc_id",
        params![path, doc_id],
    )?;
    Ok(())
}

/// Atomically repoint a document to `new_path`: drop any existing rows for
/// `doc_id` and map `new_path` to it, in one transaction. A document maps to
/// exactly one current path, so a rename can never leave a stale second row
/// (which would make [`path_for_doc_id`]'s lookup nondeterministic) even if
/// the process crashes mid-rename. If `new_path` was occupied by another doc,
/// the upsert repoints it (last-writer-wins; the accept path pre-checks
/// rename collisions before reaching here).
///
/// status: op-log-store-layout
pub(super) fn repoint_doc(conn: &Connection, doc_id: &str, new_path: &str) -> Result<(), Error> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM doc_index WHERE doc_id = ?1", params![doc_id])?;
    tx.execute(
        "INSERT INTO doc_index (path, doc_id) VALUES (?1, ?2)
         ON CONFLICT(path) DO UPDATE SET doc_id = excluded.doc_id",
        params![new_path, doc_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Look up the doc_id for a vault-relative path.
pub(super) fn doc_id_for_path(conn: &Connection, path: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row(
            "SELECT doc_id FROM doc_index WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )
        .optional()?)
}

/// Look up the current vault-relative path for a doc_id. A rename repoints
/// the mapping in place (`put_doc_id` on the new path), so the most recent
/// `path` row for a doc_id is its current location — the changes/activity
/// projection resolves a side-table row's `doc_id` back to a path through
/// this. Returns `None` for an unmapped doc_id.
pub(super) fn path_for_doc_id(conn: &Connection, doc_id: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row(
            "SELECT path FROM doc_index WHERE doc_id = ?1 LIMIT 1",
            params![doc_id],
            |row| row.get(0),
        )
        .optional()?)
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
