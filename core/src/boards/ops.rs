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
use crate::kinds::{Kind, Registry};
use crate::editing::LayeredDoc;
use crate::store::dto::new_id;
use crate::vault::{next_free_md_path, Vault};
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
/// columns (per-kind mapping seed or the plain default — see
/// [`plan_new_board`]), and re-indexes the board-doc. Mirrors
/// `core::trails::ops::create_trail`.
///
/// `name` is used verbatim as the basename; the function appends `-N.md`
/// (1..1000) only on a collision.
///
/// The board-doc is created on disk via `vault.write_file` (not the
/// layered-doc save path) so the file exists before its first card-move;
/// the layered doc adopts it on its first `user_save`. Watcher suppression
/// brackets the write.
///
/// status: board-create
/// status: board-default-location
/// status: sprint-board-subtype
pub async fn create_board(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    log: &LayeredDoc,
    vault: &Vault,
    config: &BoardsConfig,
    name: &str,
    kind: Option<&Kind>,
) -> Result<CreateBoardOutcome, HikerError> {
    let plan = plan_new_board(vault, config, name, kind)?;

    watcher.suppress(plan.board_doc_rel.clone());
    vault.write_file(&plan.board_doc_rel, &plan.body)?;
    watcher.suppress(plan.board_doc_rel.clone());

    // status: store-path-is-identity
    // Seed the layered-doc document so the board has a stable storage key
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
/// The board's storage `doc_id` is sourced from the layered doc after the
/// write — under `board-doc-shape` the board-doc carries no id field.
pub struct NewBoardPlan {
    pub board_doc_rel: String,
    pub body: String,
}

/// Compute a new board's path + body WITHOUT writing: ensures
/// `new_board_dir` exists, seeds the columns, and resolves the first free
/// `<dir>/<name>.md` (auto-suffixing `-N` on collision). The direct
/// `create_board` writes the result; the MCP review path stages it as a
/// whole-file create. Picking the path is filesystem-read-only (it probes
/// `abs.exists()`); the actual create is the caller's.
///
/// `kind = Some` creates a board of that board-like kind: `hiker.kind` is
/// the kind's name, and the columns seed from the kind's column-state
/// mapping (`Kind::seed_columns` — state-declaration order), falling back
/// to the plain `Todo`/`Doing`/`Done` seed when the kind maps no columns —
/// so a fresh sprint is born meaning something. `None` = plain board.
///
/// status: board-mcp-tools
/// status: sprint-board-subtype
pub fn plan_new_board(
    vault: &Vault,
    config: &BoardsConfig,
    name: &str,
    kind: Option<&Kind>,
) -> Result<NewBoardPlan, HikerError> {
    let folder = config.new_board_dir.trim_end_matches('/');
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create new_board_dir: {e}")))?;
        }
    }

    let mut column_names: Vec<String> = kind.map(Kind::seed_columns).unwrap_or_default();
    if column_names.is_empty() {
        column_names = DEFAULT_COLUMNS.iter().map(|n| (*n).to_string()).collect();
    }
    let board = Board {
        kind: kind.map_or_else(|| "board".to_string(), |k| k.name.clone()),
        columns: column_names
            .into_iter()
            .map(|name| Column {
                name,
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

    let board_doc_rel = next_free_md_path(vault, folder, name)?;

    Ok(NewBoardPlan {
        board_doc_rel,
        body,
    })
}

// The first-free-path probe (`<folder>/<stem>.md`, `-N` suffix on
// collision) lives at `crate::vault::next_free_md_path` so the vault rules
// layer's `create_note` verb shares the exact collision rule
// (`rule-closed-verbs`); [`plan_new_board`] and [`promote_text_card`] call
// it via the import at the top of this file.

/// Persist a mutated board-doc through the layered-doc user-save path, then
/// enqueue a re-index so the derived `board_cards` rows re-derive. The
/// layered doc diffs the new frontmatter against the current accepted state into
/// localized spans — concurrent moves of different cards merge, same-card
/// moves surface as conflict hunks (`op-log-merge-conflict`). The layered doc
/// writes the resulting `.md` itself, so we do NOT also `write_file`.
///
/// status: board-move
async fn persist_board(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    src: &str,
    board: &Board,
) -> Result<(), HikerError> {
    let new_src = render_board(src, board)?;
    crate::ops::op_writes::user_save(log, vault, board_doc_rel, &new_src)?;
    enqueue_reindex(jobs, board_doc_rel).await;
    Ok(())
}

/// Enqueue a re-index of the board-doc so the derived `board_cards` rows
/// re-derive after a committed write.
async fn enqueue_reindex(jobs: &IndexJobTx, board_doc_rel: &str) {
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: board_doc_rel.to_string(),
            force: false,
        })
        .await;
}

