//! Files-mode body: lazy-loaded file tree. Click a file to open it as a
//! preview-slot buffer tab (or the board view for a board-doc);
//! double-click to inline-rename; right-click for the per-row verb menu
//! (open / rename / duplicate / reveal / properties / reindex /
//! add-to-trail / set-active-trail / add-to-board / delete). Folder rows
//! expand/collapse and accept drag-dropped paths to re-parent subtrees.
//!
//! Migrated onto the Files `Activity`'s `View`: rendering goes
//! through the narrow `activity::SurfaceCtx` instead of `&mut AppState`. The
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

use crate::activity::SurfaceCtx;
use crate::item_menu;
use crate::state::{AppState, FileTreeState};
use super::rename::{
    commit_observed_rename, commit_rename, rename_draft_for, rename_text_edit,
    repoint_open_buffer, start_rename,
};
use hiker_theme as theme;

/// A context-menu verb picked on a file row. The menu render records one
/// of these; the mutation runs afterwards as a deferred effect so the
/// `&mut AppState` helpers don't fight the menu closure's `ui` borrow nor
/// the narrow `SurfaceCtx` borrow.
enum FileVerb {
    /// A shared note-item base action (Open / Reveal-in-tree / Properties),
    /// composed from [`item_menu::note_item_base`] so the universal verbs live
    /// in one place (status: ctxmenu-item-base).
    Base(item_menu::ItemAction),
    Rename,
    Duplicate,
    Reveal,
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
    /// Open a non-md source in the OS default handler
    /// (`extract-open-original-external`).
    OpenExternal,
    Delete,
}

/// Fixed height of a single tree row, in points. Used both to paint rows
/// ([`row_button_with_chevron`]) and to drive `ScrollArea::show_rows`
/// virtualization — the two must agree or the viewport math drifts. The
/// global item-spacing (`hiker_theme`) is added between rows by egui.
const ROW_HEIGHT: f32 = 22.0;

/// One visible row in the flattened, virtualized tree. The tree is walked
/// into a `Vec<FlatRow>` in render order whenever a structural change
/// invalidates the cache (see `FileTreeState::flat_cache`); `show_rows` then
/// lays out / paints only the rows inside the scroll viewport. Decorations,
/// child counts, and the active-row highlight are NOT stored here — they're
/// computed live per-render, so the cache only needs invalidating on
/// structural edits.
pub(crate) enum FlatRow {
    /// A file or folder row at the given indent depth.
    Entry { entry: DirEntryDto, depth: usize },
    /// A directory whose listing failed — renders the error inline, as the
    /// recursive walk did before.
    Error {
        rel: String,
        err: String,
        depth: usize,
    },
}

impl FlatRow {
    /// Vault-relative path of an entry row (folders + files); `None` for
    /// error rows. Used to match the one-shot reveal scroll target.
    fn rel_path(&self) -> Option<&str> {
        match self {
            FlatRow::Entry { entry, .. } => Some(&entry.rel_path),
            FlatRow::Error { .. } => None,
        }
    }
}

