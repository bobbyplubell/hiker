use super::{
    compile_query, parse_filter_json, parse_query_doc_for, run_query, smart_folders, BoardScope,
    Clause, Error, Order, OrderBy, Query,
};
use crate::kinds::{Registry, StateCategory};
use crate::store::dto::{
    BoardCardRow, MetaEntry, MetaFilter, NoteOrder, NoteQuery, NoteUpsert, OrderDir,
};
use crate::store::Store;
use crate::test_helpers::test_store;
use crate::vault::Vault;

/// Category expansion stub for compile-only tests: queries without a
/// `category` scope never call it.
fn no_categories(_: &str, _: StateCategory) -> Result<Vec<String>, Error> {
    panic!("category expansion not expected in this test")
}

// ---- parse: good docs ----

/// The spec's own example doc (`docs/queries.md` §"Query-doc shape").
const SPEC_EXAMPLE: &str = r#"---
hiker:
  kind: query
  query:
    kind: story
    tags: [rust, embedded]
    path: "work/**"
    board: { path: "boards/q3.md", column: Doing }
    fields:
      - { key: priority, min: 2 }
      - { key: due, max: "2026-07-01" }
    order: { by: due, dir: asc }
    limit: 50
---
# Open embedded work

Freeform prose.
"#;

#[test]
fn parse_spec_example_doc() {
    let q = parse_query_doc_for("queries/open-work.md", SPEC_EXAMPLE).unwrap();
    assert_eq!(
        q.clauses,
        vec![
            Clause::FieldEq { key: "hiker.kind".into(), values: vec!["story".into()] },
            Clause::FieldEq { key: "tags".into(), values: vec!["rust".into(), "embedded".into()] },
            Clause::PathGlob("work/**".into()),
            Clause::Board {
                board_path: "boards/q3.md".into(),
                scope: BoardScope::Column("Doing".into()),
            },
            Clause::FieldRange { key: "priority".into(), min: Some(2.0), max: None },
            // ISO date bound encodes to epoch seconds (midnight UTC).
            Clause::FieldRange { key: "due".into(), min: None, max: Some(1_782_864_000.0) },
        ],
    );
    assert_eq!(q.order, Some(Order { by: OrderBy::Field("due".into()), dir: OrderDir::Asc }));
    assert_eq!(q.limit, Some(50));
}

#[test]
fn parse_scalar_sugar_and_inline_json() {
    // Scalar `kind:` sugar via the inline-JSON entry (the MCP path).
    let q = parse_filter_json(&serde_json::json!({ "kind": "story" })).unwrap();
    assert_eq!(
        q.clauses,
        vec![Clause::FieldEq { key: "hiker.kind".into(), values: vec!["story".into()] }],
    );
    // `fields` eq with a list value, exists, and a numeric range.
    let q = parse_filter_json(&serde_json::json!({
        "fields": [
            { "key": "status", "eq": ["active", "blocked"] },
            { "key": "due", "exists": true },
            { "key": "priority", "min": 1, "max": 5 },
        ],
    }))
    .unwrap();
    assert_eq!(
        q.clauses,
        vec![
            Clause::FieldEq { key: "status".into(), values: vec!["active".into(), "blocked".into()] },
            Clause::FieldExists { key: "due".into() },
            Clause::FieldRange { key: "priority".into(), min: Some(1.0), max: Some(5.0) },
        ],
    );
    assert!(q.order.is_none() && q.limit.is_none());
}

// ---- parse: loud failures ----

#[test]
fn parse_rejects_non_md_path() {
    assert!(matches!(
        parse_query_doc_for("q.txt", SPEC_EXAMPLE),
        Err(Error::NotMarkdown(_)),
    ));
}

