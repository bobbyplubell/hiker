//! Trash bin pinned at the bottom of the sidebar. Shows a collapsible
//! listing built from the on-disk trash directory + manifest. Each entry
//! offers Restore and Purge actions.

use std::sync::Arc;

use eframe::egui;

use hiker_core::trash::Trash;

use crate::icons;
use crate::state::{AppState, ToastLevel};
use crate::theme;

pub fn show(ui: &mut egui::Ui, state: &mut AppState, _rt: &Arc<tokio::runtime::Runtime>) {
    let trash = Trash::open(&state.vault_session.vault_root);
    let items = trash.list_from_disk().unwrap_or_default();
    let count = items.len();

    let label = if count == 0 {
        "Trash".to_string()
    } else {
        format!("Trash ({})", count)
    };
    let chevron_icon = if state.session.sidebar.trash_expanded {
        icons::expand()
    } else {
        icons::collapse()
    };

    let mut empty_clicked = false;
    let row = ui.horizontal(|ui| {
        let resp_chev = ui.add(egui::Button::image(chevron_icon).frame(false).small());
        let resp_trash = ui.add(egui::Button::image(icons::trash()).frame(false).small());
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
                            egui::RichText::new(format_ts(item.deleted_at))
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
                        ToastLevel::Info,
                    );
                }
                Err(err) => state.push_toast(
                    format!("Restore failed: {}", err),
                    ToastLevel::Error,
                ),
            }
        }
        Action::Purge { trashed_name } => {
            let trash = Trash::open(&state.vault_session.vault_root);
            match trash.permanent_delete(&trashed_name) {
                Ok(()) => state.push_toast(
                    format!("Purged {}", trashed_name),
                    ToastLevel::Info,
                ),
                Err(err) => state.push_toast(
                    format!("Purge failed: {}", err),
                    ToastLevel::Error,
                ),
            }
        }
    }
}

fn format_ts(unix_secs: i64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let Ok(t) = OffsetDateTime::from_unix_timestamp(unix_secs) else {
        return String::new();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
    t.format(fmt).unwrap_or_default()
}
