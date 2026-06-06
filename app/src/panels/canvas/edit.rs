//! Inline-edit mode for canvas cards: the focused editable overlay a card
//! becomes on double-click, and the aliasing-safe storage for its heavyweight
//! per-edit view.
//!
//! A canvas card renders read-only by default (`content::Engine`); double-clicking
//! a full-detail File or Text node enters edit mode. The pane records the editing
//! node id ([`super::Pane::editing`]); the heavyweight view for that one edit —
//! a [`buffer_view::EmbeddedView`] for a File node, or a transient `editor-egui`
//! editor for a Text node — lives in the [`EDIT_VIEWS`] thread-local keyed by
//! `TabId`, NOT on `AppState`. That separation is load-bearing: rendering a File
//! node calls [`buffer_view::show_embedded_buffer`], which needs `&mut AppState`
//! AND `&mut EmbeddedView` at once, so the view cannot live inside `app`.
//!
//! Two write paths by kind:
//! - **File node** → edits the one shared `session.buffers[path]` editor through
//!   the reusable embedded buffer view, so typing on the canvas shows in any open
//!   tab of that note and rides the op-log binding (one dirty buffer, save /
//!   autosave / agent-review for free).
//! - **Text node** → edits the node's own `text` (which lives in the `.canvas`,
//!   not a vault note), committing an [`EditOp::SetText`] through the pane's
//!   `persist_canvas` path on every change.
//!
//! status: canvas-inline-edit

use std::cell::RefCell;
use std::collections::HashMap;

use eframe::egui;

use editor_core::state::Editor as EditorState;
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::ViewState;
use hiker_canvas::model::{Node, NodeKind};
use hiker_canvas::ops::EditOp;

use crate::buffer::DecorationCache;
use crate::buffer_view::{EmbeddedView, EmbedOpts};
use crate::panels::buffer::decorations::{rebuild_editor_decorations, DecoRebuildCtx};
use crate::state::AppState;
use crate::tab::TabId;

// Heavyweight per-edit view state, keyed by `TabId`, parked off `AppState` so
// the File-node path can hold `&mut AppState` and `&mut EmbeddedView` at the
// same time without aliasing. Mirrors `content::PANES`; dropped on tab close
// (and on edit exit) via `forget`. status: canvas-inline-edit
thread_local! {
    static EDIT_VIEWS: RefCell<HashMap<TabId, EditView>> = RefCell::new(HashMap::new());
}

/// The kind-specific editable view behind one in-progress canvas card edit. The
/// overlay requests keyboard focus every frame *only while it lacks it* (the
/// request is a no-op once focused, so it doesn't fight click-to-place-caret) —
/// needed because the canvas interaction surface would otherwise hold focus and
/// the editor would never see typing / Backspace.
enum EditView {
    /// A File node attaches to the shared note buffer via the reusable embed.
    File { embed: Box<EmbeddedView> },
    /// A Text node edits its own `.canvas` `text` in a transient editor whose
    /// document is reconciled back into the node via `SetText`.
    Text { edit: Box<TextEdit> },
}

/// The transient editor backing a Text-node edit: an `editor-egui` editor over
/// the node's body, its own view / paint / decoration caches, and a mirror of
/// the last body we wrote out (so a change is detected without re-diffing the
/// whole `.canvas`).
struct TextEdit {
    editor: EditorState,
    view: ViewState,
    paint: PaintCache,
    decorations: DecorationCache,
    /// The node body as of the last committed `SetText` (or the initial text),
    /// so an unchanged frame skips the persist path.
    last: String,
}

impl TextEdit {
    fn new(text: &str) -> Self {
        let mut view = ViewState { font_size: 14.0, hide_gutter: true, ..ViewState::default() };
        view.wrap_map.set_enabled(true);
        Self {
            editor: EditorState::new(text),
            view,
            paint: PaintCache::default(),
            decorations: DecorationCache::default(),
            last: text.to_string(),
        }
    }
}

/// Drop the in-progress edit view for `tab_id`. Called from the canvas tab-close
/// path (beside `content::forget`) and whenever edit mode exits, so a focused
/// editor's galley caches don't leak. status: canvas-inline-edit
pub fn forget(tab_id: TabId) {
    EDIT_VIEWS.with(|views| {
        views.borrow_mut().remove(&tab_id);
    });
}

