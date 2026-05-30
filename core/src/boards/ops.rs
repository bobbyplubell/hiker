//! Board ops: mutation verbs (create / move-card / add-card / remove-card,
//! column add/rename/reorder/delete) plus the path-remap surface invoked
//! from the indexer on note moves.
//!
//! Every write to a board-doc goes through `core::ops::op_writes::user_save`
//! (`op-log-ops-producer-helpers`) — a board move is an ordinary versioned,
//! undoable, syncable user edit, and the referenced notes are NEVER
//! mutated. Adding a card is a frontmatter edit only: under path-as-
//! identity (`board-card-references`) the card holds just the note's
//! vault path; no ULID is minted or stamped.

use serde::{Deserialize, Serialize};

use crate::config::sections::BoardsConfig;
use crate::errors::HikerError;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::oplog::OpLog;
use crate::store::dto::new_id;
use crate::vault::Vault;
use crate::watcher::Watcher;

use crate::boards::BoardCard;

use super::{parse_board_for, write_board_frontmatter, Board, Column};

/// Outcome of a successful `create_board` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBoardOutcome {
    pub board_doc_rel: String,
    pub board_id: String,
}

/// Default columns a fresh board ships with (editable afterward). Per
/// `docs/kanban.md` §"Creating a board".
const DEFAULT_COLUMNS: &[&str] = &["Todo", "Doing", "Done"];

/// Create a new board. Mints a ULID, writes the board-doc to
/// `<new_board_dir>/<name>.md` (auto-suffixed on collision), seeds the
/// default `Todo` / `Doing` / `Done` columns (each empty), and re-indexes
/// the board-doc. Mirrors `core::trails::ops::create_trail`.
///
/// `name` is used verbatim as the basename; the function appends `-N.md`
/// (1..1000) only on a collision.
///
/// The board-doc is created on disk via `vault.write_file` (not the op-log
/// path) so the file exists before its first card-move; the op log adopts
/// it on its first `user_save`. Watcher suppression brackets the write.
///
/// status: board-create
/// status: board-default-location
pub async fn create_board(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    log: &OpLog,
    vault: &Vault,
    config: &BoardsConfig,
    name: &str,
) -> Result<CreateBoardOutcome, HikerError> {
    let plan = plan_new_board(vault, config, name)?;

    watcher.suppress(plan.board_doc_rel.clone());
    vault.write_file(&plan.board_doc_rel, &plan.body)?;
    watcher.suppress(plan.board_doc_rel.clone());

    // status: store-id-from-oplog
    // Seed the op-log document so the board has a stable storage key
    // (its `doc_id`) before any cards are added.
    let board_id =
        crate::ops::op_writes::doc_id_or_seed(log, vault, &plan.board_doc_rel, &plan.body)?;

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: plan.board_doc_rel.clone(),
            force: false,
        })
        .await;

    Ok(CreateBoardOutcome {
        board_doc_rel: plan.board_doc_rel,
        board_id,
    })
}

/// A planned-but-unwritten new board: the chosen vault-relative path
/// and the seed body. Bundled so [`plan_new_board`] can feed both the
/// direct `create_board` write and the MCP review-mode whole-file stage.
/// The board's storage `doc_id` is sourced from the op-log after the
/// write — under `board-doc-shape` the board-doc carries no id field.
pub struct NewBoardPlan {
    pub board_doc_rel: String,
    pub body: String,
}

/// Compute a new board's path + body WITHOUT writing: ensures
/// `new_board_dir` exists, seeds the default columns, and resolves the
/// first free `<dir>/<name>.md` (auto-suffixing `-N` on collision). The
/// direct `create_board` writes the result; the MCP review path stages
/// it as a whole-file create. Picking the path is filesystem-read-only
/// (it probes `abs.exists()`); the actual create is the caller's.
///
/// status: board-mcp-tools
pub fn plan_new_board(
    vault: &Vault,
    config: &BoardsConfig,
    name: &str,
) -> Result<NewBoardPlan, HikerError> {
    let folder = config.new_board_dir.trim_end_matches('/');
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create new_board_dir: {e}")))?;
        }
    }

    let board = Board {
        columns: DEFAULT_COLUMNS
            .iter()
            .map(|n| Column {
                name: (*n).to_string(),
                cards: Vec::new(),
                wip_limit: None,
            })
            .collect(),
    };
    // Seed the body from the bare frontmatter assembled off the default
    // board — `write_board_frontmatter("", &board)` produces a clean
    // frontmatter block with an empty body.
    let body = write_board_frontmatter("", &board)
        .map_err(|e| HikerError::Io(format!("seed board-doc: {e}")))?;

    let candidate_at = |n: Option<u32>| -> String {
        let stem = match n {
            None => name.to_string(),
            Some(n) => format!("{name}-{n}"),
        };
        if folder.is_empty() {
            format!("{stem}.md")
        } else {
            format!("{folder}/{stem}.md")
        }
    };
    let mut chosen: Option<String> = None;
    for attempt in std::iter::once(None).chain((1..1000).map(Some)) {
        let candidate = candidate_at(attempt);
        if !vault.abs_path(&candidate)?.exists() {
            chosen = Some(candidate);
            break;
        }
    }
    let board_doc_rel = chosen
        .ok_or_else(|| HikerError::AlreadyExists(format!("ran out of {name}-N candidates")))?;

    Ok(NewBoardPlan {
        board_doc_rel,
        body,
    })
}

