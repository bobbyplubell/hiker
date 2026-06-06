# MCP server

Hiker exposes its vault as an MCP server so external agents (Claude Code, Goose, Codex, custom MCP clients) can read, search, and write notes. Lands the v3 milestone from `design.md`.

**In-process, decoupled by crate.** `core::mcp` is a sibling crate (`mcp-server/`); the UI launches it on vault open and stops it on vault close. Single-process means MCP shares the indexer's writer and the read store — no two-writer coordination — while UI imports zero MCP types and MCP imports zero UI types. [mcp-in-process, mcp-crate-decoupled] The implementation library is **rmcp** (the official Anthropic Rust SDK), wrapped in hiker's own tool-surface trait the same way graniet/`llm` is. [mcp-rmcp-backed]


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
   (writer connection in indexer task; core::changes is a thin projection over the oplog)
```

`core::mcp` is started from the host on vault open with three handles:
- `IndexerHandle` (for write tools — routes through the same MPSC the UI uses).
- A read `Store` clone (for read tools — shares the existing `read_store` pool from the arch cleanup).
- The vault's `Vault` for path resolution + abs-path translation when needed.

Tokio task lifecycle: spawned at vault open, dropped at vault close. The HTTP listener binds an ephemeral port; the bound address is written to `vault/.hiker/mcp.json` for agents to discover. Listener task drains gracefully on close.


## Tool surface

Read + write tools, covering the cases with concrete value today and leaving room for the rest as backing features land. Every agent write produces ops in the document's op log tagged `author='agent:<client-id>'` (per `op-log.md`); `write_note` stamps `hiker.author: agent-authored` *only when creating* a note, and every other write skips the stamp. [mcp-tool-surface, mcp-author-stamp-on-create-only] The canonical registered list is in Capability negotiation below.

### Read tools

- **`search_notes(query: string, modes?: SearchModes, top_k?: number)`** — wraps `core::search::query`. Returns `SearchResponse` per the existing `search-cmd` shape. Default top_k is the spec's `FUSED_TOP_K = 20`; agents can request smaller (1) or larger (up to a config-pinned cap, default 50). [mcp-tool-search-notes]
- **`get_note(rel_path: string, detail?: 'digest'|'snippet'|'full')`** — fetch a single note. `digest` returns id + title + (when summary enrichment lands) cached summary. `snippet` returns top-1 chunk + heading_path. `full` returns the entire body. Default for explicit `get_note` calls is `full`; multi-hit search responses default to `digest`. [mcp-tool-get-note, mcp-progressive-disclosure]
- **`related_notes(rel_path: string, top_k?: number)`** — wraps the existing `related-notes-query`. Returns the same `RelatedHit` shape the UI's related panel already consumes. [mcp-tool-related-notes]

### UI context tools

Three read tools surfacing "what is the user looking at right now." No new permission beyond vault-read (every value is derivable with an extra `get_note` round-trip). All honor per-tool toggles and return inert payloads when no buffer is focused.

- **`get_active_note()`** — the focused editor tab's vault-relative path plus the cursor's byte offset and (if non-empty) the selection's `{ start_byte, end_byte }`. Buffer-only — an app-page tab (settings / home / queue / etc., per `tab-kinds`) returns `{ path: null }`. Same value `chat-active-note-context-injection` injects per turn. [mcp-tool-get-active-note]
- **`get_open_notes()`** — the ordered list of open `buffer` tabs, each `{ path, active: bool }`, in tab-strip order. Non-buffer tab kinds (`agent` / `graph` / `home`) are omitted. [mcp-tool-get-open-notes]
- **`get_selection()`** — the active buffer's selection as `{ path, start_byte, end_byte, text }` when non-empty, else `{ path: null }`. Same data the chat input's `@selection` token captures (per `chat-input-at-selection`). [mcp-tool-get-selection]

All three are read-only and bypass the read-before-write set — calling them does **not** count as "the agent read this path" for `mcp-read-before-write` purposes; only `get_note` populates the read set. Otherwise an agent could `get_open_notes()` and then claim it had read the file.

### Pending-op introspection tools (not yet implemented)

When `review_required` is on (per `agent-write-review-mode`), an agent's write enters the document's pending queue instead of landing on disk, so a follow-up `get_note` against that path returns `1002 note_not_found`. Three tools are specced to let an agent confirm, inspect, and revise its own pending work, all wrapping `core::oplog::query` filtered to pending ops by surface + session:

- **`list_pending_proposals(filter?)`** — list pending ops visible to MCP (default scope `surface = "mcp-tool-call"`); returns `{ op_id, target_path, action, surface, session_id, created_at, content_hash }` per op, no body. [mcp-tool-list-pending-proposals]
- **`get_pending_proposal(op_id)`** — one pending op's metadata + proposed `content`; read-only (accept/reject is human-only). For `edit_note`-shaped ops it adds an `anchors` array (one per `Replace` op in the batch, resolved by shared `batch_id`) recomputed against `materialize(accepted)`, each `{ edit_index, anchor_status, old_str_preview }` where `anchor_status` is `holds` (matches once) / `drifted` (zero matches) / `ambiguous` (>1 match, edit wasn't `replace_all`). Racy by construction. Whole-document ops omit `anchors` (treat absence as "n/a"). [mcp-tool-get-pending-proposal, mcp-pending-proposal-anchor-status]
- **`amend_pending_proposal(op_id, new_content)`** — replace a pending op's payload in place (same `metadata.client_id` only; whole-document shapes only — `edit_note` batches re-issue after accept/reject). Recomputes `content_hash`, stamps `amended_at_ms`, increments `amend_count`, discards the prior payload (no version history), fires op-log change events. If the user has already accepted, the op has left the queue and the call returns `1002` — "amend works until the user takes action," so the human still gets exactly one gate per accepted change. [mcp-tool-amend-pending-proposal]

None of the three is registered in the router today — they appear only inside two tool descriptions. Tracked as `bug-mcp-pending-proposal-tools-unimplemented`. Per-tool toggles for them (`*_enabled`) follow the standard pattern once built.

### Write tools

All writes route through `core::ops`. Every agent write produces ops tagged `author='agent:<client-id>'`: applied to the document's `accepted` Doc when `review_required` is off, queued in `<doc-id>.pending` when it's on. Authorship stamping is creation-only (full statement under Authorship + audit trail, `mcp-author-stamp-on-create-only`).

**Pending-mode caveat — load-bearing for agent behavior.** When `[mcp.tools].review_required` is on (see `agent-write-review-mode`), every write tool produces ops with `status = pending` *instead of* writing to disk, returning `{ status: "staged", proposal_id }` (or `proposal_ids` for `edit_note`, one per edit); direct mode returns `{ status: "written" }`. The file is **not** visible on disk or via `get_note` until the user accepts — `get_note` returns `1002 note_not_found` for a path that exists only as pending ops (per `mcp-staging-read-disk-only`). Tool descriptions surface this in plain language so the agent doesn't mistake a pending write for a failed one. `edit_note` produces *one `Replace` op per edit* sharing a `batch_id` per `op-log-op-shape`; the other write tools produce one op per call. [mcp-write-tools-staging-aware]

- **`write_note(rel_path: string, content: string, expected_hash?: string)`** — create or replace a note's body. If `expected_hash` is provided, the write is drift-aware (checks against `materialize(accepted)`); without it, an unconditional write. Refuses paths under `.hiker/`. Stamps `hiker.author: agent-authored` on the resulting frontmatter *only when the target path did not previously exist* (per `mcp-author-stamp-on-create-only`). When the target path already exists, the call requires the agent to have read the note in the current session via `get_note` first (`1008 read_required`); see `mcp-read-before-write`. Creates are exempt. Returns the new content hash. [mcp-tool-write-note]
- **`edit_note(rel_path: string, edits: [{ old_str: string, new_str: string, replace_all?: bool }])`** — apply one or more span-anchored patches to an existing note. Each `old_str` must match exactly once in the file unless `replace_all: true`. Refuses non-existent paths (use `write_note` to create). Validation happens at receive time as one transaction; on any failure the whole call rejects and nothing is queued. Returns `{ status: "staged", proposal_ids: [...] }` in review mode or `{ status: "written", content_hash }` in direct mode. [mcp-tool-edit-note]

  Validation rules (all must hold before the call is accepted):

  1. **Path exists.** Non-existent path → `1002 note_not_found`. Creates go through `write_note`.
  2. **Per-edit anchor resolves uniquely.** Each `old_str` matches exactly one byte range in the current file content. Multiple matches without `replace_all: true` → `invalid_params` naming the offending edit index. Zero matches → `1003 drift`.
  3. **No textual overlap.** No two edits' resolved byte ranges may overlap. Overlap → `invalid_params` naming the offending pair. Two edits modifying the same span are conceptually one edit; the agent merges them into a single edit with a larger `old_str` / `new_str`.
  4. **All anchors hold against the *pre-application* file.** Each `old_str` is resolved against the original file content, not against the running buffer of earlier edits' results. Sequential dependencies between edits (where edit B's anchor only appears after edit A is applied) are rejected as `invalid_params`. The agent expresses such dependencies as one edit with a wider span.
  5. **Path was read this session.** The agent must have called `get_note(rel_path)` (any detail level) at least once in the current MCP session before issuing `edit_note` against the path. Editing a note the agent hasn't seen is overwhelmingly a hallucinated-anchor situation; the per-session read set makes the foot-gun an explicit error (`1008 read_required`) instead of a silent garbage edit. The check is per-session (not per-call) — re-issuing `edit_note` against the same path doesn't require re-reading. See `mcp-read-before-write`. [mcp-edit-note-validation]

  After validation passes, the call emits N `Replace` ops to the document's op log (one per edit) sharing a `batch_id` in metadata so consumers can group them as one originating tool call. When `[mcp.tools].review_required` is off, ops enter as `status = accepted` and the save-to-disk projection runs once for the batch per `op-log-atomic-write`. When on, ops enter as `status = pending`.

- **`set_frontmatter(rel_path: string, fields: map<string, json>)`** — merge frontmatter fields into a note. Implementation merges into the existing frontmatter via a small frontmatter-aware writer (`core::ops::set_frontmatter`). Used for summary writes, status changes, and other structured-metadata mutations. Does not stamp `hiker.author: agent-authored` (per `mcp-author-stamp-on-create-only`). [mcp-tool-set-frontmatter]
- **`apply_tag(rel_path: string, tag: string)`** / **`remove_tag(rel_path: string, tag: string)`** — convenience wrappers over `set_frontmatter` for the most common case. [mcp-tool-apply-tag]

### Trail tools

Trails (per `trails.md`) get a six-tool surface — three read, three write — so agents can both consume curated context and transcribe their investigations as draft trails. Write tools route through `core::ops::agent_*` like every other MCP write and produce pending ops when `agent-write-review-mode` is on.

- **`trails_list(filters?)`** — enumerate trails with optional filters (containing-note, recently-activated, name-substring); returns id + title + waypoint count + activation timestamp + path. [mcp-tool-trails-list]
- **`trail_get(id, detail?)`** — full trail-doc body + ordered waypoint list (each waypoint's source-note ref + annotation body); detail levels mirror `mcp-tool-get-note`'s `digest` / `full`. [mcp-tool-trail-get]
- **`trails_containing_note(rel_path)`** — reverse lookup; returns trails that include the given note as a waypoint. [mcp-tool-trails-containing-note]
- **`trail_create(name)`** — create a new trail (empty waypoint list, default placement per `[trails] new_trail_dir`); returns id + path. [mcp-tool-trail-create]
- **`trail_append_waypoint(trail_id, source_rel, annotation?)`** — append a waypoint; creates the waypoint-note under `.hiker/trails/<trail-id>/waypoints/`, links to source, seeds optional starter annotation (omitted → empty body). [mcp-tool-trail-append-waypoint]
- **`trail_remove_waypoint(trail_id, waypoint_id)`** — symmetric to the sidebar's `trails-mode-remove-waypoint-verb`; routes the waypoint-note delete through `core::ops::delete` so it lands in trash. [mcp-tool-trail-remove-waypoint]

### Board tools

Boards (per `kanban.md`) get a read + curate MCP surface so attached agents can read boards as context and reorganize them. Every **write** tool routes through the same op-log user-save path the board UI uses and produces a pending op when `agent-write-review-mode` is on (the staged board-doc edit appears in the patch-review surface; disk is unchanged until accept), commits via `op_writes::user_save` in direct mode, returns `{status: "staged", proposal_id}` in review mode or `{status: "written"}` direct, and is independently toggleable under `[mcp.tools]`. Card-targeting writes identify the card by its board-local `card_id` (from `board_get`); column writes by column name. All board mutations touch only the board-doc frontmatter — referenced notes are never modified.

Read:

- **`boards_list()`** — enumerate every board-doc in the vault; returns `rel_path` + `board_id` + `title` + `column_count` + `card_count` per board (the `core::boards::list` shape). [mcp-tool-boards-list]
- **`board_get(rel_path)`** — full board-doc body + resolved columns, each column carrying its ordered cards (each card's `card_id`, title, and reference-resolution outcome), via `core::boards::get_board`. The `card_id`s it returns are the handles the write tools below take. [mcp-tool-board-get]

Write:

- **`board_create(name)`** — create a new board-doc (default `Todo`/`Doing`/`Done` columns) at the configured `[boards] new_board_dir`; returns the new `rel_path` + `board_id`. Wraps `core::boards::ops::create_board`. [mcp-tool-board-create]
- **`board_add_card(board_rel_path, column, source_rel_path)`** — append a note as a card to a column; idempotent per board (a note already on the board returns `status: "noop"`). Wraps `core::boards::ops::add_card`. [mcp-tool-board-add-card]
- **`board_add_text_card(board_rel_path, column, text)`** — append a freeform (non-note) text card to a column; returns the new `card_id`. Wraps `core::boards::ops::add_text_card`. [mcp-tool-board-add-text-card]
- **`board_move_card(board_rel_path, card_id, to_column, to_index?)`** — move/reorder a card to `to_column` at `to_index` (tail when omitted). Wraps `core::boards::ops::move_card`. [mcp-tool-board-move-card]
- **`board_set_card_text(board_rel_path, card_id, text)`** — rewrite a freeform card's text (errors on a note card). Wraps `core::boards::ops::set_card_text`. [mcp-tool-board-set-card-text]
- **`board_remove_card(board_rel_path, card_id)`** — drop a card from the board (the referenced note is untouched). Wraps `core::boards::ops::remove_card`. [mcp-tool-board-remove-card]
- **`board_add_column(board_rel_path, name)`** / **`board_rename_column(board_rel_path, old_name, new_name)`** / **`board_reorder_column(board_rel_path, name, to_index)`** / **`board_delete_column(board_rel_path, name)`** — column management; delete drops that column's card references (notes untouched). Wrap the matching `core::boards::ops::*_column` verbs. [mcp-tool-board-add-column, mcp-tool-board-rename-column, mcp-tool-board-reorder-column, mcp-tool-board-delete-column]

`repoint_card` (path-conflict resolution) is intentionally **not** exposed — re-pointing a card whose note identity changed is a human-judgment call surfaced as the board's Keep/Repoint/Break modal, not an agent action.

### Task queue tools

Per `task-queue.md`, the MCP server exposes the queue's checkout/submit surface so external rmcp clients can drain hiker's non-interactive LLM work. The same tools are in-process-dispatched to the basic chat agent's tool set when `[tasks] expose_to_chat_agent = true` — one tool registry shared across the chat agent and external clients.

- **`task_checkout(types?, shapes?, min_priority?, lease_secs?)`** — return the next eligible task or null; stamps a lease against the calling rmcp client id. [tasks-mcp-tool-checkout]
- **`task_submit(task_id, value)`** — write the result; validates against the task's `output_schema` if any. [tasks-mcp-tool-submit]
- **`task_fail(task_id, error)`** — agent gives up. [tasks-mcp-tool-fail]
- **`task_heartbeat(task_id)`** — extend the current lease. [tasks-mcp-tool-heartbeat]

Plus a read-only `task_list(states?, types?)` for queue inspection. [tasks-mcp-tool-list]

Two new positive error codes ride this surface: `1006` (`stale_lease`) and `1007` (`schema_violation`). See `task-queue.md` for behavior.

Cancellation is **not** an MCP tool. External agents learn cancellation via `stale_lease` on submit; mid-work cancellation push (rmcp server→client streamable notification) is `task-queue-mcp-cancel-notification`, deferred.

Notably absent from v3:

- `move_note`, `delete_note`, `create_folder` — heavier writes; deferred until a real motivating case appears.
- `list_landmarks`, `list_collections`, `get_collection` — landmarks/collections unbuilt; added (and advertised) when those features land. [mcp-tool-landmarks-deferred, mcp-tool-collections-deferred]
- `expand_chunk`, `get_note_context` — sketched in `design.md` but not load-bearing for v3. Deferred.
- Vision OCR helpers — depend on the extractor pipeline being real. Deferred to v4+.


## Read-before-write

Both write tools that touch *existing* content require a prior `get_note` call against the same path in the current MCP session. The rule is a foot-gun guard, not a security boundary: an agent that issues `edit_note` against a path it has never read is almost always hallucinating anchors (or rewriting the wrong file); blocking the call early — with a clear error naming the path and the required tool — turns the silent garbage-edit case into a recoverable one. [mcp-read-before-write]

- **Scope.** `edit_note` always requires a prior read. `write_note` requires a prior read only when the target path *already exists* on disk; creating a new note is exempt (there is nothing to have read). `set_frontmatter` / `apply_tag` / `remove_tag` are merge-into-frontmatter operations and don't need to have seen the body — they're exempt.
- **Read set is per-session.** Each MCP session (one rmcp connection) carries an in-memory `HashSet<rel_path>` populated by every successful `get_note` call. The set is dropped at session close. Re-issuing a write against the same path within the session doesn't require re-reading.
- **Implementation.** A small `ReadSet` lives on the per-session handler state in `mcp-server/src/handler.rs`, populated in `get_note_inner` after a successful fetch and consulted in `write_note_inner` / `edit_note_inner` before validation, ahead of the staging / direct-write branch. Per-session (not per-call) because the agent often already holds the content in its own context; it matches Claude Code's Read-before-Edit precedent.


## UI refresh on agent writes

Agent writes route through `core::ops::agent_*`, which suppress the watcher around the fs write (load-bearing for rename/delete correctness, see `watcher.md`). Suppression means the UI's watcher-file-events listener never fires for an agent-authored save, leaving the tree stale.

Resolution: ride the existing op-log append events. Every agent write appends a `Changes` row tagged `author = "agent:<client-id>"`; the host's tokio bridge re-emits each row as an op-log append event, which the home-page activity widget already consumes. The frontend's tree + buffer-reload code subscribes to the same event and applies its post-mutation refresh — gated on `author.startsWith("agent:")` so non-agent rows (user saves, rollbacks) keep flowing through the watcher path unchanged. [mcp-ui-refresh-on-agent-write]

When an accepted `edit_note` lands on a path whose buffer is currently dirty, the plain disk-reload path would clobber the user's unsaved edits. The patch-review accept flow (see `patch-review.md`) instead applies the span-anchored patch to both disk and the in-memory buffer in one transactional move, refusing with a clear error when the user's edits have clobbered the anchor. Direct-mode agent writes use this same machinery: the append-events listener delegates to the patch-aware buffer-update path when the row carries an `edit_note` patch in metadata, falling back to disk reload otherwise.


## Authorship + audit trail

Every accepted MCP-driven write produces two artifacts, plus a frontmatter stamp only on creation:

1. **Op-log entries** (per `op-log.md`) — one or more ops with `author='agent:<client-id>'`, `status` reflecting `review_required`, and `metadata` carrying `{ tool: "<tool-name>", session_id: "<session>", reason: "<optional>", batch_id?: "<id>" }`. `batch_id` is set on `edit_note`-derived `Replace` ops so the activity feed can group per-edit ops back to their originating tool call. This is the rollback substrate — `materialize` at any prior op reconstructs the document's state at that point.
2. **An entry in `vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`** — the existing LLM-strategy audit log, per `llm.md`. Records the MCP call itself (tool name, input, response status, timestamp) for telemetry/debugging — separate concern from the content-change log in the op log. [mcp-audit-log-jsonl]

**Frontmatter stamp on creation only.** When `write_note` brings a note into existence (the target path didn't exist), the resulting frontmatter carries `hiker.author: agent-authored` (and optionally `hiker.provenance: mcp-<client-id>`). Every other write tool — `write_note` against an existing path, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag` — skips the stamp. The frontmatter field expresses *origin*; per-modification provenance lives on each op's `author` field. [mcp-author-stamp-on-create-only]

