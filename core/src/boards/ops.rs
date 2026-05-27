//! Board ops: mutation verbs (create / move-card / add-card / remove-card,
//! column add/rename/reorder/delete) plus the path-remap surface invoked
//! from the indexer on note moves.
//!
//! Every write to a board-doc goes through `core::ops::op_writes::user_save`
//! (`op-log-ops-producer-helpers`) — a board move is an ordinary versioned,
//! undoable, syncable user edit, and the referenced notes are NEVER mutated.
//! Adding a card lazy-stamps the referenced note's ULID via the same trigger
//! trails use (`note-id-stamping`).

use serde::{Deserialize, Serialize};

use crate::config::sections::BoardsConfig;
use crate::errors::HikerError;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::oplog::OpLog;
use crate::store::dto::new_id;
use crate::store::Store;
use crate::trails::DoubleLinkRef;
use crate::vault::Vault;
use crate::watcher::Watcher;

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
    vault: &Vault,
    config: &BoardsConfig,
    name: &str,
) -> Result<CreateBoardOutcome, HikerError> {
    let folder = config.new_board_dir.trim_end_matches('/');
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create new_board_dir: {e}")))?;
        }
    }

    let board_id = new_id();
    let board = Board {
        id: board_id.clone(),
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

    let mut chosen: Option<String> = None;
    let base_candidate = if folder.is_empty() {
        format!("{name}.md")
    } else {
        format!("{folder}/{name}.md")
    };
    {
        let abs = vault.abs_path(&base_candidate)?;
        if !abs.exists() {
            watcher.suppress(base_candidate.clone());
            vault.write_file(&base_candidate, &body)?;
            chosen = Some(base_candidate);
        }
    }
    if chosen.is_none() {
        for n in 1..1000 {
            let candidate = if folder.is_empty() {
                format!("{name}-{n}.md")
            } else {
                format!("{folder}/{name}-{n}.md")
            };
            let abs = vault.abs_path(&candidate)?;
            if !abs.exists() {
                watcher.suppress(candidate.clone());
                vault.write_file(&candidate, &body)?;
                chosen = Some(candidate);
                break;
            }
        }
    }
    let board_doc_rel = chosen
        .ok_or_else(|| HikerError::AlreadyExists(format!("ran out of {name}-N candidates")))?;

    watcher.suppress(board_doc_rel.clone());
    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: board_doc_rel.clone(),
            force: false,
        })
        .await;

    Ok(CreateBoardOutcome {
        board_doc_rel,
        board_id,
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
/// column, into which column at which index. The environment handles (log /
/// jobs / vault / board path) are passed separately. Bundling keeps
/// `move_card` under the argument-count budget.
pub struct MoveCardRequest<'a> {
    pub board_doc_rel: &'a str,
    pub from_column: &'a str,
    pub card_id: &'a str,
    pub to_column: &'a str,
    /// Target index in the destination column; clamps to the tail when out
    /// of range (pass `usize::MAX` to append).
    pub to_index: usize,
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
    let (src, mut board) = read_board(vault, req.board_doc_rel)?;
    let from_idx = board
        .column_index(req.from_column)
        .ok_or_else(|| HikerError::NotFound(format!("column: {}", req.from_column)))?;
    let card_pos = board.columns[from_idx]
        .cards
        .iter()
        .position(|c| c.id == req.card_id)
        .ok_or_else(|| HikerError::NotFound(format!("card id: {}", req.card_id)))?;
    let card = board.columns[from_idx].cards.remove(card_pos);
    let to_idx = board
        .column_index(req.to_column)
        .ok_or_else(|| HikerError::NotFound(format!("column: {}", req.to_column)))?;
    let dest = &mut board.columns[to_idx].cards;
    let insert_at = req.to_index.min(dest.len());
    dest.insert(insert_at, card);
    persist_board(log, jobs, vault, req.board_doc_rel, &src, &board).await
}

/// Borrowed bundle of inputs to `add_card`. Bundles the vault-side handles
/// plus the mutable `store` so the function stays under the
/// `too_many_arguments` threshold while keeping the explicit `&mut Store`
/// lifetime the id-stamping helper needs.
pub struct AddCardArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub store: &'a mut Store,
    pub log: &'a OpLog,
    pub board_doc_rel: &'a str,
    pub column_name: &'a str,
    pub source_rel: &'a str,
}

