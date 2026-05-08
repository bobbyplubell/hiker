# LLM strategy

How hiker uses generative LLMs. Pins the modules, the routing rule per feature type, where prompts and config live, and the policy posture that keeps subscription-billed agents in the role they're priced for.

Embeddings are out of scope of this doc — `core::embed` is its own module with its own trait and version-tag machinery (see `index.md`'s embedder section). The local fastembed-rs default and the cloud/Ollama options (via the `llm` crate's `EmbeddingProvider` trait, sharing the same crate dep this doc uses for generative access) are documented there. The two consumers of the `llm` crate — `core::embed::LlmEmbedder` and `core::llm` — share a dep but have separate trait boundaries and policy postures (embeddings are always automation-shaped → pay-per-call APIs always; the interactive-vs-background distinction below applies only to generative use).

The headline decisions:

- **`core::llm` is the foundation.** A new module wrapping the [`llm`](https://crates.io/crates/llm) crate (graniet/llm) for multi-provider access — Anthropic, OpenAI, Ollama, Google, Groq, Mistral, DeepSeek, etc. Module discipline: `llm` crate confined to this module, mirroring rusqlite-only-in-store and fastembed-only-in-embed. [llm-core-module]
- **Background and fan-out features call `core::llm` directly.** Single-shot prompts for auto-tag-on-save, summary-on-save, cluster summarization (background); pre-scoped batch fan-outs for RAPTOR-shaped tree building, cluster naming across N clusters, regenerate-all-summaries (fan-out). Pay-per-call billing model — no ToS grey area. [llm-strategy-direct-non-interactive]
- **Interactive features use a basic in-hiker agent loop by default.** A new module (`core::agent`) implementing a simple message-history + tool-dispatch loop on top of `core::llm`. Calls hiker's vault primitives as tools. Just enough to make chat-over-vault and similar interactive features work without requiring an external agent install. [llm-basic-agent-loop]
- **ACP client is an optional escape hatch.** Users who want a more capable agent (Claude Code, Codex, Goose, Gemini CLI) can configure one; the chat panel routes through it instead of the basic agent loop. ACP is *only* for interactive features — never used for background or fan-out. [llm-acp-client-optional]
- **The whole agent layer is disable-able.** `[llm] enabled = false` (or equivalent) turns off background features, fan-out features, and the chat panel. Hiker becomes a pure local notes app; the MCP server stays available for users who want to query the vault from their own external tools. [llm-features-disable-entirely]
- **Prompts are files.** Two-tier user/vault scope, mustache placeholders, settings UI Prompts tab when settings UI lands. Same for every feature, regardless of which module fires the prompt. [llm-prompts-file-store]


## Architecture

```
                   hiker UI (Tauri)
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

User explicitly clicks something in the chat panel or an agent affordance, response shown to the user before being applied. Examples: chat-over-vault (subsumes the previously-deferred "RAG chat over vault" entry in `design.md`), vision OCR review, "ask the agent to propose a name for this cluster," bulk-reorg conversation walks.

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

Configured per-vault in `vault/.hiker/llm.toml` (with user-scope fallback at `~/.config/hiker/llm.toml`):

```toml
[provider]
backend = "anthropic"           # or "openai", "ollama", "google", "openrouter", ...
model = "claude-sonnet-4-7"
api_key_env = "ANTHROPIC_API_KEY"
base_url = ""                   # optional override (Ollama / OpenAI-compat)

[limits]
max_tokens = 4096
timeout_secs = 60
```

[llm-providers-config]

API keys are read from environment variables; `api_key_env` names the variable. Never stored in TOML — including per-vault TOML that gets synced.


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


## Chat panel UI

The chat panel is the user-facing surface for interactive features. Whether the backend is `core::agent` (the basic in-hiker loop) or `core::acp` (an external agent), the panel shape is the same — only the bytes flowing through it change.

The headline decisions:

- **The chat panel is the bottom region of the discovery panel** — same right-hand column that already hosts search results and related notes (`search-discovery-panel`). The panel's vertical layout is: discovery sections (search results / related / future surfaces) take the top, the chat surface is **pinned at the bottom and expands upward**. Same column, same width, same toggle button (`panel-toggle-buttons`) flips the whole panel open / closed. [chat-panel-pinned-bottom]
- **Chat scrolls independently from the sections above it.** The discovery panel becomes a two-region layout: a top region holding the search/related/future sections (which scroll as a unit, same shape as today), and a bottom region holding the chat (which scrolls on its own). Scrolling a long agent transcript doesn't move the search results out of view, and scrolling search results doesn't unanchor the chat input. [chat-panel-detached-scroll]
- **The chat region is vertically resizable** via a drag handle on its top edge — the boundary between the discovery sections region and the chat region. Standard UX: hovering the boundary swaps the cursor to `row-resize`; dragging up grows the chat region (shrinking the sections region) and vice versa. Same affordance shape as `side-panel-resize`, rotated 90°. [chat-panel-vertical-resize]

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

- **Default split.** First open of a vault gives the chat region a small but useful default height (~30% of the panel) so the input is visible without dragging. Persisted per-vault via `settings-write-back` to `vault.chat_height` (eligible-key set grows by one). [chat-panel-default-height]
- **Min height.** The chat region has a minimum height that fits the input row plus one or two transcript lines — dragging below that snaps to the minimum rather than disappearing. The chat surface doesn't have its own collapse toggle; the discovery panel toggle (`panel-toggle-buttons`) is the only way to hide it, and it hides the whole right column.
- **Disable mode interaction.** When LLM features are disabled (`llm-features-disable-entirely`), the chat region is removed entirely (not just minimized) — the discovery sections take the full panel height, and the divider handle disappears. Re-enabling LLM features restores the persisted split.
- **Empty / pre-conversation state.** Before the first turn, the transcript area shows a small placeholder ("Ask about your vault, or pick a suggestion below…") plus optional starter chips. Once a turn lands, the placeholder is gone for the rest of the session. The transcript autoscrolls to the latest message on each turn unless the user has scrolled up — same well-trodden chat-UI rule.
- **Keybind.** Reserves `chat.focusInput` in `keybind-registry` for focusing the chat input from anywhere (chord TBD; lands when the keybind is wired). Esc in the chat input blurs back to the editor — symmetric with the existing search-input Esc behavior.

The chat region's *contents* (transcript rendering, tool-call display, streaming behavior, attached-context affordances) are out of scope for this section — they ride on top of the placement / resize shape pinned here. Concrete UI shapes for tool-call confirmations, embedded-resource context cards, and the multi-turn affordances live in their own slugs when each backend's interactive surface is implemented (`llm-basic-agent-loop`, `llm-acp-client-optional`).


## Event streams and Tauri command surface

The agent loop streams its progress to the chat panel through a typed event channel; the panel calls back into the loop for user-driven actions (continue past a cap, stop, cancel mid-stream). Same shape applies whether the backend is `core::agent` or `core::acp` — the UI only sees the event enum.

### AgentEvent

A discriminated-union enum emitted on the Tauri event `hiker:chat-event`. Every event carries `turn_id` (one per user message) and most carry `step_id` (one per LLM call within a turn — increments on each tool-loop iteration). [agent-event-stream-shape]

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

**Why a single global event channel** rather than Tauri 2's per-invoke `Channel<T>`: continue / stop / cancel commands address an existing in-flight turn from a separate Tauri call, which a per-invoke channel doesn't model cleanly. Frontend filters by `turn_id`; one line.

### Tauri command surface

```rust
chat_send(message, turn_id) -> Result<()>     // start a turn; events stream back
chat_continue(turn_id)      -> Result<()>     // resume a loop paused at IterationCapHit
chat_stop(turn_id)          -> Result<()>     // user halt; drops loop state, emits TurnFinished
chat_cancel(turn_id)        -> Result<()>     // mid-stream abort; cancels the in-flight LLM call too
```

Backend keeps an `Arc<Mutex<HashMap<TurnId, TurnState>>>` so continue / stop / cancel can address active turns. Each turn owns its tokio task; cancel drops the task handle. Stop preserves whatever has been streamed; cancel is the harsher abort. [agent-chat-command-surface]

### Iteration cap + Continue/Stop prompt

The loop has a per-turn cap on LLM calls. Default **10** (i.e. up to 9 tool roundtrips before the model has to produce a terminal answer). Configurable per-vault under `[llm.agent] iteration_cap` in `llm.toml`.

On hit:

1. The loop suspends; in-memory turn state retained.
2. `IterationCapHit { turn_id, completed_iterations }` fires.
3. The chat panel renders a system-style row in the transcript: "Agent has made N tool calls — [Continue] [Stop]."
4. **Continue** calls `chat_continue(turn_id)`, the loop resumes with the cap **reset to its full budget** (so 10 more), not "+1." Resetting on continue is honest — the user explicitly opted into more work, and incremental "+1" continues would be a worse UX.
5. **Stop** calls `chat_stop(turn_id)`, which emits `TurnFinished` with `finish_reason = "user_halted"` and drops the turn state.

The cap is a circuit-breaker against runaway tool-call loops, not a hard semantic limit. The prompt makes the pause visible rather than letting "thinking…" spin forever or auto-killing a turn that was about to land its terminal answer. [agent-iteration-cap-prompt]

### Per-tool-call timeout

Each MCP tool call gets a default **30s** timeout (configurable under `[llm.agent] tool_timeout_secs`). On timeout, the loop emits a synthesized `ToolResult { ok: false, summary: "tool timed out" }` back into the model's context so it can decide to retry, try a different tool, or give up. The loop does not bubble timeouts as turn-killing errors — the agent is allowed to recover.

A timed-out tool task is dropped (its tokio handle cancelled) so resources don't leak when the model moves on without it. Repeated timeouts on the same tool name within a turn are not specially handled in v1 — the iteration cap will catch any pathological "retry-the-stuck-tool-forever" loop. [agent-tool-call-timeout]

### Other event shapes (forward refs)

The agent path covers interactive features. Background and fan-out features have intentionally different UI shapes since their concerns aren't streaming text — the shapes are pinned here so they don't accidentally diverge.

- **Fan-out features** emit a separate `FanoutEvent` enum (`JobStarted` / `ItemStarted` / `ItemFinished` / `JobFinished` / `JobCancelled` / `Error`) on `hiker:fanout-event`, plus a `fanout_cancel(job_id)` Tauri command. UI is a progress widget (count, ETA, cancel) per the fan-out feature-type rules above. Lands with the first fan-out feature; RAPTOR build is the natural anchor. [fanout-event-stream-shape]
- **Background features** have no per-call UI. Failures show a toast; success applies silently. The aggregate `llm-cost-transparency` status-bar indicator is the only persistent surface, reading counts from `llm-audit-log`. No event channel needed.
- **Note-mutation features** route through `core::llm` direct (per `note-mutations-menu`), so no AgentEvent stream. UI is a small in-flight indicator with cancel during the call, followed by the diff viewer (`diff-viewer-pane`) for accept/decline review per `note-mutation-diff-review`. The single-shot completion fires through `core::llm::chat` (non-streaming) since the deliverable is a derived file rather than a live conversation; streaming-into-the-derived-buffer is an additive UX, not a different architecture. [note-mutation-progress-toast]

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
