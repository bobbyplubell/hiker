# Search

Vault-wide retrieval over note content. Lexical (full-text) and semantic (embedding) results in one panel, type-ahead, with the user picking which signal sources are active. Lands the v2 milestone from `design.md`.

The headline decisions:

- **The right-hand panel is the discovery panel.** Search and related-notes both live there as collapsible sections. Renames (in framing, not the toggle slug — `panel-toggle-buttons` already exists) the v1 "related" panel to its more honest job: vault-wide retrieval surfaces the user might want to consult, of which related-notes was always just one. Future surfaces (landmarks, collections, clustering hints) plug in as additional sections following the same shape. [search-discovery-panel]
- **Two mode toggles next to the input — semantic and lexical.** Both on by default = hybrid via reciprocal rank fusion. One on = single-source results, no fusion step. Both off = input disabled with a "pick a mode" hint; explicit failure beats silent fallback. State persists per-vault. [search-mode-toggles, search-modes-both-off-disabled, search-mode-state-persisted]
- **Type-ahead with 250ms debounce.** Each keystroke advances an epoch number; in-flight queries that come back stamped stale get dropped before render. Same pattern the related-notes refresh already uses. [search-typeahead-debounce]
- **SQLite FTS5 is the lexical backend, behind a swappable trait.** One database file, no second index lifecycle, no separate schema-version dance. Tantivy can swap in later as a single targeted change if ranking quality ever becomes a complaint — the engine trait is the seam. [search-engine-trait, search-fts5-lexical]
- **Ctrl-Space focuses the search input on every platform.** Same on macOS — Cmd-Space is Spotlight; we don't take it. Opens the discovery panel if collapsed. The keybind registry takes precedence over the editor's own Ctrl-Space (start-autocomplete) binding inside the editor. [search-keybind-ctrl-space]


## Discovery panel

The right-hand panel is one column with a fixed input row at the top and a stack of collapsible sections below. v2 ships with two sections; the shape is built so adding more is mechanical.

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
- **Non-empty query** — both sections visible, both expanded by default. Chevron collapses either; state persists per-vault via `settings-write-back` (`settings-section-vault`). [search-section-collapsible]
- **Section headers carry live counts.** "Search results (8)" updates as type-ahead returns; "Related notes (5)" updates on active-file change. Subtle in-section spinner while a query is in flight. [search-section-counts, search-loading-shimmer]
- **Related stays bound to the active editor file** even when search is active. Searching is exploration, editing is anchored. [search-related-stays-bound]

The toggle button on the editor toolbar still flips the panel open/closed (existing `panel-toggle-buttons`); only the panel's contents change.


## The search input + mode toggles

Text input pinned at panel top. To its right, two icon-only toggle buttons matching the editor toolbar's treatment (sidebar wheel, discovery magnifying glass, view eye): [search-bar-input, search-mode-toggles]

- **Semantic toggle** — brain glyph. Tooltip "Semantic search."
- **Lexical toggle** — `Aa` glyph (typographic, signaling "match these letters"). Tooltip "Lexical search."

Pressed states show which modes are active.

Mode rules:

- **Both on (default).** Both backends run. Results fused via reciprocal rank fusion (k=60), grouped by note. [search-rrf-fusion]
- **One on.** Only that backend runs, native ranking (BM25 for lexical, cosine for semantic). No fusion, no second backend warmed up.
- **Both off.** Input visually disabled (greyed out, no focus ring) with tooltip "Enable Semantic or Lexical to search." Ctrl-Space still focuses the input so the hint is visible. [search-modes-both-off-disabled]

State persists via `settings-write-back` to a new `search.modes` config section (`semantic`, `lexical`, both default true). Eligible-key set in `core::config::ELIGIBLE_*` grows two entries. [search-mode-state-persisted]


## Mode option menus

**Right-click** (long-press / two-finger tap on macOS) on either toggle opens a popover anchored under the button with mode-specific options. Left-click still flips on/off. [search-mode-options-menu]

Implementation: `contextmenu` handler reuses the existing `openContextMenu` helper (View menu / tree-actions menu / sort menu). Rows are checkable booleans, sliders, or numeric inputs. Closes on outside-click and Esc.

Both menus persist immediately via `settings-write-back` to `search.lexical.*` / `search.semantic.*`. Eligible-key set grows accordingly. Defaults preserve current behavior.


### Lexical options menu

Anchored under the `Aa` toggle. Rows: [search-lexical-options]

