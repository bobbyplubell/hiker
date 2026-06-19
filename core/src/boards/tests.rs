//! Pure board-frontmatter parse / write / contains-note tests under
//! path-as-identity (`board-card-references`), the registry-aware parse
//! gate (`sprint-board-subtype`), the one-sprint card-add guard
//! (`derived-status-rule`), and the freeform-card promotion op
//! (`freeform-promote-note`).

use super::*;

use crate::kinds::builtin_registry;
use crate::store::dto::{BoardCardRow, MetaEntry, NoteUpsert};
use crate::test_helpers::{test_indexer, test_vault};

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

fn sample_board() -> Board {
    Board {
        kind: "board".into(),
        columns: vec![
            col(
                "Todo",
                vec![
                    note("research/raptor-paper.md"),
                    note("inbox/follow-up.md"),
                ],
            ),
            col("Doing", vec![note("work/migration.md")]),
            col("Done", vec![]),
        ],
    }
}

/// Register a bare `notes` row so `Store::all_note_paths` (the `list`
/// walk) sees the path.
fn index_note_row(store: &mut Store, rel: &str) {
    store
        .upsert_note(&NoteUpsert {
            path: rel,
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "zero-test",
            chunks: Vec::new(),
        })
        .expect("note row");
}

/// Record `board_path`'s indexed `hiker.kind` (the `note_meta` half of the
/// derived-status join).
fn index_board_kind(store: &mut Store, board_path: &str, kind: &str) {
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

/// Record one derived `board_cards` row: `note_rel` sits in `column` on
/// the board at `board_path` (id `board_id`).
fn index_card(store: &mut Store, board_id: &str, board_path: &str, note_rel: &str, column: &str) {
    store
        .replace_board_cards(
            board_id,
            &[BoardCardRow {
                board_id: board_id.into(),
                board_path: board_path.into(),
                card_note_path: note_rel.into(),
                column_name: column.into(),
                ordinal: 0,
            }],
        )
        .expect("board_cards row");
}

#[test]
fn board_build_parse_round_trip() {
    let board = sample_board();
    let body = write_board_frontmatter("", &board).unwrap();
    let parsed = parse_board_for("boards/q3.md", &body, None).unwrap();
    assert_eq!(parsed, board);
}

#[test]
fn parse_rejects_non_board_kind() {
    let src = "---\nhiker:\n  kind: trail\n---\n";
    assert!(parse_board(src, None).is_err());
}

#[test]
fn parse_for_rejects_non_md_extension() {
    let src = "---\nhiker:\n  kind: board\n---\n";
    let err = parse_board_for("boards/q3.txt", src, None).unwrap_err();
    assert!(matches!(err, Error::NotMarkdown(_)));
    assert!(parse_board_for("boards/q3.md", src, None).is_ok());
}

/// The parse gate's acceptance set is `{ "board" } ∪ registry board-like
/// kinds (`sprint-board-subtype`): a sprint board-doc parses with the
/// registry attached, is rejected without one, and unknown / non-board-like
/// kinds stay rejected either way. The `.md` rule applies to sprints too.
#[test]
fn sprint_kind_accepted_by_registry_gate() {
    let registry = builtin_registry();
    let sprint = "---\nhiker:\n  kind: sprint\n  columns:\n    - name: Todo\n      cards: []\n---\n";
    let parsed = parse_board_for("boards/s1.md", sprint, Some(&registry)).unwrap();
    assert_eq!(parsed.kind, "sprint");
    assert!(matches!(
        parse_board_for("boards/s1.md", sprint, None),
        Err(Error::KindMismatch { .. })
    ));
    assert!(matches!(
        parse_board_for("boards/s1.txt", sprint, Some(&registry)),
        Err(Error::NotMarkdown(_))
    ));
    // Unregistered kind: rejected even with the registry attached.
    let unknown = "---\nhiker:\n  kind: zettel\n---\n";
    assert!(matches!(
        parse_board_for("boards/z.md", unknown, Some(&registry)),
        Err(Error::KindMismatch { .. })
    ));
    // Registered but NOT board-like (leaf `story`): rejected — acceptance
    // is shape-driven.
    let leaf = "---\nhiker:\n  kind: story\n---\n";
    assert!(matches!(
        parse_board_for("notes/s.md", leaf, Some(&registry)),
        Err(Error::KindMismatch { .. })
    ));
}

/// A board mutation write-back round-trips the board's kind — a card move
/// on a sprint must never retype the doc to `board`.
/// status: sprint-board-subtype
#[test]
fn write_round_trips_sprint_kind() {
    let registry = builtin_registry();
    let board = Board {
        kind: "sprint".into(),
        columns: vec![col("Todo", vec![note("a.md")])],
    };
    let s = write_board_frontmatter("", &board).unwrap();
    assert!(s.contains("kind: sprint"), "kind written back: {s}");
    let parsed = parse_board_for("boards/s1.md", &s, Some(&registry)).unwrap();
    assert_eq!(parsed, board);
}

#[test]
fn write_preserves_unknown_hiker_siblings_and_top_level() {
    let src = "---\ntitle: Q3 plan\nhiker:\n  kind: board\n  author: user-authored\n  columns: []\ntags: [planning]\n---\nbody\n";
    let parsed = parse_board(src, None).unwrap();
    let written = write_board_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("title: Q3 plan"));
    assert!(written.contains("author: user-authored"));
    assert!(written.contains("tags:"));
}

