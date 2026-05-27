//! Board tab: a per-doc kanban view over a curated board-doc.
//!
//! Renders the board-doc's columns side by side; each card shows the
//! referenced note's title and opens that note in the editor pane on click.
//! Card moves / removals and column management go through the `core::board`
//! ops (op-log user-save path) — the board-doc on disk is the single source
//! of truth, re-read each frame via `core::board::get_board`.
//!
//! Implements: board-view, board-view-toggle, board-move, board-remove-card,
//! board-column-management. The "Add to board…" verb and create flow live in
//! the file tree / sidebar; the file-tree glyph in `sidebar::files`.
//
// status: board-view

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;
use crate::theme;
use hiker_core::boards::{BoardDetail, ResolvedCard, ResolvedColumn};
use hiker_core::trails::ops::ResolutionOutcome;

/// Error / broken-reference accent (the theme has no dedicated error token).
const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}

/// Which render the board pane shows. The toggle is a render choice over the
/// one underlying op-log document, not two tabs — switching to `Markdown`
/// hosts the live editor widget over the board-doc inline (mirroring the
/// cluster editor's view menu), so frontmatter / body edits and column moves
/// ride the same document. status: board-view-toggle
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum BoardView {
    #[default]
    Board,
    Markdown,
}

/// Per-board-tab local UI state.
#[derive(Default)]
pub struct Pane {
    /// Active in-pane render (Board columns vs. the inline markdown editor).
    ///
    /// status: board-view-toggle
    pub view: BoardView,
    /// Inline-rename draft for a column header: `(old_name, draft_text)`.
    pub renaming_column: Option<(String, String)>,
    /// Pending column-delete confirm: the column name awaiting confirmation
    /// (set when the column still holds cards).
    pub confirm_delete_column: Option<String>,
    /// Inline-rename draft for the board's title (basename, no `.md`). Set
    /// when the user double-clicks the title or right after `new_board`, so a
    /// freshly created board opens with its name editable.
    ///
    /// status: board-create
    pub renaming_title: Option<String>,
    /// Set the frame after `renaming_title` is seeded so the title field
    /// grabs focus once (and selects its text) without stealing focus every
    /// frame.
    pub title_rename_focus: bool,
}

/// Find-or-focus a board tab for `path`, opening one if none exists.
/// Returns the tab id so callers (e.g. `new_board`) can seed per-tab pane
/// state like an active inline-rename.
///
/// status: board-view
pub fn open(app: &mut AppState, path: &str) -> TabId {
    use crate::tab::{Tab, TabKind};
    if let Some(existing) = app
        .session
        .tabs
        .iter()
        .find(|t| matches!(&t.kind, TabKind::Board { path: p } if p == path))
    {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::Board { path: path.to_string() },
        sticky: true,
    });
    app.session.active_tab = Some(id);
    id
}

/// Open `path` in a board tab and immediately enter title inline-rename,
/// seeded with the current basename. Used by `new_board` so a freshly
/// created board opens with its name editable (mirrors the new-trail /
/// new-file gesture).
///
/// status: board-create
pub fn open_for_rename(app: &mut AppState, path: &str) {
    let tab_id = open(app, path);
    let title = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .strip_suffix(".md")
        .unwrap_or(path)
        .to_string();
    let pane = app.panels.boards.entry(tab_id).or_default();
    pane.renaming_title = Some(title);
    pane.title_rename_focus = true;
}

