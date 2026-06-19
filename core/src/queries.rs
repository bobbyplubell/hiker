//! Saved queries over the derived indexes. See `docs/queries.md`.
//!
//! A **query-doc** is a regular markdown note with `hiker.kind: query`;
//! its `hiker.query` frontmatter block holds a filter in a small closed
//! grammar (kind / tags / path glob / field comparisons / board
//! membership, ANDed; OR only inside a clause). This module owns the
//! parse of that block, the compile onto the structured store surface
//! (`query_notes` over `note_meta` + `board_cards`, every value a bound
//! parameter), and the [`run_query`] entry point every consumer shares —
//! smart folders in Vault mode, the `query` MCP tool, and any later layer
//! all call the same compile path, so no two surfaces can disagree about
//! what a query matches. A malformed filter is a loud parse [`Error`],
//! never a silent empty or match-everything fallback.
//
// status: query-doc-shape

use serde_yml::Value as YamlValue;
use thiserror::Error as ThisError;

use crate::frontmatter::{iso_date_epoch, split};
use crate::kinds::{Registry, StateCategory};
use crate::store::dto::{MetaFilter, NoteOrder, NoteQuery, NoteQueryRow, OrderDir};
use crate::store::Store;
use crate::vault::Vault;

/// The `hiker.kind` discriminator value for query-docs.
pub const KIND: &str = "query";

/// One clause of the closed filter grammar. Every clause must hold (AND);
/// a list value inside a clause matches any element (OR).
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// Field equality (`fields: [{key, eq}]`, plus the `kind:` / `tags:`
    /// sugar). A multi-element `values` is an any-of OR; because
    /// `note_meta` explodes list-valued frontmatter one row per element,
    /// `eq` against a list-valued key *is* "list contains".
    FieldEq { key: String, values: Vec<String> },
    /// The key is present at all (`fields: [{key, exists: true}]`).
    FieldExists { key: String },
    /// Inclusive numeric range over the `note_meta.num` mirror. Bound
    /// values were YAML numbers or ISO-8601 date strings (encoded to
    /// epoch seconds at parse time).
    FieldRange {
        key: String,
        min: Option<f64>,
        max: Option<f64>,
    },
    /// Vault-relative path GLOB (`path: "work/**"`).
    PathGlob(String),
    /// The note is a card on the board at `board_path`, optionally scoped
    /// to one named column or to a state category (`kind-column-state-map`).
    Board {
        board_path: String,
        scope: BoardScope,
    },
}

/// How a board clause narrows the board: the whole board, one named
/// column, or every column whose mapped state carries a category — the
/// `category` form expands to a column-name set at compile time through
/// the board's kind's column-state mapping (`kind-column-state-map`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardScope {
    Whole,
    Column(String),
    Category(StateCategory),
}

/// What a query orders by: the note path, its mtime, or a frontmatter
/// field's indexed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderBy {
    Path,
    Mtime,
    Field(String),
}

/// The `order:` clause — result shaping, not filtering.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub by: OrderBy,
    pub dir: OrderDir,
}

/// A parsed query: the filter clauses plus result shaping. Produced only
/// by the parse functions here, so holding a `Query` means the filter was
/// inside the grammar.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub clauses: Vec<Clause>,
    /// `None` = the spec default, path ascending (applied at compile).
    pub order: Option<Order>,
    pub limit: Option<u32>,
}

