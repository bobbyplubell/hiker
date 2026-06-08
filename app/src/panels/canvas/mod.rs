//! Canvas tab: a per-doc spatial editor over a `.canvas` JSON Canvas document.
//!
//! A `.canvas` file is a first-class vault document — it opens in its own tab,
//! its edits ride the op-log exactly like a note, and it syncs across devices.
//! This panel is the *spine*: it loads the file's JSON text through the op-log
//! materialized buffer, parses it into a [`hiker_canvas::model::Canvas`], hosts
//! the [`canvas_view::CanvasView`] editor widget, and persists every edit back
//! through the same op-log user-save path boards use (`op_writes::user_save`),
//! so canvas edits are versioned, undoable, and mergeable like note bytes.
//!
//! A "View as: Canvas / JSON" toggle flips the pane between the spatial editor
//! and the standard editor widget over the raw `.canvas` text (JSON syntax via
//! the existing `tree-sitter-json`), both over the one op-log document —
//! mirroring the board view's `board-view-toggle`.
//!
//! ## Content seam
//!
//! Node *content* is painted through `canvas_view`'s [`NodeContentRenderer`]
//! trait. This panel wires the real all-source content engine
//! ([`content::Engine`]): markdown via a read-only editor widget, image /
//! HTML / sidecar / code embeds, and link cards. The engine is the single spot
//! behind the trait — see the `// content seam:` marker in `render::canvas_body`
//! where it's constructed and the `ContentRenderer` alias below. The engine's
//! heavyweight per-node state lives in a UI-thread-local store keyed by tab +
//! node id, dropped on tab close via [`content::forget`].
//!
//! Implements: canvas-tab, canvas-view-toggle, canvas-oplog-binding,
//! canvas-nav-stack. The file-tree glyph / routing lives in
//! `crate::files::sidebar`; the create flow in `crate::sidebar`.
//
// status: canvas-tab

use eframe::egui;

use canvas_view::content::CardView;
use canvas_view::widget::CanvasView;
use hiker_canvas::geometry::Point;
use hiker_canvas::model::Canvas;
use hiker_core::autosave::{CanvasViewState, CardViewState};

use crate::state::AppState;
use crate::tab::TabId;

pub mod content;
pub mod edit;
pub mod menu;
pub mod overview;
pub(crate) mod render;
pub mod thumbnail;

/// Which render the canvas pane shows. The toggle is a render choice over the
/// one underlying op-log document, not two tabs — switching to `Json` hosts the
/// live editor widget over the `.canvas` text inline (mirroring the board
/// view's Markdown branch), so spatial edits and raw-text edits ride the same
/// document. status: canvas-view-toggle
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Canvas,
    Json,
}

