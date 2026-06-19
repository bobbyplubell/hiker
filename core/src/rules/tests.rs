//! Vault rules engine tests (`docs/rules.md`): the strict-load compile
//! (good entries, reserved `script`, unknown verbs / triggers), each
//! closed verb end-to-end through its op path (attributed frames,
//! single-sprint guard, kind templates), the no-cascade generation check,
//! the date-sweep watermark, multi-action one-batch staging, and
//! review-mode pending staging.

use std::collections::BTreeMap;

use tempfile::TempDir;

use super::*;
use crate::boards::{write_board_frontmatter, Board, BoardCard, Column};
use crate::kinds::builtin_registry;
use crate::store::dto::NoteUpsert;
use crate::test_helpers::test_vault;

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

struct Fx {
    _td: TempDir,
    vault: Vault,
    store: Store,
    log: LayeredDoc,
    kinds: Registry,
}

impl Fx {
    fn new() -> Self {
        let (td, vault) = test_vault();
        std::fs::create_dir_all(vault.root().join(".hiker")).expect("mk .hiker");
        let store = Store::open(vault.root()).expect("store open");
        let log = LayeredDoc::open(vault.root()).expect("layered open");
        Self { _td: td, vault, store, log, kinds: builtin_registry() }
    }

    fn ctx(&self) -> FireCtx<'_> {
        FireCtx {
            vault: &self.vault,
            store: &self.store,
            log: &self.log,
            kinds: &self.kinds,
        }
    }

    /// Seed a note on disk, in the layered doc (a `user`-authored doc, the
    /// bootstrap shape), and in the index (`notes` row + `note_meta`).
    fn seed_note(&mut self, rel: &str, contents: &str) {
        self.vault.write_file(rel, contents).expect("write note");
        self.log
            .create_document(rel, "markdown", contents, &Author::User)
            .expect("seed doc");
        self.index_note(rel, contents);
    }

    /// Index a note's row + flattened frontmatter without touching disk
    /// or the layered doc.
    fn index_note(&mut self, rel: &str, contents: &str) {
        self.store
            .upsert_note(&NoteUpsert {
                path: rel,
                content_hash: "h",
                mtime: 0,
                size: 0,
                indexed_at: 0,
                embedder_version: "test",
                chunks: Vec::new(),
            })
            .expect("notes row");
        let entries: Vec<MetaEntry> = crate::frontmatter::split(contents)
            .frontmatter
            .map(|fm| crate::frontmatter::flatten(&fm))
            .unwrap_or_default()
            .into_iter()
            .map(|f| MetaEntry { key: f.key, value: f.value, num: f.num })
            .collect();
        self.store.replace_note_metadata(rel, &entries).expect("note meta");
    }

    /// Seed a sprint board-doc (disk + layered doc + index) holding `cards` in
    /// `column`, plus its derived `board_cards` rows.
    fn seed_sprint(&mut self, rel: &str, columns: &[(&str, &[&str])]) {
        let board = Board {
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
        };
        let src = write_board_frontmatter("", &board).expect("board source");
        self.seed_note(rel, &src);
        let rows: Vec<BoardCardRow> = columns
            .iter()
            .flat_map(|(name, cards)| {
                cards.iter().enumerate().map(|(i, p)| BoardCardRow {
                    board_id: rel.to_string(),
                    board_path: rel.to_string(),
                    card_note_path: (*p).to_string(),
                    column_name: (*name).to_string(),
                    ordinal: i as i64,
                })
            })
            .collect();
        self.store.replace_board_cards(rel, &rows).expect("board cards");
    }
}

/// Compile a `[rules.*]` TOML document against the builtin kinds.
fn compile(src: &str) -> Result<RuleSet, Error> {
    let doc: toml::Value = toml::from_str(src).expect("valid TOML");
    let table = doc
        .get("rules")
        .and_then(toml::Value::as_table)
        .expect("a [rules.*] table");
    let raw: BTreeMap<String, toml::Value> =
        table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    RuleSet::compile(&raw, &builtin_registry())
}

fn ruleset(src: &str) -> RuleSet {
    compile(src).expect("rules compile")
}

fn engine(src: &str, review_required: bool) -> Engine {
    Engine::new(ruleset(src), review_required)
}