/// Append a note as a card to a board column. Lazy-stamps the source note's
/// ULID (`note-id-stamping`), then appends the `{id,path}` ref to the named
/// column. Idempotent per board: if the note is already a card anywhere on
/// the board, this is a no-op (returns `Ok` without a duplicate). The
/// referenced note is never mutated beyond the ULID stamp.
///
/// status: board-add-card
pub async fn add_card(args: AddCardArgs<'_>) -> Result<(), HikerError> {
    let AddCardArgs {
        watcher,
        jobs,
        vault,
        store,
        log,
        board_doc_rel,
        column_name,
        source_rel,
    } = args;

    // Lazy-stamp the source; adopt the indexer's path_ids ULID so the card's
    // recorded id matches what `resolve_reference` later returns.
    let source_id =
        crate::ops::buffer::ensure_note_id_stamped(watcher, jobs, vault, store, source_rel).await?;

    let (src, mut board) = read_board(vault, board_doc_rel)?;
    // Idempotent per board: already a card anywhere → no-op.
    if board.contains_note(&source_id, source_rel) {
        return Ok(());
    }
    let col_idx = board
        .column_index(column_name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {column_name}")))?;
    board.columns[col_idx].cards.push(DoubleLinkRef {
        id: source_id,
        path: source_rel.to_string(),
    });
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

/// Remove a card from the board-doc by its card id (the referenced note's
/// ULID). The referenced note is untouched — removal is a board-membership
/// edit, not a note deletion.
///
/// status: board-remove-card
pub async fn remove_card(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    board_doc_rel: &str,
    card_id: &str,
) -> Result<(), HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let mut removed = false;
    for col in &mut board.columns {
        if let Some(pos) = col.cards.iter().position(|c| c.id == card_id) {
            col.cards.remove(pos);
            removed = true;
            break;
        }
    }
    if !removed {
        return Err(HikerError::NotFound(format!("card id: {card_id}")));
    }
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
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
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    if board.column_index(name).is_some() {
        return Ok(());
    }
    board.columns.push(Column {
        name: name.to_string(),
        cards: Vec::new(),
        wip_limit: None,
    });
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
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
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let idx = board
        .column_index(old_name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {old_name}")))?;
    board.columns[idx].name = new_name.to_string();
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
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
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let idx = board
        .column_index(name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
    let col = board.columns.remove(idx);
    let insert_at = to_index.min(board.columns.len());
    board.columns.insert(insert_at, col);
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
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
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let idx = board
        .column_index(name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {name}")))?;
    board.columns.remove(idx);
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
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

/// Repoint a card at a `PathConflict`: rewrite the card identified by
/// `card_id` to adopt the identity (`{id, path}`) of `new_path` and the note
/// currently there. The new id is resolved from the index; when the note is
/// unstamped the id half is left empty (the path still anchors it). A
/// frontmatter `user_save` edit. Resolves the board-card half of the shared
/// Keep mine / Repoint / Break modal.
///
/// status: board-card-references
pub async fn repoint_card(
    log: &OpLog,
    jobs: &IndexJobTx,
    vault: &Vault,
    store: &Store,
    board_doc_rel: &str,
    card_id: &str,
    new_path: &str,
) -> Result<(), HikerError> {
    let (src, mut board) = read_board(vault, board_doc_rel)?;
    let new_id = store
        .id_for_path(new_path)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .unwrap_or_default();
    let mut found = false;
    for col in &mut board.columns {
        if let Some(card) = col.cards.iter_mut().find(|c| c.id == card_id) {
            card.id = new_id;
            card.path = new_path.to_string();
            found = true;
            break;
        }
    }
    if !found {
        return Err(HikerError::NotFound(format!("card id: {card_id}")));
    }
    persist_board(log, jobs, vault, board_doc_rel, &src, &board).await
}

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
    store: &mut Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    // All store reads happen here (before the async fan-out) so no `&Store`
    // is held across an await — rusqlite is !Sync.
    let board_docs = affected_board_docs(store, vault, old_rel);

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
    store: &Store,
    vault: &Vault,
    note_rel: &str,
) -> std::collections::HashSet<String> {
    let containing = store.boards_containing_note(note_rel).unwrap_or_else(|e| {
        tracing::warn!(error = %e, path = %note_rel,
            "board on_note_moved: boards_containing_note failed");
        Vec::new()
    });
    let board_path_by_id: std::collections::HashMap<String, String> =
        super::list(vault, store)
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
                if card.path == old_rel {
                    card.path = new_rel.to_string();
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
