//! Unified work queue for non-interactive LLM jobs. See `docs/task-queue.md`.
//!
//! Producers submit `Task` records; the queue arbitrates who processes
//! each one. Two worker lanes:
//!
//! - **Direct-LLM worker** — in-process tokio task drains `Direct`-shape
//!   tasks via `core::llm::chat`. Toggled by `[tasks] direct_worker.enabled`.
//! - **MCP clients** — external rmcp callers (Claude Code, Codex, …) and
//!   the basic chat agent (when `[tasks] expose_to_chat_agent = true`)
//!   reach the queue's checkout/submit primitives via `task_*` MCP tools.
//!
//! The queue is in-memory only in v1 (`task-queue-in-memory-only`) — no
//! persistence across app restarts. Producers awaiting a handle get
//! `Cancelled { app_exit }` on shutdown.
//
// status: task-queue-core-module
// status: task-queue-task-shape
// status: task-queue-priority-tiers
// status: task-queue-lifecycle
// status: task-queue-terminal-retention
// status: task-queue-lease-timeout
// status: task-queue-submit-handle
// status: task-queue-event-stream
// status: task-queue-cancel-app-only
// status: task-queue-cancel-propagation-internal
// status: task-queue-stale-lease-rejection
// status: task-queue-shape-routing
// status: task-queue-worker-preference
// status: task-queue-worker-preference-internal
// status: task-queue-worker-preference-external
// status: task-queue-worker-preference-auto
// status: task-queue-structured-output
// status: task-queue-in-memory-only

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::StopSignal;
use crate::audit::{AgentLog, AuditEntry};
use crate::config::TasksConfig;
use crate::llm::{LlmClient, Message};

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
    fn rank(self) -> u8 {
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
    /// RAPTOR cluster summarization.
    RaptorSummarize { cluster_id: String, level: u8 },
    /// Background auto-tag-on-save.
    AutoTag { source_path: String },
    /// Background summary-on-save.
    SummaryOnSave { source_path: String },
}

