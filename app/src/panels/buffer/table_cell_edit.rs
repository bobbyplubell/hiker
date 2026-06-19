//! In-place editing of a single rendered-table cell (`widget-table-cell-edit-inplace`).
//!
//! Phase D of the table widget. Earlier phases render a pipe table as a
//! natively-painted `BlockWidget` (`widget-table-render`), host block content in
//! cells (`widget-table-render` Phase B/C), surface a per-cell click → caret
//! (`widget-table-cell-edit`), and add a right-click Fit ⇄ Scrollable overflow
//! menu (`widget-table-overflow-scroll`). This module lets the user edit ONE cell
//! — especially a diagram cell — *in place*, without the whole table collapsing
//! to its raw pipe source.
//!
//! It is the buffer-panel sibling of [`super::table_overflow_menu`] and a close
//! mirror of the canvas inline-edit overlay (`canvas::edit`): a foreground
//! `egui::Area` over the active cell hosts a transient `editor-egui` editor seeded
//! with ONLY that cell's source text. Each change splices the cell's byte range in
//! the underlying markdown through the ordinary layered-doc binding (so undo / layered-doc /
//! sync just work — there is no structured-model-then-reserialize). The owning
//! table suppresses its whole-table reveal (`TableProviderInputs::editing_table`)
//! so it stays fully rendered; two accent cues draw on top — a soft frame around
//! the table, a brighter outline on the active cell.
//!
//! The editor-core widget stays interaction-agnostic: it only emits the
//! per-cell whole-widget click zones (`widget-table-cell-edit`). Right-click,
//! double-click, and the focused-cell are resolved host-side here against those
//! zones — exactly the seam [`super::table_overflow_menu::table_under_right_click`]
//! already uses for the overflow menu.

use std::cell::RefCell;
use std::collections::HashMap;

use eframe::egui;

use editor_core::state::Editor as EditorState;
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::{ClickAction, ClickZone, ViewState};

use crate::buffer::DecorationCache;
use crate::panels::buffer::decorations::{rebuild_editor_layers, DecoRebuildCtx};
use crate::panels::buffer::widgets::tables::cell_edit::{cell_is_block, TableCellTarget};
use crate::state::AppState;

/// The active in-place cell edit, stored on the `Buffer` (`buffer.editing_cell`).
/// Ephemeral; the transient overlay editor itself lives in [`OVERLAYS`] keyed by
/// the buffer path. `range` is the cell's CURRENT byte range in the document — it
/// is updated after every splice so the next splice targets the right bytes even
/// as the cell text grows / shrinks. status: widget-table-cell-edit-inplace
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellEdit {
    /// Byte start of the enclosing table block — the reveal-suppression key and
    /// the per-table edit identity.
    pub table_start: usize,
    /// The active cell's whole-widget click id (`table_cell_id`), used to hit-test
    /// the cell's on-screen rect against the per-frame click zones.
    pub cell_id: u64,
    /// The cell's current editable byte range in the document.
    pub range: std::ops::Range<usize>,
}

/// The transient editor backing one in-progress cell edit: an `editor-egui`
/// editor over the cell's source, its own view / paint / decoration caches, and a
/// mirror of the last text spliced back (so an unchanged frame skips the splice).
/// Mirrors `canvas::edit::TextEdit`. Parked off `AppState` in [`OVERLAYS`] so the
/// splice path can hold `&mut AppState` and `&mut TransientEditor` at once.
struct TransientEditor {
    editor: EditorState,
    view: ViewState,
    paint: PaintCache,
    decorations: DecorationCache,
    /// The cell source as of the last committed splice, so an unchanged frame is a
    /// no-op.
    last: String,
}

impl TransientEditor {
    fn new(text: &str) -> Self {
        let mut view = ViewState { font_size: 14.0, hide_gutter: true, ..ViewState::default() };
        view.wrap_map.set_enabled(true);
        Self {
            editor: EditorState::new(text),
            view,
            paint: PaintCache::default(),
            decorations: DecorationCache::default(),
            last: text.to_string(),
        }
    }
}