- **Case sensitive** (default off) — post-filter top-25 hits in Rust for case-sensitive substring match. FTS5's `unicode61` tokenizer is case-folded at index time and can't be reconfigured per-query; the post-filter is cheap (few KB of text) and avoids a second FTS5 table. [search-lexical-case-sensitive]
- **Match diacritics** (default off) — same shape as case sensitivity; tokenizer config `remove_diacritics 2` strips at index time, so a per-query flip is a post-filter pass (Unicode NFD-aware substring match against raw chunk text). Default off matches today's behavior and the common case for English-leaning users. [search-lexical-diacritic-sensitive]
- **Prefix match** (default off) — rewrite each whitespace-separated query token to `token*` before FTS5's `MATCH` (so `auto` matches `automation`). FTS5 supports the prefix operator natively, so it's a query-string transform, no schema change. Default off keeps current exact-token matching to avoid silently changing precision. [search-lexical-prefix-match]
- **Phrase mode** (default off) — wrap the entire query in double quotes before FTS5, forcing exact-phrase matching. When off, FTS5's standard implicit-AND token semantics apply. Mutually exclusive with prefix match in practice (FTS5 ignores `*` inside a quoted phrase); the menu doesn't enforce that — checking both just yields phrase semantics, with a subtle hint in the prefix-match row tooltip. [search-lexical-phrase-mode]

No stemming toggle: FTS5's tokenizer is fixed at table-creation time, so a per-query flip would need a second porter-stemmer FTS5 table (double write cost + disk). Stemming, if wanted, is a one-line tokenizer change at a schema bump, not a runtime knob.


### Semantic options menu

Anchored under the brain toggle. Rows: [search-semantic-options]

- **Minimum similarity** (slider `0.00`–`0.95`, 0.05 steps, default `0.00`) — hits below threshold dropped before fusion (or before render in single-mode). Default `0.00` = no filter, current behavior. Useful when the embedder returns weak global matches; raising the floor trades "always 20, some weak" for "fewer-but-stronger, sometimes empty." Empty-after-filter shows a hint row "No results above threshold X.XX — lower the threshold or refine your query." [search-semantic-min-similarity]
- **Top-k override** (numeric input, 5–100, default 25, matches `PER_BACKEND_TOP_K`) — affects only the semantic side; lexical stays at 25. Enables a wider semantic net without touching the global budget. Capped at 100 to keep the panel responsive (sqlite-vec scan cost). [search-semantic-top-k-override]
- **Recency bias** (radio `Off` / `Mild` / `Strong`, default Off) — fuses mtime-rank into the semantic score via the same RRF shape as cross-mode fusion: `score = 1/(k + sim_rank) + w · 1/(k + recency_rank)`, where `w` is `0.0` / `0.5` / `1.0` and `recency_rank` is the note's position sorted by `notes.mtime DESC`. Default Off — hiker doesn't otherwise privilege recent files in retrieval; recency boost should be a deliberate choice, not a silent default. [search-semantic-recency-bias]

No embedder/model picker here: switching embedders invalidates every cached vector, so it's a vault-level decision in `embedder-config-section` (config + reindex), not a per-query toggle.


## Type-ahead

Search runs as the user types. Mechanic:

- **Debounce 250ms.** Keystroke schedules a query 250ms out; subsequent keystrokes cancel and reschedule. Empty query collapses results immediately, no debounce. [search-typeahead-debounce]
- **Epoch / cancel-in-flight.** Each query carries a monotonically-increasing epoch. The command runs both backends in parallel; stale-epoch results dropped on the frontend before render. Mirrors the related-notes file-switch cancel pattern.
- **Lexical returns near-instantly** (sqlite). **Semantic embeds the query string** on the existing `spawn_blocking` pool (`embedder-spawn-blocking`), tens of ms with bge-small warm. Both run in parallel; panel renders each section as it arrives — lexical may paint a beat before semantic, covered by section spinners. [search-query-embed-spawn-blocking]


## Result rendering

One row per *note*, not per chunk. The query may match many chunks within a note; the row shows the highest-ranked chunk as snippet. Mirrors `related-notes-snippet` and matches `design.md`'s "fuse → group by parent note" rule. [search-result-grouped-by-note]

Rows are rendered as cards, not flat list items. Search results and related-notes share the same card primitive — both surfaces show the matched chunk excerpt, the answer to "why is this note here" lives inside the card next to the title. The two sections differ only in their headers and the query that fills them. [discovery-result-card]

