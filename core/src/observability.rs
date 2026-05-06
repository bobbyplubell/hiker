//! Tracing subscriber init + log file routing. See docs/observability.md.
//!
//! v1 surface: a single `init_tracing(vault_root)` call wires up two
//! `tracing-subscriber` layers — a compact stderr formatter for dev, and a
//! daily-rotating file appender at `<vault>/.hiker/logs/hiker.log` retained
//! for 7 days. No `EnvFilter`, no spans, no in-app viewer; those are the
//! deferred follow-ups in `docs/observability.md`.
//!
//! Discipline: only this module touches `tracing_subscriber`. Every other
//! module emits events via the `tracing` macros and lets the binary
//! configure the subscriber.
//!
//! What never enters the log stream:
//! - Note **content** (titles + paths only, never body bytes). [obs-no-content]
//! - Secrets / API keys / auth tokens. [obs-no-secrets]
//! - Embedding vectors (log dimensions if you must, never the values).
//!
//! Error events use structured fields (`error = %e`, `path = %p`) rather than
//! string-interpolated context — the fields are the contract, the message
//! stays grep-stable. [obs-error-context]
//!
//! status: obs-tracing-baseline

use std::path::Path;
use std::sync::OnceLock;

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder as RollingBuilder, Rotation};
use tracing_subscriber::{filter::Targets, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Errors surfaced by `init_tracing`. Subscriber init failures are reported
/// rather than panicking so the binary can decide whether to abort or carry
/// on without file logging.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tracing-appender: {0}")]
    Appender(String),
    #[error("subscriber init: {0}")]
    Init(String),
}

/// The non-blocking writer's `WorkerGuard` must outlive the program — when
/// dropped, queued events are flushed and the worker thread joins. Stashing
/// it in a process-global `OnceLock` is the simplest way to guarantee that
/// without forcing every binary to thread the guard through its main fn.
static GUARD: OnceLock<WorkerGuard> = OnceLock::new();
/// Subscriber init is process-global. Subsequent calls (e.g. opening a
/// second vault in the same UI session) become no-ops; the file layer
/// already-open from the first call keeps writing — switching vaults
/// mid-session is not a v1 concern, and re-init would either need a
/// reload-style API in `tracing-subscriber` (none exists) or a custom
/// dynamic-writer layer (deferred).
static INIT: OnceLock<()> = OnceLock::new();

/// Stand up the v1 tracing pipeline. Idempotent: the first caller wins;
/// subsequent calls return Ok without reconfiguring.
///
/// Layers:
/// - **stderr**: compact pretty format for dev runs. [obs-tracing-baseline]
/// - **file**: `tracing-appender` daily rotation under
///   `<vault>/.hiker/logs/hiker.log`, retained for 7 days. [obs-log-files] [obs-log-rotation]
///
/// Filter: hardcoded `info` for everything, `debug` for `hiker_*` targets.
/// `EnvFilter` tuning is the natural follow-up (`obs-env-filter`); for v1 the
/// fix-it path is to edit this default and recompile.
pub fn init_tracing(vault_root: &Path) -> Result<(), ObsError> {
    if INIT.get().is_some() {
        return Ok(());
    }

    let log_dir = vault_root.join(".hiker").join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Daily rotation, 7-day retention. `Builder::build` returns the appender
    // configured with both knobs; the older `rolling::daily` helper has no
    // retention setting. [obs-log-rotation]
    let file_appender = RollingBuilder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("hiker")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)
        .map_err(|e| ObsError::Appender(e.to_string()))?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let targets = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("hiker_core", LevelFilter::DEBUG)
        .with_target("hiker", LevelFilter::DEBUG);

    let stderr_layer = fmt::layer().with_writer(std::io::stderr).compact();
    let file_layer = fmt::layer()
        .with_writer(file_writer)
        // Log files are read with `less` / `rg` — ANSI escapes there are
        // noise. Stderr keeps colors on (terminal default).
        .with_ansi(false)
        .compact();

    tracing_subscriber::registry()
        .with(targets)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|e| ObsError::Init(e.to_string()))?;

    let _ = GUARD.set(guard);
    let _ = INIT.set(());
    Ok(())
}