/// Loud parse / run failures. Per spec, a query-doc that fails here
/// surfaces an explicit error state wherever it renders — there is no
/// silent fallback.
#[derive(Debug, ThisError)]
pub enum Error {
    #[error("missing frontmatter (expected hiker.kind = query)")]
    MissingFrontmatter,
    #[error("frontmatter not a mapping")]
    NotMapping,
    #[error("hiker.kind expected `query`, found `{found}`")]
    KindMismatch { found: String },
    #[error("non-.md path cannot be a query-doc: {0}")]
    NotMarkdown(String),
    #[error("required field `{0}` missing or wrong type")]
    MissingField(&'static str),
    #[error("unknown clause `{0}` (closed grammar: kind / tags / path / fields / board / order / limit)")]
    UnknownClause(String),
    #[error("clause `{clause}`: {detail}")]
    InvalidClause {
        clause: &'static str,
        detail: String,
    },
    #[error("read query-doc: {0}")]
    Read(String),
    #[error("query failed: {0}")]
    Store(String),
}

fn invalid(clause: &'static str, detail: impl Into<String>) -> Error {
    Error::InvalidClause { clause, detail: detail.into() }
}

// ---------------------------------------------------------------------------
// Parse — mirrors `boards::parse_board_for` (discriminator + `.md` rule).
// ---------------------------------------------------------------------------

/// Parse a query-doc's `hiker.query` filter. Caller MUST verify the source
/// path has a `.md` extension first — a non-`.md` file carrying the
/// discriminator is a regular note per the rule trails and boards share;
/// [`parse_query_doc_for`] is the path-aware wrapper.
pub fn parse_query_doc(source: &str) -> Result<Query, Error> {
    let view = split(source);
    let fm = view.frontmatter.ok_or(Error::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(Error::NotMapping);
    };
    let Some(YamlValue::Mapping(hiker)) = map.get("hiker") else {
        return Err(Error::MissingField("hiker"));
    };
    let kind = hiker
        .get("kind")
        .and_then(YamlValue::as_str)
        .ok_or(Error::MissingField("hiker.kind"))?;
    if kind != KIND {
        return Err(Error::KindMismatch { found: kind.to_string() });
    }
    let query = hiker.get("query").ok_or(Error::MissingField("hiker.query"))?;
    parse_filter(query)
}

/// Path-aware wrapper around [`parse_query_doc`]: rejects non-`.md`
/// extensions before parsing, mirroring `boards::parse_board_for`.
pub fn parse_query_doc_for(rel: &str, source: &str) -> Result<Query, Error> {
    if !rel.ends_with(".md") {
        return Err(Error::NotMarkdown(rel.to_string()));
    }
    parse_query_doc(source)
}

/// Parse a filter mapping (the `hiker.query` block, or an inline MCP
/// filter — same parser, same closed clause set, nothing extra). Unknown
/// keys are loud [`Error::UnknownClause`]s, never ignored.
///
/// status: query-filter-grammar
pub fn parse_filter(filter: &YamlValue) -> Result<Query, Error> {
    let YamlValue::Mapping(map) = filter else {
        return Err(Error::MissingField("hiker.query"));
    };
    let mut q = Query { clauses: Vec::new(), order: None, limit: None };
    for (k, v) in map {
        let Some(key) = k.as_str() else {
            return Err(Error::UnknownClause(format!("{k:?}")));
        };
        match key {
            // Sugar: `kind` / `tags` are fields-eq on fixed keys.
            "kind" => q.clauses.push(Clause::FieldEq {
                key: "hiker.kind".into(),
                values: scalar_list("kind", v)?,
            }),
            "tags" => q.clauses.push(Clause::FieldEq {
                key: "tags".into(),
                values: scalar_list("tags", v)?,
            }),
            "path" => match v.as_str() {
                Some(glob) => q.clauses.push(Clause::PathGlob(glob.to_string())),
                None => return Err(invalid("path", "expected a glob string")),
            },
            "board" => q.clauses.push(parse_board_clause(v)?),
            "fields" => parse_fields_clause(v, &mut q.clauses)?,
            "order" => q.order = Some(parse_order_clause(v)?),
            "limit" => {
                let n = v
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("limit", "expected a non-negative integer"))?;
                q.limit = Some(n);
            }
            other => return Err(Error::UnknownClause(other.to_string())),
        }
    }
    Ok(q)
}

/// Parse an inline JSON filter (the MCP tool's `filter` argument) through
/// the same grammar.
pub fn parse_filter_json(filter: &serde_json::Value) -> Result<Query, Error> {
    let yaml: YamlValue = serde_yml::to_value(filter)
        .map_err(|e| invalid("filter", format!("not a filter object: {e}")))?;
    parse_filter(&yaml)
}

