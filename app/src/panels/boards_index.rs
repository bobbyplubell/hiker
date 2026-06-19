//! Boards index page: a singleton meta-surface listing every board-doc in
//! the vault.
//!
//! Boards are per-doc (each opens in its own `board`-kind tab), so there is
//! no single home tab for them — this page is that home. It enumerates every
//! board via `core::boards::list` (the same listing `boards_list` exposes to
//! MCP), including empty boards, and shows each row's title + column / card
//! counts. A row click opens that board in its board view; a **New board**
//! action runs the create op; the row's context menu carries Open / Rename /
//! Delete — Delete moves the board-doc to trash behind the shared confirm
//! modal (`interaction.md` [destructive-verbs-in-menu]: destroy verbs are
//! menu-only, never a bare row button).
//!
//! See `docs/kanban.md` §"Boards index page" / §"Deleting a board".
//
// status: board-index-page

use eframe::egui;

use crate::state::{AppState, Modal};
use hiker_theme as theme;
use hiker_core::boards::BoardListItem;

/// A row action requested this frame, applied after the list render so the
/// borrow on the gathered listing is released first.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowAction {
    Open(String),
    /// Open the board with its title inline-rename active (the same
    /// machinery `new_board` uses via `board::open_for_rename`).
    Rename(String),
    Delete(String),
}

/// Build a board row's context menu (`interaction.md`
/// [rightclick-menu-always]): Open / Rename, then the destructive Delete in
/// its own section. Delete routes through the shared confirm-delete modal —
/// the menu entry arms the confirm, it never trashes directly.
fn build_board_row_menu(rel: &str) -> egui_workbench::menu::Menu<RowAction> {
    egui_workbench::menu::Menu::new()
        .action("Open board", RowAction::Open(rel.to_string()))
        .action("Rename board", RowAction::Rename(rel.to_string()))
        .section()
        .action("Delete board", RowAction::Delete(rel.to_string()))
}

/// Render the Boards index page. status: board-index-page
pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.horizontal(|ui| {
        ui.heading("Boards");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The same create op the sidebar `+` uses (`board-create`); a
            // fresh board opens in its board view with inline-rename active.
            if ui.button("+ New board").clicked() {
                app.new_board();
            }
        });
    });
    ui.separator();

    let boards = gather_boards(app);
    if boards.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("No boards yet. Create one with “New board”.")
                .color(theme::muted()),
        );
        return;
    }

    let mut action: Option<RowAction> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for item in &boards {
            render_row(ui, item, &mut action);
        }
    });

    match action {
        Some(RowAction::Open(rel)) => {
            crate::panels::board::open(app, &rel);
        }
        Some(RowAction::Rename(rel)) => {
            // The board tab's title inline-rename (commit on Enter/focus
            // loss, Esc cancels) — the same flow a fresh board opens with.
            crate::panels::board::open_for_rename(app, &rel);
        }
        Some(RowAction::Delete(rel)) => {
            // Confirm before trashing — the layout is discarded (though trash
            // makes it recoverable). Reuses the shared delete-confirm modal;
            // the confirm callback routes through `core::ops::delete`.
            // status: board-delete
            app.session.modal = Some(Modal::ConfirmDelete { path: rel });
        }
        None => {}
    }
}

/// Read every board-doc + its column / card counts via `core::boards::list`
/// (includes empty boards). Returns an empty vec when the index store is
/// unavailable or the listing fails — the page then renders its empty state.
fn gather_boards(app: &AppState) -> Vec<BoardListItem> {
    let Ok(store) = app.vault_session.services.read_store.lock() else {
        return Vec::new();
    };
    hiker_core::boards::list(
        &app.vault_session.vault,
        &store,
        &app.vault_session.services.layered,
        Some(app.vault_session.services.kinds.as_ref()),
    )
    .unwrap_or_default()
}

/// One board row: title (a click-to-open link) + column / card counts. The
/// row's verbs (Open / Rename / Delete-with-confirm) live in its right-click
/// menu.
fn render_row(ui: &mut egui::Ui, item: &BoardListItem, action: &mut Option<RowAction>) {
    let row = egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6));
    let response = row
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.link(egui::RichText::new(&item.title).strong()).clicked() {
                    *action = Some(RowAction::Open(item.rel_path.clone()));
                }
                ui.label(
                    egui::RichText::new(format!(
                        "{} column{} · {} card{}",
                        item.column_count,
                        if item.column_count == 1 { "" } else { "s" },
                        item.card_count,
                        if item.card_count == 1 { "" } else { "s" },
                    ))
                    .small()
                    .color(theme::muted()),
                );
            });
        })
        .response;
    // Right-click anywhere on the row → its context menu (`interaction.md`
    // [rightclick-menu-always]).
    let mut chosen = None;
    response.interact(egui::Sense::click()).context_menu(|ui| {
        chosen = egui_workbench::menu::show(ui, build_board_row_menu(&item.rel_path));
    });
    if chosen.is_some() {
        *action = chosen;
    }
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use egui_workbench::menu::Entry;

    use super::{build_board_row_menu, RowAction};

    /// Menu composition: Open / Rename, then Delete alone in the destructive
    /// section (and Delete arms the confirm — it carries the rel-path the
    /// modal needs, never a direct trash).
    #[test]
    fn board_row_menu_offers_open_rename_and_sectioned_delete() {
        let menu = build_board_row_menu("boards/b.md");
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "verbs section + destructive section");
        let label_action = |e: &Entry<RowAction>| match e {
            Entry::Action { label, action, .. } => (label.to_string(), action.clone()),
            _ => panic!("expected an Action entry"),
        };
        assert_eq!(
            label_action(&sections[0][0]),
            ("Open board".to_string(), RowAction::Open("boards/b.md".to_string()))
        );
        assert_eq!(
            label_action(&sections[0][1]),
            ("Rename board".to_string(), RowAction::Rename("boards/b.md".to_string()))
        );
        assert_eq!(sections[0].len(), 2);
        assert_eq!(
            label_action(&sections[1][0]),
            ("Delete board".to_string(), RowAction::Delete("boards/b.md".to_string()))
        );
        assert_eq!(sections[1].len(), 1);
    }
}
