use std::collections::BTreeMap;

use super::{
    builtin_kinds_value, validate_note, FieldType, RefTarget, Registry, Shape, StateCategory,
};
use crate::store::dto::MetaEntry;

/// Parse a `[kinds.*]` TOML snippet into the raw entry map `Config.kinds`
/// carries (entry name -> value).
fn entries(toml_src: &str) -> BTreeMap<String, toml::Value> {
    let doc: toml::Value = toml::from_str(toml_src).unwrap();
    let kinds = doc.get("kinds").expect("snippet declares [kinds.*]");
    let toml::Value::Table(table) = kinds else { panic!("kinds not a table") };
    table.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn compile(toml_src: &str) -> Result<Registry, super::Error> {
    Registry::compile(&entries(toml_src))
}

fn meta(pairs: &[(&str, &str)]) -> Vec<MetaEntry> {
    pairs
        .iter()
        .map(|(k, v)| MetaEntry { key: (*k).to_string(), value: (*v).to_string(), num: None })
        .collect()
}

const NO_REFS: fn(&str) -> RefTarget = |_| RefTarget::Missing;

// ---- registry: good entries ----

#[test]
fn compile_spec_example_registry() {
    // The spec's own example (`docs/kinds.md` §"The registry").
    let reg = compile(
        r#"
[kinds.story]
shape = "leaf"
fields = [
  { name = "priority", type = "number" },
  { name = "due",      type = "date" },
  { name = "estimate", type = "number" },
]

[kinds.sprint]
shape = "board-like"
fields = [
  { name = "start", type = "date", required = true },
  { name = "end",   type = "date", required = true },
  { name = "goal",  type = "string" },
]
states = [
  { name = "Backlog", category = "backlog" },
  { name = "Todo",    category = "todo" },
  { name = "Doing",   category = "in_progress" },
  { name = "Review",  category = "in_progress" },
  { name = "Done",    category = "done" },
  { name = "Dropped", category = "canceled" },
]

[kinds.sprint.columns]
"Todo"   = "Todo"
"Doing"  = "Doing"
"Review" = "Review"
"Done"   = "Done"
"#,
    )
    .unwrap();

    let story = reg.get("story").unwrap();
    assert_eq!(story.shape, Shape::Leaf);
    assert_eq!(story.field("priority").unwrap().field_type, FieldType::Number);
    assert!(!story.field("priority").unwrap().required);

    let sprint = reg.get("sprint").unwrap();
    assert_eq!(sprint.shape, Shape::BoardLike);
    assert!(sprint.field("start").unwrap().required);
    assert_eq!(sprint.state_category("Review"), Some(StateCategory::InProgress));
    // Several states may share one category; the column expansion follows
    // the mapping, not the state list.
    let mut cols = sprint.columns_for_category(StateCategory::InProgress);
    cols.sort();
    assert_eq!(cols, vec!["Doing".to_string(), "Review".to_string()]);
    assert_eq!(sprint.columns_for_category(StateCategory::Done), vec!["Done".to_string()]);
    // Backlog is a state but no column maps to it — empty set, not an error.
    assert!(sprint.columns_for_category(StateCategory::Backlog).is_empty());
}

#[test]
fn enum_and_ref_field_declarations_compile() {
    let reg = compile(
        r#"
[kinds.ticket]
shape = "leaf"
fields = [
  { name = "severity", type = "enum", values = ["low", "high"] },
  { name = "epic",     type = "ref",  kind = "epic" },
]
[kinds.epic]
shape = "list-like"
"#,
    )
    .unwrap();
    let ticket = reg.get("ticket").unwrap();
    assert_eq!(ticket.field("severity").unwrap().values, vec!["low", "high"]);
    assert_eq!(ticket.field("epic").unwrap().ref_kind.as_deref(), Some("epic"));
}

// ---- registry: strict-load errors name the offender ----

#[test]
fn unknown_key_names_the_offending_entry() {
    let err = compile("[kinds.story]\nshape = \"leaf\"\nshap = 1\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[kinds.story]"), "{msg}");
    assert!(msg.contains("shap"), "{msg}");
}

