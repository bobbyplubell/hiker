//! Board tab: a per-doc kanban view over a curated board-doc.
//!
//! Renders the board-doc's columns side by side; each card shows the
//! referenced note's title and opens that note in the editor pane on click.
//! Card moves / removals and column management go through the `core::board`
//! ops (layered-doc user-save path) — the board-doc on disk is the single source
//! of truth, re-read each frame via `core::board::get_board`.
//!
//! Implements: board-view, board-view-toggle, board-move, board-remove-card,
//! board-column-management. The "Add to board…" verb and create flow live in
//! the file tree / sidebar; the file-tree glyph in `crate::files::sidebar`.
//
// status: board-view

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;
use hiker_theme as theme;
use hiker_core::boards::{BoardDetail, ResolvedCard, ResolvedColumn};
use hiker_core::trails::ops::ResolutionOutcome;

/// Error / broken-reference accent (the theme has no dedicated error token).
const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}

/// Fixed column-lane width. Cards fill it; also caps the drag preview, which
/// is rendered in a free tooltip layer where `available_width` is the screen.
const COLUMN_WIDTH: f32 = 230.0;

/// Which render the board pane shows. The toggle is a render choice over the
/// one underlying layered-doc document, not two tabs — switching to `Markdown`
/// hosts the live editor widget over the board-doc inline (mirroring the
/// cluster editor's view menu), so frontmatter / body edits and column moves
/// ride the same document. status: board-view-toggle
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
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
    pub view: ViewMode,
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
    /// Inline-edit draft for a freeform card: `(card_id, draft_text)`. Set
    /// when the user clicks a text card to edit it, or right after a
    /// `+ Add card` mints a fresh empty card. Commits on Enter / focus-loss,
    /// cancels on Esc.
    ///
    /// status: board-freeform-card
    pub editing_card: Option<(String, String)>,
    /// Set the frame after `editing_card` is seeded so the card field grabs
    /// focus once without stealing it every frame.
    ///
    /// status: board-freeform-card
    pub card_edit_focus: bool,
    /// Whether the sprint metrics chart strip is toggled on (the Metrics
    /// header entry). status: pm-layered-metrics
    pub show_metrics: bool,
    /// Rendered metrics charts, memoized by the board's newest accepted
    /// op id (recomputed only when the board-doc gains history).
    /// status: pm-layered-metrics
    pub metrics: Option<crate::panels::board_metrics::Strip>,
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
    app.session.tabs.push(Tab::new(id, TabKind::Board { path: path.to_string() }, true));
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
    /// Open the card's note: into the preview slot on a plain click, sticky
    /// on mod-click (`interaction.md` [modclick-sticky]).
    OpenNote { path: String, sticky: bool },
    MoveCard { from: String, card_handle: String, to: String, to_index: usize },
    RemoveCard(String),
    /// A note (by rel-path) dropped onto a column from the file tree → add it
    /// as a card. status: board-dnd
    AddCardFromFile { column: String, source_rel: String },
    AddColumn,
    RenameColumn { old: String, new: String },
    /// Seed the inline column-rename text field for `name` (the `…`-menu
    /// "Rename column" entry; needs the live `app`/`tab_id`, so it routes
    /// through `apply_action` rather than a core op).
    StartRenameColumn(String),
    ReorderColumn { name: String, to: usize },
    DeleteColumn(String),
    /// Arm the inline delete-with-cards confirm row for `name` (used when the
    /// column still has cards; an empty column deletes immediately).
    RequestDeleteColumn(String),
    RenameBoard { new_title: String },
    /// Set the WIP limit on a column (`None` clears it).
    /// status: board-wip-limits
    SetWipLimit { name: String, limit: Option<usize> },
    /// Create a freeform card in `column` (empty text) and enter inline edit
    /// on it. status: board-freeform-card
    AddTextCard { column: String },
    /// Commit the edited text of a freeform card. status: board-freeform-card
    SetCardText { card_id: String, text: String },
    /// A shared note-item base verb picked on a note card's context menu
    /// (Open / Reveal / Properties), dispatched through
    /// `item_menu::apply_item_action`.
    CardItem { action: crate::item_menu::ItemAction, path: String },
    /// Convert a freeform card into a note and swap it in place
    /// (`freeform-promote-note`).
    PromoteCard { card_id: String },
    /// Close this sprint into `destination`: stage the rollover batch
    /// (`sprint-rollover`).
    CloseSprint { destination: String },
    /// Open/focus the Graph tab on this board-doc's node, its cards at depth
    /// 1 — the container variant of the note-item "Open in graph" entry
    /// (the board title's right-click menu). status: open-in-graph-containers
    OpenInGraph,
}