/// Assemble the board-doc source from a mutated `Board`, mapping the
/// frontmatter error into the op-level error shape.
fn render_board(src: &str, board: &Board) -> Result<String, HikerError> {
    write_board_frontmatter(src, board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))
}

/// Read + parse the board-doc, returning `(source, board)`. `kinds`
/// extends the parse gate to registered board-like kinds
/// (`sprint-board-subtype`), so every op here works on sprints unchanged.
fn read_board(
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
) -> Result<(String, Board), HikerError> {
    let src = vault.read_file(board_doc_rel)?;
    let board = parse_board_for(board_doc_rel, &src, kinds)
        .map_err(|e| HikerError::Io(format!("parse board-doc: {e}")))?;
    Ok((src, board))
}

/// Where a board write op reads its board-doc text and lands the result —
/// the authorship seam. Both authors run the SAME read → guard → mutate →
/// render pipeline ([`edit_board`] / [`add_note_card`] /
/// [`move_card_to_column`]); only the source and sink differ, so the two
/// paths cannot drift (the rules engine used to re-drive these steps with
/// its own copy of the mutation code).
///
/// status: rule-closed-verbs
/// status: board-move
pub enum BoardWriteMode<'a> {
    /// The direct user path: read from disk, commit the new text through
    /// `op_writes::user_save` (`Author::User`, the layered-doc atomic-write
    /// path). The async verb wrappers enqueue the re-index afterwards.
    UserDirect { log: &'a LayeredDoc },
    /// An automation firing: read through (and write into) the firing's
    /// draft overlay. Nothing commits here — the producer stages the
    /// collected texts as ONE `auto:<producer>` batch afterwards
    /// (`op_writes::stage_auto_content_batch`), so a multi-action firing
    /// still stages exactly once.
    AutoStaged {
        draft: &'a mut crate::ops::op_writes::Draft,
    },
}

impl BoardWriteMode<'_> {
    /// Read + parse the board-doc through the mode's source: disk for the
    /// user path, the firing's draft overlay (falling back to disk) for
    /// automation — so a later action in the same firing sees an earlier
    /// action's output.
    fn read_board(
        &self,
        vault: &Vault,
        kinds: Option<&Registry>,
        board_doc_rel: &str,
    ) -> Result<(String, Board), HikerError> {
        let src = match self {
            Self::UserDirect { .. } => vault.read_file(board_doc_rel)?,
            Self::AutoStaged { draft } => draft.read(vault, board_doc_rel)?,
        };
        let board = parse_board_for(board_doc_rel, &src, kinds)
            .map_err(|e| HikerError::Io(format!("parse board-doc: {e}")))?;
        Ok((src, board))
    }

    /// Land the rendered board-doc text through the mode's sink.
    fn land(&mut self, vault: &Vault, board_doc_rel: &str, new_src: &str) -> Result<(), HikerError> {
        match self {
            Self::UserDirect { log } => {
                crate::ops::op_writes::user_save(log, vault, board_doc_rel, new_src)
            }
            Self::AutoStaged { draft } => {
                draft.put(board_doc_rel, new_src.to_string());
                Ok(())
            }
        }
    }
}

/// Read → [`apply_edit`] → render → land through `mode` — the one
/// edit-shaped mutation body both authors share. Returns `false` when the
/// edit was an idempotent no-op (nothing landed).
///
/// status: rule-closed-verbs
pub fn edit_board(
    mode: &mut BoardWriteMode<'_>,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    edit: &BoardEdit<'_>,
) -> Result<bool, HikerError> {
    let (src, mut board) = mode.read_board(vault, kinds, board_doc_rel)?;
    if !apply_edit(&mut board, edit)? {
        return Ok(false);
    }
    let new_src = render_board(&src, &board)?;
    mode.land(vault, board_doc_rel, &new_src)?;
    Ok(true)
}