The two artifacts have different consumers: the op log is the content-change log (powers rollback UI), the JSONL is the call-telemetry log (powers prompt-edit debugging and cost transparency per `llm.md`).


## HTTP server + transport

rmcp's Streamable HTTP transport (the only v3 transport; stdio and old HTTP+SSE are deferred): single endpoint accepting POST for client→server messages and GET (with SSE upgrade) for server→client streaming. JSON-RPC 2.0 over the wire. Cutting stdio keeps the "only when hiker is running" lifecycle trivially honored, since the server is just a tokio task in hiker's process. [mcp-transport-streamable-http]

**Bind address.** Default `127.0.0.1` (localhost-only). The auth model is localhost-trust — no token; anyone who can reach the port is trusted, and the discovery file is local-readable but not network-reachable. [mcp-localhost-trust] Configurable in `[mcp] host` for LAN access; a non-loopback bind keeps the same trust model (effectively *trust everyone on the LAN*), so the settings UI warns when the value isn't `127.0.0.1`. `0.0.0.0` is allowed for users gating an all-interfaces bind behind their own reverse proxy. [mcp-bind-host-configurable]

**Port.** Default ephemeral (port 0 → OS-assigned), written with the connect URL to the discovery file at startup; configurable to a fixed port for static MCP config. [mcp-port-discovery] Configurable in `[mcp]`:

```toml
[mcp]
enabled = true                 # turns the whole server on/off
host = "127.0.0.1"             # bind address; localhost-trust auth requires careful thought before changing
port = 0                       # 0 = ephemeral; otherwise a fixed port
discovery_file = ".hiker/mcp.json"   # vault-relative; written on bind, removed on shutdown
max_top_k = 50                 # cap on agent-requested top_k for search/related
```

[mcp-config-section]

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
- **Notes (write):** `write_note`, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag`.
- **Boards:** `boards_list`, `board_get`, `board_create`, `board_add_card`, `board_add_text_card`, `board_move_card`, `board_set_card_text`, `board_remove_card`, `board_add_column`, `board_rename_column`, `board_reorder_column`, `board_delete_column`.
- **Task queue:** `task_checkout`, `task_submit`, `task_fail`, `task_heartbeat`, `task_list`.

(The pending-op introspection tools above — `list_pending_proposals`, `get_pending_proposal`, `amend_pending_proposal` — are *not* registered; see `bug-mcp-pending-proposal-tools-unimplemented`.) Conditionally advertised: the mechanism is built for future tools that depend on backing features (trails, landmarks, collections, vision extractors) — each defines an `is_available()` predicate and the server filters at initialize time, so agents see a coherent capability set instead of calling tools that error with "feature not implemented." [mcp-dynamic-capabilities]


## Lifecycle awareness (not yet implemented)

The lifecycle fields are a deferred `design.md` feature; until they land the MCP server treats them as absent and returns everything. The intended behavior: `search_notes` / `get_note` / `related_notes` exclude notes with `hiker.archived` / `hiker.redacted` / `hiker.retired` by default, with a `scope` opt-in to include them; **redacted notes return id + title only** regardless of scope. Enforcement lives in `core::search::query` and `core::store::get_note` (not the MCP layer), so the same rules apply to UI search and any other consumer once built. [mcp-lifecycle-aware]


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
    - `1008` (`read_required`) — `write_note` against an existing path, or `edit_note` against any path, when the agent hasn't called `get_note` for that path in the current MCP session. The error message names the path and the tool the agent should call first. See `mcp-read-before-write`.

Error messages are user-facing-friendly enough that an agent can echo them back to its user without translation. [mcp-error-model]


## Configuration

Full `[mcp]` schema:

```toml
[mcp]
enabled = true                       # off → server doesn't bind, no MCP
host = "127.0.0.1"                   # bind address; non-loopback exposes vault — see warning above
port = 0                             # 0 = ephemeral
discovery_file = ".hiker/mcp.json"   # vault-relative
max_top_k = 50

