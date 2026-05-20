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
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use ulid::Ulid;

use crate::changes::{ChangeAppend, ChangeOp, Changes, ChangesError};
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::vault::Vault;

mod ops;
mod queries;
mod recheck;

#[cfg(test)]
mod tests;

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

/// zstd compression level for the `content` BLOB. Matches `changes-content-zstd`.
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Error)]
pub enum StagingError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("proposal not found: {0}")]
    ProposalNotFound(String),
    #[error("disk drift: file changed since proposal (expected hash {expected}, found {found})")]
    DiskDrift { expected: String, found: String },
    #[error("missing content: proposal {0} has no content to write")]
    MissingContent(String),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    /// status: staging-per-edit-proposals
    /// Anchor (`old_str`) failed to resolve against current disk on accept:
    /// either zero matches (`anchor_missing`) or multiple matches without
    /// `replace_all` (`anchor_not_unique`).
    #[error("edit anchor: {0}")]
    AnchorConflict(String),
    #[error("changes error: {0}")]
    Changes(#[from] ChangesError),
    #[error("vault error: {0}")]
    Vault(String),
    #[error("connection mutex poisoned")]
    Poisoned,
}

impl From<HikerError> for StagingError {
    fn from(e: HikerError) -> Self {
        match e {
            HikerError::DiskDrift { expected, found } => StagingError::DiskDrift { expected, found },
            HikerError::Io(s) => StagingError::Io(io::Error::other(s)),
            HikerError::NotFound(s) => StagingError::ProposalNotFound(s),
            _ => StagingError::Vault(e.to_string()),
        }
    }
}

/// Patch payload for `edit_note`-shaped proposals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    pub old_str: String,
    pub new_str: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProposalInput {
    pub surface: String,
    pub action: String,
    pub target_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Hash of the target file's disk content at propose time, or `None`
    /// when the target doesn't yet exist (a create-shaped `write_note`).
    /// status: staging-proposal-state
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    /// For `action = "move_note"`: the file's current vault-relative
    /// path. `target_path` carries the destination. NULL on every
    /// non-move action.
    /// status: staging-action-move-note
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// One entry in a `propose_batch` call.
/// status: staging-per-edit-proposals
#[derive(Debug, Clone)]
pub struct EditProposalInput {
    pub surface: String,
    pub action: String,
    pub target_path: String,
    pub edit: EditPayload,
    pub content: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub source_hash: Option<String>,
}

/// status: staging-proposal-state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    #[default]
    Applyable,
    Conflicted,
}

impl ProposalState {
    fn as_str(self) -> &'static str {
        match self {
            ProposalState::Applyable => "applyable",
            ProposalState::Conflicted => "conflicted",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "conflicted" => ProposalState::Conflicted,
            _ => ProposalState::Applyable,
        }
    }
}

/// status: staging-proposal-state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    AnchorMissing,
    AnchorNotUnique,
    TargetMissing,
    HashChanged,
    /// status: staging-action-move-note
    /// `move_note` row: source file no longer exists at propose-time
    /// path.
    SourceMissing,
    /// status: staging-action-move-note
    /// `move_note` row: target path is occupied (another file landed
    /// there since the proposal was made).
    TargetOccupied,
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::AnchorMissing => "anchor_missing",
            ConflictReason::AnchorNotUnique => "anchor_not_unique",
            ConflictReason::TargetMissing => "target_missing",
            ConflictReason::HashChanged => "hash_changed",
            ConflictReason::SourceMissing => "source_missing",
            ConflictReason::TargetOccupied => "target_occupied",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "anchor_missing" => ConflictReason::AnchorMissing,
            "anchor_not_unique" => ConflictReason::AnchorNotUnique,
            "target_missing" => ConflictReason::TargetMissing,
            "hash_changed" => ConflictReason::HashChanged,
            "source_missing" => ConflictReason::SourceMissing,
            "target_occupied" => ConflictReason::TargetOccupied,
            _ => return None,
        })
    }
}

/// Stable string the producer uses for `Proposal.action` when proposing
/// a filesystem rename of a note. Centralized here so producers
/// (triage, cluster-editor multi-select) don't drift on the literal.
///
/// status: staging-action-move-note
pub const ACTION_MOVE_NOTE: &str = "move_note";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub surface: String,
    pub action: String,
    pub target_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<EditPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub state: ProposalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_reason: Option<ConflictReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    /// status: staging-action-move-note
    /// Populated only on `action = "move_note"` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StagingFilter {
    pub path: Option<String>,
    pub trail_id: Option<String>,
    pub surface: Option<String>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub state: Option<ProposalState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AcceptOutcome {
    pub proposal_id: String,
    pub target_path: String,
    pub new_hash: String,
}

/// status: staging-per-edit-proposals
#[derive(Debug, Clone, Serialize)]
pub struct BatchOutcome {
    pub batch_id: String,
    pub ids: Vec<String>,
}