// One transient overlay editor per buffer path, parked off `AppState`. Mirrors
// `canvas::edit::EDIT_VIEWS`; dropped on edit exit (and never leaks because only
// one cell edits at a time per buffer). status: widget-table-cell-edit-inplace
thread_local! {
    static OVERLAYS: RefCell<HashMap<String, TransientEditor>> = RefCell::new(HashMap::new());
}

/// Drop the transient overlay editor for `path`. status: widget-table-cell-edit-inplace
fn forget(path: &str) {
    OVERLAYS.with(|o| {
        o.borrow_mut().remove(path);
    });
}

/// Enter in-place edit of `target`'s cell for the buffer at `path`: record the
/// edit on the buffer and seed the transient overlay editor with the cell's
/// current source text. Replaces any prior edit (one cell at a time).
/// status: widget-table-cell-edit-inplace
pub fn enter(app: &mut AppState, path: &str, target: &TableCellTarget) {
    let Some(buffer) = app.session.buffers.get_mut(path) else {
        return;
    };
    let doc = buffer.editor.doc.to_string();
    let src = doc.get(target.range.clone()).unwrap_or("").to_string();
    buffer.editing_cell = Some(CellEdit {
        table_start: target.table_start,
        cell_id: target.cell_id,
        range: target.range.clone(),
    });
    OVERLAYS.with(|o| {
        o.borrow_mut().insert(path.to_string(), TransientEditor::new(&src));
    });
}

/// Exit any in-place cell edit for the buffer at `path`: clear the buffer state
/// and drop the overlay editor. Idempotent. status: widget-table-cell-edit-inplace
pub fn exit(app: &mut AppState, path: &str) {
    if let Some(buffer) = app.session.buffers.get_mut(path) {
        buffer.editing_cell = None;
    }
    forget(path);
}

/// Resolve a trigger to enter in-place edit this frame, without entering it — the
/// caller (after the editor borrow ends) calls [`enter`] with the returned
/// target. Two triggers, both hit-tested against the cell click zones:
///
/// - **Double-click a block cell** (math / mermaid / wavedrom / image) → fast
///   path. A double-click on a text cell does NOT enter (text cells keep their
///   click-reveal; in-place edit is menu-driven for them).
/// - The right-click menu item is handled separately (see [`menu_item`]); it
///   passes its chosen target straight to [`enter`].
///
/// `targets` are this frame's [`tables::table_cell_targets`]; `zones` the editor's
/// per-frame click zones. `stash_id` keys the per-press hit stash in egui temp
/// memory (per buffer, mirroring [`cell_under_right_click`]'s `menu_id`).
/// status: widget-table-cell-edit-inplace
#[must_use]
pub fn double_click_target(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    zones: &[ClickZone],
    targets: &[TableCellTarget],
    doc: &str,
    stash_id: egui::Id,
) -> Option<TableCellTarget> {
    let (pressed, double, pos) = ctx.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.button_double_clicked(egui::PointerButton::Primary),
            i.pointer.interact_pos(),
        )
    });
    // Stash the cell under EVERY primary press (self-correcting, like the
    // right-click stash), so resolving the double-click doesn't depend on the
    // cell's zone still existing at click #2 — a reveal between the clicks
    // collapses the table and drops its zones.
    if pressed {
        let hit = pos
            .filter(|p| editor_rect.contains(*p))
            .and_then(|p| cell_under(p, editor_rect, zones, targets));
        ctx.data_mut(|d| d.insert_temp(stash_id, hit));
    }
    if !double {
        return None;
    }
    let p = pos.filter(|p| editor_rect.contains(*p))?;
    // Prefer the live zone hit; fall back to the press stash when the zones
    // vanished mid-sequence.
    let hit = cell_under(p, editor_rect, zones, targets)
        .or_else(|| ctx.data(|d| d.get_temp::<Option<TableCellTarget>>(stash_id)).flatten())?;
    // Double-click enters edit only for a block cell (the fast path the spec
    // reserves for diagrams); a text cell keeps its reveal-on-click.
    let is_block = doc.get(hit.range.clone()).is_some_and(cell_is_block);
    is_block.then_some(hit)
}

