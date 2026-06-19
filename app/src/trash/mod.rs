//! Trash — a sidebar `Activity` listing the vault's trashed items, each
//! with a Restore button, a clickable read-only preview, and a row menu
//! carrying Preview / Restore / Purge. Purge destroys data, so it is
//! menu-only (`interaction.md` [destructive-verbs-in-menu]) and arms the
//! house confirm modal (`ConfirmIntent::PurgeTrashItem`) instead of
//! deleting directly. Migrated off `panels_registry`'s `P_TRASH` to a real
//! `Activity` whose `View` renders through the narrow
//! `activity::SurfaceCtx`: the listing is read from disk via
//! `ctx.vault.root()` and every mutation (restore / arm-purge-confirm /
//! open-preview) is deferred with full `&mut AppState` via `ctx.defer`.
//! The batch "Empty trash" verb stays in the workbench host's
//! `side_bar_actions_menu` (which has full `&mut AppState`), routing
//! through `ConfirmIntent::EmptyTrash`.
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Restore { id: String },
    /// Arm the house confirm modal for a permanent delete — destroying data
    /// always confirms (`interaction.md` [destructive-verbs-in-menu]).
    /// `label` is the display basename the confirm copy names.
    Purge { trashed_name: String, label: String },
    /// Open the trashed file as a read-only preview tab.
    Preview { trash_path: String, original_path: String },
}

/// Build a trash row's context menu (`interaction.md`
/// [rightclick-menu-always]): Preview / Restore, then the destructive Purge
/// in its own section. Restore greys out with the reason on an orphaned item
/// (no manifest record to restore from); its placeholder action is
/// unreachable while disabled. Purge arms the house confirm modal — it never
/// deletes directly ([destructive-verbs-in-menu]).
fn build_trash_row_menu(
    trash_dir: &std::path::Path,
    item: &hiker_core::trash::ListItem,
) -> egui_workbench::menu::Menu<Action> {
    use egui_workbench::menu::{Action as MenuAction, Enabled, Menu};
    let preview = Action::Preview {
        trash_path: trash_dir.join(&item.trashed_name).to_string_lossy().into_owned(),
        original_path: item
            .original_path
            .clone()
            .unwrap_or_else(|| item.trashed_name.clone()),
    };
    let restore = match &item.id {
        Some(id) => MenuAction::new("Restore", Action::Restore { id: id.clone() }),
        None => MenuAction::new("Restore", Action::Restore { id: String::new() })
            .enabled(Enabled::No("no manifest record to restore from".into())),
    };
    Menu::new()
        .action("Preview", preview)
        .action_with(restore)
        .section()
        .action(
            "Purge",
            Action::Purge {
                trashed_name: item.trashed_name.clone(),
                label: display_basename(item).to_string(),
            },
        )
}