/// Whether `node` is an inline-editable kind (File or Text). Link and Group
/// nodes keep their existing double-click activation (open URL / no-op).
/// status: canvas-inline-edit
#[must_use]
pub const fn is_editable(node: &Node) -> bool {
    matches!(node.kind, NodeKind::File { .. } | NodeKind::Text { .. })
}

/// Seed the [`EDIT_VIEWS`] entry for a freshly-entered edit of `node`, picking
/// the File or Text view by kind. Replaces any prior entry for the tab (only one
/// card edits at a time). status: canvas-inline-edit
pub fn enter(tab_id: TabId, node: &Node, scroll: f32) {
    let view = match &node.kind {
        NodeKind::Text { text } => {
            let mut e = TextEdit::new(text);
            // Seed the editor at the card's current scroll so entering edit mode
            // keeps the user's position instead of jumping to the top.
            e.view.scroll_y = scroll.max(0.0);
            EditView::Text { edit: Box::new(e) }
        }
        // File (and anything else routed here) attaches to the shared buffer.
        _ => {
            let mut embed = EmbeddedView::new();
            embed.view.scroll_y = scroll.max(0.0);
            EditView::File { embed: Box::new(embed) }
        }
    };
    EDIT_VIEWS.with(|views| {
        views.borrow_mut().insert(tab_id, view);
    });
}

/// Whether the editing node is still on screen: its current screen rect must
/// intersect the viewport, else edit mode auto-exits (the spec's "editing node
/// going off-screen" exit). status: canvas-inline-edit
#[must_use]
pub fn node_on_screen(edit_rect: egui::Rect, viewport: egui::Rect) -> bool {
    edit_rect.intersects(viewport)
}

/// Whether a pointer button was pressed this frame at a position OUTSIDE the
/// overlay rect — the click-outside exit (the spec's "clicking empty canvas, or
/// selecting another node exits edit mode"). A press with no interact position
/// (a keyboard-only frame) is not an outside click; a press inside `edit_rect`
/// (including the entering double-click, which lands on the node) is not either.
/// status: canvas-inline-edit
#[must_use]
pub fn press_outside(ui: &egui::Ui, edit_rect: egui::Rect) -> bool {
    ui.input(|i| {
        i.pointer.any_pressed()
            && i.pointer
                .interact_pos()
                .is_some_and(|p| !edit_rect.contains(p))
    })
}

/// Render the inline-edit overlay for `node` over `edit_rect` (the node's
/// on-screen rect, computed by the caller before the widget was put back).
///
/// Draws an `egui::Area` on the foreground layer — above the canvas interaction
/// surface — so it captures keyboard + pointer, clipped to the node rect. Returns
/// `true` if the user pressed Escape inside the overlay this frame (the caller
/// then exits edit mode). The File path delegates to
/// [`buffer_view::show_embedded_buffer`] (pulling the `EmbeddedView` out of the
/// thread-local so it doesn't alias `&mut app`); the Text path renders a transient
/// editor and persists changes through `persist`. status: canvas-inline-edit
pub fn show_overlay(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    node: &Node,
    edit_rect: egui::Rect,
    persist: &mut dyn FnMut(&mut AppState, &EditOp),
) -> bool {
    let id = ui.id().with(("canvas-inline-edit", tab_id.0, node.id.clone()));
    let mut escaped = false;
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(edit_rect.min)
        .show(ui.ctx(), |ui| {
            ui.set_clip_rect(edit_rect);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(edit_rect));
            child.set_clip_rect(edit_rect);
            egui::Frame::canvas(child.style()).show(&mut child, |ui| {
                ui.set_min_size(edit_rect.size());
                escaped = render_body(ui, app, tab_id, node, persist);
            });
        });
    escaped
}

/// Render the editable body for the editing node inside the overlay, by kind.
/// Reports an Escape press so the caller can exit edit mode.
fn render_body(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    node: &Node,
    persist: &mut dyn FnMut(&mut AppState, &EditOp),
) -> bool {
    let escaped = ui.input(|i| i.key_pressed(egui::Key::Escape));
    match &node.kind {
        NodeKind::File { file, .. } if !file.trim().is_empty() => {
            show_file_edit(ui, app, tab_id, file);
        }
        NodeKind::Text { .. } => {
            show_text_edit(ui, tab_id, &node.id, persist, app);
        }
        // A File node with an empty path, or any other kind, has nothing to edit.
        _ => {}
    }
    escaped
}

