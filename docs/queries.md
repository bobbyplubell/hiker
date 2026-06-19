# Queries

Saved queries over the derived indexes. A query is authored as a note (a **query-doc**), compiled onto the structured store indexes ([[spec:store-note-metadata-index]], [[spec:board-cards-derived-table]]), and rendered wherever a live set of matching notes is useful — first as smart folders in Vault mode, and as a generic MCP tool. The query layer deliberately ships first in a larger arc: later layers (board-lane presentations of a query, automation conditions) reuse this saved-query primitive rather than inventing parallel condition languages.

The headline decisions:

- A query-doc is a regular markdown note with `hiker.kind: query` — frontmatter holds the filter, the body is prose. Same authored-doc pattern as board-docs and trail-docs: synced, diffable, agent-writable, re-derived by the indexer. [query-doc-shape]
- The filter grammar is small and closed — kind, field comparisons (string eq / list contains / numeric + date ranges), tags, path glob, board membership — combined as a top-level AND of clauses with OR only inside a clause. It compiles to parameterized SQL over `note_meta` + `board_cards`; no user-written joins, no functions, no negation of groups. [query-filter-grammar]
- Vault mode renders a query-doc as a **smart folder**: a virtual folder containing its matches. Membership is virtual and visually marked; the note stays at its real path. [smart-folder-view]
 Member rows are drag sources like every note row (vault-relative path payload, `bug-note-rows-not-drag-sources` fixed 2026-06-12) — dragging *out* onto a board or canvas works; dropping *in* stays undefined. The drag-in-sets-field affordance is reserved and explicitly deferred. [smart-folder-drag-sets-field]
- One generic `query` MCP tool takes a query-doc path or an inline filter and returns matching note rows — most of the agent-facing win in a single tool. [query-mcp-tool]


## Query-doc shape

A query-doc is a regular markdown note at any vault location. Frontmatter:

```yaml
---
hiker:
  kind: query
  query:
    kind: story                          # sugar for { key: hiker.kind, eq: story }
    tags: [rust, embedded]               # any-of; sugar for { key: tags, eq: [...] }
    path: "work/**"                      # vault-relative glob
    board: { path: "boards/q3.md", column: Doing }
    fields:
      - { key: priority, min: 2 }
      - { key: due, max: "2026-07-01" }
    order: { by: due, dir: asc }         # optional; default path asc
    limit: 50                            # optional
---
# Open embedded work

Freeform prose. Why this query exists, what counts as a match, who reads it.
```

`hiker.query` is the single source of truth for the filter; the body is hand-authored framing that the indexer never touches — the same split board-docs use ([[spec:board-doc-shape]]). Parsing mirrors `boards::parse_board_for`: the `hiker.kind: query` discriminator plus a required `.md` extension (a non-`.md` file carrying the discriminator is a regular note — the rule trails and boards share), unknown `hiker.*` siblings and top-level keys preserved on write-back. A query-doc is an ordinary indexed note, so a hand-typed or agent-written file with the right frontmatter is a query-doc with no registration step ([[spec:subsystem-notes-visible]]), and enumeration is one indexed lookup — `hiker.kind = query` against the metadata index ([[spec:store-note-query]]), never a vault walk. [query-doc-shape]
status:: done
implements:: [[code:hiker/queries/parse_query_doc_for]], [[code:hiker/queries/list_query_docs]], [[code:hiker/queries/run_query]]
verifies:: [[code:hiker/queries/tests/parse_spec_example_doc]], [[code:hiker/queries/tests/parse_rejects_non_md_path]], [[code:hiker/queries/tests/parse_rejects_missing_frontmatter_and_wrong_kind]], [[code:hiker/queries/tests/parse_rejects_unknown_clause]]
note:: `core/src/queries.rs` — `parse_query_doc` / `parse_query_doc_for` mirror `boards::parse_board_for` (discriminator + `.md` rule); a malformed filter is a loud `queries::Error` (unknown clause / bad value), never a silent fallback; enumeration is `list_query_docs`, one `hiker.kind = query` lookup through `query_notes` with non-`.md` carriers excluded. No dedicated query-doc serializer yet: nothing in this phase rewrites a `hiker.query` block (smart folders and the MCP tool are read-only; authoring goes through `write_note` / `set_frontmatter`, whose deep-merge already preserves unknown `hiker.*` siblings and top-level keys)

