//! Settings-shaped Tauri commands.
//!
//! Reads, writes, scope-isolated reads, manual reload, OS-reveal, and the
//! user-scope `vault.default` lookup that backs the bootstrap auto-open.
//!
//! `restart_mcp_server` is the bind-restart helper invoked by
//! `set_setting_inner` when a `[mcp]` field that affects the axum bind
//! changes; it lives here because it's a pure settings-write side effect.
//!
//! status: settings-load-once-at-startup, settings-write-back,
//! settings-pane-scope-toggle, settings-pane-manual-refresh,
//! settings-pane-open-toml-link, settings-default-vault-autoopen,
//! mcp-bind-host-configurable, embedder-hot-reload-on-model-change

use std::time::Instant;

use hiker_core::config::{Config, SettingsScope};
use hiker_core::indexer::IndexJob;
use hiker_core::HikerError;
use tauri::State;

use crate::cmds::vault::reveal_path;
use crate::{log_cmd_result, AppState};

/// Snapshot of the active vault's merged settings. Frontend uses this on
/// vault open to seed View menu / tree-state defaults.
///
/// status: settings-load-once-at-startup
#[tauri::command]
pub(crate) fn get_settings(state: State<AppState>) -> Result<Config, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let cfg = session
            .config
            .read()
            .map_err(|_| "config lock poisoned".to_string())?;
        Ok(cfg.clone())
    })();
    log_cmd_result("get_settings", result)
}

/// Persist a single setting. The eligible-key set is closed (see
/// `core::config::ELIGIBLE_*`); anything not in it is rejected.
///
/// Concurrency: `Config::set` does file IO + reload outside the session
/// lock, then we re-acquire the write lock to swap the in-memory copy.
/// Two concurrent flips can therefore race so the older reload wins. In
/// practice users flip one toggle at a time, and the next set_setting
/// reload will reconverge — not worth a global write mutex for now.
///
/// status: settings-write-back
#[tauri::command]
pub(crate) async fn set_setting(
    state: State<'_, AppState>,
    scope: SettingsScope,
    key: String,
    value: serde_json::Value,
) -> Result<Config, HikerError> {
    let result = set_setting_inner(state, scope, key, value).await;
    log_cmd_result("set_setting", result)
}

async fn set_setting_inner(
    state: State<'_, AppState>,
    scope: SettingsScope,
    key: String,
    value: serde_json::Value,
) -> Result<Config, HikerError> {
    // Snapshot the previous mcp config (for the bind-restart decision)
    // and the vault root before doing any disk I/O. Also capture the
    // previous `indexing.model` so the `embedder-hot-reload-on-model-change`
    // branch below can decide whether to ship the reload job.
    let (root, prev_mcp, prev_indexing_model, reload_tx) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let cfg = session
            .config
            .read()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        (
            session.root.clone(),
            cfg.mcp.clone(),
            cfg.indexing.model.clone(),
            session.indexer.job_sender(),
        )
    };
    // status: embedder-hot-reload-on-model-change
    // For `indexing.model` flips, drive the embedder hot-reload *before*
    // any TOML write so a load failure rolls back cleanly — the on-disk
    // value never disagrees with the loaded model. Same-value writes
    // short-circuit and fall through to the regular path (which itself
    // no-ops if the file is unchanged).
    if key == "indexing.model" {
        if let Some(new_id) = value.as_str() {
            if new_id != prev_indexing_model {
                // Validate the id before paying the load cost. Unknown
                // models bail with a clean Config error and no side
                // effects; matches the `is_known_model` check in
                // `core::config` strict-load.
                if !hiker_core::embed::is_known_model(new_id) {
                    return Err(HikerError::Config(format!(
                        "unknown embedder model: {new_id}"
                    )));
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                reload_tx
                    .send(IndexJob::ReloadEmbedder {
                        model_id: new_id.to_string(),
                        reply: tx,
                    })
                    .await
                    .map_err(|_| HikerError::Config("indexer unavailable".into()))?;
                match rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        return Err(HikerError::Config(
                            "indexer dropped reload reply".into(),
                        ))
                    }
                }
            }
        }
    }
    let updated = Config::set(scope, &key, value, &root)?;
    // Suppress the config-file watcher for a short window after this write
    // so the resulting fs event doesn't round-trip back through
    // `Config::load` -> `hiker:config-reloaded`.
    {
        if let Ok(mut guard) = state.config_last_write.lock() {
            *guard = Some(Instant::now());
        }
    }
    // Apply the live in-memory updates (config swap, queue cfg,
    // mcp tool gates).
    {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated.clone();
        drop(w);
        session.tasks.set_cfg(updated.tasks.clone());
        match session.mcp_tools.write() {
            Ok(mut tools) => *tools = updated.mcp.tools.clone(),
            Err(_) => {} // poisoned — best-effort
        };
        match session.staging_config.write() {
            Ok(mut s) => *s = updated.staging.clone(),
            Err(_) => {} // poisoned — best-effort
        };
    }
    // status: mcp-bind-host-configurable
    // Bind-affecting change → tear the MCP server down and start it
    // back up so port/host/discovery-file flips apply without a vault
    // re-open. `mcp.tools.*` is already live via the shared RwLock so
    // it's excluded; everything else in `[mcp]` (enabled, host, port,
    // discovery_file, max_top_k, audit.log_full_input) takes effect
    // through a server restart.
    let bind_changed = prev_mcp.enabled != updated.mcp.enabled
        || prev_mcp.host != updated.mcp.host
        || prev_mcp.port != updated.mcp.port
        || prev_mcp.discovery_file != updated.mcp.discovery_file
        || prev_mcp.max_top_k != updated.mcp.max_top_k
        || prev_mcp.audit.log_full_input != updated.mcp.audit.log_full_input;
    if bind_changed {
        restart_mcp_server(&state, &updated).await;
    }
    Ok(updated)
}

