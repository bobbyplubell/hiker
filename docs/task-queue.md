# Task queue

A unified work queue for non-interactive LLM jobs — note mutations, RAPTOR / fan-out summarization, background single-shots like auto-tag-on-save. Producers submit tasks and await results; consumers (in-process workers and external MCP-attached agents) drain the queue. Replaces the "every feature calls `core::llm` directly" routing in `llm.md` for everything except chat.

The headline decisions:

- **`core::tasks` is the queue module.** A queue + dispatcher in core, sibling to `core::llm` and `core::agent`. Producers (UI mutation actions, RAPTOR build, background save hooks) submit `Task` records; the queue arbitrates who processes each one. Whether a task ends up serviced by `core::llm` direct, by an MCP-attached external agent, or by the in-process chat agent (when given the queue tools) is a runtime choice the queue makes. [task-queue-core-module]
- **Scope is everything non-interactive: mutations, fan-out, background single-shots.** Chat (basic agent loop + ACP) keeps its existing direct path because chat is a streaming session, not a discrete unit of work. Anything else that fires an LLM prompt routes through the queue. [task-queue-scope-non-chat]
- **One app-driven worker — the direct-LLM background drain — plus MCP-client consumers.** The only thing the app actively *drives* against the queue is an in-process worker that pulls `Direct`-shape tasks and runs them through `core::llm::chat`. Everything else that processes tasks is an MCP client: external agents over rmcp HTTP (Claude Code, Codex, an ACP-driven Goose, …) and hiker's own basic chat agent (which already dispatches tools through MCP per `agent-tool-routing-via-mcp`, so exposing `task_*` to it is a tool-surface decision, not a separate background worker). [task-queue-worker-categories]
- **Two independent toggles — direct-worker on/off, expose-to-chat-agent on/off — plus a worker-preference setting.** `[tasks] direct_worker.enabled` controls the background drain. `[tasks] expose_to_chat_agent` controls whether the `task_*` tools are advertised to the basic chat agent's tool set; when on, the user chatting with the in-app agent can ask it to process queue work and the agent can pick up tasks during its turns. `worker_preference = 'auto' | 'internal' | 'external'` arbitrates when both the direct worker and external MCP clients are eligible for the same task — `'external'` makes the direct worker abstain so external agents win; `'auto'` gives external a short grace window before internal takes over. Same toggles available inline on the queue page. [task-queue-worker-toggles]
- **Cancellation is in-process only — never an MCP tool.** UI cancel buttons and producer drops both call `core::tasks::cancel(id)`. Effect by lease holder: in-process worker has its stop signal fired; external MCP worker has its lease invalidated, with the eventual `task_submit` rejected as `stale_lease`. External agents learn cancellation by submit failure rather than push notification (notification deferred). [task-queue-cancel-app-only]
- **Tasks declare their output schema; submissions are validated.** Optional `output_schema` (JSON Schema) on the task. Direct-LLM worker uses provider-side structured-output enforcement when available, else "ask for JSON, validate, retry once" fallback. MCP-client consumers hand back JSON via `task_submit`, which validates against the schema before completing — agent or external alike. [task-queue-structured-output]
- **In-memory only in v1.** The queue does not persist across app restarts. Producers awaiting handles get `Cancelled { app_exit }` on shutdown; they re-submit on next launch if they care. Persistence is deferred until a real loss-on-restart story bites. [task-queue-in-memory-only]
- **One detail page, two queues, filter pills.** The home-page detail view shows both this queue and the existing embedding queue (`core::indexer`) on the same page with two multi-select icon pills — **LLM tasks** (robot icon, matching the chat panel's agent glyph) and **Embedding** (brain icon, matching the search bar's semantic-search glyph). Both default to active, which reproduces the previous "All" view; toggle either off to narrow. The two queues remain strictly separate at the data layer — different producers, different workers, different modules — but the UI shares a row-rendering primitive because the user's mental model is one: "what's the app working on right now." Worker toggles + preference radio appear only when the LLM-tasks pill is on. [queue-detail-shared-page]


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

`core::tasks` owns the queue, the (optional) direct worker handle, the lease table, and the event emitter. `core::llm` is imported by the direct worker; `core::agent` is *not* a queue dependency — the chat agent's interaction with the queue is purely through the MCP tool surface, just like any other client.

