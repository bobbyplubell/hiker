# Task queue

A unified work queue for non-interactive jobs — LLM work (note mutations, RAPTOR / fan-out summarization, background single-shots like auto-tag-on-save) and other long-running background I/O. Producers submit tasks and await results; consumers (the in-process direct-LLM worker and external MCP-attached agents) drain the queue. Replaces the "every feature calls `core::llm` directly" routing in `llm.md`, and gives non-LLM background work the same progress/cancel/visibility surface.

`core::tasks` is the queue + dispatcher module, sibling to `core::llm`. Producers submit `Task` records; the queue arbitrates at runtime who processes each — the in-process direct-LLM worker, or an external MCP-attached agent (Claude Code, Codex, …) that has pointed itself at hiker's MCP server. [task-queue-core-module]
status:: done
touches:: [[code:hiker/tasks]]
note:: new `core::tasks` module (queue + lease table + event emitter) wired into `VaultSession` at the host · evidence: `core/src/tasks.rs` (`Queue`, `Slot`, broadcast event channel, lease table)

**Scope is everything non-interactive** — mutations, fan-out, background single-shots. (There is no in-app chat after the rework; interactive use is an external agent over MCP, which is itself just another queue client.) Anything that fires an LLM prompt for non-interactive work routes through the queue. [task-queue-scope-non-chat]
status:: partial
note:: note-mutation menu (the first non-chat producer) now flows through the queue; RAPTOR / auto-tag / summary-on-save producers are still planned, so the slug stays partial until they land · evidence: `core/src/tasks.rs` (`TaskKind` enum); `submit_note_mutation` producer in the host

**Non-LLM I/O lane.** I/O-bound work (a web crawl) drains on a dedicated in-process worker — not the single-shot direct-LLM drain, and not the synchronous `NonLlmHandlers` side-channel (a crawl runs for minutes with concurrent fetches). It's never an MCP client, carries no `output_schema`, and lives on the queue purely for its lease/progress/cancel/visibility surface. A crawl's per-page extractions roll up under the parent crawl via [[spec:task-queue-task-grouping]] so the widget shows one row, not N. [task-queue-io-worker-lane, crawl-task-queue-lane]


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
  (background tokio task,                 (rmcp surface over the queue's
   drains Direct-shape tasks               checkout/submit primitives;
   via core::llm::chat;                    external rmcp clients drain it)
   on/off via setting)
                                                   │
                                  ┌────────────────┘
                                  │
                                  ▼
                           external HTTP rmcp clients
                           (Claude Code, Codex, an
                           ACP-driven Goose pointed
                           at hiker's MCP server, …)

   each consumer's terminal action (direct worker completes, MCP submit/fail,
   internal cancel) resolves the producer's handle and emits a queue event
   for the home-page widget.
```

`core::tasks` owns the queue, the (optional) direct worker handle, the lease table, and the event emitter. `core::llm` is imported by the direct worker. The MCP server's `task_*` tools are a thin facade over the queue's checkout/submit primitives (policy lives in `core::tasks`); external rmcp clients drain the queue through them. There is no in-process chat agent (the in-app chat / agent loop was removed).


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
    Agent,    // expects an external MCP client that can call tools mid-task
}

enum Priority { High, Normal, Low }
```