fn created(path: &str) -> RuleEvent {
    RuleEvent::NoteCreated { path: path.to_string() }
}

// ---------------------------------------------------------------------------
// Strict-load compile.
// ---------------------------------------------------------------------------

/// The spec's own examples compile: both `on` forms, both `when` forms,
/// and the closed verbs.
#[test]
fn spec_examples_compile() {
    let set = ruleset(
        r#"
        [rules.escalate-overdue]
        on   = { trigger = "date-passed", key = "due" }
        when = { filter = { kind = "story", board = { path = "boards/sprint-12.md" } } }
        do   = [ { set_field = { key = "priority", value = 1 } } ]

        [rules.triage-new-stories]
        enabled = true
        on   = "note-created"
        when = { query_doc = "queries/unplaced-stories.md" }
        do   = [ { add_to_board = { board = "boards/triage.md", column = "Todo" } } ]
        "#,
    );
    assert_eq!(set.len(), 2);
    let overdue = set.get("escalate-overdue").expect("registered");
    assert_eq!(overdue.trigger, Trigger::DatePassed { key: "due".into() });
    assert!(matches!(overdue.condition, Some(Condition::Filter(_))));
    assert_eq!(overdue.actions.len(), 1);
    assert_eq!(overdue.actions[0].verb(), "set_field");
    let triage = set.get("triage-new-stories").expect("registered");
    assert!(triage.enabled);
    assert_eq!(triage.trigger, Trigger::NoteCreated);
    assert_eq!(
        triage.condition,
        Some(Condition::QueryDoc("queries/unplaced-stories.md".into())),
    );
}

/// The `script` verb is rejected as *reserved* — not unknown — naming the
/// rule. [rule-script-slot-reserved]
#[test]
fn script_verb_is_a_reserved_error() {
    let err = compile(
        r#"
        [rules.run-stuff]
        on = "note-created"
        do = [ { script = { src = "evil.lua" } } ]
        "#,
    )
    .expect_err("script must not compile");
    assert!(err.to_string().contains("[rules.run-stuff]"), "{err}");
    assert!(err.to_string().contains("reserved"), "{err}");
    assert!(!err.to_string().contains("unknown"), "{err}");
}

/// An unknown verb fails naming the rule and the closed set.
#[test]
fn unknown_verb_is_an_error() {
    let err = compile(
        r#"
        [rules.bad]
        on = "note-created"
        do = [ { frobnicate = { key = "x" } } ]
        "#,
    )
    .expect_err("unknown verb");
    assert!(err.to_string().contains("unknown action verb `frobnicate`"), "{err}");
}

/// An unknown trigger string fails naming the closed trigger set; the
/// `date-passed` bare-string form points at the table form.
#[test]
fn bad_triggers_are_errors() {
    let err = compile("[rules.r]\non = \"note-updated\"\ndo = [ { set_field = { key = \"a\", value = 1 } } ]")
        .expect_err("unknown trigger");
    assert!(err.to_string().contains("unknown trigger `note-updated`"), "{err}");

    let err = compile("[rules.r]\non = \"date-passed\"\ndo = [ { set_field = { key = \"a\", value = 1 } } ]")
        .expect_err("bare date-passed");
    assert!(err.to_string().contains("table form"), "{err}");
}

/// `when` takes exactly one of `query_doc` / `filter`; a filter clause
/// outside the queries grammar is a loud load-time error.
#[test]
fn condition_validation_is_strict() {
    let err = compile(
        r#"
        [rules.r]
        on = "note-created"
        when = { query_doc = "q.md", filter = { kind = "story" } }
        do = [ { set_field = { key = "a", value = 1 } } ]
        "#,
    )
    .expect_err("both condition forms");
    assert!(err.to_string().contains("exactly one"), "{err}");

    let err = compile(
        r#"
        [rules.r]
        on = "note-created"
        when = { filter = { regex = "x" } }
        do = [ { set_field = { key = "a", value = 1 } } ]
        "#,
    )
    .expect_err("clause outside the grammar");
    assert!(err.to_string().contains("[rules.r]"), "{err}");
    assert!(err.to_string().contains("unknown clause"), "{err}");
}

