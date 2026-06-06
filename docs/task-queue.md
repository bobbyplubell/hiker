# Task queue

A unified work queue for non-interactive jobs — LLM work (note mutations, RAPTOR / fan-out summarization, background single-shots like auto-tag-on-save) and other long-running background I/O. Producers submit tasks and await results; consumers (in-process workers and external MCP-attached agents) drain the queue. Replaces the "every feature calls `core::llm` directly" routing in `llm.md` for everything except chat, and gives non-LLM background work the same progress/cancel/visibility surface.

`core::tasks` is the queue + dispatcher module, sibling to `core::llm` and `core::agent`. Producers submit `Task` records; the queue arbitrates at runtime who processes each — the direct-LLM worker, an MCP-attached external agent, or the in-process chat agent. [task-queue-core-module]

**Scope is everything non-interactive** — mutations, fan-out, background single-shots. Chat (basic agent loop + ACP) keeps its existing direct path because it's a streaming session, not a discrete unit of work; anything else that fires an LLM prompt routes through the queue. [task-queue-scope-non-chat]

**Non-LLM I/O lane.** I/O-bound work (a web crawl) drains on a dedicated in-process worker — not the single-shot direct-LLM drain, and not the synchronous `NonLlmHandlers` side-channel (a crawl runs for minutes with concurrent fetches). It's never an MCP client, carries no `output_schema`, and lives on the queue purely for its lease/progress/cancel/visibility surface. A crawl's per-page extractions roll up under the parent crawl via `task-queue-task-grouping` so the widget shows one row, not N. [task-queue-io-worker-lane, crawl-task-queue-lane]


## Architecture

```
                producers (in-process)
        ┌──────────────┬────────────────────┐
        │              │                    │
   note-mutations  raptor build       background hooks
   menu actions    (fanout)           (auto-tag, summary)
        │              │                    │
        └──────────────┴────────────────────┘
                       │
                       ▼ submit(Task) -> TaskHandle
                ┌─────────────┐
                │ core::tasks │  ◄────── cancel(id)  (in-process)
                │   queue     │
                └─────────────┘
                       │
        ┌──────────────┴───────────────────────────┐
        │                                          │
        ▼                                          ▼
  direct-LLM worker                       MCP task_* tools
  (background tokio task,                 (rmcp surface; same in-process
   drains Direct-shape tasks               registry whether the caller is
   via core::llm::chat;                    external HTTP rmcp or the basic
   on/off via setting)                     chat agent's tool dispatch)
                                                   │
                                  ┌────────────────┼────────────────────┐
                                  │                │                    │
                                  ▼                ▼                    ▼
                           external HTTP     basic chat agent     ACP-acting-as-
                           clients           (when expose_to_     MCP-client (when
                           (Claude Code,     chat_agent = true,   the user has set
                           Codex, …)         the user can ask it  one up that way)
                                             to drain queue)

   each consumer's terminal action (direct worker completes, MCP submit/fail,
   internal cancel) resolves the producer's handle and emits a queue event
   for the home-page widget.
```

`core::tasks` owns the queue, the (optional) direct worker handle, the lease table, and the event emitter. `core::llm` is imported by the direct worker; `core::agent` is *not* a queue dependency — the chat agent reaches the queue purely through the MCP tool surface, like any other client. The MCP server's `task_*` tools are a thin facade over the queue's checkout/submit primitives (policy lives in `core::tasks`); the chat-agent and external-rmcp paths share one tool registry, audit shape, and error-code set.


## Task shape

```rust
struct Task {
    id: TaskId,
    kind: TaskKind,                    // see below
    priority: Priority,                // High | Normal | Low
    shape: TaskShape,                  // Direct | Agent
    payload: TaskPayload,              // prompt + structured inputs
    output_schema: Option<JsonSchema>, // structured output enforcement
    submitted_at: SystemTime,
    metadata: serde_json::Value,       // free-form, surfaced in audit log
}

enum TaskKind {
    NoteMutation { mutation: NoteMutationKind, source_path: VaultRel },
    RaptorSummarize { tree_id: TreeId, cluster_node_id: NodeId, level: u8 },
    RaptorTriageMatch { tree_id: TreeId, source_path: VaultRel },
    AutoTag { source_path: VaultRel },
    SummaryOnSave { source_path: VaultRel },
    // ... new kinds added with each feature
}

enum NoteMutationKind {
    ReformatAsMarkdown,
    // ... new mutation kinds added with each feature
}

enum TaskShape {
    Direct,   // single-shot prompt → response, possibly with output schema
    Agent,    // expects an MCP client (chat agent / external) that can call tools mid-task
}

enum Priority { High, Normal, Low }
```

