//! Derived-status reads (`derived-status-rule`), the sprint
//! close/rollover batch (`sprint-rollover`), the list-doc parse + epic
//! rollup (`pm-epic-rollup`), plan resolution (`plan-kind`), and the
//! list-refs rename arm (`pm-epic-derived-table`).

use super::*;
use crate::boards::{write_board_frontmatter, Board, Column};
use crate::kinds::builtin_registry;
use crate::ops::op_writes;
use crate::store::dto::{BoardCardRow, MetaEntry, NoteUpsert};
use crate::test_helpers::{test_store, test_vault};

fn note(path: &str) -> BoardCard {
    BoardCard::Note { path: path.into() }
}

fn text(card_id: &str, body: &str) -> BoardCard {
    BoardCard::Text {
        card_id: card_id.into(),
        text: body.into(),
    }
}

fn col(name: &str, cards: Vec<BoardCard>) -> Column {
    Column {
        name: name.into(),
        cards,
        wip_limit: None,
    }
}

fn board(kind: &str, columns: Vec<Column>) -> Board {
    Board {
        kind: kind.into(),
        columns,
    }
}

fn set_board_kind(store: &mut Store, board_path: &str, kind: &str) {
    store
        .replace_note_metadata(
            board_path,
            &[MetaEntry {
                key: "hiker.kind".into(),
                value: kind.into(),
                num: None,
            }],
        )
        .expect("note meta");
}

fn set_cards(store: &mut Store, board_id: &str, board_path: &str, hits: &[(&str, &str)]) {
    let rows: Vec<BoardCardRow> = hits
        .iter()
        .enumerate()
        .map(|(i, (note_rel, column))| BoardCardRow {
            board_id: board_id.into(),
            board_path: board_path.into(),
            card_note_path: (*note_rel).into(),
            column_name: (*column).into(),
            ordinal: i as i64,
        })
        .collect();
    store.replace_board_cards(board_id, &rows).expect("rows");
}

// ---------------------------------------------------------------------------
// Derived status.
// ---------------------------------------------------------------------------

/// Happy path: one sprint, mapped column -> the mapped state + category.
#[test]
fn derived_status_maps_the_one_sprints_column() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_cards(&mut store, "S1", "boards/s1.md", &[("story.md", "Doing")]);

    let status = derived_status(&store, &registry, "story.md").unwrap();
    assert_eq!(
        status,
        DerivedStatus::Active {
            sprint_path: "boards/s1.md".into(),
            column: "Doing".into(),
            state: "Doing".into(),
            category: StateCategory::InProgress,
        }
    );
}

/// No sprint membership at all -> no derived status (rollups count it
/// under `backlog`). Plain-board membership carries zero PM semantics.
#[test]
fn derived_status_none_without_sprint_membership() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    // On a plain board only — boards of kind `board` are views, not sprints.
    set_board_kind(&mut store, "boards/roadmap.md", "board");
    set_cards(&mut store, "R", "boards/roadmap.md", &[("story.md", "Doing")]);

    assert_eq!(
        derived_status(&store, &registry, "story.md").unwrap(),
        DerivedStatus::None
    );
    assert_eq!(
        derived_status(&store, &registry, "never-carded.md").unwrap(),
        DerivedStatus::None
    );
}

/// Hand-edited double membership across two sprints is the loud
/// conflicted read — never a silent pick.
#[test]
fn derived_status_two_sprints_is_conflicted_loud() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_board_kind(&mut store, "boards/s2.md", "sprint");
    set_cards(&mut store, "S1", "boards/s1.md", &[("story.md", "Doing")]);
    set_cards(&mut store, "S2", "boards/s2.md", &[("story.md", "Todo")]);

    assert_eq!(
        derived_status(&store, &registry, "story.md").unwrap(),
        DerivedStatus::Conflicted {
            sprint_paths: vec!["boards/s1.md".into(), "boards/s2.md".into()],
        }
    );
}

/// An unmapped lane ("Icebox") is a plain lane with no PM semantics — the
/// note has no derived status.
#[test]
fn derived_status_unmapped_column_is_none() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_cards(&mut store, "S1", "boards/s1.md", &[("story.md", "Icebox")]);

    assert_eq!(
        derived_status(&store, &registry, "story.md").unwrap(),
        DerivedStatus::None
    );
}

