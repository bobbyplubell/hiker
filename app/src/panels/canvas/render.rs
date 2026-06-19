//! Canvas-pane rendering: the header (title + view toggle + create toolbar)
//! and the spatial-editor body (the forward layered-doc binding).
//
// status: canvas-view-toggle
// status: canvas-layered-binding

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
        view_menu(ui, app, tab_id);
        let current = app
            .panels
            .canvases
            .get(&tab_id)
            .map(|p| p.view)
            .unwrap_or_default();
        // The node-creation toolbar + canvas settings gear are hidden in reader
        // mode when the user opts into hiding in-tab toolbars. [view-reader-hide-toolbar]
        let show_canvas_tools = current == ViewMode::Canvas && !app.reader_hides_view_toolbar();
        if show_canvas_tools {
            ui.separator();
            create_toolbar(ui, app, tab_id);
        }
        ui.separator();
        link_control(ui, app, tab_id);
        // Gear: canvas-scoped interaction settings, rightmost on the toolbar.
        // status: canvas-settings-menu
        if show_canvas_tools {
            ui.separator();
            canvas_settings_menu(ui, app);
        }
    });
}

/// The eye-icon **View** menu: the Canvas / JSON mode switch plus the canvas-only
/// "Fit to content" action, folded into one dropdown so the header row stays
/// uncluttered. status: canvas-view-toggle
fn view_menu(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let current = app
        .panels
        .canvases
        .get(&tab_id)
        .map(|p| p.view)
        .unwrap_or_default();
    let resp = ui
        .add(
            egui::ImageButton::new(ICONS.image(Icon::Eye))
                .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
        )
        .on_hover_text("View options");
    egui::Popup::menu(&resp).show(|ui| {
        ui.label(
            egui::RichText::new("View as")
                .small()
                .color(hiker_theme::muted()),
        );
        let canvas_btn = egui::Button::image_and_text(ICONS.image(Icon::Canvas), "Canvas")
            .selected(current == ViewMode::Canvas);
        if ui.add(canvas_btn).clicked() {
            app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Canvas;
            ui.close();
        }
        let json_btn = egui::Button::image_and_text(ICONS.image(Icon::Braces), "JSON")
            .selected(current == ViewMode::Json);
        if ui.add(json_btn).clicked() {
            app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Json;
            ui.close();
        }
        if current == ViewMode::Canvas {
            ui.separator();
            if ui.button("Fit to content").clicked() {
                if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
                    pane.fit_pending = true;
                }
                ui.close();
            }
            ui.separator();
            projection_menu(ui, app, tab_id);
            ui.separator();
            minimap_menu(ui, app, tab_id);
        }
    });
}

/// The "Overview" section of the canvas View menu. The overview is a
/// `hiker-graph-view` [`Minimap`](hiker_graph_view::graph_view::minimap::Minimap) — a
/// Poincaré disk of the canvas's cards (coloured dots + edges), not a second
/// camera over the board; clicking a dot moves the canvas to that card and
/// clicking empty overview space swaps it full-pane. The placement / size /
/// indicator controls are engine-owned (`Minimap::options_menu`). status: canvas-minimap
fn minimap_menu(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let Some(pane) = app.panels.canvases.get_mut(&tab_id) else {
        return;
    };
    ui.label(
        egui::RichText::new("Overview")
            .small()
            .color(hiker_theme::muted()),
    );
    pane.overview.options_menu(ui);
}

/// The "Projection" section of the canvas View menu: an Off / Fisheye / Poincaré
/// selector plus (when non-Off) the lens sliders — Strength, Size falloff, and the
/// per-card scale clamp min/max. Wires straight to the canvas widget's camera lens
/// (`proj-canvas-mode`) and the card-scale clamp (`proj-cfg-card-scale-clamp`).
/// Selecting a non-Off mode makes the canvas navigate-only (drag-move / resize /
/// edge-create are gated out in the widget); Off restores full editing.
/// status: proj-canvas-mode, proj-cfg-card-scale-clamp
fn projection_menu(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    use hiker_projection::ProjectionKind;
    let Some(pane) = app.panels.canvases.get_mut(&tab_id) else {
        return;
    };
    ui.label(
        egui::RichText::new("Projection")
            .small()
            .color(hiker_theme::muted()),
    );
    let current = pane.view_widget.projection().kind;
    for (kind, label) in [
        (ProjectionKind::Affine, "Off"),
        (ProjectionKind::Fisheye, "Fisheye"),
        (ProjectionKind::Poincare, "Poincar\u{e9}"),
    ] {
        let mut selected = current == kind;
        if ui.checkbox(&mut selected, label).clicked() && selected {
            pane.view_widget.projection_mut().kind = kind;
        }
    }
    if pane.view_widget.projection().kind == ProjectionKind::Affine {
        return;
    }
    // Lens shape sliders. [proj-cfg-strength, proj-cfg-size-falloff]
    {
        let cfg = pane.view_widget.projection_mut();
        ui.add(egui::Slider::new(&mut cfg.strength, 0.1..=3.0).text("Strength"));
        ui.add(egui::Slider::new(&mut cfg.size_falloff, 0.0..=1.0).text("Size falloff"));
    }
    // Per-card scale clamp. [proj-cfg-card-scale-clamp]
    ui.label(
        egui::RichText::new("Card scale")
            .small()
            .color(hiker_theme::muted()),
    );
    let clamp = pane.view_widget.card_scale_clamp_mut();
    ui.add(egui::Slider::new(&mut clamp.min, 0.05..=1.0).text("Min"));
    ui.add(egui::Slider::new(&mut clamp.max, 0.5..=3.0).text("Max"));
    // Neighbor-gap fill: how aggressively a card grows to fill the screen gap to
    // its nearest neighbour under the lens. [proj-card-fill]
    ui.add(egui::Slider::new(&mut clamp.fill, 0.4..=1.2).text("Fill"));
    // Poincaré-only: the unit-disk boundary circle toggle. [proj-canvas-mode]
    if pane.view_widget.projection().kind == ProjectionKind::Poincare {
        ui.checkbox(pane.view_widget.show_boundary_mut(), "Boundary circle");
    }
}

