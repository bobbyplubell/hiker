# MCP server

Hiker exposes its vault as an MCP server so external agents (Claude Code, Goose, Codex, custom MCP clients) can read, search, and write notes. After the core rework the MCP server is the **sole agent surface** — there is no in-app chat, no in-house agent loop, and no ACP client (`llm.md`). External agents reach the vault through this server and use their own UI; their writes land as reviewable **pending** edits (`op-log.md`, `patch-review.md`).

**Opt-in, off by default.** A write-capable localhost listener is deliberate, so `[mcp] enabled` defaults to **false**: the server binds only when the user turns it on (per vault). When enabled it binds `127.0.0.1` (localhost-trust). [mcp-listener-opt-in]

**In-process, decoupled by crate.** `core::mcp` is a sibling crate (`mcp-server/`); the UI launches it on vault open *when enabled* and stops it on vault close. Single-process means MCP shares the indexer's writer and the read store — no two-writer coordination — while UI imports zero MCP types and MCP imports zero UI types. [mcp-in-process, mcp-crate-decoupled] The implementation library is **rmcp** (the official Anthropic Rust SDK), wrapped in hiker's own tool-surface trait the same way graniet/`llm` is. [mcp-rmcp-backed]


## Architecture

```
                   hiker UI process
                          │
       ┌──────────────────┼──────────────────┐
       │                  │                  │
   user actions      core::mcp           core::indexer
   (UI tree, save,    │ rmcp Server      (owns the writer)
   editor, etc.)      │ tokio task            ▲
       │              │ HTTP listener         │
       │              │  on 127.0.0.1         │ writes routed via
       ▼              ▼                       │ IndexerHandle
   core::ops ─────────┴───────────────────────┘  (same as UI's writes)
       │
       ▼
   core::store + core::oplog
   (index.db writer in the indexer task — single writer; the op log is the in-memory layered model)
```

`core::mcp` is started from the host on vault open *when `[mcp] enabled`* with three handles:
- `IndexerHandle` (for write tools — routes through the same MPSC the UI uses).
- A read `Store` clone (for read tools — shares the existing `read_store` pool from the arch cleanup).
- The vault's `Vault` for path resolution + abs-path translation when needed.

Tokio task lifecycle: spawned at vault open *only when `[mcp] enabled`*, dropped at vault close. The HTTP listener binds an ephemeral port; the bound address is written to `vault/.hiker/mcp.json` for agents to discover. Listener task drains gracefully on close. Toggling `enabled` at runtime binds/unbinds in place (below).


## Tool surface

Read + write tools, covering the cases with concrete value today and leaving room for the rest as backing features land. Every agent write carries the `Author::Agent(<client-id>)` class (per `op-log.md`; surfaced via the git `Hiker-Author` trailer when git is integrated, `git.md`); `write_note` stamps `hiker.author: agent-authored` *only when creating* a note, and every other write skips the stamp. [mcp-tool-surface, mcp-author-stamp-on-create-only] The canonical registered list is in Capability negotiation below.

### Read tools

- **`search_notes(query: string, modes?: SearchModes, top_k?: number)`** — wraps `core::search::query`. Returns `SearchResponse` per the existing [[spec:search-cmd]] shape. Default top_k is the spec's `FUSED_TOP_K = 20`; agents can request smaller (1) or larger (up to a config-pinned cap, default 50). [mcp-tool-search-notes]
status:: done
touches:: [[code:hiker/handler]]
note:: `mcp-server/src/handler.rs::HikerHandler::search_notes` wraps `core::search::query`; returns lexical_hits + semantic_hits + fused. `top_k` clamps the fused bucket to `[mcp] max_top_k`. Embedder unavailability surfaces as `1005 indexer_unavailable`
- **`get_note(rel_path: string, detail?: 'digest'|'snippet'|'full')`** — fetch a single note. `digest` returns id + title + (when summary enrichment lands) cached summary. `snippet` returns top-1 chunk + heading_path. `full` returns the entire body. Default for explicit `get_note` calls is `full`; multi-hit search responses default to `digest`. [mcp-tool-get-note, mcp-progressive-disclosure]
- **`related_notes(rel_path: string, top_k?: number)`** — wraps the existing [[spec:related-notes-query]]. Returns the same `RelatedHit` shape the UI's related panel already consumes. [mcp-tool-related-notes]
status:: done
note:: `HikerHandler::related_notes` calls `Store::related_notes`; `top_k` capped by `[mcp] max_top_k`. Unindexed source returns an empty vec rather than erroring

### UI context tools

Three read tools surfacing "what is the user looking at right now." No new permission beyond vault-read (every value is derivable with an extra `get_note` round-trip). All honor per-tool toggles and return inert payloads when no buffer is focused.