/// The cell target whose on-screen click zone contains `pos` (editor-relative
/// hit-test against the per-frame zones), if any.
fn cell_under(
    pos: egui::Pos2,
    editor_rect: egui::Rect,
    zones: &[ClickZone],
    targets: &[TableCellTarget],
) -> Option<TableCellTarget> {
    let (lx, ly) = (pos.x - editor_rect.min.x, pos.y - editor_rect.min.y);
    let id = zones.iter().find_map(|z| match z.action {
        ClickAction::WidgetClick(id) if z.rect.contains(lx, ly) => Some(id),
        _ => None,
    })?;
    targets.iter().find(|t| t.cell_id == id).cloned()
}

/// The cell under the most recent secondary (right) click, resolved + stashed in
/// egui temp memory keyed by `menu_id` so the choice persists while the menu is
/// open and self-corrects on each right-click. Mirrors
/// [`super::table_overflow_menu::table_under_right_click`].
/// status: widget-table-cell-edit-inplace
#[must_use]
pub fn cell_under_right_click(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    zones: &[ClickZone],
    targets: &[TableCellTarget],
    menu_id: egui::Id,
) -> Option<TableCellTarget> {
    let (secondary, pos) =
        ctx.input(|i| (i.pointer.secondary_clicked(), i.pointer.interact_pos()));
    if secondary {
        let hit = pos
            .filter(|p| editor_rect.contains(*p))
            .and_then(|p| cell_under(p, editor_rect, zones, targets));
        ctx.data_mut(|d| d.insert_temp(menu_id, hit));
    }
    ctx.data(|d| d.get_temp::<Option<TableCellTarget>>(menu_id)).flatten()
}

/// Render the "Edit diagram" / "Edit cell" item inside an already-open
/// `context_menu` `ui` for the right-clicked `target`, returning the target to
/// enter (or `None` if the user didn't pick it). The label is "Edit diagram" for
/// a block cell (math / diagram / image), "Edit cell" for a text cell.
/// status: widget-table-cell-edit-inplace
#[must_use]
pub fn menu_item(ui: &mut egui::Ui, target: &TableCellTarget, doc: &str) -> Option<TableCellTarget> {
    let is_block = doc.get(target.range.clone()).is_some_and(cell_is_block);
    let label = if is_block { "Edit diagram" } else { "Edit cell" };
    let mut chosen = None;
    if ui.button(label).clicked() {
        chosen = Some(target.clone());
        ui.close();
    }
    ui.separator();
    chosen
}

/// The accent cues drawn while a table is in cell-edit (`widget-table-cell-edit-inplace`):
/// a soft frame around the whole table + a brighter outline on the active cell.
/// Returns the active cell's on-screen rect (the overlay anchor), if resolvable
/// from this frame's click zones. No dimming of other cells.
fn draw_cues(
    ui: &egui::Ui,
    editor_rect: egui::Rect,
    zones: &[ClickZone],
    edit: &CellEdit,
    accent: egui::Color32,
) -> Option<egui::Rect> {
    // Resolve the active cell's on-screen rect from this frame's click zones (the
    // whole-table frame is computed separately by `table_frame_rect`).
    let to_screen = |r: &editor_view::viewport::ClickRect| {
        egui::Rect::from_min_max(
            egui::pos2(editor_rect.min.x + r.x_min, editor_rect.min.y + r.y_min),
            egui::pos2(editor_rect.min.x + r.x_max, editor_rect.min.y + r.y_max),
        )
    };
    let active = zones.iter().find_map(|z| match z.action {
        ClickAction::WidgetClick(id) if id == edit.cell_id => Some(to_screen(&z.rect)),
        _ => None,
    })?;
    let p = ui.painter();
    // Brighter outline on the active cell.
    p.rect_stroke(
        active.expand(1.0),
        2.0,
        egui::Stroke::new(2.0, accent),
        egui::StrokeKind::Outside,
    );
    Some(active)
}

