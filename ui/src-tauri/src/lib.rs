mod chat;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use notify::{Event as NotifyEvent, EventKind as NotifyEventKind, Watcher as NotifyWatcher, RecursiveMode};

use hiker_core::autosave::{Autosave, RecoveredEntry, TabState};
use hiker_core::changes::{ChangeOp, ChangeRow, Changes};
use hiker_core::config::{Config, SettingsScope, TreeSortBy};
use hiker_core::indexer::{
    route_watcher_events, start_indexer, IndexJob, IndexStatus, IndexerHandle, ProgressEvent,
};
use hiker_core::search::{self, LexicalOpts, SearchModes, SearchResponse, SemanticOpts};
use hiker_core::staging::{Staging, AcceptOutcome, Proposal, StagingFilter};
use hiker_core::store::{ChunkBounds, RecentNote, RelatedHit, Store, VaultStats};
use hiker_core::trash::{Trash, TrashEntry, TrashListItem};
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, DirEntryDto, HikerError, Vault};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

/// All long-lived state for an open vault. Constructed in `open_vault_at`,
/// dropped on swap.
pub(crate) struct VaultSession {
    vault: Vault,
    pub(crate) root: PathBuf,
    indexer: IndexerHandle,
    /// Held to keep the watcher alive; dropping this closes the broadcast.
    /// Also referenced by `create_note` / `move_note` to register self-write
    /// suppression around fs mutations. Wrapped in `Arc` so the mutating
    /// commands (which call `core::ops::*`) can clone a cheap handle out
    /// from under the session lock and pass it across the indexer-reply
    /// `.await` without holding the sync mutex across it.
    watcher: Arc<Watcher>,
    /// status: changes-write-path
    /// Append-only changelog. Shared writer (single mutex inside `Changes`)
    /// across every mutating command site so all writes flow into one file.
    /// Subscribed by a tokio task that re-emits each append as
    /// `hiker:changes-appended` for the home-page activity widget.
    changes: Arc<Changes>,
    /// Staging area for proposed writes (see docs/settings.md "## Staging
    /// review"). Created at vault open, passed to the MCP server so write
    /// tools can route proposals through it when `[mcp.tools].review_required`
    /// is true.
    ///
    /// status: agent-write-review-mode
    pub(crate) staging: Arc<Staging>,
    /// status: autosave-backend-module
    /// Owns all `<vault>/.hiker/autosave/` writes and recovery. Same
    /// module-discipline shape as `core::store` / `core::changes` —
    /// every Tauri `autosave_*` command wraps a 5–15 line call into
    /// this handle.
    autosave: Arc<Autosave>,
    /// status: settings-load-once-at-startup
    /// Frozen merged user+vault settings. `set_setting` writes through to
    /// disk via `Config::set` and swaps the in-memory copy in this RwLock.
    pub(crate) config: RwLock<Config>,
    /// Long-lived read-side store handle. The indexer task owns the writer
    /// connection (one per vault); this is a *second* connection against
    /// the same on-disk db, used by every read-side Tauri command
    /// (`index_state_for`, `chunks_for`, `related_notes`) so they don't
    /// pay sqlite/PRAGMA/sqlite-vec setup cost on every call.
    ///
    /// Safe to coexist with the writer: WAL mode is per-file, the
    /// sqlite-vec auto-extension is registered process-once via `Once`,
    /// and `ensure_schema` is idempotent.
    ///
    /// Wrapped in `Mutex` because `rusqlite::Connection` is `Send` but not
    /// `Sync`. Read calls are sub-millisecond so serializing them through
    /// the mutex is fine; if read concurrency ever matters, swap this for
    /// an `r2d2` pool — `core::store` confines all SQL so the change is
    /// local.
    ///
    /// Convention only: nothing in the type prevents a writer call. Read
    /// commands stick to the `&self` methods on `Store`.
    read_store: Arc<Mutex<Store>>,
    /// status: mcp-server-crate
    /// In-process MCP server task — bound on vault open, dropped on swap.
    /// `None` when the vault is opened with `[mcp] enabled = false` or when
    /// the bind failed (logged but non-fatal — vault open still succeeds).
    /// Held purely for its `Drop` side effect (cancel the task + remove the
    /// discovery file); never read directly.
    pub(crate) mcp: Option<hiker_mcp::McpServerHandle>,
    /// status: agent-chat-command-surface
    /// Per-turn live state for the basic agent loop (`core::agent`). See
    /// `chat.rs`. Outlives any single `chat_send`/`chat_continue` call so
    /// cap-hit pauses can be resumed mid-session.
    pub(crate) chat: Arc<chat::ChatRegistry>,
    /// status: llm-prompts-file-store
    /// Loaded once at vault open and shared across every chat turn so we
    /// don't re-read disk on every `chat_send`. The user-edited prompt
    /// file is the authoritative surface; relaunching hiker picks up
    /// changes (matches the rest of the settings-load-once-at-startup
    /// discipline).
    pub(crate) prompts: Arc<hiker_core::prompts::Prompts>,
    /// status: llm-audit-log
    /// Shared JSONL audit-log writer. Every LLM-driven surface
    /// (`core::agent`, `core::llm`, MCP tool calls) records through
    /// this single writer so all rows land in one daily file. See
    /// `core::audit`.
    pub(crate) audit: Arc<hiker_core::audit::AgentLog>,
    /// status: task-queue-core-module
    /// Shared work queue for non-interactive LLM jobs. Plumbed into the
    /// MCP server (so external rmcp clients + the basic chat agent reach
    /// the same `task_*` surface) and drained by the in-process direct
    /// worker.
    pub(crate) tasks: Arc<hiker_core::tasks::Queue>,
    /// CancellationToken used to wind down the direct worker + queue
    /// maintenance task on vault swap. Dropped with the session.
    pub(crate) tasks_cancel: tokio_util::sync::CancellationToken,
    /// CancellationToken that stops the config-file watcher task on vault
    /// swap. Dropped with the session; the watcher task selects on this.
    pub(crate) config_watcher_cancel: tokio_util::sync::CancellationToken,
    /// status: mcp-tool-toggles
    /// Shared `[mcp.tools]` config — also held by the MCP handler so
    /// per-tool toggles apply live. Mutated by `set_setting` /
    /// `reload_config`.
    pub(crate) mcp_tools: Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>>,
}

impl Drop for VaultSession {
    fn drop(&mut self) {
        // Stop the direct worker + queue maintenance/event-pump tasks.
        // Safe to call multiple times — `CancellationToken::cancel` is
        // idempotent.
        self.tasks_cancel.cancel();
        // Stop the config-file watcher task.
        self.config_watcher_cancel.cancel();
    }
}

pub(crate) struct AppState {
    pub(crate) session: Mutex<Option<VaultSession>>,
    /// Suppression timestamp for the config-file watcher so
    /// `set_setting` writes don't round-trip back through the file
    /// watcher and re-fire `hiker:config-reloaded`.
    pub(crate) config_last_write: Mutex<Option<Instant>>,
}

fn with_vault<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Vault) -> Result<R, String>,
) -> Result<R, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(&session.vault)
}

/// Log an `Err(_)` returned to the frontend, then pass the Result through
/// unchanged. Wrap a command's final expression in this so every failure
/// shows up in the unified log without scattering `tracing::error!` calls
/// across each `.map_err` chain. Per `obs-error-context`: the error chain
/// rides the `error` field, the message stays grep-stable.
fn log_cmd_result<T, E: std::fmt::Display>(
    command: &'static str,
    r: Result<T, E>,
) -> Result<T, E> {
    if let Err(e) = &r {
        tracing::error!(error = %e, command, "tauri command failed");
    }
    r
}

#[tauri::command]
fn list_dir(
    state: State<AppState>,
    rel: String,
    sort: Option<TreeSortBy>,
) -> Result<Vec<DirEntryDto>, String> {
    let result = (|| -> Result<Vec<DirEntryDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let order = match sort {
            Some(o) => o,
            None => session
                .config
                .read()
                .map_err(|_| "config lock poisoned".to_string())?
                .vault
                .tree
                .sort_by,
        };
        session.vault.list_dir(&rel, order).map_err(|e| e.to_string())
    })();
    log_cmd_result("list_dir", result)
}

#[tauri::command]
fn read_file(state: State<AppState>, rel: String) -> Result<String, String> {
    log_cmd_result(
        "read_file",
        with_vault(&state, |v| v.read_file(&rel).map_err(|e| e.to_string())),
    )
}

#[derive(Serialize)]
struct FileWithHash {
    contents: String,
    hash: String,
}

#[tauri::command]
fn read_file_with_hash(state: State<AppState>, rel: String) -> Result<FileWithHash, String> {
    log_cmd_result(
        "read_file_with_hash",
        with_vault(&state, |v| {
            v.read_file_with_hash(&rel)
                .map(|(contents, hash)| FileWithHash { contents, hash })
                .map_err(|e| e.to_string())
        }),
    )
}

/// status: note-mutation-stash-changes-tag
/// Build the `metadata` JSON for a save's changes-row. Frontend may pass
/// `extra_metadata` to stamp one-shot context (e.g.
/// `{ "mutation": "<kind>" }` for the save that accepts an in-buffer
/// mutation). Object inputs are taken as-is; non-object / `None` falls
/// back to the empty object — same default as before this hook landed.
fn merge_extra_metadata(extra: Option<serde_json::Value>) -> serde_json::Value {
    match extra {
        Some(serde_json::Value::Object(_)) => extra.unwrap(),
        _ => serde_json::json!({}),
    }
}