/// A mutation the user requested this frame, applied after rendering so the
/// borrow on the resolved board is released first.
enum BoardAction {
    OpenNote(String),
    MoveCard { from: String, card_id: String, to: String, to_index: usize },
    RemoveCard(String),
    /// A note (by rel-path) dropped onto a column from the file tree → add it
    /// as a card. status: board-dnd
    AddCardFromFile { column: String, source_rel: String },
    AddColumn,
    RenameColumn { old: String, new: String },
    ReorderColumn { name: String, to: usize },
    DeleteColumn(String),
    RenameBoard { new_title: String },
    /// Surface the shared Keep mine / Repoint / Break modal for a card whose
    /// reference is a `PathConflict`. status: board-card-references
    OpenPathConflict {
        card_id: String,
        path: String,
        recorded_id: String,
        current_path_id: String,
    },
    /// Set the WIP limit on a column (`None` clears it).
    /// status: board-wip-limits
    SetWipLimit { name: String, limit: Option<usize> },
}

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    path: &str,
    rt: &std::sync::Arc<tokio::runtime::Runtime>,
) {
    // Resolve the board fresh each frame from disk + index.
    let detail = {
        let store = match app.vault_session.services.read_store.lock() {
            Ok(s) => s,
            Err(_) => {
                ui.colored_label(error_color(), "index store unavailable");
                return;
            }
        };
        hiker_core::boards::get_board(&app.vault_session.vault, &store, path)
    };
    let detail = match detail {
        Ok(d) => d,
        Err(e) => {
            ui.colored_label(error_color(), format!("board error: {e}"));
            return;
        }
    };

    let mut action: Option<BoardAction> = None;
    render_header(ui, app, tab_id, &detail, &mut action);
    ui.separator();

    let view = app
        .panels
        .boards
        .get(&tab_id)
        .map(|p| p.view)
        .unwrap_or_default();
    match view {
        BoardView::Board => render_columns(ui, app, tab_id, &detail, &mut action),
        BoardView::Markdown => {
            // Host the live editor widget over the board-doc inline, in this
            // same tab — a render choice over the one op-log document, not a
            // separate buffer tab. status: board-view-toggle
            if crate::editor_pane::ensure_vault_buffer_loaded(app, path) {
                crate::panels::buffer::show(ui, app, path, rt);
            }
        }
    }

    if let Some(a) = action {
        apply_action(app, tab_id, path, a);
    }
}

/// Header: title (double-click or new-board → inline rename) + "View as:
/// Board / Markdown" toggle (mirrors the cluster editor's view menu) + an
/// "Add column" affordance.
///
/// status: board-view-toggle
/// status: board-column-management
/// status: board-create
fn render_header(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    detail: &BoardDetail,
    action: &mut Option<BoardAction>,
) {
    let title = detail
        .rel_path
        .rsplit('/')
        .next()
        .unwrap_or(&detail.rel_path)
        .strip_suffix(".md")
        .unwrap_or(&detail.rel_path)
        .to_string();
    ui.horizontal(|ui| {
        render_title(ui, app, tab_id, &title, action);
        ui.separator();
        ui.label(egui::RichText::new("View as:").small().color(theme::muted()));
        // The toggle flips this tab's in-pane render between the column
        // view and the inline markdown editor over the same board-doc —
        // one op-log document, not two tabs. status: board-view-toggle
        let current = app
            .panels
            .boards
            .get(&tab_id)
            .map(|p| p.view)
            .unwrap_or_default();
        if ui.selectable_label(current == BoardView::Board, "Board").clicked() {
            app.panels.boards.entry(tab_id).or_default().view = BoardView::Board;
        }
        if ui.selectable_label(current == BoardView::Markdown, "Markdown").clicked() {
            app.panels.boards.entry(tab_id).or_default().view = BoardView::Markdown;
        }
        // Column management only applies in the Board render.
        if current == BoardView::Board {
            ui.separator();
            if ui.button("+ Column").clicked() {
                *action = Some(BoardAction::AddColumn);
            }
        }
    });
    if !detail.body.trim().is_empty() {
        ui.label(
            egui::RichText::new(detail.body.lines().next().unwrap_or("").trim())
                .small()
                .color(theme::muted()),
        );
    }
}