/// Malformed action references abort: an unregistered `create_note.kind`,
/// a non-`.md` board path, an empty `do` list, a non-scalar field value.
#[test]
fn malformed_action_references_are_errors() {
    let err = compile(
        r#"
        [rules.r]
        on = "note-created"
        do = [ { create_note = { path = "a.md", kind = "ghost" } } ]
        "#,
    )
    .expect_err("unregistered kind");
    assert!(err.to_string().contains("`ghost` is not a registered kind"), "{err}");

    let err = compile(
        r#"
        [rules.r]
        on = "note-created"
        do = [ { add_to_board = { board = "boards/triage" } } ]
        "#,
    )
    .expect_err("non-md board path");
    assert!(err.to_string().contains("`.md` path"), "{err}");

    let err = compile("[rules.r]\non = \"note-created\"\ndo = []")
        .expect_err("empty do");
    assert!(err.to_string().contains("at least one action"), "{err}");

    let err = compile(
        r#"
        [rules.r]
        on = "note-created"
        do = [ { set_field = { key = "a", value = [1, 2] } } ]
        "#,
    )
    .expect_err("non-scalar value");
    assert!(err.to_string().contains("scalar"), "{err}");
}

/// `enabled = false` keeps the entry registered (the panel lists it) but
/// the engine never fires it.
#[test]
fn disabled_rule_is_registered_but_never_fires() {
    let mut fx = Fx::new();
    fx.seed_note("a.md", "---\nkind: x\n---\nbody\n");
    let eng = engine(
        r#"
        [rules.off]
        enabled = false
        on = "note-created"
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    assert_eq!(eng.rules().count(), 1, "disabled entry stays listed");
    assert!(!eng.rules().next().expect("rule").enabled);
    let applied = eng.on_events(&fx.ctx(), &[created("a.md")]);
    assert!(applied.is_empty());
    assert_eq!(fx.vault.read_file("a.md").expect("read"), "---\nkind: x\n---\nbody\n");
}

// ---------------------------------------------------------------------------
// Trigger detection diffs.
// ---------------------------------------------------------------------------

/// `meta_changed` compares (key, value) multisets: reorderings are not
/// changes, value / key edits are.
#[test]
fn meta_changed_compares_multisets() {
    let entry = |k: &str, v: &str| MetaEntry { key: k.into(), value: v.into(), num: None };
    let a = vec![entry("tags", "x"), entry("tags", "y")];
    let b = vec![entry("tags", "y"), entry("tags", "x")];
    assert!(!meta_changed(&a, &b), "reorder is not a change");
    let c = vec![entry("tags", "x"), entry("tags", "z")];
    assert!(meta_changed(&a, &c));
    assert!(meta_changed(&a, &[]));
}

/// `card_moves` reports only column changes for cards present in both row
/// sets — adds and removes are not moves.
#[test]
fn card_moves_diffs_columns_only() {
    let row = |note: &str, col: &str| BoardCardRow {
        board_id: "b".into(),
        board_path: "b.md".into(),
        card_note_path: note.into(),
        column_name: col.into(),
        ordinal: 0,
    };
    let before = vec![row("a.md", "Todo"), row("gone.md", "Todo")];
    let after = vec![row("a.md", "Doing"), row("new.md", "Todo")];
    let events = card_moves("b.md", &before, &after);
    assert_eq!(
        events,
        vec![RuleEvent::CardMoved {
            board_path: "b.md".into(),
            note_path: "a.md".into(),
            from_column: "Todo".into(),
            to_column: "Doing".into(),
        }],
    );
}

// ---------------------------------------------------------------------------
// Verbs end-to-end through their op paths.
// ---------------------------------------------------------------------------

/// `set_field` lands as an accepted edit authored
/// `auto:rule:<name>` and the merged frontmatter reaches disk.
/// [rule-attribution]
#[test]
fn set_field_writes_an_attributed_frame() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\nbody\n");
    let eng = engine(
        r#"
        [rules.prioritize]
        on = "note-created"
        when = { filter = { kind = "story" } }
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    assert_eq!(applied, vec!["story.md".to_string()]);
    let on_disk = fx.vault.read_file("story.md").expect("read");
    assert!(on_disk.contains("priority: 1"), "{on_disk}");
    assert!(on_disk.contains("body"), "{on_disk}");
    // The op-log frame-author assertion is retired with the history engine;
    // the durable observable is the merged frontmatter on disk above.
    assert!(eng.failures().is_empty());
}

/// A condition that doesn't match fires nothing.
#[test]
fn non_matching_condition_skips_the_firing() {
    let mut fx = Fx::new();
    fx.seed_note("note.md", "---\nhiker:\n  kind: task\n---\n\n");
    let eng = engine(
        r#"
        [rules.prioritize]
        on = "note-created"
        when = { filter = { kind = "story" } }
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("note.md")]);
    assert!(applied.is_empty());
    assert!(!fx.vault.read_file("note.md").expect("read").contains("priority"));
}