Card anatomy (top to bottom):

- **Title row.** Basename or first H1 if present (same logic across both surfaces). Right-aligned muted score, single line, ellipsized when long.
- **Path subtitle.** Vault-relative path, muted, single line, ellipsized from the left so the filename stays visible. Lets the user disambiguate same-named notes in different folders without opening them.
- **Heading-path breadcrumb.** From the matched chunk's `heading_path` (`chunker-heading-path`). Omitted when the chunk has none. Single line, ellipsized when long.
- **Excerpt.** ~2–3 lines from the matched chunk. Lexical hits use FTS5's `snippet()` for highlighting; semantic-only hits show plain context centered on the chunk start. Wraps within the card; no horizontal scroll. [search-result-row]

Cards are visually distinct from flat list rows: subtle bordered frame, small inner padding, vertical spacing between cards. Hover lifts the background tone; the active focus ring sits on the card itself for keyboard nav. The card is the whole click target.

**Click → open + scroll-to-chunk.** Opens the file and scrolls to the matched chunk via its stored line range (`cmd-chunks-for-path`). [search-result-click-opens-chunk]

**Result budget.** Each backend returns top 25 internally; fused list shows 20. RRF benefits from a tail of below-the-fold candidates per side. Fixed for v2; configurability waits until MCP needs different budgets. [search-result-budget]


## Keyboard model

- **Ctrl-Space** — focuses the search input; opens the discovery panel if collapsed. Same on macOS — we deliberately don't take Cmd-Space (Spotlight). Registers at document level (`keybind-registry` pattern) with precedence to win over the editor's own `Ctrl-Space` (start-autocomplete) binding inside the editor. Hiker doesn't lean on autocomplete in v2; revisit if a wikilinks-completion feature ever needs the binding back. [search-keybind-ctrl-space]
- **↑ / ↓** — moves the active result within whichever section has focus. ↑ at the top of Related jumps to the bottom of Search results; ↓ at the bottom of Search results jumps to the top of Related. Stops at the panel boundaries.
- **Enter** — opens the focused result.
- **Tab** — moves focus from input → search results → related → out of panel.
- **Esc in the input** — clears the query and collapses the search-results section. Esc in a result list returns focus to the input.

[search-keyboard-nav]


## Engine architecture

Two traits in `core::search`. The query pipeline composes them; the panel doesn't know which backend is which beyond the toggle state.

```rust
pub trait LexicalEngine: Send + Sync {
    fn upsert_chunk(&self, chunk_id: &ChunkId, text: &str) -> Result<(), SearchError>;
    fn delete_chunks_for_note(&self, note_id: &NoteId) -> Result<(), SearchError>;
    fn query(&self, q: &str, top_k: usize) -> Result<Vec<LexicalHit>, SearchError>;
    fn version(&self) -> &str;
}

pub trait SemanticEngine: Send + Sync {
    fn query(&self, embedding: &[f32], top_k: usize) -> Result<Vec<SemanticHit>, SearchError>;
    fn version(&self) -> &str;
}
```

Both traits live alongside the existing `Embedder` trait (`embedder-module-discipline`) and follow the same pattern: trait bound for the engine, concrete impls live in submodules (`core::search::fts5`, `core::search::semantic`). Adding a new lexical backend is one new module + one selection point in the search pipeline. [search-engine-trait]

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

`content='chunks'` makes FTS5 a contentless / external-content table, so the indexed text isn't duplicated — the virtual table just stores tokens and offsets pointing at `chunks.text`. Saves ~half the on-disk size that a content-storing FTS5 table would cost. [search-fts5-schema]

Writes ride alongside the existing chunk upsert in `core::store` — wherever an `ingest-tx-upsert` transaction inserts/updates rows in `chunks`, it also writes to `chunks_fts`. Same transaction, same atomicity guarantees. Deletes via `ingest-delete-cascade` similarly extend to clear the FTS rows.

Ranking and snippeting:

- **BM25** is the default ranking — `ORDER BY bm25(chunks_fts)` returns the standard FTS5 score. [search-fts5-bm25-snippet]
- **Snippets** use FTS5's built-in `snippet()` aux function with a window size of ~64 tokens, returning HTML-escaped text with `<mark>` highlights. The TS side renders the marks as styled spans.

The new FTS5 virtual table bumps the on-disk schema to v3. Same fail-loud policy as `store-version-fail-loud` — version mismatch aborts with a clear message; user runs an explicit reindex (existing `reindex-rebuild-action`) to migrate. No silent migration, same as the rest of `core::store`. [search-rebuild-on-schema-bump]