/// Per-canvas-tab local state: the parsed document, the editor widget, the
/// view-as mode, and the text last parsed (so the reverse op-log binding only
/// re-parses when the materialized JSON actually changed).
pub struct Pane {
    /// The live parsed canvas. `None` until first load (or after a parse
    /// failure — see `parse_error`).
    canvas: Option<Canvas>,
    /// The egui adapter: camera, selection, in-session undo stack. View state
    /// only — never serialized. status: canvas-pan-zoom
    view_widget: CanvasView,
    /// Active in-pane render (spatial editor vs. inline JSON editor).
    /// status: canvas-view-toggle
    view: ViewMode,
    /// The materialized `.canvas` text the current `canvas` was parsed from.
    /// The reverse binding compares the live buffer text against this each
    /// frame; a difference (a remote sync edit, an external file change, or our
    /// own JSON-view edit) triggers a re-parse. Selection + camera survive by
    /// node id since the adapter keys on ids. status: canvas-oplog-binding
    last_parsed_text: String,
    /// A clear parse-error message when the `.canvas` text isn't valid JSON
    /// Canvas, so the pane shows an error state (with a JSON escape hatch)
    /// rather than panicking.
    parse_error: Option<String>,
    /// Set right after a fresh-create so the canvas view frames its (empty)
    /// content once on first paint.
    fit_pending: bool,
    /// When `Some`, the inline `+ Link` URL prompt is open with this draft text.
    /// Submitting (non-empty, trimmed) builds a `Link` node and drops it at the
    /// viewport center; closing it clears the draft. status: canvas-node-create
    link_prompt: Option<String>,
    /// The "Insert from vault" autocomplete picker. Opened by the toolbar verb;
    /// a pick builds a `File { file, subpath: None }` pointer node and drops it
    /// at the viewport center via `insert_node_centered`. status:
    /// canvas-insert-from-vault
    insert_picker: crate::widgets::autocomplete_picker::PickerState,
    /// The id of the node currently in inline-edit mode, if any. Entered by
    /// double-clicking a full-detail File or Text card; cleared on Esc, a
    /// click outside the overlay, selecting another node, or the node scrolling
    /// off-screen. The heavyweight per-edit view lives in `edit::EDIT_VIEWS`
    /// (off `AppState`), not here. status: canvas-inline-edit
    editing: Option<String>,
    /// Whether persisted view state (camera pan/zoom + per-card scroll/zoom) has
    /// been applied to `view_widget` yet. Applied once, on the first
    /// `canvas_body` frame for this pane, so a restored canvas opens where the
    /// user left it rather than re-fitting. status: canvas-view-state-persist
    view_restored: bool,
    /// The last note path this canvas FOLLOWED into focus (when linked to a
    /// source group). Dedupes the select-and-center so the camera only moves
    /// when the linked group's active note actually changes, leaving the
    /// user free to pan/zoom in between. status: tab-linking
    followed: Option<String>,
    /// One-shot "snap to the node referencing this note on the next render",
    /// set when the canvas is opened from the "Appears in" sidebar so the view
    /// lands on the referencing file-node rather than the whole board. Consumed
    /// (and cleared) by `apply_pending_focus`, the same posture as `fit_pending`.
    /// status: canvas-appears-in
    focus_note_pending: Option<String>,
    /// The Poincaré OVERVIEW graph-view state: a simplified graph of the canvas
    /// (each card → a coloured node at its canvas position, canvas edges → graph
    /// edges) rendered as a locked Poincaré disk. Drives the corner minimap and
    /// the expand swap; navigating it + swapping back re-centers the canvas on the
    /// focused node. View state only — never serialized. status: canvas-minimap
    overview: hiker_graph_view::graph_view::State,
    /// Whether the corner overview is shown. status: canvas-minimap
    overview_enabled: bool,
    /// Which pane corner the overview occupies. status: canvas-minimap
    overview_corner: overview_layout::Corner,
    /// Overview side as a fraction of the shorter viewport dimension
    /// (`0.12..=0.5`). status: canvas-minimap
    overview_size: f32,
    /// The swap state: when `true` the overview promotes to fill the pane (the
    /// canvas demotes); a swap back re-centers the canvas on the overview's
    /// current focus. status: canvas-minimap
    overview_expanded: bool,
}

/// Panel-owned overview placement config (the panel composes the corner inset
/// itself rather than depending on canvas-view's old minimap fields). status: canvas-minimap
pub mod overview_layout {
    /// Which pane corner the overview is anchored to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum Corner {
        TopLeft,
        TopRight,
        BottomLeft,
        /// The default — bottom-right, out of the toolbar's way.
        #[default]
        BottomRight,
    }
}

impl Default for Pane {
    fn default() -> Self {
        Self {
            canvas: None,
            view_widget: CanvasView::new(),
            view: ViewMode::default(),
            last_parsed_text: String::new(),
            parse_error: None,
            fit_pending: false,
            link_prompt: None,
            insert_picker: crate::widgets::autocomplete_picker::PickerState::default(),
            editing: None,
            view_restored: false,
            followed: None,
            focus_note_pending: None,
            overview: new_overview_state(),
            overview_enabled: false,
            overview_corner: overview_layout::Corner::default(),
            overview_size: 0.26,
            overview_expanded: false,
        }
    }
}

