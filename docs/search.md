# Search

Vault-wide retrieval over note content. Lexical (full-text) and semantic (embedding) results in one panel, type-ahead, with the user picking which signal sources are active. Lands the v2 milestone from `design.md`. Quickfind by name/metadata ([[spec:search-quickfind-names-metadata]]) and source-type filtering ([[spec:search-source-type-filter]]) ride alongside the content path.


## Discovery panel

The right-hand panel is one column with a fixed input row at the top and a stack of collapsible sections below. v2 ships with two sections; the shape is built so adding more is mechanical. The right panel is renamed to Discovery, with the input plus collapsible search/related sections; [[spec:panel-toggle-buttons]] (existing) still flips the panel as a whole. [search-discovery-panel]
status:: done
note:: `app/src/panels/search.rs` (`show()`) + `app/src/panels/related.rs` — right panel renamed to Discovery with input + collapsible search/related sections; [[spec:panel-toggle-buttons]] (existing) still flips the panel as a whole

```
┌─ Discovery ─────────────────────┐
│  [search input]      [S] [L]    │  ← input + mode toggles
│                                 │
│  ▼ Search results (8)           │  ← present iff query non-empty
│    ┌─────────────────────────┐  │
│    │ Note title         0.42 │  │
│    │ folder/sub/note.md      │  │  ← shared card primitive
│    │ Heading > Subheading    │  │     [discovery-result-card]
│    │ …excerpt of the matched │  │
│    │ chunk wrapping a few    │  │
│    │ lines…                  │  │
│    └─────────────────────────┘  │
│    ...                          │
│                                 │
│  ▼ Related notes (5)            │  ← always present (active editor file)
│    ┌─ same card shape ───────┐  │
│    │ …                       │  │
│    └─────────────────────────┘  │
└─────────────────────────────────┘
```

Behavior:

- **Empty query** — search-results section not rendered; Related-notes takes the whole panel (identical to v1). [search-empty-collapses-results]
status:: done
note:: `app/src/panels/search.rs` (section visibility) — non-empty query reveals the search section, empty hides it; related section keeps the panel content as before
- **Non-empty query** — both sections visible, both expanded by default. Chevron collapses either; state persists per-vault via [[spec:settings-write-back]] ([[spec:settings-section-vault]]). [search-section-collapsible]
status:: superseded
note:: superseded by [[spec:feature-panel-single-accordion]]: Search/Related/Backlinks render directly under the workbench accordion header with no inner collapsible. The `search.sections.*_expanded` settings are inert
- **Section headers carry live counts.** "Search results (8)" updates as type-ahead returns; "Related notes (5)" updates on active-file change. [search-section-counts]
status:: done
note:: `app/src/panels/search.rs` + `app/src/panels/related.rs` — each section header shows its result count
- **In-flight spinner.** A subtle in-section spinner shows while a debounced query is in flight. [search-loading-shimmer]
status:: done
note:: `app/src/panels/search.rs` — minimal "…" spinner shown while a debounced query is in flight; styling can be upgraded later
- **Related stays bound to the active editor file** even when search is active. [search-related-stays-bound]
status:: done
note:: `app/src/panels/related.rs` — search wiring leaves the related-refresh path untouched; the related section still updates only on file-open and debounced-save

The toggle button on the editor toolbar still flips the panel open/closed (existing [[spec:panel-toggle-buttons]]); only the panel's contents change.


## The search input + mode toggles

Text input pinned at panel top. [search-bar-input]
status:: done
note:: `app/src/panels/search.rs` (search input) — text input pinned at panel top
implements:: [[code:hiker/search/search_input_and_run]], [[code:hiker/search/input_row]]

To its right, two icon-only toggle buttons matching the editor toolbar's treatment (sidebar wheel, discovery magnifying glass, view eye):

- **Semantic toggle** — brain glyph. Tooltip "Semantic search."
- **Lexical toggle** — `Aa` glyph (typographic, signaling "match these letters"). Tooltip "Lexical search."

Pressed states show which modes are active. [search-mode-toggles]
status:: done
note:: `app/src/panels/search.rs` (semantic/lexical toggles) — S/L pills next to the input; pressed state same as existing toolbar buttons
implements:: [[code:hiker/search/mode_toggles]]