### Semantic: existing chunk_vecs

The semantic backend reuses the existing `chunk_vecs` virtual table (sqlite-vec, populated by `ingest-tx-upsert`). No new structure, no second embedding pass — we already have per-chunk embeddings; querying by embedding similarity is what `related-notes-query` already does, just keyed on a different vector. The `SemanticEngine` impl is a thin wrapper over an existing `Store::knn_chunks` query. [search-semantic-existing-vecs]

Query embedding goes through the same `Embedder` trait already loaded for ingest. No model-swap, no warm-up cost beyond the first query after vault open.


### Fusion

When both modes are on, results combine via reciprocal rank fusion:

```
score(note_id) = Σ over each backend that returned the note:
                   1 / (k + rank_in_that_backend)   where k = 60
```

Group-by-note happens *before* fusion: lexical hits are first reduced to one-row-per-note (taking the best chunk per note), same for semantic, then the two reduced lists fuse on `note_id`. The chunk shown in the panel is the best chunk from whichever backend ranked the note highest. RRF with k=60 is the standard choice and works well without per-backend score normalization (BM25 and cosine aren't on the same scale). [search-rrf-fusion]


## Scope

v2 searches the whole vault. No folder scope, tag scope, or lifecycle filters (`archive` / `redact` / `retire` from `design.md` aren't implemented yet). [search-vault-scope-only]

- **Skipped notes** (`cmd-file-index-state` — too-large, not-utf-8) aren't indexed, so aren't searchable. The tree-row Skipped marker is the user-facing signal.
- **`.hiker/` and ignored paths** (`watcher-ignore-hardcoded`) are already excluded from indexing, naturally excluded from search.
- **Trash entries** under `.hiker/trash/` aren't indexed, don't appear in results. The trash bin's row list (`tree-trash-flat-by-deleted`) is the dedicated surface — by design, search shows your live vault.


## Command surface

A single new command:

```rust
async fn search_vault(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,        // { semantic: bool, lexical: bool }
    lexical_opts: LexicalOpts, // { case_sensitive, diacritic_sensitive, prefix_match, phrase_mode }
    semantic_opts: SemanticOpts, // { min_similarity, top_k, recency_bias }
    epoch: u64,                // echoed back so the frontend can drop stale results
) -> Result<SearchResponse, HikerError>;
```

`SearchResponse { epoch, lexical_hits: Vec<NoteHit>, semantic_hits: Vec<NoteHit>, fused: Vec<NoteHit> }`. Frontend renders `fused` when both modes are on, else the relevant single list.

Both option structs use `#[serde(default)]` on every field, so older payloads decode to documented defaults — `settings-strict-load` discipline applies at config load only, not at the command boundary.

Returning all three buckets is deliberate: keeps the command flat and lets us add UI affordances later (e.g. "show me what each backend found separately") without a new command. Frontend ignores the buckets it doesn't need. [search-cmd]

Wires through `core::search::query()`, which composes the two engine traits. The command is a thin wrapper (~10 lines) per the layer-split rules in `design.md`.


## Module discipline

- `core::search` — engine traits, fusion, public `query` function. Zero host imports.
- `core::search::fts5` — `Fts5LexicalEngine` impl. The only place that touches FTS5 SQL.
- `core::search::semantic` — `SemanticEngine` impl over the existing `chunk_vecs` table.
- `core::store` — gains FTS5 writes alongside existing chunk writes; `chunks_fts` schema lives here next to the rest of the schema, rusqlite confined as before (`store-module-discipline`).

Adding tantivy later is: new file `core::search::tantivy.rs` implementing `LexicalEngine`, plus a selection point (config key or feature flag) in the engine factory. The FTS5 impl, the schema, and the existing call sites stay intact during the transition.


## Forward refs

The same `core::search::query` is what the MCP server's `search_notes` tool will call when v3 lands — the response shape is intentionally agent-friendly (stable chunk ids, snippet detail, per-note grouping). Budget control (`design.md` MCP section) maps onto the existing `top_k` argument. The MCP integration is its own spec; this doc just leaves the seam clean.


## Deferred

Slugs registered as `planned` so future work plugs in cleanly:

- **`search-folder-scope`** — restrict search to a vault subtree. Useful once vaults grow; trivial to bolt onto the FTS5 query (`MATCH ... AND path GLOB ?`) and the semantic query (path-prefix filter on the joined notes row).
- **`search-lifecycle-filters`** — exclude/include archived, redacted, retired notes. Lands when the lifecycle slugs from `design.md` (`hiker.archived` / `hiker.redacted` / `hiker.retired`) get implemented. Currently moot — none of those exist on disk yet.
- **`search-tag-scope`** — filter by frontmatter tag. Lands with the auto-tag enrichment stage (`design.md` enrichment pipeline).
- **`search-tantivy-swap`** — implement `LexicalEngine` over tantivy as an alternate backend. Single config key picks between FTS5 and tantivy. Triggered by ranking-quality complaints, not on a schedule.
- **`search-history`** — recent queries dropdown under the input. Cheap to add, defer until someone asks.
- **`search-result-snippet-context`** — expand a result row to show surrounding chunks. The chunk-level neighbors are already addressable via `chunker-heading-path` / chunk index; needs a UI affordance.
- **`search-multi-vault`** — `design.md` mentions a vault-level routing axis. Out of scope until multi-vault open is itself a feature.
- **`search-result-pin-as-collection`** — promote a result set to a saved collection (`design.md` collections). Lands with the collections feature; the search panel is the natural entry point.
- **`search-result-multi-select`** — checkbox-style selection on result rows + a select-all affordance in the section header. Backs the bulk-action slugs below; without it, the only target a bulk action has is "all current results." Selection state is per-query — clearing the input or running a new query drops it. [search-result-multi-select]
- **`search-bulk-action-tag`** — apply (or remove) a frontmatter tag across a result set. Two targets: the entire current result list, or the multi-select subset when `search-result-multi-select` is engaged. Routes through whatever the auto-tag enrichment stage settles on for tag writes (`design.md` enrichment pipeline) so bulk-from-search and per-note tagging share one code path. Confirm-before-apply with a count ("Tag 12 notes as `topic:rust`?"). Depends on tags being a real feature first — not load-bearing for v2. [search-bulk-action-tag]
- **`search-bulk-action-move`** — move all results (or the multi-select subset) into a target folder. Routes through `core::ops::move_note` per result so watcher suppression, index updates, and dirty-buffer-follow behavior come for free. Confirm-before-apply with a count + destination preview ("Move 8 notes to `archive/old-projects/`?"). Same shape as the tag bulk action, different terminal op. [search-bulk-action-move]
- **`search-authorship-filter`** — filter results by the authorship trichotomy from `design.md`'s Provenance index axis: `user-authored / agent-authored / imported`. UI shape: small pill row above the result list, multi-select, defaults to all-on. Cheap to add once `hiker.author:` is populated (the index already carries the field; the filter is a `WHERE` predicate). [search-authorship-filter]
- **`search-source-type-filter`** — filter results by source type — markdown, trail, pdf, epub, image, audio, archived-website, transcript, etc. Same UI shape as the authorship filter (pill row, multi-select, defaults to all-on). Type comes from `hiker.type:` in frontmatter (already part of the source-derived-notes shape). Visually pairs with the per-source-type tree icons seedling in `design.md`'s trails section — same axis, two consumers. [search-source-type-filter]


## Out of scope

- **In-file find / replace.** That's a within-buffer affordance the editor provides on its own. Different feature, different keybind (Cmd/Ctrl-F when it lands), no overlap with vault-wide search.
- **Search-and-replace across the vault.** Destructive bulk edit is its own feature with its own confirmation/undo story. Search is read-only.
- **Saved searches as durable infrastructure.** That's the collections feature in `design.md`'s auto-organization layer; this doc leaves the seam (`search-result-pin-as-collection`) and stops there.
- **Highlighting matches inside the open editor.** Implementable via editor decorations, but it's a different surface (editor view) and a different cognitive model (where am I, vs. what's in the vault). Possibly worth doing later — not a v2 concern.
- **Query syntax** beyond what FTS5 natively understands. v2 passes the user's input straight to FTS5's `MATCH`, which already handles `"phrase queries"`, `term1 OR term2`, `NEAR()`, etc. We don't add a hiker-specific DSL on top.
- **Single-row context-menu mutations** (right-click a result → delete this one note). The tree's context menu is the canonical place for per-note destructive actions; duplicating it in the discovery panel splits the mental model. *Bulk* actions across a whole result set or a multi-selected subset are explicitly *deferred*, not out of scope — see `search-bulk-action-tag` / `search-bulk-action-move` above.