A new `core::queries` module owns parse/serialize of the `hiker.query` block, compile of the filter to SQL, and a `run_query` entry point returning resolved note rows (path, title, mtime). Smart folders, the MCP tool, and any later consumer all call `run_query` — one compile path, so no two surfaces can disagree about what a query matches. A malformed filter (unknown clause, value outside the grammar) is a loud parse error: the query-doc surfaces an explicit error state wherever it renders, never a silent empty or match-everything fallback.


## Filter grammar

Closed clause set, fixed combinators. Each top-level clause must hold (AND); a list value inside a clause matches any element (OR). There is no OR across clauses, no negation of groups, no user-written joins, no functions in v1 — wanting more expressiveness is the signal to grow the grammar deliberately, not to open an escape hatch. [query-filter-grammar]
status:: done
implements:: [[code:hiker/queries/parse_filter]], [[code:hiker/queries/compile_query]], [[code:hiker/store/dto/MetaFilter]], [[code:hiker/frontmatter/iso_date_epoch]]
verifies:: [[code:hiker/queries/tests/compile_maps_each_clause_to_bound_predicates]], [[code:hiker/queries/tests/run_query_combines_clauses_end_to_end]], [[code:hiker/store/tests/query_notes_equals_value_list_is_any_of]], [[code:hiker/store/tests/query_notes_board_membership_filter]], [[code:hiker/frontmatter/tests/iso_date_epoch_accepts_dates_and_datetimes]]
note:: closed grammar: kind / fields (eq, exists, min/max incl. dates) / tags / path glob / board membership; AND of clauses, OR within; compiles to bound-parameter SQL over `note_meta` + `board_cards`. Store extensions landed: `MetaFilter::Equals` carries a value list (`value IN (...)`), `MetaFilter::Board` is the `board_cards` EXISTS, `NoteQuery::path_glob` the path GLOB; the date mirror lands in `frontmatter::scalar_field` via `iso_date_epoch` (rows indexed before it backfill on their next re-ingest or a forced reindex — the unchanged-content short-circuit skips `note_meta`). The board `category` form is live now that [[spec:kind-column-state-map]] landed: it compiles to a column-name set through the board's kind's mapping (`queries::BoardScope::Category` + `store_category_columns`); field ordering picks `MetaNum` vs `MetaText` by a one-probe `Store::meta_key_has_num`