/// Append `source_rel` as a note card on `board_doc_rel` — the add-card
/// mutation body shared by the direct user verb ([`add_card`]) and the
/// rules `add_to_board` verb. Idempotent per board (`Ok(false)` when the
/// note is already a card anywhere on it), and the single-sprint guard
/// (`pm::ensure_single_sprint_membership`) refuses a second sprint exactly
/// the same for both authors. `column = None` appends to the board's
/// first column (the rules-verb default); the user path always names one.
///
/// status: board-add-card
/// status: derived-status-rule
/// status: rule-closed-verbs
pub fn add_note_card(
    mode: &mut BoardWriteMode<'_>,
    vault: &Vault,
    store: &crate::store::Store,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    column: Option<&str>,
    source_rel: &str,
) -> Result<bool, HikerError> {
    let (src, mut board) = mode.read_board(vault, kinds, board_doc_rel)?;
    // Idempotent per board: already a card anywhere → no-op.
    if board.contains_note(source_rel) {
        return Ok(false);
    }
    crate::pm::ensure_single_sprint_membership(
        store,
        kinds,
        board_doc_rel,
        &board.kind,
        source_rel,
    )?;
    let column_name = match column {
        Some(name) => name.to_string(),
        None => board
            .columns
            .first()
            .map(|c| c.name.clone())
            .ok_or_else(|| {
                HikerError::NotFound(format!("board `{board_doc_rel}` has no columns"))
            })?,
    };
    let col_idx = board
        .column_index(&column_name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {column_name}")))?;
    board.columns[col_idx].cards.push(BoardCard::Note {
        path: source_rel.to_string(),
    });
    let new_src = render_board(&src, &board)?;
    mode.land(vault, board_doc_rel, &new_src)?;
    Ok(true)
}

/// Move the card named by `card_handle` to the tail of `to_column`,
/// resolving its current column off the parsed board — the rules
/// `move_card` verb shape (the MCP `preview_move_card` resolves the same
/// way). A card already in the target column is a no-op (`Ok(false)`),
/// per rules.md's decided-at-implementation note.
///
/// status: rule-closed-verbs
pub fn move_card_to_column(
    mode: &mut BoardWriteMode<'_>,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    card_handle: &str,
    to_column: &str,
) -> Result<bool, HikerError> {
    let (src, mut board) = mode.read_board(vault, kinds, board_doc_rel)?;
    let from_column = current_column_of(&board, card_handle).ok_or_else(|| {
        HikerError::NotFound(format!("card: {card_handle} on `{board_doc_rel}`"))
    })?;
    if from_column == to_column {
        return Ok(false);
    }
    let req = MoveCardRequest {
        board_doc_rel,
        from_column: &from_column,
        card_handle,
        to_column,
        to_index: usize::MAX,
    };
    apply_edit(&mut board, &BoardEdit::MoveCard(&req))?;
    let new_src = render_board(&src, &board)?;
    mode.land(vault, board_doc_rel, &new_src)?;
    Ok(true)
}

/// The name of the column currently holding the card matching `handle`
/// (a note card's vault path or a freeform card's `card_id`).
fn current_column_of(board: &Board, handle: &str) -> Option<String> {
    board
        .columns
        .iter()
        .find(|col| col.cards.iter().any(|c| card_matches(c, handle)))
        .map(|col| col.name.clone())
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
    /// Swap a freeform card in place — same column, same position — from
    /// `{ card_id, text }` to `{ path }`: the card half of
    /// [`promote_text_card`] (`freeform-promote-note`).
    PromoteTextCard { card_id: &'a str, path: &'a str },
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
        BoardEdit::PromoteTextCard { card_id, path } => {
            // status: freeform-promote-note — in-place swap keeps the
            // card's column and position; only the card's shape changes.
            let card = board
                .columns
                .iter_mut()
                .flat_map(|col| col.cards.iter_mut())
                .find(|c| matches!(c, BoardCard::Text { card_id: cid, .. } if cid == *card_id))
                .ok_or_else(|| HikerError::NotFound(format!("card id: {card_id}")))?;
            *card = BoardCard::Note {
                path: (*path).to_string(),
            };
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
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    card_handle: &str,
    to_column: &str,
    to_index: Option<usize>,
) -> Result<String, HikerError> {
    let (src, mut board) = read_board(vault, kinds, board_doc_rel)?;
    let from_column = current_column_of(&board, card_handle)
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
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    edit: &BoardEdit,
) -> Result<Option<String>, HikerError> {
    let (src, mut board) = read_board(vault, kinds, board_doc_rel)?;
    if !apply_edit(&mut board, edit)? {
        return Ok(None);
    }
    let new_src = write_board_frontmatter(&src, &board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))?;
    Ok(Some(new_src))
}

/// [`edit_board`] in `UserDirect` mode plus the post-commit re-index — the
/// commit (UI) path. Skips the write on an idempotent no-op. Every
/// edit-shaped board verb is a thin wrapper over this, so the UI commit
/// path, the rules engine's staged path, and the MCP review-preview path
/// share one mutation step.
async fn commit_edit(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    edit: &BoardEdit<'_>,
) -> Result<(), HikerError> {
    let mut mode = BoardWriteMode::UserDirect { log };
    if edit_board(&mut mode, vault, kinds, board_doc_rel, edit)? {
        enqueue_reindex(jobs, board_doc_rel).await;
    }
    Ok(())
}

/// Move or reorder a card between (or within) columns. The card identified
/// by `req.card_id` is removed from `req.from_column` and inserted at
/// `req.to_index` in `req.to_column`. Reordering within a column is the
/// same call with `from_column == to_column`. An out-of-range `to_index`
/// clamps to the column's tail.
///
/// status: board-move
pub async fn move_card(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    req: MoveCardRequest<'_>,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, kinds, req.board_doc_rel, &BoardEdit::MoveCard(&req)).await
}

/// Borrowed bundle of inputs to `add_card`. Bundles the vault-side
/// handles so the function stays under the `too_many_arguments`
/// threshold. Under path-as-identity (`board-card-references`) the card
/// holds only the source's vault path; `store` + `kinds` back the
/// at-most-one-sprint membership check (`derived-status-rule`), read-only.
pub struct AddCardArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub log: &'a LayeredDoc,
    pub store: &'a crate::store::Store,
    pub kinds: Option<&'a Registry>,
    pub board_doc_rel: &'a str,
    pub column_name: &'a str,
    pub source_rel: &'a str,
}

