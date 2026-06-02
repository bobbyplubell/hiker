//! Files-mode body: lazy-loaded file tree. Click a file to open it as a
//! preview-slot buffer tab (or the board view for a board-doc);
//! double-click to inline-rename; right-click for the per-row verb menu
//! (open / rename / duplicate / reveal / properties / reindex /
//! add-to-trail / set-active-trail / add-to-board / delete). Folder rows
//! expand/collapse and accept drag-dropped paths to re-parent subtrees.
//!
//! Migrated onto the Files `Feature`'s `SidebarSurface`: rendering goes
//! through the narrow `feature::Ctx` instead of `&mut AppState`. The
//! tree UI state lives in `AppState::file_tree_state` (reached via
//! `ctx.state`); directory listings come from `ctx.vault`; the index /
//! dirty / skip / trail decorations come from the once-per-frame
//! `FileTreeDeco` snapshot (see the `mod.rs` decoration note); the
//! active-note highlight from `ctx.active_path`. Every broad mutation
//! (open a note / board, move, rename, duplicate, reindex, add-to-trail,
//! set-active-trail, add-to-board, delete) is queued via `ctx.defer` and
//! applied with full `&mut AppState` after the surface returns.

use eframe::egui;

use hiker_core::vault::{DirEntryDto, EntryKind};

use crate::feature::Ctx;
use crate::state::{AppState, FileTreeState};
use hiker_theme as theme;

/// A context-menu verb picked on a file row. The menu render records one
/// of these; the mutation runs afterwards as a deferred effect so the
/// `&mut AppState` helpers don't fight the menu closure's `ui` borrow nor
/// the narrow `Ctx` borrow.
enum FileVerb {
    Open,
    Rename,
    Duplicate,
    Reveal,
    Properties,
    Reindex,
    AddToTrail { trail_name: String },
    SetActiveTrail,
    /// Open a board-doc in the board view (vs. the default buffer).
    OpenAsBoard,
    /// Open a `.canvas` file in the spatial canvas editor (the default
    /// click already routes here). status: canvas-file-tree-glyph
    OpenAsCanvas,
    /// Open a `.canvas` file's raw JSON in the standard editor (the
    /// "View as JSON" escape hatch). status: canvas-file-tree-glyph
    ViewCanvasAsJson,
    /// Append this note as a card to `board_rel`'s `column`.
    AddToBoard { board_rel: String, column: String },
    /// Insert this row's vault path as a file-node pointer into the `.canvas`
    /// at `canvas_rel` (whether or not that canvas is open).
    /// status: canvas-add-to-canvas-verb
    AddToCanvas { canvas_rel: String },
    /// Snapshot this trail-doc into a fresh `.canvas` and open it framed-to-fit.
    /// Only offered on `.hiker/trails/*.md` rows. status: canvas-export-trail-verb
    ExportTrailToCanvas,
    /// Extract a non-md source into a `.md` sidecar and index it
    /// (`extract-trigger-on-demand`).
    MakeSearchable,
    /// Open a non-md source in the OS default handler
    /// (`extract-open-original-external`).
    OpenExternal,
    Delete,
}

/// Shared per-frame context for the files sidebar. Wraps the narrow
/// feature `Ctx` so the render/mutation helpers can be `&mut self`
/// methods on one receiver. `ui` is threaded as a method arg rather than
/// held here so the deferred closures don't contend with the `ui` borrow.
pub(crate) struct FilesCtx<'a, 'c> {
    pub(crate) ctx: &'a mut Ctx<'c>,
}

impl FilesCtx<'_, '_> {
    /// Mutable handle to the feature's own file-tree state slice.
    fn st(&mut self) -> &mut FileTreeState {
        self.ctx
            .state
            .downcast_mut::<FileTreeState>()
            .expect("file_tree state")
    }

    /// Immutable handle to the feature's own file-tree state slice.
    fn st_ref(&self) -> &FileTreeState {
        self.ctx
            .state
            .downcast_ref::<FileTreeState>()
            .expect("file_tree state")
    }

    /// Active sort order from persisted vault settings.
    fn sort(&self) -> hiker_core::config::sections::TreeSortBy {
        default_sort(self.ctx.config)
    }

    /// Sidebar entry point: refresh the decoration snapshot for next
    /// frame, then draw the sort header + the tree from the vault root.
    pub(crate) fn render(&mut self, ui: &mut egui::Ui) {
        // Pre-pass: snapshot the AppState-only row decorations (dirty
        // buffers, skipped paths, active-trail membership) into
        // `file_tree_state.deco` for next frame. Deferred so it runs with
        // full `&mut AppState`; the render below reads only the snapshot,
        // keeping the render path within the narrow `Ctx`.
        self.ctx.defer(refresh_deco);
        self.sort_header(ui);
        let _g = crate::profiling::FrameProf::guard("files:tree");
        self.show_dir(ui, "", 0);
    }