/// The op-level guard: target sprint + a different holding sprint errors
/// naming the holder; same-sprint and plain-board targets pass.
#[test]
fn single_sprint_guard_names_the_holding_sprint() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_cards(&mut store, "S1", "boards/s1.md", &[("story.md", "Todo")]);

    let err = ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "boards/s2.md",
        "sprint",
        "story.md",
    )
    .unwrap_err();
    assert!(err.to_string().contains("boards/s1.md"), "{err}");

    // The holding sprint itself: fine (idempotency owns that case).
    ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "boards/s1.md",
        "sprint",
        "story.md",
    )
    .unwrap();
    // A plain board: unconstrained.
    ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "boards/roadmap.md",
        "board",
        "story.md",
    )
    .unwrap();
    // No registry attached: no PM semantics anywhere.
    ensure_single_sprint_membership(&store, None, "boards/s2.md", "sprint", "story.md")
        .unwrap();
}

/// A denormalized `board_rel` (leading `./`, redundant `.` segment) for the
/// SAME holding sprint must be recognized as that sprint and pass the guard
/// — an idempotent re-add to the board the note already sits on, not a
/// false "already on a different sprint" refusal.
#[test]
fn single_sprint_guard_normalizes_denormalized_target() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_cards(&mut store, "S1", "boards/s1.md", &[("story.md", "Todo")]);

    // Same board, denormalized spelling -> must NOT be treated as a
    // different holding sprint.
    ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "./boards/s1.md",
        "sprint",
        "story.md",
    )
    .unwrap();
    ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "boards/./s1.md",
        "sprint",
        "story.md",
    )
    .unwrap();

    // A genuinely different sprint still errors after normalization.
    let err = ensure_single_sprint_membership(
        &store,
        Some(&registry),
        "./boards/s2.md",
        "sprint",
        "story.md",
    )
    .unwrap_err();
    assert!(err.to_string().contains("boards/s1.md"), "{err}");
}

// ---------------------------------------------------------------------------
// Close / rollover.
// ---------------------------------------------------------------------------

/// `op_writes::stage_auto_content_batch` (the multi-doc sibling of
/// `stage_auto_content`, `sprint-rollover`'s layered-doc substrate): N whole-doc
/// texts share ONE batch id across documents, an unchanged doc stages
/// nothing, and `flip_batch_status` applies the whole set.
#[test]
fn stage_auto_content_batch_spans_documents_under_one_batch() {
    use crate::ops::op_writes::ContentStage;

    let (td, vault) = test_vault();
    vault.write_file("a.md", "alpha v1\n").unwrap();
    vault.write_file("b.md", "bravo v1\n").unwrap();
    vault.write_file("c.md", "charlie v1\n").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();

    let outcome = op_writes::stage_auto_content_batch(
        &log,
        &vault,
        "sprint-close",
        "sprint-close",
        &[
            ContentStage { rel: "a.md".into(), new_text: "alpha v2\n".into() },
            ContentStage { rel: "b.md".into(), new_text: "bravo v2\n".into() },
            // Unchanged text: stages nothing for this doc.
            ContentStage { rel: "c.md".into(), new_text: "charlie v1\n".into() },
        ],
    )
    .unwrap();
    assert_eq!(outcome.op_ids.len(), 2, "unchanged doc stages no op");

    // One batch id spans both documents; nothing reached disk yet.
    let in_batch = log.pending_ops_in_batch(&outcome.batch_id).unwrap();
    let mut docs: Vec<&str> = in_batch.iter().map(|(d, _)| d.as_str()).collect();
    docs.sort_unstable();
    assert_eq!(docs, ["a.md", "b.md"]);
    assert_eq!(vault.read_file("a.md").unwrap(), "alpha v1\n");

    // The ops are authored auto:<producer>.
    let pending = log.all_pending_ops().unwrap();
    assert!(pending
        .iter()
        .all(|(_, op)| op.author.as_wire() == "auto:sprint-close"));

    // Accepting the batch applies every doc's edit.
    let accepted = op_writes::flip_batch_status(&log, &outcome.batch_id, true).unwrap();
    assert_eq!(accepted.len(), 2);
    assert_eq!(vault.read_file("a.md").unwrap(), "alpha v2\n");
    assert_eq!(vault.read_file("b.md").unwrap(), "bravo v2\n");
    assert_eq!(vault.read_file("c.md").unwrap(), "charlie v1\n");
    assert!(log.all_pending_ops().unwrap().is_empty());
}