/// Small "Link" control in the canvas header: opens a popup to wire this
/// canvas tab to follow / drive another editor group, identical to the graph
/// tab's control. status: tab-linking
fn link_control(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let linked = app
        .tab_by_id(tab_id)
        .map(|t| t.link.source.is_some() || t.link.target.is_some())
        .unwrap_or(false);
    // A tab-with-link-in-the-corner icon; pressed-state when this tab is linked.
    let resp = ui
        .add(
            egui::ImageButton::new(ICONS.image(Icon::TabLink))
                .selected(linked)
                .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
        )
        .on_hover_text(if linked {
            "Linked to another tab group"
        } else {
            "Link this canvas to another tab group"
        });
    egui::Popup::menu(&resp).show(|ui| {
        crate::editor_pane::link_menu_ui(ui, app, tab_id);
    });
}

/// The gear **Settings** menu: canvas-scoped interaction toggles surfaced on the
/// toolbar for quick access. These also live in the global Settings window; both
/// read/write the same `[ui]` config keys, which the canvas widget + swipe
/// handler consult live, so a toggle here takes effect immediately. The two
/// settings are scroll-pans-vs-zooms (`canvas-scroll-mode`) and the two-finger
/// swipe-to-navigate opt-out (`navigation-swipe-disable`). status: canvas-settings-menu
fn canvas_settings_menu(ui: &mut egui::Ui, app: &mut AppState) {
    let resp = ui
        .add(
            egui::ImageButton::new(ICONS.image(Icon::Settings))
                .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
        )
        .on_hover_text("Canvas settings");
    egui::Popup::menu(&resp).show(|ui| {
        ui.label(
            egui::RichText::new("Canvas")
                .small()
                .color(hiker_theme::muted()),
        );
        // Scroll behavior: Auto (detect device) / Pan / Zoom. [canvas-scroll-mode]
        ui.label(
            egui::RichText::new("Scroll")
                .small()
                .color(hiker_theme::muted()),
        );
        canvas_scroll_mode_selector(ui, app);
        ui.separator();
        // Two-finger horizontal swipe → Back/Forward. [navigation-swipe-disable]
        let mut swipe = app
            .vault_session
            .config
            .read()
            .map(|c| c.ui.swipe_nav_enabled)
            .unwrap_or(true);
        if ui
            .checkbox(&mut swipe, "Two-finger swipe navigates Back/Forward")
            .on_hover_text("Turn off if a horizontal scroll misfires as Back/Forward navigation.")
            .changed()
        {
            app.set_setting(
                hiker_core::config::SettingsScope::Vault,
                "ui.swipe_nav_enabled",
                &serde_json::json!(swipe),
                "Save swipe navigation toggle failed",
            );
        }
    });
}

/// Radio selector for `[ui].canvas_scroll_mode` (Auto / Pan / Zoom), shared by
/// the canvas gear menu and the global Settings window so both stay in sync. Reads
/// live config and commits via `set_setting`; the canvas widget reads the same
/// `[ui]` key each frame, so a change applies immediately. status: canvas-scroll-mode
pub(crate) fn canvas_scroll_mode_selector(ui: &mut egui::Ui, app: &mut AppState) {
    use hiker_core::config::CanvasScrollMode as M;
    let current = app
        .vault_session
        .config
        .read()
        .map(|c| c.ui.canvas_scroll_mode)
        .unwrap_or_default();
    let mut choice = current;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut choice, M::Auto, "Auto")
            .on_hover_text("Detect the device: a mouse wheel zooms to the cursor, a touchpad pans.");
        ui.selectable_value(&mut choice, M::Pan, "Pan")
            .on_hover_text("Always pan the camera (\u{2318}/Ctrl+scroll still zooms).");
        ui.selectable_value(&mut choice, M::Zoom, "Zoom")
            .on_hover_text("Always zoom to the cursor.");
    });
    if choice != current {
        app.set_setting(
            hiker_core::config::SettingsScope::Vault,
            "ui.canvas_scroll_mode",
            &serde_json::json!(choice.as_str()),
            "Save canvas scroll mode failed",
        );
    }
}

/// Map the persisted `[ui].canvas_scroll_mode` onto the canvas widget's view-state
/// enum. The two are deliberately separate types — the canvas crates stay free of
/// `hiker_core::config` so they can be lifted into a standalone repo (`canvas-crate-split`).
const fn to_view_scroll_mode(mode: hiker_core::config::CanvasScrollMode) -> canvas_view_core::state::ScrollMode {
    use canvas_view_core::state::ScrollMode as V;
    use hiker_core::config::CanvasScrollMode as C;
    match mode {
        C::Auto => V::Auto,
        C::Pan => V::Pan,
        C::Zoom => V::Zoom,
    }
}

