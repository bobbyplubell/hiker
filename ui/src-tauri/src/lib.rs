use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use hiker_core::config::{Config, SettingsScope};
use hiker_core::indexer::{
    route_watcher_events, start_indexer, IndexJob, IndexStatus, IndexerHandle, ProgressEvent,
};
use hiker_core::store::{ChunkBounds, RelatedHit, Store};
use hiker_core::trash::{Trash, TrashEntry, TrashListItem};
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, DirEntryDto, HikerError, Vault};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// All long-lived state for an open vault. Constructed in `pick_vault`,
/// dropped on swap.
struct VaultSession {
    vault: Vault,
    root: PathBuf,
    indexer: IndexerHandle,
    /// Held to keep the watcher alive; dropping this closes the broadcast.
    /// Also referenced by `create_note` / `move_note` to register self-write
    /// suppression around fs mutations.
    watcher: Watcher,
    /// status: settings-load-once-at-startup
    /// Frozen merged user+vault settings. `set_setting` writes through to
    /// disk via `Config::set` and swaps the in-memory copy in this RwLock.
    config: RwLock<Config>,
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
    log_cmd_result(
        "write_file",
        with_vault(&state, |v| {
            v.write_file(&rel, &contents).map_err(|e| e.to_string())
        }),
    )
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
        session
            .vault
            .write_file_checked(&rel, &expected_hash, &contents)
    })();
    log_cmd_result("write_file_checked", result)
}

