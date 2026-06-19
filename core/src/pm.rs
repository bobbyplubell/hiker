//! PM semantics over the built-in kinds. See `docs/pm.md`.
//!
//! This module owns the PM-specific reads and ops that consume the kind
//! registry (`docs/kinds.md`) and the board surface (`docs/kanban.md`):
//!
//! - **Derived status** (`derived-status-rule`): a work note's status is a
//!   *read* — its column in its one sprint-kind board, mapped through the
//!   kind's column-state map. Never stored, no reconciliation. Hand-edited
//!   double membership surfaces as a loud [`DerivedStatus::Conflicted`].
//! - **One-sprint enforcement** ([`ensure_single_sprint_membership`]):
//!   refused at every card-add op hiker controls; ingest never rejects.
//! - **Sprint close / rollover** (`sprint-rollover`): [`close_sprint`]
//!   stages one multi-doc batch (author `auto:sprint-close`) moving every
//!   non-`done`/`canceled`-category card to the destination board and
//!   stamping `closed_at` into the closing board-doc. The default
//!   destination is plan-derived: the plan's next sprint by `start`, else
//!   the plan's backlog board.
//! - **Epic rollup** (`pm-epic-rollup`): an epic is a list-like note (an
//!   ordered `hiker.refs` list of `{ path }` members); [`epic_progress`]
//!   is a category-anchored rollup over the members' derived statuses —
//!   never stored, so it can never disagree with the boards.
//! - **Plan resolution** (`plan-kind`): a plan is the list-like root
//!   container; a board or epic belongs to a plan iff the plan's refs (or
//!   its `backlog` key) name it. A plan owns *defaults only*
//!   ([`plan_default_kind`], [`default_rollover_destination`]) — never a
//!   column-state mapping, never member status.
//!
//! Sprint metrics replay the layered doc in [`metrics`] (`pm-layered-metrics`).
//
// status: derived-status-rule
// status: sprint-rollover

use serde_yml::Value as YamlValue;

use crate::boards::{parse_board_for, write_board_frontmatter, Board, BoardCard};
use crate::errors::HikerError;
use crate::frontmatter::{assemble, iso_date_epoch, merge_json_into_yaml, split};
use crate::indexer::{IndexJob, IndexJobTx};
use crate::kinds::{Kind, Registry, StateCategory};
use crate::editing::LayeredDoc;
use crate::ops::op_writes::{self, ContentStage};
use crate::store::dto::{MetaFilter, NoteQuery};
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;

pub mod metrics;

/// The derived status of a work note (`derived-status-rule`): computed
/// from `board_cards` plus the registry, never stored anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedStatus {
    /// No sprint membership, or the note sits in an unmapped lane —
    /// rollups count this under the `backlog` category.
    None,
    /// The one sprint's mapped column, resolved through the kind's
    /// column-state map: the state is the status, its category is what
    /// rollups and automation consume.
    Active {
        sprint_path: String,
        column: String,
        state: String,
        category: StateCategory,
    },
    /// Hand-edited multiple membership (two sprints, or two columns of
    /// one sprint) — the explicit conflicted outcome pm.md requires,
    /// rendered as a problem pill on the affected sprints' cards until
    /// the user removes one. `sprint_paths` is the distinct sprint set.
    Conflicted { sprint_paths: Vec<String> },
}

/// One sprint-kind membership hit for a note: the holding board's path,
/// the column the card sits in, and the board's kind name.
struct SprintHit {
    board_path: String,
    column: String,
    kind: String,
}

