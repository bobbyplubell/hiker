//! A realistic [`NodeContentRenderer`] for the profiler — a faithful, trimmed
//! mirror of `app::panels::canvas::content`.
//!
//! Each non-group node is rendered by reproducing the app's real markdown cost:
//! a read-only `editor-egui` widget over the node's text, with the live-preview
//! markdown decoration layers rebuilt every frame through the widget's
//! decoration hook (the per-frame churn that dominates at zoom-to-fit). The
//! heavyweight editor state (rope, galley caches, paint cache) is cached per
//! node id behind a content fingerprint, exactly like the app's `PANES` store,
//! so steady-state frames hit the cache and only pay the decoration rebuild +
//! layout/paint — the realistic steady state.
//!
//! Every `render` call accumulates its wall time into a per-frame counter the
//! harness reads and resets, so the profiler can report the content-render
//! share of frame time. It also counts how many nodes it was called for this
//! frame (the view only calls us for nodes whose card rect intersects the
//! viewport), giving the visible-node count per zoom level.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use editor_core::state::Editor as EditorState;
use editor_core::theme::{dark_default, Theme as EditorTheme};
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::ViewState;
use hiker_canvas::model::{Node, NodeKind};

use canvas_view::content::{CardView, NodeContentRenderer};
use canvas_view::menu::{CanvasMenuRenderer, EdgeMenuAction, EmptyMenuAction, NodeMenuAction};

/// A hosted read-only editor for one node's markdown / text body, plus the
/// fingerprint of the content it was built for.
struct NodePane {
    /// Fingerprint of the content the editor was built for; a change rebuilds.
    fingerprint: String,
    editor: EditorState,
    view: ViewState,
    paint: PaintCache,
}

/// The realistic content engine: a per-node editor cache plus content-time
/// instrumentation.
pub struct ProfRenderer {
    vault_root: PathBuf,
    panes: HashMap<String, NodePane>,
    /// Microseconds spent inside `render` since the last reset.
    content_micros: Cell<u128>,
    /// Nodes rendered (called for) since the last reset.
    rendered: Cell<usize>,
    /// The render count captured at the most recent reset boundary — the
    /// visible-node count for the level being measured.
    last_visible: Cell<usize>,
}

impl ProfRenderer {
    /// Build a renderer resolving file nodes against `vault_root`.
    pub fn new(vault_root: PathBuf) -> Self {
        Self {
            vault_root,
            panes: HashMap::new(),
            content_micros: Cell::new(0),
            rendered: Cell::new(0),
            last_visible: Cell::new(0),
        }
    }

    /// Reset the per-frame content-time and rendered-node counters. Call before
    /// each timed frame; the previous frame's rendered count is latched as the
    /// visible-node count.
    pub fn reset_content_timer(&self) {
        self.last_visible.set(self.rendered.get());
        self.content_micros.set(0);
        self.rendered.set(0);
    }

    /// Microseconds spent in content rendering during the last frame.
    pub fn content_micros(&self) -> u128 {
        self.content_micros.get()
    }

    /// The visible-node count latched at the last reset.
    pub fn last_visible(&self) -> usize {
        self.last_visible.get()
    }

    /// Resolve a node into the markdown/text body to render and a fingerprint,
    /// or `None` for a node we render cheaply (link / missing file / unknown).
    fn body_for(&self, node: &Node) -> Option<(String, String, bool)> {
        match &node.kind {
            NodeKind::Text { text } => {
                Some((text.clone(), format!("text:{}", hash(text)), true))
            }
            NodeKind::File { file, .. } => self.file_body(file),
            NodeKind::Link { .. } | NodeKind::Group { .. } => None,
        }
    }

    /// Resolve a file node's body: markdown for `.md`, plain text for other
    /// readable files. Returns `(text, fingerprint, is_markdown)`.
    fn file_body(&self, file: &str) -> Option<(String, String, bool)> {
        let abs = self.vault_root.join(file);
        let meta = std::fs::metadata(&abs).ok()?;
        let text = std::fs::read_to_string(&abs).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        let is_md = extension(file) == "md";
        let fp = format!("file:{}:{}:{}", abs.display(), meta.len(), mtime);
        Some((text, fp, is_md))
    }
}

