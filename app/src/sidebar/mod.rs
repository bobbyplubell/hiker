//! Sidebar-adjacent `AppState` helpers. The sidebar surfaces themselves
//! now live with their activities (`crate::files`, `crate::clusters`,
//! `crate::trails`, ...) and render through the activity registry; what
//! remains here is a small set of `AppState` methods the sidebar toolbar
//! buttons (the `+` new-item menu, the sort control) call into:
//!
//! - [`AppState::new_note`] / [`AppState::create_new_note`] — create a
//!   fresh blank vault note (the latter without opening a tab).
//! - [`AppState::new_board`] — create a board and open it in the board view.
//! - [`AppState::new_canvas`] — create an empty `.canvas` and open it.
//! - [`AppState::persist_tree_sort`] — persist the file-tree sort choice.
//!
//! All three create paths route through the indexer-driven
//! `core::ops::file::create_at` so the new file is indexed without a
//! duplicate watcher-driven ingest.

use crate::editor_pane;
use crate::state::{AppState, ToastLevel};

impl AppState {
    pub fn persist_tree_sort(&mut self, sort_str: &str) {
    self.set_setting(
        hiker_core::config::SettingsScope::Vault,
        "vault.tree.sort_by",
        &serde_json::json!(sort_str),
        "Save sort failed",
    );
    }

    pub fn new_note(&mut self) {
        match self.create_new_note() {
            Ok(actual) => editor_pane::open_file(self, &actual, /* sticky */ true),
            Err(err) => self.push_toast(format!("Create failed: {}", err), ToastLevel::Error),
        }
    }

    /// Create a fresh blank vault note (`new-note-N.md`, suffix-counted to skip
    /// names already in the target dir) and return its vault-relative path,
    /// WITHOUT opening it in a tab. The note placement and the indexer-driven
    /// creation are shared with [`AppState::new_note`]; callers that want a tab
    /// open it themselves (the sidebar `+`), while others (the canvas "New note"
    /// verb) drop a File-node pointer instead.
    ///
    /// Routes through the indexer-driven `core::ops::file::create_at` (watcher
    /// suppression + `IndexJob::Upsert`) rather than the bare `vault::create_note`,
    /// so the new note is indexed without a duplicate watcher-driven ingest.
    pub fn create_new_note(&mut self) -> Result<String, hiker_core::errors::HikerError> {
        let target_dir = self
            .file_tree_state
            .selected_folder
            .as_deref()
            .unwrap_or("")
            .to_string();
        // Pick `new-note-N.md` skipping any already present in the target dir.
        let sort = self
            .vault_session
            .config
            .read()
            .ok()
            .map(|c| c.vault.tree.sort_by)
            .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc);
        let listed = self
            .vault_session
            .vault
            .list_dir(&target_dir, sort)
            .unwrap_or_default();
        let existing: std::collections::HashSet<&str> =
            listed.iter().map(|e| e.name.as_str()).collect();
        let mut candidate = String::new();
        for n in 1.. {
            let name = format!("new-note-{}.md", n);
            if !existing.contains(name.as_str()) {
                candidate = name;
                break;
            }
        }
        let rel = if target_dir.is_empty() {
            candidate
        } else {
            format!("{}/{}", target_dir, candidate)
        };
        let watcher = self.vault_session.services.watcher.clone();
        let jobs = self.vault_session.services.indexer.job_sender();
        let vault = self.vault_session.vault.clone();
        let rel_owned = rel.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, "").await
            }),
            Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
        };
        if result.is_ok() {
            self.file_tree_state.invalidate_dir(&target_dir);
        }
        result
    }

    /// Create a new board via `core::board::create_board` (default columns,
    /// `[boards] new_board_dir` placement) and open it in the board view.
    /// The cross-type new-item picker (`sidebar-new-item-button`) routes
    /// here. Runs synchronously on the frame's tokio runtime.
    ///
    /// status: board-create
    pub fn new_board(&mut self) {
        let state = self;
        let watcher = state.vault_session.services.watcher.clone();
        let jobs = state.vault_session.services.indexer.job_sender();
        let vault = state.vault_session.vault.clone();
        let oplog = state.vault_session.services.oplog.clone();
        let cfg = state
            .vault_session
            .config
            .read()
            .map(|c| c.boards.clone())
            .unwrap_or_default();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            state.push_toast("New board failed: no runtime", ToastLevel::Error);
            return;
        };
        let result = handle.block_on(async {
            hiker_core::boards::ops::create_board(
                &watcher, &jobs, &oplog, &vault, &cfg, "new-board",
            )
            .await
        });
        match result {
            Ok(outcome) => {
                state.file_tree_state.invalidate_all();
                // Open in the board view with inline-rename active so the
                // user names it before submitting (mirrors new-trail /
                // new-file). status: board-create
                crate::panels::board::open_for_rename(state, &outcome.board_doc_rel);
            }
            Err(err) => {
                state.push_toast(format!("New board failed: {err}"), ToastLevel::Error);
            }
        }
    }

    /// Create a new empty `.canvas` file and open it in the canvas view. Seeds
    /// `{"nodes":[],"edges":[]}` (via `Canvas::default().to_canonical_json()`)
    /// through the same indexer-driven `core::ops::file::create_at` path the `+`
    /// new-note / new-board buttons use, so the file is written + op-log-adopted
    /// on its first save exactly like a note. The cross-type new-item picker
    /// (`sidebar-new-item-button`) routes here. status: canvas-create
    pub fn new_canvas(&mut self) {
        let state = self;
        let target_dir = state
            .file_tree_state
            .selected_folder
            .as_deref()
            .unwrap_or("")
            .to_string();
        let sort = state
            .vault_session
            .config
            .read()
            .ok()
            .map(|c| c.vault.tree.sort_by)
            .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc);
        let existing: std::collections::HashSet<String> = state
            .vault_session
            .vault
            .list_dir(&target_dir, sort)
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .collect();
        let mut candidate = String::new();
        for n in 1.. {
            let name = format!("new-canvas-{n}.canvas");
            if !existing.contains(&name) {
                candidate = name;
                break;
            }
        }
        let rel = if target_dir.is_empty() {
            candidate
        } else {
            format!("{target_dir}/{candidate}")
        };
        let seed = hiker_canvas::model::Canvas::default().to_canonical_json();
        let watcher = state.vault_session.services.watcher.clone();
        let jobs = state.vault_session.services.indexer.job_sender();
        let vault = state.vault_session.vault.clone();
        let rel_owned = rel.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, &seed).await
            }),
            Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
        };
        match result {
            Ok(actual) => {
                state.file_tree_state.invalidate_dir(&target_dir);
                crate::panels::canvas::open_fresh(state, &actual);
            }
            Err(err) => {
                state.push_toast(format!("New canvas failed: {err}"), ToastLevel::Error);
            }
        }
    }
}