/// Parse an inline TOML filter (a vault rule's `when.filter` table,
/// `docs/rules.md`) through the same grammar — the TOML-value bridge is a
/// JSON hop into [`parse_filter_json`], so rules add no second condition
/// language. TOML datetimes arrive as their ISO-8601 string forms, exactly
/// what the grammar's date bounds take.
///
/// status: rule-condition-reuses-queries
pub fn parse_filter_toml(filter: &toml::Value) -> Result<Query, Error> {
    let json = serde_json::to_value(filter)
        .map_err(|e| invalid("filter", format!("not a filter table: {e}")))?;
    parse_filter_json(&json)
}

/// A scalar (string / number / bool) or a non-empty list of scalars, as
/// the string forms `note_meta.value` stores (numbers and bools stringify
/// exactly as `frontmatter::flatten` writes them).
fn scalar_list(clause: &'static str, v: &YamlValue) -> Result<Vec<String>, Error> {
    let one = |item: &YamlValue| -> Result<String, Error> {
        match item {
            YamlValue::String(s) => Ok(s.clone()),
            YamlValue::Bool(b) => Ok(b.to_string()),
            YamlValue::Number(n) => Ok(n.to_string()),
            other => Err(invalid(clause, format!("expected a scalar, got {other:?}"))),
        }
    };
    match v {
        YamlValue::Sequence(items) if items.is_empty() => {
            Err(invalid(clause, "value list must be non-empty"))
        }
        YamlValue::Sequence(items) => items.iter().map(one).collect(),
        scalar => Ok(vec![one(scalar)?]),
    }
}

/// `board: { path, column? | category? }`. At most one of `column` /
/// `category`; `category` must name one of the five closed anchors and
/// expands at compile time through the board's kind's column-state
/// mapping (`kind-column-state-map`).
///
/// status: kind-column-state-map
fn parse_board_clause(v: &YamlValue) -> Result<Clause, Error> {
    let YamlValue::Mapping(m) = v else {
        return Err(invalid("board", "expected a mapping with `path` (+ optional `column` or `category`)"));
    };
    let mut board_path = None;
    let mut column = None;
    let mut category = None;
    for (k, val) in m {
        match k.as_str() {
            Some("path") => {
                board_path = Some(
                    val.as_str()
                        .ok_or_else(|| invalid("board", "`path` must be a string"))?
                        .to_string(),
                );
            }
            Some("column") => {
                column = Some(
                    val.as_str()
                        .ok_or_else(|| invalid("board", "`column` must be a string"))?
                        .to_string(),
                );
            }
            Some("category") => {
                let raw = val
                    .as_str()
                    .ok_or_else(|| invalid("board", "`category` must be a string"))?;
                category = Some(StateCategory::parse(raw).ok_or_else(|| {
                    invalid(
                        "board",
                        format!(
                            "`category` must be one of backlog / todo / in_progress / \
                             done / canceled, got {raw:?}"
                        ),
                    )
                })?);
            }
            other => return Err(invalid("board", format!("unknown key {other:?}"))),
        }
    }
    let board_path = board_path.ok_or_else(|| invalid("board", "`path` is required"))?;
    let scope = match (column, category) {
        (Some(_), Some(_)) => {
            return Err(invalid("board", "at most one of `column` / `category`"));
        }
        (Some(col), None) => BoardScope::Column(col),
        (None, Some(cat)) => BoardScope::Category(cat),
        (None, None) => BoardScope::Whole,
    };
    Ok(Clause::Board { board_path, scope })
}

/// `fields: [{ key, eq | exists | min/max }]` — one comparison per entry,
/// each entry its own AND clause.
fn parse_fields_clause(v: &YamlValue, clauses: &mut Vec<Clause>) -> Result<(), Error> {
    let YamlValue::Sequence(entries) = v else {
        return Err(invalid("fields", "expected a list of { key, eq | exists | min/max } entries"));
    };
    for entry in entries {
        clauses.push(parse_field_entry(entry)?);
    }
    Ok(())
}

