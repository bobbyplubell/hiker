//! `Queue` — the central in-memory store and lifecycle driver. Owns the
//! slot table, lease arbitration, event broadcast, and the small set of
//! pure helpers (time math + the subset-JSON-Schema validator) used by
//! both the queue and the direct-LLM worker.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, SystemTime};

use tokio::sync::{broadcast, oneshot, Mutex};

use crate::agent::StopSignal;
use crate::config::TasksConfig;

use super::types::{
    CancelReason, McpClientVia, Priority, QueueError, QueueEvent, Task, TaskDetails, TaskHandle,
    TaskId, TaskOutcome, TaskRecord, TaskShape, TaskState, WorkerKind,
};

/// Mirror of the TOML `[tasks] worker_preference` enum. Lives in
/// `core::config`; re-exported here so the queue can match on it.
pub use crate::config::WorkerPreferenceCfg;

/// Internal record carrying everything the queue needs to drive a task
/// through its lifecycle.
pub(super) struct Slot {
    pub(super) task: Task,
    pub(super) state: TaskState,
    /// One-shot channel that resolves the producer's handle.
    pub(super) sender: Option<oneshot::Sender<TaskOutcome>>,
    /// `Some` while leased.
    pub(super) lease: Option<Lease>,
    /// Terminal-state details (set on Completed/Failed/Cancelled).
    pub(super) finished_at: Option<SystemTime>,
    /// Last worker that held a lease — retained on terminal states for
    /// the home widget's "worker:" label.
    pub(super) last_worker: Option<WorkerKind>,
    /// Time the task became eligible for the direct worker. Driven by
    /// `worker_preference` + the per-preference grace window. MCP
    /// `task_checkout` ignores this.
    pub(super) eligible_to_direct_at: SystemTime,
    /// status: task-queue-row-details
    /// Retained terminal result so the queue-detail UI can show the
    /// final response when the user clicks a row. The producer's handle
    /// also carries this, but the handle is consumed exactly once —
    /// retaining a copy on the slot keeps the inspect path independent
    /// of producer ownership. Bounded by `terminal_retention_secs` (the
    /// row + its result get GC'd together).
    pub(super) last_result: Option<serde_json::Value>,
    /// status: task-queue-row-details
    /// Retained terminal error string (Failed) so the queue-detail UI
    /// can show the underlying provider / worker error verbatim.
    pub(super) last_error: Option<String>,
}

pub(super) struct Lease {
    pub(super) worker: WorkerKind,
    pub(super) expires_at: SystemTime,
    /// Stop signal fired by `Queue::cancel` when an in-process worker
    /// holds the lease. MCP-client leases get their lease invalidated
    /// instead — the eventual `task_submit` returns `stale_lease`.
    pub(super) stop: Option<StopSignal>,
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

pub(super) struct QueueState {
    pub(super) slots: HashMap<TaskId, Slot>,
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

    /// Replace the live config. Plumbed from the `set_setting`
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

    /// Insert a task in `Leased` state, owned by an in-process producer
    /// (the indexer) that runs the underlying work itself. Returns the
    /// id; the producer then drives the slot through `submit_result` /
    /// `fail` on completion.
    ///
    /// Used for non-LLM work that rides the queue purely for UI
    /// visibility (e.g. `EmbedderModelLoad`). The slot is never offered
    /// via `checkout_direct` / `checkout_mcp` (those only consider
    /// `Queued` slots), so there's no risk of a worker double-leasing.
    /// The producer handle is internal: drop fires no cancel, mirroring
    /// the existing `task-queue-submit-handle` semantics. Callers that
    /// want cancel can still call `Queue::cancel(id)` — the slot will
    /// flip to `Cancelled` and the producer's next `complete`/`fail`
    /// call will return `StaleLease`.
    ///
    /// status: embedder-model-load-as-task
    pub async fn submit_self_managed(&self, mut task: Task) -> TaskId {
        if task.id.is_empty() {
            task.id = ulid::Ulid::new().to_string();
        }
        let id = task.id.clone();
        let now = SystemTime::now();
        let worker = WorkerKind::Indexer;
        let queued_event = QueueEvent::TaskQueued {
            id: id.clone(),
            kind: task.kind.clone(),
            priority: task.priority,
            shape: task.shape,
            submitted_at_ms: ms_since_epoch(task.submitted_at),
        };
        // No real lease timeout for the indexer — it runs the work
        // synchronously inside `spawn_blocking` and reports completion
        // through `submit_result` / `fail`. We still stamp an expiry so
        // a wedged producer eventually has the row GC'd by maintenance.
        let expires = now + Duration::from_secs(3600);
        let leased_event = QueueEvent::TaskLeased {
            id: id.clone(),
            worker: worker.clone(),
            lease_expires_at_ms: ms_since_epoch(expires),
        };
        let mut state = self.inner.state.lock().await;
        state.slots.insert(
            id.clone(),
            Slot {
                task,
                state: TaskState::Leased,
                sender: None,
                lease: Some(Lease {
                    worker: worker.clone(),
                    expires_at: expires,
                    stop: None,
                }),
                finished_at: None,
                last_worker: Some(worker),
                eligible_to_direct_at: now,
                last_result: None,
                last_error: None,
            },
        );
        drop(state);
        let _ = self.inner.events_tx.send(queued_event);
        let _ = self.inner.events_tx.send(leased_event);
        id
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
                .map_err(QueueError::SchemaViolation)?;
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
        let mut to_orphan_fail: Vec<TaskId> = Vec::new();
        for (id, slot) in state.slots.iter() {
            if matches!(slot.state, TaskState::Leased)
                && let Some(lease) = &slot.lease
                && lease.expires_at <= now
            {
                // Indexer-owned (self-managed) leases never go
                // back into the queue — no other worker can
                // make progress on them. An expired one means
                // the indexer producer dropped the row without
                // calling complete/fail (shouldn't happen, but
                // we orphan-fail it to avoid stuck rows).
                // status: embedder-model-load-as-task
                if matches!(lease.worker, WorkerKind::Indexer) {
                    to_orphan_fail.push(id.clone());
                } else {
                    to_requeue.push(id.clone());
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
        let mut failed_events: Vec<QueueEvent> = Vec::new();
        for id in &to_orphan_fail {
            let slot = state.slots.get_mut(id).unwrap();
            let worker = slot.lease.as_ref().map(|l| l.worker.clone());
            let started = slot.task.submitted_at;
            let err = "self-managed lease expired without completion".to_string();
            slot.state = TaskState::Failed;
            slot.finished_at = Some(now);
            slot.lease = None;
            slot.last_error = Some(err.clone());
            failed_events.push(QueueEvent::TaskFailed {
                id: id.clone(),
                worker,
                error_summary: summarize(&err, 80),
                duration_ms: duration_ms(started, now),
            });
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
        drop(state);
        for ev in failed_events {
            let _ = self.inner.events_tx.send(ev);
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
            if let Some(ks) = kinds
                && !ks.iter().any(|k| k == slot.task.kind.variant_name())
            {
                continue;
            }
            if let Some(ss) = shapes
                && !ss.contains(&slot.task.shape)
            {
                continue;
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

pub(super) fn slot_to_record(slot: &Slot) -> TaskRecord {
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

pub(super) fn ms_since_epoch(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn duration_ms(start: SystemTime, end: SystemTime) -> u64 {
    end.duration_since(start)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn summarize(s: &str, max: usize) -> String {
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
pub(super) fn validate_against_schema(
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