#[test]
fn type_outside_primitive_set_is_an_error() {
    let err = compile(
        "[kinds.story]\nshape = \"leaf\"\nfields = [ { name = \"x\", type = \"blob\" } ]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("[kinds.story]"), "{err}");
}

#[test]
fn state_without_category_is_an_error() {
    // A state without a category anchor is a strict-load error
    // (`kind-state-categories`), as is one outside the closed set.
    let missing = compile(
        "[kinds.s]\nshape = \"board-like\"\nstates = [ { name = \"Todo\" } ]\n",
    )
    .unwrap_err();
    assert!(missing.to_string().contains("[kinds.s]"), "{missing}");
    assert!(missing.to_string().contains("category"), "{missing}");
    let unknown = compile(
        "[kinds.s]\nshape = \"board-like\"\nstates = [ { name = \"T\", category = \"later\" } ]\n",
    )
    .unwrap_err();
    assert!(unknown.to_string().contains("[kinds.s]"), "{unknown}");
}

#[test]
fn column_to_undeclared_state_is_an_error() {
    let err = compile(
        r#"
[kinds.s]
shape = "board-like"
states = [ { name = "Todo", category = "todo" } ]
[kinds.s.columns]
"Doing" = "Doing"
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[kinds.s]") && msg.contains("Doing"), "{msg}");
}

#[test]
fn columns_require_board_like_shape_and_states() {
    let leaf = compile(
        "[kinds.s]\nshape = \"leaf\"\n[kinds.s.columns]\n\"A\" = \"A\"\n",
    )
    .unwrap_err();
    assert!(leaf.to_string().contains("board-like"), "{leaf}");
    let stateless = compile(
        "[kinds.s]\nshape = \"board-like\"\n[kinds.s.columns]\n\"A\" = \"A\"\n",
    )
    .unwrap_err();
    assert!(stateless.to_string().contains("states"), "{stateless}");
}

#[test]
fn machinery_discriminator_collision_is_an_error() {
    let err = compile("[kinds.board]\nshape = \"leaf\"\n").unwrap_err();
    assert!(err.to_string().contains("[kinds.board]"), "{err}");
    assert!(err.to_string().contains("machinery"), "{err}");
}

#[test]
fn enum_requires_values_and_values_rejected_elsewhere() {
    let no_values = compile(
        "[kinds.s]\nshape = \"leaf\"\nfields = [ { name = \"x\", type = \"enum\" } ]\n",
    )
    .unwrap_err();
    assert!(no_values.to_string().contains("values"), "{no_values}");
    let misplaced = compile(
        "[kinds.s]\nshape = \"leaf\"\nfields = [ { name = \"x\", type = \"string\", values = [\"a\"] } ]\n",
    )
    .unwrap_err();
    assert!(misplaced.to_string().contains("enum"), "{misplaced}");
    let kind_on_string = compile(
        "[kinds.s]\nshape = \"leaf\"\nfields = [ { name = \"x\", type = \"string\", kind = \"epic\" } ]\n",
    )
    .unwrap_err();
    assert!(kind_on_string.to_string().contains("ref"), "{kind_on_string}");
}

// ---- built-ins ----

#[test]
fn builtin_set_compiles_with_spec_entries() {
    let doc = builtin_kinds_value();
    let table = doc.get("kinds").unwrap().as_table().unwrap();
    let raw: BTreeMap<String, toml::Value> =
        table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let reg = Registry::compile(&raw).unwrap();
    for name in ["story", "task", "epic", "sprint", "plan"] {
        assert!(reg.get(name).is_some(), "builtin `{name}` missing");
    }
    assert_eq!(reg.get("epic").unwrap().shape, Shape::ListLike);
    assert_eq!(reg.get("plan").unwrap().shape, Shape::ListLike);
    assert_eq!(reg.get("sprint").unwrap().shape, Shape::BoardLike);
    // story and task share one definition — two names for the same shape.
    assert_eq!(reg.get("story").unwrap().fields, reg.get("task").unwrap().fields);
}

#[test]
fn disabled_entry_is_skipped() {
    let reg = compile("[kinds.story]\nshape = \"leaf\"\nenabled = false\n").unwrap();
    assert!(reg.get("story").is_none());
    assert!(reg.is_empty());
}

// ---- lenient validation per primitive ----

fn ticket_registry() -> Registry {
    compile(
        r#"
[kinds.ticket]
shape = "leaf"
fields = [
  { name = "priority", type = "number" },
  { name = "due",      type = "date", required = true },
  { name = "severity", type = "enum", values = ["low", "high"] },
  { name = "epic",     type = "ref",  kind = "epic" },
  { name = "note",     type = "string" },
]
[kinds.epic]
shape = "list-like"
"#,
    )
    .unwrap()
}

#[test]
fn clean_note_validates_with_no_problems() {
    let reg = ticket_registry();
    let kind = reg.get("ticket").unwrap();
    let entries = meta(&[
        ("hiker.kind", "ticket"),
        ("priority", "3"),
        ("due", "2026-07-01"),
        ("severity", "high"),
        ("epic", "epics/q3.md"),
        ("note", "anything goes"),
        ("extra_key", "extras are always fine"),
    ]);
    let resolve = |path: &str| -> RefTarget {
        assert_eq!(path, "epics/q3.md");
        RefTarget::Found { kind: Some("epic".into()) }
    };
    assert!(validate_note(kind, &entries, &resolve).is_empty());
}

#[test]
fn each_primitive_violation_is_reported() {
    let reg = ticket_registry();
    let kind = reg.get("ticket").unwrap();
    // Non-number priority, malformed date, out-of-enum severity, ref that
    // doesn't resolve — one problem per violation; the string field takes
    // anything.
    let entries = meta(&[
        ("priority", "soon"),
        ("due", "next tuesday"),
        ("severity", "catastrophic"),
        ("epic", "missing.md"),
        ("note", "fine"),
    ]);
    let problems = validate_note(kind, &entries, &NO_REFS);
    // One problem per violation, in declared-field order.
    let fields: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
    assert_eq!(fields, vec!["priority", "due", "severity", "epic"]);
}

#[test]
fn required_field_missing_is_reported() {
    let reg = ticket_registry();
    let kind = reg.get("ticket").unwrap();
    let problems = validate_note(kind, &meta(&[("priority", "1")]), &NO_REFS);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].field, "due");
    assert!(problems[0].message.contains("required"), "{}", problems[0].message);
}

#[test]
fn ref_to_wrong_kind_is_reported() {
    let reg = ticket_registry();
    let kind = reg.get("ticket").unwrap();
    let resolve =
        |_: &str| -> RefTarget { RefTarget::Found { kind: Some("story".into()) } };
    let entries = meta(&[("due", "2026-07-01"), ("epic", "notes/a.md")]);
    let problems = validate_note(kind, &entries, &resolve);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].field, "epic");
    assert!(problems[0].message.contains("story"), "{}", problems[0].message);
}