fn parse_field_entry(entry: &YamlValue) -> Result<Clause, Error> {
    let YamlValue::Mapping(m) = entry else {
        return Err(invalid("fields", "each entry must be a mapping"));
    };
    let mut key = None;
    let mut eq = None;
    let mut exists = None;
    let mut min = None;
    let mut max = None;
    for (k, val) in m {
        match k.as_str() {
            Some("key") => {
                key = Some(
                    val.as_str()
                        .ok_or_else(|| invalid("fields", "`key` must be a string"))?
                        .to_string(),
                );
            }
            Some("eq") => eq = Some(scalar_list("fields", val)?),
            Some("exists") => match val.as_bool() {
                // The grammar has no negation: `exists: false` is outside it.
                Some(true) => exists = Some(()),
                _ => return Err(invalid("fields", "`exists` must be `true` (no negation in v1)")),
            },
            Some("min") => min = Some(num_or_date(val)?),
            Some("max") => max = Some(num_or_date(val)?),
            other => return Err(invalid("fields", format!("unknown comparison {other:?}"))),
        }
    }
    let key = key.ok_or_else(|| invalid("fields", "`key` is required"))?;
    // Exactly one comparison form per entry.
    match (eq, exists, min.is_some() || max.is_some()) {
        (Some(values), None, false) => Ok(Clause::FieldEq { key, values }),
        (None, Some(()), false) => Ok(Clause::FieldExists { key }),
        (None, None, true) => Ok(Clause::FieldRange { key, min, max }),
        _ => Err(invalid(
            "fields",
            "exactly one of eq / exists / min-max per entry (write two entries to AND)",
        )),
    }
}

/// A range bound: a YAML number, or an ISO-8601 date string encoded to
/// epoch seconds (date-only = midnight UTC) — the same encoding the
/// `note_meta.num` date mirror uses, so bounds and rows compare in one
/// number space.
fn num_or_date(v: &YamlValue) -> Result<f64, Error> {
    match v {
        YamlValue::Number(n) => n
            .as_f64()
            .ok_or_else(|| invalid("fields", "min/max number out of range")),
        YamlValue::String(s) => iso_date_epoch(s).ok_or_else(|| {
            invalid("fields", format!("min/max must be a number or ISO-8601 date, got {s:?}"))
        }),
        other => Err(invalid("fields", format!("min/max must be a number or ISO-8601 date, got {other:?}"))),
    }
}

fn parse_order_clause(v: &YamlValue) -> Result<Order, Error> {
    let YamlValue::Mapping(m) = v else {
        return Err(invalid("order", "expected { by, dir? }"));
    };
    let mut by = None;
    let mut dir = OrderDir::Asc;
    for (k, val) in m {
        match k.as_str() {
            Some("by") => {
                let name = val
                    .as_str()
                    .ok_or_else(|| invalid("order", "`by` must be a string"))?;
                by = Some(match name {
                    "path" => OrderBy::Path,
                    "mtime" => OrderBy::Mtime,
                    field => OrderBy::Field(field.to_string()),
                });
            }
            Some("dir") => {
                dir = match val.as_str() {
                    Some("asc") => OrderDir::Asc,
                    Some("desc") => OrderDir::Desc,
                    _ => return Err(invalid("order", "`dir` must be `asc` or `desc`")),
                };
            }
            other => return Err(invalid("order", format!("unknown key {other:?}"))),
        }
    }
    let by = by.ok_or_else(|| invalid("order", "`by` is required"))?;
    Ok(Order { by, dir })
}

// ---------------------------------------------------------------------------
// Compile + run — the one path every consumer shares.
// ---------------------------------------------------------------------------