#[test]
fn parse_rejects_missing_frontmatter_and_wrong_kind() {
    assert!(matches!(
        parse_query_doc_for("q.md", "# just a note\n"),
        Err(Error::MissingFrontmatter),
    ));
    let board_doc = "---\nhiker:\n  kind: board\n---\n";
    assert!(matches!(
        parse_query_doc_for("q.md", board_doc),
        Err(Error::KindMismatch { .. }),
    ));
    let no_block = "---\nhiker:\n  kind: query\n---\n";
    assert!(matches!(
        parse_query_doc_for("q.md", no_block),
        Err(Error::MissingField("hiker.query")),
    ));
}

#[test]
fn parse_rejects_unknown_clause() {
    // A clause outside the closed grammar is a loud error, never a silent
    // match-all.
    let err = parse_filter_json(&serde_json::json!({ "body_contains": "x" })).unwrap_err();
    assert!(matches!(err, Error::UnknownClause(ref c) if c == "body_contains"), "{err}");
}

#[test]
fn parse_board_category_accepts_anchors_and_rejects_the_rest() {
    // status: kind-column-state-map — `category` is a real clause now: the
    // five closed anchors parse; anything else (and column+category
    // together) stays a loud error.
    let q = parse_filter_json(
        &serde_json::json!({ "board": { "path": "boards/q3.md", "category": "in_progress" } }),
    )
    .unwrap();
    assert_eq!(
        q.clauses,
        vec![Clause::Board {
            board_path: "boards/q3.md".into(),
            scope: BoardScope::Category(StateCategory::InProgress),
        }],
    );
    let err = parse_filter_json(
        &serde_json::json!({ "board": { "path": "boards/q3.md", "category": "active" } }),
    )
    .unwrap_err();
    assert!(err.to_string().contains("backlog"), "{err}");
    let err = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/q3.md", "column": "Doing", "category": "done" },
    }))
    .unwrap_err();
    assert!(err.to_string().contains("at most one"), "{err}");
}

#[test]
fn parse_rejects_malformed_field_entries() {
    // Two comparison forms in one entry.
    let err = parse_filter_json(
        &serde_json::json!({ "fields": [{ "key": "p", "eq": "x", "min": 1 }] }),
    )
    .unwrap_err();
    assert!(matches!(err, Error::InvalidClause { clause: "fields", .. }), "{err}");
    // `exists: false` is negation — outside the grammar.
    assert!(parse_filter_json(
        &serde_json::json!({ "fields": [{ "key": "p", "exists": false }] })
    )
    .is_err());
    // Empty eq list.
    assert!(parse_filter_json(&serde_json::json!({ "fields": [{ "key": "p", "eq": [] }] }))
        .is_err());
    // Non-date string range bound.
    assert!(parse_filter_json(
        &serde_json::json!({ "fields": [{ "key": "due", "max": "someday" }] })
    )
    .is_err());
    // Missing key.
    assert!(parse_filter_json(&serde_json::json!({ "fields": [{ "eq": "x" }] })).is_err());
}

#[test]
fn parse_rejects_bad_order_and_limit() {
    assert!(parse_filter_json(&serde_json::json!({ "order": { "dir": "asc" } })).is_err());
    assert!(
        parse_filter_json(&serde_json::json!({ "order": { "by": "due", "dir": "up" } })).is_err()
    );
    assert!(parse_filter_json(&serde_json::json!({ "limit": -1 })).is_err());
    assert!(parse_filter_json(&serde_json::json!({ "limit": "ten" })).is_err());
}

// ---- compile ----

