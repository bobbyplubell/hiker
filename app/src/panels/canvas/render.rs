//! Canvas-pane rendering: the header (title + view toggle + create toolbar)
//! and the spatial-editor body (the forward op-log binding).
//
// status: canvas-view-toggle
// status: canvas-oplog-binding

use eframe::egui;

use canvas_view_core::state::{CreateKind, Tool};
use hiker_canvas::model::Canvas;
use hiker_theme as theme;

use crate::icons::{Icon, ICONS};
use crate::panels::canvas::{ContentRenderer, ViewMode};
use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;

/// Error / broken-state accent (the theme has no dedicated error token), shared
/// with the board pane's posture.
pub const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}

/// Header: title (basename, no `.canvas`) + "View as: Canvas / JSON" toggle
/// (mirrors the board view) + a per-kind create toolbar in the Canvas render.
/// status: canvas-view-toggle
pub fn header(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, path: &str) {
    let title = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .strip_suffix(".canvas")
        .unwrap_or(path)
        .to_string();
    ui.horizontal(|ui| {
        ui.heading(title);
        ui.separator();
        let current = app
            .panels
            .canvases
            .get(&tab_id)
            .map(|p| p.view)
            .unwrap_or_default();
        if icon_toggle(ui, Icon::Canvas, current == ViewMode::Canvas, "Canvas view") {
            app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Canvas;
        }
        if icon_toggle(ui, Icon::Braces, current == ViewMode::Json, "JSON view") {
            app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Json;
        }
        if current == ViewMode::Canvas {
            ui.separator();
            create_toolbar(ui, app, tab_id);
        }
    });
}

/// The per-kind create toolbar. Each verb drops a node at the viewport center
/// immediately (one click) and selects it — no arm-then-place gesture. `+ Text`
/// and `+ Group` create directly; `+ Link` opens a small inline URL prompt that
/// drops a built `Link` node on submit. `Fit` frames all content.
/// status: canvas-node-create
fn create_toolbar(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    tool_toggle(ui, app, tab_id);
    ui.separator();
    if ui.button("+ Text").clicked() {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.view_widget.create_centered(CreateKind::Text);
        }
    }
    // Insert from vault: open the autocomplete picker over notes + sources; a
    // pick drops a `File { file, subpath: None }` pointer at the viewport
    // center. status: canvas-insert-from-vault
    if ui.button("Insert from vault").clicked() {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.insert_picker.open();
        }
    }
    let link_resp = ui.button("+ Link");
    if link_resp.clicked() {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            // Toggle the inline prompt: open with an empty draft, or close it.
            pane.link_prompt = match pane.link_prompt {
                Some(_) => None,
                None => Some(String::new()),
            };
        }
    }
    if ui.button("+ Group").clicked() {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.view_widget.arm_group_draw();
        }
    }
    if ui.button("Fit").clicked() {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.fit_pending = true;
        }
    }
    link_prompt(app, tab_id, &link_resp);
    insert_from_vault(ui, app, tab_id);
}

/// The Select / Hand tool toggle: two pressed-state labels driving the widget's
/// active tool. Select routes a left-drag by what's under the cursor; Hand pans
/// on any left-drag. status: canvas-tool-mode
fn tool_toggle(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let current = app
        .panels
        .canvases
        .get(&tab_id)
        .map(|p| p.view_widget.tool())
        .unwrap_or_default();
    if icon_toggle(ui, Icon::Cursor, current == Tool::Select, "Select (V) — drag to select/move") {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.view_widget.set_tool(Tool::Select);
        }
    }
    if icon_toggle(ui, Icon::Hand, current == Tool::Hand, "Hand (H) — drag to pan") {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.view_widget.set_tool(Tool::Hand);
        }
    }
}

/// A pressed-state icon toggle button with a hover tooltip; returns `true` on
/// click. Backs the canvas/JSON view switch and the Select/Hand tool toggle so
/// both read as icons rather than text. status: canvas-view-toggle
fn icon_toggle(ui: &mut egui::Ui, icon: Icon, selected: bool, hover: &str) -> bool {
    ui.add(egui::ImageButton::new(ICONS.image(icon)).selected(selected))
        .on_hover_text(hover)
        .clicked()
}