/// Tear down the existing in-process MCP server (if any) and bring up a
/// fresh one against the latest config. Failures during the restart log
/// at warn but don't propagate — a stale-but-running server is worse
/// UX than the user thinking their toggle didn't apply, but a setting
/// flip shouldn't kill the whole vault session.
///
/// status: mcp-bind-host-configurable
async fn restart_mcp_server(state: &State<'_, AppState>, updated: &Config) {
    // Pull the deps we need under the sync lock, then drop it before
    // any `.await` so we don't hold a std::sync::Mutex across the
    // bind. Take the old handle out of the session first so its `Drop`
    // (which cancels the axum task and removes the discovery file)
    // fires before we attempt to bind the new one.
    let restart_inputs = {
        let mut guard = match state.session.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(session) = guard.as_mut() else { return };
        let old = session.mcp.take();
        let inputs = (
            session.vault.clone(),
            session.root.clone(),
            session.indexer.job_sender(),
            session.indexer.embedder_provider(),
            session.read_store.clone(),
            session.watcher.clone(),
            session.changes.clone(),
            session.audit.clone(),
            session.tasks.clone(),
            session.mcp_tools.clone(),
            session.staging.clone(),
        );
        drop(old);
        inputs
    };
    if !updated.mcp.enabled {
        return;
    }
    let (vault, root, jobs, embedder_provider, read_store, watcher, changes, audit, tasks, mcp_tools, staging) =
        restart_inputs;
    let deps = hiker_mcp::McpDeps {
        vault,
        vault_root: root,
        read_store,
        jobs,
        watcher,
        changes,
        embedder_provider,
        config: updated.mcp.clone(),
        tools: mcp_tools,
        audit,
        tasks,
        tasks_config: updated.tasks.clone(),
        llm_enabled: updated.llm.enabled,
        staging,
    };
    match hiker_mcp::start(deps).await {
        Ok(handle) => {
            if let Ok(mut guard) = state.session.lock() {
                if let Some(session) = guard.as_mut() {
                    session.mcp = Some(handle);
                }
            }
        }
        Err(hiker_mcp::StartError::Disabled) => {}
        Err(e) => {
            tracing::warn!(error = %e, "mcp: restart failed");
        }
    }
}

/// Read a single TOML scope's contents (user or vault) without merging or
/// triggering auto-create. Backs the settings pane's per-section scope
/// toggle: each section card shows the values that the *currently-displayed
/// file alone* would contribute. Missing file → `Config::default()`.
///
/// status: settings-pane-scope-toggle
#[tauri::command]
pub(crate) fn get_settings_scoped(
    state: State<AppState>,
    scope: SettingsScope,
) -> Result<Config, HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Config("no vault open".into()))?;
            session.root.clone()
        };
        Config::read_file_only(scope, &root)
    })();
    log_cmd_result("get_settings_scoped", result)
}

/// Force `Config::load` to re-run and swap the in-memory copy. Backs the
/// settings pane's manual-refresh affordance for the "user hand-edited the
/// TOML while the pane was open" case.
///
/// status: settings-pane-manual-refresh
#[tauri::command]
pub(crate) fn reload_config(state: State<AppState>) -> Result<Config, HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Config("no vault open".into()))?;
            session.root.clone()
        };
        let updated = Config::load(&root)?;
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated.clone();
        drop(w);
        session.tasks.set_cfg(updated.tasks.clone());
        if let Ok(mut tools) = session.mcp_tools.write() {
            *tools = updated.mcp.tools.clone();
        }
        if let Ok(mut s) = session.staging_config.write() {
            *s = updated.staging.clone();
        }
        Ok(updated)
    })();
    log_cmd_result("reload_config", result)
}

/// Resolve the absolute path of one config TOML and reveal it in the OS file
/// manager. Used by the settings pane's "Open user/vault config.toml"
/// affordances and the read-only-row popovers.
///
/// status: settings-pane-open-toml-link
#[tauri::command]
pub(crate) fn reveal_config_file(
    state: State<AppState>,
    scope: SettingsScope,
) -> Result<(), HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Io("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Io("no vault open".into()))?;
            session.root.clone()
        };
        let paths = hiker_core::config::ConfigPaths::resolve(&root);
        let abs = match scope {
            SettingsScope::User => paths
                .user
                .ok_or_else(|| HikerError::Config("no platform config dir available".into()))?,
            SettingsScope::Vault => paths.vault,
        };
        // Settings TOMLs auto-create on first `Config::load`, so a vault
        // that was opened normally already has both files. If the user TOML
        // dir was unresolvable that branch errored above; this fallback
        // covers the rare "file deleted between open and reveal" case.
        let target = if abs.exists() {
            abs
        } else if let Some(parent) = abs.parent() {
            parent.to_path_buf()
        } else {
            abs
        };
        reveal_path(&target).map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("reveal_config_file", result)
}

/// Read-only lookup of the user-scope `vault.default` field. Used by the
/// frontend bootstrap to decide whether to auto-open a configured default
/// vault before falling through to the JS-side folder picker. Returns
/// `Ok(None)` when no default is set or the user TOML doesn't exist yet;
/// real I/O / parse failures bubble up as `Err`.
///
/// status: settings-default-vault-autoopen
#[tauri::command]
pub(crate) fn get_default_vault() -> Result<Option<String>, String> {
    log_cmd_result(
        "get_default_vault",
        hiker_core::config::Config::user_default_vault().map_err(|e| e.to_string()),
    )
}