/// The apply-time one-sprint re-check at the flip seam
/// (`derived-status-rule`): two card-adds of the same note to DIFFERENT
/// sprints both pass stage time (the review-mode hole — the derived state
/// shows no membership when each stages), but accepting the second is
/// refused with the typed `SprintConflict` against the accepted state at
/// that moment, and the refused op stays pending.
#[test]
fn flip_checked_refuses_the_second_sprint_add() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let empty = board("sprint", vec![col("Todo", vec![])]);
    vault
        .write_file("boards/a.md", &write_board_frontmatter("", &empty).unwrap())
        .unwrap();
    vault
        .write_file("boards/b.md", &write_board_frontmatter("", &empty).unwrap())
        .unwrap();
    vault.write_file("story.md", "work\n").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let mut store = Store::open(td.path()).unwrap();
    // Index both boards' kind rows so the flip check's sprint-board
    // enumeration (the `note_meta` hiker.kind query) sees them.
    for rel in ["boards/a.md", "boards/b.md"] {
        seed_note_row(&mut store, rel);
        set_board_kind(&mut store, rel, "sprint");
    }

    let with_story = board("sprint", vec![col("Todo", vec![note("story.md")])]);
    let stage_add = |rel: &str, producer: &str| {
        let text = write_board_frontmatter(
            &vault.read_file(rel).unwrap(),
            &with_story,
        )
        .unwrap();
        op_writes::stage_auto_content_batch(
            &log,
            &vault,
            producer,
            "rules",
            &[crate::ops::op_writes::ContentStage { rel: rel.into(), new_text: text }],
        )
        .unwrap()
    };
    // Both adds stage cleanly — the derived state holds no membership yet.
    let first = stage_add("boards/a.md", "rule:route-a");
    let second = stage_add("boards/b.md", "rule:route-b");

    let ctx = op_writes::FlipCtx { vault: &vault, store: &store, kinds: &registry };
    // Accepting the first add is fine.
    let accepted =
        op_writes::flip_batch_status_checked(&log, &ctx, &first.batch_id, true).unwrap();
    assert_eq!(accepted.len(), 1);
    assert!(vault.read_file("boards/a.md").unwrap().contains("story.md"));

    // Accepting the second is refused: the accepted state now holds the
    // note on sprint a, so the flip would violate the one-sprint rule.
    let err = op_writes::flip_batch_status_checked(&log, &ctx, &second.batch_id, true)
        .unwrap_err();
    assert!(
        matches!(err, HikerError::SprintConflict(_)),
        "typed refusal, got: {err:?}",
    );
    assert!(err.to_string().contains("boards/a.md"), "{err}");
    // The refused op stays pending, and nothing reached disk.
    assert_eq!(log.all_pending_ops().unwrap().len(), 1);
    assert!(!vault.read_file("boards/b.md").unwrap().contains("story.md"));

    // Rejecting the refused add is never blocked.
    let rejected =
        op_writes::flip_batch_status_checked(&log, &ctx, &second.batch_id, false).unwrap();
    assert_eq!(rejected.len(), 1);
    assert!(log.all_pending_ops().unwrap().is_empty());
}

/// A sprint registry whose mapping also covers a canceled-category column,
/// so the keep-filter is exercised for both `done` and `canceled`.
fn sprint_registry_with_dropped_lane() -> Registry {
    let doc: toml::Value = toml::from_str(
        r#"
[kinds.sprint]
shape = "board-like"
states = [
  { name = "Todo",    category = "todo" },
  { name = "Doing",   category = "in_progress" },
  { name = "Done",    category = "done" },
  { name = "Dropped", category = "canceled" },
]
[kinds.sprint.columns]
"Todo"    = "Todo"
"Doing"   = "Doing"
"Done"    = "Done"
"Dropped" = "Dropped"
"#,
    )
    .unwrap();
    let table = doc.get("kinds").and_then(toml::Value::as_table).unwrap();
    let raw: std::collections::BTreeMap<String, toml::Value> =
        table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    Registry::compile(&raw).unwrap()
}

/// Stand up a vault with a closing sprint (done/canceled keep their cards;
/// todo/doing/unmapped lanes + freeform roll) and a destination sprint.
fn rollover_fixture(vault: &Vault) {
    let closing = board(
        "sprint",
        vec![
            col("Todo", vec![note("a.md")]),
            col("Doing", vec![note("b.md"), text("01HFREE", "loose end")]),
            col("Done", vec![note("done.md")]),
            col("Dropped", vec![note("dropped.md")]),
            col("Icebox", vec![note("iced.md")]),
        ],
    );
    let dest = board(
        "sprint",
        vec![col("Done", vec![]), col("Todo", vec![note("existing.md")])],
    );
    vault
        .write_file(
            "boards/s1.md",
            &write_board_frontmatter("", &closing).unwrap(),
        )
        .unwrap();
    vault
        .write_file("boards/s2.md", &write_board_frontmatter("", &dest).unwrap())
        .unwrap();
}

