# Rules

Post-index automation over the derived indexes: a rule watches for a change the indexer has
already derived (`note_meta`, `board_cards`), checks a condition written in the queries grammar,
and applies actions from a closed verb set through the same attributed, reviewable write paths
every other producer uses. The last layer of the PM arc (queries → kinds → PM semantics → rules),
and deliberately the smallest — enough automation for kanban-shaped PM, not a workflow engine.

The headline decisions:

- A rule is **trigger + condition + actions**, declared as data in vault config — `[rules.<name>]`
  entries beside `[kinds.<name>]`, strict-loaded, the registry's format family. [rule-shape]
- Four triggers, each riding a seam that already exists: note created, frontmatter changed, card
  moved (the indexer's derived-table updates), date passed (a daily/lazy sweep over the indexed
  date mirror). [rule-triggers]
- The condition is a query-doc reference or an inline filter in the **same closed grammar**
  ([[spec:query-filter-grammar]]) — rules add no second condition language.
  [rule-condition-reuses-queries]
- Actions are a **closed verb set** in v1 — set field, move card, add to board/column, create note
  from kind template — each routed through an existing op path; no rule-private write machinery.
  [rule-closed-verbs]
- **No cascades**: rule-initiated writes never fire rules — one generation per external event.
  This single decision eliminates loops, rule ordering, and conflict resolution wholesale.
  [rule-no-cascade]
- Every firing is **attributed and reviewable**: frames are authored `auto:rule:<name>` through
  the op-log ([[spec:op-log-author-classes]]), staged like agent writes under review mode, and a
  rules panel shows recent firings per rule. [rule-attribution] [rule-firings-panel]


## Rule shape

