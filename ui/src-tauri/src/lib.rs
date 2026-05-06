use std::path::PathBuf;
use std::sync::Mutex;

use hiker_core::indexer::{
    route_watcher_events, start_indexer, IndexJob, IndexStatus, IndexerHandle, ProgressEvent,
};
use hiker_core::store::{RelatedHit, Store};
use hiker_core::vault::move_note as vault_move_note;
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

#[tauri::command]
fn list_dir(state: State<AppState>, rel: String) -> Result<Vec<DirEntryDto>, String> {
    with_vault(&state, |v| v.list_dir(&rel).map_err(|e| e.to_string()))
}

#[tauri::command]
fn read_file(state: State<AppState>, rel: String) -> Result<String, String> {
    with_vault(&state, |v| v.read_file(&rel).map_err(|e| e.to_string()))
}

#[derive(Serialize)]
struct FileWithHash {
    contents: String,
    hash: String,
}

#[tauri::command]
fn read_file_with_hash(state: State<AppState>, rel: String) -> Result<FileWithHash, String> {
    with_vault(&state, |v| {
        v.read_file_with_hash(&rel)
            .map(|(contents, hash)| FileWithHash { contents, hash })
            .map_err(|e| e.to_string())
    })
}

#[tauri::command]
fn write_file(state: State<AppState>, rel: String, contents: String) -> Result<(), String> {
    with_vault(&state, |v| {
        v.write_file(&rel, &contents).map_err(|e| e.to_string())
    })
}

#[tauri::command]
fn write_file_checked(
    state: State<AppState>,
    rel: String,
    expected_hash: String,
    contents: String,
) -> Result<String, hiker_core::HikerError> {
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
}

#[tauri::command]
async fn pick_vault(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    let folder = rx.recv().map_err(|e| e.to_string())?;
    let Some(path) = folder else { return Ok(None) };
    let path_buf = path.into_path().map_err(|e| e.to_string())?;

    let vault = Vault::open(&path_buf).map_err(|e| e.to_string())?;
    let root = vault.root().to_path_buf();
    let display = root.to_string_lossy().into_owned();

    // Open the store (creates .hiker/index.db on first run).
    let store = Store::open(&root).map_err(|e| e.to_string())?;

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately.
    let indexer = start_indexer(root.clone(), store, || {
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
    let job_sender = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.indexer.job_sender()
    };
    let job = match scope {
        IndexScope::All => IndexJob::FullScan,
        IndexScope::Path { rel } => IndexJob::Upsert { rel_path: rel },
    };
    job_sender.send(job).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn index_status(state: State<AppState>) -> Result<IndexStatus, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    Ok(session.indexer.status())
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
        (created, session.indexer.job_sender())
    };
    // Explicitly index the new file (the watcher event was suppressed).
    let _ = sender
        .send(IndexJob::Upsert { rel_path: created.clone() })
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
    let guard = state
        .session
        .lock()
        .map_err(|_| HikerError::Io("lock poisoned".into()))?;
    let session = guard
        .as_ref()
        .ok_or_else(|| HikerError::Io("no vault open".into()))?;
    // Open a fresh writer for this op rather than threading the indexer's
    // owned writer back through a channel — store-module-discipline: one
    // writer at a time is the SQLite invariant, but `rename_note` is a
    // tiny tx that can briefly contend without harm. (The indexer is
    // serial and any concurrent upsert just retries on its next pull.)
    let mut store = Store::open(&session.root).map_err(|e| HikerError::Io(e.to_string()))?;
    vault_move_note(&session.vault, &mut store, Some(&session.watcher), &from, &to)
}

#[tauri::command]
fn related_notes(
    state: State<AppState>,
    rel: String,
    top_k: Option<usize>,
) -> Result<Vec<RelatedHit>, String> {
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
            related_notes,
            create_note,
            move_note,
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
