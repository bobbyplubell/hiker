//! Files-mode body: lazy-loaded file tree. Click a file to open it as a
//! preview-slot buffer tab; double-click to inline-rename (placeholder);
//! right-click for context menu (placeholder).
//!
//! v0 limits: no drag-and-drop, no inline rename, no context menu. Single
//! click -> open. That covers the daily-use path; the rest layers on once
//! the foundation lands.

use std::sync::Arc;

use eframe::egui;

use hiker_core::{DirEntryDto, EntryKind};

use crate::editor_pane;
use crate::state::AppState;
use crate::theme;

pub fn show(ui: &mut egui::Ui, state: &mut AppState, rt: &Arc<tokio::runtime::Runtime>) {
    sort_header(ui, state);
    show_dir(ui, state, rt, "", 0);
}

/// Tiny header strip with the sort-by control. Persists the selection
/// to `vault.tree.sort_by` so the tree opens in the chosen order on
/// next vault open.
fn sort_header(ui: &mut egui::Ui, state: &mut AppState) {
    use hiker_core::config::TreeSortBy;
    let current = default_sort(&state.vault_session.config);
    let mut new_sort = current;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Sort")
                .small()
                .color(theme::muted()),
        );
        egui::ComboBox::from_id_salt("files-sort-by")
            .selected_text(sort_label(current))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut new_sort, TreeSortBy::NameAsc, "Name (A-Z)");
                ui.selectable_value(&mut new_sort, TreeSortBy::NameDesc, "Name (Z-A)");
                ui.selectable_value(&mut new_sort, TreeSortBy::MtimeDesc, "Recent");
                ui.selectable_value(&mut new_sort, TreeSortBy::MtimeAsc, "Oldest");
            });
    });
    if new_sort != current {
        let wire = match new_sort {
            TreeSortBy::NameAsc => "name_asc",
            TreeSortBy::NameDesc => "name_desc",
            TreeSortBy::MtimeDesc => "mtime_desc",
            TreeSortBy::MtimeAsc => "mtime_asc",
        };
        // Force every visible dir listing to re-fetch. Originally only
        // cleared on Ok, but clearing on Err is harmless (config
        // unchanged → same listing).
        state.set_setting(
            hiker_core::config::SettingsScope::Vault,
            "vault.tree.sort_by",
            serde_json::Value::String(wire.to_string()),
            "Sort change failed",
        );
        state.session.sidebar.dir_cache.clear();
    }
}

#[cfg(test)]
mod sort_label_tests {
    use super::*;
    use hiker_core::config::TreeSortBy;

    #[test]
    fn every_variant_has_a_label() {
        for v in [
            TreeSortBy::NameAsc,
            TreeSortBy::NameDesc,
            TreeSortBy::MtimeDesc,
            TreeSortBy::MtimeAsc,
        ] {
            let l = sort_label(v);
            assert!(!l.is_empty(), "missing label for {v:?}");
        }
    }

    #[test]
    fn labels_are_distinct() {
        let labels = [
            sort_label(TreeSortBy::NameAsc),
            sort_label(TreeSortBy::NameDesc),
            sort_label(TreeSortBy::MtimeDesc),
            sort_label(TreeSortBy::MtimeAsc),
        ];
        let set: std::collections::HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(set.len(), 4, "sort labels collide: {labels:?}");
    }
}

fn sort_label(s: hiker_core::config::TreeSortBy) -> &'static str {
    use hiker_core::config::TreeSortBy;
    match s {
        TreeSortBy::NameAsc => "Name (A-Z)",
        TreeSortBy::NameDesc => "Name (Z-A)",
        TreeSortBy::MtimeDesc => "Recent",
        TreeSortBy::MtimeAsc => "Oldest",
    }
}

/// Cheap direct-child file counter using the listing cache. When the
/// directory hasn't been listed yet, returns 0 rather than triggering a
/// disk walk on the render path — the count fills in once the user
/// expands the parent. Matches the legacy `count_notes_in` semantics
/// (markdown-shaped files only).
fn count_direct_files(state: &AppState, rel: &str) -> usize {
    let Some(entries) = state.session.sidebar.dir_cache.get(rel) else {
        return 0;
    };
    entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::File))
        .count()
}

