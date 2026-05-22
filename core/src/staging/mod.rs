//! Staging area for proposed writes that haven't been accepted yet.
//! See docs/settings.md "## Staging review".
//!
//! Storage at `<vault>/.hiker/staging.db`: a single SQLite database with one
//! `proposals` table. Body content lives in a zstd-compressed BLOB column
//! (same encoding as `core::changes`). Module discipline mirrors
//! `core::changes` and `core::store` — all SQLite + filesystem access
//! confined here, no host imports, narrow public API.
//
// status: staging-dir
// status: staging-sqlite-store
// status: staging-review-filtering
// status: staging-retention
// status: staging-proposal-state
// status: staging-drift-eager-recheck

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::broadcast;

pub mod patch;
pub mod error;
pub mod types;

mod ops;
mod queries;

#[cfg(test)]
mod tests;

use error::Error;
use types::{map_row, Proposal, SELECT_FULL_BY_ID};

/// Bumped only when the on-disk schema changes. Same fail-loud policy as
/// `core::store` and `core::changes`.
///
/// v2 added `source_path TEXT` + `proposals_source_path` index so the
/// `move_note` action (per `staging-action-move-note`) can target a
/// rename rather than a content write. The column is NULL for write /
/// edit / tag rows; required for `action = "move_note"`. Pre-1.0
/// policy: schema bumps are handled by deleting `staging.db` (per
/// `staging-sqlite-store`); no migration code.
pub const SCHEMA_VERSION: i32 = 2;

pub struct Staging {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    changed_tx: broadcast::Sender<()>,
}

impl Staging {
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        let hiker_dir = vault_root.join(".hiker");
        fs::create_dir_all(&hiker_dir)?;
        let db_path = hiker_dir.join("staging.db");

        let conn = Connection::open(&db_path)?;
        // Per-connection PRAGMAs: WAL for concurrent reads, NORMAL fsync
        // budget (the staging DB rarely outlives a crash anyway), and a
        // generous busy timeout so the brief contention with the eager
        // recheck task doesn't surface as `Database is locked`.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        // Schema bootstrap. `user_version` mismatch is a hard error so a
        // forward-incompatible vault doesn't silently truncate state.
        {
            let user_version: i32 =
                conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if user_version != 0 && user_version != SCHEMA_VERSION {
                return Err(Error::VersionMismatch {
                    found: user_version,
                    expected: SCHEMA_VERSION,
                });
            }
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS proposals (
                    id               TEXT PRIMARY KEY,
                    surface          TEXT NOT NULL,
                    action           TEXT NOT NULL,
                    target_path      TEXT NOT NULL,
                    trail_id         TEXT,
                    content_hash     TEXT,
                    content          BLOB,
                    created_at_ms    INTEGER NOT NULL,
                    batch_id         TEXT,
                    edit_old_str     TEXT,
                    edit_new_str     TEXT,
                    edit_replace_all INTEGER,
                    state            TEXT NOT NULL DEFAULT 'applyable',
                    conflict_reason  TEXT,
                    source_hash      TEXT,
                    metadata         TEXT,
                    amended_at_ms    INTEGER,
                    amend_count      INTEGER NOT NULL DEFAULT 0,
                    -- status: staging-action-move-note
                    -- Non-NULL only for `action = "move_note"` rows;
                    -- carries the file's current vault-relative path
                    -- (target_path stays the destination, same as every
                    -- other row).
                    source_path      TEXT
                );
                CREATE INDEX IF NOT EXISTS proposals_target_path ON proposals(target_path);
                CREATE INDEX IF NOT EXISTS proposals_surface     ON proposals(surface);
                CREATE INDEX IF NOT EXISTS proposals_state       ON proposals(state);
                CREATE INDEX IF NOT EXISTS proposals_batch_id    ON proposals(batch_id);
                CREATE INDEX IF NOT EXISTS proposals_created_at  ON proposals(created_at_ms);
                CREATE INDEX IF NOT EXISTS proposals_source_path ON proposals(source_path);
                "#,
            )?;
            if user_version == 0 {
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tracing::info!(
                    schema_version = SCHEMA_VERSION,
                    "staging: created staging db schema",
                );
            }
        }

        let (changed_tx, _) = broadcast::channel(64);
        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
            changed_tx,
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Lock the connection mutex and run `f` against the shared connection.
    /// Standardizes the lock + poisoning-error mapping that every SQL call
    /// site used to spell inline; lock poisoning surfaces as
    /// `Error::Poisoned` rather than panicking.
    pub(super) fn with_conn<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        f(&conn)
    }

    /// Mutable counterpart of `with_conn`. Use this whenever the closure
    /// needs `conn.transaction()` (which borrows `&mut Connection`).
    pub(super) fn with_conn_mut<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        f(&mut conn)
    }

    /// Subscribe to staging-change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed_tx.subscribe()
    }

    /// status: staging-review-activity-detail-filter
    pub fn content(&self, id: &str) -> Result<String, Error> {
        let row = self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT content FROM proposals WHERE id = ?1",
                    params![id],
                    |row| {
                        let blob: Option<Vec<u8>> = row.get(0)?;
                        Ok(blob)
                    },
                )
                .optional()?)
        })?;
        let blob = row.ok_or_else(|| Error::ProposalNotFound(id.to_string()))?;
        match blob {
            Some(b) => Ok(String::from_utf8_lossy(&zstd::decode_all(b.as_slice())?).into_owned()),
            None => Ok(String::new()),
        }
    }
}

// ── private helpers ────────────────────────────────────────────────

impl Staging {
    fn get_full(&self, id: &str) -> Result<Option<Proposal>, Error> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(SELECT_FULL_BY_ID)?;
            let row = stmt
                .query_row(params![id], map_row)
                .optional()?;
            Ok(row)
        })
    }

    fn read_content(&self, id: &str) -> Result<Option<String>, Error> {
        let blob: Option<Option<Vec<u8>>> = self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT content FROM proposals WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        match blob.flatten() {
            None => Ok(None),
            Some(b) => Ok(Some(
                String::from_utf8_lossy(&zstd::decode_all(b.as_slice())?).into_owned(),
            )),
        }
    }

    fn delete_row(&self, id: &str) -> Result<(), Error> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM proposals WHERE id = ?1", params![id])?;
            Ok(())
        })
    }
}