[task-queue-task-shape]
status:: done
touches:: [[code:hiker/tasks]]
note:: exhaustive `TaskKind` enum + `TaskShape::{Direct, Agent}` + `Priority::{Low, Normal, High}` · evidence: `core/src/tasks.rs` (`Task`, `TaskKind`, `TaskShape`, `Priority`, `TaskPayload`)
implements:: [[code:hiker/tasks/types/impl#[TaskKind]variant_name]]

`TaskKind` is an exhaustive enum — one source of truth across worker code, audit log, MCP schema, and the home-page widget; adding a task type is one variant + its producer, no string-typed dispatch.

`TaskShape` is the worker-routing hint. `Direct` tasks drain on either lane (direct-LLM worker or any MCP client); `Agent` tasks need tool use mid-processing and drain only on an MCP client — the direct-LLM worker skips them because it can't make tool calls. [task-queue-shape-routing]
status:: done
touches:: [[code:hiker/tasks]]
note:: direct worker only takes `Direct`; MCP clients can take either shape · evidence: `core/src/tasks.rs::pick_next` (skips `Agent` shape for direct lane), `Queue::checkout_mcp` (no shape filter beyond opt-in)


## Priority

Three named tiers — `High`, `Normal`, `Low`. Strict ordering: High drains before Normal, Normal before Low; within a tier, FIFO by `submitted_at`. [task-queue-priority-tiers]
status:: done
touches:: [[code:hiker/tasks]]
note:: strict High/Normal/Low ordering with FIFO inside via `submitted_at` · evidence: `core/src/tasks.rs::Priority::rank`, `pick_next` ordering
implements:: [[code:hiker/tasks/queue/impl#[Queue]pick_next]], [[code:hiker/tasks/types/impl#[Priority]rank]]

Producers default to `Normal` and bump only with a reason: `High` for user-initiated foreground work (the note-mutation menu, because the user is watching), `Low` for ambient bulk work (RAPTOR's hundreds of per-cluster summaries, so they don't block a foreground mutation).


## Cluster-tree task types

Two task types are produced by the cluster-editor / triage pipeline (per `clustering.md`, `cluster-editor.md`). Both are `TaskShape::Direct` — they don't need tool calls during execution.

### `RaptorSummarize` [task-queue-raptor-summarize]
status:: partial
note:: Enum variant + payload (Sprint A) plus a producer side (Sprint D): `cluster_regenerate_names` submits one task per non-user-edited cluster node at `Priority::Normal`. **Gap:** no concrete LLM-backed worker that consumes the task yet — the direct-LLM drain receives it but the worker that resolves members + writes the summary back through `Trees::set_summary` + `reset_churn` is the next chunk. Retry rides producer-side error handling

Per-cluster LLM call during a tree build pass (one task per cluster per level), or on-demand regeneration triggered from the cluster editor ([[spec:cluster-editor-regenerate-via-task-queue]]).

- **Payload:** `tree_id`, `cluster_node_id`, `level`, member titles + summaries (read by `core::cluster` at task-construction time and passed inline). Output schema enforces `{ name: string, summary: string, confidence: f32 }`.
- **Priority:** `Low` during initial build (large fan-outs shouldn't block foreground work). `Normal` for user-triggered regenerations (user is watching).
- **Retry:** one retry on transient LLM error; mark the cluster as "summarization failed, falling back" and run the tf-idf template path (`cluster-summarize-fallback-tfidf`) on second failure.
- **Routing:** direct-LLM worker drains by default; any MCP client can also drain.
- **Sample-and-merge variant.** For clusters with > 30 members (`raptor-summarize-sample-merge-threshold`, configurable), the producer splits into batches and submits them as sibling tasks plus a fan-in merge task that depends on them. The merge task carries the partial summaries as inputs and produces the final cluster summary. Fan-in coordination uses the queue's standard dependency mechanism (per `task-queue-dependencies`). Capped at 300 members per cluster — beyond that, fall back to the template path. [raptor-summarize-sample-merge]
status:: partial
implements:: [[code:hiker/cluster/SampleMergePlan]]
note:: pure planner: members partitioned into 30-per-batch siblings above the threshold, with a 300-member cap that routes to `TooLarge` (producer skips summarization for that cluster — no fallback path). Constants `SAMPLE_MERGE_BATCH_THRESHOLD` / `BATCH_SIZE` / `MEMBER_CAP` exported for producer reuse. **Gap:** no fan-in machinery in `core::tasks` yet — sibling batches submit independently; the merge step is the producer's responsibility (matches [[spec:task-queue-task-grouping]] being deferred). When the producer lands, each batch is a `RaptorSummarize` task; the merge is a separate `RaptorSummarize` whose payload aggregates the partial summaries · evidence: `core/src/cluster.rs::plan_sample_merge` + `SampleMergePlan { Single, SampleAndMerge { batches }, TooLarge { member_count } }`

### `RaptorTriageMatch` [task-queue-raptor-triage-match]
status:: done
implements:: [[code:hiker/tasks/handlers/NonLlm]], [[code:hiker/tasks/handlers/run_direct_worker]]
note:: Enum variant + payload (Sprint A), producer side (Sprint D Phase 1) — `cluster_triage_enqueue` submits one `RaptorTriageMatch` task per saved-as-triage tree at `Priority::Normal` — and consumer side (Sprint D fix-up): the direct worker checks a `NonLlmHandlers` side-channel before falling back to the LLM (`core/src/tasks.rs::NonLlmHandlers::try_handle` + `drive_one_task`). `the host's `DirectWorkerHandlers`` implements the trait and dispatches `RaptorTriageMatch` to `core::suggest::triage_match` with the session-scoped trees / vault / staging / store handles and the live `[suggestions.triage]` config — same classifier the synchronous on-save fast path uses. Payload schema carries `tree_id` + `source_path` on the variant itself (no prompt body required). Author class is hardcoded `User` for now — see [[spec:triage-author-class]] (partial) for the agent-author lift. Cancel-by-source-path remains a producer-side concern

Per-note classifier run against a saved Evergreen tree. Triggered on the three triage pathways ([[spec:cluster-editor-triage-on-save]], [[spec:cluster-editor-triage-scheduled-rerun]], [[spec:cluster-editor-triage-modified-rerun]]).

- **Payload:** `tree_id`, `source_path`. The worker reads the note's embedding from `index.db` and the saved tree's centroids from `index.db`'s `cluster_centroids` table, runs the beam-descent classifier ([[spec:cluster-place-beam-descent]]), produces a `PlacementMatch { leaf_node_id, confidence, margin }`, resolves the matched node's policy, and emits the corresponding pending op into the op log (per [[spec:triage-staging-proposals]]). No LLM call at all — the entire task is cosine arithmetic + an op append.
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
status:: done
touches:: [[code:hiker/tasks]]
note:: `Queued → Leased → Completed/Failed/Cancelled` with expiry-requeue path · evidence: `core/src/tasks.rs::TaskState`, `tick_maintenance` (lease-expiry requeue)

States are plain enum values on the in-memory record. Terminal states (Completed / Failed / Cancelled) stay in the queue for a short retention window (default 60s) so the home-page widget can render the "just finished" row before it disappears, then are GC'd. [task-queue-terminal-retention]
status:: done
touches:: [[code:hiker/tasks]]
note:: terminal rows kept for `terminal_retention_secs` (default 60) then GC'd · evidence: `core/src/tasks.rs::tick_maintenance` (GC past `terminal_retention_secs`); ticked every 2s from the host


## Lease + heartbeat

External workers checkout via MCP and stamp a lease for `lease_secs` (default **60**, configurable per call up to a queue-wide cap of **600**). If no `task_submit` / `task_fail` / `task_heartbeat` arrives within the lease window, the lease expires and the task returns to `Queued` for someone else to pick up. [task-queue-lease-timeout]
status:: done
touches:: [[code:hiker/tasks]]
note:: external checkout stamps a lease + heartbeat extends; expiry requeues · evidence: `core/src/tasks.rs::checkout_mcp` (clamps to `[1, max_secs]`), `heartbeat` extends, `tick_maintenance` requeues

`task_heartbeat(task_id)` extends the current lease by another `lease_secs` window — polite long-running external agents call it periodically. In-process workers don't time out or heartbeat: they hold the lease for the natural duration of their work, interruptible only by the synchronous in-process cancel. [tasks-mcp-tool-heartbeat]
status:: done
touches:: [[code:hiker/handler]]
note:: returns the new `lease_expires_at_ms` · evidence: `mcp-server/src/handler.rs::task_heartbeat_inner`


## Producer API

In-process callers submit through `core::tasks::Queue`: `submit(task) -> TaskHandle`, `cancel(id)` (app-side only), and `snapshot() -> Vec<TaskRecord>` (for the home-page widget). The `TaskHandle` carries the `id` and a future-shaped `await_outcome(self) -> TaskOutcome`, where `TaskOutcome` is one of `Completed { value }` / `Failed { error }` / `Cancelled { reason: CancelReason }`. [task-queue-submit-handle]
status:: done
touches:: [[code:hiker/tasks]]
note:: producer gets a handle awaitable for `TaskOutcome`; drop is *not* implicit cancel (deviates from spec — see notes) · evidence: `core/src/tasks.rs::TaskHandle::await_outcome`
implements:: [[code:hiker/tasks/queue/impl#[Queue]submit]], [[code:hiker/tasks/queue/impl#[Queue]snapshot]], [[code:hiker/tasks/types/impl#[TaskHandle]await_outcome]]

`submit` returns immediately with a handle backed by a tokio oneshot. Producers `.await` for the result; dropping the handle implicitly cancels the task (cheap because cancel is a queue-side mark, not a worker-reaching RPC). UI surfaces that need finer control (a button that doesn't own the handle) call `Queue::cancel(id)` directly.

Fan-out producers submit N tasks and await all handles — `try_join_all` on the bundle, with `Cancelled` outcomes bubbling up if the user cancels mid-batch. The producer aggregates partial results itself; the queue doesn't model "jobs" above the task level (deferred — see below).


## Event stream

Every state transition emits a `QueueEvent` on the queue-events channel: `TaskQueued { id, kind, priority, submitted_at }`, `TaskLeased { id, worker: WorkerKind, lease_expires_at }`, `TaskHeartbeat { id, lease_expires_at }`, `TaskCompleted { id, duration_ms }`, `TaskFailed { id, error_summary }`, `TaskCancelled { id, reason }`. `WorkerKind` is either `DirectLlm` (in-process drain) or `McpClient { client_id, via }` where `via` is `External | InProcessChatAgent`. [task-queue-event-stream]
status:: done
touches:: [[code:hiker/tasks]]
note:: full event lifecycle on a single queue events channel · evidence: `core/src/tasks.rs::QueueEvent`; emitted onto queue events from the host (broadcast→host bridge)
implements:: [[code:hiker/tasks/queue/impl#[Queue]subscribe]]

The home-page widget subscribes to the channel, applies events to a local mirror of the queue snapshot, and re-renders. On widget mount it calls `tasks_snapshot()` once to seed the local mirror; the event stream is the live update path. Same shape as the existing indexer-progress events + initial-status pattern in [[spec:vault-home-stats-widget]].

Result *bodies* never travel on the event channel — only summaries. The full result goes back through the producer's handle (in-process) or as the response to the producer's `await_outcome` resolution. The event channel exists for UI awareness, not for delivering payloads.

Fan-out features ride the queue events like every other producer; cancellation of an in-flight fan-out is `tasks_cancel(task_ids)` over the bundle of submitted task ids.


## Cancellation semantics

Cancellation is in-process only — UI cancel buttons and producer drops both call `core::tasks::cancel(id)`, the only path to cancel; there is no MCP cancel tool. [task-queue-cancel-app-only] Behavior depends on the task's current state:
status:: done
touches:: [[code:hiker/tasks]]
note:: in-process function only; not exposed as an MCP tool · evidence: `core/src/tasks.rs::Queue::cancel` + `tasks_cancel` command (the host)
implements:: [[code:hiker/tasks/queue/impl#[Queue]cancel]]

- **Queued** — removed from the queue immediately; producer handle resolves to `Cancelled { user_action }`.
- **Leased to direct-LLM worker** — the worker's `StopSignal` is fired. The worker drops its in-flight LLM call, emits no submission, and the queue resolves the handle to `Cancelled`. [task-queue-cancel-propagation-internal]
status:: done
touches:: [[code:hiker/tasks]]
note:: `core/src/tasks.rs::Queue::cancel` fires the leased `StopSignal`; the direct worker observes via `tokio::select!` in `call_with_cancel`
- **Leased to an MCP client** — an external rmcp client. The lease is marked invalid; the eventual `task_submit` returns `stale_lease`, so the client should stop work and not retry. The producer handle resolves to `Cancelled` immediately, without waiting for the client to acknowledge. The queue-side cancel is the producer-side path (someone clicked ✕ on the queue widget); the external agent has its own client-side controls. [task-queue-stale-lease-rejection]
status:: done
touches:: [[code:hiker/tasks]]
note:: producer handle resolves immediately; client learns at submit time · evidence: `core/src/tasks.rs::Queue::cancel` flips state to `Cancelled` while resolving the producer handle; subsequent `submit_result`/`fail` return `QueueError::StaleLease`; MCP layer maps that to error code `1006`
- **Already terminal** (Completed / Failed / Cancelled) — no-op.

MCP-client workers aren't notified mid-work — there's no server→client cancel push in v1, so they learn at submit time ([[spec:task-queue-mcp-cancel-notification]], deferred). The chat-agent path is partly covered by the user's chat-turn Stop button, which aborts the in-flight tool call including a `task_*` one.

Cancellation is **never exposed as an MCP tool** — external agents don't cancel each other's tasks, and the app's cancel doesn't round-trip through MCP. [task-queue-cancel-not-via-mcp]
status:: done
touches:: [[code:hiker/handler]]
note:: cancel intentionally not in the MCP surface · evidence: `mcp-server/src/handler.rs` — no `task_cancel` tool advertised


## Workers

Two consumer lanes: the direct-LLM background worker (the only LLM lane the app actively *drives*, in-process), and MCP clients — external agents over rmcp HTTP that have pointed themselves at hiker's MCP server. (There is no in-process chat-agent lane after the rework.) [task-queue-worker-categories]

### Direct-LLM worker

A tokio task that drains `Direct`-shaped tasks. For each task: build prompt from `task.payload`, call `core::llm::chat` (or the structured-output equivalent when `output_schema` is set), parse and validate the response, complete the task back through the queue. One task at a time per worker instance — concurrency comes from running multiple instances if needed (config: `[tasks] direct_worker.parallelism = 1`). [task-queue-direct-worker]
status:: done
implements:: [[code:hiker/tasks/handlers/run_direct_worker]]
note:: tokio task drains `Direct`-shape tasks via `core::llm::chat` · evidence: `core/src/tasks.rs::run_direct_worker`; spawned per-parallelism in the host
implements:: [[code:hiker/tasks/handlers/impl#[DirectWorker]run]], [[code:hiker/tasks/handlers/impl#[DirectWorker]process]], [[code:hiker/tasks/queue/impl#[Queue]checkout_direct]]

Toggled by `[tasks] direct_worker.enabled` (default `true`). When false the worker doesn't spawn; `Direct` tasks sit in the queue until an MCP client checks them out, or — if no MCP client ever picks them up — until they're cancelled. [task-queue-direct-worker-toggle]
status:: done
implements:: [[code:hiker/tasks/handlers/run_direct_worker]]
note:: `[tasks] direct_worker.enabled` (default true) · evidence: `core/src/config.rs::DirectWorkerConfig::enabled`; gate at the host (only spawns when enabled + `[llm] enabled`)

An `Agent`-shape task with no MCP-client consumer available sits in the queue indefinitely; the home-page widget surfaces this state so the user can cancel.

Structured-output handling: a task's optional `output_schema` is validated on every submission, direct or MCP-client alike. [task-queue-structured-output] When `output_schema` is present, the direct worker prefers provider-side enforcement (the `llm` crate's structured-output API where it exposes one — Anthropic tool-forcing, OpenAI `response_format`). When the provider doesn't support enforcement, the worker appends "Respond strictly as JSON matching this schema: …" to the prompt, parses, and on parse failure retries once with the parse error appended as guidance. Second failure → fail the task with `schema_violation`. [task-queue-structured-output-direct]
status:: done
touches:: [[code:hiker/tasks]]
note:: optional `output_schema`; lightweight schema validator (top-level `type` + `required` + nested `properties`) — full Draft-2020 deferred until a producer needs it · evidence: `core/src/tasks.rs::validate_against_schema` (subset JSON Schema validator); MCP `task_submit_inner` validates before completing

[task-queue-structured-output-direct]
status:: done
implements:: [[code:hiker/tasks/handlers/run_direct_worker]]
note:: provider-side enforcement deferred (graniet/`llm` doesn't expose a unified API); v1 fallback path is the spec's "ask for JSON, validate, retry once" · evidence: `core/src/tasks.rs::drive_one_task` (parse-and-retry-once with parse error appended; second failure → `schema_violation`)
implements:: [[code:hiker/tasks/handlers/impl#[DirectWorker]drive_one_task]]

### MCP clients

Any rmcp caller that hits `task_checkout` is an MCP-client consumer.

- **External rmcp clients over HTTP** — Claude Code, Codex, an ACP-driven Goose, anything that's been pointed at hiker's MCP server URL from `vault/.hiker/mcp.json`. The queue doesn't model these as "registered workers" — an agent can come, take a task, and leave at will, identified only by the rmcp client id stamped on the lease. After the rework this is the **only** MCP-client consumer; there is no in-app chat agent to dispatch in-process. [task-queue-external-mcp-client]
status:: done
touches:: [[code:hiker/handler]]
note:: rmcp HTTP clients pull tasks via `task_*`; identified by client id, leased with `McpClientVia::External` · evidence: `mcp-server/src/handler` `task_checkout_inner` stamps lease against the client id. (`McpClientVia::InProcessChatAgent` + `[tasks] expose_to_chat_agent` are vestigial config/enum remnants of the removed in-app chat agent — there is no in-process dispatcher; only `External` is reachable.)

The surface is the `task_checkout` / `task_submit` / `task_fail` / `task_heartbeat` tools below, with the lease stamped against the rmcp client id.

MCP clients handle both `Direct` and `Agent` shapes — the agent on the other end interprets the prompt however it likes (a tool-using agent will call hiker's other MCP tools naturally during its work; a tool-less direct caller just returns text). The `shape` field in the checkout response tells the client whether the task expects tool use; enforcement is at submit time via the schema check. [task-queue-mcp-client-handles-both-shapes]
status:: done
note:: MCP clients can take both `Direct` and `Agent` shapes · evidence: `core/src/tasks.rs::Queue::checkout_mcp` accepts shape filter but doesn't restrict by lane

There is **no** ACP path. ACP was removed (`llm.md`). If a user configures their agent to act as an rmcp client and pull tasks, that's the external-rmcp-clients case — the user pointed it at hiker, hiker didn't drive it. [task-queue-acp-via-mcp-only]
status:: done
note:: enforced by absence — the queue is reachable only via `Queue::*` (in-process) and the `task_*` MCP tools; `core::acp` does not exist


### Worker preference

When both the direct worker and MCP-client consumers are eligible for a task, `[tasks] worker_preference` picks the winner:

- **`internal`** — direct worker grabs eligible `Direct` tasks immediately; MCP `task_checkout` only sees a task if the direct worker is disabled or busy. [task-queue-worker-preference-internal]
status:: done
touches:: [[code:hiker/tasks]]
note:: direct worker grabs immediately · evidence: `core/src/config.rs::TasksConfig::direct_grace` returns 0s for `Internal`; `Queue::submit` sets eligibility to `now`
- **`external`** — direct worker abstains for a queue-wide grace window (default 5s) before taking a task; MCP `task_checkout` sees newly-queued tasks immediately, and if no MCP client picks one up within the grace, the direct worker takes over. [task-queue-worker-preference-external]
status:: done
touches:: [[code:hiker/tasks]]
note:: direct worker waits 5s before becoming eligible · evidence: `direct_grace` returns 5s for `External`
- **`auto`** (default) — short grace window of **1s** before the direct worker grabs: low latency on the common case (no external attached) while letting an attached MCP client win when it polls fast enough. [task-queue-worker-preference-auto]
status:: done
touches:: [[code:hiker/tasks]]
note:: 1s grace · evidence: `direct_grace` returns 1s for `Auto` (default)

The grace window is implemented as a per-task `eligible_to_direct_at = submitted_at + grace`. The direct worker's `next_eligible()` query filters on this; MCP `task_checkout` ignores it. The window starts on `Queued`, not on requeue-after-lease-expiry — once a task has been around long enough that the direct worker is eligible, it stays eligible. (Per-task-type override is out of v1 — see Out of scope.)


## MCP tool surface

Added to `mcp.md`'s tool list. All are advertised whenever `[mcp] enabled = true`, regardless of `worker_preference` (the preference only affects who *gets* each task).

- **`task_checkout(types?: TaskKind[], shapes?: TaskShape[], min_priority?: Priority, lease_secs?: number)`** — return the next eligible task (or null if none). Filters: only tasks whose `kind` is in `types` (default: any), whose `shape` is in `shapes` (default: any), and whose priority is at least `min_priority` (default: `Low`). Stamps a lease against the calling rmcp client id. Returns `{ task_id, kind, shape, payload, output_schema?, lease_expires_at }`. [tasks-mcp-tool-checkout]
status:: done
touches:: [[code:hiker/handler]]
note:: full filter set + lease cap · evidence: `mcp-server/src/handler.rs::task_checkout` (+ `_inner`)
implements:: [[code:hiker/tasks/queue/impl#[Queue]checkout_mcp]], [[code:hiker/tasks/queue/impl#[Queue]pick_next_filtered]]
- **`task_submit(task_id: string, value: json)`** — submit the result. Validates `value` against the task's `output_schema` if any; rejects `schema_violation` on mismatch with the lease retained so the agent can retry. Rejects `stale_lease` if the lease has expired or been invalidated by cancellation. On success, resolves the producer handle and emits `TaskCompleted`. [tasks-mcp-tool-submit]
status:: done
touches:: [[code:hiker/handler]]
note:: validates against `output_schema`; maps errors to 1006/1007 · evidence: `mcp-server/src/handler.rs::task_submit_inner`
implements:: [[code:hiker/tasks/queue/impl#[Queue]submit_result]]
- **`task_fail(task_id: string, error: string)`** — agent gives up. Emits `TaskFailed`; producer handle resolves to `Failed`. The task is *not* auto-requeued — producers re-submit if they want a retry. [tasks-mcp-tool-fail]
status:: done
touches:: [[code:hiker/handler]]
note:: emits `TaskFailed`; not auto-requeued · evidence: `mcp-server/src/handler.rs::task_fail_inner`
implements:: [[code:hiker/tasks/queue/impl#[Queue]fail]]
- **`task_heartbeat(task_id: string)`** — extend the current lease by another `lease_secs` window. Returns the new `lease_expires_at`. [tasks-mcp-tool-heartbeat]
implements:: [[code:hiker/tasks/queue/impl#[Queue]heartbeat]]

Read-only `task_list(states?, types?)` lets external tooling see the queue without taking work. [tasks-mcp-tool-list]
status:: done
touches:: [[code:hiker/handler]]
note:: read-only filtered snapshot · evidence: `mcp-server/src/handler.rs::task_list_inner`
implements:: [[code:hiker/tasks/queue/impl#[Queue]list]]

The MCP error model adds two new codes per `mcp.md`'s positive-code series:

- `1006` (`stale_lease`) — submit/fail/heartbeat against a lease that's expired or been cancelled.
- `1007` (`schema_violation`) — submit value didn't match `output_schema`. Lease retained.

[tasks-mcp-error-codes]
status:: done
touches:: [[code:hiker/handler]]
note:: both codes wired · evidence: `mcp-server/src/handler.rs` returns `ErrorCode(1006)` (`stale_lease`) and `ErrorCode(1007)` (`schema_violation`)


## Settings

New section in the standard config (per `settings.md`'s eligibility model):

```toml
[tasks]
worker_preference = "auto"          # "auto" | "internal" | "external"
terminal_retention_secs = 60        # how long terminal rows stay visible

[tasks.direct_worker]
enabled = true
parallelism = 1                     # how many direct-LLM tasks can run concurrently

# Vestigial: this gated the (now-removed) in-app chat agent's tool set. There
# is no in-process chat agent after the rework — external rmcp clients over
# HTTP always see the task_* tools when [mcp] enabled is true. The key is kept
# for config back-compat but has no live effect.
expose_to_chat_agent = true

[tasks.lease]
default_secs = 60
max_secs = 600
```

Eligibility: every key in `[tasks]` is per-vault overridable except `[tasks].worker_preference`, which is also valid at user scope. Strict-load + auto-create + `set_setting` write-back behave the same as every other section. [task-queue-settings-section]
status:: done
implements:: [[code:hiker/config/sections/TasksConfig]]
note:: full schema, vault-scope-eligible; `worker_preference` also user-scope eligible per spec · evidence: `core/src/config.rs::TasksConfig` + sub-structs (`DirectWorkerConfig`, `LeaseConfig`); eligibility entries in `ELIGIBLE_VAULT` and `ELIGIBLE_USER`

Settings UI gets a new "Task queue" section: the `direct_worker.enabled` toggle, a `worker_preference` radio, and a numeric retention field — all also surfaced inline on the queue detail page so the user can flip them without leaving the queue view. (`expose_to_chat_agent` is vestigial — no in-app chat agent — so it carries no live UI toggle.) [task-queue-settings-ui-section, task-queue-worker-toggles]

When `[llm] enabled = false` (per [[spec:llm-features-disable-entirely]]), the `[tasks]` settings still load but the direct worker is force-disabled regardless of `direct_worker.enabled` — no LLM means no in-process worker, by definition. The MCP `task_*` tools stop being advertised since the queue's only purpose is LLM work. The home-page widget renders an empty state. [task-queue-respects-llm-disable]
status:: done
touches:: [[code:hiker/handler]], [[code:hiker/panels/home]]
note:: tools still surface in the rmcp registry but error coherently — agents see a uniform disabled state · evidence: the host skips spawning the direct worker when `!config.llm.enabled`; `mcp-server/src/handler.rs::guard_tasks` answers `1004 disabled` from every `task_*` tool when `llm_enabled = false`; home tile hidden in `app/src/panels/home.rs`


## Entry points

Two surfaces open the queue detail page:

- **Top-strip Queue button** ([[spec:vault-bar-queue-button]], per `editor.md` Top strip) — primary entry point. Always visible in the top strip's leading cluster. Indicator on the icon shows the active count; pulses subtly on `Leased`. Click → opens the shared queue detail page.
- **Home-page Task queue widget** (below) — a tile on the vault home overview that drills into the same detail page, pre-selecting the LLM-tasks filter. The existing "Queued" tile on [[spec:vault-home-stats-widget]] drills into the same page pre-selecting the Embedding filter.

All three converge on the same shared queue detail surface ([[spec:queue-detail-shared-page]]).


## Home page widget

A vault-home tile per the existing `editor.md` widget pattern: "Task queue" + active count + a small icon.

- **Tile state** — count of `Queued + Leased` tasks. Empty state ("No tasks queued") when zero. [task-queue-home-widget]
status:: partial
touches:: [[code:hiker/panels/home]]
note:: shipped as a section under the home overview rather than a fifth stat tile; active-count summary live-updates from queue events · evidence: `app/src/panels/home.rs` (tasks section; subscribes to queue events, seeds via `tasks_snapshot`)
implements:: [[code:hiker/tasks/types/impl#[TaskKind]metadata_oneliner]]
- **Click drills into the detail view** per [[spec:vault-home-detail-views]]. Detail view body is a list of task rows + a settings strip at the top with the worker toggles + preference radio. [task-queue-home-detail-view]
status:: done
implements:: [[code:hiker/tasks/queue/Slot]], [[code:hiker/tasks/queue/Slot#last_error]], [[code:hiker/tasks/queue/impl#[Queue]details]], [[code:hiker/tasks/types/TaskDetails]]
touches:: [[code:hiker/panels/queue]]
note:: queue opens as a `queue`-kind tab per [[spec:tab-kinds]]. Migrated from swap-out sub-mode to app-page tab (S2).

### Shared queue detail page with the embedding queue

The detail view for this tile and the existing embedding-queue detail (the "Queued" tile from [[spec:vault-home-stats-widget]], currently flowing into [[spec:vault-home-stats-detail]]) render on **the same page** with filter buttons. The two queues stay strictly separate underneath — different producers, different workers, different `core::*` modules (`core::indexer` for embeddings, `core::tasks` for LLM work) — but the UI consolidates them because the user's mental model is one: "what is the app currently working on in the background." [queue-detail-shared-page]
status:: done
touches:: [[code:hiker/panels/queue]]
note:: strict data-layer separation preserved (separate event channels, separate maps, separate render builders) · evidence: `app/src/panels/queue.rs` subscribes to both queue events and indexer-progress events; renders interleaved Active / Queued / Recently finished sections with source badges

Page shape: a "Background work" panel with the filter pills at top, a worker-toggles + preference-radio strip shown only when the filter includes LLM (the embedding queue has no user-facing toggles), then rows grouped into **Active** / **Queued** / **Recently finished** sections.

Filter pills (two icon-only buttons, multi-select; both active = the previous "All" view):

- **LLM tasks** — robot icon. Toggles `core::tasks` rows in/out of the view. [queue-detail-filter-tasks]
status:: done
touches:: [[code:hiker/panels/queue]]
note:: icon-only multi-select pill (robot glyph). Toggling it on/off filters task rows. Worker toggles strip is hidden when this pill is off. Slug `queue-detail-filter-all` is retired — both pills active reproduces the previous "All" view, so the dedicated all-pill is gone. Drill-in from the home tasks tile still pre-selects this pill · evidence: `app/src/panels/queue.rs` (active-filters set seeded with tasks + embedding, robot-icon filter pill)
- **Embedding** — brain icon, same glyph as the search bar's semantic-search toggle. Toggles `core::indexer` rows in/out of the view. [queue-detail-filter-index]
status:: done
note:: brain glyph matches `#toggle-mode-semantic` in the search bar. Multi-select with [[spec:queue-detail-filter-tasks]]; turning off the only active pill is suppressed so the page can never render empty. Drill-in from the embedding-stats tile pre-selects this pill via `setFilter("embedding")` · evidence: same evidence as [[spec:queue-detail-filter-tasks]] (sibling brain-icon pill `data-filter="embedding"`)

Each row carries a small badge identifying its source (`task` / `index`); when both pills are active, rows interleave by state-bucket then submitted_at. The two pills can't both be off (clicking the only-active one is a no-op). Drilling in from a tile pre-selects one pill; the user re-enables the other with a click.

Embedding-queue rows render with the same chrome as task rows — priority-pill slot left empty (the indexer has no priorities), state pill driven by indexer-progress events (`queued` / `started` / `finished` / `skipped`), pulsing `…` on `started` rows reusing [[spec:tree-row-queued-marker]]'s animation. The indexer queue has no per-row cancel; the ✕ button appears only on `core::tasks` rows. [queue-detail-embedding-row-shape]
status:: done
touches:: [[code:hiker/panels/queue]]
note:: no per-row cancel button per spec · evidence: embedding-row rendering in `app/src/panels/queue.rs`; pulsing `…` on `started` rows reuses the same shape as task `Leased` rows; `skip` / `✓` / `✗` for terminal

**Shared** across the two: the row-rendering primitive (priority pill / state pill / pulsing indicator / hover reveal of cancel button), the section grouping (Active / Queued / Recently finished), and the event-driven local-mirror pattern (one snapshot fetch + delta updates from an event channel). The shared row rendering lives in `app/src/panels/queue.rs` (`task_row`), parameterized by source. **Not shared:** the data layer (separate commands, event channels, and `core::*` modules — the indexer is a watcher-driven streaming pipeline with no priorities/leases/schemas, the task queue is producer-pull-and-await), the worker controls (only the LLM queue has any), and the cancellation path. Each queue keeps its own data fetch + event subscription. [queue-detail-shared-row-primitive]
status:: done
note:: both queues use the same chrome · evidence: shared helpers (`statePill`, `escapeHtml`, the source-badge prepend in `buildTaskRowWithBadge`, `buildIndexRow`) drive both lanes; section grouping (`Active` / `Queued` / `Recently finished`) shared

Entering from the original "Queued" tile ([[spec:vault-home-stats-widget]]) pre-selects the **Embedding** filter; the new "Task queue" tile pre-selects **LLM tasks**. The header pills let the user broaden / swap from either. [vault-home-stats-queued-tile-shared-detail]
status:: planned
note:: existing Queued tile in [[spec:vault-home-stats-widget]] still drills into its own detail; not yet routed to the new shared page

Per-task row: `[priority pill] <kind summary> <state> <submitted-rel-time> [✕]` with a `↳ <metadata one-liner>` + `worker: <worker-kind>` sub-line. State rendering rules:

- **Queued** — neutral state pill. Static.
- **Leased** — pill carries a pulsing `…` indicator (three CSS-animated dots, same shape as [[spec:chat-panel-thinking-indicator]]). Worker label names who's working ("Direct LLM" / "Chat agent" / "External: Claude Code (id: …)"). [task-queue-row-pulsing-leased]
status:: done
touches:: [[code:hiker/panels/queue]]
note:: pulsing dot on Leased rows, worker label rendered · evidence: `app/src/panels/queue.rs` (state pill + pulsing dot)
implements:: [[code:hiker/tasks/types/impl#[WorkerKind]label]]
- **Completed / Failed / Cancelled** — terminal pill with appropriate color; the row stays for `terminal_retention_secs` then disappears.

Cancel button: visible on Queued + Leased rows; calls `Queue::cancel` via a command. Terminal rows have no cancel. [task-queue-row-cancel-action]
status:: done
touches:: [[code:hiker/panels/queue]]
note:: exactly the spec'd shape · evidence: `app/src/panels/queue.rs` (✕ button calls `tasks_cancel`); terminal rows omit the button

Sort: by priority tier then submitted_at, matching drain order so the visible order tells the user what comes next.

The widget is hidden entirely (no tile rendered) when LLM is disabled, since the queue is meaningless in that mode. Gated on the same [[spec:llm-features-disable-entirely]] check. [task-queue-home-widget-respects-llm-disable]
status:: done
touches:: [[code:hiker/panels/home]]
note:: `app/src/panels/home.rs` reads `[llm] enabled` from settings at mount and hides the tasks section when disabled


## Audit log integration

Every terminal task (Completed / Failed / Cancelled) writes one row to [[spec:llm-audit-log]] per the existing JSONL format. The row carries:

- `surface = "core::tasks"` — distinguishes from `core::llm` / `mcp-tool-call`.
- `feature = task.kind` (the enum variant name).
- `worker = WorkerKind` (which worker handled it).
- `priority`, `duration_ms`, `submitted_at`, optional `error`.
- `task_id` so a debugging trail can correlate widget events with audit entries.

The prompt + response are *not* duplicated in the audit row — the underlying worker writes its own through its existing surface; the task row is queue-view metadata, not a second copy. Content-redaction discipline applies ([[spec:obs-no-content]]); `[llm.audit] log_full_prompt` is honored by the underlying surface. [task-queue-audit-row]
status:: done
note:: one row per terminal task from the direct worker; MCP-client-driven completions don't yet write the queue-side audit row (they write the `mcp-tool-call` row from `mcp-server/src/audit.rs` through the existing path) · evidence: `core/src/tasks.rs::run_direct_worker` writes `surface = "core::tasks"`, `feature = task.kind.variant_name()`, with `task_id` / `worker` / `priority` / `duration_ms` in details
implements:: [[code:hiker/tasks/handlers/impl#[DirectWorker]record_outcome]]


## Out of scope

- **Task persistence across app restarts.** In-memory only; producers awaiting handles get `Cancelled { app_exit }` on shutdown and re-submit on next launch if they care. Adding durability needs a real story for "what does the user see when a task survives a restart" — not load-bearing for v1. [task-queue-in-memory-only]
status:: done
touches:: [[code:hiker/tasks]]
note:: no on-disk persistence in v1; queue resets on vault swap · evidence: `core/src/tasks.rs::QueueState` is a plain `HashMap` — no persistence
- **Server→client cancellation push to external workers.** External agents learn cancellation at submit time via `stale_lease`. rmcp-streamable cancellation notifications are the right v2 fix; tracked as [[spec:task-queue-mcp-cancel-notification]] (deferred).
- **Task grouping / jobs.** Fan-out producers submit N tasks and aggregate themselves; the queue doesn't model "this batch of 50 tasks is one logical job." If the home-page widget grows a "RAPTOR build (50 tasks)" header view, that's a presentation-layer grouping by `metadata.group_id`, not a queue concept. ([[spec:task-queue-task-grouping]], deferred.)
- **Per-task-type worker preference.** One global preference for v1. Per-type matrix (e.g. "always external for note-mutation, always internal for auto-tag") is a settings UI elaboration if the global setting proves too coarse. ([[spec:task-queue-per-type-preference]], deferred.)
- **Cancellation chaining across producers.** A fan-out producer canceling itself cancels each of its outstanding handles individually; the queue doesn't have a "cancel all tasks tagged X" API. Producers manage their own bundles.
- **Cross-vault queue.** One queue per vault, mirroring every other core module's vault scoping.
- **Quota / rate-limit enforcement at the queue.** Throttling LLM calls is the underlying worker's concern (or the provider's). The queue doesn't model "no more than N direct-LLM calls per minute."


## Forward refs

- `core::tasks` is the implementation home; lands as a sibling to `core::llm` in the v3.5 milestone. RAPTOR / fan-out features that consume it are post-v3.5 by `design.md`'s build order.
- `mcp.md` adds the `task_*` tool surface alongside this spec; no separate spec doc.
- `settings.md`'s settings UI gets the Task queue section once settings UI lands per its existing milestone.
- `editor.md`'s vault-home widget table gains a row for the queue tile; no shape change to the home-page architecture.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **task-queue-io-worker-lane** — a dedicated in-process worker for long-running non-LLM I/O (web crawl, `crawl-task-queue-lane`); distinct from the direct-LLM drain and the synchronous `NonLlmHandlers` path; never an MCP client; no `output_schema`; on the queue for lease/progress/cancel/visibility; per-page tasks roll up via [[spec:task-queue-task-grouping]] [task-queue-io-worker-lane]
  status:: planned
- **task-queue-worker-toggles** — independent toggles [task-queue-worker-toggles]
  status:: done
  touches:: [[code:hiker/panels/settings]]
  note:: evidence: `core/src/config.rs::TasksConfig` (independent `direct_worker.enabled` + `expose_to_chat_agent`); same toggles surfaced in `app/src/panels/settings/mod.rs`'s Task queue section
- **task-queue-worker-preference** — `worker_preference = 'auto' | 'internal' | 'external'` arbitrates direct vs MCP [task-queue-worker-preference]
  status:: done
  touches:: [[code:hiker/tasks]]
  note:: evidence: `core/src/config.rs::WorkerPreferenceCfg`; `core/src/tasks.rs::Queue::submit` uses `direct_grace()` to set `eligible_to_direct_at`
- **task-queue-settings-ui-section** — both surfaces wired [task-queue-settings-ui-section]
  status:: done
  touches:: [[code:hiker/panels/queue]], [[code:hiker/panels/settings]]
  note:: evidence: `app/src/panels/settings/mod.rs` Task queue section + inline toggles in `app/src/panels/queue.rs` (direct-worker, expose-to-chat-agent, worker-preference) bound to `set_setting` with a status flash on save
- **task-queue-mcp-cancel-notification** — deferred — rmcp server→client streamable cancellation push [task-queue-mcp-cancel-notification]
  status:: planned
- **task-queue-persistence** — deferred — in-memory only in v1 [task-queue-persistence]
  status:: planned
- **task-queue-task-grouping** — deferred — `metadata.group_id` for fan-out producers [task-queue-task-grouping]
  status:: planned
- **task-queue-per-type-preference** — deferred — per-task-type override of `worker_preference` [task-queue-per-type-preference]
  status:: planned