/// `add_to_board` refuses through the single-sprint guard exactly as it
/// would a user: the note already on sprint A never lands on sprint B,
/// and the failure surfaces in the diagnostics ring. [rule-closed-verbs]
#[test]
fn add_to_board_binds_the_single_sprint_guard() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    fx.seed_sprint("boards/a.md", &[("Todo", &["story.md"]), ("Done", &[])]);
    fx.seed_sprint("boards/b.md", &[("Todo", &[]), ("Done", &[])]);
    let b_before = fx.vault.read_file("boards/b.md").expect("read");
    let eng = engine(
        r#"
        [rules.route]
        on = "note-created"
        do = [ { add_to_board = { board = "boards/b.md", column = "Todo" } } ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    assert!(applied.is_empty(), "guard must refuse the firing");
    assert_eq!(fx.vault.read_file("boards/b.md").expect("read"), b_before);
    let failures = eng.failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].rule, "route");
    assert!(failures[0].message.contains("at most one sprint"), "{}", failures[0].message);
}

/// `add_to_board` onto a free note appends the card (defaulting to the
/// first column when unset), through the staged-batch path.
#[test]
fn add_to_board_appends_to_the_default_column() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    fx.seed_sprint("boards/b.md", &[("Todo", &[]), ("Done", &[])]);
    let eng = engine(
        r#"
        [rules.route]
        on = "note-created"
        do = [ { add_to_board = { board = "boards/b.md" } } ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    assert_eq!(applied, vec!["boards/b.md".to_string()]);
    let board = fx.vault.read_file("boards/b.md").expect("read");
    let parsed = boards::parse_board_for("boards/b.md", &board, Some(&fx.kinds)).expect("parse");
    assert_eq!(parsed.columns[0].name, "Todo");
    assert_eq!(parsed.columns[0].cards, vec![BoardCard::Note { path: "story.md".into() }]);
}

/// `move_card` rides the shared `apply_edit` step; `board` unset resolves
/// the note's one sprint via the derived-status read.
#[test]
fn move_card_defaults_to_the_one_sprint() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    fx.seed_sprint("boards/a.md", &[("Todo", &["story.md"]), ("Done", &[])]);
    let eng = engine(
        r#"
        [rules.finish]
        on = "frontmatter-changed"
        do = [ { move_card = { column = "Done" } } ]
        "#,
        false,
    );
    let applied = eng.on_events(
        &fx.ctx(),
        &[RuleEvent::FrontmatterChanged { path: "story.md".into() }],
    );
    assert_eq!(applied, vec!["boards/a.md".to_string()]);
    let board = fx.vault.read_file("boards/a.md").expect("read");
    let parsed = boards::parse_board_for("boards/a.md", &board, Some(&fx.kinds)).expect("parse");
    assert!(parsed.columns[0].cards.is_empty(), "left Todo");
    assert_eq!(parsed.columns[1].cards, vec![BoardCard::Note { path: "story.md".into() }]);
    assert!(eng.failures().is_empty());
}

