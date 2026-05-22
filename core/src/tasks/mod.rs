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
//
//! Module layout (refactor only — no behavior changes):
//! - `types`    — public data types: `Task`, `TaskKind`, `Priority`,
//!   `TaskShape`, `WorkerKind`, records, events, errors, `TaskHandle`.
//! - `queue`    — internal `Slot`/`Lease`, `Queue` struct + impl, and
//!   the pure helpers (`validate_against_schema`, time helpers).
//! - `handlers` — `NonLlm` trait + the in-process direct-LLM
//!   worker entry point (`run_direct_worker`).
//! - `tests`    — `#[cfg(test)]` unit tests.

pub mod handlers;
pub mod queue;
pub mod types;

#[cfg(test)]
mod tests;