#[tauri::command]
fn write_file(
    state: State<AppState>,
    rel: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let abs = session
            .vault
            .abs_path(&rel)
            .map_err(|e| e.to_string())?;
        let existed = abs.exists();
        // Baseline-on-first-save: if the file already existed but the
        // changelog has no row for it, snapshot the pre-write state so
        // rollback of this save has somewhere to go. Read failures fall
        // through silently — better to log a hash-less save than refuse
        // the write.
        if existed {
            if let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel) {
                if let Err(e) = session.changes.ensure_baseline(
                    &rel,
                    "user",
                    pre_text.as_bytes(),
                    &pre_hash,
                ) {
                    tracing::warn!(error = %e, "changes: ensure_baseline failed");
                }
            }
        }
        session
            .vault
            .write_file(&rel, &contents)
            .map_err(|e| e.to_string())?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        let hash = hiker_core::hash_str(&contents);
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file) failed");
        }
        Ok(())
    })();
    log_cmd_result("write_file", result)
}

/// Open `rel` for editing — read its bytes and mint an opaque
/// `BufferToken`. The UI seeds CM6 with `contents` and round-trips the
/// token verbatim through `commit_buffer`; it never holds the hash.
#[tauri::command]
fn open_for_edit(
    state: State<AppState>,
    rel: String,
) -> Result<hiker_core::ops::OpenForEditOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::open_for_edit(&session.vault, &rel)
    })();
    log_cmd_result("open_for_edit", result)
}

/// Commit a buffer's new text using the drift-check encoded in `token`.
/// Returns `Written { new_hash, token }` on success or `DriftDetected
/// { current_disk_text, current_hash }` on conflict — the UI shows its
/// modal and dispatches to `resolve_drift`.
#[tauri::command]
fn commit_buffer(
    state: State<AppState>,
    token: hiker_core::ops::BufferToken,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::CommitOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::commit_buffer(
            &session.vault,
            Some(&session.changes),
            &token,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("commit_buffer", result)
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the UI; this is the typed action surface.
#[tauri::command]
fn resolve_drift(
    state: State<AppState>,
    rel: String,
    choice: hiker_core::ops::DriftChoice,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::DriftResolution, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::resolve_drift(
            &session.vault,
            Some(&session.changes),
            &rel,
            choice,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("resolve_drift", result)
}

#[tauri::command]
fn write_file_checked(
    state: State<AppState>,
    rel: String,
    expected_hash: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        // Detect created-vs-modified before the write. The drift check
        // upstream means `expected_hash` is empty for first-write (file
        // missing); after the write we tag the row accordingly.
        let abs = session.vault.abs_path(&rel)?;
        let existed = abs.exists();
        // Baseline-on-first-save: snapshot the pre-write content before
        // overwriting so rollback of this save restores the prior state.
        // No-op when the changelog already has a row for this path.
        if existed {
            if let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel) {
                if let Err(e) = session.changes.ensure_baseline(
                    &rel,
                    "user",
                    pre_text.as_bytes(),
                    &pre_hash,
                ) {
                    tracing::warn!(error = %e, "changes: ensure_baseline failed");
                }
            }
        }
        let new_hash = session
            .vault
            .write_file_checked(&rel, &expected_hash, &contents)?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file_checked) failed");
        }
        Ok(new_hash)
    })();
    log_cmd_result("write_file_checked", result)
}

