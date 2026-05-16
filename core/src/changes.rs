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
/// `store-version-fail-loud`: a mismatch is an error, not a migration —
/// *except* for paths the changelog can't recover from disk regeneration.
/// v1 → v2 (zstd content) is migrated in place at open per
/// `changes-content-zstd`.
pub const SCHEMA_VERSION: i32 = 2;

/// zstd compression level for the `content` BLOB. Level 3 is zstd's default
/// and matches the spec — fast encode, near-best ratio for prose; higher
/// levels gain <10% for 3× the encode time.
const ZSTD_LEVEL: i32 = 3;

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
    /// zstd decode failure on a stored `content` BLOB. Carries the row id
    /// and the post-op `content_hash` so the caller can correlate against
    /// the activity feed without re-reading the row.
    #[error("corrupt content for change {id} (content_hash={content_hash:?}): {message}")]
    Corrupt {
        id: i64,
        content_hash: Option<String>,
        message: String,
    },
    #[error("connection mutex poisoned")]
    Poisoned,
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
    /// True when this row is the most recent change for its `path` across
    /// the whole changelog (i.e. its `id` is `MAX(id)` partitioned by
    /// path). Computed by the SQL query, not over a paginated client-side
    /// window — so the "current" badge stays correct even when an older
    /// version pages out. The append-only `appended_tx` broadcast leaves
    /// this field at `false`; subscribers re-fetching via `recent` /
    /// `history_for_path` get the up-to-date value.
    #[serde(default)]
    pub is_current: bool,
    /// Coarse author classification derived from `author`. The wire format
    /// of `author` is `class[:identifier]`; UIs and filter pills only ever
    /// need the class half, so it's surfaced as a typed enum here rather
    /// than re-parsed at every call site. Stays in sync with `author` by
    /// being computed in `map_row` whenever a row is loaded.
    #[serde(default)]
    pub author_class: AuthorClass,
}

/// Coarse author taxonomy from `design.md`'s authorship trichotomy
/// (user / agent / sync / import). The wire format of `ChangeRow.author`
/// is `class[:identifier]` — e.g. `agent:claude-code`, `sync:phone`. This
/// enum carries the class half typed so consumers don't have to parse the
/// string. `Other` is a forward-compat slot for unknown future classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthorClass {
    #[default]
    User,
    Agent,
    Sync,
    Import,
    /// Auto-accepted staging proposal (no user gate at accept time).
    /// Wire form is `auto:<producer>` — e.g. `auto:triage` for the saved-tree
    /// triage classifier (per `suggestions.md` `triage-author-class`). The
    /// `auto:*` prefix is reserved for genuinely-unattended writes.
    Auto,
    Other,
}

impl AuthorClass {
    /// Parse the class prefix from a wire-format `author` string.
    pub fn from_author(author: &str) -> Self {
        let class = match author.find(':') {
            Some(i) => &author[..i],
            None => author,
        };
        match class {
            "user" => AuthorClass::User,
            "agent" => AuthorClass::Agent,
            "sync" => AuthorClass::Sync,
            "import" => AuthorClass::Import,
            "auto" => AuthorClass::Auto,
            _ => AuthorClass::Other,
        }
    }
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
        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        ensure_schema(&mut conn)?;
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