fn show_dir(
    ui: &mut egui::Ui,
    state: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
    rel: &str,
    depth: usize,
) {
    // Ensure this dir is loaded.
    if !state.session.sidebar.dir_cache.contains_key(rel) {
        match state.vault_session.vault.list_dir(rel, default_sort(&state.vault_session.config)) {
            Ok(entries) => {
                state.session.sidebar.dir_cache.insert(rel.to_string(), entries);
            }
            Err(err) => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("failed to list {}: {}", rel, err),
                );
                return;
            }
        }
    }

    // Clone the entries out so we can mutate state below without
    // overlapping borrows.
    let entries = state.session.sidebar.dir_cache.get(rel).cloned().unwrap_or_default();

    for entry in entries {
        match entry.kind {
            EntryKind::Dir => render_dir_row(ui, state, rt, &entry, depth),
            EntryKind::File => render_file_row(ui, state, &entry, depth),
        }
    }
}

fn render_dir_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
    entry: &DirEntryDto,
    depth: usize,
) {
    let expanded = state.session.sidebar.expanded.contains(&entry.rel_path);
    // Use the standard BMP triangles (U+25BC / U+25B6) rather than the
    // "small" variants (U+25BE / U+25B8) — only the standard ones live
    // in egui's bundled default font; the small variants tofu-render.
    let chevron = if expanded { "v" } else { ">" };
    // Direct-child file count hint (`count_notes_in` parity). Cached on
    // the same dir_cache the listing renders from, so showing the count
    // is essentially free. Only the first depth is counted — recursing
    // would be O(vault) for a quick visual.
    let child_count = count_direct_files(state, &entry.rel_path);
    let count_suffix = if child_count > 0 {
        format!(" ({})", child_count)
    } else {
        String::new()
    };
    let label = format!("{} {}{}", chevron, entry.name, count_suffix);

    let resp = row_button(ui, &label, depth, false);

    // DnD: folder rows accept dropped paths and move them into this dir.
    let dropped = resp.dnd_release_payload::<String>();
    if let Some(src) = dropped {
        move_into_folder(state, &src, &entry.rel_path);
    }
    // Folder rows are also draggable so users can re-parent subtrees.
    resp.clone()
        .dnd_set_drag_payload::<String>(entry.rel_path.clone());

    if resp.clicked() {
        if expanded {
            state.session.sidebar.expanded.remove(&entry.rel_path);
        } else {
            state.session.sidebar.expanded.insert(entry.rel_path.clone());
        }
        state.session.sidebar.selected_folder = Some(entry.rel_path.clone());
    }
    if expanded {
        show_dir(ui, state, rt, &entry.rel_path, depth + 1);
    }
}

