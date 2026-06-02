//! Boards index page: a singleton meta-surface listing every board-doc in
//! the vault.
//!
//! Boards are per-doc (each opens in its own `board`-kind tab), so there is
//! no single home tab for them — this page is that home. It enumerates every
//! board via `core::boards::list` (the same listing `boards_list` exposes to
//! MCP), including empty boards, and shows each row's title + column / card
//! counts. A row click opens that board in its board view; a **New board**
//! action runs the create op; a per-row **Delete** moves the board-doc to
//! trash (confirm-guarded) via `core::ops::delete`.
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
enum RowAction {
    Open(String),
    Delete(String),
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
        &app.vault_session.services.oplog,
    )
    .unwrap_or_default()
}

/// One board row: title (a click-to-open link) + column / card counts + a
/// Delete button.
fn render_row(ui: &mut egui::Ui, item: &BoardListItem, action: &mut Option<RowAction>) {
    let row = egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6));
    row.show(ui, |ui| {
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button(egui::RichText::new("Delete").color(error_color()))
                    .on_hover_text("Move this board to trash")
                    .clicked()
                {
                    *action = Some(RowAction::Delete(item.rel_path.clone()));
                }
            });
        });
    });
    ui.add_space(4.0);
}

/// Error / danger accent (the theme has no dedicated error token).
const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}
