//! Right-click clipboard menu for the editor text area (`bug-editor-no-context-menu`).
//!
//! Mirrors the keyboard clipboard verbs so a right-click offers Cut / Copy /
//! Paste / Select All. The verbs deliberately reuse the editor's existing
//! input path rather than re-implementing clipboard handling: Cut / Copy /
//! Paste are issued as egui viewport commands, which inject the same
//! `Event::Cut` / `Event::Copy` / `Event::Paste` the OS shortcuts produce, so
//! the editor widget's per-frame event loop runs them through the one
//! clipboard implementation in `editor_view::command::handle`. Select All
//! injects a primary-modifier `A` key event that the same loop already maps
//! to `motion::select_all`. The injected events only reach the editor while
//! it holds egui focus, so each verb requests focus on the editor response
//! before dispatching.

use eframe::egui;

/// The editor clipboard verbs, mirrored from the keyboard shortcuts.
#[derive(Clone, Copy)]
enum ClipboardVerb {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// Build the editor clipboard menu (status: ctxmenu-editor-clipboard).
/// Cut / Copy / Paste, a separator, then Select All — one source of truth for
/// the editor's right-click clipboard verbs.
fn build_clipboard_menu() -> egui_workbench::menu::Menu<ClipboardVerb> {
    egui_workbench::menu::Menu::new()
        .action("Cut", ClipboardVerb::Cut)
        .action("Copy", ClipboardVerb::Copy)
        .action("Paste", ClipboardVerb::Paste)
        .section()
        .action("Select All", ClipboardVerb::SelectAll)
}

/// Everything the editor's right-click context menu needs beyond the clipboard
/// verbs: the optional chart / table targets the right-click landed on, plus the
/// out-params the chosen action is written back through (applied by the caller
/// once the editor's buffer borrow has ended). Grouped so [`attach`] stays under
/// the argument cap. status: ctxmenu-editor-clipboard
pub struct AttachCtx<'a> {
    pub editor_resp: &'a egui::Response,
    pub chart_target: Option<&'a super::widgets::chart::EditTarget>,
    pub chart_open: &'a mut Option<super::widgets::chart::EditTarget>,
    /// The table the right-click landed on, if any — drives the Fit ⇄ Scrollable
    /// toggle items. status: widget-table-overflow-scroll
    pub table_target: Option<super::widgets::tables::TableOverflowTarget>,
    /// The live per-table overflow map, read to mark the current mode.
    pub table_views: &'a super::widgets::tables::TableViewMap,
    /// The `(byte_start, mode)` the user picked, written back for the caller to
    /// apply after the borrow ends. status: widget-table-overflow-scroll
    pub table_choice: &'a mut Option<(usize, super::widgets::tables::TableOverflow)>,
    /// The cell the right-click landed on, if any — drives the "Edit diagram" /
    /// "Edit cell" in-place edit item. status: widget-table-cell-edit-inplace
    pub cell_target: Option<super::widgets::tables::cell_edit::TableCellTarget>,
    /// The current document text, so the item can classify the cell (block →
    /// "Edit diagram", text → "Edit cell"). status: widget-table-cell-edit-inplace
    pub cell_doc: &'a str,
    /// The cell to enter in-place edit on, written back for the caller to enter
    /// after the borrow ends. status: widget-table-cell-edit-inplace
    pub cell_edit: &'a mut Option<super::widgets::tables::cell_edit::TableCellTarget>,
}

/// Attach the editor's right-click context menu: the clipboard verbs, plus —
/// when the right-click landed on an inline ```` ```chart ```` widget
/// (`chart_target` is `Some`) — an "Open in chart editor" item at the top, and —
/// when it landed on a rendered pipe table (`table_target` is `Some`) — the
/// Fit ⇄ Scrollable overflow toggle (`widget-table-overflow-scroll`). The chosen
/// chart / table action is written to the ctx's out-params for the caller to act
/// on once the editor's buffer borrow has ended. A left click on a chart / table
/// reveals or edits its source instead (handled upstream).
/// status: ctxmenu-editor-clipboard, chart-open-in-builder, widget-table-overflow-scroll
pub fn attach(ctx: AttachCtx<'_>) {
    let AttachCtx {
        editor_resp,
        chart_target,
        chart_open,
        table_target,
        table_views,
        table_choice,
        cell_target,
        cell_doc,
        cell_edit,
    } = ctx;
    let mut chosen = None;
    editor_resp.context_menu(|ui| {
        if let Some(target) = chart_target {
            if ui.button("Open in chart editor").clicked() {
                *chart_open = Some(target.clone());
                ui.close();
            }
            ui.separator();
        }
        // In-place cell edit (`widget-table-cell-edit-inplace`): "Edit diagram"
        // (block cell) / "Edit cell" (text cell) — above the overflow toggle.
        if let Some(target) = &cell_target {
            if let Some(pick) = super::table_cell_edit::menu_item(ui, target, cell_doc) {
                *cell_edit = Some(pick);
            }
        }
        if let Some(target) = table_target {
            if let Some(pick) =
                super::table_overflow_menu::menu_items(ui, target, table_views)
            {
                *table_choice = Some(pick);
            }
        }
        chosen = egui_workbench::menu::show(ui, build_clipboard_menu());
    });
    let Some(verb) = chosen else { return };
    // Each verb reuses the editor's existing input path: focus the editor so
    // the injected event reaches it, then dispatch the same viewport command /
    // synthetic key the keyboard shortcuts produce.
    editor_resp.request_focus();
    let ctx = editor_resp.ctx.clone();
    match verb {
        ClipboardVerb::Cut => ctx.send_viewport_cmd(egui::ViewportCommand::RequestCut),
        ClipboardVerb::Copy => ctx.send_viewport_cmd(egui::ViewportCommand::RequestCopy),
        ClipboardVerb::Paste => ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste),
        ClipboardVerb::SelectAll => {
            // The editor's event translation reads the concrete `ctrl` /
            // `mac_cmd` fields, not egui's logical `command` flag, so set the
            // platform's primary modifier directly — a bare
            // `Modifiers::COMMAND` would translate to no modifier and the
            // editor would insert a literal "a".
            let mac = cfg!(target_os = "macos");
            ctx.input_mut(|i| {
                i.events.push(egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        ctrl: !mac,
                        mac_cmd: mac,
                        command: true,
                        ..Default::default()
                    },
                });
            });
        }
    }
}