/// Snapshot of the active vault's merged settings. Frontend uses this on
/// vault open to seed View menu / tree-state defaults.
///
/// status: settings-load-once-at-startup
#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<Config, String> {
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
async fn set_setting(
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
    // and the vault root before doing any disk I/O.
    let (root, prev_mcp) = {
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
        (session.root.clone(), cfg.mcp.clone())
    };
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
fn get_settings_scoped(
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
fn reload_config(state: State<AppState>) -> Result<Config, HikerError> {
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
fn reveal_config_file(
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
fn get_default_vault() -> Result<Option<String>, String> {
    log_cmd_result(
        "get_default_vault",
        hiker_core::config::Config::user_default_vault().map_err(|e| e.to_string()),
    )
}

/// Open the vault at `path`. Single shared entry point for the frontend's
/// "Open vault" flow, the bootstrap auto-open path, and (eventually) CLI
/// / MCP entry points. The folder picker is *not* a backend concern —
/// the frontend uses `@tauri-apps/plugin-dialog` from JS when it needs
/// one. A path that no longer resolves returns `HikerError::NotFound` so
/// the bootstrap path can react with a toast + fall-through to picker
/// rather than auto-clearing the setting.
///
/// status: settings-default-vault-autoopen
#[tauri::command]
async fn open_vault_at(
    app: tauri::AppHandle,
    path: String,
) -> Result<String, HikerError> {
    log_cmd_result("open_vault_at", open_vault_at_inner(app, PathBuf::from(path)).await)
}

async fn open_vault_at_inner(
    app: tauri::AppHandle,
    path_buf: PathBuf,
) -> Result<String, HikerError> {
    if !path_buf.is_dir() {
        tracing::warn!(
            path = %path_buf.display(),
            "open_vault_at: path does not resolve to a directory",
        );
        return Err(HikerError::NotFound(path_buf.display().to_string()));
    }
    let vault = Vault::open(&path_buf).map_err(|e| HikerError::Io(e.to_string()))?;
    let root = vault.root().to_path_buf();
    let display = root.to_string_lossy().into_owned();

    // Stand up the tracing pipeline (per-vault log files). Idempotent across
    // vault swaps in the same UI session — the first call wins.
    if let Err(e) = hiker_core::observability::init_tracing(&root) {
        // Subscriber init only fails on disk errors or a competing global
        // subscriber; surface it on stderr and keep the vault open. Falling
        // back to no logging is strictly better than refusing to open.
        eprintln!("[hiker] init_tracing failed: {e}");
    }
    tracing::info!(
        vault_root = %root.display(),
        "ui: vault opened",
    );

    // status: settings-load-once-at-startup
    // Read user + vault TOML, merge, validate. Auto-creates either file
    // with the current defaults if missing (settings-auto-create-defaults).
    // Strict-load: any unknown key, type mismatch, or schema-version
    // mismatch aborts here with a clear error.
    let mut config = Config::load(&root)?;

    // Push this vault onto the user-scope `vault.recent` list. Best-effort:
    // if the platform config dir isn't resolvable (sandboxed env), the
    // write fails silently rather than aborting vault open. The returned
    // Config is the freshly-reloaded merged view — adopt it so the in-memory
    // copy in the session matches what's on disk.
    let recent = hiker_core::config::push_recent_vault(&config.vault.recent, &root);
    if recent != config.vault.recent {
        let value = serde_json::Value::Array(
            recent.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
        );
        match Config::set(SettingsScope::User, "vault.recent", value, &root) {
            Ok(updated) => config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.recent"),
        }
    }

    // Persist this vault as the user's default so `bootstrapDefaultVault`
    // auto-opens it on next launch. `vault.recent` alone isn't enough —
    // bootstrap reads `vault.default` per `settings-default-vault-autoopen`.
    let root_str = root.to_string_lossy().to_string();
    if config.vault.default.as_deref() != Some(root_str.as_str()) {
        match Config::set(
            SettingsScope::User,
            "vault.default",
            serde_json::Value::String(root_str),
            &root,
        ) {
            Ok(updated) => config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.default"),
        }
    }

    // status: changes-store-file
    // Open the changelog db (separate from index.db so the index can be
    // regenerated freely while the changelog stays durable). Best-effort:
    // a failed open shouldn't block vault open, but every subsequent
    // append call will silently no-op until next vault swap.
    let changes = Arc::new(
        Changes::open(&root).map_err(|e| HikerError::Io(format!("changes db: {e}")))?,
    );

    // Open the staging area for proposed writes. Created at vault open,
    // lives for the duration of the session. The MCP server references
    // this instance to route write tools through propose() when
    // `[mcp.tools].review_required` is true.
    //
    // status: agent-write-review-mode
    let staging = Arc::new(
        Staging::open(&root).map_err(|e| HikerError::Io(format!("staging: {e}")))?,
    );

    // status: autosave-backend-module, autosave-store-layout
    // Open the per-vault autosave store. Failure is fatal at vault open
    // (a future tick would just keep failing silently otherwise).
    let autosave = Arc::new(
        Autosave::open(&root).map_err(|e| HikerError::Io(format!("autosave: {e}")))?,
    );

    // One-shot retention pass at vault open. Bounds storage without a
    // periodic task; spec calls for "low-priority job from the indexer
    // task, opportunistically when no other work is queued" — vault open
    // is the cheapest such moment.
    if let Err(e) = changes.gc(50) {
        tracing::warn!(error = %e, "changes: gc on open failed");
    }

    // Forward each append to the frontend as `hiker:changes-appended`.
    // Lagging is fine — the home page widget re-fetches `recent` on each
    // notification so a missed event just means one less repaint.
    let app_for_changes = app.clone();
    let mut changes_rx = changes.subscribe();
    tokio::spawn(async move {
        loop {
            match changes_rx.recv().await {
                Ok(row) => {
                    let _ = app_for_changes.emit("hiker:changes-appended", &row);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Open the store (creates .hiker/index.db on first run). This is the
    // writer connection that the indexer task takes ownership of below.
    let store = Store::open(&root).map_err(|e| HikerError::Io(e.to_string()))?;

    // Open a *second* connection against the same db for every read-side
    // Tauri command. WAL mode (set on the writer above) is per-file, so
    // both connections see committed writes without locking; the sqlite-vec
    // extension auto-registers process-once. See `VaultSession.read_store`.
    let read_store =
        Arc::new(Mutex::new(Store::open(&root).map_err(|e| HikerError::Io(e.to_string()))?));

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately.
    let indexer = start_indexer(vault.clone(), store, || {
        FastembedEmbedder::load()
            .map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn hiker_core::embed::Embedder>)
    });

    // Start the filesystem watcher and bridge its events into the indexer.
    let watcher = Arc::new(Watcher::start(&root).map_err(|e| HikerError::Io(e.to_string()))?);
    let watcher_rx = watcher.subscribe();
    let job_sender = indexer.job_sender();
    tokio::spawn(route_watcher_events(watcher_rx, job_sender));

    // status: trail-auto-update-on-note-move
    // Late-bind watcher + changes to the indexer so the trails
    // auto-update path can suppress watcher events around its rewrites
    // and append `core::changes` rows for each touched file.
    indexer.attach_watcher(watcher.clone());
    indexer.attach_changes(changes.clone());

    // Forward watcher events to the frontend so the editor's drift logic
    // sees them too. Separate subscription so a slow consumer on one side
    // doesn't lag the other.
    let app_for_files = app.clone();
    let mut file_rx = watcher.subscribe();
    tokio::spawn(async move {
        loop {
            match file_rx.recv().await {
                Ok(FileEvent::Overflow) => {
                    let _ = app_for_files.emit("hiker:watcher-overflow", ());
                }
                Ok(ev) => {
                    let _ = app_for_files.emit("hiker:file-changed", &ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Forward indexer progress events to the frontend.
    let app_for_progress = app.clone();
    let mut progress_rx = indexer.subscribe_progress();
    tokio::spawn(async move {
        loop {
            match progress_rx.recv().await {
                Ok(ev) => {
                    let _ = app_for_progress.emit("hiker:reindex-progress", &ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Forward status snapshots to the frontend so it can drop its 2s poll.
    // Emit the seeded value first (queued/total_notes/last_error are populated
    // before the indexer task even runs), then on every change.
    let app_for_status = app.clone();
    let mut status_rx = indexer.subscribe_status();
    tokio::spawn(async move {
        let _ = app_for_status.emit("hiker:index-status", &*status_rx.borrow_and_update());
        while status_rx.changed().await.is_ok() {
            let _ = app_for_status.emit("hiker:index-status", &*status_rx.borrow_and_update());
        }
    });

    // Kick the initial scan. Returns immediately; jobs flow as the model
    // load completes.
    let _ = indexer.full_scan().await;

    // status: llm-audit-log
    // One shared JSONL agent-log writer for every LLM-driven surface in
    // this session (core::agent turns, core::llm direct, mcp-tool-call).
    // `[llm.audit] log_full_prompt` mirrors the obs-no-content default;
    // callers that carry user content (the MCP wrapper) consult the
    // toggle before stuffing bodies into `details`. Constructed before
    // `start_mcp` so the MCP server can share the same writer.
    let audit = Arc::new(hiker_core::audit::AgentLog::new(
        root.join(".hiker").join("agent-log"),
        config.llm.audit.log_full_prompt,
    ));

    // status: task-queue-core-module
    // Stand up the unified work queue and the direct-LLM worker. Always
    // construct the queue so the MCP server can advertise `task_*`
    // (gated separately on `[mcp] enabled`); the direct worker only
    // spawns when both `[llm] enabled` and `[tasks] direct_worker.enabled`
    // are true (per `task-queue-respects-llm-disable`).
    let tasks = Arc::new(hiker_core::tasks::Queue::new(config.tasks.clone()));
    let tasks_cancel = tokio_util::sync::CancellationToken::new();
    {
        // Forward queue events to the frontend.
        let app_for_queue = app.clone();
        let mut rx = tasks.subscribe();
        let cancel = tasks_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Ok(e) => { let _ = app_for_queue.emit("hiker:queue-event", &e); }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        });
        // Maintenance tick: requeue expired leases + GC terminal rows.
        let q_for_tick = tasks.clone();
        let cancel_for_tick = tasks_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_for_tick.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        q_for_tick.tick_maintenance().await;
                    }
                }
            }
        });
        // Direct-LLM worker. Spawned whenever `[llm] enabled = true` —
        // the per-iteration `direct_worker.enabled` check inside
        // `run_direct_worker` honors live toggles from the settings UI
        // without a vault restart. (Spawning still requires `[llm]
        // enabled` because we need a valid LlmClient; flipping LLM on/off
        // remains restart-bound for now.)
        if config.llm.enabled {
            match hiker_core::llm::GraniteLlmClient::from_config(&config.llm) {
                Ok(client) => {
                    let llm_client: Arc<dyn hiker_core::llm::LlmClient> = Arc::new(client);
                    let q = tasks.clone();
                    let audit_for_worker = audit.clone();
                    let cancel = tasks_cancel.clone();
                    let parallelism = config.tasks.direct_worker.parallelism.max(1);
                    for _ in 0..parallelism {
                        let q = (*q).clone();
                        let client = llm_client.clone();
                        let audit = Some(audit_for_worker.clone());
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            hiker_core::tasks::run_direct_worker(q, client, audit, cancel).await;
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "tasks: direct worker not started (llm client build failed)",
                    );
                }
            }
        }
    }

    // status: mcp-tool-toggles
    // Shared per-tool gate config. Held by the MCP handler so dispatches
    // read it live; updated by `set_setting` so flips in the settings UI
    // apply without a vault restart.
    let mcp_tools: Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>> =
        Arc::new(std::sync::RwLock::new(config.mcp.tools.clone()));

    // status: mcp-server-crate
    // Start the in-process MCP server. Failure to bind logs and continues —
    // the user's vault is more important than MCP availability.
    let mcp = match start_mcp(&vault, &root, &indexer, &watcher, &changes, &read_store, &config, &audit, &tasks, &mcp_tools, &staging).await {
        Ok(handle) => Some(handle),
        Err(hiker_mcp::StartError::Disabled) => None,
        Err(e) => {
            tracing::warn!(error = %e, "mcp: start failed");
            None
        }
    };

    // status: llm-prompts-file-store
    // Load the prompt store once at vault open; auto-creates user-scope
    // defaults on first run. Cached on the session so chat_send doesn't
    // re-read disk per turn.
    let prompts = Arc::new(
        hiker_core::prompts::Prompts::load(&root)
            .map_err(|e| HikerError::Io(format!("prompts: {e}")))?,
    );

    // status: llm-prompts-staleness-on-upgrade
    // Surface bundled-default drift once per session. Writes both a
    // tracing warn and an audit-log row per stale feature so the future
    // Prompts-tab can read either surface.
    match hiker_core::prompts::Prompts::staleness(&root) {
        Ok(stale) => {
            for feature in &stale {
                tracing::warn!(
                    feature = %feature,
                    "prompts: bundled default has drifted from the user's stamped hash; review and merge if desired",
                );
                audit.record(&hiker_core::audit::AuditEntry {
                    surface: "core::agent",
                    feature,
                    status: "stale_prompt",
                    error: None,
                    turn_id: None,
                    step_id: None,
                    details: serde_json::json!({
                        "message": "bundled default drifted; user override not clobbered",
                    }),
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "prompts: staleness check failed"),
    }

    // status: llm-providers-config
    // API-key preflight: surface a missing key at vault open rather
    // than waiting for the user's first chat send to fail. Logs *and*
    // emits `hiker:llm-warning` so the frontend can render a user-visible
    // toast. Per the spec's two-source rule, the literal `api_key`
    // (user-scope TOML) takes precedence; if it's set we don't need an
    // env var. Skipped when LLM is disabled or the provider doesn't
    // need a key (Ollama et al — empty `api_key_env` AND empty literal).
    if config.llm.enabled {
        let literal = config.llm.provider.api_key.as_str();
        let env_name = config.llm.provider.api_key_env.as_str();
        let literal_set = !literal.is_empty();
        let env_named_and_unset =
            !env_name.is_empty() && std::env::var(env_name).is_err();
        if !literal_set && env_named_and_unset {
            tracing::warn!(
                env = %env_name,
                backend = %config.llm.provider.backend,
                "llm: no api key — literal unset and env var missing; chat will fail until set",
            );
            let _ = app.emit(
                "hiker:llm-warning",
                serde_json::json!({
                    "kind": "missing_api_key",
                    "env": env_name,
                    "message": format!(
                        "{env_name} unset and no literal api_key — chat will fail until you set one in Settings or your shell",
                    ),
                }),
            );
        }
    }

    let chat_registry = Arc::new(chat::ChatRegistry::default());
    // status: chat-session-resume-latest
    // Adopt the most-recent on-disk session as the active one (if any
    // exist). The registry's `active` slot drives `chat_session_active`,
    // which the frontend calls on vault open to seed the panel.
    if let Err(e) = chat::resume_latest_at_open(&chat_registry, &root, &config) {
        tracing::warn!(error = %e, "sessions: resume_latest_at_open failed");
    }

    // Start the config-file watcher so external edits to either TOML are
    // picked up live and the UI re-applies settings without a restart.
    let config_watcher_cancel = tokio_util::sync::CancellationToken::new();
    {
        let app_for_cw = app.clone();
        let root_for_cw = root.clone();
        let cancel = config_watcher_cancel.clone();
        tokio::spawn(async move {
            start_config_watcher(app_for_cw, root_for_cw, cancel).await;
        });
    }

    let session = VaultSession {
        vault,
        root,
        indexer,
        watcher,
        changes,
        staging,
        autosave,
        config: RwLock::new(config),
        read_store,
        mcp,
        chat: chat_registry,
        prompts,
        audit,
        tasks,
        tasks_cancel,
        config_watcher_cancel,
        mcp_tools,
    };

    let state = app.state::<AppState>();
    *state
        .session
        .lock()
        .map_err(|_| HikerError::Io("session lock poisoned".into()))? = Some(session);
    Ok(display)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IndexScope {
    All,
    Path { rel: String },
}

#[tauri::command]
async fn index(state: State<'_, AppState>, scope: IndexScope) -> Result<(), String> {
    let result = (|| -> Result<(IndexJob, hiker_core::indexer::IndexJobTx), String> {
        let job_sender = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            session.indexer.job_sender()
        };
        let job = match scope {
            // Explicit user-driven reindex: bypass the hash short-circuit so a
            // click on the menu actually re-embeds even when content is unchanged.
            IndexScope::All => IndexJob::FullScan { force: true },
            IndexScope::Path { rel } => IndexJob::Upsert { rel_path: rel, force: true },
        };
        Ok((job, job_sender))
    })();
    let send_result = match result {
        Ok((job, sender)) => sender.send(job).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    log_cmd_result("index", send_result)
}

/// Per-file index state for the tree-row markers and the active-file
/// status-bar mirror. See docs/index.md `tauri-cmd-file-index-state`.
///
/// status: tauri-cmd-file-index-state
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IndexState {
    Indexed,
    Unsupported,
    Skipped { reason: String },
    Queued,
}

#[tauri::command]
fn index_state_for(state: State<AppState>, rel: String) -> Result<IndexState, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        if !hiker_core::indexer::is_indexable_path(&rel) {
            return Ok(IndexState::Unsupported);
        }
        if session.indexer.is_pending(&rel) {
            return Ok(IndexState::Queued);
        }
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        match read_store.get_note_by_path(&rel).map_err(|e| e.to_string())? {
            Some(row) if row.skipped => Ok(IndexState::Skipped {
                reason: row.skip_reason.unwrap_or_else(|| "skipped".into()),
            }),
            Some(_) => Ok(IndexState::Indexed),
            // No row yet for a supported file — either it's about to be indexed
            // or the watcher hasn't surfaced its create event. Either way, the
            // user's mental model is "queued."
            None => Ok(IndexState::Queued),
        }
    })();
    log_cmd_result("index_state_for", result)
}

/// Recursive count of indexable files under a folder. Backs the
/// delete-confirm modal so the UI doesn't have to walk the tree itself
/// via N round-trip `list_dir` calls. Empty vec / 0 for a file path.
/// Filters via `core::indexer::is_indexable_path` so the count matches
/// the indexer's allowlist (md / markdown / txt at v1) — same rule that
/// drives `tauri-cmd-file-index-state`.
#[tauri::command]
fn count_notes_in(state: State<AppState>, rel: String) -> Result<u32, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let files = session.vault.walk_indexable_files(&rel).map_err(|e| e.to_string())?;
        Ok(u32::try_from(files.len()).unwrap_or(u32::MAX))
    })();
    log_cmd_result("count_notes_in", result)
}

/// status: diff-core-module
/// Thin wrapper over `core::diff::compute`. Pure text-in / diff-out — no
/// session lock, no I/O, no async. The UI passes both strings (current
/// buffer text, snapshot blob via `change_content`, derived file via
/// `read_file`, etc.) and renders the returned `DiffResult`.
#[tauri::command]
fn compute_diff(before: String, after: String) -> hiker_core::diff::DiffResult {
    hiker_core::diff::compute(&before, &after)
}

#[tauri::command]
fn index_status(state: State<AppState>) -> Result<IndexStatus, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        Ok(session.indexer.status())
    })();
    log_cmd_result("index_status", result)
}

/// Create a new empty note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed `new-note-N.md` name. Returns the rel path of the
/// file actually created so the UI can open and inline-rename it.
///
/// status: create-note-button
#[tauri::command]
async fn create_note(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    log_cmd_result("create_note", create_note_inner(state, folder).await)
}

async fn create_note_inner(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::create_with_suffix(&watcher, &jobs, &vault, Some(&changes), &folder, "new-note").await
}

/// Atomic note rename. Backs both tree drag-and-drop and inline rename of
/// freshly-created notes. Errors leave both sides untouched per the spec.
///
/// status: drag-and-drop-move
#[tauri::command]
async fn move_note(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_note", move_note_inner(state, from, to).await)
}

async fn move_note_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_note(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Reveal a vault note in the OS file manager (Finder on macOS, Explorer on
/// Windows, default file manager on Linux). Backs the status-bar basename
/// click target.
///
/// status: status-bar-path-reveal
#[tauri::command]
fn reveal_in_file_manager(state: State<AppState>, rel: String) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let abs = session.vault.abs_path(&rel)?;
        reveal_path(&abs).map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("reveal_in_file_manager", result)
}

/// Spawn the platform's reveal-in-file-manager command. Runs the spawn
/// without waiting — the file manager UI is the user's concern, not ours.
#[cfg(target_os = "macos")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg("-R").arg(abs).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", abs.display()))
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    // Linux has no portable "select this file" verb. Open the parent
    // directory in the user's file manager — close enough that the user
    // can pick the file out by sight, no dependency on a specific DE.
    let target = abs.parent().unwrap_or(abs);
    std::process::Command::new("xdg-open").arg(target).spawn()?;
    Ok(())
}

/// Folder rename: fs rename of the whole directory + bulk store path remap
/// for every contained `.md` file. Backs tree drag-and-drop of folder rows.
/// Empty subfolders move with the rename for free.
#[tauri::command]
async fn move_folder(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_folder", move_folder_inner(state, from, to).await)
}

async fn move_folder_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_folder(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Soft-delete a note or folder. Backs the tree context-menu Delete entry
/// (`tree-context-delete`). Mirrors `move_note` shape: suppress watcher,
/// route through the indexer task so all writes go through its owned store
/// connection, await the reply, re-suppress for the post-op TTL window.
///
/// status: delete-note-core-cmd
#[tauri::command]
async fn delete_note(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result("delete_note", delete_note_inner(app, state, rel).await)
}

async fn delete_note_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::delete(&watcher, &jobs, &vault, Some(&changes), &rel).await;
    // Trash bin auto-refresh hook: forgetting this emit breaks
    // `tree-trash-flat-by-deleted` silently, so it stays in the Tauri layer
    // (core::ops doesn't depend on tauri).
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    result
}

/// Restore a previously soft-deleted entry from the vault trash. Backs the
/// undo affordance on the post-delete toast (`tree-context-delete`) and the
/// CLI `hiker trash restore` command.
///
/// status: vault-trash-restore
#[tauri::command]
async fn restore_trash_entry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result(
        "restore_trash_entry",
        restore_trash_entry_inner(app, state, id).await,
    )
}

async fn restore_trash_entry_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, trash, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            Trash::open(session.vault.root()),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::restore(&watcher, &jobs, &vault, Some(&changes), &trash, &id).await;
    // Trash bin auto-refresh hook — kept in the Tauri layer, see
    // `delete_note_inner` for the same rationale.
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    result
}

/// Disk-true listing of the vault trash. Backs the trash bin pinned at the
/// top of the file tree.
///
/// status: tree-trash-disk-listing
#[tauri::command]
fn list_trash(state: State<AppState>) -> Result<Vec<TrashListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.list_from_disk()
    })();
    log_cmd_result("list_trash", result)
}

/// Permanently empty the vault trash. Irrecoverable.
///
/// status: vault-trash-empty
#[tauri::command]
fn empty_trash(app: tauri::AppHandle, state: State<AppState>) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.empty()
    })();
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    log_cmd_result("empty_trash", result)
}