#[test]
fn wip_limit_round_trips_and_omits_when_none() {
    let mut board = Board {
        kind: "board".into(),
        columns: vec![Column {
            name: "Doing".into(),
            cards: vec![],
            wip_limit: Some(3),
        }],
    };
    let s = write_board_frontmatter("", &board).unwrap();
    assert!(s.contains("wip_limit: 3"));
    let parsed = parse_board(&s, None).unwrap();
    assert_eq!(parsed.columns[0].wip_limit, Some(3));
    // Clear and re-write: the key should be gone.
    board.columns[0].wip_limit = None;
    let s2 = write_board_frontmatter("", &board).unwrap();
    assert!(!s2.contains("wip_limit"));
}

// status: board-add-card / board-card-references
#[test]
fn contains_note_matches_by_path() {
    let board = sample_board();
    assert!(board.contains_note("research/raptor-paper.md"));
    assert!(board.contains_note("inbox/follow-up.md"));
    assert!(!board.contains_note("unrelated.md"));
}

// status: board-freeform-card / board-card-references
#[test]
fn mixed_note_and_text_cards_round_trip() {
    let board = Board {
        kind: "board".into(),
        columns: vec![col(
            "Doing",
            vec![
                note("work/migration.md"),
                text("01HTEXT", "quick reminder"),
            ],
        )],
    };
    let s = write_board_frontmatter("", &board).unwrap();
    let parsed = parse_board(&s, None).unwrap();
    assert_eq!(parsed, board);
    // Note card serializes as `{ path }` only (no id half).
    assert!(s.contains("path: work/migration.md"));
    // Freeform card serializes with `card_id`. Be lenient about
    // whitespace / surrounding quotes since the YAML serializer chooses
    // its own layout — round-trip equality is the real contract above.
    assert!(s.contains("card_id"));
    assert!(s.contains("01HTEXT"));
    assert!(s.contains("quick reminder"));
}

// status: board-create
#[test]
fn plan_new_board_picks_free_path_without_writing() {
    use crate::config::sections::BoardsConfig;
    let td = tempfile::tempdir().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let cfg = BoardsConfig {
        new_board_dir: "boards".into(),
    };
    let plan = ops::plan_new_board(&vault, &cfg, "q3", None).unwrap();
    assert_eq!(plan.board_doc_rel, "boards/q3.md");
    assert!(plan.body.contains("hiker:"));
    assert!(plan.body.contains("kind: board"));
    // Plan is read-only: the file shouldn't exist yet.
    assert!(!td.path().join(plan.board_doc_rel).exists());
}