#[test]
fn compile_maps_each_clause_to_bound_predicates() {
    let q = Query {
        clauses: vec![
            Clause::FieldEq { key: "tags".into(), values: vec!["rust".into(), "embedded".into()] },
            Clause::FieldExists { key: "due".into() },
            Clause::FieldRange { key: "priority".into(), min: Some(2.0), max: None },
            Clause::PathGlob("work/**".into()),
            Clause::Board {
                board_path: "boards/q3.md".into(),
                scope: BoardScope::Column("Doing".into()),
            },
        ],
        order: None,
        limit: Some(50),
    };
    let nq: NoteQuery = compile_query(&q, &["due".into()], &|_| false, &no_categories).unwrap();
    assert_eq!(nq.filters.len(), 4);
    assert!(matches!(
        &nq.filters[0],
        MetaFilter::Equals { key, values } if key == "tags" && values.len() == 2,
    ));
    assert!(matches!(&nq.filters[1], MetaFilter::Exists { key } if key == "due"));
    assert!(matches!(
        &nq.filters[2],
        MetaFilter::NumRange { key, min: Some(lo), max: None } if key == "priority" && *lo == 2.0,
    ));
    assert!(matches!(
        &nq.filters[3],
        MetaFilter::Board { board_path, columns: Some(cols) }
            if board_path == "boards/q3.md" && cols == &vec!["Doing".to_string()],
    ));
    assert_eq!(nq.path_glob.as_deref(), Some("work/**"));
    assert_eq!(nq.limit, Some(50));
    assert_eq!(nq.select, vec!["due".to_string()]);
    // Default order is path ascending.
    assert!(matches!(nq.order, Some(NoteOrder::Path { dir: OrderDir::Asc })));
}

#[test]
fn compile_field_order_uses_num_mirror_when_present() {
    let q = Query {
        clauses: vec![],
        order: Some(Order { by: OrderBy::Field("due".into()), dir: OrderDir::Desc }),
        limit: None,
    };
    let nq = compile_query(&q, &[], &|key| key == "due", &no_categories).unwrap();
    assert!(matches!(nq.order, Some(NoteOrder::MetaNum { ref key, dir: OrderDir::Desc }) if key == "due"));
    // No numeric mirror anywhere for the key -> text ordering.
    let nq = compile_query(&q, &[], &|_| false, &no_categories).unwrap();
    assert!(matches!(nq.order, Some(NoteOrder::MetaText { ref key, .. }) if key == "due"));
}

/// Finding 1: a predicate-less query must match NOTHING, not enumerate the
/// whole vault on every refresh. `hiker.query: {}`, `fields: []`, and an
/// order/limit-only doc all parse to zero clauses; each compiles to a
/// constant-false predicate.
#[test]
fn empty_query_compiles_to_match_none() {
    // `{}` -> no clauses.
    let q = parse_filter_json(&serde_json::json!({})).unwrap();
    assert!(q.clauses.is_empty());
    let nq = compile_query(&q, &[], &|_| false, &no_categories).unwrap();
    assert!(matches!(nq.filters.as_slice(), [MetaFilter::MatchNone]));

    // `fields: []` -> still no clauses (each entry would be its own AND).
    let q = parse_filter_json(&serde_json::json!({ "fields": [] })).unwrap();
    assert!(q.clauses.is_empty());
    let nq = compile_query(&q, &[], &|_| false, &no_categories).unwrap();
    assert!(matches!(nq.filters.as_slice(), [MetaFilter::MatchNone]));

    // Order/limit only, no filtering clause -> still match-none, but the
    // shaping is preserved on the compiled query.
    let q = parse_filter_json(&serde_json::json!({
        "order": { "by": "mtime", "dir": "desc" },
        "limit": 10,
    }))
    .unwrap();
    assert!(q.clauses.is_empty());
    let nq = compile_query(&q, &[], &|_| false, &no_categories).unwrap();
    assert!(matches!(nq.filters.as_slice(), [MetaFilter::MatchNone]));
    assert_eq!(nq.limit, Some(10));
    assert!(matches!(nq.order, Some(NoteOrder::Mtime { dir: OrderDir::Desc })));

    // A query that DOES carry a real predicate (plus order/limit) is left
    // alone: no spurious MatchNone slipped in.
    let q = parse_filter_json(&serde_json::json!({
        "kind": "story",
        "order": { "by": "path" },
        "limit": 5,
    }))
    .unwrap();
    let nq = compile_query(&q, &[], &|_| false, &no_categories).unwrap();
    assert!(!nq.filters.iter().any(|f| matches!(f, MetaFilter::MatchNone)));
    assert_eq!(nq.filters.len(), 1);
}

