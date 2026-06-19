# Kinds

User-definable note kinds, declared as data in a vault-level registry: each kind names its typed fields, an optional state set, and a shape, and the rest of the system — queries, boards, validation badges, generated MCP tools — is written against those declarations. The registry is the schema half of a larger arc: the query layer (`queries.md`) shipped first, the PM semantics that consume these kinds (derived status, sprints, rollups) land in a later spec over the same entries.

The headline decisions:

- The registry lives in vault config — a `[kinds]` table in `vault/.hiker/config.toml`, strict-loaded like every other config section. The registry itself is strict; notes validated *against* it are lenient. Kind-definition notes were considered and rejected for v1. [kind-registry]
- Every user-defined vocabulary maps onto a small closed anchor set the system is written against: field types onto five primitives [kind-field-primitives], state names onto five categories [kind-state-categories], kinds onto three shapes [kind-shapes]. User vocabulary is presentation; anchors are semantics.
- The column→state mapping lives on the **board-like kind definition**, not on individual boards and not on a plan — every board of a kind means the same thing, and a plan only picks defaults. [kind-column-state-map]
- Validation of notes is lenient: a note violating its kind's schema gets a badge and a problems report; writes are never blocked, data never dropped, files never rewritten. Markdown-first. [kind-lenient-validation]
- Built-in entries — `story`/`task`, `epic`, `sprint`, `plan` — ship as registry entries written in the same format users write, proving the format before exposing it. [kind-builtin-pm-set]
- Each kind definition generates its own typed MCP surface (`create_<kind>` / `update_<kind>`), so a new kind ships with agent tools automatically — the structural fix for tools lagging features. [mcp-registry-tools]


## The registry