[task-queue-task-shape]

`TaskKind` is an exhaustive enum — one source of truth across worker code, audit log, MCP schema, and the home-page widget; adding a task type is one variant + its producer, no string-typed dispatch.

`TaskShape` is the worker-routing hint. `Direct` tasks drain on either lane (direct-LLM worker or any MCP client); `Agent` tasks need tool use mid-processing and drain only on an MCP client — the direct-LLM worker skips them because it can't make tool calls. [task-queue-shape-routing]


## Priority

Three named tiers — `High`, `Normal`, `Low`. Strict ordering: High drains before Normal, Normal before Low; within a tier, FIFO by `submitted_at`. [task-queue-priority-tiers]

Producers default to `Normal` and bump only with a reason: `High` for user-initiated foreground work (the note-mutation menu, because the user is watching), `Low` for ambient bulk work (RAPTOR's hundreds of per-cluster summaries, so they don't block a foreground mutation).


## Cluster-tree task types

Two task types are produced by the cluster-editor / triage pipeline (per `clustering.md`, `cluster-editor.md`). Both are `TaskShape::Direct` — they don't need tool calls during execution.

### `RaptorSummarize` [task-queue-raptor-summarize]

Per-cluster LLM call during a tree build pass (one task per cluster per level), or on-demand regeneration triggered from the cluster editor (`cluster-editor-regenerate-via-task-queue`).

