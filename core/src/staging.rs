//! Staging area for proposed writes that haven't been accepted yet.
//! See docs/settings.md "## Staging review".
//!
//! Storage at `<vault>/.hiker/staging.db`: a single SQLite database with one
//! `proposals` table. Body content lives in a zstd-compressed BLOB column
//! (same encoding as `core::changes`). Module discipline mirrors
//! `core::changes` and `core::store` — all SQLite + filesystem access
//! confined here, no Tauri imports, narrow public API.
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

/// Bumped only when the on-disk schema changes. Same fail-loud policy as
/// `core::store` and `core::changes`.
pub const SCHEMA_VERSION: i32 = 1;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::AnchorMissing => "anchor_missing",
            ConflictReason::AnchorNotUnique => "anchor_not_unique",
            ConflictReason::TargetMissing => "target_missing",
            ConflictReason::HashChanged => "hash_changed",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "anchor_missing" => ConflictReason::AnchorMissing,
            "anchor_not_unique" => ConflictReason::AnchorNotUnique,
            "target_missing" => ConflictReason::TargetMissing,
            "hash_changed" => ConflictReason::HashChanged,
            _ => return None,
        })
    }
}

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

    /// Subscribe to staging-change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed_tx.subscribe()
    }

    pub fn propose(&self, input: ProposalInput) -> Result<String, StagingError> {
        let id = Ulid::new().to_string();
        let content_hash = input.content.as_ref().map(|c| hash_str(c));
        let encoded_content: Option<Vec<u8>> = match input.content.as_ref() {
            Some(c) => Some(zstd::encode_all(c.as_bytes(), ZSTD_LEVEL)?),
            None => None,
        };
        let metadata_str = input
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        {
            let conn = self.conn.lock().expect("staging mutex poisoned");
            conn.execute(
                INSERT_SQL,
                params![
                    id,
                    input.surface,
                    input.action,
                    input.target_path,
                    input.trail_id,
                    content_hash,
                    encoded_content,
                    now_ms(),
                    Option::<String>::None,         // batch_id
                    Option::<String>::None,         // edit_old_str
                    Option::<String>::None,         // edit_new_str
                    Option::<i64>::None,            // edit_replace_all
                    ProposalState::Applyable.as_str(),
                    Option::<String>::None,         // conflict_reason
                    input.source_hash,
                    metadata_str,
                    Option::<i64>::None,            // amended_at_ms
                    0i64,                           // amend_count
                ],
            )?;
        }
        let _ = self.changed_tx.send(());
        Ok(id)
    }

    /// status: staging-per-edit-proposals
    pub fn propose_batch(
        &self,
        inputs: Vec<EditProposalInput>,
    ) -> Result<BatchOutcome, StagingError> {
        let batch_id = Ulid::new().to_string();
        let mut ids = Vec::with_capacity(inputs.len());

        let mut conn = self.conn.lock().expect("staging mutex poisoned");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(INSERT_SQL)?;
            for input in &inputs {
                let id = Ulid::new().to_string();
                let content_hash = input.content.as_ref().map(|c| hash_str(c));
                let encoded_content: Option<Vec<u8>> = match input.content.as_ref() {
                    Some(c) => Some(zstd::encode_all(c.as_bytes(), ZSTD_LEVEL)?),
                    None => None,
                };
                let metadata_str = input
                    .metadata
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?;

                stmt.execute(params![
                    id,
                    input.surface,
                    input.action,
                    input.target_path,
                    Option::<String>::None,
                    content_hash,
                    encoded_content,
                    now_ms(),
                    Some(&batch_id),
                    Some(&input.edit.old_str),
                    Some(&input.edit.new_str),
                    Some(if input.edit.replace_all { 1i64 } else { 0i64 }),
                    ProposalState::Applyable.as_str(),
                    Option::<String>::None,
                    input.source_hash,
                    metadata_str,
                    Option::<i64>::None,
                    0i64,
                ])?;
                ids.push(id);
            }
        }
        tx.commit()?;
        drop(conn);
        let _ = self.changed_tx.send(());
        Ok(BatchOutcome { batch_id, ids })
    }

    pub fn accept(
        &self,
        id: &str,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<AcceptOutcome, StagingError> {
        let proposal = self
            .get_full(id)?
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;

        let proposed_content: Option<String>;
        let new_hash: String;
        let is_create: bool;

        if let Some(ref edit) = proposal.edit {
            // status: staging-per-edit-proposals
            let (disk_content, disk_hash) =
                vault.read_file_with_hash(&proposal.target_path)?;
            // Baseline-on-first-touch: snapshot pre-write state so rollback
            // of this user-accepted agent edit has somewhere to go. Mirrors
            // `ops::agent_write_note` and `ops::commit_buffer`.
            if let Some(c) = changes {
                if let Err(e) = c.ensure_baseline(
                    &proposal.target_path,
                    "user",
                    disk_content.as_bytes(),
                    &disk_hash,
                ) {
                    tracing::warn!(error = %e, "changes: ensure_baseline failed (staging accept edit)");
                }
            }
            let applied = apply_edit(&disk_content, edit)?;
            new_hash = vault.write_file_checked(
                &proposal.target_path,
                &disk_hash,
                &applied,
            )?;
            is_create = false;
            proposed_content = Some(applied);
        } else if let Some(ref content_hash) = proposal.content_hash {
            let content = self
                .read_content(id)?
                .ok_or_else(|| StagingError::MissingContent(id.to_string()))?;

            let actual_hash = hash_str(&content);
            if &actual_hash != content_hash {
                return Err(StagingError::DiskDrift {
                    expected: content_hash.clone(),
                    found: actual_hash,
                });
            }

            let disk_read = vault.read_file_with_hash(&proposal.target_path);
            let file_exists = disk_read.is_ok();
            let (disk_text, disk_hash) =
                disk_read.unwrap_or((String::new(), String::new()));

            // Baseline-on-first-touch: snapshot pre-write state for existing
            // files so rollback of this user-accepted agent write has
            // somewhere to go. Skipped for creates — there's no prior state.
            if file_exists {
                if let Some(c) = changes {
                    if let Err(e) = c.ensure_baseline(
                        &proposal.target_path,
                        "user",
                        disk_text.as_bytes(),
                        &disk_hash,
                    ) {
                        tracing::warn!(error = %e, "changes: ensure_baseline failed (staging accept write)");
                    }
                }
            }

            new_hash = vault.write_file_checked(
                &proposal.target_path,
                &disk_hash,
                &content,
            )?;
            is_create = !file_exists;
            proposed_content = Some(content);
        } else {
            new_hash = String::new();
            is_create = false;
            proposed_content = None;
        }

        if let Some(changes) = changes {
            let op = if is_create {
                ChangeOp::Created
            } else if proposal.action == "delete_note" || proposal.action == "waypoint_remove" {
                ChangeOp::Deleted
            } else {
                ChangeOp::Modified
            };
            let content_bytes = proposed_content.as_ref().map(|c| c.as_bytes().to_vec());
            changes.append(ChangeAppend {
                path: &proposal.target_path,
                op,
                author: "user",
                content_hash: if new_hash.is_empty() {
                    None
                } else {
                    Some(&new_hash)
                },
                content: content_bytes.as_deref(),
                rename_from: None,
                metadata: {
                    let mut m = serde_json::json!({
                        "staging_proposal_id": id,
                        "action": proposal.action,
                        "reviewed": true,
                    });
                    if let Some(ref bid) = proposal.batch_id {
                        m["batch_id"] = serde_json::Value::String(bid.clone());
                    }
                    m
                },
            })?;
        }

        self.delete_row(id)?;
        let _ = self.changed_tx.send(());
        Ok(AcceptOutcome {
            proposal_id: id.to_string(),
            target_path: proposal.target_path,
            new_hash,
        })
    }

    pub fn reject(&self, id: &str) -> Result<(), StagingError> {
        let removed = {
            let conn = self.conn.lock().expect("staging mutex poisoned");
            conn.execute("DELETE FROM proposals WHERE id = ?1", params![id])?
        };
        if removed == 0 {
            return Err(StagingError::ProposalNotFound(id.to_string()));
        }
        let _ = self.changed_tx.send(());
        Ok(())
    }

    pub fn accept_all(
        &self,
        filter: &StagingFilter,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<Vec<AcceptOutcome>, StagingError> {
        let proposals = self.list(filter)?;
        let mut outcomes = Vec::new();
        for p in &proposals {
            match self.accept(&p.id, vault, changes) {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %p.id,
                        error = %e,
                        "staging: accept_all skipped failed proposal",
                    );
                }
            }
        }
        Ok(outcomes)
    }

    pub fn list(&self, filter: &StagingFilter) -> Result<Vec<Proposal>, StagingError> {
        let (sql, params) = build_list_query(filter);
        let conn = self.conn.lock().expect("staging mutex poisoned");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), map_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count(&self, filter: &StagingFilter) -> Result<u32, StagingError> {
        // Reuse the list query rather than maintaining a second SELECT —
        // proposal counts are small and the filter shape is identical.
        Ok(self.list(filter)?.len() as u32)
    }

    pub fn gc(&self, max_age_days: u32) -> Result<usize, StagingError> {
        let cutoff = now_ms() - (max_age_days as i64) * 86_400_000;
        let removed = {
            let conn = self.conn.lock().expect("staging mutex poisoned");
            conn.execute(
                "DELETE FROM proposals WHERE created_at_ms < ?1",
                params![cutoff],
            )? as usize
        };
        if removed > 0 {
            let _ = self.changed_tx.send(());
        }
        Ok(removed)
    }

    /// status: staging-proposal-state
    pub fn recheck(
        &self,
        id: &str,
        current_disk: Option<&str>,
    ) -> Result<RecheckOutcome, StagingError> {
        let proposal = self
            .get_full(id)?
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;

        let (new_state, new_reason) = derive_state(&proposal, current_disk);
        let prior_state = proposal.state;
        let prior_reason = proposal.conflict_reason;

        if prior_state == new_state && prior_reason == new_reason {
            return Ok(RecheckOutcome {
                prior_state,
                new_state,
                new_reason,
            });
        }

        {
            let conn = self.conn.lock().expect("staging mutex poisoned");
            conn.execute(
                "UPDATE proposals SET state = ?1, conflict_reason = ?2 WHERE id = ?3",
                params![new_state.as_str(), new_reason.map(|r| r.as_str()), id],
            )?;
        }
        let _ = self.changed_tx.send(());
        Ok(RecheckOutcome {
            prior_state,
            new_state,
            new_reason,
        })
    }

    /// status: staging-review-activity-detail-filter
    pub fn content(&self, id: &str) -> Result<String, StagingError> {
        let conn = self.conn.lock().expect("staging mutex poisoned");
        let row = conn
            .query_row(
                "SELECT content FROM proposals WHERE id = ?1",
                params![id],
                |row| {
                    let blob: Option<Vec<u8>> = row.get(0)?;
                    Ok(blob)
                },
            )
            .optional()?;
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
        let conn = self.conn.lock().expect("staging mutex poisoned");
        let mut stmt = conn.prepare(SELECT_FULL_BY_ID)?;
        let row = stmt
            .query_row(params![id], map_row)
            .optional()?;
        Ok(row)
    }

    fn read_content(&self, id: &str) -> Result<Option<String>, StagingError> {
        let conn = self.conn.lock().expect("staging mutex poisoned");
        let blob: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT content FROM proposals WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        match blob.flatten() {
            None => Ok(None),
            Some(b) => Ok(Some(
                String::from_utf8_lossy(&zstd::decode_all(b.as_slice())?).into_owned(),
            )),
        }
    }

    fn delete_row(&self, id: &str) -> Result<(), StagingError> {
        let conn = self.conn.lock().expect("staging mutex poisoned");
        conn.execute("DELETE FROM proposals WHERE id = ?1", params![id])?;
        Ok(())
    }
}

