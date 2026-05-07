# Search

Vault-wide retrieval over note content. Lexical (full-text) and semantic (embedding) results in one panel, type-ahead, with the user picking which signal sources are active. Lands the v2 milestone from `design.md`.

The headline decisions:

- **The right-hand panel is the discovery panel.** Search and related-notes both live there as collapsible sections. Renames (in framing, not the toggle slug — `panel-toggle-buttons` already exists) the v1 "related" panel to its more honest job: vault-wide retrieval surfaces the user might want to consult, of which related-notes was always just one. Future surfaces (landmarks, collections, clustering hints) plug in as additional sections following the same shape. [search-discovery-panel]
- **Two mode toggles next to the input — semantic and lexical.** Both on by default = hybrid via reciprocal rank fusion. One on = single-source results, no fusion step. Both off = input disabled with a "pick a mode" hint; explicit failure beats silent fallback. State persists per-vault. [search-mode-toggles, search-modes-both-off-disabled, search-mode-state-persisted]
- **Type-ahead with 250ms debounce.** Each keystroke advances an epoch number; in-flight queries that come back stamped stale get dropped before render. Same pattern the related-notes refresh already uses. [search-typeahead-debounce]
- **SQLite FTS5 is the lexical backend, behind a swappable trait.** One database file, no second index lifecycle, no separate schema-version dance. Tantivy can swap in later as a single targeted change if ranking quality ever becomes a complaint — the engine trait is the seam. [search-engine-trait, search-fts5-lexical]
- **Ctrl-Space focuses the search input on every platform.** Same on macOS — Cmd-Space is Spotlight; we don't take it. Opens the discovery panel if collapsed. The keybind registry overrides the CM6 default that would otherwise fire `startCompletion` inside the editor. [search-keybind-ctrl-space]


## Discovery panel

The right-hand panel is one column with a fixed input row at the top and a stack of collapsible sections below. v2 ships with two sections; the shape is built so adding more is mechanical.

```
┌─ Discovery ─────────────────────┐
│  [search input]      [S] [L]    │  ← input + mode toggles
│                                 │
│  ▼ Search results (8)           │  ← present iff query non-empty
│    ...                          │
│                                 │
│  ▼ Related notes (5)            │  ← always present (active editor file)
│    ...                          │
└─────────────────────────────────┘
```

Behavior:

- **Empty query** — search-results section isn't rendered. Related-notes takes the whole panel; identical to current v1 behavior, no regression. [search-empty-collapses-results]
- **Non-empty query** — both sections visible, both expanded by default. User can collapse either with the chevron; collapse state persists per-vault via `settings-write-back` (`settings-section-vault`). [search-section-collapsible]
- **Section headers carry live counts.** "Search results (8)" updates as the type-ahead returns; "Related notes (5)" updates when the active file changes. A subtle in-section spinner shows while a query is in flight. [search-section-counts, search-loading-shimmer]
- **Related stays bound to the active editor file** even when search is active. Conceptually: searching is exploration, editing is anchored. Rebinding Related to the top search hit was considered and rejected — it muddies "what is this section about" and steals an affordance the user already relies on. [search-related-stays-bound]

The toggle button on the editor toolbar still flips the panel open/closed (existing `panel-toggle-buttons`); only the panel's contents change.


## The search input + mode toggles

A single text input pinned at the top of the panel. To its right, two compact icon-only toggle buttons matching the editor toolbar's icon-only treatment (sidebar wheel, discovery magnifying glass, view eye): [search-bar-input, search-mode-toggles]

- **Semantic toggle** — brain glyph. Tooltip "Semantic search."
- **Lexical toggle** — `Aa` glyph (typographic, signaling "match these letters"). Tooltip "Lexical search."

Pressed/unpressed states show which modes are active; both pressed = hybrid via RRF, one pressed = single-source, neither pressed = disabled input + "pick a mode" hint per `search-modes-both-off-disabled`.

Mode rules:

