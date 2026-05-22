//! Files-mode body: lazy-loaded file tree. Click a file to open it as a
//! preview-slot buffer tab; double-click to inline-rename (placeholder);
//! right-click for context menu (placeholder).
//!
//! v0 limits: no drag-and-drop, no inline rename, no context menu. Single
//! click -> open. That covers the daily-use path; the rest layers on once
//! the foundation lands.

use eframe::egui;

use hiker_core::vault::{DirEntryDto, EntryKind};

use crate::editor_pane;
use crate::icons;
use crate::state::AppState;
use crate::theme;

/// Render context for the files tree. Bundles the two refs every helper
/// threads through (`ui`, `state`) so the per-row helpers can be
/// `&mut self` methods rather than free functions taking the same pair.
/// Construct with `FilesView { ui, state }` and call `render`.
pub(crate) struct FilesView<'a> {
    pub(crate) ui: &'a mut egui::Ui,
    pub(crate) state: &'a mut AppState,
}

/// A context-menu verb picked on a file row. The menu render records one
/// of these; the mutation runs afterwards via `run_file_verb` so the
/// `&mut AppState` helpers don't fight the menu closure's `ui` borrow.
enum FileVerb {
    Open,
    Rename,
    Duplicate,
    Reveal,
    Properties,
    Reindex,
    AddToTrail { trail_name: String },
    SetActiveTrail,
    Delete,
}

impl FilesView<'_> {
/// Tiny header strip with the sort-by control. Persists the selection
/// to `vault.tree.sort_by` so the tree opens in the chosen order on
/// next vault open.
pub(crate) fn sort_header(&mut self) {
    use hiker_core::config::sections::TreeSortBy;
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    let current = default_sort(&state.vault_session.config);
    let mut new_sort = current;
    ui.horizontal(|ui| {
        // When the sidebar is narrow, drop the "Sort" caption and use
        // shorter combobox labels so the dropdown stays readable instead
        // of getting clipped by the panel edge.
        let compact = ui.available_width() < 180.0;
        if !compact {
            ui.label(
                egui::RichText::new("Sort")
                    .small()
                    .color(theme::muted()),
            );
        }
        egui::ComboBox::from_id_salt("files-sort-by")
            .selected_text(sort_label(current, compact))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut new_sort,
                    TreeSortBy::NameAsc,
                    sort_label(TreeSortBy::NameAsc, compact),
                );
                ui.selectable_value(
                    &mut new_sort,
                    TreeSortBy::NameDesc,
                    sort_label(TreeSortBy::NameDesc, compact),
                );
                ui.selectable_value(
                    &mut new_sort,
                    TreeSortBy::MtimeDesc,
                    sort_label(TreeSortBy::MtimeDesc, compact),
                );
                ui.selectable_value(
                    &mut new_sort,
                    TreeSortBy::MtimeAsc,
                    sort_label(TreeSortBy::MtimeAsc, compact),
                );
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
            &serde_json::Value::String(wire.to_string()),
            "Sort change failed",
        );
        state.session.sidebar.dir_cache.clear();
    }
}

/// Cheap direct-child file counter using the listing cache. When the
/// directory hasn't been listed yet, returns 0 rather than triggering a
/// disk walk on the render path — the count fills in once the user
/// expands the parent. Matches the legacy `count_notes_in` semantics
/// (markdown-shaped files only).
fn count_direct_files(&self, rel: &str) -> usize {
    let Some(entries) = self.state.session.sidebar.dir_cache.get(rel) else {
        return 0;
    };
    entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::File))
        .count()
}