/// Drive one frame of the "Insert from vault" autocomplete picker. While the
/// pane's picker is open, render it as a centered overlay window over notes +
/// sources; on a pick, build a `File { file, subpath: None }` pointer node
/// (default file-node size) and drop it at the viewport center via
/// `insert_node_centered`, so it persists through the existing op-log binding
/// and renders via the content engine. status: canvas-insert-from-vault
fn insert_from_vault(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    use crate::autocomplete::vault_source::{Scope, VaultSource};
    use crate::widgets::autocomplete_picker::{self, PickerOutcome};

    let open = app
        .panels
        .canvases
        .get(&tab_id)
        .is_some_and(|p| p.insert_picker.is_open());
    if !open {
        return;
    }
    let source = VaultSource::new(app.vault_session.vault.clone(), Scope::NotesAndSources);
    let Some(pane) = app.panels.canvases.get_mut(&tab_id) else {
        return;
    };
    if let PickerOutcome::Selected(item) =
        autocomplete_picker::show(ui, &mut pane.insert_picker, &source)
    {
        let node = file_node(item.insert.as_str());
        pane.view_widget.insert_node_centered(node);
    }
}

/// Build a fresh `File` pointer node referencing `rel` (a vault-relative path),
/// sized by the default file-node dimensions. Its id / position are overwritten
/// by `insert_node_centered` when it drops; the node stores only the vault path,
/// never the content. status: canvas-insert-from-vault, canvas-file-node-embed
fn file_node(rel: &str) -> hiker_canvas::model::Node {
    use hiker_canvas::model::{Node, NodeKind};
    Node {
        id: String::new(),
        x: 0,
        y: 0,
        width: 300,
        height: 200,
        color: None,
        kind: NodeKind::File { file: rel.to_string(), subpath: None },
        extra: std::collections::BTreeMap::new(),
    }
}

/// The inline `+ Link` URL prompt: a popup below the button with a text field +
/// OK. On submit (non-empty, trimmed) it builds a `Link { url }` node and drops
/// it at the viewport center via `insert_node_centered`; Esc / click-outside
/// closes it. The pane's `link_prompt` is the source of truth for open + draft;
/// egui's `open_bool` mirrors it so click-outside closes cleanly.
/// status: canvas-node-create, canvas-link-node-card
fn link_prompt(app: &mut AppState, tab_id: TabId, anchor: &egui::Response) {
    let Some(mut draft) = app.panels.canvases.get(&tab_id).and_then(|p| p.link_prompt.clone())
    else {
        return;
    };
    let mut open = true;
    let mut submit = false;
    let mut close = false;
    egui::Popup::from_response(anchor)
        .open_bool(&mut open)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| link_prompt_body(ui, &mut draft, &mut submit, &mut close));
    // egui set `open = false` on a click-outside close; treat that as a close.
    if !open {
        close = true;
    }
    finish_link_prompt(app, tab_id, draft, submit, close);
}

/// Paint the prompt's field + OK button, reporting submit / close back through
/// the out-params.
fn link_prompt_body(ui: &mut egui::Ui, draft: &mut String, submit: &mut bool, close: &mut bool) {
    ui.set_min_width(260.0);
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(draft)
                .hint_text("https://example.com")
                .desired_width(190.0),
        );
        field.request_focus();
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button("OK").clicked() || enter) && !draft.trim().is_empty() {
            *submit = true;
        }
    });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        *close = true;
    }
}

/// Apply the result of one frame of the link prompt: drop the built `Link` node
/// on submit, persist the draft otherwise, and clear the prompt on submit/close.
fn finish_link_prompt(app: &mut AppState, tab_id: TabId, draft: String, submit: bool, close: bool) {
    let Some(pane) = app.panels.canvases.get_mut(&tab_id) else {
        return;
    };
    if submit {
        let node = link_node(draft.trim());
        pane.view_widget.insert_node_centered(node);
        pane.link_prompt = None;
    } else if close {
        pane.link_prompt = None;
    } else {
        pane.link_prompt = Some(draft);
    }
}

/// Build a fresh `Link` node carrying `url`, sized by the create defaults. Its
/// id / position are overwritten by `insert_node_centered` when it drops.
/// status: canvas-link-node-card
fn link_node(url: &str) -> hiker_canvas::model::Node {
    use hiker_canvas::model::{Node, NodeKind};
    let (w, h) = CreateKind::Link.default_size();
    Node {
        id: String::new(),
        x: 0,
        y: 0,
        width: w,
        height: h,
        color: None,
        kind: NodeKind::Link { url: url.to_string() },
        extra: std::collections::BTreeMap::new(),
    }
}

