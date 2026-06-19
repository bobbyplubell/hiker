//! User-definable note kinds. See `docs/kinds.md`.
//!
//! The registry is declared as data — `[kinds.<name>]` entries in vault
//! config — and compiled here into a strict-load [`Registry`]: every field
//! type maps onto a closed primitive set, every state carries a required
//! category anchor, every kind declares one of three shapes, and a
//! board-like kind may carry the column-to-state mapping the query
//! grammar's `category` clause compiles against. The registry itself is
//! strict (an invalid entry aborts startup naming the offender, the
//! inbox-rules posture); notes validated *against* it are lenient —
//! [`validate_note`] derives a problems report on ingest and never blocks
//! a write. The built-in PM set ships as TOML in the same format users
//! write, merged as the lowest config layer.
//
// status: kind-registry

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::store::dto::{MetaEntry, NoteProblem};

/// `hiker.kind` values the machinery already dispatches on. A registry
/// entry may not collide with these — the discriminators keep their
/// existing meaning unchanged.
pub const MACHINERY_DISCRIMINATORS: &[&str] = &[
    "board",
    "query",
    "cluster-tree",
    "cluster-preset",
    "trail",
    "waypoint",
    "session",
    "capture",
    "project",
];

/// The closed five-value category anchor every user-named state maps onto.
/// Names are per-vault vocabulary; categories are what automation, UI, and
/// rollups are written against. [kind-state-categories]
///
/// status: kind-state-categories
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateCategory {
    Backlog,
    Todo,
    InProgress,
    Done,
    Canceled,
}

impl StateCategory {
    /// The snake_case wire string, matching the serde encoding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Canceled => "canceled",
        }
    }

    /// Parse the wire string; `None` for anything outside the closed set.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "backlog" => Some(Self::Backlog),
            "todo" => Some(Self::Todo),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }
}

/// The closed structural anchor deciding which authored-doc machinery a
/// kind's notes ride: a plain note, an ordered refs list, or a board.
/// [kind-shapes]
///
/// status: kind-shapes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    Leaf,
    ListLike,
    BoardLike,
}

/// The closed primitive set field types map onto. `enum` and `ref` are
/// validation-time constraints over string storage. [kind-field-primitives]
///
/// status: kind-field-primitives
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Number,
    Date,
    Enum,
    Ref,
}

impl FieldType {
    /// Human-readable name for error / problem messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Date => "date",
            Self::Enum => "enum",
            Self::Ref => "ref",
        }
    }
}

// ---------------------------------------------------------------------------
// TOML shapes — the strict serde forms each `[kinds.<name>]` entry
// deserializes through. Kept private; the compiled `Kind` is the API.
// ---------------------------------------------------------------------------

const fn enabled_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KindDefToml {
    /// `enabled = false` disables an entry (the built-in opt-out).
    #[serde(default = "enabled_default")]
    enabled: bool,
    shape: Shape,
    #[serde(default)]
    fields: Vec<FieldDefToml>,
    #[serde(default)]
    states: Vec<StateDefToml>,
    /// Column name -> state name; board-like kinds only.
    #[serde(default)]
    columns: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldDefToml {
    name: String,
    #[serde(rename = "type")]
    field_type: FieldType,
    #[serde(default)]
    required: bool,
    /// Mandatory for `enum`, rejected elsewhere.
    #[serde(default)]
    values: Option<Vec<String>>,
    /// Optional target-kind constraint; `ref` only.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateDefToml {
    name: String,
    /// Required — a state without a category is a strict-load error.
    category: StateCategory,
}

// ---------------------------------------------------------------------------
// Compiled registry.
// ---------------------------------------------------------------------------

/// One typed field of a kind, compiled from `{ name, type, required?,
/// values?, kind? }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub field_type: FieldType,
    pub required: bool,
    /// The declared value set; non-empty exactly when `field_type` is
    /// [`FieldType::Enum`].
    pub values: Vec<String>,
    /// Target-kind constraint for a `ref` field, when declared.
    pub ref_kind: Option<String>,
}

/// One user-named state with its required category anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub name: String,
    pub category: StateCategory,
}

/// A compiled kind definition. Holding one means the entry passed
/// strict-load validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub name: String,
    pub shape: Shape,
    pub fields: Vec<Field>,
    pub states: Vec<State>,
    /// Column name -> state name. Non-empty only on board-like kinds with
    /// states; every value names a state in `states`. [kind-column-state-map]
    pub columns: BTreeMap<String, String>,
}

impl Kind {
    /// The field named `name`, if declared.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The category anchor of the state named `name`, if declared.
    #[must_use]
    pub fn state_category(&self, name: &str) -> Option<StateCategory> {
        self.states.iter().find(|s| s.name == name).map(|s| s.category)
    }