/// A verb on a card's right-click menu (`interaction.md`
/// [rightclick-menu-always]): the shared note-item base on note cards, plus
/// the card-contextual entries.
#[derive(Clone, Copy, Debug)]
enum CardVerb {
    /// A shared note-item base verb (note cards only).
    Base(crate::item_menu::ItemAction),
    /// Remove the card from the board (reference removal — the hover `×`
    /// stays as the quick path, per [destructive-verbs-in-menu]).
    Remove,
    /// Enter inline edit on a freeform card's text (the menu twin of the
    /// card-body click).
    EditText,
    /// Convert a freeform card into a note, swapping the card in place
    /// (`freeform-promote-note`).
    Promote,
}

/// Build a card's context menu. Note cards compose the shared note-item base
/// plus "Remove from board"; freeform cards get their own small menu (Edit
/// text · Convert to note · Remove from board, per `freeform-promote-note`);
/// an orphan card has no note behind it, so only the reference-removal verb
/// applies.
fn build_card_menu(card: &ResolvedCard) -> egui_workbench::menu::Menu<CardVerb> {
    use crate::item_menu::{note_item_base, BaseOpts};
    match &card.resolution {
        Some(ResolutionOutcome::Resolved { rel_path }) => {
            note_item_base(rel_path, BaseOpts { reveal: true }, CardVerb::Base)
                .section()
                .action("Remove from board", CardVerb::Remove)
        }
        None => egui_workbench::menu::Menu::new()
            .action("Edit text", CardVerb::EditText)
            .action("Convert to note", CardVerb::Promote)
            .section()
            .action("Remove from board", CardVerb::Remove),
        Some(ResolutionOutcome::Orphan) => {
            egui_workbench::menu::Menu::new().action("Remove from board", CardVerb::Remove)
        }
    }
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
        hiker_core::boards::get_board(
            &app.vault_session.vault,
            &store,
            &app.vault_session.services.layered,
            path,
            Some(app.vault_session.services.kinds.as_ref()),
        )
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
    // The toggled metrics chart strip on sprint-kind boards
    // (`pm-layered-metrics`): derived from the board-doc's layered-doc history,
    // zero tracking writes.
    let show_metrics = view == ViewMode::Board
        && app
            .panels
            .boards
            .get(&tab_id)
            .is_some_and(|p| p.show_metrics)
        && app
            .vault_session
            .services
            .kinds
            .board_like(&detail.kind)
            .is_some();
    if show_metrics {
        crate::panels::board_metrics::show(ui, app, tab_id, &detail.rel_path);
    }
    match view {
        ViewMode::Board => render_columns(ui, app, tab_id, &detail, &mut action),
        ViewMode::Markdown => {
            // Host the live editor widget over the board-doc inline, in this
            // same tab — a render choice over the one layered-doc document, not a
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
        // one layered-doc document, not two tabs. status: board-view-toggle
        let current = app
            .panels
            .boards
            .get(&tab_id)
            .map(|p| p.view)
            .unwrap_or_default();
        if ui.selectable_label(current == ViewMode::Board, "Board").clicked() {
            app.panels.boards.entry(tab_id).or_default().view = ViewMode::Board;
        }
        if ui.selectable_label(current == ViewMode::Markdown, "Markdown").clicked() {
            app.panels.boards.entry(tab_id).or_default().view = ViewMode::Markdown;
        }
        // Column management only applies in the Board render.
        if current == ViewMode::Board {
            ui.separator();
            if ui.button("+ Column").clicked() {
                *action = Some(BoardAction::AddColumn);
            }
            // Sprint-kind boards get the close/rollover verb: pick a
            // destination (another sprint, or any board as a backlog) and
            // stage the rollover batch. status: sprint-rollover
            let is_sprint = app
                .vault_session
                .services
                .kinds
                .board_like(&detail.kind)
                .is_some();
            if is_sprint {
                ui.menu_button("Close sprint…", |ui| {
                    if let Some(destination) =
                        crate::panels::board_close::render_menu(ui, app, &detail.rel_path)
                    {
                        *action = Some(BoardAction::CloseSprint { destination });
                    }
                });
                // Toggle the layered-doc-derived metrics chart strip.
                // status: pm-layered-metrics
                let metrics_on = app
                    .panels
                    .boards
                    .get(&tab_id)
                    .is_some_and(|p| p.show_metrics);
                if ui.selectable_label(metrics_on, "Metrics").clicked() {
                    let pane = app.panels.boards.entry(tab_id).or_default();
                    pane.show_metrics = !pane.show_metrics;
                }
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
        let resp = ui.heading(title).interact(egui::Sense::click());
        if resp.double_clicked() {
            let pane = app.panels.boards.entry(tab_id).or_default();
            pane.renaming_title = Some(title.to_string());
            pane.title_rename_focus = true;
        }
        // The board title's right-click menu — the container's graph entry
        // (`interaction.md` [rightclick-menu-always]: a menu, even with one
        // verb; menus are the universal path, no extra header button).
        // status: open-in-graph-containers
        resp.context_menu(|ui| {
            let menu = egui_workbench::menu::Menu::new()
                .action("Open board in graph", BoardAction::OpenInGraph);
            if let Some(verb) = egui_workbench::menu::show(ui, menu) {
                *action = Some(verb);
            }
        });
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
    // `auto_shrink([false, true])`: fill the available width (so columns that
    // fit never trip a scrollbar) but shrink to the columns' height (so the
    // horizontal scrollbar, when columns overflow, hugs the lanes instead of
    // floating at the bottom of the empty tab — the "wonky" placement).
    egui::ScrollArea::horizontal()
        .auto_shrink([false, true])
        .show(ui, |ui| {
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
    /// Polymorphic handle: a note card's vault path or a freeform
    /// card's `card_id`. status: board-card-references
    card_handle: String,
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
    // The column is a subtle lane (light gray, rounded); the *cards* inside are
    // the elevated white boxes. We paint our OWN `Frame` rather than going
    // through `dnd_drop_zone` — `dnd_drop_zone` ignores the frame colour (it
    // overrides fill/stroke with the widget-state visuals), which is why the
    // styling only appeared mid-drag. Drops are read off the lane response's
    // payload below. status: board-dnd
    let col_frame = egui::Frame::default()
        .fill(theme::active_bg())
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(8));
    let response = col_frame
        .show(ui, |ui| {
            // A `Frame`'s inner ui inherits the parent layout (here
            // `horizontal_top`); force a vertical lane so header + cards stack.
            ui.vertical(|ui| {
                ui.set_width(COLUMN_WIDTH);
                render_column_header(ui, app, tab_id, col, col_index, column_names, action);
                ui.add_space(6.0);
                for (card_index, card) in col.cards.iter().enumerate() {
                    render_card(ui, app, tab_id, &col.name, card, card_index, action);
                }
                if col.cards.is_empty() {
                    ui.add_space(24.0);
                }
                // Per-column freeform-card affordance. status: board-freeform-card
                ui.add_space(2.0);
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("+ Add card").small().color(theme::muted()),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    *action = Some(BoardAction::AddTextCard { column: col.name.clone() });
                }
            });
        })
        .response;
    // A card released over the lane — but NOT over a specific card, which takes
    // the payload first during the frame above — appends to the tail.
    if let Some(drag) = response.dnd_release_payload::<CardDrag>() {
        *action = Some(BoardAction::MoveCard {
            from: drag.from_column.clone(),
            card_handle: drag.card_handle.clone(),
            to: col.name.clone(),
            to_index: usize::MAX,
        });
    }
    // A file-tree rel-path dropped onto the lane → add that note as a card.
    if let Some(src) = response.dnd_release_payload::<String>() {
        *action = Some(BoardAction::AddCardFromFile {
            column: col.name.clone(),
            source_rel: (*src).clone(),
        });
    }
}

/// Column header: name + count, with an inline-rename text field when this
/// column is being renamed, plus a `…` menu for rename / reorder / delete.
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
            ui.menu_button("…", |ui| {
                if let Some(chosen) = egui_workbench::menu::show(
                    ui,
                    build_board_column_menu(col, col_index, column_names.len()),
                ) {
                    *action = Some(chosen);
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

/// Build the `…` column-options menu as a `egui_workbench::menu::Menu<BoardAction>`
/// (status: ctxmenu-board): WIP-limit submenu, Rename column, Move left / right
/// (each gated by position), and Delete column. Delete routes through
/// `RequestDeleteColumn` when the column still has cards (the inline confirm
/// flow stays as-is) and `DeleteColumn` when it's empty.
fn build_board_column_menu(
    col: &ResolvedColumn,
    col_index: usize,
    column_count: usize,
) -> egui_workbench::menu::Menu<BoardAction> {
    let mut menu = egui_workbench::menu::Menu::new()
        .submenu("WIP limit", build_wip_limit_menu(col))
        .action("Rename column", BoardAction::StartRenameColumn(col.name.clone()));
    if col_index > 0 {
        menu = menu.action(
            "Move left",
            BoardAction::ReorderColumn { name: col.name.clone(), to: col_index - 1 },
        );
    }
    if col_index + 1 < column_count {
        menu = menu.action(
            "Move right",
            BoardAction::ReorderColumn { name: col.name.clone(), to: col_index + 1 },
        );
    }
    let delete = if col.cards.is_empty() {
        BoardAction::DeleteColumn(col.name.clone())
    } else {
        // Delete-with-cards prompts first via the inline confirm row.
        BoardAction::RequestDeleteColumn(col.name.clone())
    };
    menu.action("Delete column", delete)
}

/// The WIP-limit submenu: "No limit" plus presets 1..6, rendered as toggles so
/// the current limit shows a checkmark. Presets cover the common cases; overflow
/// is flagged (soft) rather than blocking the move. status: board-wip-limits
fn build_wip_limit_menu(col: &ResolvedColumn) -> egui_workbench::menu::Menu<BoardAction> {
    let mut menu = egui_workbench::menu::Menu::new().toggle(
        "No limit",
        col.wip_limit.is_none(),
        BoardAction::SetWipLimit { name: col.name.clone(), limit: None },
    );
    for n in 1..=6usize {
        menu = menu.toggle(
            n.to_string(),
            col.wip_limit == Some(n),
            BoardAction::SetWipLimit { name: col.name.clone(), limit: Some(n) },
        );
    }
    menu
}

fn render_card(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    column_name: &str,
    card: &ResolvedCard,
    card_index: usize,
    action: &mut Option<BoardAction>,
) {
    // If this freeform card is being edited, render the inline editor
    // (interactive) and skip the drag overlay so typing/selection work.
    // Inline-edit only applies to freeform cards (they have an
    // editable `text` body); note cards open via OpenNote.
    let editing = app
        .panels
        .boards
        .get(&tab_id)
        .and_then(|p| p.editing_card.as_ref())
        .filter(|(id, _)| Some(id.as_str()) == card.card_id.as_deref())
        .map(|(_, draft)| draft.clone());
    if let Some(draft) = editing {
        render_card_editor(ui, app, tab_id, card, draft, action);
        ui.add_space(6.0);
        return;
    }

    // The WHOLE card is the drag source — drag from anywhere — and also senses
    // clicks. Its face is non-interactive labels + a painted `×`, so there are
    // no inner widgets fighting the drag for presses (the cause of the earlier
    // "can't click" trouble). A click is disambiguated by hit-testing its
    // position: on the `×` → remove; elsewhere → open the note / edit the text.
    // status: board-dnd
    let card_frame = egui::Frame::default()
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8));
    let handle = card.handle().to_string();
    let drag_id =
        ui.make_persistent_id(("board-card", column_name, handle.as_str(), card_index));
    // Hover feedback is the shared "click acts here" signal (`interaction.md`
    // [hover-open-signal]): the standard card hover wash as the frame fill —
    // read off last frame's response, the idiomatic egui hover-fill pattern —
    // plus the pointer cursor below. Not a hand-rolled accent overlay.
    let hovered_last_frame = ui
        .ctx()
        .read_response(drag_id)
        .is_some_and(|r| r.hovered());
    let card_frame = card_frame.fill(
        theme::open_signal_wash(false, hovered_last_frame).unwrap_or(egui::Color32::WHITE),
    );
    let egui::InnerResponse { inner: x_rect, response } = ui.dnd_drag_source(
        drag_id,
        CardDrag {
            card_handle: handle.clone(),
            from_column: column_name.to_string(),
        },
        |ui| card_frame.show(ui, |ui| render_card_face(ui, card)).inner,
    );
    let resp = response.interact(egui::Sense::click());
    // The `×` keeps its DISTINCT signal — a red wash means "click to remove"
    // (destructive-hover), a different meaning from the open wash above.
    let over_x = ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|p| x_rect.expand(4.0).contains(p));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        if over_x {
            let c = error_color();
            ui.painter().rect_filled(
                x_rect.expand(3.0),
                egui::CornerRadius::same(4),
                egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 30),
            );
        }
    }
    // Shared note hover-preview card on resolved note cards (interaction.md
    // [hover-preview-universal]); freeform/orphan cards have no note to
    // preview, and hovering the `×` previews nothing (it signals remove).
    if resp.hovered()
        && !over_x
        && let Some(ResolutionOutcome::Resolved { rel_path }) = &card.resolution
    {
        crate::widgets::preview::register_note_hover(ui, resp.rect, rel_path);
    }
    if resp.clicked() {
        if over_x {
            *action = Some(BoardAction::RemoveCard(handle.clone()));
        } else {
            // Plain click opens into the preview slot; mod-click opens sticky
            // (`interaction.md` [modclick-sticky], shared modifier branch).
            let sticky = crate::widgets::note_row::open_sticky(ui.input(|i| i.modifiers));
            card_open_action(app, tab_id, card, sticky, action);
        }
    }
    // Right-click → the card's context menu (`interaction.md`
    // [rightclick-menu-always]): note-item base + Remove on note cards, Edit
    // text + Remove on freeform cards, Remove only on orphans.
    let mut chosen = None;
    resp.context_menu(|ui| chosen = egui_workbench::menu::show(ui, build_card_menu(card)));
    match chosen {
        Some(CardVerb::Base(item)) => {
            if let Some(ResolutionOutcome::Resolved { rel_path }) = &card.resolution {
                *action = Some(BoardAction::CardItem { action: item, path: rel_path.clone() });
            }
        }
        Some(CardVerb::Remove) => *action = Some(BoardAction::RemoveCard(handle.clone())),
        // Freeform only: enter inline edit, exactly like the card-body click
        // (the sticky flag is irrelevant — freeform cards never open a note).
        Some(CardVerb::EditText) => card_open_action(app, tab_id, card, false, action),
        // Freeform only: convert to a note + in-place card swap.
        // status: freeform-promote-note
        Some(CardVerb::Promote) => {
            if let Some(card_id) = card.card_id.clone() {
                *action = Some(BoardAction::PromoteCard { card_id });
            }
        }
        None => {}
    }
    // A card released over this card inserts *before* it (precise index).
    if let Some(drag) = resp.dnd_release_payload::<CardDrag>() {
        *action = Some(BoardAction::MoveCard {
            from: drag.from_column.clone(),
            card_handle: drag.card_handle.clone(),
            to: column_name.to_string(),
            to_index: card_index,
        });
    }
    ui.add_space(6.0);
}

/// The card's non-interactive face: the title/text on the left, a painted `×`
/// docked right. Returns the `×` glyph's rect so `render_card` can hit-test a
/// click against it — there are no inner widgets, so the whole card owns clicks
/// + drag and nothing competes for presses. status: board-dnd
fn render_card_face(ui: &mut egui::Ui, card: &ResolvedCard) -> egui::Rect {
    // Fill the column width so cards read as full-width rows and the `×` docks
    // to the right edge.
    ui.set_width(ui.available_width().min(COLUMN_WIDTH));
    let mut x_rect = egui::Rect::NOTHING;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            x_rect = ui
                .label(egui::RichText::new("×").color(theme::muted()))
                .on_hover_text("Remove from board")
                .rect;
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                render_card_face_body(ui, card);
            });
        });
    });
    x_rect
}