/// The board title: a heading (double-click → inline rename) or, when a
/// rename is active, a text field that commits on Enter / focus-loss and
/// cancels on Esc. A new board opens with this already active (`new_board`
/// → `open_for_rename`).
///
/// status: board-create
fn render_title(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    title: &str,
    action: &mut Option<BoardAction>,
) {
    let draft = app
        .panels
        .boards
        .get(&tab_id)
        .and_then(|p| p.renaming_title.clone());
    let Some(draft) = draft else {
        let resp = ui.heading(title);
        if resp
            .interact(egui::Sense::click())
            .double_clicked()
        {
            let pane = app.panels.boards.entry(tab_id).or_default();
            pane.renaming_title = Some(title.to_string());
            pane.title_rename_focus = true;
        }
        return;
    };

    let mut buf = draft;
    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .desired_width(220.0)
            .hint_text("board name"),
    );
    // One-shot focus grab on the first frame after entering rename mode.
    let take_focus = app
        .panels
        .boards
        .get(&tab_id)
        .map(|p| p.title_rename_focus)
        .unwrap_or(false);
    if take_focus {
        resp.request_focus();
        if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
            pane.title_rename_focus = false;
        }
    }
    if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
        if let Some(d) = pane.renaming_title.as_mut() {
            *d = buf.clone();
        }
    }
    let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if commit {
        let trimmed = buf.trim().to_string();
        if !trimmed.is_empty() && trimmed != title {
            *action = Some(BoardAction::RenameBoard { new_title: trimmed });
        }
        if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
            pane.renaming_title = None;
        }
    } else if cancel {
        if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
            pane.renaming_title = None;
        }
    }
}

fn render_columns(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    detail: &BoardDetail,
    action: &mut Option<BoardAction>,
) {
    let column_names: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
    egui::ScrollArea::horizontal().show(ui, |ui| {
        ui.horizontal_top(|ui| {
            for (idx, col) in detail.columns.iter().enumerate() {
                render_column(ui, app, tab_id, col, idx, &column_names, action);
            }
        });
    });
}

/// A card drag payload: the card's id + its source column, so a column drop
/// zone can issue a precise `move_card`. A distinct type from the file-tree
/// `String` rel-path payload so one drop zone can tell a card-move from an
/// add-from-file. status: board-dnd
#[derive(Clone)]
struct CardDrag {
    card_id: String,
    from_column: String,
}

fn render_column(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    col: &ResolvedColumn,
    col_index: usize,
    column_names: &[String],
    action: &mut Option<BoardAction>,
) {
    let col_frame = egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::same(8));
    // The whole column is a drop zone: a `CardDrag` payload moves a card here
    // (appended to the tail), a `String` rel-path payload (from the file tree)
    // adds that note as a card. status: board-dnd
    let (_, card_drop) = ui.dnd_drop_zone::<CardDrag, _>(col_frame, |ui| {
        ui.set_width(230.0);
        render_column_header(ui, app, tab_id, col, col_index, column_names, action);
        ui.add_space(4.0);
        for (card_index, card) in col.cards.iter().enumerate() {
            render_card(ui, &col.name, card, card_index, column_names, action);
        }
        // Filler so an empty / short column still presents a droppable body.
        if col.cards.is_empty() {
            ui.add_space(24.0);
        }
    });
    // Card move dropped onto the column body → append to the tail. (A
    // card-level drop computes a precise index; see `render_card`.)
    if let Some(drag) = card_drop {
        *action = Some(BoardAction::MoveCard {
            from: drag.from_column.clone(),
            card_id: drag.card_id.clone(),
            to: col.name.clone(),
            to_index: usize::MAX,
        });
    }
    // A file-tree rel-path dropped onto the column → add-card.
    let col_resp = ui.interact(
        ui.min_rect(),
        ui.id().with(("board-col-file-drop", col_index)),
        egui::Sense::hover(),
    );
    if let Some(src) = col_resp.dnd_release_payload::<String>() {
        *action = Some(BoardAction::AddCardFromFile {
            column: col.name.clone(),
            source_rel: (*src).clone(),
        });
    }
}

