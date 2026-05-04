use std::sync::Mutex;

use hiker_core::{DirEntryDto, Vault};
use tauri::{Manager, State};
use tauri_plugin_dialog::DialogExt;

struct AppState {
    vault: Mutex<Option<Vault>>,
}

fn vault_or_err<R>(state: &State<AppState>, f: impl FnOnce(&Vault) -> Result<R, String>) -> Result<R, String> {
    let guard = state.vault.lock().map_err(|e| e.to_string())?;
    let vault = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(vault)
}

#[tauri::command]
fn list_dir(state: State<AppState>, rel: String) -> Result<Vec<DirEntryDto>, String> {
    vault_or_err(&state, |v| v.list_dir(&rel).map_err(|e| e.to_string()))
}

#[tauri::command]
fn read_file(state: State<AppState>, rel: String) -> Result<String, String> {
    vault_or_err(&state, |v| v.read_file(&rel).map_err(|e| e.to_string()))
}

#[tauri::command]
fn write_file(state: State<AppState>, rel: String, contents: String) -> Result<(), String> {
    vault_or_err(&state, |v| v.write_file(&rel, &contents).map_err(|e| e.to_string()))
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
    let state = app.state::<AppState>();
    let vault = Vault::open(&path_buf).map_err(|e| e.to_string())?;
    let display = vault.root().to_string_lossy().into_owned();
    *state.vault.lock().map_err(|e| e.to_string())? = Some(vault);
    Ok(Some(display))
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState { vault: Mutex::new(None) })
        .invoke_handler(tauri::generate_handler![list_dir, read_file, write_file, pick_vault])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
