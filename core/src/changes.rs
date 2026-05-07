//! Append-only changelog of every vault-content write. See docs/changes.md.
//!
//! Lives in `<vault>/.hiker/changes.db`, separate from the regenerable
//! `index.db`. Each row is one mutation (user save, agent write, eventual
//! sync receive / import); the `author` field distinguishes them. The store
//! is the substrate for agent rollback (`mcp.md`), the home-page recent
//! activity widget (`editor.md`), per-file history views (deferred), and
//! the future sync layer (`design.md`).
//!
//! All rusqlite use is confined to this module — same discipline as
//! `core::store`. Callers receive `ChangeRow` DTOs (without the content
//! blob) and pull content separately via `content_at` /
//! `previous_content_for_path` so listings stay cheap.
//!
//! Concurrency: a single shared writer connection sits behind a `Mutex`,
//! cloned via `Arc<Changes>` everywhere a mutation logs a row. Per spec the
//! indexer task is the canonical writer — in practice, ops-layer call sites
//! also append directly through the shared mutex; one logical writer is
//! preserved.
//
// status: changes-log-table
// status: changes-write-path
// status: changes-query-api
// status: changes-rollback-helper
// status: changes-retention
// status: changes-store-file

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

/// Bumped only when the on-disk schema changes. Pre-real-use policy mirrors
/// `store-version-fail-loud`: a mismatch is an error, not a migration.
pub const SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Error)]
pub enum ChangesError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("metadata json: {0}")]
    MetadataJson(String),
    #[error("not found: change {0}")]
    NotFound(i64),
}

/// One row in the changelog. `content` is fetched separately via
/// `content_at` to keep listings cheap on long histories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRow {
    pub id: i64,
    pub timestamp_ms: i64,
    pub path: String,
    pub op: ChangeOp,
    pub author: String,
    pub content_hash: Option<String>,
    pub rename_from: Option<String>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Created,
    Modified,
    Deleted,
    Renamed,
}

impl ChangeOp {
    fn as_str(self) -> &'static str {
        match self {
            ChangeOp::Created => "created",
            ChangeOp::Modified => "modified",
            ChangeOp::Deleted => "deleted",
            ChangeOp::Renamed => "renamed",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "created" => ChangeOp::Created,
            "modified" => ChangeOp::Modified,
            "deleted" => ChangeOp::Deleted,
            "renamed" => ChangeOp::Renamed,
            _ => return None,
        })
    }
}

/// Bundle of fields for a single append. Borrowed strings + slices to avoid
/// extra allocations on the hot save path.
pub struct ChangeAppend<'a> {
    pub path: &'a str,
    pub op: ChangeOp,
    pub author: &'a str,
    pub content_hash: Option<&'a str>,
    pub content: Option<&'a [u8]>,
    pub rename_from: Option<&'a str>,
    pub metadata: serde_json::Value,
}

/// Owned writer connection to `<vault>/.hiker/changes.db`. Cheap to clone via
/// `Arc<Changes>`; internally serialized through a `Mutex` so multiple ops
/// callers can append concurrently without stepping on each other.
pub struct Changes {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    /// Broadcast of "a row was just appended" notifications. Carries the
    /// new row (without content blob) so a Tauri-bridge consumer can emit
    /// `hiker:changes-appended` to the frontend without a second round
    /// trip. Capacity is small — the consumer is expected to re-fetch the
    /// recent list on its next refresh; lagging just means a few coalesced
    /// emits.
    appended_tx: broadcast::Sender<ChangeRow>,
}