    /// Tiny header strip with the sort-by control. A sort change persists
    /// `vault.tree.sort_by` and clears the dir cache (deferred), so the
    /// tree re-fetches in the chosen order.
    fn sort_header(&mut self, ui: &mut egui::Ui) {
        use hiker_core::config::sections::TreeSortBy;
        let current = self.sort();
        let mut new_sort = current;
        ui.horizontal(|ui| {
            // When the sidebar is narrow, drop the "Sort" caption and use
            // shorter combobox labels so the dropdown stays readable.
            let compact = ui.available_width() < 180.0;
            if !compact {
                ui.label(egui::RichText::new("Sort").small().color(theme::muted()));
            }
            egui::ComboBox::from_id_salt("files-sort-by")
                .selected_text(sort_label(current, compact))
                .show_ui(ui, |ui| {
                    for v in [
                        TreeSortBy::NameAsc,
                        TreeSortBy::NameDesc,
                        TreeSortBy::MtimeDesc,
                        TreeSortBy::MtimeAsc,
                    ] {
                        ui.selectable_value(&mut new_sort, v, sort_label(v, compact));
                    }
                });
        });
        if new_sort != current {
            let wire = new_sort.as_str().to_string();
            self.ctx.defer(move |app| {
                app.set_setting(
                    hiker_core::config::SettingsScope::Vault,
                    "vault.tree.sort_by",
                    &serde_json::Value::String(wire),
                    "Sort change failed",
                );
                app.file_tree_state.dir_cache.clear();
            });
        }
    }