/// `create_note` mints the note from the kind template — `hiker.kind` set,
/// the kind's fields seeded empty — collision-suffixed like promote, with
/// the Create frame rule-authored.
#[test]
fn create_note_seeds_the_kind_template() {
    let mut fx = Fx::new();
    fx.seed_note("trigger.md", "---\nkind: x\n---\n\n");
    fx.vault.write_file("notes/report.md", "taken\n").expect("collision seed");
    let eng = engine(
        r#"
        [rules.mint]
        on = "note-created"
        do = [ { create_note = { path = "notes/report.md", kind = "story" } } ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("trigger.md")]);
    assert_eq!(applied, vec!["notes/report-1.md".to_string()], "collision suffix");
    let minted = fx.vault.read_file("notes/report-1.md").expect("read");
    assert!(minted.contains("kind: story"), "{minted}");
    for field in ["priority", "due", "estimate"] {
        assert!(minted.contains(field), "field `{field}` seeded: {minted}");
    }
    // The op-log frame-author assertion is retired with the history engine;
    // the durable observable is the minted note on disk above.
}

// ---------------------------------------------------------------------------
// No cascades.
// ---------------------------------------------------------------------------

/// Cascade behavior after the op-log history engine + per-write
/// attribution were retired: the authorship-based no-cascade guard is gone
/// (snapshots carry no author to read). The rule now fires on a matching
/// event regardless of who last wrote the note; cascade protection rests on
/// rule idempotency + the watcher's self-write suppression. This guards that
/// the firing still APPLIES (the durable observable) on a rule-seeded note —
/// the case the old guard would have skipped. [rule-no-cascade]
#[test]
fn rule_fires_regardless_of_prior_author() {
    let mut fx = Fx::new();
    let src = "---\nhiker:\n  kind: story\n---\n\n";
    fx.vault.write_file("story.md", src).expect("write");
    // Seed the doc as rule-authored — the old guard skipped this; now it
    // fires.
    fx.log
        .create_document("story.md", "markdown", src, &Author::Auto("rule:other".into()))
        .expect("rule-authored doc");
    fx.index_note("story.md", src);
    let eng = engine(
        r#"
        [rules.prioritize]
        on = "frontmatter-changed"
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    let event = RuleEvent::FrontmatterChanged { path: "story.md".into() };
    let applied = eng.on_events(&fx.ctx(), &[event]);
    assert_eq!(applied, vec!["story.md".to_string()]);
    assert!(fx.vault.read_file("story.md").expect("read").contains("priority: 1"));
}

// ---------------------------------------------------------------------------
// Date sweep.
// ---------------------------------------------------------------------------

/// The sweep fires once per crossing between watermarks and never
/// double-fires: the first sweep only records the watermark, the crossing
/// sweep fires, the next sweep is silent. [rule-triggers]
#[test]
fn date_sweep_watermark_prevents_double_fire() {
    let mut fx = Fx::new();
    let due = crate::frontmatter::iso_date_epoch("2026-06-10").expect("epoch");
    fx.seed_note(
        "story.md",
        "---\nhiker:\n  kind: story\ndue: 2026-06-10\n---\n\n",
    );
    let eng = engine(
        r#"
        [rules.overdue]
        on = { trigger = "date-passed", key = "due" }
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    // First sweep, before the due date: records the watermark, fires nothing.
    assert!(eng.date_sweep(&fx.ctx(), due - 100.0).is_empty());
    // The crossing sweep fires once.
    let applied = eng.date_sweep(&fx.ctx(), due + 100.0);
    assert_eq!(applied, vec!["story.md".to_string()]);
    assert!(fx.vault.read_file("story.md").expect("read").contains("priority: 1"));
    // Subsequent sweeps never re-fire the same crossing.
    assert!(eng.date_sweep(&fx.ctx(), due + 200.0).is_empty());
    assert!(eng.failures().is_empty());
}

/// A failed watermark write skips the crossing's firings entirely (the
/// watermark persists BEFORE any firing — the fail-safe direction: a lost
/// firing over a duplicate one), lands in the failure ring, and the next
/// sweep — write healthy again — fires the deferred crossing exactly
/// once. [rule-triggers]
#[test]
fn failed_watermark_write_defers_firings_to_the_next_sweep() {
    let mut fx = Fx::new();
    let due = crate::frontmatter::iso_date_epoch("2026-06-10").expect("epoch");
    fx.seed_note(
        "story.md",
        "---\nhiker:\n  kind: story\ndue: 2026-06-10\n---\n\n",
    );
    let eng = engine(
        r#"
        [rules.overdue]
        on = { trigger = "date-passed", key = "due" }
        do = [ { set_field = { key = "priority", value = 1 } } ]
        "#,
        false,
    );
    // First sweep, before the due date: records the watermark.
    assert!(eng.date_sweep(&fx.ctx(), due - 100.0).is_empty());
    // Sabotage: DB triggers on the sidecar table reject watermark upserts
    // (both upsert arms), simulating a failed write.
    let saboteur =
        rusqlite::Connection::open(fx.store.db_path()).expect("open second conn");
    saboteur
        .execute_batch(
            "CREATE TRIGGER fail_wm_insert BEFORE INSERT ON meta
               WHEN NEW.key LIKE 'rules.sweep.%'
               BEGIN SELECT RAISE(ABORT, 'injected watermark failure'); END;
             CREATE TRIGGER fail_wm_update BEFORE UPDATE ON meta
               WHEN NEW.key LIKE 'rules.sweep.%'
               BEGIN SELECT RAISE(ABORT, 'injected watermark failure'); END;",
        )
        .expect("install failure triggers");
    // The crossing sweep can't persist its watermark: nothing fires, the
    // failure is loud in the ring.
    assert!(eng.date_sweep(&fx.ctx(), due + 100.0).is_empty());
    assert!(!fx.vault.read_file("story.md").expect("read").contains("priority"));
    let failures = eng.failures();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].message.contains("watermark"), "{}", failures[0].message);
    // Heal the store; the next sweep fires the deferred crossing once.
    saboteur
        .execute_batch("DROP TRIGGER fail_wm_insert; DROP TRIGGER fail_wm_update;")
        .expect("drop failure triggers");
    let applied = eng.date_sweep(&fx.ctx(), due + 200.0);
    assert_eq!(applied, vec!["story.md".to_string()]);
    assert!(fx.vault.read_file("story.md").expect("read").contains("priority: 1"));
    // And never again after.
    assert!(eng.date_sweep(&fx.ctx(), due + 300.0).is_empty());
}