- **Both on (default).** Lexical and semantic both run. Results fused via reciprocal rank fusion (k=60) and grouped by note. [search-rrf-fusion]
- **One on.** Only that backend runs. Results come straight from its native ranking (BM25 for lexical, cosine similarity for semantic). No fusion step, no second backend warmed up.
- **Both off.** Input is visually disabled (greyed out, no focus ring) with a tooltip hint "Enable Semantic or Lexical to search." Pressing Ctrl-Space in this state still focuses the input — the hint is then visible. [search-modes-both-off-disabled]

State persists via `settings-write-back` to a new `search.modes` config section (`search.modes.semantic = true`, `search.modes.lexical = true`). Default both-on; the eligible-key set in `core::config::ELIGIBLE_*` grows two entries. [search-mode-state-persisted]


## Mode option menus

Each mode toggle has a deeper config surface than on/off. **Right-click** (or long-press / two-finger tap on macOS) on either the Semantic or Lexical toggle opens a small popover anchored under the button with mode-specific options. Left-click still flips on/off — no behavioral overload of the primary affordance. [search-mode-options-menu]

Implementation: a `contextmenu` event handler on each toggle button reuses the existing `openContextMenu` popover helper (same one View menu / tree-actions menu / sort menu use). Rows are checkable booleans, sliders for numeric ranges, and small numeric inputs where appropriate. Opening the menu does *not* flip the mode — left-click is the only path that toggles enabled state. The menu closes on outside-click and on Esc, identical to the View menu.

Both menus persist every flip immediately via `settings-write-back` to `search.lexical.*` / `search.semantic.*`. Eligible-key set grows accordingly. Defaults preserve current behavior so existing users see no change until they reach into the menu.


### Lexical options menu

Anchored under the `Aa` toggle. Rows: [search-lexical-options]

- **Case sensitive** — when on, results are post-filtered to those whose chunk text contains the query as a case-sensitive substring. FTS5's `unicode61` tokenizer is case-folded at index time and can't be reconfigured per-query; doing the case check in Rust as a post-filter on the top-25 hits is cheap (a few KB of text) and avoids a second FTS5 table. Default off. [search-lexical-case-sensitive]
- **Match diacritics** — same shape as case sensitivity. The current tokenizer config `remove_diacritics 2` strips them at index time, so a per-query flip is implemented as a post-filter pass (Unicode NFD-aware substring match against the raw chunk text). Default off — diacritic-blind matching is the common case for English-leaning users and matches today's behavior. [search-lexical-diacritic-sensitive]
- **Prefix match** — when on, each whitespace-separated query token is rewritten to `token*` before being passed to FTS5's `MATCH` (so `auto` matches `automation`, `automatic`). FTS5 supports the prefix operator natively, so this is a query-string transform, no schema change. Default off — current FTS5 behavior is exact-token matching and we keep it as the default to avoid silently changing precision. [search-lexical-prefix-match]
- **Phrase mode** — when on, the entire query is wrapped in double quotes before being passed to FTS5, forcing exact-phrase matching. When off (default), FTS5's standard implicit-AND token semantics apply. Mutually exclusive with prefix match in practice (FTS5 ignores `*` inside a quoted phrase); the menu doesn't enforce that — checking both just means the user gets phrase semantics, and we surface a subtle hint in the prefix-match row tooltip. [search-lexical-phrase-mode]

Rejected for v2: a "stemming on/off" toggle. FTS5's tokenizer is configured at table-creation time; offering it as a per-query toggle would require a second FTS5 table with porter-stemmer tokenization, doubling write cost and on-disk size. If users actually want stemming, it's a one-line tokenizer change at schema bump, not a runtime knob.


### Semantic options menu

Anchored under the brain toggle. Rows: [search-semantic-options]

