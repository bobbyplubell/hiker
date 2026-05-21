//! Read-only preview of a single `changes` row (a snapshot of a past file
//! state). Banner + Restore + Toggle-diff actions over the unified diff
//! between the snapshot ("before", what the file looked like at that
//! change) and the current disk text ("after"). Restore writes the
//! snapshot back to disk via the vault + appends a `Modified` row to
//! the changelog.

use eframe::egui;

use hiker_core::changes::{ChangeAppend, ChangeOp};

use crate::panels::diff_view::{self, PreviewBuffer};
use crate::panels::preview_common::{banner, close_active};
use crate::state::{AppState, ToastLevel};

pub fn show(ui: &mut egui::Ui, app: &mut AppState, path: &str, change_id: &str) {
    let changes = app.vault_session.services.changes.clone();

    let Ok(id_num) = change_id.parse::<i64>() else {
        ui.colored_label(egui::Color32::RED, format!("bad change id: {}", change_id));
        return;
    };

    let key = format!("snapshot:{}", change_id);

    if !app.panels.preview_buffers.contains_key(&key) {
        let snapshot_bytes = match changes.content_at(id_num) {
            Ok(Some(b)) => b,
            Ok(None) => {
                ui.label("This change has no recorded content (likely a delete row).");
                return;
            }
            Err(err) => {
                ui.colored_label(egui::Color32::RED, format!("changes.content_at: {}", err));
                return;
            }
        };
        let snapshot_text = String::from_utf8_lossy(&snapshot_bytes).into_owned();
        // "Before" is the content *this change replaced* — the prior row
        // for the same path. That makes Toggle diff show the change this
        // row actually introduced, instead of comparing the snapshot to
        // the current disk (which would be empty for the latest change
        // and confusing for older ones — the change-this-row-made is the
        // useful question, restore decisions can still read the snapshot
        // itself in the no-diff view).
        let previous_text = changes
            .previous_content_for_path(path, id_num)
            .ok()
            .flatten()
            .map(|(_id, bytes)| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let buf = PreviewBuffer::new(key.clone(), previous_text, snapshot_text, true);
        app.panels.preview_buffers.insert(key.clone(), buf);
    }

    let mut restore_clicked = false;
    let mut toggle_diff = false;

    banner(ui, "Snapshot", path, |ui| {
        if ui
            .add(
                egui::Button::image_and_text(
                    crate::icons::primary_restore(),
                    egui::RichText::new("Restore").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0x2f, 0x6f, 0xed)),
            )
            .on_hover_text("Write this snapshot back to disk")
            .clicked()
        {
            restore_clicked = true;
        }
        if ui.button("Toggle diff").clicked() {
            toggle_diff = true;
        }
    });

    ui.separator();

    if let Some(buf) = app.panels.preview_buffers.get_mut(&key) {
        if toggle_diff {
            buf.diff_active = !buf.diff_active;
        }
        diff_view::show(ui, buf);
    }

    if restore_clicked {
        let Some(buf) = app.panels.preview_buffers.get(&key) else {
            return;
        };
        let snapshot_text = buf.after_text.clone();
        let snapshot_hash = hiker_core::hash_str(&snapshot_text);
        let (_disk_text, disk_hash) = app
            .vault_session.vault
            .read_file_with_hash(path)
            .unwrap_or_else(|_| (String::new(), String::new()));
        match app
            .vault_session.vault
            .write_file_checked(path, &disk_hash, &snapshot_text)
        {
            Ok(_) => {
                if let Err(err) = changes.append(ChangeAppend {
                    path,
                    op: ChangeOp::Modified,
                    author: "user",
                    content_hash: Some(&snapshot_hash),
                    content: Some(snapshot_text.as_bytes()),
                    rename_from: None,
                    metadata: serde_json::json!({
                        "restored_from_change_id": id_num,
                    }),
                }) {
                    app.push_toast(
                        format!("changes.append (restore): {}", err),
                        ToastLevel::Warn,
                    );
                }
                app.push_toast(
                    format!("Restored snapshot of {}", path),
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
    }
}

