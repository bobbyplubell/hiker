//! Modal renderer. Drained by the top-level app each frame; only one
//! modal can be open at a time (matches the TS UI's behaviour).

use eframe::egui;

use crate::editor_pane;
use crate::state::{AppState, Modal, ToastLevel};

pub fn show(ctx: &egui::Context, app: &mut AppState) {
    let Some(modal) = app.session.modal.take() else { return };
    match modal {
        Modal::Confirm {
            title,
            body,
            confirm_label,
            cancel_label,
            danger,
            intent,
        } => {
            confirm_dialog(
                ctx,
                app,
                title,
                body,
                confirm_label,
                cancel_label,
                danger,
                intent,
            );
        }
        Modal::DirtyClose { path, tab_id } => {
            dirty_close_dialog(ctx, app, path, tab_id);
        }
        Modal::Recovery { entries } => {
            recovery_dialog(ctx, app, entries);
        }
        Modal::ConfirmDelete { path } => {
            confirm_delete_dialog(ctx, app, path);
        }
        Modal::DiskDrift { path, in_buffer_text } => {
            disk_drift_dialog(ctx, app, path, in_buffer_text);
        }
    }
}

/// Resolve a `pre-write-drift-check` failure. Offers three branches that
/// mirror the TS UI's drift modal: keep mine (force-overwrite), take
/// theirs (reload from disk, discard local edits), or open the dirty-
/// buffer diff so the user can merge by hand.
fn disk_drift_dialog(
    ctx: &egui::Context,
    app: &mut AppState,
    path: String,
    in_buffer_text: String,
) {
    #[derive(Clone, Copy)]
    enum Choice {
        KeepMine,
        TakeTheirs,
        OpenDiff,
        Cancel,
    }
    let mut decision: Option<Choice> = None;
    let mut open = true;
    egui::Window::new("File changed on disk")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} was modified outside hiker after you loaded it.",
                path
            ));
            ui.label("Pick how to resolve the conflict before saving:");
            ui.add_space(8.0);
            if ui.button("Open diff…").clicked() {
                decision = Some(Choice::OpenDiff);
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui.button("Take theirs (reload)").clicked() {
                    decision = Some(Choice::TakeTheirs);
                }
                let btn = egui::Button::new(
                    egui::RichText::new("Keep mine (overwrite)")
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b));
                if ui.add(btn).clicked() {
                    decision = Some(Choice::KeepMine);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(Choice::Cancel);
                }
            });
        });

    match decision {
        Some(Choice::KeepMine) => {
            if let Err(err) = editor_pane::force_save(app, &path, &in_buffer_text) {
                app.push_toast(
                    format!("Save failed: {err}"),
                    ToastLevel::Error,
                );
            }
        }
        Some(Choice::TakeTheirs) => {
            if let Err(err) = editor_pane::reload_from_disk(app, &path) {
                app.push_toast(
                    format!("Reload failed: {err}"),
                    ToastLevel::Error,
                );
            }
        }
        Some(Choice::OpenDiff) => {
            // Switch the open buffer's tab to the dirty-buffer diff view.
            // The view itself already exists at `panels::buffer_diff` and
            // is keyed by buffer path.
            use crate::tab::{Tab, TabKind};
            let id = app.next_tab_id();
            app.session.tabs.push(Tab {
                id,
                kind: TabKind::BufferDiff { path: path.clone() },
                sticky: true,
            });
            app.session.active_tab = Some(id);
        }
        Some(Choice::Cancel) => {}
        None if !open => {}
        None => {
            app.session.modal = Some(Modal::DiskDrift {
                path,
                in_buffer_text,
            });
        }
    }
}

fn confirm_delete_dialog(ctx: &egui::Context, app: &mut AppState, path: String) {
    let mut decision: Option<bool> = None;
    let mut open = true;
    egui::Window::new("Delete note")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Move {} to trash?", path));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
                let btn = egui::Button::new(
                    egui::RichText::new("Move to trash")
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b));
                if ui.add(btn).clicked() {
                    decision = Some(true);
                }
            });
        });

    match decision {
        Some(true) => apply_confirm_delete(app, &path),
        Some(false) => {}
        None if !open => {}
        None => {
            app.session.modal = Some(Modal::ConfirmDelete { path });
        }
    }
}