/// End-to-end: an empty query over a populated vault returns ZERO rows,
/// never every note (the regression — match-everything on each refresh).
#[test]
fn empty_query_resolves_to_no_notes_not_all_notes() {
    let (_dir, mut store) = test_store();
    put_note(&mut store, "work/a.md", 1, &[("hiker.kind", "story", None)]);
    put_note(&mut store, "work/b.md", 2, &[("tags", "rust", None)]);
    put_note(&mut store, "personal/c.md", 3, &[]);

    let q = parse_filter_json(&serde_json::json!({})).unwrap();
    let rows = run_query(&store, &Registry::empty(), &q, &[]).unwrap();
    assert!(rows.is_empty(), "empty query must match nothing, got {rows:?}");

    // Sanity: a real predicate over the same vault still matches.
    let q = parse_filter_json(&serde_json::json!({ "kind": "story" })).unwrap();
    let rows = run_query(&store, &Registry::empty(), &q, &[]).unwrap();
    assert_eq!(rows.len(), 1);
}

// ---- end-to-end against a real store ----

/// Insert an indexed note row plus its flattened metadata, mirroring what
/// the indexer writes (`note_meta` keyed on the note's path).
fn put_note(store: &mut Store, path: &str, mtime: i64, meta: &[(&str, &str, Option<f64>)]) {
    store
        .upsert_note(&NoteUpsert {
            path,
            content_hash: "h",
            mtime,
            size: 1,
            indexed_at: mtime,
            embedder_version: "test",
            chunks: Vec::new(),
        })
        .unwrap();
    let entries: Vec<MetaEntry> = meta
        .iter()
        .map(|(k, v, n)| MetaEntry { key: (*k).to_string(), value: (*v).to_string(), num: *n })
        .collect();
    store.replace_note_metadata(path, &entries).unwrap();
}

fn board_card(path: &str, column: &str) -> BoardCardRow {
    BoardCardRow {
        board_id: "boards/q3.md".into(),
        board_path: "boards/q3.md".into(),
        card_note_path: path.into(),
        column_name: column.into(),
        ordinal: 0,
    }
}

/// Seed three work stories (one off-board, one out of date range) plus a
/// decoy outside the path glob, then run the spec example's combined
/// filter end-to-end through parse -> compile -> `query_notes`.
#[test]
fn run_query_combines_clauses_end_to_end() {
    let (_dir, mut store) = test_store();
    let due_jun = ("due", "2026-06-01", Some(1_780_272_000.0));
    let story = ("hiker.kind", "story", None);
    let rust = ("tags", "rust", None);
    put_note(&mut store, "work/a.md", 10, &[story, rust, ("priority", "3", Some(3.0)), due_jun]);
    // Wrong kind -> excluded.
    put_note(&mut store, "work/b.md", 20, &[("hiker.kind", "note", None), rust, due_jun]);
    // Due after the max bound -> excluded.
    put_note(
        &mut store,
        "work/c.md",
        30,
        &[story, rust, ("priority", "9", Some(9.0)), ("due", "2026-12-31", Some(1_798_675_200.0))],
    );
    // Outside the path glob -> excluded.
    put_note(&mut store, "personal/d.md", 40, &[story, rust, due_jun]);
    // Matches everything but isn't on the board -> excluded once the board
    // clause applies.
    put_note(&mut store, "work/e.md", 50, &[story, rust, ("priority", "2", Some(2.0)), due_jun]);
    store
        .replace_board_cards(
            "boards/q3.md",
            &[board_card("work/a.md", "Doing"), board_card("work/c.md", "Doing")],
        )
        .unwrap();

    let q = parse_query_doc_for("queries/open-work.md", SPEC_EXAMPLE).unwrap();
    let rows = run_query(&store, &Registry::empty(), &q, &[]).unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["work/a.md"]);

    // Drop the board clause: e.md joins a.md, ordered by the due-date
    // numeric mirror.
    let q2 = Query {
        clauses: q.clauses.iter().filter(|c| !matches!(c, Clause::Board { .. })).cloned().collect(),
        order: Some(Order { by: OrderBy::Field("due".into()), dir: OrderDir::Asc }),
        limit: None,
    };
    let rows = run_query(&store, &Registry::empty(), &q2, &["due".into()]).unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["work/a.md", "work/e.md"]);
    // `select` projects the named field into each row.
    assert_eq!(rows[0].fields.get("due").map(String::as_str), Some("2026-06-01"));
}