/// The spatial-editor body: parse-on-change, render the `CanvasView`, and
/// persist any committed edits through the op-log user-save path. On a parse
/// error, show a clear error state with a JSON escape hatch instead of
/// painting a stale / panicking canvas. status: canvas-oplog-binding
pub fn canvas_body(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, path: &str) {
    // Reverse binding: re-read the live buffer text (kept current by the editor
    // binding / fs-event reload) and re-parse when it changed.
    let live_text = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
        .unwrap_or_default();
    {
        let pane = app.panels.canvases.entry(tab_id).or_default();
        pane.sync_from_text(&live_text);
        if let Some(err) = pane.parse_error.clone() {
            render_parse_error(ui, app, tab_id, &err);
            return;
        }
    }
    // Apply persisted view state (camera pan/zoom + per-card scroll/zoom) once,
    // now that the pane exists and the path is known. Suppresses fresh-create
    // framing so a restored canvas opens where the user left it.
    // status: canvas-view-state-persist
    super::apply_persisted_view(app, tab_id, path);

    // Cmd/Ctrl+S in the Canvas view. We consume the key here (so it doesn't also
    // reach a global handler) and route by edit state: editing a File node saves
    // that note's shared buffer; otherwise save the canvas document itself. The
    // JSON view path delegates to the buffer panel, which has its own save.
    handle_canvas_save(ui, app, tab_id, path);

    // Drive one frame of the canvas view against the live document. We take the
    // canvas + widget out of the pane to satisfy the borrow checker (the view's
    // `show` borrows both mutably), then put them back.
    let Some(mut taken) = take_pane_doc(app, tab_id) else {
        return;
    };
    // The canvas occupies the space below the header — the available rect, not
    // the full clip rect (which would include the header and let the view's
    // interaction surface cover the toolbar). Mirrors `CanvasView::show`.
    let viewport = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    if taken.fit_pending {
        taken.view_widget.fit(viewport, &taken.canvas);
        taken.fit_pending = false;
    }
    // content seam: the real all-source content engine, behind the same
    // `NodeContentRenderer` trait. It resolves file-node paths against the vault
    // root and caches heavyweight per-node state (editor / htmlview panes) in a
    // UI-thread-local store keyed by tab + node id. status: canvas-node-content-trait
    //
    // Live (unsaved) text for every loaded shared buffer, keyed by its
    // vault-relative path, so a file-node card reflects an open note's unsaved
    // edits instead of stale disk bytes. status: canvas-inline-edit
    let live_text: std::collections::HashMap<String, String> = app
        .session
        .buffers
        .iter()
        .map(|(path, buf)| (path.clone(), buf.current_text()))
        .collect();
    let mut content = ContentRenderer::new(tab_id, app.vault_session.vault_root.clone(), live_text);
    let resp = taken
        .view_widget
        .show(ui, &mut taken.canvas, &mut content);

    if !resp.committed.is_empty() {
        persist_canvas(app, path, &taken.canvas, &mut taken.last_parsed_text);
    }
    // A double-clicked node either ENTERS inline-edit mode (a full-detail File /
    // Text card) or "activates" (a link opens in the OS browser, a file pointer
    // or LOD placeholder opens in a tab). status: canvas-link-node-card, canvas-inline-edit
    if let Some(id) = resp.activated.clone() {
        activate_or_edit(ui, app, tab_id, &taken, &id, viewport);
    }
    // Context-menu verbs that need host-side UI: open the link prompt / vault
    // picker on the pane (the same state the toolbar drives). status: canvas-context-menu
    if resp.request_link_prompt || resp.request_insert_picker {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            if resp.request_link_prompt {
                pane.link_prompt = Some(String::new());
            }
            if resp.request_insert_picker {
                pane.insert_picker.open();
            }
        }
    }
    // Resolve where the inline-edit overlay should sit (the editing node's
    // on-screen rect) BEFORE putting the widget back — the camera lives on
    // `taken.view_widget`. Selection-based exit (click empty / select another
    // node) also reads off `taken`, while it's still owned here.
    let edit = resolve_edit_overlay(ui, tab_id, &taken, viewport, app);
    put_pane_doc(app, tab_id, taken);
    // "New note" verb: mint a brand-new vault note and drop a File-node pointer
    // to it onto the canvas, so it can be edited inline immediately. Runs AFTER
    // `put_pane_doc` so the widget is back in the pane — the shared
    // `new_note_on_canvas` queues the centered insert on it. The Cmd/Ctrl+N
    // binding (when a canvas tab is active) calls the same helper.
    // status: canvas-new-note
    if resp.request_new_note {
        super::new_note_on_canvas(app, tab_id);
    }
    // After `put_pane_doc`, `&mut app` is free — render the overlay (the File
    // path needs `&mut app` + `&mut EmbeddedView` at once). status: canvas-inline-edit
    if let Some((node, edit_rect)) = edit {
        render_edit_overlay(ui, app, tab_id, path, &node, edit_rect);
    }
}

