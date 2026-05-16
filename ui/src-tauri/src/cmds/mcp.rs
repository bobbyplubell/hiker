//! MCP server + config-file watcher + frontend log bridge.
//!
//! Bundles three cross-cutting bridges that were inline in `lib.rs`:
//! - `start_mcp` — wires the in-process MCP server against a vault
//!   session's handles. Called from `cmds::bootstrap` on vault open
//!   and on `restart_mcp_server` from `cmds::settings`.
//! - `start_config_watcher` — background task that reloads the merged
//!   `Config` when either of the two TOML files changes on disk.
//! - `log_from_frontend` — `#[tauri::command]` that pipes webview
//!   `tracing` events into the same logfile as the Rust half.
//!
//! status: mcp-server-crate, obs-frontend-bridge

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use notify::{Event as NotifyEvent, EventKind as NotifyEventKind, Watcher as NotifyWatcher, RecursiveMode};
use serde::Deserialize;
use tauri::Manager;

use hiker_core::changes::Changes;
use hiker_core::config::Config;
use hiker_core::indexer::IndexerHandle;
use hiker_core::staging::Staging;
use hiker_core::store::Store;
use hiker_core::watcher::Watcher;
use hiker_core::Vault;

use crate::AppState;

// ---------- frontend-bridge logger ----------

/// status: obs-frontend-bridge
/// Wire-side level enum for the `log_from_frontend` command. Tagged via
/// serde's snake_case so the JS payload `{ level: "error", ... }` round-trips
/// without an extra string match — Tauri's serde-driven arg deserialization
/// rejects garbage at the seam rather than at a `match` inside the body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// status: obs-frontend-bridge
/// Pipe a structured event from the webview into the unified `tracing`
/// stream so `vault/.hiker/logs/hiker.log` is the single log file for both
/// halves of the app.
///
/// The `target` is constrained by convention to the `ui::` prefix — anything
/// else is rewritten to `ui::bad_target` and a `bad_target` field is recorded
/// rather than rejecting; the bridge should never become the reason a UI
/// error is lost. Each `fields` entry is flattened as a stringified key/value
/// pair on the event (matching the `error = %e` shape used in core).
///
/// Discipline: callers (the `Logger` wrapper in `ui/src/logger.ts`) MUST NOT
/// pass note body text, embeddings, or auth tokens through `fields`. Same
/// `obs-no-content` / `obs-no-secrets` rule that applies to every other
/// event in the system.
#[tauri::command]
pub(crate) fn log_from_frontend(
    level: LogLevel,
    target: String,
    message: String,
    fields: serde_json::Value,
) {
    // Flatten `fields` (expected to be an object) into a single compact JSON
    // string so the event carries one structured `fields` value rather than a
    // dynamic field set — `tracing::event!` field names are `'static` and
    // can't be built from a runtime map. Compact JSON keeps grep behavior
    // sane: fields land as `fields={"command":"open_vault_at",...}` in the
    // log line.
    let fields_str = match &fields {
        serde_json::Value::Object(_) => fields.to_string(),
        serde_json::Value::Null => "{}".to_string(),
        // Non-object payload is a caller bug; log as-is so it's grep-able
        // rather than dropping it.
        other => other.to_string(),
    };

    let target_str = if target.starts_with("ui::") {
        target.as_str()
    } else {
        // Stay in the `ui::` namespace so log filtering by target keeps
        // working even when a caller passes the wrong shape.
        "ui::bad_target"
    };
    let bad_target: Option<&str> = if target.starts_with("ui::") {
        None
    } else {
        Some(target.as_str())
    };

    match level {
        LogLevel::Trace => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::TRACE,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Debug => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::DEBUG,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Info => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::INFO,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Warn => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::WARN,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Error => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::ERROR,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
    }
}

