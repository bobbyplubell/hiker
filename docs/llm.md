# LLM strategy

How hiker uses generative LLMs. Pins the modules, the routing rule per feature type, where prompts and config live, and the policy posture that keeps subscription-billed agents in the role they're priced for.

Embeddings are out of scope of this doc — `core::embed` is its own module with its own trait and version-tag machinery (see `index.md`'s embedder section). The local fastembed-rs default and the cloud/Ollama options (via the `llm` crate's `EmbeddingProvider` trait, sharing the same crate dep this doc uses for generative access) are documented there. The two consumers of the `llm` crate — `core::embed::LlmEmbedder` and `core::llm` — share a dep but have separate trait boundaries and policy postures (embeddings are always automation-shaped → pay-per-call APIs always; the interactive-vs-background distinction below applies only to generative use).

The headline decisions:

- **`core::llm` is the foundation.** A new module wrapping the [`llm`](https://crates.io/crates/llm) crate (graniet/llm) for multi-provider access — Anthropic, OpenAI, Ollama, Google, Groq, Mistral, DeepSeek, etc. Module discipline: `llm` crate confined to this module, mirroring rusqlite-only-in-store and fastembed-only-in-embed. [llm-core-module]
- **Background and fan-out features submit to `core::tasks`; the in-process direct-LLM worker drains via `core::llm`.** Single-shot prompts for auto-tag-on-save, summary-on-save, cluster summarization (background); pre-scoped batch fan-outs for RAPTOR-shaped tree building, cluster naming across N clusters, regenerate-all-summaries (fan-out). The queue layer (per `task-queue.md`) lets external rmcp clients — Claude Code, Codex, an ACP-driven Goose acting as an MCP client — drain the same queue if the user has them attached. Pay-per-call billing model on the direct-LLM lane — no ToS grey area. [llm-strategy-direct-non-interactive]
- **Interactive features use a basic in-hiker agent loop by default.** A new module (`core::agent`) implementing a simple message-history + tool-dispatch loop on top of `core::llm`. Calls hiker's vault primitives as tools. Just enough to make chat-over-vault and similar interactive features work without requiring an external agent install. [llm-basic-agent-loop]
- **ACP client is an optional escape hatch.** Users who want a more capable agent (Claude Code, Codex, Goose, Gemini CLI) can configure one; the chat panel routes through it instead of the basic agent loop. ACP is *only* for interactive features — never used for background or fan-out. [llm-acp-client-optional]
- **The whole agent layer is disable-able.** `[llm] enabled = false` (or equivalent) turns off background features, fan-out features, and the chat panel. Hiker becomes a pure local notes app; the MCP server stays available for users who want to query the vault from their own external tools. [llm-features-disable-entirely]
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

Configured as an `[llm]` section in the standard hiker config (per `settings.md`): per-vault `vault/.hiker/config.toml` with user-scope fallback at the platform config dir, deep-merged like every other section. No separate `llm.toml` — the unified loader already handles strict validation, auto-create with defaults, schema versioning, and the in-app `set_setting` write-back path; an isolated file would duplicate that machinery for no real benefit (API keys are env-only, so the secrets concern that might justify segregation doesn't apply here).

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

1. **`[llm.provider].api_key` (user-scope TOML only).** When the user-scope TOML's `api_key` field is non-empty, hiker uses it directly. The eligibility list in `core::config` refuses writes to this key from the vault TOML — it is *user-scope only* — so a vault that travels via Syncthing/git can't carry the secret. Plain text on disk in the platform config dir; users who want stronger isolation should prefer the env-var path or wait for the future keychain integration.
2. **`api_key_env` (default).** Names the environment variable holding the key. Read at provider construction time. Both user and vault TOML can set the env var name (per-vault override of which env to read is meaningful; per-vault override of the literal key is not — the eligibility list rejects it).

The vault TOML can still carry `api_key_env`, `backend`, `model`, `base_url`, and the rest — only the literal `api_key` is restricted. The settings pane hides the `api_key` row whenever the section's scope toggle is at `[Vault]`, since writes through that scope are refused. Empty `api_key` + empty `api_key_env` = no key set on the builder, which is correct for local Ollama and similar key-less backends.

*Keychain-backed storage is the future direction; lands when there's a real cross-platform keyring story. The two-source rule above is forward-compatible — keychain becomes a third precedence tier above the literal.*


## `core::agent` (the basic agent loop)

The default backend for interactive features. Just enough loop to make chat-over-vault work without requiring users to install an external ACP agent.

Shape:

- Takes a user message + accumulated history.
- Calls `core::llm::chat_stream` with a system prompt that describes the available vault tools.
- Parses tool-call requests from the response (using the `llm` crate's tool-calling support).
- **Dispatches tool calls through hiker's in-process MCP server** (`core::mcp`), not through direct `core::*` calls. The MCP tool registry is already the source of truth for the agent-facing tool surface (`mcp-tool-search-notes`, `mcp-tool-get-note`, `mcp-tool-write-note`, etc.), and the external ACP path goes through it too. Routing the basic agent loop through the same surface means one tool registry, one set of audit-log shapes, one place to add a new agent-callable verb. The in-process call uses the rmcp client against the local server (no HTTP for the hiker-internal case — direct trait dispatch). [agent-tool-routing-via-mcp]
- Loops until the model produces a terminal user-facing response (no further tool calls), with the iteration cap and per-tool-call timeout described in the next section as circuit-breakers.
- Returns a streaming response to the chat panel.

Not trying to compete with Claude Code or Goose — those are full agents with multi-step planning, sub-agent spawning, code execution, etc. This is *just* "let the model search and read the vault, respond." If a user wants more, ACP is the upgrade path.

Tool dispatch surface lives in this module; tool implementations are thin wrappers over existing `core::*` API. [llm-basic-agent-loop]


## Sessions

A **session** is the unit of conversational continuity: a single accumulating message history that survives across many user turns. A **turn** is one user message → one terminal model response (possibly with tool roundtrips and a cap-pause / continue inside it). One session contains many turns; the session owns the history, the turn is just the cursor moving through it.

The headline decisions:

- **The registry is session-keyed; history accumulates across user turns.** `core::agent`'s in-memory state lives under a `SessionId`, not a turn id. Each `chat_send` appends the user message to the existing session history before invoking `run_turn`; nothing GCs the session on `EndTurn`. A turn id is still emitted per call (and carried on every `AgentEvent`) so the frontend can correlate streaming events with the in-flight turn, but the registry doesn't key on it. The registry's shape is `HashMap<SessionId, SessionState>`; `chat_send` / `chat_continue` / `chat_stop` / `chat_cancel` take `(session_id, turn_id)`. [chat-session-persisted-history]
- **New sessions are an explicit user action.** A "New session" affordance in the chat panel (button next to the input, plus a keybind reserved in `keybind-registry`) ends the active session and starts a fresh one. No automatic session reset on idle, focus change, or vault re-open within the same app launch — sessions only end when the user says so or when the app process exits. The first `chat_send` of an app launch lazily creates a session if none is active, so the user doesn't have to click "New" before their first message. [chat-session-new-button]
- **Sessions persist as markdown notes in the vault.** Each session lives at `vault/.hiker/sessions/<YYYY-MM-DD>-<session-id>.md` (mirrors the path shape of `.hiker/agent-log/` and `.hiker/trash/`). The file is appended to on every `TurnFinished` so a crash mid-session loses at most the in-flight turn. Format: YAML frontmatter (session id, created-at, model, provider, turn count) followed by alternating `## User` / `## Agent` sections in the order the turns landed; tool calls render as fenced `hiker-tool-call` blocks under the agent section that issued them, with tool *results* rendered as paired fenced `hiker-tool-result` blocks. Reading the file is enough to reconstruct the session — the markdown is the source of truth, the in-memory `SessionState` is a working cache rebuilt from it. **Tool-call structure round-trips on resume.** `parse_session` reads both `hiker-tool-call` and `hiker-tool-result` blocks back into the rehydrated `SessionState.history` so a resumed session shows the agent its prior tool use; without this, the agent infers from a tool-call-stripped history that it can write notes without ever calling `write_note` / `edit_note`, and may emit a plain assistant message claiming a write happened. Native sessions preserve the full structure; the legacy "lossy resume" convention (text-only) stays as the fallback for imported sessions whose source format doesn't carry tool calls. [chat-session-markdown-store]
- **Hidden by default; opt-in to surface in the tree, via the per-source-type registry.** `.hiker/sessions/` rides the existing tree-hide-`.hiker/` rule (per `design.md`'s sidecar-hide convention) — sessions never clutter the regular file tree. They opt in via the generalized tree-source-visibility registry (per `editor.md`'s `tree-source-visibility-toggles`) which gives each source category — native sessions, imported sessions (per "Imported sessions" below), future categories — its own `vault.show_*_in_tree` setting; native sessions specifically are `vault.show_sessions_in_tree` (default `false`), surfacing them as a virtual top-level "Sessions" group when enabled, sorted newest first. The setting plugs into the existing eligible-key set so it persists per-vault via `set_setting`. Search and related-notes always include sessions regardless of the visibility toggle — they're notes, and dropping them out of search would mean the agent can't recall its own past investigations. [chat-session-show-in-tree-toggle]
- **The currently open note is auto-injected as turn context.** When the user has a note open in the editor at the moment `chat_send` fires, hiker pre-injects that note as a context block on the outgoing turn — vault-relative path, current buffer text (live, not last-saved, so the agent sees what the user is looking at), and a one-line "user is currently viewing this note" framing. The injection is scoped to the *single turn* it rides on, not the whole session: the next turn re-evaluates what's open and re-injects (or skips, if no note is open then). Mechanism plugs into `llm-context-injection`: for the basic agent loop this is appended to the system prompt or prepended as a synthetic user-context message; for ACP it's an Embedded Resource ContentBlock on `session/prompt`. A trash-preview / snapshot-preview / mutation-preview buffer is *not* injected — those are derived views, not the user's working note. When LLM features see the same note injected on N consecutive turns, the second-and-later injections collapse to a "still viewing <path>" reference rather than re-sending the whole content, so a long conversation about one note doesn't burn context window on duplicate copies. The agent has tools to read other notes; the injection is just the "what is the user looking at right now" hint. [chat-active-note-context-injection]
- **Resume on app open.** On vault open, hiker scans `.hiker/sessions/` and offers the most-recent session as the active one (so the user re-opens hiker and keeps talking from where they left off). The "New session" button is the way to *not* resume. Sessions older than the most recent stay on disk as searchable history but aren't auto-loaded — opening one is a future affordance (deferred below). [chat-session-resume-latest]
- **Trash routes through the regular vault trash.** Sessions are notes (they live under `.hiker/sessions/` which is carved out of the standard `.hiker/` ignore rule precisely so the indexer + watcher route them like any other note); deleting one moves the file to `<vault>/.hiker/trash/` via `core::ops::delete`, appends the regular `'deleted'` row to `core::changes`, and clears the in-memory `SessionState` from the registry. Restore + permanent-delete ride the existing trash-bin commands (`restore_trash_entry` / `permanent_delete_trash_entry`); a session that's been restored shows back up in the session-picker dropdown the next time it's opened. No separate "session trash" surface — the regular bin is the single source of truth so users don't have to learn two delete affordances. [chat-session-trash]

Out of this section's scope:

- **Multi-session UI** (a sidebar list of past sessions, click to re-open, branch off, delete). Deferred — v1 ships with implicit single-active-session UX (resume latest or start new). [chat-session-list-ui]
- **Cross-session memory / summarization.** The model only sees the current session's history. Distilling past sessions into long-term memory is a separate problem and not on the v1 roadmap.
- **Token-budget trimming inside a session.** When a session's history exceeds the provider's context window, the right answer is summarize-and-roll-forward; v1 simply lets the provider error and surfaces it as a turn error. The fix is its own slug (`chat-session-history-compaction`, deferred) once it bites.


### Imported sessions from other agents

Hiker has a sidecar that ingests conversation exports from other agents and converts them to the hiker session format so the user can read them — and optionally continue them — inside hiker. Future feature, but the shape is pinned now so the storage layout, search behavior, and respawn path are consistent with the native-sessions decisions above.

The headline decisions:

- **Imports land at `vault/.hiker/sessions/imported/<source>-<YYYY-MM-DD>-<id>.md`.** Same on-disk format as native sessions per `chat-session-markdown-store` — YAML frontmatter (id, source, source_id, created_at, model, provider, turn_count) plus alternating `## User` / `## Agent` sections; tool calls render as fenced `hiker-tool-call` JSON blocks under the agent section. Sub-directory `imported/` carves the namespace cleanly from native sessions; the watcher's `.hiker/sessions/` carve-out (per `chat-session-show-in-tree-toggle`) extends to cover this subfolder so imports are indexed and search-reachable like native sessions. [chat-session-import-storage-path]
- **Source-specific converters map foreign formats to the hiker shape.** A small `core::sessions::import::<source>` module per supported export format. v1 candidates: Claude Code transcript JSON, Codex session export, ChatGPT data-export JSON, Goose session log. Each converter is a one-file translator with a clearly named entry point; new converters land alongside their first user-driven import. Tool-call structure is preserved when the foreign format carries it in a way that maps onto hiker's `hiker-tool-call` block; otherwise it collapses to plain text under the agent section (the same lossy convention `parse_session` already accepts on resume). [chat-session-import-converters]
- **Sidecar entry point: `hiker session import <path>` (CLI) and a settings-UI affordance.** Both call `core::sessions::import::detect_and_convert(path)` which sniffs the file shape, picks a converter, writes the resulting markdown into `imported/`, and enqueues an indexer upsert. Bulk import (a directory of exports) is just the same call applied per file. Idempotency: re-importing the same source file (same `source` + `source_id`) updates the existing imported note rather than creating a duplicate. [chat-session-import-sidecar]
- **Imported sessions are search-reachable but tree-hidden by default.** Same shape as native sessions: indexed and surfaced in search regardless of tree visibility, opted into the tree via `vault.show_imported_sessions_in_tree` (default `false`). Two separate tree-visibility toggles (this and `vault.show_sessions_in_tree`) instead of one combined one, because users typically have very different volumes of native vs. imported sessions and may want to surface only one category. Both plug into `editor.md`'s `tree-source-visibility-toggles` registry. [chat-session-imported-show-in-tree-toggle]
- **"Continue as chat" respawns an imported session as a native one.** When the user opens an imported session in hiker, the chat panel surface offers a "Continue as chat" affordance. Click creates a new native session at `vault/.hiker/sessions/<YYYY-MM-DD>-<new-id>.md` whose history is seeded from the imported session's parsed turns (text-only by default — same lossy resume convention as `parse_session`); the basic chat agent picks up from the seeded history using the current `[llm]` provider. The respawned session is independent of the original — appending to it doesn't touch the imported file, and the original stays in `imported/` as a frozen artifact. The new session's frontmatter records `respawned_from: imported/<source>-<date>-<id>.md` so the lineage is recoverable. ACP's chat path can also respawn an imported session — the seed is just an Embedded Resource ContentBlock on the first `session/prompt`. [chat-session-respawn-from-import]

Out of this subsection's scope:

- **Round-tripping back out to the original agent's format.** Hiker imports as a one-way conversion; exporting a hiker session in Claude Code / ChatGPT format is not in v1.
- **Live import from external agents' running state.** v1 reads exported files; hooking into Claude Code's live session directory is reserved as `chat-session-import-live-sync`, deferred.
- **Multi-agent merging.** Combining several imported sessions into one hiker session is not in v1; each import lands as its own file.


## Chat panel UI

The chat panel is the user-facing surface for interactive features. Whether the backend is `core::agent` (the basic in-hiker loop) or `core::acp` (an external agent), the panel shape is the same — only the bytes flowing through it change.

The headline decisions:

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

- **Expand to center pane.** A button in the chat region's header opens the active chat session as an `agent`-kind tab in the top tab strip (per `editor.md`'s `tab-kinds`). The agent tab joins the existing buffer tabs as a peer; clicking it shows the chat surface at the center pane's full size; clicking any other tab restores that tab's content. The discovery panel's bottom-docked chat region collapses while at least one agent tab is open — the chat surface lives in the tab and duplicating it below would just be visual noise; closing the last agent tab restores the docked region at the prior `chat-panel-default-height` fraction. The discovery panel's other sections (search results, related notes) stay visible in the right column at all times. Multiple agent tabs are supported (one per chat session) and ride the standard tab-strip / autosave-tab-state machinery for free; agent tabs persist across launches the same way buffer tabs do. Composes with split view (per `design.md`'s deferred split-view bullet) — shift-clicking an agent tab puts chat side-by-side with a buffer for "read a note while talking to the agent about it." Reserves keybind id `chat.toggle-expand` in `keybind-registry`: with no agent tab open, opens one for the active session; with the active tab being an agent tab, closes it. [chat-panel-expand-to-editor]
- **Default split.** First open of a vault gives the chat region a small but useful default height (~30% of the panel) so the input is visible without dragging. Persisted per-vault via `settings-write-back` to `vault.chat_height` (eligible-key set grows by one). [chat-panel-default-height]
- **Min height.** The chat region has a minimum height that fits the input row plus one or two transcript lines — dragging below that snaps to the minimum rather than disappearing. The chat surface doesn't have its own collapse toggle; the discovery panel toggle (`panel-toggle-buttons`) is the only way to hide it, and it hides the whole right column.
- **Disable mode interaction.** When LLM features are disabled (`llm-features-disable-entirely`), the chat region is removed entirely (not just minimized) — the discovery sections take the full panel height, and the divider handle disappears. Re-enabling LLM features restores the persisted split.
- **Empty / pre-conversation state.** Before the first turn, the transcript area shows a small placeholder ("Ask about your vault, or pick a suggestion below…") plus optional starter chips. Once a turn lands, the placeholder is gone for the rest of the session. The transcript autoscrolls to the latest message on each turn unless the user has scrolled up — same well-trodden chat-UI rule.
- **Keybind.** Reserves `chat.focusInput` in `keybind-registry` for focusing the chat input from anywhere (chord TBD; lands when the keybind is wired). Esc in the chat input blurs back to the editor — symmetric with the existing search-input Esc behavior.

Deeper transcript chrome (tool-call confirmations, embedded-resource context cards, attached-context affordances) is still out of scope for this section — those shapes land with each backend's interactive surface (`llm-basic-agent-loop`, `llm-acp-client-optional`). The in-flight UX below covers the ambient affordances every interactive turn needs regardless of backend.

### In-flight UX

Affordances pinned here so both backends render the same thing.

- **Stop button while a turn is in flight.** The send affordance flips to a Stop button as soon as `chat_send` (or `chat_continue`) returns — visible for the whole busy period, not just at the iteration-cap pause. Click invokes `chat_stop(turn_id)` and the panel falls back to the idle send state on the next `TurnFinished`. Same command the cap-hit row's Stop already calls (`agent-chat-command-surface`); the difference is surfacing it as the always-on busy-state affordance rather than only at the cap. Cancel-vs-stop semantics from the command surface stand: this is Stop (preserves streamed transcript, finish reason `user_halted`), not Cancel — the harsher mid-stream abort stays an internal affordance for now (e.g., closing the panel mid-turn). [chat-panel-stop-button]
- **Thinking indicator between user turn and first agent event.** From the moment `chat_send` is invoked until the first `TextDelta` or `ToolCallStart` event arrives for the same `turn_id`, the panel shows a small inline indicator in the would-be assistant bubble — a labeled spinner ("Thinking…"). The indicator is tied to the absence of streamed content for the active turn, not to a timer: it disappears the instant any content arrives, and reappears between steps if a tool result comes back and the model goes quiet again before the next `TextDelta`. Cap-hit rows and tool-call cards are content for this purpose — a visible tool card displaces the indicator. [chat-panel-thinking-indicator]
- **Agent messages render as markdown.** Assistant text bubbles render as markdown (headings, lists, inline code, fenced code blocks, links, emphasis). User messages stay plain text — what the user typed is what shows. Streaming text deltas append into a markdown-rendered buffer; the bubble re-renders on each delta against the accumulated text rather than diffing tokens, which is fine at chat-bubble sizes. Sanitize: no raw HTML pass-through, no `javascript:` links, no embedded scripts — the markdown renderer's safe mode is the contract. Code blocks get a language hint when fenced; syntax highlighting itself is deferred. The same shape applies to ACP-emitted text. [chat-panel-markdown-render]
- **Agent-emitted note links are clickable and open the note in the editor.** When the markdown renderer encounters a link whose target resolves to a vault-relative note path (either a bare relative path like `subdir/foo.md`, or a `hiker://note/<rel-path>` URL — the latter unambiguous, the former resolved against the vault root), it renders as an in-app link that opens the note in the editor pane on click rather than opening an external browser. Non-vault links (http(s), file://) keep their normal browser-open behavior. The system prompt advertises the `hiker://note/<rel-path>` form as the canonical way for the agent to reference a note, so model output gravitates toward unambiguous links rather than bare relative paths. Hover shows the resolved absolute path; click respects the same dirty-buffer guard the file tree uses (`file-switch-guard-dirty`). [chat-panel-note-link-render]
- **Tool calls collapsed by default; click to expand.** Tool-call cards render in a one-line minified shape: `▸ <tool_name>(<short args summary>)` plus a status glyph (spinner / ✓ / ✗) and a one-line result summary. Clicking the row expands the card to show the full pretty-printed JSON of both args and result; clicking again re-collapses. The "short args summary" is a deterministic one-liner — first 1–2 fields of the args object, truncated at ~80 chars — generated frontend-side from the assembled args (so streaming deltas don't churn the summary). Expanded view is `<pre>`-formatted JSON via `JSON.stringify(..., null, 2)` with a copy button. State (collapsed / expanded) is per-card, not persisted across sessions — every tool card starts collapsed. Errored calls (`ok = false`) are *also* collapsed by default; the result summary plus the ✗ glyph is enough at a glance, and the user expands when they want to debug. [chat-panel-tool-call-collapsible]
- **Tool-call cards that touched a note jump to the note on header-click.** When a tool call resolves to a note path (read / write / modify tool variants — `get_note`, `write_note`, `edit_note`, `set_frontmatter`, `apply_tag`, `remove_tag`, future `modify_note`), the card's header row becomes a clickable target that opens the touched note in the editor. The expand-chevron stays clickable separately so the JSON-detail-on-click behavior (`chat-panel-tool-call-collapsible`) is unaffected — header click opens, chevron click expands. For tool calls that touched multiple notes (batch ops, search results), header click opens a small picker listing each touched path. Resolution rule: parse the rel-path out of the tool call's result (`{path, ...}` is the conventional shape for note-touching tools); if the result doesn't carry one, fall back to the args. Honors `file-switch-guard-dirty` like every other in-app note-open. Useful for auditing: trace what a tool call actually saw or wrote without leaving the chat. **Staged writes route to the staging preview.** When the tool result carries `status: "staged"` (per `mcp-write-tools-staging-aware`), the header-click opens the staging preview for the proposal — same surface as clicking a pending row on the activity detail page — rather than calling `openFile` against a path that doesn't exist on disk yet. `edit_note` calls produce N proposals sharing a `batch_id`; header-click opens the patch-review mode for the target path with all N hunks visible (per `note-open-routes-to-pending-review`). [chat-tool-call-opens-touched-note]

### Retry from a prior user message

The transcript exposes a Retry affordance on every user message in the current session. Clicking it rolls the conversation back to the state it was in just before that user message was sent, then re-submits the message as a fresh turn. The use case is the obvious one: the agent went off the rails, or the user wants to try the same prompt with a tweak after seeing how it landed the first time.

The headline decisions:

- **Retry button on each user message bubble.** A small icon-button (circular-arrow glyph) in the user message's right-side affordance cluster, visible on hover or always-on per the panel's existing message-action styling. Click rolls the session back to *immediately before* that user message and starts a new turn from the same text. Tooltip "Retry from here." Disabled while another turn is in flight for the session — retry is mutually exclusive with an active turn. [chat-retry-from-user-message]
- **Truncates the in-memory session and the on-disk markdown.** Retry is a destructive history operation, not a branch: every turn at or after the retried user message is removed from the session's `SessionState` and the markdown file is rewritten to match (per `chat-session-markdown-store`'s "markdown is the source of truth" rule). A single `core::changes` row is appended to the session note covering the truncation. No "previous attempts" archive in v1 — the user opted to throw the prior outcome away; keeping a hidden branch tree complicates the resume-on-open flow for marginal gain. Branching is reserved as a future affordance (`chat-retry-branches-session`, deferred). [chat-retry-truncates-session-history]
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

A discriminated-union enum emitted on the event `hiker:chat-event`. Every event carries `turn_id` (one per user message) and most carry `step_id` (one per LLM call within a turn — increments on each tool-loop iteration). [agent-event-stream-shape]

```rust
enum AgentEvent {
    TurnStarted       { turn_id, user_message_summary }
    StepStarted       { turn_id, step_id }
    TextDelta         { turn_id, step_id, text }
    ToolCallStart     { turn_id, step_id, call_id, tool_name }
    ToolCallArgsDelta { turn_id, step_id, call_id, args_delta }
    ToolCallComplete  { turn_id, step_id, call_id, args }
    ToolResult        { turn_id, step_id, call_id, ok, summary }
    StepFinished      { turn_id, step_id, finish_reason }
    IterationCapHit   { turn_id, completed_iterations }
    TurnFinished      { turn_id, usage, cost_estimate }
    Error             { turn_id, step_id: Option<u32>, message }
}
```

Translation happens at the `core::agent` boundary: provider-specific chunks from the `llm` crate's `chat_stream` are normalized into this enum so the chat panel never sees Anthropic-vs-OpenAI-vs-Ollama shape differences. The ACP path (`core::acp`) emits the same enum so the panel renders both backends identically.

**Why a single global event channel** rather than a per-invoke `Channel<T>`: continue / stop / cancel commands address an existing in-flight turn from a separate call, which a per-invoke channel doesn't model cleanly. Frontend filters by `turn_id`; one line.

### Command surface

```rust
chat_send(session_id, turn_id, message)    -> Result<()>  // start a turn within a session; events stream back
chat_continue(session_id, turn_id)         -> Result<()>  // resume a loop paused at IterationCapHit
chat_stop(session_id, turn_id)             -> Result<()>  // user halt; emits TurnFinished, session retained
chat_cancel(session_id, turn_id)           -> Result<()>  // mid-stream abort; cancels the in-flight LLM call too
chat_session_new()                         -> Result<SessionId>  // end active session, start a new one
chat_session_active()                      -> Result<Option<SessionId>>  // resume-latest at vault open
```

Backend keeps an `Arc<Mutex<HashMap<SessionId, SessionState>>>` so commands can address the active session and the turn cursor inside it. Each turn owns its tokio task; cancel drops the task handle. Stop preserves whatever has been streamed; cancel is the harsher abort. The session entry is **never** GC'd on terminal `TurnFinished` — only on explicit `chat_session_new` (or process exit, after the markdown file has been flushed). [agent-chat-command-surface]

### Iteration cap + Continue/Stop prompt

The loop has a per-turn cap on LLM calls. Default **10** (i.e. up to 9 tool roundtrips before the model has to produce a terminal answer). Configurable per-vault under `[llm.agent] iteration_cap` in `vault/.hiker/config.toml` (per `llm-providers-config`).

On hit:

1. The loop suspends; in-memory turn state retained.
2. `IterationCapHit { turn_id, completed_iterations }` fires.
3. The chat panel renders a system-style row in the transcript: "Agent has made N tool calls — [Continue] [Stop]."
4. **Continue** calls `chat_continue(turn_id)`, the loop resumes with the cap **reset to its full budget** (so 10 more), not "+1." Resetting on continue is honest — the user explicitly opted into more work, and incremental "+1" continues would be a worse UX.
5. **Stop** calls `chat_stop(turn_id)`, which emits `TurnFinished` with `finish_reason = "user_halted"` and drops the turn state.

The cap is a circuit-breaker against runaway tool-call loops, not a hard semantic limit. The prompt makes the pause visible rather than letting "thinking…" spin forever or auto-killing a turn that was about to land its terminal answer. [agent-iteration-cap-prompt]

`IterationCapHit` is a **suspend**, not a terminal event: the loop emits it without a paired `TurnFinished`, the in-memory turn state is retained, and the next event for that `turn_id` is whatever Continue/Stop produces (`StepStarted` on resume, or `TurnFinished { user_halted }` on Stop). The chat panel's busy/active-turn machine should treat the cap-hit row as "still the same turn, just paused" rather than rolling over to a new turn id.

### Per-tool-call timeout

Each MCP tool call gets a default **30s** timeout (configurable under `[llm.agent] tool_timeout_secs`). On timeout, the loop emits a synthesized `ToolResult { ok: false, summary: "tool timed out" }` back into the model's context so it can decide to retry, try a different tool, or give up. The loop does not bubble timeouts as turn-killing errors — the agent is allowed to recover.

A timed-out tool task is dropped (its tokio handle cancelled) so resources don't leak when the model moves on without it. Repeated timeouts on the same tool name within a turn are not specially handled in v1 — the iteration cap will catch any pathological "retry-the-stuck-tool-forever" loop. [agent-tool-call-timeout]

### Other event shapes (forward refs)

The agent path covers interactive features. Background and fan-out features have intentionally different UI shapes since their concerns aren't streaming text — the shapes are pinned here so they don't accidentally diverge.

- **Fan-out, background, and note-mutation features** route through `core::tasks` (per `task-queue.md`). The queue's `QueueEvent` channel + the home-page Task queue widget are the user-visible progress surface; per-feature toasts and bespoke event enums are out. Single-note mutations land in the active editor buffer as a single CM6 transaction (per `editor.md`'s Note-mutations menu); batch mutations and agent writes route to staging review (per `settings.md`'s Staging review).

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

`[llm] enabled = false` (or `[acp] agent = "none"` if only ACP needs disabling). When fully disabled:

- All background and fan-out features no-op (toggles greyed with a "LLM disabled" tooltip).
- Chat panel is hidden.
- No agent process spawns; no provider API calls fire.
- Hiker is a pure local notes app. MCP server stays available for users who want to drive the vault from their own external tooling. [llm-features-disable-entirely]


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