/// Dispatch a double-click: a full-detail File / Text card enters inline-edit
/// mode (the card becomes a focused editor); a Link node, or a LOD placeholder of
/// any kind, keeps the existing open-on-activate behavior (open URL / open the
/// file in a tab). status: canvas-inline-edit
fn activate_or_edit(
    ui: &egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    taken: &TakenDoc,
    id: &str,
    viewport: egui::Rect,
) {
    use crate::panels::canvas::edit;
    let Some(node) = taken.canvas.nodes.iter().find(|n| n.id == id) else {
        return;
    };
    if edit::is_editable(node) && !taken.view_widget.is_node_lod(viewport, node) {
        // Seed the edit overlay at the card's current scroll so clicking into a
        // note keeps its position instead of jumping to the top.
        let scroll = taken.view_widget.card_scroll(id);
        edit::enter(tab_id, node, scroll);
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.editing = Some(id.to_string());
        }
    } else {
        activate_node(ui, app, &taken.canvas, id);
    }
}

/// Decide whether the inline-edit overlay should render this frame and where.
/// Returns the editing node (cloned) plus its current on-screen rect, or `None`
/// (clearing edit state) when edit mode should exit: the node vanished, it
/// scrolled off-screen, or a pointer press landed outside the overlay rect this
/// frame (the click-outside exit).
///
/// Edit mode is owned by `pane.editing` alone — NOT by the canvas selection.
/// Gating on selection broke entering edit under the Hand tool (a press there
/// pans without selecting, so the node is never in the selection and edit mode
/// exited the same frame it was entered) and flickered under Select. The
/// entering double-click lands inside `edit_rect` (it's a press on the node), so
/// it never self-exits; presses inside the overlay place the caret and don't
/// exit; a press anywhere else exits. status: canvas-inline-edit
fn resolve_edit_overlay(
    ui: &egui::Ui,
    tab_id: TabId,
    taken: &TakenDoc,
    viewport: egui::Rect,
    app: &mut AppState,
) -> Option<(hiker_canvas::model::Node, egui::Rect)> {
    use crate::panels::canvas::edit;
    let editing = app.panels.canvases.get(&tab_id).and_then(|p| p.editing.clone())?;
    let exit = |app: &mut AppState| {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.editing = None;
        }
        edit::forget(tab_id);
    };
    let Some(node) = taken.canvas.nodes.iter().find(|n| n.id == editing) else {
        exit(app);
        return None;
    };
    let edit_rect = taken.view_widget.node_screen_rect(viewport, node);
    if !edit::node_on_screen(edit_rect, viewport) {
        exit(app);
        return None;
    }
    // Click-outside exit: a pointer press this frame at a position outside the
    // overlay rect leaves edit mode. Compute against `edit_rect` (above) so the
    // entering double-click — a press on the node, i.e. inside the rect — never
    // self-exits. A press with no interact position (keyboard-only frame) is not
    // an outside click.
    if edit::press_outside(ui, edit_rect) {
        exit(app);
        return None;
    }
    Some((node.clone(), edit_rect))
}

/// Draw the inline-edit overlay for `node` over `edit_rect` and apply its exits.
/// An Escape inside the overlay clears edit mode; a Text-node change persists a
/// `SetText` through the same op-log user-save path moves use.
/// status: canvas-inline-edit
fn render_edit_overlay(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    path: &str,
    node: &hiker_canvas::model::Node,
    edit_rect: egui::Rect,
) {
    use crate::panels::canvas::edit;
    let path_owned = path.to_string();
    let mut persist = move |app: &mut AppState, op: &hiker_canvas::ops::EditOp| {
        persist_text_edit(app, tab_id, &path_owned, op);
    };
    let escaped = edit::show_overlay(ui, app, tab_id, node, edit_rect, &mut persist);
    if escaped {
        if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
            pane.editing = None;
        }
        edit::forget(tab_id);
    }
}