    /// Column names whose mapped state carries `category` — the
    /// compile-time expansion the query grammar's `category` board clause
    /// uses. Empty when no mapped column carries the category (a query
    /// over it matches nothing). [kind-column-state-map]
    ///
    /// status: kind-column-state-map
    #[must_use]
    pub fn columns_for_category(&self, category: StateCategory) -> Vec<String> {
        self.columns
            .iter()
            .filter(|(_, state)| self.state_category(state) == Some(category))
            .map(|(col, _)| col.clone())
            .collect()
    }

    /// Column names to seed a NEW board of this kind with, so a fresh
    /// sprint is born meaning something (`sprint-board-subtype`). Ordered
    /// by the kind's *states* declaration order (the `columns` mapping is
    /// stored name-sorted — TOML table key order is not preserved through
    /// the config layer — while the `states` list is an ordered TOML
    /// array, so the state progression is the faithful ordering source);
    /// columns sharing a state keep name order. Empty when the kind maps
    /// no columns (callers fall back to the plain `Todo`/`Doing`/`Done`
    /// seed per `board-create`).
    ///
    /// status: sprint-board-subtype
    #[must_use]
    pub fn seed_columns(&self) -> Vec<String> {
        let mut cols: Vec<(usize, &String)> = self
            .columns
            .iter()
            .filter_map(|(col, state)| {
                self.states
                    .iter()
                    .position(|s| &s.name == state)
                    .map(|idx| (idx, col))
            })
            .collect();
        cols.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        cols.into_iter().map(|(_, col)| col.clone()).collect()
    }
}

/// Strict-load registry failure. Every variant names the offending
/// `[kinds.<name>]` entry, per the inbox-rules posture.
#[derive(Debug, thiserror::Error)]
#[error("[kinds.{kind}]: {detail}")]
pub struct Error {
    pub kind: String,
    pub detail: String,
}

fn entry_err(kind: &str, detail: impl Into<String>) -> Error {
    Error { kind: kind.to_string(), detail: detail.into() }
}

/// The compiled kind registry: every enabled `[kinds.<name>]` entry,
/// validated. Constructed once at config load; consumers (queries, the
/// indexer's lenient validation, the MCP tool generator) read it.
#[derive(Debug, Default)]
pub struct Registry {
    kinds: BTreeMap<String, Kind>,
}

impl Registry {
    /// An empty registry — the no-kinds fallback hosts use when compile
    /// fails after load-time validation already passed (drift guard).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Compile the raw `[kinds]` config table (entry name -> TOML value)
    /// into a validated registry. Disabled entries are skipped; any
    /// invalid entry is a loud [`Error`] naming the offender.
    pub fn compile(entries: &BTreeMap<String, toml::Value>) -> Result<Self, Error> {
        let mut kinds = BTreeMap::new();
        for (name, value) in entries {
            if let Some(kind) = compile_entry(name, value)? {
                kinds.insert(name.clone(), kind);
            }
        }
        Ok(Self { kinds })
    }

    /// The kind named `name`, if registered and enabled.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Kind> {
        self.kinds.get(name)
    }

    /// The kind named `name` when it is registered AND board-like — the
    /// board parse gate's acceptance lookup (`sprint-board-subtype`): the
    /// accepted discriminator set is `{ "board" }` plus every kind this
    /// returns `Some` for. `None` for unregistered names and for leaf /
    /// list-like kinds.
    ///
    /// status: sprint-board-subtype
    #[must_use]
    pub fn board_like(&self, name: &str) -> Option<&Kind> {
        self.kinds.get(name).filter(|k| k.shape == Shape::BoardLike)
    }

    /// The kind named `name` when it is registered AND list-like — the
    /// list-doc parse gate's acceptance lookup (`pm-epic-derived-table`):
    /// a note whose `hiker.kind` this returns `Some` for derives
    /// `list_refs` rows on ingest. `None` for unregistered names and for
    /// leaf / board-like kinds.
    ///
    /// status: pm-epic-derived-table
    #[must_use]
    pub fn list_like(&self, name: &str) -> Option<&Kind> {
        self.kinds.get(name).filter(|k| k.shape == Shape::ListLike)
    }

    /// Every registered (enabled) kind, in name order.
    pub fn iter(&self) -> impl Iterator<Item = &Kind> {
        self.kinds.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds.len()
    }
}