/// The display basename for a trash item: the original path's final segment
/// when the manifest recorded one, else the on-disk trashed name.
fn display_basename(item: &hiker_core::trash::ListItem) -> &str {
    item.original_path
        .as_deref()
        .unwrap_or(&item.trashed_name)
        .rsplit('/')
        .next()
        .unwrap_or(&item.trashed_name)
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
        let basename = display_basename(item);
        let preview = Action::Preview {
            trash_path: trash
                .dir()
                .join(&item.trashed_name)
                .to_string_lossy()
                .into_owned(),
            original_path: item
                .original_path
                .clone()
                .unwrap_or_else(|| item.trashed_name.clone()),
        };
        // Reserve a background slot so the hover wash paints BEHIND the row's
        // content (the rect is only known after layout).
        let wash_slot = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal(|ui| {
            let name = ui
                .add(
                    egui::Label::new(egui::RichText::new(basename).small())
                        .sense(egui::Sense::click()),
                )
                .on_hover_text("Preview (read-only)");
            if name.clicked() {
                pending = Some(preview.clone());
            }
            ui.label(
                egui::RichText::new(format_ts(item.deleted_at))
                    .color(theme::muted())
                    .small(),
            );
            // Restore is non-destructive, so it may stay a row button; Purge
            // destroys data and is menu-only behind the confirm modal
            // (`interaction.md` [destructive-verbs-in-menu]).
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(id) = &item.id
                    && ui
                        .add(egui::Button::new(egui::RichText::new("Restore").small()).small())
                        .clicked()
                {
                    pending = Some(Action::Restore { id: id.clone() });
                }
            });
        });
        // The shared "click acts here" signal on the whole row — hover wash +
        // pointer (`interaction.md` [hover-open-signal]) — and the row click
        // opens the preview like the name label does.
        let row_resp = row.response.interact(egui::Sense::click());
        if let Some(c) = theme::open_signal_wash(false, row_resp.hovered()) {
            ui.painter()
                .set(wash_slot, egui::Shape::rect_filled(row_resp.rect, 2.0, c));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if row_resp.clicked() {
            pending = Some(preview);
        }
        // Right-click anywhere on the row → its context menu (`interaction.md`
        // [rightclick-menu-always]).
        let mut chosen = None;
        row_resp.context_menu(|ui| {
            chosen = egui_workbench::menu::show(ui, build_trash_row_menu(trash.dir(), item));
        });
        if chosen.is_some() {
            pending = chosen;
        }
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
/// the pre-migration `TrashView::apply` paths; Purge only ARMS the house
/// confirm modal — the actual delete runs in
/// `AppState::apply_confirm(ConfirmIntent::PurgeTrashItem)` once confirmed.
fn apply(state: &mut crate::state::AppState, action: Action) {
    use hiker_core::trash::Trash;
    if let Action::Preview { trash_path, original_path } = &action {
        editor_pane::open_trash_in_tab(state, trash_path, original_path);
        return;
    }
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
                    let parent =
                        entry.original_path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    state.file_tree_state.invalidate_dir(parent);
                    state.push_toast(
                        format!("Restored {}", entry.original_path),
                        crate::state::ToastLevel::Info,
                    );
                }
                Err(err) => state.push_toast(
                    format!("Restore failed: {}", err),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        Action::Purge { trashed_name, label } => {
            state.session.modal = Some(crate::state::Modal::Confirm {
                title: "Purge item".to_string(),
                body: format!("Permanently delete \u{201c}{label}\u{201d}? This can't be undone."),
                confirm_label: "Purge".to_string(),
                cancel_label: "Cancel".to_string(),
                danger: true,
                intent: crate::state::ConfirmIntent::PurgeTrashItem { trashed_name },
            });
        }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use egui_workbench::menu::{Enabled, Entry};
    use hiker_core::trash::{Kind, ListItem};

    use super::{build_trash_row_menu, Action};

    fn item(id: Option<&str>) -> ListItem {
        ListItem {
            id: id.map(str::to_string),
            trashed_name: "2026-06-12T10-00-00_a.md".to_string(),
            original_path: Some("notes/a.md".to_string()),
            deleted_at: 0,
            kind: Kind::File,
            member_count: None,
            orphaned: id.is_none(),
            doc_id: None,
        }
    }

    /// Menu composition: Preview / Restore, then Purge alone in the
    /// destructive section. Restore greys out (with the reason) on an
    /// orphaned item instead of disappearing — the menu teaches why.
    #[test]
    fn trash_row_menu_offers_preview_restore_and_sectioned_purge() {
        let menu = build_trash_row_menu(Path::new("/v/.hiker/trash"), &item(Some("id-1")));
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "safe verbs + destructive section");
        let Entry::Action { label, action, enabled, .. } = &sections[0][0] else {
            panic!("expected Preview action");
        };
        assert_eq!(label, "Preview");
        assert!(enabled.is_enabled());
        assert_eq!(
            *action,
            Action::Preview {
                trash_path: "/v/.hiker/trash/2026-06-12T10-00-00_a.md".to_string(),
                original_path: "notes/a.md".to_string(),
            }
        );
        let Entry::Action { label, action, enabled, .. } = &sections[0][1] else {
            panic!("expected Restore action");
        };
        assert_eq!(label, "Restore");
        assert!(enabled.is_enabled());
        assert_eq!(*action, Action::Restore { id: "id-1".to_string() });
        let Entry::Action { label, action, .. } = &sections[1][0] else {
            panic!("expected Purge action");
        };
        assert_eq!(label, "Purge");
        assert_eq!(sections[1].len(), 1);
        // Purge arms the confirm with the on-disk name to delete AND the
        // display basename the confirm copy shows.
        assert_eq!(
            *action,
            Action::Purge {
                trashed_name: "2026-06-12T10-00-00_a.md".to_string(),
                label: "a.md".to_string(),
            }
        );

        // Orphan: Restore stays listed but greys out with the reason.
        let menu = build_trash_row_menu(Path::new("/v/.hiker/trash"), &item(None));
        let Entry::Action { enabled, .. } = &menu.sections()[0][1] else {
            panic!("expected Restore action");
        };
        assert!(matches!(enabled, Enabled::No(_)), "orphan → greyed Restore");
    }
}