/// The title/text portion of a card face — all non-interactive labels;
/// `render_card`'s card-level click does the opening/editing. A resolved note
/// renders accent-coloured (reads as openable); an orphan/conflict renders
/// muted with a hint; a freeform card shows its text. status: board-freeform-card
fn render_card_face_body(ui: &mut egui::Ui, card: &ResolvedCard) {
    match &card.resolution {
        None => {
            let text = card.text.as_deref().unwrap_or(&card.title);
            let label = if text.trim().is_empty() { "(empty)" } else { text };
            ui.label(label);
        }
        Some(ResolutionOutcome::Orphan) => {
            ui.label(egui::RichText::new(&card.title).color(theme::muted()));
            ui.label(egui::RichText::new("broken reference").small().color(error_color()));
        }
        Some(ResolutionOutcome::Resolved { .. }) => {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(&card.title).color(theme::accent()));
                render_card_pm_strip(ui, card);
            });
        }
    }
}

/// The compact PM field strip on sprint-board note cards
/// (`pm-story-kind`): estimate + near-`due` pills off the index, plus the
/// loud conflicted pill when the note is hand-edited onto more than one
/// sprint (`derived-status-rule`'s problem-pill posture). Renders nothing
/// on plain boards (`card.pm` is `None` there).
fn render_card_pm_strip(ui: &mut egui::Ui, card: &ResolvedCard) {
    let Some(pm) = &card.pm else { return };
    if pm.estimate.is_none() && pm.due.is_none() && !pm.conflicted {
        return;
    }
    ui.horizontal(|ui| {
        if let Some(est) = &pm.estimate {
            ui.label(
                egui::RichText::new(format!("est {est}"))
                    .small()
                    .color(theme::muted()),
            );
        }
        if let Some(due) = &pm.due {
            ui.label(
                egui::RichText::new(format!("due {due}"))
                    .small()
                    .color(theme::muted()),
            );
        }
        if pm.conflicted {
            ui.label(
                egui::RichText::new("in 2+ sprints")
                    .small()
                    .color(error_color()),
            )
            .on_hover_text(
                "This note is a card on more than one sprint — its derived status is \
                 conflicted until it is removed from all but one",
            );
        }
    });
}

