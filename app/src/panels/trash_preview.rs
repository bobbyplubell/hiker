//! Read-only preview of a single trash entry. Renders the trashed file's
//! contents in the editor (no diff — there's no "after" to compare
//! against until the entry is restored) and offers Restore / Delete
//! permanently. Restore goes through `vault::restore_note`; permanent
//! delete through `Trash::permanent_delete`.

use std::fs;

use eframe::egui;

use hiker_core::trash::Trash;

use crate::panels::diff_view::{self, PreviewBuffer};
use crate::panels::preview_common::{banner, close_active};
use crate::state::{AppState, ToastLevel};

pub fn show(ui: &mut egui::Ui, app: &mut AppState, trash_path: &str, original_path: &str) {
    let key = format!("trash:{}", trash_path);

    if !app.panels.preview_buffers.contains_key(&key) {
        // `trash_path` is a vault-relative path under `.hiker/trash/`.
        // Read directly off disk — Vault::read_file would also work but
        // we skip the path resolver to keep the dot-prefixed component.
        let abs = app.vault_session.vault_root.join(trash_path);
        let contents = match fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(err) => {
                ui.colored_label(
                    egui::Color32::RED,
                    format!("read trash entry: {}", err),
                );
                return;
            }
        };
        // No diff for trash previews — the entry just shows its body.
        let buf = PreviewBuffer::new(key.clone(), String::new(), contents, false);
        app.panels.preview_buffers.insert(key.clone(), buf);
    }

    let mut restore_clicked = false;
    let mut delete_clicked = false;

    banner(ui, "Trash entry", original_path, |ui| {
        if ui
            .add(
                egui::Button::image_and_text(
                    crate::icons::primary_restore(),
                    egui::RichText::new("Restore").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0x2f, 0x6f, 0xed)),
            )
            .on_hover_text("Move this entry back to its original location")
            .clicked()
        {
            restore_clicked = true;
        }
        if ui
            .add(
                egui::Button::image_and_text(
                    crate::icons::primary_trash(),
                    egui::RichText::new("Delete permanently").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
            )
            .on_hover_text("Erase this entry from disk — cannot be undone")
            .clicked()
        {
            delete_clicked = true;
        }
    });

    ui.separator();

    if let Some(buf) = app.panels.preview_buffers.get_mut(&key) {
        diff_view::show(ui, buf);
    }

    if restore_clicked {
        let trash = Trash::open(&app.vault_session.vault_root);
        // Find the trash entry whose on-disk basename matches the
        // `.hiker/trash/<name>` suffix the tab carries.
        let trashed_name = trash_path
            .rsplit('/')
            .next()
            .unwrap_or(trash_path)
            .to_string();
        let entry = match trash.list_from_disk() {
            Ok(items) => items
                .into_iter()
                .find(|i| i.trashed_name == trashed_name),
            Err(err) => {
                app.push_toast(
                    format!("Trash list: {}", err),
                    ToastLevel::Error,
                );
                return;
            }
        };
        let Some(entry) = entry else {
            app.push_toast(
                format!("Trash entry no longer present: {}", trashed_name),
                ToastLevel::Warn,
            );
            return;
        };
        let Some(id) = entry.id else {
            app.push_toast(
                "Cannot restore: trash entry has no manifest record (orphan)",
                ToastLevel::Warn,
            );
            return;
        };
        match hiker_core::vault::restore_note(
            &app.vault_session.vault,
            Some(app.vault_session.services.watcher.as_ref()),
            &trash,
            &id,
        ) {
            Ok(restored) => {
                app.push_toast(
                    format!("Restored {}", restored.original_path),
                    ToastLevel::Info,
                );
                app.panels.preview_buffers.remove(&key);
                close_active(app);
            }
            Err(err) => app.push_toast(
                format!("Restore failed: {}", err),
                ToastLevel::Error,
            ),
        }
    } else if delete_clicked {
        let trash = Trash::open(&app.vault_session.vault_root);
        let trashed_name = trash_path
            .rsplit('/')
            .next()
            .unwrap_or(trash_path)
            .to_string();
        match trash.permanent_delete(&trashed_name) {
            Ok(()) => {
                app.push_toast(
                    format!("Permanently deleted {}", trashed_name),
                    ToastLevel::Info,
                );
                app.panels.preview_buffers.remove(&key);
                close_active(app);
            }
            Err(err) => app.push_toast(
                format!("Delete failed: {}", err),
                ToastLevel::Error,
            ),
        }
    }
}