// ---------------------------------------------------------------------------
// Batching, review mode, and the firings projection.
// ---------------------------------------------------------------------------

/// A multi-action firing stages every write under ONE batch id (the
/// sprint-close shape) — and under review mode the batch stays pending,
/// nothing applied. [rule-attribution]
#[test]
fn multi_action_firing_stages_one_pending_batch_under_review() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    fx.seed_sprint("boards/b.md", &[("Todo", &[])]);
    let board_before = fx.vault.read_file("boards/b.md").expect("read");
    let eng = engine(
        r#"
        [rules.intake]
        on = "note-created"
        do = [
          { set_field = { key = "priority", value = 2 } },
          { add_to_board = { board = "boards/b.md", column = "Todo" } },
        ]
        "#,
        true,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    assert!(applied.is_empty(), "review mode applies nothing");
    // Nothing reached disk.
    assert!(!fx.vault.read_file("story.md").expect("read").contains("priority"));
    assert_eq!(fx.vault.read_file("boards/b.md").expect("read"), board_before);
    // Both writes are pending under one batch id.
    let pending = op_writes::list_pending_proposals(&fx.log).expect("pending");
    assert_eq!(pending.len(), 2);
    let batch = pending[0].batch_id.clone().expect("batch id");
    assert!(pending.iter().all(|p| p.batch_id.as_deref() == Some(batch.as_str())));
    let paths: Vec<&str> = pending.iter().map(|p| p.target_path.as_str()).collect();
    assert!(paths.contains(&"story.md") && paths.contains(&"boards/b.md"));
    // Accepting the batch applies both — and acceptance keeps the rule
    // author, so the firings projection still sees it.
    op_writes::flip_batch_status(&fx.log, &batch, true).expect("accept");
    assert!(fx.vault.read_file("story.md").expect("read").contains("priority: 2"));
}