/// The create toolbar: the Select/Hand tool toggle plus a `+` split-button. The
/// primary `+` mints a new vault note and drops a pointer to it at the viewport
/// center (`canvas-new-note`); the caret dropdown holds the other insert verbs —
/// Add text, Insert from vault…, Add link… (a small inline URL prompt on
/// submit), Add group. Each drops a node at the viewport center immediately (one
/// click) and selects it — no arm-then-place gesture. (Fit-to-content lives in
/// the header View menu, `view_menu`.) status: canvas-node-create
fn create_toolbar(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    tool_toggle(ui, app, tab_id);
    ui.separator();
    let add = crate::widgets::split_button::split_add_button(ui, "New note", |ui| {
        if ui.button("Add text").clicked() {
            if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
                pane.view_widget.create_centered(CreateKind::Text);
            }
            ui.close();
        }
        // Insert from vault: open the autocomplete picker over notes + sources;
        // a pick drops a `File { file, subpath: None }` pointer at the viewport
        // center. status: canvas-insert-from-vault
        if ui.button("Insert from vault\u{2026}").clicked() {
            if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
                pane.insert_picker.open();
            }
            ui.close();
        }
        // Open the inline link prompt anchored beneath the split-button.
        if ui.button("Add link\u{2026}").clicked() {
            if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
                pane.link_prompt = Some(String::new());
            }
            ui.close();
        }
        if ui.button("Add group").clicked() {
            if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
                pane.view_widget.arm_group_draw();
            }
            ui.close();
        }
    });
    // Primary `+`: mint a fresh vault note + pointer node. status: canvas-new-note
    if add.primary_clicked {
        super::new_note_on_canvas(app, tab_id);
    }
    link_prompt(app, tab_id, &add.response);
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
    ui.add(
        egui::ImageButton::new(ICONS.image(icon))
            .selected(selected)
            .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
    )
    .on_hover_text(hover)
    .clicked()
}

/// Drive one frame of the "Insert from vault" autocomplete picker. While the
/// pane's picker is open, render it as a centered overlay window over notes +
/// sources; on a pick, build a `File { file, subpath: None }` pointer node
/// (default file-node size) and drop it at the viewport center via
/// `insert_node_centered`, so it persists through the existing layered-doc binding
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