/// The whole-table frame rect: the union of every on-screen cell zone of the
/// active edit's table, resolved by matching each zone id against `targets`.
fn table_frame_rect(
    editor_rect: egui::Rect,
    zones: &[ClickZone],
    targets: &[TableCellTarget],
    table_start: usize,
) -> Option<egui::Rect> {
    let ids: std::collections::HashSet<u64> =
        targets.iter().filter(|t| t.table_start == table_start).map(|t| t.cell_id).collect();
    let mut frame: Option<egui::Rect> = None;
    for z in zones {
        let ClickAction::WidgetClick(id) = z.action else { continue };
        if !ids.contains(&id) {
            continue;
        }
        let r = egui::Rect::from_min_max(
            egui::pos2(editor_rect.min.x + z.rect.x_min, editor_rect.min.y + z.rect.y_min),
            egui::pos2(editor_rect.min.x + z.rect.x_max, editor_rect.min.y + z.rect.y_max),
        );
        frame = Some(frame.map_or(r, |f| f.union(r)));
    }
    frame
}

/// Everything one [`show`] frame of the in-place cell editor needs beyond `app` /
/// `path`, bundled so the entry point stays under the argument cap.
pub struct ShowCtx<'a> {
    pub editor_rect: egui::Rect,
    pub zones: &'a [ClickZone],
    pub targets: &'a [TableCellTarget],
    pub theme: Option<&'a editor_core::theme::Theme>,
    /// The frame's transaction sink — the splice rides it into the layered-doc binding
    /// the caller runs after this returns. status: widget-table-cell-edit-inplace
    pub txns: &'a mut Vec<editor_core::transaction::Transaction>,
}

/// Drive one frame of the active in-place cell edit for the buffer at `path`: draw
/// the accent cues, render the overlay editor over the active cell, splice any
/// change into the buffer's cell byte range (pushed onto `ctx.txns`), and handle
/// the exits (Esc, click-outside, Tab / Shift+Tab cell nav). A no-op when no cell
/// is being edited. status: widget-table-cell-edit-inplace
pub fn show(ui: &mut egui::Ui, app: &mut AppState, path: &str, ctx: ShowCtx<'_>) {
    let Some(edit) = app.session.buffers.get(path).and_then(|b| b.editing_cell.clone()) else {
        return;
    };
    let accent = ui.visuals().selection.stroke.color;
    // Whole-table soft frame.
    if let Some(frame) = table_frame_rect(ctx.editor_rect, ctx.zones, ctx.targets, edit.table_start) {
        ui.painter().rect_stroke(
            frame.expand(3.0),
            4.0,
            egui::Stroke::new(1.5, accent.gamma_multiply(0.6)),
            egui::StrokeKind::Outside,
        );
    }
    // Active-cell outline + anchor rect for the popover.
    let Some(cell_rect) = draw_cues(ui, ctx.editor_rect, ctx.zones, &edit, accent) else {
        // The cell scrolled off-screen → exit (mirrors the canvas off-screen exit).
        exit(app, path);
        return;
    };
    let ShowCtx { editor_rect: _, zones: _, targets, theme, txns } = ctx;
    let nav = render_popover(ui, app, path, &edit, cell_rect, theme, txns);
    apply_exit(ui, app, path, &edit, targets, nav);
}

/// What the popover frame reported back: a cell-nav request, an Esc / click-out
/// exit, or nothing (keep editing).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavSignal {
    None,
    Escape,
    Next,
    Prev,
}

/// Render the overlay popover anchored to `cell_rect`: a foreground `egui::Area`
/// holding the transient editor (it may extend past the narrow cell width — a
/// one-line fence is wider than the cell). Splices any change into the buffer.
/// Returns the nav / exit signal the frame produced.
fn render_popover(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    edit: &CellEdit,
    cell_rect: egui::Rect,
    theme: Option<&editor_core::theme::Theme>,
    txns: &mut Vec<editor_core::transaction::Transaction>,
) -> NavSignal {
    // Width the popover is allowed to grow to: at least a comfy editing width
    // (a one-line fence is wider than the cell), clamped to a sane max.
    let want_w = cell_rect.width().max(320.0).min(720.0);
    let anchor = cell_rect.left_top();
    let id = egui::Id::new(("table-cell-edit", path, edit.cell_id));
    let theme_owned = editor_core::theme::light_default();
    let theme = theme.unwrap_or(&theme_owned);
    let dpr = ui.ctx().pixels_per_point();
    let mut nav = NavSignal::None;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .show(ui.ctx(), |ui| {
            nav = read_nav(ui);
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(want_w);
                ui.set_max_width(want_w);
                if let Some(text) = render_transient(ui, path, theme, dpr) {
                    splice(app, path, edit, &text, txns);
                }
            });
        });
    nav
}