/// Wire the MCP server up against the vault session's handles. The server
/// task lives until the returned handle is dropped (which happens when the
/// `VaultSession` containing it is dropped — i.e. on vault swap or app
/// shutdown).
///
// Each parameter maps slot-by-slot onto a distinct field of
// `hiker_mcp::McpDeps`; a wrapper struct here would just duplicate that
// shape. The single call site lives next to the existing `SessionHandle`
// bundle in bootstrap.rs, so allow the lint locally.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_mcp(
    vault: &Vault,
    root: &PathBuf,
    indexer: &IndexerHandle,
    watcher: &Arc<Watcher>,
    changes: &Arc<Changes>,
    read_store: &Arc<Mutex<Store>>,
    config: &Config,
    audit: &Arc<hiker_core::audit::AgentLog>,
    tasks: &Arc<hiker_core::tasks::Queue>,
    mcp_tools: &Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>>,
    staging: &Arc<Staging>,
) -> Result<hiker_mcp::McpServerHandle, hiker_mcp::StartError> {
    let deps = hiker_mcp::McpDeps {
        vault: vault.clone(),
        vault_root: root.clone(),
        read_store: read_store.clone(),
        jobs: indexer.job_sender(),
        watcher: watcher.clone(),
        changes: changes.clone(),
        embedder_provider: indexer.embedder_provider(),
        config: config.mcp.clone(),
        tools: mcp_tools.clone(),
        audit: audit.clone(),
        tasks: tasks.clone(),
        tasks_config: config.tasks.clone(),
        llm_enabled: config.llm.enabled,
        staging: staging.clone(),
    };
    hiker_mcp::start(deps).await
}

/// Background task: watches both config TOML files for external edits,
/// reloads the merged Config, swaps the in-memory copy, and emits
/// `hiker:config-reloaded` so the frontend re-applies settings.
///
/// Debounced at ~500 ms. Suppressed for 2 s after a `set_setting` write
/// (same `SUPPRESS_TTL` shape as the vault watcher) so UI-driven flips
/// don't round-trip back through the file watcher.
pub(crate) async fn start_config_watcher(
    app: tauri::AppHandle,
    vault_root: PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) {
    use std::collections::HashSet;
    let paths = hiker_core::config::ConfigPaths::resolve(&vault_root);

    // Watch parent directories (notify works more reliably on dirs than
    // non-existent files, and some backends require dirs). Collect unique
    // parents; filter events by exact file path below.
    let mut parent_dirs: HashSet<PathBuf> = HashSet::new();
    parent_dirs.insert(
        paths
            .vault
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| paths.vault.clone()),
    );
    if let Some(ref user) = paths.user {
        parent_dirs.insert(
            user.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| user.clone()),
        );
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Result<NotifyEvent>>(16);

    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.blocking_send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "config watcher: failed to start");
            return;
        }
    };

    for dir in &parent_dirs {
        if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
            tracing::warn!(error = %e, dir = %dir.display(), "config watcher: failed to watch");
        }
    }

    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
    const SUPPRESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    let mut last_reload = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            ev = rx.recv() => {
                match ev {
                    Some(Ok(event)) => {
                        // Only care about Modify / Create events on our config files.
                        let relevant = matches!(
                            event.kind,
                            NotifyEventKind::Modify(_) | NotifyEventKind::Create(_)
                        );
                        if !relevant {
                            continue;
                        }
                        let hits_config = event.paths.iter().any(|p| {
                            p == &paths.vault
                                || paths.user.as_ref().map_or(false, |u| p == u)
                        });
                        if !hits_config {
                            continue;
                        }

                        // Debounce: skip rapid bursts from a single save.
                        if last_reload.elapsed() < DEBOUNCE {
                            continue;
                        }

                        // Suppress: skip if `set_setting` wrote recently.
                        {
                            let suppressed = app.state::<AppState>()
                                .config_last_write
                                .lock()
                                .map_or(false, |g| g.map_or(false, |t| t.elapsed() < SUPPRESS_WINDOW));
                            if suppressed {
                                continue;
                            }
                        }

                        last_reload = Instant::now();

                        match hiker_core::config::Config::load(&vault_root) {
                            Ok(config) => {
                                // Swap in-memory copy + live mirrors.
                                let state = app.state::<AppState>();
                                if let Ok(guard) = state.session.lock()
                                    && let Some(session) = guard.as_ref()
                                {
                                    if let Ok(mut w) = session.config.write() {
                                        *w = config.clone();
                                    }
                                    session.tasks.set_cfg(config.tasks.clone());
                                    if let Ok(mut tools) = session.mcp_tools.write() {
                                        *tools = config.mcp.tools.clone();
                                    }
                                    if let Ok(mut s) = session.staging_config.write() {
                                        *s = config.staging.clone();
                                    }
                                }
                                crate::events::emit_config_reloaded(&app, &config);
                                tracing::debug!("config watcher: reloaded, emitted hiker:config-reloaded");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "config watcher: Config::load failed");
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "config watcher: notify error");
                    }
                    None => break,
                }
            }
        }
    }
}