/// Append a note as a card to a board column. Idempotent per board: if
/// the note is already a card anywhere on the board, this is a no-op
/// (returns `Ok` without a duplicate). The referenced note is never
/// mutated — boards record card membership in their own frontmatter.
///
/// Adding to a *sprint-kind* board is refused when the note is already a
/// card on a different sprint-kind board, naming the holding sprint —
/// the one-sprint rule enforced at every card-add path
/// (`derived-status-rule`). Plain boards stay unconstrained.
///
/// status: board-add-card
/// status: derived-status-rule
pub async fn add_card(args: AddCardArgs<'_>) -> Result<(), HikerError> {
    let AddCardArgs {
        watcher: _,
        jobs,
        vault,
        log,
        store,
        kinds,
        board_doc_rel,
        column_name,
        source_rel,
    } = args;

    let mut mode = BoardWriteMode::UserDirect { log };
    if add_note_card(
        &mut mode,
        vault,
        store,
        kinds,
        board_doc_rel,
        Some(column_name),
        source_rel,
    )? {
        enqueue_reindex(jobs, board_doc_rel).await;
    }
    Ok(())
}

/// Append a freeform (text) card to a board column. Mints a card-local ULID
/// and appends a `BoardCard::Text`; no note is referenced or stamped. The
/// text is rewritten later via [`set_card_text`] on the same layered-doc
/// user-save path. Returns the new card's id so the caller can immediately
/// open it for inline editing.
///
/// status: board-freeform-card
pub async fn add_text_card(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    column_name: &str,
    text: &str,
) -> Result<String, HikerError> {
    let card_id = new_id();
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
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
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    card_id: &str,
    text: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
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
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    card_handle: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
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
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    name: &str,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, kinds, board_doc_rel, &BoardEdit::AddColumn { name }).await
}

