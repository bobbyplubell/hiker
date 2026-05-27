//! Boards: a curated kanban view over the vault. See `docs/kanban.md`.
//!
//! A board-doc is a regular markdown note with `hiker.kind: board` in its
//! frontmatter; the frontmatter owns an ordered list of user-named
//! columns, each an ordered list of card references. The board-doc is to a
//! note-card what a trail-doc is to a waypoint — so this module mirrors
//! `core::trails` and reuses its reference-resolution + auto-update-on-move
//! machinery (`DoubleLinkRef`, `resolve_reference`, the `rewrite_*`
//! helpers).
//!
//! Per-spec (`docs/kanban.md` §"Board-doc shape"), a non-`.md` file with
//! `hiker.kind: board` is NOT a board — callers verify the extension
//! before parsing.
//
// status: board-doc-shape
// status: board-column-model

use serde::{Deserialize, Serialize};
use serde_yml::Value as YamlValue;
use thiserror::Error;

use crate::errors::HikerError;
use crate::frontmatter::{assemble, merge_json_into_yaml, split, Error as FmError};
use crate::store::Store;
use crate::trails::ops::{resolve_reference, ResolutionOutcome};
use crate::trails::DoubleLinkRef;
use crate::vault::Vault;

pub mod ops;
#[cfg(test)]
mod tests;

/// One column of a board: a user-named, ordered list of card references.
/// Empty columns render — the column set is explicit, never inferred from
/// the cards present.
///
/// status: board-column-model
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(default)]
    pub cards: Vec<DoubleLinkRef>,
    /// Optional per-column WIP (work-in-progress) limit. When set, the
    /// board view shows the column's count against this cap and flags
    /// overflow. `None` is omitted from the serialized frontmatter so
    /// columns without a limit round-trip unchanged.
    ///
    /// status: board-wip-limits
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wip_limit: Option<usize>,
}

/// Parsed `hiker.*` frontmatter for a board-doc. Sibling fields under
/// `hiker.*` and any non-`hiker` top-level keys round-trip via the source
/// YAML and are not part of this struct — round-trip is via `parse_board`
/// / `write_board_frontmatter`, which preserve unknown siblings (mirrors
/// `TrailDocFrontmatter`).
///
/// status: board-doc-shape
/// status: board-column-model
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    #[serde(default)]
    pub columns: Vec<Column>,
}

impl Board {
    /// True if any column already holds a card pointing at `note_rel` (by
    /// path) or `note_id` (by ULID, when non-empty). Drives the
    /// per-board idempotency check for `add_card` and the
    /// "Already on this board" verb state.
    ///
    /// status: board-add-card
    pub fn contains_note(&self, note_id: &str, note_rel: &str) -> bool {
        self.columns.iter().any(|c| {
            c.cards
                .iter()
                .any(|card| card.path == note_rel || (!note_id.is_empty() && card.id == note_id))
        })
    }