/// A fresh overview graph-view state configured for the canvas minimap: the
/// locked Poincaré projection over a flat (one-color-fallback) style. The dot
/// colors come per-node from the [`overview::CanvasGraphSource`], not the style,
/// and positions are set directly each frame (never force-laid-out). status: canvas-minimap
fn new_overview_state() -> hiker_graph_view::graph_view::State {
    use hiker_graph::LayoutKind;
    use hiker_graph_view::graph_view::{State, Style};
    use hiker_projection::ProjectionKind;
    let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
    state.projection.kind = ProjectionKind::Poincare;
    state.projection.strength = 1.0;
    // The disk is locked to the pane — no view framing to do; never reset `nav`.
    state.needs_fit = false;
    // A minimap reads as bare dots — labels off so a large canvas's hundreds of
    // titles never overlap into a text hairball in the corner.
    state.toggles.show_labels = false;
    state.toggles.show_preview = false;
    state
}

/// Find-or-focus a canvas tab for `path`, opening one if none exists, and
/// record a `NavTarget::File` on the global Back/Forward stack (like opening a
/// note / board / zim archive). Returns the tab id so callers (e.g. the create
/// flow) can seed per-tab state. status: canvas-tab, canvas-nav-stack
pub fn open(app: &mut AppState, path: &str) -> TabId {
    use crate::tab::{Tab, TabKind};
    // Nav history: skip while mid back/forward (`nav.locked` — the index
    // already points at this entry). status: canvas-nav-stack
    if !app.session.nav.locked {
        crate::state::nav_push(app, path);
    }
    if let Some(existing) = app
        .session
        .tabs
        .iter()
        .find(|t| matches!(&t.kind, TabKind::Canvas { path: p } if p == path))
    {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(id, TabKind::Canvas { path: path.to_string() }, true));
    app.session.active_tab = Some(id);
    id
}

/// Open `path` in a canvas tab and queue an initial zoom-to-fit. Used by the
/// create flow so a freshly seeded `.canvas` opens framed. status: canvas-create
pub fn open_fresh(app: &mut AppState, path: &str) {
    let tab_id = open(app, path);
    app.panels.canvases.entry(tab_id).or_default().fit_pending = true;
}

/// Open `path` in a canvas tab flipped to the raw-JSON editor view — the
/// file-tree "View as JSON" escape hatch for hand-editing a `.canvas` file.
/// status: canvas-file-tree-glyph
pub fn open_as_json(app: &mut AppState, path: &str) {
    let tab_id = open(app, path);
    app.panels.canvases.entry(tab_id).or_default().view = ViewMode::Json;
}

/// Open `path` in a canvas tab and queue a one-shot "snap to the file-node that
/// references `note`" for the next render. Drives the "Appears in" sidebar, so
/// clicking a canvas there lands the view on the referencing node (selected)
/// rather than the whole board. status: canvas-appears-in
pub fn open_focused(app: &mut AppState, path: &str, note: &str) {
    let tab_id = open(app, path);
    app.panels.canvases.entry(tab_id).or_default().focus_note_pending = Some(note.to_string());
}

/// Act on a double-clicked node: open a link node's URL in the OS browser, or a
/// file node's referenced vault file in a tab (routing `.canvas` to the canvas
/// view, everything else through the standard open path). Other kinds (text /
/// group) have no activation. The in-place counterpart of
/// [`open_target_in_new_tab`]. status: canvas-link-node-card
pub(crate) fn activate_node(ui: &egui::Ui, app: &mut AppState, tab_id: TabId, canvas: &Canvas, id: &str) {
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
                open(app, file);
            } else {
                // DRIVE: a linked target group opens the note there instead
                // of this canvas's own preview/active slot. status: tab-linking
                let target = app.tab_by_id(tab_id).and_then(|t| t.link.target);
                match crate::editor_pane::drive_target_group(app, target) {
                    Some(group) => {
                        crate::editor_pane::open_file_in_group(app, file, group, true);
                    }
                    None => crate::editor_pane::open_file(app, file, /* sticky */ true),
                }
            }
        }
        _ => {}
    }
}