/// Deserialize + validate one entry. `Ok(None)` for a disabled entry.
fn compile_entry(name: &str, value: &toml::Value) -> Result<Option<Kind>, Error> {
    let def: KindDefToml = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| entry_err(name, e.to_string()))?;
    if !def.enabled {
        return Ok(None);
    }
    if MACHINERY_DISCRIMINATORS.contains(&name) {
        return Err(entry_err(
            name,
            format!("name collides with the machinery discriminator `{name}`"),
        ));
    }
    let fields = compile_fields(name, def.fields)?;
    let states = compile_states(name, def.states)?;
    let columns = compile_columns(name, def.shape, &states, def.columns)?;
    Ok(Some(Kind { name: name.to_string(), shape: def.shape, fields, states, columns }))
}

fn compile_fields(name: &str, defs: Vec<FieldDefToml>) -> Result<Vec<Field>, Error> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(defs.len());
    for f in defs {
        if !seen.insert(f.name.clone()) {
            return Err(entry_err(name, format!("duplicate field `{}`", f.name)));
        }
        let values = match (f.field_type, f.values) {
            (FieldType::Enum, Some(values)) if !values.is_empty() => values,
            (FieldType::Enum, _) => {
                return Err(entry_err(
                    name,
                    format!("enum field `{}` requires a non-empty `values` list", f.name),
                ));
            }
            (_, Some(_)) => {
                return Err(entry_err(
                    name,
                    format!(
                        "field `{}`: `values` is only valid on enum fields",
                        f.name
                    ),
                ));
            }
            (_, None) => Vec::new(),
        };
        let ref_kind = match (f.field_type, f.kind) {
            (FieldType::Ref, kind) => kind,
            (_, Some(_)) => {
                return Err(entry_err(
                    name,
                    format!("field `{}`: `kind` is only valid on ref fields", f.name),
                ));
            }
            (_, None) => None,
        };
        out.push(Field {
            name: f.name,
            field_type: f.field_type,
            required: f.required,
            values,
            ref_kind,
        });
    }
    Ok(out)
}

fn compile_states(name: &str, defs: Vec<StateDefToml>) -> Result<Vec<State>, Error> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(defs.len());
    for s in defs {
        if !seen.insert(s.name.clone()) {
            return Err(entry_err(name, format!("duplicate state `{}`", s.name)));
        }
        out.push(State { name: s.name, category: s.category });
    }
    Ok(out)
}