/// Compile a parsed [`Query`] onto the structured store surface
/// (`store-note-query`). Every clause becomes a bound-parameter predicate;
/// `key_has_num` reports whether a frontmatter key carries the numeric
/// mirror, so a field order uses `MetaNum` when present and `MetaText`
/// otherwise. A board `category` scope expands here — at compile time —
/// through `category_columns` (the board's kind's column-state mapping,
/// `kind-column-state-map`) to a column-name set; an empty set matches
/// nothing. The default order is path ascending.
pub fn compile_query(
    query: &Query,
    select: &[String],
    key_has_num: &dyn Fn(&str) -> bool,
    category_columns: &dyn Fn(&str, StateCategory) -> Result<Vec<String>, Error>,
) -> Result<NoteQuery, Error> {
    let mut nq = NoteQuery {
        select: select.to_vec(),
        limit: query.limit,
        ..Default::default()
    };
    // A predicate-less query (`hiker.query: {}`, `fields: []`, or
    // order/limit only) carries no filter to narrow the vault. The module
    // promises no match-everything fallback, so it must resolve to the
    // empty set, never enumerate every note on each refresh. A single
    // constant-false predicate (no `?` binds) forces zero rows while still
    // honouring any order/limit shaping the doc declared.
    if query.clauses.is_empty() {
        nq.filters.push(MetaFilter::MatchNone);
    }
    for clause in &query.clauses {
        match clause {
            Clause::FieldEq { key, values } => nq.filters.push(MetaFilter::Equals {
                key: key.clone(),
                values: values.clone(),
            }),
            Clause::FieldExists { key } => {
                nq.filters.push(MetaFilter::Exists { key: key.clone() });
            }
            Clause::FieldRange { key, min, max } => nq.filters.push(MetaFilter::NumRange {
                key: key.clone(),
                min: *min,
                max: *max,
            }),
            Clause::PathGlob(glob) => nq.path_glob = Some(glob.clone()),
            Clause::Board { board_path, scope } => {
                // status: kind-column-state-map
                let columns = match scope {
                    BoardScope::Whole => None,
                    BoardScope::Column(col) => Some(vec![col.clone()]),
                    BoardScope::Category(cat) => Some(category_columns(board_path, *cat)?),
                };
                nq.filters.push(MetaFilter::Board {
                    board_path: board_path.clone(),
                    columns,
                });
            }
        }
    }
    let default_order = Order { by: OrderBy::Path, dir: OrderDir::Asc };
    let order = query.order.as_ref().unwrap_or(&default_order);
    nq.order = Some(match (&order.by, order.dir) {
        (OrderBy::Path, dir) => NoteOrder::Path { dir },
        (OrderBy::Mtime, dir) => NoteOrder::Mtime { dir },
        (OrderBy::Field(key), dir) if key_has_num(key) => {
            NoteOrder::MetaNum { key: key.clone(), dir }
        }
        (OrderBy::Field(key), dir) => NoteOrder::MetaText { key: key.clone(), dir },
    });
    Ok(nq)
}

/// Resolve a board `category` scope to the matching column-name set: read
/// the board-doc's `hiker.kind` off the metadata index, look the kind up
/// in the registry, and filter its column-state mapping by category.
///
/// A board-doc whose `hiker.kind` isn't indexed yet (transient indexer lag
/// — the board file exists but its meta hasn't been re-derived) yields an
/// empty column set: that clause matches nothing, so a `category`-scoped
/// query returns its other clauses' matches rather than failing entirely
/// and flickering on every refresh. A board carrying a registered kind
/// that declares no column-state mapping, or a kind name not in the
/// registry, stays a loud error — those are genuine misconfiguration, not
/// timing.
fn store_category_columns(
    store: &Store,
    registry: &Registry,
    board_path: &str,
    category: StateCategory,
) -> Result<Vec<String>, Error> {
    let Some(kind_name) = store
        .meta_value(board_path, "hiker.kind")
        .map_err(|e| Error::Store(e.to_string()))?
    else {
        // Unindexed board kind (indexer lag): degrade to "matches nothing".
        return Ok(Vec::new());
    };
    let kind = registry.get(&kind_name).ok_or_else(|| {
        invalid(
            "board",
            format!("category over {board_path}: `{kind_name}` is not a registered kind"),
        )
    })?;
    if kind.columns.is_empty() {
        return Err(invalid(
            "board",
            format!(
                "category over {board_path}: kind `{kind_name}` declares no \
                 column-state mapping"
            ),
        ));
    }
    Ok(kind.columns_for_category(category))
}