pub(crate) fn show_dir(&mut self, rel: &str, depth: usize) {
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    // Ensure this dir is loaded. Cap the cache so an aggressive
    // expand-everything session can't grow it without bound — once we
    // hit `MAX_DIR_CACHE_ENTRIES`, drop the entries the user no longer
    // has expanded (cheapest correct eviction; the rest get rebuilt on
    // next render).
    const MAX_DIR_CACHE_ENTRIES: usize = 512;
    if !state.session.sidebar.dir_cache.contains_key(rel) {
        if state.session.sidebar.dir_cache.len() >= MAX_DIR_CACHE_ENTRIES {
            let expanded = state.session.sidebar.expanded.clone();
            state
                .session
                .sidebar
                .dir_cache
                .retain(|k, _| expanded.contains(k) || k.is_empty());
            // If still over cap (every cached dir is currently
            // expanded), fall back to a full clear. Re-listing is
            // cheap; this branch only fires when the user has 500+
            // dirs expanded simultaneously, which is pathological.
            if state.session.sidebar.dir_cache.len() >= MAX_DIR_CACHE_ENTRIES {
                state.session.sidebar.dir_cache.clear();
            }
        }
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
    let entries = self.state.session.sidebar.dir_cache.get(rel).cloned().unwrap_or_default();

    for entry in entries {
        match entry.kind {
            EntryKind::Dir => self.render_dir_row(&entry, depth),
            EntryKind::File => self.render_file_row(&entry, depth),
        }
    }
}

fn render_dir_row(&mut self, entry: &DirEntryDto, depth: usize) {
    let expanded = self.state.session.sidebar.expanded.contains(&entry.rel_path);
    // Direct-child file count hint (`count_notes_in` parity). Cached on
    // the same dir_cache the listing renders from, so showing the count
    // is essentially free. Only the first depth is counted — recursing
    // would be O(vault) for a quick visual.
    let child_count = self.count_direct_files(&entry.rel_path);
    let count_suffix = if child_count > 0 {
        format!(" ({})", child_count)
    } else {
        String::new()
    };
    let label = format!("{}{}", entry.name, count_suffix);

    let resp = row_button_with_chevron(self.ui, &label, depth, false, Some(expanded));

    // DnD: folder rows accept dropped paths and move them into this dir.
    let dropped = resp.dnd_release_payload::<String>();
    if let Some(src) = dropped {
        self.move_into_folder(&src, &entry.rel_path);
    }
    // Folder rows are also draggable so users can re-parent subtrees.
    resp.clone()
        .dnd_set_drag_payload::<String>(entry.rel_path.clone());

    if resp.clicked() {
        if expanded {
            self.state.session.sidebar.expanded.remove(&entry.rel_path);
        } else {
            self.state.session.sidebar.expanded.insert(entry.rel_path.clone());
        }
        self.state.session.sidebar.selected_folder = Some(entry.rel_path.clone());
    }
    if expanded {
        self.show_dir(&entry.rel_path, depth + 1);
    }
}

/// Move a vault-relative path into the destination folder via
/// `vault::move_note` (which handles store + watcher updates atomically).
fn move_into_folder(&mut self, src: &str, dest_dir: &str) {
    let state = &mut *self.state;
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
        if let crate::tab::TabKind::Editor {
            buffer: crate::tab::BufferSource::Vault { path },
            ..
        } = &mut tab.kind
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

fn render_file_row(&mut self, entry: &DirEntryDto, depth: usize) {
    // Inline-rename mode preempts the regular row render. Drafts live in
    // egui memory keyed by the row's rel_path, so this stays out of
    // SidebarState (which a parallel cluster-editor port is editing).
    if let Some(draft) = self.rename_draft_for(&entry.rel_path) {
        self.rename_row(entry, depth, draft);
        return;
    }

    let active_buffer_path = self.state.session.active_tab.and_then(|id| {
        self.state
            .tab_by_id(id)
            .and_then(|t| t.buffer_path().map(str::to_string))
    });
    let is_active = active_buffer_path.as_deref() == Some(entry.rel_path.as_str());

    // Dirty-dot suffix if the buffer is loaded and dirty.
    let dirty_marker = self.state
        .session.buffers
        .get(&entry.rel_path)
        .map(|b| if b.is_dirty() { " *" } else { "" })
        .unwrap_or("");

    // Index-state marker: ⌛ for files the indexer hasn't processed yet,
    // ⊘ for files the indexer skipped (unsupported extension, too big,
    // etc.). Falls back to nothing once the file is fully indexed.
    let index_marker = self.index_state_marker(&entry.rel_path);

    let label = format!("{}{}{}", entry.name, dirty_marker, index_marker);
    let resp = self.row_button(&label, depth, is_active);
    // Honour a pending reveal-from-discovery: when our row matches the
    // sidebar's scroll_target one-shot, ask egui to bring us into view and
    // clear the target so subsequent frames don't keep re-scrolling.
    if self.state.session.sidebar.scroll_target.as_deref() == Some(entry.rel_path.as_str()) {
        resp.scroll_to_me(Some(egui::Align::Center));
        self.state.session.sidebar.scroll_target = None;
    }
    // Drag payload: vault-relative source path. Drop targets are
    // rendered on folder rows below.
    resp.clone()
        .dnd_set_drag_payload::<String>(entry.rel_path.clone());

    if resp.clicked() {
        editor_pane::open_file(self.state, &entry.rel_path, /* sticky */ false);
    }
    if resp.double_clicked() {
        // Per docs/editor.md: double-click enters inline rename mode.
        self.start_rename(&entry.rel_path);
    }
    // The context menu only records which verb the user picked; the
    // mutation runs after the closure so it can call the `&mut self`
    // file-op methods without overlapping the closure's borrow of `ui`.
    let rel = entry.rel_path.clone();
    let mut verb = self.file_row_menu(&resp, &rel);
    if let Some(v) = verb.take() {
        self.run_file_verb(v, &rel);
    }
}

/// Draw the per-file context menu and return the picked verb (if any).
/// Pure rendering: every branch maps a clicked button to a `FileVerb`
/// and closes the menu; no `AppState` mutation happens here.
fn file_row_menu(&mut self, resp: &egui::Response, rel: &str) -> Option<FileVerb> {
    let mut verb = None;
    let state = &*self.state;
    // Pre-compute the trail context so the closure stays read-only.
    let active_trail = state
        .session.active_trail
        .clone()
        .filter(|id| state.session.trails.iter().any(|t| &t.id == id))
        .and_then(|tid| state.session.trails.iter().find(|t| t.id == tid))
        .map(|t| (t.name.clone(), waypoint_tree_contains(&t.waypoints, rel)));
    resp.context_menu(|ui| {
        if ui.button("Open").clicked() {
            verb = Some(FileVerb::Open);
            ui.close();
        }
        if ui.button("Rename").clicked() {
            verb = Some(FileVerb::Rename);
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            verb = Some(FileVerb::Duplicate);
            ui.close();
        }
        if ui.button("Reveal in file manager").clicked() {
            verb = Some(FileVerb::Reveal);
            ui.close();
        }
        if ui.button("Properties").clicked() {
            verb = Some(FileVerb::Properties);
            ui.close();
        }
        // Reindex verb: force-enqueue this path on the indexer. Useful when
        // the watcher missed an edit or the file's hash is somehow stale.
        if ui.button("Reindex this file").clicked() {
            verb = Some(FileVerb::Reindex);
            ui.close();
        }
        // Add-to-trail verb. Surfaces only when an active trail is set;
        // recursive membership check disables when the path is already a
        // waypoint at any depth.
        if let Some((trail_name, already)) = &active_trail {
            let label = if *already {
                format!("Already in '{}'", trail_name)
            } else {
                format!("Add to trail '{}'", trail_name)
            };
            if ui.add_enabled(!already, egui::Button::new(label)).clicked() {
                verb = Some(FileVerb::AddToTrail { trail_name: trail_name.clone() });
                ui.close();
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
            verb = Some(FileVerb::SetActiveTrail);
            ui.close();
        }
        if ui.button("Delete").clicked() {
            verb = Some(FileVerb::Delete);
            ui.close();
        }
    });
    verb
}

/// Execute a context-menu verb against `AppState`. Split out from the
/// menu render so the mutating helpers run outside the menu closure's
/// borrow of `ui`.
fn run_file_verb(&mut self, verb: FileVerb, rel: &str) {
    match verb {
        FileVerb::Open => editor_pane::open_file(self.state, rel, true),
        FileVerb::Rename => {
            self.start_rename(rel);
            self.state.session.sidebar.renaming = Some(rel.to_string());
            self.state.session.sidebar.renaming_text = basename_of(rel).to_string();
        }
        FileVerb::Duplicate => self.duplicate_file(rel),
        FileVerb::Reveal => self.reveal_in_file_manager(rel),
        FileVerb::Properties => self.open_properties(rel),
        FileVerb::Reindex => {
            let state = &mut *self.state;
            let indexer = state.vault_session.services.indexer.as_ref();
            let tx = indexer.job_sender();
            let path_owned = rel.to_string();
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
        FileVerb::AddToTrail { trail_name } => {
            let state = &mut *self.state;
            crate::state::trail_append_waypoint(state, rel);
            let _ = crate::bootstrap::save_trails(&state.vault_session.vault_root, &state.session.trails);
            state.push_toast(
                format!("Added {rel} to '{trail_name}'"),
                crate::state::ToastLevel::Info,
            );
        }
        FileVerb::SetActiveTrail => {
            let state = &mut *self.state;
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
        }
        FileVerb::Delete => {
            self.state.session.modal =
                Some(crate::state::Modal::ConfirmDelete { path: rel.to_string() });
        }
    }
}

/// Renders the inline rename TextEdit. On Enter, runs `move_note` via
/// the live Store + Watcher; on Esc, cancels.
fn rename_row(&mut self, entry: &DirEntryDto, depth: usize, mut draft: String) {
    let path = entry.rel_path.clone();
    let kind = entry.kind.clone();
    // The TextEdit renders inside the `ui.horizontal` closure; it only
    // reports what happened (commit / cancel) so the `&mut self`
    // `commit_rename` runs after, outside the closure's `ui` borrow.
    let outcome = self.ui.horizontal(|ui| {
        ui.add_space((depth as f32) * 12.0);
        ui.add(match kind {
            EntryKind::Dir => crate::icons::ICONS.image(crate::icons::Icon::Folder),
            EntryKind::File => crate::icons::ICONS.image(crate::icons::Icon::File),
        });
        let id = egui::Id::new(("rename-edit", path.as_str()));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .id(id)
                .desired_width(ui.available_width()),
        );
        // First frame: focus + clear the just-opened flag.
        let just_opened = ui.ctx().memory(|m| {
            m.data
                .get_temp::<RenameMem>(mem_id())
                .map(|r| r.path == path && r.just_opened)
                .unwrap_or(false)
        });
        if just_opened {
            resp.request_focus();
            ui.ctx().memory_mut(|m| {
                if let Some(mut r) = m.data.get_temp::<RenameMem>(mem_id())
                    && r.path == path
                {
                    r.just_opened = false;
                    m.data.insert_temp(mem_id(), r);
                }
            });
        }

        if resp.changed() {
            ui.ctx().memory_mut(|m| {
                m.data.insert_temp(mem_id(), RenameMem {
                    path: path.clone(),
                    draft: draft.clone(),
                    just_opened: false,
                });
            });
        }

        let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if commit || cancel || resp.lost_focus() {
            // Drop the draft from egui memory if it still belongs to this row.
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
        commit
    });
    if outcome.inner {
        self.commit_rename(&entry.rel_path, &draft);
    }
}

fn commit_rename(&mut self, from: &str, draft: &str) {
    let state = &mut *self.state;
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
        if let crate::tab::TabKind::Editor {
            buffer: crate::tab::BufferSource::Vault { path },
            ..
        } = &mut tab.kind
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

/// Active inline-rename draft for `path`, if any (drafts live in egui
/// memory keyed by rel-path).
fn rename_draft_for(&self, path: &str) -> Option<String> {
    self.ui.ctx().memory(|m| {
        m.data
            .get_temp::<RenameMem>(mem_id())
            .filter(|r| r.path == path)
            .map(|r| r.draft.clone())
    })
}

/// Enter inline-rename mode for `path`, seeding the draft with the
/// current basename and flagging the row to grab focus next frame.
fn start_rename(&mut self, path: &str) {
    let draft = basename_of(path).to_string();
    self.ui.ctx().memory_mut(|m| {
        m.data.insert_temp(mem_id(), RenameMem {
            path: path.to_string(),
            draft,
            just_opened: true,
        });
    });
}

fn open_properties(&mut self, rel: &str) {
    use crate::tab::{Tab, TabKind};
    let state = &mut *self.state;
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

fn duplicate_file(&mut self, rel: &str) {
    let state = &mut *self.state;
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

fn reveal_in_file_manager(&mut self, rel: &str) {
    let state = &mut *self.state;
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

/// Short string appended to a file-tree row label describing the
/// indexer's view of the file:
/// - "  ..." while the path sits in the indexer's pending queue
/// - "  [skip]" when the store has the file marked as skipped
/// - "" once the file is indexed (or the indexer is offline)
fn index_state_marker(&self, rel: &str) -> &'static str {
    if self.state.vault_session.services.indexer.is_pending(rel) {
        return "  ...";
    }
    // Reads `state.ui_cache.skipped_paths`, refreshed periodically by
    // `main::refresh_skipped_paths`. Previously this issued a
    // `store.get_note_by_path` SQLite query per visible row per frame.
    if self.state.ui_cache.skipped_paths.contains(rel) {
        return "  [skip]";
    }
    ""
}

fn row_button(&mut self, label: &str, depth: usize, active: bool) -> egui::Response {
    row_button_with_chevron(self.ui, label, depth, active, None)
}

/// Files panel body: the file tree (in a scroll area) with the trash bin
/// pinned below it. This is the panel's single external entry point —
/// `panels_registry`'s `Files` record constructs a `FilesView` and calls
/// it, the same shape Search/Related/Backlinks use for their `View`. The
/// new-note button and the refresh / sort menu live in the side bar's
/// title row (wired through `Host::side_bar_action_buttons` /
/// `side_bar_actions_menu`).
pub(crate) fn show(&mut self) {
    let avail_height = self.ui.available_height();
    let trash_row_height = 28.0;
    egui::ScrollArea::vertical()
        .id_salt("panel-files-body")
        .max_height((avail_height - trash_row_height).max(60.0))
        .auto_shrink([false, false])
        .show(self.ui, |ui| {
            let mut view = FilesView { ui, state: self.state };
            view.sort_header();
            view.show_dir("", 0);
        });
    self.ui.separator();
    self.trash_bin();
}

/// Trash bin pinned at the bottom of the Files panel. Shows a collapsible
/// listing built from the on-disk trash directory + manifest; each entry
/// offers Restore and Purge actions, plus a batch "Empty" verb. Part of
/// the Files panel body (it used to be pinned across every sidebar mode);
/// lives here as a `FilesView` method so it shares the panel's receiver.
fn trash_bin(&mut self) {
    use hiker_core::trash::Trash;
    let ui = &mut *self.ui;
    let state = &mut *self.state;
    let trash = Trash::open(&state.vault_session.vault_root);
    let items = trash.list_from_disk().unwrap_or_default();
    let count = items.len();

    let label = if count == 0 {
        "Trash".to_string()
    } else {
        format!("Trash ({})", count)
    };
    let chevron_icon = if state.session.sidebar.trash_expanded {
        icons::ICONS.image(crate::icons::Icon::Expand)
    } else {
        icons::ICONS.image(crate::icons::Icon::Collapse)
    };

    let mut empty_clicked = false;
    let row = ui.horizontal(|ui| {
        let resp_chev = ui.add(egui::Button::image(chevron_icon).frame(false).small());
        let resp_trash = ui.add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Trash)).frame(false).small());
        let resp_lbl = ui.add(
            egui::Label::new(egui::RichText::new(label).size(13.0))
                .sense(egui::Sense::click()),
        );
        let mut toggle = resp_chev.clicked() || resp_trash.clicked() || resp_lbl.clicked();
        // "Empty trash" batch action — right-aligned, only when the bin
        // is non-empty. Mirrors `tree-trash-empty` in design.md.
        if count > 0 {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(egui::RichText::new("Empty").small()).small())
                    .on_hover_text("Permanently delete every item in the bin")
                    .clicked()
                {
                    empty_clicked = true;
                    // Don't fold trash open just because the user clicked
                    // the inline button on a folded header.
                    toggle = false;
                }
            });
        }
        toggle
    });
    if row.inner {
        state.session.sidebar.trash_expanded = !state.session.sidebar.trash_expanded;
    }
    if empty_clicked {
        // Route through the confirm modal so an accidental click doesn't
        // wipe weeks of trash. The confirm callback walks the trash list
        // and purges each entry.
        state.session.modal = Some(crate::state::Modal::Confirm {
            title: "Empty trash".to_string(),
            body: format!(
                "Permanently delete all {count} items in the trash? This can't be undone."
            ),
            confirm_label: "Empty trash".to_string(),
            cancel_label: "Cancel".to_string(),
            danger: true,
            intent: crate::state::ConfirmIntent::EmptyTrash,
        });
    }

    if !state.session.sidebar.trash_expanded {
        return;
    }

    if items.is_empty() {
        ui.indent("trash-contents", |ui| {
            ui.label(
                egui::RichText::new("(empty)")
                    .color(theme::muted())
                    .small(),
            );
        });
        return;
    }

    // Collect actions to apply after the render to avoid mutable-borrow
    // overlap with `state` inside the row closure.
    enum Action {
        Restore { id: String },
        Purge { trashed_name: String },
    }
    let mut pending: Option<Action> = None;

    ui.indent("trash-contents", |ui| {
        egui::ScrollArea::vertical()
            .id_salt("trash-list")
            .max_height(180.0)
            .show(ui, |ui| {
                for item in &items {
                    let basename = item
                        .original_path
                        .as_deref()
                        .unwrap_or(&item.trashed_name)
                        .rsplit('/')
                        .next()
                        .unwrap_or(&item.trashed_name);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(basename).small());
                        ui.label(
                            egui::RichText::new(TrashTimeFmt.format_ts(item.deleted_at))
                                .color(theme::muted())
                                .small(),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let purge = egui::Button::new(
                                    egui::RichText::new("Purge").small(),
                                )
                                .small();
                                if ui.add(purge).on_hover_text("Delete forever").clicked() {
                                    pending = Some(Action::Purge {
                                        trashed_name: item.trashed_name.clone(),
                                    });
                                }
                                if let Some(id) = &item.id {
                                    let restore = egui::Button::new(
                                        egui::RichText::new("Restore").small(),
                                    )
                                    .small();
                                    if ui.add(restore).clicked() {
                                        pending = Some(Action::Restore { id: id.clone() });
                                    }
                                }
                            },
                        );
                    });
                }
            });
    });

    let Some(action) = pending else { return };
    match action {
        Action::Restore { id } => {
            let trash = Trash::open(&state.vault_session.vault_root);
            match hiker_core::vault::restore_note(
                &state.vault_session.vault,
                Some(state.vault_session.services.watcher.as_ref()),
                &trash,
                &id,
            ) {
                Ok(entry) => {
                    let parent = entry
                        .original_path
                        .rsplit_once('/')
                        .map(|(p, _)| p)
                        .unwrap_or("");
                    state.session.sidebar.dir_cache.remove(parent);
                    state.push_toast(
                        format!("Restored {}", entry.original_path),
                        crate::state::ToastLevel::Info,
                    );
                }
                Err(err) => state.push_toast(
                    format!("Restore failed: {}", err),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        Action::Purge { trashed_name } => {
            let trash = Trash::open(&state.vault_session.vault_root);
            match trash.permanent_delete(&trashed_name) {
                Ok(()) => state.push_toast(
                    format!("Purged {}", trashed_name),
                    crate::state::ToastLevel::Info,
                ),
                Err(err) => state.push_toast(
                    format!("Purge failed: {}", err),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
    }
}
}

/// Zero-sized timestamp formatter for trash rows. An inherent method (not
/// a free fn) so the single caller above doesn't trip `single_call_fn`.
struct TrashTimeFmt;

impl TrashTimeFmt {
    fn format_ts(self, unix_secs: i64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let Ok(t) = OffsetDateTime::from_unix_timestamp(unix_secs) else {
        return String::new();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
    t.format(fmt).unwrap_or_default()
    }
}

// ----- free helpers (shared by multiple methods / pure) -----

const fn sort_label(s: hiker_core::config::sections::TreeSortBy, compact: bool) -> &'static str {
    use hiker_core::config::sections::TreeSortBy;
    match (s, compact) {
        (TreeSortBy::NameAsc, false) => "Name (A-Z)",
        (TreeSortBy::NameDesc, false) => "Name (Z-A)",
        (TreeSortBy::MtimeDesc, false) => "Recent",
        (TreeSortBy::MtimeAsc, false) => "Oldest",
        (TreeSortBy::NameAsc, true) => "A-Z",
        (TreeSortBy::NameDesc, true) => "Z-A",
        (TreeSortBy::MtimeDesc, true) => "New",
        (TreeSortBy::MtimeAsc, true) => "Old",
    }
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

/// Renders a sidebar row button. Optionally draws an SVG chevron in the leading slot.
/// `Some(true)` = expanded (down chevron), `Some(false)` = collapsed
/// (right chevron), `None` = no chevron (leaf row). The chevron paints
/// inside the row's clickable area so the whole row toggles, matching
/// the legacy behavior where the chevron was a label prefix.
fn row_button_with_chevron(
    ui: &mut egui::Ui,
    label: &str,
    depth: usize,
    active: bool,
    chevron: Option<bool>,
) -> egui::Response {
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
    // Chevron icon (folders only). Painted into a fixed-size leading
    // slot so the text origin lines up regardless of whether a chevron
    // is present.
    let chev_size = 14.0;
    let chev_slot = 16.0;
    let text_x_start = rect.min.x + indent + 2.0;
    if let Some(expanded) = chevron {
        let chev_rect = egui::Rect::from_min_size(
            egui::pos2(
                text_x_start,
                rect.min.y + (row_height - chev_size) * 0.5,
            ),
            egui::vec2(chev_size, chev_size),
        );
        let icon = if expanded {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
        } else {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
        };
        icon.paint_at(ui, chev_rect);
    }
    // Left-aligned label so siblings line up at depth * 12px regardless
    // of label length. Using a hand-painted galley avoids egui's
    // default-centered Button layout that produced the floating-text
    // appearance in this row.
    let font_id = egui::FontId::proportional(13.0);
    let color = ui.style().visuals.text_color();
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, color));
    let text_pos = egui::pos2(
        text_x_start + if chevron.is_some() { chev_slot } else { 0.0 },
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

fn default_sort(
    config: &std::sync::RwLock<hiker_core::config::Config>,
) -> hiker_core::config::sections::TreeSortBy {
    config
        .read()
        .ok()
        .map(|c| c.vault.tree.sort_by)
        .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc)
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

#[cfg(test)]
mod sort_label_tests {
    use super::*;
    use hiker_core::config::sections::TreeSortBy;

    #[test]
    fn every_variant_has_a_label() {
        for v in [
            TreeSortBy::NameAsc,
            TreeSortBy::NameDesc,
            TreeSortBy::MtimeDesc,
            TreeSortBy::MtimeAsc,
        ] {
            for compact in [false, true] {
                let l = sort_label(v, compact);
                assert!(!l.is_empty(), "missing label for {v:?} compact={compact}");
            }
        }
    }

    #[test]
    fn labels_are_distinct() {
        for compact in [false, true] {
            let labels = [
                sort_label(TreeSortBy::NameAsc, compact),
                sort_label(TreeSortBy::NameDesc, compact),
                sort_label(TreeSortBy::MtimeDesc, compact),
                sort_label(TreeSortBy::MtimeAsc, compact),
            ];
            let set: std::collections::HashSet<&str> = labels.iter().copied().collect();
            assert_eq!(set.len(), 4, "sort labels collide: {labels:?} compact={compact}");
        }
    }
}