- **`get_active_note()`** — the focused editor tab's vault-relative path plus the cursor's byte offset and (if non-empty) the selection's `{ start_byte, end_byte }`. Buffer-only — an app-page tab (settings / home / queue / etc., per [[spec:tab-kinds]]) returns `{ path: null }`. [mcp-tool-get-active-note]
status:: done
implements:: [[code:hiker/config/sections/McpToolsConfig#get_active_note_enabled]], [[code:hiker/impl#[AppState]refresh_ui_context_snapshot]], [[code:hiker/bootstrap/open_vault]], [[code:hiker/state/Services]]
touches:: [[code:hiker/ui_context]]
note:: returns the focused buffer tab's vault-rel path + cursor byte offset + (if non-empty) selection `{start_byte, end_byte}`. Returns `{path: null}` for app-page / non-buffer tabs. Read-only; per-tool toggle `[mcp.tools].get_active_note_enabled`. Does NOT populate the per-session read set (only `get_note` does, per [[spec:mcp-read-before-write]]) · evidence: `mcp-server/src/ui_context.rs`, `mcp-server/src/handler/` (dispatch + router), `mcp-server/tests/smoke.rs`
- **`get_open_notes()`** — the ordered list of open `buffer` tabs, each `{ path, active: bool }`, in tab-strip order. Non-buffer tab kinds (`graph` / `home` / etc.) are omitted. [mcp-tool-get-open-notes]
status:: done
implements:: [[code:hiker/config/sections/McpToolsConfig#get_open_notes_enabled]], [[code:hiker/impl#[AppState]refresh_ui_context_snapshot]]
touches:: [[code:hiker/ui_context]]
note:: returns the ordered list of open `buffer` tabs as `[{path, active}]`; non-buffer kinds omitted. Per-tool toggle `[mcp.tools].get_open_notes_enabled`. Read-only; does NOT populate the read set · evidence: `mcp-server/src/ui_context.rs`, `mcp-server/src/handler/` (dispatch + router), `mcp-server/tests/smoke.rs`
- **`get_selection()`** — the active buffer's selection as `{ path, start_byte, end_byte, text }` when non-empty, else `{ path: null }`. [mcp-tool-get-selection]
status:: done
implements:: [[code:hiker/config/sections/McpToolsConfig#get_selection_enabled]], [[code:hiker/impl#[AppState]refresh_ui_context_snapshot]]
touches:: [[code:hiker/ui_context]]
note:: returns `{path, start_byte, end_byte, text}` for the active buffer's non-empty selection; `{path: null}` when empty or no buffer. Per-tool toggle `[mcp.tools].get_selection_enabled`. Read-only; does NOT populate the read set · evidence: `mcp-server/src/ui_context.rs`, `mcp-server/src/handler/` (dispatch + router), `mcp-server/tests/smoke.rs`

All three are read-only and bypass the read-before-write set — calling them does **not** count as "the agent read this path" for [[spec:mcp-read-before-write]] purposes; only `get_note` populates the read set. Otherwise an agent could `get_open_notes()` and then claim it had read the file.

### Pending-proposal introspection tools (not yet implemented)

When `review_required` is on (per [[spec:agent-write-review-mode]]), an agent's write enters the document's pending review queue instead of landing on disk, so a follow-up `get_note` against that path returns `1002 note_not_found`. Three tools are specced to let an agent confirm, inspect, and revise its own pending work, all wrapping the `.pending` edits filtered by surface + session:

- **`list_pending_proposals(filter?)`** — list pending edits visible to MCP (default scope `surface = "mcp-tool-call"`); returns `{ proposal_id, target_path, action, surface, session_id, created_at, content_hash }` per proposal, no body. [mcp-tool-list-pending-proposals]
status:: planned
note:: wraps the op-log pending query (`op_writes::list_pending_proposals`) with default `surface = "mcp-tool-call"` so the agent sees only MCP-originated proposals. Returns id + target_path + action + surface + session_id + created_at + content_hash; no body. Read-only; honors per-tool toggle. Lets an agent confirm its staged write landed when `get_note` returns 1002
- **`get_pending_proposal(proposal_id)`** — one pending edit's metadata + proposed `content`; read-only (accept/reject is human-only). For `edit_note`-shaped proposals it adds an `anchors` array (one per `Replace` in the batch, resolved by shared `batch_id`) recomputed against `materialize(accepted)`, each `{ edit_index, anchor_status, old_str_preview }` where `anchor_status` is `holds` (matches once) / `drifted` (zero matches) / `ambiguous` (>1 match, edit wasn't `replace_all`). Racy by construction. Whole-document proposals omit `anchors` (treat absence as "n/a"). [mcp-tool-get-pending-proposal, mcp-pending-proposal-anchor-status]
- **`amend_pending_proposal(proposal_id, new_content)`** — replace a pending edit's payload in place (same `metadata.client_id` only; whole-document shapes only — `edit_note` batches re-issue after accept/reject). Recomputes `content_hash`, stamps `amended_at_ms`, increments `amend_count`, discards the prior payload (no version history), fires the op-log pending-change events so an open review surface re-renders. If the user has already accepted, the proposal has left the queue and the call returns `1002` — "amend works until the user takes action," so the human still gets exactly one gate per accepted change. [mcp-tool-amend-pending-proposal]
status:: planned
note:: new MCP write tool letting an agent revise its own pending proposal in place before the user reviews. Same-client only; whole-file shapes only (`write_note` / `set_frontmatter` / `apply_tag`); `edit_note` batch-shape amend deferred. No version history — overwrites the stored body, bumps `metadata.amended_at_ms` + `metadata.amend_count`, recomputes `content_hash`. Fires op-log change events so an open review surface re-renders. User-accept races resolve in the pending store's transaction (last-write to the amend). Per-tool toggle `[mcp.tools].amend_pending_proposal_enabled`. Unblocks the "agent realized its first attempt was wrong" workflow without breaking the one-human-gate-per-change model

None of the three is registered in the router today — they appear only inside two tool descriptions. Tracked as `bug-mcp-pending-proposal-tools-unimplemented`. Per-tool toggles for them (`*_enabled`) follow the standard pattern once built.

### Write tools

All writes route through `core::ops`. Every agent write carries the `Author::Agent(<client-id>)` class: folded into `accepted` and written to the `.md` (with a snapshot, and a git commit when integrated) when `review_required` is off, queued as a pending edit anchored against `accepted` when it's on. Authorship stamping is creation-only (full statement under Authorship + audit trail, [[spec:mcp-author-stamp-on-create-only]]).

**Pending-mode caveat — load-bearing for agent behavior.** When `[mcp.tools].review_required` is on (see [[spec:agent-write-review-mode]]), every write tool produces a **pending edit** anchored against `accepted` *instead of* writing to disk, returning `{ status: "staged", proposal_id }` (or `proposal_ids` for `edit_note`, one per edit); direct mode returns `{ status: "written" }`. The edit is persisted to `.pending` but the file is **not** visible on disk or via `get_note` until the user accepts — `get_note` returns `1002 note_not_found` for a path that exists only as a pending edit (per [[spec:mcp-staging-read-disk-only]]). Tool descriptions surface this in plain language so the agent doesn't mistake a pending write for a failed one. `edit_note` produces *one `Replace` per edit* sharing a `batch_id` per [[spec:op-log-op-shape]]; the other write tools produce one edit per call. [mcp-write-tools-staging-aware]
status:: partial
touches:: [[code:hiker/handler/dispatch]]
note:: `mcp-server/src/handler/dispatch.rs::save_note` / `merge_frontmatter` / `update_tag` stage an op-log pending edit (via `op_writes::stage_agent_edits`) when `[mcp.tools].review_required` is on and return `status: "staged"` + `proposal_id`. Tool description strings call out the staged behavior so agents don't read a staged write as a failed write. **Partial**: (a) the agent-facing introspection tools ([[spec:mcp-tool-list-pending-proposals]], [[spec:mcp-tool-get-pending-proposal]]) are specced but not implemented; (b) [[spec:mcp-tool-edit-note]] adds `propose_batch` and per-edit proposals per `staging-per-edit-proposals`

- **`write_note(rel_path: string, content: string, expected_hash?: string)`** — create or replace a note's body. If `expected_hash` is provided, the write is drift-aware (checks against `materialize(accepted)`); without it, an unconditional write. Refuses paths under `.hiker/`. Stamps `hiker.author: agent-authored` on the resulting frontmatter *only when the target path did not previously exist* (per [[spec:mcp-author-stamp-on-create-only]]). When the target path already exists, the call requires the agent to have read the note in the current session via `get_note` first (`1008 read_required`); see [[spec:mcp-read-before-write]]. Creates are exempt. Returns the new content hash. [mcp-tool-write-note]
status:: done
implements:: [[code:hiker/ops/agent/write_note]]
note:: `core/src/ops/agent.rs::write_note` + `HikerHandler::write_note`. Watcher-suppresses, writes via `vault.write_file_checked` when `expected_hash` is set, queues a pending op-log op (`author='agent:<client_id>'`, whole-body `Replace`) via `op_writes::stage_agent_edits`, enqueues `IndexJob::Upsert`
- **`edit_note(rel_path: string, edits: [{ old_str: string, new_str: string, replace_all?: bool }])`** — apply one or more span-anchored patches to an existing note. Each `old_str` must match exactly once in the file unless `replace_all: true`. Refuses non-existent paths (use `write_note` to create). Validation happens at receive time as one transaction; on any failure the whole call rejects and nothing is queued. Returns `{ status: "staged", proposal_ids: [...] }` in review mode or `{ status: "written", content_hash }` in direct mode. [mcp-tool-edit-note]
status:: partial
touches:: [[code:hiker/handler/dispatch]]
note:: `mcp-server/src/handler/dispatch.rs::apply_edits` — span-anchored patch tool, advertised in `tool_router`. Direct mode (review off) validates + applies all edits transactionally via `apply_edit` and routes through `agent_write_note` once. Review-on mode stages op-log pending edits via `core::ops::op_writes::stage_agent_edits` (one anchored `Replace` per edit, all sharing the returned `batch_id`, author `agent:<client-id>`), reviewed through the op-log pending surfaces. A `replace_all` edit with >1 match collapses the call to one anchorless whole-body op (the op-log anchor must resolve uniquely). **Partial**: rule 5 (read-before-write) is owned by [[spec:mcp-read-before-write]] (still planned); the per-tool `edit_note_enabled` toggle row is wired in the settings UI

  Validation rules (all must hold before the call is accepted):

  1. **Path exists.** Non-existent path → `1002 note_not_found`. Creates go through `write_note`.
  2. **Per-edit anchor resolves uniquely.** Each `old_str` matches exactly one byte range in the current file content. Multiple matches without `replace_all: true` → `invalid_params` naming the offending edit index. Zero matches → `1003 drift`.
  3. **No textual overlap.** No two edits' resolved byte ranges may overlap. Overlap → `invalid_params` naming the offending pair. Two edits modifying the same span are conceptually one edit; the agent merges them into a single edit with a larger `old_str` / `new_str`.
  4. **All anchors hold against the *pre-application* file.** Each `old_str` is resolved against the original file content, not against the running buffer of earlier edits' results. Sequential dependencies between edits (where edit B's anchor only appears after edit A is applied) are rejected as `invalid_params`. The agent expresses such dependencies as one edit with a wider span.
  5. **Path was read this session.** The agent must have called `get_note(rel_path)` (any detail level) at least once in the current MCP session before issuing `edit_note` against the path. Editing a note the agent hasn't seen is overwhelmingly a hallucinated-anchor situation; the per-session read set makes the foot-gun an explicit error (`1008 read_required`) instead of a silent garbage edit. The check is per-session (not per-call) — re-issuing `edit_note` against the same path doesn't require re-reading. See [[spec:mcp-read-before-write]]. [mcp-edit-note-validation]
status:: partial
touches:: [[code:hiker/handler]]
note:: `mcp-server/src/handler.rs::edit_note_inner` enforces rules 1–4: path exists (else `1002`), per-edit anchor unique unless `replace_all: true` (else `1003` for missing, `invalid_params` for non-unique), no overlap between resolved byte ranges across edits (`invalid_params` naming the offending pair), all anchors resolve against the pre-application file content (single read up front; never sequential dependency). Smoke tests cover each branch. **Partial**: rule 5 (per-session read set) lands with [[spec:mcp-read-before-write]]

  After validation passes, the call produces N `Replace` edits (one per edit) sharing a `batch_id` in metadata so consumers can group them as one originating tool call. When `[mcp.tools].review_required` is off, the edits commit into `accepted` and the atomic disk write runs once for the batch per [[spec:op-log-atomic-write]]. When on, they enter the pending review queue as anchored edits.

- **`set_frontmatter(rel_path: string, fields: map<string, json>)`** — merge frontmatter fields into a note. Implementation merges into the existing frontmatter via a small frontmatter-aware writer (`core::ops::set_frontmatter`). Used for summary writes, status changes, and other structured-metadata mutations. Does not stamp `hiker.author: agent-authored` (per [[spec:mcp-author-stamp-on-create-only]]). [mcp-tool-set-frontmatter]
status:: done
implements:: [[code:hiker/frontmatter/DELIMITER]], [[code:hiker/ops/agent/set_frontmatter]]
note:: new `core/src/frontmatter.rs` (split/merge/assemble); `core::ops::agent_set_frontmatter` reads existing, deep-merges patch via `merge_agent_patch`, stamps `hiker.author: agent-authored`, routes through `agent_write_note`. Errors `invalid_params` if `fields` isn't a JSON object
- **`apply_tag(rel_path: string, tag: string)`** / **`remove_tag(rel_path: string, tag: string)`** — convenience wrappers over `set_frontmatter` for the most common case. [mcp-tool-apply-tag]

### Trail tools

Trails (per `trails.md`) get a six-tool surface — three read, three write — so agents can both consume curated context and transcribe their investigations as draft trails. Write tools route through `core::ops::agent_*` like every other MCP write and produce pending edits when [[spec:agent-write-review-mode]] is on.

- **`trails_list(filters?)`** — enumerate trails with optional filters (containing-note, recently-activated, name-substring); returns id + title + waypoint count + activation timestamp + path. [mcp-tool-trails-list]
status:: planned
note:: enumerate trails with optional filters (containing-note, recently-activated, name-substring); returns id + title + waypoint count + activation timestamp + path. Lands with `trails.md`
- **`trail_get(id, detail?)`** — full trail-doc body + ordered waypoint list (each waypoint's source-note ref + annotation body); detail levels mirror [[spec:mcp-tool-get-note]]'s `digest` / `full`. [mcp-tool-trail-get]
status:: planned
note:: fetch a trail's full body + ordered waypoint list (each waypoint's source ref + annotation body); detail levels `digest` / `full`. Lands with `trails.md`
- **`trails_containing_note(rel_path)`** — reverse lookup; returns trails that include the given note as a waypoint. [mcp-tool-trails-containing-note]
status:: planned
note:: reverse lookup; returns trails that include a given note as a waypoint. Lands with `trails.md`
- **`trail_create(name)`** — create a new trail (empty waypoint list, default placement per `[trails] new_trail_dir`); returns id + path. [mcp-tool-trail-create]
status:: planned
note:: create a new trail; placement per [[spec:trails-default-location]]; routes through `core::ops::agent_*` with `author='agent:<client-id>'` and rides the op-log pending/patch-review path like any other agent write (drafts removed 2026-06-05)
- **`trail_append_waypoint(trail_id, source_rel, annotation?)`** — append a waypoint; creates the waypoint-note under `.hiker/trails/<trail-id>/waypoints/`, links to source, seeds optional starter annotation (omitted → empty body). [mcp-tool-trail-append-waypoint]
status:: planned
note:: append a waypoint to a trail; `parent_waypoint_path` arg makes the new waypoint a side-trail child of the given parent (omitted → root-level append); creates the waypoint-note in the trail-doc's visible companion folder ([[spec:trail-storage-layout]]), links to source, seeds optional starter annotation (default empty body); same agent-write routing as [[spec:mcp-tool-trail-create]]
- **`trail_remove_waypoint(trail_id, waypoint_id)`** — symmetric to the sidebar's [[spec:trails-mode-remove-waypoint-verb]]; routes the waypoint-note delete through `core::ops::delete` so it lands in trash. [mcp-tool-trail-remove-waypoint]
status:: planned
note:: remove a waypoint from a trail; cascades to descendants when target has children (per [[spec:trails-mode-remove-waypoint-verb]]'s cascade rule); routes waypoint-note delete through `core::ops::delete`

### Board tools

Boards (per `kanban.md`) get a read + curate MCP surface so attached agents can read boards as context and reorganize them. Every **write** tool routes through the same user-save path the board UI uses and produces a pending edit when [[spec:agent-write-review-mode]] is on (the staged board-doc edit appears in the patch-review surface; disk is unchanged until accept), commits via `op_writes::user_save` in direct mode, returns `{status: "staged", proposal_id}` in review mode or `{status: "written"}` direct, and is independently toggleable under `[mcp.tools]`. Card-targeting writes identify the card by its board-local `card_id` (from `board_get`); column writes by column name. All board mutations touch only the board-doc frontmatter — referenced notes are never modified.

Read:

- **`boards_list()`** — enumerate every board-doc in the vault; returns `rel_path` + `board_id` + `title` + `column_count` + `card_count` per board (the `core::boards::list` shape). [mcp-tool-boards-list]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`boards_list` → `enumerate_boards`) wraps `core::boards::list`; read-only; per-tool toggle `mcp.tools.boards_list_enabled`
- **`board_get(rel_path)`** — full board-doc body + resolved columns, each column carrying its ordered cards (each card's `card_id`, title, and reference-resolution outcome), via `core::boards::get_board`. The `card_id`s it returns are the handles the write tools below take. [mcp-tool-board-get]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`board_get` → `fetch_board`) wraps `core::boards::get_board` (body + resolved columns/cards); per-tool toggle `mcp.tools.board_get_enabled`

Write:

- **`board_create(name)`** — create a new board-doc (default `Todo`/`Doing`/`Done` columns) at the configured `[boards] new_board_dir`; returns the new `rel_path` + `board_id`. Wraps `core::boards::ops::create_board`. [mcp-tool-board-create]
status:: done
note:: DEVIATION from the generic review-mode contract: `board_create` commits directly EVEN under `review_required` — the op-log whole-file-create staging path seeds the doc by writing an empty `.md` to disk (via `OpLog::create_document`'s `write_md_file`), which would leave a phantom empty board-doc visible until the user accepted. Creates are structural; the safer fallback is direct-commit and let the user delete on reject. Subsequent board *edits* on that board still stage in review mode · evidence: `mcp-server/src/handler/{router,dispatch}.rs` (`board_create` → `create_board`) wraps `core::boards::ops::create_board`; returns `rel_path`+`board_id`+`status`; smoke `board_create_commits_directly_even_in_review_mode` + `board_write_tools_round_trip_direct`; toggle `mcp.tools.board_create_enabled`
- **`board_add_card(board_rel_path, column, source_rel_path)`** — append a note as a card to a column; idempotent per board (a note already on the board returns `status: "noop"`). Wraps `core::boards::ops::add_card`. [mcp-tool-board-add-card]
status:: done
touches:: [[code:hiker/handler/dispatch/boards]]
note:: under path-as-identity the card serializes as `{path}` — no ULID to stamp, and the prior Send-safety gap dissolves · evidence: `mcp-server/src/handler/dispatch/boards.rs::add_board_card`; idempotent per board (`status:"noop"`); review mode stages via `stage_whole_body` (`status:"staged"`), direct mode commits via `op_writes::user_save` (`status:"written"`); `core::boards::add_card_preview` reads the board-doc only (no Store handle needed)
- **`board_add_text_card(board_rel_path, column, text)`** — append a freeform (non-note) text card to a column; returns the new `card_id`. Wraps `core::boards::ops::add_text_card`. [mcp-tool-board-add-text-card]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`board_add_text_card` → `add_board_text_card`) drives `core::boards::ops::preview_edit(BoardEdit::AddTextCard)`; mints `card_id` (returned in the response); review stages via `stage_whole_body`, direct commits via `op_writes::user_save`; smoke `board_write_tools_round_trip_direct`; toggle `mcp.tools.board_add_text_card_enabled`
- **`board_move_card(board_rel_path, card_id, to_column, to_index?)`** — move/reorder a card to `to_column` at `to_index` (tail when omitted). Wraps `core::boards::ops::move_card`. [mcp-tool-board-move-card]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`board_move_card` → `move_board_card`) drives `core::boards::ops::preview_move_card` (resolves source column off the parsed board, then `apply_edit(BoardEdit::MoveCard)`); review stages, direct commits; smoke `board_write_tools_round_trip_direct`; toggle `mcp.tools.board_move_card_enabled`
- **`board_set_card_text(board_rel_path, card_id, text)`** — rewrite a freeform card's text (errors on a note card). Wraps `core::boards::ops::set_card_text`. [mcp-tool-board-set-card-text]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`board_set_card_text` → `set_board_card_text`) drives `core::boards::ops::preview_edit(BoardEdit::SetCardText)`; errors on a note card; review stages, direct commits; toggle `mcp.tools.board_set_card_text_enabled`
- **`board_remove_card(board_rel_path, card_id)`** — drop a card from the board (the referenced note is untouched). Wraps `core::boards::ops::remove_card`. [mcp-tool-board-remove-card]
status:: done
note:: `mcp-server/src/handler/{router,dispatch}.rs` (`board_remove_card` → `remove_board_card`) drives `core::boards::ops::preview_edit(BoardEdit::RemoveCard)`; referenced note untouched; review stages, direct commits; toggle `mcp.tools.board_remove_card_enabled`
- **`board_add_column(board_rel_path, name)`** / **`board_rename_column(board_rel_path, old_name, new_name)`** / **`board_reorder_column(board_rel_path, name, to_index)`** / **`board_delete_column(board_rel_path, name)`** — column management; delete drops that column's card references (notes untouched). Wrap the matching `core::boards::ops::*_column` verbs. [mcp-tool-board-add-column, mcp-tool-board-rename-column, mcp-tool-board-reorder-column, mcp-tool-board-delete-column]

`repoint_card` (path-conflict resolution) is intentionally **not** exposed — re-pointing a card whose note identity changed is a human-judgment call surfaced as the board's Keep/Repoint/Break modal, not an agent action.

### Task queue tools

Per `task-queue.md`, the MCP server exposes the queue's checkout/submit surface so external rmcp clients can drain hiker's non-interactive LLM work — alongside hiker's own in-process direct-LLM worker (`llm.md`).

- **`task_checkout(types?, shapes?, min_priority?, lease_secs?)`** — return the next eligible task or null; stamps a lease against the calling rmcp client id. [tasks-mcp-tool-checkout]
- **`task_submit(task_id, value)`** — write the result; validates against the task's `output_schema` if any. [tasks-mcp-tool-submit]
- **`task_fail(task_id, error)`** — agent gives up. [tasks-mcp-tool-fail]
- **`task_heartbeat(task_id)`** — extend the current lease. [tasks-mcp-tool-heartbeat]

Plus a read-only `task_list(states?, types?)` for queue inspection. [tasks-mcp-tool-list]

Two new positive error codes ride this surface: `1006` (`stale_lease`) and `1007` (`schema_violation`). See `task-queue.md` for behavior.

Cancellation is **not** an MCP tool. External agents learn cancellation via `stale_lease` on submit; mid-work cancellation push (rmcp server→client streamable notification) is [[spec:task-queue-mcp-cancel-notification]], deferred.

Notably absent from v3:

- `move_note`, `delete_note`, `create_folder` — heavier writes; deferred until a real motivating case appears.
- `list_landmarks`, `list_collections`, `get_collection` — landmarks/collections unbuilt; added (and advertised) when those features land. [mcp-tool-landmarks-deferred, mcp-tool-collections-deferred]
- `expand_chunk`, `get_note_context` — sketched in `design.md` but not load-bearing for v3. Deferred.
- Vision OCR helpers — depend on the extractor pipeline being real. Deferred to v4+.


## Read-before-write

Both write tools that touch *existing* content require a prior `get_note` call against the same path in the current MCP session. The rule is a foot-gun guard, not a security boundary: an agent that issues `edit_note` against a path it has never read is almost always hallucinating anchors (or rewriting the wrong file); blocking the call early — with a clear error naming the path and the required tool — turns the silent garbage-edit case into a recoverable one. [mcp-read-before-write]
status:: planned
touches:: [[code:hiker/handler]]
note:: per-session `ReadSet` in `mcp-server/src/handler.rs` populated by successful `get_note` calls; `edit_note` always requires a prior read, `write_note` requires one when target path exists on disk (creates exempt). Violations return `1008 read_required` before validation. Foot-gun guard against hallucinated-anchor edits

- **Scope.** `edit_note` always requires a prior read. `write_note` requires a prior read only when the target path *already exists* on disk; creating a new note is exempt (there is nothing to have read). `set_frontmatter` / `apply_tag` / `remove_tag` are merge-into-frontmatter operations and don't need to have seen the body — they're exempt.
- **Read set is per-session.** Each MCP session (one rmcp connection) carries an in-memory `HashSet<rel_path>` populated by every successful `get_note` call. The set is dropped at session close. Re-issuing a write against the same path within the session doesn't require re-reading.
- **Implementation.** A small `ReadSet` lives on the per-session handler state in `mcp-server/src/handler.rs`, populated in `get_note_inner` after a successful fetch and consulted in `write_note_inner` / `edit_note_inner` before validation, ahead of the staging / direct-write branch. Per-session (not per-call) because the agent often already holds the content in its own context; it matches Claude Code's Read-before-Edit precedent.


## UI refresh on agent writes

Agent writes route through `core::ops::agent_*`, which suppress the watcher around the fs write (load-bearing for rename/delete correctness, see `watcher.md`). Suppression means the UI's watcher-file-events listener never fires for an agent-authored save, leaving the tree stale.

Resolution: ride the op-log's accepted-write events. Every accepted agent write emits a path-scoped change event carrying `Author::Agent(<client-id>)`; the frontend's tree + buffer-reload code subscribes and applies its post-mutation refresh — gated on `author.startsWith("agent:")` so non-agent writes (user saves, rollbacks) keep flowing through the watcher path unchanged. [mcp-ui-refresh-on-agent-write]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: `app/src/workbench_host.rs` (agent-write handler) — listens on op-log accepted-write events, gates on `author.startsWith("agent:")`, and runs the same tree-refresh / vault-home / active-buffer-reload sequence the watcher handler does. Dirty buffer is kept (toast surfaces the conflict) rather than prompting modally — agent writes are server-driven so an interrupt prompt would surprise the user

When an accepted `edit_note` lands on a path whose buffer is currently dirty, the plain disk-reload path would clobber the user's unsaved edits. The patch-review accept flow (see `patch-review.md`) instead applies the span-anchored patch to both disk and the in-memory buffer in one transactional move, refusing with a clear error when the user's edits have clobbered the anchor. Direct-mode agent writes use this same machinery: the append-events listener delegates to the patch-aware buffer-update path when the row carries an `edit_note` patch in metadata, falling back to disk reload otherwise.


## Authorship + audit trail

Every accepted MCP-driven write produces two artifacts, plus a frontmatter stamp only on creation:

1. **An accepted write into `accepted` + the `.md`** (per `op-log.md`) — folded into the layered model and written atomically, with a plain-file snapshot for local history. When git is integrated, the save also commits with `Hiker-Author: agent:<client-id>` (`git.md`) — the durable, self-describing attribution record. (In review mode the proposed edit lives in `.pending` until the user accepts; the write lands on accept.) Local history rollback reads a snapshot (`op-log.md` "Local history").
2. **An entry in `vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`** — the audit log, per `llm.md`. Records the MCP call itself (tool name, input, response status, timestamp) for telemetry/debugging — a durable provenance log, separate from the content-change record (snapshots + git). [mcp-audit-log-jsonl]

**Frontmatter stamp on creation only.** When `write_note` brings a note into existence (the target path didn't exist), the resulting frontmatter carries `hiker.author: agent-authored` (and optionally `hiker.provenance: mcp-<client-id>`). Every other write tool — `write_note` against an existing path, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag` — skips the stamp. The frontmatter field expresses *origin*; per-modification provenance lives on the write's `Author` class (the git `Hiker-Author` trailer when git is integrated). [mcp-author-stamp-on-create-only]
status:: planned
note:: drop the automatic `hiker.author: agent-authored` stamp on every agent write; stamp only when `write_note` creates a previously-non-existent path. `edit_note` / `set_frontmatter` / `apply_tag` never stamp. Updates [[spec:mcp-tool-write-note]] / [[spec:mcp-tool-set-frontmatter]] / [[spec:mcp-tool-apply-tag-remove-tag]] to drop the stamp call

The two artifacts have different consumers: snapshots + git are the content-change history (power the version dropdown + rollback UI), the JSONL is the call-telemetry log (powers prompt-edit debugging and cost transparency per `llm.md`).


## HTTP server + transport

rmcp's Streamable HTTP transport (the only v3 transport; stdio and old HTTP+SSE are deferred): single endpoint accepting POST for client→server messages and GET (with SSE upgrade) for server→client streaming. JSON-RPC 2.0 over the wire. Cutting stdio keeps the "only when hiker is running" lifecycle trivially honored, since the server is just a tokio task in hiker's process. [mcp-transport-streamable-http]

**Bind address.** Default `127.0.0.1` (localhost-only). The auth model is localhost-trust — no token; anyone who can reach the port is trusted, and the discovery file is local-readable but not network-reachable. [mcp-localhost-trust] Configurable in `[mcp] host` for LAN access; a non-loopback bind keeps the same trust model (effectively *trust everyone on the LAN*), so the settings UI warns when the value isn't `127.0.0.1`. `0.0.0.0` is allowed for users gating an all-interfaces bind behind their own reverse proxy. [mcp-bind-host-configurable]

[mcp-bind-host-configurable]
status:: done
note:: `core/src/config.rs::McpConfig::host`; `mcp-server/src/lib.rs` builds the bind address from `host` + `port` (with IPv6 bracket handling). Default stays `127.0.0.1`. The settings UI row carries the LAN-exposure warning text in its `desc` so the consequence is visible at the choice site

**Port.** Default ephemeral (port 0 → OS-assigned), written with the connect URL to the discovery file at startup; configurable to a fixed port for static MCP config. [mcp-port-discovery] Configurable in `[mcp]`:
status:: done
touches:: [[code:hiker/discovery]]
note:: `mcp-server/src/discovery.rs` writes `<vault>/.hiker/mcp.json` on bind; `McpServerHandle::shutdown` (and `Drop`) remove it. OS-assigned ephemeral by default; honors `[mcp].port` for a fixed port. Smoke test asserts both write and removal

```toml
[mcp]
enabled = false                # OFF by default — opt-in write-capable localhost listener
host = "127.0.0.1"             # bind address; localhost-trust auth requires careful thought before changing
port = 0                       # 0 = ephemeral; otherwise a fixed port
discovery_file = ".hiker/mcp.json"   # vault-relative; written on bind, removed on shutdown
max_top_k = 50                 # cap on agent-requested top_k for search/related
```

[mcp-config-section]
status:: done
implements:: [[code:hiker/config/sections/McpConfig]]
note:: `core/src/config/sections.rs` (`McpConfig`, `McpToolsConfig`, `McpAuditConfig`); strict-load validates the section. Defaults: enabled=**false** (opt-in), host="127.0.0.1", port=0, discovery_file=`.hiker/mcp.json`, max_top_k=50, tools.writes_enabled=true, tools.allow_redacted_lookup=false, every per-tool `<name>_enabled=true`, audit.log_full_input=false

**Discovery file** at `vault/.hiker/mcp.json`:

```json
{
  "url": "http://127.0.0.1:54321",
  "version": "1",
  "started_at": "2026-05-12T14:30:00Z",
  "vault_root": "/home/me/vault"
}
```

Written on bind, removed on graceful shutdown. Stale files are detected by attempting to connect; an agent finding a discovery file but no listening port should treat it as stale and report a clear error. [mcp-discovery-file]


## Capability negotiation

At rmcp `initialize` time, hiker advertises its tool list dynamically based on what features are present. The currently-registered set (canonical — `mcp-server/src/handler/router.rs`):

- **Notes (read):** `search_notes`, `get_note`, `related_notes`, `get_active_note`, `get_open_notes`, `get_selection`.
- **Queries (read):** `query` — the generic saved-query / inline-filter tool ([[spec:query-mcp-tool]], owned by `queries.md`).
- **Notes (write):** `write_note`, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag`.
- **Boards:** `boards_list`, `board_get`, `board_create`, `board_add_card`, `board_add_text_card`, `board_move_card`, `board_set_card_text`, `board_remove_card`, `board_add_column`, `board_rename_column`, `board_reorder_column`, `board_delete_column`.
- **Task queue:** `task_checkout`, `task_submit`, `task_fail`, `task_heartbeat`, `task_list`.
- **Kinds (write, generated):** `create_<kind>` / `update_<kind>` per registered kind, derived from the kind registry ([[spec:mcp-registry-tools]], owned by `kinds.md`); gated by the `[mcp.tools] kind_tools_enabled` family toggle plus `writes_enabled`.

The one-time sweep comparing shipped features against this surface is [[spec:mcp-tool-audit]] (owned by `kinds.md`); its filed gaps are the `bug-mcp-tool-coverage-gaps` row in `bug_tracking.md`.

(The pending-proposal introspection tools above — `list_pending_proposals`, `get_pending_proposal`, `amend_pending_proposal` — are *not* registered; see `bug-mcp-pending-proposal-tools-unimplemented`.) Conditionally advertised: the mechanism is built for future tools that depend on backing features (trails, landmarks, collections, vision extractors) — each defines an `is_available()` predicate and the server filters at initialize time, so agents see a coherent capability set instead of calling tools that error with "feature not implemented." [mcp-dynamic-capabilities]
status:: done
touches:: [[code:hiker/handler]]
note:: the mechanism is in place — the rmcp router is the natural seam for conditional advertising. No `is_available()` predicate yet because no feature-gated tool has landed; future feature-gated tools own their own predicates and will plug into the same router · evidence: `mcp-server/src/handler.rs` (rmcp `tool_router` advertises the v3 surface)


## Lifecycle awareness (not yet implemented)

The lifecycle fields are a deferred `design.md` feature; until they land the MCP server treats them as absent and returns everything. The intended behavior: `search_notes` / `get_note` / `related_notes` exclude notes with `hiker.archived` / `hiker.redacted` / `hiker.retired` by default, with a `scope` opt-in to include them; **redacted notes return id + title only** regardless of scope. Enforcement lives in `core::search::query` and `core::store::get_note` (not the MCP layer), so the same rules apply to UI search and any other consumer once built. [mcp-lifecycle-aware]
status:: partial
note:: per-spec, the lifecycle fields (`hiker.archived` / `redacted` / `retired`) aren't yet implemented in hiker so the filter is a no-op. The redacted-body restriction lives on `core::store::get_note` rather than the MCP layer when it lands


## Error model

rmcp's standard JSON-RPC error shape: `{ code, message, data? }`. Hiker's `HikerError` translates at the MCP boundary into a small set of agent-friendly codes:

- `-32601` (method not found) — tool isn't advertised. Standard JSON-RPC.
- `-32602` (invalid params) — argument validation failures (path escape, missing required field, top_k out of range).
- Hiker-specific positive codes:
    - `1001` (`vault_not_open`) — server is up but no vault is currently open.
    - `1002` (`note_not_found`) — `rel_path` doesn't resolve to an existing note.
    - `1003` (`drift`) — `write_note` with `expected_hash` mismatch (the file changed under the agent).
    - `1004` (`disabled`) — feature flag for the requested tool is off (e.g. write tools disabled per `[mcp.tools] writes_enabled = false`).
    - `1005` (`indexer_unavailable`) — embedder hasn't loaded yet, or the indexer task is shut down.
    - `1008` (`read_required`) — `write_note` against an existing path, or `edit_note` against any path, when the agent hasn't called `get_note` for that path in the current MCP session. The error message names the path and the tool the agent should call first. See [[spec:mcp-read-before-write]].

Error messages are user-facing-friendly enough that an agent can echo them back to its user without translation. [mcp-error-model]
status:: done
touches:: [[code:hiker/handler]]
note:: `mcp-server/src/handler.rs::translate_hiker_err` translates `HikerError` → JSON-RPC error codes. Hiker-specific: 1002 (`note_not_found`), 1003 (`drift`), 1004 (`disabled`), 1005 (`indexer_unavailable`). `1001 vault_not_open` not wired — the server only exists while a vault is open, so `vault_not_open` can't occur over the wire today; reserved for the future "MCP outlives session" mode


## Configuration

Full `[mcp]` schema:

```toml
[mcp]
enabled = false                      # OFF by default; off → server doesn't bind, no MCP
host = "127.0.0.1"                   # bind address; non-loopback exposes vault — see warning above
port = 0                             # 0 = ephemeral
discovery_file = ".hiker/mcp.json"   # vault-relative
max_top_k = 50

[mcp.tools]
# Master write gate: when false, every write tool is refused with
# `1004 disabled` regardless of the per-tool flags below.
writes_enabled = true
allow_redacted_lookup = false

# Plus one `<tool>_enabled` toggle per registered tool (default true) —
# every note read/write, every board tool, and every task_* tool (the full
# list in core::config::sections::McpToolsConfig). Each is independent of
# `writes_enabled`: flipping one off un-advertises that tool at `initialize`
# and rejects direct rmcp calls with `1004 disabled` even when writes are on.
# Exception: the generated kind tools (`create_<kind>` / `update_<kind>`)
# have no per-tool keys — they share the single `kind_tools_enabled` family
# toggle documented under "Kinds (write, generated)" above.

[mcp.audit]
log_full_input = false               # mirror of [llm.audit] log_full_prompt; default off
```

Per-tool toggles apply live (the next dispatch re-gates immediately, no restart). [mcp-tool-toggles] Bind-affecting changes — `enabled` / `host` / `port` / `discovery_file` / `max_top_k` / `audit.log_full_input` — trigger an in-place restart from `set_setting` (the handle drops, cancelling the axum task and removing the discovery file, then `hiker_mcp::start(...)` rebinds) while keeping the vault session intact. [mcp-server-restart-on-config-change, mcp-config-section]
status:: done
touches:: [[code:hiker/handler]]
note:: `core/src/config/sections.rs::McpToolsConfig::tool_allowed`; `mcp-server/src/handler` `guard_tool` called at the top of every dispatch; a disabled tool is also un-advertised at `initialize` so a client never sees it. Live-applied via `Arc<RwLock<McpToolsConfig>>` shared between `VaultSession.mcp_tools` and the handler — `set_setting` swaps the contents in place so the next dispatch sees the new gate without a vault restart


## Settings UI section

The settings pane ([[spec:settings-pane-section-list]]) gets a dedicated `mcp` section rendering the schema above as interactive rows (Enabled, Port, Max top-k, Allow-redacted, Log-full-input, the per-tool toggles + `writes_enabled` gate, and a read-only Discovery-file path). [mcp-settings-ui-section] Two rows carry behavior beyond their bool:
status:: done
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]]
touches:: [[code:hiker/panels/settings]]
note:: `app/src/panels/settings/mod.rs` `mcp` section — bool / string / port-int rows for every eligible key, plus a read-only display for `discovery_file`. Defaults to vault scope (matches `[tasks]`); user scope works via the per-section toggle

- **Host** — the pane renders a warning row underneath when the value isn't `127.0.0.1` / `localhost` / `::1`, so the user sees the LAN-exposure consequence where they set it.
- **Review required** — bool, default `true`, per [[spec:agent-write-review-mode]]. When on, every MCP tool-write produces a pending edit anchored against `accepted` instead of writing directly; off bypasses the inline patch-review UI (the edit commits into `accepted` and reaches disk on the next atomic write). Surfaced alongside the per-tool toggles.

Defaults to `vault` scope (the discovery file lives in the vault); user scope still works for a global default.

Loader and validator land alongside the v3 milestone. Until then the section is unrecognized and [[spec:settings-strict-load]] will refuse it. [mcp-config-section]


## Out of scope (v3)

- **stdio transport.** Standard for local MCP servers but conflicts with the "only when hiker is running" lifecycle constraint without ceremony. Deferred; if a concrete agent needs stdio, revisit.
- **Old HTTP+SSE transport.** Being deprecated in MCP spec evolution. No reason to support both old and new HTTP shapes.
- **Token-based auth.** Localhost-trust suffices for personal-machine-with-single-user. Token auth lands when a multi-user-machine or shared-host scenario is real. [mcp-token-auth-deferred]
- **Long-running operation streaming.** No v3 tool is long-running (reads return synchronously, writes are fast); streaming notifications wait until a tool genuinely needs them.
- **Cross-vault queries.** The MCP server is bound to one open vault. Multi-vault is `design.md`'s [[spec:search-multi-vault]] deferred slug.


## Forward refs

- `op-log.md` — the layered editing model. Agent writes are text edits against `accepted`; local-history rollback reads a plain-file snapshot. There is no `.ops` history engine.
- `git.md` — the `Hiker-Author` commit trailer that carries agent authorship when git is integrated (the durable attribution record).
- `llm.md` — `core::llm` background/fan-out (the other LLM surface). The MCP server here is the *sole* agent surface — no in-app chat, no ACP. External agents are the MCP clients connecting to this server.
- Future MCP tools (post-v3): trails-related (`list_trails` / `get_trail`), landmark-related (`list_landmarks`), collection-related (`list_collections` / `get_collection`), bulk write tools (`move_note` / `delete_note`), chunk-context (`expand_chunk` / `get_note_context`), streaming notifications. Each lands when its backing feature does, advertised dynamically.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **mcp-server-crate** — `mcp-server/` is now a library crate; `hiker_mcp::start(McpDeps) -> McpServerHandle` spawns an axum task wrapping `rmcp::StreamableHttpService`. the host (`open_vault_at_inner` → `start_mcp`) brings it up on vault open; the handle drops on session swap, cancelling the task and removing the discovery file. `mcp-server/tests/smoke.rs` exercises the full HTTP path [mcp-server-crate]
  status:: done
- **mcp-audit-log-mcp-calls** — `mcp-server/src/audit.rs::AuditLog` appends one JSONL row per call to `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl` with `surface="mcp-tool-call"`. When `[mcp.audit] log_full_input = false` (default), bulky fields (`content`, `query`, `fields`) are redacted to `{redacted: true, len: N}` [mcp-audit-log-mcp-calls]
  status:: done
- **mcp-tool-get-note** — snippet | full`. Snippet uses chunk 0 from the store when available, fallback head-of-file otherwise. Missing files return `1002 note_not_found` [mcp-tool-get-note]
  status:: done
  note:: evidence: `HikerHandler::get_note`; `detail = digest
- **mcp-tool-apply-tag-remove-tag** — `core::ops::agent_apply_tag` / `agent_remove_tag` thin wrappers over `agent_set_frontmatter` operating on the `tags` list. Idempotent (no-op when tag is already present / absent) [mcp-tool-apply-tag-remove-tag]
  status:: done
  implements:: [[code:hiker/ops/agent/apply_tag]], [[code:hiker/ops/agent/read_existing_tags]]
- **mcp-staging-read-disk-only** — `HikerHandler::get_note` resolves `vault.abs_path(rel_path)` and returns `1002 note_not_found` when the file doesn't exist on disk; pending proposals are not transparently surfaced. Agents discover staged content via the introspection tools, keeping the disk-vs-staging boundary explicit [mcp-staging-read-disk-only]
  status:: done
- **mcp-tool-get-pending-proposal** — wraps the op-log pending lookup by id; returns full metadata + proposed body. `1002`-style error when id is unknown (accepted / rejected / never existed). Agents cannot accept or reject from MCP — that stays a human-in-the-loop action [mcp-tool-get-pending-proposal]
  status:: planned
- **mcp-pending-proposal-anchor-status** — extends `get_pending_proposal` response: for `edit_note`-shaped proposals, recompute anchor match state on the fly against buffer-if-open-else-disk and return an `anchors: [{ edit_index, anchor_status: "holds"|"drifted"|"ambiguous", old_str_preview }]` array plus `target: "buffer"|"disk"`. Per-edit, joint-batch (resolved via shared `batch_id`); omitted entirely for whole-file shapes. Racy by construction (true at read time only). Lets the agent detect doomed staged edits in a concurrent-edit workflow and amend before the user clicks Accept, instead of finding out via drift-at-apply [mcp-pending-proposal-anchor-status]
  status:: planned
- **mcp-tool-board-add-column** — `mcp-server/src/handler/{router,dispatch}.rs` (`board_add_column` → `add_board_column`) drives `core::boards::ops::preview_edit(BoardEdit::AddColumn)`; idempotent on name collision (`status:"noop"`); review stages, direct commits; smoke `board_write_tools_round_trip_direct` + `board_create_commits_directly_even_in_review_mode`; toggle `mcp.tools.board_add_column_enabled` [mcp-tool-board-add-column]
  status:: done
- **mcp-tool-board-rename-column** — `mcp-server/src/handler/{router,dispatch}.rs` (`board_rename_column` → `rename_board_column`) drives `core::boards::ops::preview_edit(BoardEdit::RenameColumn)`; review stages, direct commits; smoke `board_write_tools_round_trip_direct`; toggle `mcp.tools.board_rename_column_enabled` [mcp-tool-board-rename-column]
  status:: done
- **mcp-tool-board-reorder-column** — `mcp-server/src/handler/{router,dispatch}.rs` (`board_reorder_column` → `reorder_board_column`) drives `core::boards::ops::preview_edit(BoardEdit::ReorderColumn)`; clamps `to_index` to tail; review stages, direct commits; toggle `mcp.tools.board_reorder_column_enabled` [mcp-tool-board-reorder-column]
  status:: done
- **mcp-tool-board-delete-column** — `mcp-server/src/handler/{router,dispatch}.rs` (`board_delete_column` → `delete_board_column`) drives `core::boards::ops::preview_edit(BoardEdit::DeleteColumn)`; drops the column's card refs (notes untouched); review stages, direct commits; toggle `mcp.tools.board_delete_column_enabled` [mcp-tool-board-delete-column]
  status:: done