/// Permanently delete a single trash entry by its on-disk basename. Works on
/// orphaned entries too.
///
/// status: tree-trash-restore-action
#[tauri::command]
fn permanent_delete_trash_entry(
    app: tauri::AppHandle,
    state: State<AppState>,
    trashed_name: String,
) -> Result<(), HikerError> {
    tracing::info!(
        command = "permanent_delete_trash_entry",
        trashed_name = %trashed_name,
        "tauri cmd",
    );
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.permanent_delete(&trashed_name)
    })();
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    log_cmd_result("permanent_delete_trash_entry", result)
}

/// Ordered chunk bounds for the active note. Empty vec when the note has
/// no row in the store (unsupported / queued / never indexed) or has zero
/// chunks. Spec: never errors on absence.
///
/// status: tauri-cmd-chunks-for-path
#[tauri::command]
fn chunks_for(state: State<AppState>, rel: String) -> Result<Vec<ChunkBounds>, String> {
    let result = (|| -> Result<Vec<ChunkBounds>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let mut bounds = {
            let read_store = session
                .read_store
                .lock()
                .map_err(|_| "read_store mutex poisoned".to_string())?;
            read_store.chunk_bounds_for(&rel).map_err(|e| e.to_string())?
        };
        // Read the file once and enrich each row's UTF-8 byte offsets with
        // matching UTF-16 char offsets. JS strings (and CM6) index by UTF-16
        // code units, so this saves the frontend from re-doing the encode
        // step every time it wants to map a chunk into the editor.
        if !bounds.is_empty() {
            if let Ok(text) = session.vault.read_file(&rel) {
                hiker_core::store::enrich_char_offsets(&text, &mut bounds);
            }
        }
        Ok(bounds)
    })();
    log_cmd_result("chunks_for", result)
}