// ── SQL constants ──────────────────────────────────────────────────

const INSERT_SQL: &str = "
    INSERT INTO proposals (
        id, surface, action, target_path, trail_id,
        content_hash, content, created_at_ms, batch_id,
        edit_old_str, edit_new_str, edit_replace_all,
        state, conflict_reason, source_hash, metadata,
        amended_at_ms, amend_count
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5,
        ?6, ?7, ?8, ?9,
        ?10, ?11, ?12,
        ?13, ?14, ?15, ?16,
        ?17, ?18
    )
";

const SELECT_COLS: &str = "
    id, surface, action, target_path, trail_id,
    content_hash, created_at_ms, batch_id,
    edit_old_str, edit_new_str, edit_replace_all,
    state, conflict_reason, source_hash, metadata
";

const SELECT_FULL_BY_ID: &str = "
    SELECT id, surface, action, target_path, trail_id,
           content_hash, created_at_ms, batch_id,
           edit_old_str, edit_new_str, edit_replace_all,
           state, conflict_reason, source_hash, metadata
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
            amend_count      INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS proposals_target_path ON proposals(target_path);
        CREATE INDEX IF NOT EXISTS proposals_surface     ON proposals(surface);
        CREATE INDEX IF NOT EXISTS proposals_state       ON proposals(state);
        CREATE INDEX IF NOT EXISTS proposals_batch_id    ON proposals(batch_id);
        CREATE INDEX IF NOT EXISTS proposals_created_at  ON proposals(created_at_ms);
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

    fn staged() -> (tempfile::TempDir, Staging) {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn propose_returns_id_and_appears_in_list() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/test.md".into(),
                trail_id: None,
                content: Some("# Hello".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        assert!(!id.is_empty());
        let list = s.list(&StagingFilter::default()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].surface, "mcp-tool-call");
        assert!(list[0].content_hash.is_some());
    }

    #[test]
    fn propose_without_content_has_no_hash() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "trails".into(),
            action: "waypoint_add".into(),
            target_path: "notes/raptor.md".into(),
            trail_id: Some("trail-abc".into()),
            content: None,
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        let list = s.list(&StagingFilter::default()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].content_hash.is_none());
    }

    #[test]
    fn list_filters_by_path() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                path: Some("notes/a.md".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target_path, "notes/a.md");
    }

    #[test]
    fn list_filters_by_surface() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "background-llm".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                surface: Some("background-llm".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].surface, "background-llm");
    }

    #[test]
    fn list_filters_by_trail_id() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "trails".into(),
            action: "trail_create".into(),
            target_path: "trails/new-trail.md".into(),
            trail_id: Some("t1".into()),
            content: None,
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "trails".into(),
            action: "waypoint_add".into(),
            target_path: "notes/x.md".into(),
            trail_id: Some("t2".into()),
            content: None,
            metadata: None,
            source_hash: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                trail_id: Some("t1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].trail_id.as_deref(), Some("t1"));
    }

    #[test]
    fn list_filters_by_session_id_from_metadata() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: Some(serde_json::json!({"session_id": "s1"})),
            source_hash: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: Some(serde_json::json!({"session_id": "s2"})),
            source_hash: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                session_id: Some("s1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target_path, "notes/a.md");
    }

    #[test]
    fn count_returns_filtered_total() {
        let (_dir, s) = staged();
        for i in 0..5 {
            s.propose(ProposalInput {
                surface: "batch-mutation".into(),
                action: "write_note".into(),
                target_path: format!("notes/{i}.md"),
                trail_id: None,
                content: Some("x".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        }
        assert_eq!(s.count(&StagingFilter::default()).unwrap(), 5);
        assert_eq!(
            s.count(&StagingFilter {
                path: Some("notes/0.md".into()),
                ..Default::default()
            })
            .unwrap(),
            1
        );
        assert_eq!(
            s.count(&StagingFilter {
                surface: Some("nonexistent".into()),
                ..Default::default()
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn accept_writes_content_and_removes_from_pending() {
        let (dir, s) = staged();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert_eq!(outcome.proposal_id, id);
        assert_eq!(outcome.target_path, "notes/a.md");
        assert!(!outcome.new_hash.is_empty());

        let (disk_content, _) = vault.read_file_with_hash("notes/a.md").unwrap();
        assert_eq!(disk_content, "proposed");

        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn accept_metadata_only_removes_without_write() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        let id = s
            .propose(ProposalInput {
                surface: "trails".into(),
                action: "waypoint_add".into(),
                target_path: "notes/x.md".into(),
                trail_id: Some("t1".into()),
                content: None,
                metadata: None,
                source_hash: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert_eq!(outcome.proposal_id, id);
        assert!(outcome.new_hash.is_empty());
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn reject_removes_row() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("x".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();

        s.reject(&id).unwrap();
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn reject_nonexistent_returns_error() {
        let (_dir, s) = staged();
        match s.reject("nonexistent") {
            Err(StagingError::ProposalNotFound(_)) => {}
            other => panic!("expected ProposalNotFound, got {other:?}"),
        }
    }

    #[test]
    fn accept_nonexistent_returns_error() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        match s.accept("nonexistent", &vault, None) {
            Err(StagingError::ProposalNotFound(_)) => {}
            other => panic!("expected ProposalNotFound, got {other:?}"),
        }
    }

    #[test]
    fn accept_all_batches_successes_and_skips_failures() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "orig-a").unwrap();
        vault.write_file("notes/b.md", "orig-b").unwrap();

        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("new-a".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("new-b".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();

        let outcomes = s
            .accept_all(&StagingFilter::default(), &vault, None)
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn gc_removes_old_proposals() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/old.md".into(),
                trail_id: None,
                content: Some("old".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        // Backdate the row directly so the GC pass picks it up.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE proposals SET created_at_ms = 0 WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }
        let removed = s.gc(1).unwrap();
        assert_eq!(removed, 1);
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn gc_keeps_recent_proposals() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/recent.md".into(),
            trail_id: None,
            content: Some("recent".into()),
            metadata: None,
            source_hash: None,
        })
        .unwrap();
        let removed = s.gc(30).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(s.list(&StagingFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn propose_then_accept_with_changes_log() {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();
        let changes = Changes::open(dir.path()).unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, Some(&changes)).unwrap();
        assert!(!outcome.new_hash.is_empty());

        // Two rows: the pre-write baseline + the user-accepted write.
        // `recent` is newest-first, so [0] is the write and [1] is the baseline.
        let rows = changes.recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        let meta = &rows[0].metadata;
        assert_eq!(
            meta.get("staging_proposal_id").and_then(|v| v.as_str()),
            Some(id.as_str())
        );
        assert_eq!(meta.get("action").and_then(|v| v.as_str()), Some("write_note"));
        assert_eq!(meta.get("reviewed").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(rows[0].author, "user");

        let baseline_meta = &rows[1].metadata;
        assert_eq!(
            baseline_meta.get("baseline").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(rows[1].author, "user");
    }

    #[test]
    fn accept_full_write_snapshots_baseline_for_existing_file() {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "original-body").unwrap();
        let changes = Changes::open(dir.path()).unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("rewritten-body".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        s.accept(&id, &vault, Some(&changes)).unwrap();

        // Rollback target must be the pre-write body, not None: there should
        // be a baseline row whose content captures the original body.
        let rows = changes.recent(10).unwrap();
        let baseline = rows
            .iter()
            .find(|r| {
                r.metadata
                    .get("baseline")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .expect("baseline row should be present");
        let prior = changes
            .previous_content_for_path("notes/a.md", baseline.id + 1)
            .unwrap()
            .expect("baseline should provide a prior body");
        assert_eq!(prior.1, b"original-body");
    }

    #[test]
    fn accept_edit_snapshots_baseline_for_existing_file() {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "hello foo world").unwrap();
        let changes = Changes::open(dir.path()).unwrap();

        let batch = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: None,
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        s.accept(&batch.ids[0], &vault, Some(&changes)).unwrap();

        let rows = changes.recent(10).unwrap();
        let baseline = rows
            .iter()
            .find(|r| {
                r.metadata
                    .get("baseline")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .expect("baseline row should be present");
        let prior = changes
            .previous_content_for_path("notes/a.md", baseline.id + 1)
            .unwrap()
            .expect("baseline should provide a prior body");
        assert_eq!(prior.1, b"hello foo world");
    }

    #[test]
    fn accept_with_nulled_content_returns_missing_content() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        // Simulate corruption: row claims a content_hash but the BLOB has been wiped.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE proposals SET content = NULL WHERE id = ?1",
                params![id],
            )
            .unwrap();
        }

        match s.accept(&id, &vault, None) {
            Err(StagingError::MissingContent(_)) => {}
            other => panic!("expected MissingContent, got {other:?}"),
        }
    }

    #[test]
    fn accept_with_tampered_content_detects_integrity_failure() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        // Swap the BLOB for a different (but valid zstd) frame so the
        // decoded hash no longer matches the stored content_hash.
        let tampered = zstd::encode_all(&b"tampered"[..], ZSTD_LEVEL).unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE proposals SET content = ?1 WHERE id = ?2",
                params![tampered, id],
            )
            .unwrap();
        }

        match s.accept(&id, &vault, None) {
            Err(StagingError::DiskDrift { .. }) => {}
            other => panic!("expected DiskDrift, got {other:?}"),
        }
    }

    #[test]
    fn accept_create_action_works_when_file_does_not_exist() {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();

        let proposed = "# New note";
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/new.md".into(),
                trail_id: None,
                content: Some(proposed.into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert!(!outcome.new_hash.is_empty());

        let (content, _) = vault.read_file_with_hash("notes/new.md").unwrap();
        assert_eq!(content, proposed);
    }

    #[test]
    fn propose_batch_assigns_shared_batch_id_and_per_edit_payloads() {
        let (_dir, s) = staged();
        let inputs = vec![
            EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            },
            EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "baz".into(),
                    new_str: "qux".into(),
                    replace_all: false,
                },
                content: Some("qux".into()),
                metadata: None,
                source_hash: None,
            },
        ];
        let outcome = s.propose_batch(inputs).unwrap();
        assert_eq!(outcome.ids.len(), 2);
        let list = s.list(&StagingFilter::default()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].batch_id.as_deref(), Some(outcome.batch_id.as_str()));
        assert_eq!(list[1].batch_id.as_deref(), Some(outcome.batch_id.as_str()));
        assert!(list[0].edit.is_some());
        assert_eq!(list[0].edit.as_ref().unwrap().old_str, "foo");
    }

    #[test]
    fn accept_edit_row_reanchors_against_current_disk() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "hello foo world").unwrap();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        let accepted = s.accept(id, &vault, None).unwrap();
        assert!(!accepted.new_hash.is_empty());
        let (after, _) = vault.read_file_with_hash("notes/a.md").unwrap();
        assert_eq!(after, "hello bar world");
    }

    #[test]
    fn accept_edit_row_returns_anchor_conflict_when_old_str_missing() {
        let (dir, s) = staged();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "hello world").unwrap();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "missing".into(),
                    new_str: "x".into(),
                    replace_all: false,
                },
                content: Some("x".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        match s.accept(id, &vault, None) {
            Err(StagingError::AnchorConflict(_)) => {}
            other => panic!("expected AnchorConflict, got {other:?}"),
        }
    }

    #[test]
    fn apply_edit_replaces_unique_match() {
        let out = apply_edit(
            "hello foo world",
            &EditPayload {
                old_str: "foo".into(),
                new_str: "BAR".into(),
                replace_all: false,
            },
        )
        .unwrap();
        assert_eq!(out, "hello BAR world");
    }

    #[test]
    fn apply_edit_rejects_multiple_matches_without_replace_all() {
        let res = apply_edit(
            "foo foo",
            &EditPayload {
                old_str: "foo".into(),
                new_str: "x".into(),
                replace_all: false,
            },
        );
        assert!(matches!(res, Err(StagingError::AnchorConflict(_))));
    }

    #[test]
    fn apply_edit_replace_all_swaps_every_match() {
        let out = apply_edit(
            "foo foo bar",
            &EditPayload {
                old_str: "foo".into(),
                new_str: "x".into(),
                replace_all: true,
            },
        )
        .unwrap();
        assert_eq!(out, "x x bar");
    }

    // ── staging-proposal-state / staging-drift-eager-recheck ──

    #[test]
    fn recheck_edit_row_stays_applyable_when_anchor_still_unique() {
        let (_dir, s) = staged();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        let st = s.recheck(id, Some("hello foo world")).unwrap();
        assert_eq!(st.new_state, ProposalState::Applyable);
        let p = &s.list(&StagingFilter::default()).unwrap()[0];
        assert_eq!(p.state, ProposalState::Applyable);
        assert!(p.conflict_reason.is_none());
    }

    #[test]
    fn recheck_edit_row_flips_to_anchor_missing() {
        let (_dir, s) = staged();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        let st = s.recheck(id, Some("nothing here")).unwrap();
        assert_eq!(st.new_state, ProposalState::Conflicted);
        let p = &s.list(&StagingFilter::default()).unwrap()[0];
        assert_eq!(p.conflict_reason, Some(ConflictReason::AnchorMissing));
    }

    #[test]
    fn recheck_edit_row_flips_to_anchor_not_unique() {
        let (_dir, s) = staged();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        let st = s.recheck(id, Some("foo and foo again")).unwrap();
        assert_eq!(st.new_state, ProposalState::Conflicted);
        let p = &s.list(&StagingFilter::default()).unwrap()[0];
        assert_eq!(p.conflict_reason, Some(ConflictReason::AnchorNotUnique));
    }

    #[test]
    fn recheck_edit_row_target_missing_when_disk_none() {
        let (_dir, s) = staged();
        let outcome = s
            .propose_batch(vec![EditProposalInput {
                surface: "mcp-tool-call".into(),
                action: "edit_note".into(),
                target_path: "notes/a.md".into(),
                edit: EditPayload {
                    old_str: "foo".into(),
                    new_str: "bar".into(),
                    replace_all: false,
                },
                content: Some("bar".into()),
                metadata: None,
                source_hash: None,
            }])
            .unwrap();
        let id = &outcome.ids[0];
        let st = s.recheck(id, None).unwrap();
        assert_eq!(st.new_state, ProposalState::Conflicted);
        let p = &s.list(&StagingFilter::default()).unwrap()[0];
        assert_eq!(p.conflict_reason, Some(ConflictReason::TargetMissing));
    }

    #[test]
    fn recheck_write_row_applyable_when_hash_unchanged() {
        let (_dir, s) = staged();
        let propose_time = "original content";
        let source = hash_str(propose_time);
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: Some(source),
            })
            .unwrap();
        let st = s.recheck(&id, Some(propose_time)).unwrap();
        assert_eq!(st.new_state, ProposalState::Applyable);
    }

    #[test]
    fn recheck_write_row_flips_on_hash_changed() {
        let (_dir, s) = staged();
        let source = hash_str("original");
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: Some(source),
            })
            .unwrap();
        let st = s.recheck(&id, Some("drifted")).unwrap();
        assert_eq!(st.new_state, ProposalState::Conflicted);
        let p = &s.list(&StagingFilter::default()).unwrap()[0];
        assert_eq!(p.conflict_reason, Some(ConflictReason::HashChanged));
    }

    #[test]
    fn recheck_create_row_applyable_while_target_absent() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/new.md".into(),
                trail_id: None,
                content: Some("# New".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        let st = s.recheck(&id, None).unwrap();
        assert_eq!(st.new_state, ProposalState::Applyable);
        let st2 = s.recheck(&id, Some("someone wrote here first")).unwrap();
        assert_eq!(st2.new_state, ProposalState::Conflicted);
    }

    #[test]
    fn recheck_transition_broadcasts_changed_event() {
        let (_dir, s) = staged();
        let mut rx = s.subscribe();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
                source_hash: Some(hash_str("original")),
            })
            .unwrap();
        let _ = rx.try_recv();

        s.recheck(&id, Some("drifted")).unwrap();
        assert!(rx.try_recv().is_ok(), "transition should broadcast");

        s.recheck(&id, Some("drifted")).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "idempotent recheck should not broadcast"
        );
    }

    #[test]
    fn list_filters_by_state() {
        let (_dir, s) = staged();
        let id_a = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("a".into()),
                metadata: None,
                source_hash: Some(hash_str("orig")),
            })
            .unwrap();
        let _id_b = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/b.md".into(),
                trail_id: None,
                content: Some("b".into()),
                metadata: None,
                source_hash: Some(hash_str("orig")),
            })
            .unwrap();
        s.recheck(&id_a, Some("drifted")).unwrap();
        let applyable = s
            .list(&StagingFilter {
                state: Some(ProposalState::Applyable),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(applyable.len(), 1);
        let conflicted = s
            .list(&StagingFilter {
                state: Some(ProposalState::Conflicted),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(conflicted.len(), 1);
        assert_eq!(conflicted[0].id, id_a);
    }

    #[test]
    fn content_round_trips_through_zstd() {
        let (_dir, s) = staged();
        let body = "# Big note\n\n".repeat(50);
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some(body.clone()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        assert_eq!(s.content(&id).unwrap(), body);
    }

}