/// Handle a card-body click (not on the `×`): open the referenced note
/// (preview slot, or sticky on mod-click), surface the path-conflict modal,
/// or enter inline edit for a freeform card.
fn card_open_action(
    app: &mut AppState,
    tab_id: TabId,
    card: &ResolvedCard,
    sticky: bool,
    action: &mut Option<BoardAction>,
) {
    match &card.resolution {
        // Freeform card → enter inline edit. status: board-freeform-card
        None => {
            let Some(card_id) = card.card_id.clone() else { return };
            let text = card.text.clone().unwrap_or_default();
            let pane = app.panels.boards.entry(tab_id).or_default();
            pane.editing_card = Some((card_id, text));
            pane.card_edit_focus = true;
        }
        Some(ResolutionOutcome::Resolved { rel_path }) => {
            *action = Some(BoardAction::OpenNote { path: rel_path.clone(), sticky });
        }
        Some(ResolutionOutcome::Orphan) => {}
    }
}

/// Inline editor for a freeform card being edited (entered via a card-body
/// click on a text card). Commits on Enter / focus-loss → `SetCardText`,
/// cancels on Esc. Rendered in the same white card frame as the static face.
///
/// status: board-freeform-card
fn render_card_editor(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    card: &ResolvedCard,
    draft: String,
    action: &mut Option<BoardAction>,
) {
    let card_frame = egui::Frame::default()
        .fill(egui::Color32::WHITE)
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8));
    card_frame.show(ui, |ui| {
    ui.set_width(ui.available_width().min(COLUMN_WIDTH));
    let mut buf = draft;
    let resp = ui.add(
        egui::TextEdit::multiline(&mut buf)
            .desired_width(ui.available_width())
            .desired_rows(2)
            .hint_text("card text"),
    );
    let take_focus = app
        .panels
        .boards
        .get(&tab_id)
        .map(|p| p.card_edit_focus)
        .unwrap_or(false);
    if take_focus {
        resp.request_focus();
        if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
            pane.card_edit_focus = false;
        }
    }
    if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
        if let Some((_, d)) = pane.editing_card.as_mut() {
            *d = buf.clone();
        }
    }
    // Enter commits (without inserting a newline); Esc cancels; focus-loss
    // commits the current draft.
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
    let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let commit = (resp.lost_focus() && !cancel) || (resp.has_focus() && enter);
    if commit {
        if let Some(card_id) = card.card_id.clone() {
            *action = Some(BoardAction::SetCardText {
                card_id,
                text: buf.trim().to_string(),
            });
        }
        if let Some(pane) = app.panels.boards.get_mut(&tab_id) {
            pane.editing_card = None;
        }
    } else if cancel
        && let Some(pane) = app.panels.boards.get_mut(&tab_id)
    {
        pane.editing_card = None;
    }
    });
}