/// A canvas node's openable target, resolved from its kind for the "Open in new
/// tab" context-menu verb. Mirrors the kinds `activate_node` opens (a File node's
/// referenced file, a Link node's URL); Text / group nodes (and empty targets)
/// have none. status: canvas-open-in-new-tab
pub(crate) enum OpenTarget {
    /// A File node's referenced vault file (routed by extension when opened).
    File(String),
    /// A Link node's URL.
    Url(String),
}

/// Resolve node `id`'s openable target, or `None` for a Text / group node or an
/// empty File / Link target — matching the openability the node context menu
/// gates its "Open in new tab" item on. status: canvas-open-in-new-tab
pub(crate) fn node_open_target(canvas: &Canvas, id: &str) -> Option<OpenTarget> {
    use hiker_canvas::model::NodeKind;
    let node = canvas.nodes.iter().find(|n| n.id == id)?;
    match &node.kind {
        NodeKind::File { file, .. } if !file.trim().is_empty() => Some(OpenTarget::File(file.clone())),
        NodeKind::Link { url } if !url.trim().is_empty() => Some(OpenTarget::Url(url.clone())),
        _ => None,
    }
}

/// Open a resolved [`OpenTarget`] in a NEW tab, leaving the current canvas tab in
/// place: a File node's referenced file gets a fresh editor tab (extension-routed
/// to the canvas / chart / graph view as usual via `open_file_new_tab`), and a
/// Link node's URL opens in a new browser tab. The new-tab counterpart of
/// `activate_node`'s in-place open. status: canvas-open-in-new-tab
pub(crate) fn open_target_in_new_tab(ui: &egui::Ui, app: &mut AppState, target: OpenTarget) {
    match target {
        OpenTarget::File(file) => {
            tracing::debug!(target = %file, "canvas: open node target in new tab");
            crate::editor_pane::open_file_new_tab(app, &file);
        }
        OpenTarget::Url(url) => ui.ctx().open_url(egui::OpenUrl::new_tab(url)),
    }
}

/// The vault path of the note currently inline-edited on canvas tab `tab_id` —
/// the `file` of the File node in edit mode. `None` when nothing is being edited,
/// the edited node is a Text node, or its path is empty. Lets the host treat the
/// edited note as the "active note" so the context panel (backlinks / related /
/// appears-in) follows what you're editing on the canvas rather than the
/// `.canvas` file itself. status: canvas-inline-edit
#[must_use]
pub fn inline_edited_note(app: &AppState, tab_id: TabId) -> Option<String> {
    let pane = app.panels.canvases.get(&tab_id)?;
    let node_id = pane.editing.as_deref()?;
    let canvas = pane.canvas.as_ref()?;
    canvas.nodes.iter().find(|n| n.id == node_id).and_then(|n| match &n.kind {
        hiker_canvas::model::NodeKind::File { file, .. } if !file.trim().is_empty() => {
            Some(file.clone())
        }
        _ => None,
    })
}

/// Render the canvas tab body. Mirrors `panels::board::show`.
pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    path: &str,
    rt: &std::sync::Arc<tokio::runtime::Runtime>,
) {
    // Ensure the op-log buffer is loaded so both views share the one document
    // (the JSON view hosts the editor widget over it; the Canvas view reads its
    // live text for the reverse binding). status: canvas-oplog-binding
    if !crate::editor_pane::ensure_vault_buffer_loaded(app, path) {
        ui.colored_label(render::error_color(), "Couldn't load this .canvas file.");
        return;
    }
    // The toolbar/header keeps a padded band of its own (the pane itself is
    // edge-to-edge, so without this the title would sit flush against the window
    // chrome). The canvas body below is edge-to-edge and sits FLUSH under the
    // toolbar — no separator line, no inter-element gap.
    egui::Frame::default()
        .inner_margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 0 })
        .show(ui, |ui| {
            render::header(ui, app, tab_id, path);
        });
    ui.spacing_mut().item_spacing.y = 0.0;

    let view = app
        .panels
        .canvases
        .get(&tab_id)
        .map(|p| p.view)
        .unwrap_or_default();
    match view {
        ViewMode::Canvas => render::canvas_body(ui, app, tab_id, path),
        ViewMode::Json => {
            // Host the live editor widget over the `.canvas` text inline, in
            // this same tab — a render choice over the one op-log document, not
            // a separate buffer tab. JSON highlighting comes from the existing
            // `tree-sitter-json` by extension. status: canvas-view-toggle
            crate::panels::buffer::show(ui, app, path, rt);
        }
    }
}

