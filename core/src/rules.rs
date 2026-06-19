//! Vault rules: post-index automation over the derived indexes. See
//! `docs/rules.md`.
//!
//! A rule is **trigger + condition + actions**, declared as data —
//! `[rules.<name>]` entries in vault config beside `[kinds.<name>]`,
//! strict-loaded into a [`RuleSet`] (an invalid entry aborts startup naming
//! the offender, the kind-registry posture). The condition reuses the
//! queries grammar wholesale (a per-note membership check, never a full
//! query run); actions are a closed verb set, each routed through a write
//! path that already exists; every firing stages through the layered doc as ONE
//! batch authored `auto:rule:<name>` (the sprint-close shape), auto-flipped
//! when review mode is off. Rule-initiated writes never fire rules — one
//! generation per external event ([`Engine::on_events`]'s generation check).
//
// status: rule-shape

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde::Deserialize;

use crate::boards;
use crate::kinds::Registry;
use crate::editing::shapes::Author;
use crate::editing::LayeredDoc;
use crate::ops::op_writes::{self, Draft};
use crate::queries;
use crate::store::dto::{BoardCardRow, MetaEntry};
use crate::store::Store;
use crate::vault::Vault;

/// The author wire prefix every rule firing carries (`auto:rule:<name>`,
/// `op-log-author-classes`). The class-prefix query makes `auto:rule:%`
/// the all-firings filter; the generation check skips changes whose newest
/// accepted frame starts with this. [rule-attribution] [rule-no-cascade]
pub const RULE_AUTHOR_PREFIX: &str = "auto:rule:";

/// Producer surface stamped on every staged firing.
pub const SURFACE: &str = "rules";

/// The reserved `script` verb name — strict-load rejects it with an error
/// naming it *reserved* (not unknown), so the slot is visibly held.
/// [rule-script-slot-reserved]
pub const RESERVED_SCRIPT_VERB: &str = "script";

/// `meta(key, value)` watermark key for one rule's date sweep.
fn sweep_watermark_key(rule: &str) -> String {
    format!("rules.sweep.{rule}")
}

// ---------------------------------------------------------------------------
// Compiled rule shapes.
// ---------------------------------------------------------------------------

/// One of the four closed triggers. The three event triggers ride the
/// ingest pipeline's derived-table updates; `date-passed` is the lazy
/// sweep over the `note_meta.num` epoch mirror. [rule-triggers]
///
/// status: rule-triggers
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    NoteCreated,
    FrontmatterChanged,
    CardMoved,
    DatePassed { key: String },
}

impl Trigger {
    /// Display label for the rules panel.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::NoteCreated => "note-created".to_string(),
            Self::FrontmatterChanged => "frontmatter-changed".to_string(),
            Self::CardMoved => "card-moved".to_string(),
            Self::DatePassed { key } => format!("date-passed ({key})"),
        }
    }
}

/// The optional condition: a query-doc reference or an inline filter in
/// the same closed grammar — rules add no second condition language.
/// [rule-condition-reuses-queries]
///
/// status: rule-condition-reuses-queries
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// A saved query-doc path (`query-doc-shape`); read at firing time, so
    /// a missing or non-query doc is a loud per-firing error. The parse is
    /// memoized on the source bytes ([`Engine::parsed_query_doc`]) — a
    /// date sweep firing one rule over many crossings parses the doc once.
    QueryDoc(String),
    /// An inline filter, parsed at load time through the one grammar.
    Filter(queries::Query),
}

/// One closed action verb. Each routes through an op path that already
/// exists — rules decide *when*, existing ops decide *how*.
/// [rule-closed-verbs]
///
/// status: rule-closed-verbs
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Merge `key: value` into the triggering note's frontmatter through
    /// the attributed staged-content path (no shared primitive existed to
    /// ride — built over `op_writes::stage_auto_content_batch`).
    SetField { key: String, value: serde_json::Value },
    /// Move the triggering note's card to `column` via the shared board
    /// mutation step (`boards::ops::apply_edit`). `board` defaults to the
    /// note's one sprint-kind board via the derived-status read.
    MoveCard { column: String, board: Option<String> },
    /// Add the triggering note as a card — the `boards::ops::add_card`
    /// shape, so the single-sprint guard refuses exactly as it would a
    /// user. `column` defaults to the board's first column.
    AddToBoard { board: String, column: Option<String> },
    /// Mint a new note at `path` (collision-suffixed like promote), seeded
    /// from the registered `kind`'s template when given.
    CreateNote { path: String, kind: Option<String> },
}