/// The nav / exit key signal this frame: Tab / Shift+Tab move cells; Escape
/// exits. Consumed here so the keys don't reach the transient editor.
fn read_nav(ui: &egui::Ui) -> NavSignal {
    ui.input_mut(|i| {
        if i.key_pressed(egui::Key::Escape) {
            NavSignal::Escape
        } else if i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab) {
            NavSignal::Prev
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
            NavSignal::Next
        } else {
            NavSignal::None
        }
    })
}

/// Render the transient overlay editor (the cell's source) with the markdown
/// live-preview pipeline on, so a diagram cell shows + live-previews as you type.
/// Returns the new cell text when it changed this frame. Mirrors
/// `canvas::edit::render_text_widget`. status: widget-table-cell-edit-inplace
fn render_transient(
    ui: &mut egui::Ui,
    path: &str,
    theme: &editor_core::theme::Theme,
    dpr: f32,
) -> Option<String> {
    OVERLAYS.with(|o| {
        let mut o = o.borrow_mut();
        let edit = o.get_mut(path)?;
        let body = edit.editor.doc.to_string();
        let font_px = edit.view.font_size;
        let mut deco_ctx = DecoRebuildCtx {
            cache: &mut edit.decorations,
            folds: &EMPTY_FOLDS,
            loaded_text: &body,
            // No dirty-diff gutter in the in-place cell-edit popover. status: git-dirty-diff-gutter
            git_head_text: None,
            theme: Some(theme),
            live_preview: true,
            render_widgets: true,
            is_markdown: true,
            code_language: None,
            dpr,
            font_px,
            chunk_boundaries: false,
            show_whitespace: false,
            highlight_trailing_whitespace: false,
            diff: None,
            conflict: None,
            resolve_title: None,
            diagram_cache: None,
            chart_resolver: None,
            image_resolver: None,
            table_overflow: &EMPTY_TABLE_OVERFLOW,
            editing_table: None,
        };
        let mut rebuild = |state: &EditorState, view: &mut ViewState| {
            rebuild_editor_layers(state, view, &mut deco_ctx);
        };
        let response = EditorWidget::new(&mut edit.editor, &mut edit.view)
            .with_paint_cache(&mut edit.paint)
            .with_decoration_rebuild(&mut rebuild)
            .show(ui);
        // Hold focus while editing (no-op once focused) so the buffer panel can't
        // steal it — otherwise typing / Backspace never land.
        if !response.has_focus() {
            response.request_focus();
        }
        let new_body = edit.editor.doc.to_string();
        if new_body == edit.last {
            return None;
        }
        edit.last = new_body.clone();
        Some(new_body)
    })
}

/// Splice the new cell `text` into the buffer's cell byte range as an ordinary
/// buffer edit: build the change set, apply it to `buffer.editor` (advancing the
/// doc + mapping selection like the widget does), push the transaction onto the
/// frame sink so the layered-doc binding mirrors it, and update the tracked cell range
/// to the new length. Buffer stays the source of truth — undo / layered-doc / sync ride
/// the same path every keystroke does. status: widget-table-cell-edit-inplace
fn splice(
    app: &mut AppState,
    path: &str,
    edit: &CellEdit,
    text: &str,
    txns: &mut Vec<editor_core::transaction::Transaction>,
) {
    let Some(buffer) = app.session.buffers.get_mut(path) else {
        return;
    };
    let doc_len = buffer.editor.doc.len_bytes();
    // Guard against a stale range (the doc shifted out from under the cell, e.g. a
    // concurrent layered-doc refresh): a range past the doc is dropped this frame.
    if edit.range.end > doc_len {
        return;
    }
    let set = editor_core::change::Set::of(
        doc_len,
        std::iter::once((edit.range.clone(), text.to_string())),
    );
    let tx = editor_core::transaction::Transaction::new(set)
        .with_edit_type(editor_core::transaction::EditType::Input);
    buffer.editor = buffer.editor.apply(tx.clone());
    txns.push(tx);
    // The cell now spans `start .. start + new_len`; record it so the next splice
    // targets the right bytes.
    let new_range = edit.range.start..(edit.range.start + text.len());
    if let Some(e) = buffer.editing_cell.as_mut() {
        e.range = new_range;
    }
}