/// Column header: name + count, with an inline-rename text field when this
/// column is being renamed, plus a `⋯` menu for rename / reorder / delete.
fn render_column_header(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    col: &ResolvedColumn,
    col_index: usize,
    column_names: &[String],
    action: &mut Option<BoardAction>,
) {
    let renaming = app
        .panels
        .boards
        .get(&tab_id)
        .and_then(|p| p.renaming_column.as_ref())
        .filter(|(old, _)| old == &col.name)
        .map(|(_, draft)| draft.clone());

    ui.horizontal(|ui| {
        if let Some(draft) = renaming {
            let mut buf = draft.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .desired_width(150.0)
                    .hint_text("column name"),
            );
            let pane = app.panels.boards.entry(tab_id).or_default();
            if let Some((_, d)) = pane.renaming_column.as_mut() {
                *d = buf.clone();
            }
            let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if commit {
                let trimmed = buf.trim().to_string();
                if !trimmed.is_empty() && trimmed != col.name {
                    *action = Some(BoardAction::RenameColumn {
                        old: col.name.clone(),
                        new: trimmed,
                    });
                }
                pane.renaming_column = None;
            } else if cancel {
                pane.renaming_column = None;
            }
        } else {
            // Count vs. WIP limit: "Doing (3/3)" with overflow flagged red.
            // status: board-wip-limits
            let count = col.cards.len();
            let (count_text, over) = match col.wip_limit {
                Some(limit) => (format!("{} ({}/{})", col.name, count, limit), count > limit),
                None => (format!("{} ({})", col.name, count), false),
            };
            let mut rich = egui::RichText::new(count_text).strong();
            if over {
                rich = rich.color(error_color());
            }
            ui.label(rich);
            if over {
                ui.label(
                    egui::RichText::new("over limit")
                        .small()
                        .color(error_color()),
                )
                .on_hover_text("This column exceeds its WIP limit");
            }
            ui.menu_button("⋯", |ui| {
                render_wip_limit_menu(ui, col, action);
                if ui.button("Rename column").clicked() {
                    app.panels
                        .boards
                        .entry(tab_id)
                        .or_default()
                        .renaming_column = Some((col.name.clone(), col.name.clone()));
                    ui.close();
                }
                if col_index > 0 && ui.button("Move left").clicked() {
                    *action = Some(BoardAction::ReorderColumn {
                        name: col.name.clone(),
                        to: col_index - 1,
                    });
                    ui.close();
                }
                if col_index + 1 < column_names.len() && ui.button("Move right").clicked() {
                    *action = Some(BoardAction::ReorderColumn {
                        name: col.name.clone(),
                        to: col_index + 1,
                    });
                    ui.close();
                }
                if ui.button("Delete column").clicked() {
                    if col.cards.is_empty() {
                        *action = Some(BoardAction::DeleteColumn(col.name.clone()));
                    } else {
                        // Delete-with-cards prompts first.
                        app.panels
                            .boards
                            .entry(tab_id)
                            .or_default()
                            .confirm_delete_column = Some(col.name.clone());
                    }
                    ui.close();
                }
            });
        }
    });

    // Inline confirm row for delete-with-cards.
    let pending_delete = app
        .panels
        .boards
        .get(&tab_id)
        .and_then(|p| p.confirm_delete_column.clone())
        .filter(|n| n == &col.name);
    if let Some(name) = pending_delete {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Delete \"{name}\" + {} card(s)?", col.cards.len()))
                    .small()
                    .color(error_color()),
            );
            if ui.small_button("Delete").clicked() {
                *action = Some(BoardAction::DeleteColumn(name.clone()));
                app.panels.boards.entry(tab_id).or_default().confirm_delete_column = None;
            }
            if ui.small_button("Cancel").clicked() {
                app.panels.boards.entry(tab_id).or_default().confirm_delete_column = None;
            }
        });
    }
}

/// Column-menu submenu to set or clear the WIP limit. Presets 1..6 cover the
/// common cases; "No limit" clears it. Overflow is flagged (soft) rather than
/// blocking the move. status: board-wip-limits
fn render_wip_limit_menu(
    ui: &mut egui::Ui,
    col: &ResolvedColumn,
    action: &mut Option<BoardAction>,
) {
    ui.menu_button("WIP limit", |ui| {
        let none_label = if col.wip_limit.is_none() { "* No limit" } else { "No limit" };
        if ui.button(none_label).clicked() {
            *action = Some(BoardAction::SetWipLimit { name: col.name.clone(), limit: None });
            ui.close();
        }
        for n in 1..=6usize {
            let label = if col.wip_limit == Some(n) {
                format!("* {n}")
            } else {
                n.to_string()
            };
            if ui.button(label).clicked() {
                *action = Some(BoardAction::SetWipLimit {
                    name: col.name.clone(),
                    limit: Some(n),
                });
                ui.close();
            }
        }
    });
}