    /// Cheap direct-child file counter using the listing cache. Returns 0
    /// when the directory hasn't been listed yet rather than triggering a
    /// disk walk on the render path — the count fills in once the parent
    /// expands. Markdown-shaped files only (legacy `count_notes_in`).
    fn count_direct_files(&self, rel: &str) -> usize {
        let Some(entries) = self.st_ref().dir_cache.get(rel) else {
            return 0;
        };
        entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::File))
            .count()
    }

    fn show_dir(&mut self, ui: &mut egui::Ui, rel: &str, depth: usize) {
        if let Some(err) = self.ensure_listed(rel) {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("failed to list {rel}: {err}"),
            );
            return;
        }
        // Clone the entries out so we can mutate state below without
        // overlapping borrows.
        let entries = self
            .st_ref()
            .dir_cache
            .get(rel)
            .cloned()
            .unwrap_or_default();
        for entry in entries {
            match entry.kind {
                EntryKind::Dir => self.render_dir_row(ui, &entry, depth),
                EntryKind::File => self.render_file_row(ui, &entry, depth),
            }
        }
    }

    /// Ensure `rel`'s listing is cached, capping the cache so an
    /// expand-everything session can't grow it unbounded. Returns a
    /// listing error string on disk failure (the row prints it).
    fn ensure_listed(&mut self, rel: &str) -> Option<String> {
        const MAX_DIR_CACHE_ENTRIES: usize = 512;
        if self.st_ref().dir_cache.contains_key(rel) {
            return None;
        }
        if self.st_ref().dir_cache.len() >= MAX_DIR_CACHE_ENTRIES {
            let expanded = self.st_ref().expanded.clone();
            let st = self.st();
            st.dir_cache
                .retain(|k, _| expanded.contains(k) || k.is_empty());
            // If still over cap (every cached dir is currently expanded),
            // fall back to a full clear — re-listing is cheap. Only fires
            // when 500+ dirs are expanded at once (pathological).
            if st.dir_cache.len() >= MAX_DIR_CACHE_ENTRIES {
                st.dir_cache.clear();
            }
        }
        let sort = self.sort();
        match self.ctx.vault.list_dir(rel, sort) {
            Ok(entries) => {
                self.st().dir_cache.insert(rel.to_string(), entries);
                None
            }
            Err(err) => Some(err.to_string()),
        }
    }

    fn render_dir_row(&mut self, ui: &mut egui::Ui, entry: &DirEntryDto, depth: usize) {
        let expanded = self.st_ref().expanded.contains(&entry.rel_path);
        // Direct-child file count hint. Cached on the same listing the
        // tree renders from, so showing it is essentially free. Only the
        // first depth is counted — recursing would be O(vault).
        let child_count = self.count_direct_files(&entry.rel_path);
        let count_suffix = if child_count > 0 {
            format!(" ({child_count})")
        } else {
            String::new()
        };
        let label = format!("{}{}", entry.name, count_suffix);

        let resp = row_button_with_chevron(ui, &label, depth, false, Some(expanded));

        // DnD: folder rows accept dropped paths and move them into this dir.
        if let Some(src) = resp.dnd_release_payload::<String>() {
            self.move_into_folder(&src, &entry.rel_path);
        }
        // Folder rows are also draggable so users can re-parent subtrees.
        resp.clone()
            .dnd_set_drag_payload::<String>(entry.rel_path.clone());

        if resp.clicked() {
            let rel = entry.rel_path.clone();
            let st = self.st();
            if expanded {
                st.expanded.remove(&rel);
            } else {
                st.expanded.insert(rel.clone());
            }
            st.selected_folder = Some(rel);
        }
        if expanded {
            self.show_dir(ui, &entry.rel_path, depth + 1);
        }
    }

    fn render_file_row(&mut self, ui: &mut egui::Ui, entry: &DirEntryDto, depth: usize) {
        // Inline-rename mode preempts the regular row render. Drafts live
        // in egui memory keyed by the row's rel_path, so this stays out
        // of FileTreeState.
        if let Some(draft) = rename_draft_for(ui, &entry.rel_path) {
            self.rename_row(ui, entry, depth, draft);
            return;
        }

        let is_active = self.ctx.active_path.as_deref() == Some(entry.rel_path.as_str());
        let label = format!(
            "{}{}{}{}",
            canvas_glyph_marker(&entry.rel_path),
            entry.name,
            self.dirty_marker(&entry.rel_path),
            self.index_state_marker(&entry.rel_path),
        );
        let resp = row_button_with_chevron(ui, &label, depth, is_active, None);

        // Honour a pending reveal-from-discovery: bring the matching row
        // into view once, then clear the one-shot target.
        if self.st_ref().scroll_target.as_deref() == Some(entry.rel_path.as_str()) {
            resp.scroll_to_me(Some(egui::Align::Center));
            self.st().scroll_target = None;
        }
        // Drag payload: vault-relative source path.
        resp.clone()
            .dnd_set_drag_payload::<String>(entry.rel_path.clone());

        let rel = entry.rel_path.clone();
        if resp.clicked() {
            self.open_row(&rel);
        }
        if resp.double_clicked() {
            // Per docs/editor.md: double-click enters inline rename mode.
            start_rename(ui, &rel);
        }
        // The context menu only records which verb the user picked; the
        // mutation runs afterwards (deferred) so it can use the broad
        // `&mut AppState` helpers without overlapping the `ui` borrow.
        if let Some(v) = self.file_row_menu(&resp, &rel) {
            self.run_file_verb(ui, v, &rel);
        }
    }

    /// Open a row on click: a board-doc routes to the board view, anything
    /// else opens as a buffer. The frontmatter check runs only on click,
    /// not per-frame, so the tree paint stays cheap. Deferred so the open
    /// helpers run with full `&mut AppState`.
    fn open_row(&mut self, rel: &str) {
        let rel = rel.to_string();
        self.ctx.defer(move |app| {
            if rel.ends_with(".zim") {
                // Offline encyclopedia archive — open the ZIM viewer tab
                // (HTML rendered via the `hiker-htmlview` renderer).
                // status: zim-view
                crate::panels::zim::open(app, &rel);
            } else if is_canvas_doc(&rel) {
                // A `.canvas` file opens in the spatial canvas editor by
                // default; "View as JSON" is the escape hatch.
                // status: canvas-file-tree-glyph
                crate::panels::canvas::open(app, &rel);
            } else if is_board_doc(app, &rel) {
                crate::panels::board::open(app, &rel);
            } else if is_capture_doc(app, &rel) {
                crate::panels::capture::open(app, &rel);
            } else {
                crate::editor_pane::open_file(app, &rel, /* sticky */ false);
            }
        });
    }

    /// Trailing ` *` dirty-dot when the row's loaded buffer is dirty.
    /// Reads the once-per-frame snapshot rather than `session.buffers`.
    fn dirty_marker(&self, rel: &str) -> &'static str {
        if self.st_ref().deco.dirty.contains(rel) {
            " *"
        } else {
            ""
        }
    }

    /// Index-state marker:
    /// - "  ..." while the indexer hasn't processed the path yet
    /// - "  [skip]" when the store marked it skipped
    /// - "" once indexed (or the indexer is offline)
    fn index_state_marker(&self, rel: &str) -> &'static str {
        if self.ctx.services.indexer.is_pending(rel) {
            return "  ...";
        }
        if self.st_ref().deco.skipped.contains(rel) {
            return "  [skip]";
        }
        ""
    }

    /// Draw the per-file context menu and return the picked verb (if any).
    /// Pure rendering: every branch maps a clicked button to a `FileVerb`;
    /// no mutation happens here.
    fn file_row_menu(&mut self, resp: &egui::Response, rel: &str) -> Option<FileVerb> {
        let mut verb = None;
        // Gather the menu context LAZILY, inside the closure: egui only runs it
        // when the menu is actually open (right-click), not every frame for every
        // visible row. These reads — every board-doc + its columns + this note's
        // memberships (`picker_context_ctx`), and every `.canvas` doc in the vault
        // (`list_canvases`) — are O(boards)/O(vault) and were previously computed
        // eagerly per row per frame, which dominated the file tree's render time.
        resp.context_menu(|ui| {
            let (boards, membership, board_doc) =
                crate::panels::board::picker_context_ctx(self.ctx, rel);
            let active_trail = self
                .st_ref()
                .deco
                .active_trail
                .as_ref()
                .map(|(name, paths)| (name.clone(), paths.contains(rel)));
            let canvases = crate::panels::canvas::list_canvases(self.ctx.vault);
            verb = file_menu_body(
                ui,
                MenuArgs { rel, active_trail: &active_trail, board_doc },
                &boards,
                &membership,
                &canvases,
            );
        });
        verb
    }

    /// Dispatch a context-menu verb. `Rename` only seeds egui memory and
    /// is handled inline (it needs the live frame's `ui.ctx()`, which a
    /// deferred closure lacks); every other verb is a broad `&mut AppState`
    /// mutation queued via `defer`.
    fn run_file_verb(&mut self, ui: &egui::Ui, verb: FileVerb, rel: &str) {
        if matches!(verb, FileVerb::Rename) {
            start_rename(ui, rel);
            return;
        }
        let rel = rel.to_string();
        self.ctx.defer(move |app| apply_file_verb(app, verb, &rel));
    }

    /// Renders the inline rename TextEdit. On Enter, runs the move via the
    /// indexer-driven op (deferred); on Esc, cancels.
    fn rename_row(&mut self, ui: &mut egui::Ui, entry: &DirEntryDto, depth: usize, draft: String) {
        let path = entry.rel_path.clone();
        if let Some(committed) = rename_text_edit(ui, &path, &entry.kind, depth, draft) {
            self.ctx
                .defer(move |app| commit_rename(app, &path, &committed));
        }
    }

    /// Move a vault-relative path into `dest_dir` (deferred). A file moves
    /// via `move_note`; a folder via `move_folder` — both the full
    /// indexer-driven ops (op-log rename + referrer rewrites + watcher
    /// suppression), not the bare `vault::move_note`.
    fn move_into_folder(&mut self, src: &str, dest_dir: &str) {
        let src = src.to_string();
        let dest_dir = dest_dir.to_string();
        self.ctx
            .defer(move |app| move_into_folder(app, &src, &dest_dir));
    }
}