/// The full close: ONE staged batch spanning both board-docs
/// (multi-doc atomicity), category filtering (done/canceled keep, todo /
/// doing / unmapped / freeform roll), append to the destination's first
/// todo-category column, and the `closed_at` stamp — all landing only on
/// batch accept, authored `auto:sprint-close`.
#[test]
fn close_sprint_stages_one_batch_and_accept_applies_both_docs() {
    let (td, vault) = test_vault();
    let registry = sprint_registry_with_dropped_lane();
    rollover_fixture(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let store = Store::open(td.path()).unwrap();

    let outcome = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/s2.md"),
        review_required: true,
    })
    .unwrap();
    // a.md, b.md, the freeform card, and the unmapped-lane iced.md move.
    assert_eq!(outcome.moved, 4);
    assert_eq!(outcome.destination_rel, "boards/s2.md");
    assert_eq!(outcome.destination_column, "Todo");
    assert_eq!(outcome.op_ids.len(), 2, "one op per board-doc");
    assert!(!outcome.applied, "review mode stages; nothing auto-applies");
    // One batch id spans both documents (the op-log-reorg-batch shape).
    let in_batch = log.pending_ops_in_batch(&outcome.batch_id).unwrap();
    let mut docs: Vec<&str> = in_batch.iter().map(|(d, _)| d.as_str()).collect();
    docs.sort_unstable();
    assert_eq!(docs, ["boards/s1.md", "boards/s2.md"]);
    // The review-surface feed carries the shared batch id on both rows (the
    // Patch review tab's grouping key), and each op's batch siblings resolve
    // to the OTHER doc — the per-doc-accept split warning's read.
    let props = op_writes::list_pending_proposals(&log).unwrap();
    assert!(props.iter().all(|p| p.batch_id.as_deref() == Some(outcome.batch_id.as_str())));
    let s1_op = props.iter().find(|p| p.target_path == "boards/s1.md").unwrap();
    assert_eq!(
        op_writes::pending_batch_siblings(&log, &s1_op.op_id).unwrap(),
        ["boards/s2.md"]
    );

    // Staged, not committed: disk is untouched until accept.
    assert!(!vault.read_file("boards/s1.md").unwrap().contains("closed_at"));
    // The frames are authored auto:sprint-close.
    let pending = log.all_pending_ops().unwrap();
    assert!(pending
        .iter()
        .all(|(_, op)| op.author.as_wire() == "auto:sprint-close"));

    let accepted = op_writes::flip_batch_status(&log, &outcome.batch_id, true).unwrap();
    assert_eq!(accepted.len(), 2, "both docs applied on accept");

    let closing_after = vault.read_file("boards/s1.md").unwrap();
    assert!(closing_after.contains("closed_at:"), "{closing_after}");
    for kept in ["done.md", "dropped.md"] {
        assert!(closing_after.contains(kept), "{kept} stays on the closed sprint");
    }
    for moved in ["a.md", "b.md", "iced.md", "loose end"] {
        assert!(!closing_after.contains(moved), "{moved} left the closed sprint");
    }

    let dest_after = vault.read_file("boards/s2.md").unwrap();
    let dest_board = crate::boards::parse_board_for(
        "boards/s2.md",
        &dest_after,
        Some(&registry),
    )
    .unwrap();
    let todo = &dest_board.columns[1];
    assert_eq!(todo.name, "Todo");
    let handles: Vec<&str> = todo
        .cards
        .iter()
        .map(|c| c.path().or(c.card_id()).unwrap())
        .collect();
    assert_eq!(handles, ["existing.md", "a.md", "b.md", "01HFREE", "iced.md"]);
    assert!(dest_board.columns[0].cards.is_empty(), "Done column untouched");
}