/// Apply a requested board mutation. Card refs are identified by id; moves
/// insert at the destination tail (v1 — DnD with a precise index is
/// deferred). Each op runs synchronously on the current tokio runtime
/// (entered by the frame loop); the next frame re-reads the board from disk.
fn apply_action(app: &mut AppState, tab_id: TabId, board_rel: &str, action: BoardAction) {
    let rel = board_rel.to_string();
    let log = app.vault_session.services.layered.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    // status: sprint-board-subtype — every board op parses through the
    // registry-aware gate so sprint boards take the same verbs.
    let kinds = app.vault_session.services.kinds.clone();
    use hiker_core::boards::ops as bops;

    // Each arm builds an owned future and hands it to `run`; owning the args
    // keeps the future free of borrows into `apply_action`'s locals.
    let (label, result): (&str, Result<(), hiker_core::errors::HikerError>) = match action {
        BoardAction::OpenNote { path, sticky } => {
            crate::editor_pane::open_file(app, &path, sticky);
            return;
        }
        BoardAction::CardItem { action, path } => {
            crate::item_menu::apply_item_action(app, action, &path);
            return;
        }
        // Focus the Graph tab on the board-doc node; its cards sit one
        // membership edge away, so depth 1 is the board's neighbourhood.
        // status: open-in-graph-containers
        BoardAction::OpenInGraph => {
            crate::panels::graph::open_focused(app, &rel, 1);
            return;
        }
        BoardAction::RenameBoard { new_title } => {
            rename_board(app, tab_id, &rel, &new_title);
            return;
        }
        BoardAction::AddCardFromFile { column, source_rel } => {
            crate::panels::board_picker::add_card(app, &rel, &column, &source_rel);
            return;
        }
        BoardAction::PromoteCard { card_id } => {
            promote_card(app, &rel, &card_id);
            return;
        }
        BoardAction::CloseSprint { destination } => {
            crate::panels::board_close::close_sprint(app, &rel, &destination);
            return;
        }
        BoardAction::AddTextCard { column } => {
            add_text_card(app, tab_id, &rel, &column);
            return;
        }
        BoardAction::StartRenameColumn(name) => {
            app.panels.boards.entry(tab_id).or_default().renaming_column =
                Some((name.clone(), name));
            return;
        }
        BoardAction::RequestDeleteColumn(name) => {
            app.panels.boards.entry(tab_id).or_default().confirm_delete_column = Some(name);
            return;
        }
        BoardAction::SetCardText { card_id, text } => (
            "Edit card",
            run(async move {
                bops::set_card_text(
                    &log, &jobs, &vault, Some(kinds.as_ref()), &rel, &card_id, &text,
                )
                .await
            }),
        ),
        BoardAction::MoveCard { from, card_handle, to, to_index } => (
            "Move card",
            run(async move {
                bops::move_card(
                    &log,
                    &jobs,
                    &vault,
                    Some(kinds.as_ref()),
                    bops::MoveCardRequest {
                        board_doc_rel: &rel,
                        from_column: &from,
                        card_handle: &card_handle,
                        to_column: &to,
                        to_index,
                    },
                )
                .await
            }),
        ),
        BoardAction::RemoveCard(card_handle) => (
            "Remove card",
            run(async move {
                bops::remove_card(&log, &jobs, &vault, Some(kinds.as_ref()), &rel, &card_handle)
                    .await
            }),
        ),
        BoardAction::AddColumn => {
            let name = unique_column_name(app, &rel);
            (
                "Add column",
                run(async move {
                    bops::add_column(&log, &jobs, &vault, Some(kinds.as_ref()), &rel, &name).await
                }),
            )
        }
        BoardAction::RenameColumn { old, new } => (
            "Rename column",
            run(async move {
                bops::rename_column(&log, &jobs, &vault, Some(kinds.as_ref()), &rel, &old, &new)
                    .await
            }),
        ),
        BoardAction::ReorderColumn { name, to } => (
            "Reorder column",
            run(async move {
                bops::reorder_column(&log, &jobs, &vault, Some(kinds.as_ref()), &rel, &name, to)
                    .await
            }),
        ),
        BoardAction::DeleteColumn(name) => (
            "Delete column",
            run(async move {
                bops::delete_column(&log, &jobs, &vault, Some(kinds.as_ref()), &rel, &name).await
            }),
        ),
        BoardAction::SetWipLimit { name, limit } => (
            "Set WIP limit",
            run(async move {
                bops::set_column_wip_limit(
                    &log, &jobs, &vault, Some(kinds.as_ref()), &rel, &name, limit,
                )
                .await
            }),
        ),
    };
    if let Err(e) = result {
        app.push_toast(format!("{label} failed: {e}"), ToastLevel::Error);
    }
}