// ----- deferred-effect free helpers (full &mut AppState) -----

/// Refresh the row-decoration snapshot from the AppState data the narrow
/// `Ctx` doesn't carry: the dirty-buffer set (`session.buffers`), the
/// skipped-paths set (`ui_cache.skipped_paths`), and the active-trail
/// name + membership / all trail names (`trails_state`). Runs as a
/// deferred pre-pass so the render path reads only the snapshot.
fn refresh_deco(app: &mut AppState) {
    let dirty: std::collections::HashSet<String> = app
        .session
        .buffers
        .iter()
        .filter(|(_, b)| b.is_dirty())
        .map(|(p, _)| p.clone())
        .collect();
    let skipped = app.ui_cache.skipped_paths.clone();
    let active_trail = app
        .trails_state
        .active_trail
        .clone()
        .filter(|id| app.trails_state.trails.iter().any(|t| &t.id == id))
        .and_then(|tid| app.trails_state.trails.iter().find(|t| t.id == tid))
        .map(|t| (t.name.clone(), collect_waypoint_paths(&t.waypoints)));
    let trail_names = app
        .trails_state
        .trails
        .iter()
        .map(|t| t.name.to_lowercase())
        .collect();
    let deco = &mut app.file_tree_state.deco;
    deco.dirty = dirty;
    deco.skipped = skipped;
    deco.active_trail = active_trail;
    deco.trail_names = trail_names;
}

/// Dispatch a context-menu verb against `AppState`.
fn apply_file_verb(app: &mut AppState, verb: FileVerb, rel: &str) {
    match verb {
        FileVerb::Open => crate::editor_pane::open_file(app, rel, true),
        // `Rename` is seeded inline in `run_file_verb` (egui memory); it
        // never reaches the deferred dispatch.
        FileVerb::Rename => {}
        FileVerb::Duplicate => duplicate_file(app, rel),
        FileVerb::Reveal => reveal_in_file_manager(app, rel),
        FileVerb::Properties => open_properties(app, rel),
        FileVerb::Reindex => reindex_path(app, rel),
        FileVerb::AddToTrail { trail_name } => add_to_trail(app, rel, &trail_name),
        FileVerb::SetActiveTrail => set_active_trail(app, rel),
        FileVerb::OpenAsBoard => {
            crate::panels::board::open(app, rel);
        }
        FileVerb::OpenAsCanvas => {
            crate::panels::canvas::open(app, rel);
        }
        FileVerb::ViewCanvasAsJson => {
            // Open the canvas tab and flip it to the raw-JSON editor view —
            // the escape hatch for hand-editing a `.canvas` file's text.
            // status: canvas-file-tree-glyph
            crate::panels::canvas::open_as_json(app, rel);
        }
        FileVerb::AddToBoard { board_rel, column } => {
            crate::panels::board::add_card(app, &board_rel, &column, rel);
        }
        FileVerb::AddToCanvas { canvas_rel } => {
            crate::panels::canvas::add_file_node(app, &canvas_rel, rel);
        }
        // status: canvas-export-trail-verb
        FileVerb::ExportTrailToCanvas => export_trail_to_canvas(app, rel),
        FileVerb::MakeSearchable => crate::extract::make_searchable(app, rel),
        FileVerb::OpenExternal => crate::extract::open_external(app, rel),
        FileVerb::Delete => {
            app.session.modal = Some(crate::state::Modal::ConfirmDelete {
                path: rel.to_string(),
            });
        }
    }
}

