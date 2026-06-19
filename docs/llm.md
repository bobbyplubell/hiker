# LLM strategy

How hiker uses generative LLMs. After the core rework there are exactly **two**
LLM surfaces, and neither is an in-app chat:

1. **`core::llm`** — the in-process client for **background** and **fan-out**
   features (summaries, cluster naming, the embeddings/summaries that feed trees).
   Single-shot prompts, no agent loop.
2. **The MCP server** (`mcp.md`) — the **sole agent surface**. External agents
   (Claude Code, Goose, Codex, any rmcp client) reach the vault through MCP; their
   writes land as reviewable **pending** edits (`op-log.md`, `patch-review.md`).

The in-house agent loop (`core::agent`), the ACP client (`core::acp`), and all
in-app chat (the chat panel, sessions, the `Agent` tab) are **deleted**. There is
no interactive LLM surface inside hiker — you bring your own agent and point it at
hiker's MCP server. This keeps subscription-billed agents in the role they're
priced for and keeps hiker a notes app, not an agent host.

Embeddings are out of scope of this doc — `core::embed` is its own module with its
own trait and version-tag machinery (`index.md`'s embedder section). It and
`core::llm` share the `hiker-llm` dependency but have separate trait boundaries:
embeddings are always automation-shaped, while the background/fan-out distinction
below applies to generative use.


## `core::llm`

The foundation. A thin config bridge in `core::llm` builds a client from the
`[llm]` config; the generative client itself lives in the `hiker-llm` leaf crate
(so `hiker-core` and `hiker-crawler` share one client). The `llm` crate
(graniet/`llm`, multi-provider — Anthropic, OpenAI, Ollama, Google, Groq, Mistral,
DeepSeek, …) is confined to `hiker-llm`. [llm-core-module]
status:: done
touches:: [[code:hiker/llm]]
note:: `core/src/llm.rs` — `client_from_config` / `provider_config_from` translate the `[llm]` section into a `hiker_llm::GraniteLlmClient`; the `llm` crate is imported only in `hiker-llm`. `MockLlmClient` for downstream tests.

`core::llm` exposes single-shot completion (and a streaming variant) used by:

- **Background features** — triggered by routine actions (save, ingest), applying
  terminal results without per-call review (the user opted into the feature).
  Examples: auto-tag-on-save, summary-on-save, cluster summarization on cluster
  build. Debounced so save bursts coalesce to one prompt. Default off; opt-in per
  feature. When `[llm.background] review_required` is on, a background write stages
  a **pending** op (reviewed per `patch-review.md`) instead of mutating frontmatter
  directly. [llm-feature-type-background, llm-feature-debounce]
- **Fan-out features** — user-initiated batch operations spanning many items.
  Examples: RAPTOR-shaped tree building (cluster naming + summarization across N
  clusters), regenerate-all-summaries, tag-all-unenriched-notes. Scope is determined
  by hiker's pre-batch logic — the LLM doesn't decide its own scope. Visible progress
  (count, ETA, cancel) via `core::tasks` (`task-queue.md`). [llm-feature-type-fanout]

Both shapes submit to `core::tasks`; the in-process direct-LLM worker drains via
`core::llm`. The same queue lets external rmcp clients (Claude Code, Codex) drain
the queue if the user has them attached (`task-queue.md`). Pay-per-call billing on
the direct-LLM lane — no ToS grey area. [llm-strategy-direct-non-interactive]


## `[llm]` config section

Standard hiker config (per `settings.md`): per-vault `vault/.hiker/config.toml`
with a user-scope fallback, deep-merged like every other section.

```toml
[llm]
enabled = true                  # master AI gate; false → background + fan-out no-op

[llm.provider]
backend = "anthropic"           # or "openai", "ollama", "google", "openrouter", ...
model = "claude-sonnet-4-7"
api_key_env = "ANTHROPIC_API_KEY"
base_url = ""                   # optional override (Ollama / OpenAI-compat)

[llm.limits]
max_tokens = 4096
timeout_secs = 60

[llm.background]
review_required = false         # background writes stage a pending op instead of writing direct

[llm.audit]
log_full_prompt = false         # see Audit log; obs-no-content discipline
```

[llm-providers-config]
status:: done
implements:: [[code:hiker/config/sections/LlmConfig]]
note:: `core/src/config/sections.rs::LlmConfig` — top-level `enabled`, `[llm.provider]` (backend / model / api_key_env / api_key / base_url), `[llm.limits]` (max_tokens / timeout_secs), `[llm.background] review_required`, `[llm.audit] log_full_prompt`. Strict-load validates the section. The removed `[llm.agent]` (in-house-loop tuning) is ignored on load with a one-time warning. `core::llm::provider_config_from` builds a usable client from the loaded section.

**API keys** come from one of two sources, in precedence order:

1. **`[llm.provider].api_key` (user-scope TOML only).** When non-empty, used
   directly. The eligibility list in `core::config` refuses writes to this key from
   the vault TOML, so a vault that travels via a file-sync or git can't carry the
   secret.
2. **`api_key_env` (default).** Names the environment variable holding the key, read
   at provider construction time. Both user and vault TOML can set the env var *name*.

Empty `api_key` + empty `api_key_env` = no key set on the builder (correct for local
Ollama). The settings pane hides the `api_key` row when the section's scope toggle is
at `[Vault]`.


## Disable mode

`[llm] enabled = false` is the master AI gate. When off, background and fan-out
features no-op; per-feature toggles render greyed with a "LLM disabled" tooltip.
The **MCP server stays available** (gated by its own `[mcp] enabled`, default off)
— disabling AI *inside* hiker means hiker shouldn't host generative features, not
that the user is hostile to external agents they run out-of-process. Deterministic
pipelines (indexing, embedding, search, clustering build, wikilink resolution, the
editing model) are not AI in the sense the gate targets and keep running.
[llm-features-disable-entirely]
status:: planned
note:: master AI gate: `[llm] enabled = false` no-ops background + fan-out features and greys their toggles. MCP is independent (its own `[mcp] enabled`). User-scope default; per-vault override via the section scope toggle. (The chat-panel hide behaviors from the pre-rework design no longer apply — there is no chat panel.)


## Prompts as files

Every LLM-driven feature stores its prompt as a markdown file; editing the file
changes the prompt. Two-tier scope (mirrors the config tiers):

- User scope: `~/.config/hiker/prompts/<feature>.md` — bundled defaults, user can
  override.
- Vault scope: `vault/.hiker/prompts/<feature>.md` — per-project overrides, wins
  over user.

Defaults written to user scope on first run if absent. Mustache `{{var}}`
substitution; each shipped default starts with a comment block listing the
placeholders that feature accepts. [llm-prompts-file-store, llm-prompts-mustache-templating]
status:: done
touches:: [[code:hiker/prompts]]
note:: `core/src/prompts.rs` — two-tier loader (`ensure_user_default` auto-creates from bundled defaults; vault wins), `Prompts::load(vault_root)`, `bundled_defaults()`, `substitute` (`{{var}}`, unknown placeholders pass through verbatim).

**Upgrade-aware staleness.** Hiker stamps each bundled default's content hash in a
`<feature>.default.sha` sidecar next to the user-scope prompt; if the bundled
default's hash changes upstream, the user's override isn't clobbered — staleness is
flagged in the agent log (and a future Prompts tab). [llm-prompts-staleness-on-upgrade]
status:: done
touches:: [[code:hiker/prompts]]
note:: `core/src/prompts.rs::Prompts::staleness` — `<feature>.default.sha` records the bundled default's blake3 hash at first-write; a bumped upstream default surfaces as a stale-feature name, never clobbering the user file. The host emits a `tracing::warn!` + an `AgentLog` row per stale feature.

Bundled defaults register in `core::prompts::bundled_defaults()` keyed by feature;
the prompt file lives at `core/prompts/<feature_key>.md` and is baked into the
binary. New background/fan-out features land their default there.


## Audit log

Every LLM call (background, fan-out) and every MCP tool call appends one row to
`vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`: timestamp, feature slug, surface
(`core::llm` / `mcp-tool-call`), triggering action, prompt hash + template version,
response summary (tokens, finish reason), cost estimate when the provider reports
one. Daily rotation. Full prompt/response text is gated on
`[llm.audit] log_full_prompt = true` (default off — the no-content discipline,
`observability.md`). [llm-audit-log]
status:: done
note:: `core/src/audit.rs` — `AgentLog` JSONL writer at `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl`, daily rotation, content-blind unless `log_full_prompt`. `AuditEntry` carries `surface` (`core::llm` / `mcp-tool-call`), `feature`, `status`, optional `error` + `details`. `mcp-server/src/audit.rs` is a thin wrapper over the shared writer carrying the `[mcp.audit] log_full_input` redaction policy. The shared writer means both `log_full_input` and `log_full_prompt` land their rows in the same daily file.

The `agent-log/` directory is **durable** provenance under `.hiker/` (the call
record, distinct from the content-change record git carries when integrated).


## Operational rules

- **One user action ≤ one prompt** for background features. Fan-out is the explicit
  exception, with scope determined pre-batch by hiker (the LLM can't expand scope
  mid-run).
- **No recursion.** A response is applied directly; it doesn't trigger further
  automatic prompts.
- **No silent retries.** Failed calls surface as errors; no auto-retry.
- **Cost transparency.** A status-bar indicator shows recent LLM activity when any
  feature is enabled; click → audit log viewer. [llm-cost-transparency]
status:: planned
note:: status-bar indicator of recent LLM activity; click opens the audit log viewer
- **Prompts visible.** The audit log + the prompt files expose exactly what gets
  sent. No hidden internal prompts.


## Enrichment routing

The enrichment pipeline (`design.md`) — auto-tag, type classification, summary,
vision OCR review for imported content — runs every LLM-driven stage as a
*background* feature when triggered automatically (on save) and as a *fan-out*
feature when triggered as a batch. Both call `core::llm` direct: single-shot prompts
per note, no agent loop. Entity/reference extraction may use NER / pattern-matching
instead; when they use LLM calls, the same routing applies. Enrichment is on-demand
and frontmatter-only — it writes structured metadata back into note frontmatter,
never into a hidden `.hiker/` substrate.


## Out of scope

- **In-app chat / interactive agent.** Removed. External agents reach the vault via
  MCP (`mcp.md`); use the agent's own UI.
- **ACP client.** Removed. An external agent connects to hiker's MCP server as a
  client; hiker is not an ACP client.
- **Hosting an LLM model in-process.** Local-Ollama use is via the `llm` crate's
  Ollama backend (a separately-running Ollama server); hiker bundles no model runtime.
- **Function calling from `core::llm` direct calls.** Tools are the MCP server's
  concern. Background and fan-out features fire single-shot completions with no tool
  surface.
- **Prompt safety / jailbreak filtering.** The provider's safety layer is the safety
  layer.


## Forward refs

- `mcp.md` — the MCP server, hiker's sole agent surface; agent writes land as pending edits.
- `task-queue.md` — the queue background/fan-out features submit to; external rmcp clients can drain it.
- `op-log.md` / `patch-review.md` — the pending/patch-review layer agent and review-mode background writes flow through.
- `index.md` — `core::embed` (embeddings), the separate module sharing the `hiker-llm` dep.
- `design.md` enrichment pipeline — the background/fan-out features that consume the prompts above.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc.

- **llm-feature-debounce** — 1–2s coalesce window for save-driven background LLM features so save bursts → one prompt [llm-feature-debounce]
  status:: planned
- **llm-prompts-mustache-templating** — `core/src/prompts.rs::substitute` — `{{var}}` substitution (whitespace-tolerant); unknown placeholders pass through verbatim so a partially-customized prompt doesn't silently lose data [llm-prompts-mustache-templating]
  status:: done
  touches:: [[code:hiker/prompts]]
- **llm-prompts-settings-tab** — settings UI Prompts tab: editable text, default reference, reset, diff, test affordance [llm-prompts-settings-tab]
  status:: planned
- **llm-prompt-test-button** — "test prompt with sample data" affordance in the Prompts tab [llm-prompt-test-button]
  status:: planned