fn render_card(
    ui: &mut egui::Ui,
    column_name: &str,
    card: &ResolvedCard,
    card_index: usize,
    column_names: &[String],
    action: &mut Option<BoardAction>,
) {
    let card_frame = egui::Frame::default()
        .fill(theme::hover_bg())
        .inner_margin(egui::Margin::symmetric(6, 4));
    let drag_id = ui.make_persistent_id(("board-card", column_name, &card.card_ref.id, card_index));
    // The card is a drag source carrying its id + source column; the card is
    // also a drop zone so a card dropped here inserts *before* it (precise
    // index). status: board-dnd
    let (_, dropped_before) = ui.dnd_drop_zone::<CardDrag, _>(card_frame, |ui| {
        ui.dnd_drag_source(
            drag_id,
            CardDrag {
                card_id: card.card_ref.id.clone(),
                from_column: column_name.to_string(),
            },
            |ui| render_card_body(ui, column_name, card, column_names, action),
        );
    });
    if let Some(drag) = dropped_before {
        // Insert the dragged card at this card's position in this column.
        *action = Some(BoardAction::MoveCard {
            from: drag.from_column.clone(),
            card_id: drag.card_id.clone(),
            to: column_name.to_string(),
            to_index: card_index,
        });
    }
    ui.add_space(4.0);
}

/// The inner contents of a card (title row + per-card verbs), rendered inside
/// the drag source. Split out so the drag-source closure stays small.
fn render_card_body(
    ui: &mut egui::Ui,
    column_name: &str,
    card: &ResolvedCard,
    column_names: &[String],
    action: &mut Option<BoardAction>,
) {
    // Distinguish a `PathConflict` (actionable — the path now resolves to a
    // different note, repointable) from an `Orphan` (neither half resolves —
    // greyed, non-actionable). status: board-card-references
    let path_conflict = matches!(card.resolution, ResolutionOutcome::PathConflict { .. });
    let orphan = matches!(card.resolution, ResolutionOutcome::Orphan);
    ui.horizontal(|ui| {
        if orphan {
            ui.label(egui::RichText::new(&card.title).color(theme::muted()));
            ui.label(
                egui::RichText::new("broken reference")
                    .small()
                    .color(error_color()),
            );
        } else if path_conflict {
            ui.label(egui::RichText::new(&card.title).color(theme::muted()));
            if ui
                .small_button(egui::RichText::new("conflict — resolve…").small().color(error_color()))
                .clicked()
                && let ResolutionOutcome::PathConflict { recorded_id, current_path_id, path } =
                    &card.resolution
            {
                *action = Some(BoardAction::OpenPathConflict {
                    card_id: card.card_ref.id.clone(),
                    path: path.clone(),
                    recorded_id: recorded_id.clone(),
                    current_path_id: current_path_id.clone(),
                });
            }
        } else if ui.link(&card.title).clicked() {
            let target = match &card.resolution {
                ResolutionOutcome::Resolved { rel_path, .. } => rel_path.clone(),
                ResolutionOutcome::SelfHeal { canonical_path, .. } => canonical_path.clone(),
                _ => card.card_ref.path.clone(),
            };
            *action = Some(BoardAction::OpenNote(target));
        }
    });
    ui.horizontal(|ui| {
        // Per-card "Move to >" menu is kept as a fallback to drag-and-drop.
        ui.menu_button("Move to >", |ui| {
            for target in column_names {
                if target != column_name && ui.button(target).clicked() {
                    *action = Some(BoardAction::MoveCard {
                        from: column_name.to_string(),
                        card_id: card.card_ref.id.clone(),
                        to: target.clone(),
                        to_index: usize::MAX,
                    });
                    ui.close();
                }
            }
        });
        if ui.small_button("Remove").clicked() {
            *action = Some(BoardAction::RemoveCard(card.card_ref.id.clone()));
        }
    });
}

