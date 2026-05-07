# MCP server

Hiker exposes its vault as an MCP server so external agents (Claude Code, Goose, Codex, custom MCP clients) can read, search, and write notes. Lands the v3 milestone from `design.md`.

The headline decisions:

- **In-process with hiker UI, decoupled by crate.** `core::mcp` is a sibling crate (`mcp-server/`); UI launches it on vault open, stops it on vault close. Single-process means MCP shares the indexer's writer and the read store — no two-writer coordination problem. UI imports zero MCP types; MCP imports zero UI types. [mcp-in-process, mcp-crate-decoupled]
- **Streamable HTTP transport only for v3.** Single transport. stdio and old-style HTTP+SSE are deferred. Streamable HTTP is the modern MCP transport and is supported by every current ACP-side agent ecosystem; cutting stdio for v3 keeps the lifecycle constraint ("only when hiker is running") trivially honored, since the server is just a tokio task in hiker's process. [mcp-transport-streamable-http]
- **rmcp as the implementation library.** Official Anthropic Rust SDK named in `design.md`'s target stack. Same wrap-it-in-our-own-trait discipline as graniet/`llm`: hiker's MCP-facing code defines its own tool surface; the rmcp Server wires it to the wire. [mcp-rmcp-backed]
- **Read + write tools both.** Read: `search_notes`, `get_note`, `related_notes`. Write: `set_frontmatter`, `apply_tag`, `write_note`. Every write stamps `hiker.author: agent-authored` and appends a `changes.db` row tagged `author='agent:<client-id>'` per `changes.md`. [mcp-tool-surface]
- **Agent rollback via `core::changes`.** No separate snapshot directory. Agent writes log to `changes.db` like any other write; the home page's agent-activity widget (per `editor.md`) and detail view query that log to render the activity feed and drive rollback. [mcp-rollback-via-changes]
- **Localhost-trust auth for v3.** No token. The HTTP server binds to `127.0.0.1` only (never an external interface), and any process on the local machine that can reach the port can connect. Token-based auth is deferred until there's a concrete multi-user-machine or shared-host need. The discovery file is local-readable but not network-reachable. [mcp-localhost-trust]
- **Random ephemeral port written to a discovery file.** Port chosen by the OS at startup; written to `vault/.hiker/mcp.json` with the URL the agent should connect to. Most security gain over a fixed port; agents read the discovery file (already in the vault directory they have access to). Configurable to a fixed port if a user wants stability for static MCP config. [mcp-port-discovery]
- **Dynamic capability advertising.** Tools that depend on unbuilt features (`list_trails`, `list_landmarks`, `list_collections`, vision OCR helpers) aren't advertised at `initialize` time. Agents see only what's actually backed by working code. [mcp-dynamic-capabilities]


## Architecture

```
                   hiker UI process (Tauri)
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
   core::store + core::changes
   (writer connection in indexer task)
```

`core::mcp` is started from `ui/src-tauri/src/lib.rs` on vault open with three handles:
- `IndexerHandle` (for write tools — routes through the same MPSC the UI uses).
- A read `Store` clone (for read tools — shares the existing `read_store` pool from the arch cleanup).
- The vault's `Vault` for path resolution + abs-path translation when needed.

Tokio task lifecycle: spawned at vault open, dropped at vault close. The HTTP listener binds an ephemeral port; the bound address is written to `vault/.hiker/mcp.json` for agents to discover. Listener task drains gracefully on close.


## Tool surface

v3 ships a deliberately small surface — three reads, three writes — covering the cases that have concrete value today and leaving room for the rest as backing features land.

### Read tools

- **`search_notes(query: string, modes?: SearchModes, top_k?: number)`** — wraps `core::search::query`. Returns `SearchResponse` per the existing `search-tauri-cmd` shape. Default top_k is the spec's `FUSED_TOP_K = 20`; agents can request smaller (1) or larger (up to a config-pinned cap, default 50). [mcp-tool-search-notes]
- **`get_note(rel_path: string, detail?: 'digest'|'snippet'|'full')`** — fetch a single note. `digest` returns id + title + (when summary enrichment lands) cached summary. `snippet` returns top-1 chunk + heading_path. `full` returns the entire body. Default for explicit `get_note` calls is `full`; multi-hit search responses default to `digest`. [mcp-tool-get-note, mcp-progressive-disclosure]
- **`related_notes(rel_path: string, top_k?: number)`** — wraps the existing `related-notes-query`. Returns the same `RelatedHit` shape the UI's related panel already consumes. [mcp-tool-related-notes]