/// Accept a file-tree row dropped onto the canvas: a `String` vault-relative
/// path released over the `viewport`. The sidebar emits the dropped file's path
/// as the `String` dnd payload (the same payload the board's lanes accept). We
/// register a hover surface over the EXACT viewport rect — `dnd_drop_zone` would
/// re-allocate at the layout cursor, which no longer aligns to the viewport after
/// the widget paints, so we interact directly like the widget's own surface does
/// — paint the drop affordance while a path hovers, and on release map the
/// pointer to world coords and queue a File-node insert THERE via
/// [`CanvasView::insert_node_at`] — the cursor-positioned analogue of the
/// "Insert from vault" picker's centered insert. The node is built by the same
/// [`file_node`] helper, so it serializes through the layered-doc binding identically;
/// `insert_node_at` consumes it on the next frame, so we request a repaint to
/// flush it promptly. status: canvas-file-drop
fn handle_file_drop(ui: &mut egui::Ui, taken: &mut TakenDoc, viewport: egui::Rect) {
    // A hover surface over the canvas, registered AFTER the widget's own surface
    // so it wins the pointer and reads the release. A stable id keeps it distinct
    // from the canvas surface's id.
    let drop = ui.interact(viewport, ui.id().with("canvas-file-drop"), egui::Sense::hover());
    // While a file path is dragged over the canvas, paint a subtle highlight
    // outline as the drop affordance — the board surfaces a hover state too.
    // status: canvas-file-drop
    if drop.dnd_hover_payload::<String>().is_some() {
        let v = ui.visuals().widgets.active;
        ui.painter().rect_stroke(
            viewport,
            egui::CornerRadius::same(0),
            v.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    let Some(src_rel) = drop.dnd_release_payload::<String>() else {
        return;
    };
    // The release position (the pointer where the user let go). Fall back to the
    // viewport center if egui has no interact pos this frame (shouldn't happen on
    // a real drop). status: canvas-file-drop
    let pos = ui
        .input(|i| i.pointer.interact_pos())
        .unwrap_or_else(|| viewport.center());
    let world = taken.view_widget.camera().screen_to_world(viewport, pos);
    let node = file_node(src_rel.as_str());
    taken.view_widget.insert_node_at(node, world);
    // The insert is consumed on the next `show`; repaint so it lands immediately.
    ui.ctx().request_repaint();
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

/// The inline "Add link" URL prompt: a popup below the split-button with a text
/// field + OK. On submit (non-empty, trimmed) it builds a `Link { url }` node and drops
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
/// persist any committed edits through the layered-doc user-save path. On a parse
/// error, show a clear error state with a JSON escape hatch instead of
/// painting a stale / panicking canvas. status: canvas-layered-binding
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
    // Scroll mode (`[ui].canvas_scroll_mode`): drive the widget's pan-vs-zoom
    // behavior. In PAN and AUTO a touchpad two-finger scroll pans the canvas, so
    // claim its rect as a swipe-nav skip region — otherwise the same gesture would
    // also fire back/forward navigation. ZOOM never pans, so swipe-nav stays live.
    // status: canvas-scroll-mode
    let scroll_mode = app
        .vault_session
        .config
        .read()
        .map(|c| c.ui.canvas_scroll_mode)
        .unwrap_or_default();
    taken.view_widget.set_scroll_mode(to_view_scroll_mode(scroll_mode));
    if !matches!(scroll_mode, hiker_core::config::CanvasScrollMode::Zoom) {
        app.session.nav.swipe_skip_rects.push(viewport);
    }
    // A pending "snap to node" (opened from the "Appears in" sidebar) takes
    // precedence over the one-shot fit: framing the whole board would undo the
    // point of jumping to a specific node. status: canvas-appears-in
    let focusing = app
        .panels
        .canvases
        .get(&tab_id)
        .is_some_and(|p| p.focus_note_pending.is_some());
    if taken.fit_pending && !focusing {
        taken.view_widget.fit(viewport, &taken.canvas);
    }
    taken.fit_pending = false;
    // FOLLOW: when this canvas is linked to a source group, select + center the
    // file-node referencing whatever note is active there. Deduped on `followed`
    // so the camera only moves when the linked note changes — the user keeps free
    // pan/zoom in between. status: tab-linking
    apply_follow(app, tab_id, &mut taken, viewport);
    apply_pending_focus(app, tab_id, &mut taken, viewport);
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
    // The app owns the canvas right-click menus (the lean widget has no menu lib);
    // it implements the widget's menu seam with `egui_workbench::menu`.
    // status: ctxmenu-canvas
    let mut menus = super::menu::CanvasMenus;
    let resp = taken
        .view_widget
        .show(ui, &mut taken.canvas, &mut content, &mut menus);

    // A file-tree row dropped onto the canvas → add a File-node pointer at the
    // cursor. The sidebar emits the dropped file's vault-relative path as the
    // `String` dnd payload (the same payload the board's lanes accept); we read
    // it off a drop zone over the canvas viewport, map the release position to
    // world coords, and queue an insert there. The node is built by the same
    // `file_node` helper the "Insert from vault" picker uses, so it persists
    // through the layered-doc binding identically. status: canvas-file-drop
    handle_file_drop(ui, &mut taken, viewport);

    // The Poincaré OVERVIEW (corner minimap + expand swap): a simplified graph of
    // the canvas. Rendered after the canvas paints so it sits on top; clicking a
    // dot or swapping back re-centers the canvas camera on the focused card.
    // status: canvas-minimap
    render_overview(ui, &mut taken, viewport);

    if !resp.committed.is_empty() {
        persist_canvas(app, path, &taken.canvas, &mut taken.last_parsed_text);
    }
    // A double-clicked node either ENTERS inline-edit mode (a full-detail File /
    // Text card) or "activates" (a link opens in the OS browser, a file pointer
    // or LOD placeholder opens in a tab). status: canvas-link-node-card, canvas-inline-edit
    if let Some(id) = resp.activated.clone() {
        activate_or_edit(ui, app, tab_id, &taken, &id, viewport);
    }
    // Enter inline-edit WITHOUT a double-click: a click-again on the already-sole-
    // selected node (`resp.edit_requested`), or Enter / F2 on a single selected
    // editable node. Gated on NOT already editing (and no host popup) — the canvas
    // reads keys globally, so a Backspace/Enter inside the overlay must never
    // re-trigger entry. status: canvas-inline-edit
    let busy = app
        .panels
        .canvases
        .get(&tab_id)
        .is_some_and(|p| p.editing.is_some() || p.link_prompt.is_some());
    let edit_target = if busy {
        None
    } else {
        resp.edit_requested.clone().or_else(|| {
            // Only treat Enter/F2 as "edit the selected node" when no widget owns
            // the keyboard — else it would steal Enter from a focused field (e.g.
            // the edge-label editor). The canvas surface surrenders focus each
            // frame, so an idle canvas reports no focus.
            if ui.memory(|m| m.focused().is_some()) {
                return None;
            }
            let enter = ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::F2)
            });
            if !enter {
                return None;
            }
            let sel = taken.view_widget.selection();
            (sel.edges.is_empty() && sel.nodes.len() == 1)
                .then(|| sel.nodes.iter().next().cloned())
                .flatten()
        })
    };
    if let Some(id) = edit_target {
        try_enter_edit(app, tab_id, &taken, &id, viewport);
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
    // "Open in new tab" verb (node context menu): resolve the right-clicked
    // node's openable target from the canvas WHILE `taken` is still owned, then
    // act after `put_pane_doc` (the open needs `&mut app` alone).
    // status: canvas-open-in-new-tab
    let open_in_new_tab = resp
        .request_open_in_new_tab
        .as_deref()
        .and_then(|id| super::node_open_target(&taken.canvas, id));
    // Resolve where the inline-edit overlay should sit (the editing node's
    // on-screen rect) BEFORE putting the widget back — the camera lives on
    // `taken.view_widget`. Selection-based exit (click empty / select another
    // node) also reads off `taken`, while it's still owned here.
    let edit = resolve_edit_overlay(ui, tab_id, &taken, viewport, app);
    put_pane_doc(app, tab_id, taken);
    if let Some(target) = open_in_new_tab {
        super::open_target_in_new_tab(ui, app, target);
    }
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

/// Render the Poincaré overview (corner minimap, or full-pane when expanded) and
/// wire the camera sync. A no-op when disabled+collapsed or the canvas has no
/// non-group cards. The overview is a `hiker-graph-view` instance over a
/// [`super::overview::CanvasGraphSource`] (coloured dots + edges, in-viewport
/// cards highlighted). A clicked dot re-centers the canvas on that card; an
/// empty-area click toggles the expand swap, and collapsing re-centers the canvas
/// on the overview's current focus. status: canvas-minimap
fn render_overview(ui: &mut egui::Ui, taken: &mut TakenDoc, viewport: egui::Rect) {
    let model = super::overview::Model::build(&taken.canvas, &ui.visuals().clone());
    if model.node_count() == 0 {
        return;
    }
    // The overview reads the canvas's ACTUAL layout: the card centers are handed
    // straight to the minimap as positions (never force-laid-out), so the disk
    // projects the real positions. The viewport world rect drives the engine's
    // viewport-location indicator.
    let positions = model.positions();
    let viewport_world = camera_viewport_world(taken, viewport);
    let source = super::overview::CanvasGraphSource::new(&model, ui.visuals());

    let out = taken
        .overview
        .ui(ui, viewport, &source, &positions, Some(viewport_world));

    // A clicked dot → select that card AND bring it into view on the canvas. On a
    // collapse the engine reports the focused card, which the canvas recenters on.
    if let Some(card_id) = out.clicked.or(out.focused_on_collapse) {
        taken.view_widget.focus_node(viewport, &taken.canvas, &card_id);
    }
}

/// The canvas viewport as a WORLD-space rect (in the overview's `f32` position
/// space), via the camera's screen↔world map — the region the minimap's indicator
/// highlights as "where you are". status: canvas-minimap
fn camera_viewport_world(taken: &TakenDoc, viewport: egui::Rect) -> egui::Rect {
    let cam = taken.view_widget.camera();
    let a = cam.screen_to_world(viewport, viewport.min);
    let b = cam.screen_to_world(viewport, viewport.max);
    let (x0, x1) = (a.x.min(b.x) as f32, a.x.max(b.x) as f32);
    let (y0, y1) = (a.y.min(b.y) as f32, a.y.max(b.y) as f32);
    egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
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
    // Double-click: enter inline-edit for an editable full-detail card, else
    // activate (open a link / file pointer / LOD placeholder in a tab).
    if !try_enter_edit(app, tab_id, taken, id, viewport) {
        super::activate_node(ui, app, tab_id, &taken.canvas, id);
    }
}

/// Enter inline-edit for node `id` iff it's an EDITABLE, full-detail (non-LOD)
/// card. Returns whether it entered edit. The shared seam behind every way to
/// start editing — double-click (`activate_or_edit`), click-again, and Enter/F2
/// on the selected node — so all three behave identically and never edit a
/// non-editable node. status: canvas-inline-edit
fn try_enter_edit(
    app: &mut AppState,
    tab_id: TabId,
    taken: &TakenDoc,
    id: &str,
    viewport: egui::Rect,
) -> bool {
    use crate::panels::canvas::edit;
    let Some(node) = taken.canvas.nodes.iter().find(|n| n.id == id) else {
        return false;
    };
    if !(edit::is_editable(node) && !taken.view_widget.is_node_lod(viewport, node)) {
        return false;
    }
    // Seed the edit overlay at the card's current scroll so clicking into a note
    // keeps its position instead of jumping to the top.
    let scroll = taken.view_widget.card_scroll(id);
    edit::enter(tab_id, node, scroll);
    if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
        pane.editing = Some(id.to_string());
    }
    true
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
/// `SetText` through the same layered-doc user-save path moves use.
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
    // Clear "you're editing" affordance: a bright accent outline around the
    // active overlay so inline-edit mode is unmistakable — distinct from the
    // plain selection outline. Painted on a foreground layer so it sits above the
    // card. status: canvas-inline-edit
    ui.ctx()
        .layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new(("canvas-edit-outline", tab_id)),
        ))
        .rect_stroke(
            edit_rect.expand(3.0),
            egui::CornerRadius::same(6),
            egui::Stroke::new(2.0, theme::accent()),
            egui::StrokeKind::Outside,
        );
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
/// canvas and persist it through the same layered-doc path `persist_canvas` uses, so a
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