/// status: staging-proposal-state
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RecheckOutcome {
    pub prior_state: ProposalState,
    pub new_state: ProposalState,
    pub new_reason: Option<ConflictReason>,
}

pub struct Staging {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    changed_tx: broadcast::Sender<()>,
}

impl Staging {
    pub fn open(vault_root: &Path) -> Result<Self, StagingError> {
        let hiker_dir = vault_root.join(".hiker");
        fs::create_dir_all(&hiker_dir)?;
        let db_path = hiker_dir.join("staging.db");

        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        ensure_schema(&mut conn)?;

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
    /// `StagingError::Poisoned` rather than panicking.
    pub(super) fn with_conn<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, StagingError>,
    ) -> Result<R, StagingError> {
        let conn = self.conn.lock().map_err(|_| StagingError::Poisoned)?;
        f(&conn)
    }

    /// Mutable counterpart of `with_conn`. Use this whenever the closure
    /// needs `conn.transaction()` (which borrows `&mut Connection`).
    pub(super) fn with_conn_mut<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, StagingError>,
    ) -> Result<R, StagingError> {
        let mut conn = self.conn.lock().map_err(|_| StagingError::Poisoned)?;
        f(&mut conn)
    }

    /// Subscribe to staging-change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed_tx.subscribe()
    }

    /// status: staging-review-activity-detail-filter
    pub fn content(&self, id: &str) -> Result<String, StagingError> {
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
        let blob = row.ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;
        match blob {
            Some(b) => Ok(String::from_utf8_lossy(&zstd::decode_all(b.as_slice())?).into_owned()),
            None => Ok(String::new()),
        }
    }
}

// ── private helpers ────────────────────────────────────────────────

impl Staging {
    fn get_full(&self, id: &str) -> Result<Option<Proposal>, StagingError> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(SELECT_FULL_BY_ID)?;
            let row = stmt
                .query_row(params![id], map_row)
                .optional()?;
            Ok(row)
        })
    }

    fn read_content(&self, id: &str) -> Result<Option<String>, StagingError> {
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

    fn delete_row(&self, id: &str) -> Result<(), StagingError> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM proposals WHERE id = ?1", params![id])?;
            Ok(())
        })
    }
}

// ── SQL constants ──────────────────────────────────────────────────

const INSERT_SQL: &str = "
    INSERT INTO proposals (
        id, surface, action, target_path, trail_id,
        content_hash, content, created_at_ms, batch_id,
        edit_old_str, edit_new_str, edit_replace_all,
        state, conflict_reason, source_hash, metadata,
        amended_at_ms, amend_count, source_path
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5,
        ?6, ?7, ?8, ?9,
        ?10, ?11, ?12,
        ?13, ?14, ?15, ?16,
        ?17, ?18, ?19
    )
";

const SELECT_COLS: &str = "
    id, surface, action, target_path, trail_id,
    content_hash, created_at_ms, batch_id,
    edit_old_str, edit_new_str, edit_replace_all,
    state, conflict_reason, source_hash, metadata,
    source_path
";

const SELECT_FULL_BY_ID: &str = "
    SELECT id, surface, action, target_path, trail_id,
           content_hash, created_at_ms, batch_id,
           edit_old_str, edit_new_str, edit_replace_all,
           state, conflict_reason, source_hash, metadata,
           source_path
    FROM proposals
    WHERE id = ?1
";