- **Minimum similarity** — slider from `0.00` to `0.95` in 0.05 steps. Hits with cosine similarity below the threshold are dropped before fusion (or before render in single-mode). Default `0.00` — no filter, current behavior. Useful when the embedder returns weak global matches that crowd out genuinely relevant results; raising the floor turns "always 20 results, some weak" into "fewer-but-stronger, sometimes empty." Empty results when the threshold filters everything out show a hint row "No results above threshold X.XX — lower the threshold or refine your query." [search-semantic-min-similarity]
- **Top-k override** — small numeric input, range 5–100, default 25 (matches `PER_BACKEND_TOP_K`). Override only affects the semantic side; lexical stays at 25. Enables the "I want a wider semantic net" workflow without touching the global budget. Larger values cost more sqlite-vec scan but the index is small; capped at 100 to keep the panel responsive. [search-semantic-top-k-override]
- **Recency bias** — three-way radio: `Off` / `Mild` / `Strong`. When on, fuses an mtime-rank into the semantic score using the same RRF shape as cross-mode fusion: `score = 1/(k + sim_rank) + w · 1/(k + recency_rank)`, where `w` is `0.0` / `0.5` / `1.0` for Off/Mild/Strong and `recency_rank` is the note's position when the result set is sorted by `notes.mtime DESC`. Default Off — hiker doesn't otherwise privilege recent files in retrieval, and a recency boost should be a deliberate user choice, not a silent default. [search-semantic-recency-bias]

Rejected for v2: an embedder/model picker in this menu. Choosing an embedder is a vault-level decision tied to the existing embedding index — switching mid-session would invalidate every cached vector. That belongs in `embedder-config-section` (a config-file restart, with the existing reindex flow), not a per-query toggle.


## Type-ahead

Search runs as the user types. Mechanic:

- **Debounce 250ms.** A keystroke schedules a query 250ms in the future; subsequent keystrokes within that window cancel and reschedule. Empty query collapses results immediately, no debounce. [search-typeahead-debounce]
- **Epoch / cancel-in-flight.** Each query carries a monotonically-increasing epoch. The Tauri command runs both backends in parallel; results that come back tagged with an epoch lower than the current input's epoch get dropped on the frontend before render. Mirrors the cancel pattern already used for related-notes refresh on file-switch.
- **Lexical returns near-instantly** (sqlite query). **Semantic requires embedding the query string** — runs on the existing `spawn_blocking` pool (per `embedder-spawn-blocking`), tens of ms with the bge-small embedder warm. Both run in parallel; the panel renders each section as it arrives, so lexical may paint a beat before semantic. Acceptable — the section spinners cover the gap. [search-query-embed-spawn-blocking]


## Result rendering

One row per *note*, not per chunk. The query may match many chunks within a note; the row shows the highest-ranked chunk as snippet. Mirrors `related-notes-snippet` and matches `design.md`'s "fuse → group by parent note" rule. [search-result-grouped-by-note]

Row anatomy:

- **Title.** Basename or first H1 if present (same logic as related-notes panel).
- **Heading-path breadcrumb.** From the matched chunk's `heading_path` (already stored, see `chunker-heading-path`). Subtle, single line, ellipsized when long.
- **Snippet.** ~2–3 lines from the matched chunk. Lexical hits use FTS5's `snippet()` for highlighting; semantic-only hits show plain context. [search-result-row]
- **Score.** Small, muted, right-aligned. Debug-friendly; users tend to ignore it but it's useful when ranking feels wrong.

**Click → open + scroll-to-chunk.** Clicking a row opens the file in the editor and scrolls so the matched chunk is visible. Uses the chunk's stored line range (already stable per `tauri-cmd-chunks-for-path`). [search-result-click-opens-chunk]

**Result budget.** Each backend returns its top 25 hits internally; the fused list shows 20. Rationale: RRF benefits from a tail of below-the-fold candidates from each side. Fixed for v2; configurability waits until MCP needs different budgets. [search-result-budget]


## Keyboard model