/// FOLLOW seam: if this canvas tab links to a source group, resolve that
/// group's active note and — when it changed since last frame — single-select
/// and center the file-node referencing it. No-op when unlinked, when the
/// followed note hasn't changed, or when no node references it.
/// status: tab-linking
fn apply_follow(app: &mut AppState, tab_id: TabId, taken: &mut TakenDoc, viewport: egui::Rect) {
    use hiker_canvas::model::NodeKind;
    let source = app.tab_by_id(tab_id).and_then(|t| t.link.source);
    let Some(path) = crate::editor_pane::followed_note_path(app, source) else {
        return;
    };
    let already = app
        .panels
        .canvases
        .get(&tab_id)
        .and_then(|p| p.followed.clone());
    if already.as_deref() == Some(path.as_str()) {
        return;
    }
    let node_id = taken.canvas.nodes.iter().find_map(|n| match &n.kind {
        NodeKind::File { file, .. } if *file == path => Some(n.id.clone()),
        _ => None,
    });
    if let Some(node_id) = node_id {
        taken.view_widget.focus_node(viewport, &taken.canvas, &node_id);
    }
    if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
        pane.followed = Some(path);
    }
}

/// One-shot "snap to node" seam: when the pane carries a `focus_note_pending`
/// (set by `canvas::open_focused` from the "Appears in" sidebar), single-select
/// and center the file-node referencing that note, then clear the flag. No-op
/// when nothing is pending or no node references the note. Mirrors `apply_follow`
/// but fires once per request rather than tracking a linked group.
/// status: canvas-appears-in
fn apply_pending_focus(app: &mut AppState, tab_id: TabId, taken: &mut TakenDoc, viewport: egui::Rect) {
    use hiker_canvas::model::NodeKind;
    let Some(note) = app
        .panels
        .canvases
        .get_mut(&tab_id)
        .and_then(|p| p.focus_note_pending.take())
    else {
        return;
    };
    let node_id = taken.canvas.nodes.iter().find_map(|n| match &n.kind {
        NodeKind::File { file, .. } if *file == note => Some(n.id.clone()),
        _ => None,
    });
    if let Some(node_id) = node_id {
        taken.view_widget.focus_node(viewport, &taken.canvas, &node_id);
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
    /// The Poincaré overview minimap, carried out alongside the widget so the
    /// panel can render the corner overview / expand-swap and sync the camera in
    /// the same borrow window. status: canvas-minimap
    overview: hiker_graph_view::graph_view::minimap::Minimap,
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
        overview: std::mem::replace(
            &mut pane.overview,
            hiker_graph_view::graph_view::minimap::Minimap::new(),
        ),
    })
}