### Write tools

All writes route through `core::ops` (or directly through the indexer for finer-grained ops). Every write appends a `changes.db` row tagged `author='agent:<client-id>'` and stamps `hiker.author: agent-authored` in the affected note's frontmatter (when applicable).

- **`write_note(rel_path: string, content: string, expected_hash?: string)`** — create or replace a note's body. If `expected_hash` is provided, the write is a `write_file_checked` (drift-aware); without it, an unconditional write. Returns the new content hash. Used by agents creating new notes or regenerating content. [mcp-tool-write-note]
- **`set_frontmatter(rel_path: string, fields: map<string, json>)`** — merge frontmatter fields into a note. Implementation merges into the existing frontmatter via a small frontmatter-aware writer (`core::ops::set_frontmatter`, new — currently `core::vault::write_file` is body-only). Stamps `hiker.author: agent-authored` automatically. Used for tag application, summary writes, status changes. [mcp-tool-set-frontmatter]
- **`apply_tag(rel_path: string, tag: string)`** / **`remove_tag(rel_path: string, tag: string)`** — convenience wrappers over `set_frontmatter` for the most common case. [mcp-tool-apply-tag]

Notably absent from v3:

- `move_note`, `delete_note`, `create_folder` — heavier write operations. Agent enrichment use cases don't need them; deferred until a real motivating case appears.
- `list_trails`, `get_trail`, `list_landmarks`, `list_collections`, `get_collection` — backing features don't exist yet (trails are deferred; landmarks and collections are unbuilt). When they land, the corresponding tools are added; until then they're not advertised at initialize. [mcp-tool-trails-deferred, mcp-tool-landmarks-deferred, mcp-tool-collections-deferred]
- `expand_chunk`, `get_note_context` — sketched in `design.md` but not load-bearing for v3 use cases. The chunk-id format issue can be settled later. Deferred.
- Vision OCR helpers — depend on the extractor pipeline being real. Deferred to v4+.


## Authorship + audit trail

Every MCP-driven write produces three artifacts, in this order:

1. **A `changes.db` row** (per `changes.md`) with `op='created'|'modified'|'deleted'`, `author='agent:<client-id>'`, full post-op `content`, and `metadata` carrying `{ tool: "<tool-name>", session_id: "<session>", reason: "<optional>" }`. This is the rollback substrate — the `content` blob lets the UI's agent-activity detail view reconstruct any prior state.
2. **A frontmatter stamp** on the affected note: `hiker.author: agent-authored` and (optionally) `hiker.provenance: mcp-<client-id>` for fine-grained provenance per `design.md`'s authorship trichotomy.
3. **An entry in `vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`** — the existing LLM-strategy audit log, per `llm.md`. Records the MCP call itself (tool name, input, response status, timestamp) for telemetry/debugging — separate concern from the content-change log in `changes.db`. [mcp-audit-log-jsonl]