/// Non-review mode (`review_required: false`): the close batch is
/// auto-flipped immediately after staging — BOTH board-docs apply before
/// `close_sprint` returns, nothing is left pending, and the rollover can
/// never split across the two docs.
#[test]
fn close_sprint_auto_flip_applies_both_docs_without_review() {
    let (td, vault) = test_vault();
    let registry = sprint_registry_with_dropped_lane();
    rollover_fixture(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let store = Store::open(td.path()).unwrap();

    let outcome = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/s2.md"),
        review_required: false,
    })
    .unwrap();
    assert!(outcome.applied, "auto-accepted, not staged");
    assert!(log.all_pending_ops().unwrap().is_empty(), "nothing left pending");

    // Both docs landed on disk in the one call: the closing board is
    // stamped + drained, the destination gained the rolled-over cards.
    let closing_after = vault.read_file("boards/s1.md").unwrap();
    assert!(closing_after.contains("closed_at:"), "{closing_after}");
    assert!(!closing_after.contains("a.md"), "card left the closed sprint");
    let dest_after = vault.read_file("boards/s2.md").unwrap();
    for moved in ["a.md", "b.md", "iced.md"] {
        assert!(dest_after.contains(moved), "{moved} reached the destination");
    }
}

/// `closed_at` guards double-close: closing an already-closed sprint is a
/// typed error.
#[test]
fn close_sprint_refuses_double_close() {
    let (td, vault) = test_vault();
    let registry = sprint_registry_with_dropped_lane();
    rollover_fixture(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let store = Store::open(td.path()).unwrap();
    let args = CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/s2.md"),
        review_required: true,
    };
    let outcome = close_sprint(&args).unwrap();
    op_writes::flip_batch_status(&log, &outcome.batch_id, true).unwrap();

    let err = close_sprint(&args).unwrap_err();
    assert!(matches!(err, CloseError::AlreadyClosed { .. }), "{err}");
}

/// Missing / unusable destinations are the typed `MissingTarget` error —
/// including the no-default case (`destination_rel: None` with no owning
/// plan) — and a non-sprint closing board is refused outright.
#[test]
fn close_sprint_missing_target_and_not_sprint_errors() {
    let (td, vault) = test_vault();
    let registry = sprint_registry_with_dropped_lane();
    rollover_fixture(&vault);
    vault
        .write_file(
            "boards/plain.md",
            &write_board_frontmatter("", &board("board", vec![col("Todo", vec![])])).unwrap(),
        )
        .unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let store = Store::open(td.path()).unwrap();

    // Destination file doesn't exist.
    let err = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/nope.md"),
        review_required: true,
    })
    .unwrap_err();
    assert!(matches!(err, CloseError::MissingTarget(_)), "{err}");

    // Destination == closing sprint.
    let err = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/s1.md"),
        review_required: true,
    })
    .unwrap_err();
    assert!(matches!(err, CloseError::MissingTarget(_)), "{err}");

    // No pick + no owning plan: the default resolves nothing.
    let err = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: None,
        review_required: true,
    })
    .unwrap_err();
    assert!(matches!(err, CloseError::MissingTarget(_)), "{err}");

    // Closing a plain board is not a sprint close.
    let err = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/plain.md",
        destination_rel: Some("boards/s2.md"),
        review_required: true,
    })
    .unwrap_err();
    assert!(matches!(err, CloseError::NotSprint { .. }), "{err}");

    // Nothing got staged by any refused close.
    assert!(log.all_pending_ops().unwrap().is_empty());
}