/// Put the document + widget back after the frame.
fn put_pane_doc(app: &mut AppState, tab_id: TabId, taken: TakenDoc) {
    let pane = app.panels.canvases.entry(tab_id).or_default();
    pane.canvas = Some(taken.canvas);
    pane.view_widget = taken.view_widget;
    pane.last_parsed_text = taken.last_parsed_text;
    pane.fit_pending = taken.fit_pending;
    pane.overview = taken.overview;
}

/// Forward binding: re-serialize the live canvas to canonical JSON and mirror
/// it into the layered-doc `working` layer — the SAME dirty/save model the text
/// editor uses (`editor_binding::run`). A canvas edit becomes an uncommitted
/// `working` edit (buffer DIRTY), not a write straight to `accepted`/disk; the
/// fold to `accepted` + the `.canvas` rewrite + the peer poke all happen on
/// Ctrl+S (`save_canvas_document`), exactly like a note save. Skips the write
/// when the serialization is unchanged. Also keeps the shared buffer's editable
/// text in lockstep so a flip to the JSON view shows the just-edited canvas, and
/// updates `last_parsed_text` so the reverse binding doesn't re-parse our own
/// write. status: canvas-layered-binding
fn persist_canvas(app: &mut AppState, path: &str, canvas: &Canvas, last_parsed_text: &mut String) {
    let json = canvas.to_canonical_json();
    if json == *last_parsed_text {
        return;
    }
    let log = &app.vault_session.services.layered;
    let doc_id = match log.doc_id_for_path(path) {
        Ok(Some(id)) => id,
        Ok(None) => {
            app.push_toast(
                format!("Canvas edit failed: no op-log document for {path}"),
                ToastLevel::Error,
            );
            return;
        }
        Err(e) => {
            app.push_toast(format!("Canvas edit failed: {e}"), ToastLevel::Error);
            return;
        }
    };
    // Mirror the serialized canvas into `working` so `materialize_working ==
    // json` and the buffer reads DIRTY (`is_dirty()` is `hash(editor.doc) !=
    // loaded_hash`; we set `editor.doc` to `json` below but DON'T advance
    // `loaded_hash`, so it stays dirty until Ctrl+S commits). This is the
    // canvas analogue of `editor_binding`'s forward step — there's no editor
    // change set to walk (the canvas isn't a text widget), so we replace the
    // full `working` span with the new JSON in one edit.
    if let Err(e) = mirror_json_to_working(log, &doc_id, &json) {
        app.push_toast(format!("Canvas edit failed: {e}"), ToastLevel::Error);
        return;
    }
    // Keep the shared buffer's editable text in lockstep so a flip to the JSON
    // view shows the just-edited canvas. We DON'T touch `loaded_text` /
    // `loaded_hash` — leaving them at the last-saved baseline is what makes
    // `is_dirty()` true, lighting the tab dirty dot like a text buffer.
    if let Some(buf) = app.session.buffers.get_mut(path) {
        buf.set_doc_clamping_selection(&json);
    }
    *last_parsed_text = json;
}

/// Make `materialize_working(doc_id) == json` by replacing the whole current
/// `working` span with `json` in one `apply_working_edit` (`working` is seeded
/// from `accepted` on the first edit, so its length is the current materialized
/// length). The canvas pure-binding step, factored out of [`persist_canvas`] so
/// it runs against a plain `&LayeredDoc` — testable end-to-end against a real `LayeredDoc`
/// without an egui pane (`persist_canvas` itself is wired to `AppState` + the
/// canvas widget). status: canvas-layered-binding
fn mirror_json_to_working(
    log: &hiker_core::editing::LayeredDoc,
    doc_id: &str,
    json: &str,
) -> Result<(), hiker_core::editing::error::Error> {
    // One atomic whole-span replace (`replace_working`) — NOT a length read
    // followed by a separate `apply_working_edit`. With live dirty-buffer sync
    // on, the autosave tick's `commit_working` clears `working` on its own
    // cadence; reading the length and replacing across two locks could race it
    // and tear the buffer. status: canvas-layered-binding
    log.replace_working(doc_id, json)
}

/// Handle Cmd/Ctrl+S while the Canvas view is active. Consumes the chord (so it
/// doesn't double-fire with any global save) and routes by edit state:
/// - editing a **File** node → save that note's shared buffer (`save_buffer`,
///   which folds `working` into `accepted` and rewrites the `.md`);
/// - otherwise → save the **canvas document** itself by committing its layered-doc
///   `working` layer to disk (the canvas buffer is kept clean — edits route
///   through the layered doc, not the editor — so `save_buffer` would no-op; commit
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
    // A File node in inline-edit mode holds its TEXT in the note, not the
    // `.canvas` — save that note's buffer too (a no-op when it isn't dirty).
    // But ALWAYS also commit the canvas document below: spatial edits (move /
    // delete / edge) live in the canvas `working` layer regardless of which node
    // is selected or inline-edited, so the save must never be diverted away from
    // the canvas. status: canvas-inline-edit
    let editing_file = editing_file_path(app, tab_id);
    if let Some(file) = &editing_file {
        if let Err(err) = crate::editor_pane::save_buffer(app, file) {
            app.push_toast(format!("Save failed: {err}"), ToastLevel::Error);
        }
    }
    tracing::info!(target: "hiker::canvas_save", path, editing = ?editing_file, "canvas Ctrl+S");
    save_canvas_document(app, path);
}