// ---- enumeration + smart folders ----

#[test]
fn smart_folders_enumerate_run_and_surface_errors() {
    let (dir, mut store) = test_store();
    let vault = Vault::open(dir.path()).unwrap();

    let good = "---\nhiker:\n  kind: query\n  query:\n    tags: rust\n---\nWhy this query exists.\n";
    let broken = "---\nhiker:\n  kind: query\n  query:\n    nonsense: 1\n---\n";
    std::fs::create_dir_all(dir.path().join("queries")).unwrap();
    std::fs::write(dir.path().join("queries/rust.md"), good).unwrap();
    std::fs::write(dir.path().join("queries/broken.md"), broken).unwrap();
    std::fs::write(dir.path().join("queries/not-a-doc.txt"), good).unwrap();

    // Mirror the indexer: note rows + flattened `hiker.kind` metadata. The
    // non-`.md` file carries the discriminator but is NOT a query-doc.
    let qkind = ("hiker.kind", "query", None);
    put_note(&mut store, "queries/rust.md", 1, &[qkind]);
    put_note(&mut store, "queries/broken.md", 2, &[qkind]);
    put_note(&mut store, "queries/not-a-doc.txt", 3, &[qkind]);
    put_note(&mut store, "notes/lang.md", 4, &[("tags", "rust", None)]);
    put_note(&mut store, "notes/other.md", 5, &[("tags", "go", None)]);

    let folders = smart_folders(&store, &vault, &Registry::empty()).unwrap();
    let names: Vec<&str> = folders.iter().map(|f| f.rel_path.as_str()).collect();
    // One indexed lookup found both `.md` query-docs; the `.txt` is out.
    assert_eq!(names, vec!["queries/broken.md", "queries/rust.md"]);

    let rust = folders.iter().find(|f| f.rel_path == "queries/rust.md").unwrap();
    assert_eq!(rust.title, "rust");
    let members = rust.result.as_ref().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].path, "notes/lang.md");

    // The malformed doc surfaces its loud parse error, not an empty match.
    let broken = folders.iter().find(|f| f.rel_path == "queries/broken.md").unwrap();
    let err = broken.result.as_ref().unwrap_err();
    assert!(matches!(err, Error::UnknownClause(c) if c == "nonsense"), "{err}");
}

// ---- board category clause: registry + board + store end-to-end ----