impl Pane {
    /// Re-read the buffer's live text and re-parse into `canvas` when it differs
    /// from `last_parsed_text`. The forward (edit → op-log) direction lives in
    /// `render::canvas_body`; this is the reverse direction — a remote sync
    /// edit, an external file change, or our own JSON-view edit advancing the
    /// materialized text. Selection + camera survive because the adapter keys on
    /// node ids, not text offsets. status: canvas-oplog-binding
    fn sync_from_text(&mut self, text: &str) {
        if self.canvas.is_some() && text == self.last_parsed_text {
            return;
        }
        match Canvas::from_json(text) {
            Ok(parsed) => {
                self.canvas = Some(parsed);
                self.last_parsed_text = text.to_string();
                self.parse_error = None;
            }
            Err(e) => {
                self.parse_error = Some(e.to_string());
                // Keep any previously-parsed canvas so a transient bad edit in
                // the JSON view doesn't discard live state; the error banner
                // routes the user to the JSON view to fix it.
            }
        }
    }
}

/// Every `.canvas` document in the vault as `(rel_path, title)`, sorted by
/// title — the list the "Add to canvas…" submenu shows. Walks the vault for the
/// `.canvas` extension (the cheap extension check `is_canvas_doc` uses), skipping
/// watcher-ignored subtrees. The title is the basename without `.canvas`.
/// status: canvas-add-to-canvas-verb
#[must_use]
pub fn list_canvases(vault: &hiker_core::vault::Vault) -> Vec<(String, String)> {
    let root = vault.root();
    let mut out: Vec<(String, String)> = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let Ok(rel) = e.path().strip_prefix(root) else {
                return true;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            rel_str.is_empty() || !hiker_core::watcher::is_ignored(&rel_str)
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !rel_str.ends_with(".canvas") {
                return None;
            }
            let title = rel_str
                .rsplit('/')
                .next()
                .and_then(|n| n.strip_suffix(".canvas"))
                .unwrap_or(&rel_str)
                .to_string();
            Some((rel_str, title))
        })
        .collect();
    out.sort_by_key(|(_, title)| title.to_lowercase());
    out
}