The MCP server's `task_*` tools are a thin facade over the queue's checkout/submit primitives — `mcp.md` adds the tool surface, but the policy lives in `core::tasks`. The basic chat agent reaches the same tools through the in-process `McpAgentDispatcher` (`agent-tool-routing-via-mcp`), so the chat-agent path and the external-rmcp-client path share one tool registry, one audit shape, one set of error codes.


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
    RaptorSummarize { cluster_id: ClusterId, level: u8 },
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

`TaskKind` is an exhaustive enum so the worker code, the audit log, the MCP schema, and the home-page widget all share one source of truth for "what kinds of work exist." Adding a new task type is a single enum variant + the producer that submits it; no string-typed dispatch.

`TaskShape` is the worker-routing hint. `Direct` tasks can be drained by either lane — the direct-LLM worker, or any MCP client (chat agent, external rmcp client). `Agent` tasks signal "this needs tool use during processing" and can be drained only by an MCP client (the chat agent on the in-process side, or any tool-using external agent on the rmcp side). The direct-LLM worker skips `Agent` tasks because it can't make tool calls. [task-queue-shape-routing]


## Priority

Three named tiers — `High`, `Normal`, `Low`. Strict ordering: High tasks always drain before Normal, Normal before Low. Within a tier, FIFO by `submitted_at`. [task-queue-priority-tiers]