A rule is a named `[rules.<name>]` entry in `vault/.hiker/config.toml`, strict-loaded per
[[spec:settings-strict-load]] with the same posture as the kind registry ([[spec:kind-registry]]):
rules are config — rare edits, one load point, an actionable failure naming the offending entry —
validated in the existing cross-field hook (`core/src/config/mod.rs::validate_cross_field`, the
`[kinds]` / `[inbox]` precedent). An unknown trigger, a condition outside the queries grammar, an
unknown verb, or an action whose board/kind reference is malformed aborts startup naming the rule.
`enabled = false` disables an entry without deleting it, the kind-registry convention. [rule-shape]
status:: done
implements:: [[code:hiker/rules/impl#[RuleSet]compile]], [[code:hiker/config/validate_registries]], [[code:hiker/config/Config#rules]]
verifies:: [[code:hiker/rules/tests/spec_examples_compile]], [[code:hiker/rules/tests/bad_triggers_are_errors]], [[code:hiker/rules/tests/unknown_verb_is_an_error]], [[code:hiker/rules/tests/malformed_action_references_are_errors]], [[code:hiker/config/tests/rules_invalid_entry_fails_cross_field_naming_offender]], [[code:hiker/config/tests/rules_valid_entry_passes_cross_field]]
note:: `[rules.<name>]` entries kept as raw TOML values on `Config.rules` (the `Config.kinds` pattern, so errors name the offending entry) and compiled in the cross-field hook (`validate_cross_field` -> `validate_registries`, after the kind registry it references); `core::rules` owns the compiled form (`RuleSet` / `Rule` / `Trigger` / `Condition` / `Action`), recompiled at vault open into the live `rules::Engine` (`app/src/bootstrap.rs::attach_config_engines`) and attached to the indexer beside kinds. One deliberate divergence from the kind-registry convention: a disabled entry is still fully validated and stays in the set (flagged `enabled: false`, never fired) so the panel can list its trigger + enabled state — kinds skip disabled entries wholesale

```toml
[rules.escalate-overdue]
on   = { trigger = "date-passed", key = "due" }
when = { filter = { kind = "story", board = { path = "boards/sprint-12.md" } } }
do   = [ { set_field = { key = "priority", value = 1 } } ]

[rules.triage-new-stories]
enabled = true                      # default; false disables without deleting
on   = "note-created"
when = { query_doc = "queries/unplaced-stories.md" }
do   = [ { add_to_board = { board = "boards/triage.md", column = "Todo" } } ]
```

- `on` — the trigger: one of the four below. The three event triggers are plain strings;
  `date-passed` takes the table form naming the watched date key.
- `when` — optional condition, evaluated against the triggering note: exactly one of `query_doc`
  (a query-doc path, [[spec:query-doc-shape]]) or `filter` (inline, same clause set). Absent means
  every trigger event matches.
- `do` — an ordered list of actions, each exactly one closed verb. Actions apply in order; a
  failed action aborts the remaining actions of that firing (already-applied ones stand — no
  cross-document rollback, per [[spec:op-log-reorg-batch]]) and surfaces in the rules panel.

Tradeoff — config over rule-docs: declaring rules as notes (the [[spec:subsystem-notes-visible]]
pattern) was rejected for the same reason kind-definition notes were ([[spec:kind-registry]]'s
tradeoff): automation that writes to the vault wants one load point and a strict validation gate,
not behavior that depends on indexer state and mid-session edits.


## Triggers

All four triggers fire **post-index** — after the indexer has derived the state the condition
reads — so a condition over `note_meta` / `board_cards` always sees the note as the trigger left
it. The first three ride the ingest pipeline (`core/src/indexer/jobs.rs::process_upsert`), which
already re-derives every table the triggers watch; the rule pass hooks in directly after those
updates, on the indexer task — the single-writer discipline every vault mutation already obeys.
[rule-triggers]
status:: done
implements:: [[code:hiker/indexer/jobs/rule_events]], [[code:hiker/indexer/jobs/update_board_cards_if_relevant]], [[code:hiker/rules/meta_changed]], [[code:hiker/rules/card_moves]], [[code:hiker/rules/impl#[Engine]date_sweep]], [[code:hiker/indexer/IndexJob#RulesDateSweep]], [[code:hiker/store/metadata/impl#[Store]note_metadata]], [[code:hiker/store/metadata/impl#[Store]meta_kv_get]], [[code:hiker/store/metadata/impl#[Store]note_paths_with_meta_num_between]], [[code:hiker/indexer/impl#[Handle]attach_rules_engine]]
verifies:: [[code:hiker/rules/tests/meta_changed_compares_multisets]], [[code:hiker/rules/tests/card_moves_diffs_columns_only]], [[code:hiker/rules/tests/date_sweep_watermark_prevents_double_fire]], [[code:hiker/rules/tests/failed_watermark_write_defers_firings_to_the_next_sweep]]
note:: before-rows landed as reads-prior-to-replace: `process_upsert` snapshots `Store::note_metadata` before `replace_note_metadata` (the frontmatter-changed diff, compared as key/value multisets) and `update_board_cards_if_relevant` snapshots `cards_of(board_id)` before its clear-then-reinsert, returning the card-moved events; `note-created` rides the watcher's `IndexJob::Created` arm (first ingest, post-inbox-rules) — a first ingest with no prior note row is creation, never a frontmatter change, so a fresh index over an existing vault fires nothing. The sweep watermark is a per-rule `rules.sweep.<name>` row in the store's `meta(key, value)` sidecar (a brand-new rule's first sweep only records it — no firing for already-past dates); the host enqueues `IndexJob::RulesDateSweep` via a 24h interval whose first tick is immediate (`bootstrap.rs::rules_date_sweep_ticker` — vault open + daily tick), and the whole pass runs on the indexer task. The sweep persists the new watermark BEFORE running the crossing's firings: a watermark that fails to write produces zero firings (loud in the engine's failure ring) and the whole crossing defers to the next sweep — firing first would let the next sweep re-walk the same crossing and double-fire (`create_note` would mint duplicate notes), so the chosen fail-safe is a lost firing (a crash between watermark write and firing drops that crossing), never a duplicate one. Named skip case for the event triggers: an out-of-app edit to a note not open in any editor pane writes no op-log frame mid-session, so if that note's newest frame is rule-authored, the no-cascade generation check skips the genuine external event — fail-closed, a missed fire, never a wrong fire; the frame catches up through the op-log reconcile seams (the watcher relay / open-time per-doc reconcile, [[spec:op-log-external-edit-sync]]). Applied firings re-index through an explicit non-blocking enqueue (`with_rules_engine`), and each successfully enqueued path is registered for watcher self-write suppression ([[spec:watcher-suppress-self-writes]], the `ops::file` suppress-then-Upsert discipline) so the firing's `.md` writes don't echo back as duplicate upserts; a path whose enqueue failed stays unsuppressed so the ambient watcher route remains its ingest fallback

| trigger | fires when | detection seam |
|---|---|---|
| `note-created` | a new note's first ingest completes | the indexer's create/first-upsert path — post-index, so frontmatter is already queryable (contrast inbox rules, below) |
| `frontmatter-changed` | a note's indexed metadata changed | diff of `note_meta` rows across `replace_note_metadata` ([[spec:store-note-metadata-index]]) |
| `card-moved` | a card's column changed on a board | diff of `board_cards` rows across `update_board_cards_if_relevant` ([[spec:board-cards-derived-table]]) — DnD, MCP ops, and hand edits all converge at the derived table, so every path is covered by one seam |
| `date-passed` | the watched date key crossed "now" | a daily/lazy sweep — at vault open and on a daily tick — over the `note_meta.num` epoch mirror (the query grammar's date encoding): fires for notes whose key falls between the last sweep watermark and now, once per crossing |

The sweep is deliberately lazy: no scheduler, no per-note timers. A vault closed for a week fires
the missed crossings once on open, watermarked so nothing double-fires.


## Condition

The condition reuses the queries layer wholesale: `query_doc` names a saved query-doc, `filter`
is the inline form — same parser, same closed clause set, same compile to bound-parameter SQL
([[spec:query-filter-grammar]]). Growing rule expressiveness means growing the one grammar, where
smart folders and the MCP `query` tool ([[spec:query-mcp-tool]]) pick the growth up for free.
[rule-condition-reuses-queries]
status:: done
implements:: [[code:hiker/queries/matches_note]], [[code:hiker/queries/parse_filter_toml]], [[code:hiker/rules/condition_matches]]
verifies:: [[code:hiker/queries/tests/matches_note_covers_each_clause_type]], [[code:hiker/queries/tests/parse_filter_toml_bridges_the_same_grammar]], [[code:hiker/rules/tests/non_matching_condition_skips_the_firing]], [[code:hiker/rules/tests/missing_query_doc_is_a_loud_firing_error]]
note:: `queries::matches_note(store, registry, query, path)` = the compiled query plus one bound `NoteQuery.path_eq` constraint (`store::metadata` adds the `AND n.path = ?` arm), limit 1 — one indexed probe per firing; the TOML bridge is `parse_filter_toml` (toml -> JSON -> `parse_filter_json`, so the one grammar parses all three inline forms and unknown clauses stay loud). Inline filters compile at strict-load; `query_doc` paths are shape-checked at load (`.md` required) and read per firing, so a missing or non-query doc is the loud per-firing error in the panel's ring. The parse is memoized per doc on the exact source bytes (`Engine::parsed_query_doc` — a date sweep firing one rule over many crossings parses once); the per-firing read keeps edits live and errors loud, and parse failures are never cached

A rule asks "does the triggering note match", not "what matches" — one indexed per-note check per
firing, never a full query run, never a vault walk. A condition referencing a missing or
non-query `query_doc` is a load-time error where resolvable and a loud per-firing error in the
panel otherwise (the query layer's no-silent-fallback posture).


## Actions: the closed verbs

Four verbs in v1. Each routes through an op path that already exists — rules decide *when*,
existing ops decide *how* — so every guard those paths enforce binds rules exactly as it binds
users and agents. A rule cannot bypass the single-sprint guard, WIP limits, or review mode,
because there is no rule-private write path to bypass them with. [rule-closed-verbs]
status:: done
implements:: [[code:hiker/rules/apply_set_field]], [[code:hiker/rules/apply_move_card]], [[code:hiker/rules/apply_add_to_board]], [[code:hiker/rules/apply_create_note]], [[code:hiker/boards/ops/BoardWriteMode]], [[code:hiker/boards/ops/add_note_card]], [[code:hiker/boards/ops/move_card_to_column]], [[code:hiker/ops/op_writes/Draft]], [[code:hiker/kinds/template_note_body]], [[code:hiker/vault/next_free_md_path]]
verifies:: [[code:hiker/rules/tests/set_field_writes_an_attributed_frame]], [[code:hiker/rules/tests/move_card_defaults_to_the_one_sprint]], [[code:hiker/rules/tests/add_to_board_binds_the_single_sprint_guard]], [[code:hiker/rules/tests/add_to_board_appends_to_the_default_column]], [[code:hiker/rules/tests/create_note_seeds_the_kind_template]], [[code:hiker/rules/tests/failed_action_aborts_the_rest_but_keeps_the_prefix]]
note:: `set_field` BUILT its attributed write as planned — frontmatter merge (`merge_json_into_yaml` + `assemble`) staged through the op-log batch seam authored `auto:rule:<name>`; inbox `add_tag` rides the same staged seam authored `auto:inbox` now (the original sibling divergence, closed). The board verbs CALL the real board ops: the write ops are parameterized over authorship (`boards::ops::BoardWriteMode` — `UserDirect` commits via `user_save`, `AutoStaged` reads through and lands into the firing's draft overlay, `op_writes::Draft`), so `move_card` is `boards::ops::move_card_to_column` and `add_to_board` is `boards::ops::add_note_card` — the SAME read → guard → mutate → render body the user verbs commit through (same per-board idempotency, same `pm::ensure_single_sprint_membership` binding), with nothing committing per-op: the draft stages as the firing's one batch. `create_note` shares `vault::next_free_md_path` (the promote collision rule, extracted from `boards::ops`) + `kinds::template_note_body` (the `freeform-promote-note` seeding, extracted from `promote_text_card`). Decided at implementation: `add_to_board.column` unset defaults to the board's first column (the op's `column = None` arm); `move_card` of a card already in the target column is a no-op; multiple actions in one firing read each other's output through the draft overlay before staging

| verb | params | routes through |
|---|---|---|
| `set_field` | `key`, `value` | a new attributed frontmatter-merge built over `op_writes::stage_auto_content` (auto-flip when review is off, the `suggest.rs` precedent) — there was no shared primitive to ride: [[spec:inbox-rules]]' `add_tag` now stages through the same seam authored `auto:inbox` (the former bare-write divergence, closed), and the MCP `set_frontmatter` rides `ops::agent::set_frontmatter` |
| `move_card` | `column`, `board?` | the real board op (`boards::ops::move_card_to_column`) in its `AutoStaged` authorship mode — the moved board-doc text lands in the firing's draft; `board` defaults to the note's one sprint-kind board via the derived-status read ([[spec:derived-status-rule]]) when unset |
| `add_to_board` | `board`, `column?` | the real board op (`boards::ops::add_note_card`, `AutoStaged` mode) — the single-sprint membership guard applies and refuses exactly as it would a user |
| `create_note` | `path`, `kind?` | note create plus kind-template seeding (`hiker.kind` set, the kind's fields seeded empty — the [[spec:freeform-promote-note]] seeding); collision suffixes like promote; `kind` must name a registered entry ([[spec:kind-builtin-pm-set]] or user-defined) |

`set_field`, `move_card`, and `add_to_board` act on the triggering note; `create_note` mints a new
one. There is no delete verb, no move-note verb, no arbitrary-write verb — destruction and
relocation stay user verbs, and "run this command" is exactly the escape hatch the closed set
exists to refuse (the inbox-rules posture, kept).


## No cascades

A rule-initiated write never fires a rule: one generation per external event. A user edit (or
agent write, sync frame, extractor pass) may fire rules; the writes those firings produce are
authored `auto:rule:<name>`, and the rule pass skips evaluation for any change whose newest
accepted frame carries the `auto:rule:` author prefix — the op-log is already threaded into the
ingest path (`JobCtx.oplog_cell`), so the generation check is one indexed read at the trigger
seam. [rule-no-cascade]
status:: done
implements:: [[code:hiker/rules/newest_frame_is_rule_authored]], [[code:hiker/rules/impl#[Engine]on_events]], [[code:hiker/rules/ensure_doc_rule_authored]]
verifies:: [[code:hiker/rules/tests/rule_authored_frames_do_not_refire]]
note:: generation check = `doc_history(doc, 1)` on the changed path (the note for note-created / frontmatter-changed, the board-doc for card-moved); an `auto:rule:%` author prefix skips every rule for that event. Acceptance keeps the rule author (per [[spec:op-log-attribution]]), so accepting a staged firing can't re-fire; a `create_note` firing seeds the new note's op-log doc with the rule author too (`ensure_doc_rule_authored`), so the minted note's own first ingest is covered by the same check

One named leak: the generation check guards the event-driven triggers only — the `date-passed`
sweep carries no generation check, so a rule-written date field (a `set_field` that sets the
watched key to a just-passed date) can fire a `date-passed` rule at the next sweep. Bounded: one
extra generation, never a loop — the advancing watermark passes that crossing exactly once.

Tradeoff, stated plainly: no rule chaining. "A fires B fires C" is rejected in full — with it come
loop detection, rule ordering, conflict resolution, and debugging emergent behavior; without it, a
rule's effect is readable off its own definition. A workflow that wants A-then-B writes one rule
that does both (actions are an ordered list).


## Attribution and review

Every firing writes through the op-log with the rule author class: `Author::Auto` with producer
`rule:<name>`, wire form `auto:rule:<name>` ([[spec:op-log-author-classes]] — the class-prefix
query makes `auto:rule:%` the all-firings filter and `auto:rule:escalate-overdue` one rule's).
The sprint-close batch ([[spec:sprint-rollover]], author `auto:sprint-close`) is the precedent:
rules ride the same staged-batch substrate (`op_writes::stage_auto_content` /
`stage_auto_content_batch` — a multi-action firing stages its writes under one batch id, the
[[spec:op-log-reorg-batch]] shape, accept/reject with per-item partial apply). Under review mode
(`review_required`, the op-log config) firings stage like agent writes ([[spec:agent-write-review-mode]]);
with review off they commit directly, still rule-authored. The firing's authorship rides the write's
`Author` class — `auto:rule:<name>` — surfaced on the git `Hiker-Author` trailer when git is
integrated; who: the rule by name; what: the changed docs; when: the trigger. [rule-attribution]
status:: done
implements:: [[code:hiker/rules/impl#[Engine]stage_firing]]
verifies:: [[code:hiker/rules/tests/multi_action_firing_stages_one_pending_batch_under_review]], [[code:hiker/rules/tests/set_field_writes_an_attributed_op]]
note:: every firing stages through `op_writes::stage_auto_content_batch` with producer `rule:<name>` (wire `auto:rule:<name>`) and surface `rules` — a multi-action firing is ONE cross-document batch id, the sprint-close shape, reviewed and flipped as one unit. Review mode reads the op-log config's `review_required` at engine construction: on, the batch stays pending; off, the batch auto-flips immediately after staging (the `suggest.rs` / sprint-close precedent). There is no activity-feed projection (the `core::activity` feed was removed) — the `Author` class on each write (the git trailer when integrated) is the firing record


## The rules panel

Every firing is visible: a Rules panel lists each registered rule — name, trigger, enabled state,
last firing — and expands to its recent firings, read from the op-history projection filtered by
the rule's author wire (no new store; the frames are the log). Firing rows carry the standard
item grammar (`interaction.md`): click opens the affected note ([[spec:click-opens]]), right-click
is its full menu ([[spec:rightclick-menu-always]]), hover previews ([[spec:hover-preview-universal]]).
A failed firing (action errored, nothing written) has no frame; it renders as an error row from
the in-memory diagnostics ring ([[spec:obs-log-ring-buffer]] posture) so misfiring rules are
debuggable without log-diving. The panel is read-only in v1 — the TOML is the editing surface,
the inbox-rules posture. [rule-firings-panel]
status:: done
implements:: [[code:hiker/panels/rules]], [[code:hiker/tab/TabKind#Rules]], [[code:hiker/rules/impl#[Engine]failures]], [[code:hiker/rules/FiringFailure]]
verifies:: [[code:hiker/rules/tests/firings_project_by_rule_author]], [[code:hiker/rules/tests/add_to_board_binds_the_single_sprint_guard]], [[code:hiker/rules/tests/missing_query_doc_is_a_loud_firing_error]]
note:: surface shape decided: a singleton tab (`TabKind::Rules`, the Changes / BoardsIndex pattern — toolbar row, `:rules` persist key). Each rule renders name + trigger + enabled state + last firing, expanding to its recent firings via `AcceptedFeed::recent_by_author("auto:rule:<name>")`; failed firings (no frame written) render as red rows from the engine's bounded in-memory ring (`Engine::failures`, cap 200 — the [[spec:obs-log-ring-buffer]] posture, kept engine-local since that spec's shared ring is still planned). Rows carry the item grammar: click opens the note, the right-click menu is the shared note-item base (`item_menu::note_item_base`), hover previews via `widgets::preview::register_note_hover`. Read-only — no edit verbs anywhere on the surface


## Relation to inbox rules

Inbox rules ([[spec:inbox-rules]]) stay exactly as they are: pre-index create-time placement —
basename/body regex against a file the indexer hasn't seen yet, first-match-wins, `move_to` /
`add_tag`. Vault rules are post-index: they fire on derived-state change and read conditions off
the indexes. Prior art, not a conflict — the two occupy different pipeline stages, and the
ordering is well-defined: an inbox rule places and tags the new file first; a `note-created`
vault rule runs after first ingest and sees the note where the inbox rule left it. Whether inbox
rules eventually become a preset of the general system is a post-v1 question; nothing here blocks
on it, and nothing here changes their behavior. [rule-inbox-relation]
status:: done
implements:: [[code:hiker/indexer/jobs/handle_simple_job]]
note:: prose entry — nothing new to build. Inbox rules are untouched; the well-defined ordering is enforced by the `IndexJob::Created` arm itself: `run_inbox_rules` places/tags the new file first, then the post-move ingest runs and the `note-created` vault-rule pass fires against the note where the inbox rule left it. `inbox-rules.md` carries the reciprocal one-liner (its §"Interaction with other systems")


## Deferred

- **The `script` action.** [rule-script-slot-reserved] The verb name `script` is reserved in the
  rule format now: strict-load rejects a `script` action with an error naming it *reserved* (not
  unknown), so the slot is visibly held and no user vocabulary squats on it. When revisited, a
  sandboxed scripting host (Lua via `mlua`, WASM via `wasmtime`, or Rhai — pure Rust, built-in fuel/depth
  limits, no C dependency — evaluate sandboxing, fuel and
  timeout limits) whose entire API surface is *read the triggering note, emit closed-verb
  actions*: scripts compute, the closed verbs act, so attribution, review staging, and the
  no-cascade invariant hold without a special case. v1 ships no implementation.
status:: done
implements:: [[code:hiker/rules/compile_action]]
verifies:: [[code:hiker/rules/tests/script_verb_is_a_reserved_error]]
note:: exactly the reservation shipped, nothing more: `compile_action`'s `script` arm rejects with an error that says *reserved* (and names the deferred scripting slot), distinct from the unknown-verb error. The sandboxed scripting host itself stays deferred per this entry's own contract


## Out of scope

- **A rules settings editor.** The TOML is the surface, same as inbox rules and the kind registry;
  a settings-pane editor waits on the array-of-tables row control.
- **Body-content conditions.** The grammar filters metadata and membership, not text — the same
  boundary `queries.md` draws; content retrieval is `search.md`'s job.
- **Scheduled arbitrary jobs.** The date-passed sweep is the ceiling of v1's time-awareness;
  recurring background work is the task queue's domain (`task-queue.md`).
- **Cross-vault rules.** Per-vault, full stop — the boards/trails/queries boundary.
- **Smart columns and query presentations.** Later layers over the saved-query primitive
  (`queries.md` Out of scope), not rule actions.