/// Apply a Text-node `SetText` from the inline editor: mutate the pane's live
/// canvas and persist it through the same op-log path `persist_canvas` uses, so a
/// text edit is versioned / mergeable like a move. status: canvas-inline-edit
fn persist_text_edit(app: &mut AppState, tab_id: TabId, path: &str, op: &hiker_canvas::ops::EditOp) {
    let Some(mut canvas) = app
        .panels
        .canvases
        .get_mut(&tab_id)
        .and_then(|p| p.canvas.take())
    else {
        return;
    };
    op.apply(&mut canvas);
    let mut last = app
        .panels
        .canvases
        .get_mut(&tab_id)
        .map(|p| std::mem::take(&mut p.last_parsed_text))
        .unwrap_or_default();
    persist_canvas(app, path, &canvas, &mut last);
    if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
        pane.canvas = Some(canvas);
        pane.last_parsed_text = last;
    }
}

/// Act on a double-clicked node: open a link node's URL in the OS browser, or a
/// file node's referenced vault file in a tab (routing `.canvas` to the canvas
/// view, everything else through the standard open path). Other kinds (text /
/// group) have no activation. status: canvas-link-node-card
fn activate_node(ui: &egui::Ui, app: &mut AppState, canvas: &Canvas, id: &str) {
    use hiker_canvas::model::NodeKind;
    let Some(node) = canvas.nodes.iter().find(|n| n.id == id) else {
        return;
    };
    match &node.kind {
        NodeKind::Link { url } if !url.trim().is_empty() => {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
        }
        NodeKind::File { file, .. } if !file.trim().is_empty() => {
            if file.ends_with(".canvas") {
                super::open(app, file);
            } else {
                crate::editor_pane::open_file(app, file, /* sticky */ true);
            }
        }
        _ => {}
    }
}

/// The pieces the canvas view needs by value for one frame: the parsed canvas,
/// the widget, and the dirty/fit bookkeeping. Carried out of the pane so the
/// view's mutable borrow of both canvas + widget doesn't alias the `app` map.
struct TakenDoc {
    canvas: Canvas,
    view_widget: canvas_view::widget::CanvasView,
    last_parsed_text: String,
    fit_pending: bool,
}

/// Move the parsed document + widget out of the pane for the frame.
fn take_pane_doc(app: &mut AppState, tab_id: TabId) -> Option<TakenDoc> {
    let pane = app.panels.canvases.get_mut(&tab_id)?;
    let canvas = pane.canvas.take()?;
    Some(TakenDoc {
        canvas,
        view_widget: std::mem::take(&mut pane.view_widget),
        last_parsed_text: std::mem::take(&mut pane.last_parsed_text),
        fit_pending: std::mem::replace(&mut pane.fit_pending, false),
    })
}

/// Put the document + widget back after the frame.
fn put_pane_doc(app: &mut AppState, tab_id: TabId, taken: TakenDoc) {
    let pane = app.panels.canvases.entry(tab_id).or_default();
    pane.canvas = Some(taken.canvas);
    pane.view_widget = taken.view_widget;
    pane.last_parsed_text = taken.last_parsed_text;
    pane.fit_pending = taken.fit_pending;
}

/// Forward binding: re-serialize the live canvas to canonical JSON and persist
/// it through the same op-log user-save path boards use (`op_writes::user_save`
/// → minimal localized Yrs ops), so a node move is a mergeable, versioned,
/// syncable edit. Skips the write when the serialization is unchanged. Also
/// mirrors the new text into the shared buffer so the JSON view follows
/// immediately, and updates `last_parsed_text` so the reverse binding doesn't
/// re-parse our own write. status: canvas-oplog-binding
fn persist_canvas(app: &mut AppState, path: &str, canvas: &Canvas, last_parsed_text: &mut String) {
    let json = canvas.to_canonical_json();
    if json == *last_parsed_text {
        return;
    }
    // Keep the shared buffer's editable text in lockstep so a flip to the JSON
    // view shows the just-edited canvas. The buffer is clean (we route the
    // change through the op-log, not the editor), so a later fs-event reload is
    // a no-op against the same bytes.
    if let Some(buf) = app.session.buffers.get_mut(path) {
        buf.set_doc_clamping_selection(&json);
    }
    let result = hiker_core::ops::op_writes::user_save(
        &app.vault_session.services.oplog,
        &app.vault_session.vault,
        path,
        &json,
    );
    match result {
        Ok(()) => *last_parsed_text = json,
        Err(e) => app.push_toast(format!("Canvas save failed: {e}"), ToastLevel::Error),
    }
}