/// A plain board can serve as the rollover destination (the backlog-board
/// case): no mapping means the cards land in its FIRST column; rejecting
/// the batch leaves both docs untouched.
#[test]
fn close_sprint_into_plain_board_first_column_and_reject_is_noop() {
    let (td, vault) = test_vault();
    let registry = sprint_registry_with_dropped_lane();
    rollover_fixture(&vault);
    let backlog = board(
        "board",
        vec![col("Inbox", vec![]), col("Someday", vec![])],
    );
    vault
        .write_file(
            "boards/backlog.md",
            &write_board_frontmatter("", &backlog).unwrap(),
        )
        .unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let store = Store::open(td.path()).unwrap();
    let before_closing = vault.read_file("boards/s1.md").unwrap();
    let before_backlog = vault.read_file("boards/backlog.md").unwrap();

    let outcome = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: Some("boards/backlog.md"),
        review_required: true,
    })
    .unwrap();
    assert_eq!(outcome.destination_column, "Inbox", "first column, no mapping");

    op_writes::flip_batch_status(&log, &outcome.batch_id, false).unwrap();
    assert_eq!(vault.read_file("boards/s1.md").unwrap(), before_closing);
    assert_eq!(vault.read_file("boards/backlog.md").unwrap(), before_backlog);
    assert!(log.all_pending_ops().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// List-doc parse (`pm-epic-rollup`).
// ---------------------------------------------------------------------------

const EPIC_DOC: &str = "---\ntitle: search arc\nhiker:\n  kind: epic\n  refs:\n    - { path: a.md }\n    - { path: b.md }\n---\nprose framing\n";

/// The list-doc gate is registry-shape-driven: a registered list-like
/// kind parses (refs in order), an unregistered / non-list kind and a
/// non-`.md` path are refused, and no registry accepts nothing.
#[test]
fn parse_list_doc_gate_is_registry_shape_driven() {
    let registry = builtin_registry();
    let doc = parse_list_doc_for("epics/e1.md", EPIC_DOC, Some(&registry)).unwrap();
    assert_eq!(doc.kind, "epic");
    assert_eq!(doc.refs, ["a.md", "b.md"]);

    let sprintish = EPIC_DOC.replace("kind: epic", "kind: sprint");
    assert!(matches!(
        parse_list_doc("---\nhiker:\n  kind: zettel\n---\n", Some(&registry)),
        Err(ListDocError::KindMismatch { .. })
    ));
    assert!(matches!(
        parse_list_doc(&sprintish, Some(&registry)),
        Err(ListDocError::KindMismatch { .. }),
    ), "board-like kinds are not list-docs");
    assert!(matches!(
        parse_list_doc_for("epics/e1.txt", EPIC_DOC, Some(&registry)),
        Err(ListDocError::NotMarkdown(_))
    ));
    assert!(parse_list_doc(EPIC_DOC, None).is_err(), "no registry, no list-likes");
}

/// `write_list_doc_frontmatter` round-trips: refs replaced wholesale,
/// sibling top-level keys (a plan's policy keys live there) preserved.
#[test]
fn write_list_doc_frontmatter_replaces_refs_and_preserves_siblings() {
    let registry = builtin_registry();
    let mut doc = parse_list_doc(EPIC_DOC, Some(&registry)).unwrap();
    doc.refs = vec!["c.md".to_string()];
    let out = write_list_doc_frontmatter(EPIC_DOC, &doc).unwrap();
    let reparsed = parse_list_doc(&out, Some(&registry)).unwrap();
    assert_eq!(reparsed.refs, ["c.md"]);
    assert!(out.contains("title: search arc"), "sibling key preserved: {out}");
    assert!(out.contains("prose framing"), "body preserved");
    assert!(!out.contains("a.md"), "stale refs replaced wholesale");
}

// ---------------------------------------------------------------------------
// Epic rollup (`pm-epic-rollup`).
// ---------------------------------------------------------------------------

/// Categories anchor the rollup: members on mapped sprint columns count
/// under their column's category with estimate sums; members on no sprint
/// AND members whose ref resolves to nothing count under `backlog`;
/// hand-edited multi-sprint members count under `conflicted`.
#[test]
fn epic_progress_rolls_up_categories_estimates_missing_and_conflicts() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    set_board_kind(&mut store, "boards/s1.md", "sprint");
    set_board_kind(&mut store, "boards/s2.md", "sprint");
    set_cards(
        &mut store,
        "S1",
        "boards/s1.md",
        &[("a.md", "Doing"), ("b.md", "Done"), ("d.md", "Todo")],
    );
    set_cards(&mut store, "S2", "boards/s2.md", &[("d.md", "Doing")]);
    store
        .replace_note_metadata(
            "a.md",
            &[MetaEntry { key: "estimate".into(), value: "2".into(), num: Some(2.0) }],
        )
        .unwrap();
    store
        .replace_note_metadata(
            "b.md",
            &[MetaEntry { key: "estimate".into(), value: "3".into(), num: Some(3.0) }],
        )
        .unwrap();
    let members: Vec<String> =
        ["a.md", "b.md", "c.md", "missing.md", "d.md"].iter().map(|s| (*s).to_string()).collect();
    store.replace_list_refs("epics/e1.md", &members).unwrap();

    let progress = epic_progress(&store, &registry, "epics/e1.md").unwrap();
    assert_eq!(progress.total, 5);
    assert_eq!(progress.in_progress.count, 1);
    assert!((progress.in_progress.estimate - 2.0).abs() < f64::EPSILON);
    assert_eq!(progress.done.count, 1);
    assert!((progress.done.estimate - 3.0).abs() < f64::EPSILON);
    // c.md (no sprint) and missing.md (resolves to nothing) both: backlog.
    assert_eq!(progress.backlog.count, 2);
    assert_eq!(progress.conflicted, 1, "d.md sits on two sprints");
    assert_eq!(progress.summary(), "1/5 done");
}