/// Persist a mutated board-doc through the op-log user-save path, then
/// enqueue a re-index so the derived `board_cards` rows re-derive. The
/// op-log diffs the new frontmatter against the current accepted state into
/// localized spans — concurrent moves of different cards merge, same-card
/// moves surface as conflict hunks (`op-log-merge-conflict`). The op-log
/// writes the materialized `.md` itself, so we do NOT also `write_file`.
///
/// status: board-move
async fn persist_board(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    src: &str,
    board: &Board,
) -> Result<(), HikerError> {
    let new_src = write_board_frontmatter(src, board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))?;
    crate::ops::op_writes::user_save(log, vault, board_doc_rel, &new_src)?;
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: board_doc_rel.to_string(),
            force: false,
        })
        .await;
    Ok(())
}

/// Read + parse the board-doc, returning `(source, board)`.
fn read_board(vault: &Vault, board_doc_rel: &str) -> Result<(String, Board), HikerError> {
    let src = vault.read_file(board_doc_rel)?;
    let board = parse_board_for(board_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse board-doc: {e}")))?;
    Ok((src, board))
}

/// The varying half of a `move_card` call: which card moves, out of which
/// column, into which column at which index. The environment handles
/// (log / jobs / vault / board path) are passed separately. Bundling
/// keeps `move_card` under the argument-count budget.
///
/// `card_handle` names the card to move: a note card's vault path under
/// path-as-identity (`board-card-references`) or a freeform card's
/// `card_id`. The lookup tries both — there's no ambiguity since the two
/// strings live in disjoint shapes.
pub struct MoveCardRequest<'a> {
    pub board_doc_rel: &'a str,
    pub from_column: &'a str,
    pub card_handle: &'a str,
    pub to_column: &'a str,
    /// Target index in the destination column; clamps to the tail when
    /// out of range (pass `usize::MAX` to append).
    pub to_index: usize,
}

/// True if `card` matches the polymorphic `handle`: a note card matches
/// when `handle == path`; a freeform card matches when
/// `handle == card_id`. Disambiguating by shape lets callers pass a
/// single string regardless of which kind of card they're naming.
fn card_matches(card: &BoardCard, handle: &str) -> bool {
    match card {
        BoardCard::Note { path } => path == handle,
        BoardCard::Text { card_id, .. } => card_id == handle,
    }
}