/// Hybrid search across the vault. Runs the lexical + semantic backends
/// in parallel (per the requested modes) and returns all three buckets
/// (lexical, semantic, fused). The frontend renders whichever matches
/// its toggle state. Empty query, both modes off, or model-not-yet-ready
/// all return empty buckets without erroring — see
/// `embedder-first-run-nonblocking`.
///
/// status: search-tauri-cmd
#[tauri::command]
async fn search_vault(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,
    epoch: u64,
    lexical_opts: Option<LexicalOpts>,
    semantic_opts: Option<SemanticOpts>,
) -> Result<SearchResponse, String> {
    log_cmd_result(
        "search_vault",
        search_vault_inner(
            state,
            query,
            modes,
            epoch,
            lexical_opts.unwrap_or_default(),
            semantic_opts.unwrap_or_default(),
        )
        .await,
    )
}

async fn search_vault_inner(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,
    epoch: u64,
    lexical_opts: LexicalOpts,
    semantic_opts: SemanticOpts,
) -> Result<SearchResponse, String> {
    // Empty buckets short-circuit: empty query, both modes off, or no
    // session. Each early-return preserves the echoed `epoch` so the
    // frontend's stale-result check still works.
    if query.trim().is_empty() || (!modes.lexical && !modes.semantic) {
        return Ok(SearchResponse {
            epoch,
            lexical_hits: Vec::new(),
            semantic_hits: Vec::new(),
            fused: Vec::new(),
            hits: Vec::new(),
        });
    }
    // Embed the query string (only when semantic is on) on the blocking
    // pool, off the loaded indexer embedder. Per
    // `search-query-embed-spawn-blocking`. Skip entirely when the model
    // isn't ready — search returns empty rather than blocking.
    let embedding: Option<Vec<f32>> = if modes.semantic {
        let embedder = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            session.indexer.embedder()
        };
        match embedder {
            Some(e) => {
                let q = query.clone();
                let res = tokio::task::spawn_blocking(move || e.embed_batch(&[q]))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                res.into_iter().next()
            }
            None => None,
        }
    } else {
        None
    };

    // If semantic was requested but embedding isn't available, run lexical
    // only (still return the requested-modes shape so the panel knows to
    // fall back). Mirrors the spec's "search returns empty with indicator
    // until first batch completes" but keeps any usable hit visible.
    let effective_modes = SearchModes {
        lexical: modes.lexical,
        semantic: modes.semantic && embedding.is_some(),
    };

    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    let read_store = session
        .read_store
        .lock()
        .map_err(|_| "read_store mutex poisoned".to_string())?;
    search::query(
        &read_store,
        epoch,
        effective_modes,
        Some(&query),
        embedding.as_deref(),
        lexical_opts,
        semantic_opts,
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn related_notes(
    state: State<AppState>,
    rel: String,
    top_k: Option<usize>,
) -> Result<Vec<RelatedHit>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let id = match read_store.id_for_path(&rel).map_err(|e| e.to_string())? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        read_store
            .related_notes(&id, top_k.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("related_notes", result)
}

/// Vault home stats payload: cheap counts off the index store, plus the live
/// queued count from the indexer handle. Surfaced by the home page; refreshed
/// on every `hiker:reindex-progress` tick.
///
/// status: vault-home-stats-widget
#[derive(Serialize)]
struct VaultHomeStats {
    total_notes: u32,
    total_chunks: u32,
    indexed: u32,
    skipped: u32,
    queued: u32,
}

#[tauri::command]
fn vault_home_stats(state: State<AppState>) -> Result<VaultHomeStats, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let stats: VaultStats = read_store.vault_stats().map_err(|e| e.to_string())?;
        let queued = session.indexer.status().queued;
        Ok(VaultHomeStats {
            total_notes: stats.total_notes,
            total_chunks: stats.total_chunks,
            indexed: stats.indexed,
            skipped: stats.skipped,
            queued,
        })
    })();
    log_cmd_result("vault_home_stats", result)
}

/// Top-N notes by filesystem mtime DESC. Backs the vault-home recently-modified
/// widget.
///
/// status: vault-home-recent-modified
#[tauri::command]
fn recent_notes_modified(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_mtime(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_modified", result)
}

/// Top-N notes by `last_accessed_at` DESC. Backs the vault-home
/// recently-accessed widget.
///
/// status: vault-home-recent-accessed
#[tauri::command]
fn recent_notes_accessed(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_access(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_accessed", result)
}

/// Stamp `notes.last_accessed_at` via the indexer's owned writer. Called
/// from the frontend whenever a note becomes the active buffer. No-op when
/// the note isn't yet in the index — the next ingest creates the row, and
/// subsequent opens record normally.
///
/// status: note-access-tracking
#[tauri::command]
async fn note_accessed(state: State<'_, AppState>, rel: String) -> Result<(), String> {
    let jobs = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.indexer.job_sender()
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let send_result = jobs
        .send(IndexJob::TouchAccess { rel_path: rel, ts })
        .await
        .map_err(|e| e.to_string());
    log_cmd_result("note_accessed", send_result)
}

// status: note-properties-tab-content
/// DTO returned by `note_properties`. Mirrors `core::store::NoteProperties`
/// plus the changes count from `core::changes`. The struct uses the same
/// `#[serde(rename_all = "camelCase")]` as the core type.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePropertiesDto {
    pub path: String,
    pub note_id: Option<String>,
    pub path_ids_id: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
    pub content_hash: Option<String>,
    pub extension: Option<String>,
    pub indexed_at: Option<i64>,
    pub embedder_version: Option<String>,
    pub skipped: Option<bool>,
    pub skip_reason: Option<String>,
    pub chunk_count: Option<i64>,
    pub last_accessed_at: Option<i64>,
    pub change_count: Option<i64>,
}

#[tauri::command]
fn note_properties(
    state: State<AppState>,
    rel: String,
) -> Result<NotePropertiesDto, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let mut props = read_store
            .note_properties(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("note not indexed: {rel}"))?;
        let change_count = session
            .changes
            .count_for_path(&rel)
            .map_err(|e| e.to_string())?;
        props.change_count = Some(change_count);
        Ok(NotePropertiesDto {
            path: props.path,
            note_id: props.note_id,
            path_ids_id: props.path_ids_id,
            mtime: props.mtime,
            size: props.size,
            content_hash: props.content_hash,
            extension: props.extension,
            indexed_at: props.indexed_at,
            embedder_version: props.embedder_version,
            skipped: props.skipped,
            skip_reason: props.skip_reason,
            chunk_count: props.chunk_count,
            last_accessed_at: props.last_accessed_at,
            change_count: props.change_count,
        })
    })();
    log_cmd_result("note_properties", result)
}

/// Resolve a chat `@<rel-path-without-extension>` token to a concrete
/// vault path + file body. Probes `.md`, `.markdown`, `.txt` in order.
/// Errors with "note not found: <rel>" if no extension resolves — the
/// frontend toasts this and aborts the send (per `chat-input-at-note`).
///
/// status: chat-input-at-note
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtNoteResolved {
    pub rel_path: String,
    pub content: String,
}

#[tauri::command]
fn chat_resolve_at_note(
    state: State<AppState>,
    rel_no_ext: String,
) -> Result<AtNoteResolved, String> {
    let result = (|| -> Result<AtNoteResolved, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let vault = session.vault.clone();
        drop(guard);
        for ext in hiker_core::indexer::INDEXABLE_EXTENSIONS {
            let candidate = format!("{}.{}", rel_no_ext, ext);
            if let Ok(abs) = vault.abs_path(&candidate) {
                if abs.is_file() {
                    if let Ok(content) = vault.read_file(&candidate) {
                        return Ok(AtNoteResolved {
                            rel_path: candidate,
                            content,
                        });
                    }
                }
            }
        }
        Err(format!("note not found: {rel_no_ext}"))
    })();
    log_cmd_result("chat_resolve_at_note", result)
}

/// Notes-table autocomplete for the chat `@`-mention popover. Empty
/// `prefix` returns the most-recently-accessed notes; non-empty filters by
/// case-insensitive basename substring with prefix-matches ranked first.
/// `limit` defaults to 10 to match the spec.
///
/// status: chat-input-at-autocomplete-tauri-cmd
#[tauri::command]
fn chat_at_autocomplete(
    state: State<AppState>,
    prefix: String,
    limit: Option<u32>,
) -> Result<Vec<hiker_core::store::AtSuggestion>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .at_autocomplete(&prefix, limit.unwrap_or(10) as usize)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("chat_at_autocomplete", result)
}

// ---------- changelog query / rollback commands ----------

