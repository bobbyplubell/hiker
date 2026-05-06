# Observability

Plan for instrumenting hiker with `tracing` + `tracing-subscriber`. Goal: when something goes wrong (a note didn't index, a reconcile took 40s, the watcher fired 3000 events) we can answer *what happened and where* from logs alone, without rerunning under a debugger.

The headline decisions:

- Use the `tracing` crate ecosystem end-to-end — `tracing` for emission, `tracing-subscriber` for filtering/formatting, `tracing-appender` for file output. No `log`, no `env_logger`, no `println!` for diagnostics. [obs-tracing-baseline]
- Spans wrap pipeline stages, not individual function calls. We want "indexing this file" to be one span with child events, not fifty nested function spans. [obs-spans-pipeline]
- Filtering is `EnvFilter`-driven (`HIKER_LOG=info,hiker::core::embed=debug`) so users and devs can crank up verbosity on a single module without recompiling. [obs-env-filter]
- Production logs go to `vault/.hiker/logs/hiker.log` with daily rotation; dev logs go to stderr. Both layers run when both are enabled. [obs-log-files]
- Errors carry context via `tracing::error!` with structured fields, not stringified `format!` blobs. The structured fields are the contract; the human message is for grep. [obs-error-context]


## Why `tracing` (and not `log`)

`log` is fine for "emit a string at level X." We need more:

- **Spans.** A file going through `discover → read → chunk → embed → store` is one logical operation; we want a single timeline that shows where time was spent and which stage failed. `log` has no concept of this; `tracing` spans give it for free.
- **Structured fields.** `note_id=...`, `chunk_count=12`, `embed_ms=340` is greppable and machine-parseable. String interpolation isn't.
- **Async-aware.** Spans propagate across `.await` points correctly. With `log` in async code you lose the call site context the moment you yield.

The cost is a slightly heavier dependency tree and a one-time learning curve on `#[instrument]`. Worth it.


## Subscriber setup

Single `init_tracing()` call in `main.rs` (binary) and a corresponding test helper in `core::test_support`. Layered subscriber:

1. **EnvFilter** — `HIKER_LOG` env var, defaults to `info` in release, `debug` for `hiker::*` in debug builds. Lets a user run `HIKER_LOG=trace hiker reindex` to debug a stuck reindex without a custom build. [obs-env-filter]
2. **Format layer (stderr)** — pretty-printed compact format in debug; JSON in release if `HIKER_LOG_JSON=1`. JSON output is for piping into `jq` or shipping to a log collector later. [obs-format-json]
3. **File layer (rolling)** — `tracing-appender` daily rotation in `vault/.hiker/logs/`, retained for 7 days. The vault is the right home for these (per-vault state, follows the user, gitignored by default). [obs-log-files] [obs-log-rotation]

```rust
let env = EnvFilter::try_from_env("HIKER_LOG")
    .unwrap_or_else(|_| EnvFilter::new("info,hiker=debug"));

let file_appender = tracing_appender::rolling::daily(
    vault.join(".hiker/logs"), "hiker.log"
);
let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

tracing_subscriber::registry()
    .with(env)
    .with(fmt::layer().with_writer(io::stderr).compact())
    .with(fmt::layer().with_writer(file_writer).json())
    .init();
```

The `_guard` from `non_blocking` must outlive the program — store it on the Tauri `App` state (or a static `OnceLock`) so the writer thread isn't dropped on init return.


## What gets instrumented

Spans go on the boundaries that matter for diagnosis. Inside a span, raw events (`info!`, `warn!`, `error!`) carry the per-step detail.

### Watcher [obs-instrument-watcher]

```rust
#[instrument(skip(self), fields(path = %event.path.display(), kind = ?event.kind))]
fn handle_event(&self, event: WatchEvent) { ... }
```

One span per debounced event after coalescing. Pre-debounce raw events are too noisy to span individually — log them at `trace!` only. Self-write suppression hits (per `watcher-suppress-self-writes`) emit a `debug!` with the suppressing token so we can see when the round-trip guard fired.

### Indexer [obs-instrument-indexer]

```rust
#[instrument(skip_all, fields(path = %job.path.display(), job_id = job.id))]
async fn process_job(job: IndexJob) -> Result<()> { ... }
```

Top-level span per job. Inside it, child spans for the heavy stages: `chunk`, `embed`, `store`. Each child records elapsed time and an outcome field on close (`outcome = "ok" | "skipped" | "error"`).

### Embedder [obs-instrument-embed]

```rust
#[instrument(skip(self, texts), fields(batch_size = texts.len()))]
fn embed_batch(&self, texts: &[String]) -> Result<Vec<Embedding>> { ... }
```

Records batch size and elapsed. Embedding is the dominant cost; we want a clean signal of `embed_ms` per batch in the log so we can correlate latency spikes against batch size or content shape.

### Store [obs-instrument-store]

Slow-query logging only — no span on every SQL call (too noisy). Wrap the connection with a thin layer that emits `warn!(query, elapsed_ms)` if a single query exceeds 100ms. Schema migrations log at `info!`.

### Cluster / summarize [obs-instrument-cluster]

`hiker reconcile` gets a top-level span; each level of the recursive cluster build is a child span with `level`, `member_count`, `algorithm` fields. Summarizer calls (per `cluster-summarize-llm`) get their own span with the model name and token usage when the provider returns it.

### Tauri command boundary [obs-tauri-command-spans]

Every `#[tauri::command]` function gets `#[instrument]`. The frontend → core boundary is exactly where we want a span: it gives us a trace per user action with everything done in service of it nested underneath.

```rust
#[tauri::command]
#[instrument(skip(state))]
async fn move_note(from: &str, to: &str, state: State<'_, AppState>) -> Result<()> { ... }
```


## Frontend bridge [obs-frontend-bridge]

The webview can't emit `tracing` events directly. Two options:

- **Cheap:** a `log_from_frontend(level, message, fields)` Tauri command the frontend calls; the command emits the corresponding `tracing` event server-side.
- **Better later:** ship structured frontend errors (uncaught exceptions, failed fetches) via the same command, batched. Defer until we have a frontend error worth catching.

Start with the cheap version. The frontend rarely needs to emit; this is mostly for surfacing errors that bubble up to the UI.


## Error context [obs-error-context]

Every `error!` call must include the operation context as fields, not interpolated strings. Bad:

```rust
error!("failed to embed note {}: {}", path.display(), e);
```

Good:

```rust
error!(error = %e, path = %path.display(), "failed to embed note");
```

The `error = %e` field captures the chain; the message stays grep-stable. Combined with `anyhow::Context` on the error itself, we get both the structured trace and the error chain in one event.


## What we are *not* logging

- Note **content**. Titles and paths yes, body text no. Logs are persisted on disk and may be shared during debugging; note content is the user's data. [obs-no-content]
- **Embeddings.** Vectors are huge and meaningless to a human reader; log dimensions and norms if needed, never the values themselves.
- **API keys, secrets, or auth tokens** — covered by the `obs-no-secrets` rule. Use the `tracing::field::Empty` pattern + explicit recording so we can't accidentally Display a config struct that contains a key. [obs-no-secrets]


## Performance overhead

`tracing` with the default subscriber is in the tens-of-nanoseconds-per-event range when filtered out (the macros expand to a level check first). The file appender is non-blocking. We can leave instrumentation on in release without measurable overhead at `info` level. At `debug` in hot paths (per-chunk events) it adds up — the convention is `debug!` for per-stage, `trace!` for per-item.


## Test integration

Tests get `tracing-subscriber`'s test layer (`with_test_writer()`) so events are captured into the test output and only printed on failure. Helper in `core::test_support::init_tracing()` called from each test that wants logging. No global init in tests — that breaks parallel test execution. [obs-test-subscriber]


## Module placement

- `core::observability` — exports `init_tracing()`, the file appender guard type, and the `EnvFilter` defaults.
- No other module should call `tracing_subscriber::*` directly. Emit events; let the binary configure the subscriber.


## Deferred

- **`tracing-flame` / chrome trace export** for flamegraph generation when investigating perf regressions. One-line addition (`FlameLayer::new(...)`) when needed. [obs-perf-flamegraph]
- **Per-user opt-in telemetry upload.** Not now, not without an explicit consent flow. Local logs only.


## Out of scope

- Replacing the existing error type strategy (`anyhow` for binary, `thiserror` for library boundaries — not yet locked in but covered separately).
- A custom log viewer UI inside the app. The vault `.hiker/logs/` directory is enough; users (or we) tail it with their tool of choice.
- Crash reporting (sentry-style). Different problem; revisit when there's a reason.