/// A single in-memory mutation of a parsed `Board`. The one source of truth
/// for what each board verb changes: the commit-path verbs apply it then
/// `persist_board`, and the MCP review path applies it then assembles the
/// new board-doc source via [`preview_edit`] WITHOUT writing. Sharing this
/// step means the staged text and the directly-written text can't diverge.
///
/// `add_card` (note card) is deliberately absent — it needs `&mut Store` for
/// ULID stamping, which can't ride this pure step; it keeps its own preview
/// (`crate::boards::add_card_preview`).
///
/// status: board-mcp-tools
pub enum BoardEdit<'a> {
    /// Append a freeform text card to a column; mints `card_id`.
    AddTextCard { column: &'a str, card_id: String, text: &'a str },
    /// Move/reorder a card between (or within) columns.
    MoveCard(&'a MoveCardRequest<'a>),
    /// Rewrite a freeform card's text in place (errors on a note card).
    SetCardText { card_id: &'a str, text: &'a str },
    /// Drop a card from the board (referenced note untouched). `handle`
    /// is the card's vault path (note card) or `card_id` (freeform).
    RemoveCard { handle: &'a str },
    /// Append a new empty column at the tail (idempotent on name collision).
    AddColumn { name: &'a str },
    /// Rename a column in place; cards keep their order/membership.
    RenameColumn { old_name: &'a str, new_name: &'a str },
    /// Move a column to a new index (clamps to the tail).
    ReorderColumn { name: &'a str, to_index: usize },
    /// Delete a column (drops its card refs; notes untouched).
    DeleteColumn { name: &'a str },
}

/// Apply a [`BoardEdit`] to a parsed board in place. Returns `false` when the
/// edit was an idempotent no-op (`AddColumn` of an existing name), so callers
/// can skip a redundant write/stage. The single pure mutation step both the
/// commit verbs and the MCP preview path run.
///
/// status: board-mcp-tools
pub fn apply_edit(board: &mut Board, edit: &BoardEdit) -> Result<bool, HikerError> {
    match edit {
        BoardEdit::AddTextCard { column, card_id, text } => {
            let col_idx = board
                .column_index(column)
                .ok_or_else(|| HikerError::NotFound(format!("column: {column}")))?;
            board.columns[col_idx].cards.push(BoardCard::Text {
                card_id: card_id.clone(),
                text: (*text).to_string(),
            });
        }
        BoardEdit::MoveCard(req) => {
            let from_idx = board
                .column_index(req.from_column)
                .ok_or_else(|| HikerError::NotFound(format!("column: {}", req.from_column)))?;
            let card_pos = board.columns[from_idx]
                .cards
                .iter()
                .position(|c| card_matches(c, req.card_handle))
                .ok_or_else(|| {
                    HikerError::NotFound(format!("card: {}", req.card_handle))
                })?;
            let card = board.columns[from_idx].cards.remove(card_pos);
            let to_idx = board
                .column_index(req.to_column)
                .ok_or_else(|| HikerError::NotFound(format!("column: {}", req.to_column)))?;
            let dest = &mut board.columns[to_idx].cards;
            let insert_at = req.to_index.min(dest.len());
            dest.insert(insert_at, card);
        }
        BoardEdit::SetCardText { card_id, text } => {
            let card = board
                .columns
                .iter_mut()
                .flat_map(|col| col.cards.iter_mut())
                .find(|c| matches!(c, BoardCard::Text { card_id: cid, .. } if cid == *card_id))
                .ok_or_else(|| HikerError::NotFound(format!("card id: {card_id}")))?;
            if let BoardCard::Text { text: t, .. } = card {
                *t = (*text).to_string();
            }
        }
        BoardEdit::RemoveCard { handle } => {
            let removed = board.columns.iter_mut().any(|col| {
                col.cards
                    .iter()
                    .position(|c| card_matches(c, handle))
                    .map(|pos| col.cards.remove(pos))
                    .is_some()
            });
            if !removed {
                return Err(HikerError::NotFound(format!("card: {handle}")));
            }
        }
        BoardEdit::AddColumn { name } => {
            if board.column_index(name).is_some() {
                return Ok(false);
            }
            board.columns.push(Column {
                name: (*name).to_string(),
                cards: Vec::new(),
                wip_limit: None,
            });
        }
        BoardEdit::RenameColumn { old_name, new_name } => {
            let idx = board
                .column_index(old_name)
                .ok_or_else(|| HikerError::NotFound(format!("column: {old_name}")))?;
            board.columns[idx].name = (*new_name).to_string();
        }
        BoardEdit::ReorderColumn { name, to_index } => {
            let idx = board
                .column_index(name)
                .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
            let col = board.columns.remove(idx);
            let insert_at = (*to_index).min(board.columns.len());
            board.columns.insert(insert_at, col);
        }
        BoardEdit::DeleteColumn { name } => {
            let idx = board
                .column_index(name)
                .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
            board.columns.remove(idx);
        }
    }
    Ok(true)
}

/// Compute the board-doc source after moving the card `card_id` to
/// `to_column` at `to_index` (tail when `None`), WITHOUT writing. The MCP
/// `board_move_card` tool names the card by id only (no source column), so
/// this resolves the current column off the parsed board, then runs the
/// shared [`apply_edit`] `MoveCard` step. Returns the new board-doc source.
///
/// status: board-mcp-tools
pub fn preview_move_card(
    vault: &Vault,
    board_doc_rel: &str,
    card_handle: &str,
    to_column: &str,
    to_index: Option<usize>,
) -> Result<String, HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let from_column = board
        .columns
        .iter()
        .find(|col| col.cards.iter().any(|c| card_matches(c, card_handle)))
        .map(|col| col.name.clone())
        .ok_or_else(|| HikerError::NotFound(format!("card: {card_handle}")))?;
    let req = MoveCardRequest {
        board_doc_rel,
        from_column: &from_column,
        card_handle,
        to_column,
        to_index: to_index.unwrap_or(usize::MAX),
    };
    apply_edit(&mut board, &BoardEdit::MoveCard(&req))?;
    write_board_frontmatter(&src, &board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))
}

/// Compute the board-doc source after applying `edit`, WITHOUT writing.
/// Reads + parses the board-doc, runs the shared [`apply_edit`] step, and
/// re-assembles the frontmatter. Returns `Ok(None)` when the edit was an
/// idempotent no-op. Drives the review-mode MCP staging path for the edit
/// verbs (mirrors `crate::boards::add_card_preview` for the add-note-card
/// case); the commit verbs run the SAME `apply_edit` step before
/// `persist_board`, so staged and direct text can't diverge.
///
/// status: board-mcp-tools
pub fn preview_edit(
    vault: &Vault,
    board_doc_rel: &str,
    edit: &BoardEdit,
) -> Result<Option<String>, HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    if !apply_edit(&mut board, edit)? {
        return Ok(None);
    }
    let new_src = write_board_frontmatter(&src, &board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))?;
    Ok(Some(new_src))
}

