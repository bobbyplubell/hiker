//! Board unit + integration tests. Pure tests cover frontmatter
//! build/parse round-trips and the in-memory column/card mutations; the
//! async tests drive the real ops through the op-log + indexer (mirroring
//! the trails test harness).

use super::ops::{
    add_card, create_board, delete_column, move_card, rename_column, remove_card,
    reorder_column, set_column_wip_limit, AddCardArgs, MoveCardRequest,
};
use super::*;
use crate::config::sections::BoardsConfig;
use crate::embed::{Embedder, Error as EmbedError};
use crate::indexer::{self, Handle};
use crate::oplog::OpLog;
use crate::store::Store;
use crate::trails::DoubleLinkRef;
use crate::vault::Vault;
use std::sync::Arc;
use tempfile::TempDir;

fn dl(id: &str, path: &str) -> DoubleLinkRef {
    DoubleLinkRef {
        id: id.to_string(),
        path: path.to_string(),
    }
}

fn col(name: &str, cards: Vec<DoubleLinkRef>) -> Column {
    Column {
        name: name.to_string(),
        cards,
        wip_limit: None,
    }
}

fn sample_board() -> Board {
    Board {
        id: "01BOARD".to_string(),
        columns: vec![
            col("Todo", vec![dl("01A", "research/a.md"), dl("01B", "inbox/b.md")]),
            col("Doing", vec![dl("01C", "work/c.md")]),
            col("Done", vec![]),
        ],
    }
}

// ── Pure: build / parse round-trip ───────────────────────────────────

#[test]
fn board_build_parse_round_trip() {
    let board = sample_board();
    let src = write_board_frontmatter("# My Board\n\nProse.\n", &board).unwrap();
    let parsed = parse_board(&src).unwrap();
    assert_eq!(parsed, board);
    // Body preserved.
    assert!(src.contains("# My Board"));
    assert!(src.contains("Prose."));
    // Empty Done column round-trips (columns are explicit).
    assert_eq!(parsed.columns[2].name, "Done");
    assert!(parsed.columns[2].cards.is_empty());
}

#[test]
fn parse_rejects_non_board_kind() {
    let src = "---\nhiker:\n  kind: trail\n  id: x\n---\n";
    assert!(parse_board(src).is_err());
}

#[test]
fn parse_for_rejects_non_md_extension() {
    let board = sample_board();
    let src = write_board_frontmatter("", &board).unwrap();
    assert!(parse_board_for("boards/x.txt", &src).is_err());
    assert!(parse_board_for("boards/x.md", &src).is_ok());
}

#[test]
fn write_preserves_unknown_hiker_siblings_and_top_level() {
    let body = "---\nhiker:\n  kind: board\n  id: 01BOARD\n  author: agent-authored\ntags: [x]\ncolumns: []\n---\nbody\n";
    let mut board = parse_board(body).unwrap();
    board.columns.push(col("New", vec![dl("01Z", "z.md")]));
    let out = write_board_frontmatter(body, &board).unwrap();
    // Unknown sibling + top-level keys survive.
    assert!(out.contains("author: agent-authored"));
    assert!(out.contains("tags:"));
    let reparsed = parse_board(&out).unwrap();
    assert_eq!(reparsed.columns.len(), 1);
    assert_eq!(reparsed.columns[0].name, "New");
}

#[test]
fn wip_limit_round_trips_and_omits_when_none() {
    let mut board = sample_board();
    board.columns[1].wip_limit = Some(3); // Doing capped at 3
    let src = write_board_frontmatter("# B\n", &board).unwrap();
    // The set column serializes the cap; the unset columns do NOT acquire a
    // `wip_limit` key.
    assert!(src.contains("wip_limit"));
    assert_eq!(src.matches("wip_limit").count(), 1, "only the capped column writes it");
    let parsed = parse_board(&src).unwrap();
    assert_eq!(parsed.columns[1].wip_limit, Some(3));
    assert_eq!(parsed.columns[0].wip_limit, None);
    assert_eq!(parsed.columns[2].wip_limit, None);
    assert_eq!(parsed, board);
}

#[test]
fn contains_note_matches_by_id_or_path() {
    let board = sample_board();
    assert!(board.contains_note("01A", "research/a.md"));
    assert!(board.contains_note("", "inbox/b.md")); // path-only
    assert!(board.contains_note("01C", "anything.md")); // id-only
    assert!(!board.contains_note("nope", "nowhere.md"));
}

// ── Async: integration through op-log + indexer ──────────────────────

struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(batch.iter().map(|_| vec![0.0; 384]).collect())
    }
    fn version(&self) -> &str {
        "zero-test"
    }
    fn dim(&self) -> usize {
        384
    }
}