/// Apply a requested board mutation. Card refs are identified by id; moves
/// insert at the destination tail (v1 — DnD with a precise index is
/// deferred). Each op runs synchronously on the current tokio runtime
/// (entered by the frame loop); the next frame re-reads the board from disk.
fn apply_action(app: &mut AppState, tab_id: TabId, board_rel: &str, action: BoardAction) {
    let rel = board_rel.to_string();
    let log = app.vault_session.services.oplog.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    use hiker_core::boards::ops as bops;

    // Each arm builds an owned future and hands it to `run`; owning the args
    // keeps the future free of borrows into `apply_action`'s locals.
    let (label, result): (&str, Result<(), hiker_core::errors::HikerError>) = match action {
        BoardAction::OpenNote(target) => {
            crate::editor_pane::open_file(app, &target, true);
            return;
        }
        BoardAction::RenameBoard { new_title } => {
            rename_board(app, tab_id, &rel, &new_title);
            return;
        }
        BoardAction::OpenPathConflict { card_id, path, recorded_id, current_path_id } => {
            app.session.modal = Some(crate::state::Modal::PathConflict {
                path,
                recorded_id,
                current_path_id,
                target: crate::state::PathConflictTarget::BoardCard {
                    board_rel: rel,
                    card_id,
                },
            });
            return;
        }
        BoardAction::AddCardFromFile { column, source_rel } => {
            add_card(app, &rel, &column, &source_rel);
            return;
        }
        BoardAction::MoveCard { from, card_id, to, to_index } => (
            "Move card",
            run(async move {
                bops::move_card(
                    &log,
                    &jobs,
                    &vault,
                    bops::MoveCardRequest {
                        board_doc_rel: &rel,
                        from_column: &from,
                        card_id: &card_id,
                        to_column: &to,
                        to_index,
                    },
                )
                .await
            }),
        ),
        BoardAction::RemoveCard(card_id) => (
            "Remove card",
            run(async move { bops::remove_card(&log, &jobs, &vault, &rel, &card_id).await }),
        ),
        BoardAction::AddColumn => {
            let name = unique_column_name(app, &rel);
            (
                "Add column",
                run(async move { bops::add_column(&log, &jobs, &vault, &rel, &name).await }),
            )
        }
        BoardAction::RenameColumn { old, new } => (
            "Rename column",
            run(async move { bops::rename_column(&log, &jobs, &vault, &rel, &old, &new).await }),
        ),
        BoardAction::ReorderColumn { name, to } => (
            "Reorder column",
            run(async move { bops::reorder_column(&log, &jobs, &vault, &rel, &name, to).await }),
        ),
        BoardAction::DeleteColumn(name) => (
            "Delete column",
            run(async move { bops::delete_column(&log, &jobs, &vault, &rel, &name).await }),
        ),
        BoardAction::SetWipLimit { name, limit } => (
            "Set WIP limit",
            run(async move {
                bops::set_column_wip_limit(&log, &jobs, &vault, &rel, &name, limit).await
            }),
        ),
    };
    if let Err(e) = result {
        app.push_toast(format!("{label} failed: {e}"), ToastLevel::Error);
    }
}

/// Rename the board-doc by moving it to `<parent>/<new_title>.md` via
/// `core::vault::move_note` (the same path the file-tree inline-rename uses),
/// then repoint the open board tab + any buffer/editor tabs at the new path.
/// The board carries its identity in frontmatter, so a rename is a path-only
/// move; the auto-update-on-move hook fixes any cards referencing it.
///
/// status: board-create
fn rename_board(app: &mut AppState, tab_id: TabId, from: &str, new_title: &str) {
    let parent = from.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let to = if parent.is_empty() {
        format!("{new_title}.md")
    } else {
        format!("{parent}/{new_title}.md")
    };
    if to == from {
        return;
    }
    let store_mutex = app.vault_session.services.read_store.clone();
    let watcher = app.vault_session.services.watcher.clone();
    {
        let mut store = match store_mutex.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Err(err) = hiker_core::vault::move_note(
            &app.vault_session.vault,
            &mut store,
            Some(watcher.as_ref()),
            from,
            &to,
        ) {
            drop(store);
            app.push_toast(format!("Rename board failed: {err}"), ToastLevel::Error);
            return;
        }
    }
    app.session.sidebar.dir_cache.remove(parent);
    // Repoint this board tab + any editor/buffer tabs at the old path.
    if let Some(tab) = app.tab_by_id_mut(tab_id) {
        if let crate::tab::TabKind::Board { path } = &mut tab.kind {
            *path = to.clone();
        }
    }
    if let Some(buf) = app.session.buffers.remove(from) {
        let mut moved = buf;
        moved.path = to.clone();
        app.session.buffers.insert(to.clone(), moved);
    }
    for tab in &mut app.session.tabs {
        if let crate::tab::TabKind::Editor {
            buffer: crate::tab::BufferSource::Vault { path },
            ..
        } = &mut tab.kind
            && path == from
        {
            *path = to.clone();
        }
    }
    app.push_toast(format!("Renamed board -> {to}"), ToastLevel::Info);
}

