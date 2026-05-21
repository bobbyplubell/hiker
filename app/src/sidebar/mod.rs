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
mod files;
mod trails;
mod trash;

use std::sync::Arc;

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

/// Files panel: file tree + trash bin at the bottom. The new-note button
/// and the refresh / sort menu live in the side bar's title row (wired
/// through `WorkbenchBehavior::side_bar_action_buttons` /
/// `side_bar_actions_menu`).
pub fn files_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    let avail_height = ui.available_height();
    let trash_row_height = 28.0;
    egui::ScrollArea::vertical()
        .id_salt("panel-files-body")
        .max_height((avail_height - trash_row_height).max(60.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            files::show(ui, state, rt);
        });
    ui.separator();
    trash::show(ui, state, rt);
}

/// Clusters panel: cluster-trees sidebar body.
pub fn clusters_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    egui::ScrollArea::vertical()
        .id_salt("panel-clusters-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            clusters::show(ui, state, rt);
        });
}

/// Trails panel: trail picker + waypoints.
pub fn trails_panel(ui: &mut egui::Ui, state: &mut AppState) {
    egui::ScrollArea::vertical()
        .id_salt("panel-trails-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            trails::show(ui, state);
        });
}

pub fn persist_tree_sort(state: &mut AppState, sort_str: &str) {
    state.set_setting(
        hiker_core::config::SettingsScope::Vault,
        "vault.tree.sort_by",
        serde_json::json!(sort_str),
        "Save sort failed",
    );
}

pub fn new_note(state: &mut AppState) {
    let target_dir = state
        .session
        .sidebar
        .selected_folder
        .as_deref()
        .unwrap_or("");
    let candidate = next_new_note_name(state, target_dir);
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

fn next_new_note_name(state: &AppState, dir: &str) -> String {
    let listed = state
        .vault_session
        .vault
        .list_dir(dir, default_sort(&state.vault_session.config))
        .unwrap_or_default();
    let existing: std::collections::HashSet<&str> =
        listed.iter().map(|e| e.name.as_str()).collect();
    for n in 1.. {
        let candidate = format!("new-note-{}.md", n);
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!()
}

fn default_sort(
    config: &std::sync::RwLock<hiker_core::config::Config>,
) -> hiker_core::config::TreeSortBy {
    config
        .read()
        .ok()
        .map(|c| c.vault.tree.sort_by)
        .unwrap_or(hiker_core::config::TreeSortBy::NameAsc)
}
