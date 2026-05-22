//! Proposal data model for the staging area: the input shapes producers
//! submit, the stored `Proposal` row and its enums, query filters and
//! operation outcomes, plus the SQL column lists and row-mapping that bind
//! those structs to the `proposals` table. Shared by the staging root and
//! every operation split (`ops`, `queries`).

use serde::{Deserialize, Serialize};

use super::patch::EditPayload;

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
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            ProposalState::Applyable => "applyable",
            ProposalState::Conflicted => "conflicted",
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
    pub const fn as_str(self) -> &'static str {
        match self {
            ConflictReason::AnchorMissing => "anchor_missing",
            ConflictReason::AnchorNotUnique => "anchor_not_unique",
            ConflictReason::TargetMissing => "target_missing",
            ConflictReason::HashChanged => "hash_changed",
            ConflictReason::SourceMissing => "source_missing",
            ConflictReason::TargetOccupied => "target_occupied",
        }
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
pub struct Filter {
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

/// zstd compression level for the `content` BLOB. Matches `changes-content-zstd`.
pub(super) const ZSTD_LEVEL: i32 = 3;

pub(super) const INSERT_SQL: &str = "
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

pub(super) const SELECT_COLS: &str = "
    id, surface, action, target_path, trail_id,
    content_hash, created_at_ms, batch_id,
    edit_old_str, edit_new_str, edit_replace_all,
    state, conflict_reason, source_hash, metadata,
    source_path
";

pub(super) const SELECT_FULL_BY_ID: &str = "
    SELECT id, surface, action, target_path, trail_id,
           content_hash, created_at_ms, batch_id,
           edit_old_str, edit_new_str, edit_replace_all,
           state, conflict_reason, source_hash, metadata,
           source_path
    FROM proposals
    WHERE id = ?1
";

pub(super) fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
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
        state: match state_str.as_str() {
            "conflicted" => ProposalState::Conflicted,
            _ => ProposalState::Applyable,
        },
        conflict_reason: conflict_reason_str.as_deref().and_then(|s| {
            Some(match s {
                "anchor_missing" => ConflictReason::AnchorMissing,
                "anchor_not_unique" => ConflictReason::AnchorNotUnique,
                "target_missing" => ConflictReason::TargetMissing,
                "hash_changed" => ConflictReason::HashChanged,
                "source_missing" => ConflictReason::SourceMissing,
                "target_occupied" => ConflictReason::TargetOccupied,
                _ => return None,
            })
        }),
        source_hash: row.get(13)?,
        source_path,
    })
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