| clause | matches | compiles to |
|---|---|---|
| `kind: <v \| [v]>` | `hiker.kind` equality | sugar for a `fields` eq on `hiker.kind` |
| `tags: <v \| [v]>` | tag contains (any-of) | sugar for a `fields` eq on `tags` |
| `path: <glob>` | vault-relative note path | `n.path GLOB ?` (generalizes `query_notes`' folder prefix) |
| `fields: [{ key, eq \| exists \| min/max }]` | one comparison per entry, each its own AND clause | an `EXISTS` subquery against `note_meta` per entry |
| `board: { path, column? \| category? }` | note is a card on the board / in the column | an `EXISTS` subquery against `board_cards` |
| `order` / `limit` | result shaping, not filtering | the existing `NoteOrder` / `LIMIT` paths in `query_notes` |

Field comparison forms (the closed set):

- **`eq`** — string equality; a list value is an OR (`value IN (...)` inside the `EXISTS`). Because `note_meta` explodes list-valued frontmatter to one row per element, `eq` against a list-valued key *is* "list contains" — no separate operator. To AND two conditions on the same key (note has tag a *and* tag b), write two `fields` entries.
- **`exists`** — the key is present at all.
- **`min` / `max`** — inclusive range over `note_meta.num`, the numeric mirror (`MetaFilter::NumRange`). Values are YAML numbers or ISO-8601 date strings; dates encode as epoch seconds (date-only = midnight UTC). Today `frontmatter::flatten` fills `num` only for YAML numbers and bools (`core/src/frontmatter.rs`, `scalar_field`) — date support extends that mirror to recognize ISO-date-shaped strings, with `value` keeping the string verbatim. Existing rows pick the mirror up on their next re-ingest (or a forced reindex); no schema change, `num` already exists with a `(key, num)` index.

**Board membership** joins through the derived [[spec:board-cards-derived-table]]: `EXISTS (SELECT 1 FROM board_cards b WHERE b.card_note_path = n.path AND b.board_path = ? [AND b.column_name = ?])`. The board is identified by its vault-relative path (path is identity). Freeform cards never appear in `board_cards`, so a board clause only ever matches note cards — consistent, since a freeform card isn't a note ([[spec:board-freeform-card]]). The `category` form — "in a column whose mapped state carries category Z" — rides the column-to-state mapping ([[spec:kind-column-state-map]]): it resolves at compile time by reading the board's kind's mapping and expanding the category to the matching column-name set (`column_name IN (...)`), so the SQL stays on the existing table. An empty expansion matches nothing; a board whose `hiker.kind` isn't a registered kind with a mapping is a loud compile error, never a silent empty result. Column names themselves stay arbitrary per-board strings ([[spec:board-column-model]]); only the mapping gives them cross-board meaning.

The compile target is the [[spec:store-note-query]] surface: every clause becomes a bound parameter (never interpolated), skipped notes are excluded, and the existing `MetaFilter` set (`Equals` / `Exists` / `NumRange`) covers everything except two store-side extensions — value-list OR inside `Equals`, and the `board_cards` membership filter. Ordering reuses `NoteOrder` (mtime / path / a field's indexed value — numeric mirror when present, text otherwise).


## Smart folders

Vault mode ([[spec:vault-view-mode]]) renders each query-doc as a **smart folder**: a folder-like node whose children are the query's current matches, nested in the composed lens alongside the other metadata-derived groupings. [smart-folder-view]
status:: done
implements:: [[code:hiker/queries/smart_folders]], [[code:hiker/vault_view/tree/build_smart_folder_nodes]]
verifies:: [[code:hiker/queries/tests/smart_folders_enumerate_run_and_surface_errors]], [[code:hiker/vault_view/tree/tests/smart_folder_members_are_virtual_and_header_is_consumed]], [[code:hiker/vault_view/tree/tests/smart_folder_error_renders_loud_error_row]]
note:: Vault-mode virtual folder per query-doc: header row is the query-doc (query glyph, match count, opens the doc), children are matches in query order; membership virtual + visually marked (italic + muted "ref" badge), members deliberately not consumed so they keep appearing in their other groupings; read-only in v1; recomputed from the store on render like the sibling derived projections (`core::queries::smart_folders`: one indexed kind lookup + `run_query`, never a vault walk); a failed query renders a loud error child. Rows go through the standard `render_node` path (`attach_note_item_menu`, hover preview, click / mod-click open, drag source carrying the vault-relative path per [[spec:drag-note-payload]])

- **The header row is the query-doc itself.** It carries a query glyph; click opens the query-doc in the editor (it is a plain note — no dedicated tab kind in v1), expand/collapse reveals the matches. Match count renders on the row.
- **Membership is virtual.** A matching note stays at its real path — and in every other grouping that claims it — and *also* appears under the smart folder, visually marked (italic plus a badge) as a reference rather than a residence. Smart folders share one visual language for "not a real folder" with the rest of Vault mode's virtual groupings ([[spec:vault-view-source-groups]]), so the lens doesn't grow a second marking vocabulary.
- **Read-only membership in v1.** Drag-into-a-smart-folder is undefined for arbitrary queries (which write would satisfy a path glob, or a board clause?), and Vault mode is a read-only lens with no placement authority ([[spec:vault-view-readonly-lens]]) — nothing is persisted, no `hiker.placement`, no remembered drags. The reserved write affordance is [[spec:smart-folder-drag-sets-field]], below.
- **Member rows keep the full item grammar** (`interaction.md`): click opens in the preview slot ([[spec:click-opens]]), mod-click opens sticky ([[spec:modclick-sticky]]), hover previews ([[spec:hover-preview-universal]]), right-click is the note's full menu ([[spec:rightclick-menu-always]]), and the row is a drag *source* carrying the vault-relative path ([[spec:drag-note-payload]]) — dragging out of a smart folder onto a board or canvas works like dragging from anywhere; only dropping *in* is undefined.
- **Refresh is event-driven like every other derived view**: membership is recomputed from the store on render and on indexer events, running against indexed `note_meta` / `board_cards` — never a vault walk, never a cached membership that can drift. Matches order per the query's `order` clause.

Tradeoff: a smart folder is a live query result, not a curated list — the symmetric opposite of a board, whose membership is reference-owned and never derived from note metadata (`kanban.md` "Out of scope"). Both exist on purpose; neither subsumes the other.


## The query MCP tool

A single generic read tool rounds out the layer — for agents, one tool over the saved-query primitive replaces a family of bespoke enumeration tools. [query-mcp-tool]
status:: done
implements:: [[code:hiker/handler/dispatch/queries]], [[code:hiker/config/sections/McpToolsConfig#query_enabled]]
note:: `query(query_doc | filter, select?, limit?)` MCP read tool; `mcp-server/src/handler/dispatch/queries.rs::run_query_tool` routes through `core::queries::run_query` (same compile path as smart folders); exactly-one-of `query_doc`/`filter` enforced (`invalid_params`), limit default 100 / cap 500 with a stricter in-query limit winning; `1002` for a missing or non-query `query_doc`, `invalid_params` for a filter outside the grammar; per-tool toggle `[mcp.tools].query_enabled`; advertised in `router.rs` + in-process `dispatch_tool` · evidence: smoke `query_tool_runs_inline_filter_and_saved_doc`, `query_tool_error_codes`, `query_tool_respects_per_tool_disable`, `server_lists_expected_tools`

- **`query(query_doc?: rel_path, filter?: object, select?: [string], limit?: number)`** — exactly one of `query_doc` (run a saved query-doc) or `filter` (an inline filter in the same closed grammar — same parser, same clause set, nothing extra). `select` packs the named frontmatter keys into each returned row's `fields` (the existing `query_notes` projection); `limit` defaults to 100, capped at 500. Returns `{ rows: [{ path, title, mtime, fields }] }`.
- Routes through `core::queries::run_query` — the same compile path smart folders use, so agent and UI results never diverge.
- Read-only; does **not** populate the per-session read set (only `get_note` does, per [[spec:mcp-read-before-write]]).
- Standard MCP conventions apply (`mcp.md`): per-tool toggle `[mcp.tools].query_enabled` ([[spec:mcp-tool-toggles]]), advertised through the router ([[spec:mcp-dynamic-capabilities]]), errors per [[spec:mcp-error-model]] — `1002 note_not_found` for a missing or non-query `query_doc` path, `invalid_params` for a filter outside the grammar or for both/neither of `query_doc` and `filter`.

Creating and editing query-docs needs no dedicated write tool: a query-doc is a plain note, so `write_note` / `edit_note` / `set_frontmatter` already cover it, with the usual staging behavior under review mode ([[spec:agent-write-review-mode]]).


## Deferred

- **Drag-into-smart-folder sets the field.** When — and only when — a query's filter is a single positive field-equality clause (`{ key, eq: <one value> }`, including the `kind:`/`tags:` sugar), drag-in has an unambiguous inverse: set that field on the dropped note. Deferred from v1 on two grounds: the affordance is undefined for every other query shape, and Vault mode currently has no write gestures at all ([[spec:vault-view-readonly-lens]]) — a single drop target that writes would break the lens's "derived, never owns" contract before a general story exists for marking which virtual rows accept drops. Revisit once the lens grows any write affordance. [smart-folder-drag-sets-field]
status:: planned
note:: deferred — drag-in = set-field, only for single positive field-equality queries; blocked on Vault mode's read-only contract, not on the query layer


## Out of scope

- **Body-content matching.** The grammar filters metadata and membership, not text — lexical/semantic content retrieval is `search.md`'s job, and the two compose at the surface level (run a search, run a query), not inside one language.
- **Board-lane presentation of a query** ("smart columns"). A later layer over the same primitive; boards stay curated reference lists ([[spec:board-card-references]]).
- **Automation.** The rules layer (`rules.md`) references a query-doc (or an inline filter) as its condition — reusing this grammar so no second condition language appears ([[spec:rule-condition-reuses-queries]]) — but triggers and actions are that spec's problem.
- **Custom grouping lenses.** [[spec:vault-view-saved-lenses]] (group *all* notes by a rule) is a different shape from a smart folder (one virtual folder of matches); it stays a Vault-view deferral.
- **Cross-vault queries.** Paths aren't unique across vaults — the same boundary as boards and trails.