/// Creating a board of a board-like kind seeds its columns from the kind's
/// column mapping (state-declaration order) and stamps the kind — a fresh
/// sprint is born meaning something. status: sprint-board-subtype
#[test]
fn plan_new_board_with_sprint_kind_seeds_mapped_columns() {
    use crate::config::sections::BoardsConfig;
    let td = tempfile::tempdir().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let registry = builtin_registry();
    let sprint = registry.get("sprint").expect("builtin sprint");
    let cfg = BoardsConfig {
        new_board_dir: "boards".into(),
    };
    let plan = ops::plan_new_board(&vault, &cfg, "sprint-12", Some(sprint)).unwrap();
    assert!(plan.body.contains("kind: sprint"));
    let parsed =
        parse_board_for(&plan.board_doc_rel, &plan.body, Some(&registry)).unwrap();
    let names: Vec<&str> = parsed.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Todo", "Doing", "Review", "Done"]);
}

/// `boards::list` (one of the three registry-aware gate callers) picks up
/// sprint board-docs alongside plain boards once the registry is threaded;
/// without it, sprints stay invisible. status: sprint-board-subtype
#[test]
fn list_picks_up_sprint_boards_with_registry() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let plain = write_board_frontmatter("", &sample_board()).unwrap();
    let sprint = write_board_frontmatter(
        "",
        &Board {
            kind: "sprint".into(),
            columns: vec![col("Todo", vec![note("a.md")])],
        },
    )
    .unwrap();
    vault.write_file("boards/plain.md", &plain).unwrap();
    vault.write_file("boards/s1.md", &sprint).unwrap();
    vault.write_file("a.md", "a body\n").unwrap();

    let mut store = Store::open(td.path()).unwrap();
    for rel in ["boards/plain.md", "boards/s1.md", "a.md"] {
        index_note_row(&mut store, rel);
    }
    let log = LayeredDoc::open(td.path()).unwrap();
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();

    let with_registry = list(&vault, &store, &log, Some(&registry)).unwrap();
    let mut rels: Vec<&str> = with_registry.iter().map(|b| b.rel_path.as_str()).collect();
    rels.sort_unstable();
    assert_eq!(rels, ["boards/plain.md", "boards/s1.md"]);

    let without = list(&vault, &store, &log, None).unwrap();
    assert_eq!(without.len(), 1, "no registry -> plain boards only");
    assert_eq!(without[0].rel_path, "boards/plain.md");
}