fn build_list_query(filter: &StagingFilter) -> (String, Vec<Value>) {
    let mut sql = format!("SELECT {SELECT_COLS} FROM proposals");
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(ref path) = filter.path {
        clauses.push("target_path = ?");
        params.push(Value::Text(path.clone()));
    }
    if let Some(ref trail_id) = filter.trail_id {
        clauses.push("trail_id = ?");
        params.push(Value::Text(trail_id.clone()));
    }
    if let Some(ref surface) = filter.surface {
        clauses.push("surface = ?");
        params.push(Value::Text(surface.clone()));
    }
    if let Some(ref session_id) = filter.session_id {
        clauses.push("json_extract(metadata, '$.session_id') = ?");
        params.push(Value::Text(session_id.clone()));
    }
    if let Some(state) = filter.state {
        clauses.push("state = ?");
        params.push(Value::Text(state.as_str().to_string()));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    // ORDER BY rowid (insertion order) as the tiebreaker — ULID ids share
    // a timestamp prefix but have random tails, so `id ASC` doesn't
    // preserve the order propose_batch saw inputs in (the test
    // `propose_batch_assigns_shared_batch_id_and_per_edit_payloads`
    // depends on this).
    sql.push_str(" ORDER BY created_at_ms ASC, rowid ASC");
    (sql, params)
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
    let edit_old_str: Option<String> = row.get(8)?;
    let edit_new_str: Option<String> = row.get(9)?;
    let edit_replace_all: Option<i64> = row.get(10)?;
    let edit = match (edit_old_str, edit_new_str) {
        (Some(old_str), Some(new_str)) => Some(EditPayload {
            old_str,
            new_str,
            replace_all: edit_replace_all.unwrap_or(0) != 0,
        }),
        _ => None,
    };

    let state_str: String = row.get(11)?;
    let conflict_reason_str: Option<String> = row.get(12)?;
    let metadata_str: Option<String> = row.get(14)?;
    let source_path: Option<String> = row.get(15)?;
    let metadata = metadata_str
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    Ok(Proposal {
        id: row.get(0)?,
        surface: row.get(1)?,
        action: row.get(2)?,
        target_path: row.get(3)?,
        trail_id: row.get(4)?,
        content_hash: row.get(5)?,
        created_at_ms: row.get(6)?,
        batch_id: row.get(7)?,
        edit,
        metadata,
        state: ProposalState::parse(&state_str),
        conflict_reason: conflict_reason_str.as_deref().and_then(ConflictReason::parse),
        source_hash: row.get(13)?,
        source_path,
    })
}

fn configure(conn: &Connection) -> Result<(), StagingError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn ensure_schema(conn: &mut Connection) -> Result<(), StagingError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(StagingError::VersionMismatch {
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
            -- Non-NULL only for `action = "move_note"` rows; carries the
            -- file's current vault-relative path (target_path stays the
            -- destination, same as every other row).
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
    Ok(())
}

/// status: staging-per-edit-proposals
pub fn apply_edit(content: &str, edit: &EditPayload) -> Result<String, StagingError> {
    let matches = find_all_matches(content, &edit.old_str);
    if matches.is_empty() {
        return Err(StagingError::AnchorConflict(
            "anchor_missing: old_str not found".to_string(),
        ));
    }
    if matches.len() > 1 && !edit.replace_all {
        return Err(StagingError::AnchorConflict(format!(
            "anchor_not_unique: old_str matches {} ranges; pass replace_all=true to replace all",
            matches.len(),
        )));
    }
    Ok(apply_replacements(content, &matches, &edit.new_str))
}

pub fn find_all_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let bytes = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            out.push((i, i + nb.len()));
            i += nb.len();
        } else {
            i += 1;
        }
    }
    out
}

fn apply_replacements(content: &str, ranges: &[(usize, usize)], new_str: &str) -> String {
    let mut sorted: Vec<(usize, usize)> = ranges.to_vec();
    sorted.sort_by_key(|r| r.0);
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut cursor = 0;
    for (start, end) in sorted {
        out.push_str(std::str::from_utf8(&bytes[cursor..start]).unwrap_or(""));
        out.push_str(new_str);
        cursor = end;
    }
    out.push_str(std::str::from_utf8(&bytes[cursor..]).unwrap_or(""));
    out
}

/// status: staging-proposal-state
fn derive_state(
    p: &Proposal,
    current_disk: Option<&str>,
) -> (ProposalState, Option<ConflictReason>) {
    // status: staging-action-move-note
    // move_note rows are recheckable only via `recheck_move` (which
    // has access to both source and target presence). Keep this path a
    // no-op so a stale `recheck(id, None)` against a move row doesn't
    // spuriously flip it to TargetMissing.
    if p.action == ACTION_MOVE_NOTE {
        return (p.state, p.conflict_reason);
    }
    if let Some(ref edit) = p.edit {
        let Some(disk) = current_disk else {
            return (
                ProposalState::Conflicted,
                Some(ConflictReason::TargetMissing),
            );
        };
        let matches = find_all_matches(disk, &edit.old_str);
        if matches.is_empty() {
            (
                ProposalState::Conflicted,
                Some(ConflictReason::AnchorMissing),
            )
        } else if matches.len() > 1 && !edit.replace_all {
            (
                ProposalState::Conflicted,
                Some(ConflictReason::AnchorNotUnique),
            )
        } else {
            (ProposalState::Applyable, None)
        }
    } else {
        let current_hash = current_disk.map(hash_str);
        let matches = match (&p.source_hash, &current_hash) {
            (None, None) => true,
            (Some(propose), Some(now)) => propose == now,
            _ => false,
        };
        if matches {
            (ProposalState::Applyable, None)
        } else {
            (ProposalState::Conflicted, Some(ConflictReason::HashChanged))
        }
    }
}

/// Recheck-state derivation for `action = "move_note"` rows. Drift
/// flips applyability when (a) the source file disappeared since the
/// proposal was made (`SourceMissing`) or (b) the target path is
/// already occupied (`TargetOccupied`). Both at once → SourceMissing
/// is reported first since it's the harder block.
///
/// status: staging-action-move-note
fn derive_move_state(
    source_exists: bool,
    target_exists: bool,
) -> (ProposalState, Option<ConflictReason>) {
    if !source_exists {
        (ProposalState::Conflicted, Some(ConflictReason::SourceMissing))
    } else if target_exists {
        (ProposalState::Conflicted, Some(ConflictReason::TargetOccupied))
    } else {
        (ProposalState::Applyable, None)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