/// Insert a `File` pointer node referencing `vault_rel` into the `.canvas` at
/// `canvas_rel`, whether or not that canvas is currently open — the right-click
/// "Add to canvas…" write path, mirroring `panels::board::add_card`'s
/// open-or-closed posture. Reads the canvas's current text (the open buffer if
/// present, else disk = materialized accepted), parses it, and appends a
/// uniquely-id'd pointer at a non-overlapping cascade position.
///
/// Routing follows the dirty/save model the in-editor binding now uses
/// (`canvas-oplog-binding`):
/// - **Open canvas** → the edit is mirrored into the op-log `working` layer
///   (the buffer goes DIRTY) exactly like a spatial edit, and the user commits
///   it with Ctrl+S. The reverse binding re-parses the new working text next
///   frame, so the node appears on the open canvas immediately. We don't touch
///   `loaded_hash`, so the dirty dot lights up.
/// - **Closed canvas** → there's no open buffer/tab to hold a dirty state, so
///   the edit commits straight to `accepted` + disk via `op_writes::user_save`,
///   the same one-shot posture `board::add_card` has for a closed board.
///
/// status: canvas-add-to-canvas-verb
pub fn add_file_node(app: &mut AppState, canvas_rel: &str, vault_rel: &str) {
    use crate::state::ToastLevel;
    let is_open = app.session.buffers.contains_key(canvas_rel);
    let current = app
        .session
        .buffers
        .get(canvas_rel)
        .map(crate::buffer::Buffer::current_text)
        .or_else(|| app.vault_session.vault.read_file(canvas_rel).ok())
        .unwrap_or_default();
    let mut canvas = match Canvas::from_json(&current) {
        Ok(c) => c,
        Err(e) => {
            app.push_toast(format!("Add to canvas failed: {e}"), ToastLevel::Error);
            return;
        }
    };
    let node = build_file_pointer(&canvas, vault_rel);
    canvas.nodes.push(node);
    let json = canvas.to_canonical_json();
    let log = &app.vault_session.services.oplog;
    if is_open {
        // Open canvas: route through `working` so it's a dirty edit the user
        // saves with Ctrl+S, consistent with `render::persist_canvas`.
        let doc_id = match log.doc_id_for_path(canvas_rel) {
            Ok(Some(id)) => id,
            Ok(None) | Err(_) => {
                app.push_toast("Add to canvas failed: no op-log document".to_string(), ToastLevel::Error);
                return;
            }
        };
        let mirror = log
            .materialize_working(&doc_id)
            .and_then(|c| log.apply_working_edit(&doc_id, 0, c.text.len(), &json));
        match mirror {
            Ok(()) => {
                // Lockstep the editable buffer (DIRTY — loaded baseline left as
                // the last save) so the JSON view + dirty dot follow.
                if let Some(buf) = app.session.buffers.get_mut(canvas_rel) {
                    buf.set_doc_clamping_selection(&json);
                }
                app.push_toast("Added to canvas".to_string(), ToastLevel::Info);
            }
            Err(e) => app.push_toast(format!("Add to canvas failed: {e}"), ToastLevel::Error),
        }
        return;
    }
    // Closed canvas: no dirty-buffer surface, so commit straight to disk.
    let result = hiker_core::ops::op_writes::user_save(
        &app.vault_session.services.oplog,
        &app.vault_session.vault,
        canvas_rel,
        &json,
    );
    match result {
        Ok(()) => app.push_toast("Added to canvas".to_string(), ToastLevel::Info),
        Err(e) => app.push_toast(format!("Add to canvas failed: {e}"), ToastLevel::Error),
    }
}

/// Create a fresh blank vault note and drop a `File` pointer to it onto the
/// canvas at `tab_id`. The shared entry point for both the right-click "New
/// note" context-menu verb and the Cmd/Ctrl+N binding when a canvas tab is
/// active. Mints the note through the sidebar's `create_new_note` (suffix-counted,
/// indexed, no tab opened) and queues a centered insert on the pane's widget —
/// `insert_node_centered` only sets a pending insert consumed on the next
/// `show`, so no taken-doc dance is needed. The dropped node renders via the
/// content engine and is ready to inline-edit. Toasts on failure; never panics.
/// status: canvas-new-note
pub(crate) fn new_note_on_canvas(app: &mut AppState, tab_id: TabId) {
    use crate::state::ToastLevel;
    let rel = match app.create_new_note() {
        Ok(rel) => rel,
        Err(err) => {
            app.push_toast(format!("Create failed: {err}"), ToastLevel::Error);
            return;
        }
    };
    if let Some(pane) = app.panels.canvases.get_mut(&tab_id) {
        pane.view_widget.insert_node_centered(new_file_pointer(&rel));
        app.push_toast(format!("Created {rel}"), ToastLevel::Info);
    }
}