/// Most recent changelog rows across the whole vault. Backs the home-page
/// recent-activity widget preview and detail view.
///
/// status: vault-home-recent-activity-widget
#[tauri::command]
fn recent_changes(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<ChangeRow>, HikerError> {
    let result = (|| -> Result<Vec<ChangeRow>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .recent(limit.unwrap_or(50))
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("recent_changes", result)
}

/// Total changelog row count. Backs the widget's "any rows yet?" gate so a
/// post-upgrade fresh vault doesn't render a confusing zero-count tile.
#[tauri::command]
fn changes_count(state: State<AppState>) -> Result<i64, HikerError> {
    let result = (|| -> Result<i64, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .count()
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("changes_count", result)
}

/// Pull the post-op content blob for a change. Returns an empty string for
/// `op='deleted'` rows. Decoded as UTF-8 with a fallback to lossy so the
/// detail-view diff renderer always has something to show.
#[tauri::command]
fn change_content(
    state: State<AppState>,
    change_id: i64,
) -> Result<Option<String>, HikerError> {
    let result = (|| -> Result<Option<String>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let blob = session
            .changes
            .content_at(change_id)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        Ok(blob.map(|b| String::from_utf8_lossy(&b).into_owned()))
    })();
    log_cmd_result("change_content", result)
}

/// Roll the file at `change.path` back to the most recent prior content
/// before `change_id`. Implementation per `changes.md` "Rollback":
///
/// 1. Resolve `(prior_id, prior_content)` via `previous_content_for_path`.
/// 2. Write that content via the standard `write_file_checked` path. The
///    write itself appends a *new* `'modified'` row tagged with
///    `metadata.rolled_back_from` so the activity feed shows the linkage.
///
/// Errors:
/// - `not_found` — no prior content within retention; rollback impossible.
/// - `drift` — the on-disk file changed since the change row was appended.
///   Caller can prompt the user to overwrite.
///
/// status: changes-rollback-helper
/// status: vault-home-recent-activity-detail
#[tauri::command]
async fn rollback_change(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result("rollback_change", rollback_change_inner(state, change_id).await)
}

#[derive(Serialize)]
struct RollbackOutcome {
    /// The id of the change row whose content was just rolled back to.
    /// Used by the UI's un-rollback affordance ("recently rolled back —
    /// restore?") so it knows which path/state was just left behind.
    prior_change_id: i64,
    /// The path that was rolled back. Convenience for UI refresh; identical
    /// to the original change row's path field.
    path: String,
    /// New on-disk hash after the rollback write. The Tauri write also
    /// appended a new changelog row; the UI re-reads `recent_changes` to
    /// pick that up.
    new_hash: String,
}

async fn rollback_change_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    // Resolve everything off the session up front so we don't hold the
    // session lock across the await/IO of the write.
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let row = session
            .changes
            .recent(0)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        let _ = row; // shut clippy up; we instead query via history below.
        // Resolve the change's path via a direct lookup — `recent` would
        // miss rows past the default window. The history call filters by
        // path post-hoc, so we use a single-row query.
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let (prior_id, prior_bytes) = changes_arc
        .previous_content_for_path(&target_path, change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "no earlier version of {target_path} is recorded — this is the oldest change in the log for this file"
            ))
        })?;

    let prior_content = String::from_utf8(prior_bytes)
        .map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    // Compute current on-disk hash for the drift-aware write. Empty hash
    // when the file is missing — matches the contract of write_file_checked.
    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash = vault.write_file_checked(&target_path, &current_hash, &prior_content)?;

    // Append the rollback row directly (rather than relying on the `write_file`
    // command) so we can stamp `metadata.rolled_back_from = <change_id>` per
    // spec — and so the on-disk file write + changelog append happen here as
    // one logical step instead of being routed through the Tauri write_file
    // command which doesn't carry the metadata.
    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(prior_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"rolled_back_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (rollback) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: prior_id,
        path: target_path,
        new_hash,
    })
}

/// Restore the file's content to match the given snapshot row. Writes the
/// row's `content` blob back to its `path` and appends a new `'modified'`
/// row stamped `metadata.restored_from = change_id`.
///
/// Different from `rollback_change` (which uses
/// `previous_content_for_path` to walk *before* the change): this command
/// matches the snapshot mental model — each row IS a saved version, and
/// "Restore" writes that version. The two share the changelog primitives
/// but live side-by-side: agent rollback per `mcp.md` calls
/// `rollback_change`; the home-page activity widget calls
/// `restore_snapshot`.
///
/// Errors:
/// - `not_found` — change row doesn't exist or has no content (e.g. a
///   `'deleted'` row, which carries NULL content by design).
/// - `drift` — the on-disk file changed since `expected_hash` was taken.
///   Surfaced as the same drift error `write_file_checked` produces; the
///   UI prompts the user.
///
/// status: vault-home-recent-activity-detail
#[tauri::command]
async fn restore_snapshot(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result(
        "restore_snapshot",
        restore_snapshot_inner(state, change_id).await,
    )
}

async fn restore_snapshot_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let blob = changes_arc
        .content_at(change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "change {change_id} has no recorded content (deleted-row snapshots can't be restored directly — restore an earlier created/modified row instead)"
            ))
        })?;

    let snapshot_content =
        String::from_utf8(blob).map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash =
        vault.write_file_checked(&target_path, &current_hash, &snapshot_content)?;

    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(snapshot_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"restored_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (restore_snapshot) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: change_id,
        path: target_path,
        new_hash,
    })
}

/// Look up the path of a single change by id. Walks `recent` widely enough
/// to find it; rollback targets are usually recent so this is fine in
/// practice. Falls back to `NotFound` if the row is past the search window
/// (in which case retention has likely already dropped its content too).
fn lookup_change_path(changes: &Changes, change_id: i64) -> Result<String, HikerError> {
    // 5000 rows is well past the default 50-per-pair retention; if we don't
    // find it here, it's effectively gone.
    let rows = changes
        .recent(5000)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    rows.into_iter()
        .find(|r| r.id == change_id)
        .map(|r| r.path)
        .ok_or_else(|| HikerError::NotFound(format!("change {change_id}")))
}

/// status: task-queue-home-widget
/// status: task-queue-event-stream
/// Snapshot the current task-queue rows. Frontend seeds its local mirror
/// with this once at mount and applies `hiker:queue-event` deltas after.
#[tauri::command]
async fn tasks_snapshot(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::tasks::TaskRecord>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("tasks_snapshot", Ok::<_, String>(queue.snapshot().await))
}

/// status: task-queue-row-details
/// Lazy inspection: prompt + final result + final error + metadata for
/// a single task id. Returns `None` if the id has already been GC'd
/// past `terminal_retention_secs` (the user can scroll the queue tile
/// fast enough to miss the row).
#[tauri::command]
async fn task_details(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<hiker_core::tasks::TaskDetails>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("task_details", Ok::<_, String>(queue.details(&id).await))
}

/// status: task-queue-row-cancel-action
/// Cancel a task by id. Behavior depends on lease state — see
/// `core::tasks::Queue::cancel`.
#[tauri::command]
async fn tasks_cancel(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    queue.cancel(&id).await;
    log_cmd_result("tasks_cancel", Ok::<(), String>(()))
}

// ---------- note-mutation producer surface ----------
//
// status: note-mutations-menu
// status: note-mutations-menu-task-shape
// status: note-mutation-reformat-as-markdown
// status: note-mutation-replace-original
// status: note-mutation-discard-derived
//
// The mutations menu submits a `Direct` `High`-priority task carrying the
// buffer's *live* text (per `chat-active-note-context-injection`'s same
// rule) plus the source extension. The direct-LLM worker drains it and
// produces text; on success the awaiter spawned here emits
// `hiker:note-mutation-applied` carrying the result content + the
// source-hash captured at submit time so the frontend can replace the
// open buffer (or hold + toast if the buffer was closed).

/// Frontend payload for a successful mutation result. The frontend
/// dispatches a single CM6 transaction replacing the active buffer's
/// content (when the buffer is still open and its content hash matches
/// `source_hash_at_submit`) or holds the result for a click-to-apply
/// toast (when the buffer has been closed).
#[derive(Debug, Clone, Serialize)]
struct NoteMutationAppliedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation_kind: &'a str,
    content: &'a str,
    source_hash_at_submit: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct NoteMutationFailedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation: &'a str,
    error: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteMutationSubmitOutcome {
    pub task_id: String,
}

/// status: note-mutations-menu-task-shape
/// Submit a note-mutation task. `mutation` selects the prompt feature key
/// and is recorded in the changes-row metadata when the user accepts
/// (`note-mutation-replace-original`). Returns the task id immediately;
/// callers watch `hiker:queue-event` (and the new
/// `hiker:note-mutation-completed` / `-failed` events) for terminal state.
#[tauri::command]
async fn submit_note_mutation(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    let outcome = submit_note_mutation_inner(state, app, rel, mutation, source_extension, content)
        .await;
    log_cmd_result("submit_note_mutation", outcome)
}