    /// Index of the column named `name`, if present.
    fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing frontmatter (expected hiker.kind = board)")]
    MissingFrontmatter,
    #[error("hiker.kind expected `board`, found `{found}`")]
    KindMismatch { found: String },
    #[error("required field `{0}` missing or wrong type")]
    MissingField(&'static str),
    #[error("frontmatter not a mapping")]
    NotMapping,
    #[error("non-.md path cannot be a board-doc: {0}")]
    NotMarkdown(String),
    #[error("frontmatter assemble: {0}")]
    Assemble(#[from] FmError),
}

/// Parse a board-doc's frontmatter. Caller MUST verify the source path has
/// a `.md` extension before calling this — a non-`.md` file with
/// `hiker.kind: board` is not a board per spec; `parse_board_for` is the
/// path-aware wrapper.
///
/// status: board-doc-shape
pub fn parse_board(source: &str) -> Result<Board, Error> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(Error::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(Error::NotMapping);
    };
    let hiker = map.get("hiker").ok_or(Error::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(Error::MissingField("hiker"));
    };

    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.kind"))?;
    if kind != "board" {
        return Err(Error::KindMismatch {
            found: kind.to_string(),
        });
    }

    let id = hiker_map
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.id"))?
        .to_string();

    let columns = match hiker_map.get("columns") {
        None => Vec::new(),
        Some(YamlValue::Sequence(seq)) => seq.iter().filter_map(parse_column).collect(),
        Some(_) => return Err(Error::MissingField("hiker.columns")),
    };

    Ok(Board { id, columns })
}

/// Path-aware wrapper around `parse_board`: rejects non-`.md` extensions
/// before parsing, per the spec's "discriminator alone isn't enough" rule.
pub fn parse_board_for(rel: &str, source: &str) -> Result<Board, Error> {
    if !rel.ends_with(".md") {
        return Err(Error::NotMarkdown(rel.to_string()));
    }
    parse_board(source)
}

fn parse_column(v: &YamlValue) -> Option<Column> {
    let YamlValue::Mapping(m) = v else { return None };
    let name = m.get("name")?.as_str()?.to_string();
    let cards = match m.get("cards") {
        Some(YamlValue::Sequence(seq)) => seq.iter().filter_map(parse_card).collect(),
        _ => Vec::new(),
    };
    // `wip_limit` is optional; absent or non-integer → no limit.
    let wip_limit = m
        .get("wip_limit")
        .and_then(YamlValue::as_u64)
        .map(|n| n as usize);
    Some(Column { name, cards, wip_limit })
}

fn parse_card(v: &YamlValue) -> Option<DoubleLinkRef> {
    let YamlValue::Mapping(m) = v else { return None };
    let id = m.get("id")?.as_str()?.to_string();
    let path = m.get("path")?.as_str()?.to_string();
    Some(DoubleLinkRef { id, path })
}

/// Serialize a board-doc frontmatter back into the source. Preserves
/// non-`hiker.*` top-level fields and any unknown sibling fields under
/// `hiker.*` — only `hiker.{kind,id,columns}` are rewritten, mirroring
/// `write_trail_doc_frontmatter`'s deep-merge semantics.
///
/// status: board-doc-shape
pub fn write_board_frontmatter(body_source: &str, board: &Board) -> Result<String, Error> {
    let split_view = split(body_source);
    let mut existing = match split_view.frontmatter {
        Some(v) => v,
        None => YamlValue::Mapping(Default::default()),
    };
    if !matches!(existing, YamlValue::Mapping(_)) {
        existing = YamlValue::Mapping(Default::default());
    }
    let mut hiker_patch = serde_json::Map::new();
    hiker_patch.insert("kind".into(), serde_json::Value::String("board".into()));
    hiker_patch.insert("id".into(), serde_json::Value::String(board.id.clone()));
    hiker_patch.insert(
        "columns".into(),
        serde_json::Value::Array(board.columns.iter().map(column_to_json).collect()),
    );
    let patch = serde_json::json!({ "hiker": serde_json::Value::Object(hiker_patch) });
    // `merge_json_into_yaml` deep-merges maps but *replaces* arrays — so
    // the existing `columns` array is fully overwritten with the new one.
    // Strip the pre-existing `hiker.columns` first so no stale entries
    // linger if the merge ever changes its array policy.
    if let YamlValue::Mapping(top) = &mut existing
        && let Some(YamlValue::Mapping(hiker)) = top.get_mut("hiker")
    {
        hiker.remove("columns");
    }
    merge_json_into_yaml(&mut existing, patch);
    Ok(assemble(&existing, split_view.body)?)
}

fn column_to_json(c: &Column) -> serde_json::Value {
    let cards: Vec<_> = c
        .cards
        .iter()
        .map(|card| serde_json::json!({ "id": card.id, "path": card.path }))
        .collect();
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), serde_json::Value::String(c.name.clone()));
    obj.insert("cards".into(), serde_json::Value::Array(cards));
    // Omit `wip_limit` entirely when unset so limit-free columns round-trip
    // without acquiring a `wip_limit: null` key.
    if let Some(limit) = c.wip_limit {
        obj.insert("wip_limit".into(), serde_json::Value::from(limit));
    }
    serde_json::Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Listing / detail helpers — drives the board view + the planned MCP tools.