/// `get_board` populates the PM strip on sprint-kind boards only
/// (`pm-story-kind` / `derived-status-rule`): estimate read off the index,
/// `due` only when near or overdue, and the loud conflicted flag for a
/// hand-edited double sprint membership. Plain boards carry no strip.
#[test]
fn get_board_populates_pm_strip_on_sprint_boards() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let sprint = Board {
        kind: "sprint".into(),
        columns: vec![col(
            "Doing",
            vec![note("a.md"), note("b.md"), note("doubled.md")],
        )],
    };
    vault
        .write_file("boards/s1.md", &write_board_frontmatter("", &sprint).unwrap())
        .unwrap();
    let plain = Board {
        kind: "board".into(),
        columns: vec![col("Doing", vec![note("a.md")])],
    };
    vault
        .write_file(
            "boards/plain.md",
            &write_board_frontmatter("", &plain).unwrap(),
        )
        .unwrap();
    for rel in ["a.md", "b.md", "doubled.md"] {
        vault.write_file(rel, "body\n").unwrap();
    }

    let mut store = Store::open(td.path()).unwrap();
    index_board_kind(&mut store, "boards/s1.md", "sprint");
    index_board_kind(&mut store, "boards/s2.md", "sprint");
    // a.md: estimate + an overdue due (shown); b.md: a far-future due (hidden).
    store
        .replace_note_metadata(
            "a.md",
            &[
                MetaEntry { key: "estimate".into(), value: "3".into(), num: Some(3.0) },
                MetaEntry { key: "due".into(), value: "2020-01-01".into(), num: None },
            ],
        )
        .unwrap();
    store
        .replace_note_metadata(
            "b.md",
            &[MetaEntry { key: "due".into(), value: "2999-01-01".into(), num: None }],
        )
        .unwrap();
    // doubled.md sits on two sprints in the derived table -> conflicted.
    index_card(&mut store, "S1", "boards/s1.md", "doubled.md", "Doing");
    index_card(&mut store, "S2", "boards/s2.md", "doubled.md", "Todo");

    let log = LayeredDoc::open(td.path()).unwrap();
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();

    let detail =
        get_board(&vault, &store, &log, "boards/s1.md", Some(&registry)).unwrap();
    assert_eq!(detail.kind, "sprint");
    let cards = &detail.columns[0].cards;
    let a = cards[0].pm.as_ref().expect("note cards on sprints carry pm");
    assert_eq!(a.estimate.as_deref(), Some("3"));
    assert_eq!(a.due.as_deref(), Some("2020-01-01"), "overdue due surfaces");
    assert!(!a.conflicted);
    let b = cards[1].pm.as_ref().unwrap();
    assert_eq!(b.due, None, "far-future due stays off the strip");
    let doubled = cards[2].pm.as_ref().unwrap();
    assert!(doubled.conflicted, "double sprint membership is loud");

    let plain_detail =
        get_board(&vault, &store, &log, "boards/plain.md", Some(&registry)).unwrap();
    assert_eq!(plain_detail.kind, "board");
    assert!(plain_detail.columns[0].cards[0].pm.is_none(), "plain boards: no strip");
}

/// The MCP card-add path (`add_card_preview`) enforces the one-sprint rule
/// (`derived-status-rule`): adding to a second sprint errors naming the
/// holding sprint; plain boards stay unconstrained.
#[test]
fn add_card_preview_enforces_one_sprint_membership() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let sprint_empty = write_board_frontmatter(
        "",
        &Board {
            kind: "sprint".into(),
            columns: vec![col("Todo", vec![])],
        },
    )
    .unwrap();
    vault.write_file("boards/sprint-a.md", &sprint_empty).unwrap();
    vault.write_file("boards/sprint-b.md", &sprint_empty).unwrap();
    let plain = write_board_frontmatter(
        "",
        &Board {
            kind: "board".into(),
            columns: vec![col("Todo", vec![])],
        },
    )
    .unwrap();
    vault.write_file("boards/plain.md", &plain).unwrap();
    vault.write_file("story.md", "the story\n").unwrap();

    let mut store = Store::open(td.path()).unwrap();
    index_board_kind(&mut store, "boards/sprint-a.md", "sprint");
    index_board_kind(&mut store, "boards/sprint-b.md", "sprint");
    index_board_kind(&mut store, "boards/plain.md", "board");
    index_card(&mut store, "A", "boards/sprint-a.md", "story.md", "Todo");

    // A second sprint refuses, naming the holding sprint.
    let err = add_card_preview(
        &vault,
        &store,
        Some(&registry),
        "boards/sprint-b.md",
        "Todo",
        "story.md",
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("boards/sprint-a.md"),
        "error names the holding sprint: {err}"
    );

    // A plain board stays unconstrained (`board-many-to-many`).
    let ok = add_card_preview(
        &vault,
        &store,
        Some(&registry),
        "boards/plain.md",
        "Todo",
        "story.md",
    )
    .unwrap();
    assert!(ok.is_some(), "plain-board membership unconstrained");
}