Mode rules:

- **Both on (default).** Both backends run. Results fused via reciprocal rank fusion (k=60), grouped by note. [search-rrf-fusion]
status:: done
note:: `core/src/search.rs` (`rrf_fuse`) — k=60, applied when both modes on; group-by-note happens before fuse
- **One on.** Only that backend runs, native ranking (BM25 for lexical, cosine for semantic). No fusion, no second backend warmed up.
- **Both off.** Input visually disabled (greyed out, no focus ring) with tooltip "Enable Semantic or Lexical to search." Ctrl-Space still focuses the input so the hint is visible. [search-modes-both-off-disabled]
status:: done
note:: `app/src/panels/search.rs` (input disabled state) — both toggles off → input disabled + placeholder swaps to "Enable Semantic or Lexical to search"

State persists via [[spec:settings-write-back]] to a new `search.modes` config section (`semantic`, `lexical`, both default true). Eligible-key set in `core::config::ELIGIBLE_*` grows two entries. [search-mode-state-persisted]
status:: done
note:: `core/src/config.rs` (`SearchConfig`, `SearchModesConfig` defaults true/true) + eligible-key set entries `search.modes.{semantic,lexical}`; `app/src/panels/search.rs` seeds from settings on vault open and persists every flip
implements:: [[code:hiker/search/persist_search_setting]]


## Mode option menus

**Right-click** (long-press / two-finger tap on macOS) on either toggle opens a popover anchored under the button with mode-specific options. Left-click still flips on/off. [search-mode-options-menu]
status:: done
note:: `app/src/panels/search.rs` (lexical / semantic options menus, right-click on the mode toggles; slider / number / radio rows). Left-click still flips on/off; right-click opens the mode-specific popover anchored under the button
implements:: [[code:hiker/search/filters_menu]]

Implementation: `contextmenu` handler reuses the existing `openContextMenu` helper (View menu / tree-actions menu / sort menu). Rows are checkable booleans, sliders, or numeric inputs. Closes on outside-click and Esc.

Both menus persist immediately via [[spec:settings-write-back]] to `search.lexical.*` / `search.semantic.*`. Eligible-key set grows accordingly. Defaults preserve current behavior.


### Lexical options menu

Anchored under the `Aa` toggle. Rows: [search-lexical-options]
status:: done
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/sections/SearchLexicalConfig]], [[code:hiker/search/LexicalOpts]]
note:: `core/src/config.rs` (`SearchLexicalConfig`, `ELIGIBLE_VAULT` entries `search.lexical.*`); `core/src/search.rs` (`LexicalOpts`); `app/src/panels/search.rs` (lexical options menu) — flips persist via [[spec:settings-write-back]] to `search.lexical.*` and rerun the in-flight query
implements:: [[code:hiker/search/lexical_options_menu]]