Why both `changes.db` rows and JSONL entries: `changes.db` is content-change log (what files changed, what's the rollback target); JSONL is call-telemetry log (which tool ran, what was sent, did it succeed). Different consumers — `changes.db` powers rollback UI, JSONL powers prompt-edit debugging and cost transparency per `llm.md`. They overlap in mention but not in shape.


## HTTP server + transport

rmcp's Streamable HTTP transport: single endpoint accepting POST for client→server messages and GET (with SSE upgrade) for server→client streaming. JSON-RPC 2.0 over the wire.

**Bind address.** `127.0.0.1` only. Never `0.0.0.0`. The server is local-only by construction; users wanting LAN/remote access need to set up their own reverse proxy and own the security implications. [mcp-bind-localhost-only]

**Port.** Default ephemeral (port 0 → OS-assigned). Configurable in `[mcp]`:

```toml
[mcp]
enabled = true                 # turns the whole server on/off
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

At rmcp `initialize` time, hiker advertises the tool list dynamically based on what features are present:

- Always advertised in v3: `search_notes`, `get_note`, `related_notes`, `write_note`, `set_frontmatter`, `apply_tag`, `remove_tag`.
- Conditionally advertised: nothing in v3, but the mechanism is built for future tools that depend on backing features (trails, landmarks, collections, vision extractors). Each future tool defines a `is_available()` predicate; the server filters its tool list at initialize time. [mcp-dynamic-capabilities]

This means agents see a coherent capability set rather than calling tools that error with "feature not implemented." Cleanest UX for the agent side; modest implementation cost for hiker.


## Lifecycle awareness

By default, `search_notes`, `get_note`, and `related_notes` exclude notes with `hiker.archived` / `hiker.redacted` / `hiker.retired` set, per `design.md`'s lifecycle operations section. Agents can opt in via a `scope` parameter to include them when intentionally auditing or recovering history. **Redacted notes are returned as id + title only** — body and chunks unreachable via MCP regardless of scope. [mcp-lifecycle-aware]

This is enforced in `core::search::query` and `core::store::get_note`, not at the MCP layer, so the same rules apply to the UI's search and any other consumer.

In v3 these lifecycle fields aren't yet implemented in hiker (they're a deferred feature in `design.md`). The MCP server treats them as absent and returns everything; once lifecycle lands, the filter is automatic.


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

Error messages are user-facing-friendly enough that an agent can echo them back to its user without translation. [mcp-error-model]


## Configuration

Full `[mcp]` schema:

```toml
[mcp]
enabled = true                       # off → server doesn't bind, no MCP
port = 0                             # 0 = ephemeral
discovery_file = ".hiker/mcp.json"   # vault-relative
max_top_k = 50

[mcp.tools]
writes_enabled = true                # gate on/off for the write tools (set_frontmatter / apply_tag / write_note); reads always available when MCP is on
allow_redacted_lookup = false        # if true, agents passing scope can fetch redacted bodies; default conservative

[mcp.audit]
log_full_input = false               # mirror of [llm.audit] log_full_prompt; default off
```

Loader and validator land alongside the v3 milestone. Until then the section is unrecognized and `settings-strict-load` will refuse it. [mcp-config-section]


## Out of scope (v3)

- **stdio transport.** Standard for local MCP servers but conflicts with the "only when hiker is running" lifecycle constraint without ceremony. Deferred; if a concrete agent needs stdio, revisit.
- **Old HTTP+SSE transport.** Being deprecated in MCP spec evolution. No reason to support both old and new HTTP shapes.
- **Token-based auth.** Localhost-trust suffices for personal-machine-with-single-user. Token auth lands when a multi-user-machine or shared-host scenario is real. [mcp-token-auth-deferred]
- **Long-running operation streaming.** None of the v3 tools are long-running — search, get_note, related_notes return synchronously; writes are fast. Reindex isn't an MCP-triggered operation. Streaming notifications are deferred until a tool genuinely needs them.
- **Bulk write tools** (`move_note`, `delete_note`, `create_folder`). The agent enrichment cases v3 targets don't need them; the rollback story is simpler when writes are content-only.
- **Cross-vault queries.** The MCP server is bound to one open vault. Multi-vault is `design.md`'s `search-multi-vault` deferred slug.


## Forward refs

- `core::changes` (`changes.md`) — load-bearing for agent rollback. v3 ships them together.
- `editor.md` vault home page — agent-activity widget + detail view consume `core::changes`.
- `llm.md` — interactive LLM features (chat over vault, vision OCR) flow through external ACP agents. Those agents are the typical MCP clients connecting to this server.
- Future MCP tools (post-v3): trails-related (`list_trails` / `get_trail`), landmark-related (`list_landmarks`), collection-related (`list_collections` / `get_collection`), bulk write tools (`move_note` / `delete_note`), chunk-context (`expand_chunk` / `get_note_context`), streaming notifications. Each lands when its backing feature does, advertised dynamically.