/// Read → parse → [`apply_edit`] → `persist_board` for the commit (UI) path.
/// Skips the write on an idempotent no-op. Every edit-shaped board verb is a
/// thin wrapper over this, so the UI commit path and the MCP review-preview
/// path share one mutation step.
async fn commit_edit(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    edit: &BoardEdit<'_>,
) -> Result<(), HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    if !apply_edit(&mut board, edit)? {
        return Ok(());
    }
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

/// Move or reorder a card between (or within) columns. The card identified
/// by `req.card_id` is removed from `req.from_column` and inserted at
/// `req.to_index` in `req.to_column`. Reordering within a column is the
/// same call with `from_column == to_column`. An out-of-range `to_index`
/// clamps to the column's tail.
///
/// status: board-move
pub async fn move_card(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    req: MoveCardRequest<'_>,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, req.board_doc_rel, &BoardEdit::MoveCard(&req)).await
}

/// Borrowed bundle of inputs to `add_card`. Bundles the vault-side
/// handles so the function stays under the `too_many_arguments`
/// threshold. Under path-as-identity (`board-card-references`) the card
/// holds only the source's vault path; no `store` is needed for id
/// stamping (the previous `note-id-stamping` trigger retired).
pub struct AddCardArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub log: &'a OpLog,
    pub board_doc_rel: &'a str,
    pub column_name: &'a str,
    pub source_rel: &'a str,
}

/// Append a note as a card to a board column. Idempotent per board: if
/// the note is already a card anywhere on the board, this is a no-op
/// (returns `Ok` without a duplicate). The referenced note is never
/// mutated — boards record card membership in their own frontmatter.
///
/// status: board-add-card
pub async fn add_card(args: AddCardArgs<'_>) -> Result<(), HikerError> {
    let AddCardArgs {
        watcher: _,
        jobs,
        vault,
        log,
        board_doc_rel,
        column_name,
        source_rel,
    } = args;

    let (src, mut board) = read_board(vault, board_doc_rel)?;
    // Idempotent per board: already a card anywhere → no-op.
    if board.contains_note(source_rel) {
        return Ok(());
    }
    let col_idx = board
        .column_index(column_name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {column_name}")))?;
    board.columns[col_idx].cards.push(BoardCard::Note {
        path: source_rel.to_string(),
    });
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

/// Append a freeform (text) card to a board column. Mints a card-local ULID
/// and appends a `BoardCard::Text`; no note is referenced or stamped. The
/// text is rewritten later via [`set_card_text`] on the same op-log
/// user-save path. Returns the new card's id so the caller can immediately
/// open it for inline editing.
///
/// status: board-freeform-card
pub async fn add_text_card(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    column_name: &str,
    text: &str,
) -> Result<String, HikerError> {
    let card_id = new_id();
    commit_edit(
        log,
        jobs,
        vault,
        board_doc_rel,
        &BoardEdit::AddTextCard {
            column: column_name,
            card_id: card_id.clone(),
            text,
        },
    )
    .await?;
    Ok(card_id)
}

/// Rewrite a freeform card's text in place, keyed by `card_id`. Errors if
/// the id names a note card (note cards have no editable text) or no card.
/// A frontmatter `user_save` edit like any other board mutation.
///
/// status: board-freeform-card
pub async fn set_card_text(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    card_id: &str,
    text: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        board_doc_rel,
        &BoardEdit::SetCardText { card_id, text },
    )
    .await
}

/// Remove a card from the board-doc by its card id (a note card's referenced
/// ULID, or a freeform card's card-local ULID). For a note card the
/// referenced note is untouched — removal is a board-membership edit, not a
/// note deletion.
///
/// status: board-remove-card
pub async fn remove_card(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    card_handle: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        board_doc_rel,
        &BoardEdit::RemoveCard { handle: card_handle },
    )
    .await
}