impl Changes {
    /// Open or create the changelog db. Idempotent; fails loud on schema
    /// version mismatch (no migration in v3 — the bump from no-such-table to
    /// v1 is handled by `ensure_schema` on first open).
    pub fn open(vault_root: &Path) -> Result<Self, ChangesError> {
        let db_path = vault_root.join(".hiker").join("changes.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        configure(&conn)?;
        ensure_schema(&conn)?;
        let (appended_tx, _) = broadcast::channel(64);
        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
            appended_tx,
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Subscribe to "row appended" notifications. Each successful `append`
    /// fires once. Lagging receivers are dropped silently — consumers
    /// should re-query `recent` on resume rather than rely on the stream
    /// being lossless.
    pub fn subscribe(&self) -> broadcast::Receiver<ChangeRow> {
        self.appended_tx.subscribe()
    }

    /// Append a row. Returns the new row's id (monotonic; future sync uses
    /// this as a watermark).
    /// Whether `path` has any rows yet. Used by the save-path baseline: the
    /// first time a pre-existing vault file is mutated through hiker, we
    /// don't have a prior row to roll back to. The fix is a lazy baseline
    /// — snapshot the *pre-mutation* state once, on the first append for
    /// the path, then proceed normally. See `ensure_baseline`.
    pub fn has_any_for_path(&self, path: &str) -> Result<bool, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM changes WHERE path = ?1 LIMIT 1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// If no row exists yet for `path`, append a baseline `created` row
    /// recording the current state. Idempotent — once any row exists for
    /// the path, this no-ops and returns `Ok(false)`. Returns `Ok(true)`
    /// when a baseline was written.
    ///
    /// This is what makes rollback work for files that pre-date the
    /// changelog feature: without it, the first save of an existing
    /// `.md` file leaves only one row (the save itself), and
    /// `previous_content_for_path` returns `None` because there is no
    /// prior. The baseline captures the "before edit" state so the
    /// first rollback restores what was on disk before hiker ever
    /// touched the file.
    pub fn ensure_baseline(
        &self,
        path: &str,
        author: &str,
        content: &[u8],
        content_hash: &str,
    ) -> Result<bool, ChangesError> {
        if self.has_any_for_path(path)? {
            return Ok(false);
        }
        self.append(ChangeAppend {
            path,
            op: ChangeOp::Created,
            author,
            content_hash: Some(content_hash),
            content: Some(content),
            rename_from: None,
            metadata: serde_json::json!({"baseline": true}),
        })?;
        Ok(true)
    }

    pub fn append(&self, append: ChangeAppend<'_>) -> Result<i64, ChangesError> {
        let metadata_str = serde_json::to_string(&append.metadata)
            .map_err(|e| ChangesError::MetadataJson(e.to_string()))?;
        let ts = now_ms();
        let id = {
            let conn = self.conn.lock().expect("changes mutex poisoned");
            conn.execute(
                "INSERT INTO changes
                    (timestamp, path, op, author, content_hash, content, rename_from, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    ts,
                    append.path,
                    append.op.as_str(),
                    append.author,
                    append.content_hash,
                    append.content,
                    append.rename_from,
                    metadata_str,
                ],
            )?;
            conn.last_insert_rowid()
        };
        // Notify subscribers (Tauri bridge etc.) without holding the
        // connection mutex. Send failure when there are no receivers — fine.
        let _ = self.appended_tx.send(ChangeRow {
            id,
            timestamp_ms: ts,
            path: append.path.to_string(),
            op: append.op,
            author: append.author.to_string(),
            content_hash: append.content_hash.map(|s| s.to_string()),
            rename_from: append.rename_from.map(|s| s.to_string()),
            metadata: append.metadata,
        });
        Ok(id)
    }

    /// Most recent N rows across the whole vault, descending by id.
    pub fn recent(&self, limit: usize) -> Result<Vec<ChangeRow>, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata
             FROM changes
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Most recent N rows with `author` matching the SQL LIKE pattern. Use
    /// e.g. `"agent:%"` to fetch everything stamped by any MCP agent.
    pub fn recent_by_author(
        &self,
        author_pattern: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata
             FROM changes
             WHERE author LIKE ?1
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![author_pattern, limit as i64], map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Most recent N rows for a single path, descending. Backs the
    /// per-file history view (deferred slug) and is what rollback walks
    /// internally via `previous_content_for_path`.
    pub fn history_for_path(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata
             FROM changes
             WHERE path = ?1
             ORDER BY id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![path, limit as i64], map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Pull the content blob for a given change row. Returns `None` for
    /// `op='deleted'` rows (which carry no content) and for unknown ids.
    pub fn content_at(&self, change_id: i64) -> Result<Option<Vec<u8>>, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let blob = conn
            .query_row(
                "SELECT content FROM changes WHERE id = ?1",
                params![change_id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?;
        Ok(blob.flatten())
    }

    /// The most recent prior content for `path` strictly before `before_id`,
    /// returned as `(prior_id, prior_content)`. Used by rollback consumers:
    /// "give me what `path` looked like before change X." Skips `'deleted'`
    /// rows (they have no content); finds the last preceding row that
    /// actually carries a blob. Returns `None` when no such row exists
    /// (e.g. retention dropped it, or `before_id` is the first row for the
    /// path).
    ///
    /// status: changes-rollback-helper
    pub fn previous_content_for_path(
        &self,
        path: &str,
        before_id: i64,
    ) -> Result<Option<(i64, Vec<u8>)>, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let row = conn
            .query_row(
                "SELECT id, content FROM changes
                 WHERE path = ?1 AND id < ?2 AND content IS NOT NULL
                 ORDER BY id DESC
                 LIMIT 1",
                params![path, before_id],
                |row| {
                    let id: i64 = row.get(0)?;
                    let content: Vec<u8> = row.get(1)?;
                    Ok((id, content))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Run a retention pass. Keeps the most recent `keep_per_pair` rows per
    /// `(path, author)` combination, drops older ones. `keep_per_pair` of
    /// `-1` means unlimited (no-op). `op='deleted'` rows are preserved
    /// regardless — they're the rollback target for "undelete" until the
    /// path is fully gone (per spec). Returns the number of rows dropped.
    ///
    /// status: changes-retention
    pub fn gc(&self, keep_per_pair: i32) -> Result<usize, ChangesError> {
        if keep_per_pair < 0 {
            return Ok(0);
        }
        let keep = keep_per_pair as i64;
        let conn = self.conn.lock().expect("changes mutex poisoned");
        // Per (path, author), rank only the non-`deleted` rows by id DESC and
        // drop those past the Nth. Deleted rows are excluded from the
        // partition entirely so they never count toward the quota and are
        // never themselves dropped here (per spec they're the rollback
        // target for "undelete").
        let removed = conn.execute(
            "DELETE FROM changes WHERE id IN (
                 SELECT id FROM (
                     SELECT id,
                            ROW_NUMBER() OVER (
                                PARTITION BY path, author
                                ORDER BY id DESC
                            ) AS rn
                     FROM changes
                     WHERE op != 'deleted'
                 )
                 WHERE rn > ?1
             )",
            params![keep],
        )?;
        Ok(removed)
    }

    /// Total row count. Cheap; used by the home-page recent-activity widget
    /// to decide whether to render at all (hidden when empty).
    pub fn count(&self) -> Result<i64, ChangesError> {
        let conn = self.conn.lock().expect("changes mutex poisoned");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))?;
        Ok(n)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRow> {
    let op_str: String = row.get(3)?;
    let op = ChangeOp::parse(&op_str).unwrap_or(ChangeOp::Modified);
    let metadata_str: String = row.get(7)?;
    let metadata = serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({}));
    Ok(ChangeRow {
        id: row.get(0)?,
        timestamp_ms: row.get(1)?,
        path: row.get(2)?,
        op,
        author: row.get(4)?,
        content_hash: row.get(5)?,
        rename_from: row.get(6)?,
        metadata,
    })
}

fn configure(conn: &Connection) -> Result<(), ChangesError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), ChangesError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(ChangesError::VersionMismatch {
            found: user_version,
            expected: SCHEMA_VERSION,
        });
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS changes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            path TEXT NOT NULL,
            op TEXT NOT NULL,
            author TEXT NOT NULL,
            content_hash TEXT,
            content BLOB,
            rename_from TEXT,
            metadata TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS changes_path_ts ON changes(path, timestamp DESC);
        CREATE INDEX IF NOT EXISTS changes_author_ts ON changes(author, timestamp DESC);
        CREATE INDEX IF NOT EXISTS changes_ts ON changes(timestamp DESC);
        "#,
    )?;
    if user_version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = SCHEMA_VERSION,
            "changes: created changelog db schema",
        );
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fresh() -> (tempfile::TempDir, Changes) {
        let dir = tempdir().unwrap();
        let c = Changes::open(dir.path()).unwrap();
        (dir, c)
    }

    #[test]
    fn append_and_recent_round_trip() {
        let (_dir, c) = fresh();
        c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Created,
            author: "user",
            content_hash: Some("h1"),
            content: Some(b"hello"),
            rename_from: None,
            metadata: serde_json::json!({}),
        })
        .unwrap();
        c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some("h2"),
            content: Some(b"hello world"),
            rename_from: None,
            metadata: serde_json::json!({}),
        })
        .unwrap();

        let rows = c.recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].op, ChangeOp::Modified);
        assert_eq!(rows[1].op, ChangeOp::Created);
    }

    #[test]
    fn previous_content_for_path_returns_prior_blob() {
        let (_dir, c) = fresh();
        let id1 = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Created,
                author: "user",
                content_hash: Some("h1"),
                content: Some(b"v1"),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let id2 = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content_hash: Some("h2"),
                content: Some(b"v2"),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let id3 = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content_hash: Some("h3"),
                content: Some(b"v3"),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();

        let (prior_id, prior) =
            c.previous_content_for_path("a.md", id3).unwrap().unwrap();
        assert_eq!(prior_id, id2);
        assert_eq!(prior, b"v2");

        let (prior_id, prior) =
            c.previous_content_for_path("a.md", id2).unwrap().unwrap();
        assert_eq!(prior_id, id1);
        assert_eq!(prior, b"v1");

        // Nothing before id1.
        assert!(c.previous_content_for_path("a.md", id1).unwrap().is_none());
    }

    #[test]
    fn previous_content_skips_deleted_rows() {
        let (_dir, c) = fresh();
        let id1 = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Created,
                author: "user",
                content_hash: Some("h1"),
                content: Some(b"v1"),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let id_del = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Deleted,
                author: "user",
                content_hash: None,
                content: None,
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let (prior_id, prior) =
            c.previous_content_for_path("a.md", id_del).unwrap().unwrap();
        assert_eq!(prior_id, id1);
        assert_eq!(prior, b"v1");
    }

    #[test]
    fn gc_keeps_recent_n_per_path_author_and_preserves_deletes() {
        let (_dir, c) = fresh();
        for i in 0..5 {
            c.append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content_hash: Some("h"),
                content: Some(format!("v{i}").as_bytes()),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        }
        // A delete row should survive any per-pair retention.
        c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Deleted,
            author: "user",
            content_hash: None,
            content: None,
            rename_from: None,
            metadata: serde_json::json!({}),
        })
        .unwrap();

        let removed = c.gc(2).unwrap();
        assert_eq!(removed, 3);
        let rows = c.history_for_path("a.md", 100).unwrap();
        // 2 modified survivors + 1 deleted = 3.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|r| r.op == ChangeOp::Deleted));
    }

    #[test]
    fn recent_by_author_filters_with_like() {
        let (_dir, c) = fresh();
        c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some("h"),
            content: Some(b"x"),
            rename_from: None,
            metadata: serde_json::json!({}),
        })
        .unwrap();
        c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Modified,
            author: "agent:claude-code",
            content_hash: Some("h"),
            content: Some(b"x"),
            rename_from: None,
            metadata: serde_json::json!({}),
        })
        .unwrap();
        let agents = c.recent_by_author("agent:%", 10).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].author, "agent:claude-code");
    }

    #[test]
    fn ensure_baseline_is_idempotent_and_makes_first_rollback_work() {
        let (_dir, c) = fresh();
        // No prior rows → baseline is written.
        let wrote = c
            .ensure_baseline("a.md", "user", b"original", "h0")
            .unwrap();
        assert!(wrote);
        // Second call → no-op, no extra row.
        let wrote2 = c
            .ensure_baseline("a.md", "user", b"original", "h0")
            .unwrap();
        assert!(!wrote2);
        assert_eq!(c.history_for_path("a.md", 100).unwrap().len(), 1);

        // Simulate a save: append a Modified row. Now rollback from that
        // row should resolve to the baseline content.
        let mod_id = c
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content_hash: Some("h1"),
                content: Some(b"edited"),
                rename_from: None,
                metadata: serde_json::json!({}),
            })
            .unwrap();
        let (_prior_id, prior) =
            c.previous_content_for_path("a.md", mod_id).unwrap().unwrap();
        assert_eq!(prior, b"original");
    }

    #[test]
    fn version_mismatch_fails_loud() {
        let dir = tempdir().unwrap();
        Changes::open(dir.path()).unwrap();
        let conn = Connection::open(dir.path().join(".hiker/changes.db")).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        match Changes::open(dir.path()) {
            Err(ChangesError::VersionMismatch { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            Err(e) => panic!("expected VersionMismatch, got error {e:?}"),
            Ok(_) => panic!("expected VersionMismatch, got Ok"),
        }
    }
}
