//! Shared right-click menu for any list item that references a note/path.
//!
//! Every sidebar list whose rows point at a vault note shows the same base
//! options — Open, Reveal in file tree, Open in graph, Copy path, Properties
//! — so the menu lives here once instead of being re-listed per surface
//! (status: ctxmenu-item-base). A surface with no extra actions calls
//! [`attach_note_item_menu`] directly; a surface that needs contextual entries
//! builds with [`note_item_base`] and composes its own section, dispatching the
//! shared part through [`apply_item_action`]. Surfaces whose pane owns its
//! response (the graph engines) host their menu through the shared
//! [`latched_menu_popup`] instead of `Response::context_menu`.

use egui_workbench::menu::{Action, Menu};

use crate::activity::SurfaceCtx;
use crate::state::AppState;

/// The base actions every note-referencing row supports. Copy-path is *not*
/// here — it needs the egui context to write the clipboard, so it renders as a
/// `Custom` entry inside [`note_item_base`] rather than as an `ItemAction`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ItemAction {
    /// Open the note in the editor (non-sticky / preview slot).
    Open,
    /// Reveal the note's row in the file tree.
    RevealInFiles,
    /// Open/focus the Graph tab on this note's neighbourhood (depth 2, the
    /// code view's default). One base entry, every surface — files, search,
    /// backlinks, board cards, graph nodes, … — with zero per-surface
    /// wiring. status: open-in-graph
    OpenInGraph,
    /// Open the note's Properties tab.
    Properties,
}

/// Knobs for the base menu. `reveal` is `false` when the list *is* the file
/// tree, where "Reveal in file tree" is redundant.
#[derive(Clone, Copy)]
pub(crate) struct BaseOpts {
    /// Include the "Reveal in file tree" entry.
    pub reveal: bool,
}

/// The universal base menu for a list item that references a note/path.
///
/// Order: Open · (Reveal in file tree) · Open in graph · Copy path ·
/// Properties — all in one section. `wrap` lifts an [`ItemAction`] into the
/// caller's own action type so a surface can compose extra entries onto the
/// same `Menu<A>`; surfaces with no extras pass the identity closure and get
/// a `Menu<ItemAction>`.
pub(crate) fn note_item_base<A: 'static>(
    path: &str,
    opts: BaseOpts,
    wrap: impl Fn(ItemAction) -> A,
) -> Menu<A> {
    let mut menu = Menu::new().action_with(Action::new("Open", wrap(ItemAction::Open)));
    if opts.reveal {
        menu = menu.action_with(Action::new(
            "Reveal in file tree",
            wrap(ItemAction::RevealInFiles),
        ));
    }
    menu = menu.action_with(Action::new("Open in graph", wrap(ItemAction::OpenInGraph)));
    let copy_path = path.to_owned();
    menu = menu.custom(move |ui| {
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(copy_path.clone());
            ui.close();
        }
        None
    });
    menu.action_with(Action::new("Properties", wrap(ItemAction::Properties)))
}

/// Apply a base [`ItemAction`] to `path`. The single dispatch path for the
/// shared entries, reused by every surface that embeds [`note_item_base`]
/// (status: ctxmenu-item-base-apply).
pub(crate) fn apply_item_action(app: &mut AppState, action: ItemAction, path: &str) {
    match action {
        ItemAction::Open => crate::editor_pane::open_file(app, path, false),
        ItemAction::RevealInFiles => crate::search::reveal_in_files(app, path),
        // Focus the Graph tab on this note's 2-hop neighbourhood (the code
        // view's default depth). status: open-in-graph
        ItemAction::OpenInGraph => crate::panels::graph::open_focused(app, path, 2),
        ItemAction::Properties => crate::files::sidebar::open_properties(app, path),
    }
}

/// Attach the base note-item menu to an existing row `response` and return the
/// chosen [`ItemAction`], if any. The renderer-only half: surfaces that dispatch
/// through out-params (the search list) capture the action and route it
/// themselves, while [`attach_note_item_menu`] wraps this for the common
/// `ctx.defer` path.
pub(crate) fn note_item_menu_response(
    response: &egui::Response,
    path: &str,
    opts: BaseOpts,
) -> Option<ItemAction> {
    let mut chosen = None;
    response.context_menu(|ui| {
        chosen = egui_workbench::menu::show(ui, note_item_base(path, opts, |a| a));
    });
    chosen
}

/// Attach the base note-item menu to an existing row `response` and defer the
/// chosen action through `ctx`. The one-call entry point for menu-less lists:
/// the clickable row already exists, this only adds the right-click menu.
pub(crate) fn attach_note_item_menu(
    response: &egui::Response,
    ctx: &mut SurfaceCtx<'_>,
    path: &str,
    opts: BaseOpts,
) {
    if let Some(action) = note_item_menu_response(response, path, opts) {
        let owned = path.to_owned();
        ctx.defer(move |app| apply_item_action(app, action, &owned));
    }
}

/// Render a latched context menu as a floating popup at its captured position.
/// For surfaces whose pane owns its `Response` (the graph engines), where
/// `Response::context_menu` isn't available — the host latches
/// `(node key, pointer pos)` on a right-click and calls this each frame. Same
/// menu, hosted in an `egui::Area`. Returns the picked action; clears the
/// latch on pick, Esc, or a press outside the popup.
pub(crate) fn latched_menu_popup<A>(
    ui: &egui::Ui,
    id: egui::Id,
    latch: &mut Option<(String, egui::Pos2)>,
    menu: Menu<A>,
) -> Option<A> {
    let Some((_, pos)) = latch else { return None };
    let pos = *pos;
    let mut picked = None;
    let area = egui::Area::new(id)
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                picked = egui_workbench::menu::show(ui, menu);
            });
        });
    let rect = area.response.rect;
    let pressed_outside = ui.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !rect.expand(2.0).contains(p))
    });
    let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if picked.is_some() || pressed_outside || esc {
        *latch = None;
    }
    picked
}