    /// Lock the connection mutex and run `f` against the shared connection.
    /// Standardizes the lock + poisoning-error mapping that every SQL call
    /// site used to spell inline; lock poisoning surfaces as
    /// `ChangesError::Poisoned` rather than panicking.
    fn with_conn<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, ChangesError>,
    ) -> Result<R, ChangesError> {
        let conn = self.conn.lock().map_err(|_| ChangesError::Poisoned)?;
        f(&conn)
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
        let n: i64 = self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM changes WHERE path = ?1 LIMIT 1",
                params![path],
                |row| row.get(0),
            )?)
        })?;
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
        // status: changes-content-zstd
        // Encode the content blob at append time. Deleted rows pass content=None
        // and stay NULL; everything else (including empty bodies) goes through
        // zstd::encode_all so reads can decode uniformly.
        let encoded: Option<Vec<u8>> = match append.content {
            Some(bytes) => Some(zstd::encode_all(bytes, ZSTD_LEVEL)?),
            None => None,
        };
        let id = self.with_conn(|conn| {
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
                    encoded.as_deref(),
                    append.rename_from,
                    metadata_str,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })?;
        // Notify subscribers (Tauri bridge etc.) without holding the
        // connection mutex. Send failure when there are no receivers — fine.
        let author_class = AuthorClass::from_author(append.author);
        let _ = self.appended_tx.send(ChangeRow {
            id,
            timestamp_ms: ts,
            path: append.path.to_string(),
            op: append.op,
            author: append.author.to_string(),
            content_hash: append.content_hash.map(|s| s.to_string()),
            rename_from: append.rename_from.map(|s| s.to_string()),
            metadata: append.metadata,
            // A freshly appended row is by definition the current state for
            // its path. Re-fetches via `recent` will compute this from SQL.
            is_current: true,
            author_class,
        });
        Ok(id)
    }

    /// Most recent N rows across the whole vault, descending by id.
    pub fn recent(&self, limit: usize) -> Result<Vec<ChangeRow>, ChangesError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(LIST_SQL_BY_ID)?;
            let rows = stmt
                .query_map(params![limit as i64], map_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Most recent N rows with `author` matching the SQL LIKE pattern. Use
    /// e.g. `"agent:%"` to fetch everything stamped by any MCP agent.
    pub fn recent_by_author(
        &self,
        author_pattern: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, ChangesError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(LIST_SQL_BY_AUTHOR)?;
            let rows = stmt
                .query_map(params![author_pattern, limit as i64], map_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Most recent N rows for a single path, descending. Backs the
    /// per-file history view (deferred slug) and is what rollback walks
    /// internally via `previous_content_for_path`.
    pub fn history_for_path(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, ChangesError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(LIST_SQL_BY_PATH)?;
            let rows = stmt
                .query_map(params![path, limit as i64], map_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    // status: note-properties-tab-content
    /// Count of changes rows for a single path. Used by the
    /// `hiker_core::store::note_properties` response (changes section).
    pub fn count_for_path(&self, path: &str) -> Result<i64, ChangesError> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM changes WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )?)
        })
    }

    /// Pull the content blob for a given change row. Returns `None` for
    /// `op='deleted'` rows (which carry no content) and for unknown ids.
    /// status: changes-content-zstd — decodes the stored zstd frame
    /// transparently; consumers see plaintext bytes.
    pub fn content_at(&self, change_id: i64) -> Result<Option<Vec<u8>>, ChangesError> {
        let row = self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT content, content_hash FROM changes WHERE id = ?1",
                    params![change_id],
                    |row| {
                        let blob: Option<Vec<u8>> = row.get(0)?;
                        let content_hash: Option<String> = row.get(1)?;
                        Ok((blob, content_hash))
                    },
                )
                .optional()?)
        })?;
        let Some((blob, content_hash)) = row else {
            return Ok(None);
        };
        match blob {
            Some(b) => Ok(Some(decode_blob(change_id, &content_hash, &b)?)),
            None => Ok(None),
        }
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
        let row = self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, content, content_hash FROM changes
                     WHERE path = ?1 AND id < ?2 AND content IS NOT NULL
                     ORDER BY id DESC
                     LIMIT 1",
                    params![path, before_id],
                    |row| {
                        let id: i64 = row.get(0)?;
                        let content: Vec<u8> = row.get(1)?;
                        let content_hash: Option<String> = row.get(2)?;
                        Ok((id, content, content_hash))
                    },
                )
                .optional()?)
        })?;
        match row {
            None => Ok(None),
            Some((id, blob, hash)) => Ok(Some((id, decode_blob(id, &hash, &blob)?))),
        }
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
        // Per (path, author), rank only the non-`deleted` rows by id DESC and
        // drop those past the Nth. Deleted rows are excluded from the
        // partition entirely so they never count toward the quota and are
        // never themselves dropped here (per spec they're the rollback
        // target for "undelete").
        let removed = self.with_conn(|conn| {
            Ok(conn.execute(
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
            )?)
        })?;
        Ok(removed)
    }

    /// Total row count. Cheap; used by the home-page recent-activity widget
    /// to decide whether to render at all (hidden when empty).
    pub fn count(&self) -> Result<i64, ChangesError> {
        self.with_conn(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))?)
        })
    }
}