// Lives in `core` (not the adapter) because the UI and the future MCP tools
// share the same data-shaping policy.
// ---------------------------------------------------------------------------

/// One row of `list`. Title is the board-doc's basename without `.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardListItem {
    pub rel_path: String,
    pub board_id: String,
    pub title: String,
    pub column_count: u32,
    pub card_count: u32,
}

/// One card of a resolved board, post reference-resolution. `title` is the
/// referenced note's display title; `resolution` reports whether the
/// double-link resolves, self-heals, conflicts, or is broken (orphan).
///
/// status: board-card-references
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedCard {
    pub card_ref: DoubleLinkRef,
    pub title: String,
    pub resolution: ResolutionOutcome,
}

/// One column of a resolved board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedColumn {
    pub name: String,
    pub cards: Vec<ResolvedCard>,
    /// The column's WIP limit, if any. Drives the count-vs-limit badge and
    /// overflow flag in the board view.
    ///
    /// status: board-wip-limits
    #[serde(default)]
    pub wip_limit: Option<usize>,
}

/// Full detail bundle returned by `get_board`: the board-doc body plus the
/// resolved columns the board view renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardDetail {
    pub rel_path: String,
    pub board_id: String,
    pub body: String,
    pub columns: Vec<ResolvedColumn>,
}

/// Enumerate every board-doc in the vault. Strategy mirrors `trails::list`:
/// walk the indexer's note-path listing and try `parse_board_for` on each
/// `.md` file; rows that parse Ok are board-docs.
///
/// status: board-many-to-many
pub fn list(vault: &Vault, store: &Store) -> Result<Vec<BoardListItem>, HikerError> {
    let paths = store
        .all_note_paths()
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for rel in paths {
        if !rel.ends_with(".md") {
            continue;
        }
        let Ok(src) = vault.read_file(&rel) else { continue };
        let Ok(board) = parse_board_for(&rel, &src) else { continue };
        let card_count: u32 = board.columns.iter().map(|c| c.cards.len() as u32).sum();
        out.push(BoardListItem {
            rel_path: rel.clone(),
            board_id: board.id,
            title: {
                let base = rel.rsplit('/').next().unwrap_or(&rel);
                base.strip_suffix(".md").unwrap_or(base).to_string()
            },
            column_count: board.columns.len() as u32,
            card_count,
        });
    }
    Ok(out)
}