/// An empty epic rolls up to zeros — no members, no categories, no panic.
#[test]
fn epic_progress_empty_epic_is_all_zeros() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    store.replace_list_refs("epics/empty.md", &[]).unwrap();
    let progress = epic_progress(&store, &registry, "epics/empty.md").unwrap();
    assert_eq!(progress, EpicProgress::default());
    assert_eq!(progress.summary(), "0/0 done");
}

// ---------------------------------------------------------------------------
// Plan resolution (`plan-kind`).
// ---------------------------------------------------------------------------

/// Insert a bare `notes` row so `query_notes` (the backlog reverse
/// lookup's substrate) can see the path.
fn seed_note_row(store: &mut Store, path: &str) {
    store
        .upsert_note(&NoteUpsert {
            path,
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "t",
            chunks: Vec::new(),
        })
        .unwrap();
}

/// Mark `path` as a plan note with the given policy entries.
fn seed_plan(store: &mut Store, path: &str, extra: &[(&str, &str)]) {
    let mut entries = vec![MetaEntry {
        key: "hiker.kind".into(),
        value: "plan".into(),
        num: None,
    }];
    entries.extend(extra.iter().map(|(k, v)| MetaEntry {
        key: (*k).to_string(),
        value: (*v).to_string(),
        num: None,
    }));
    store.replace_note_metadata(path, &entries).unwrap();
    seed_note_row(store, path);
}

/// Membership is plan-owned: a board named by the plan's refs (or its
/// `backlog` key) resolves to that plan; an orphan board resolves to none.
#[test]
fn owning_plan_resolves_refs_and_backlog_membership() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    seed_plan(&mut store, "plans/p.md", &[("backlog.path", "boards/backlog.md")]);
    store
        .replace_list_refs("plans/p.md", &["boards/s1.md".to_string(), "epics/e1.md".to_string()])
        .unwrap();
    // A non-plan list naming the same board never claims it.
    store
        .replace_list_refs("epics/e1.md", &["boards/s1.md".to_string()])
        .unwrap();

    assert_eq!(
        owning_plan(&store, &registry, "boards/s1.md").unwrap().as_deref(),
        Some("plans/p.md"),
        "named by the plan's refs"
    );
    assert_eq!(
        owning_plan(&store, &registry, "boards/backlog.md").unwrap().as_deref(),
        Some("plans/p.md"),
        "named by the plan's backlog key"
    );
    assert_eq!(
        owning_plan(&store, &registry, "boards/orphan.md").unwrap(),
        None,
        "an orphan board belongs to no plan"
    );
}

/// `plan_default_kind` resolves the owning plan's `default_kind` policy
/// key against the registry — the promote-template source
/// (`freeform-promote-note`); no plan / no key / unregistered kind all
/// mean "born plain".
#[test]
fn plan_default_kind_resolves_the_promote_template() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    seed_plan(&mut store, "plans/p.md", &[("default_kind", "story")]);
    store
        .replace_list_refs("plans/p.md", &["boards/s1.md".to_string()])
        .unwrap();

    let kind = plan_default_kind(&store, &registry, "boards/s1.md")
        .unwrap()
        .expect("the plan declares default_kind: story");
    assert_eq!(kind.name, "story");
    assert!(
        plan_default_kind(&store, &registry, "boards/orphan.md").unwrap().is_none(),
        "no plan: born plain"
    );
}