/// Shared SELECT shape that joins `is_current` (id == MAX(id) partitioned
/// by path) onto every row. Correlated subquery is fine — the
/// `changes_path_ts` index makes the per-path MAX cheap.
const LIST_SQL_BY_ID: &str = "
    SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata,
           id = (SELECT MAX(id) FROM changes c2 WHERE c2.path = changes.path) AS is_current
    FROM changes
    ORDER BY id DESC
    LIMIT ?1
";

const LIST_SQL_BY_AUTHOR: &str = "
    SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata,
           id = (SELECT MAX(id) FROM changes c2 WHERE c2.path = changes.path) AS is_current
    FROM changes
    WHERE author LIKE ?1
    ORDER BY id DESC
    LIMIT ?2
";

const LIST_SQL_BY_PATH: &str = "
    SELECT id, timestamp, path, op, author, content_hash, rename_from, metadata,
           id = (SELECT MAX(id) FROM changes c2 WHERE c2.path = changes.path) AS is_current
    FROM changes
    WHERE path = ?1
    ORDER BY id DESC
    LIMIT ?2
";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRow> {
    let op_str: String = row.get(3)?;
    let op = ChangeOp::parse(&op_str).unwrap_or(ChangeOp::Modified);
    let metadata_str: String = row.get(7)?;
    let metadata = serde_json::from_str(&metadata_str).unwrap_or(serde_json::json!({}));
    let is_current: i64 = row.get(8).unwrap_or(0);
    let author: String = row.get(4)?;
    let author_class = AuthorClass::from_author(&author);
    Ok(ChangeRow {
        id: row.get(0)?,
        timestamp_ms: row.get(1)?,
        path: row.get(2)?,
        op,
        author,
        content_hash: row.get(5)?,
        rename_from: row.get(6)?,
        metadata,
        is_current: is_current != 0,
        author_class,
    })
}