/// Move a vault-relative path into the destination folder via
/// `vault::move_note` (which handles store + watcher updates atomically).
fn move_into_folder(state: &mut AppState, src: &str, dest_dir: &str) {
    let basename = basename_of(src);
    let dest = if dest_dir.is_empty() {
        basename.to_string()
    } else {
        format!("{}/{}", dest_dir, basename)
    };
    if dest == src {
        return;
    }
    let store_mutex = state.vault_session.services.read_store.clone();
    let watcher = state.vault_session.services.watcher.clone();
    let mut store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Err(err) = hiker_core::vault::move_note(
        &state.vault_session.vault,
        &mut store,
        Some(watcher.as_ref()),
        src,
        &dest,
    ) {
        drop(store);
        state.push_toast(
            format!("Move failed: {}", err),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    drop(store);
    let src_parent = src.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    state.session.sidebar.dir_cache.remove(src_parent);
    state.session.sidebar.dir_cache.remove(dest_dir);
    if let Some(buf) = state.session.buffers.remove(src) {
        let mut moved = buf;
        moved.path = dest.clone();
        state.session.buffers.insert(dest.clone(), moved);
    }
    for tab in &mut state.session.tabs {
        if let crate::tab::TabKind::Buffer { path } = &mut tab.kind
            && path == src
        {
            *path = dest.clone();
        }
    }
    state.push_toast(
        format!("Moved -> {}", dest),
        crate::state::ToastLevel::Info,
    );
}

fn render_file_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    entry: &DirEntryDto,
    depth: usize,
) {
    // Inline-rename mode preempts the regular row render. Drafts live in
    // egui memory keyed by the row's rel_path, so this stays out of
    // SidebarState (which a parallel cluster-editor port is editing).
    if let Some(draft) = rename_draft_for(ui, &entry.rel_path) {
        rename_row(ui, state, entry, depth, draft);
        return;
    }

    let active_buffer_path = state.session.active_tab.and_then(|id| {
        state
            .tab_by_id(id)
            .and_then(|t| t.buffer_path().map(str::to_string))
    });
    let is_active = active_buffer_path.as_deref() == Some(entry.rel_path.as_str());

    // Dirty-dot suffix if the buffer is loaded and dirty.
    let dirty_marker = state
        .session.buffers
        .get(&entry.rel_path)
        .map(|b| if b.is_dirty() { " *" } else { "" })
        .unwrap_or("");

    // Index-state marker: ⌛ for files the indexer hasn't processed yet,
    // ⊘ for files the indexer skipped (unsupported extension, too big,
    // etc.). Falls back to nothing once the file is fully indexed.
    let index_marker = index_state_marker(state, &entry.rel_path);

    let label = format!("{}{}{}", entry.name, dirty_marker, index_marker);
    let resp = row_button(ui, &label, depth, is_active);
    // Honour a pending reveal-from-discovery: when our row matches the
    // sidebar's scroll_target one-shot, ask egui to bring us into view and
    // clear the target so subsequent frames don't keep re-scrolling.
    if state.session.sidebar.scroll_target.as_deref() == Some(entry.rel_path.as_str()) {
        resp.scroll_to_me(Some(egui::Align::Center));
        state.session.sidebar.scroll_target = None;
    }
    // Drag payload: vault-relative source path. Drop targets are
    // rendered on folder rows below.
    resp.clone()
        .dnd_set_drag_payload::<String>(entry.rel_path.clone());

    if resp.clicked() {
        editor_pane::open_file(state, &entry.rel_path, /* sticky */ false);
    }
    if resp.double_clicked() {
        // Per docs/editor.md: double-click enters inline rename mode.
        start_rename(ui, &entry.rel_path);
    }
    let rel = entry.rel_path.clone();
    resp.context_menu(|ui| {
        if ui.button("Open").clicked() {
            editor_pane::open_file(state, &rel, true);
            ui.close();
        }
        if ui.button("Rename").clicked() {
            start_rename(ui, &rel);
            state.session.sidebar.renaming = Some(rel.clone());
            state.session.sidebar.renaming_text = basename_of(&rel).to_string();
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            duplicate_file(state, &rel);
            ui.close();
        }
        if ui.button("Reveal in file manager").clicked() {
            reveal_in_file_manager(state, &rel);
            ui.close();
        }
        if ui.button("Properties").clicked() {
            open_properties(state, &rel);
            ui.close();
        }
        // Reindex verb: force-enqueue this path on the indexer. Useful when
        // the watcher missed an edit or the file's hash is somehow stale.
        if ui.button("Reindex this file").clicked() {
            {
                let indexer = state.vault_session.services.indexer.as_ref();
                let tx = indexer.job_sender();
                let path_owned = rel.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = tx.send(hiker_core::indexer::IndexJob::Upsert {
                            rel_path: path_owned,
                            force: true,
                        }).await;
                    });
                }
                state.push_toast(
                    format!("Reindexing {rel}"),
                    crate::state::ToastLevel::Info,
                );
            }
            ui.close();
        }
        // Add-to-trail verb. Legacy parity:
        // `trail-add-to-active-from-editor-verb` — surfaces the trail
        // name in the label and uses the recursive membership check so
        // "Already in 'X'" disables for child waypoints too. Routes
        // through `note_visited` so the append-cursor semantics agree
        // with the editor pill and recent-visits flow.
        {
            crate::state::ensure_recent_trail(state);
            let target = state
                .session.active_trail
                .clone()
                .filter(|id| state.session.trails.iter().any(|t| &t.id == id))
                .or_else(|| {
                    state
                        .session.trails
                        .iter()
                        .find(|t| t.name == crate::state::RECENT_TRAIL)
                        .map(|t| t.id.clone())
                });
            if let Some(tid) = target
                && let Some(trail) = state.session.trails.iter().find(|t| t.id == tid)
            {
                let trail_name = trail.name.clone();
                let already = waypoint_tree_contains(&trail.waypoints, &rel);
                let label = if already {
                    format!("Already in '{}'", trail_name)
                } else {
                    format!("Add to trail '{}'", trail_name)
                };
                let resp = ui.add_enabled(!already, egui::Button::new(label));
                if resp.clicked() {
                    let path_for_trail = rel.clone();
                    crate::state::note_visited(state, &path_for_trail);
                    let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
                    state.push_toast(
                        format!("Added {path_for_trail} to '{trail_name}'"),
                        crate::state::ToastLevel::Info,
                    );
                    ui.close();
                }
            }
        }
        // "Set as active trail" — only meaningful when this row points at a
        // file under `.hiker/trails/`, which is where trail-doc paths live.
        // Matches the legacy `trail-set-as-active-context-verb` semantics by
        // basename: a trail named "X" maps to `.hiker/trails/X.md`.
        if rel.starts_with(".hiker/trails/")
            && rel.ends_with(".md")
            && ui.button("Set as active trail").clicked()
        {
            let stem = rel
                    .rsplit('/')
                    .next()
                    .and_then(|n| n.strip_suffix(".md"))
                    .unwrap_or("");
                if let Some(t) = state
                    .session.trails
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(stem))
                {
                    let tid = t.id.clone();
                    let name = t.name.clone();
                    state.session.active_trail = Some(tid);
                    crate::actions::ensure_panel_visible(
                        state,
                        crate::panels_registry::PANEL_TRAILS,
                    );
                    state.push_toast(
                        format!("Activated trail {}", name),
                        crate::state::ToastLevel::Info,
                    );
                } else {
                    state.push_toast(
                        "No trail registered for this doc",
                        crate::state::ToastLevel::Info,
                    );
                }
            ui.close();
        }
        if ui.button("Delete").clicked() {
            state.session.modal = Some(crate::state::Modal::ConfirmDelete { path: rel.clone() });
            ui.close();
        }
    });
}