/// Fetch a single board's body + resolved columns. The board-doc's
/// `hiker.columns` array is the source of truth for column + card order;
/// each card is resolved against the index for self-heal / broken-card
/// rendering.
///
/// status: board-view
/// status: board-card-references
pub fn get_board(
    vault: &Vault,
    store: &Store,
    board_doc_rel: &str,
) -> Result<BoardDetail, HikerError> {
    let src = vault.read_file(board_doc_rel)?;
    let board = parse_board_for(board_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse board-doc: {e}")))?;
    let body = split(&src).body.to_string();

    let columns = board
        .columns
        .iter()
        .map(|col| ResolvedColumn {
            name: col.name.clone(),
            wip_limit: col.wip_limit,
            cards: col
                .cards
                .iter()
                .map(|card| {
                    let resolution =
                        resolve_reference(store, vault, card).unwrap_or(ResolutionOutcome::Orphan);
                    // Display title: the canonical path's basename when the
                    // card resolves / self-heals; the recorded path's
                    // basename otherwise.
                    let title_path = match &resolution {
                        ResolutionOutcome::Resolved { rel_path, .. } => rel_path.clone(),
                        ResolutionOutcome::SelfHeal { canonical_path, .. } => {
                            canonical_path.clone()
                        }
                        _ => card.path.clone(),
                    };
                    ResolvedCard {
                        card_ref: card.clone(),
                        title: title_of(&title_path),
                        resolution,
                    }
                })
                .collect(),
        })
        .collect();

    Ok(BoardDetail {
        rel_path: board_doc_rel.to_string(),
        board_id: board.id,
        body,
        columns,
    })
}

/// One row of `containing_note_with_paths`. Pairs the derived-table hit's
/// `board_id` with the board-doc's vault-relative path so the UI can
/// decide membership for any specific board without a second round-trip.
///
/// status: board-add-card
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainingNoteHit {
    pub board_id: String,
    pub board_doc_rel: String,
    pub column_name: String,
}

/// Reverse-lookup: which boards contain `source_rel` as a card. Resolves
/// each derived-table `board_id` to its board-doc rel-path via the same
/// `list` walk the picker uses, so the UI gets both halves in one call.
/// Symmetric to `trails::containing_note_with_paths`.
///
/// status: board-many-to-many
/// status: board-add-card
pub fn containing_note_with_paths(
    vault: &Vault,
    store: &Store,
    source_rel: &str,
) -> Result<Vec<ContainingNoteHit>, HikerError> {
    let hits = store
        .boards_containing_note(source_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    let listing = list(vault, store)?;
    let mut by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for b in &listing {
        by_id.insert(b.board_id.as_str(), b.rel_path.as_str());
    }
    let mut out = Vec::new();
    for h in hits {
        if let Some(rel) = by_id.get(h.board_id.as_str()) {
            out.push(ContainingNoteHit {
                board_id: h.board_id,
                board_doc_rel: (*rel).to_string(),
                column_name: h.column_name,
            });
        }
        // Hit without a matching board-doc row → board-doc removed since the
        // indexer wrote the row; skip. The next re-derive cleans it up.
    }
    Ok(out)
}

/// Compute the new board-doc source after appending `source_rel` as a card
/// to `column_name`, without writing anything. Read-only: resolves the
/// source note's current ULID via the index (empty when unstamped — the
/// path half still anchors the card). Returns `Ok(None)` when the note is
/// already a card anywhere on the board (idempotent no-op), so callers can
/// skip a redundant write/stage. Drives the review-mode MCP `board_add_card`
/// staging path; the direct path uses [`ops::add_card`], which also lazy-
/// stamps the source note's ULID.
///
/// status: board-mcp-tools
pub fn add_card_preview(
    vault: &Vault,
    store: &Store,
    board_doc_rel: &str,
    column_name: &str,
    source_rel: &str,
) -> Result<Option<String>, HikerError> {
    let src = vault.read_file(board_doc_rel)?;
    let mut board = parse_board_for(board_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse board-doc: {e}")))?;
    let source_id = store
        .id_for_path(source_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .unwrap_or_default();
    if board.contains_note(&source_id, source_rel) {
        return Ok(None);
    }
    let col_idx = board
        .column_index(column_name)
        .ok_or_else(|| HikerError::NotFound(format!("column: {column_name}")))?;
    board.columns[col_idx].cards.push(DoubleLinkRef {
        id: source_id,
        path: source_rel.to_string(),
    });
    let new_src = write_board_frontmatter(&src, &board)
        .map_err(|e| HikerError::Io(format!("rewrite board-doc: {e}")))?;
    Ok(Some(new_src))
}

/// Display title from a vault-relative path: basename without `.md`.
fn title_of(rel: &str) -> String {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// Auto-update-on-move entry: rewrite board-doc card path references when a
/// referenced note (or a board-doc itself) moves. This is the module-root
/// entry the indexer calls; the sweep itself lives in
/// [`ops::run_note_moved`]. Errors are logged inside, never propagated.
/// Returns the count of board-docs whose frontmatter was rewritten.
///
/// status: board-card-references
pub async fn on_note_moved(
    watcher: Option<&crate::watcher::Watcher>,
    jobs: Option<&crate::indexer::IndexJobTx>,
    log: Option<&crate::oplog::OpLog>,
    vault: &Vault,
    store: &mut Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    ops::run_note_moved(watcher, jobs, log, vault, store, old_rel, new_rel).await
}