/// `board: { path, category }` compiles by reading the board's kind off the
/// metadata index, expanding the category through the kind's column-state
/// mapping to a column-name set, and filtering `board_cards`
/// (`kind-column-state-map`).
#[test]
fn run_query_expands_board_category_through_kind_mapping() {
    let (_dir, mut store) = test_store();
    let registry = crate::kinds::builtin_registry();

    // The board-doc itself, carrying the sprint kind in its indexed meta.
    put_note(&mut store, "boards/sprint-12.md", 1, &[("hiker.kind", "sprint", None)]);
    for path in ["work/a.md", "work/b.md", "work/c.md", "work/d.md"] {
        put_note(&mut store, path, 2, &[]);
    }
    let card = |path: &str, column: &str| BoardCardRow {
        board_id: "boards/sprint-12.md".into(),
        board_path: "boards/sprint-12.md".into(),
        card_note_path: path.into(),
        column_name: column.into(),
        ordinal: 0,
    };
    store
        .replace_board_cards(
            "boards/sprint-12.md",
            &[
                card("work/a.md", "Doing"),
                card("work/b.md", "Review"),
                card("work/c.md", "Done"),
                card("work/d.md", "Parking Lot"), // unmapped column: no PM semantics
            ],
        )
        .unwrap();

    // in_progress expands to the Doing + Review columns.
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/sprint-12.md", "category": "in_progress" },
    }))
    .unwrap();
    let rows = run_query(&store, &registry, &q, &[]).unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, vec!["work/a.md", "work/b.md"]);

    // done expands to the single Done column.
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/sprint-12.md", "category": "done" },
    }))
    .unwrap();
    let rows = run_query(&store, &registry, &q, &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "work/c.md");

    // backlog has no mapped column: matches nothing, not an error.
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/sprint-12.md", "category": "backlog" },
    }))
    .unwrap();
    assert!(run_query(&store, &registry, &q, &[]).unwrap().is_empty());
}

#[test]
fn board_category_over_unmapped_kind_is_a_loud_error() {
    let (_dir, mut store) = test_store();
    let registry = crate::kinds::builtin_registry();
    // A plain board (machinery `hiker.kind: board`) has no kind mapping —
    // genuine misconfiguration, not timing, so it stays a loud error.
    put_note(&mut store, "boards/plain.md", 1, &[("hiker.kind", "board", None)]);

    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/plain.md", "category": "done" },
    }))
    .unwrap();
    let err = run_query(&store, &registry, &q, &[]).unwrap_err();
    assert!(err.to_string().contains("not a registered kind"), "{err}");
}

/// Finding 2: a `category` scope over a board whose `hiker.kind` isn't
/// indexed yet (transient indexer lag — the board file exists, its meta
/// hasn't been re-derived) must degrade to "matches nothing" rather than
/// erroring the WHOLE query. The query's other clauses still resolve.
#[test]
fn board_category_over_unindexed_board_kind_degrades_to_empty() {
    let (_dir, mut store) = test_store();
    let registry = crate::kinds::builtin_registry();

    // A board with NO indexed `hiker.kind` at all (mid-reindex), and a real
    // note that satisfies the query's other clause.
    put_note(&mut store, "boards/bare.md", 1, &[]);
    put_note(&mut store, "work/a.md", 2, &[("hiker.kind", "story", None)]);
    store
        .replace_board_cards("boards/bare.md", &[board_card("work/a.md", "Doing")])
        .unwrap();

    // Category clause alone: degrades to empty, NOT an error.
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/bare.md", "category": "done" },
    }))
    .unwrap();
    let rows = run_query(&store, &registry, &q, &[]).expect("must not error on indexer lag");
    assert!(rows.is_empty(), "unindexed board kind -> empty column set, got {rows:?}");

    // Category clause is ANDed with a kind clause: the category clause
    // matches nothing, so the whole AND is empty — but still no error, so
    // sibling surfaces keep refreshing instead of flickering an error.
    let q = parse_filter_json(&serde_json::json!({
        "kind": "story",
        "board": { "path": "boards/bare.md", "category": "done" },
    }))
    .unwrap();
    let rows = run_query(&store, &registry, &q, &[]).expect("must not error on indexer lag");
    assert!(rows.is_empty(), "{rows:?}");
}

// ---- per-note membership (`matches_note`, rule-condition-reuses-queries) ----