/// Rename a column in place; cards keep their order and membership.
///
/// status: board-column-management
pub async fn rename_column(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    old_name: &str,
    new_name: &str,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
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
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    name: &str,
    to_index: usize,
) -> Result<(), HikerError> {
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
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
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    name: &str,
) -> Result<(), HikerError> {
    commit_edit(log, jobs, vault, kinds, board_doc_rel, &BoardEdit::DeleteColumn { name }).await
}

/// Set or clear a column's WIP limit. `limit = Some(n)` caps the column at
/// `n` cards (a soft flag — the board view marks overflow, moves are not
/// hard-blocked); `limit = None` clears the cap (the key is omitted from
/// frontmatter on the next write). A frontmatter edit on the same layered-doc
/// user-save path as the other column ops.
///
/// status: board-wip-limits
pub async fn set_column_wip_limit(
    log: &LayeredDoc,
    jobs: &IndexJobTx,
    vault: &Vault,
    kinds: Option<&Registry>,
    board_doc_rel: &str,
    name: &str,
    limit: Option<usize>,
) -> Result<(), HikerError> {
    let (src, mut board) = read_board(vault, kinds, board_doc_rel)?;
    let idx = board
        .column_index(name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
    board.columns[idx].wip_limit = limit;
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

/// Borrowed bundle of inputs to [`promote_text_card`].
pub struct PromoteTextCardArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub log: &'a LayeredDoc,
    pub vault: &'a Vault,
    pub kinds: Option<&'a Registry>,
    pub board_doc_rel: &'a str,
    /// The freeform card's internal id (the only handle a text card has).
    pub card_id: &'a str,
    /// Kind template for the born note (`freeform-promote-note`): `Some`
    /// seeds `hiker.kind` plus the kind's fields, empty. Callers resolve
    /// it from the owning plan's `default_kind` policy key via
    /// `pm::plan_default_kind` (`plan-kind`); `None` — no plan, no
    /// declared default — births a plain note, per pm.md's no-plan case.
    pub template_kind: Option<&'a Kind>,
}

/// Convert a freeform card into a note (`freeform-promote-note`): create a
/// note from the card's text — first line slugified as the filename,
/// suffix-on-collision, full text as the body, landing in the board-doc's
/// directory — then swap the card in place (same column, same position)
/// from `{ card_id, text }` to `{ path }` through the shared [`apply_edit`]
/// mutation step. A direct user verb, not staged — the same posture as
/// add-card. Returns the new note's vault-relative path.
///
/// status: freeform-promote-note
pub async fn promote_text_card(args: PromoteTextCardArgs<'_>) -> Result<String, HikerError> {
    let PromoteTextCardArgs {
        watcher,
        jobs,
        log,
        vault,
        kinds,
        board_doc_rel,
        card_id,
        template_kind,
    } = args;

    let (_, board) = read_board(vault, kinds, board_doc_rel)?;
    let text = board
        .columns
        .iter()
        .flat_map(|col| col.cards.iter())
        .find_map(|card| match card {
            BoardCard::Text { card_id: cid, text } if cid == card_id => Some(text.clone()),
            _ => None,
        })
        .ok_or_else(|| HikerError::NotFound(format!("card id: {card_id}")))?;

    // The note lands in the board-doc's directory, named off the card's
    // first line, suffixed on collision.
    let folder = board_doc_rel.rsplit_once('/').map_or("", |(dir, _)| dir);
    let note_rel = next_free_md_path(vault, folder, &slugify(&text))?;
    // The promoted note's full source: the card text as the body plus the
    // kind-template seeding — shared with the rules layer's `create_note`
    // verb (`rule-closed-verbs`).
    let body = crate::kinds::template_note_body(&text, template_kind)?;

    // Create the note on disk (suppressed, like `create_board`), seed its
    // layered doc, and index it so the swapped-in card resolves immediately.
    watcher.suppress(note_rel.clone());
    vault.write_file(&note_rel, &body)?;
    watcher.suppress(note_rel.clone());
    crate::ops::op_writes::doc_id_or_seed(log, vault, &note_rel, &body)?;
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: note_rel.clone(),
            force: false,
        })
        .await;

    // The in-place card swap — one user-initiated board-doc commit.
    commit_edit(
        log,
        jobs,
        vault,
        kinds,
        board_doc_rel,
        &BoardEdit::PromoteTextCard {
            card_id,
            path: &note_rel,
        },
    )
    .await?;
    Ok(note_rel)
}