/// Push `root` to the front of `current`, dedupe by string equality, cap
/// at 10 entries. Used to update the user-scope `vault.recent` list on
/// each successful vault open.
fn update_recent(current: &[String], root: &std::path::Path) -> Vec<String> {
    let display = root.to_string_lossy().into_owned();
    let mut out = Vec::with_capacity(current.len() + 1);
    out.push(display.clone());
    for entry in current {
        if entry != &display {
            out.push(entry.clone());
        }
        if out.len() >= 10 {
            break;
        }
    }
    out
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

#[tauri::command]
async fn pick_vault(app: tauri::AppHandle) -> Result<Option<String>, String> {
    log_cmd_result("pick_vault", pick_vault_inner(app).await)
}

async fn pick_vault_inner(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    let folder = rx.await.map_err(|e| e.to_string())?;
    let Some(path) = folder else { return Ok(None) };
    let path_buf = path.into_path().map_err(|e| e.to_string())?;
    open_vault_at(app, path_buf).await
}

/// Try to auto-open the user-scope `vault.default`. Returns the opened
/// vault's display path on success, `Ok(None)` when no default is set or
/// the configured path no longer exists (deleted, unmounted, typo'd).
/// Per the failure-mode rule in `settings.md`: log a `warn!` and fall
/// back to the picker; never auto-clear the setting (the user might just
/// have the drive unplugged).
///
/// status: settings-default-vault-autoopen
#[tauri::command]
async fn try_open_default_vault(app: tauri::AppHandle) -> Result<Option<String>, String> {
    log_cmd_result("try_open_default_vault", try_open_default_vault_inner(app).await)
}

async fn try_open_default_vault_inner(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let default = match hiker_core::config::Config::user_default_vault()
        .map_err(|e| e.to_string())?
    {
        Some(p) => p,
        None => return Ok(None),
    };
    let path = PathBuf::from(&default);
    if !path.is_dir() {
        tracing::warn!(
            configured = %default,
            "vault.default path no longer exists; falling back to picker. Setting left intact in case the path is just temporarily unavailable."
        );
        return Ok(None);
    }
    open_vault_at(app, path).await
}

async fn open_vault_at(
    app: tauri::AppHandle,
    path_buf: PathBuf,
) -> Result<Option<String>, String> {
    let vault = Vault::open(&path_buf).map_err(|e| e.to_string())?;
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
    let mut config = Config::load(&root).map_err(|e| e.to_string())?;

    // Push this vault onto the user-scope `vault.recent` list. Best-effort:
    // if the platform config dir isn't resolvable (sandboxed env), the
    // write fails silently rather than aborting vault open. The returned
    // Config is the freshly-reloaded merged view — adopt it so the in-memory
    // copy in the session matches what's on disk.
    let recent = update_recent(&config.vault.recent, &root);
    if recent != config.vault.recent {
        let value = serde_json::Value::Array(
            recent.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
        );
        match Config::set(SettingsScope::User, "vault.recent", value, &root) {
            Ok(updated) => config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.recent"),
        }
    }

    // Open the store (creates .hiker/index.db on first run).
    let store = Store::open(&root).map_err(|e| e.to_string())?;

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately.
    let indexer = start_indexer(vault.clone(), store, || {
        FastembedEmbedder::load()
            .map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn hiker_core::embed::Embedder>)
    });

    // Start the filesystem watcher and bridge its events into the indexer.
    let watcher = Watcher::start(&root).map_err(|e| e.to_string())?;
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
        config: RwLock::new(config),
    };

    let state = app.state::<AppState>();
    *state.session.lock().map_err(|e| e.to_string())? = Some(session);
    Ok(Some(display))
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
        let store = Store::open(&session.root).map_err(|e| e.to_string())?;
        match store.get_note_by_path(&rel).map_err(|e| e.to_string())? {
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
    let (created, sender) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        // Pick the lowest free `new-note-N.md` in the target folder.
        let folder = folder.trim_end_matches('/');
        let mut created: Option<String> = None;
        let mut last_err: Option<HikerError> = None;
        for n in 1..1000 {
            let candidate = if folder.is_empty() {
                format!("new-note-{n}.md")
            } else {
                format!("{folder}/new-note-{n}.md")
            };
            session.watcher.suppress(candidate.clone());
            match session.vault.create_note(&candidate) {
                Ok(p) => {
                    created = Some(p);
                    break;
                }
                Err(HikerError::AlreadyExists(_)) => continue,
                Err(e) => {
                    last_err = Some(e);
                    break;
                }
            }
        }
        let created = match created {
            Some(p) => p,
            None => return Err(last_err.unwrap_or_else(|| {
                HikerError::AlreadyExists("ran out of new-note-N candidates".into())
            })),
        };
        // Re-suppress so the TTL window starts close to when notify
        // surfaces the Created event, not at function entry.
        session.watcher.suppress(created.clone());
        (created, session.indexer.job_sender())
    };
    // Explicitly index the new file (the watcher event was suppressed).
    let _ = sender
        .send(IndexJob::Upsert { rel_path: created.clone(), force: false })
        .await;
    Ok(created)
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
    // Suppress the watcher around the move so its eventual rename event
    // doesn't double-trigger an index update; the indexer task itself does
    // the fs rename + index update on its owned store connection.
    let mover = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session.watcher.suppress(from.clone());
        session.watcher.suppress(to.clone());
        // Borrow nothing across the await — pull a clone of the sender we
        // need (via job_sender) and drop the lock.
        session.indexer.job_sender()
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    mover
        .send(IndexJob::Move {
            from: from.clone(),
            to: to.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped move reply".into()))?;
    // Re-suppress so the TTL window starts close to when notify surfaces
    // its events post-rename.
    if let Ok(guard) = state.session.lock() {
        if let Some(session) = guard.as_ref() {
            session.watcher.suppress(from);
            session.watcher.suppress(to);
        }
    }
    result
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
    let (sender, members) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        // Pre-suppress every `.md` member at its old AND new path so notify's
        // platform-specific event ordering (per-child events on some OSes,
        // single-dir event on others) can't surface stale Created/Deleted
        // pairs around the move. Folder root suppressed too.
        session.watcher.suppress(from.clone());
        session.watcher.suppress(to.clone());
        let members = session.vault.walk_md_files(&from).unwrap_or_default();
        let from_prefix = format!("{from}/");
        for m in &members {
            session.watcher.suppress(m.clone());
            let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
            session.watcher.suppress(format!("{to}/{suffix}"));
        }
        (session.indexer.job_sender(), members)
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    sender
        .send(IndexJob::MoveFolder {
            from: from.clone(),
            to: to.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped move_folder reply".into()))?;
    if let Ok(guard) = state.session.lock() {
        if let Some(session) = guard.as_ref() {
            session.watcher.suppress(from.clone());
            session.watcher.suppress(to.clone());
            let from_prefix = format!("{from}/");
            for m in &members {
                session.watcher.suppress(m.clone());
                let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
                session.watcher.suppress(format!("{to}/{suffix}"));
            }
        }
    }
    result
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
    let sender = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        // Pre-suppress every `.md` member as well as the folder root. On
        // Linux/macOS `fs::rename` of a directory is a single inode op, so
        // notify shouldn't emit per-child events — but other platforms may,
        // and the cost of pre-suppressing is just adding strings to a TTL
        // map. The post-rename re-suppression below covers the same paths
        // again so the TTL window starts close to when notify surfaces.
        session.watcher.suppress(rel.clone());
        let members = session.vault.walk_md_files(&rel).unwrap_or_default();
        for m in &members {
            session.watcher.suppress(m.clone());
        }
        session.indexer.job_sender()
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    sender
        .send(IndexJob::DeleteNote {
            rel: rel.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped delete reply".into()))?;
    if let Ok(guard) = state.session.lock() {
        if let Some(session) = guard.as_ref() {
            session.watcher.suppress(rel);
            if let Ok(entry) = &result {
                if let Some(members) = &entry.members {
                    for m in members {
                        session.watcher.suppress(m.clone());
                    }
                }
            }
        }
    }
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
    let sender = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        // Pre-suppress paths the restore is about to write. We resolve the
        // entry here (rather than in the indexer) so the suppression is in
        // place before the fs::rename fires.
        let trash = Trash::open(session.vault.root());
        if let Ok(Some(entry)) = trash.find(&id) {
            session.watcher.suppress(entry.original_path.clone());
            if let Some(members) = &entry.members {
                for m in members {
                    session.watcher.suppress(m.clone());
                }
            }
        }
        session.indexer.job_sender()
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    sender
        .send(IndexJob::RestoreFromTrash {
            id: id.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| HikerError::Io("indexer task is shut down".into()))?;
    let result = reply_rx
        .await
        .map_err(|_| HikerError::Io("indexer dropped restore reply".into()))?;
    if let Ok(guard) = state.session.lock() {
        if let Some(session) = guard.as_ref() {
            if let Ok(entry) = &result {
                session.watcher.suppress(entry.original_path.clone());
                if let Some(members) = &entry.members {
                    for m in members {
                        session.watcher.suppress(m.clone());
                    }
                }
            }
        }
    }
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
        let store = Store::open(&session.root).map_err(|e| e.to_string())?;
        store.chunk_bounds_for(&rel).map_err(|e| e.to_string())
    })();
    log_cmd_result("chunks_for", result)
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
        // Open a fresh read connection — see the store module's notes on this.
        let store = Store::open(&session.root).map_err(|e| e.to_string())?;
        let id = match store.id_for_path(&rel).map_err(|e| e.to_string())? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        store
            .related_notes(&id, top_k.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("related_notes", result)
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
            pick_vault,
            index,
            index_status,
            index_state_for,
            related_notes,
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
            try_open_default_vault,
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