/// `matches_note` answers "does THIS note match" for every clause type —
/// one bound path-equality probe over the compiled query, the vault rules
/// layer's condition check.
#[test]
fn matches_note_covers_each_clause_type() {
    let (_dir, mut store) = test_store();
    let registry = Registry::empty();

    put_note(
        &mut store,
        "work/a.md",
        10,
        &[
            ("hiker.kind", "story", None),
            ("tags", "rust", None),
            ("priority", "3", Some(3.0)),
            ("due", "2026-06-01", Some(1_780_272_000.0)),
        ],
    );
    put_note(&mut store, "personal/b.md", 20, &[("hiker.kind", "note", None)]);
    store
        .replace_board_cards("boards/q3.md", &[board_card("work/a.md", "Doing")])
        .unwrap();
    let m = |q: &Query, path: &str| super::matches_note(&store, &registry, q, path).unwrap();

    // FieldEq (the `kind:` sugar).
    let q = parse_filter_json(&serde_json::json!({ "kind": "story" })).unwrap();
    assert!(m(&q, "work/a.md"));
    assert!(!m(&q, "personal/b.md"));

    // FieldExists.
    let q = parse_filter_json(&serde_json::json!({
        "fields": [ { "key": "due", "exists": true } ],
    }))
    .unwrap();
    assert!(m(&q, "work/a.md"));
    assert!(!m(&q, "personal/b.md"));

    // FieldRange over the numeric mirror (date bound form).
    let q = parse_filter_json(&serde_json::json!({
        "fields": [ { "key": "due", "max": "2026-07-01" } ],
    }))
    .unwrap();
    assert!(m(&q, "work/a.md"));
    let q = parse_filter_json(&serde_json::json!({
        "fields": [ { "key": "priority", "min": 5 } ],
    }))
    .unwrap();
    assert!(!m(&q, "work/a.md"));

    // PathGlob.
    let q = parse_filter_json(&serde_json::json!({ "path": "work/**" })).unwrap();
    assert!(m(&q, "work/a.md"));
    assert!(!m(&q, "personal/b.md"));

    // Board membership, whole-board and column-scoped.
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/q3.md" },
    }))
    .unwrap();
    assert!(m(&q, "work/a.md"));
    assert!(!m(&q, "personal/b.md"));
    let q = parse_filter_json(&serde_json::json!({
        "board": { "path": "boards/q3.md", "column": "Done" },
    }))
    .unwrap();
    assert!(!m(&q, "work/a.md"), "wrong column never matches");

    // Clauses AND; an unindexed path never matches anything.
    let q = parse_filter_json(&serde_json::json!({ "kind": "story", "path": "work/**" })).unwrap();
    assert!(m(&q, "work/a.md"));
    assert!(!m(&q, "ghost.md"));
}

/// The TOML bridge parses the same closed grammar (a rule's `when.filter`
/// table), including the datetime-to-string hop for date bounds.
#[test]
fn parse_filter_toml_bridges_the_same_grammar() {
    let doc: toml::Value = toml::from_str(
        r#"
        kind = "story"
        board = { path = "boards/q3.md", column = "Doing" }
        [[fields]]
        key = "due"
        max = "2026-07-01"
        "#,
    )
    .unwrap();
    let q = super::parse_filter_toml(&doc).unwrap();
    // TOML tables don't preserve key order, so compare as a clause set.
    assert_eq!(q.clauses.len(), 3);
    for clause in [
        Clause::Board {
            board_path: "boards/q3.md".into(),
            scope: BoardScope::Column("Doing".into()),
        },
        Clause::FieldEq { key: "hiker.kind".into(), values: vec!["story".into()] },
        Clause::FieldRange { key: "due".into(), min: None, max: Some(1_782_864_000.0) },
    ] {
        assert!(q.clauses.contains(&clause), "missing {clause:?} in {:?}", q.clauses);
    }
    // Outside the grammar stays loud through the bridge.
    let bad: toml::Value = toml::from_str("regex = \"x\"").unwrap();
    let err = super::parse_filter_toml(&bad).unwrap_err();
    assert!(err.to_string().contains("unknown clause"), "{err}");
}
