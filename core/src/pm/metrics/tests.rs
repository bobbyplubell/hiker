//! Metrics replay over a synthetic snapshot history (`pm-snapshot-metrics`):
//! a board-doc's saved snapshots drive burnup, a moved card's cycle time,
//! the estimate join off `note_meta`, and the velocity tally at a closed
//! sprint's close frame. Note: a bootstrap-adopted file is not re-snapshotted,
//! so each board state must reach disk through a save to appear in the replay.

use super::*;
use crate::boards::{write_board_frontmatter, Board, BoardCard, Column};
use crate::kinds::builtin_registry;
use crate::store::dto::MetaEntry;
use crate::test_helpers::{test_store, test_vault};
use crate::vault::Vault;

fn board(columns: &[(&str, &[&str])]) -> Board {
    Board {
        kind: "sprint".into(),
        columns: columns
            .iter()
            .map(|(name, cards)| Column {
                name: (*name).to_string(),
                cards: cards
                    .iter()
                    .map(|p| BoardCard::Note { path: (*p).to_string() })
                    .collect(),
                wip_limit: None,
            })
            .collect(),
    }
}

fn save_frame(log: &LayeredDoc, vault: &Vault, rel: &str, b: &Board) {
    let src = vault.read_file(rel).unwrap_or_default();
    let text = write_board_frontmatter(&src, b).unwrap();
    op_writes::user_save(log, vault, rel, &text).unwrap();
}

fn set_meta(store: &mut Store, path: &str, entries: &[(&str, &str)]) {
    let rows: Vec<MetaEntry> = entries
        .iter()
        .map(|(k, v)| MetaEntry {
            key: (*k).to_string(),
            value: (*v).to_string(),
            num: None,
        })
        .collect();
    store.replace_note_metadata(path, &rows).unwrap();
}

/// A synthetic three-frame history — `a.md` walks Todo -> Doing -> Done
/// while `b.md` appears in Todo — yields the burnup tally at the last
/// frame (1/2 done), the estimate join re-weighting it (a.md carries
/// `estimate: 3`), and a cycle-time row for the moved card. No writes
/// beyond the synthetic saves themselves.
#[test]
fn replay_builds_burnup_estimate_join_and_cycle_time() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let rel = "boards/s1.md";
    vault
        .write_file(rel, &write_board_frontmatter("", &board(&[("Todo", &["a.md"])])).unwrap())
        .unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    save_frame(&log, &vault, rel, &board(&[("Todo", &[]), ("Doing", &["a.md"])]));
    save_frame(
        &log,
        &vault,
        rel,
        &board(&[("Todo", &["b.md"]), ("Doing", &[]), ("Done", &["a.md"])]),
    );

    let (_meta_td, mut store) = test_store();
    set_meta(&mut store, "a.md", &[("estimate", "3")]);

    let ctx = Ctx { log: &log, store: &store, registry: &registry };
    let metrics = sprint_tables(&ctx, rel).unwrap();

    assert!(metrics.newest_op_id.is_some(), "newest snapshot id populated");

    // All frames land within one day, so burnup is a single row showing
    // the final tally: a.md done, b.md still in Todo.
    assert_eq!(metrics.burnup.len(), 1, "{:?}", metrics.burnup);
    let row = &metrics.burnup[0];
    assert_eq!((row.done_count, row.total_count), (1, 2));
    // The estimate join: a.md's `estimate: 3` weights both series; b.md
    // has no estimate and contributes 0.
    assert!((row.done_estimate - 3.0).abs() < f64::EPSILON);
    assert!((row.total_estimate - 3.0).abs() < f64::EPSILON);

    // The moved card's cycle time: first Doing entry -> first Done entry.
    assert_eq!(metrics.cycle.len(), 1);
    let cycle = &metrics.cycle[0];
    assert_eq!(cycle.handle, "a.md");
    assert!(cycle.done_ms >= cycle.started_ms);

    // Not closed, no plan: no velocity rows.
    assert!(metrics.velocity.is_empty());
}

/// A reopened card records a SECOND cycle. `a.md` walks
/// Todo -> Doing -> Done -> Doing (reopen) -> Done, so `cycle_times` must
/// clear the open `started` mark when the first cycle completes and start a
/// fresh one on the reopen — yielding two cycle rows, not one.
#[test]
fn reopened_card_records_two_cycles() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let rel = "boards/s1.md";
    vault
        .write_file(rel, &write_board_frontmatter("", &board(&[("Todo", &["a.md"])])).unwrap())
        .unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    // Cycle 1: Doing then Done.
    save_frame(&log, &vault, rel, &board(&[("Doing", &["a.md"]), ("Done", &[])]));
    save_frame(&log, &vault, rel, &board(&[("Doing", &[]), ("Done", &["a.md"])]));
    // Reopen -> cycle 2: back to Doing, then Done again.
    save_frame(&log, &vault, rel, &board(&[("Doing", &["a.md"]), ("Done", &[])]));
    save_frame(&log, &vault, rel, &board(&[("Doing", &[]), ("Done", &["a.md"])]));

    let (_meta_td, store) = test_store();
    let ctx = Ctx { log: &log, store: &store, registry: &registry };
    let metrics = sprint_tables(&ctx, rel).unwrap();

    assert_eq!(metrics.cycle.len(), 2, "reopen must record a second cycle: {:?}", metrics.cycle);
    assert!(metrics.cycle.iter().all(|c| c.handle == "a.md"));
    assert!(metrics.cycle.iter().all(|c| c.done_ms >= c.started_ms));
}

