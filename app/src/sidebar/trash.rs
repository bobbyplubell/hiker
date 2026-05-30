//! Trash panel — a standalone dockable sidebar surface listing the
//! vault's trashed items, each with Restore / Purge actions. The batch
//! "Empty trash" verb lives in the panel's header right-click menu
//! (`Host::side_bar_actions_menu` for `HikerMode::Trash`). The workbench
//! accordion provides the section header + collapse, so the body renders
//! the listing directly. [feature-trash-panel]

use eframe::egui;

use crate::state::AppState;
use crate::theme;

/// Per-frame render context for the Trash panel.
pub(crate) struct TrashView<'a> {
    pub(crate) ui: &'a mut egui::Ui,
    pub(crate) state: &'a mut AppState,
}

/// Deferred action collected during the render loop and applied after,
/// to avoid a mutable-borrow overlap with `state`.
enum Action {
    Restore { id: String },
    Purge { trashed_name: String },
    /// Open the trashed file as a read-only preview.
    Preview { trash_path: String, original_path: String },
}

impl TrashView<'_> {
    pub(crate) fn show(&mut self) {
        use hiker_core::trash::Trash;
        let ui = &mut *self.ui;
        let state = &mut *self.state;
        let trash = Trash::open(&state.vault_session.vault_root);
        let items = trash.list_from_disk().unwrap_or_default();

        if items.is_empty() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Trash is empty").color(theme::muted()).small());
            return;
        }

        let mut pending: Option<Action> = None;
        egui::ScrollArea::vertical()
            .id_salt("panel-trash-body")
            .auto_shrink([false, false])
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
                        let name = ui
                            .add(
                                egui::Label::new(egui::RichText::new(basename).small())
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("Preview (read-only)");
                        if name.clicked() {
                            pending = Some(Action::Preview {
                                trash_path: trash
                                    .dir()
                                    .join(&item.trashed_name)
                                    .to_string_lossy()
                                    .into_owned(),
                                original_path: item
                                    .original_path
                                    .clone()
                                    .unwrap_or_else(|| item.trashed_name.clone()),
                            });
                        }
                        ui.label(
                            egui::RichText::new(format_ts(item.deleted_at))
                                .color(theme::muted())
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(egui::Button::new(egui::RichText::new("Purge").small()).small())
                                .on_hover_text("Delete forever")
                                .clicked()
                            {
                                pending = Some(Action::Purge {
                                    trashed_name: item.trashed_name.clone(),
                                });
                            }
                            if let Some(id) = &item.id
                                && ui
                                    .add(egui::Button::new(egui::RichText::new("Restore").small()).small())
                                    .clicked()
                            {
                                pending = Some(Action::Restore { id: id.clone() });
                            }
                        });
                    });
                }
            });

        if let Some(action) = pending {
            self.apply(action);
        }
    }

    fn apply(&mut self, action: Action) {
        use hiker_core::trash::Trash;
        let state = &mut *self.state;
        if let Action::Preview { trash_path, original_path } = &action {
            crate::editor_pane::open_trash_in_tab(state, trash_path, original_path);
            return;
        }
        let trash = Trash::open(&state.vault_session.vault_root);
        match action {
            Action::Restore { id } => match hiker_core::vault::restore_note(
                &state.vault_session.vault,
                Some(state.vault_session.services.watcher.as_ref()),
                &trash,
                &id,
            ) {
                Ok(entry) => {
                    let parent =
                        entry.original_path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    state.session.file_tree.dir_cache.remove(parent);
                    state.push_toast(
                        format!("Restored {}", entry.original_path),
                        crate::state::ToastLevel::Info,
                    );
                }
                Err(err) => state
                    .push_toast(format!("Restore failed: {}", err), crate::state::ToastLevel::Error),
            },
            Action::Purge { trashed_name } => match trash.permanent_delete(&trashed_name) {
                Ok(()) => state
                    .push_toast(format!("Purged {}", trashed_name), crate::state::ToastLevel::Info),
                Err(err) => state
                    .push_toast(format!("Purge failed: {}", err), crate::state::ToastLevel::Error),
            },
            // Handled by the early return above.
            Action::Preview { .. } => unreachable!(),
        }
    }
}

/// Format a trash entry's unix-seconds timestamp as `YYYY-MM-DD HH:MM`.
fn format_ts(unix_secs: i64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let Ok(t) = OffsetDateTime::from_unix_timestamp(unix_secs) else {
        return String::new();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
    t.format(fmt).unwrap_or_default()
}