/// Validate the column-to-state mapping: board-like kinds with states
/// only, every mapped value naming a declared state.
/// [kind-column-state-map]
fn compile_columns(
    name: &str,
    shape: Shape,
    states: &[State],
    columns: Option<BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>, Error> {
    let Some(columns) = columns else {
        return Ok(BTreeMap::new());
    };
    if shape != Shape::BoardLike {
        return Err(entry_err(name, "`columns` is only valid on board-like kinds"));
    }
    if states.is_empty() {
        return Err(entry_err(name, "`columns` requires a `states` list to map onto"));
    }
    for (col, state) in &columns {
        if !states.iter().any(|s| &s.name == state) {
            return Err(entry_err(
                name,
                format!("column \"{col}\" maps to undeclared state \"{state}\""),
            ));
        }
    }
    Ok(columns)
}

// ---------------------------------------------------------------------------
// Built-in PM set — registry entries in the exact TOML format users write,
// merged as the lowest config layer (built-ins <- user <- vault).
// ---------------------------------------------------------------------------

/// The built-in PM kinds: `story`/`task` (leaf), `epic` (list-like),
/// `sprint` (board-like with the state set + column mapping), `plan`
/// (list-like root). Editable and disable-able per entry — there is no
/// privileged code path; a built-in is exactly a registry entry the user
/// didn't have to type. [kind-builtin-pm-set]
//
// status: kind-builtin-pm-set
pub const BUILTIN_KINDS_TOML: &str = r#"
[kinds.story]
shape = "leaf"
fields = [
  { name = "priority", type = "number" },
  { name = "due",      type = "date" },
  { name = "estimate", type = "number" },
]

[kinds.task]
shape = "leaf"
fields = [
  { name = "priority", type = "number" },
  { name = "due",      type = "date" },
  { name = "estimate", type = "number" },
]

[kinds.epic]
shape = "list-like"

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

[kinds.plan]
shape = "list-like"
"#;

/// The built-in set parsed to a TOML value (`{ kinds = { ... } }`), ready
/// to seed the config deep-merge as its lowest layer.
#[must_use]
pub fn builtin_kinds_value() -> toml::Value {
    toml::from_str(BUILTIN_KINDS_TOML).expect("builtin kinds TOML is valid")
}

/// The built-in set compiled standalone — what a vault with no `[kinds]`
/// overrides registers. Hosts compile from the merged config instead;
/// this is the test / fixture convenience.
#[must_use]
pub fn builtin_registry() -> Registry {
    let doc = builtin_kinds_value();
    let table = doc
        .get("kinds")
        .and_then(toml::Value::as_table)
        .expect("builtin kinds TOML declares [kinds.*]");
    let raw: BTreeMap<String, toml::Value> =
        table.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    Registry::compile(&raw).expect("builtin kinds compile")
}

// ---------------------------------------------------------------------------
// Kind-template note seeding.
// ---------------------------------------------------------------------------

/// A new note's full source from a kind template: `text` as the body,
/// plus — when a kind applies — `hiker.kind` set and the kind's fields
/// seeded empty in frontmatter (the `freeform-promote-note` seeding). A
/// plain note (no frontmatter) otherwise. Shared by the freeform-card
/// promote and the vault rules layer's `create_note` verb.
///
/// status: freeform-promote-note
/// status: rule-closed-verbs
pub fn template_note_body(
    text: &str,
    template_kind: Option<&Kind>,
) -> Result<String, crate::errors::HikerError> {
    let Some(kind) = template_kind else {
        return Ok(format!("{text}\n"));
    };
    let mut fields = serde_json::Map::new();
    for field in &kind.fields {
        fields.insert(field.name.clone(), serde_json::Value::Null);
    }
    let mut fm = serde_yml::Value::Mapping(serde_yml::Mapping::default());
    crate::frontmatter::merge_json_into_yaml(
        &mut fm,
        serde_json::json!({ "hiker": { "kind": kind.name } }),
    );
    crate::frontmatter::merge_json_into_yaml(&mut fm, serde_json::Value::Object(fields));
    crate::frontmatter::assemble(&fm, &format!("{text}\n"))
        .map_err(|e| crate::errors::HikerError::Io(format!("seed note from kind template: {e}")))
}

// ---------------------------------------------------------------------------
// Lenient per-note validation — derives the problems report on ingest.
// ---------------------------------------------------------------------------

/// Resolution outcome for a `ref` field's target path at validation time.
pub enum RefTarget {
    /// The path doesn't resolve to an indexed note.
    Missing,
    /// The path resolves; `kind` is the target's `hiker.kind`, when set.
    Found { kind: Option<String> },
}

/// Validate one note's flattened frontmatter against its kind definition,
/// returning the problems report (empty = clean). Lenient by contract:
/// callers record the problems and never block the write, drop data, or
/// rewrite the file. Extra keys beyond the kind's fields are always fine.
/// [kind-lenient-validation]
///
/// status: kind-lenient-validation
#[must_use]
pub fn validate_note(
    kind: &Kind,
    entries: &[MetaEntry],
    resolve_ref: &dyn Fn(&str) -> RefTarget,
) -> Vec<NoteProblem> {
    let mut problems = Vec::new();
    for field in &kind.fields {
        let values: Vec<&MetaEntry> =
            entries.iter().filter(|e| e.key == field.name).collect();
        if values.is_empty() {
            if field.required {
                problems.push(NoteProblem {
                    field: field.name.clone(),
                    message: "required field is missing".into(),
                });
            }
            continue;
        }
        for entry in values {
            if let Some(message) = check_value(field, entry, resolve_ref) {
                problems.push(NoteProblem { field: field.name.clone(), message });
            }
        }
    }
    problems
}

/// One value against its field's primitive; `Some(message)` on violation.
fn check_value(
    field: &Field,
    entry: &MetaEntry,
    resolve_ref: &dyn Fn(&str) -> RefTarget,
) -> Option<String> {
    let v = entry.value.as_str();
    match field.field_type {
        FieldType::String => None,
        FieldType::Number => v
            .parse::<f64>()
            .is_err()
            .then(|| format!("expected a number, found `{v}`")),
        FieldType::Date => crate::frontmatter::iso_date_epoch(v)
            .is_none()
            .then(|| format!("expected an ISO-8601 date, found `{v}`")),
        FieldType::Enum => (!field.values.iter().any(|allowed| allowed == v)).then(|| {
            format!("`{v}` is not one of: {}", field.values.join(", "))
        }),
        FieldType::Ref => match resolve_ref(v) {
            RefTarget::Missing => Some(format!("ref `{v}` does not resolve to a note")),
            RefTarget::Found { kind } => match &field.ref_kind {
                Some(want) if kind.as_deref() != Some(want.as_str()) => Some(format!(
                    "ref `{v}` resolves to kind `{}`, expected `{want}`",
                    kind.as_deref().unwrap_or("<none>"),
                )),
                _ => None,
            },
        },
    }
}

#[cfg(test)]
mod tests;