/// Add a new (empty) column to a board. Appended at the tail. No-op if a
/// column of the same name already exists.
///
/// status: board-column-management
pub async fn add_column(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    name: &str,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, board_doc_rel, &BoardEdit::AddColumn { name }).await
}

/// Rename a column in place; cards keep their order and membership.
///
/// status: board-column-management
pub async fn rename_column(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    old_name: &str,
    new_name: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        board_doc_rel,
        &BoardEdit::RenameColumn { old_name, new_name },
    )
    .await
}

/// Move a column to a new index in the column order. Out-of-range
/// `to_index` clamps to the tail.
///
/// status: board-column-management
pub async fn reorder_column(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    name: &str,
    to_index: usize,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        board_doc_rel,
        &BoardEdit::ReorderColumn { name, to_index },
    )
    .await
}

/// Delete a column (and drop its card references — the referenced notes are
/// untouched). The core layer allows deleting a column that still holds
/// cards; the UI prompts first per `docs/kanban.md` §"Managing columns".
///
/// status: board-column-management
pub async fn delete_column(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    name: &str,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, board_doc_rel, &BoardEdit::DeleteColumn { name }).await
}

/// Set or clear a column's WIP limit. `limit = Some(n)` caps the column at
/// `n` cards (a soft flag — the board view marks overflow, moves are not
/// hard-blocked); `limit = None` clears the cap (the key is omitted from
/// frontmatter on the next write). A frontmatter edit on the same op-log
/// user-save path as the other column ops.
///
/// status: board-wip-limits
pub async fn set_column_wip_limit(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    name: &str,
    limit: Option<usize>,
) -> Result<(), HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let idx = board
        .column_index(name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
    board.columns[idx].wip_limit = limit;
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

// Note: `repoint_card` retired with `trail-path-conflict-modal` —
// under path-as-identity (`board-card-references`) there's no id half
// left to disagree with a path, so the Keep / Repoint / Break modal
// has no analogue. The path-rewrite-on-move pass below covers the
// rename case; an unresolved card is just an orphan the user removes.

// ---------------------------------------------------------------------------
// Auto-update on note move
// ---------------------------------------------------------------------------

/// Implementation of the auto-update-on-move sweep (the public entry is
/// [`crate::boards::on_note_moved`], which forwards here). Invoked from the
/// indexer task right after the path remap for an `IndexJob::Move` /
/// `IndexJob::MoveFolder` succeeds, and from the watcher-driven
/// `IndexJob::Rename` branch — the same hooks trails use.
///
/// The ULID is unchanged (the move is path-only), so the rewrite targets
/// the `path` half of every card double-link that pointed at the moved
/// note. Two shapes are handled off the derived index:
///   1. A referenced note moved → every board-doc holding a card at
///      `old_rel` gets that card's `path` rewritten to `new_rel`.
///   2. A board-doc itself moved → the derived `board_cards` rows for that
///      board get their `board_path` column rewritten (the in-frontmatter
///      card refs don't point at the board-doc, so no frontmatter rewrite
///      is needed for case 2 — only the derived table tracks the board path).
///
/// Errors are logged via `tracing::warn!` but never propagated. Returns the
/// count of board-docs whose frontmatter was rewritten.
///
/// status: board-card-references
pub async fn run_note_moved(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    log: Option<&OpLog>,
    vault: &Vault,
    store: &mut crate::store::Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    // All store reads happen here (before the async fan-out) so no `&Store`
    // is held across an await — rusqlite is !Sync.
    let board_docs = affected_board_docs(store, vault, log, old_rel);

    // Case 1: a referenced note moved — rename its derived card rows' note
    // path so the table tracks the new path before the next board re-derive.
    if let Err(e) = store.rename_board_card_note_paths(old_rel, new_rel) {
        tracing::warn!(error = %e, "board on_note_moved: rename_board_card_note_paths failed");
    }
    // Case 2: a board-doc moved — rename its derived rows' board_path.
    if let Err(e) = store.rename_board_card_paths_for_board(old_rel, new_rel) {
        tracing::warn!(error = %e, "board on_note_moved: rename_board_card_paths_for_board failed");
    }

    let mut touched = 0usize;
    for board_doc_rel in board_docs {
        let ctx = RewriteCtx { log, jobs, watcher, vault };
        match ctx.rewrite_card_path(&board_doc_rel, old_rel, new_rel).await {
            Ok(true) => touched += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %board_doc_rel,
                    "board on_note_moved: card-path rewrite failed");
            }
        }
    }
    Ok(touched)
}