/// Drive an owned board-op future to completion on the current tokio
/// runtime (entered by the egui frame loop). `move_card` / `add_card` etc.
/// hold a `!Send` `&mut Store` internally, so `block_on` on this thread —
/// not `spawn` — is the right shape.
fn run<F>(fut: F) -> Result<(), hiker_core::errors::HikerError>
where
    F: std::future::Future<Output = Result<(), hiker_core::errors::HikerError>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(fut),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    }
}

/// "Repoint" branch of the shared path-conflict modal for a board card:
/// rewrite the card to adopt the identity of the note now at `new_path`, via
/// the core `repoint_card` op (frontmatter user-save). Runs synchronously on
/// the frame's tokio runtime.
///
/// status: board-card-references
pub fn repoint_card(app: &mut AppState, board_rel: &str, card_id: &str, new_path: &str) {
    let log = app.vault_session.services.oplog.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let store_arc = app.vault_session.services.read_store.clone();
    let (board_rel, card_id, new_path) =
        (board_rel.to_string(), card_id.to_string(), new_path.to_string());
    let result = run(async move {
        let guard = store_arc
            .lock()
            .map_err(|_| hiker_core::errors::HikerError::Io("store lock poisoned".into()))?;
        hiker_core::boards::ops::repoint_card(
            &log, &jobs, &vault, &guard, &board_rel, &card_id, &new_path,
        )
        .await
    });
    if let Err(e) = result {
        app.push_toast(format!("Repoint failed: {e}"), ToastLevel::Error);
    }
}

/// "Break" branch of the shared path-conflict modal for a board card: drop
/// the card from the board-doc (the referenced note is untouched). Reuses
/// the core `remove_card` op.
///
/// status: board-card-references
pub fn break_card(app: &mut AppState, board_rel: &str, card_id: &str) {
    let log = app.vault_session.services.oplog.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let (board_rel, card_id) = (board_rel.to_string(), card_id.to_string());
    let result =
        run(async move { hiker_core::boards::ops::remove_card(&log, &jobs, &vault, &board_rel, &card_id).await });
    if let Err(e) = result {
        app.push_toast(format!("Break reference failed: {e}"), ToastLevel::Error);
    }
}

/// One board for the "Add to board…" picker: path, title, column names.
pub type PickerEntry = (String, String, Vec<String>);

/// Gather every board-doc + its columns, the set of board paths `note_rel`
/// is already a card on, and whether `note_rel` is itself a board-doc.
/// Read-only; runs on menu open. Shared by the file-tree verb and the
/// editor pill.
///
/// status: board-add-card
pub fn picker_context(
    app: &AppState,
    note_rel: &str,
) -> (Vec<PickerEntry>, std::collections::HashSet<String>, bool) {
    let vault = &app.vault_session.vault;
    let Ok(store) = app.vault_session.services.read_store.lock() else {
        return (Vec::new(), std::collections::HashSet::new(), false);
    };
    let is_board = vault
        .read_file(note_rel)
        .ok()
        .map(|s| hiker_core::boards::parse_board_for(note_rel, &s).is_ok())
        .unwrap_or(false);
    let mut boards: Vec<PickerEntry> = Vec::new();
    for item in hiker_core::boards::list(vault, &store).unwrap_or_default() {
        let columns = vault
            .read_file(&item.rel_path)
            .ok()
            .and_then(|s| hiker_core::boards::parse_board_for(&item.rel_path, &s).ok())
            .map(|b| b.columns.into_iter().map(|c| c.name).collect::<Vec<_>>())
            .unwrap_or_default();
        boards.push((item.rel_path, item.title, columns));
    }
    let membership: std::collections::HashSet<String> =
        hiker_core::boards::containing_note_with_paths(vault, &store, note_rel)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.board_doc_rel)
            .collect();
    (boards, membership, is_board)
}

