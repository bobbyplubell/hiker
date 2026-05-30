//! Pure board-frontmatter parse / write / contains-note tests under
//! path-as-identity (`board-card-references`). The op-level integration
//! tests retired with the legacy double-link shape; they're replaced by
//! parse / write coverage that exercises the new `{path}` note-card and
//! `{card_id, text}` freeform-card serialization.

use super::*;

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

#[test]
fn board_build_parse_round_trip() {
    let board = sample_board();
    let body = write_board_frontmatter("", &board).unwrap();
    let parsed = parse_board_for("boards/q3.md", &body).unwrap();
    assert_eq!(parsed, board);
}

#[test]
fn parse_rejects_non_board_kind() {
    let src = "---\nhiker:\n  kind: trail\n---\n";
    assert!(parse_board(src).is_err());
}

#[test]
fn parse_for_rejects_non_md_extension() {
    let src = "---\nhiker:\n  kind: board\n---\n";
    let err = parse_board_for("boards/q3.txt", src).unwrap_err();
    assert!(matches!(err, Error::NotMarkdown(_)));
    assert!(parse_board_for("boards/q3.md", src).is_ok());
}

#[test]
fn write_preserves_unknown_hiker_siblings_and_top_level() {
    let src = "---\ntitle: Q3 plan\nhiker:\n  kind: board\n  author: user-authored\n  columns: []\ntags: [planning]\n---\nbody\n";
    let parsed = parse_board(src).unwrap();
    let written = write_board_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("title: Q3 plan"));
    assert!(written.contains("author: user-authored"));
    assert!(written.contains("tags:"));
}

#[test]
fn wip_limit_round_trips_and_omits_when_none() {
    let mut board = Board {
        columns: vec![Column {
            name: "Doing".into(),
            cards: vec![],
            wip_limit: Some(3),
        }],
    };
    let s = write_board_frontmatter("", &board).unwrap();
    assert!(s.contains("wip_limit: 3"));
    let parsed = parse_board(&s).unwrap();
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
        columns: vec![col(
            "Doing",
            vec![
                note("work/migration.md"),
                text("01HTEXT", "quick reminder"),
            ],
        )],
    };
    let s = write_board_frontmatter("", &board).unwrap();
    let parsed = parse_board(&s).unwrap();
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

// status: board-doc-shape — rewriting a legacy board-doc that carried
// `hiker.id` drops the stale field.
#[test]
fn write_strips_legacy_hiker_id() {
    let src = "---\nhiker:\n  kind: board\n  id: 01HLEGACY\n  columns: []\n---\nbody\n";
    let parsed = parse_board(src).unwrap();
    let written = write_board_frontmatter(src, &parsed).unwrap();
    assert!(!written.contains("01HLEGACY"));
}

// status: board-freeform-card — legacy `id:` on a freeform card parses
// as the new `card_id` so existing board-docs round-trip.
#[test]
fn parse_accepts_legacy_id_on_freeform_card() {
    let src = "---\nhiker:\n  kind: board\n  columns:\n    - name: Todo\n      cards:\n        - { id: 01HLEGACY, text: 'old freeform' }\n---\n";
    let parsed = parse_board(src).unwrap();
    let BoardCard::Text { card_id, text } = &parsed.columns[0].cards[0] else {
        panic!("expected text card");
    };
    assert_eq!(card_id, "01HLEGACY");
    assert_eq!(text, "old freeform");
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
    let plan = ops::plan_new_board(&vault, &cfg, "q3").unwrap();
    assert_eq!(plan.board_doc_rel, "boards/q3.md");
    assert!(plan.body.contains("hiker:"));
    assert!(plan.body.contains("kind: board"));
    // Plan is read-only: the file shouldn't exist yet.
    assert!(!td.path().join(plan.board_doc_rel).exists());
}