/// Build a fresh `File` pointer node referencing `rel` at the default file-node
/// size. Its id / position are overwritten by `insert_node_centered` when it
/// drops; the node stores only the vault path, never the content.
/// status: canvas-new-note, canvas-file-node-embed
fn new_file_pointer(rel: &str) -> hiker_canvas::model::Node {
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

/// Build a `File` pointer node for `vault_rel` with a canvas-unique id and a
/// non-overlapping position: offset from the content bounds' bottom-right with a
/// small per-node-count cascade, so repeated inserts fan out rather than stack.
/// Default file-node size (300×200). status: canvas-add-to-canvas-verb
fn build_file_pointer(canvas: &Canvas, vault_rel: &str) -> hiker_canvas::model::Node {
    use hiker_canvas::geometry::content_bounds;
    use hiker_canvas::model::{Node, NodeKind};
    let id = mint_node_id(canvas);
    let cascade = (canvas.nodes.len() as i64) * 40;
    let (x, y) = content_bounds(canvas).map_or((0, 0), |b| {
        (b.x.round() as i64 + cascade, b.bottom().round() as i64 + 40 + cascade)
    });
    Node {
        id,
        x,
        y,
        width: 300,
        height: 200,
        color: None,
        kind: NodeKind::File { file: vault_rel.to_string(), subpath: None },
        extra: std::collections::BTreeMap::new(),
    }
}

/// Mint a node id not already present on `canvas` (the `nN` scheme the canvas
/// view widget uses). status: canvas-add-to-canvas-verb
fn mint_node_id(canvas: &Canvas) -> String {
    for n in 1.. {
        let id = format!("n{n}");
        let taken = canvas.nodes.iter().any(|node| node.id == id)
            || canvas.edges.iter().any(|edge| edge.id == id);
        if !taken {
            return id;
        }
    }
    unreachable!("u64 range exhausted minting a canvas node id")
}

/// Apply persisted view state to a freshly-created canvas pane, once. Called on
/// the first `canvas_body` frame for `tab_id` (when `path` is known): if the
/// session map has saved view state for `path`, convert it and call
/// `restore_view`, which sets the camera + per-card scroll/zoom. Suppresses the
/// fresh-create `fit_pending` so a restored canvas opens where the user left it
/// rather than re-fitting. The `view_restored` guard makes this idempotent.
/// status: canvas-view-state-persist
pub(crate) fn apply_persisted_view(app: &mut AppState, tab_id: TabId, path: &str) {
    let saved = app.session.canvas_views.get(path).cloned();
    let Some(pane) = app.panels.canvases.get_mut(&tab_id) else {
        return;
    };
    if pane.view_restored {
        return;
    }
    pane.view_restored = true;
    let Some(state) = saved else {
        return;
    };
    let (pan, scale, cards) = view_state_to_snapshot(&state);
    pane.view_widget.restore_view(pan, scale, cards);
    // A restored camera wins over fresh-create framing.
    pane.fit_pending = false;
}

/// Snapshot the pane's current view state into the session map under `path`, so
/// it survives the pane being dropped (tab close) and feeds tab-state
/// persistence on exit. status: canvas-view-state-persist
pub(crate) fn capture_view(app: &mut AppState, tab_id: TabId, path: &str) {
    let Some(pane) = app.panels.canvases.get(&tab_id) else {
        return;
    };
    let snapshot = pane.view_widget.view_snapshot();
    app.session
        .canvas_views
        .insert(path.to_string(), snapshot_to_view_state(&snapshot));
}

/// Convert a snapshotted `(pan, scale, cards)` into the serializable
/// [`CanvasViewState`]. status: canvas-view-state-persist
fn snapshot_to_view_state(snapshot: &(Point, f32, Vec<(String, CardView)>)) -> CanvasViewState {
    let (pan, scale, cards) = snapshot;
    CanvasViewState {
        pan_x: pan.x,
        pan_y: pan.y,
        scale: *scale,
        cards: cards
            .iter()
            .map(|(id, c)| (id.clone(), CardViewState { zoom: c.zoom, scroll_y: c.scroll_y }))
            .collect(),
    }
}

/// Convert a stored [`CanvasViewState`] back into the `restore_view` arguments.
/// status: canvas-view-state-persist
fn view_state_to_snapshot(state: &CanvasViewState) -> (Point, f32, Vec<(String, CardView)>) {
    let cards = state
        .cards
        .iter()
        .map(|(id, c)| (id.clone(), CardView { zoom: c.zoom, scroll_y: c.scroll_y }))
        .collect();
    (Point::new(state.pan_x, state.pan_y), state.scale, cards)
}

/// The node-content renderer the canvas view paints with: the real all-source
/// content engine ([`content::Engine`]) behind `canvas_view`'s
/// [`NodeContentRenderer`] trait.
///
/// content seam: this alias + the constructor in `render::canvas_body` are the
/// single spot the spine reserves for the content engine; nothing else in the
/// spine touches node content. status: canvas-node-content-trait
pub(crate) type ContentRenderer = content::Engine;
