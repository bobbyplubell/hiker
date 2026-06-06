# Observability

Plan for instrumenting hiker with `tracing`. Goal: when something goes wrong (a note didn't index, the watcher fired 3000 events, an embedder call hung) we can answer *what happened and where* from the log stream, without rerunning under a debugger.

Use the `tracing` crate ecosystem — `tracing` for emission, `tracing-subscriber` for formatting. No `log`, no `env_logger`, no `println!` for diagnostics. [obs-tracing-baseline]


## Subscriber setup

The live subscriber is an inline `tracing_subscriber::fmt()` in the app entry point (`app/src/main.rs`) writing to **stderr** with an `EnvFilter`:

```rust
tracing_subscriber::fmt()
    .with_env_filter(
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")),
    )
    .init();
```

`RUST_LOG` works — set it to override the bare-`info` default per-module (`RUST_LOG=info,hiker=debug`). There is no file appender and no per-vault log file today; output goes to the terminal that launched the app.

The `cli` and `mcp-server` binaries install **no** subscriber, so their `tracing` events are dropped unless run in-process under the app. Wiring them (and file logging) is tracked under Deferred (`bug-observability-file-logging-unwired`).


## Where to log

No `#[instrument]` decorations, no spans. Just events at the obvious sites, with structured fields; convert every existing `eprintln!("[hiker::indexer] ...")` to a `tracing` event.

Event sites:

- **Watcher**: each debounced event (`debug!(path, kind)`); each suppression hit (`debug!(path)` with the suppression token).
- **Indexer**: per-job decision points — file indexed (`info!(path, chunks)`), file skipped (`info!(path, reason)`), file errored (`error!(error = %e, path)`), full-scan summary (`info!(seen, queued, deleted)`).
- **Embedder**: model load (`info!`), per-batch (`debug!(batch_size, elapsed_ms)`).
- **Store**: schema migrations and slow queries (`warn!(query, elapsed_ms)` if elapsed > 100ms — cheap to add inline).
- **Host commands**: errors only — emit on the `Err(...)` branch with `error!(error = %e, command = "<name>")`.

That's enough to reconstruct any failure from the log stream. Span-based grouping ("everything that happened during job 42") is the upgrade path when this stops being enough.


## Error context [obs-error-context]

Every `error!` (and most `warn!`) call must include the operation context as fields, not interpolated strings. Bad:

```rust
error!("failed to embed note {}: {}", path.display(), e);
```

Good:

```rust
error!(error = %e, path = %path.display(), "failed to embed note");
```

The `error = %e` field captures the chain; the message stays grep-stable. Combined with `anyhow::Context` on the error itself, both the structured trace and the error chain land in one event.


## What we are *not* logging

- Note **content**. Titles and paths yes, body text no. Logs are persisted on disk and may be shared during debugging; note content is the user's data. [obs-no-content]
- **Embeddings.** Vectors are huge and meaningless to a human reader; log dimensions and norms if needed, never the values themselves.
- **API keys, secrets, or auth tokens.** Use the `tracing::field::Empty` pattern + explicit recording so we can't accidentally Display a config struct that contains a key. [obs-no-secrets]


## Module placement

- No module under `core::*` calls `tracing_subscriber::*` directly. Modules emit events; the binary configures the subscriber.
- `core::observability` exists but its `init_tracing(vault_root)` (the deferred file-logging entry point) is **dead code** with zero callers — see Deferred.


## Deferred

Each lands when the live stderr surface stops answering questions, not before.

### File logging (unwired) [obs-log-files] [obs-log-rotation]

The designed-but-unwired model: `core::observability::init_tracing(vault_root)` called from each binary's entry point, standing up a two-layer subscriber — a compact stderr format layer plus a `tracing-appender` daily-rotating file layer at `vault/.hiker/logs/hiker.log` (7-day retention, gitignored, per-vault state that follows the user). The non-blocking writer's `_guard` must outlive the program (stash it in a `OnceLock`).

This function exists in `core::observability` but has **no callers** — no file logging runs anywhere, and `cli`/`mcp-server` have no subscriber at all. Wiring it up (and replacing the inline app `fmt()` with it) is tracked as `bug-observability-file-logging-unwired`.

### Spans on pipeline stages [obs-spans-pipeline]

Spans wrap pipeline stages (`indexer.process_job`, `embedder.embed_batch`, `cluster.reconcile`) — not individual function calls — via `#[instrument]`, giving a single timeline showing where time was spent and which stage failed, async-aware across `.await`. Adopt when flat-event volume makes correlation painful. Per-subsystem slots reserved:

- **Watcher** — one span per debounced event after coalescing; pre-debounce raw events stay at `trace!`. [obs-instrument-watcher]
- **Indexer** — top-level span per job; child spans for `chunk` / `embed` / `store`, each recording elapsed and outcome on close. [obs-instrument-indexer]
- **Embedder** — span on `embed_batch` with `batch_size` and elapsed. [obs-instrument-embed]
- **Store** — slow-query log only (no span per SQL call); migrations at `info!`. [obs-instrument-store]
- **Cluster / summarize** — top-level span on `hiker reconcile`; per-level child spans with `level`, `member_count`, `algorithm` fields. [obs-instrument-cluster]
- **Host command boundary** — `#[instrument]` on every host command so each user action is one nested trace. [obs-command-spans]

### Named env var + richer default [obs-env-filter]

The live subscriber already honors `RUST_LOG` via `EnvFilter`. The follow-up is a hiker-namespaced `HIKER_LOG` var and a richer default than bare `info` (e.g. `info,hiker=debug`) so module-level verbosity needs no env var at all.

### UI logging [obs-frontend-bridge]

The UI is native egui (Rust), so panel code emits `tracing` events directly — there's no separate frontend process and no log bridge to cross. UI call sites use the standard `tracing` macros with a `ui::`-prefixed target naming the panel; the events ride the same subscriber as the rest of the app.

**Targets.** Use the `ui::` prefix with the panel name as the second segment: `ui::files`, `ui::search`, `ui::chat`, `ui::settings`, `ui::app`. Keeps the namespace clean for env-filter filtering.

**No content.** The same `obs-no-content` and `obs-no-secrets` rules apply: panels MUST NOT log note body text, embeddings, or auth tokens. Discipline-only — reviewers should reject any UI log call that includes buffer text.

**Levels by site type:**

- `error` — a failure the user can see (toast / red banner / aborted action).
- `warn` — a failure the UI swallows on purpose (e.g. `persist_view_setting` fire-and-forget — the user already saw the local effect succeed).
- `info` — vault open/close, panel mount/unmount, settings reload. Low volume, high signal for understanding "what was the app doing when it broke."
- `debug` — chatty per-event diagnostics (search debounce fired, watcher refresh queued). Off by default once `obs-env-filter` lands; for now, written but filtered by the `INFO` default.

**Out of scope for this slug.** No global panic-hook capture into the log stream and no automatic backtrace enrichment. Those are useful but each is a follow-up: a dedicated `obs-frontend-uncaught` slug can land later if direct `tracing` calls leave blind spots.

### In-app log viewer

Three-piece feature for browsing logs without leaving the app:

- **Broadcast layer** — custom `tracing-subscriber` layer fans every formatted event into a `tokio::sync::broadcast` channel; the host subscribes and emits each as log events. [obs-log-channel]
- **Ring buffer** — same layer keeps the most recent N events (default 2000) in a server-side `VecDeque` so the viewer has history when it opens mid-session; `get_log_buffer(filter) -> Vec<LogEvent>` command returns the snapshot. [obs-log-ring-buffer]
- **Viewer panel** — collapsible UI panel showing the live event stream. Per-row: timestamp, level, module, message, expandable fields. Top bar: level filter, free-text filter, pause/resume. Filter is client-side only. [obs-log-viewer-panel]

### Test subscriber [obs-test-subscriber]

`core::test_support::init_tracing()` per-test using `tracing-subscriber`'s test layer (`with_test_writer()`) so events are captured into the test output and only printed on failure. No global init in tests — that breaks parallel execution. Add the first time a failing test would have been easier to diagnose with logs.

### Performance flamegraph [obs-perf-flamegraph]

`tracing-flame` / chrome trace export for flamegraph generation when investigating perf regressions. One-line addition (`FlameLayer::new(...)`) when needed.

### Per-user opt-in telemetry upload

Not now, not without an explicit consent flow. Local logs only.


## Out of scope

- Replacing the existing error type strategy (`anyhow` for binary, `thiserror` for library boundaries — covered separately).
- Crash reporting (sentry-style). Different problem; revisit when there's a reason.
- Span tree visualization in the eventual log viewer. The flat event list is enough.