/// Act on the popover's nav / exit signal: Escape / a press outside the popover
/// commits and closes; Tab / Shift+Tab commit the current cell then move to the
/// next / previous cell (across row boundaries), re-seeding the overlay.
/// status: widget-table-cell-edit-inplace
fn apply_exit(
    ui: &egui::Ui,
    app: &mut AppState,
    path: &str,
    edit: &CellEdit,
    targets: &[TableCellTarget],
    nav: NavSignal,
) {
    match nav {
        NavSignal::Escape => exit(app, path),
        NavSignal::Next => move_cell(app, path, edit, targets, 1),
        NavSignal::Prev => move_cell(app, path, edit, targets, -1),
        NavSignal::None => {
            if press_outside_popover(ui, path, edit) {
                exit(app, path);
            }
        }
    }
}

/// Whether a pointer press this frame landed OUTSIDE the popover area — the
/// click-outside commit-and-close. The entering double-click / menu click is a
/// prior frame, so it never trips this.
fn press_outside_popover(ui: &egui::Ui, path: &str, edit: &CellEdit) -> bool {
    let id = egui::Id::new(("table-cell-edit", path, edit.cell_id));
    let area_rect = ui.ctx().memory(|m| m.area_rect(id));
    ui.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| area_rect.map_or(true, |r| !r.contains(p)))
    })
}

/// Tab / Shift+Tab cell nav (`step` = +1 next, −1 prev): move to the adjacent
/// cell of the SAME table in document order (row-major, crossing row
/// boundaries), re-seeding the overlay there. Wraps at the table's ends by
/// exiting (no cell to move to). The current cell's text is already committed
/// (splice rides every keystroke). status: widget-table-cell-edit-inplace
fn move_cell(
    app: &mut AppState,
    path: &str,
    edit: &CellEdit,
    targets: &[TableCellTarget],
    step: i64,
) {
    // Cells of this table in document order (the targets are emitted row-major).
    let cells: Vec<&TableCellTarget> =
        targets.iter().filter(|t| t.table_start == edit.table_start).collect();
    let cur = cells.iter().position(|t| t.cell_id == edit.cell_id);
    match cur.and_then(|c| next_cell_index(c, cells.len(), step)) {
        Some(next) => {
            let target = cells[next].clone();
            enter(app, path, &target);
        }
        // Off either end of the table (or the cell vanished) → commit + close.
        None => exit(app, path),
    }
}

/// The next cell index for a Tab (`step` +1) / Shift+Tab (`step` −1) from `cur`
/// among `len` cells, or `None` when the step runs off either end (no wrap — the
/// caller exits). Pure so it's unit-testable. status: widget-table-cell-edit-inplace
const fn next_cell_index(cur: usize, len: usize, step: i64) -> Option<usize> {
    let next = cur as i64 + step;
    if next < 0 || next as usize >= len {
        None
    } else {
        Some(next as usize)
    }
}

/// An always-empty fold set for the transient overlay's decoration rebuild,
/// borrowed `'static` so it isn't allocated per frame. status: widget-table-cell-edit-inplace
static EMPTY_FOLDS: std::sync::LazyLock<std::collections::HashSet<u64>> =
    std::sync::LazyLock::new(std::collections::HashSet::new);

/// An always-empty per-table overflow map for the transient overlay's rebuild
/// (the popover edits one cell's source, which never hosts a nested table).
/// status: widget-table-cell-edit-inplace
static EMPTY_TABLE_OVERFLOW: std::sync::LazyLock<
    crate::panels::buffer::widgets::tables::TableViewMap,
> = std::sync::LazyLock::new(crate::panels::buffer::widgets::tables::TableViewMap::new);

#[cfg(test)]
mod tests;