impl TaskKind {
    /// Stable string used in audit-log `feature` and home-widget rendering.
    pub fn variant_name(&self) -> &'static str {
        match self {
            TaskKind::NoteMutation { .. } => "note_mutation",
            TaskKind::RaptorSummarize { .. } => "raptor_summarize",
            TaskKind::AutoTag { .. } => "auto_tag",
            TaskKind::SummaryOnSave { .. } => "summary_on_save",
        }
    }

    /// One-line summary the home-widget renders under the kind label.
    pub fn metadata_oneliner(&self) -> String {
        match self {
            TaskKind::NoteMutation { source_path, .. } => source_path.clone(),
            TaskKind::RaptorSummarize { cluster_id, level } => {
                format!("cluster {cluster_id} (level {level})")
            }
            TaskKind::AutoTag { source_path } => source_path.clone(),
            TaskKind::SummaryOnSave { source_path } => source_path.clone(),
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
    /// Any rmcp caller. `via` discriminates the basic chat agent's
    /// in-process dispatch from external HTTP rmcp clients.
    McpClient { client_id: String, via: McpClientVia },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpClientVia {
    External,
    InProcessChatAgent,
}

impl WorkerKind {
    pub fn label(&self) -> String {
        match self {
            WorkerKind::DirectLlm => "Direct LLM".to_string(),
            WorkerKind::McpClient { client_id, via } => match via {
                McpClientVia::InProcessChatAgent => "Chat agent".to_string(),
                McpClientVia::External => format!("External: {client_id}"),
            },
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

/// status: task-queue-row-details
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
    rx: oneshot::Receiver<TaskOutcome>,
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

/// Internal record carrying everything the queue needs to drive a task
/// through its lifecycle.
struct Slot {
    task: Task,
    state: TaskState,
    /// One-shot channel that resolves the producer's handle.
    sender: Option<oneshot::Sender<TaskOutcome>>,
    /// `Some` while leased.
    lease: Option<Lease>,
    /// Terminal-state details (set on Completed/Failed/Cancelled).
    finished_at: Option<SystemTime>,
    /// Last worker that held a lease — retained on terminal states for
    /// the home widget's "worker:" label.
    last_worker: Option<WorkerKind>,
    /// Time the task became eligible for the direct worker. Driven by
    /// `worker_preference` + the per-preference grace window. MCP
    /// `task_checkout` ignores this.
    eligible_to_direct_at: SystemTime,
    /// status: task-queue-row-details
    /// Retained terminal result so the queue-detail UI can show the
    /// final response when the user clicks a row. The producer's handle
    /// also carries this, but the handle is consumed exactly once —
    /// retaining a copy on the slot keeps the inspect path independent
    /// of producer ownership. Bounded by `terminal_retention_secs` (the
    /// row + its result get GC'd together).
    last_result: Option<serde_json::Value>,
    /// status: task-queue-row-details
    /// Retained terminal error string (Failed) so the queue-detail UI
    /// can show the underlying provider / worker error verbatim.
    last_error: Option<String>,
}

struct Lease {
    worker: WorkerKind,
    expires_at: SystemTime,
    /// Stop signal fired by `Queue::cancel` when an in-process worker
    /// holds the lease. MCP-client leases get their lease invalidated
    /// instead — the eventual `task_submit` returns `stale_lease`.
    stop: Option<StopSignal>,
}

/// The queue's central state. Cheap to clone (`Arc` of the inner mutex).
#[derive(Clone)]
pub struct Queue {
    inner: Arc<QueueInner>,
}

struct QueueInner {
    state: Mutex<QueueState>,
    events_tx: broadcast::Sender<QueueEvent>,
    /// Live config — re-readable so `set_setting` can flip
    /// `worker_preference` / `direct_worker.enabled` /
    /// `terminal_retention_secs` without a vault restart. Wrapped in a
    /// `std::sync::RwLock` (not the tokio variant) since reads are a
    /// fast struct copy and we don't want to make every internal config
    /// peek `.await`.
    cfg: StdRwLock<TasksConfig>,
}

struct QueueState {
    slots: HashMap<TaskId, Slot>,
}

impl Queue {
    pub fn new(cfg: TasksConfig) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(QueueInner {
                state: Mutex::new(QueueState { slots: HashMap::new() }),
                events_tx,
                cfg: StdRwLock::new(cfg),
            }),
        }
    }

    /// Live snapshot of the config. Cheap (just a struct clone). Use
    /// instead of holding a borrow across `await` points.
    pub fn cfg(&self) -> TasksConfig {
        self.inner.cfg.read().expect("tasks cfg lock poisoned").clone()
    }

    /// Replace the live config. Plumbed from the Tauri `set_setting`
    /// command after `Config::set` succeeds so flips of
    /// `worker_preference` / `direct_worker.enabled` /
    /// `terminal_retention_secs` apply without a vault restart.
    pub fn set_cfg(&self, cfg: TasksConfig) {
        *self.inner.cfg.write().expect("tasks cfg lock poisoned") = cfg;
    }

    /// Subscribe to `QueueEvent`s. The home-widget seeds via `snapshot()`
    /// then applies events.
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.inner.events_tx.subscribe()
    }

    /// Submit a task. Returns immediately with an awaitable handle.
    pub async fn submit(&self, mut task: Task) -> TaskHandle {
        if task.id.is_empty() {
            task.id = ulid::Ulid::new().to_string();
        }
        let (tx, rx) = oneshot::channel();
        let id = task.id.clone();
        let mut state = self.inner.state.lock().await;
        let now = SystemTime::now();
        let cfg = self.inner.cfg.read().expect("tasks cfg lock poisoned");
        let grace = cfg.direct_grace();
        // worker_preference == "internal" means direct worker is eligible
        // immediately; "auto"/"external" wait the configured grace.
        let eligible = match cfg.worker_preference {
            WorkerPreferenceCfg::Internal => now,
            _ => now + grace,
        };
        drop(cfg);
        let event = QueueEvent::TaskQueued {
            id: id.clone(),
            kind: task.kind.clone(),
            priority: task.priority,
            shape: task.shape,
            submitted_at_ms: ms_since_epoch(task.submitted_at),
        };
        state.slots.insert(
            id.clone(),
            Slot {
                task,
                state: TaskState::Queued,
                sender: Some(tx),
                lease: None,
                finished_at: None,
                last_worker: None,
                eligible_to_direct_at: eligible,
                last_result: None,
                last_error: None,
            },
        );
        drop(state);
        let _ = self.inner.events_tx.send(event);
        TaskHandle { id, rx }
    }

    /// Snapshot every non-GC'd row. Sorted by drain order: priority desc,
    /// then submitted_at asc.
    pub async fn snapshot(&self) -> Vec<TaskRecord> {
        let state = self.inner.state.lock().await;
        let mut rows: Vec<TaskRecord> = state.slots.values().map(slot_to_record).collect();
        rows.sort_by(|a, b| {
            b.priority
                .rank()
                .cmp(&a.priority.rank())
                .then(a.submitted_at_ms.cmp(&b.submitted_at_ms))
        });
        rows
    }

    /// In-process cancel. The only path. Behavior depends on state:
    /// - Queued     → resolve handle as `Cancelled { user_action }`
    /// - Leased to direct worker → fire `StopSignal::cancel`
    /// - Leased to MCP client    → invalidate the lease; the eventual
    ///   `task_submit` returns `stale_lease`. Producer handle resolves
    ///   immediately.
    pub async fn cancel(&self, id: &TaskId) {
        let mut state = self.inner.state.lock().await;
        let Some(slot) = state.slots.get_mut(id) else { return };
        match slot.state {
            TaskState::Queued | TaskState::Leased => {
                if let Some(lease) = &slot.lease {
                    // Fire any in-process stop signal so the worker drops
                    // its in-flight call quickly.
                    if let Some(stop) = &lease.stop {
                        stop.cancel();
                    }
                }
                slot.state = TaskState::Cancelled;
                slot.finished_at = Some(SystemTime::now());
                slot.lease = None;
                if let Some(tx) = slot.sender.take() {
                    let _ = tx.send(TaskOutcome::Cancelled {
                        reason: CancelReason::UserAction,
                    });
                }
                drop(state);
                let _ = self.inner.events_tx.send(QueueEvent::TaskCancelled {
                    id: id.clone(),
                    reason: CancelReason::UserAction,
                });
            }
            _ => {} // already terminal
        }
    }

    /// Direct worker peek: return the next eligible Direct-shape task,
    /// stamping a lease against `WorkerKind::DirectLlm`. Returns the
    /// stamped `Task` plus the `StopSignal` the worker should observe.
    /// Returns `None` if nothing is eligible.
    pub async fn checkout_direct(&self, lease_secs: u64) -> Option<(Task, StopSignal)> {
        let mut state = self.inner.state.lock().await;
        let now = SystemTime::now();
        let id = self.pick_next(&state, now, true)?;
        let stop = StopSignal::new();
        let slot = state.slots.get_mut(&id).unwrap();
        let expires = now + Duration::from_secs(lease_secs);
        let worker = WorkerKind::DirectLlm;
        slot.state = TaskState::Leased;
        slot.last_worker = Some(worker.clone());
        slot.lease = Some(Lease {
            worker: worker.clone(),
            expires_at: expires,
            stop: Some(stop.clone()),
        });
        let task = slot.task.clone();
        drop(state);
        let _ = self.inner.events_tx.send(QueueEvent::TaskLeased {
            id: id.clone(),
            worker,
            lease_expires_at_ms: ms_since_epoch(expires),
        });
        Some((task, stop))
    }

    /// MCP-client checkout. Filters by allowed `kinds` / `shapes` /
    /// `min_priority`; stamps a lease against `client_id`.
    pub async fn checkout_mcp(
        &self,
        client_id: &str,
        via: McpClientVia,
        kinds: Option<&[String]>,
        shapes: Option<&[TaskShape]>,
        min_priority: Priority,
        lease_secs: u64,
    ) -> Option<Task> {
        let mut state = self.inner.state.lock().await;
        let now = SystemTime::now();
        let id = self.pick_next_filtered(&state, kinds, shapes, min_priority)?;
        let slot = state.slots.get_mut(&id).unwrap();
        let expires = now + Duration::from_secs(lease_secs);
        let worker = WorkerKind::McpClient {
            client_id: client_id.to_string(),
            via,
        };
        slot.state = TaskState::Leased;
        slot.last_worker = Some(worker.clone());
        slot.lease = Some(Lease {
            worker: worker.clone(),
            expires_at: expires,
            stop: None,
        });
        let task = slot.task.clone();
        drop(state);
        let _ = self.inner.events_tx.send(QueueEvent::TaskLeased {
            id: id.clone(),
            worker,
            lease_expires_at_ms: ms_since_epoch(expires),
        });
        Some(task)
    }

    /// Submit a result. Validates `value` against `output_schema` if any.
    /// On success: resolves the producer handle and emits `TaskCompleted`.
    pub async fn submit_result(
        &self,
        id: &TaskId,
        value: serde_json::Value,
    ) -> Result<(), QueueError> {
        let mut state = self.inner.state.lock().await;
        let slot = state
            .slots
            .get_mut(id)
            .ok_or_else(|| QueueError::NotFound(id.clone()))?;
        // Stale-lease check: a cancellation may have flipped state to
        // `Cancelled` while the worker was running.
        if slot.state != TaskState::Leased {
            return Err(QueueError::StaleLease);
        }
        if let Some(schema) = slot.task.output_schema.as_ref() {
            validate_against_schema(schema, &value)
                .map_err(|e| QueueError::SchemaViolation(e))?;
        }
        let worker = slot
            .lease
            .as_ref()
            .map(|l| l.worker.clone())
            .unwrap_or(WorkerKind::DirectLlm);
        let started = slot.task.submitted_at;
        let now = SystemTime::now();
        slot.state = TaskState::Completed;
        slot.finished_at = Some(now);
        slot.lease = None;
        slot.last_result = Some(value.clone());
        let duration_ms = duration_ms(started, now);
        if let Some(tx) = slot.sender.take() {
            let _ = tx.send(TaskOutcome::Completed {
                value,
                worker: worker.clone(),
                duration_ms,
            });
        }
        drop(state);
        let _ = self.inner.events_tx.send(QueueEvent::TaskCompleted {
            id: id.clone(),
            worker,
            duration_ms,
        });
        Ok(())
    }

    /// Worker gives up. Emits `TaskFailed`; producer handle resolves to
    /// `Failed`. Not auto-requeued.
    pub async fn fail(&self, id: &TaskId, error: String) -> Result<(), QueueError> {
        let mut state = self.inner.state.lock().await;
        let slot = state
            .slots
            .get_mut(id)
            .ok_or_else(|| QueueError::NotFound(id.clone()))?;
        if slot.state != TaskState::Leased {
            return Err(QueueError::StaleLease);
        }
        let worker = slot.lease.as_ref().map(|l| l.worker.clone());
        let started = slot.task.submitted_at;
        let now = SystemTime::now();
        slot.state = TaskState::Failed;
        slot.finished_at = Some(now);
        slot.lease = None;
        slot.last_error = Some(error.clone());
        let duration_ms = duration_ms(started, now);
        let summary = summarize(&error, 80);
        if let Some(tx) = slot.sender.take() {
            let _ = tx.send(TaskOutcome::Failed {
                error: error.clone(),
                worker: worker.clone(),
                duration_ms,
            });
        }
        drop(state);
        let _ = self.inner.events_tx.send(QueueEvent::TaskFailed {
            id: id.clone(),
            worker,
            error_summary: summary,
            duration_ms,
        });
        Ok(())
    }

    /// Extend the current lease on a leased task. Returns the new
    /// expiry timestamp.
    pub async fn heartbeat(
        &self,
        id: &TaskId,
        lease_secs: u64,
    ) -> Result<SystemTime, QueueError> {
        let mut state = self.inner.state.lock().await;
        let slot = state
            .slots
            .get_mut(id)
            .ok_or_else(|| QueueError::NotFound(id.clone()))?;
        if slot.state != TaskState::Leased {
            return Err(QueueError::StaleLease);
        }
        let new_expiry = SystemTime::now() + Duration::from_secs(lease_secs);
        if let Some(lease) = slot.lease.as_mut() {
            lease.expires_at = new_expiry;
        }
        drop(state);
        let _ = self.inner.events_tx.send(QueueEvent::TaskHeartbeat {
            id: id.clone(),
            lease_expires_at_ms: ms_since_epoch(new_expiry),
        });
        Ok(new_expiry)
    }

    /// Read-only inspection. Filters mirror MCP `task_list`.
    pub async fn list(
        &self,
        states: Option<&[TaskState]>,
        kinds: Option<&[String]>,
    ) -> Vec<TaskRecord> {
        let snap = self.snapshot().await;
        snap.into_iter()
            .filter(|r| {
                states
                    .as_ref()
                    .map(|ss| ss.contains(&r.state))
                    .unwrap_or(true)
                    && kinds
                        .as_ref()
                        .map(|ks| ks.iter().any(|k| k == r.kind.variant_name()))
                        .unwrap_or(true)
            })
            .collect()
    }

    /// status: task-queue-row-details
    /// Full inspection payload for one task — the lazy "click a row to
    /// see prompt / result / error" path. `None` if the id is unknown
    /// (already GC'd past `terminal_retention_secs`, or never existed).
    pub async fn details(&self, id: &TaskId) -> Option<TaskDetails> {
        let state = self.inner.state.lock().await;
        let slot = state.slots.get(id)?;
        Some(TaskDetails {
            id: slot.task.id.clone(),
            kind: slot.task.kind.clone(),
            priority: slot.task.priority,
            shape: slot.task.shape,
            state: slot.state,
            submitted_at_ms: ms_since_epoch(slot.task.submitted_at),
            finished_at_ms: slot.finished_at.map(ms_since_epoch),
            worker: slot.last_worker.clone(),
            prompt: slot.task.payload.prompt.clone(),
            inputs: slot.task.payload.inputs.clone(),
            output_schema: slot.task.output_schema.clone(),
            metadata: slot.task.metadata.clone(),
            result: slot.last_result.clone(),
            error: slot.last_error.clone(),
        })
    }

    /// One-shot tick: GC terminal rows past retention, requeue
    /// expired leases. Caller (the maintenance task) calls this on a
    /// timer.
    pub async fn tick_maintenance(&self) {
        let retention = Duration::from_secs(
            self.inner
                .cfg
                .read()
                .expect("tasks cfg lock poisoned")
                .terminal_retention_secs
                .max(1),
        );
        let mut state = self.inner.state.lock().await;
        let now = SystemTime::now();
        // Requeue expired leases.
        let mut to_requeue: Vec<TaskId> = Vec::new();
        for (id, slot) in state.slots.iter() {
            if matches!(slot.state, TaskState::Leased) {
                if let Some(lease) = &slot.lease {
                    if lease.expires_at <= now {
                        to_requeue.push(id.clone());
                    }
                }
            }
        }
        for id in &to_requeue {
            let slot = state.slots.get_mut(id).unwrap();
            slot.state = TaskState::Queued;
            slot.lease = None;
            // Once the task has been around long enough that the direct
            // worker is eligible, it stays eligible — the grace window
            // is from initial submit, not requeue.
        }
        // GC terminal rows past retention.
        let stale_terminal: Vec<TaskId> = state
            .slots
            .iter()
            .filter_map(|(id, slot)| match slot.finished_at {
                Some(t) if matches!(
                    slot.state,
                    TaskState::Completed | TaskState::Failed | TaskState::Cancelled
                ) && now.duration_since(t).map(|d| d > retention).unwrap_or(false)
                    => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in stale_terminal {
            state.slots.remove(&id);
        }
    }

    /// Walk the queue and pick the next id that the direct worker should
    /// take. Honors `eligible_to_direct_at` and skips Agent-shape tasks.
    fn pick_next(&self, state: &QueueState, now: SystemTime, direct: bool) -> Option<TaskId> {
        let mut best: Option<(&TaskId, &Slot)> = None;
        for (id, slot) in state.slots.iter() {
            if slot.state != TaskState::Queued {
                continue;
            }
            if direct {
                if !matches!(slot.task.shape, TaskShape::Direct) {
                    continue;
                }
                if slot.eligible_to_direct_at > now {
                    continue;
                }
            }
            best = match best {
                None => Some((id, slot)),
                Some((_, b)) => {
                    let s_rank = slot.task.priority.rank();
                    let b_rank = b.task.priority.rank();
                    if s_rank > b_rank
                        || (s_rank == b_rank && slot.task.submitted_at < b.task.submitted_at)
                    {
                        Some((id, slot))
                    } else {
                        best
                    }
                }
            };
        }
        best.map(|(id, _)| id.clone())
    }

    fn pick_next_filtered(
        &self,
        state: &QueueState,
        kinds: Option<&[String]>,
        shapes: Option<&[TaskShape]>,
        min_priority: Priority,
    ) -> Option<TaskId> {
        let mut best: Option<(&TaskId, &Slot)> = None;
        for (id, slot) in state.slots.iter() {
            if slot.state != TaskState::Queued {
                continue;
            }
            if slot.task.priority.rank() < min_priority.rank() {
                continue;
            }
            if let Some(ks) = kinds {
                if !ks.iter().any(|k| k == slot.task.kind.variant_name()) {
                    continue;
                }
            }
            if let Some(ss) = shapes {
                if !ss.contains(&slot.task.shape) {
                    continue;
                }
            }
            best = match best {
                None => Some((id, slot)),
                Some((_, b)) => {
                    let s_rank = slot.task.priority.rank();
                    let b_rank = b.task.priority.rank();
                    if s_rank > b_rank
                        || (s_rank == b_rank && slot.task.submitted_at < b.task.submitted_at)
                    {
                        Some((id, slot))
                    } else {
                        best
                    }
                }
            };
        }
        best.map(|(id, _)| id.clone())
    }
}

/// Mirror of the TOML `[tasks] worker_preference` enum. Lives in
/// `core::config`; re-exported here so the queue can match on it.
pub use crate::config::WorkerPreferenceCfg;

fn slot_to_record(slot: &Slot) -> TaskRecord {
    TaskRecord {
        id: slot.task.id.clone(),
        kind: slot.task.kind.clone(),
        kind_summary: slot.task.kind.metadata_oneliner(),
        priority: slot.task.priority,
        shape: slot.task.shape,
        state: slot.state,
        submitted_at_ms: ms_since_epoch(slot.task.submitted_at),
        lease_expires_at_ms: slot.lease.as_ref().map(|l| ms_since_epoch(l.expires_at)),
        worker: slot.last_worker.clone(),
        finished_at_ms: slot.finished_at.map(ms_since_epoch),
    }
}

fn ms_since_epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn duration_ms(start: SystemTime, end: SystemTime) -> u64 {
    end.duration_since(start)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn summarize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Light-weight JSON Schema validation. v1 covers the subset features
/// will actually need: top-level `type`, `required` on objects, and
/// nested-object `properties` types. This is intentionally a subset of
/// JSON Schema — the heavyweight `jsonschema` crate isn't on the dep
/// list, and the queue doesn't need full Draft-2020 conformance to do
/// useful enforcement. Returns the violation reason on mismatch.
fn validate_against_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        // Malformed schema — accept the value rather than refusing on
        // the producer's bug.
        return Ok(());
    };
    if let Some(ty) = schema.get("type").and_then(|v| v.as_str()) {
        let ok = match ty {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.is_i64() || value.is_u64(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        if !ok {
            return Err(format!("expected type {ty}"));
        }
    }
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        let obj = value.as_object();
        for req in required {
            if let Some(name) = req.as_str() {
                let present = obj.map(|o| o.contains_key(name)).unwrap_or(false);
                if !present {
                    return Err(format!("missing required field `{name}`"));
                }
            }
        }
    }
    if let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(|v| v.as_object()),
        value.as_object(),
    ) {
        for (k, sub_schema) in props {
            if let Some(v) = obj.get(k) {
                validate_against_schema(sub_schema, v).map_err(|e| format!("{k}: {e}"))?;
            }
        }
    }
    Ok(())
}

// ---------- direct-LLM worker ----------

/// Spawn the in-process direct-LLM worker. Drains `Direct`-shape tasks
/// one at a time per instance; spawn `parallelism` instances if the
/// config asks for more. Returns when `cancel` fires.
///
/// status: task-queue-direct-worker
/// status: task-queue-direct-worker-toggle
/// status: task-queue-structured-output-direct
pub async fn run_direct_worker(
    queue: Queue,
    client: Arc<dyn LlmClient>,
    audit: Option<Arc<AgentLog>>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let cfg = queue.cfg();
        // Live toggle: when `[tasks] direct_worker.enabled = false`, the
        // worker stays running but stops draining. Re-checked at the top
        // of each iteration so the user can flip the toggle in the
        // settings UI without a vault restart. (The session-wide
        // `tasks_cancel` token is the only hard exit.)
        if !cfg.direct_worker.enabled {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
            }
        }
        let lease_secs = cfg.lease.default_secs.max(1);
        let Some((task, stop)) = queue.checkout_direct(lease_secs).await else {
            // Nothing eligible right now. Sleep briefly; either a new
            // task lands (event channel) or the grace window passes.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
            }
        };

        let id = task.id.clone();
        let started = SystemTime::now();
        let outcome = drive_one_task(client.as_ref(), &task, &stop).await;
        match outcome {
            Ok(value) => {
                if let Err(e) = queue.submit_result(&id, value).await {
                    tracing::warn!(task = %id, error = %e, "tasks: submit_result failed");
                }
                if let Some(log) = audit.as_ref() {
                    log.record(&AuditEntry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "ok",
                        error: None,
                        turn_id: None,
                        step_id: None,
                        details: serde_json::json!({
                            "task_id": id,
                            "worker": "direct_llm",
                            "priority": task.priority,
                            "duration_ms": duration_ms(started, SystemTime::now()),
                        }),
                    });
                }
            }
            Err(WorkerError::Cancelled) => {
                // Cancellation already flipped the slot to `Cancelled` and
                // resolved the producer handle; nothing more to do.
            }
            Err(WorkerError::Failed(reason)) => {
                if let Err(e) = queue.fail(&id, reason.clone()).await {
                    tracing::warn!(task = %id, error = %e, "tasks: fail() failed");
                }
                if let Some(log) = audit.as_ref() {
                    log.record(&AuditEntry {
                        surface: "core::tasks",
                        feature: task.kind.variant_name(),
                        status: "error",
                        error: Some(reason),
                        turn_id: None,
                        step_id: None,
                        details: serde_json::json!({
                            "task_id": id,
                            "worker": "direct_llm",
                            "priority": task.priority,
                            "duration_ms": duration_ms(started, SystemTime::now()),
                        }),
                    });
                }
            }
        }
    }
}

