//! Public data types for the task queue. Kept free of queue/worker
//! implementation so producers can depend on the shapes without pulling
//! in the lock-touching paths.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Stable id for one task. ULID so producers can sort by submission time
/// without consulting `submitted_at` and the home-page widget can render
/// a stable identity.
pub type TaskId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
}

impl Priority {
    pub(super) const fn rank(self) -> u8 {
        match self {
            Priority::High => 2,
            Priority::Normal => 1,
            Priority::Low => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskShape {
    /// Single-shot prompt → response. Drainable by either lane.
    Direct,
    /// Needs tool-use during processing. Drainable only by an MCP client
    /// (chat agent or external rmcp client) — the direct-LLM worker can't
    /// make tool calls.
    Agent,
}

/// Exhaustive enum of task types. Adding a new feature = one new variant
/// here + the producer that submits it; no string-typed dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskKind {
    /// User-driven note mutation (rewrite-as-markdown, summarize, etc.).
    NoteMutation { mutation: String, source_path: String },
    /// Per-cluster LLM summarize task (RAPTOR build or user-triggered
    /// regenerate). Payload identifies the tree + node + level so the
    /// worker can look up members and resolve the output back into
    /// the tree's `.md`. status: task-queue-raptor-summarize
    RaptorSummarize {
        tree_id: String,
        cluster_node_id: String,
        level: u8,
    },
    /// Per-note classifier run against a saved Evergreen tree. Pure
    /// cosine — no LLM call. status: task-queue-raptor-triage-match
    RaptorTriageMatch {
        tree_id: String,
        source_path: String,
    },
    /// Initial cluster-tree build from a scope. CPU + LLM-heavy; runs
    /// through the direct-worker non-LLM side-channel so the IPC thread
    /// can return immediately and the queue page surfaces progress.
    ClusterBuildTree {
        name: String,
        source: String,
        scope_json: String,
        method_json: String,
    },
    /// Re-build a saved tree against current vault state.
    ClusterRebuildTree {
        tree_id: String,
        new_name: Option<String>,
    },
    /// Recluster the subtree under a selected cluster node.
    ClusterReclusterSubtree {
        tree_id: String,
        node_id: String,
        cluster_params_json: String,
        carry_policies_down: bool,
    },
    /// Embedder model load — wraps every `FastembedEmbedder::load_id`
    /// call (first-run startup load + hot-swap on `[indexing].model`
    /// change) so the user sees the work in the queue. Indeterminate
    /// row: no byte-progress (fastembed v5 exposes no callback), just
    /// kind + model id + start/end + outcome. Self-managed by the
    /// indexer — leased on submit (`WorkerKind::Indexer`), completed
    /// or failed when the underlying `spawn_blocking` load returns.
    /// status: embedder-model-load-as-task
    EmbedderModelLoad { model_id: String },
    /// Umbrella coordinator task for a Summarize sweep (`Db::summarize`).
    /// Submitted at `Priority::High` ahead of the per-cluster
    /// `RaptorSummarize` fan-out so the queue page shows one row covering
    /// the whole sweep; the per-cluster tasks each carry their own row.
    /// Per `cluster-op-summarize-sweep`. The coordinator has no direct
    /// worker payload — it's left in the `Leased` state by the producer
    /// and the orphan-fail sweep clears it after the synthetic expiry.
    ClusterSummarize {
        tree_id: String,
        /// One of `"all"`, `"stale-or-unfilled"`, `"subset"`.
        scope_kind: String,
        n_targets: u32,
    },
}

impl TaskKind {
    /// Stable string used in audit-log `feature` and home-widget rendering.
    pub const fn variant_name(&self) -> &'static str {
        match self {
            TaskKind::NoteMutation { .. } => "note_mutation",
            TaskKind::RaptorSummarize { .. } => "raptor_summarize",
            TaskKind::RaptorTriageMatch { .. } => "raptor_triage_match",
            TaskKind::ClusterBuildTree { .. } => "cluster_build_tree",
            TaskKind::ClusterRebuildTree { .. } => "cluster_rebuild_tree",
            TaskKind::ClusterReclusterSubtree { .. } => "cluster_recluster_subtree",
            TaskKind::EmbedderModelLoad { .. } => "embedder_model_load",
            TaskKind::ClusterSummarize { .. } => "cluster_summarize",
        }
    }

    /// One-line summary the home-widget renders under the kind label.
    pub fn metadata_oneliner(&self) -> String {
        match self {
            TaskKind::NoteMutation { source_path, .. } => source_path.clone(),
            TaskKind::RaptorSummarize {
                tree_id,
                cluster_node_id,
                level,
            } => format!("tree {tree_id} node {cluster_node_id} (L{level})"),
            TaskKind::RaptorTriageMatch {
                tree_id,
                source_path,
            } => format!("tree {tree_id} ← {source_path}"),
            TaskKind::ClusterBuildTree { name, .. } => format!("build {name}"),
            TaskKind::ClusterRebuildTree { tree_id, .. } => format!("rebuild {tree_id}"),
            TaskKind::ClusterReclusterSubtree { tree_id, node_id, .. } => {
                format!("recluster {tree_id}/{node_id}")
            }
            TaskKind::EmbedderModelLoad { model_id } => {
                format!("Loading embedder model: {model_id}")
            }
            TaskKind::ClusterSummarize {
                tree_id,
                scope_kind,
                n_targets,
            } => format!("summarize {tree_id} [{scope_kind}] ×{n_targets}"),
        }
    }
}

/// Producer-supplied prompt + structured inputs. The direct worker reads
/// `prompt` straight, the MCP client lane sees the same payload via
/// `task_checkout`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPayload {
    pub prompt: String,
    /// Optional structured inputs. Free-form so each `TaskKind` can carry
    /// what it needs; consumers parse against their expected shape.
    #[serde(default)]
    pub inputs: serde_json::Value,
}