[mcp.tools]
# Master write gate, kept for backwards-compat: when false, every write tool
# is refused with `1004 disabled` regardless of the per-tool flags below.
writes_enabled = true
allow_redacted_lookup = false

# Plus one `<tool>_enabled` toggle per registered tool (default true) —
# every note read/write, every board tool, and every task_* tool (the full
# list in core::config::sections::McpToolsConfig). Each is independent of
# `writes_enabled`: flipping one off hides that tool from `agent_tool_defs`
# and rejects direct rmcp calls with `1004 disabled` even when writes are on.

[mcp.audit]
log_full_input = false               # mirror of [llm.audit] log_full_prompt; default off
```

Per-tool toggles apply live (the next dispatch re-gates immediately, no restart). [mcp-tool-toggles] Bind-affecting changes — `enabled` / `host` / `port` / `discovery_file` / `max_top_k` / `audit.log_full_input` — trigger an in-place restart from `set_setting` (the handle drops, cancelling the axum task and removing the discovery file, then `hiker_mcp::start(...)` rebinds) while keeping the vault session intact. [mcp-server-restart-on-config-change, mcp-config-section]


## Settings UI section

The settings pane (`settings-pane-section-list`) gets a dedicated `mcp` section rendering the schema above as interactive rows (Enabled, Port, Max top-k, Allow-redacted, Log-full-input, the per-tool toggles + `writes_enabled` gate, and a read-only Discovery-file path). [mcp-settings-ui-section] Two rows carry behavior beyond their bool:

- **Host** — the pane renders a warning row underneath when the value isn't `127.0.0.1` / `localhost` / `::1`, so the user sees the LAN-exposure consequence where they set it.
- **Review required** — bool, default `true`, per `agent-write-review-mode`. When on, every MCP tool-write produces ops with `status = pending` instead of writing directly; off bypasses the inline patch-review UI (ops enter `accepted` and reach disk on the next save-projection). Surfaced alongside the per-tool toggles.

Defaults to `vault` scope (the discovery file lives in the vault); user scope still works for a global default.

Loader and validator land alongside the v3 milestone. Until then the section is unrecognized and `settings-strict-load` will refuse it. [mcp-config-section]


## Out of scope (v3)

- **stdio transport.** Standard for local MCP servers but conflicts with the "only when hiker is running" lifecycle constraint without ceremony. Deferred; if a concrete agent needs stdio, revisit.
- **Old HTTP+SSE transport.** Being deprecated in MCP spec evolution. No reason to support both old and new HTTP shapes.
- **Token-based auth.** Localhost-trust suffices for personal-machine-with-single-user. Token auth lands when a multi-user-machine or shared-host scenario is real. [mcp-token-auth-deferred]
- **Long-running operation streaming.** No v3 tool is long-running (reads return synchronously, writes are fast); streaming notifications wait until a tool genuinely needs them.
- **Cross-vault queries.** The MCP server is bound to one open vault. Multi-vault is `design.md`'s `search-multi-vault` deferred slug.


## Forward refs

- `op-log.md` — the substrate. Agent writes are ops; rollback walks the log.
- `core::activity` (`op-log.md` "History materialization") — thin projection over the op log; load-bearing for agent rollback UX (no separate snapshot directory — agent writes are ops like any other, and the home-page agent-activity widget + detail view query this projection to render the feed and drive rollback). v3 ships them together. [mcp-rollback-via-changes]
- `editor.md` vault home page — agent-activity widget + detail view consume the `core::changes` projection.
- `llm.md` — interactive LLM features (chat over vault, vision OCR) flow through external ACP agents. Those agents are the typical MCP clients connecting to this server.
- Future MCP tools (post-v3): trails-related (`list_trails` / `get_trail`), landmark-related (`list_landmarks`), collection-related (`list_collections` / `get_collection`), bulk write tools (`move_note` / `delete_note`), chunk-context (`expand_chunk` / `get_note_context`), streaming notifications. Each lands when its backing feature does, advertised dynamically.
