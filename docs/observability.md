# Observability

Plan for instrumenting hiker with `tracing`. Goal: when something goes wrong (a note didn't index, the watcher fired 3000 events, an embedder call hung) we can answer *what happened and where* by reading a log file, without rerunning under a debugger.

v1 is deliberately small: stand up `tracing`, write a rotating log file, replace `eprintln!` calls with structured events. Spans, in-app log viewer, frontend bridge, and `EnvFilter` tuning are deferred — they're the natural follow-ups but the file alone covers the immediate troubleshooting need.

The headline decisions:

- Use the `tracing` crate ecosystem — `tracing` for emission, `tracing-subscriber` for formatting, `tracing-appender` for file output. No `log`, no `env_logger`, no `println!` for diagnostics. [obs-tracing-baseline]
- Production logs go to `vault/.hiker/logs/hiker.log` with daily rotation, 7-day retention; dev logs also go to stderr. [obs-log-files] [obs-log-rotation]
- Errors carry context via structured fields, not stringified `format!` blobs. The fields are the contract; the human message is for grep. [obs-error-context]
- Note **content** never enters the log stream. Paths and titles yes; body bytes no. [obs-no-content]
- Secrets / API keys / auth tokens never enter the log stream. [obs-no-secrets]


## Why `tracing` (and not `log`)

`log` is fine for "emit a string at level X." `tracing` adds two things we'll want as soon as v1 lands and we want to grow into them:

- **Structured fields.** `note_id=...`, `chunk_count=12`, `embed_ms=340` is greppable and machine-parseable; string interpolation isn't.
- **Spans (deferred).** A file going through `discover → read → chunk → embed → store` is one logical operation. Spans group the events into a single timeline and survive `.await` points. We're not using them in v1, but adopting `tracing` now means we don't have to migrate later.

Cost is a slightly heavier dependency tree. Worth it; the next-step features (spans, in-app viewer) only fit on top of `tracing`.


## v1: subscriber setup

Single `init_tracing(vault_root)` call from each binary's entry point (`ui/src-tauri`, `cli`, `mcp-server`). Two-layer subscriber:

1. **Format layer (stderr)** — pretty-printed compact format. What you see while running the app in dev.
2. **File layer (rolling)** — `tracing-appender` daily rotation in `vault/.hiker/logs/`, retained for 7 days. Same compact format. The vault is the right home for these (per-vault state, follows the user, gitignored by default). [obs-log-files] [obs-log-rotation]

Hardcoded default level: `info` for everything, `debug` for `hiker::*`. No `EnvFilter` parsing in v1 — if you need more verbosity, edit the default and recompile. The env-var path is a small addition when the friction is real (see Deferred).

```rust
let file_appender = tracing_appender::rolling::daily(
    vault.join(".hiker/logs"), "hiker.log"
);
let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

tracing_subscriber::registry()
    .with(filter::Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("hiker", LevelFilter::DEBUG))
    .with(fmt::layer().with_writer(io::stderr).compact())
    .with(fmt::layer().with_writer(file_writer).compact())
    .init();
```

The `_guard` from `non_blocking` must outlive the program. Easiest: stash it in a `OnceLock` from `init_tracing`.


## v1: where to log

No `#[instrument]` decorations, no spans. Just events at the obvious sites, with structured fields. Convert every existing `eprintln!("[hiker::indexer] ...")` to a `tracing` event as part of this work — those are the lowest-hanging fruit and the wrong shape today.

Concretely, v1 adds events at:

- **Watcher**: each debounced event (`debug!(path, kind)`); each suppression hit (`debug!(path)` with the suppression token).
- **Indexer**: per-job decision points — file indexed (`info!(path, chunks)`), file skipped (`info!(path, reason)`), file errored (`error!(error = %e, path)`), full-scan summary (`info!(seen, queued, deleted)`).
- **Embedder**: model load (`info!`), per-batch (`debug!(batch_size, elapsed_ms)`).
- **Store**: schema migrations and slow queries (`warn!(query, elapsed_ms)` if elapsed > 100ms — cheap to add inline).
- **Tauri commands**: errors only — emit on the `Err(...)` branch with `error!(error = %e, command = "<name>")`.

That's enough to reconstruct any v1 failure from a tail of `hiker.log`. Span-based grouping ("everything that happened during job 42") is the upgrade path when this stops being enough.


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

- `core::observability` — exports `init_tracing(vault_root)`, the file appender guard type, and any default constants.
- No other module calls `tracing_subscriber::*` directly. Emit events; let the binary configure the subscriber.


## Deferred

Everything below is real, considered, and explicitly *not v1*. Each lands when the v1 file-log surface stops answering questions, not before.

### Spans on pipeline stages [obs-spans-pipeline]

Spans wrap pipeline stages — `indexer.process_job`, `embedder.embed_batch`, `cluster.reconcile` — not individual function calls. The point is a single timeline showing where time was spent and which stage failed, async-aware across `.await`. Adopt when log volume from flat events makes correlation painful.

```rust
#[instrument(skip_all, fields(path = %job.path.display(), job_id = job.id))]
async fn process_job(job: IndexJob) -> Result<()> { ... }
```

Per-subsystem instrumentation slots already reserved:

- **Watcher** — one span per debounced event after coalescing; pre-debounce raw events stay at `trace!`. [obs-instrument-watcher]
- **Indexer** — top-level span per job; child spans for `chunk` / `embed` / `store`, each recording elapsed and outcome on close. [obs-instrument-indexer]
- **Embedder** — span on `embed_batch` with `batch_size` and elapsed; embedding is the dominant cost so its latency signal is worth pulling out. [obs-instrument-embed]
- **Store** — slow-query log only (no span per SQL call); migrations at `info!`. [obs-instrument-store]
- **Cluster / summarize** — top-level span on `hiker reconcile`; per-level child spans with `level`, `member_count`, `algorithm` fields. [obs-instrument-cluster]
- **Tauri command boundary** — `#[instrument]` on every `#[tauri::command]` so each user action is one trace with everything done in service of it nested underneath. [obs-tauri-command-spans]

### Env-var-driven filter [obs-env-filter]

`HIKER_LOG` env var → `EnvFilter`, defaulting to `info,hiker=debug`. Lets a user crank verbosity on a single module (`HIKER_LOG=trace,hiker::core::embed=debug hiker reindex`) without recompiling. Cheap addition; pulled into v1 the first time someone wants module-level tuning.

### Frontend bridge [obs-frontend-bridge]

The webview can't emit `tracing` events directly. A `log_from_frontend(level, message, fields)` Tauri command emits the corresponding event server-side. Mostly for surfacing UI-side errors into the unified log. Defer until there's a real frontend error worth catching.

### In-app log viewer

Three-piece feature for browsing logs without leaving the app:

- **Broadcast layer** — custom `tracing-subscriber` layer fans every formatted event into a `tokio::sync::broadcast` channel; Tauri side subscribes and emits each as `hiker:log-event`. [obs-log-tauri-channel]
- **Ring buffer** — same layer keeps the most recent N events (default 2000) in a server-side `VecDeque` so the viewer has history when it opens mid-session; `get_log_buffer(filter) -> Vec<LogEvent>` Tauri command returns the snapshot. [obs-log-ring-buffer]
- **Viewer panel** — collapsible UI panel showing the live event stream. Per-row: timestamp, level, module, message, expandable fields. Top bar: level filter, free-text filter, pause/resume, "open log file" button. Filter is client-side only. [obs-log-viewer-panel]

The vault `.hiker/logs/` directory is fine to read with `less` / `rg` for now; the viewer is a convenience, not a necessity.

### Test subscriber [obs-test-subscriber]

`core::test_support::init_tracing()` per-test using `tracing-subscriber`'s test layer (`with_test_writer()`) so events are captured into the test output and only printed on failure. No global init in tests — that breaks parallel execution. Add the first time a failing test would have been easier to diagnose with logs.

### Performance flamegraph [obs-perf-flamegraph]

`tracing-flame` / chrome trace export for flamegraph generation when investigating perf regressions. One-line addition (`FlameLayer::new(...)`) when needed.

### Per-user opt-in telemetry upload

Not now, not without an explicit consent flow. Local logs only.


## Out of scope

- Replacing the existing error type strategy (`anyhow` for binary, `thiserror` for library boundaries — covered separately).
- Crash reporting (sentry-style). Different problem; revisit when there's a reason.
- Span tree visualization in the eventual log viewer. The flat event list is enough; if span trees later prove useful, the data is in the file output for offline tooling.
- Search across rotated log files inside the eventual viewer. Older logs are read with `less` / `rg`.
