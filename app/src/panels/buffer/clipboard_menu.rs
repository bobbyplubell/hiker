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

/// Attach the editor's right-click context menu: the clipboard verbs, plus —
/// when the right-click landed on an inline ```` ```chart ```` widget
/// (`chart_target` is `Some`) — an "Open in chart editor" item at the top. The
/// chosen chart target is written to `chart_open` for the caller to act on once
/// the editor's buffer borrow has ended (opening the builder needs `&mut app`).
/// A left click on a chart reveals its source instead (handled upstream).
/// status: ctxmenu-editor-clipboard, chart-open-in-builder
pub fn attach(
    editor_resp: &egui::Response,
    chart_target: Option<&super::widgets::chart::EditTarget>,
    chart_open: &mut Option<super::widgets::chart::EditTarget>,
) {
    let mut chosen = None;
    editor_resp.context_menu(|ui| {
        if let Some(target) = chart_target {
            if ui.button("Open in chart editor").clicked() {
                *chart_open = Some(target.clone());
                ui.close();
            }
            ui.separator();
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
