//! Trash — a sidebar `Activity` listing the vault's trashed items, each
//! with Restore / Purge actions, plus a clickable read-only preview.
//! Migrated off `panels_registry`'s `P_TRASH` to a real `Activity` whose
//! `View` renders through the narrow `activity::SurfaceCtx`: the
//! listing is read from disk via `ctx.vault.root()` and every mutation
//! (restore / purge / open-preview) is deferred with full `&mut
//! AppState` via `ctx.defer`. The batch "Empty trash" verb stays in the
//! workbench host's `side_bar_actions_menu` (which has full `&mut
//! AppState`), routing through `ConfirmIntent::EmptyTrash`.
//! status: feature-trash-panel

use eframe::egui;

use crate::editor_pane;
use egui_workbench::activity::{Activity, View};
use crate::activity::{AppCtx, SurfaceCtx};
use crate::icons;
use hiker_theme as theme;

/// Per-activity UI state for the Trash sidebar. The panel is effectively
/// stateless — the listing is read fresh from disk each frame — but the
/// registry's `AppCtx::session` hands every activity a `&mut dyn Any` state
/// slice, so a zero-field marker keeps the seam uniform. Owned by
/// `AppState::trash_state` (top-level, per `feature-state-ownership`).
#[derive(Default)]
pub struct State;

/// A user action collected during the render loop. Each is applied via
/// `ctx.defer` so the mutation runs with full `&mut AppState` after the
/// narrow session borrow is released.
enum Action {
    Restore { id: String },
    Purge { trashed_name: String },
    /// Open the trashed file as a read-only preview tab.
    Preview { trash_path: String, original_path: String },
}

/// Render the trash listing through the narrow activity `SurfaceCtx`. The
/// listing comes from `hiker_core::trash::Trash` opened on the vault
/// root; restore/purge/preview are deferred to `&mut AppState`.
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    use hiker_core::trash::Trash;
    let trash = Trash::open(ctx.vault.root());
    let items = trash.list_from_disk().unwrap_or_default();

    if items.is_empty() {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Trash is empty").color(theme::muted()).small());
        return;
    }

    let mut pending: Option<Action> = None;
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

    if let Some(action) = pending {
        defer_action(ctx, action);
    }
}

/// Queue the collected action onto `ctx.defer`, where it runs with full
/// `&mut AppState` (restore/purge mutate the vault + toasts + file tree;
/// preview opens an editor tab).
fn defer_action(ctx: &mut SurfaceCtx<'_>, action: Action) {
    ctx.defer(move |state| apply(state, action));
}

/// Apply a deferred trash action against full `&mut AppState`. Mirrors
/// the pre-migration `TrashView::apply` paths.
fn apply(state: &mut crate::state::AppState, action: Action) {
    use hiker_core::trash::Trash;
    if let Action::Preview { trash_path, original_path } = &action {
        editor_pane::open_trash_in_tab(state, trash_path, original_path);
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
                state.file_tree_state.invalidate_dir(parent);
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

// ---- Activity impl ----------------------------------------------------

/// Zero-sized `Activity` descriptor for the Trash panel. State lives in
/// `AppState::trash_state`; the surface reads the listing fresh from
/// disk and defers every mutation.
pub struct Trash;

impl Activity<dyn AppCtx> for Trash {
    fn id(&self) -> &'static str {
        "trash"
    }
    fn label(&self) -> &'static str {
        "Trash"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Trash)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&TrashSidebar]
    }
}

struct TrashSidebar;

impl View<dyn AppCtx> for TrashSidebar {
    fn id(&self) -> &'static str {
        "trash"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-trash-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}