/// One task record. The submitting producer constructs it and hands it to
/// `Queue::submit`; the queue keeps a copy under its lock and emits state
/// transitions on the event channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub kind: TaskKind,
    pub priority: Priority,
    pub shape: TaskShape,
    pub payload: TaskPayload,
    /// Optional JSON Schema; `task_submit` (and the direct worker on
    /// completion) validates the produced value against it before
    /// resolving the producer handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    pub submitted_at: SystemTime,
    /// Free-form per-feature metadata (e.g. `group_id` for fan-out).
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Task {
    pub(super) fn task_output_schema_str(&self) -> Option<String> {
        self.output_schema
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Leased,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    UserAction,
    LeaseExpired,
    AppExit,
}

/// Identification of the worker currently leasing (or that completed) a
/// task. Drives the home-widget's "worker:" label and the audit row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerKind {
    /// In-process direct-LLM background drain.
    DirectLlm,
    /// Any rmcp caller. `via` records how the client reached the queue
    /// (currently only external HTTP rmcp clients).
    McpClient { client_id: String, via: McpClientVia },
    /// Self-managed by the indexer task — used for non-LLM work that
    /// rides the queue purely for UI visibility (currently:
    /// `EmbedderModelLoad`). The indexer owns the lease + the
    /// complete/fail call; the queue's worker-arbitration code never
    /// hands these tasks out via `checkout_*`.
    /// status: embedder-model-load-as-task
    Indexer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpClientVia {
    External,
}

impl WorkerKind {
    pub fn label(&self) -> String {
        match self {
            WorkerKind::DirectLlm => "Direct LLM".to_string(),
            WorkerKind::McpClient { client_id, via } => match via {
                McpClientVia::External => format!("External: {client_id}"),
            },
            WorkerKind::Indexer => "Indexer".to_string(),
        }
    }
}

/// Snapshot row exposed to the home-page widget via `tasks_snapshot()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub kind: TaskKind,
    pub kind_summary: String,
    pub priority: Priority,
    pub shape: TaskShape,
    pub state: TaskState,
    pub submitted_at_ms: u64,
    /// Populated only when state is `Leased` — milliseconds since epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    /// Populated only when state is `Leased`, `Completed`, `Failed`, or
    /// `Cancelled` — i.e. once we've decided who handled it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerKind>,
    /// Populated only on terminal state — milliseconds since epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
}

/// status: task-queue-home-detail-view
/// On-demand inspection payload for a single task. Bigger than
/// `TaskRecord` (carries the full prompt + retained result/error
/// bodies); fetched lazily when the user clicks a queue-detail row so
/// the snapshot path stays lean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDetails {
    pub id: TaskId,
    pub kind: TaskKind,
    pub priority: Priority,
    pub shape: TaskShape,
    pub state: TaskState,
    pub submitted_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerKind>,
    pub prompt: String,
    /// Free-form structured inputs the producer attached. Returned
    /// verbatim — `inputs` is `Value::Null` for the common case.
    #[serde(default)]
    pub inputs: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Producer-attached metadata (e.g. `source_hash_at_submit` on note
    /// mutation). Returned verbatim.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Terminal result — `Some` only on Completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Terminal error — `Some` only on Failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskOutcome {
    Completed { value: serde_json::Value, worker: WorkerKind, duration_ms: u64 },
    Failed { error: String, worker: Option<WorkerKind>, duration_ms: u64 },
    Cancelled { reason: CancelReason },
}

/// Awaitable handle returned to producers. `await_outcome` resolves once
/// the task reaches a terminal state. Dropping the handle is *not* an
/// implicit cancel — producers that want cancel call `Queue::cancel(id)`
/// explicitly. (The spec mentions drop-cancel as a future affordance; we
/// keep cancel explicit for now to avoid surprising drops during fan-out
/// `try_join_all`.)
pub struct TaskHandle {
    pub id: TaskId,
    pub(super) rx: oneshot::Receiver<TaskOutcome>,
}

impl TaskHandle {
    pub async fn await_outcome(self) -> TaskOutcome {
        match self.rx.await {
            Ok(outcome) => outcome,
            Err(_) => TaskOutcome::Cancelled { reason: CancelReason::AppExit },
        }
    }
}

/// State transition fanned out on `hiker:queue-event`. Result *bodies*
/// never travel here — only summaries. Producers receive the full body
/// via their `TaskHandle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum QueueEvent {
    TaskQueued {
        id: TaskId,
        kind: TaskKind,
        priority: Priority,
        shape: TaskShape,
        submitted_at_ms: u64,
    },
    TaskLeased {
        id: TaskId,
        worker: WorkerKind,
        lease_expires_at_ms: u64,
    },
    TaskHeartbeat {
        id: TaskId,
        lease_expires_at_ms: u64,
    },
    TaskCompleted {
        id: TaskId,
        worker: WorkerKind,
        duration_ms: u64,
    },
    TaskFailed {
        id: TaskId,
        worker: Option<WorkerKind>,
        error_summary: String,
        duration_ms: u64,
    },
    TaskCancelled {
        id: TaskId,
        reason: CancelReason,
    },
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum QueueError {
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("stale lease (expired or cancelled)")]
    StaleLease,
    #[error("schema violation: {0}")]
    SchemaViolation(String),
    #[error("invalid state: {0}")]
    InvalidState(String),
}