- **Case sensitive** (default off) — post-filter top-25 hits in Rust for case-sensitive substring match. FTS5's `unicode61` tokenizer is case-folded at index time and can't be reconfigured per-query; the post-filter is cheap and avoids a second FTS5 table. [search-lexical-case-sensitive]
status:: done
implements:: [[code:hiker/search/impl#[`Fts5LexicalEngine<'a>`][LexicalEngine]query]]
note:: `core/src/search.rs` (`Fts5LexicalEngine::query` post-filter via `chunk_contains` over `c.text`) — pulled alongside the snippet so no second fetch needed
- **Match diacritics** (default off) — same shape as case sensitivity; a per-query flip is a post-filter pass (Unicode NFD-aware substring match against raw chunk text), since `remove_diacritics 2` strips at index time. [search-lexical-diacritic-sensitive]
status:: done
note:: `core/src/search.rs` (same `chunk_contains` post-filter, byte-level substring match against `c.text`) — diacritic-strict falls out of literal substring naturally; an NFD-aware variant can swap in if a user hits the case-sensitive-but-diacritic-insensitive cell
- **Prefix match** (default off) — rewrite each whitespace-separated query token to `token*` before FTS5's `MATCH` (so `auto` matches `automation`). FTS5's native prefix operator, so a query-string transform, no schema change. [search-lexical-prefix-match]
status:: done
note:: `core/src/search.rs` (`build_match_string` rewrites each whitespace token to `token*` before the FTS5 `MATCH`) — phrase mode wins when both flags are set, FTS5 ignores `*` inside a quoted phrase
- **Phrase mode** (default off) — wrap the entire query in double quotes before FTS5, forcing exact-phrase matching. When off, FTS5's standard implicit-AND token semantics apply. Mutually exclusive with prefix match in practice (FTS5 ignores `*` inside a quoted phrase); the menu doesn't enforce that — checking both just yields phrase semantics, with a subtle hint in the prefix-match row tooltip. [search-lexical-phrase-mode]
status:: done
implements:: [[code:hiker/search/impl#[`Fts5LexicalEngine<'a>`][LexicalEngine]query]]
note:: `core/src/search.rs` (`build_match_string` wraps the trimmed query in double quotes; embedded `"` is doubled per FTS5 string-literal rules)

No stemming toggle: FTS5's tokenizer is fixed at table-creation time. Stemming, if wanted, is a one-line tokenizer change at a schema bump, not a runtime knob.


### Semantic options menu

Anchored under the brain toggle. Rows: [search-semantic-options]
status:: done
implements:: [[code:hiker/config/sections/SearchSemanticConfig]], [[code:hiker/search/SemanticOpts]]
note:: `core/src/config.rs` (`SearchSemanticConfig`, `RecencyBias`, `ELIGIBLE_VAULT` entries `search.semantic.*`); `core/src/search.rs` (`SemanticOpts`); `app/src/panels/search.rs` (semantic options menu)
implements:: [[code:hiker/search/semantic_options_menu]]

- **Minimum similarity** (slider `0.00`–`0.95`, 0.05 steps, default `0.00`) — hits below threshold dropped before fusion (or before render in single-mode). Default `0.00` = no filter, current behavior. Useful when the embedder returns weak global matches; raising the floor trades "always 20, some weak" for "fewer-but-stronger, sometimes empty." Empty-after-filter shows a hint row "No results above threshold X.XX — lower the threshold or refine your query." [search-semantic-min-similarity]
status:: done
implements:: [[code:hiker/config/sections/SearchSemanticConfig#min_similarity]], [[code:hiker/search/SemanticOpts#min_similarity]], [[code:hiker/search/query]]
note:: `core/src/search.rs::query` drops hits with `score < min_similarity` before fusion; UI surfaces a 0.00–0.95 / 0.05-step slider in the right-click popover
- **Top-k override** (numeric input, 5–100, default 25, matches `PER_BACKEND_TOP_K`) — affects only the semantic side; lexical stays at 25. Enables a wider semantic net without touching the global budget. Capped at 100 to keep the panel responsive (sqlite-vec scan cost). [search-semantic-top-k-override]
status:: done
implements:: [[code:hiker/config/sections/SearchSemanticConfig#top_k]], [[code:hiker/search/SemanticOpts#top_k]]
note:: `core/src/search.rs::query` clamps the override to `[5, 100]` and routes it into `VecSemanticEngine::query` for the semantic side only; UI exposes a numeric input (default 25)
- **Recency bias** (radio `Off` / `Mild` / `Strong`, default Off) — fuses mtime-rank into the semantic score via the same RRF shape as cross-mode fusion: `score = 1/(k + sim_rank) + w · 1/(k + recency_rank)`, where `w` is `0.0` / `0.5` / `1.0` and `recency_rank` is the note's position sorted by `notes.mtime DESC`. Default Off. [search-semantic-recency-bias]
status:: done
implements:: [[code:hiker/config/sections/SearchSemanticConfig#recency_bias]], [[code:hiker/search/SemanticOpts#recency_bias]], [[code:hiker/search/query]]
note:: `core/src/search.rs::apply_recency_bias` pulls `notes.mtime` for each candidate in one bulk `IN (...)` query, blends sim-rank + recency-rank via the spec's RRF k=60 shape (weights 0.5 / 1.0 for Mild / Strong), then re-sorts. UI radio (Off / Mild / Strong) in the right-click popover

No embedder/model picker here: switching embedders invalidates every cached vector, so it's a vault-level decision in [[spec:embedder-config-section]] (config + reindex), not a per-query toggle.


## Type-ahead

Search runs as the user types. Mechanic:

- **Debounce 250ms.** Keystroke schedules a query 250ms out; subsequent keystrokes cancel and reschedule. Empty query collapses results immediately, no debounce. [search-typeahead-debounce]
status:: done
note:: `app/src/panels/search.rs` — 250ms debounce + monotonically-increasing search epoch; stale responses dropped before render. Empty query short-circuits without scheduling and bumps the epoch so any in-flight call drops
implements:: [[code:hiker/search/honour_debounce]]
- **Epoch / cancel-in-flight.** Each query carries a monotonically-increasing epoch; stale-epoch results dropped on the frontend before render. Mirrors the related-notes file-switch cancel pattern.
- **Lexical returns near-instantly** (sqlite); **semantic embeds the query string** on the existing `spawn_blocking` pool ([[spec:embedder-spawn-blocking]]), tens of ms with bge-small warm. Both run in parallel; panel renders each section as it arrives, covered by section spinners. [search-query-embed-spawn-blocking]
status:: done
note:: `app/src/search/mod.rs::fire_query` hands the whole fired query to `tokio::task::spawn_blocking` — query-string embed (via `indexer.embedder()`, the slow ONNX step) plus the lexical/vector core query, the federated-ZIM searches, and the recency/ext post-filters all run off the UI thread. The task ships a `SearchOutcome` (tagged with `query_epoch`) over an `mpsc` channel; `drain_results` applies it each frame and drops stale epochs (superseded by newer typing), so typing never blocks. A `Spinner` + "Searching…" hint shows while in flight. The `.zim` registry (`panels/zim.rs`) is a global `Mutex<ZimRegistry>` (was thread-local) so `federated_search`/`federated_fulltext_search` are callable from the worker. Runs alongside the 250ms [[spec:search-typeahead-debounce]]. Per-section incremental render (lexical painting before semantic) is not split out — one combined outcome lands at once
implements:: [[code:hiker/search/fire_query]], [[code:hiker/search/compute_outcome]], [[code:hiker/search/run_query]], [[code:hiker/search/drain_results]], [[code:hiker/search/SearchOutcome]], [[code:hiker/search/QueryParams]], [[code:hiker/search/impl#[QueryParams]from_state]]


## Result rendering

One row per *note*, not per chunk. The query may match many chunks within a note; the row shows the highest-ranked chunk as snippet. Mirrors [[spec:related-notes-snippet]] and matches `design.md`'s "fuse → group by parent note" rule. [search-result-grouped-by-note]
status:: done
note:: `core/src/search.rs` (`group_by_note`) — chunk-level engine output is collapsed to one row per note before fusion, matching `design.md`'s fuse → group rule
implements:: [[code:hiker/search/render_groups]], [[code:hiker/search/results_section]]

Rows are cards, not flat list items. Search results and related-notes share the same card primitive — both show the matched chunk excerpt; the two sections differ only in their headers and the query that fills them. [discovery-result-card]
status:: planned
note:: shared card primitive used by search results and related-notes rows in the discovery panel: title + score, path subtitle, heading-path breadcrumb, matched-chunk excerpt. Whole card is click target; consumes existing `NoteHit` / `RelatedHit` fields
implements:: [[code:hiker/search/result_card]], [[code:hiker/search/card_title_row]], [[code:hiker/search/DiscoveryHit]], [[code:hiker/search/CardAction]]

Card anatomy (top to bottom):

- **Title row.** Basename or first H1 if present (same logic across both surfaces). Right-aligned muted score, single line, ellipsized when long.
- **Path subtitle.** Vault-relative path, muted, single line, ellipsized from the left so the filename stays visible. Lets the user disambiguate same-named notes in different folders without opening them.
- **Heading-path breadcrumb.** From the matched chunk's `heading_path` ([[spec:chunker-heading-path]]). Omitted when the chunk has none. Single line, ellipsized when long.
- **Excerpt.** ~2–3 lines from the matched chunk. Lexical hits use FTS5's `snippet()` for highlighting; semantic-only hits show plain context centered on the chunk start. Wraps within the card; no horizontal scroll. [search-result-row]
status:: done
note:: `app/src/panels/search.rs` (`DiscoveryHit` rendering + snippet mark styling) — title + heading-path + snippet (match substrings styled as highlighted spans) + score
implements:: [[code:hiker/search/card_snippet]], [[code:hiker/search/MarkPart]]

Cards are visually distinct from flat list rows: subtle bordered frame, small inner padding, vertical spacing between cards. Hover lifts the background tone; the active focus ring sits on the card itself for keyboard nav. The card is the whole click target.

**Click → open + scroll-to-chunk.** Opens the file and scrolls to the matched chunk via its stored line range ([[spec:cmd-chunks-for-path]]). [search-result-click-opens-chunk]
status:: done
note:: `app/src/panels/search.rs` (open-hit handler) — clicking a result row opens the file then scrolls the editor (`editor-view` `ViewState`) to the chunk's `byte_start`
implements:: [[code:hiker/search/open_at_chunk]]

**Result budget.** Each backend returns top 25 internally; fused list shows 20 (the per-side tail feeds RRF). Fixed for v2; configurability waits until MCP needs different budgets. [search-result-budget]
status:: done
note:: `core/src/search.rs` — `PER_BACKEND_TOP_K = 25`, `FUSED_TOP_K = 20`; configurability deferred to MCP needs


## Keyboard model

- **Ctrl-Space** — focuses the search input; opens the discovery panel if collapsed. Same on macOS — we deliberately don't take Cmd-Space (Spotlight). Registers at document level ([[spec:keybind-registry]] pattern) with precedence over the editor's own `Ctrl-Space` (start-autocomplete) binding inside the editor. [search-keybind-ctrl-space]
status:: done
touches:: [[code:hiker/keybinds]]
note:: `app/src/keybinds.rs` (`search.focusInput` / `Ctrl-Space`) — registered so it wins over completion inside the editor, plus a cross-pane handler for the global case (checks `ctrlKey && !metaKey`, so Cmd-Space on macOS stays Spotlight). Both focus the search input, which expands the panel if collapsed and selects existing input contents. The keybind registry doesn't yet have a `scope` field — the global half lives outside the registry until that refactor lands
- **↑ / ↓** — moves the active result within whichever section has focus. ↑ at the top of Related jumps to the bottom of Search results; ↓ at the bottom of Search results jumps to the top of Related. Stops at the panel boundaries.
- **Enter** — opens the focused result.
- **Tab** — moves focus from input → search results → related → out of panel.
- **Esc in the input** — clears the query and collapses the search-results section. Esc in a result list returns focus to the input.

[search-keyboard-nav]
status:: done
note:: `app/src/panels/search.rs` + `app/src/panels/related.rs` (list keyboard handling) — ↑/↓ within a list, vertical wrap between Search-bottom ↔ Related-top only; Enter triggers the row's open action; Tab steps one row per list so input → search → related → out flows naturally; Esc in the input clears the query (or blurs if already empty), Esc on a row refocuses the input. ↓ from the input jumps to the first available result row
implements:: [[code:hiker/search/keyboard_nav]]


## Engine architecture

Two traits in `core::search`. The query pipeline composes them; the panel doesn't know which backend is which beyond the toggle state.

- **`LexicalEngine`** — `upsert_chunk` / `delete_chunks_for_note` / `query(q, top_k)` / `version`.
- **`SemanticEngine`** — `query(embedding, top_k)` / `version`.

Both live alongside the existing `Embedder` trait ([[spec:embedder-module-discipline]]) and follow the same pattern: trait bound for the engine, concrete impls in submodules (`core::search::fts5`, `core::search::semantic`). Adding a new lexical backend is one new module + one selection point in the search pipeline. [search-engine-trait]
status:: done
note:: `core/src/search.rs` — `LexicalEngine` + `SemanticEngine` traits with concrete impls in same file; tantivy swap-point preserved

Hybrid query (both modes on):

```
query_str
  ├─ Lexical::query(query_str, k=25)        → top-25 lexical hits
  └─ Embedder.embed(query_str) → SemanticEngine::query(embedding, k=25)
                                              → top-25 semantic hits
RRF fuse (k=60), group by note, take top 20
```

Single-mode query: skip the fusion step; pass the engine's native ranking through.


### Lexical: SQLite FTS5

A new `chunks_fts` virtual table inside the existing `index.db`. Schema mirrors the row identity of `chunks` so deletes/upserts cascade naturally:

```sql
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    content='chunks',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);
```

`content='chunks'` makes FTS5 an external-content table, so the indexed text isn't duplicated — it stores tokens and offsets pointing at `chunks.text`. [search-fts5-schema]
status:: done
touches:: [[code:hiker/store]]
note:: `core/src/store.rs` (`ensure_schema`) — contentless `chunks_fts` + sync triggers on `chunks` (insert/update/delete); schema bumped (now `SCHEMA_VERSION = 10` after later feature migrations)

Writes ride alongside the existing chunk upsert in `core::store` — wherever an [[spec:ingest-tx-upsert]] transaction touches `chunks`, it also writes `chunks_fts` (same transaction, same atomicity). Deletes via [[spec:ingest-delete-cascade]] similarly extend to clear the FTS rows.

The `Fts5LexicalEngine` in `core/src/search.rs` is the concrete `LexicalEngine` impl over this table. [search-fts5-lexical]
status:: done
note:: `core/src/search.rs` (`Fts5LexicalEngine`)

Ranking and snippeting:

- **BM25** is the default ranking — `ORDER BY bm25(chunks_fts)` returns the standard FTS5 score. [search-fts5-bm25-snippet]
status:: done
implements:: [[code:hiker/search/impl#[`Fts5LexicalEngine<'a>`][LexicalEngine]query]]
note:: `core/src/search.rs` (`Fts5LexicalEngine::query`) — `ORDER BY bm25` + `snippet(chunks_fts, 0, '<mark>', '</mark>', '…', 32)`; BM25 sign-flipped so higher = better matches the semantic side
- **Snippets** use FTS5's built-in `snippet()` aux function with a window size of ~64 tokens, returning HTML-escaped text with `<mark>` highlights. The TS side renders the marks as styled spans.

The new FTS5 virtual table bumps the on-disk schema to v3. Same fail-loud policy as [[spec:store-version-fail-loud]] — version mismatch aborts with a clear message; user runs an explicit reindex (existing [[spec:reindex-rebuild-action]]) to migrate. No silent migration, same as the rest of `core::store`. [search-rebuild-on-schema-bump]
status:: done
note:: covered by [[spec:store-version-fail-loud]]: opening a v2 db with this binary aborts with a version-mismatch error; user runs the existing reindex flow


### Semantic: existing chunk_vecs

The semantic backend reuses the existing `chunk_vecs` virtual table (sqlite-vec, populated by [[spec:ingest-tx-upsert]]). No new structure, no second embedding pass — we already have per-chunk embeddings; querying by embedding similarity is what [[spec:related-notes-query]] already does, just keyed on a different vector. The `SemanticEngine` impl is a thin wrapper over an existing `Store::knn_chunks` query. [search-semantic-existing-vecs]
status:: done
implements:: [[code:hiker/search/VecSemanticEngine]]
note:: `core/src/search.rs` (`VecSemanticEngine`) — thin wrapper over `Store::knn_chunks_on`

Query embedding goes through the same `Embedder` trait already loaded for ingest. No model-swap, no warm-up cost beyond the first query after vault open.


### Fusion

When both modes are on, results combine via reciprocal rank fusion:

```
score(note_id) = Σ over each backend that returned the note:
                   1 / (k + rank_in_that_backend)   where k = 60
```

Group-by-note happens *before* fusion: lexical hits are first reduced to one-row-per-note (best chunk per note), same for semantic, then the two reduced lists fuse on `note_id`. The chunk shown is the best chunk from whichever backend ranked the note highest. RRF with k=60 needs no per-backend score normalization (BM25 and cosine aren't on the same scale). [search-rrf-fusion]


## Scope

v2 searches the whole vault. No folder scope, tag scope, or lifecycle filters (`archive` / `redact` / `retire` from `design.md` aren't implemented yet). [search-vault-scope-only]
status:: done
note:: `core/src/search.rs` — engine queries hit every non-skipped chunk in the vault; no scope filter; folder/tag/lifecycle filters stay deferred per spec

- **Skipped notes** ([[spec:cmd-file-index-state]] — too-large, not-utf-8) aren't indexed, so aren't searchable. The tree-row Skipped marker is the user-facing signal.
- **`.hiker/` and ignored paths** ([[spec:watcher-ignore-hardcoded]]) are already excluded from indexing, naturally excluded from search.
- **Trash entries** under `.hiker/trash/` aren't indexed, don't appear in results. The trash bin's row list ([[spec:tree-trash-flat-by-deleted]]) is the dedicated surface — by design, search shows your live vault.


## Command surface

A single new command, `search_vault(query, modes, lexical_opts, semantic_opts, epoch) -> SearchResponse`:

- `modes` = `{ semantic, lexical }`; `lexical_opts` = `{ case_sensitive, diacritic_sensitive, prefix_match, phrase_mode }`; `semantic_opts` = `{ min_similarity, top_k, recency_bias }`. `epoch` is echoed back so the frontend can drop stale results.
- `SearchResponse { epoch, lexical_hits, semantic_hits, fused }` (all `Vec<NoteHit>`). Frontend renders `fused` when both modes are on, else the relevant single list. Returning all three buckets is deliberate — lets UI affordances ("what each backend found separately") land later without a new command.
- Both option structs use `#[serde(default)]` on every field, so older payloads decode to documented defaults — [[spec:settings-strict-load]] discipline applies at config load only, not the command boundary.

[search-cmd]
status:: done
note:: `core/src/search.rs` returning `SearchResponse { epoch, lexical_hits, semantic_hits, fused }`; both-modes-off / empty-query / model-not-ready short-circuit to empty buckets without erroring

Wires through `core::search::query()`, which composes the two engine traits. The command is a thin wrapper (~10 lines) per the layer-split rules in `design.md`.


## Module discipline

- `core::search` — engine traits, fusion, public `query` function. Zero host imports.
- `core::search::fts5` — `Fts5LexicalEngine` impl. The only place that touches FTS5 SQL.
- `core::search::semantic` — `SemanticEngine` impl over the existing `chunk_vecs` table.
- `core::store` — gains FTS5 writes alongside existing chunk writes; `chunks_fts` schema lives here next to the rest of the schema, rusqlite confined as before ([[spec:store-module-discipline]]).

Adding tantivy later: new file `core::search::tantivy.rs` implementing `LexicalEngine`, plus a selection point (config key or feature flag) in the engine factory. The FTS5 impl, schema, and existing call sites stay intact.


## Forward refs

The same `core::search::query` is what the MCP server's `search_notes` tool will call when v3 lands — the response shape is intentionally agent-friendly (stable chunk ids, snippet detail, per-note grouping). Budget control (`design.md` MCP section) maps onto the existing `top_k` argument. MCP integration is its own spec; this doc leaves the seam clean.


## Deferred

Slugs registered as `planned` so future work plugs in cleanly:

- **Quickfind by name/metadata** — a lightweight name/title/frontmatter lookup mode, a separate toggle from lexical/semantic; rides the wikilink picker's basename ranking + the structured index ([[spec:store-note-query]]), not the content path. [search-quickfind-names-metadata]
status:: planned
note:: lightweight name/title/frontmatter lookup mode, a separate toggle from lexical/semantic; rides the wikilink picker's basename ranking + the structured index ([[spec:store-note-query]]), not the content path
- **Folder scope** — restrict search to a vault subtree. FTS5 side `MATCH ... AND path GLOB ?`; semantic side path-prefix filter on the joined notes row. [search-folder-scope]
status:: planned
note:: restrict to vault subtree; deferred
- **Lifecycle filters** — exclude/include archived, redacted, retired notes. Lands when the `design.md` lifecycle slugs (`hiker.archived` / `hiker.redacted` / `hiker.retired`) exist. [search-lifecycle-filters]
status:: planned
note:: exclude/include archived/redacted/retired; waits on `design.md` lifecycle slugs
- **Tag scope** — filter by frontmatter tag. Lands with the auto-tag enrichment stage (`design.md` enrichment pipeline). [search-tag-scope]
status:: planned
note:: filter by frontmatter tag; waits on auto-tag enrichment
- **Tantivy swap** — implement `LexicalEngine` over tantivy as an alternate backend; a single config key picks between FTS5 and tantivy. [search-tantivy-swap]
status:: planned
note:: `LexicalEngine` impl over tantivy; triggered by ranking-quality complaints
- **Search history** — recent queries dropdown under the input. [search-history]
status:: planned
note:: recent queries dropdown under input
- **Result snippet context** — expand a result row to show surrounding chunks (neighbors already addressable via [[spec:chunker-heading-path]] / chunk index; needs a UI affordance). [search-result-snippet-context]
status:: planned
note:: expand row to show surrounding chunks
- **Multi-vault** — `design.md`'s vault-level routing axis. Out of scope until multi-vault open is itself a feature. [search-multi-vault]
status:: planned
note:: vault-level routing axis from `design.md`; needs multi-vault open first
- **Pin as collection** — promote a result set to a saved collection (`design.md` collections); the search panel is the natural entry point. [search-result-pin-as-collection]
status:: planned
note:: promote result set to a saved collection (`design.md` collections)
- **[[spec:search-result-multi-select]]** — checkbox selection on result rows + a select-all in the section header. Backs the bulk-action slugs below. Selection state is per-query — clearing the input or running a new query drops it. [search-result-multi-select]
status:: planned
note:: checkbox selection on result rows + select-all in section header; per-query state
- **[[spec:search-bulk-action-tag]]** — apply/remove a frontmatter tag across a result set or the multi-select subset. Routes through the auto-tag enrichment stage's tag-write path (`design.md` enrichment pipeline) so bulk-from-search and per-note tagging share one code path. Confirm-before-apply with a count ("Tag 12 notes as `topic:rust`?"). [search-bulk-action-tag]
status:: planned
note:: apply/remove a tag across all results or the multi-select subset; depends on auto-tag enrichment landing first
- **[[spec:search-bulk-action-move]]** — move all results (or the multi-select subset) into a target folder via `core::ops::move_note` per result (watcher suppression, index updates, dirty-buffer-follow come for free). Confirm-before-apply with a count + destination preview. [search-bulk-action-move]
status:: planned
note:: move all results (or multi-select subset) to a folder via `core::ops::move_note`; confirm-with-count
- **[[spec:search-authorship-filter]]** — filter results by the authorship trichotomy from `design.md`'s Provenance axis (`user-authored / agent-authored / imported`). Pill row above the result list, multi-select, defaults all-on; the filter is a `WHERE` predicate on `hiker.author:`. [search-authorship-filter]
status:: planned
note:: pill-row filter on user-authored/agent-authored/imported (`hiker.author:`); reads from Provenance index axis
- **[[spec:search-source-type-filter]]** — filter results by source type (markdown, trail, pdf, epub, image, audio, archived-website, transcript, …). Same pill-row UI as the authorship filter. Type comes from `hiker.type:` in frontmatter. Pairs with the per-source-type tree icons in `design.md`'s trails section — same axis, two consumers. [search-source-type-filter]
status:: planned
note:: include/exclude results by provenance (hand-written vs imported web/pdf/…, `import.md`); reads `hiker.provenance` via the structured index


## Out of scope

- **In-file find / replace.** A within-buffer editor affordance; different keybind (Cmd/Ctrl-F when it lands), no overlap with vault-wide search.
- **Search-and-replace across the vault.** Destructive bulk edit is its own feature with its own confirmation/undo story. Search is read-only.
- **Saved searches as durable infrastructure.** The collections feature in `design.md`; this doc leaves the seam ([[spec:search-result-pin-as-collection]]) and stops.
- **Highlighting matches inside the open editor.** Different surface (editor view), different cognitive model. Possibly later — not a v2 concern.
- **Query syntax** beyond what FTS5 natively understands. v2 passes input straight to FTS5's `MATCH` (already handles `"phrase queries"`, `term1 OR term2`, `NEAR()`); no hiker-specific DSL.
- **Single-row context-menu mutations** (right-click a result → delete this note). The tree's context menu is the canonical place for per-note destructive actions. *Bulk* actions are explicitly *deferred*, not out of scope — see [[spec:search-bulk-action-tag]] / [[spec:search-bulk-action-move]].