enum WorkerError {
    Cancelled,
    Failed(String),
}

async fn drive_one_task(
    client: &dyn LlmClient,
    task: &Task,
    stop: &StopSignal,
) -> Result<serde_json::Value, WorkerError> {
    // Build messages. The producer-supplied prompt is the user message;
    // when an output_schema is set we append a JSON-strict instruction
    // (the v1 fallback for providers without server-side structured
    // output enforcement, per `task-queue-structured-output-direct`).
    let mut user_prompt = task.payload.prompt.clone();
    if let Some(schema) = task.task_output_schema_str() {
        user_prompt.push_str(
            "\n\nRespond strictly as JSON matching this schema; do not wrap in markdown or prose:\n",
        );
        user_prompt.push_str(&schema);
    }

    let attempt = call_with_cancel(client, &user_prompt, stop).await?;
    if let Some(schema) = task.output_schema.as_ref() {
        match parse_and_validate(&attempt, schema) {
            Ok(value) => return Ok(value),
            Err(parse_err) => {
                // Retry once with the parse error appended as guidance.
                let mut retry_prompt = user_prompt.clone();
                retry_prompt.push_str("\n\nThe previous attempt did not match the schema: ");
                retry_prompt.push_str(&parse_err);
                retry_prompt.push_str("\nReturn only valid JSON.");
                let retry = call_with_cancel(client, &retry_prompt, stop).await?;
                return parse_and_validate(&retry, schema)
                    .map_err(|e| WorkerError::Failed(format!("schema_violation: {e}")));
            }
        }
    }
    // No schema → wrap the text response so the producer always sees a
    // JSON value (most callers will define a schema, but the bare-text
    // path keeps the queue useful for ad-hoc producers).
    Ok(serde_json::Value::String(attempt))
}