- **Ctrl-Space** — focuses the search input. Opens the discovery panel if it's collapsed. Same on macOS; we deliberately don't take Cmd-Space. The keybind registers at the document level (matching the `keybind-registry` pattern), with high enough precedence to win over CM6's default `Ctrl-Space → startCompletion` binding inside the editor. Hiker doesn't lean on autocomplete in v2; we can revisit if a wikilinks-completion feature ever needs the binding back. [search-keybind-ctrl-space]
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

v2 searches the whole vault. No folder scope, no tag scope, no lifecycle filters (`archive` / `redact` / `retire` from `design.md` aren't yet implemented anyway). [search-vault-scope-only]

Skipped notes (`tauri-cmd-file-index-state`'s "Skipped" — too-large, not-utf-8) aren't indexed and therefore aren't searchable. Their tree-row Skipped marker is the user-facing signal.

`.hiker/` paths and ignored paths (`watcher-ignore-hardcoded`) are already excluded from indexing, so they're naturally excluded from search.

Trash entries are stored under `.hiker/trash/` and therefore not indexed; they don't appear in search results. The trash bin's row list (`tree-trash-flat-by-deleted`) is the dedicated surface for finding trashed notes — by design, search results show your live vault.


## Tauri command surface

A single new command:

```rust
#[tauri::command]
async fn search_vault(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,        // { semantic: bool, lexical: bool }
    lexical_opts: LexicalOpts, // { case_sensitive, diacritic_sensitive, prefix_match, phrase_mode }
    semantic_opts: SemanticOpts, // { min_similarity, top_k, recency_bias }
    epoch: u64,                // echoed back so the frontend can drop stale results
) -> Result<SearchResponse, HikerError>;
```

`SearchResponse { epoch, lexical_hits: Vec<NoteHit>, semantic_hits: Vec<NoteHit>, fused: Vec<NoteHit> }`. The frontend renders `fused` when both modes are on, otherwise the relevant single list.

Both option structs have `#[serde(default)]` on every field, so older frontend payloads (or the empty-options shape) decode to the documented defaults — the loader-level `settings-strict-load` discipline doesn't apply at the Tauri boundary, only at config load.

Returning all three buckets (rather than the frontend-relevant one) is deliberate: it keeps the Tauri command flat and lets us add UI affordances later (e.g. "show me what each backend found separately") without a new command. The frontend ignores the buckets it doesn't need. [search-tauri-cmd]

Wires through `core::search::query()`, which composes the two engine traits. Tauri command is a thin wrapper over the core call (~10 lines) per the layer-split rules in `design.md`.


## Module discipline

- `core::search` — engine traits, fusion, public `query` function. Zero `tauri::` imports.
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

- **In-file find / replace.** That's a within-buffer affordance; CM6 has `@codemirror/search` for it. Different feature, different keybind (Cmd/Ctrl-F when it lands), no overlap with vault-wide search.
- **Search-and-replace across the vault.** Destructive bulk edit is its own feature with its own confirmation/undo story. Search is read-only.
- **Saved searches as durable infrastructure.** That's the collections feature in `design.md`'s auto-organization layer; this doc leaves the seam (`search-result-pin-as-collection`) and stops there.
- **Highlighting matches inside the open editor.** Implementable via CM6 decorations, but it's a different surface (editor view) and a different cognitive model (where am I, vs. what's in the vault). Possibly worth doing later — not a v2 concern.
- **Query syntax** beyond what FTS5 natively understands. v2 passes the user's input straight to FTS5's `MATCH`, which already handles `"phrase queries"`, `term1 OR term2`, `NEAR()`, etc. We don't add a hiker-specific DSL on top.
- **Single-row context-menu mutations** (right-click a result → delete this one note). The tree's context menu is the canonical place for per-note destructive actions; duplicating it in the discovery panel splits the mental model. *Bulk* actions across a whole result set or a multi-selected subset are explicitly *deferred*, not out of scope — see `search-bulk-action-tag` / `search-bulk-action-move` above.