/// The direct card-add op (`ops::add_card`) refuses the second sprint too —
/// the other card-add path pm.md names. status: derived-status-rule
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_card_op_refuses_second_sprint() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let sprint_empty = write_board_frontmatter(
        "",
        &Board {
            kind: "sprint".into(),
            columns: vec![col("Todo", vec![])],
        },
    )
    .unwrap();
    vault.write_file("boards/sprint-a.md", &sprint_empty).unwrap();
    vault.write_file("boards/sprint-b.md", &sprint_empty).unwrap();
    vault.write_file("story.md", "the story\n").unwrap();

    let mut store = Store::open(td.path()).unwrap();
    index_board_kind(&mut store, "boards/sprint-a.md", "sprint");
    index_board_kind(&mut store, "boards/sprint-b.md", "sprint");
    index_card(&mut store, "A", "boards/sprint-a.md", "story.md", "Todo");

    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let idx = test_indexer(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();

    let err = ops::add_card(ops::AddCardArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        vault: &vault,
        log: &log,
        store: &store,
        kinds: Some(&registry),
        board_doc_rel: "boards/sprint-b.md",
        column_name: "Todo",
        source_rel: "story.md",
    })
    .await
    .unwrap_err();
    assert!(err.to_string().contains("boards/sprint-a.md"));
    // The refused add left the destination board untouched.
    let after = vault.read_file("boards/sprint-b.md").unwrap();
    assert!(!after.contains("story.md"));

    idx.shutdown().await;
}

/// `promote_text_card` creates the note from the card text (first line
/// slugified, body = full text, landing in the board-doc's directory) and
/// swaps the card in place — same column, same position.
/// status: freeform-promote-note
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_text_card_swaps_in_place() {
    let (td, vault) = test_vault();
    let board = Board {
        kind: "board".into(),
        columns: vec![col(
            "Todo",
            vec![
                note("a.md"),
                text("01HCARD", "Fix the bug!\nIt crashes on save."),
                note("c.md"),
            ],
        )],
    };
    let src = write_board_frontmatter("", &board).unwrap();
    vault.write_file("boards/b.md", &src).unwrap();

    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let idx = test_indexer(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();

    let note_rel = ops::promote_text_card(ops::PromoteTextCardArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        log: &log,
        vault: &vault,
        kinds: None,
        board_doc_rel: "boards/b.md",
        card_id: "01HCARD",
        template_kind: None,
    })
    .await
    .unwrap();
    assert_eq!(note_rel, "boards/fix-the-bug.md");

    // The note carries the full card text as a plain body (no plan layer
    // -> plain note per pm.md's no-plan case).
    let body = vault.read_file(&note_rel).unwrap();
    assert_eq!(body, "Fix the bug!\nIt crashes on save.\n");

    // In-place swap: same column, same position, `{ path }` shape.
    let after = parse_board_for(
        "boards/b.md",
        &vault.read_file("boards/b.md").unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(
        after.columns[0].cards,
        vec![note("a.md"), note("boards/fix-the-bug.md"), note("c.md")],
    );

    idx.shutdown().await;
}

/// The kind-template seam (`freeform-promote-note`'s plan `default_kind`
/// hook): a promoted note born with a kind gets `hiker.kind` plus the
/// kind's fields seeded empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_text_card_applies_kind_template() {
    let (td, vault) = test_vault();
    let registry = builtin_registry();
    let board = Board {
        kind: "board".into(),
        columns: vec![col("Todo", vec![text("01HCARD", "Ship onboarding")])],
    };
    let src = write_board_frontmatter("", &board).unwrap();
    vault.write_file("boards/b.md", &src).unwrap();

    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let idx = test_indexer(&vault);
    let log = LayeredDoc::open(td.path()).unwrap();
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();

    let note_rel = ops::promote_text_card(ops::PromoteTextCardArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        log: &log,
        vault: &vault,
        kinds: Some(&registry),
        board_doc_rel: "boards/b.md",
        card_id: "01HCARD",
        template_kind: registry.get("story"),
    })
    .await
    .unwrap();

    let body = vault.read_file(&note_rel).unwrap();
    assert!(body.contains("kind: story"), "hiker.kind seeded: {body}");
    for field in ["priority", "due", "estimate"] {
        assert!(body.contains(field), "field `{field}` seeded: {body}");
    }
    assert!(body.contains("Ship onboarding"), "card text is the body");

    idx.shutdown().await;
}