fn reindex_path(app: &mut AppState, rel: &str) {
    let tx = app.vault_session.services.indexer.job_sender();
    let path_owned = rel.to_string();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = tx
                .send(hiker_core::indexer::IndexJob::Upsert {
                    rel_path: path_owned,
                    force: true,
                })
                .await;
        });
    }
    app.push_toast(format!("Reindexing {rel}"), crate::state::ToastLevel::Info);
}

fn add_to_trail(app: &mut AppState, rel: &str, trail_name: &str) {
    crate::state::trail_append_waypoint(app, rel);
    let _ = crate::bootstrap::save_trails(&app.vault_session.vault_root, &app.trails_state.trails);
    app.push_toast(
        format!("Added {rel} to '{trail_name}'"),
        crate::state::ToastLevel::Info,
    );
}

/// Snapshot the trail-doc at `rel` into a fresh `.canvas` (the core export
/// builder), then open the new file framed-to-fit in the canvas view. On
/// success toasts the new basename; on failure surfaces the core error as an
/// error toast (never panics). status: canvas-export-trail-verb
fn export_trail_to_canvas(app: &mut AppState, rel: &str) {
    let result = {
        let Ok(store) = app.vault_session.services.read_store.lock() else {
            app.push_toast("index store unavailable", crate::state::ToastLevel::Error);
            return;
        };
        hiker_core::canvas::export::write_trail_canvas(
            &app.vault_session.vault,
            &store,
            &app.vault_session.services.oplog,
            rel,
        )
    };
    match result {
        Ok(new_rel) => {
            crate::panels::canvas::open_fresh(app, &new_rel);
            app.push_toast(
                format!("Exported to {}", basename_of(&new_rel)),
                crate::state::ToastLevel::Info,
            );
        }
        Err(e) => app.push_toast(
            format!("Export to canvas failed: {e}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

/// Activate the trail whose name matches the doc basename (trail-doc paths
/// live under `.hiker/trails/<name>.md`).
fn set_active_trail(app: &mut AppState, rel: &str) {
    let stem = rel
        .rsplit('/')
        .next()
        .and_then(|n| n.strip_suffix(".md"))
        .unwrap_or("");
    let hit = app
        .trails_state
        .trails
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(stem))
        .map(|t| (t.id.clone(), t.name.clone()));
    match hit {
        Some((tid, name)) => {
            app.trails_state.active_trail = Some(tid);
            crate::actions::ensure_panel_visible(app, crate::tab::PANEL_TRAILS);
            app.push_toast(
                format!("Activated trail {name}"),
                crate::state::ToastLevel::Info,
            );
        }
        None => app.push_toast(
            "No trail registered for this doc",
            crate::state::ToastLevel::Info,
        ),
    }
}

fn open_properties(app: &mut AppState, rel: &str) {
    use crate::tab::{Tab, TabKind};
    if let Some(existing) = app.session.tabs.iter().find(|t| match &t.kind {
        TabKind::Properties { path } => path == rel,
        _ => false,
    }) {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::Properties {
            path: rel.to_string(),
        },
        sticky: false,
    });
    app.session.active_tab = Some(id);
    app.session.preview_tab = Some(id);
}

fn duplicate_file(app: &mut AppState, rel: &str) {
    // Read the source body, choose a `<stem>-copy-N.<ext>` target in the
    // same dir, then create + seed it via the indexer-driven
    // `core::ops::file::create_at` (watcher suppression + `IndexJob::Upsert`)
    // rather than the bare `vault::create_note` — same discipline as `+`.
    let body = match app.vault_session.vault.read_file(rel) {
        Ok(s) => s,
        Err(err) => {
            app.push_toast(
                format!("Duplicate failed: {err}"),
                crate::state::ToastLevel::Error,
            );
            return;
        }
    };
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let target = pick_copy_target(app, rel, parent);
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let target_owned = target.clone();
    let actual = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &target_owned, &body).await
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    match actual {
        Ok(actual) => {
            app.file_tree_state.dir_cache.remove(parent);
            app.push_toast(
                format!("Duplicated -> {actual}"),
                crate::state::ToastLevel::Info,
            );
        }
        Err(err) => app.push_toast(
            format!("Duplicate failed: {err}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

/// Pick the first free `<stem>-copy-N.<ext>` name in `parent`.
fn pick_copy_target(app: &AppState, rel: &str, parent: &str) -> String {
    let base = basename_of(rel);
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (base.to_string(), String::new()),
    };
    let listed = app
        .vault_session
        .vault
        .list_dir(parent, default_sort(&app.vault_session.config))
        .unwrap_or_default();
    let existing: std::collections::HashSet<&str> =
        listed.iter().map(|e| e.name.as_str()).collect();
    let mut chosen = String::new();
    for n in 1.. {
        let candidate = format!("{stem}-copy-{n}{ext}");
        if !existing.contains(candidate.as_str()) {
            chosen = candidate;
            break;
        }
    }
    if parent.is_empty() {
        chosen
    } else {
        format!("{parent}/{chosen}")
    }
}

fn reveal_in_file_manager(app: &mut AppState, rel: &str) {
    let abs = match app.vault_session.vault.abs_path(rel) {
        Ok(p) => p,
        Err(err) => {
            app.push_toast(
                format!("Reveal failed: {err}"),
                crate::state::ToastLevel::Error,
            );
            return;
        }
    };
    // Best-effort cross-platform launch.
    #[cfg(target_os = "macos")]
    let res = std::process::Command::new("open")
        .arg("-R")
        .arg(&abs)
        .spawn();
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
        app.push_toast(
            format!("Reveal failed: {err}"),
            crate::state::ToastLevel::Error,
        );
    }
}

fn move_into_folder(app: &mut AppState, src: &str, dest_dir: &str) {
    let basename = basename_of(src);
    let dest = if dest_dir.is_empty() {
        basename.to_string()
    } else {
        format!("{dest_dir}/{basename}")
    };
    if dest == src {
        return;
    }
    // A dragged folder moves via `move_folder`; a file via `move_note`.
    // The drag payload is just a path — resolve the kind from disk so a
    // re-parented subtree keeps its members indexed.
    let is_dir = app
        .vault_session
        .vault
        .abs_path(src)
        .map(|p| p.is_dir())
        .unwrap_or(false);
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let src_owned = src.to_string();
    let dest_owned = dest.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            if is_dir {
                hiker_core::ops::file::move_folder(&watcher, &jobs, &vault, &src_owned, &dest_owned)
                    .await
            } else {
                hiker_core::ops::file::move_note(&watcher, &jobs, &src_owned, &dest_owned).await
            }
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    if let Err(err) = result {
        app.push_toast(
            format!("Move failed: {err}"),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    let src_parent = src.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    app.file_tree_state.dir_cache.remove(src_parent);
    app.file_tree_state.dir_cache.remove(dest_dir);
    repoint_open_buffer(app, src, &dest);
    app.push_toast(format!("Moved -> {dest}"), crate::state::ToastLevel::Info);
}

fn commit_rename(app: &mut AppState, from: &str, draft: &str) {
    let draft = draft.trim();
    if draft.is_empty() || draft == basename_of(from) {
        return;
    }
    let parent = from.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let to = if parent.is_empty() {
        draft.to_string()
    } else {
        format!("{parent}/{draft}")
    };
    // Route through the indexer-driven `move_note` (op-log rename +
    // referrer rewrites + watcher suppression), not the bare
    // `vault::move_note`.
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let from_owned = from.to_string();
    let to_owned = to.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .block_on(async { hiker_core::ops::file::move_note(&watcher, &jobs, &from_owned, &to_owned).await }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    if let Err(err) = result {
        app.push_toast(
            format!("Rename failed: {err}"),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    app.file_tree_state.dir_cache.remove(parent);
    repoint_open_buffer(app, from, &to);
    app.push_toast(format!("Renamed -> {to}"), crate::state::ToastLevel::Info);
}

/// Move any loaded buffer + open editor tabs from `from` to `to` after a
/// move/rename so the open view keeps tracking the file.
fn repoint_open_buffer(app: &mut AppState, from: &str, to: &str) {
    if let Some(buf) = app.session.buffers.remove(from) {
        let mut moved = buf;
        moved.path = to.to_string();
        app.session.buffers.insert(to.to_string(), moved);
    }
    for tab in &mut app.session.tabs {
        if let crate::tab::TabKind::Editor {
            buffer: crate::tab::BufferSource::Vault { path },
            ..
        } = &mut tab.kind
            && path == from
        {
            *path = to.to_string();
        }
    }
}

// ----- free helpers (pure / UI) -----

/// The precomputed per-row context the menu renders against (everything the
/// pure-rendering `file_menu_body` can't reach through the narrow `Ctx`).
#[derive(Clone, Copy)]
struct MenuArgs<'a> {
    rel: &'a str,
    active_trail: &'a Option<(String, bool)>,
    board_doc: bool,
}

/// Build the body of the per-file context menu, returning the picked verb.
/// Pure rendering — no `AppState` access; the `active_trail` / `boards` /
/// `membership` / `canvases` reads are passed in precomputed.
fn file_menu_body(
    ui: &mut egui::Ui,
    args: MenuArgs<'_>,
    boards: &[crate::panels::board::PickerEntry],
    membership: &std::collections::HashSet<String>,
    canvases: &[(String, String)],
) -> Option<FileVerb> {
    let MenuArgs { rel, active_trail, board_doc } = args;
    let mut verb = None;
    for (label, made) in [
        ("Open", FileVerb::Open),
        ("Rename", FileVerb::Rename),
        ("Duplicate", FileVerb::Duplicate),
        ("Reveal in file manager", FileVerb::Reveal),
        ("Properties", FileVerb::Properties),
        ("Reindex this file", FileVerb::Reindex),
    ] {
        if ui.button(label).clicked() {
            verb = Some(made);
            ui.close();
        }
    }
    // Non-markdown sources get the extraction affordances: "Make searchable"
    // (extract a sidecar + index it) and "Open original externally" (hand the
    // source to the OS handler — there is no in-app renderer). Indexable
    // rows (`.md` / `.txt`) ride the ordinary ingest path and don't need
    // them. See docs/extract.md.
    if !hiker_core::indexer::is_indexable_path(rel) {
        if ui.button("Make searchable").clicked() {
            verb = Some(FileVerb::MakeSearchable);
            ui.close();
        }
        if ui.button("Open original externally").clicked() {
            verb = Some(FileVerb::OpenExternal);
            ui.close();
        }
    }
    // Add-to-trail: only when a trail is active; disabled when `rel` is
    // already a waypoint at any depth.
    if let Some((trail_name, already)) = active_trail {
        let label = if *already {
            format!("Already in '{trail_name}'")
        } else {
            format!("Add to trail '{trail_name}'")
        };
        if ui
            .add_enabled(!already, egui::Button::new(label))
            .clicked()
        {
            verb = Some(FileVerb::AddToTrail {
                trail_name: trail_name.clone(),
            });
            ui.close();
        }
    }
    // "Set as active trail" + "Export to canvas" — only on a `.hiker/trails/*.md`
    // row (the trail-doc detection). status: canvas-export-trail-verb
    if rel.starts_with(".hiker/trails/") && rel.ends_with(".md") {
        if ui.button("Set as active trail").clicked() {
            verb = Some(FileVerb::SetActiveTrail);
            ui.close();
        }
        if ui.button("Export to canvas").clicked() {
            verb = Some(FileVerb::ExportTrailToCanvas);
            ui.close();
        }
    }
    // Board-docs get an explicit "Open as board" verb (the default click
    // already routes there).
    if board_doc && ui.button("Open as board").clicked() {
        verb = Some(FileVerb::OpenAsBoard);
        ui.close();
    }
    // `.canvas` files: an explicit "Open as canvas" (the default click route)
    // and a "View as JSON" escape hatch that opens the raw text in the editor.
    // status: canvas-file-tree-glyph
    if is_canvas_doc(rel) {
        if ui.button("Open as canvas").clicked() {
            verb = Some(FileVerb::OpenAsCanvas);
            ui.close();
        }
        if ui.button("View as JSON").clicked() {
            verb = Some(FileVerb::ViewCanvasAsJson);
            ui.close();
        }
    }
    // "Add to board…" on indexable note rows: a board → column nested
    // picker. Hidden on board-doc rows and non-`.md` rows; disabled
    // per-board when the note is already a card.
    if !board_doc && rel.ends_with(".md") && !boards.is_empty() {
        ui.menu_button("Add to board…", |ui| {
            let mut pick: Option<(String, String)> = None;
            crate::panels::board::column_picker(ui, boards, membership, &mut pick);
            if let Some((board_rel, column)) = pick {
                verb = Some(FileVerb::AddToBoard { board_rel, column });
            }
        });
    }
    // "Add to canvas…" on non-`.canvas` rows: a nested picker listing every
    // `.canvas` doc in the vault. Selecting one inserts this row's vault path
    // as a file-node pointer (whether or not that canvas is open). A canvas can
    // hold the same note twice, so there's no already-present disabling.
    // status: canvas-add-to-canvas-verb
    if !is_canvas_doc(rel) && !canvases.is_empty() {
        ui.menu_button("Add to canvas…", |ui| {
            for (canvas_rel, title) in canvases {
                if ui.button(title).clicked() {
                    verb = Some(FileVerb::AddToCanvas { canvas_rel: canvas_rel.clone() });
                    ui.close();
                }
            }
        });
    }
    if ui.button("Delete").clicked() {
        verb = Some(FileVerb::Delete);
        ui.close();
    }
    verb
}

/// Recursively collect every waypoint path in a forest into a flat set
/// (used for the "already in trail" membership decoration).
fn collect_waypoint_paths(waypoints: &[crate::state::Waypoint]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(ws: &[crate::state::Waypoint], out: &mut std::collections::HashSet<String>) {
        for w in ws {
            out.insert(w.path.clone());
            walk(&w.children, out);
        }
    }
    walk(waypoints, &mut out);
    out
}

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

fn default_sort(
    config: &std::sync::RwLock<hiker_core::config::Config>,
) -> hiker_core::config::sections::TreeSortBy {
    config
        .read()
        .ok()
        .map(|c| c.vault.tree.sort_by)
        .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc)
}

/// True if the `.md` at `rel` is a board-doc (frontmatter `hiker.kind:
/// board`). Reads + parses the file — called on click / menu open, never
/// per-frame.
fn is_board_doc(app: &AppState, rel: &str) -> bool {
    if !rel.ends_with(".md") {
        return false;
    }
    app.vault_session
        .vault
        .read_file(rel)
        .ok()
        .map(|src| hiker_core::boards::parse_board_for(rel, &src).is_ok())
        .unwrap_or(false)
}

/// A leading glyph marker distinguishing a `.canvas` row in the file tree.
/// The row renderer paints a left-aligned galley with no per-file icon slot
/// (only folders get a chevron), so a label prefix is the in-pattern way to
/// flag the row — the same label-decoration approach as the dirty / index-state
/// markers. status: canvas-file-tree-glyph
fn canvas_glyph_marker(rel: &str) -> &'static str {
    if is_canvas_doc(rel) {
        "⬚ "
    } else {
        ""
    }
}

/// True if `rel` is a JSON Canvas document, recognized purely by its
/// `.canvas` extension (no file read needed — cheap enough to call per-row).
/// status: canvas-file-tree-glyph
fn is_canvas_doc(rel: &str) -> bool {
    rel.ends_with(".canvas")
}

/// True if the `.md` at `rel` is a capture-spec note (frontmatter
/// `hiker.kind: capture` — a crawl job or RSS feed). Reads + parses the
/// file — called on click / menu open, never per-frame. Routes the row to
/// the capture form (`crawl-job-form`) rather than the raw markdown editor.
fn is_capture_doc(app: &AppState, rel: &str) -> bool {
    if !rel.ends_with(".md") {
        return false;
    }
    app.vault_session
        .vault
        .read_file(rel)
        .ok()
        .and_then(|src| {
            let split = hiker_core::frontmatter::split(&src);
            let fm = split.frontmatter.as_ref()?;
            Some(hiker_extract::capture::Spec::from_frontmatter(fm).is_ok())
        })
        .unwrap_or(false)
}

// ----- inline-rename draft storage in egui memory -----

#[derive(Clone, Default)]
struct RenameMem {
    path: String,
    draft: String,
    just_opened: bool,
}

fn mem_id() -> egui::Id {
    egui::Id::new("sidebar-files-rename")
}

/// Active inline-rename draft for `path`, if any.
fn rename_draft_for(ui: &egui::Ui, path: &str) -> Option<String> {
    ui.ctx().memory(|m| {
        m.data
            .get_temp::<RenameMem>(mem_id())
            .filter(|r| r.path == path)
            .map(|r| r.draft.clone())
    })
}

/// Enter inline-rename mode (egui-memory side): seed the draft + flag the
/// row to grab focus next frame.
fn start_rename(ui: &egui::Ui, path: &str) {
    let draft = basename_of(path).to_string();
    ui.ctx().memory_mut(|m| {
        m.data.insert_temp(
            mem_id(),
            RenameMem {
                path: path.to_string(),
                draft,
                just_opened: true,
            },
        );
    });
}

/// Draw the inline-rename TextEdit row, returning the committed draft on
/// Enter (otherwise `None`). Manages focus + egui-memory draft lifecycle.
fn rename_text_edit(
    ui: &mut egui::Ui,
    path: &str,
    kind: &EntryKind,
    depth: usize,
    mut draft: String,
) -> Option<String> {
    let outcome = ui.horizontal(|ui| {
        ui.add_space((depth as f32) * 12.0);
        ui.add(match kind {
            EntryKind::Dir => crate::icons::ICONS.image(crate::icons::Icon::Folder),
            EntryKind::File => crate::icons::ICONS.image(crate::icons::Icon::File),
        });
        let id = egui::Id::new(("rename-edit", path));
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
                m.data.insert_temp(
                    mem_id(),
                    RenameMem {
                        path: path.to_string(),
                        draft: draft.clone(),
                        just_opened: false,
                    },
                );
            });
        }
        let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if commit || resp.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            // Drop the draft if it still belongs to this row.
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
        commit.then(|| draft.clone())
    });
    outcome.inner
}

/// Renders a sidebar row button. Optionally draws an SVG chevron in the
/// leading slot. `Some(true)` = expanded (down chevron), `Some(false)` =
/// collapsed (right chevron), `None` = no chevron (leaf row).
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
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(total_width, row_height), egui::Sense::click());
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
    // Chevron icon (folders only), painted into a fixed leading slot so the
    // text origin lines up whether or not a chevron is present.
    let chev_size = 14.0;
    let chev_slot = 16.0;
    let text_x_start = rect.min.x + indent + 2.0;
    if let Some(expanded) = chevron {
        let chev_rect = egui::Rect::from_min_size(
            egui::pos2(text_x_start, rect.min.y + (row_height - chev_size) * 0.5),
            egui::vec2(chev_size, chev_size),
        );
        let icon = if expanded {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
        } else {
            crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
        };
        icon.paint_at(ui, chev_rect);
    }
    // Left-aligned hand-painted galley (egui's centered Button layout
    // produced floating-text in this row).
    let font_id = egui::FontId::proportional(13.0);
    let color = ui.style().visuals.text_color();
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, color));
    let text_pos = egui::pos2(
        text_x_start + if chevron.is_some() { chev_slot } else { 0.0 },
        rect.min.y + (row_height - galley.size().y) * 0.5,
    );
    // Clip to the row rect so long names don't paint over neighbours.
    ui.painter_at(rect).galley(text_pos, galley, color);
    resp
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

    #[test]
    fn waypoint_paths_collected_recursively() {
        use crate::state::Waypoint;
        let wp = |path: &str, children: Vec<Waypoint>| Waypoint {
            path: path.to_string(),
            at_ms: 0,
            children,
            annotation: String::new(),
        };
        let forest = vec![wp("a.md", vec![wp("b.md", vec![wp("c.md", vec![])])])];
        let set = collect_waypoint_paths(&forest);
        assert!(set.contains("a.md") && set.contains("b.md") && set.contains("c.md"));
    }
}