- **Payload:** `tree_id`, `cluster_node_id`, `level`, member titles + summaries (read by `core::cluster` at task-construction time and passed inline). Output schema enforces `{ name: string, summary: string, confidence: f32 }`.
- **Priority:** `Low` during initial build (large fan-outs shouldn't block foreground work). `Normal` for user-triggered regenerations (user is watching).
- **Retry:** one retry on transient LLM error; mark the cluster as "summarization failed, falling back" and run the tf-idf template path (`cluster-summarize-fallback-tfidf`) on second failure.
- **Routing:** direct-LLM worker drains by default; any MCP client can also drain.
- **Sample-and-merge variant.** For clusters with > 30 members (`raptor-summarize-sample-merge-threshold`, configurable), the producer splits into batches and submits them as sibling tasks plus a fan-in merge task that depends on them. The merge task carries the partial summaries as inputs and produces the final cluster summary. Fan-in coordination uses the queue's standard dependency mechanism (per `task-queue-dependencies`). Capped at 300 members per cluster — beyond that, fall back to the template path. [raptor-summarize-sample-merge]

### `RaptorTriageMatch` [task-queue-raptor-triage-match]

Per-note classifier run against a saved Evergreen tree. Triggered on the three triage pathways (`cluster-editor-triage-on-save`, `cluster-editor-triage-scheduled-rerun`, `cluster-editor-triage-modified-rerun`).

- **Payload:** `tree_id`, `source_path`. The worker reads the note's embedding from `index.db` and the saved tree's centroids from `index.db`'s `cluster_centroids` table, runs the beam-descent classifier (`cluster-place-beam-descent`), produces a `PlacementMatch { leaf_node_id, confidence, margin }`, resolves the matched node's policy, and emits the corresponding pending op into the op log (per `triage-staging-proposals`). No LLM call at all — the entire task is cosine arithmetic + an op append.
- **Priority:** `Normal` for on-save matches (user just authored the note; they want the routing to happen now). `Low` for scheduled and modified-note reruns (ambient bulk work).
- **Retry:** transient errors retry once. Permanent errors (target tree missing — the user deleted the Evergreen tree while a task was queued) drop the task with a warning, no staging row emitted.
- **Routing:** direct-LLM worker drains (it doesn't actually call the LLM for this task, but the worker is the queue-draining lane). MCP clients also eligible but don't gain anything by draining cosine-only tasks; the in-process worker is faster.
- **Cancellation:** cancel-by-source-path. If the user deletes the source note before the task runs, the task drops cleanly. If the user accepts/rejects a triage row produced by a *previous* match for the same path before the new task runs, the new task still runs (it's a fresh evaluation against the current note state) and emits a new row.


## Lifecycle

```
                ┌───────────┐
                │  Queued   │ ← submit
                └─────┬─────┘
        cancel        │ checkout (worker takes lease)
            ▼         ▼
        ┌─────────┐ ┌────────┐  cancel
        │Cancelled│ │ Leased │ ─────►  Cancelled
        └─────────┘ └───┬────┘         (in-process: stop signal;
                        │               external: lease invalidated)
            heartbeat ──┤
            (extends    │
             lease)     │
                        ▼
              ┌─────────────────┐
              │   Completed /   │ ← submit
              │     Failed      │ ← fail
              └─────────────────┘

       lease expiry → requeue (back to Queued)
```

[task-queue-lifecycle]

States are plain enum values on the in-memory record. Terminal states (Completed / Failed / Cancelled) stay in the queue for a short retention window (default 60s) so the home-page widget can render the "just finished" row before it disappears, then are GC'd. [task-queue-terminal-retention]


## Lease + heartbeat

External workers checkout via MCP and stamp a lease for `lease_secs` (default **60**, configurable per call up to a queue-wide cap of **600**). If no `task_submit` / `task_fail` / `task_heartbeat` arrives within the lease window, the lease expires and the task returns to `Queued` for someone else to pick up. [task-queue-lease-timeout]

`task_heartbeat(task_id)` extends the current lease by another `lease_secs` window — polite long-running external agents call it periodically. In-process workers don't time out or heartbeat: they hold the lease for the natural duration of their work, interruptible only by the synchronous in-process cancel. [tasks-mcp-tool-heartbeat]


## Producer API

In-process callers submit through `core::tasks::Queue`: `submit(task) -> TaskHandle`, `cancel(id)` (app-side only), and `snapshot() -> Vec<TaskRecord>` (for the home-page widget). The `TaskHandle` carries the `id` and a future-shaped `await_outcome(self) -> TaskOutcome`, where `TaskOutcome` is one of `Completed { value }` / `Failed { error }` / `Cancelled { reason: CancelReason }`. [task-queue-submit-handle]

`submit` returns immediately with a handle backed by a tokio oneshot. Producers `.await` for the result; dropping the handle implicitly cancels the task (cheap because cancel is a queue-side mark, not a worker-reaching RPC). UI surfaces that need finer control (a button that doesn't own the handle) call `Queue::cancel(id)` directly.

Fan-out producers submit N tasks and await all handles — `try_join_all` on the bundle, with `Cancelled` outcomes bubbling up if the user cancels mid-batch. The producer aggregates partial results itself; the queue doesn't model "jobs" above the task level (deferred — see below).


## Event stream

Every state transition emits a `QueueEvent` on the queue-events channel: `TaskQueued { id, kind, priority, submitted_at }`, `TaskLeased { id, worker: WorkerKind, lease_expires_at }`, `TaskHeartbeat { id, lease_expires_at }`, `TaskCompleted { id, duration_ms }`, `TaskFailed { id, error_summary }`, `TaskCancelled { id, reason }`. `WorkerKind` is either `DirectLlm` (in-process drain) or `McpClient { client_id, via }` where `via` is `External | InProcessChatAgent`. [task-queue-event-stream]

The home-page widget subscribes to the channel, applies events to a local mirror of the queue snapshot, and re-renders. On widget mount it calls `tasks_snapshot()` once to seed the local mirror; the event stream is the live update path. Same shape as the existing indexer-progress events + initial-status pattern in `vault-home-stats-widget`.

Result *bodies* never travel on the event channel — only summaries. The full result goes back through the producer's handle (in-process) or as the response to the producer's `await_outcome` resolution. The event channel exists for UI awareness, not for delivering payloads.

Fan-out features ride the queue events like every other producer; cancellation of an in-flight fan-out is `tasks_cancel(task_ids)` over the bundle of submitted task ids.


## Cancellation semantics

Cancellation is in-process only — UI cancel buttons and producer drops both call `core::tasks::cancel(id)`, the only path to cancel; there is no MCP cancel tool. [task-queue-cancel-app-only] Behavior depends on the task's current state:

- **Queued** — removed from the queue immediately; producer handle resolves to `Cancelled { user_action }`.
- **Leased to direct-LLM worker** — the worker's `StopSignal` is fired (mirrors the cancel path `core::agent` already uses per `agent-chat-command-surface`). The worker drops its in-flight LLM call, emits no submission, and the queue resolves the handle to `Cancelled`. [task-queue-cancel-propagation-internal]
- **Leased to an MCP client** — external rmcp client *or* the basic chat agent (they're the same lease shape). The lease is marked invalid; the eventual `task_submit` returns `stale_lease`, the client/agent should stop work and not retry. The producer handle resolves to `Cancelled` immediately, without waiting for the client to acknowledge. The chat agent's existing `agent-tool-call-timeout` + Stop button cover the user-side cancel path during a chat turn; the queue-side cancel covers the producer-side path (someone clicked ✕ on the queue widget). [task-queue-stale-lease-rejection]
- **Already terminal** (Completed / Failed / Cancelled) — no-op.

MCP-client workers aren't notified mid-work — there's no server→client cancel push in v1, so they learn at submit time (`task-queue-mcp-cancel-notification`, deferred). The chat-agent path is partly covered by the user's chat-turn Stop button, which aborts the in-flight tool call including a `task_*` one.

Cancellation is **never exposed as an MCP tool** — external agents don't cancel each other's tasks, and the app's cancel doesn't round-trip through MCP. [task-queue-cancel-not-via-mcp]


## Workers

Two consumer lanes: the direct-LLM background worker (the only LLM lane the app actively *drives*, in-process), and MCP clients — external agents over rmcp HTTP plus hiker's own basic chat agent (which dispatches tools through MCP per `agent-tool-routing-via-mcp`, so exposing `task_*` to it is a tool-surface decision, not a separate worker). [task-queue-worker-categories]

### Direct-LLM worker

A tokio task that drains `Direct`-shaped tasks. For each task: build prompt from `task.payload`, call `core::llm::chat` (or the structured-output equivalent when `output_schema` is set), parse and validate the response, complete the task back through the queue. One task at a time per worker instance — concurrency comes from running multiple instances if needed (config: `[tasks] direct_worker.parallelism = 1`). [task-queue-direct-worker]

Toggled by `[tasks] direct_worker.enabled` (default `true`). When false the worker doesn't spawn; `Direct` tasks sit in the queue until an MCP client checks them out, or — if no MCP client ever picks them up — until they're cancelled. [task-queue-direct-worker-toggle]

An `Agent`-shape task with no MCP-client consumer available sits in the queue indefinitely; the home-page widget surfaces this state so the user can cancel.

Structured-output handling: a task's optional `output_schema` is validated on every submission, direct or MCP-client alike. [task-queue-structured-output] When `output_schema` is present, the direct worker prefers provider-side enforcement (the `llm` crate's structured-output API where it exposes one — Anthropic tool-forcing, OpenAI `response_format`). When the provider doesn't support enforcement, the worker appends "Respond strictly as JSON matching this schema: …" to the prompt, parses, and on parse failure retries once with the parse error appended as guidance. Second failure → fail the task with `schema_violation`. [task-queue-structured-output-direct]

### MCP clients

Any rmcp caller that hits `task_checkout` is an MCP-client consumer. Two flavors of caller share this surface:

- **External rmcp clients over HTTP** — Claude Code, Codex, an ACP-driven Goose, anything that's been pointed at hiker's MCP server URL from `vault/.hiker/mcp.json`. The queue doesn't model these as "registered workers" — an agent can come, take a task, and leave at will, identified only by the rmcp client id stamped on the lease. [task-queue-external-mcp-client]
- **The basic chat agent (in-process)** — when `[tasks] expose_to_chat_agent = true`, the queue's `task_*` tools are added to the chat agent's tool set via the existing `McpAgentDispatcher` (`agent-tool-routing-via-mcp`). The user can then ask the in-app chat agent to drain queue work, or the agent can pick up a task on its own initiative during a turn. From the queue's perspective this is just another MCP client; the only difference is the dispatch path is in-process trait calls rather than HTTP. The chat agent's existing iteration cap, tool timeout, and Stop button all work unchanged — when an `task_*` tool call is in flight, those are the user's controls over the work. [task-queue-expose-to-chat-agent]

For both flavors the surface is identical (the `task_checkout` / `task_submit` / `task_fail` / `task_heartbeat` tools below), with the lease stamped against the rmcp client id.

MCP clients handle both `Direct` and `Agent` shapes — the agent on the other end interprets the prompt however it likes (a tool-using agent will call hiker's other MCP tools naturally during its work; a tool-less direct caller just returns text). The `shape` field in the checkout response tells the client whether the task expects tool use; enforcement is at submit time via the schema check. [task-queue-mcp-client-handles-both-shapes]

There is **no** "expose to ACP" mode. ACP is wired only for interactive chat (`llm.md`'s rule). If a user has configured their ACP agent to also act as an rmcp client and pull tasks, that's an external-rmcp-clients scenario from the queue's perspective — the app didn't drive it, the user did. [task-queue-acp-via-mcp-only]


### Worker preference

When both the direct worker and MCP-client consumers are eligible for a task, `[tasks] worker_preference` picks the winner:

- **`internal`** — direct worker grabs eligible `Direct` tasks immediately; MCP `task_checkout` only sees a task if the direct worker is disabled or busy. [task-queue-worker-preference-internal]
- **`external`** — direct worker abstains for a queue-wide grace window (default 5s) before taking a task; MCP `task_checkout` sees newly-queued tasks immediately, and if no MCP client picks one up within the grace, the direct worker takes over. [task-queue-worker-preference-external]
- **`auto`** (default) — short grace window of **1s** before the direct worker grabs: low latency on the common case (no external attached) while letting an attached MCP client win when it polls fast enough. [task-queue-worker-preference-auto]

The grace window is implemented as a per-task `eligible_to_direct_at = submitted_at + grace`. The direct worker's `next_eligible()` query filters on this; MCP `task_checkout` ignores it. The window starts on `Queued`, not on requeue-after-lease-expiry — once a task has been around long enough that the direct worker is eligible, it stays eligible. (Per-task-type override is out of v1 — see Out of scope.)


## MCP tool surface

Added to `mcp.md`'s tool list. All are advertised whenever `[mcp] enabled = true`, regardless of `worker_preference` (the preference only affects who *gets* each task).

- **`task_checkout(types?: TaskKind[], shapes?: TaskShape[], min_priority?: Priority, lease_secs?: number)`** — return the next eligible task (or null if none). Filters: only tasks whose `kind` is in `types` (default: any), whose `shape` is in `shapes` (default: any), and whose priority is at least `min_priority` (default: `Low`). Stamps a lease against the calling rmcp client id. Returns `{ task_id, kind, shape, payload, output_schema?, lease_expires_at }`. [tasks-mcp-tool-checkout]
- **`task_submit(task_id: string, value: json)`** — submit the result. Validates `value` against the task's `output_schema` if any; rejects `schema_violation` on mismatch with the lease retained so the agent can retry. Rejects `stale_lease` if the lease has expired or been invalidated by cancellation. On success, resolves the producer handle and emits `TaskCompleted`. [tasks-mcp-tool-submit]
- **`task_fail(task_id: string, error: string)`** — agent gives up. Emits `TaskFailed`; producer handle resolves to `Failed`. The task is *not* auto-requeued — producers re-submit if they want a retry. [tasks-mcp-tool-fail]
- **`task_heartbeat(task_id: string)`** — extend the current lease by another `lease_secs` window. Returns the new `lease_expires_at`. [tasks-mcp-tool-heartbeat]

Read-only `task_list(states?, types?)` lets external tooling see the queue without taking work. [tasks-mcp-tool-list]

The MCP error model adds two new codes per `mcp.md`'s positive-code series:

- `1006` (`stale_lease`) — submit/fail/heartbeat against a lease that's expired or been cancelled.
- `1007` (`schema_violation`) — submit value didn't match `output_schema`. Lease retained.

[tasks-mcp-error-codes]


## Settings

New section in the standard config (per `settings.md`'s eligibility model):

```toml
[tasks]
worker_preference = "auto"          # "auto" | "internal" | "external"
terminal_retention_secs = 60        # how long terminal rows stay visible

[tasks.direct_worker]
enabled = true
parallelism = 1                     # how many direct-LLM tasks can run concurrently

# Whether the in-process basic chat agent gets the task_* MCP tools advertised
# in its tool set. When true, the user can ask the chat agent to drain queue
# work or the agent can pick up tasks during its turns. When false, the
# task_* tools are simply not in the chat agent's tool registry. External
# rmcp clients over HTTP are unaffected by this setting — they always see the
# task_* tools when [mcp] enabled is true.
expose_to_chat_agent = true

[tasks.lease]
default_secs = 60
max_secs = 600
```

Eligibility: every key in `[tasks]` is per-vault overridable except `[tasks].worker_preference`, which is also valid at user scope. Strict-load + auto-create + `set_setting` write-back behave the same as every other section. [task-queue-settings-section]

Settings UI gets a new "Task queue" section: two independent toggles (`direct_worker.enabled`, `expose_to_chat_agent`), a `worker_preference` radio, and a numeric retention field — all also surfaced inline on the queue detail page so the user can flip them without leaving the queue view. [task-queue-settings-ui-section, task-queue-worker-toggles]

When `[llm] enabled = false` (per `llm-features-disable-entirely`), the `[tasks]` settings still load but the direct worker is force-disabled regardless of `direct_worker.enabled` — no LLM means no in-process worker, by definition. `expose_to_chat_agent` is also moot in that mode because the chat panel itself is hidden. The MCP `task_*` tools stop being advertised since the queue's only purpose is LLM work. The home-page widget renders an empty state. [task-queue-respects-llm-disable]


## Entry points

Two surfaces open the queue detail page:

- **Top-strip Queue button** (`vault-bar-queue-button`, per `editor.md` Top strip) — primary entry point. Always visible in the top strip's leading cluster. Indicator on the icon shows the active count; pulses subtly on `Leased`. Click → opens the shared queue detail page.
- **Home-page Task queue widget** (below) — a tile on the vault home overview that drills into the same detail page, pre-selecting the LLM-tasks filter. The existing "Queued" tile on `vault-home-stats-widget` drills into the same page pre-selecting the Embedding filter.

All three converge on the same shared queue detail surface (`queue-detail-shared-page`).


## Home page widget

A vault-home tile per the existing `editor.md` widget pattern: "Task queue" + active count + a small icon.

- **Tile state** — count of `Queued + Leased` tasks. Empty state ("No tasks queued") when zero. [task-queue-home-widget]
- **Click drills into the detail view** per `vault-home-detail-views`. Detail view body is a list of task rows + a settings strip at the top with the worker toggles + preference radio. [task-queue-home-detail-view]

### Shared queue detail page with the embedding queue

The detail view for this tile and the existing embedding-queue detail (the "Queued" tile from `vault-home-stats-widget`, currently flowing into `vault-home-stats-detail`) render on **the same page** with filter buttons. The two queues stay strictly separate underneath — different producers, different workers, different `core::*` modules (`core::indexer` for embeddings, `core::tasks` for LLM work) — but the UI consolidates them because the user's mental model is one: "what is the app currently working on in the background." [queue-detail-shared-page]

Page shape: a "Background work" panel with the filter pills at top, a worker-toggles + preference-radio strip shown only when the filter includes LLM (the embedding queue has no user-facing toggles), then rows grouped into **Active** / **Queued** / **Recently finished** sections.

Filter pills (two icon-only buttons, multi-select; both active = the previous "All" view):

- **LLM tasks** — robot icon, same glyph as the chat panel's agent indicator. Toggles `core::tasks` rows in/out of the view. [queue-detail-filter-tasks]
- **Embedding** — brain icon, same glyph as the search bar's semantic-search toggle. Toggles `core::indexer` rows in/out of the view. [queue-detail-filter-index]

Each row carries a small badge identifying its source (`task` / `index`); when both pills are active, rows interleave by state-bucket then submitted_at. The two pills can't both be off (clicking the only-active one is a no-op). Drilling in from a tile pre-selects one pill; the user re-enables the other with a click.

Embedding-queue rows render with the same chrome as task rows — priority-pill slot left empty (the indexer has no priorities), state pill driven by indexer-progress events (`queued` / `started` / `finished` / `skipped`), pulsing `…` on `started` rows reusing `tree-row-queued-marker`'s animation. The indexer queue has no per-row cancel; the ✕ button appears only on `core::tasks` rows. [queue-detail-embedding-row-shape]

**Shared** across the two: the row-rendering primitive (priority pill / state pill / pulsing indicator / hover reveal of cancel button), the section grouping (Active / Queued / Recently finished), and the event-driven local-mirror pattern (one snapshot fetch + delta updates from an event channel). The shared row rendering lives in `app/src/panels/queue.rs` (`task_row`), parameterized by source. **Not shared:** the data layer (separate commands, event channels, and `core::*` modules — the indexer is a watcher-driven streaming pipeline with no priorities/leases/schemas, the task queue is producer-pull-and-await), the worker controls (only the LLM queue has any), and the cancellation path. Each queue keeps its own data fetch + event subscription. [queue-detail-shared-row-primitive]

Entering from the original "Queued" tile (`vault-home-stats-widget`) pre-selects the **Embedding** filter; the new "Task queue" tile pre-selects **LLM tasks**. The header pills let the user broaden / swap from either. [vault-home-stats-queued-tile-shared-detail]

Per-task row: `[priority pill] <kind summary> <state> <submitted-rel-time> [✕]` with a `↳ <metadata one-liner>` + `worker: <worker-kind>` sub-line. State rendering rules:

- **Queued** — neutral state pill. Static.
- **Leased** — pill carries a pulsing `…` indicator (three CSS-animated dots, same shape as `chat-panel-thinking-indicator`). Worker label names who's working ("Direct LLM" / "Chat agent" / "External: Claude Code (id: …)"). [task-queue-row-pulsing-leased]
- **Completed / Failed / Cancelled** — terminal pill with appropriate color; the row stays for `terminal_retention_secs` then disappears.

Cancel button: visible on Queued + Leased rows; calls `Queue::cancel` via a command. Terminal rows have no cancel. [task-queue-row-cancel-action]

Sort: by priority tier then submitted_at, matching drain order so the visible order tells the user what comes next.

The widget is hidden entirely (no tile rendered) when LLM is disabled, since the queue is meaningless in that mode. Gated on the same `llm-features-disable-entirely` check the chat panel uses. [task-queue-home-widget-respects-llm-disable]


## Audit log integration

Every terminal task (Completed / Failed / Cancelled) writes one row to `llm-audit-log` per the existing JSONL format. The row carries:

- `surface = "core::tasks"` — distinguishes from `core::llm` / `core::agent` / `core::acp` / `mcp-tool-call`.
- `feature = task.kind` (the enum variant name).
- `worker = WorkerKind` (which worker handled it).
- `priority`, `duration_ms`, `submitted_at`, optional `error`.
- `task_id` so a debugging trail can correlate widget events with audit entries.

The prompt + response are *not* duplicated in the audit row — the underlying worker writes its own through its existing surface; the task row is queue-view metadata, not a second copy. Content-redaction discipline applies (`obs-no-content`); `[llm.audit] log_full_prompt` is honored by the underlying surface. [task-queue-audit-row]


## Out of scope

- **Task persistence across app restarts.** In-memory only; producers awaiting handles get `Cancelled { app_exit }` on shutdown and re-submit on next launch if they care. Adding durability needs a real story for "what does the user see when a task survives a restart" — not load-bearing for v1. [task-queue-in-memory-only]
- **Server→client cancellation push to external workers.** External agents learn cancellation at submit time via `stale_lease`. rmcp-streamable cancellation notifications are the right v2 fix; tracked as `task-queue-mcp-cancel-notification` (deferred).
- **Task grouping / jobs.** Fan-out producers submit N tasks and aggregate themselves; the queue doesn't model "this batch of 50 tasks is one logical job." If the home-page widget grows a "RAPTOR build (50 tasks)" header view, that's a presentation-layer grouping by `metadata.group_id`, not a queue concept. (`task-queue-task-grouping`, deferred.)
- **Per-task-type worker preference.** One global preference for v1. Per-type matrix (e.g. "always external for note-mutation, always internal for auto-tag") is a settings UI elaboration if the global setting proves too coarse. (`task-queue-per-type-preference`, deferred.)
- **Cancellation chaining across producers.** A fan-out producer canceling itself cancels each of its outstanding handles individually; the queue doesn't have a "cancel all tasks tagged X" API. Producers manage their own bundles.
- **Cross-vault queue.** One queue per vault, mirroring every other core module's vault scoping.
- **Quota / rate-limit enforcement at the queue.** Throttling LLM calls is the underlying worker's concern (or the provider's). The queue doesn't model "no more than N direct-LLM calls per minute."


## Forward refs

- `core::tasks` is the implementation home; lands as a sibling to `core::llm` and `core::agent` in the v3.5 milestone. RAPTOR / fan-out features that consume it are post-v3.5 by `design.md`'s build order.
- `mcp.md` adds the `task_*` tool surface alongside this spec; no separate spec doc.
- `settings.md`'s settings UI gets the Task queue section once settings UI lands per its existing milestone.
- `editor.md`'s vault-home widget table gains a row for the queue tile; no shape change to the home-page architecture.