/// Skipped frames are counted, not silently dropped: an unparseable frame
/// (the file's pre-board prose) increments `skipped_unparseable`, the
/// truncation total goes nonzero, and the burnup series starts at the
/// first RETAINED frame instead of zero-filling from a `start` date the
/// replay can't actually see (which would emit a thousand confident-zero
/// rows here).
#[test]
fn replay_counts_skipped_frames_and_burnup_starts_at_first_retained() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let rel = "boards/s1.md";
    // Seed empty, then SAVE a prose frame (snapshotted, unparseable as a
    // board) followed by a board frame. The first save's snapshot must be
    // skipped AND counted by the replay.
    vault.write_file(rel, "").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    op_writes::user_save(&log, &vault, rel, "plain prose, no frontmatter\n").unwrap();
    save_frame(&log, &vault, rel, &board(&[("Todo", &["a.md"]), ("Done", &[])]));

    let (_meta_td, mut store) = test_store();
    // A `start` far before the first retained frame: without the
    // truncation clamp this would zero-fill ~1000 pre-replay days.
    set_meta(&mut store, rel, &[("start", "2000-01-01")]);

    let ctx = Ctx { log: &log, store: &store, registry: &registry };
    let metrics = sprint_tables(&ctx, rel).unwrap();

    assert_eq!(metrics.skipped_unparseable, 1, "the prose frame is counted");
    assert_eq!(metrics.skipped_unretained, 0);
    assert_eq!(metrics.skipped_frames(), 1, "truncation marker input is nonzero");
    // The series starts at the first retained frame's day, not the stale
    // `start` date — one row, today.
    assert_eq!(metrics.burnup.len(), 1, "{:?}", metrics.burnup);
    assert_eq!((metrics.burnup[0].done_count, metrics.burnup[0].total_count), (0, 1));
}

/// A clean replay (every frame retained and parseable) reports zero
/// skipped frames — the marker stays off.
#[test]
fn clean_replay_reports_no_skipped_frames() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let rel = "boards/s1.md";
    vault
        .write_file(rel, &write_board_frontmatter("", &board(&[("Todo", &["a.md"])])).unwrap())
        .unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();

    let (_meta_td, store) = test_store();
    let ctx = Ctx { log: &log, store: &store, registry: &registry };
    let metrics = sprint_tables(&ctx, rel).unwrap();
    assert_eq!(metrics.skipped_frames(), 0);
}

/// Velocity tallies done-category cards at the close frame of each closed
/// sprint — across the owning plan's sprints when the board belongs to a
/// plan, skipping the plan's open sprints.
#[test]
fn velocity_spans_the_plans_closed_sprints() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    // Seed empty, then SAVE each board state so the save writes a plain-file
    // snapshot (the replay source). A bootstrap-adopted file is not
    // re-snapshotted, so the board state must reach disk through a save.
    vault.write_file("boards/s1.md", "").unwrap();
    vault.write_file("boards/s2.md", "").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    op_writes::bootstrap(&vault, &log).unwrap();
    save_frame(&log, &vault, "boards/s1.md", &board(&[("Done", &["a.md", "b.md"])]));
    save_frame(&log, &vault, "boards/s2.md", &board(&[("Todo", &["c.md"])]));

    let (_meta_td, mut store) = test_store();
    set_meta(&mut store, "a.md", &[("estimate", "2")]);
    set_meta(&mut store, "b.md", &[("estimate", "1.5")]);
    // s1 closed in the past -> its first (and only) frame is at/after the
    // stamp's fallback resolution; s2 stays open.
    set_meta(
        &mut store,
        "boards/s1.md",
        &[("hiker.kind", "sprint"), ("closed_at", "2000-01-02T00:00:00Z")],
    );
    set_meta(&mut store, "boards/s2.md", &[("hiker.kind", "sprint")]);
    set_meta(&mut store, "plans/p.md", &[("hiker.kind", "plan")]);
    store
        .replace_list_refs(
            "plans/p.md",
            &["boards/s1.md".to_string(), "boards/s2.md".to_string()],
        )
        .unwrap();

    let ctx = Ctx { log: &log, store: &store, registry: &registry };
    // Asking from the OPEN sprint still reports across the plan.
    let metrics = sprint_tables(&ctx, "boards/s2.md").unwrap();
    assert_eq!(metrics.velocity.len(), 1, "{:?}", metrics.velocity);
    let row = &metrics.velocity[0];
    assert_eq!(row.sprint_rel, "boards/s1.md");
    assert_eq!(row.done_count, 2);
    assert!((row.done_estimate - 3.5).abs() < f64::EPSILON);
}