fn apply_confirm_delete(app: &mut AppState, rel: &str) {
    let store_mutex = app.vault_session.services.read_store.clone();
    let watcher = app.vault_session.services.watcher.clone();
    let trash = hiker_core::trash::Trash::open(&app.vault_session.vault_root);
    let mut store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Err(err) = hiker_core::vault::delete_note(
        &app.vault_session.vault,
        &mut store,
        Some(watcher.as_ref()),
        &trash,
        rel,
    ) {
        drop(store);
        app.push_toast(format!("Delete failed: {}", err), ToastLevel::Error);
        return;
    }
    drop(store);

    // Close any open tabs for the deleted path + drop its buffer.
    let to_close: Vec<crate::tab::TabId> = app
        .session
        .tabs
        .iter()
        .filter(|t| t.buffer_path() == Some(rel))
        .map(|t| t.id)
        .collect();
    for id in to_close {
        editor_pane::close_tab(app, id);
    }
    app.session.buffers.remove(rel);
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    app.session.sidebar.dir_cache.remove(parent);
    app.push_toast(format!("Moved {} to trash", rel), ToastLevel::Info);
}

#[allow(clippy::too_many_arguments)]
fn confirm_dialog(
    ctx: &egui::Context,
    app: &mut AppState,
    title: String,
    body: String,
    confirm_label: String,
    cancel_label: String,
    danger: bool,
    intent: crate::state::ConfirmIntent,
) {
    let mut open = true;
    let mut decision: Option<bool> = None;

    egui::Window::new(&title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(&body);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(&cancel_label).clicked() {
                    decision = Some(false);
                }
                let confirm_btn = egui::Button::new(
                    egui::RichText::new(&confirm_label)
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(if danger {
                    egui::Color32::from_rgb(0xc0, 0x39, 0x2b)
                } else {
                    egui::Color32::from_rgb(0x3d, 0x59, 0x9c)
                });
                if ui.add(confirm_btn).clicked() {
                    decision = Some(true);
                }
            });
        });

    match decision {
        Some(true) => crate::state::apply_confirm(app, intent),
        Some(false) | None if !open => {}
        _ => {
            // Reinsert the modal — user hasn't decided yet.
            app.session.modal = Some(Modal::Confirm {
                title,
                body,
                confirm_label,
                cancel_label,
                danger,
                intent,
            });
        }
    }
}

fn dirty_close_dialog(
    ctx: &egui::Context,
    app: &mut AppState,
    path: String,
    tab_id: crate::tab::TabId,
) {
    let mut decision: Option<DirtyChoice> = None;
    let mut open = true;

    egui::Window::new("Unsaved changes")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} has unsaved changes. What would you like to do?",
                path
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision = Some(DirtyChoice::Cancel);
                }
                if ui.button("Discard & close").clicked() {
                    decision = Some(DirtyChoice::Discard);
                }
                let save_btn = egui::Button::new(
                    egui::RichText::new("Save & close").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0x3d, 0x59, 0x9c));
                if ui.add(save_btn).clicked() {
                    decision = Some(DirtyChoice::Save);
                }
            });
        });

    match decision {
        Some(DirtyChoice::Save) => {
            if let Err(err) = editor_pane::save_buffer(app, &path) {
                app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                app.session.modal = Some(Modal::DirtyClose { path, tab_id });
                return;
            }
            editor_pane::close_tab(app, tab_id);
        }
        Some(DirtyChoice::Discard) => {
            // Drop the buffer's in-memory edits by removing it; close_tab
            // would otherwise leave it (other tabs may reference it).
            if !app.session.tabs.iter().any(|t| {
                t.id != tab_id
                    && t.buffer_path() == Some(&path)
            }) {
                app.session.buffers.remove(&path);
            }
            editor_pane::close_tab(app, tab_id);
        }
        Some(DirtyChoice::Cancel) => {
            // User declined; drop the modal.
        }
        None if !open => {
            // X-closed counts as cancel.
        }
        None => {
            // No decision yet — keep the modal up.
            app.session.modal = Some(Modal::DirtyClose { path, tab_id });
        }
    }
}