/// With review off, a multi-action firing applies as one batch and both
/// frames project under the rule's author wire — the firings panel's
/// per-rule history read. [rule-firings-panel]
#[test]
fn firings_project_by_rule_author() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    fx.seed_sprint("boards/b.md", &[("Todo", &[])]);
    let eng = engine(
        r#"
        [rules.intake]
        on = "note-created"
        do = [
          { set_field = { key = "priority", value = 2 } },
          { add_to_board = { board = "boards/b.md", column = "Todo" } },
        ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    // Both actions applied (the story note's field set + the board add). The
    // op-log author projection that this test used to assert on is retired
    // (the core rework); the applied set is the surviving observable.
    assert_eq!(applied.len(), 2);
    assert!(applied.iter().any(|p| p == "story.md"));
    assert!(applied.iter().any(|p| p == "boards/b.md"));
}

/// A failed action aborts the remaining actions of the firing; the
/// already-computed prefix stands (no cross-document rollback) and the
/// failure lands in the diagnostics ring.
#[test]
fn failed_action_aborts_the_rest_but_keeps_the_prefix() {
    let mut fx = Fx::new();
    fx.seed_note("story.md", "---\nhiker:\n  kind: story\n---\n\n");
    let eng = engine(
        r#"
        [rules.partial]
        on = "note-created"
        do = [
          { set_field = { key = "priority", value = 3 } },
          { add_to_board = { board = "boards/missing.md", column = "Todo" } },
          { set_field = { key = "never", value = 1 } },
        ]
        "#,
        false,
    );
    let applied = eng.on_events(&fx.ctx(), &[created("story.md")]);
    assert_eq!(applied, vec!["story.md".to_string()]);
    let on_disk = fx.vault.read_file("story.md").expect("read");
    assert!(on_disk.contains("priority: 3"), "prefix stands: {on_disk}");
    assert!(!on_disk.contains("never"), "tail aborted: {on_disk}");
    let failures = eng.failures();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].message.contains("add_to_board"), "{}", failures[0].message);
}

/// A missing `query_doc` condition is a loud per-firing error in the
/// ring, never a silent skip-or-match. [rule-condition-reuses-queries]
#[test]
fn missing_query_doc_is_a_loud_firing_error() {
    let mut fx = Fx::new();
    fx.seed_note("a.md", "---\nkind: x\n---\n\n");
    let eng = engine(
        r#"
        [rules.q]
        on = "note-created"
        when = { query_doc = "queries/gone.md" }
        do = [ { set_field = { key = "a", value = 1 } } ]
        "#,
        false,
    );
    assert!(eng.on_events(&fx.ctx(), &[created("a.md")]).is_empty());
    let failures = eng.failures();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].message.contains("queries/gone.md"), "{}", failures[0].message);
}

/// The query-doc parse memo is keyed on the source bytes: an edited doc
/// takes effect on the very next firing (never a stale cached condition),
/// and an unchanged doc serves repeat firings off the memo.
/// [rule-condition-reuses-queries]
#[test]
fn edited_query_doc_takes_effect_on_the_next_firing() {
    let mut fx = Fx::new();
    fx.seed_note("a.md", "---\nhiker:\n  kind: task\n---\n\n");
    fx.seed_note("b.md", "---\nhiker:\n  kind: task\n---\n\n");
    fx.vault
        .write_file("queries/cond.md", "---\nhiker:\n  kind: query\n  query:\n    kind: story\n---\n")
        .expect("seed query doc");
    let eng = engine(
        r#"
        [rules.q]
        on = "note-created"
        when = { query_doc = "queries/cond.md" }
        do = [ { set_field = { key = "routed", value = 1 } } ]
        "#,
        false,
    );
    // First firing parses + memoizes; `kind: story` doesn't match the note.
    assert!(eng.on_events(&fx.ctx(), &[created("a.md")]).is_empty());
    assert!(eng.failures().is_empty());
    // Edit the doc on disk: the memoized parse is stale and must not serve.
    fx.vault
        .write_file("queries/cond.md", "---\nhiker:\n  kind: query\n  query:\n    kind: task\n---\n")
        .expect("edit query doc");
    assert_eq!(eng.on_events(&fx.ctx(), &[created("a.md")]), vec!["a.md".to_string()]);
    assert!(fx.vault.read_file("a.md").expect("read").contains("routed"));
    // Unchanged doc: the memo's hit path serves a second note's firing.
    assert_eq!(eng.on_events(&fx.ctx(), &[created("b.md")]), vec!["b.md".to_string()]);
    assert!(fx.vault.read_file("b.md").expect("read").contains("routed"));
    assert!(eng.failures().is_empty());
}