/// Handle Cmd/Ctrl+S while the Canvas view is active. Consumes the chord (so it
/// doesn't double-fire with any global save) and routes by edit state:
/// - editing a **File** node → save that note's shared buffer (`save_buffer`,
///   which folds `working` into `accepted` and rewrites the `.md`);
/// - otherwise → save the **canvas document** itself by committing its op-log
///   `working` layer to disk (the canvas buffer is kept clean — edits route
///   through the op-log, not the editor — so `save_buffer` would no-op; commit
///   the canvas doc directly instead).
///
/// The global `editor.save` action no-ops on a Canvas tab anyway
/// (`keybinds::active_buffer_path` returns `None` for non-`Editor` tabs), so the
/// consume here is belt-and-suspenders against any future global save path.
/// status: canvas-inline-edit
fn handle_canvas_save(ui: &egui::Ui, app: &mut AppState, tab_id: TabId, path: &str) {
    let save_pressed = ui.ctx().input_mut(|i| {
        i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)
    });
    if !save_pressed {
        return;
    }
    // If editing a File node, save that referenced note's buffer instead of the
    // canvas. (Text-node edits live in the `.canvas` itself, so they fall
    // through to the canvas-document save.)
    let editing_file = editing_file_path(app, tab_id);
    if let Some(file) = editing_file {
        if let Err(err) = crate::editor_pane::save_buffer(app, &file) {
            app.push_toast(format!("Save failed: {err}"), ToastLevel::Error);
        } else {
            app.push_toast(format!("Saved {file}"), ToastLevel::Info);
        }
        return;
    }
    save_canvas_document(app, path);
}

/// The vault path of the File node currently in inline-edit mode on this canvas
/// tab, if any. `None` when nothing is editing, or the editing node is a Text
/// node (which saves with the canvas document). status: canvas-inline-edit
fn editing_file_path(app: &AppState, tab_id: TabId) -> Option<String> {
    use hiker_canvas::model::NodeKind;
    let pane = app.panels.canvases.get(&tab_id)?;
    let editing = pane.editing.as_ref()?;
    let canvas = pane.canvas.as_ref()?;
    let node = canvas.nodes.iter().find(|n| &n.id == editing)?;
    match &node.kind {
        NodeKind::File { file, .. } if !file.trim().is_empty() => Some(file.clone()),
        _ => None,
    }
}

/// Commit the canvas document's op-log `working` layer to disk. Mirrors
/// `editor_pane::save_buffer`'s commit path, but reaches for the canvas doc
/// directly because the canvas buffer is kept clean (edits ride the op-log), so
/// `save_buffer`'s dirty-gate would skip it. status: canvas-inline-edit
fn save_canvas_document(app: &mut AppState, path: &str) {
    let log = &app.vault_session.services.oplog;
    let doc_id = match log.doc_id_for_path(path) {
        Ok(Some(id)) => id,
        Ok(None) => {
            app.push_toast(format!("Save failed: no op-log document for {path}"), ToastLevel::Error);
            return;
        }
        Err(e) => {
            app.push_toast(format!("Save failed: {e}"), ToastLevel::Error);
            return;
        }
    };
    match log.commit_working(&doc_id) {
        Ok(_) => app.push_toast(format!("Saved {path}"), ToastLevel::Info),
        Err(e) => app.push_toast(format!("Save failed: {e}"), ToastLevel::Error),
    }
}

/// Parse-error state: a clear message + a button to flip to the JSON view so the
/// user can fix the malformed `.canvas` text by hand. status: canvas-tab
fn render_parse_error(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, err: &str) {
    ui.add_space(8.0);
    ui.colored_label(error_color(), "This .canvas file isn't valid JSON Canvas.");
    ui.label(egui::RichText::new(err).small().color(theme::muted()));
    ui.add_space(6.0);
    if ui.button("Edit as JSON").clicked() {
        app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Json;
    }
}