async fn submit_note_mutation_inner(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    if mutation != "reformat-as-markdown" {
        return Err(format!("unknown mutation: {mutation}"));
    }

    // Grab the per-vault handles we need before awaiting anywhere — clone
    // out from under the sync mutex. The source hash captured here is the
    // pre-mutation on-disk hash; the frontend uses it at apply-time to
    // decide whether the buffer's content still matches what the LLM saw.
    let (queue, prompts, source_hash) = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let source_hash = session
            .vault
            .read_file_with_hash(&rel)
            .map(|(_, h)| h)
            .map_err(|e| e.to_string())?;
        (
            session.tasks.clone(),
            session.prompts.clone(),
            source_hash,
        )
    };

    let title = std::path::Path::new(&rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel)
        .to_string();

    let prompt = prompts
        .render(
            "note_mutation_reformat_as_markdown",
            [
                ("title", title.as_str()),
                ("content", content.as_str()),
                ("source_extension", source_extension.as_str()),
            ],
        )
        .map_err(|e| e.to_string())?;

    let task = hiker_core::tasks::Task {
        id: String::new(),
        kind: hiker_core::tasks::TaskKind::NoteMutation {
            mutation: mutation.clone(),
            source_path: rel.clone(),
        },
        priority: hiker_core::tasks::Priority::High,
        shape: hiker_core::tasks::TaskShape::Direct,
        payload: hiker_core::tasks::TaskPayload {
            prompt,
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata: serde_json::json!({
            "source_hash_at_submit": source_hash,
        }),
    };

    let handle = queue.submit(task).await;
    let task_id = handle.id.clone();

    // Spawn the awaiter. On Completed → emit the result content as a
    // frontend event so the UI can replace the open buffer in a single
    // CM6 transaction (or hold + toast if the buffer is closed). On
    // Failed → toast via event. On Cancelled → silent (the user
    // already knows; queue events drive the widget).
    let app_for_await = app.clone();
    let rel_for_await = rel.clone();
    let mutation_for_await = mutation.clone();
    let source_hash_for_await = source_hash.clone();
    let task_id_for_await = task_id.clone();
    tokio::spawn(async move {
        let task_id = task_id_for_await;
        let outcome = handle.await_outcome().await;
        match outcome {
            hiker_core::tasks::TaskOutcome::Completed { value, .. } => {
                let body_owned: String;
                let result_body: &str = match &value {
                    serde_json::Value::String(s) => s.as_str(),
                    other => {
                        body_owned = serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| other.to_string());
                        body_owned.as_str()
                    }
                };
                // Empty / whitespace-only completions almost certainly
                // mean the provider returned a malformed or refused
                // response — replacing the buffer with empty bytes is a
                // worse failure than surfacing the problem.
                if result_body.trim().is_empty() {
                    let _ = app_for_await.emit(
                        "hiker:note-mutation-failed",
                        &NoteMutationFailedEvent {
                            task_id: &task_id,
                            source_path: &rel_for_await,
                            mutation: &mutation_for_await,
                            error: "empty response from LLM provider",
                        },
                    );
                    return;
                }
                let _ = app_for_await.emit(
                    "hiker:note-mutation-applied",
                    &NoteMutationAppliedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation_kind: &mutation_for_await,
                        content: result_body,
                        source_hash_at_submit: &source_hash_for_await,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Failed { error, .. } => {
                let _ = app_for_await.emit(
                    "hiker:note-mutation-failed",
                    &NoteMutationFailedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation: &mutation_for_await,
                        error: &error,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Cancelled { .. } => {
                // No preview, no toast — the queue widget already showed
                // the cancellation.
            }
        }
    });

    Ok(NoteMutationSubmitOutcome { task_id })
}

// ---------- frontend-bridge logger ----------

/// status: obs-frontend-bridge
/// Wire-side level enum for the `log_from_frontend` command. Tagged via
/// serde's snake_case so the JS payload `{ level: "error", ... }` round-trips
/// without an extra string match — Tauri's serde-driven arg deserialization
/// rejects garbage at the seam rather than at a `match` inside the body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LogLevel {
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
fn log_from_frontend(
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
async fn start_mcp(
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

// status: autosave-backend-module
// Tauri command surface for the autosave layer. Each command parses args
// → calls `Autosave::*` → returns DTO; one-to-one with the Rust API per
// the spec.

fn with_autosave<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Autosave) -> Result<R, String>,
) -> Result<R, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(&session.autosave)
}

#[tauri::command]
fn autosave_write(
    state: State<AppState>,
    path: String,
    contents: String,
) -> Result<(), String> {
    // Hash on the backend — frontend doesn't carry a blake3 dep, and
    // hashing a markdown buffer at 5s tick cadence is sub-millisecond
    // anyway. Same hash function (blake3) the rest of core uses, so
    // recover()'s on-disk-hash comparison stays apples-to-apples.
    let bytes = contents.as_bytes();
    let buffer_hash = hiker_core::hash_str(&contents);
    log_cmd_result(
        "autosave_write",
        with_autosave(&state, |a| {
            a.write(&path, bytes, &buffer_hash)
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_clear(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_clear",
        with_autosave(&state, |a| a.clear(&path).map_err(|e| e.to_string())),
    )
}

#[tauri::command]
fn autosave_save_tab_state(
    state: State<AppState>,
    state_payload: TabState,
) -> Result<(), String> {
    log_cmd_result(
        "autosave_save_tab_state",
        with_autosave(&state, |a| {
            a.save_tab_state(state_payload).map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_load_tab_state(state: State<AppState>) -> Result<Option<TabState>, String> {
    log_cmd_result(
        "autosave_load_tab_state",
        with_autosave(&state, |a| a.load_tab_state().map_err(|e| e.to_string())),
    )
}

/// Wire DTO for `autosave_recover` — the autosaved bytes ride as a UTF-8
/// string since hiker is a markdown editor and the frontend's CM6 can
/// only restore text-typed content. Non-UTF-8 sidecars (which shouldn't
/// happen for markdown buffers) become lossy strings; the recovery flow
/// still surfaces them so the user isn't silently denied their work.
#[derive(Serialize)]
struct RecoveredEntryDto {
    path: String,
    autosave_id: String,
    autosaved_content: String,
    autosaved_hash: String,
    on_disk_hash: Option<String>,
    saved_at_ms: i64,
}

impl From<RecoveredEntry> for RecoveredEntryDto {
    fn from(e: RecoveredEntry) -> Self {
        Self {
            path: e.path,
            autosave_id: e.autosave_id,
            autosaved_content: String::from_utf8_lossy(&e.autosaved_content).into_owned(),
            autosaved_hash: e.autosaved_hash,
            on_disk_hash: e.on_disk_hash,
            saved_at_ms: e.saved_at_ms,
        }
    }
}

#[tauri::command]
fn autosave_recover(state: State<AppState>) -> Result<Vec<RecoveredEntryDto>, String> {
    log_cmd_result(
        "autosave_recover",
        with_autosave(&state, |a| {
            a.recover()
                .map(|v| v.into_iter().map(RecoveredEntryDto::from).collect())
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_discard(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_discard",
        with_autosave(&state, |a| a.discard(&path).map_err(|e| e.to_string())),
    )
}

// ---------------------------------------------------------------------------
// Trails commands (slice U1)
// ---------------------------------------------------------------------------
// Tauri seams over `hiker_core::trails::*`. Each is the standard
// `parse args -> snapshot session deps -> call core -> return DTO` shape
// (see the rule in `hiker-dev` skill: commands are wrappers, not
// orchestrators). The `core::trails` ops own watcher suppression and
// changes append; this layer just plumbs the session deps in.
//
// status: active-trail-state

#[derive(Serialize)]
struct TrailCreatedDto {
    trail_doc_rel: String,
    trail_id: String,
}

#[derive(Serialize)]
struct WaypointAppendedDto {
    waypoint_rel: String,
    waypoint_id: String,
    trail_id: String,
}

#[derive(Serialize)]
struct WaypointRemovedDto {
    removed_count: u32,
}

#[tauri::command]
async fn trail_create(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let result = trail_create_inner(state, name).await;
    log_cmd_result("trail_create", result)
}

async fn trail_create_inner(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let (watcher, vault, jobs, changes, trails_cfg) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let cfg = session
            .config
            .read()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            cfg.trails.clone(),
        )
    };
    let outcome = hiker_core::trails::create_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trails_cfg,
        &name,
    )
    .await?;
    Ok(TrailCreatedDto {
        trail_doc_rel: outcome.trail_doc_rel,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
async fn trail_append_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
    let result = trail_append_waypoint_inner(
        state,
        trail_doc_rel,
        source_rel,
        parent_waypoint_id,
        annotation,
    )
    .await;
    log_cmd_result("trail_append_waypoint", result)
}

async fn trail_append_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    // Open a fresh Store reader for the call. `Store::open` against an
    // existing db is cheap (sub-ms warm) and is the documented per-command
    // read pattern (see `VaultSession.read_store` doc-comment). We can't
    // hand out the shared `Arc<Mutex<Store>>` here because the call holds
    // the reference across `.await` points and `MutexGuard` isn't `Send`.
    let mut store = Store::open(vault.root()).map_err(|e| HikerError::Io(e.to_string()))?;
    let outcome = hiker_core::trails::append_waypoint(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &mut store,
        &trail_doc_rel,
        &source_rel,
        parent_waypoint_id.as_deref(),
        annotation.as_deref(),
    )
    .await?;
    Ok(WaypointAppendedDto {
        waypoint_rel: outcome.waypoint_rel,
        waypoint_id: outcome.waypoint_id,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
async fn trail_remove_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let result = trail_remove_waypoint_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_remove_waypoint", result)
}

async fn trail_remove_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            Trash::open(session.vault.root()),
        )
    };
    let outcome = hiker_core::trails::remove_waypoint(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
        &waypoint_id,
    )
    .await?;
    Ok(WaypointRemovedDto {
        removed_count: outcome.removed_count,
    })
}

#[tauri::command]
fn trail_descendant_count(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<u32, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        hiker_core::trails::descendant_count(
            &session.vault,
            &trail_doc_rel,
            &waypoint_id,
        )
    })();
    log_cmd_result("trail_descendant_count", result)
}

#[tauri::command]
async fn trail_delete(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<(), HikerError> {
    let result = trail_delete_inner(app, state, trail_doc_rel).await;
    log_cmd_result("trail_delete", result.map(|_| ()))
}

async fn trail_delete_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            Trash::open(session.vault.root()),
        )
    };
    let entry = hiker_core::trails::delete_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
    )
    .await?;
    // Trash bin auto-refresh hook — same shape as `delete_note_inner`.
    let _ = app.emit("hiker:trash-changed", ());
    Ok(entry)
}

#[tauri::command]
fn trails_list(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::trails::TrailListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::list_trails(&session.vault, &store)
    })();
    log_cmd_result("trails_list", result)
}

#[tauri::command]
fn trail_get(
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<hiker_core::trails::TrailDetail, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::get_trail(&session.vault, &store, &trail_doc_rel)
    })();
    log_cmd_result("trail_get", result)
}