async fn call_with_cancel(
    client: &dyn LlmClient,
    prompt: &str,
    stop: &StopSignal,
) -> Result<String, WorkerError> {
    let messages = vec![Message::user(prompt)];
    tokio::select! {
        _ = stop.token().cancelled() => Err(WorkerError::Cancelled),
        out = client.chat(&messages) => out.map_err(|e| WorkerError::Failed(e.to_string())),
    }
}

fn parse_and_validate(
    text: &str,
    schema: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    // Strip common code-fence wrappers the model sometimes inserts.
    let stripped = strip_code_fence(text.trim());
    let value: serde_json::Value =
        serde_json::from_str(stripped).map_err(|e| format!("not valid JSON: {e}"))?;
    validate_against_schema(schema, &value)?;
    Ok(value)
}

fn strip_code_fence(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("```json") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.trim_start_matches('\n');
        if let Some(end) = rest.rfind("```") {
            return rest[..end].trim();
        }
    }
    s
}

impl Task {
    fn task_output_schema_str(&self) -> Option<String> {
        self.output_schema
            .as_ref()
            .and_then(|s| serde_json::to_string(s).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TasksConfig;

    fn task(kind: TaskKind, priority: Priority, shape: TaskShape) -> Task {
        Task {
            id: ulid::Ulid::new().to_string(),
            kind,
            priority,
            shape,
            payload: TaskPayload {
                prompt: "test prompt".into(),
                inputs: serde_json::Value::Null,
            },
            output_schema: None,
            submitted_at: SystemTime::now(),
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn submit_and_complete_round_trip() {
        let mut cfg = TasksConfig::default();
        cfg.worker_preference = WorkerPreferenceCfg::Internal;
        let q = Queue::new(cfg);
        let handle = q
            .submit(task(
                TaskKind::AutoTag { source_path: "a.md".into() },
                Priority::Normal,
                TaskShape::Direct,
            ))
            .await;
        let id = handle.id.clone();
        let (taken, _stop) = q.checkout_direct(60).await.expect("eligible");
        assert_eq!(taken.id, id);
        q.submit_result(&id, serde_json::json!({"tag": "ok"}))
            .await
            .unwrap();
        match handle.await_outcome().await {
            TaskOutcome::Completed { value, .. } => assert_eq!(value["tag"], "ok"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_queued_task_resolves_handle() {
        let q = Queue::new(TasksConfig::default());
        let handle = q
            .submit(task(
                TaskKind::AutoTag { source_path: "a.md".into() },
                Priority::Normal,
                TaskShape::Direct,
            ))
            .await;
        let id = handle.id.clone();
        q.cancel(&id).await;
        match handle.await_outcome().await {
            TaskOutcome::Cancelled { reason } => {
                assert_eq!(reason, CancelReason::UserAction);
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn priority_ordering_high_drains_first() {
        let mut cfg = TasksConfig::default();
        cfg.worker_preference = WorkerPreferenceCfg::Internal;
        let q = Queue::new(cfg);
        let _low = q
            .submit(task(
                TaskKind::AutoTag { source_path: "low.md".into() },
                Priority::Low,
                TaskShape::Direct,
            ))
            .await;
        // Different submitted_at to keep ordering deterministic.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let high_handle = q
            .submit(task(
                TaskKind::AutoTag { source_path: "hi.md".into() },
                Priority::High,
                TaskShape::Direct,
            ))
            .await;
        let (taken, _) = q.checkout_direct(60).await.unwrap();
        assert_eq!(taken.id, high_handle.id);
    }

    #[tokio::test]
    async fn agent_shape_skipped_by_direct_worker() {
        let mut cfg = TasksConfig::default();
        cfg.worker_preference = WorkerPreferenceCfg::Internal;
        let q = Queue::new(cfg);
        let _ = q
            .submit(task(
                TaskKind::NoteMutation {
                    mutation: "rewrite".into(),
                    source_path: "n.md".into(),
                },
                Priority::High,
                TaskShape::Agent,
            ))
            .await;
        assert!(q.checkout_direct(60).await.is_none());
    }

    #[tokio::test]
    async fn schema_violation_rejects_submit() {
        let q = Queue::new({
            let mut c = TasksConfig::default();
            c.worker_preference = WorkerPreferenceCfg::Internal;
            c
        });
        let mut t = task(
            TaskKind::AutoTag { source_path: "a.md".into() },
            Priority::Normal,
            TaskShape::Direct,
        );
        t.output_schema = Some(serde_json::json!({
            "type": "object",
            "required": ["tag"],
        }));
        let handle = q.submit(t).await;
        let id = handle.id.clone();
        let _ = q.checkout_direct(60).await.unwrap();
        let err = q
            .submit_result(&id, serde_json::json!({"wrong": 1}))
            .await
            .unwrap_err();
        match err {
            QueueError::SchemaViolation(_) => {}
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_checkout_filters_min_priority() {
        let q = Queue::new({
            let mut c = TasksConfig::default();
            c.worker_preference = WorkerPreferenceCfg::External;
            c
        });
        let _low = q
            .submit(task(
                TaskKind::AutoTag { source_path: "lo.md".into() },
                Priority::Low,
                TaskShape::Direct,
            ))
            .await;
        let normal = q
            .submit(task(
                TaskKind::AutoTag { source_path: "no.md".into() },
                Priority::Normal,
                TaskShape::Direct,
            ))
            .await;
        let taken = q
            .checkout_mcp(
                "client-a",
                McpClientVia::External,
                None,
                None,
                Priority::Normal,
                60,
            )
            .await
            .unwrap();
        assert_eq!(taken.id, normal.id);
    }

    #[tokio::test]
    async fn lease_expiry_requeues_via_tick() {
        let q = Queue::new({
            let mut c = TasksConfig::default();
            c.worker_preference = WorkerPreferenceCfg::Internal;
            c
        });
        let h = q
            .submit(task(
                TaskKind::AutoTag { source_path: "a.md".into() },
                Priority::Normal,
                TaskShape::Direct,
            ))
            .await;
        let _ = q.checkout_mcp(
            "ext", McpClientVia::External, None, None, Priority::Low, 0).await.unwrap();
        // Lease secs = 0 → expired immediately. Tick should requeue.
        tokio::time::sleep(Duration::from_millis(20)).await;
        q.tick_maintenance().await;
        // Now direct worker should be able to take it.
        let (again, _) = q.checkout_direct(60).await.expect("requeued");
        assert_eq!(again.id, h.id);
    }

    #[test]
    fn schema_validates_top_level_type() {
        let s = serde_json::json!({"type": "object"});
        assert!(validate_against_schema(&s, &serde_json::json!({})).is_ok());
        assert!(validate_against_schema(&s, &serde_json::json!([])).is_err());
    }
}