/// The vault path of the File node whose note a canvas Ctrl+S should ALSO save:
/// the single SELECTED node, or (falling back) the node in inline-edit mode. A
/// File node's TEXT lives in its `.md`, not the `.canvas`, so saving the canvas
/// also flushes that note. Keying on SELECTION (not just inline-edit) is the fix
/// for "click out of the editor, re-select the node, then save" — selecting it
/// is enough. `None` for a Text node, a non-File / multi / empty selection with
/// nothing inline-edited (then Ctrl+S just saves the canvas). status: canvas-inline-edit
fn editing_file_path(app: &AppState, tab_id: TabId) -> Option<String> {
    use hiker_canvas::model::NodeKind;
    let pane = app.panels.canvases.get(&tab_id)?;
    let canvas = pane.canvas.as_ref()?;
    // A single selected node takes precedence; otherwise the inline-edited node.
    let sel = pane.view_widget.selection();
    let id = if sel.edges.is_empty() && sel.nodes.len() == 1 {
        sel.nodes.iter().next().cloned()
    } else {
        pane.editing.clone()
    }?;
    let node = canvas.nodes.iter().find(|n| n.id == id)?;
    match &node.kind {
        NodeKind::File { file, .. } if !file.trim().is_empty() => Some(file.clone()),
        _ => None,
    }
}