/// Edit a File node: attach to the shared `session.buffers[path]` editor through
/// the reusable embed. The `EmbeddedView` is pulled out of the thread-local for
/// the call so `show_embedded_buffer` can hold `&mut app` and `&mut embed` at
/// once without aliasing — the thread-local entry never overlaps the `&mut app`
/// borrow. Markdown live-preview is on for `.md` / text-like files, off for
/// other types. status: canvas-inline-edit
fn show_file_edit(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, file: &str) {
    EDIT_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(EditView::File { embed }) = views.get_mut(&tab_id) else {
            return;
        };
        // Request focus every frame the editor lacks it (a no-op once focused),
        // so the canvas surface can't hold keyboard focus away from the editor —
        // otherwise typing / Backspace never reach it. status: canvas-inline-edit
        let opts = EmbedOpts {
            read_only: false,
            markdown: is_markdownish(file),
            font_size: 14.0,
            focus: true,
        };
        crate::buffer_view::show_embedded_buffer(ui, app, file, embed, &opts);
    });
}

/// Edit a Text node: render the transient editor over the node's body, then
/// reconcile any change back into the `.canvas` via `SetText`. The editor is the
/// source of truth while editing; on every changed frame we persist the new body
/// (the op-log binding folds repeated small edits, like the move path does).
/// status: canvas-text-node-markdown
fn show_text_edit(
    ui: &mut egui::Ui,
    tab_id: TabId,
    node_id: &str,
    persist: &mut dyn FnMut(&mut AppState, &EditOp),
    app: &mut AppState,
) {
    let theme = editor_core::theme::light_default();
    let dpr = ui.ctx().pixels_per_point();
    let changed = EDIT_VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        let Some(EditView::Text { edit }) = views.get_mut(&tab_id) else {
            return None;
        };
        render_text_widget(ui, edit, &theme, dpr);
        let body = edit.editor.doc.to_string();
        if body == edit.last {
            return None;
        }
        edit.last = body.clone();
        Some(body)
    });
    if let Some(text) = changed {
        persist(app, &EditOp::SetText { id: node_id.to_string(), text });
    }
}

/// Run the editor widget for a Text-node edit: the same markdown decoration
/// pipeline a text-node card uses, but editable. Mirrors `buffer_view`'s render
/// wiring, scoped to the transient editor.
fn render_text_widget(
    ui: &mut egui::Ui,
    edit: &mut TextEdit,
    theme: &editor_core::theme::Theme,
    dpr: f32,
) {
    let body = edit.editor.doc.to_string();
    let font_px = edit.view.font_size;
    let mut deco_ctx = DecoRebuildCtx {
        cache: &mut edit.decorations,
        folds: &EMPTY_FOLDS,
        loaded_text: &body,
        theme: Some(theme),
        live_preview: true,
        render_widgets: true,
        is_markdown: true,
        dpr,
        font_px,
        chunk_boundaries: false,
        show_whitespace: false,
        highlight_trailing_whitespace: false,
        diff: None,
        resolve_title: None,
        // Canvas node editing renders through the in-memory caches only.
        // status: widget-render-disk-cache
        diagram_cache: None,
    };
    let mut rebuild = |state: &EditorState, view: &mut ViewState| {
        rebuild_editor_decorations(state, view, &mut deco_ctx);
    };
    let response = EditorWidget::new(&mut edit.editor, &mut edit.view)
        .with_paint_cache(&mut edit.paint)
        .with_decoration_rebuild(&mut rebuild)
        .show(ui);
    // Hold keyboard focus while editing (no-op once focused) so the canvas
    // surface can't steal it — otherwise typing / Backspace never land.
    if !response.has_focus() {
        response.request_focus();
    }
}

/// Whether a file should render with the markdown live-preview providers in the
/// inline editor (the editable mirror of `content::plan_file`'s markdown branch):
/// markdown and the extension-less / `.txt` text bodies. Other types still edit
/// as plain text. A non-markdown editable body is rare on a canvas, but the embed
/// handles it gracefully.
fn is_markdownish(file: &str) -> bool {
    let ext = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    matches!(ext.as_str(), "md" | "markdown" | "txt" | "")
}

/// An always-empty fold set for the Text-node decoration rebuild (a canvas card
/// never folds), borrowed `'static` so it isn't allocated per frame.
static EMPTY_FOLDS: std::sync::LazyLock<std::collections::HashSet<u64>> =
    std::sync::LazyLock::new(std::collections::HashSet::new);