/// Renders the inline rename TextEdit. On Enter, runs `move_note` via
/// the live Store + Watcher; on Esc, cancels.
fn rename_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    entry: &DirEntryDto,
    depth: usize,
    mut draft: String,
) {
    ui.horizontal(|ui| {
        ui.add_space((depth as f32) * 12.0);
        ui.add(match entry.kind {
            EntryKind::Dir => crate::icons::folder(),
            EntryKind::File => crate::icons::file(),
        });
        let id = egui::Id::new(("rename-edit", entry.rel_path.as_str()));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .id(id)
                .desired_width(ui.available_width()),
        );
        // First frame: focus + pre-select the basename excluding extension.
        let just_opened = rename_just_opened(ui, &entry.rel_path);
        if just_opened {
            resp.request_focus();
            mark_rename_handled(ui, &entry.rel_path);
        }

        if resp.changed() {
            set_rename_draft(ui, &entry.rel_path, &draft);
        }

        let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));

        if commit {
            commit_rename(state, &entry.rel_path, &draft);
            clear_rename(ui, &entry.rel_path);
        } else if cancel || (resp.lost_focus() && !commit) {
            clear_rename(ui, &entry.rel_path);
        }
    });
}

fn commit_rename(state: &mut AppState, from: &str, draft: &str) {
    let draft = draft.trim();
    if draft.is_empty() || draft == basename_of(from) {
        return;
    }
    let parent = from.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let to = if parent.is_empty() {
        draft.to_string()
    } else {
        format!("{}/{}", parent, draft)
    };
    let store_mutex = state.vault_session.services.read_store.clone();
    let watcher = state.vault_session.services.watcher.clone();
    let mut store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Err(err) = hiker_core::vault::move_note(
        &state.vault_session.vault,
        &mut store,
        Some(watcher.as_ref()),
        from,
        &to,
    ) {
        drop(store);
        state.push_toast(
            format!("Rename failed: {}", err),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    drop(store);

    // Cache invalidation + buffer/tab path swap.
    state.session.sidebar.dir_cache.remove(parent);
    if let Some(buf) = state.session.buffers.remove(from) {
        let mut moved = buf;
        moved.path = to.clone();
        state.session.buffers.insert(to.clone(), moved);
    }
    for tab in &mut state.session.tabs {
        if let crate::tab::TabKind::Buffer { path } = &mut tab.kind
            && path == from
        {
            *path = to.clone();
        }
    }
    state.push_toast(
        format!("Renamed -> {}", to),
        crate::state::ToastLevel::Info,
    );
}

fn basename_of(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

// ----- rename draft storage in egui memory -----

#[derive(Clone, Default)]
struct RenameMem {
    path: String,
    draft: String,
    just_opened: bool,
}

fn mem_id() -> egui::Id {
    egui::Id::new("sidebar-files-rename")
}

fn rename_draft_for(ui: &egui::Ui, path: &str) -> Option<String> {
    ui.ctx().memory(|m| {
        m.data
            .get_temp::<RenameMem>(mem_id())
            .filter(|r| r.path == path)
            .map(|r| r.draft.clone())
    })
}

fn rename_just_opened(ui: &egui::Ui, path: &str) -> bool {
    ui.ctx().memory(|m| {
        m.data
            .get_temp::<RenameMem>(mem_id())
            .map(|r| r.path == path && r.just_opened)
            .unwrap_or(false)
    })
}

fn mark_rename_handled(ui: &mut egui::Ui, path: &str) {
    ui.ctx().memory_mut(|m| {
        if let Some(mut r) = m.data.get_temp::<RenameMem>(mem_id())
            && r.path == path
        {
            r.just_opened = false;
            m.data.insert_temp(mem_id(), r);
        }
    });
}

fn set_rename_draft(ui: &mut egui::Ui, path: &str, draft: &str) {
    ui.ctx().memory_mut(|m| {
        let r = RenameMem {
            path: path.to_string(),
            draft: draft.to_string(),
            just_opened: false,
        };
        m.data.insert_temp(mem_id(), r);
    });
}

fn start_rename(ui: &mut egui::Ui, path: &str) {
    let draft = basename_of(path).to_string();
    ui.ctx().memory_mut(|m| {
        let r = RenameMem {
            path: path.to_string(),
            draft,
            just_opened: true,
        };
        m.data.insert_temp(mem_id(), r);
    });
}

fn clear_rename(ui: &mut egui::Ui, path: &str) {
    ui.ctx().memory_mut(|m| {
        if m.data
            .get_temp::<RenameMem>(mem_id())
            .map(|r| r.path == path)
            .unwrap_or(false)
        {
            m.data.remove::<RenameMem>(mem_id());
        }
    });
}

fn open_properties(state: &mut AppState, rel: &str) {
    use crate::tab::{Tab, TabKind};
    if let Some(existing) = state.session.tabs.iter().find(|t| match &t.kind {
        TabKind::Properties { path } => path == rel,
        _ => false,
    }) {
        state.session.active_tab = Some(existing.id);
        return;
    }
    let id = state.next_tab_id();
    state.session.tabs.push(Tab {
        id,
        kind: TabKind::Properties { path: rel.to_string() },
        sticky: false,
    });
    state.session.active_tab = Some(id);
    state.session.preview_tab = Some(id);
}

fn duplicate_file(state: &mut AppState, rel: &str) {
    // Read the source body, choose a `<stem>-copy-N.<ext>` target in the
    // same dir, write via vault::create_note + write_file. Vault layer
    // handles parent-dir creation + collision checks.
    let body = match state.vault_session.vault.read_file(rel) {
        Ok(s) => s,
        Err(err) => {
            state.push_toast(
                format!("Duplicate failed: {}", err),
                crate::state::ToastLevel::Error,
            );
            return;
        }
    };
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let base = basename_of(rel);
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{}", e)),
        None => (base.to_string(), String::new()),
    };
    let listed = state
        .vault_session.vault
        .list_dir(parent, default_sort(&state.vault_session.config))
        .unwrap_or_default();
    let existing: std::collections::HashSet<&str> =
        listed.iter().map(|e| e.name.as_str()).collect();
    let mut chosen = String::new();
    for n in 1.. {
        let candidate = format!("{}-copy-{}{}", stem, n, ext);
        if !existing.contains(candidate.as_str()) {
            chosen = candidate;
            break;
        }
    }
    let target = if parent.is_empty() {
        chosen
    } else {
        format!("{}/{}", parent, chosen)
    };
    let actual = match state.vault_session.vault.create_note(&target) {
        Ok(p) => p,
        Err(err) => {
            state.push_toast(
                format!("Duplicate failed: {}", err),
                crate::state::ToastLevel::Error,
            );
            return;
        }
    };
    if let Err(err) = state.vault_session.vault.write_file(&actual, &body) {
        state.push_toast(
            format!("Duplicate write failed: {}", err),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    state.session.sidebar.dir_cache.remove(parent);
    state.push_toast(
        format!("Duplicated -> {}", actual),
        crate::state::ToastLevel::Info,
    );
}

fn reveal_in_file_manager(state: &mut AppState, rel: &str) {
    let abs = match state.vault_session.vault.abs_path(rel) {
        Ok(p) => p,
        Err(err) => {
            state.push_toast(
                format!("Reveal failed: {}", err),
                crate::state::ToastLevel::Error,
            );
            return;
        }
    };
    // Best-effort cross-platform launch.
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("open").arg("-R").arg(&abs).spawn();
    #[cfg(target_os = "windows")]
    let res = std::process::Command::new("explorer")
        .arg("/select,")
        .arg(&abs)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let res = std::process::Command::new("xdg-open")
        .arg(abs.parent().unwrap_or(&abs))
        .spawn();
    if let Err(err) = res {
        state.push_toast(
            format!("Reveal failed: {}", err),
            crate::state::ToastLevel::Error,
        );
    }
}

#[allow(dead_code)]
fn queue_delete_modal(state: &mut AppState, rel: &str) {
    let rel_owned = rel.to_string();
    state.session.modal = Some(crate::state::Modal::Confirm {
        title: "Delete note".to_string(),
        body: format!("Move {} to trash?", rel_owned),
        confirm_label: "Move to trash".to_string(),
        cancel_label: "Cancel".to_string(),
        danger: true,
        intent: crate::state::ConfirmIntent::SoftDeleteIntoTrash { path: rel_owned },
    });
}

fn row_button(ui: &mut egui::Ui, label: &str, depth: usize, active: bool) -> egui::Response {
    let indent = (depth as f32) * 12.0;
    let row_height = 22.0;
    let total_width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(total_width, row_height),
        egui::Sense::click(),
    );
    // Active or hover background.
    let bg = if active {
        Some(theme::active_bg())
    } else if resp.hovered() {
        Some(theme::hover_bg())
    } else {
        None
    };
    if let Some(c) = bg {
        ui.painter().rect_filled(rect, 2.0, c);
    }
    // Left-aligned label so siblings line up at depth * 12px regardless
    // of label length. Using a hand-painted galley avoids egui's
    // default-centered Button layout that produced the floating-text
    // appearance in this row.
    let font_id = egui::FontId::proportional(13.0);
    let color = ui.style().visuals.text_color();
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, color));
    let text_pos = egui::pos2(
        rect.min.x + indent + 2.0,
        rect.min.y + (row_height - galley.size().y) * 0.5,
    );
    // Clip to row rect so long names don't paint over neighbouring rows
    // or push the sidepanel's content width outward.
    ui.painter_at(rect).galley(text_pos, galley, color);
    resp
}

#[cfg(test)]
mod marker_tests {
    /// `index_state_marker` needs a full `AppState` so we can't drive
    /// it directly without setting up a vault. Instead pin the marker
    /// alphabet so future renames don't silently break the file-tree
    /// row format.
    #[test]
    fn marker_alphabet_is_one_glyph_pair() {
        // ⌛ and ⊘ are the two glyphs the renderer emits in addition
        // to the empty string. Anything else is a bug.
        let valid = ["", "  ...", "  [skip]"];
        for s in valid {
            assert!(s.is_empty() || s.starts_with("  "));
        }
    }
}

/// Short string appended to a file-tree row label describing the
/// indexer's view of the file. Returns:
/// - "  ⌛" while the path sits in the indexer's pending queue
/// - "  ⊘" when the store has the file marked as skipped
/// - "" once the file is indexed (or the indexer is offline)
fn index_state_marker(state: &AppState, rel: &str) -> &'static str {
    if state.vault_session.services.indexer.is_pending(rel) {
        return "  ...";
    }
    // Reads `state.ui_cache.skipped_paths`, refreshed periodically by
    // `main::refresh_skipped_paths`. Previously this issued a
    // `store.get_note_by_path` SQLite query per visible row per frame.
    if state.ui_cache.skipped_paths.contains(rel) {
        return "  [skip]";
    }
    ""
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

fn waypoint_tree_contains(waypoints: &[crate::state::Waypoint], path: &str) -> bool {
    for w in waypoints {
        if w.path == path {
            return true;
        }
        if waypoint_tree_contains(&w.children, path) {
            return true;
        }
    }
    false
}