/// Commit the canvas document's layered-doc `working` layer to disk — the canvas
/// Ctrl+S, mirroring `editor_pane::save_buffer`'s commit path. The forward
/// binding (`persist_canvas`) mirrors every spatial edit into `working`, so by
/// the time the user saves, `commit_working` has real content to fold into
/// `accepted` and rewrite the `.canvas` with. On a successful commit it advances
/// the shared buffer's saved baseline (`loaded_text` / `loaded_hash`) to the
/// committed JSON so `is_dirty()` clears (the tab dirty dot goes out), and pokes
/// enrolled peers so the edit syncs — exactly like a note save.
/// status: canvas-inline-edit
fn save_canvas_document(app: &mut AppState, path: &str) {
    let log = &app.vault_session.services.layered;
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
    // The committed text is `materialize_working` — capture it before the commit
    // folds + clears `working`, so the buffer's clean baseline matches exactly
    // what landed on `accepted`/disk.
    let committed = log.materialize_working(&doc_id).map(|c| c.text);
    // Advance the buffer's saved baseline to the (just-)committed JSON so
    // `is_dirty()` clears, like `save_buffer`. Without this the buffer's
    // `editor.doc` (set to the edited JSON by `persist_canvas`) stays ahead of
    // `loaded_hash` and the dirty dot never goes out. Safe whether or not the
    // commit advanced `accepted` (on a no-op, working == accepted already).
    let advance_baseline = |app: &mut AppState| {
        if let Ok(text) = &committed {
            let saved_hash = hiker_core::hash_string(text);
            if let Some(buf) = app.session.buffers.get_mut(path) {
                buf.loaded_text = text.clone();
                buf.loaded_hash = saved_hash;
            }
        }
    };
    match log.commit_working(&doc_id) {
        Ok(true) => {
            // A real commit advanced `accepted`. Clear dirty, nudge the git
            // transport to commit-on-save, and confirm. status: git-commit-on-save
            advance_baseline(app);
            if let Some(git) = &app.vault_session.services.git_sync {
                git.notify_local_change();
            }
            tracing::info!(target: "hiker::canvas_save", path, "canvas committed + poked");
            app.push_toast(format!("Saved {path}"), ToastLevel::Info);
        }
        Ok(false) => {
            // Nothing to commit — the canvas `working` already matched `accepted`
            // (e.g. only an inline-edited note changed, or no spatial edit since
            // the last save). Keep the baseline consistent, but do NOT poke or
            // claim a save: a no-op must not masquerade as a synced change (that
            // was the "saved but didn't sync" symptom).
            //
            // …UNLESS the `.canvas` has since vanished from disk (deleted/moved
            // out-of-band after an autosave): the layered-doc content is unchanged so
            // there is nothing to commit, but the user pressing Ctrl+S plainly
            // expects the file back. Re-materialize `accepted` to disk and report
            // it as a real save. Capture the result before `advance_baseline`
            // takes its `&mut app` borrow (which would conflict with `log`).
            // status: op-log-disk-canonical
            let restored = log.ensure_on_disk(&doc_id);
            advance_baseline(app);
            match restored {
                Ok(true) => {
                    tracing::info!(target: "hiker::canvas_save", path, "canvas restored to disk (file was missing)");
                    app.push_toast(format!("Saved {path}"), ToastLevel::Info);
                }
                Ok(false) => {
                    tracing::info!(target: "hiker::canvas_save", path, "canvas save was a no-op (working == accepted)");
                }
                Err(e) => app.push_toast(format!("Save failed: {e}"), ToastLevel::Error),
            }
        }
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

#[cfg(test)]
mod canvas_working_binding {
    //! The canvas forward binding routed through the layered-doc `working` layer
    //! (`canvas-layered-binding`), driven against a *real* `LayeredDoc` + `Vault` +
    //! `Buffer` — no egui pane. `persist_canvas` itself is wired to `AppState`
    //! and the canvas widget, so the model→json→`working` step is exercised via
    //! the extracted [`super::mirror_json_to_working`] helper plus the real
    //! `commit_working` Save path; the `is_dirty()` semantics are checked against
    //! a real `Buffer` whose `editor.doc`/baseline mirror what `persist_canvas`
    //! sets. The sync half (a committed canvas — including a node-DELETE — landing
    //! on a peer in one settle) lives in `hiker-sync/tests/scenarios.rs`.

    use std::sync::Arc;

    use hiker_canvas::model::Canvas;
    use hiker_core::editing::LayeredDoc;
    use hiker_core::vault::Vault;
    use tempfile::TempDir;

    use super::mirror_json_to_working;
    use crate::buffer::Buffer;

    /// A real layered-doc-backed vault holding `board.canvas`, seeded from `initial`
    /// on disk exactly as `bootstrap` does at vault open, plus the editable
    /// `Buffer` the app opens over it.
    struct Fixture {
        td: TempDir,
        log: Arc<LayeredDoc>,
        buffer: Buffer,
        doc_id: String,
    }

    const PATH: &str = "board.canvas";

    fn setup(initial: &str) -> Fixture {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join(PATH), initial).unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let log = Arc::new(LayeredDoc::open(td.path()).unwrap());
        // The layered-doc bootstrap walk skips `.canvas` (it's not markdown-chunked,
        // so `is_indexable_path` excludes it); the app seeds the canvas doc
        // lazily on open via `ensure_doc` (in `ensure_vault_buffer_loaded`). Do
        // the same so the doc exists with `meta.kind = "canvas"` before edits.
        let doc_id =
            hiker_core::ops::op_writes::ensure_doc(&log, &vault, PATH).unwrap();
        let buffer = Buffer::with_config_and_vault(
            PATH.to_string(),
            initial,
            hiker_core::hash_string(initial),
            None,
            None,
        );
        Fixture { td, log, buffer, doc_id }
    }

    impl Fixture {
        /// Replay exactly what `persist_canvas` does to the editable buffer +
        /// `working` for a live canvas edit: mirror the serialized JSON into
        /// `working` and set `editor.doc` to it WITHOUT advancing the saved
        /// baseline (so the buffer reads dirty).
        fn edit(&mut self, canvas: &Canvas) -> String {
            let json = canvas.to_canonical_json();
            mirror_json_to_working(&self.log, &self.doc_id, &json).unwrap();
            self.buffer.set_doc_clamping_selection(&json);
            json
        }

        /// Replay `save_canvas_document`: commit `working` → `accepted`/disk and
        /// advance the buffer's saved baseline so `is_dirty()` clears.
        fn save(&mut self) {
            let committed = self.log.materialize_working(&self.doc_id).unwrap().text;
            assert!(self.log.commit_working(&self.doc_id).unwrap(), "a working commit");
            self.buffer.loaded_hash = hiker_core::hash_string(&committed);
            self.buffer.loaded_text = committed;
        }

        fn working(&self) -> String {
            self.log.materialize_working(&self.doc_id).unwrap().text
        }
        fn accepted(&self) -> String {
            self.log.materialize_accepted(&self.doc_id).unwrap().text
        }
        fn disk(&self) -> String {
            std::fs::read_to_string(self.td.path().join(PATH)).unwrap()
        }
    }

    /// A canvas with two file-pointer nodes, used as the starting document.
    fn two_node_canvas() -> Canvas {
        let json = r#"{
  "nodes": [
    {"id": "n1", "type": "file", "file": "a.md", "x": 0, "y": 0, "width": 300, "height": 200},
    {"id": "n2", "type": "file", "file": "b.md", "x": 400, "y": 0, "width": 300, "height": 200}
  ],
  "edges": []
}"#;
        Canvas::from_json(json).unwrap()
    }

    #[test]
    fn edit_makes_working_dirty_accepted_unchanged_then_save_clears() {
        // Canvas edit → `working` DIRTY; `accepted` unchanged; Ctrl+S folds
        // `working` into `accepted` + writes the `.canvas`; `is_dirty()` clears.
        let mut fx = setup("{\n  \"nodes\": [],\n  \"edges\": []\n}");
        assert!(!fx.buffer.is_dirty(), "clean on open");
        let accepted_before = fx.accepted();

        // Move a node (here: drop in the two-node canvas via a full edit).
        let canvas = two_node_canvas();
        let json = fx.edit(&canvas);

        assert!(fx.buffer.is_dirty(), "a canvas edit marks the buffer dirty");
        assert_eq!(fx.working(), json, "materialize(working) == the new JSON");
        assert_eq!(fx.accepted(), accepted_before, "accepted is untouched before Save");

        fx.save();
        assert!(!fx.buffer.is_dirty(), "Save clears dirty");
        assert_eq!(fx.accepted(), json, "Save folded working into accepted");
        assert_eq!(fx.disk(), json, "Save rewrote the .canvas on disk");
    }

    #[test]
    fn deleting_a_node_is_a_normal_working_edit_committed_on_save() {
        // The deletion-bug regression at the binding level: removing a node is a
        // plain `working` edit that commits on the FIRST Save (no second change
        // needed). Start with two nodes (committed), delete one, save once.
        let mut fx = setup("{\n  \"nodes\": [],\n  \"edges\": []\n}");
        let two = two_node_canvas();
        let two_json = fx.edit(&two);
        fx.save();
        assert_eq!(fx.accepted(), two_json);

        // Delete n2.
        let mut one = two;
        one.nodes.retain(|n| n.id != "n2");
        let one_json = fx.edit(&one);
        assert!(fx.buffer.is_dirty(), "delete marks dirty");
        assert!(one_json.contains("n1") && !one_json.contains("n2"), "n2 removed from JSON");

        // ONE save commits the delete — no second change required.
        fx.save();
        assert!(!fx.buffer.is_dirty());
        assert_eq!(fx.accepted(), one_json, "the delete folded to accepted on the first Save");
        assert_eq!(fx.disk(), one_json);
        assert!(!fx.disk().contains("n2"), "the deleted node is gone from disk");
    }
}