/// Shared per-frame context for the files sidebar. Wraps the narrow
/// feature `SurfaceCtx` so the render/mutation helpers can be `&mut self`
/// methods on one receiver. `ui` is threaded as a method arg rather than
/// held here so the deferred closures don't contend with the `ui` borrow.
pub(crate) struct FilesCtx<'a, 'c> {
    pub(crate) ctx: &'a mut SurfaceCtx<'c>,
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
    /// frame, draw the (fixed) sort header, then the virtualized tree.
    pub(crate) fn render(&mut self, ui: &mut egui::Ui) {
        // Pre-pass: snapshot the AppState-only row decorations (dirty
        // buffers, skipped paths, active-trail membership) into
        // `file_tree_state.deco` for next frame. Deferred so it runs with
        // full `&mut AppState`; the render below reads only the snapshot,
        // keeping the render path within the narrow `SurfaceCtx`.
        self.ctx.defer(refresh_deco);
        self.sort_header(ui);
        // A transient "move to vault root" drop target, shown only while a row is
        // being dragged (dropping onto a folder moves INTO it). [filetree-drag-root]
        self.root_drop_strip(ui);
        let _g = crate::profiling::FrameProf::guard("files:tree");

        // Reuse the cached flattened row list, rebuilding it only when a
        // structural change invalidated it (an expand / collapse toggle, or a
        // directory-listing change routed through `FileTreeState::invalidate_*`).
        // Take it out of state for the frame so the per-row render below can
        // hold `&mut self` without borrowing the cache; the epilogue restores
        // it. This replaces the previous per-frame re-walk, which cloned every
        // expanded directory's full listing on every frame. `show_rows` then
        // lays out / paints only the ~viewport-height band of rows.
        let rows = match self.st().flat_cache.take() {
            Some(rows) => rows,
            None => self.flatten_visible(),
        };

        // Capture the visible render order (entry rows) so a Shift-click can
        // resolve its contiguous selection range against it. Cheap — one clone
        // of the rel-paths per frame. [filetree-multiselect]
        let order: Vec<String> = rows
            .iter()
            .filter_map(|r| r.rel_path().map(str::to_string))
            .collect();
        self.st().flat_order = order;

        // Reveal-from-discovery (`reveal-in-sidebar-scroll`): a one-shot
        // scroll target arms a jump to a specific row. With virtualization
        // that row may sit outside the rendered band, so `scroll_to_me`
        // can't reach it — instead translate the target into an explicit
        // scroll offset that centres the row, then consume the one-shot.
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("panel-files-body")
            .auto_shrink([false, false]);
        if let Some(target) = self.st_ref().scroll_target.clone() {
            if let Some(idx) = rows.iter().position(|r| r.rel_path() == Some(target.as_str())) {
                let stride = ROW_HEIGHT + ui.spacing().item_spacing.y;
                let centred = (idx as f32 * stride) - (ui.available_height() - stride) * 0.5;
                scroll = scroll.vertical_scroll_offset(centred.max(0.0));
            }
            self.st().scroll_target = None;
        }

        scroll.show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
            for i in range {
                self.render_flat_row(ui, &rows[i]);
            }
        });

        // Epilogue: restore the cache for reuse next frame — unless an
        // expand / collapse toggled the structure mid-render (`flat_dirty`),
        // in which case leave `flat_cache` empty so the next render rebuilds.
        if !std::mem::take(&mut self.st().flat_dirty) {
            self.st().flat_cache = Some(rows);
        }
    }

    /// Walk the expanded tree from the vault root into a flat, render-order
    /// list of visible rows. Lists each expanded directory on demand (via
    /// [`Self::ensure_listed`]) so the cache fills as folders open.
    fn flatten_visible(&mut self) -> Vec<FlatRow> {
        let mut rows = Vec::new();
        self.flatten_dir(&mut rows, "", 0);
        rows
    }

    fn flatten_dir(&mut self, rows: &mut Vec<FlatRow>, rel: &str, depth: usize) {
        if let Some(err) = self.ensure_listed(rel) {
            rows.push(FlatRow::Error {
                rel: rel.to_string(),
                err,
                depth,
            });
            return;
        }
        // Clone the listing out so the recursive walk can keep reading /
        // mutating `dir_cache` (e.g. listing a child) without overlapping
        // borrows — mirrors the old `show_dir` clone.
        let entries = self
            .st_ref()
            .dir_cache
            .get(rel)
            .cloned()
            .unwrap_or_default();
        for entry in entries {
            let expanded = matches!(entry.kind, EntryKind::Dir)
                && self.st_ref().expanded.contains(&entry.rel_path);
            let child_rel = expanded.then(|| entry.rel_path.clone());
            rows.push(FlatRow::Entry { entry, depth });
            if let Some(child_rel) = child_rel {
                self.flatten_dir(rows, &child_rel, depth + 1);
            }
        }
    }

    /// Render one flattened row at its captured depth.
    fn render_flat_row(&mut self, ui: &mut egui::Ui, row: &FlatRow) {
        match row {
            FlatRow::Error { rel, err, .. } => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("failed to list {rel}: {err}"),
                );
            }
            FlatRow::Entry { entry, depth } => match entry.kind {
                EntryKind::Dir => self.render_dir_row(ui, entry, *depth),
                EntryKind::File => self.render_file_row(ui, entry, *depth),
            },
        }
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
                app.file_tree_state.invalidate_all();
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

        let is_selected = self.st_ref().selection.contains(&entry.rel_path);
        let resp = row_button_with_chevron(ui, &label, depth, is_selected, Some(expanded));

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
            // A folder click is a navigation action — clear the file selection.
            st.selection.clear();
            if expanded {
                st.expanded.remove(&rel);
            } else {
                st.expanded.insert(rel.clone());
            }
            st.selected_folder = Some(rel);
            // Structural change: drop the flattened-row cache so the next
            // render rebuilds it with this folder expanded / collapsed.
            st.invalidate_flat();
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
        let is_selected = self.st_ref().selection.contains(&entry.rel_path);
        let label = format!(
            "{}{}{}{}",
            canvas_glyph_marker(&entry.rel_path),
            entry.name,
            self.dirty_marker(&entry.rel_path),
            self.index_state_marker(&entry.rel_path),
        );
        let resp = row_button_with_chevron(ui, &label, depth, is_active || is_selected, None);

        // Shared note hover-preview card, like every sibling note list
        // (interaction.md [hover-preview-universal]). Note rows only —
        // folders and non-note files have nothing to preview.
        if resp.hovered() && hiker_core::indexer::is_indexable_path(&entry.rel_path) {
            crate::widgets::preview::register_note_hover(ui, resp.rect, &entry.rel_path);
        }

        // Drag payload: vault-relative source path.
        resp.clone()
            .dnd_set_drag_payload::<String>(entry.rel_path.clone());

        let rel = entry.rel_path.clone();
        if resp.clicked() {
            // Cmd/Ctrl = toggle, Shift = range, plain = single-select + open.
            let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
            self.handle_select_click(&rel, shift, ctrl);
            if !shift && !ctrl {
                self.open_row(&rel);
            }
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

    /// Update the multi-selection for a click on `rel` with the Shift / Cmd-Ctrl
    /// modifiers. Shift selects the contiguous range from `selection_anchor` to
    /// `rel` over the captured `flat_order`; Cmd/Ctrl toggles a single row; a
    /// plain click selects just `rel` (and the caller opens it). [filetree-multiselect]
    fn handle_select_click(&mut self, rel: &str, shift: bool, ctrl: bool) {
        if shift {
            let order = self.st_ref().flat_order.clone();
            let anchor = self.st_ref().selection_anchor.clone();
            let range = anchor
                .as_deref()
                .and_then(|a| order.iter().position(|p| p == a))
                .zip(order.iter().position(|p| p == rel))
                .map(|(ai, ti)| {
                    let (lo, hi) = (ai.min(ti), ai.max(ti));
                    order[lo..=hi]
                        .iter()
                        .cloned()
                        .collect::<std::collections::HashSet<String>>()
                });
            let st = self.st();
            match range {
                // Shift-range replaces the selection; the anchor stays put so a
                // further Shift-click re-pivots from the same origin.
                Some(range) => st.selection = range,
                // No usable anchor → behave like a plain single select.
                None => {
                    st.selection.clear();
                    st.selection.insert(rel.to_string());
                    st.selection_anchor = Some(rel.to_string());
                }
            }
        } else if ctrl {
            let st = self.st();
            if !st.selection.remove(rel) {
                st.selection.insert(rel.to_string());
            }
            st.selection_anchor = Some(rel.to_string());
        } else {
            let st = self.st();
            st.selection.clear();
            st.selection.insert(rel.to_string());
            st.selection_anchor = Some(rel.to_string());
        }
    }

    /// While a row is being dragged, paint a "move to vault root" drop target at
    /// the top of the tree. Dropping a row onto a folder moves it INTO that
    /// folder, so this strip is the only way to send an item back to the vault
    /// root. Hidden when nothing is being dragged. [filetree-drag-root]
    fn root_drop_strip(&mut self, ui: &mut egui::Ui) {
        if !egui::DragAndDrop::has_payload_of_type::<String>(ui.ctx()) {
            return;
        }
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::hover(),
        );
        // `contains_pointer` (not `hovered`, which is false mid-drag) lights the
        // strip when the dragged row is over it.
        let hot = resp.contains_pointer();
        let bg = if hot {
            theme::active_bg()
        } else {
            theme::hover_bg().gamma_multiply(0.5)
        };
        ui.painter().rect_filled(rect, 3.0, bg);
        ui.painter().rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(1.0, theme::divider()),
            egui::StrokeKind::Inside,
        );
        let color = ui.style().visuals.weak_text_color();
        let galley = ui.fonts(|f| {
            f.layout_no_wrap(
                "\u{21A5}  Move to vault root".to_string(),
                egui::FontId::proportional(12.0),
                color,
            )
        });
        let pos = egui::pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter_at(rect).galley(pos, galley, color);
        if let Some(src) = resp.dnd_release_payload::<String>() {
            self.move_into_folder(&src, "");
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
            } else if rel.ends_with(".scip") {
                // A SCIP index opens directly as a code-entity graph — no project note needed.
                // status: code-graph-view-source
                crate::panels::code_graph::open(app, crate::tab::CodeSource::Index(rel));
            } else if crate::panels::code_graph::is_project_doc(app, &rel) {
                // A `hiker.kind: project` note opens its repo source as a code-entity graph.
                // status: code-graph-view-source
                crate::panels::code_graph::open(app, crate::tab::CodeSource::Project(rel));
            } else if is_code_file(&rel) {
                // A code file (`.rs`, `.py`, …) opens as strictly read-only
                // reference content — never editable, never written/committed.
                // Comes AFTER `.scip`/`.canvas`/`.zim`/board/project-doc so
                // those richer views win. status: code-read-only-view
                crate::editor_pane::open_code_file(app, &rel);
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
                crate::panels::board_picker::picker_context_ctx(self.ctx, rel);
            let active_trail = active_trail_membership(self.ctx, rel);
            let canvases = crate::panels::canvas::list_canvases(self.ctx.vault);
            if let Some(v) = egui_workbench::menu::show(
                ui,
                build_file_menu(
                    MenuArgs { rel, active_trail, board_doc },
                    boards,
                    membership,
                    canvases,
                ),
            ) {
                verb = Some(v);
            }
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

    /// Renders the inline rename TextEdit. On commit (Enter or focus loss),
    /// runs the move via the indexer-driven op (deferred); Esc cancels.
    fn rename_row(&mut self, ui: &mut egui::Ui, entry: &DirEntryDto, depth: usize, draft: String) {
        let path = entry.rel_path.clone();
        if let Some(committed) = rename_text_edit(ui, &path, &entry.kind, depth, draft) {
            self.ctx
                .defer(move |app| commit_rename(app, &path, &committed));
        }
    }

    /// Move a vault-relative path into `dest_dir` (deferred). A file moves
    /// via `move_note`; a folder via `move_folder` — both the full
    /// indexer-driven ops (layered-doc rename + referrer rewrites + watcher
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
/// `SurfaceCtx` doesn't carry: the dirty-buffer set (`session.buffers`) and
/// the skipped-paths set (`ui_cache.skipped_paths`). Both are cheap O(open
/// buffers)/O(skipped) reads, so this stays a per-frame deferred pre-pass.
/// Active-trail membership is deliberately NOT snapshotted here — it's an
/// O(vault) read + parse, so it's gathered lazily on context-menu open
/// instead (see [`active_trail_membership`]).
fn refresh_deco(app: &mut AppState) {
    let dirty: std::collections::HashSet<String> = app
        .session
        .buffers
        .iter()
        .filter(|(_, b)| b.is_dirty())
        .map(|(p, _)| p.clone())
        .collect();
    let skipped = app.ui_cache.skipped_paths.clone();
    let deco = &mut app.file_tree_state.deco;
    deco.dirty = dirty;
    deco.skipped = skipped;
}

/// Dispatch a context-menu verb against `AppState`.
fn apply_file_verb(app: &mut AppState, verb: FileVerb, rel: &str) {
    match verb {
        FileVerb::Base(action) => item_menu::apply_item_action(app, action, rel),
        // `Rename` is seeded inline in `run_file_verb` (egui memory); it
        // never reaches the deferred dispatch.
        FileVerb::Rename => {}
        FileVerb::Duplicate => duplicate_file(app, rel),
        FileVerb::Reveal => reveal_in_file_manager(app, rel),
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
            crate::panels::board_picker::add_card(app, &board_rel, &column, rel);
        }
        FileVerb::AddToCanvas { canvas_rel } => {
            crate::panels::canvas::add_file_node(app, &canvas_rel, rel);
        }
        // status: canvas-export-trail-verb
        FileVerb::ExportTrailToCanvas => export_trail_to_canvas(app, rel),
        FileVerb::OpenExternal => crate::os_open::open_external(app, rel),
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

/// Append `rel` as a waypoint of the active trail (parent `None` ⇒ append
/// cursor) via the core verb on the frame's tokio runtime. No-op (with a
/// toast) when no trail is active. status: trail-add-to-active-from-tree-verb
fn add_to_trail(app: &mut AppState, rel: &str, trail_name: &str) {
    let Some(trail_rel) = app
        .vault_session
        .config
        .read()
        .ok()
        .and_then(|c| c.vault.active_trail.clone())
    else {
        app.push_toast("No active trail", crate::state::ToastLevel::Info);
        return;
    };
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let log = app.vault_session.services.layered.clone();
    let vault = app.vault_session.vault.clone();
    let (trail_rel_owned, rel_owned) = (trail_rel.clone(), rel.to_string());
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::trails::ops::append_waypoint(hiker_core::trails::ops::AppendWaypointArgs {
                watcher: &watcher,
                jobs: &jobs,
                log: &log,
                vault: &vault,
                trail_doc_rel: &trail_rel_owned,
                source_rel: &rel_owned,
                parent_waypoint_path: None,
                annotation: None,
            })
            .await
            .map(|_| ())
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    match result {
        Ok(()) => app.push_toast(
            format!("Added {rel} to '{trail_name}'"),
            crate::state::ToastLevel::Info,
        ),
        Err(e) => app.push_toast(
            format!("Add to trail failed: {e}"),
            crate::state::ToastLevel::Error,
        ),
    }
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
            &app.vault_session.services.layered,
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

/// Activate the trail-doc at `rel`: set `vault.active_trail` config and
/// stamp the trail-doc's `hiker.last_activated_at`. The verb only appears
/// on trail-doc rows, so `rel` is the trail-doc path itself.
/// status: trail-set-as-active-context-verb
fn set_active_trail(app: &mut AppState, rel: &str) {
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let rel_owned = rel.to_string();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let stamp = handle.block_on(async {
            hiker_core::trails::ops::stamp_last_activated_at(&watcher, &jobs, &vault, &rel_owned)
                .await
        });
        if let Err(e) = stamp {
            tracing::warn!(error = %e, trail = %rel, "stamp last_activated_at failed");
        }
    }
    app.set_setting(
        hiker_core::config::SettingsScope::Vault,
        "vault.active_trail",
        &serde_json::Value::String(rel.to_string()),
        "Activate trail failed",
    );
    crate::actions::ensure_panel_visible(app, crate::tab::PANEL_TRAILS);
    app.push_toast(
        format!("Activated trail {}", trail_title(rel)),
        crate::state::ToastLevel::Info,
    );
}

pub(crate) fn open_properties(app: &mut AppState, rel: &str) {
    use crate::tab::{Tab, TabKind};
    if let Some(existing) = app.session.tabs.iter().find(|t| match &t.kind {
        TabKind::Properties { path } => path == rel,
        _ => false,
    }) {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(
        id,
        TabKind::Properties { path: rel.to_string() },
        false,
    ));
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
            app.file_tree_state.invalidate_dir(parent);
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
    app.file_tree_state.invalidate_dir(src_parent);
    app.file_tree_state.invalidate_dir(dest_dir);
    let repoint = repoint_open_buffer(app, src, &dest);
    if !is_dir {
        commit_observed_rename(app, src, &dest);
    }
    match repoint {
        Ok(()) => app.push_toast(format!("Moved -> {dest}"), crate::state::ToastLevel::Info),
        Err(reason) => app.push_toast(
            format!("Moved -> {dest}, but {reason}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

// ----- free helpers (pure / UI) -----

/// The precomputed per-row context the menu renders against (everything the
/// pure-data `build_file_menu` can't reach through the narrow `SurfaceCtx`).
struct MenuArgs<'a> {
    rel: &'a str,
    active_trail: Option<(String, bool)>,
    board_doc: bool,
}

/// Build the per-file context menu as a `egui_workbench::menu::Menu<FileVerb>`
/// (status: ctxmenu-files). Pure data construction — no `AppState` access; the
/// `active_trail` / `boards` / `membership` / `canvases` reads are gathered on
/// menu-open and passed in (status: ctxmenu-build-on-open). The two pickers
/// ("Add to board…", "Add to canvas…") render their own live nested
/// `menu_button` widgets, so they ride `Custom` entries (status:
/// ctxmenu-target-builder).
fn build_file_menu(
    args: MenuArgs<'_>,
    boards: Vec<crate::panels::board_picker::PickerEntry>,
    membership: std::collections::HashSet<String>,
    canvases: Vec<(String, String)>,
) -> egui_workbench::menu::Menu<FileVerb> {
    let MenuArgs { rel, active_trail, board_doc } = args;
    // The universal Open / Copy-path / Properties verbs come from the shared
    // base (status: ctxmenu-item-base); `reveal: false` because this list *is*
    // the file tree, so "Reveal in file tree" would be redundant. The
    // file-specific verbs follow in their own section. Note the base's Open and
    // the file tree's own "Reveal in file manager" (the OS finder) are distinct.
    let mut menu = item_menu::note_item_base(rel, item_menu::BaseOpts { reveal: false }, FileVerb::Base)
        .section()
        .action("Rename", FileVerb::Rename)
        .action("Duplicate", FileVerb::Duplicate)
        .action("Reveal in file manager", FileVerb::Reveal)
        .action("Reindex this file", FileVerb::Reindex);
    // Non-markdown sources the app has no in-app renderer for get the
    // "Open original externally" affordance (hand the source to the OS
    // handler). Indexable rows (`.md` / `.txt`) ride the ordinary ingest
    // path and don't need it.
    if !hiker_core::indexer::is_indexable_path(rel) {
        menu = menu.action("Open original externally", FileVerb::OpenExternal);
    }
    // Add-to-trail: only when a trail is active; disabled (with the
    // "Already in '…'" label) when `rel` is already a waypoint at any depth.
    if let Some((trail_name, already)) = active_trail {
        let action = if already {
            egui_workbench::menu::Action::new(format!("Already in '{trail_name}'"), FileVerb::AddToTrail {
                trail_name: trail_name.clone(),
            })
            .enabled(egui_workbench::menu::Enabled::No("already a waypoint".into()))
        } else {
            egui_workbench::menu::Action::new(format!("Add to trail '{trail_name}'"), FileVerb::AddToTrail {
                trail_name,
            })
        };
        menu = menu.action_with(action);
    }
    // "Set as active trail" + "Export to canvas" — only on a `.hiker/trails/*.md`
    // row (the trail-doc detection). status: canvas-export-trail-verb
    if rel.starts_with(".hiker/trails/") && rel.ends_with(".md") {
        menu = menu
            .action("Set as active trail", FileVerb::SetActiveTrail)
            .action("Export to canvas", FileVerb::ExportTrailToCanvas);
    }
    // Board-docs get an explicit "Open as board" verb (the default click
    // already routes there).
    if board_doc {
        menu = menu.action("Open as board", FileVerb::OpenAsBoard);
    }
    // `.canvas` files: an explicit "Open as canvas" (the default click route)
    // and a "View as JSON" escape hatch that opens the raw text in the editor.
    // status: canvas-file-tree-glyph
    if is_canvas_doc(rel) {
        menu = menu
            .action("Open as canvas", FileVerb::OpenAsCanvas)
            .action("View as JSON", FileVerb::ViewCanvasAsJson);
    }
    // "Add to board…" on indexable note rows: a board → column nested
    // picker. Hidden on board-doc rows and non-`.md` rows; disabled
    // per-board when the note is already a card. The picker renders its own
    // live nested widgets, so it rides a `Custom` entry.
    if !board_doc && rel.ends_with(".md") && !boards.is_empty() {
        menu = menu.custom(move |ui| {
            let mut verb = None;
            ui.menu_button("Add to board…", |ui| {
                let mut pick: Option<(String, String)> = None;
                crate::panels::board_picker::column_picker(ui, &boards, &membership, &mut pick);
                if let Some((board_rel, column)) = pick {
                    verb = Some(FileVerb::AddToBoard { board_rel, column });
                }
            });
            verb
        });
    }
    // "Add to canvas…" on non-`.canvas` rows: a nested picker listing every
    // `.canvas` doc in the vault. Selecting one inserts this row's vault path
    // as a file-node pointer (whether or not that canvas is open). A canvas can
    // hold the same note twice, so there's no already-present disabling.
    // status: canvas-add-to-canvas-verb
    if !is_canvas_doc(rel) && !canvases.is_empty() {
        menu = menu.custom(move |ui| {
            let mut verb = None;
            ui.menu_button("Add to canvas…", |ui| {
                for (canvas_rel, title) in &canvases {
                    if ui.button(title).clicked() {
                        verb = Some(FileVerb::AddToCanvas { canvas_rel: canvas_rel.clone() });
                        ui.close();
                    }
                }
            });
            verb
        });
    }
    menu.action("Delete", FileVerb::Delete)
}

/// The active trail's `(title, whether `rel` is already a waypoint)`, looked
/// up lazily when a file-row context menu opens — NOT per frame. Backs the
/// "Add to trail '…'" verb (and its "Already in '…'" disabled state).
///
/// Reading this is O(active-trail size) plus the trail-doc read/parse, which
/// is why it's gathered on menu-open (mirroring the boards / canvases pickers
/// gathered alongside it) rather than snapshotted every frame. Returns `None`
/// when no trail is active or the active trail-doc can't be read / resolved.
fn active_trail_membership(ctx: &SurfaceCtx<'_>, rel: &str) -> Option<(String, bool)> {
    let active_rel = ctx
        .config
        .read()
        .ok()
        .and_then(|c| c.vault.active_trail.clone())?;
    let store = ctx.services.read_store.lock().ok()?;
    let detail =
        hiker_core::trails::get_trail(ctx.vault, &store, &ctx.services.layered, &active_rel).ok()?;
    let mut members = std::collections::HashSet::new();
    collect_source_paths(&detail.waypoints, &mut members);
    Some((trail_title(&active_rel), members.contains(rel)))
}

/// Trail-doc title (basename without `.md`).
fn trail_title(rel: &str) -> String {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// Collect every resolved waypoint's source-note path (recursively) into
/// `out` — the "already in active trail" membership set the file tree
/// decorates source-note rows with.
fn collect_source_paths(
    waypoints: &[hiker_core::trails::ResolvedWaypoint],
    out: &mut std::collections::HashSet<String>,
) {
    for w in waypoints {
        if !w.source_path.is_empty() {
            out.insert(w.source_path.clone());
        }
        collect_source_paths(&w.children, out);
    }
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

pub(super) fn basename_of(rel: &str) -> &str {
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

/// True if the `.md` at `rel` is a board-doc — frontmatter `hiker.kind:
/// board` or a registered board-like kind like `sprint`
/// (`sprint-board-subtype`), so sprint board-docs click-route to the board
/// view too. Reads + parses the file — called on click / menu open, never
/// per-frame.
fn is_board_doc(app: &AppState, rel: &str) -> bool {
    if !rel.ends_with(".md") {
        return false;
    }
    app.vault_session
        .vault
        .read_file(rel)
        .ok()
        .map(|src| {
            hiker_core::boards::parse_board_for(
                rel,
                &src,
                Some(app.vault_session.services.kinds.as_ref()),
            )
            .is_ok()
        })
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

/// Allowlist of code-file extensions that open as strictly read-only reference
/// content (plain text, no syntax highlighting in this phase). Lowercase; the
/// match is case-insensitive. Notes (`.md`) are intentionally absent — they're
/// the only authored/editable content. status: code-read-only-view
const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "rb", "c", "h", "cpp",
    "hpp", "cc", "cs", "php", "swift", "kt", "scala", "sh", "bash", "toml",
    "yaml", "yml", "json", "sql", "lua", "ex", "exs", "hs", "ml",
];

/// True when `rel`'s extension is in the code allowlist (case-insensitive).
fn is_code_file(rel: &str) -> bool {
    rel.rsplit_once('.').is_some_and(|(_, ext)| {
        CODE_EXTENSIONS.iter().any(|c| ext.eq_ignore_ascii_case(c))
    })
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
    let row_height = ROW_HEIGHT;
    let total_width = ui.available_width();
    // `click_and_drag`, not just `click`: the row is both clickable (open /
    // expand) AND a drag SOURCE — `Response::dnd_set_drag_payload` only sets a
    // payload when `drag_started()` fires, which requires the widget to sense
    // drag. Without this the filetree drag (reparent into a folder) and the
    // canvas drag-to-add never produce a payload at all. A click vs a drag is
    // still disambiguated by egui's drag threshold, so `clicked()` is unaffected.
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(total_width, row_height), egui::Sense::click_and_drag());
    // The shared "click acts here" signal (`interaction.md` [hover-open-signal]):
    // active/hover wash through the one canonical policy, plus the pointer cursor.
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if let Some(c) = theme::open_signal_wash(active, resp.hovered()) {
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

    // Drag ghost: while this row is being dragged, paint the shared floating
    // chip of its label at the cursor (`widgets::note_row`) so the item
    // visibly "follows" the pointer.
    if resp.dragged() {
        crate::widgets::note_row::drag_ghost(ui, label);
    }
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
    fn source_paths_collected_recursively() {
        use hiker_core::trails::ops::ResolutionOutcome;
        use hiker_core::trails::ResolvedWaypoint;
        let wp = |source: &str, children: Vec<ResolvedWaypoint>| ResolvedWaypoint {
            waypoint_rel: format!("trails/t/{source}--abc123.md"),
            annotation_body: String::new(),
            source_path: source.to_string(),
            in_trail_path: "trails/t.md".to_string(),
            resolution: ResolutionOutcome::Resolved { rel_path: source.to_string() },
            children,
            tree_path: "1".to_string(),
        };
        let forest = vec![wp("a.md", vec![wp("b.md", vec![wp("c.md", vec![])])])];
        let mut set = std::collections::HashSet::new();
        collect_source_paths(&forest, &mut set);
        assert!(set.contains("a.md") && set.contains("b.md") && set.contains("c.md"));
    }
}