/// Every card of `note_rel` sitting on a board whose `hiker.kind` is a
/// registered board-like kind — the join pm.md names: `board_cards`
/// reverse lookup + the board's kind off `note_meta`. Plain `board`-kind
/// boards never appear here (zero PM semantics).
fn sprint_hits(
    store: &Store,
    registry: &Registry,
    note_rel: &str,
) -> Result<Vec<SprintHit>, HikerError> {
    let hits = store
        .boards_containing_note(note_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for hit in hits {
        let Some(kind_name) = store
            .meta_value(&hit.board_path, "hiker.kind")
            .map_err(|e| HikerError::Io(e.to_string()))?
        else {
            continue;
        };
        if registry.board_like(&kind_name).is_none() {
            continue;
        }
        out.push(SprintHit {
            board_path: hit.board_path,
            column: hit.column_name,
            kind: kind_name,
        });
    }
    Ok(out)
}

/// The derived-status read (`derived-status-rule`): find the note's cards
/// on sprint-kind boards, take the one sprint, map its column through the
/// kind's column-state map. Exactly one source of truth, zero
/// reconciliation — no `status:` frontmatter exists anywhere. A note in an
/// unmapped column, or on no sprint at all, derives [`DerivedStatus::None`]
/// (rollups count it under `backlog`); more than one membership is the
/// loud [`DerivedStatus::Conflicted`].
///
/// status: derived-status-rule
pub fn derived_status(
    store: &Store,
    registry: &Registry,
    note_rel: &str,
) -> Result<DerivedStatus, HikerError> {
    let hits = sprint_hits(store, registry, note_rel)?;
    match hits.as_slice() {
        [] => Ok(DerivedStatus::None),
        [hit] => {
            let Some(kind) = registry.board_like(&hit.kind) else {
                return Ok(DerivedStatus::None);
            };
            match column_state(kind, &hit.column) {
                Some((state, category)) => Ok(DerivedStatus::Active {
                    sprint_path: hit.board_path.clone(),
                    column: hit.column.clone(),
                    state,
                    category,
                }),
                // An unmapped lane ("Icebox") is a plain lane with no PM
                // semantics — no derived status.
                None => Ok(DerivedStatus::None),
            }
        }
        many => {
            let mut sprint_paths: Vec<String> =
                many.iter().map(|h| h.board_path.clone()).collect();
            sprint_paths.sort();
            sprint_paths.dedup();
            Ok(DerivedStatus::Conflicted { sprint_paths })
        }
    }
}

/// The mapped `(state, category)` of a column on a board-like kind, when
/// the column name appears in the kind's column-state map.
fn column_state(kind: &Kind, column: &str) -> Option<(String, StateCategory)> {
    let state = kind.columns.get(column)?;
    let category = kind.state_category(state)?;
    Some((state.clone(), category))
}

/// The at-most-one-sprint guard, enforced where membership is written
/// (`derived-status-rule`): adding a note card to a sprint-kind board is
/// refused — naming the holding sprint — when the note is already a card
/// on a *different* sprint-kind board. A no-op when the target board is a
/// plain board (`board-many-to-many` untouched), when no registry is
/// attached, or when the note's only sprint membership is the target board
/// itself (the per-board idempotency check owns that case).
///
/// Normalize a vault-relative path to the canonical form the store keys
/// boards by: strip a leading `./`, drop redundant `.` / empty segments,
/// and join on `/`. Case is preserved (vaults are case-sensitive on the
/// filesystems hiker targets, so case-folding here could wrongly equate two
/// distinct notes). Pure string work — no filesystem access.
fn normalize_vault_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// status: derived-status-rule
pub fn ensure_single_sprint_membership(
    store: &Store,
    kinds: Option<&Registry>,
    board_rel: &str,
    board_kind: &str,
    note_rel: &str,
) -> Result<(), HikerError> {
    let Some(registry) = kinds else { return Ok(()) };
    if registry.board_like(board_kind).is_none() {
        return Ok(());
    }
    let hits = sprint_hits(store, registry, note_rel)?;
    // Store-derived `board_path`s are already canonical vault-relative
    // (no leading `./`, no `.` segments); the caller-supplied `board_rel`
    // may not be (e.g. a `./boards/s1.md` form). Normalize the target so a
    // denormalized but identical path is recognized as the SAME board and
    // doesn't falsely look like a *different* holding sprint — which would
    // refuse an idempotent re-add to the board the note already sits on.
    let target = normalize_vault_rel(board_rel);
    if let Some(holding) = hits
        .iter()
        .find(|h| normalize_vault_rel(&h.board_path) != target)
    {
        return Err(HikerError::AlreadyExists(format!(
            "`{note_rel}` is already a card on sprint `{}` — a work note belongs to \
             at most one sprint; remove it there first",
            holding.board_path,
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Apply-time one-sprint re-check (the flip seam).
// ---------------------------------------------------------------------------

/// Re-verify the one-sprint invariant for pending ops at the moment they
/// are ACCEPTED — the apply-time half of `derived-status-rule`. The
/// stage-time [`ensure_single_sprint_membership`] check reads the derived
/// state when a card-add is *staged*; under review mode that can be hours
/// before the flip (two staged adds to different sprints each pass at
/// stage time), and even outside review the derived table re-derives
/// async (two rapid adds both pass). This check re-reads membership off
/// the layered doc's ACCEPTED texts at flip time and refuses with the typed
/// [`HikerError::SprintConflict`], leaving the op pending — the review
/// surface shows the refusal reason (the drifted-member precedent).
///
/// Layering: the layered-doc substrate stays pure (no store / registry / kind
/// knowledge), so this lives in `pm` and is invoked from the ops-layer
/// flip wrappers (`op_writes::flip_op_status_checked` /
/// `flip_batch_status_checked`), where producers already hold the handles.
///
/// `ops` is the `(doc_id, op_id)` set flipping together. The whole set is
/// evaluated as ONE post-flip state: a sprint-close batch whose
/// destination gains a card the closing board simultaneously loses passes,
/// while a per-op accept of just the destination half is refused — exactly
/// the half-a-close split pm.md forbids. Caveat, stated honestly: accept
/// remains per-item partial-apply after this gate, so a member that fails
/// mid-batch can still split a batch this check passed; the batch
/// drift-disable in the review UI covers the known cause.
///
/// status: derived-status-rule
pub fn verify_flip_single_sprint(
    log: &LayeredDoc,
    vault: &Vault,
    store: &Store,
    registry: &Registry,
    ops: &[(String, String)],
) -> Result<(), HikerError> {
    let proposed = proposed_texts(log, ops);
    let additions = sprint_card_additions(log, vault, registry, &proposed);
    if additions.is_empty() {
        return Ok(());
    }
    let boards = effective_sprint_boards(log, vault, store, registry, &proposed, &additions)?;
    for (board_rel, note) in &additions {
        let holders: Vec<&str> = boards
            .iter()
            .filter(|(rel, board)| rel != board_rel && board.contains_note(note))
            .map(|(rel, _)| rel.as_str())
            .collect();
        if !holders.is_empty() {
            return Err(HikerError::SprintConflict(format!(
                "accepting this change would put `{note}` on sprint `{board_rel}` while it \
                 is already a card on `{}` — a work note belongs to at most one sprint; \
                 reject this proposal or remove the other card first",
                holders.join("`, `"),
            )));
        }
    }
    Ok(())
}

/// The post-op text of each flipping op, keyed by the doc's vault-relative
/// path: `materialize(accepted + just that op)`. An op that no longer
/// materializes (already flipped, unknown doc) is skipped with a warning —
/// the raw flip surfaces the real failure. Multi-doc batches stage one op
/// per doc, so the per-doc key never collapses distinct edits in practice.
fn proposed_texts(log: &LayeredDoc, ops: &[(String, String)]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (doc_id, op_id) in ops {
        let rel = match log.path_for_doc(doc_id) {
            Ok(Some(rel)) => rel,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(doc_id, error = %e, "one-sprint flip check: path lookup failed");
                continue;
            }
        };
        match log.materialize_with_pending_op(doc_id, op_id) {
            Ok(content) => {
                out.insert(rel, content.text);
            }
            Err(e) => {
                tracing::warn!(doc_id, op_id, error = %e,
                    "one-sprint flip check: could not materialize pending op");
            }
        }
    }
    out
}

/// The note cards each proposed text ADDS to a sprint-kind board, as
/// `(board_rel, note_rel)` pairs: proposed membership minus the board's
/// current accepted membership. Docs that don't parse as board-like
/// boards never add cards and contribute nothing.
fn sprint_card_additions(
    log: &LayeredDoc,
    vault: &Vault,
    registry: &Registry,
    proposed: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (rel, text) in proposed {
        let Ok(board) = parse_board_for(rel, text, Some(registry)) else {
            continue;
        };
        if registry.board_like(&board.kind).is_none() {
            continue;
        }
        let before: std::collections::BTreeSet<String> = accepted_text_of(log, vault, rel)
            .and_then(|src| parse_board_for(rel, &src, Some(registry)).ok())
            .map(|b| board_note_paths(&b).into_iter().collect())
            .unwrap_or_default();
        for note in board_note_paths(&board) {
            if !before.contains(&note) {
                out.push((rel.clone(), note));
            }
        }
    }
    out
}

/// A doc's accepted text: `materialize(accepted)` when the path has an
/// layered doc, else the disk bytes (disk IS accepted for a never-staged
/// doc under `op-log-disk-canonical`). `None` when neither reads.
fn accepted_text_of(log: &LayeredDoc, vault: &Vault, rel: &str) -> Option<String> {
    if let Ok(Some(doc_id)) = log.doc_id_for_path(rel)
        && let Ok(content) = log.materialize_accepted(&doc_id)
    {
        return Some(content.text);
    }
    vault.read_file(rel).ok()
}

/// The note paths carded anywhere on `board`.
fn board_note_paths(board: &Board) -> Vec<String> {
    board
        .columns
        .iter()
        .flat_map(|c| c.cards.iter())
        .filter_map(|card| match card {
            BoardCard::Note { path } => Some(path.clone()),
            BoardCard::Text { .. } => None,
        })
        .collect()
}

/// Every sprint-kind board's EFFECTIVE state for this flip: the flip's own
/// proposed text where the doc is part of the set (a close batch's closing
/// board counts its cards as already removed), the accepted text
/// otherwise. Candidates are the indexed sprint-kind docs (`note_meta`
/// `hiker.kind` in the registry's board-like set) unioned with the derived
/// `board_cards` holders of each added note and the flip's own docs.
fn effective_sprint_boards(
    log: &LayeredDoc,
    vault: &Vault,
    store: &Store,
    registry: &Registry,
    proposed: &std::collections::BTreeMap<String, String>,
    additions: &[(String, String)],
) -> Result<Vec<(String, Board)>, HikerError> {
    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let board_like: Vec<String> = registry
        .iter()
        .filter(|k| registry.board_like(&k.name).is_some())
        .map(|k| k.name.clone())
        .collect();
    if !board_like.is_empty() {
        let query = NoteQuery {
            filters: vec![MetaFilter::Equals {
                key: "hiker.kind".to_string(),
                values: board_like,
            }],
            ..Default::default()
        };
        candidates.extend(
            store
                .query_notes(&query)
                .map_err(|e| HikerError::Io(e.to_string()))?
                .into_iter()
                .map(|row| row.path),
        );
    }
    for (_, note) in additions {
        candidates.extend(
            store
                .boards_containing_note(note)
                .map_err(|e| HikerError::Io(e.to_string()))?
                .into_iter()
                .map(|hit| hit.board_path),
        );
    }
    candidates.extend(proposed.keys().cloned());
    let mut out = Vec::new();
    for rel in candidates {
        let Some(text) = proposed
            .get(&rel)
            .cloned()
            .or_else(|| accepted_text_of(log, vault, &rel))
        else {
            continue;
        };
        let Ok(board) = parse_board_for(&rel, &text, Some(registry)) else {
            continue;
        };
        if registry.board_like(&board.kind).is_some() {
            out.push((rel, board));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sprint close / rollover.
// ---------------------------------------------------------------------------

/// Typed failures of [`close_sprint`]. `MissingTarget` covers both an
/// unusable explicit pick (missing file, not a board-doc, the closing
/// sprint itself) and the no-default case: no destination picked and the
/// sprint belongs to no plan with a next sprint or a backlog board
/// ([`default_rollover_destination`] resolved nothing).
#[derive(Debug, thiserror::Error)]
pub enum CloseError {
    #[error("`{path}` is not a sprint-kind board (hiker.kind `{kind}`)")]
    NotSprint { path: String, kind: String },
    #[error("sprint `{path}` is already closed (closed_at: {closed_at})")]
    AlreadyClosed { path: String, closed_at: String },
    #[error("no rollover destination: {0}")]
    MissingTarget(String),
    #[error("destination `{0}` has no columns to receive cards")]
    DestinationHasNoColumns(String),
    #[error(transparent)]
    Other(#[from] HikerError),
}

/// Borrow-bundle for [`close_sprint`] — the environment handles plus the
/// closing sprint and the destination board-doc.
pub struct CloseSprint<'a> {
    pub log: &'a LayeredDoc,
    pub vault: &'a Vault,
    /// Backs the plan-derived default destination and (transitively) the
    /// `owning_plan` membership read. Read-only.
    pub store: &'a Store,
    pub registry: &'a Registry,
    /// The sprint-kind board-doc being closed.
    pub closing_rel: &'a str,
    /// The destination board-doc — another sprint-kind board, or any plain
    /// board serving as a backlog. `None` resolves pm.md's default: the
    /// plan's next sprint by `start` date, else the plan's backlog board,
    /// else the typed [`CloseError::MissingTarget`].
    pub destination_rel: Option<&'a str>,
    /// Whether the staged close batch waits for review. `false` flips the
    /// whole batch through `op_writes::flip_batch_status` immediately
    /// after staging (the `suggest.rs` triage auto-accept precedent), so
    /// the rollover is atomic when review isn't on — accepting one doc and
    /// forgetting the other can't split the close. `true` leaves the batch
    /// pending; the review surfaces present it as ONE unit.
    pub review_required: bool,
}

/// What [`close_sprint`] staged: the one batch id spanning both board-docs
/// (flip it through `op_writes::flip_batch_status` / the standard staging
/// surfaces), the per-doc op ids, the count of cards rolled over, and the
/// destination board + column they were appended to (`destination_rel`
/// matters when the caller passed `None` and the plan default resolved it).
#[derive(Debug, Clone)]
pub struct CloseOutcome {
    pub batch_id: String,
    pub op_ids: Vec<String>,
    pub moved: usize,
    pub destination_rel: String,
    pub destination_column: String,
    /// `true` when the batch was auto-accepted (`review_required: false`)
    /// — both board-docs already applied to disk. `false` means the close
    /// is staged and pending review.
    pub applied: bool,
}

/// Close a sprint (`sprint-rollover`): stage, as ONE batch authored
/// `auto:sprint-close`, (a) the destination board-doc gaining every card
/// from the closing sprint's non-`done`/`canceled`-category columns
/// (appended to the destination's first `todo`-category-mapped column,
/// else its first column), (b) the closing board-doc losing those cards,
/// and (c) `closed_at: <iso8601>` stamped into the closing board-doc's
/// frontmatter — the double-close guard and the metrics end marker.
/// Unmapped-lane and freeform cards roll over too: only `done`/`canceled`-
/// category columns keep their cards, so unfinished work can never
/// silently vanish into a closed sprint.
///
/// With `review_required: true` the batch is staged, not committed: the
/// set of moves is machine-computed, so it reviews like a cluster reorg
/// batch — as ONE unit. With `review_required: false` the batch is
/// auto-accepted here, immediately after staging, so the close lands on
/// both board-docs atomically (the `suggest.rs` triage precedent).
///
/// status: sprint-rollover
pub fn close_sprint(args: &CloseSprint<'_>) -> Result<CloseOutcome, CloseError> {
    let CloseSprint {
        log, vault, store, registry, closing_rel, destination_rel, review_required,
    } = *args;
    let closing_src = vault.read_file(closing_rel).map_err(CloseError::Other)?;
    let mut closing = parse_board_for(closing_rel, &closing_src, Some(registry))
        .map_err(|e| CloseError::Other(HikerError::Io(format!("parse closing board: {e}"))))?;
    let Some(closing_kind) = registry.board_like(&closing.kind) else {
        return Err(CloseError::NotSprint {
            path: closing_rel.to_string(),
            kind: closing.kind,
        });
    };
    if let Some(stamp) = closed_at_of(&closing_src) {
        return Err(CloseError::AlreadyClosed {
            path: closing_rel.to_string(),
            closed_at: stamp,
        });
    }
    // No explicit pick: pm.md's default — the plan's next sprint by
    // `start`, else the plan's backlog board (`plan-kind`); neither is the
    // typed MissingTarget.
    let destination_owned = match destination_rel {
        Some(d) => d.to_string(),
        None => default_rollover_destination(store, registry, closing_rel)
            .map_err(CloseError::Other)?
            .ok_or_else(|| {
                CloseError::MissingTarget(
                    "no destination picked and the sprint belongs to no plan \
                     with a next sprint or backlog board"
                        .to_string(),
                )
            })?,
    };
    let destination_rel = destination_owned.as_str();
    if destination_rel == closing_rel {
        return Err(CloseError::MissingTarget(
            "destination is the closing sprint itself".to_string(),
        ));
    }
    let dest_src = vault.read_file(destination_rel).map_err(|e| {
        CloseError::MissingTarget(format!("destination `{destination_rel}`: {e}"))
    })?;
    let mut dest = parse_board_for(destination_rel, &dest_src, Some(registry)).map_err(|e| {
        CloseError::MissingTarget(format!(
            "destination `{destination_rel}` is not a board-doc: {e}"
        ))
    })?;

    // Drain every card outside a done/canceled-category column, in board
    // order. Unmapped lanes have no category, so they roll over.
    let mut moving: Vec<BoardCard> = Vec::new();
    for col in &mut closing.columns {
        let keeps = matches!(
            column_state(closing_kind, &col.name).map(|(_, cat)| cat),
            Some(StateCategory::Done | StateCategory::Canceled)
        );
        if !keeps {
            moving.append(&mut col.cards);
        }
    }

    let dest_column = receiving_column(registry, &dest)
        .ok_or_else(|| CloseError::DestinationHasNoColumns(destination_rel.to_string()))?;
    let moved = append_cards(&mut dest, &dest_column, moving);

    let closing_text = write_board_frontmatter(&closing_src, &closing)
        .map_err(|e| CloseError::Other(HikerError::Io(format!("rewrite closing board: {e}"))))
        .and_then(|text| stamp_closed_at(&text).map_err(CloseError::Other))?;
    let dest_text = write_board_frontmatter(&dest_src, &dest).map_err(|e| {
        CloseError::Other(HikerError::Io(format!("rewrite destination board: {e}")))
    })?;

    // One batch, two whole-doc texts — the op-log-reorg-batch shape, via
    // the multi-doc sibling of `stage_auto_content`.
    let outcome = op_writes::stage_auto_content_batch(
        log,
        vault,
        "sprint-close",
        "sprint-close",
        &[
            ContentStage { rel: closing_rel.to_string(), new_text: closing_text },
            ContentStage { rel: destination_rel.to_string(), new_text: dest_text },
        ],
    )
    .map_err(CloseError::Other)?;
    // Non-review mode: flip the whole batch right away (the `suggest.rs`
    // auto-accept precedent) — never leave half a close pending. Staging
    // and flipping are back-to-back, so neither op can have drifted; a
    // partial apply here is unexpected and logged loud. The checked flip
    // re-runs the one-sprint invariant against accepted state at apply
    // time (`derived-status-rule`); a legit close passes because the
    // batch's closing board counts its cards as already removed.
    let applied = if review_required {
        false
    } else {
        let ctx = op_writes::FlipCtx { vault, store, kinds: registry };
        let accepted = op_writes::flip_batch_status_checked(log, &ctx, &outcome.batch_id, true)
            .map_err(CloseError::Other)?;
        if accepted.len() != outcome.op_ids.len() {
            tracing::warn!(
                closing = closing_rel,
                destination = destination_rel,
                staged = outcome.op_ids.len(),
                applied = accepted.len(),
                "sprint-close auto-accept applied only part of the close batch"
            );
        }
        true
    };
    Ok(CloseOutcome {
        batch_id: outcome.batch_id,
        op_ids: outcome.op_ids,
        moved,
        destination_rel: destination_owned,
        destination_column: dest_column,
        applied,
    })
}

/// The destination column rollover appends to: the first column (in board
/// order) whose mapped state carries the `todo` category when the
/// destination is itself a board-like kind, else the board's first column.
/// `None` only when the destination has no columns at all.
fn receiving_column(registry: &Registry, dest: &Board) -> Option<String> {
    if let Some(kind) = registry.board_like(&dest.kind)
        && let Some(col) = dest.columns.iter().find(|c| {
            matches!(
                column_state(kind, &c.name).map(|(_, cat)| cat),
                Some(StateCategory::Todo)
            )
        })
    {
        return Some(col.name.clone());
    }
    dest.columns.first().map(|c| c.name.clone())
}

/// Append `cards` to `column` on `dest`, skipping note cards the
/// destination already holds (the per-board idempotency rule) and freeform
/// cards whose `card_id` already exists there. Returns the appended count.
fn append_cards(dest: &mut Board, column: &str, cards: Vec<BoardCard>) -> usize {
    let existing_ids: Vec<String> = dest
        .columns
        .iter()
        .flat_map(|c| c.cards.iter().filter_map(|card| card.card_id()))
        .map(str::to_string)
        .collect();
    let mut appended = 0usize;
    for card in cards {
        let duplicate = match &card {
            BoardCard::Note { path } => dest.contains_note(path),
            BoardCard::Text { card_id, .. } => existing_ids.iter().any(|id| id == card_id),
        };
        if duplicate {
            continue;
        }
        if let Some(col) = dest.columns.iter_mut().find(|c| c.name == column) {
            col.cards.push(card);
            appended += 1;
        }
    }
    appended
}

/// The closing board-doc's top-level `closed_at` frontmatter value, when
/// present — the marker that guards double-close.
fn closed_at_of(source: &str) -> Option<String> {
    let fm = split(source).frontmatter?;
    let value = fm.get("closed_at")?;
    match value {
        YamlValue::String(s) => Some(s.clone()),
        other => serde_yml::to_string(other)
            .ok()
            .map(|s| s.trim().to_string()),
    }
}

/// Stamp `closed_at: <iso8601 UTC now>` into the source's top-level
/// frontmatter (a plain key beside the sprint's `start`/`end` kind fields,
/// indexed by `note_meta` like any other). Whole seconds — the metadata
/// index's date mirror and the metrics close-frame lookup both parse
/// fraction-free RFC3339.
fn stamp_closed_at(source: &str) -> Result<String, HikerError> {
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .map_err(|e| HikerError::Io(format!("truncate closed_at: {e}")))?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| HikerError::Io(format!("format closed_at: {e}")))?;
    let view = split(source);
    let mut fm = view
        .frontmatter
        .unwrap_or_else(|| YamlValue::Mapping(Default::default()));
    merge_json_into_yaml(&mut fm, serde_json::json!({ "closed_at": now }));
    assemble(&fm, view.body).map_err(|e| HikerError::Io(format!("stamp closed_at: {e}")))
}

// ---------------------------------------------------------------------------
// List-like docs (epic / plan): parse + write.
// ---------------------------------------------------------------------------

/// Parsed `hiker.*` frontmatter for a list-like note (`kind-shapes`): the
/// registered list-like kind it carries (`epic`, `plan`, any user-defined
/// list-like entry) plus its ordered member refs — vault-relative paths,
/// path-as-identity. Sibling `hiker.*` fields and non-`hiker` top-level
/// keys round-trip via [`write_list_doc_frontmatter`], which preserves
/// unknown siblings (mirrors `Board` / `TrailDocFrontmatter`).
///
/// status: pm-epic-rollup
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListDoc {
    /// The note's `hiker.kind` — a registered list-like kind name.
    pub kind: String,
    /// Ordered member paths from the `hiker.refs` array of `{ path }`.
    pub refs: Vec<String>,
}

/// Typed failures of the list-doc parse, mirroring `boards::Error`.
#[derive(Debug, thiserror::Error)]
pub enum ListDocError {
    #[error("missing frontmatter (expected a registered list-like hiker.kind)")]
    MissingFrontmatter,
    #[error("hiker.kind `{found}` is not a registered list-like kind")]
    KindMismatch { found: String },
    #[error("required field `{0}` missing or wrong type")]
    MissingField(&'static str),
    #[error("frontmatter not a mapping")]
    NotMapping,
    #[error("non-.md path cannot be a list-doc: {0}")]
    NotMarkdown(String),
    #[error("frontmatter assemble: {0}")]
    Assemble(#[from] crate::frontmatter::Error),
}

/// Parse a list-like note's frontmatter. The accepted discriminator set is
/// every kind the compiled registry declares list-like — shape-driven,
/// never epic-special-cased, the same gate rule `boards::parse_board` uses
/// for board-like kinds. `kinds = None` (CLI / bare-test posture) accepts
/// nothing: unlike boards there is no plain built-in list discriminator.
/// Caller MUST verify the `.md` extension first; [`parse_list_doc_for`] is
/// the path-aware wrapper.
///
/// status: pm-epic-rollup
pub fn parse_list_doc(source: &str, kinds: Option<&Registry>) -> Result<ListDoc, ListDocError> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(ListDocError::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(ListDocError::NotMapping);
    };
    let hiker = map.get("hiker").ok_or(ListDocError::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(ListDocError::MissingField("hiker"));
    };
    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(ListDocError::MissingField("hiker.kind"))?;
    if kinds.is_none_or(|r| r.list_like(kind).is_none()) {
        return Err(ListDocError::KindMismatch { found: kind.to_string() });
    }
    let refs = match hiker_map.get("refs") {
        None => Vec::new(),
        Some(YamlValue::Sequence(seq)) => seq
            .iter()
            .filter_map(|v| {
                let YamlValue::Mapping(m) = v else { return None };
                Some(m.get("path")?.as_str()?.to_string())
            })
            .collect(),
        Some(_) => return Err(ListDocError::MissingField("hiker.refs")),
    };
    Ok(ListDoc { kind: kind.to_string(), refs })
}

/// Path-aware wrapper around [`parse_list_doc`]: rejects non-`.md`
/// extensions before parsing, per the shared "discriminator alone isn't
/// enough" rule boards, trails, and query-docs follow.
///
/// status: pm-epic-rollup
pub fn parse_list_doc_for(
    rel: &str,
    source: &str,
    kinds: Option<&Registry>,
) -> Result<ListDoc, ListDocError> {
    if !rel.ends_with(".md") {
        return Err(ListDocError::NotMarkdown(rel.to_string()));
    }
    parse_list_doc(source, kinds)
}

/// Serialize a list-doc's `hiker.{kind,refs}` back into the source,
/// preserving non-`hiker` top-level fields and unknown `hiker.*` siblings
/// (a plan's `default_kind` / `backlog` policy keys are top-level and
/// untouched). Mirrors `write_board_frontmatter`'s deep-merge semantics —
/// the existing `hiker.refs` array is stripped first so the merge never
/// leaves stale entries.
///
/// status: pm-epic-rollup
pub fn write_list_doc_frontmatter(
    body_source: &str,
    doc: &ListDoc,
) -> Result<String, ListDocError> {
    let split_view = split(body_source);
    let mut existing = match split_view.frontmatter {
        Some(v) => v,
        None => YamlValue::Mapping(Default::default()),
    };
    if !matches!(existing, YamlValue::Mapping(_)) {
        existing = YamlValue::Mapping(Default::default());
    }
    if let YamlValue::Mapping(top) = &mut existing
        && let Some(YamlValue::Mapping(hiker)) = top.get_mut("hiker")
    {
        hiker.remove("refs");
    }
    let refs: Vec<serde_json::Value> = doc
        .refs
        .iter()
        .map(|p| serde_json::json!({ "path": p }))
        .collect();
    merge_json_into_yaml(
        &mut existing,
        serde_json::json!({ "hiker": { "kind": doc.kind, "refs": refs } }),
    );
    Ok(assemble(&existing, split_view.body)?)
}

// ---------------------------------------------------------------------------
// Epic rollup.
// ---------------------------------------------------------------------------

/// One category's slice of an epic rollup: member count plus the summed
/// `estimate` of those members (joined from the metadata index at read
/// time — an epic stores nothing).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CategoryTally {
    pub count: usize,
    pub estimate: f64,
}

/// The category-anchored rollup [`epic_progress`] computes — per-category
/// member counts + estimate sums over the epic's ordered member refs,
/// derived fresh from the boards on every read and never stored, so it
/// can never disagree with them. Members with no derived status (no
/// sprint membership, unmapped lane, or a ref that resolves to nothing)
/// count under `backlog`; hand-edited multi-sprint members count under
/// `conflicted`, outside every category.
///
/// status: pm-epic-rollup
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpicProgress {
    pub total: usize,
    pub conflicted: usize,
    pub backlog: CategoryTally,
    pub todo: CategoryTally,
    pub in_progress: CategoryTally,
    pub done: CategoryTally,
    pub canceled: CategoryTally,
}

impl EpicProgress {
    /// The tally bucket anchored at `category`.
    const fn tally_mut(&mut self, category: StateCategory) -> &mut CategoryTally {
        match category {
            StateCategory::Backlog => &mut self.backlog,
            StateCategory::Todo => &mut self.todo,
            StateCategory::InProgress => &mut self.in_progress,
            StateCategory::Done => &mut self.done,
            StateCategory::Canceled => &mut self.canceled,
        }
    }

    /// The hover-preview / properties one-liner: `"4/9 done"`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{}/{} done", self.done.count, self.total)
    }
}

/// The epic rollup (`pm-epic-rollup`): run the derived-status read over
/// the epic's member set (the derived `list_refs` rows, in list order) and
/// count by category anchor, summing each member's `estimate` field off
/// the metadata index. Works on any list-like note — a plan rolls up the
/// same way if asked — because the table is generic over the shape.
///
/// status: pm-epic-rollup
pub fn epic_progress(
    store: &Store,
    registry: &Registry,
    list_path: &str,
) -> Result<EpicProgress, HikerError> {
    let members = store
        .members_of(list_path)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let mut progress = EpicProgress::default();
    for member in members {
        progress.total += 1;
        let estimate = estimate_of(store, &member.member_path);
        match derived_status(store, registry, &member.member_path)? {
            // No derived status — on no sprint, in an unmapped lane, or a
            // missing/unindexed member — counts under `backlog`.
            DerivedStatus::None => {
                let tally = progress.tally_mut(StateCategory::Backlog);
                tally.count += 1;
                tally.estimate += estimate;
            }
            DerivedStatus::Active { category, .. } => {
                let tally = progress.tally_mut(category);
                tally.count += 1;
                tally.estimate += estimate;
            }
            DerivedStatus::Conflicted { .. } => progress.conflicted += 1,
        }
    }
    Ok(progress)
}

/// A note's `estimate` frontmatter value off the metadata index, parsed as
/// a number; `0.0` when absent or non-numeric. Joined at read time — pm.md
/// stores no estimate anywhere but the story note itself.
fn estimate_of(store: &Store, note_rel: &str) -> f64 {
    store
        .meta_value(note_rel, "estimate")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Plan resolution (`plan-kind`).
// ---------------------------------------------------------------------------

/// The built-in plan kind's name. A plan is not a special shape — it's the
/// list-like `plan` registry entry; these helpers only resolve when that
/// entry is registered (and list-like), so a vault that disables it loses
/// plan semantics cleanly.
pub const PLAN_KIND: &str = "plan";

/// The plan that owns `note_rel`, if any: a board or epic belongs to a
/// plan iff the plan's refs (read through the derived `list_refs` table)
/// or its `backlog` policy key name it — membership is plan-owned, never
/// stored on the member. Multiple claiming plans resolve to the
/// lexicographically first (deterministic; pm.md defines no tiebreak).
///
/// status: plan-kind
pub fn owning_plan(
    store: &Store,
    registry: &Registry,
    note_rel: &str,
) -> Result<Option<String>, HikerError> {
    if registry.list_like(PLAN_KIND).is_none() {
        return Ok(None);
    }
    let mut plans: Vec<String> = store
        .lists_containing_note(note_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .into_iter()
        .filter(|hit| {
            store
                .meta_value(&hit.list_path, "hiker.kind")
                .ok()
                .flatten()
                .as_deref()
                == Some(PLAN_KIND)
        })
        .map(|hit| hit.list_path)
        .collect();
    // The backlog board is named by the plan's `backlog: { path }` key
    // rather than its refs — the reverse lookup is a metadata-index query.
    let backlog_query = NoteQuery {
        filters: vec![
            MetaFilter::Equals {
                key: "hiker.kind".to_string(),
                values: vec![PLAN_KIND.to_string()],
            },
            MetaFilter::Equals {
                key: "backlog.path".to_string(),
                values: vec![note_rel.to_string()],
            },
        ],
        ..Default::default()
    };
    plans.extend(
        store
            .query_notes(&backlog_query)
            .map_err(|e| HikerError::Io(e.to_string()))?
            .into_iter()
            .map(|row| row.path),
    );
    plans.sort();
    plans.dedup();
    Ok(plans.into_iter().next())
}

/// The kind a note born inside `note_rel`'s owning plan gets: the plan's
/// `default_kind` policy key resolved against the registry. `None` when
/// the note belongs to no plan, the plan declares no `default_kind`, or
/// the named kind isn't registered — every one of those is pm.md's
/// "born plain" case (`freeform-promote-note`'s no-plan behavior).
///
/// status: plan-kind
/// status: freeform-promote-note
pub fn plan_default_kind<'r>(
    store: &Store,
    registry: &'r Registry,
    note_rel: &str,
) -> Result<Option<&'r Kind>, HikerError> {
    let Some(plan) = owning_plan(store, registry, note_rel)? else {
        return Ok(None);
    };
    let Some(name) = store
        .meta_value(&plan, "default_kind")
        .map_err(|e| HikerError::Io(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(registry.get(&name))
}

/// pm.md's default rollover destination for closing `closing_rel`
/// (`sprint-rollover`): among the owning plan's member sprints (board-like
/// kind, not yet closed, carrying a `start` date), the next one by `start`
/// — strictly after the closing sprint's own `start` when it has one —
/// else the plan's `backlog` board, else `None` (the caller's typed
/// `MissingTarget`). Ties on `start` break to the lexicographically first
/// path.
///
/// status: sprint-rollover
/// status: plan-kind
pub fn default_rollover_destination(
    store: &Store,
    registry: &Registry,
    closing_rel: &str,
) -> Result<Option<String>, HikerError> {
    let Some(plan) = owning_plan(store, registry, closing_rel)? else {
        return Ok(None);
    };
    let closing_start = start_epoch(store, closing_rel);
    let mut next: Option<(f64, String)> = None;
    for member in store
        .members_of(&plan)
        .map_err(|e| HikerError::Io(e.to_string()))?
    {
        let rel = member.member_path;
        if rel == closing_rel || !is_open_sprint(store, registry, &rel) {
            continue;
        }
        let Some(start) = start_epoch(store, &rel) else {
            continue;
        };
        // "Next by start": strictly after the closing sprint's start when
        // it has one; the earliest dated sprint otherwise.
        if closing_start.is_some_and(|c| start <= c) {
            continue;
        }
        let closer = next
            .as_ref()
            .is_none_or(|(s, p)| start < *s || (start == *s && rel < *p));
        if closer {
            next = Some((start, rel));
        }
    }
    if let Some((_, rel)) = next {
        return Ok(Some(rel));
    }
    store
        .meta_value(&plan, "backlog.path")
        .map_err(|e| HikerError::Io(e.to_string()))
}

/// Whether `rel` is a sprint that can still receive rollover: its indexed
/// `hiker.kind` is a registered board-like kind and it carries no
/// `closed_at` stamp (a closed sprint never receives cards).
fn is_open_sprint(store: &Store, registry: &Registry, rel: &str) -> bool {
    let kind = store.meta_value(rel, "hiker.kind").ok().flatten();
    if !kind.is_some_and(|k| registry.board_like(&k).is_some()) {
        return false;
    }
    !matches!(store.meta_value(rel, "closed_at"), Ok(Some(_)))
}

/// A board-doc's `start` kind field off the metadata index, as an epoch
/// for ordering; `None` when absent or unparseable.
fn start_epoch(store: &Store, rel: &str) -> Option<f64> {
    let value = store.meta_value(rel, "start").ok().flatten()?;
    iso_date_epoch(&value)
}

// ---------------------------------------------------------------------------
// Auto-update on note move (the list-refs referrer arm).
// ---------------------------------------------------------------------------

/// Borrow-bundle of the environment handles the list-refs rename sweep
/// reads — the same optional-handle posture as `boards::ops::NoteMovedEnv`
/// (CLI / unit-test paths degrade to write-without-suppress, and without a
/// registry no list-doc parses, so the sweep no-ops).
pub struct ListsMovedEnv<'a> {
    pub watcher: Option<&'a Watcher>,
    pub jobs: Option<&'a IndexJobTx>,
    pub log: Option<&'a LayeredDoc>,
    pub kinds: Option<&'a Registry>,
    pub vault: &'a Vault,
}

/// The list-refs arm of the shared rename-rewrite pass
/// (`pm-epic-derived-table`): when a note moves, re-key the derived
/// `list_refs` rows (member edges AND the list-doc's own key) and rewrite
/// every list-doc whose `hiker.refs` named the old path — enumerated from
/// the derived table, the same load-bearing query the board sweep uses.
/// Errors are logged, never propagated; returns the count of list-docs
/// whose frontmatter was rewritten.
///
/// status: pm-epic-derived-table
pub async fn on_note_moved(
    env: &ListsMovedEnv<'_>,
    store: &mut Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    // All store reads happen before the async fan-out (rusqlite is !Sync).
    let list_docs: Vec<String> = {
        let mut paths: Vec<String> = store
            .lists_containing_note(old_rel)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, path = %old_rel,
                    "list on_note_moved: lists_containing_note failed");
                Vec::new()
            })
            .into_iter()
            .map(|hit| hit.list_path)
            .collect();
        paths.sort();
        paths.dedup();
        paths
    };
    // Case 1: a member note moved — re-key its member edges.
    if let Err(e) = store.rename_list_ref_member_paths(old_rel, new_rel) {
        tracing::warn!(error = %e, "list on_note_moved: rename_list_ref_member_paths failed");
    }
    // Case 2: the list-doc itself moved — re-key its rows' list_path.
    if let Err(e) = store.rename_list_refs_for_list(old_rel, new_rel) {
        tracing::warn!(error = %e, "list on_note_moved: rename_list_refs_for_list failed");
    }

    let mut touched = 0usize;
    for list_rel in list_docs {
        match rewrite_list_ref_path(env, &list_rel, old_rel, new_rel).await {
            Ok(true) => touched += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %list_rel,
                    "list on_note_moved: ref-path rewrite failed");
            }
        }
    }
    Ok(touched)
}

/// Read + parse the list-doc, rewrite every ref whose path is `old_rel` to
/// `new_rel`, and persist — through the layered-doc user-save path when a log
/// is attached, else a suppressed direct write (the `rewrite_card_path`
/// posture). Returns `true` when a rewrite landed.
async fn rewrite_list_ref_path(
    env: &ListsMovedEnv<'_>,
    list_rel: &str,
    old_rel: &str,
    new_rel: &str,
) -> Result<bool, HikerError> {
    let src = env.vault.read_file(list_rel)?;
    let mut doc = parse_list_doc_for(list_rel, &src, env.kinds)
        .map_err(|e| HikerError::Io(format!("parse list-doc: {e}")))?;
    let mut changed = false;
    for path in &mut doc.refs {
        if path == old_rel {
            *path = new_rel.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    let new_src = write_list_doc_frontmatter(&src, &doc)
        .map_err(|e| HikerError::Io(format!("rewrite list-doc: {e}")))?;
    match env.log {
        Some(log) => {
            op_writes::user_save(log, env.vault, list_rel, &new_src)?;
        }
        None => {
            if let Some(w) = env.watcher {
                w.suppress(list_rel.to_string());
            }
            env.vault.write_file(list_rel, &new_src)?;
            if let Some(w) = env.watcher {
                w.suppress(list_rel.to_string());
            }
        }
    }
    if let Some(jobs) = env.jobs {
        let _ = jobs
            .send(IndexJob::Upsert {
                rel_path: list_rel.to_string(),
                force: false,
            })
            .await;
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