impl Action {
    /// The verb's wire name, for error messages and the panel.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::SetField { .. } => "set_field",
            Self::MoveCard { .. } => "move_card",
            Self::AddToBoard { .. } => "add_to_board",
            Self::CreateNote { .. } => "create_note",
        }
    }
}

/// A compiled rule. Holding one means the `[rules.<name>]` entry passed
/// strict-load validation. Disabled entries stay in the set (the panel
/// lists them with their enabled state) but never fire. [rule-shape]
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub condition: Option<Condition>,
    pub actions: Vec<Action>,
}

/// Strict-load failure naming the offending `[rules.<name>]` entry — the
/// kind-registry error posture.
#[derive(Debug, thiserror::Error)]
#[error("[rules.{rule}]: {detail}")]
pub struct Error {
    pub rule: String,
    pub detail: String,
}

fn entry_err(rule: &str, detail: impl Into<String>) -> Error {
    Error { rule: rule.to_string(), detail: detail.into() }
}

/// The compiled rule set: every `[rules.<name>]` entry, validated.
/// Constructed once at config load (`validate_cross_field`) and again at
/// vault open for the live engine.
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: BTreeMap<String, Rule>,
}

impl RuleSet {
    /// Compile the raw `[rules]` config table (entry name -> TOML value)
    /// into a validated set. Any invalid entry — an unknown trigger, a
    /// condition outside the queries grammar, an unknown verb, a malformed
    /// board/kind reference, the reserved `script` verb — is a loud
    /// [`Error`] naming the offender. `kinds` backs the `create_note`
    /// kind-reference check.
    ///
    /// status: rule-shape
    pub fn compile(
        entries: &BTreeMap<String, toml::Value>,
        kinds: &Registry,
    ) -> Result<Self, Error> {
        let mut rules = BTreeMap::new();
        for (name, value) in entries {
            rules.insert(name.clone(), compile_entry(name, value, kinds)?);
        }
        Ok(Self { rules })
    }

    /// The rule named `name`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Rule> {
        self.rules.get(name)
    }

    /// Every registered rule, in name order (disabled ones included).
    pub fn iter(&self) -> impl Iterator<Item = &Rule> {
        self.rules.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }
}

// ---------------------------------------------------------------------------
// TOML compile — strict serde shells plus manual `on` / `when` / `do`
// walks so every failure names the entry and the offending clause.
// ---------------------------------------------------------------------------