/// Rename the board-doc by moving it to `<parent>/<new_title>.md` via the
/// indexer-driven `core::ops::file::move_note` (the same full op the file-tree
/// inline-rename uses), then repoint the open board tab + any buffer/editor
/// tabs at the new path. The board carries its identity in frontmatter, so a
/// rename is a path-only move; the op's `IndexJob::Move` remaps the store
/// (including the `board_cards.board_path` rows for this board-doc) and
/// `links_rename::on_note_moved` rewrites any cards referencing it, so cards
/// stay attached and the board doesn't drop off the Boards index.
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
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let from_owned = from.to_string();
    let to_owned = to.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::ops::file::move_note(&watcher, &jobs, &from_owned, &to_owned).await
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    if let Err(err) = result {
        app.push_toast(format!("Rename board failed: {err}"), ToastLevel::Error);
        return;
    }
    app.file_tree_state.invalidate_dir(parent);
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

// `repoint_card` / `break_card` retired with `trail-path-conflict-modal`.
// Under path-as-identity (`board-card-references`) the Keep mine /
// Repoint / Break modal has no analogue; an unresolved card is an
// orphan the user removes via the per-card `×` (RemoveCard).

/// Convert a freeform card into a note via the core `promote_text_card` op
/// (`freeform-promote-note`): note created from the card text in the
/// board-doc's directory, card swapped in place. When the board belongs to
/// a plan declaring a `default_kind` (`plan-kind`), the note is born with
/// that kind — `hiker.kind` set and the kind's fields seeded empty;
/// otherwise it's born plain, per pm.md's no-plan case. Runs synchronously
/// on the frame's tokio runtime.
///
/// status: freeform-promote-note
fn promote_card(app: &mut AppState, board_rel: &str, card_id: &str) {
    let log = app.vault_session.services.layered.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let watcher = app.vault_session.services.watcher.clone();
    let kinds = app.vault_session.services.kinds.clone();
    let (board_rel, card_id) = (board_rel.to_string(), card_id.to_string());
    // The owning plan's `default_kind`, resolved off the index — `None`
    // (born plain) when the board belongs to no plan or the plan declares
    // none. status: plan-kind
    let template_kind = app
        .vault_session
        .services
        .read_store
        .lock()
        .ok()
        .and_then(|store| {
            hiker_core::pm::plan_default_kind(&store, &kinds, &board_rel)
                .ok()
                .flatten()
                .cloned()
        });
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let result = handle.block_on(async {
        hiker_core::boards::ops::promote_text_card(
            hiker_core::boards::ops::PromoteTextCardArgs {
                watcher: &watcher,
                jobs: &jobs,
                log: &log,
                vault: &vault,
                kinds: Some(kinds.as_ref()),
                board_doc_rel: &board_rel,
                card_id: &card_id,
                template_kind: template_kind.as_ref(),
            },
        )
        .await
    });
    match result {
        Ok(note_rel) => {
            app.file_tree_state.invalidate_all();
            app.push_toast(format!("Converted to note: {note_rel}"), ToastLevel::Info);
        }
        Err(e) => app.push_toast(format!("Convert to note failed: {e}"), ToastLevel::Error),
    }
}