fn configure(conn: &Connection) -> Result<(), ChangesError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn ensure_schema(conn: &mut Connection) -> Result<(), ChangesError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    // status: changes-content-zstd
    // v1 → v2: re-encode every existing `content` BLOB with zstd. Pre-bump
    // rows held raw bytes; post-bump rows hold a zstd frame. One-shot
    // migration in a single transaction so a mid-run crash rolls back to v1
    // and the next open retries.
    if user_version == 1 {
        migrate_v1_to_v2(conn)?;
    } else if user_version != 0 && user_version != SCHEMA_VERSION {
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

/// In-place v1 → v2 migration: walk every row with non-NULL content,
/// re-encode the raw bytes with zstd, write back, bump `user_version`.
/// Whole pass in one transaction so a crash rolls back atomically.
fn migrate_v1_to_v2(conn: &mut Connection) -> Result<(), ChangesError> {
    tracing::info!("changes: migrating v1 → v2 (zstd-encoding content blobs)");
    let tx = conn.transaction()?;
    let pairs: Vec<(i64, Vec<u8>)> = {
        let mut stmt = tx.prepare(
            "SELECT id, content FROM changes WHERE content IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut count: usize = 0;
    {
        let mut update = tx.prepare("UPDATE changes SET content = ?1 WHERE id = ?2")?;
        for (id, raw) in pairs {
            let encoded = zstd::encode_all(raw.as_slice(), ZSTD_LEVEL)?;
            update.execute(params![encoded, id])?;
            count += 1;
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    tracing::info!(
        rows = count,
        schema_version = SCHEMA_VERSION,
        "changes: migration complete",
    );
    Ok(())
}

/// Decode a stored zstd frame to plaintext. Failures surface as
/// `ChangesError::Corrupt` carrying the row id + content_hash so the
/// caller can correlate against the activity feed without re-reading.
fn decode_blob(
    id: i64,
    content_hash: &Option<String>,
    blob: &[u8],
) -> Result<Vec<u8>, ChangesError> {
    zstd::decode_all(blob).map_err(|e| ChangesError::Corrupt {
        id,
        content_hash: content_hash.clone(),
        message: e.to_string(),
    })
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
        // Most recent row for `a.md` is the Modified one; the Created row
        // is no longer current.
        assert!(rows[0].is_current);
        assert!(!rows[1].is_current);
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
    fn author_class_parses_auto_prefix() {
        assert_eq!(
            AuthorClass::from_author("auto:triage"),
            AuthorClass::Auto
        );
        assert_eq!(
            AuthorClass::from_author("auto:cluster-editor"),
            AuthorClass::Auto
        );
        // Bare "auto" with no producer is still the class.
        assert_eq!(AuthorClass::from_author("auto"), AuthorClass::Auto);
        // Sanity: existing classes still parse.
        assert_eq!(AuthorClass::from_author("user"), AuthorClass::User);
        assert_eq!(
            AuthorClass::from_author("agent:claude-code"),
            AuthorClass::Agent
        );
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
    fn content_round_trip_through_zstd() {
        let (_dir, c) = fresh();
        // status: changes-content-zstd
        // Markdown body big enough to actually exercise the codec rather than
        // bottom out in zstd's framing overhead.
        let body = "# Heading\n\n".to_string()
            + &"the quick brown fox jumps over the lazy dog. ".repeat(40);
        let id = c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some("h"),
            content: Some(body.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({}),
        }).unwrap();
        let blob = c.content_at(id).unwrap().expect("content present");
        assert_eq!(blob, body.as_bytes(), "content round-trips through zstd");

        // Sanity-check that what's actually on disk is *not* the plaintext —
        // i.e. that we really compressed rather than no-op'd.
        let conn = Connection::open(c.db_path()).unwrap();
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT content FROM changes WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(raw != body.as_bytes(), "content should be compressed on disk");
        assert!(raw.len() < body.len(), "compressed < raw");
    }

    #[test]
    fn deleted_rows_keep_null_content() {
        let (_dir, c) = fresh();
        let id = c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Deleted,
            author: "user",
            content_hash: None,
            content: None,
            rename_from: None,
            metadata: serde_json::json!({}),
        }).unwrap();
        assert!(c.content_at(id).unwrap().is_none());
    }

    #[test]
    fn empty_content_round_trips() {
        let (_dir, c) = fresh();
        let id = c.append(ChangeAppend {
            path: "a.md",
            op: ChangeOp::Created,
            author: "user",
            content_hash: Some(&crate::hash_str("")),
            content: Some(&[]),
            rename_from: None,
            metadata: serde_json::json!({}),
        }).unwrap();
        let blob = c.content_at(id).unwrap().expect("empty content present");
        assert_eq!(blob.len(), 0);
    }

    #[test]
    fn v1_to_v2_migration_reencodes_existing_rows() {
        // Hand-build a v1 db: raw bytes in `content`, user_version = 1.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(".hiker/changes.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE changes (
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
            "#,
        ).unwrap();
        let body_a = b"first body".to_vec();
        let body_b = b"second body, slightly different".to_vec();
        conn.execute(
            "INSERT INTO changes (timestamp, path, op, author, content_hash, content, metadata)
             VALUES (?1, 'a.md', 'created', 'user', 'h1', ?2, '{}')",
            params![1i64, body_a],
        ).unwrap();
        conn.execute(
            "INSERT INTO changes (timestamp, path, op, author, content_hash, content, metadata)
             VALUES (?1, 'b.md', 'modified', 'user', 'h2', ?2, '{}')",
            params![2i64, body_b],
        ).unwrap();
        // Deleted row, NULL content — must survive migration untouched.
        conn.execute(
            "INSERT INTO changes (timestamp, path, op, author, metadata)
             VALUES (?1, 'gone.md', 'deleted', 'user', '{}')",
            params![3i64],
        ).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        drop(conn);

        // Open through `Changes::open` — runs the migration.
        let c = Changes::open(dir.path()).unwrap();

        // Reads return plaintext.
        let row_a = c.history_for_path("a.md", 10).unwrap()[0].clone();
        let row_b = c.history_for_path("b.md", 10).unwrap()[0].clone();
        let row_d = c.history_for_path("gone.md", 10).unwrap()[0].clone();
        assert_eq!(c.content_at(row_a.id).unwrap().unwrap(), b"first body");
        assert_eq!(
            c.content_at(row_b.id).unwrap().unwrap(),
            b"second body, slightly different",
        );
        assert!(c.content_at(row_d.id).unwrap().is_none());

        // user_version flipped to 2 so the next open is a no-op.
        let conn = Connection::open(&db_path).unwrap();
        let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
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