impl NodeContentRenderer for ProfRenderer {
    fn render(&mut self, ui: &mut egui::Ui, node: &Node, inner: egui::Rect, view: CardView) -> f32 {
        let t0 = Instant::now();
        self.rendered.set(self.rendered.get() + 1);
        let scroll = self.render_inner(ui, node, inner, view);
        let elapsed = t0.elapsed().as_micros();
        self.content_micros.set(self.content_micros.get() + elapsed);
        scroll
    }
}

impl ProfRenderer {
    /// The body of `render`, factored out so the timing wrapper stays trivial.
    fn render_inner(&mut self, ui: &mut egui::Ui, node: &Node, inner: egui::Rect, view: CardView) -> f32 {
        if inner.width() < 2.0 || inner.height() < 2.0 {
            return view.scroll_y;
        }
        let Some((text, fingerprint, markdown)) = self.body_for(node) else {
            return view.scroll_y;
        };
        let entry = self
            .panes
            .entry(node.id.clone())
            .or_insert_with(|| build_pane(&text));
        if entry.fingerprint != fingerprint {
            *entry = build_pane(&text);
            entry.fingerprint = fingerprint;
        }
        paint_editor(ui, entry, inner, view, markdown)
    }
}

/// Build a fresh read-only editor pane over `text` (fingerprint set by caller).
fn build_pane(text: &str) -> NodePane {
    let mut view = ViewState { read_only: true, hide_gutter: true, font_size: 14.0, ..Default::default() };
    view.wrap_map.set_enabled(true);
    NodePane {
        fingerprint: String::new(),
        editor: EditorState::new(text),
        view,
        paint: PaintCache::default(),
    }
}

/// Host the read-only editor widget for a node body inside `inner`, rebuilding
/// its markdown decoration layers each frame (the real per-frame cost). Mirrors
/// `content::paint_editor` + `rebuild_markdown_decorations`.
fn paint_editor(ui: &mut egui::Ui, pane: &mut NodePane, inner: egui::Rect, view: CardView, markdown: bool) -> f32 {
    let theme: EditorTheme = dark_default();
    pane.view.font_size = (14.0 * view.zoom).clamp(6.0, 48.0);
    pane.view.scroll_y = view.scroll_y.clamp(0.0, max_scroll(pane, inner));
    let mut rebuild = |state: &EditorState, view: &mut ViewState| {
        rebuild_decorations(state, view, &theme, markdown);
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    EditorWidget::new(&mut pane.editor, &mut pane.view)
        .with_paint_cache(&mut pane.paint)
        .with_decoration_rebuild(&mut rebuild)
        .show(&mut child);
    pane.view.scroll_y = pane.view.scroll_y.min(max_scroll(pane, inner));
    pane.view.scroll_y
}

/// Rebuild the live-preview markdown decoration layers (styling + callouts +
/// footnotes), exactly the focused subset the app's canvas content engine uses.
/// Plain-text bodies get no layers.
fn rebuild_decorations(state: &EditorState, view: &mut ViewState, theme: &EditorTheme, markdown: bool) {
    view.decorations.clear();
    if !markdown {
        return;
    }
    let t = Some(theme);
    view.decorations.push(editor_md::styling::markdown_decorations(state, t));
    view.decorations.push(editor_md::admonitions::callout_decorations(state, t, None));
    view.decorations.push(editor_md::notes::footnote_decorations(state, t, None));
}

/// The maximum vertical scroll for an editor body (content height minus the
/// visible viewport height), reading the height map from the last `show`.
fn max_scroll(pane: &NodePane, inner: egui::Rect) -> f32 {
    (pane.view.height_map.total_height() - inner.height()).max(0.0)
}

/// The lowercased extension of a path (no leading dot), or empty.
fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// A cheap stable hash of a text body, for the text-node fingerprint.
fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A no-op [`CanvasMenuRenderer`] for the profiler: the timed sweeps never open a
/// context menu, so the menu seam just needs a trivial implementation to satisfy
/// `CanvasView::show`. Returns `None` for every target.
pub struct NoMenus;

impl CanvasMenuRenderer for NoMenus {
    fn node_menu(&mut self, _ui: &mut egui::Ui) -> Option<NodeMenuAction> {
        None
    }

    fn edge_menu(&mut self, _ui: &mut egui::Ui) -> Option<EdgeMenuAction> {
        None
    }

    fn empty_menu(&mut self, _ui: &mut egui::Ui) -> Option<EmptyMenuAction> {
        None
    }
}