/// Create a freeform card (empty text) in `column` via the core
/// `add_text_card` op, then seed inline edit on the new card so the user
/// types its text immediately. Runs synchronously on the frame's tokio
/// runtime; the board re-reads on its next paint.
///
/// status: board-freeform-card
fn add_text_card(app: &mut AppState, tab_id: TabId, board_rel: &str, column: &str) {
    let log = app.vault_session.services.layered.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let kinds = app.vault_session.services.kinds.clone();
    let (board_rel, column) = (board_rel.to_string(), column.to_string());
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let result = handle.block_on(async {
        hiker_core::boards::ops::add_text_card(
            &log,
            &jobs,
            &vault,
            Some(kinds.as_ref()),
            &board_rel,
            &column,
            "",
        )
        .await
    });
    match result {
        Ok(card_id) => {
            let pane = app.panels.boards.entry(tab_id).or_default();
            pane.editing_card = Some((card_id, String::new()));
            pane.card_edit_focus = true;
        }
        Err(e) => app.push_toast(format!("Add card failed: {e}"), ToastLevel::Error),
    }
}

/// Pick a column name not already present (`Column`, `Column 2`, …).
fn unique_column_name(app: &AppState, board_rel: &str) -> String {
    let existing: Vec<String> = app
        .vault_session
        .vault
        .read_file(board_rel)
        .ok()
        .and_then(|s| {
            hiker_core::boards::parse_board_for(
                board_rel,
                &s,
                Some(app.vault_session.services.kinds.as_ref()),
            )
            .ok()
        })
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

#[cfg(test)]
mod tests {
    use egui_workbench::menu::Entry;
    use hiker_core::boards::ResolvedCard;
    use hiker_core::trails::ops::ResolutionOutcome;

    use super::{build_card_menu, CardVerb};

    fn card(resolution: Option<ResolutionOutcome>) -> ResolvedCard {
        ResolvedCard {
            card_id: resolution.is_none().then(|| "c1".to_string()),
            path: matches!(&resolution, Some(ResolutionOutcome::Resolved { .. }))
                .then(|| "notes/a.md".to_string()),
            text: resolution.is_none().then(|| "todo".to_string()),
            title: "a".to_string(),
            resolution,
            pm: None,
        }
    }

    fn labels(section: &[Entry<CardVerb>]) -> Vec<&str> {
        section
            .iter()
            .map(|e| match e {
                Entry::Action { label, .. } => label.as_ref(),
                Entry::Custom(_) => "(custom)",
                _ => panic!("unexpected entry kind"),
            })
            .collect()
    }

    /// Menu composition per card kind: a note card composes the shared
    /// note-item base plus a Remove section; a freeform card gets Edit text +
    /// Remove; an orphan (broken note ref) keeps only the reference-removal
    /// verb.
    #[test]
    fn card_menu_composes_per_card_kind() {
        let note = build_card_menu(&card(Some(ResolutionOutcome::Resolved {
            rel_path: "notes/a.md".to_string(),
        })));
        let sections = note.sections();
        assert_eq!(sections.len(), 2, "note base section + card-contextual section");
        assert_eq!(
            labels(&sections[0]),
            ["Open", "Reveal in file tree", "Open in graph", "(custom)", "Properties"]
        );
        assert_eq!(labels(&sections[1]), ["Remove from board"]);
        assert!(matches!(
            sections[1][0],
            Entry::Action { action: CardVerb::Remove, .. }
        ));

        let freeform = build_card_menu(&card(None));
        let sections = freeform.sections();
        assert_eq!(sections.len(), 2);
        // status: freeform-promote-note — the Convert verb lands in the
        // freeform card menu, every board.
        assert_eq!(labels(&sections[0]), ["Edit text", "Convert to note"]);
        assert_eq!(labels(&sections[1]), ["Remove from board"]);

        let orphan = build_card_menu(&card(Some(ResolutionOutcome::Orphan)));
        let sections = orphan.sections();
        assert_eq!(sections.len(), 1, "no note behind an orphan — Remove only");
        assert_eq!(labels(&sections[0]), ["Remove from board"]);
    }
}