/// Run a parsed query against the index, returning resolved note rows
/// (path, title, mtime, plus any `select`ed frontmatter fields). The one
/// entry point smart folders, the MCP tool, and later consumers share.
/// `registry` backs the board `category` scope's compile-time expansion
/// (`kind-column-state-map`); queries without a category clause never
/// consult it.
pub fn run_query(
    store: &Store,
    registry: &Registry,
    query: &Query,
    select: &[String],
) -> Result<Vec<NoteQueryRow>, Error> {
    let has_num = |key: &str| store.meta_key_has_num(key).unwrap_or(false);
    let category_columns = |board_path: &str, cat: StateCategory| {
        store_category_columns(store, registry, board_path, cat)
    };
    let nq = compile_query(query, select, &has_num, &category_columns)?;
    store.query_notes(&nq).map_err(|e| Error::Store(e.to_string()))
}

/// Per-note membership check: does the note at `rel_path` match `query`?
/// The compiled query plus one bound path-equality constraint — one
/// indexed probe, never a full match-set run, never a vault walk. The
/// vault rules layer asks this per firing ("does the triggering note
/// match", `docs/rules.md`); result shaping (`order` / `limit`) is
/// irrelevant to membership and dropped.
///
/// status: rule-condition-reuses-queries
pub fn matches_note(
    store: &Store,
    registry: &Registry,
    query: &Query,
    rel_path: &str,
) -> Result<bool, Error> {
    let has_num = |key: &str| store.meta_key_has_num(key).unwrap_or(false);
    let category_columns = |board_path: &str, cat: StateCategory| {
        store_category_columns(store, registry, board_path, cat)
    };
    let mut nq = compile_query(query, &[], &has_num, &category_columns)?;
    nq.path_eq = Some(rel_path.to_string());
    nq.order = None;
    nq.limit = Some(1);
    let rows = store.query_notes(&nq).map_err(|e| Error::Store(e.to_string()))?;
    Ok(!rows.is_empty())
}

// ---------------------------------------------------------------------------
// Enumeration + smart-folder policy.
// ---------------------------------------------------------------------------

/// Every indexed query-doc note, via one `hiker.kind = query` lookup on
/// the metadata index — never a vault walk. Non-`.md` carriers of the
/// discriminator are excluded per the shared `.md` rule.
pub fn list_query_docs(store: &Store) -> Result<Vec<NoteQueryRow>, Error> {
    let nq = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "hiker.kind".into(),
            values: vec![KIND.into()],
        }],
        order: Some(NoteOrder::Path { dir: OrderDir::Asc }),
        ..Default::default()
    };
    let rows = store.query_notes(&nq).map_err(|e| Error::Store(e.to_string()))?;
    Ok(rows.into_iter().filter(|r| r.path.ends_with(".md")).collect())
}

/// One smart folder: a query-doc (the header row) plus its current
/// matches in query order — or the loud error the doc failed with, which
/// the lens renders as an explicit error state.
#[derive(Debug)]
pub struct SmartFolder {
    /// The query-doc's vault-relative path (the header row's open target).
    pub rel_path: String,
    /// Display title (the doc's filename stem).
    pub title: String,
    /// Members in query order, or the parse / run error.
    pub result: Result<Vec<NoteQueryRow>, Error>,
}

/// The smart-folder projection Vault mode renders: every query-doc with
/// its live matches, recomputed from the indexed `note_meta` /
/// `board_cards` tables. Per-doc failures (unreadable file, filter
/// outside the grammar) land in that folder's `result` instead of hiding
/// the folder or failing the lens.
pub fn smart_folders(
    store: &Store,
    vault: &Vault,
    registry: &Registry,
) -> Result<Vec<SmartFolder>, Error> {
    let docs = list_query_docs(store)?;
    let mut out = Vec::with_capacity(docs.len());
    for doc in docs {
        let result = vault
            .read_file(&doc.path)
            .map_err(|e| Error::Read(e.to_string()))
            .and_then(|src| parse_query_doc_for(&doc.path, &src))
            .and_then(|q| run_query(store, registry, &q, &[]));
        out.push(SmartFolder { rel_path: doc.path, title: doc.title, result });
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