fn start_indexer(vault: Vault, store: Store) -> Handle {
    indexer::start(vault, store, || Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_board_writes_doc_with_default_columns() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    let cfg = BoardsConfig {
        new_board_dir: "boards/".into(),
    };
    let outcome = create_board(&watcher, &idx.job_sender(), &vault, &cfg, "roadmap")
        .await
        .unwrap();
    assert_eq!(outcome.board_doc_rel, "boards/roadmap.md");
    let src = std::fs::read_to_string(td.path().join(&outcome.board_doc_rel)).unwrap();
    let board = parse_board(&src).unwrap();
    assert_eq!(board.id, outcome.board_id);
    let names: Vec<&str> = board.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["Todo", "Doing", "Done"]);
    assert!(board.columns.iter().all(|c| c.cards.is_empty()));

    // Auto-suffix on collision.
    let out2 = create_board(&watcher, &idx.job_sender(), &vault, &cfg, "roadmap")
        .await
        .unwrap();
    assert_eq!(out2.board_doc_rel, "boards/roadmap-1.md");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_card_then_move_and_remove() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();
    let log = OpLog::open(td.path()).unwrap();

    let cfg = BoardsConfig {
        new_board_dir: "boards/".into(),
    };
    let board = create_board(&watcher, &idx.job_sender(), &vault, &cfg, "b")
        .await
        .unwrap();

    std::fs::write(td.path().join("note.md"), "hello").unwrap();

    // Add a card to Todo.
    add_card(AddCardArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        vault: &vault,
        store: &mut read_store,
        log: &log,
        board_doc_rel: &board.board_doc_rel,
        column_name: "Todo",
        source_rel: "note.md",
    })
    .await
    .unwrap();

    let detail_src = std::fs::read_to_string(td.path().join(&board.board_doc_rel)).unwrap();
    let parsed = parse_board(&detail_src).unwrap();
    assert_eq!(parsed.columns[0].cards.len(), 1, "card in Todo");
    let card_id = parsed.columns[0].cards[0].id.clone();
    assert_eq!(parsed.columns[0].cards[0].path, "note.md");
    // Source note stamped.
    let src = std::fs::read_to_string(td.path().join("note.md")).unwrap();
    assert!(src.contains("hiker:") && src.contains("id:"));

    // Idempotent: re-add is a no-op.
    add_card(AddCardArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        vault: &vault,
        store: &mut read_store,
        log: &log,
        board_doc_rel: &board.board_doc_rel,
        column_name: "Doing",
        source_rel: "note.md",
    })
    .await
    .unwrap();
    let parsed = parse_board(&std::fs::read_to_string(td.path().join(&board.board_doc_rel)).unwrap())
        .unwrap();
    let total: usize = parsed.columns.iter().map(|c| c.cards.len()).sum();
    assert_eq!(total, 1, "re-add must not duplicate");

    // Move Todo → Doing.
    move_card(
        &log,
        &idx.job_sender(),
        &vault,
        MoveCardRequest {
            board_doc_rel: &board.board_doc_rel,
            from_column: "Todo",
            card_id: &card_id,
            to_column: "Doing",
            to_index: 0,
        },
    )
    .await
    .unwrap();
    let parsed = parse_board(&std::fs::read_to_string(td.path().join(&board.board_doc_rel)).unwrap())
        .unwrap();
    assert!(parsed.columns[0].cards.is_empty(), "Todo now empty");
    assert_eq!(parsed.columns[1].cards.len(), 1, "Doing has the card");

    // Remove.
    remove_card(&log, &idx.job_sender(), &vault, &board.board_doc_rel, &card_id)
        .await
        .unwrap();
    let parsed = parse_board(&std::fs::read_to_string(td.path().join(&board.board_doc_rel)).unwrap())
        .unwrap();
    assert_eq!(parsed.columns.iter().map(|c| c.cards.len()).sum::<usize>(), 0);

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn within_column_reorder() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = OpLog::open(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    // Seed a board-doc with two cards in Todo by hand.
    std::fs::create_dir_all(td.path().join("boards")).unwrap();
    let board = Board {
        id: "01R".to_string(),
        columns: vec![col("Todo", vec![dl("01X", "x.md"), dl("01Y", "y.md")])],
    };
    let src = write_board_frontmatter("", &board).unwrap();
    std::fs::write(td.path().join("boards/r.md"), &src).unwrap();

    // Move x to index 1 (after y) — within-column reorder.
    move_card(
        &log,
        &idx.job_sender(),
        &vault,
        MoveCardRequest {
            board_doc_rel: "boards/r.md",
            from_column: "Todo",
            card_id: "01X",
            to_column: "Todo",
            to_index: 1,
        },
    )
    .await
    .unwrap();
    let parsed = parse_board(&std::fs::read_to_string(td.path().join("boards/r.md")).unwrap())
        .unwrap();
    let ids: Vec<&str> = parsed.columns[0].cards.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["01Y", "01X"]);

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn column_management_ops() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = OpLog::open(td.path()).unwrap();
    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    let cfg = BoardsConfig {
        new_board_dir: "boards/".into(),
    };
    let board = create_board(&watcher, &idx.job_sender(), &vault, &cfg, "b")
        .await
        .unwrap();
    let rel = &board.board_doc_rel;
    let job = idx.job_sender();

    rename_column(&log, &job, &vault, rel, "Todo", "Backlog").await.unwrap();
    super::ops::add_column(&log, &job, &vault, rel, "Blocked").await.unwrap();
    reorder_column(&log, &job, &vault, rel, "Blocked", 0).await.unwrap();
    delete_column(&log, &job, &vault, rel, "Done").await.unwrap();

    let parsed = parse_board(&std::fs::read_to_string(td.path().join(rel)).unwrap()).unwrap();
    let names: Vec<&str> = parsed.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["Blocked", "Backlog", "Doing"]);

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_and_clear_column_wip_limit() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = OpLog::open(td.path()).unwrap();
    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    let cfg = BoardsConfig {
        new_board_dir: "boards/".into(),
    };
    let board = create_board(&watcher, &idx.job_sender(), &vault, &cfg, "b")
        .await
        .unwrap();
    let rel = &board.board_doc_rel;
    let job = idx.job_sender();

    set_column_wip_limit(&log, &job, &vault, rel, "Doing", Some(2)).await.unwrap();
    let parsed = parse_board(&std::fs::read_to_string(td.path().join(rel)).unwrap()).unwrap();
    assert_eq!(parsed.columns[1].wip_limit, Some(2));
    assert_eq!(parsed.columns[0].wip_limit, None);

    // Clearing drops the key.
    set_column_wip_limit(&log, &job, &vault, rel, "Doing", None).await.unwrap();
    let cleared_src = std::fs::read_to_string(td.path().join(rel)).unwrap();
    assert!(!cleared_src.contains("wip_limit"));
    let parsed = parse_board(&cleared_src).unwrap();
    assert_eq!(parsed.columns[1].wip_limit, None);

    // Unknown column errors.
    assert!(set_column_wip_limit(&log, &job, &vault, rel, "Nope", Some(1)).await.is_err());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_note_moved_rewrites_card_paths() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = OpLog::open(td.path()).unwrap();
    let watcher = crate::watcher::Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);
    let mut sweep_store = Store::open(td.path()).unwrap();

    // Board-doc with a card pointing at old/note.md.
    std::fs::create_dir_all(td.path().join("boards")).unwrap();
    let board = Board {
        id: "01M".to_string(),
        columns: vec![col("Todo", vec![dl("01NOTE", "old/note.md")])],
    };
    let src = write_board_frontmatter("", &board).unwrap();
    std::fs::write(td.path().join("boards/m.md"), &src).unwrap();
    // Make the board-doc discoverable by the sweep's `super::list` (walks
    // store note paths): index it.
    sweep_store
        .upsert_note(&crate::store::dto::NoteUpsert {
            id: "01M",
            path: "boards/m.md",
            content_hash: "h",
            mtime: 0,
            size: 1,
            indexed_at: 0,
            embedder_version: "t",
            chunks: Vec::new(),
        })
        .unwrap();
    // Derived row so boards_containing_note finds it.
    sweep_store
        .replace_board_cards(
            "01M",
            &[crate::store::dto::BoardCardRow {
                board_id: "01M".to_string(),
                board_path: "boards/m.md".to_string(),
                card_note_id: "01NOTE".to_string(),
                card_note_path: "old/note.md".to_string(),
                column_name: "Todo".to_string(),
                ordinal: 0,
            }],
        )
        .unwrap();

    on_note_moved(
        Some(&watcher),
        Some(&idx.job_sender()),
        Some(&log),
        &vault,
        &mut sweep_store,
        "old/note.md",
        "new/note.md",
    )
    .await
    .unwrap();

    let parsed = parse_board(&std::fs::read_to_string(td.path().join("boards/m.md")).unwrap())
        .unwrap();
    assert_eq!(parsed.columns[0].cards[0].path, "new/note.md");
    assert_eq!(parsed.columns[0].cards[0].id, "01NOTE", "id unchanged");

    idx.shutdown().await;
}