/// Reverse-lookup: which trails contain `source_rel` as a waypoint at
/// any depth. Each hit pairs the derived-table `trail_id` with its
/// trail-doc rel-path so the UI can decide membership for a specific
/// trail (e.g. "is this note already a waypoint of the *active* trail?")
/// without a second round-trip per trail.
///
/// status: trail-add-to-active-from-editor-verb
#[tauri::command]
fn trails_containing_note(
    state: State<'_, AppState>,
    source_rel: String,
) -> Result<Vec<hiker_core::trails::TrailsContainingNoteHit>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::trails_containing_note_with_paths(
            &session.vault,
            &store,
            &source_rel,
        )
    })();
    log_cmd_result("trails_containing_note", result)
}

/// Set (or clear, with `None`) the active trail. Persists
/// `vault.active_trail` via the standard settings write-back path and
/// stamps `hiker.last_activated_at` on the trail-doc when activating.
///
/// status: active-trail-state
#[tauri::command]
async fn trail_set_active(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_active_inner(state, trail_doc_rel).await;
    log_cmd_result("trail_set_active", result)
}

async fn trail_set_active_inner(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    // Snapshot deps for the (optional) timestamp stamp before we touch
    // the settings file.
    let (watcher, vault, jobs, changes, root) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            session.root.clone(),
        )
    };

    // Stamp the trail-doc's `last_activated_at` first (only when
    // activating a non-None value). If stamping fails we still proceed
    // to persist the setting — the timestamp is dropdown-ordering chrome,
    // not load-bearing for activation correctness.
    if let Some(rel) = trail_doc_rel.as_deref() {
        if let Err(e) = hiker_core::trails::stamp_last_activated_at(
            &watcher,
            &jobs,
            &vault,
            Some(&changes),
            rel,
        )
        .await
        {
            tracing::warn!(error = %e, path = %rel,
                "trail_set_active: stamp_last_activated_at failed; proceeding");
        }
    }

    let value = match trail_doc_rel {
        Some(s) => serde_json::Value::String(s),
        None => serde_json::Value::Null,
    };
    let updated =
        Config::set(SettingsScope::Vault, "vault.active_trail", value, &root)?;
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
        *w = updated;
    }
    Ok(())
}

/// Set (or clear with `None`) the trail-doc's append cursor. Used by
/// the "Append from here" waypoint verb (`trail-append-from-here-verb`)
/// and the Trails-mode header's "Reset to main line" action
/// (`trail-reset-cursor-verb`); both surfaces land in slice C2.
///
/// status: trail-append-cursor
#[tauri::command]
async fn trail_set_append_cursor(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_append_cursor_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_set_append_cursor", result)
}

async fn trail_set_append_cursor_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::trails::set_append_cursor(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trail_doc_rel,
        waypoint_id.as_deref(),
    )
    .await
}

/// Background task: watches both config TOML files for external edits,
/// reloads the merged Config, swaps the in-memory copy, and emits
/// `hiker:config-reloaded` so the frontend re-applies settings.
///
/// Debounced at ~500 ms. Suppressed for 2 s after a `set_setting` write
/// (same `SUPPRESS_TTL` shape as the vault watcher) so UI-driven flips
/// don't round-trip back through the file watcher.
async fn start_config_watcher(
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
                                if let Ok(guard) = state.session.lock() {
                                    if let Some(session) = guard.as_ref() {
                                        if let Ok(mut w) = session.config.write() {
                                            *w = config.clone();
                                        }
                                        session.tasks.set_cfg(config.tasks.clone());
                                        if let Ok(mut tools) = session.mcp_tools.write() {
                                            *tools = config.mcp.tools.clone();
                                        }
                                    }
                                }
                                let _ = app.emit("hiker:config-reloaded", &config);
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

// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject
// Tauri command surface for core::staging. Each command is the standard
// shape: parse args → snapshot session deps → call core → translate errors
// → return DTO.

#[derive(Debug, Default, Deserialize)]
struct StagingFilterArg {
    path: Option<String>,
    trail_id: Option<String>,
    surface: Option<String>,
    session_id: Option<String>,
}

impl From<StagingFilterArg> for StagingFilter {
    fn from(a: StagingFilterArg) -> Self {
        StagingFilter {
            path: a.path,
            trail_id: a.trail_id,
            surface: a.surface,
            session_id: a.session_id,
        }
    }
}

#[tauri::command]
fn staging_list(
    state: State<'_, AppState>,
    filter: Option<StagingFilterArg>,
) -> Result<Vec<Proposal>, String> {
    let result = (|| -> Result<Vec<Proposal>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: StagingFilter = filter.unwrap_or_default().into();
        session.staging.list(&f).map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_list", result)
}

#[tauri::command]
fn staging_count(state: State<'_, AppState>) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .count(&StagingFilter::default())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_count", result)
}

#[tauri::command]
fn staging_accept(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<AcceptOutcome, String> {
    let result = (|| -> Result<AcceptOutcome, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let outcome = session
            .staging
            .accept(&proposal_id, &session.vault, Some(&session.changes))
            .map_err(|e| e.to_string())?;
        Ok(outcome)
    })();
    let r = log_cmd_result("staging_accept", result);
    // status: staging-review-activity-detail-filter
    // Emit so every active surface (activity detail, tree, trails, etc.)
    // can refresh after a proposal is accepted.
    if r.is_ok() {
        let _ = app.emit("hiker:staging-changed", ());
    }
    r
}

#[tauri::command]
fn staging_reject(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .reject(&proposal_id)
            .map_err(|e| e.to_string())
    })();
    let r = log_cmd_result("staging_reject", result);
    if r.is_ok() {
        let _ = app.emit("hiker:staging-changed", ());
    }
    r
}

#[tauri::command]
fn staging_accept_all(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Vec<AcceptOutcome>, String> {
    let result = (|| -> Result<Vec<AcceptOutcome>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .accept_all(
                &StagingFilter::default(),
                &session.vault,
                Some(&session.changes),
            )
            .map_err(|e| e.to_string())
    })();
    let r = log_cmd_result("staging_accept_all", result);
    if r.is_ok() {
        let _ = app.emit("hiker:staging-changed", ());
    }
    r
}

/// Read the proposed `.md` content for a staging proposal so the frontend
/// can open it as a read-only preview buffer with the snapshot-preview diff
/// toggle pattern.
///
/// status: staging-review-activity-detail-filter
#[tauri::command]
fn staging_content(
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<String, String> {
    let result = (|| -> Result<String, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .content(&proposal_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_content", result)
}

// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            session: Mutex::new(None),
            config_last_write: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            list_dir,
            read_file,
            read_file_with_hash,
            write_file,
            write_file_checked,
            open_for_edit,
            commit_buffer,
            resolve_drift,
            open_vault_at,
            get_default_vault,
            index,
            index_status,
            index_state_for,
            count_notes_in,
            compute_diff,
            related_notes,
            search_vault,
            chunks_for,
            create_note,
            move_note,
            move_folder,
            reveal_in_file_manager,
            delete_note,
            restore_trash_entry,
            list_trash,
            empty_trash,
            permanent_delete_trash_entry,
            get_settings,
            get_settings_scoped,
            reload_config,
            reveal_config_file,
            set_setting,
            vault_home_stats,
            recent_notes_modified,
            recent_notes_accessed,
            note_accessed,
            note_properties,
            recent_changes,
            changes_count,
            change_content,
            rollback_change,
            restore_snapshot,
            chat::chat_send,
            chat::chat_continue,
            chat::chat_stop,
            chat::chat_cancel,
            chat::chat_session_new,
            chat::chat_session_active,
            chat::chat_session_list,
            chat::chat_session_open,
            chat::chat_session_delete,
            chat_at_autocomplete,
            chat_resolve_at_note,
            tasks_snapshot,
            tasks_cancel,
            task_details,
            submit_note_mutation,
            autosave_write,
            autosave_clear,
            autosave_save_tab_state,
            autosave_load_tab_state,
            autosave_recover,
            autosave_discard,
            log_from_frontend,
            trail_create,
            trail_append_waypoint,
            trail_remove_waypoint,
            trail_descendant_count,
            trail_delete,
            trails_list,
            trail_get,
            trails_containing_note,
            trail_set_active,
            trail_set_append_cursor,
            staging_list,
            staging_count,
            staging_accept,
            staging_reject,
            staging_accept_all,
            staging_content,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    // Bypass tokio runtime drop, which blocks waiting on in-flight
    // `spawn_blocking` embed tasks (fastembed runs can take seconds) and on
    // notify-debouncer's worker thread join. The OS reclaims everything.
    std::process::exit(0);
}

// FileEvent and ProgressEvent need to be Serialize for tauri::emit. Both
// are defined in core; this const block compile-asserts the contract.
const _: fn() = || {
    fn assert_serialize<T: Serialize>() {}
    assert_serialize::<FileEvent>();
    assert_serialize::<ProgressEvent>();
};
