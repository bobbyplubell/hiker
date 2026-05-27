//! Sidebar surfaces (Files, Clusters, Trails) — each is an independently
//! dockable panel after step 4 of the egui_dock migration. The legacy
//! mode-switcher is gone; each panel renders its own internal toolbar
//! (new note / refresh / sort for Files, etc.) at the top of the body.
//!
//! `SidebarMode` survives as a compatibility shim for the persisted
//! `Config::ui.default_sidebar_mode` setting in Settings — it no longer
//! drives runtime layout. The trash bin lives inside the Files panel
//! body now (it used to be pinned at the bottom across every mode).

pub(crate) mod clusters;
pub(crate) mod files;
mod trails;

use eframe::egui;

use crate::editor_pane;
use crate::state::{AppState, ToastLevel};

/// Legacy mode discriminant — kept because Settings persists a
/// `default_sidebar_mode` field. Runtime no longer reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SidebarMode {
    Files,
    Clusters,
    Trails,
}

/// Render context shared by the three sidebar panels. The panel bodies
/// are `&mut self` methods so each is exempt from `single_call_fn` (every
/// panel is mounted from exactly one registry record) without a
/// per-panel `#[allow]` or an arbitrary split.
pub struct PanelRender<'a> {
    pub ui: &'a mut egui::Ui,
    pub state: &'a mut AppState,
}

impl PanelRender<'_> {
    /// Clusters panel: cluster-trees sidebar body.
    pub fn clusters(&mut self) {
        let state = &mut *self.state;
        egui::ScrollArea::vertical()
            .id_salt("panel-clusters-body")
            .auto_shrink([false, false])
            .show(self.ui, |ui| {
                state.clusters_panel(ui);
            });
    }

    /// Trails panel: trail picker + waypoints.
    pub fn trails(&mut self) {
        let state = &mut *self.state;
        egui::ScrollArea::vertical()
            .id_salt("panel-trails-body")
            .auto_shrink([false, false])
            .show(self.ui, |ui| {
                trails::TrailsView { ui, state }.render();
            });
    }
}

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
    let state = self;
    let target_dir = state
        .session
        .sidebar
        .selected_folder
        .as_deref()
        .unwrap_or("");
    // Pick `new-note-N.md` skipping any already present in the target dir.
    let sort = state
        .vault_session
        .config
        .read()
        .ok()
        .map(|c| c.vault.tree.sort_by)
        .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc);
    let listed = state
        .vault_session
        .vault
        .list_dir(target_dir, sort)
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
    match state.vault_session.vault.create_note(&rel) {
        Ok(actual) => {
            state.session.sidebar.dir_cache.remove(target_dir);
            editor_pane::open_file(state, &actual, /* sticky */ true);
        }
        Err(err) => {
            state.push_toast(format!("Create failed: {}", err), ToastLevel::Error);
        }
    }
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
        let result = handle
            .block_on(async { hiker_core::boards::ops::create_board(&watcher, &jobs, &vault, &cfg, "new-board").await });
        match result {
            Ok(outcome) => {
                state.session.sidebar.dir_cache.clear();
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
}