const fn enabled_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDefToml {
    #[serde(default = "enabled_default")]
    enabled: bool,
    on: toml::Value,
    #[serde(default)]
    when: Option<toml::Value>,
    #[serde(rename = "do", default)]
    actions: Vec<toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetFieldToml {
    key: String,
    value: toml::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveCardToml {
    column: String,
    #[serde(default)]
    board: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddToBoardToml {
    board: String,
    #[serde(default)]
    column: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateNoteToml {
    path: String,
    #[serde(default)]
    kind: Option<String>,
}

fn compile_entry(name: &str, value: &toml::Value, kinds: &Registry) -> Result<Rule, Error> {
    let def: RuleDefToml = value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| entry_err(name, e.to_string()))?;
    let trigger = compile_trigger(name, &def.on)?;
    let condition = match &def.when {
        Some(when) => Some(compile_condition(name, when)?),
        None => None,
    };
    if def.actions.is_empty() {
        return Err(entry_err(name, "`do` must list at least one action"));
    }
    let mut actions = Vec::with_capacity(def.actions.len());
    for action in &def.actions {
        actions.push(compile_action(name, action, kinds)?);
    }
    Ok(Rule {
        name: name.to_string(),
        enabled: def.enabled,
        trigger,
        condition,
        actions,
    })
}

/// `on`: one of the three event triggers as a plain string, or the
/// `{ trigger = "date-passed", key = "<key>" }` table form. Anything
/// else — including `"date-passed"` as a bare string — is a strict-load
/// error naming the rule. [rule-triggers]
fn compile_trigger(name: &str, on: &toml::Value) -> Result<Trigger, Error> {
    match on {
        toml::Value::String(s) => match s.as_str() {
            "note-created" => Ok(Trigger::NoteCreated),
            "frontmatter-changed" => Ok(Trigger::FrontmatterChanged),
            "card-moved" => Ok(Trigger::CardMoved),
            "date-passed" => Err(entry_err(
                name,
                "`date-passed` takes the table form \
                 { trigger = \"date-passed\", key = \"<date key>\" }",
            )),
            other => Err(entry_err(
                name,
                format!(
                    "unknown trigger `{other}` (closed set: note-created / \
                     frontmatter-changed / card-moved / date-passed)"
                ),
            )),
        },
        toml::Value::Table(t) => {
            let trigger = t.get("trigger").and_then(toml::Value::as_str);
            let key = t.get("key").and_then(toml::Value::as_str);
            if t.len() != 2 || trigger != Some("date-passed") {
                return Err(entry_err(
                    name,
                    "the table trigger form is exactly \
                     { trigger = \"date-passed\", key = \"<date key>\" }",
                ));
            }
            match key {
                Some(k) if !k.is_empty() => Ok(Trigger::DatePassed { key: k.to_string() }),
                _ => Err(entry_err(name, "`date-passed` requires a non-empty `key`")),
            }
        }
        other => Err(entry_err(name, format!("`on` must be a trigger string or table, got {other:?}"))),
    }
}

/// `when`: exactly one of `query_doc` (a `.md` query-doc path) or `filter`
/// (inline, compiled through the one grammar at load time).
/// [rule-condition-reuses-queries]
fn compile_condition(name: &str, when: &toml::Value) -> Result<Condition, Error> {
    let toml::Value::Table(t) = when else {
        return Err(entry_err(name, "`when` must be a table with exactly one of query_doc / filter"));
    };
    match (t.get("query_doc"), t.get("filter"), t.len()) {
        (Some(doc), None, 1) => {
            let rel = doc.as_str().ok_or_else(|| {
                entry_err(name, "`when.query_doc` must be a vault-relative path string")
            })?;
            if !rel.ends_with(".md") {
                return Err(entry_err(name, format!("`when.query_doc` must name a `.md` query-doc, got `{rel}`")));
            }
            Ok(Condition::QueryDoc(rel.to_string()))
        }
        (None, Some(filter), 1) => queries::parse_filter_toml(filter)
            .map(Condition::Filter)
            .map_err(|e| entry_err(name, format!("`when.filter`: {e}"))),
        _ => Err(entry_err(name, "`when` takes exactly one of query_doc / filter")),
    }
}

/// One `do` entry: a table with exactly one key — the verb. The verb set
/// is closed; `script` is reserved (not unknown) per the deferred slot.
/// [rule-closed-verbs] [rule-script-slot-reserved]
fn compile_action(name: &str, action: &toml::Value, kinds: &Registry) -> Result<Action, Error> {
    let toml::Value::Table(t) = action else {
        return Err(entry_err(name, "each `do` entry must be a one-verb table"));
    };
    if t.len() != 1 {
        return Err(entry_err(name, "each `do` entry takes exactly one verb"));
    }
    let (verb, params) = t.iter().next().expect("len checked above");
    let de = |detail: toml::de::Error| entry_err(name, format!("`{verb}`: {detail}"));
    match verb.as_str() {
        "set_field" => {
            let p: SetFieldToml = params.clone().try_into().map_err(de)?;
            let value = scalar_json(&p.value).ok_or_else(|| {
                entry_err(name, "`set_field.value` must be a scalar (string / number / bool / date)")
            })?;
            Ok(Action::SetField { key: p.key, value })
        }
        "move_card" => {
            let p: MoveCardToml = params.clone().try_into().map_err(de)?;
            if let Some(board) = &p.board {
                check_board_ref(name, "move_card.board", board)?;
            }
            Ok(Action::MoveCard { column: p.column, board: p.board })
        }
        "add_to_board" => {
            let p: AddToBoardToml = params.clone().try_into().map_err(de)?;
            check_board_ref(name, "add_to_board.board", &p.board)?;
            Ok(Action::AddToBoard { board: p.board, column: p.column })
        }
        "create_note" => {
            let p: CreateNoteToml = params.clone().try_into().map_err(de)?;
            check_board_ref(name, "create_note.path", &p.path)?;
            if let Some(kind) = &p.kind
                && kinds.get(kind).is_none()
            {
                return Err(entry_err(
                    name,
                    format!("`create_note.kind`: `{kind}` is not a registered kind"),
                ));
            }
            Ok(Action::CreateNote { path: p.path, kind: p.kind })
        }
        // status: rule-script-slot-reserved
        RESERVED_SCRIPT_VERB => Err(entry_err(
            name,
            "`script` is a reserved action verb (the sandboxed scripting \
             slot, deferred) — v1 ships no implementation",
        )),
        other => Err(entry_err(
            name,
            format!(
                "unknown action verb `{other}` (closed set: set_field / \
                 move_card / add_to_board / create_note)"
            ),
        )),
    }
}

/// A board / note path reference must be a vault-relative `.md` path.
fn check_board_ref(name: &str, field: &str, rel: &str) -> Result<(), Error> {
    if rel.ends_with(".md") && !rel.starts_with('/') {
        Ok(())
    } else {
        Err(entry_err(name, format!("`{field}` must be a vault-relative `.md` path, got `{rel}`")))
    }
}

/// A TOML scalar as the JSON value the frontmatter merge takes; `None` for
/// arrays and tables (outside the `set_field` value set).
fn scalar_json(v: &toml::Value) -> Option<serde_json::Value> {
    match v {
        toml::Value::String(s) => Some(serde_json::Value::String(s.clone())),
        toml::Value::Integer(n) => Some(serde_json::json!(n)),
        toml::Value::Float(f) => Some(serde_json::json!(f)),
        toml::Value::Boolean(b) => Some(serde_json::Value::Bool(*b)),
        toml::Value::Datetime(d) => Some(serde_json::Value::String(d.to_string())),
        toml::Value::Array(_) | toml::Value::Table(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Trigger events + the pure detection diffs the ingest pipeline calls.
// ---------------------------------------------------------------------------

/// One post-index trigger event, detected at the ingest seams. The
/// triggering note (what conditions and actions bind to) is the note for
/// the first two and the moved card's note for `CardMoved`; the *changed
/// path* (what the no-cascade generation check reads) is the note for the
/// first two and the board-doc for `CardMoved`. [rule-triggers]
///
/// status: rule-triggers
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleEvent {
    NoteCreated { path: String },
    FrontmatterChanged { path: String },
    CardMoved {
        board_path: String,
        note_path: String,
        from_column: String,
        to_column: String,
    },
}

impl RuleEvent {
    /// The path whose newest accepted frame gates the generation check.
    #[must_use]
    pub fn changed_path(&self) -> &str {
        match self {
            Self::NoteCreated { path } | Self::FrontmatterChanged { path } => path,
            Self::CardMoved { board_path, .. } => board_path,
        }
    }

    /// The triggering note conditions and actions bind to.
    #[must_use]
    pub fn note_path(&self) -> &str {
        match self {
            Self::NoteCreated { path } | Self::FrontmatterChanged { path } => path,
            Self::CardMoved { note_path, .. } => note_path,
        }
    }

    /// Whether this event fires rules carrying `trigger`.
    #[must_use]
    pub const fn fires(&self, trigger: &Trigger) -> bool {
        matches!(
            (self, trigger),
            (Self::NoteCreated { .. }, Trigger::NoteCreated)
                | (Self::FrontmatterChanged { .. }, Trigger::FrontmatterChanged)
                | (Self::CardMoved { .. }, Trigger::CardMoved)
        )
    }
}

/// Whether a note's indexed metadata changed across a
/// `replace_note_metadata` — the `frontmatter-changed` detection diff,
/// compared as (key, value) multisets (the `num` mirror is derived from
/// `value`, so it never disagrees). [rule-triggers]
///
/// status: rule-triggers
#[must_use]
pub fn meta_changed(before: &[MetaEntry], after: &[MetaEntry]) -> bool {
    let pairs = |entries: &[MetaEntry]| {
        let mut v: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();
        v.sort();
        v
    };
    pairs(before) != pairs(after)
}

/// The `card-moved` detection diff across an
/// `update_board_cards_if_relevant` replace: one event per note card
/// present in both row sets whose column changed. Newly added and removed
/// cards are not moves. [rule-triggers]
///
/// status: rule-triggers
#[must_use]
pub fn card_moves(
    board_path: &str,
    before: &[BoardCardRow],
    after: &[BoardCardRow],
) -> Vec<RuleEvent> {
    let columns = |rows: &[BoardCardRow]| {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for row in rows {
            map.entry(row.card_note_path.clone())
                .or_insert_with(|| row.column_name.clone());
        }
        map
    };
    let prior = columns(before);
    let current = columns(after);
    let mut out = Vec::new();
    for (note, from_column) in &prior {
        if let Some(to_column) = current.get(note)
            && to_column != from_column
        {
            out.push(RuleEvent::CardMoved {
                board_path: board_path.to_string(),
                note_path: note.clone(),
                from_column: from_column.clone(),
                to_column: to_column.clone(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The live engine.
// ---------------------------------------------------------------------------

/// One failed firing, kept in the engine's in-memory diagnostics ring
/// (the `obs-log-ring-buffer` posture: a failed firing writes no frame, so
/// the panel renders it from here). [rule-firings-panel]
#[derive(Debug, Clone)]
pub struct FiringFailure {
    pub rule: String,
    pub note_path: String,
    pub message: String,
    pub at_ms: i64,
}

const FAILURE_RING_CAP: usize = 200;

/// Borrow-bundle of the handles a rule pass reads and writes through.
/// Assembled by the indexer at the trigger seams — the single-writer
/// discipline every vault mutation already obeys.
pub struct FireCtx<'a> {
    pub vault: &'a Vault,
    pub store: &'a Store,
    pub log: &'a LayeredDoc,
    pub kinds: &'a Registry,
}

/// One memoized query-doc parse: the source bytes it was parsed from plus
/// the compiled query. Keyed by the doc's vault-relative path in
/// [`Engine::query_doc_cache`].
struct CachedQueryDoc {
    src: String,
    query: queries::Query,
}

/// The live rules engine: the compiled set, the review-mode flag, and the
/// failed-firings ring. Owned by the host (`Arc`), attached to the indexer
/// beside the kind registry, read by the rules panel.
pub struct Engine {
    rules: RuleSet,
    review_required: bool,
    failures: Mutex<VecDeque<FiringFailure>>,
    /// Memoized query-doc parses for `Condition::QueryDoc` rules, keyed by
    /// doc path and validated against the source bytes on every firing —
    /// the per-firing READ stays (loud missing-doc errors, mid-session
    /// edits take effect), only the re-parse of unchanged bytes is skipped.
    query_doc_cache: Mutex<BTreeMap<String, CachedQueryDoc>>,
}

impl Engine {
    #[must_use]
    pub const fn new(rules: RuleSet, review_required: bool) -> Self {
        Self {
            rules,
            review_required,
            failures: Mutex::new(VecDeque::new()),
            query_doc_cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Every registered rule, in name order (the panel listing).
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Snapshot of the failed-firings ring, newest first.
    /// [rule-firings-panel]
    #[must_use]
    pub fn failures(&self) -> Vec<FiringFailure> {
        match self.failures.lock() {
            Ok(ring) => ring.iter().rev().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    fn record_failure(&self, rule: &str, note_path: &str, message: &str) {
        tracing::warn!(rule, note = note_path, error = message, "rules: firing failed");
        let Ok(mut ring) = self.failures.lock() else { return };
        if ring.len() >= FAILURE_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(FiringFailure {
            rule: rule.to_string(),
            note_path: note_path.to_string(),
            message: message.to_string(),
            at_ms: now_ms(),
        });
    }

    /// Run the rule pass for a batch of post-index trigger events. Skips
    /// any event whose changed path's newest accepted frame is rule-
    /// authored (`auto:rule:%` — one generation per external event,
    /// [rule-no-cascade]). Returns the vault-relative paths whose staged
    /// writes were applied (review off), for the caller to re-index.
    ///
    /// status: rule-no-cascade
    /// status: rule-triggers
    pub fn on_events(&self, ctx: &FireCtx<'_>, events: &[RuleEvent]) -> Vec<String> {
        let mut reindex = Vec::new();
        if self.rules.is_empty() {
            return reindex;
        }
        for event in events {
            if newest_frame_is_rule_authored(ctx.log, event.changed_path()) {
                continue;
            }
            for rule in self.rules.iter().filter(|r| r.enabled && event.fires(&r.trigger)) {
                self.evaluate_and_fire(ctx, rule, event.note_path(), &mut reindex);
            }
        }
        reindex
    }

    /// The `date-passed` sweep — at vault open and on the daily tick — over
    /// the `note_meta.num` epoch mirror. Per rule, reads the crossings in
    /// (`watermark`, `now_epoch`], advances the rule's watermark row in the
    /// store's `meta(key, value)` sidecar, and only THEN fires — a
    /// watermark that fails to persist produces zero firings (failure in
    /// the ring, the whole crossing deferred to the next sweep), because
    /// firing first would let the next sweep re-walk the same crossing and
    /// double-fire (`create_note` would mint duplicate notes). The
    /// fail-safe direction is a lost firing, never a duplicate one. A
    /// rule's first sweep only records the watermark — a freshly added
    /// rule never fires for already-past dates. [rule-triggers]
    ///
    /// status: rule-triggers
    pub fn date_sweep(&self, ctx: &FireCtx<'_>, now_epoch: f64) -> Vec<String> {
        let mut reindex = Vec::new();
        for rule in self.rules.iter().filter(|r| r.enabled) {
            let Trigger::DatePassed { key } = &rule.trigger else { continue };
            let wm_key = sweep_watermark_key(&rule.name);
            let watermark = ctx
                .store
                .meta_kv_get(&wm_key)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok());
            // Read the crossings against the OLD watermark first (the new
            // one overwrites it), but persist the new watermark before any
            // firing — see the doc comment for why this order is load-
            // bearing.
            let crossings = match watermark {
                // First sweep: record the watermark only, fire nothing.
                None => Vec::new(),
                Some(wm) if now_epoch <= wm => continue,
                Some(wm) => {
                    match ctx.store.note_paths_with_meta_num_between(key, wm, now_epoch) {
                        Ok(paths) => paths,
                        Err(e) => {
                            self.record_failure(
                                &rule.name,
                                "",
                                &format!("date sweep query: {e}"),
                            );
                            continue;
                        }
                    }
                }
            };
            if let Err(e) = ctx.store.meta_kv_set(&wm_key, &now_epoch.to_string()) {
                self.record_failure(&rule.name, "", &format!("sweep watermark write: {e}"));
                continue;
            }
            for path in crossings {
                self.evaluate_and_fire(ctx, rule, &path, &mut reindex);
            }
        }
        reindex
    }

    /// Condition gate, then the firing. Failures land in the ring.
    fn evaluate_and_fire(
        &self,
        ctx: &FireCtx<'_>,
        rule: &Rule,
        note_path: &str,
        reindex: &mut Vec<String>,
    ) {
        match self.condition_matches(ctx, rule, note_path) {
            Ok(false) => {}
            Ok(true) => reindex.extend(self.fire(ctx, rule, note_path)),
            Err(msg) => self.record_failure(&rule.name, note_path, &format!("condition: {msg}")),
        }
    }

    /// Evaluate a rule's condition against the triggering note — one
    /// indexed per-note membership check ([`queries::matches_note`]), never
    /// a full query run. A `query_doc` is re-read every firing (a missing
    /// or non-query doc stays the loud per-firing error rules.md requires);
    /// its parse is memoized on the source bytes via
    /// [`Self::parsed_query_doc`]. [rule-condition-reuses-queries]
    ///
    /// status: rule-condition-reuses-queries
    fn condition_matches(
        &self,
        ctx: &FireCtx<'_>,
        rule: &Rule,
        note_path: &str,
    ) -> Result<bool, String> {
        let Some(condition) = &rule.condition else {
            return Ok(true);
        };
        let parsed;
        let query = match condition {
            Condition::Filter(q) => q,
            Condition::QueryDoc(rel) => {
                let src = ctx
                    .vault
                    .read_file(rel)
                    .map_err(|e| format!("query_doc `{rel}`: {e}"))?;
                parsed = self.parsed_query_doc(rel, &src)?;
                &parsed
            }
        };
        queries::matches_note(ctx.store, ctx.kinds, query, note_path).map_err(|e| e.to_string())
    }

    /// The compiled query for a query-doc's current source, memoized per
    /// path on the exact source bytes — same bytes, same parse, so the hit
    /// path returns a clone and an edited doc re-parses on its next firing.
    /// Parse failures are never cached (they stay loud per firing), and a
    /// poisoned cache lock degrades to parsing directly.
    fn parsed_query_doc(&self, rel: &str, src: &str) -> Result<queries::Query, String> {
        if let Ok(cache) = self.query_doc_cache.lock()
            && let Some(hit) = cache.get(rel)
            && hit.src == src
        {
            return Ok(hit.query.clone());
        }
        let query = queries::parse_query_doc_for(rel, src)
            .map_err(|e| format!("query_doc `{rel}`: {e}"))?;
        if let Ok(mut cache) = self.query_doc_cache.lock() {
            cache.insert(
                rel.to_string(),
                CachedQueryDoc { src: src.to_string(), query: query.clone() },
            );
        }
        Ok(query)
    }

    /// One firing: apply the actions in order into an in-firing overlay
    /// (later actions read earlier actions' output), then stage the
    /// computed texts as ONE layered-doc batch authored `auto:rule:<name>`. A
    /// failed action aborts the remaining actions; the computed prefix
    /// still stages (already-applied ones stand — no cross-document
    /// rollback). Returns the applied paths (empty when staged pending
    /// under review mode, or nothing changed). [rule-attribution]
    ///
    /// status: rule-attribution
    /// status: rule-closed-verbs
    fn fire(&self, ctx: &FireCtx<'_>, rule: &Rule, note_path: &str) -> Vec<String> {
        let mut draft = Draft::default();
        for (idx, action) in rule.actions.iter().enumerate() {
            if let Err(msg) = apply_action(ctx, note_path, action, &mut draft) {
                self.record_failure(
                    &rule.name,
                    note_path,
                    &format!("{} (action {}): {msg}", action.verb(), idx + 1),
                );
                break;
            }
        }
        match self.stage_firing(ctx, rule, &draft) {
            Ok(applied) => applied,
            Err(msg) => {
                self.record_failure(&rule.name, note_path, &format!("stage: {msg}"));
                Vec::new()
            }
        }
    }

    /// Stage the firing's computed texts as one cross-document batch (the
    /// sprint-close shape) authored `auto:rule:<name>`, auto-flipping the
    /// batch when review mode is off (the `suggest.rs` triage precedent).
    /// Under review the batch stays pending — the batch review row
    /// presents it as one unit. The auto-flip rides the checked seam, so
    /// the one-sprint invariant re-verifies against accepted state at the
    /// moment of apply (`derived-status-rule`). [rule-attribution]
    fn stage_firing(
        &self,
        ctx: &FireCtx<'_>,
        rule: &Rule,
        draft: &Draft,
    ) -> Result<Vec<String>, String> {
        let items = draft.stages();
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let producer = format!("rule:{}", rule.name);
        for item in &items {
            ensure_doc_rule_authored(ctx, &item.rel, &producer)?;
        }
        let outcome =
            op_writes::stage_auto_content_batch(ctx.log, ctx.vault, &producer, SURFACE, &items)
                .map_err(|e| e.to_string())?;
        if outcome.op_ids.is_empty() || self.review_required {
            return Ok(Vec::new());
        }
        let flip_ctx = op_writes::FlipCtx {
            vault: ctx.vault,
            store: ctx.store,
            kinds: ctx.kinds,
        };
        op_writes::flip_batch_status_checked(ctx.log, &flip_ctx, &outcome.batch_id, true)
            .map_err(|e| e.to_string())?;
        Ok(draft.paths().to_vec())
    }
}

/// Epoch milliseconds for the failure ring timestamps.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The no-cascade generation check. Historically this read `rel`'s newest
/// accepted op-log frame and skipped the pass when it was rule-authored
/// (`auto:rule:%`). With the op-log history engine + per-write attribution
/// retired (the core rework), there is no frame author to read, so this is
/// now a no-op (`false` = "not rule-authored, let the pass run").
///
/// Cascade protection survives structurally instead of via authorship: rule
/// actions are idempotent (setting a field already at its target value
/// produces no content change), and an unchanged write produces no new
/// snapshot and is dropped by the watcher's self-write suppression — so a
/// rule cannot drive itself in an infinite loop. A rule whose action *does*
/// change content on every pass would re-fire, but that is a mis-authored
/// rule, not a substrate concern.
///
/// TODO(K3c): if a future need arises, re-home a bounded one-generation
/// guard onto an in-memory per-path marker on the engine; it must not read
/// the op-log history engine.
///
/// status: rule-no-cascade
const fn newest_frame_is_rule_authored(_log: &LayeredDoc, _rel: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Action application — each verb computes a whole new document text into
// the firing's overlay (`op_writes::Draft`); the existing op machinery
// does the writing. The board verbs ARE the real board ops, run in their
// `AutoStaged` mode (`boards::ops::BoardWriteMode`).
// ---------------------------------------------------------------------------

/// [`Draft::read`] with the rules layer's error shape, naming the path.
fn draft_read(draft: &Draft, vault: &Vault, rel: &str) -> Result<String, String> {
    draft.read(vault, rel).map_err(|e| format!("read `{rel}`: {e}"))
}

fn apply_action(
    ctx: &FireCtx<'_>,
    note_path: &str,
    action: &Action,
    draft: &mut Draft,
) -> Result<(), String> {
    match action {
        Action::SetField { key, value } => apply_set_field(ctx, note_path, key, value, draft),
        Action::MoveCard { column, board } => {
            apply_move_card(ctx, note_path, column, board.as_deref(), draft)
        }
        Action::AddToBoard { board, column } => {
            apply_add_to_board(ctx, note_path, board, column.as_deref(), draft)
        }
        Action::CreateNote { path, kind } => {
            apply_create_note(ctx, path, kind.as_deref(), draft)
        }
    }
}

/// `set_field`: merge `key: value` into the triggering note's top-level
/// frontmatter. The attributed write path is BUILT here over the staged-
/// content seam — there was no shared frontmatter-merge primitive to ride
/// (inbox rules' `add_tag` rides the same staged seam, authored
/// `auto:inbox`). [rule-closed-verbs]
fn apply_set_field(
    ctx: &FireCtx<'_>,
    note_path: &str,
    key: &str,
    value: &serde_json::Value,
    draft: &mut Draft,
) -> Result<(), String> {
    let text = draft_read(draft, ctx.vault, note_path)?;
    let view = crate::frontmatter::split(&text);
    let mut fm = view
        .frontmatter
        .unwrap_or_else(|| serde_yml::Value::Mapping(serde_yml::Mapping::default()));
    if !matches!(fm, serde_yml::Value::Mapping(_)) {
        return Err("frontmatter is not a mapping".to_string());
    }
    let mut patch = serde_json::Map::new();
    patch.insert(key.to_string(), value.clone());
    crate::frontmatter::merge_json_into_yaml(&mut fm, serde_json::Value::Object(patch));
    let new_text = crate::frontmatter::assemble(&fm, view.body)
        .map_err(|e| format!("assemble frontmatter: {e}"))?;
    if new_text != text {
        draft.put(note_path, new_text);
    }
    Ok(())
}

/// `move_card`: the REAL board op (`boards::ops::move_card_to_column`) in
/// its `AutoStaged` mode — the moved board-doc text lands in the firing's
/// draft instead of committing per-op. `board` defaults to the note's one
/// sprint-kind board via the derived-status read. [rule-closed-verbs]
fn apply_move_card(
    ctx: &FireCtx<'_>,
    note_path: &str,
    column: &str,
    board: Option<&str>,
    draft: &mut Draft,
) -> Result<(), String> {
    let board_rel = match board {
        Some(rel) => rel.to_string(),
        None => default_sprint_board(ctx, note_path)?,
    };
    let mut mode = boards::ops::BoardWriteMode::AutoStaged { draft };
    boards::ops::move_card_to_column(&mut mode, ctx.vault, Some(ctx.kinds), &board_rel, note_path, column)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// `move_card`'s unset-`board` default: the note's one sprint-kind board
/// via the derived-status read (`derived-status-rule`).
fn default_sprint_board(ctx: &FireCtx<'_>, note_path: &str) -> Result<String, String> {
    match crate::pm::derived_status(ctx.store, ctx.kinds, note_path)
        .map_err(|e| e.to_string())?
    {
        crate::pm::DerivedStatus::Active { sprint_path, .. } => Ok(sprint_path),
        crate::pm::DerivedStatus::None => Err(
            "no `board` given and the note is a card on no sprint-kind board".to_string(),
        ),
        crate::pm::DerivedStatus::Conflicted { sprint_paths } => Err(format!(
            "no `board` given and the note sits on multiple sprints: {}",
            sprint_paths.join(", "),
        )),
    }
}

/// `add_to_board`: the REAL board op (`boards::ops::add_note_card`) in
/// its `AutoStaged` mode — idempotent per board, and the single-sprint
/// membership guard refuses exactly as it would a user; `column = None`
/// is the op's first-column default. [rule-closed-verbs]
fn apply_add_to_board(
    ctx: &FireCtx<'_>,
    note_path: &str,
    board_rel: &str,
    column: Option<&str>,
    draft: &mut Draft,
) -> Result<(), String> {
    let mut mode = boards::ops::BoardWriteMode::AutoStaged { draft };
    boards::ops::add_note_card(
        &mut mode,
        ctx.vault,
        ctx.store,
        Some(ctx.kinds),
        board_rel,
        column,
        note_path,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// `create_note`: mint a new note at the first free suffix of `path` (the
/// promote collision rule), seeded from the kind's template — `hiker.kind`
/// set, the kind's fields seeded empty (the `freeform-promote-note`
/// seeding, shared via `kinds::template_note_body`). [rule-closed-verbs]
fn apply_create_note(
    ctx: &FireCtx<'_>,
    path: &str,
    kind: Option<&str>,
    draft: &mut Draft,
) -> Result<(), String> {
    let stem_path = path.strip_suffix(".md").unwrap_or(path);
    let (folder, stem) = match stem_path.rsplit_once('/') {
        Some((folder, stem)) => (folder, stem),
        None => ("", stem_path),
    };
    let rel = crate::vault::next_free_md_path(ctx.vault, folder, stem)
        .map_err(|e| e.to_string())?;
    if draft.contains(&rel) {
        return Err(format!("`{rel}` was already created by this firing"));
    }
    let template = match kind {
        Some(name) => Some(
            ctx.kinds
                .get(name)
                .ok_or_else(|| format!("kind `{name}` is not registered"))?,
        ),
        None => None,
    };
    let body = crate::kinds::template_note_body("", template).map_err(|e| e.to_string())?;
    draft.put(&rel, body);
    Ok(())
}

/// Ensure `rel` has a layered-doc document before its firing stages, seeding a
/// brand-new one authored `auto:rule:<producer>` — so a `create_note`'s
/// created note carries the rule author. Existing docs are left untouched.
/// [rule-no-cascade]
fn ensure_doc_rule_authored(
    ctx: &FireCtx<'_>,
    rel: &str,
    producer: &str,
) -> Result<(), String> {
    let mapped = ctx
        .log
        .doc_id_for_path(rel)
        .map_err(|e| e.to_string())?
        .is_some();
    if mapped {
        return Ok(());
    }
    let seed = ctx.vault.read_file(rel).unwrap_or_default();
    ctx.log
        .create_document(rel, "markdown", &seed, &Author::Auto(producer.to_string()))
        .map_err(|e| format!("seed op-log doc `{rel}`: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