A kind is declared as data: a named `[kinds.<name>]` entry carrying `shape`, a `fields` list, an optional `states` list, and (board-like kinds only) a `columns` mapping. Storage is the vault config TOML — registry changes are rare, config-shaped, and want a load-time validation gate, the same reasoning that put `[inbox].rules` there ([[spec:inbox-rules]]). The registry section is **strict-load** per [[spec:settings-strict-load]]: an unknown key, a type outside the primitive set, a state without a category, a column mapped to an undeclared state, or a kind name colliding with a machinery discriminator (`board`, `query`, `cluster-tree`, …) aborts startup with an error naming the offending entry — the same cross-field hook that validates inbox rules at load (`core/src/config/mod.rs::validate_cross_field`). The strict/lenient split is deliberate: the registry is config (rare edits, one load point, actionable failure); notes are data (constant edits, never blocked — see Lenient validation below). [kind-registry]
status:: done
implements:: [[code:hiker/kinds/impl#[Registry]compile]], [[code:hiker/config/validate_cross_field]]
verifies:: [[code:hiker/kinds/tests/unknown_key_names_the_offending_entry]], [[code:hiker/kinds/tests/machinery_discriminator_collision_is_an_error]], [[code:hiker/config/tests/kinds_invalid_entry_fails_cross_field_naming_offender]]
note:: `[kinds.<name>]` entries kept as raw TOML values on `Config.kinds` so `kinds::Registry::compile` produces errors naming the offending entry (a typed serde field loses entry context in the merged-Value deserialize); compiled + validated in `validate_cross_field` (the `InboxConfig` precedent), recompiled at vault open (`app/src/bootstrap.rs::attach_config_engines`) and shared with the indexer, smart folders, and the MCP server. Machinery-discriminator collision list is `kinds::MACHINERY_DISCRIMINATORS` · evidence: `core/src/kinds.rs` (`Registry::compile`), `core/src/config/mod.rs::validate_cross_field`; tests `core/src/kinds/tests.rs` (offender-naming errors), `core/src/config/tests.rs::kinds_invalid_entry_fails_cross_field_naming_offender`

Tradeoff — registry-as-notes rejected: declaring kinds as `hiker.kind: kind-definition` notes (the [[spec:subsystem-notes-visible]] pattern boards and queries use) was considered and rejected for v1. The strict registry needs an unambiguous validation gate, and a note-based registry would make schema validity depend on indexer state and mid-session edits — including validating the kind of the very notes that define kinds. Config gives one load point and one failure mode, with the inbox-rules precedent. Revisit only if live registry editing becomes a real workflow.

```toml
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

[kinds.sprint.columns]   # column name -> state name (see Column-state mapping)
"Todo"   = "Todo"
"Doing"  = "Doing"
"Review" = "Review"
"Done"   = "Done"
```

A note opts into a kind the way it opts into being a board or a query: its `hiker.kind` frontmatter names a registered entry. Unregistered `hiker.kind` values stay inert — the machinery discriminators keep working unchanged, and arbitrary user values are legal frontmatter that simply nothing validates.


## Field primitives

Field types come from a closed set of five primitives; user field names are presentation over them. A kind's field values are plain frontmatter keys on the note — no new storage — indexed through the existing flattened metadata index ([[spec:store-note-metadata-index]]) and therefore queryable by the existing filter grammar ([[spec:query-filter-grammar]]) with no new query machinery. [kind-field-primitives]
status:: done
implements:: [[code:hiker/kinds/compile_fields]], [[code:hiker/kinds/check_value]]
touches:: [[code:hiker/kinds/FieldType]]
verifies:: [[code:hiker/kinds/tests/type_outside_primitive_set_is_an_error]], [[code:hiker/kinds/tests/enum_requires_values_and_values_rejected_elsewhere]], [[code:hiker/kinds/tests/enum_and_ref_field_declarations_compile]], [[code:hiker/kinds/tests/each_primitive_violation_is_reported]]
note:: closed set string / number / date / enum / ref (`kinds::FieldType`); values are plain frontmatter indexed by `note_meta`; enum and ref are validation-time constraints over string storage (`kinds::validate_note` + the generated tools' param schemas) — the store never enforces them. `{ name, type, required?, values?, kind? }` field entries validated at compile (`values` mandatory for enum / rejected elsewhere; `kind` ref-only) · evidence: `core/src/kinds.rs` (`FieldType`, `compile_fields`, `check_value`)

| primitive | frontmatter value | index behavior | generated tool param |
|---|---|---|---|
| `string` | YAML string | `note_meta.value` | string |
| `number` | YAML number | `value` + `num` mirror (range filters, numeric order) | number |
| `date` | ISO-8601 string | `value` verbatim + `num` mirror as epoch seconds (the query grammar's date mirror, `frontmatter::iso_date_epoch`) | string, ISO-8601 |
| `enum` | YAML string from the field's declared `values` list | `value` | enum of the declared values |
| `ref` | vault-relative note path | `value`; resolves by path identity like a board card reference ([[spec:board-card-references]]) | string, vault-relative path |

`enum` and `ref` add no storage shape — on disk both are strings; the constraint (value in the declared list; path resolves, optionally to a note of a declared `kind`) is checked by lenient validation and by the generated tools' param schemas, not by the store. A field entry is `{ name, type, required?, values?, kind? }`: `required` defaults to false, `values` is mandatory for `enum` and rejected elsewhere, `kind` optionally constrains a `ref`'s target kind.


## State sets and category anchors

A kind may declare a state set: an ordered list of user-named states, each carrying a **required `category` anchor** from a closed five-value enum. Names are per-vault vocabulary ("Review", "Blocked", "Shipped"); categories are what automation, UI, and rollups are written against — a rollup counts `done`-category states without knowing any vault's names. A state without a category is a strict-load error; several states may share one category. [kind-state-categories]
status:: done
implements:: [[code:hiker/kinds/compile_states]]
touches:: [[code:hiker/kinds/StateCategory]]
verifies:: [[code:hiker/kinds/tests/state_without_category_is_an_error]]
note:: user-named states each carry a required category from backlog / todo / in_progress / done / canceled (`kinds::StateCategory`); multiple states per category allowed; category missing or unknown = strict-load error naming the entry · evidence: `core/src/kinds.rs` (`StateCategory`, `StateDefToml.category` required); test `kinds::tests::state_without_category_is_an_error`. States are never stored on notes — nothing writes a status field; the derived-status rule stays the PM layer's

| category | meaning |
|---|---|
| `backlog` | captured, not committed |
| `todo` | committed, not started |
| `in_progress` | actively being worked |
| `done` | completed |
| `canceled` | abandoned without completion |

States are never stored on notes. There is no `status:` frontmatter field — a work note's status is *derived* from which mapped column holds it (the mapping below; the full derived-status rule is the PM layer's spec). One source of truth, no reconciliation logic.


## Shapes

Every kind declares one of three shapes — the closed structural anchor that decides which existing doc machinery its notes ride. User kinds add vocabulary and fields on top of a shape; they never add a fourth structure. [kind-shapes]
status:: done
touches:: [[code:hiker/kinds/Shape]]
verifies:: [[code:hiker/kinds/tests/columns_require_board_like_shape_and_states]]
note:: closed set leaf / list-like / board-like (`kinds::Shape`, required per entry); each rides an existing authored-doc pattern (plain note / ordered refs / columns of refs); discriminator + `.md` rule shared with boards, trails, queries. This doc owns the declaration (+ the columns-need-board-like validation); wiring board-like kinds into `boards::parse_board_for` stays the PM layer's `sprint-board-subtype` per the paragraph below · evidence: `core/src/kinds.rs` (`Shape`, `compile_columns` shape check)

| shape | structure | pattern it rides |
|---|---|---|
| `leaf` | fields + freeform body | a plain note |
| `list-like` | ordered `refs` list of `{ path }` entries in frontmatter, body is prose | trail-doc / board-card conventions: path-as-identity, refs rewritten by the shared rename-rewrite pass ([[spec:board-card-references]]) |
| `board-like` | ordered columns of card refs in frontmatter, body is prose | the board-doc ([[spec:board-doc-shape]]): the whole board surface — view, ops, WIP limits, derived `board_cards` table, MCP staging — works unchanged |

The parse gate for list-like and board-like kinds follows the rule boards, trails, and query-docs share: the `hiker.kind` discriminator plus a required `.md` extension; a non-`.md` file carrying the discriminator is a regular note. Wiring a concrete board-like kind into `boards::parse_board_for` (accepting `hiker.kind: sprint` alongside `board`) is the PM layer's work ([[spec:sprint-board-subtype]]); the shape declaration here is what makes that wiring generic rather than sprint-special-cased.


## Column-state mapping

A board-like kind's definition carries the mapping from column names to states in its own state set (`[kinds.<name>.columns]`). Column names on individual boards stay arbitrary per-board strings ([[spec:board-column-model]]); the mapping is what gives a name meaning, and only on boards of that kind. A column whose name appears in the mapping carries that state and its category; an unmapped column is a plain lane with no PM semantics. Many columns may map to one state. Strict-load requires every mapped value to name a state in the kind's state set, and a `columns` table on a kind without states (or on a non-board-like kind) is a load error. [kind-column-state-map]
status:: done
implements:: [[code:hiker/kinds/compile_columns]], [[code:hiker/kinds/impl#[Kind]columns_for_category]], [[code:hiker/queries/parse_board_clause]], [[code:hiker/queries/store_category_columns]], [[code:hiker/store/metadata/impl#[Store]query_notes]]
verifies:: [[code:hiker/queries/tests/run_query_expands_board_category_through_kind_mapping]], [[code:hiker/queries/tests/board_category_over_unmapped_board_is_a_loud_error]], [[code:hiker/store/tests/query_notes_board_membership_filter]], [[code:hiker/kinds/tests/column_to_undeclared_state_is_an_error]]
note:: mapping lives on the board-like kind definition (`[kinds.<name>.columns]`, `Kind::columns`); column name -> state name onto the kind's own state set; unmapped columns have no PM semantics (absent from the map = absent from every expansion). The query grammar's `category` board clause now compiles against it: `queries::parse_board_clause` accepts `category` (closed five-anchor set, exclusive with `column`), and `compile_query` expands it at compile time — board's `hiker.kind` read off `note_meta`, `Kind::columns_for_category` -> a column-name `IN` set on the `board_cards` EXISTS (`MetaFilter::Board.columns`); empty expansion matches nothing, a board without a registered mapped kind is a loud error · evidence: `core/src/kinds.rs::columns_for_category`, `core/src/queries.rs` (`BoardScope`, `store_category_columns`), `core/src/store/metadata.rs` (column-set IN); tests `queries::tests::run_query_expands_board_category_through_kind_mapping`, `board_category_over_unmapped_board_is_a_loud_error`, `store::tests::query_notes_board_membership_filter`

The mapping lives on the *kind*, not on each board and not on a plan, because meaning must be cross-board: a query like "everything in an `in_progress`-category column" ([[spec:query-filter-grammar]]'s `category` form) compiles by reading the board's kind, expanding the category through this mapping to a column-name set, and filtering `board_cards` — one mapping per kind keeps that expansion a single lookup and keeps every board of the kind answering consistently. A plan (the `plan` built-in, semantics in the PM layer) picks *defaults* — default kind, default column seed — and never owns a second mapping. Per-board mappings were rejected: N boards with N private vocabularies is exactly the reconciliation problem category anchors exist to avoid.


## Lenient validation

A note whose `hiker.kind` names a registered kind is validated against the definition on ingest: a `required` field missing, a value outside its primitive (a non-number `priority`), an `enum` value outside the declared list, a `ref` that doesn't resolve or resolves to the wrong kind. Violations produce a badge on the note wherever it renders plus a problems report; they **never block a write, never drop data, never rewrite the file**. The vault stays markdown-first — hand-edited, agent-written, and externally-synced notes are all legal at all times, and the schema's job is to *report* drift from the declared shape, not to enforce it. [kind-lenient-validation]
status:: partial
implements:: [[code:hiker/kinds/validate_note]], [[code:hiker/indexer/jobs/update_note_problems]], [[code:hiker/store/metadata/impl#[Store]note_problems]], [[code:hiker/store/metadata/impl#[Store]notes_with_problems]]
verifies:: [[code:hiker/indexer/tests/ingest_derives_lenient_validation_problems]], [[code:hiker/store/tests/note_problems_replace_query_and_lifecycle]], [[code:hiker/kinds/tests/required_field_missing_is_reported]], [[code:hiker/kinds/tests/ref_to_wrong_kind_is_reported]], [[code:hiker/kinds/tests/clean_note_validates_with_no_problems]]
note:: problems report landed store-side: `kinds::validate_note` runs on ingest over the flattened frontmatter (`indexer/jobs.rs::update_note_problems`, after `replace_note_metadata`) into the derived `note_problems` table (the `note_meta` lifecycle — re-derived on ingest, cleared on skip/delete, re-keyed on rename, no schema bump); read surface `Store::note_problems(path)` + `Store::notes_with_problems()` (path + count, the badge data). Never blocks writes, never mutates notes; extra keys always fine; unregistered kinds never validated; ref resolution checks `note_exists` + target `hiker.kind`. **Partial: no badge rendering yet** — the spec's "badge on the note wherever it renders" UI isn't wired into any surface; the data + report API are ready for it. Like `note_meta`, the unchanged-content short-circuit skips re-validation, so a registry change re-validates on the next content change or forced reindex · evidence: `core/src/kinds.rs::validate_note`, `core/src/indexer/jobs.rs::update_note_problems`, `core/src/store/metadata.rs`; tests `indexer::tests::ingest_derives_lenient_validation_problems`, `store::tests::note_problems_replace_query_and_lifecycle`

- Validation is re-derived on ingest from the already-flattened frontmatter ([[spec:store-note-metadata-index]]) — event-driven like every derived view, never a vault walk, never persisted into the note.
- Extra frontmatter keys beyond the kind's fields are always fine; the schema constrains what it declares and is silent about the rest.
- The asymmetry with the registry is the point: strict where an error is rare, load-time, and actionable (config); lenient where an "error" is just a note mid-edit (data).


## Built-in PM set

The PM kinds ship as registry entries written in the same TOML users write — the format is proven by its first consumer, not designed speculatively. Built-ins sit as the lowest layer of the existing config deep-merge (built-ins ← user ← vault, vault winning per-key), so a vault that redefines `kinds.story.fields` replaces that list wholesale while untouched keys keep their built-in values; `[kinds.<name>] enabled = false` disables an entry. There is no privileged code path — a built-in is exactly a registry entry the user didn't have to type. [kind-builtin-pm-set]
status:: done
implements:: [[code:hiker/kinds/builtin_kinds_value]], [[code:hiker/config/impl#[Config]load]]
verifies:: [[code:hiker/config/tests/kinds_builtins_merge_under_user_and_vault]], [[code:hiker/kinds/tests/builtin_set_compiles_with_spec_entries]], [[code:hiker/kinds/tests/disabled_entry_is_skipped]], [[code:hiker/config/tests/kinds_default_config_carries_no_entries]]
note:: story/task (leaf: priority, due, estimate), epic (list-like), sprint (board-like: start/end/goal + states + column mapping), plan (list-like root); shipped as `kinds::BUILTIN_KINDS_TOML` (the user TOML format verbatim) and merged as the lowest layer in `Config::load` (built-ins <- user <- vault). Deliberately NOT in `Config::default()`, so auto-created config files never freeze a copy of them · evidence: `core/src/kinds.rs::BUILTIN_KINDS_TOML` / `builtin_kinds_value`, `core/src/config/mod.rs::load`; tests `config::tests::kinds_builtins_merge_under_user_and_vault`, `kinds::tests::{builtin_set_compiles_with_spec_entries,disabled_entry_is_skipped}`

| entry | shape | fields | notes |
|---|---|---|---|
| `story`, `task` | leaf | `priority` (number), `due` (date), `estimate` (number) | two names, one definition — vaults pick their word; edit or disable either |
| `epic` | list-like | — | ordered refs to member work notes |
| `sprint` | board-like | `start` (date), `end` (date), `goal` (string) | carries the state set + column mapping in the example above |
| `plan` | list-like | — | root container; owns policy *defaults* (default kind, column seed) |

What these kinds *mean* — a story's derived status, sprint membership and rollover, epic progress rollups, plan policy — is the PM layer's spec, not this one. This doc owns the entries; the semantics consume them.


## Generated MCP tools

From each registered kind, the MCP server generates a typed write pair — the registry is the single source the tool surface is derived from, so a new kind ships with its agent surface automatically instead of waiting for hand-written tools. [mcp-registry-tools]
status:: partial
implements:: [[code:hiker/handler/dispatch/kinds]]
note:: `create_<kind>` / `update_<kind>` generated from the field schema (`mcp-server/src/handler/dispatch/kinds.rs`: number -> number, enum -> enum of declared values, date -> ISO string, ref -> path string; create requires the kind's required fields); routes built per handler from the loaded registry (`ToolRoute::new_dyn` merged into the rmcp router via `#[tool_handler(router = self.tool_router)]`) so they advertise + regenerate with the registry; in-process `dispatch_tool` arm covers the chat agent. Boundary strict (`invalid_params` for out-of-enum / malformed date / unknown field), disk lenient; writes ride the standard agent-write path — review mode stages a whole-body op-log pending proposal via the same `stage_whole_body` every write tool uses, direct mode routes `ops::agent::{write_note,set_frontmatter}` (author stamp on create); `update_<kind>` refuses a target whose `hiker.kind` differs. Family toggle `[mcp.tools] kind_tools_enabled` + `writes_enabled` master gate (settings row + eligible key wired). **Partial:** (a) read-before-write deferred with [[spec:mcp-read-before-write]] (still planned — no read set exists for any write tool); (b) the chat agent's advertised `agent_tool_defs` list doesn't include the generated pair (dispatch works; advertisement to the in-process model pending) · evidence: smoke tests `kind_tools_advertise_with_typed_param_schemas`, `kind_tools_create_update_round_trip_direct`, `kind_tools_stage_when_review_required`, `kind_tools_strict_boundary_and_family_toggle`

- **`create_<kind>(rel_path, body?, <fields…>)`** — create a note with `hiker.kind: <kind>` plus the given typed fields in frontmatter; field params come from the field schema (`number` → number, `enum` → an enum of the declared values, `date` → ISO-8601 string, `ref` → vault-relative path). Rides the standard agent-write path: author stamping on create, review-mode staging per [[spec:agent-write-review-mode]].
- **`update_<kind>(rel_path, <fields…>)`** — merge typed fields into an existing note of that kind (the frontmatter-merge path `set_frontmatter` uses); a target whose `hiker.kind` doesn't match errors rather than silently retyping the note.
- Param validation at the tool boundary is **strict** (`invalid_params` for an out-of-enum value or a malformed date) even though on-disk validation is lenient — the boundary is where strictness costs nothing, and it stops agents from authoring schema drift the badge would only report later.
- Reads need no generated tools: the generic `query` tool ([[spec:query-mcp-tool]]) already covers enumeration and filtering by kind and fields.
- Tools regenerate when the registry loads and are advertised through the dynamic capability set ([[spec:mcp-dynamic-capabilities]]). The per-tool toggle table ([[spec:mcp-tool-toggles]]) is a closed strict-load struct, so generated tools are gated by a single `[mcp.tools] kind_tools_enabled` family toggle (default true) plus the master `writes_enabled` gate, rather than per-kind config keys.
- Errors follow the standard model ([[spec:mcp-error-model]]); `update_<kind>` against existing content honors read-before-write ([[spec:mcp-read-before-write]]).


## MCP tool audit

One-time task, landing with this layer because the generated tools fix the pattern going forward but not the backlog: sweep every feature that shipped after the MCP layer for a missing agent surface (clusters, canvas, projects, trails — anything marked done in its spec without a corresponding tool), and file each gap as a row in `bug_tracking.md` or `todo.md`. The audit produces tracked gaps, not tools; each gap is then normal scheduled work. [mcp-tool-audit]
status:: done
note:: sweep run 2026-06-12 against the registered surface in `mcp-server/src/handler/router.rs`; gaps filed as the single `bug-mcp-tool-coverage-gaps` row in `bug_tracking.md` (cluster trees, canvas, trails, projects/code-intel, backlinks, trash + move/delete, op-log history/activity, diff — already-tracked deferrals listed there for the rollup). No tools built, per the slug's contract


## Out of scope

- **PM semantics.** Derived status, sprint membership and rollover ([[spec:sprint-board-subtype]], [[spec:derived-status-rule]], [[spec:sprint-rollover]]), epic rollups, plan policy — `pm.md` consumes the kinds defined here.
- **Automation rules.** The rules layer (`rules.md`) triggers on kind fields and derived state ([[spec:rule-triggers]]); triggers and actions are that spec's problem.
- **A registry settings editor.** The TOML is the surface, the same v1 posture as inbox rules; a settings-pane editor waits on the array-of-tables row control.
- **Typed relations between kinds.** `ref` fields are plain path references; a relation registry (named edge types with their own semantics) is a separate, later design.
- **Per-note schema overrides.** A note can't extend or amend its kind's schema from frontmatter; vocabulary changes go through the registry so anchors stay trustworthy.
