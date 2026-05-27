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

/// Attach the clipboard context menu to the editor's text-area response.
pub fn attach(editor_resp: &egui::Response) {
    editor_resp.context_menu(|ui| {
        // egui auto-closes a menu when one of its buttons is clicked, so the
        // verbs only need to request editor focus and dispatch.
        let send = |ui: &mut egui::Ui, vp: egui::ViewportCommand| {
            editor_resp.request_focus();
            ui.ctx().send_viewport_cmd(vp);
        };
        if ui.button("Cut").clicked() {
            send(ui, egui::ViewportCommand::RequestCut);
        }
        if ui.button("Copy").clicked() {
            send(ui, egui::ViewportCommand::RequestCopy);
        }
        if ui.button("Paste").clicked() {
            send(ui, egui::ViewportCommand::RequestPaste);
        }
        ui.separator();
        if ui.button("Select All").clicked() {
            editor_resp.request_focus();
            // The editor's event translation reads the concrete `ctrl` /
            // `mac_cmd` fields, not egui's logical `command` flag, so set the
            // platform's primary modifier directly — a bare
            // `Modifiers::COMMAND` would translate to no modifier and the
            // editor would insert a literal "a".
            let mac = cfg!(target_os = "macos");
            ui.ctx().input_mut(|i| {
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
    });
}
