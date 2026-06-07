# LLM strategy

How hiker uses generative LLMs. Pins the modules, the routing rule per feature type, where prompts and config live, and the policy posture that keeps subscription-billed agents in the role they're priced for.

Embeddings are out of scope of this doc — `core::embed` is its own module with its own trait and version-tag machinery (see `index.md`'s embedder section). The two consumers of the `llm` crate — `core::embed::LlmEmbedder` and `core::llm` — share a dep but have separate trait boundaries and policy postures: embeddings are always automation-shaped (pay-per-call APIs always), and the interactive-vs-background distinction below applies only to generative use.

- **`core::llm` is the foundation.** A new module wrapping the [`llm`](https://crates.io/crates/llm) crate (graniet/llm) for multi-provider access — Anthropic, OpenAI, Ollama, Google, Groq, Mistral, DeepSeek, etc. Module discipline: `llm` crate confined to this module, mirroring rusqlite-only-in-store and fastembed-only-in-embed. [llm-core-module]
- **Background and fan-out features submit to `core::tasks`; the in-process direct-LLM worker drains via `core::llm`.** Single-shot prompts for auto-tag-on-save, summary-on-save, cluster summarization (background); pre-scoped batch fan-outs for RAPTOR-shaped tree building, cluster naming across N clusters, regenerate-all-summaries (fan-out). The queue layer (per `task-queue.md`) lets external rmcp clients — Claude Code, Codex, an ACP-driven Goose acting as an MCP client — drain the same queue if the user has them attached. Pay-per-call billing on the direct-LLM lane — no ToS grey area. [llm-strategy-direct-non-interactive]
- **Interactive features use a basic in-hiker agent loop by default.** A new module (`core::agent`) implementing a simple message-history + tool-dispatch loop on top of `core::llm`, calling hiker's vault primitives as tools. Just enough to make chat-over-vault work without requiring an external agent install. [llm-basic-agent-loop]
- **ACP client is an optional escape hatch.** Users who want a more capable agent (Claude Code, Codex, Goose, Gemini CLI) can configure one; the chat panel routes through it instead of the basic agent loop. ACP is *only* for interactive features — never used for background or fan-out. [llm-acp-client-optional]
- **The whole agent layer is disable-able.** `[llm] enabled = false` (or equivalent) turns off background features, fan-out features, and the chat panel. Hiker becomes a pure local notes app; the MCP server stays available for querying the vault from external tools. [llm-features-disable-entirely]
- **Prompts are files.** Two-tier user/vault scope, mustache placeholders, settings UI Prompts tab when settings UI lands. Same for every feature, regardless of which module fires the prompt. [llm-prompts-file-store]


## Architecture

```
                   hiker UI
                          ↓
       ┌──────────────────┼──────────────────┐
       │                  │                  │
   chat panel         save / fanout      (none, when
   (interactive)      triggers           features off)
       ↓                  ↓
       │        ┌─────────┴─────────┐
       │        │                   │
       ▼        ▼                   ▼
  ┌─────────┐  ┌──────────┐  ┌─────────────┐
  │core::   │  │core::llm │  │ core::llm   │
  │agent    │  │(direct,  │  │ (direct,    │
  │  OR     │  │ single-  │  │ batch fan-  │
  │core::acp│  │ shot)    │  │ out)        │
  │(optional)│ │          │  │             │
  └────┬────┘  └─────┬────┘  └──────┬──────┘
       │             │              │
       │             └──────┬───────┘
       │                    │
       │ calls vault tools  │ calls llm crate
       ▼                    ▼
   core::mcp            provider API
   (vault tools)
```

Three call sites; all eventually flow through `core::llm` for actual provider access (the basic agent loop and the direct path share the same provider layer). The optional ACP path is the only one that doesn't go through `core::llm` — it talks to an external agent process that does its own provider access.


## Feature types

Three categories. Routing per type:

### Interactive

User explicitly clicks something in the chat panel or an agent affordance, response shown to the user before being applied. Examples: chat-over-vault, vision OCR review, "ask the agent to propose a name for this cluster," bulk-reorg conversation walks.

Default backend: `core::agent` (basic in-hiker agent loop using `core::llm`). User can switch the chat panel to an external ACP agent via `core::acp` for more capability. Either way, single conversation per user-initiated session. [llm-feature-type-interactive]

### Background

Triggered by routine actions (save, ingest), apply terminal results without per-call review (the user opted into the feature). Default off; opt-in per feature. Examples: auto-tag-on-save, summary-on-save, cluster summarization on cluster build.

Backend: `core::llm` direct, single-shot. Debounced 1–2s so save bursts coalesce to one prompt. Never routed through ACP. [llm-feature-type-background, llm-feature-debounce]

### Fan-out

User-initiated batch operations that span many items. Examples: RAPTOR-shaped clustering tree build (cluster naming + summarization across N clusters), regenerate-all-summaries, tag-all-unenriched-notes.

Backend: `core::llm` direct, batch. Scope determined by hiker's pre-batch logic (e.g., "the N clusters in the current tree"); the LLM doesn't decide its own scope. Visible progress (count, ETA, cancel button); user kicks off, watches it run. Never routed through ACP. [llm-feature-type-fanout]


## `core::llm`

A new module. Single trait (`LlmClient`) for testability, a provider-config-driven implementation backed by graniet/`llm`. Exposes:

- `chat(messages, opts)` — single-shot completion, used by background and fan-out features and by the basic agent loop's per-turn calls.
- `chat_stream(messages, opts)` — streaming variant, used by the basic agent loop's interactive surface so the chat panel can render tokens as they arrive.

Configured as an `[llm]` section in the standard hiker config (per `settings.md`): per-vault `vault/.hiker/config.toml` with user-scope fallback at the platform config dir, deep-merged like every other section. No separate `llm.toml` — it would duplicate the unified loader's validation / defaults / schema-versioning / write-back machinery for no benefit.

```toml
# vault/.hiker/config.toml
[llm]
enabled = true                  # see llm-features-disable-entirely

[llm.provider]
backend = "anthropic"           # or "openai", "ollama", "google", "openrouter", ...
model = "claude-sonnet-4-7"
api_key_env = "ANTHROPIC_API_KEY"
base_url = ""                   # optional override (Ollama / OpenAI-compat)

[llm.limits]
max_tokens = 4096
timeout_secs = 60

[llm.agent]
iteration_cap = 10              # see agent-iteration-cap-prompt
tool_timeout_secs = 30          # see agent-tool-call-timeout

[llm.audit]
log_full_prompt = false         # see llm-audit-log; obs-no-content discipline
```

[llm-providers-config]

API keys come from one of two sources, in this precedence:

1. **`[llm.provider].api_key` (user-scope TOML only).** When non-empty, used directly. The eligibility list in `core::config` refuses writes to this key from the vault TOML, so a vault that travels via Syncthing/git can't carry the secret. Plain text on disk in the platform config dir.
2. **`api_key_env` (default).** Names the environment variable holding the key, read at provider construction time. Both user and vault TOML can set the env var *name* (per-vault override of which env to read is meaningful; per-vault override of the literal key is rejected).

The vault TOML can carry `api_key_env`, `backend`, `model`, `base_url`, and the rest — only the literal `api_key` is restricted. The settings pane hides the `api_key` row when the section's scope toggle is at `[Vault]`. Empty `api_key` + empty `api_key_env` = no key set on the builder, correct for local Ollama and similar key-less backends. Keychain-backed storage is the forward-compatible future direction (a third precedence tier above the literal).


## `core::agent` (the basic agent loop)

The default backend for interactive features. Just enough loop to make chat-over-vault work without requiring users to install an external ACP agent.

Shape:

- Takes a user message + accumulated history.
- Calls `core::llm::chat_stream` with a system prompt that describes the available vault tools.
- Parses tool-call requests from the response (using the `llm` crate's tool-calling support).
- **Dispatches tool calls through hiker's in-process MCP server** (`core::mcp`), not through direct `core::*` calls. The MCP tool registry is already the source of truth for the agent-facing tool surface (`mcp-tool-search-notes`, `mcp-tool-get-note`, `mcp-tool-write-note`, etc.), and the external ACP path goes through it too. Routing the basic agent loop through the same surface means one tool registry, one set of audit-log shapes, one place to add a new agent-callable verb. The in-process call uses the rmcp client against the local server (no HTTP for the hiker-internal case — direct trait dispatch). [agent-tool-routing-via-mcp]
- Loops until the model produces a terminal user-facing response (no further tool calls), with the iteration cap and per-tool-call timeout described in the next section as circuit-breakers.
- Returns a streaming response to the chat panel.

Scope is *just* "let the model search and read the vault, respond" — not full multi-step planning / sub-agent / code-execution agents; ACP is the upgrade path. Tool dispatch surface lives in this module; tool implementations are thin wrappers over existing `core::*` API. [llm-basic-agent-loop]


## Sessions

A **session** is the unit of conversational continuity: a single accumulating message history that survives across many user turns. A **turn** is one user message → one terminal model response (possibly with tool roundtrips and a cap-pause / continue inside it). One session contains many turns; the session owns the history, the turn is just the cursor moving through it.

- **The registry is session-keyed; history accumulates across user turns.** `core::agent`'s in-memory state lives under a `SessionId`, not a turn id. Each `chat_send` appends the user message to the existing session history before invoking `run_turn`; nothing GCs the session on `EndTurn`. A turn id is still emitted per call (and carried on every `AgentEvent`) so the frontend can correlate streaming events with the in-flight turn, but the registry doesn't key on it. The registry's shape is `HashMap<SessionId, SessionState>`; `chat_send` / `chat_continue` / `chat_stop` / `chat_cancel` take `(session_id, turn_id)`. [chat-session-persisted-history]
- **New sessions are an explicit user action.** A "New session" affordance in the chat panel (button next to the input, plus a keybind reserved in `keybind-registry`) ends the active session and starts a fresh one. No automatic session reset on idle, focus change, or vault re-open within the same app launch — sessions only end when the user says so or when the app process exits. The first `chat_send` of an app launch lazily creates a session if none is active, so the user doesn't have to click "New" before their first message. [chat-session-new-button]
- **Sessions persist as markdown notes in the vault.** Each session lives at `vault/<chats_dir>/<YYYY-MM-DD>-<session-id>.md` — a visible folder (default `chats/`, configurable via `[chat] chats_dir`), not hidden under `.hiker/`, since a session carries a user/agent-authored body and is a first-class note like any other (`subsystem-notes-visible` in `design.md`). The file is appended to on every `TurnFinished` so a crash mid-session loses at most the in-flight turn. Format: YAML frontmatter (session id, created-at, model, provider, turn count) followed by alternating `## User` / `## Agent` sections in turn order; tool calls render as fenced `hiker-tool-call` blocks under the agent section that issued them, with tool *results* in paired fenced `hiker-tool-result` blocks. The markdown is the source of truth; the in-memory `SessionState` is a working cache rebuilt from it. **Tool-call structure round-trips on resume:** `parse_session` reads both block kinds back into the rehydrated `SessionState.history` so a resumed session shows the agent its prior tool use (otherwise the agent, seeing a tool-call-stripped history, may emit a plain message claiming a write it never made). Native sessions preserve the full structure; the text-only "lossy resume" convention is the fallback for imported sessions whose source format doesn't carry tool calls. [chat-session-markdown-store]
- **Visible in the tree and Vault view.** The `chats/` folder is an ordinary tree folder — sessions appear at their real path, collapsed under the folder by default, and are searchable/related like any note. Vault mode groups them cleanly by provenance/source-type (`vault-view-source-groups`) with a label derived from the session's title + date rather than the on-disk `<date>-<id>.md` filename. No hide/reveal toggle — sessions are notes, not a hidden category. [chat-session-tree-visibility]
- **What the user is currently viewing is auto-injected as a turn-scoped reference.** When a content tab is focused at the moment `chat_send` fires, hiker prepends a one-line context *reference* (the path, not the content) to the outgoing user message — `[active note: <vault-rel-path>]` for an editable note, or `[active board: <vault-rel-path>]` for a board (per `kanban.md`). The agent reads the actual content on demand with `get_note` / `board_get`. Scoped to the *single turn* it rides on; the next turn re-evaluates and re-injects (or skips). Read-only previews (trash / snapshot / staging) and non-content app-page tabs are *not* injected. Skipped when the draft already carries an explicit `@`-mention. For ACP the same reference rides as an Embedded Resource ContentBlock on `session/prompt` when `core::acp` lands. [chat-active-note-context-injection]
- **Resume on app open.** On vault open, hiker scans the `chats/` folder and offers the most-recent session as the active one (so the user re-opens hiker and keeps talking from where they left off). The "New session" button is the way to *not* resume. Sessions older than the most recent stay on disk as searchable history but aren't auto-loaded — opening one is a future affordance (deferred below). [chat-session-resume-latest]
- **Trash routes through the regular vault trash.** Sessions are ordinary notes in a visible folder, so the indexer + watcher route them with no special carve-out; deleting one moves the file to `<vault>/.hiker/trash/` via `core::ops::delete`, appends the regular `'deleted'` history frame, and clears the in-memory `SessionState` from the registry. Restore + permanent-delete ride the existing trash-bin commands (`restore_trash_entry` / `permanent_delete_trash_entry`); a session that's been restored shows back up in the session-picker dropdown the next time it's opened. No separate "session trash" surface — the regular bin is the single source of truth so users don't have to learn two delete affordances. [chat-session-trash]

Out of this section's scope:

- **Multi-session UI** (a sidebar list of past sessions, click to re-open, branch off, delete). Deferred — v1 ships with implicit single-active-session UX (resume latest or start new). [chat-session-list-ui]
- **Cross-session memory / summarization.** The model only sees the current session's history. Distilling past sessions into long-term memory is a separate problem and not on the v1 roadmap.
- **Token-budget trimming inside a session.** When a session's history exceeds the provider's context window, the right answer is summarize-and-roll-forward; v1 simply lets the provider error and surfaces it as a turn error. The fix is its own slug (`chat-session-history-compaction`, deferred) once it bites.


### Imported sessions from other agents

Hiker has a sidecar that ingests conversation exports from other agents and converts them to the hiker session format so the user can read them — and optionally continue them — inside hiker. Future feature, but the shape is pinned now so the storage layout, search behavior, and respawn path are consistent with the native-sessions decisions above.

- **Imports land at `vault/<chats_dir>/imported/<source>-<YYYY-MM-DD>-<id>.md`.** Same on-disk format as native sessions per `chat-session-markdown-store` — YAML frontmatter (id, source, source_id, created_at, model, provider, turn_count) plus alternating `## User` / `## Agent` sections; tool calls render as fenced `hiker-tool-call` JSON blocks under the agent section. The visible `imported/` sub-directory of the `chats/` folder carves the namespace cleanly from native sessions; like native sessions, imports are ordinary indexed notes (no `.hiker/` carve-out). [chat-session-import-storage-path]
- **Source-specific converters map foreign formats to the hiker shape.** A small `core::sessions::import::<source>` module per supported export format. v1 candidates: Claude Code transcript JSON, Codex session export, ChatGPT data-export JSON, Goose session log. Each converter is a one-file translator with a clearly named entry point; new converters land alongside their first user-driven import. Tool-call structure is preserved when the foreign format carries it in a way that maps onto hiker's `hiker-tool-call` block; otherwise it collapses to plain text under the agent section (the same lossy convention `parse_session` already accepts on resume). [chat-session-import-converters]
- **Sidecar entry point: `hiker session import <path>` (CLI) and a settings-UI affordance.** Both call `core::sessions::import::detect_and_convert(path)` which sniffs the file shape, picks a converter, writes the resulting markdown into `imported/`, and enqueues an indexer upsert. Bulk import (a directory of exports) is just the same call applied per file. Idempotency: re-importing the same source file (same `source` + `source_id`) updates the existing imported note rather than creating a duplicate. [chat-session-import-sidecar]
- **Imported sessions are ordinary visible notes.** Same shape as native sessions: indexed, search-reachable, and shown in the tree at `chats/imported/`. Vault mode groups them as a distinct "Imported sessions" provenance bucket (`vault-view-source-groups`) so the user can read native vs. imported separately without a hide toggle. [chat-session-imported-visibility]
- **"Continue as chat" respawns an imported session as a native one.** When the user opens an imported session in hiker, the chat panel surface offers a "Continue as chat" affordance. Click creates a new native session at `vault/<chats_dir>/<YYYY-MM-DD>-<new-id>.md` whose history is seeded from the imported session's parsed turns (text-only by default — same lossy resume convention as `parse_session`); the basic chat agent picks up from the seeded history using the current `[llm]` provider. The respawned session is independent of the original — appending to it doesn't touch the imported file, and the original stays in `imported/` as a frozen artifact. The new session's frontmatter records `respawned_from: imported/<source>-<date>-<id>.md` so the lineage is recoverable. ACP's chat path can also respawn an imported session — the seed is just an Embedded Resource ContentBlock on the first `session/prompt`. [chat-session-respawn-from-import]

Out of this subsection's scope:

- **Round-tripping back out to the original agent's format.** Hiker imports as a one-way conversion; exporting a hiker session in Claude Code / ChatGPT format is not in v1.
- **Live import from external agents' running state.** v1 reads exported files; hooking into Claude Code's live session directory is reserved as `chat-session-import-live-sync`, deferred.
- **Multi-agent merging.** Combining several imported sessions into one hiker session is not in v1; each import lands as its own file.


## Chat panel UI

The chat panel is the user-facing surface for interactive features. Whether the backend is `core::agent` (the basic in-hiker loop) or `core::acp` (an external agent), the panel shape is the same — only the bytes flowing through it change.

- **The chat panel is the bottom region of the discovery panel** — same right-hand column that already hosts search results and related notes (`search-discovery-panel`). The panel's vertical layout is: discovery sections (search results / related / future surfaces) take the top, the chat surface is **pinned at the bottom and expands upward**. Same column, same width, same toggle button (`panel-toggle-buttons`) flips the whole panel open / closed. [chat-panel-pinned-bottom]
- **Chat scrolls independently from the sections above it.** The discovery panel becomes a two-region layout: a top region holding the search/related/future sections (which scroll as a unit, same shape as today), and a bottom region holding the chat (which scrolls on its own). Scrolling a long agent transcript doesn't move the search results out of view, and scrolling search results doesn't unanchor the chat input. [chat-panel-detached-scroll]
- **The chat region is vertically resizable** via a drag handle on its top edge — the boundary between the discovery sections region and the chat region. Standard UX: hovering the boundary swaps the cursor to `row-resize`; dragging up grows the chat region (shrinking the sections region) and vice versa. Same affordance shape as `side-panel-resize`, rotated 90°. [chat-panel-vertical-resize]

- **Top edge renders a drop shadow** so the chat region reads as floating above the discovery sections region rather than meeting it at a sharp border. CSS-only — a small downward-cast `box-shadow` on the chat region's top edge (or upward-cast on the resize handle) — matches the visual depth treatment elsewhere in the app. Same shadow applies when the chat surface is expanded into an agent-tab body (`chat-panel-expand-to-editor`). No behavior change; purely a visual fix for the messy sharp-edge border in the current layout. [chat-panel-agent-tab-edge-shadow]

- **Drag-to-uncollapse from the collapsed state.** When the chat region is collapsed (height ≈ 0 / header-only), starting a drag upward on the collapsed header / handle uncollapses the chat region and continues the same gesture as a resize — pointer-down captures, the region expands to a small floor (or to the user's last-used height) on first movement past a small threshold, and subsequent pointer movement drives `chat-panel-vertical-resize` as if the user had grabbed the handle of an already-open region. Single gesture, no click-to-uncollapse-then-grab-the-handle round trip. Pointer-up persists the new height via the existing `chat-panel-default-height` write-back. [chat-panel-drag-uncollapse]

Layout sketch (extends the `search.md` discovery-panel diagram):

```
┌─ Discovery ─────────────────────┐
│  [search input]      [S] [L]    │  ← input + mode toggles
│                                 │
│  ▼ Search results (8)           │  ← scrolls independently
│    ...                          │
│  ▼ Related notes (5)            │     of the chat region below
│    ...                          │
├─────────────── ↕ ───────────────┤  ← drag handle (chat-panel-vertical-resize)
│  agent: here's what I found...  │  ← chat transcript
│  user: also check the inbox     │     scrolls independently
│  agent: ...                     │
│                                 │
│  [chat input]              [↑]  │  ← input pinned at the very bottom
└─────────────────────────────────┘
```

Behavior details:

- **Expand to center pane.** A button in the chat region's header opens the active chat session as an `agent`-kind tab in the top tab strip (per `editor.md`'s `tab-kinds`), joining the buffer tabs as a peer. The bottom-docked chat region collapses while at least one agent tab is open; closing the last agent tab restores it at the prior `chat-panel-default-height` fraction. The discovery panel's other sections stay visible in the right column at all times. Multiple agent tabs (one per session) ride the standard tab-strip / autosave-tab-state machinery and persist across launches like buffer tabs. Composes with split view (`design.md`'s deferred split-view bullet) — shift-clicking an agent tab puts chat side-by-side with a buffer. Reserves keybind `chat.toggle-expand`: with no agent tab open, opens one for the active session; with an agent tab active, closes it. [chat-panel-expand-to-editor]
- **Default split.** First open of a vault gives the chat region a ~30%-of-panel default height so the input is visible without dragging. Persisted per-vault via `settings-write-back` to `vault.chat_height`. [chat-panel-default-height]
- **Creating a new chat session reveals the chat panel.** The "New chat session" action (toolbar / palette / keybind) creates the session and forces the chat panel visible, un-collapsing it if hidden. Mirrors the create-then-reveal posture of every other creation verb. [chat-new-session-reveals-panel]
- **Min height.** The chat region has a minimum height that fits the input row plus one or two transcript lines — dragging below snaps to the minimum. The chat surface has no own collapse toggle; the discovery panel toggle (`panel-toggle-buttons`) is the only way to hide it, and it hides the whole right column.
- **Disable mode interaction.** When LLM features are disabled (`llm-features-disable-entirely`), the chat region is removed entirely (not minimized) and the divider handle disappears. Re-enabling restores the persisted split.
- **Empty / pre-conversation state.** Before the first turn, the transcript shows a placeholder ("Ask about your vault, or pick a suggestion below…") plus optional starter chips; gone for the rest of the session once a turn lands. The transcript autoscrolls to the latest message each turn unless the user has scrolled up.
- **Keybind.** Reserves `chat.focusInput` in `keybind-registry` for focusing the chat input from anywhere (chord TBD; lands when the keybind is wired). Esc in the chat input blurs back to the editor — symmetric with the existing search-input Esc behavior.

Deeper transcript chrome (tool-call confirmations, embedded-resource context cards, attached-context affordances) is still out of scope for this section — those shapes land with each backend's interactive surface (`llm-basic-agent-loop`, `llm-acp-client-optional`). The in-flight UX below covers the ambient affordances every interactive turn needs regardless of backend.

### In-flight UX

Affordances pinned here so both backends render the same thing.

- **Stop button while a turn is in flight.** The send affordance flips to a Stop button as soon as `chat_send` (or `chat_continue`) returns — visible for the whole busy period, not just at the cap pause. Click invokes `chat_stop(turn_id)`; the panel falls back to idle send on the next `TurnFinished`. This is Stop (preserves streamed transcript, finish reason `user_halted`), not the harsher Cancel (an internal-only mid-stream abort, e.g. closing the panel mid-turn). [chat-panel-stop-button]
- **Thinking indicator between user turn and first agent event.** From `chat_send` until the first `TextDelta` / `ToolCallStart` for the same `turn_id`, the panel shows a "Thinking…" spinner in the would-be assistant bubble. Tied to the absence of streamed content, not a timer: it disappears the instant any content arrives and reappears between steps if the model goes quiet again before the next `TextDelta`. Cap-hit rows and tool-call cards count as content and displace it. [chat-panel-thinking-indicator]
- **Agent messages render as markdown.** Assistant text bubbles render as markdown (headings, lists, inline code, fenced code blocks, links, emphasis); user messages stay plain text. Streaming deltas append into a markdown-rendered buffer; the bubble re-renders on each delta against the accumulated text. Sanitize: no raw HTML pass-through, no `javascript:` links, no embedded scripts — the renderer's safe mode is the contract. Code blocks get a language hint when fenced; syntax highlighting is deferred. Same shape for ACP-emitted text. [chat-panel-markdown-render]
- **Agent-emitted note links are clickable and open the note in the editor.** A link whose target resolves to a vault-relative note path — a bare relative path (`subdir/foo.md`, resolved against the vault root) or a `hiker://note/<rel-path>` URL (unambiguous) — renders as an in-app link that opens the note in the editor pane rather than a browser. Non-vault links (http(s), file://) keep browser-open behavior. The system prompt advertises the `hiker://note/<rel-path>` form as canonical so output gravitates toward unambiguous links. Hover shows the resolved absolute path; click respects `file-switch-guard-dirty`. [chat-panel-note-link-render]
- **Tool calls collapsed by default; click to expand.** Cards render minified: `▸ <tool_name>(<short args summary>)` plus a status glyph (spinner / ✓ / ✗) and a one-line result summary. Clicking the row toggles full pretty-printed JSON of args and result. The "short args summary" is a deterministic one-liner — first 1–2 args fields, truncated at ~80 chars — generated frontend-side from the assembled args. Expanded view is `<pre>`-formatted JSON with a copy button. State is per-card, not persisted; every card starts collapsed, errored calls (`ok = false`) included. [chat-panel-tool-call-collapsible]
- **Tool-call cards that touched a note jump to the note on header-click.** When a tool call resolves to a note path (`get_note`, `write_note`, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag`, future `modify_note`), the header row opens the touched note in the editor; the expand-chevron stays separately clickable (header opens, chevron expands). Multiple touched notes (batch ops, search results) → header opens a picker listing each path. Resolution rule: parse the rel-path from the result (`{path, ...}` is the conventional shape); fall back to the args. Honors `file-switch-guard-dirty`. **Staged writes route to the staging preview:** when the result carries `status: "staged"` (per `mcp-write-tools-staging-aware`), header-click opens the staging preview rather than `openFile` against a not-yet-on-disk path. `edit_note` produces N proposals sharing a `batch_id`; header-click opens patch-review mode for the target path with all N hunks (per `note-open-routes-to-pending-review`). [chat-tool-call-opens-touched-note]

### Retry from a prior user message

The transcript exposes a Retry affordance on every user message in the current session. Clicking it rolls the conversation back to the state it was in just before that user message was sent, then re-submits the message as a fresh turn. The use case is the obvious one: the agent went off the rails, or the user wants to try the same prompt with a tweak after seeing how it landed the first time.

- **Retry button on each user message bubble.** A small icon-button (circular-arrow glyph) in the user message's right-side affordance cluster, visible on hover or always-on per the panel's existing message-action styling. Click rolls the session back to *immediately before* that user message and starts a new turn from the same text. Tooltip "Retry from here." Disabled while another turn is in flight for the session — retry is mutually exclusive with an active turn. [chat-retry-from-user-message]
- **Truncates the in-memory session and the on-disk markdown.** Retry is a destructive history operation, not a branch: every turn at or after the retried user message is removed from the session's `SessionState` and the markdown file is rewritten to match (per `chat-session-markdown-store`'s "markdown is the source of truth" rule). A single history frame is appended to the session note covering the truncation. No "previous attempts" archive in v1 — the user opted to throw the prior outcome away; keeping a hidden branch tree complicates the resume-on-open flow for marginal gain. Branching is reserved as a future affordance (`chat-retry-branches-session`, deferred). [chat-retry-truncates-session-history]
- **Resends the original user message verbatim.** The retried turn carries the exact text of the original user message, including any `@`-mentions; context blocks are re-resolved at submit time the same way a fresh `chat_send` would resolve them (the active note re-injects per `chat-active-note-context-injection`; `@<note-path>` mentions re-read the current file contents; `@selection` errors if the selection no longer exists). The user message is *not* prefilled into the input box for editing — that's a separate affordance ("Edit and resend," deferred as `chat-edit-and-resend`); Retry is the one-click "same prompt, redo it" path. [chat-retry-resubmits-verbatim]
- **Command + event surface.** New `chat_retry(session_id, user_message_id) -> Result<TurnId>` on the command surface (per `agent-chat-command-surface`). Implementation: truncate session state + markdown file → emit a synthetic `TurnStarted` for the new turn id → invoke the same turn-driver as `chat_send`. The frontend's existing event handling for the new `turn_id` covers the rest (streaming, tool-call cards, stop button, cap-hit row). Confirmation modal not required — the retry-discards-future-turns invariant is obvious from the UI's roll-back animation (the disappearing tail). [chat-retry-command]
- **Available on both backends.** The basic agent loop owns the truncate-and-rerun path directly. The ACP path (`core::acp`) maps retry onto its protocol equivalent: end the current `session/prompt` line and start a new one with the truncated history; if the configured ACP agent doesn't model history truncation natively, hiker falls back to spawning a fresh ACP session seeded with the pre-retry turns. [chat-retry-acp]

Out of scope for the v1 of this feature:

- **Editing the message before retrying.** Future `chat-edit-and-resend` affordance — Retry is one-click "same prompt"; an Edit button (pencil glyph) on the same bubble drops the message text back into the input for editing and would replace Retry's destructive trim with the same trim plus a manual resend.
- **Branching sessions on retry.** Future `chat-retry-branches-session` — keep the discarded tail as a named branch of the session so the user can compare outcomes. Deferred; current rule is destructive trim.
- **Retry on assistant messages.** The retry semantics are user-message-anchored (roll back to *before* the message that asked for this output, then ask again). A "regenerate this response" button anchored on the assistant message is a related but distinct affordance — the prior user message is unchanged and the assistant output is what re-rolls. Both reduce to the same `chat_retry` plumbing internally; the assistant-side button is deferred as `chat-regenerate-assistant-message`.

### `@`-mentions for explicit context injection

The chat input parses `@`-prefixed tokens into structured context references that get injected as turn-scoped context blocks (same injection mechanism as `chat-active-note-context-injection` — synthetic user-context message in the basic agent loop, Embedded Resource ContentBlock in ACP). Distinct from the auto-injection of the active note: those auto-inject the *implicit* context ("what the user is looking at"), `@`-mentions are *explicit* user-attached context. Both can ride the same turn — they're additive, with de-duplication so the same note isn't sent twice. [chat-input-at-mentions]

Tokens recognized in v1:

- **`@selection`** — pastes the live highlighted text from the active editor at the moment `chat_send` fires. The injected block carries the source rel-path, the line range of the selection, and the selected text. If no editor is open or no text is selected, the token errors at submit time with a tooltip "no selection in editor"; the user can clear the token and resubmit. The selection is captured at submit, not at autocomplete time, so editing the selection between typing `@selection` and clicking Send does the obvious thing. [chat-input-at-selection]
- **`@<note-path>`** — pastes the named note as context. The token's value is the vault-relative path with the extension stripped (`@research/embeddings/whisper-notes` for `research/embeddings/whisper-notes.md`). At submit time hiker resolves the path against the vault, reads the file, and injects the full body as a context block carrying the rel-path. If the path no longer resolves (note moved/deleted between autocomplete and send), submit errors with "note not found: <path>" and the user can fix or remove the token. [chat-input-at-note]

Both kinds inject as separate context blocks, ordered as they appear in the user message. The user's literal `@token` text stays in the user message verbatim — the model sees both the natural-language reference ("what's the gist of `@research/embeddings/whisper-notes`?") and the resolved context block, so it can correlate the two. The auto-injected active note (per `chat-active-note-context-injection`) lands first; explicit `@`-mentions follow.

**De-duplication.** Across all context blocks for a single turn (auto-injected active note + every `@`-mention), each unique rel-path is sent exactly once. A second reference to the same path becomes a one-line "see prior block for `<path>`" reference rather than re-sending the full body. `@selection` is never de-duplicated against `@<note-path>` — even if the selection is from the same file, the slice is what matters. [chat-input-at-mentions-dedup]

### `@` autocomplete

Triggered when the user types `@` after whitespace (or at the start of the input). Suggests `selection` plus a list of vault notes; arrow keys navigate, Enter accepts, Escape dismisses. [chat-input-at-autocomplete]

- **The `selection` entry** appears at the top of the list when there's a non-empty selection in the active editor; greyed (unselectable) when there isn't. Tooltip on the greyed entry: "Select text in the editor first."
- **Note entries** are vault notes ranked by basename fuzzy-match against the typed prefix, with recency as a tiebreaker (most-recently-accessed first via `last_accessed_at`). Empty prefix (just `@` typed) shows the top 10 recents. Each row shows `<basename>` with the parent folder muted to its right (`whisper-notes  research/embeddings/`). Picking a row inserts `@<rel-path-without-extension>` and dismisses the popover.
- **Disambiguation.** When the typed prefix matches multiple notes' basenames, the popover shows all of them with their distinguishing folder paths so the user picks unambiguously. Selecting one inserts the full rel-path token, never just the basename, so the resolution at submit is deterministic regardless of future renames creating collisions.
- **Inside fenced code blocks or escaped with `\@`** — the trigger is suppressed. `\@selection` in the input stays literal in the user message and isn't parsed as a mention.
- **Backspace into the token** treats the `@<path>` as a single unit — one backspace deletes the whole token. Cursor-arrow into the token treats it as text. Same shape as how Slack / Discord handle mention chips, just rendered as plain text rather than as a chip in v1 (chip rendering deferred to keep the input shape simple).

Autocomplete data source: a command `chat_at_autocomplete(prefix, limit) -> Vec<AtSuggestion>` over the index store. Cheap — a single LIKE / fuzzy query against the notes table, ordered by `last_accessed_at DESC`. Returns at most 10 results. The `selection` synthetic entry is added frontend-side based on the live editor state; never round-trips through the command. [chat-input-at-autocomplete-cmd]

### Future `@`-mention targets (deferred)

Reserved as concept slugs so future spec writers know the surface. Not in v1.

- **`@<folder>/`** — every note under the folder, recursively. Bounded by a context-budget guard so accidentally `@vault/` doesn't blow the model's window. [chat-input-at-folder]
- **`@<tag>`** — every note carrying the named frontmatter tag. Lands once tags are real (per `design.md`'s enrichment pipeline). [chat-input-at-tag]
- **`@search:<query>`** — the current top-N search results for an inline query. Lets the user ask "summarize what we know about X" without precomputing the search. [chat-input-at-search]
- **Mention-chip rendering** in the input box — visual chip with the resolved basename + a click-to-edit affordance, instead of the plain `@token` text. Cosmetic upgrade once the underlying parsing has shipped. [chat-input-at-mention-chips]


## Event streams and command surface

The agent loop streams its progress to the chat panel through a typed event channel; the panel calls back into the loop for user-driven actions (continue past a cap, stop, cancel mid-stream). Same shape applies whether the backend is `core::agent` or `core::acp` — the UI only sees the event enum.

### AgentEvent

A discriminated-union enum. Every event carries `turn_id` (one per user message); most carry `step_id` (one per LLM call within a turn — increments on each tool-loop iteration). Variants: `TurnStarted`, `StepStarted`, `TextDelta`, `ToolCallStart`, `ToolCallArgsDelta`, `ToolCallComplete`, `ToolResult { ok, summary }`, `StepFinished { finish_reason }`, `IterationCapHit { completed_iterations }`, `TurnFinished { usage, cost_estimate }`, `Error { message }`. [agent-event-stream-shape]

Translation happens at the `core::agent` boundary: provider-specific chunks from the `llm` crate's `chat_stream` are normalized into this enum so the chat panel never sees Anthropic-vs-OpenAI-vs-Ollama shape differences. The ACP path (`core::acp`) emits the same enum so the panel renders both backends identically.

A **single global event channel** (not a per-invoke `Channel<T>`): continue / stop / cancel commands address an existing in-flight turn from a separate call, which a per-invoke channel doesn't model cleanly. Frontend filters by `turn_id`.

### Command surface

All take `(session_id, turn_id)` except where noted:

- `chat_send(.., message)` — start a turn within a session; events stream back.
- `chat_continue` — resume a loop paused at `IterationCapHit`.
- `chat_stop` — user halt; emits `TurnFinished`, session retained.
- `chat_cancel` — mid-stream abort; cancels the in-flight LLM call too.
- `chat_session_new() -> SessionId` — end active session, start a new one.
- `chat_session_active() -> Option<SessionId>` — resume-latest at vault open.

Backend keeps an `Arc<Mutex<HashMap<SessionId, SessionState>>>` so commands can address the active session and the turn cursor inside it. Each turn owns its tokio task; cancel drops the task handle. Stop preserves whatever has been streamed; cancel is the harsher abort. The session entry is **never** GC'd on terminal `TurnFinished` — only on explicit `chat_session_new` (or process exit, after the markdown file has been flushed). [agent-chat-command-surface]

### Iteration cap + Continue/Stop prompt

The loop has a per-turn cap on LLM calls. Default **10** (i.e. up to 9 tool roundtrips before the model has to produce a terminal answer). Configurable per-vault under `[llm.agent] iteration_cap` in `vault/.hiker/config.toml` (per `llm-providers-config`).

On hit:

1. The loop suspends; in-memory turn state retained.
2. `IterationCapHit { turn_id, completed_iterations }` fires.
3. The chat panel renders a system-style row in the transcript: "Agent has made N tool calls — [Continue] [Stop]."
4. **Continue** calls `chat_continue(turn_id)`, the loop resumes with the cap **reset to its full budget** (so 10 more), not "+1."
5. **Stop** calls `chat_stop(turn_id)`, which emits `TurnFinished` with `finish_reason = "user_halted"` and drops the turn state.

The cap is a circuit-breaker against runaway tool-call loops, not a hard semantic limit. The prompt makes the pause visible rather than letting "thinking…" spin forever or auto-killing a turn that was about to land its terminal answer. [agent-iteration-cap-prompt]

`IterationCapHit` is a **suspend**, not a terminal event: the loop emits it without a paired `TurnFinished`, the in-memory turn state is retained, and the next event for that `turn_id` is whatever Continue/Stop produces (`StepStarted` on resume, or `TurnFinished { user_halted }` on Stop). The chat panel's busy/active-turn machine should treat the cap-hit row as "still the same turn, just paused" rather than rolling over to a new turn id.

### Per-tool-call timeout

Each MCP tool call gets a default **30s** timeout (configurable under `[llm.agent] tool_timeout_secs`). On timeout, the loop emits a synthesized `ToolResult { ok: false, summary: "tool timed out" }` back into the model's context so it can decide to retry, try a different tool, or give up. The loop does not bubble timeouts as turn-killing errors — the agent is allowed to recover.

A timed-out tool task is dropped (its tokio handle cancelled) so resources don't leak when the model moves on without it. Repeated timeouts on the same tool name within a turn are not specially handled in v1 — the iteration cap will catch any pathological "retry-the-stuck-tool-forever" loop. [agent-tool-call-timeout]

### Other event shapes (forward refs)

The agent path covers interactive features. Background and fan-out features have intentionally different UI shapes since their concerns aren't streaming text — the shapes are pinned here so they don't accidentally diverge.

- **Fan-out, background, and note-mutation features** route through `core::tasks` (per `task-queue.md`). The queue's `QueueEvent` channel + the home-page Task queue widget are the user-visible progress surface; per-feature toasts and bespoke event enums are out. Single-note mutations land in the active editor buffer as a single editor transaction (per `editor.md`'s Note-mutations menu); batch mutations and agent writes produce pending ops reviewed per `settings.md`'s Pending change review.

### Audit log integration

Every event-emitting surface (agent turns, fan-out items, single-shot mutations, background calls) writes one row to `llm-audit-log` per LLM call. The audit row's `surface` field discriminates (`core::llm | core::agent | core::acp`), and for agent turns the row carries `turn_id` + `step_id` so a debugging trail can correlate panel events with audit entries. Audit writes happen at the `core::llm` boundary, so all four call sites share one writer.


## `core::acp` (optional ACP client)

When a user configures an external ACP agent, the chat panel routes through `core::acp` instead of `core::agent`. Same UI surface, different backend.

Configuration:

```toml
[acp]
agent = "claude-code"           # or any ACP Registry id, or "none" to use core::agent
```

The ACP path uses the [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) Rust crate. Streaming, tool-use confirmations, multimodal input — all standard ACP shapes. The external agent uses hiker's MCP server (`core::mcp`) to read/write the vault.

ACP is **only** wired for interactive features. The chat panel is its only consumer. Background and fan-out features always go through `core::llm` directly; there is no setting that routes them through ACP, even with a warning. This keeps subscription-billed agents firmly in the interactive-use role they're priced for. [llm-acp-client-optional]

Context injection: when hiker has high-confidence relevant context for an interactive turn (e.g., "ask about *this* note"), the ACP client attaches it as an Embedded Resource ContentBlock in `session/prompt`. The same pre-injection pattern applies to the basic agent loop — the system prompt or tool-pre-call response carries seeded context. [llm-context-injection]


## Disable mode

`[llm] enabled = false` (or `[acp] agent = "none"` if only ACP needs disabling) is the master AI gate. When fully disabled, hiker hides every AI-touching surface so a user who doesn't want AI in their notes app doesn't see it. When on, every surface lights back up live (no relaunch). [llm-features-disable-entirely]

What disabling hides / disables:

- **Chat panel** — the entire docked chat region (`chat-panel-pinned-bottom`) is removed (not collapsed); the discovery sections take the full panel height. Any open `agent`-kind tab (`chat-panel-expand-to-editor`) closes.
- **Background features and fan-out features** — auto-tag, summary-on-save, cluster summarization, RAPTOR tree building, regenerate-all-summaries, etc. — no-op. Per-feature toggles render greyed with a "LLM disabled" tooltip so the user knows where the dependency lies.
- **Note-mutation menu entries** (`note-mutations-menu`) — every entry that submits an LLM task (Reformat as markdown, future entries) is hidden, not just disabled. The menu button itself stays available for any future non-AI mutations.
- **Command palette** (`command-palette`) — AI-touching actions (chat-new-session, retry, mutation invocations, ACP-only verbs) are filtered out at render time.
- **Right-click context menus** — entries that invoke an LLM are hidden, not greyed, so the menus stay short.
- **Task queue** — the LLM worker lane is suspended; LLM-kind tasks already in the queue stop being checked out. Non-LLM lanes (indexer, extractor, future workers) keep running. The queue tile / badge follow `task-queue-respects-llm-disable`.
- **MCP server stays available.** Hiker becomes a pure local notes app on the inside while still exposing the vault to external agents the user runs out-of-process and bills outside hiker. Disabling AI *inside* hiker means hiker shouldn't host it, not that the user is hostile to AI generally.
- **Deterministic pipelines stay live.** Indexing, embedding, search, clustering build (non-summarization parts), wikilink resolution, op-log, sync — none are AI in the sense the gate targets; they keep running.

Sticky across vaults: `[llm].enabled` defaults to user-scope. Per-vault override works via the section's scope toggle.


## Prompts as files

Every LLM-driven feature has its prompt stored as a markdown file. Editing the file changes the prompt. Settings UI Prompts tab (when settings UI lands) edits the same files.

Two-tier (mirrors `settings-user-config-toml` + `settings-vault-config-toml`):

- User scope: `~/.config/hiker/prompts/<feature>.md` — bundled defaults, user can override.
- Vault scope: `vault/.hiker/prompts/<feature>.md` — per-project overrides, wins over user.

Defaults written to user scope on first run if absent. [llm-prompts-file-store, llm-prompts-mustache-templating]

Mustache `{{var}}` substitution. Each shipped default starts with a comment block listing available placeholders for that feature (e.g. `{{title}}`, `{{content}}`, `{{vocabulary}}`, `{{existing_tags}}`).

**Upgrade-aware staleness.** Hiker stamps the bundled default's content hash next to each prompt; if the bundled default's hash changes upstream, the user's override isn't clobbered — staleness is flagged in the agent log + Prompts tab. User decides whether to merge. [llm-prompts-staleness-on-upgrade]

**Bundled defaults registered in `core::prompts::bundled_defaults()`.** Each feature that needs a prompt registers a default with a feature key, the default body, and the placeholders it accepts. Current registry:

| Feature key                              | Owner                                  | Placeholders |
| ---------------------------------------- | -------------------------------------- | ------------ |
| `chat_system`                            | basic agent loop (`llm-basic-agent-loop`) | `{{vault_name}}` |
| `note_mutation_reformat_as_markdown`     | `note-mutation-reformat-as-markdown`   | `{{title}}`, `{{content}}`, `{{source_extension}}` |

New features land their default in the same registry; the prompt file lives at `core/prompts/<feature_key>.md` in the source tree and gets baked into the binary as the bundled default. First-run materialization writes it to the user-scope path.

**Settings UI Prompts tab (deferred).** Per-feature row: editable text, read-only shipped default, "reset to default," "diff vs. shipped default," "test prompt with sample data." [llm-prompts-settings-tab, llm-prompt-test-button]


## Audit log

Every LLM call (any module, any feature type) appends to `vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`: timestamp, feature slug, surface (`core::llm`, `core::agent`, `core::acp`), triggering action, prompt hash + template version, response summary (tokens, finish reason), cost estimate when the provider reports one. Daily rotation. Full prompt/response text gated on `[llm.audit] log_full_prompt = true` (default off — `obs-no-content` discipline). [llm-audit-log]


## Operational rules

- **One user action ≤ one prompt** for interactive and background features. Fan-out is the explicit exception, with scope determined pre-batch by hiker (the LLM can't expand scope mid-run).
- **No recursion.** A response is applied directly; it doesn't trigger further automatic prompts. (Tool-call loops within the basic agent loop are bounded — a max-iterations cap prevents runaway turns.)
- **No silent retries.** Failed calls surface as errors; no auto-retry. Retries amplify quota usage and mask provider issues.
- **Cost transparency.** Status-bar indicator shows recent LLM activity ("3 prompts today") when any feature is enabled; click → audit log viewer. [llm-cost-transparency]
- **Prompts visible.** Audit log + Prompts tab both expose what gets sent. No hidden internal prompts.


## Forward refs

- `core::mcp` (MCP server): v3 milestone in `design.md` build order. The basic agent loop probably consumes MCP for tool dispatch (consistency with the ACP path); details land with that spec.
- `core::acp` (ACP client): a milestone after MCP. Future spec doc when implementation starts.
- `core::agent` (basic agent loop): same; future spec doc when implementation starts.
- Synthetic corpus generation for evals (`qa.md` `eval-synthetic-corpus`): runs as an external Python tool, *not* through any of the above. Eval generation is a one-off batch workload that doesn't earn its keep being implemented in Rust.
- Vocabulary file (`design.md` enrichment pipeline): consumed by the `auto-tag` prompt as `{{vocabulary}}`.


## Out of scope

- **Hosting an LLM model in-process.** Local-Ollama use is via the `llm` crate's Ollama backend (talks to a separately-running Ollama server); hiker doesn't bundle a model runtime.
- **Multi-step LLM "chains" outside the basic agent loop.** Combining steps into pipelines outside the agent surface crosses the one-action-one-prompt rule for non-interactive features. If a feature genuinely needs multi-step reasoning, it belongs on the interactive path.
- **Function calling from `core::llm` direct calls.** Tools are an interactive concern (basic agent loop or external ACP agent). Background and fan-out features fire single-shot completions with no tool surface.
- **Prompt safety / jailbreak filtering.** The provider's safety layer is the safety layer.