/// "Add to board…" pill in the editor toolbar (`board-add-card`). Hidden
/// unless `path` is a regular `.md`/`.txt` note and at least one board
/// exists; hidden when `path` is itself a board-doc. The menu picks a board
/// + column; a board the note is already on shows "Already on this board".
///
/// status: board-add-card
pub fn add_to_board_pill(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let lower = path.to_lowercase();
    if !lower.ends_with(".md") && !lower.ends_with(".txt") {
        return;
    }
    let (boards, membership, is_board) = picker_context(app, path);
    if is_board || boards.is_empty() {
        return;
    }
    ui.separator();
    let mut pick: Option<(String, String)> = None;
    ui.menu_button("+ Board", |ui| {
        column_picker(ui, &boards, &membership, &mut pick);
    });
    if let Some((board_rel, column)) = pick {
        add_card(app, &board_rel, &column, path);
    }
}

/// Render the board → column nested picker, recording the user's pick.
/// Shared by the editor pill and the file-tree verb.
///
/// status: board-add-card
pub fn column_picker(
    ui: &mut egui::Ui,
    boards: &[PickerEntry],
    membership: &std::collections::HashSet<String>,
    pick: &mut Option<(String, String)>,
) {
    for (rel, title, columns) in boards {
        let already = membership.contains(rel);
        ui.menu_button(title, |ui| {
            if already {
                ui.label(
                    egui::RichText::new("Already on this board")
                        .color(theme::muted())
                        .small(),
                );
            }
            for col in columns {
                if ui.add_enabled(!already, egui::Button::new(col)).clicked() {
                    *pick = Some((rel.clone(), col.clone()));
                    ui.close();
                }
            }
        });
    }
}

/// Append `note_rel` as a card to `board_rel`'s `column` via the core
/// `add_card` op (op-log user-save + lazy id-stamp). Runs synchronously on
/// the frame's tokio runtime; the board view re-reads on its next paint.
/// Shared by the editor pill and the file-tree "Add to board…" verb.
///
/// status: board-add-card
pub fn add_card(app: &mut AppState, board_rel: &str, column: &str, note_rel: &str) {
    let log = app.vault_session.services.oplog.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let watcher = app.vault_session.services.watcher.clone();
    let store_arc = app.vault_session.services.read_store.clone();
    let board_rel = board_rel.to_string();
    let column = column.to_string();
    let note_rel = note_rel.to_string();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let result = handle.block_on(async {
        let mut guard = store_arc
            .lock()
            .map_err(|_| hiker_core::errors::HikerError::Io("store lock poisoned".into()))?;
        hiker_core::boards::ops::add_card(hiker_core::boards::ops::AddCardArgs {
            watcher: &watcher,
            jobs: &jobs,
            vault: &vault,
            store: &mut guard,
            log: &log,
            board_doc_rel: &board_rel,
            column_name: &column,
            source_rel: &note_rel,
        })
        .await
    });
    match result {
        Ok(()) => app.push_toast("Added to board".to_string(), ToastLevel::Info),
        Err(e) => app.push_toast(format!("Add to board failed: {e}"), ToastLevel::Error),
    }
}

/// Pick a column name not already present (`Column`, `Column 2`, …).
fn unique_column_name(app: &AppState, board_rel: &str) -> String {
    let existing: Vec<String> = app
        .vault_session
        .vault
        .read_file(board_rel)
        .ok()
        .and_then(|s| hiker_core::boards::parse_board_for(board_rel, &s).ok())
        .map(|b| b.columns.into_iter().map(|c| c.name).collect())
        .unwrap_or_default();
    if !existing.iter().any(|n| n == "Column") {
        return "Column".to_string();
    }
    for n in 2..1000 {
        let cand = format!("Column {n}");
        if !existing.iter().any(|name| name == &cand) {
            return cand;
        }
    }
    "Column".to_string()
}