/// The rollover default (`sprint-rollover` x `plan-kind`): the plan's next
/// sprint by `start` (strictly after the closing sprint's start; closed
/// sprints skipped), else the plan's backlog board, else nothing.
#[test]
fn default_rollover_destination_next_by_start_then_backlog() {
    let (_td, mut store) = test_store();
    let registry = builtin_registry();
    seed_plan(&mut store, "plans/p.md", &[("backlog.path", "boards/backlog.md")]);
    let sprints = ["boards/s1.md", "boards/s2.md", "boards/s3.md", "boards/closed.md"];
    store
        .replace_list_refs("plans/p.md", &sprints.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
        .unwrap();
    let mut seed_sprint = |path: &str, start: &str, closed: bool| {
        let mut entries = vec![
            MetaEntry { key: "hiker.kind".into(), value: "sprint".into(), num: None },
            MetaEntry { key: "start".into(), value: start.into(), num: None },
        ];
        if closed {
            entries.push(MetaEntry {
                key: "closed_at".into(),
                value: "2026-06-01T00:00:00Z".into(),
                num: None,
            });
        }
        store.replace_note_metadata(path, &entries).unwrap();
    };
    seed_sprint("boards/s1.md", "2026-06-01", false);
    seed_sprint("boards/closed.md", "2026-06-08", true);
    seed_sprint("boards/s2.md", "2026-06-15", false);
    seed_sprint("boards/s3.md", "2026-07-01", false);

    // Next by start after s1: the closed 06-08 sprint is skipped, s2 wins.
    assert_eq!(
        default_rollover_destination(&store, &registry, "boards/s1.md").unwrap().as_deref(),
        Some("boards/s2.md"),
    );
    // The last dated sprint has no successor: the plan's backlog board.
    assert_eq!(
        default_rollover_destination(&store, &registry, "boards/s3.md").unwrap().as_deref(),
        Some("boards/backlog.md"),
    );
    // Outside any plan: nothing — the caller's typed MissingTarget.
    assert_eq!(
        default_rollover_destination(&store, &registry, "boards/orphan.md").unwrap(),
        None,
    );
}

/// End-to-end default destination through `close_sprint(destination_rel:
/// None)`: the plan's next sprint by start receives the rollover.
#[test]
fn close_sprint_resolves_the_plan_default_destination() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    rollover_fixture(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    let mut store = Store::open(td.path()).unwrap();
    seed_plan(&mut store, "plans/p.md", &[]);
    store
        .replace_list_refs(
            "plans/p.md",
            &["boards/s1.md".to_string(), "boards/s2.md".to_string()],
        )
        .unwrap();
    store
        .replace_note_metadata(
            "boards/s1.md",
            &[
                MetaEntry { key: "hiker.kind".into(), value: "sprint".into(), num: None },
                MetaEntry { key: "start".into(), value: "2026-06-01".into(), num: None },
            ],
        )
        .unwrap();
    store
        .replace_note_metadata(
            "boards/s2.md",
            &[
                MetaEntry { key: "hiker.kind".into(), value: "sprint".into(), num: None },
                MetaEntry { key: "start".into(), value: "2026-06-15".into(), num: None },
            ],
        )
        .unwrap();

    let outcome = close_sprint(&CloseSprint {
        log: &log,
        vault: &vault,
        store: &store,
        registry: &registry,
        closing_rel: "boards/s1.md",
        destination_rel: None,
        review_required: true,
    })
    .unwrap();
    assert_eq!(outcome.destination_rel, "boards/s2.md", "next sprint by start");
    assert!(outcome.moved > 0);
}

// ---------------------------------------------------------------------------
// List-refs rename arm (`pm-epic-derived-table`).
// ---------------------------------------------------------------------------

/// Moving a member note rewrites every list-doc's `hiker.refs[].path`
/// (enumerated off the derived table) and re-keys the derived rows; a
/// list-doc that never referenced the path is untouched.
#[tokio::test]
async fn lists_on_note_moved_rewrites_refs_and_rekeys_rows() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    vault.write_file("epics/e1.md", EPIC_DOC).unwrap();
    let other = EPIC_DOC.replace("a.md", "z.md");
    vault.write_file("epics/e2.md", &other).unwrap();
    let mut store = Store::open(td.path()).unwrap();
    store
        .replace_list_refs("epics/e1.md", &["a.md".to_string(), "b.md".to_string()])
        .unwrap();
    store
        .replace_list_refs("epics/e2.md", &["z.md".to_string(), "b.md".to_string()])
        .unwrap();

    let env = ListsMovedEnv {
        watcher: None,
        jobs: None,
        log: None,
        kinds: Some(&registry),
        vault: &vault,
    };
    let touched = on_note_moved(&env, &mut store, "a.md", "moved/a.md").await.unwrap();
    assert_eq!(touched, 1, "only the referencing list-doc rewrites");

    let after = vault.read_file("epics/e1.md").unwrap();
    let doc = parse_list_doc(&after, Some(&registry)).unwrap();
    assert_eq!(doc.refs, ["moved/a.md", "b.md"], "ref rewritten in place");
    assert_eq!(vault.read_file("epics/e2.md").unwrap(), other, "non-referrer untouched");

    // Derived rows re-keyed ahead of the next ingest.
    let hits = store.lists_containing_note("moved/a.md").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].list_path, "epics/e1.md");
    assert!(store.lists_containing_note("a.md").unwrap().is_empty());

    // The list-doc itself moving re-keys its rows' list_path.
    let touched = on_note_moved(&env, &mut store, "epics/e1.md", "epics/renamed.md")
        .await
        .unwrap();
    assert_eq!(touched, 0, "no list references the list-doc itself");
    let members = store.members_of("epics/renamed.md").unwrap();
    assert_eq!(members.len(), 2, "rows follow the moved list-doc");
}