enum DirtyChoice {
    Save,
    Discard,
    Cancel,
}

fn recovery_dialog(
    ctx: &egui::Context,
    app: &mut AppState,
    mut entries: Vec<hiker_core::autosave::RecoveredEntry>,
) {
    let mut bulk_decision: Option<BulkChoice> = None;
    let mut per_row: Vec<RowChoice> = Vec::new();

    egui::Window::new("Restore unsaved changes?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                "Hiker has autosaved copies of these buffers from a previous session. \
                 Restore brings the autosaved text back into the buffer; Discard drops it.",
            );
            ui.add_space(8.0);
            for (i, entry) in entries.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&entry.path).monospace());
                    if ui.button("Restore").clicked() {
                        per_row.push(RowChoice { idx: i, restore: true });
                    }
                    if ui.button("Discard").clicked() {
                        per_row.push(RowChoice { idx: i, restore: false });
                    }
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Restore all").clicked() {
                    bulk_decision = Some(BulkChoice::RestoreAll);
                }
                if ui.button("Discard all").clicked() {
                    bulk_decision = Some(BulkChoice::DiscardAll);
                }
                if ui.button("Decide later").clicked() {
                    bulk_decision = Some(BulkChoice::Defer);
                }
            });
        });

    if let Some(bulk) = bulk_decision {
        match bulk {
            BulkChoice::RestoreAll => {
                for entry in entries.drain(..) {
                    apply_restore(app, &entry);
                }
            }
            BulkChoice::DiscardAll => {
                for entry in entries.drain(..) {
                    apply_discard(app, &entry.path);
                }
            }
            BulkChoice::Defer => {
                // Leave the sidecars on disk; close the modal so the
                // user can keep working.
            }
        }
        return;
    }

    // Apply per-row decisions (in reverse index order so indexes stay
    // valid while we drain).
    per_row.sort_by_key(|c| std::cmp::Reverse(c.idx));
    for choice in per_row {
        if choice.idx >= entries.len() {
            continue;
        }
        let entry = entries.remove(choice.idx);
        if choice.restore {
            apply_restore(app, &entry);
        } else {
            apply_discard(app, &entry.path);
        }
    }

    if !entries.is_empty() {
        app.session.modal = Some(Modal::Recovery { entries });
    }
}

enum BulkChoice {
    RestoreAll,
    DiscardAll,
    Defer,
}

struct RowChoice {
    idx: usize,
    restore: bool,
}

fn apply_restore(app: &mut AppState, entry: &hiker_core::autosave::RecoveredEntry) {
    let text = match std::str::from_utf8(&entry.autosaved_content) {
        Ok(s) => s.to_string(),
        Err(_) => {
            app.push_toast(
                format!("Skipped non-UTF8 autosave: {}", entry.path),
                crate::state::ToastLevel::Warn,
            );
            return;
        }
    };
    // The loaded_hash is whatever's on disk *now* so the buffer is
    // immediately marked dirty (current_hash != loaded_hash) and the user
    // sees an unsaved buffer they can Save or Revert.
    let on_disk_hash = entry.on_disk_hash.clone().unwrap_or_default();
    let buffer = crate::buffer::Buffer::from_disk(
        entry.path.clone(),
        text,
        on_disk_hash,
    );
    app.session.buffers.insert(entry.path.clone(), buffer);
    crate::editor_pane::open_file(app, &entry.path, /* sticky */ true);
    let autosave = app.vault_session.services.autosave.clone();
    if let Err(err) = autosave.clear(&entry.path) {
        tracing::warn!(error = %err, "autosave clear after restore failed");
    }
}

fn apply_discard(app: &mut AppState, path: &str) {
    let autosave = app.vault_session.services.autosave.clone();
    if let Err(err) = autosave.discard(path) {
        app.push_toast(
            format!("Discard failed for {}: {}", path, err),
            crate::state::ToastLevel::Error,
        );
    }
}
