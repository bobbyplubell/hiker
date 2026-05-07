use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use hiker_core::changes::{ChangeOp, ChangeRow, Changes};
use hiker_core::config::{Config, SettingsScope};
use hiker_core::indexer::{
    route_watcher_events, start_indexer, IndexJob, IndexStatus, IndexerHandle, ProgressEvent,
};
use hiker_core::search::{self, SearchModes, SearchResponse};
use hiker_core::store::{ChunkBounds, RecentNote, RelatedHit, Store, VaultStats};
use hiker_core::trash::{Trash, TrashEntry, TrashListItem};
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, DirEntryDto, HikerError, Vault};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

/// All long-lived state for an open vault. Constructed in `open_vault_at`,
/// dropped on swap.
struct VaultSession {
    vault: Vault,
    root: PathBuf,
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
    /// status: settings-load-once-at-startup
    /// Frozen merged user+vault settings. `set_setting` writes through to
    /// disk via `Config::set` and swaps the in-memory copy in this RwLock.
    config: RwLock<Config>,
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
    read_store: Mutex<Store>,
}

struct AppState {
    session: Mutex<Option<VaultSession>>,
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
fn list_dir(state: State<AppState>, rel: String) -> Result<Vec<DirEntryDto>, String> {
    log_cmd_result(
        "list_dir",
        with_vault(&state, |v| v.list_dir(&rel).map_err(|e| e.to_string())),
    )
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

#[tauri::command]
fn write_file(state: State<AppState>, rel: String, contents: String) -> Result<(), String> {
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
            metadata: serde_json::json!({}),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file) failed");
        }
        Ok(())
    })();
    log_cmd_result("write_file", result)
}

#[tauri::command]
fn write_file_checked(
    state: State<AppState>,
    rel: String,
    expected_hash: String,
    contents: String,
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
            metadata: serde_json::json!({}),
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
fn set_setting(
    state: State<AppState>,
    scope: SettingsScope,
    key: String,
    value: serde_json::Value,
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
        let updated = Config::set(scope, &key, value, &root)?;
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
        Ok(updated)
    })();
    log_cmd_result("set_setting", result)
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
        Mutex::new(Store::open(&root).map_err(|e| HikerError::Io(e.to_string()))?);

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

    // Kick the initial scan. Returns immediately; jobs flow as the model
    // load completes.
    let _ = indexer.full_scan().await;

    let session = VaultSession {
        vault,
        root,
        indexer,
        watcher,
        changes,
        config: RwLock::new(config),
        read_store,
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

/// Canonical list of file extensions the indexer chunks. Frontend caches
/// this once at vault open and uses it for client-side row classification
/// (the `tree-row-unsupported-marker` derivation) so we don't pay a Tauri
/// round trip per visible row. Single source of truth =
/// `core::indexer::INDEXABLE_EXTENSIONS`.
#[tauri::command]
fn indexable_extensions() -> Vec<&'static str> {
    hiker_core::indexer::INDEXABLE_EXTENSIONS.to_vec()
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
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store.chunk_bounds_for(&rel).map_err(|e| e.to_string())
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
) -> Result<SearchResponse, String> {
    log_cmd_result("search_vault", search_vault_inner(state, query, modes, epoch).await)
}

async fn search_vault_inner(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,
    epoch: u64,
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

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            session: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            list_dir,
            read_file,
            read_file_with_hash,
            write_file,
            write_file_checked,
            open_vault_at,
            get_default_vault,
            index,
            index_status,
            index_state_for,
            indexable_extensions,
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
            set_setting,
            vault_home_stats,
            recent_notes_modified,
            recent_notes_accessed,
            note_accessed,
            recent_changes,
            changes_count,
            change_content,
            rollback_change,
            restore_snapshot,
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