Why three named tiers rather than an integer: the UI needs a pill per tier, the user reasons in "this is urgent vs. not," and an integer field invites bikeshedding and per-feature drift. Three is enough — `High` for explicit user-initiated foreground work (clicked "rewrite this note"), `Normal` for the default (background save hooks), `Low` for ambient bulk work (RAPTOR's per-cluster summaries during a build).

Producers default to `Normal` and bump up only with a reason. The note-mutation menu submits at `High` because the user is watching; RAPTOR build submits its hundreds of summaries at `Low` so a foreground mutation doesn't queue behind them.


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

`task_heartbeat(task_id)` extends the current lease by another `lease_secs` window. Polite long-running external agents call it periodically; the in-process workers don't bother (they hold the lease until completion or cancellation through direct memory access). [tasks-mcp-tool-heartbeat]

In-process workers don't time out — they hold the lease for the natural duration of their work. Cancellation is the only way to interrupt them, and it's synchronous with the in-process API.


## Producer API

In-process callers submit through `core::tasks::Queue`:

```rust
impl Queue {
    fn submit(&self, task: Task) -> TaskHandle;
    fn cancel(&self, id: TaskId);                  // app-side only
    fn snapshot(&self) -> Vec<TaskRecord>;          // for the home-page widget
}

struct TaskHandle {
    id: TaskId,
    // Future-shaped: poll/await for the terminal outcome.
    fn await_outcome(self) -> impl Future<Output = TaskOutcome>;
}

enum TaskOutcome {
    Completed { value: serde_json::Value },
    Failed    { error: String },
    Cancelled { reason: CancelReason },
}
```

[task-queue-submit-handle]

`submit` returns immediately with a handle backed by a tokio oneshot. Producers `.await` for the result; dropping the handle implicitly cancels the task (cheap because cancel is a queue-side mark, not a worker-reaching RPC). UI surfaces that need finer control (a button that doesn't own the handle) call `Queue::cancel(id)` directly.

Fan-out producers submit N tasks and await all handles — `try_join_all` on the bundle, with `Cancelled` outcomes bubbling up if the user cancels mid-batch. The producer aggregates partial results itself; the queue doesn't model "jobs" above the task level (deferred — see below).


## Event stream

Every state transition emits a `QueueEvent` on the Tauri channel `hiker:queue-event`:

```rust
enum QueueEvent {
    TaskQueued    { id, kind, priority, submitted_at }
    TaskLeased    { id, worker: WorkerKind, lease_expires_at }
    TaskHeartbeat { id, lease_expires_at }
    TaskCompleted { id, duration_ms }
    TaskFailed    { id, error_summary }
    TaskCancelled { id, reason }
}

enum WorkerKind {
    DirectLlm,                                  // in-process background drain
    McpClient { client_id: String,              // any rmcp client
                via: McpClientVia },            //   External | InProcessChatAgent
}
```

[task-queue-event-stream]

The home-page widget subscribes to the channel, applies events to a local mirror of the queue snapshot, and re-renders. On widget mount it calls `tasks_snapshot()` once to seed the local mirror; the event stream is the live update path. Same shape as the existing `hiker:reindex-progress` + initial-status pattern in `vault-home-stats-widget`.

Result *bodies* never travel on the event channel — only summaries. The full result goes back through the producer's handle (in-process) or as the response to the producer's `await_outcome` resolution. The event channel exists for UI awareness, not for delivering payloads.

Fan-out features ride the queue events like every other producer; cancellation of an in-flight fan-out is `tasks_cancel(task_ids)` over the bundle of submitted task ids.


## Cancellation semantics

`Queue::cancel(id)` is the only path to cancel. Behavior depends on the task's current state:

- **Queued** — removed from the queue immediately; producer handle resolves to `Cancelled { user_action }`.
- **Leased to direct-LLM worker** — the worker's `StopSignal` is fired (mirrors the cancel path `core::agent` already uses per `agent-chat-command-surface`). The worker drops its in-flight LLM call, emits no submission, and the queue resolves the handle to `Cancelled`. [task-queue-cancel-propagation-internal]
- **Leased to an MCP client** — external rmcp client *or* the basic chat agent (they're the same lease shape). The lease is marked invalid; the eventual `task_submit` returns `stale_lease`, the client/agent should stop work and not retry. The producer handle resolves to `Cancelled` immediately, without waiting for the client to acknowledge. The chat agent's existing `agent-tool-call-timeout` + Stop button cover the user-side cancel path during a chat turn; the queue-side cancel covers the producer-side path (someone clicked ✕ on the queue widget). [task-queue-stale-lease-rejection]
- **Already terminal** (Completed / Failed / Cancelled) — no-op.

MCP-client workers are *not* notified mid-work that their lease was cancelled — there's no MCP server→client push for cancellation in v1. They learn at submit time. This is a known wart: a Claude Code instance might burn an extra 10 seconds on a task the user already abandoned. The fix is rmcp-streamable cancellation notifications (`task-queue-mcp-cancel-notification`, deferred); for v1 the lease-rejection path is correct, just not optimal. (The chat agent path is partly covered by the user's existing chat-turn Stop button — that fires a turn-level cancel that aborts whatever tool call is in flight, including a `task_*` call.)

Cancellation is **never exposed as an MCP tool.** External agents don't get to cancel each other's tasks, and the app's cancel doesn't need to round-trip through MCP. Keeps the trust model simple. [task-queue-cancel-not-via-mcp]


## Workers

Two consumer lanes: the direct-LLM background worker (in-process, app-driven), and MCP clients (anything that calls `task_checkout`).

### Direct-LLM worker

A tokio task that drains `Direct`-shaped tasks. For each task: build prompt from `task.payload`, call `core::llm::chat` (or the structured-output equivalent when `output_schema` is set), parse and validate the response, complete the task back through the queue. One task at a time per worker instance — concurrency comes from running multiple instances if needed (config: `[tasks] direct_worker.parallelism = 1`). [task-queue-direct-worker]

Toggled by `[tasks] direct_worker.enabled` (default `true`). When false the worker doesn't spawn; `Direct` tasks sit in the queue until an MCP client checks them out, or — if no MCP client ever picks them up — until they're cancelled. [task-queue-direct-worker-toggle]

The direct worker only takes `Direct`-shape tasks. `Agent`-shape tasks need tool-use, which the direct worker doesn't provide — those wait for an MCP-client consumer (the basic chat agent or an external one) to take them. If neither is available the task sits indefinitely; the home-page widget surfaces this state so the user can cancel.

Structured-output handling: when `output_schema` is present, the worker prefers provider-side enforcement (the `llm` crate's structured-output API where it exposes one — Anthropic tool-forcing, OpenAI `response_format`). When the provider doesn't support enforcement, the worker appends "Respond strictly as JSON matching this schema: …" to the prompt, parses, and on parse failure retries once with the parse error appended as guidance. Second failure → fail the task with `schema_violation`. [task-queue-structured-output-direct]

### MCP clients

Any rmcp caller that hits `task_checkout` is an MCP-client consumer. Two flavors of caller share this surface:

- **External rmcp clients over HTTP** — Claude Code, Codex, an ACP-driven Goose, anything that's been pointed at hiker's MCP server URL from `vault/.hiker/mcp.json`. The queue doesn't model these as "registered workers" — an agent can come, take a task, and leave at will, identified only by the rmcp client id stamped on the lease. [task-queue-external-mcp-client]
- **The basic chat agent (in-process)** — when `[tasks] expose_to_chat_agent = true`, the queue's `task_*` tools are added to the chat agent's tool set via the existing `McpAgentDispatcher` (`agent-tool-routing-via-mcp`). The user can then ask the in-app chat agent to drain queue work, or the agent can pick up a task on its own initiative during a turn. From the queue's perspective this is just another MCP client; the only difference is the dispatch path is in-process trait calls rather than HTTP. The chat agent's existing iteration cap, tool timeout, and Stop button all work unchanged — when an `task_*` tool call is in flight, those are the user's controls over the work. [task-queue-expose-to-chat-agent]

For both flavors the surface is identical: `task_checkout` returns the next eligible task (after worker-preference arbitration) and stamps a lease against the rmcp client id. `task_submit` writes the result, validates against `output_schema` if any, resolves the producer handle, and emits `TaskCompleted`. `task_fail` emits `TaskFailed` with the client's error string. `task_heartbeat` extends the lease.

MCP clients handle both `Direct` and `Agent` shapes — the agent on the other end interprets the prompt however it likes (a tool-using agent will call hiker's other MCP tools naturally during its work; a tool-less direct caller just returns text). The `shape` field in the checkout response tells the client whether the task expects tool use; enforcement is at submit time via the schema check. [task-queue-mcp-client-handles-both-shapes]

There is **no** "expose to ACP" mode. ACP is wired only for interactive chat (`llm.md`'s rule). If a user has configured their ACP agent to also act as an rmcp client and pull tasks, that's an external-rmcp-clients scenario from the queue's perspective — the app didn't drive it, the user did. [task-queue-acp-via-mcp-only]


### Worker preference

When both the direct worker and MCP-client consumers are eligible for a task, `[tasks] worker_preference` picks the winner:

- **`internal`** — direct worker grabs eligible `Direct` tasks immediately; MCP `task_checkout` only sees a task if the direct worker is disabled or busy. Useful when the user trusts the configured `[llm]` provider and wants the queue serviced predictably without external agents ever drawing from it. [task-queue-worker-preference-internal]
- **`external`** — direct worker abstains for a queue-wide grace window (default 5s) before taking a task. MCP `task_checkout` sees newly-queued tasks immediately; if no MCP client picks one up within the grace, the direct worker takes over. The setting "the user has Claude Code attached and wants it to do the work, but don't drop tasks if Claude Code goes away." [task-queue-worker-preference-external]
- **`auto`** (default) — short grace window of **1s** before the direct worker grabs. Optimizes for latency on the common case (no external attached → 1s delay on background work is fine) while letting an attached MCP client win when it polls fast enough. [task-queue-worker-preference-auto]

The grace window is implemented as a per-task `eligible_to_direct_at = submitted_at + grace`. The direct worker's `next_eligible()` query filters on this; MCP `task_checkout` ignores it. The window starts on `Queued`, not on requeue-after-lease-expiry — once a task has been around long enough that the direct worker is eligible, it stays eligible.

Per-task-type override is not in v1. The single global preference covers the load-bearing cases; per-type defaults can be added later as a setting matrix.


## MCP tool surface

Added to `mcp.md`'s tool list. All four are advertised whenever `[mcp] enabled = true` regardless of `worker_preference` — exposing the tools is what lets external agents *exist* as workers; the preference setting only affects who actually gets each task.

- **`task_checkout(types?: TaskKind[], shapes?: TaskShape[], min_priority?: Priority, lease_secs?: number)`** — return the next eligible task (or null if none). Filters: only tasks whose `kind` is in `types` (default: any), whose `shape` is in `shapes` (default: any), and whose priority is at least `min_priority` (default: `Low`). Stamps a lease against the calling rmcp client id. Returns `{ task_id, kind, shape, payload, output_schema?, lease_expires_at }`. [tasks-mcp-tool-checkout]
- **`task_submit(task_id: string, value: json)`** — submit the result. Validates `value` against the task's `output_schema` if any; rejects `schema_violation` on mismatch with the lease retained so the agent can retry. Rejects `stale_lease` if the lease has expired or been invalidated by cancellation. On success, resolves the producer handle and emits `TaskCompleted`. [tasks-mcp-tool-submit]
- **`task_fail(task_id: string, error: string)`** — agent gives up. Emits `TaskFailed`; producer handle resolves to `Failed`. The task is *not* automatically requeued — failures are real, not retry signals; producers re-submit if they want a retry. [tasks-mcp-tool-fail]
- **`task_heartbeat(task_id: string)`** — extend the current lease by another `lease_secs` window. Returns the new `lease_expires_at`. [tasks-mcp-tool-heartbeat]

Read-only inspection — `task_list(states?, types?)` — lets external tooling see the queue without taking work. Useful for "queue length" dashboards and for an external agent deciding whether it's worth checking in. [tasks-mcp-tool-list]

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

Settings UI gets a new "Task queue" section. Toggle for `direct_worker.enabled`, toggle for `expose_to_chat_agent`, radio for `worker_preference`, numeric for retention. Same toggles surface inline on the queue detail page so the user can flip them without leaving the queue view. [task-queue-settings-ui-section]

When `[llm] enabled = false` (per `llm-features-disable-entirely`), the `[tasks]` settings still load but the direct worker is force-disabled regardless of `direct_worker.enabled` — no LLM means no in-process worker, by definition. `expose_to_chat_agent` is also moot in that mode because the chat panel itself is hidden. The MCP `task_*` tools stop being advertised since the queue's only purpose is LLM work. The home-page widget renders an empty state. [task-queue-respects-llm-disable]


## Entry points

Two surfaces open the queue detail page:

- **Top-strip Queue button** (`vault-bar-queue-button`, per `editor.md` Top strip) — primary entry point. Always visible in the top strip's leading cluster. Indicator on the icon shows the active count; pulses subtly on `Leased`. Click → opens the shared queue detail page.
- **Home-page Task queue widget** (below) — a tile on the vault home overview that drills into the same detail page, pre-selecting the LLM-tasks filter. The existing "Queued" tile on `vault-home-stats-widget` drills into the same page pre-selecting the Embedding filter.

All three converge on the same shared queue detail surface (`queue-detail-shared-page`).


## Home page widget

A vault-home tile per the existing `editor.md` widget pattern: tile shows "Task queue" + active count + a small icon. Subscribes to `hiker:queue-event` with a one-time `tasks_snapshot()` seed at mount.

- **Tile state** — count of `Queued + Leased` tasks. Empty state ("No tasks queued") when zero. [task-queue-home-widget]
- **Click drills into the detail view** per `vault-home-detail-views`. Detail view body is a list of task rows + a settings strip at the top with the worker toggles + preference radio. [task-queue-home-detail-view]

### Shared queue detail page with the embedding queue

The detail view for this tile and the existing embedding-queue detail (the "Queued" tile from `vault-home-stats-widget`, currently flowing into `vault-home-stats-detail`) render on **the same page** with filter buttons. The two queues stay strictly separate underneath — different producers, different workers, different `core::*` modules (`core::indexer` for embeddings, `core::tasks` for LLM work) — but the UI consolidates them because the user's mental model is one: "what is the app currently working on in the background." [queue-detail-shared-page]

Page shape:

```
┌─ Background work ─────────────────────────────────────────┐
│  [ All ] [ LLM tasks ] [ Embedding ]      ◄ filter pills  │
│                                                           │
│  [worker toggles + preference radio    ] ◄ shown when     │
│   shown only when "All" or "LLM tasks"   the filter       │
│   is selected; embedding queue has no    includes LLM     │
│   user-facing toggles                                                              │
│                                                           │
│  ── Active ──                                             │
│  [High] rewrite-as-markdown   notes/draft.md   …pulsing   │
│         worker: Direct LLM                       [✕]      │
│  [—   ] indexing              notes/foo.md     …pulsing   │
│                                                                                    │
│  ── Queued ──                                             │
│  [Norm] auto-tag              inbox/idea.md      [✕]      │
│  [—   ] queued (3 more)       …                           │
│                                                                                    │
│  ── Recently finished ──                                  │
│  [✓] raptor-summarize  cluster #12   2s ago               │
└───────────────────────────────────────────────────────────┘
```

Filter pills (two icon-only buttons, multi-select; both active = the previous "All" view):

- **LLM tasks** — robot icon, same glyph as the chat panel's agent indicator. Toggles `core::tasks` rows in/out of the view. [queue-detail-filter-tasks]
- **Embedding** — brain icon, same glyph as the search bar's semantic-search toggle. Toggles `core::indexer` rows in/out of the view. [queue-detail-filter-index]

Each row carries a small badge identifying its source (`task` / `index`); when both pills are active, rows interleave by state-bucket then submitted_at. The pills can't both be off at the same time — clicking the only-active pill is a no-op (an empty page would have no recovery affordance). Drilling in from a tile pre-selects exactly one pill (the LLM-tasks tile and the Queued embedding tile each set their own); the user can re-enable the other with a single click. The original three-pill design (`All` / `LLM tasks` / `Embedding`) is retired: the "All" pill collapses to "both pills on," which is what the multi-select default already gives.

Embedding-queue rows render with the same chrome as task rows — priority-pill slot left empty (the indexer doesn't have priorities), state pill driven by `hiker:reindex-progress` (`queued` / `started` / `finished` / `skipped`), pulsing `…` indicator on `started` rows reusing `tree-row-queued-marker`'s animation. The indexer queue has no per-row cancel — cancelling an embedding job individually isn't supported and isn't being added with this work. The ✕ button appears only on rows from `core::tasks`. [queue-detail-embedding-row-shape]

Code shared across the two: the row-rendering primitive (priority pill / state pill / pulsing indicator / hover reveal of cancel button), the section grouping (Active / Queued / Recently finished), and the event-driven local-mirror pattern (one snapshot fetch + delta updates from a Tauri event channel). The shared module lives in `ui/src/queueDetail/index.ts` (new) and exports a `<QueueRow>` primitive parameterized by source. Each queue keeps its own data fetch + event subscription. [queue-detail-shared-row-primitive]

Code *not* shared: the data layer (separate Tauri commands, separate event channels, separate `core::*` modules), the worker controls (only the LLM queue has any), and the cancellation path. The UI is the one thing the user sees as unified; everything below the rendering layer remains decoupled.

Why the embedding queue stays its own queue: scheduling, durability, and producer model differ. The embedder queue is driven by the watcher and is essentially a streaming pipeline; the task queue is producer-pull-and-await. Conflating them at the data layer would mean either dragging LLM-task semantics (priorities, leases, schemas) into the indexer (which has none of them), or stripping them out of the task queue (defeats the point). The shared UI gives the user the consolidated view without the wrong-shape coupling underneath.

The original "Queued" tile on `vault-home-stats-widget` still exists and still drills into this same shared detail page, with the **Embedding** filter pre-selected when entered from that tile. Conversely, the new "Task queue" tile drills in with **LLM tasks** pre-selected. The header pills let the user broaden / swap views without leaving the page. [vault-home-stats-queued-tile-shared-detail]

Detail view row layout, per task:

```
[priority pill]  <kind summary>      <state>  <submitted-rel-time>   [✕ cancel]
                 ↳ <metadata one-liner>       worker: <worker-kind>
```

State rendering rules:

- **Queued** — neutral state pill. Static.
- **Leased** — pill carries a pulsing `…` indicator (three CSS-animated dots, same shape as `chat-panel-thinking-indicator`). Worker label tells the user who's working ("Direct LLM" / "Chat agent" / "External: Claude Code (id: …)"). [task-queue-row-pulsing-leased]
- **Completed / Failed / Cancelled** — terminal pill with appropriate color; the row stays for `terminal_retention_secs` then disappears.

Cancel button: visible on Queued + Leased rows; calls `Queue::cancel` via a Tauri command. Terminal rows have no cancel. [task-queue-row-cancel-action]

Sort: by priority tier then by submitted_at (matches the queue's drain order so the visible order tells the user "what comes next"). Reverse-chronological grouping isn't right here — the user wants to see what's about to happen, not the freshest submit.

The widget is hidden entirely (no tile rendered) when LLM is disabled, since the queue is meaningless in that mode. Gated on the same `llm-features-disable-entirely` check the chat panel uses. [task-queue-home-widget-respects-llm-disable]


## Audit log integration

Every terminal task (Completed / Failed / Cancelled) writes one row to `llm-audit-log` per the existing JSONL format. The row carries:

- `surface = "core::tasks"` — distinguishes from `core::llm` / `core::agent` / `core::acp` / `mcp-tool-call`.
- `feature = task.kind` (the enum variant name).
- `worker = WorkerKind` (which worker handled it).
- `priority`, `duration_ms`, `submitted_at`, optional `error`.
- `task_id` so a debugging trail can correlate widget events with audit entries.

The actual prompt + response are *not* duplicated in the audit row — the underlying worker (direct-LLM, agent, external) writes its own audit row through its existing surface. The task row is metadata about the queue's view of the work, not a second copy of the prompt. Same content-redaction discipline applies (`obs-no-content`); `[llm.audit] log_full_prompt` is honored by the underlying surface. [task-queue-audit-row]


## Out of scope

- **Task persistence across app restarts.** In-memory only; producers re-submit on next launch if they care. Adding durability needs a real story for "what does the user see when a task survives a restart" — not load-bearing for v1.
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