/// Distinct board-doc rel-paths holding a card at `note_rel`. Resolves the
/// derived-table hits' `board_id` to a board-doc path via `super::list` (the
/// same walk the menu uses). Store-only; no await.
fn affected_board_docs(
    store: &crate::store::Store,
    vault: &Vault,
    log: Option<&OpLog>,
    note_rel: &str,
) -> std::collections::HashSet<String> {
    let containing = store.boards_containing_note(note_rel).unwrap_or_else(|e| {
        tracing::warn!(error = %e, path = %note_rel,
            "board on_note_moved: boards_containing_note failed");
        Vec::new()
    });
    // Without an op-log handle (CLI / test paths) we can't translate
    // derived-table `board_id`s back to paths; the move-rewrite is a
    // best-effort pass, so degrade to "no affected boards" in that case.
    let Some(log) = log else {
        return std::collections::HashSet::new();
    };
    let board_path_by_id: std::collections::HashMap<String, String> =
        super::list(vault, store, log)
            .unwrap_or_default()
            .into_iter()
            .map(|b| (b.board_id, b.rel_path))
            .collect();
    containing
        .into_iter()
        .filter_map(|hit| board_path_by_id.get(&hit.board_id).cloned())
        .collect()
}

/// Borrow-bundle for the card-path rewrite in `on_note_moved`. Bundling the
/// optional handles keeps `rewrite_card_path` a method (exempt from
/// `single_call_fn`) under the argument-count budget.
struct RewriteCtx<'a> {
    log: Option<&'a OpLog>,
    jobs: Option<&'a IndexJobTx>,
    watcher: Option<&'a Watcher>,
    vault: &'a Vault,
}

impl RewriteCtx<'_> {
    /// Read + parse the board-doc, rewrite every card whose `path ==
    /// old_rel` to `new_rel` (id unchanged), and persist. Returns `true`
    /// when a rewrite landed. Goes through the op-log user-save path when a
    /// log is attached; falls back to a suppressed `write_file` for CLI /
    /// test paths without a log handle.
    async fn rewrite_card_path(
        &self,
        board_doc_rel: &str,
        old_rel: &str,
        new_rel: &str,
    ) -> Result<bool, HikerError> {
        let (src, mut board) = read_board(self.vault, board_doc_rel)?;
        let mut changed = false;
        for col in &mut board.columns {
            for card in &mut col.cards {
                // Freeform cards have no path — auto-update-on-move only
                // touches note cards. status: board-freeform-card
                if let BoardCard::Note { path } = card
                    && path == old_rel
                {
                    *path = new_rel.to_string();
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(false);
        }
        let new_src = write_board_frontmatter(&src, &board)
            .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))?;
        match self.log {
            Some(log) => {
                crate::ops::op_writes::user_save(log, self.vault, board_doc_rel, &new_src)?;
            }
            None => {
                if let Some(w) = self.watcher {
                    w.suppress(board_doc_rel.to_string());
                }
                self.vault.write_file(board_doc_rel, &new_src)?;
                if let Some(w) = self.watcher {
                    w.suppress(board_doc_rel.to_string());
                }
            }
        }
        if let Some(j) = self.jobs {
            let _ = j
                .send(IndexJob::Upsert {
                    rel_path: board_doc_rel.to_string(),
                    force: false,
                })
                .await;
        }
        Ok(true)
    }
}
