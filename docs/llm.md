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
- Dispatches tool calls to hiker's vault primitives (search, get_note, get_chunks, etc.) — for v0 likely calling `core::*` modules directly; eventually possibly via `core::mcp` for consistency with the ACP path.
- Loops until the model produces a terminal user-facing response (no further tool calls).
- Returns a streaming response to the chat panel.

Not trying to compete with Claude Code or Goose — those are full agents with multi-step planning, sub-agent spawning, code execution, etc. This is *just* "let the model search and read the vault, respond." If a user wants more, ACP is the upgrade path.

Tool dispatch surface lives in this module; tool implementations are thin wrappers over existing `core::*` API. [llm-basic-agent-loop]


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