/// Filename slug from a freeform card's text: the first line, lowercased,
/// non-alphanumeric runs collapsed to `-`. Falls back to `card` when the
/// text yields nothing usable.
fn slugify(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in first.chars() {
        if ch.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "card".to_string()
    } else {
        out
    }
}

// Note: `repoint_card` retired with `trail-path-conflict-modal` —
// under path-as-identity (`board-card-references`) there's no id half
// left to disagree with a path, so the Keep / Repoint / Break modal
// has no analogue. The path-rewrite-on-move pass below covers the
// rename case; an unresolved card is just an orphan the user removes.

// ---------------------------------------------------------------------------
// Auto-update on note move
// ---------------------------------------------------------------------------

/// Borrow-bundle of the environment handles the auto-update-on-move sweep
/// reads. `watcher` / `jobs` / `log` / `kinds` are optional because CLI /
/// unit-test paths run without some of them attached; each degrades the
/// same way the lazy-init scheme always has. `kinds` extends the
/// board-doc enumeration to sprint-kind boards (`sprint-board-subtype`),
/// so a moved note's cards on sprints rewrite too.
pub struct NoteMovedEnv<'a> {
    pub watcher: Option<&'a Watcher>,
    pub jobs: Option<&'a IndexJobTx>,
    pub log: Option<&'a LayeredDoc>,
    pub kinds: Option<&'a Registry>,
    pub vault: &'a Vault,
}

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
    env: &NoteMovedEnv<'_>,
    store: &mut crate::store::Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    // All store reads happen here (before the async fan-out) so no `&Store`
    // is held across an await — rusqlite is !Sync.
    let board_docs = affected_board_docs(store, env, old_rel);

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
        match rewrite_card_path(env, &board_doc_rel, old_rel, new_rel).await {
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
    env: &NoteMovedEnv<'_>,
    note_rel: &str,
) -> std::collections::HashSet<String> {
    let containing = store.boards_containing_note(note_rel).unwrap_or_else(|e| {
        tracing::warn!(error = %e, path = %note_rel,
            "board on_note_moved: boards_containing_note failed");
        Vec::new()
    });
    // Without a layered-doc handle (CLI / test paths) we can't translate
    // derived-table `board_id`s back to paths; the move-rewrite is a
    // best-effort pass, so degrade to "no affected boards" in that case.
    let Some(log) = env.log else {
        return std::collections::HashSet::new();
    };
    let board_path_by_id: std::collections::HashMap<String, String> =
        super::list(env.vault, store, log, env.kinds)
            .unwrap_or_default()
            .into_iter()
            .map(|b| (b.board_id, b.rel_path))
            .collect();
    containing
        .into_iter()
        .filter_map(|hit| board_path_by_id.get(&hit.board_id).cloned())
        .collect()
}

/// Read + parse the board-doc, rewrite every card whose `path == old_rel`
/// to `new_rel` (id unchanged), and persist. Returns `true` when a rewrite
/// landed. Goes through the layered-doc user-save path when a log is attached;
/// falls back to a suppressed `write_file` for CLI / test paths without a
/// log handle.
async fn rewrite_card_path(
    env: &NoteMovedEnv<'_>,
    board_doc_rel: &str,
    old_rel: &str,
    new_rel: &str,
) -> Result<bool, HikerError> {
    let (src, mut board) = read_board(env.vault, env.kinds, board_doc_rel)?;
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
    match env.log {
        Some(log) => {
            crate::ops::op_writes::user_save(log, env.vault, board_doc_rel, &new_src)?;
        }
        None => {
            if let Some(w) = env.watcher {
                w.suppress(board_doc_rel.to_string());
            }
            env.vault.write_file(board_doc_rel, &new_src)?;
            if let Some(w) = env.watcher {
                w.suppress(board_doc_rel.to_string());
            }
        }
    }
    if let Some(j) = env.jobs {
        let _ = j
            .send(IndexJob::Upsert {
                rel_path: board_doc_rel.to_string(),
                force: false,
            })
            .await;
    }
    Ok(true)
}
