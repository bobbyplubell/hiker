//! The real node-content engine for the canvas editor: a [`NodeContentRenderer`]
//! that paints each visible node's *body* by dispatching on its kind and (for
//! file nodes) its extension / source type, reusing hiker's existing renderers
//! rather than reinventing any of them.
//!
//! - **Text node** → markdown, rendered through a read-only `editor-egui`
//!   widget driven by the buffer panel's full decoration pipeline (live-preview
//!   styling plus the rendered math / Mermaid / WaveDrom / table widgets), so a
//!   text node reads like a note buffer. status: canvas-text-node-markdown
//! - **File node** → resolve the vault-relative `file` against the vault root,
//!   then dispatch by extension: image → `egui::Image` (`file://`), `.md` →
//!   markdown, `.html`/`.htm` → `hiker-htmlview`, a known source type with an
//!   extracted `<name>.<ext>.md` sidecar → the sidecar's markdown, a code/text
//!   source → the editor widget read-only, an unresolvable path → a
//!   broken-reference card, an unknown-but-present file → a typed placeholder.
//!   status: canvas-file-node-embed
//! - **Link node** → a link card (globe glyph + URL); a click opens the URL in
//!   the OS handler via `ctx.open_url`. status: canvas-link-node-card
//! - **Group node** → nothing: the adapter already paints the group background
//!   and label, and the view never even calls us for a group card.
//!
//! ## Heavyweight per-node state
//!
//! A text/code/markdown node hosts an `editor-egui` editor (`!Send`, holds egui
//! galley caches + a rope), and a vault-internal `.html` FILE node hosts a
//! `hiker_htmlview::HtmlView` (`!Send`, caches egui textures + a styled
//! document) — a link node is an open-externally card, not an htmlview. Neither
//! editor nor htmlview may live on the
//! `Send` `AppState`, and rebuilding one per frame would be wasteful — so, exactly
//! like the ZIM panes (`panels::zim`), they park in a UI-thread-local store keyed
//! by `(tab, node id)`. Each entry caches a content fingerprint (text / file /
//! subpath); a node's pane is rebuilt only when that fingerprint changes, and the
//! whole tab's panes are dropped on tab close ([`forget`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use eframe::egui;

use editor_core::state::Editor as EditorState;
use editor_core::theme::{dark_default, light_default, Theme as EditorTheme};
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::ViewState;
use hiker_canvas::model::{Node, NodeKind};
use hiker_htmlview::{HtmlView, ResourceProvider, Theme as HtmlTheme};

use canvas_view::content::{CardView, NodeContentRenderer};

use crate::buffer::DecorationCache;
use crate::panels::buffer::decorations::{rebuild_editor_layers, DecoRebuildCtx};
use crate::tab::TabId;

// Heavyweight per-node render state lives here, keyed by `(tab, node id)`, so
// it survives across frames (the renderer itself is rebuilt each frame) and is
// `!Send`-safe (the editor / htmlview panes never leave the UI thread). Mirrors
// the ZIM viewer's `PANES` store. status: canvas-node-content-trait
thread_local! {
    static PANES: RefCell<HashMap<(TabId, String), NodePane>> = RefCell::new(HashMap::new());
}

/// Drop every cached pane for a closed canvas tab, freeing its editor galley
/// caches / htmlview textures. Called from the tab-close path so panes don't
/// leak across the session.
pub fn forget(tab_id: TabId) {
    PANES.with(|panes| {
        panes.borrow_mut().retain(|(t, _), _| *t != tab_id);
    });
}

/// Drop every cached canvas-content pane during controlled shutdown, releasing
/// editor + htmlview egui resources while the UI thread's TLS is still alive.
pub fn shutdown() {
    PANES.with(|panes| panes.borrow_mut().clear());
}

/// One node's cached render state plus the content fingerprint it was built for.
/// A fingerprint mismatch (text / file / subpath changed) rebuilds the body.
struct NodePane {
    /// Fingerprint of the node content the body was built for; a change rebuilds.
    fingerprint: String,
    body: NodeBody,
}

/// The kind-specific heavyweight body for a node, behind the fingerprint.
enum NodeBody {
    /// A read-only `editor-egui` instance over markdown / code / plain text.
    Editor(Box<EditorPane>),
    /// A `hiker_htmlview::HtmlView` over a vault-internal `.html` / captured page.
    Html(Box<HtmlView>),
    /// No heavyweight state (image / placeholder / broken / link card): painted
    /// directly each frame.
    Light,
}

/// A hosted read-only editor for a markdown / code / text body. The decoration
/// layers are rebuilt per frame through the widget hook (markdown nodes get the
/// full buffer-panel pipeline — live-preview styling plus the rendered
/// math/mermaid/wavedrom/table widgets; code/text nodes get none).
struct EditorPane {
    editor: EditorState,
    view: ViewState,
    paint: PaintCache,
    /// Fingerprint-keyed cache for the decoration providers, so a static card
    /// re-renders cheaply each frame (mirrors `Buffer.decoration_cache`).
    /// status: widget-render-providers
    decoration_cache: DecorationCache,
    /// True when the body should render with the markdown decoration providers
    /// (text nodes and `.md` files), false for code / plain text.
    markdown: bool,
}

/// The real all-source content engine the canvas view paints node bodies with.
/// Constructed fresh each frame in `render::canvas_body`; the heavyweight state
/// it touches lives in the thread-local [`PANES`] store, so the per-frame
/// construction is cheap. status: canvas-node-content-trait
pub struct Engine {
    tab_id: TabId,
    vault_root: PathBuf,
    /// Live (unsaved) text for file-node paths that have a loaded shared buffer,
    /// keyed by vault-relative path. A file-node card reads its body from here
    /// when present so an open, dirty note shows its unsaved content instead of
    /// stale disk bytes; absent paths fall back to disk. Built fresh each frame
    /// in `render::canvas_body`, so an owned map is fine.
    /// status: canvas-inline-edit
    live_text: HashMap<String, String>,
}

impl Engine {
    /// Build the renderer for one canvas tab. `vault_root` resolves file-node
    /// paths; `tab_id` scopes the per-node cache; `live_text` overrides disk
    /// reads with the live shared-buffer text for loaded file-node paths.
    pub const fn new(tab_id: TabId, vault_root: PathBuf, live_text: HashMap<String, String>) -> Self {
        Self { tab_id, vault_root, live_text }
    }
}

impl NodeContentRenderer for Engine {
    fn render(&mut self, ui: &mut egui::Ui, node: &Node, inner: egui::Rect, view: CardView) -> f32 {
        if inner.width() < 2.0 || inner.height() < 2.0 {
            return view.scroll_y;
        }
        let plan = plan_node(&self.vault_root, node, &self.live_text);
        let key = (self.tab_id, node.id.clone());
        PANES.with(|panes| {
            let mut panes = panes.borrow_mut();
            let fp = plan.fingerprint();
            let entry = panes.entry(key).or_insert_with(|| NodePane {
                fingerprint: String::new(),
                body: NodeBody::Light,
            });
            let just_rebuilt = entry.fingerprint != fp;
            if just_rebuilt {
                entry.body = plan.build_body(ui.ctx());
                entry.fingerprint = fp;
            }
            paint_body(ui, &mut entry.body, &plan, inner, view, just_rebuilt)
        })
    }
}

/// The resolved render plan for a node: what to draw and the inputs it needs.
/// Computed cheaply each frame (no heavyweight state); the matching
/// [`NodeBody`] is cached behind the fingerprint.
enum NodePlan<'a> {
    /// Markdown body (text node, or a `.md` / sidecar file).
    Markdown { text: String, subpath_note: Option<String> },
    /// A code / plain-text source body, read-only.
    Code { text: String },
    /// An image file at this absolute path.
    Image { abs: PathBuf },
    /// An HTML file or captured page: the HTML plus the base dir for resources.
    Html { html: String, base_dir: PathBuf },
    /// A link card for an external URL.
    Link { url: &'a str },
    /// A present-but-unrenderable file: a typed placeholder (filename + label).
    Placeholder { filename: String, label: String },
    /// A `file` path that doesn't resolve: a broken-reference card.
    Broken { file: &'a str },
}

impl NodePlan<'_> {
    /// A fingerprint of the content this plan renders, so the cached body is
    /// rebuilt only when the node's text / file / subpath actually changes.
    fn fingerprint(&self) -> String {
        match self {
            NodePlan::Markdown { text, subpath_note } => {
                format!("md:{}:{}", subpath_note.as_deref().unwrap_or(""), text)
            }
            NodePlan::Code { text } => format!("code:{text}"),
            NodePlan::Image { abs } => format!("img:{}", abs.display()),
            NodePlan::Html { html, base_dir } => {
                format!("html:{}:{}", base_dir.display(), html.len())
            }
            NodePlan::Link { url } => format!("link:{url}"),
            NodePlan::Placeholder { filename, label } => format!("ph:{label}:{filename}"),
            NodePlan::Broken { file } => format!("broken:{file}"),
        }
    }

    /// Build the heavyweight [`NodeBody`] this plan needs (an editor, an
    /// htmlview, or nothing). Run once per fingerprint change.
    fn build_body(&self, _ctx: &egui::Context) -> NodeBody {
        match self {
            NodePlan::Markdown { text, .. } => NodeBody::Editor(Box::new(editor_pane(text, true))),
            NodePlan::Code { text } => NodeBody::Editor(Box::new(editor_pane(text, false))),
            NodePlan::Html { html, base_dir } => {
                let provider: Arc<dyn ResourceProvider> =
                    Arc::new(DirProvider { base: base_dir.clone() });
                let base_url = dir_base_url(base_dir);
                NodeBody::Html(Box::new(HtmlView::new(html, Some(&base_url), provider)))
            }
            NodePlan::Image { .. }
            | NodePlan::Link { .. }
            | NodePlan::Placeholder { .. }
            | NodePlan::Broken { .. } => NodeBody::Light,
        }
    }
}

/// Build a read-only editor pane over `text`. `markdown` selects the live-preview
/// decoration providers (vs. none for code / plain text).
fn editor_pane(text: &str, markdown: bool) -> EditorPane {
    let mut view = ViewState { read_only: true, hide_gutter: true, font_size: 14.0, ..Default::default() };
    view.wrap_map.set_enabled(true);
    EditorPane {
        editor: EditorState::new(text),
        view,
        paint: PaintCache::default(),
        decoration_cache: DecorationCache::default(),
        markdown,
    }
}

/// Resolve a node into its render plan: dispatch on kind, and for file nodes on
/// extension / source type. status: canvas-file-node-embed
fn plan_node<'a>(
    vault_root: &Path,
    node: &'a Node,
    live_text: &HashMap<String, String>,
) -> NodePlan<'a> {
    match &node.kind {
        // status: canvas-text-node-markdown
        NodeKind::Text { text } => NodePlan::Markdown { text: text.clone(), subpath_note: None },
        // status: canvas-file-node-embed
        NodeKind::File { file, subpath } => {
            plan_file(vault_root, file, subpath.as_deref(), live_text.get(file).map(String::as_str))
        }
        // status: canvas-link-node-card
        NodeKind::Link { url } => NodePlan::Link { url },
        // The adapter paints the group frame + label and never calls us for a
        // group card; render nothing if it somehow does.
        NodeKind::Group { .. } => NodePlan::Placeholder {
            filename: String::new(),
            label: "group".to_string(),
        },
    }
}

/// Dispatch a file node by resolving its vault-relative path and inspecting the
/// extension / sidecar. status: canvas-file-node-embed
fn plan_file<'a>(
    vault_root: &Path,
    file: &'a str,
    subpath: Option<&str>,
    live: Option<&str>,
) -> NodePlan<'a> {
    let abs = vault_root.join(file);
    let ext = extension(file);
    if is_image_ext(&ext) {
        return if abs.is_file() {
            NodePlan::Image { abs }
        } else {
            NodePlan::Broken { file }
        };
    }
    // A loaded shared buffer is the live source of truth — prefer its (possibly
    // unsaved) text over a disk read so a dirty note never looks stale on the
    // card. status: canvas-inline-edit
    if matches!(ext.as_str(), "html" | "htm") {
        return match live.map(str::to_owned).or_else(|| std::fs::read_to_string(&abs).ok()) {
            Some(html) => NodePlan::Html { html, base_dir: parent_dir(&abs) },
            None => NodePlan::Broken { file },
        };
    }
    if ext == "md" {
        return match live.map(str::to_owned).or_else(|| std::fs::read_to_string(&abs).ok()) {
            Some(body) => markdown_plan(body, subpath),
            None => NodePlan::Broken { file },
        };
    }
    // A known source type (pdf / docx / audio / …) stores extracted text in a
    // `<source-filename>.md` sidecar beside it. Render that markdown if present.
    if let Some(sidecar) = sidecar_path(&abs) {
        if let Ok(body) = std::fs::read_to_string(&sidecar) {
            return markdown_plan(body, subpath);
        }
    }
    // No sidecar. A code / text source renders read-only in the editor; an
    // unknown-but-present file gets a typed placeholder; a missing path is broken.
    if is_code_ext(&ext) {
        return match live.map(str::to_owned).or_else(|| std::fs::read_to_string(&abs).ok()) {
            Some(text) => NodePlan::Code { text },
            None => NodePlan::Broken { file },
        };
    }
    if abs.is_file() {
        NodePlan::Placeholder { filename: basename(file), label: source_label(&ext) }
    } else {
        NodePlan::Broken { file }
    }
}

/// Scope a markdown body to a `#Heading` / `#^block` subpath when one is given
/// and can be sliced cheaply; otherwise render the whole body with a note.
/// status: canvas-file-node-embed
fn markdown_plan(body: String, subpath: Option<&str>) -> NodePlan<'static> {
    match subpath.map(str::trim).filter(|s| !s.is_empty()) {
        None => NodePlan::Markdown { text: body, subpath_note: None },
        Some(anchor) => match slice_heading(&body, anchor) {
            Some(section) => NodePlan::Markdown { text: section, subpath_note: None },
            None => NodePlan::Markdown {
                text: body,
                subpath_note: Some(anchor.to_string()),
            },
        },
    }
}

/// Slice a markdown body to the section under a `#Heading` subpath: from the
/// matching ATX heading line to the next heading of the same-or-shallower depth.
/// Block refs (`#^id`) aren't sliced (returns `None` → whole-body fallback).
fn slice_heading(body: &str, anchor: &str) -> Option<String> {
    let heading = anchor.strip_prefix('#').unwrap_or(anchor);
    if heading.starts_with('^') || heading.is_empty() {
        return None;
    }
    let want = heading.trim().to_ascii_lowercase();
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| heading_matches(l, &want))?;
    let start_depth = heading_depth(lines[start])?;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if let Some(depth) = heading_depth(line) {
            if depth <= start_depth {
                end = i;
                break;
            }
        }
    }
    Some(lines[start..end].join("\n"))
}

/// The ATX heading depth (count of leading `#`) of a line, or `None` if it's not
/// a heading.
fn heading_depth(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = trimmed.get(hashes..)?;
    if after.starts_with(' ') || after.is_empty() {
        Some(hashes)
    } else {
        None
    }
}

/// Whether a heading line's text matches the wanted (lowercased) heading.
fn heading_matches(line: &str, want: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    trimmed[hashes..].trim().to_ascii_lowercase() == *want
}

/// Paint a node's cached body inside `inner` per the per-card `view` (zoom +
/// scroll). Returns the effective (clamped) scroll the body settled on — the
/// editor clamps to its content height; non-scrolling bodies echo `view.scroll_y`.
fn paint_body(
    ui: &mut egui::Ui,
    body: &mut NodeBody,
    plan: &NodePlan<'_>,
    inner: egui::Rect,
    view: CardView,
    just_rebuilt: bool,
) -> f32 {
    match (body, plan) {
        (NodeBody::Editor(pane), _) => return paint_editor(ui, pane, plan, inner, view, just_rebuilt),
        (NodeBody::Html(html), _) => paint_html(ui, html, inner),
        (NodeBody::Light, NodePlan::Image { abs }) => paint_image(ui, abs, inner),
        (NodeBody::Light, NodePlan::Link { url }) => paint_link(ui, url, inner, view.zoom),
        (NodeBody::Light, NodePlan::Placeholder { filename, label }) => {
            paint_placeholder(ui, filename, label, inner, view.zoom, false);
        }
        (NodeBody::Light, NodePlan::Broken { file }) => {
            paint_placeholder(ui, &basename(file), "missing", inner, view.zoom, true);
        }
        (NodeBody::Light, _) => {}
    }
    view.scroll_y
}

/// Host the read-only editor widget for a markdown / code / text body inside
/// `inner`, rebuilding its decoration layers (markdown providers for markdown
/// nodes) through the widget hook. status: canvas-text-node-markdown
fn paint_editor(ui: &mut egui::Ui, pane: &mut EditorPane, plan: &NodePlan<'_>, inner: egui::Rect, view: CardView, just_rebuilt: bool) -> f32 {
    let dark = ui.visuals().dark_mode;
    let theme: EditorTheme = if dark { dark_default() } else { light_default() };
    let dpr = ui.ctx().pixels_per_point();
    let markdown = pane.markdown;
    // Per-card zoom drives the font (the editor recomputes line height from it);
    // per-card scroll seeds the viewport. Both are independent of camera zoom —
    // a card is a readable, scrollable window. status: canvas-card-zoom, canvas-card-scroll
    let font_px = (14.0 * view.zoom).clamp(6.0, 48.0);
    pane.view.font_size = font_px;
    // Pre-clamp the requested scroll to the content height measured on the
    // PREVIOUS frame (the `height_map` persists on the pane), so a wheel
    // overshoot never renders an out-of-range frame that visibly snaps back. The
    // viewport height is the visible content rect (`inner`).
    //
    // EXCEPT on a just-rebuilt pane (its content changed this frame, e.g. the
    // user typed in a tab showing this note): the `height_map` isn't measured
    // yet, so `max_scroll` is a spurious 0 and clamping would reset the card's
    // scroll to the top. Pass the requested scroll through; the post-show clamp
    // below uses this frame's freshly-measured height. status: canvas-card-scroll
    pane.view.scroll_y = if just_rebuilt {
        view.scroll_y.max(0.0)
    } else {
        view.scroll_y.clamp(0.0, max_scroll(pane, inner))
    };
    // Route markdown bodies through the buffer panel's FULL decoration pipeline
    // with `render_widgets: true`, so a card shows the same rendered Mermaid /
    // WaveDrom / display-math / table widgets the editor does (not raw fences).
    // status: widget-render-providers
    let body_text = pane.editor.doc.to_string();
    let mut deco_ctx =
        card_decoration_ctx(&mut pane.decoration_cache, &theme, &body_text, markdown, font_px, dpr);
    let mut rebuild = |state: &EditorState, view: &mut ViewState| {
        rebuild_editor_layers(state, view, &mut deco_ctx);
    };
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    // Display-only: the card content paints but never senses pointer input, so
    // the canvas interaction surface above it owns resize / move / select and a
    // drag across a card never text-selects in it. status: canvas-render-widget
    EditorWidget::new(&mut pane.editor, &mut pane.view)
        .interactive(false)
        .with_paint_cache(&mut pane.paint)
        .with_decoration_rebuild(&mut rebuild)
        .show(&mut child);
    if let NodePlan::Markdown { subpath_note: Some(anchor), .. } = plan {
        subpath_badge(ui, inner, anchor);
    }
    // Re-clamp against this frame's freshly measured height (content may have
    // shrunk / the font changed) and report the effective offset so the view
    // stores the clamped value — no runaway over-scroll past the end.
    pane.view.scroll_y = pane.view.scroll_y.min(max_scroll(pane, inner));
    pane.view.scroll_y
}

/// The maximum vertical scroll for an editor body: total content height minus
/// the visible viewport height (`inner`), floored at zero. Reads the height map
/// the editor populated during its last `show`. status: canvas-card-scroll
fn max_scroll(pane: &EditorPane, inner: egui::Rect) -> f32 {
    (pane.view.height_map.total_height() - inner.height()).max(0.0)
}

/// An always-empty fold set: a canvas card never folds, but the buffer panel's
/// decoration pipeline takes `folds` by reference, so this hands it a `'static`
/// borrow without allocating per frame.
static EMPTY_FOLDS: LazyLock<HashSet<u64>> = LazyLock::new(HashSet::new);

/// Build the read-only `DecoRebuildCtx` a canvas card renders its markdown body
/// through: the buffer panel's FULL decoration pipeline, with `render_widgets`
/// on so a card shows the same rendered Mermaid / WaveDrom / display-math /
/// table widgets the editor does. A code / plain-text card passes `markdown:
/// false`, which gates the markdown + widget layers off (raw monospace text).
/// Card-specific posture: no folds, no chunk boundaries, no whitespace overlay,
/// no diff, plain (non-clickable) wikilinks (`resolve_title: None`).
/// status: widget-render-providers
fn card_decoration_ctx<'a>(
    cache: &'a mut DecorationCache,
    theme: &'a EditorTheme,
    loaded_text: &'a str,
    markdown: bool,
    font_px: f32,
    dpr: f32,
) -> DecoRebuildCtx<'a> {
    DecoRebuildCtx {
        cache,
        folds: &EMPTY_FOLDS,
        loaded_text,
        theme: Some(theme),
        live_preview: true,
        render_widgets: true,
        is_markdown: markdown,
        dpr,
        font_px,
        chunk_boundaries: false,
        show_whitespace: false,
        highlight_trailing_whitespace: false,
        diff: None,
        resolve_title: None,
        // Canvas node previews don't carry a vault-scoped disk cache — they
        // render through the in-memory caches only. status: widget-render-disk-cache
        diagram_cache: None,
        // Embedded preview: inline-CSV charts render; external `data:` charts
        // fall back to source (no note-bound resolver here). status: widget-chart-render
        chart_resolver: None,
    }
}

/// Paint an image file scaled to fit `inner`, preserving aspect ratio. The
/// loaders for `file://` URLs are installed at app startup (`main.rs`).
fn paint_image(ui: &mut egui::Ui, abs: &Path, inner: egui::Rect) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    let uri = format!("file://{}", abs.display());
    child.add(
        egui::Image::new(uri)
            .max_size(inner.size())
            .fit_to_fraction(egui::vec2(1.0, 1.0))
            .maintain_aspect_ratio(true),
    );
}

/// Host the `hiker-htmlview` renderer for a vault-internal `.html` / captured
/// page inside `inner`, driving its own scroll area. status: canvas-file-node-embed
fn paint_html(ui: &mut egui::Ui, view: &mut HtmlView, inner: egui::Rect) {
    view.set_theme(if ui.visuals().dark_mode { HtmlTheme::Dark } else { HtmlTheme::Light });
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            let width = ui.available_width().max(1.0);
            let size = view.layout(ui.ctx(), width);
            let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
            let painter = ui.painter_at(rect);
            view.paint(&painter, rect.min, painter.clip_rect());
        });
}

/// Paint a link card: a globe glyph + the URL. Display-only — the canvas
/// interaction surface owns pointer input now, so opening the URL is the host's
/// double-click *activation* path (`CanvasResponse::activated`), not a click
/// here. status: canvas-link-node-card
fn paint_link(ui: &mut egui::Ui, url: &str, inner: egui::Rect, scale: f32) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    let size = (13.0 * scale).clamp(9.0, 20.0);
    let accent = child.visuals().hyperlink_color;
    child.horizontal(|ui| {
        ui.label(egui::RichText::new("\u{1F310}").size(size));
        ui.add(
            egui::Label::new(egui::RichText::new(url).size(size).color(accent).underline())
                .truncate()
                .selectable(false),
        );
    });
}

/// Paint a typed placeholder / broken-reference card: an icon glyph, the
/// filename, and a kind/"missing" pill. `broken` greys it and uses the error
/// accent, mirroring the board's broken-reference posture
/// (`board-card-references`). status: canvas-file-node-embed
fn paint_placeholder(
    ui: &mut egui::Ui,
    filename: &str,
    label: &str,
    inner: egui::Rect,
    scale: f32,
    broken: bool,
) {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    child.set_clip_rect(inner.intersect(ui.clip_rect()));
    let size = (13.0 * scale).clamp(9.0, 20.0);
    let (glyph, accent) = if broken {
        ("\u{26A0}", egui::Color32::from_rgb(200, 60, 60))
    } else {
        ("\u{1F4C4}", child.visuals().weak_text_color())
    };
    child.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(glyph).size(size));
            ui.add(
                egui::Label::new(egui::RichText::new(filename).size(size).strong()).truncate(),
            );
        });
        ui.label(egui::RichText::new(label).size(size * 0.8).color(accent).monospace());
    });
}

/// Paint a small corner badge noting the unscoped `subpath` when a `.md` node
/// couldn't be sliced to its `#heading` / `#^block` (whole file rendered).
fn subpath_badge(ui: &egui::Ui, inner: egui::Rect, anchor: &str) {
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    let text = format!("\u{2192} {anchor}");
    painter.text(
        inner.right_top() + egui::vec2(-4.0, 2.0),
        egui::Align2::RIGHT_TOP,
        text,
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );
}

/// A directory-backed [`ResourceProvider`]: resolves a captured page's relative
/// `<img>` / `<link>` subresources against the HTML file's own directory,
/// entirely offline (no network). Returns `None` for anything outside the base
/// dir or that doesn't read.
struct DirProvider {
    base: PathBuf,
}

impl ResourceProvider for DirProvider {
    fn fetch(&self, url: &str) -> Option<(Vec<u8>, String)> {
        let rel = url.strip_prefix(&dir_base_url(&self.base)).unwrap_or(url);
        let rel = rel.split(['#', '?']).next().unwrap_or(rel);
        if rel.is_empty() || rel.contains("://") {
            return None;
        }
        let candidate = self.base.join(rel);
        // Stay inside the base dir: reject any path that escapes it.
        if !candidate.starts_with(&self.base) {
            return None;
        }
        let bytes = std::fs::read(&candidate).ok()?;
        let mime = mime_for(&extension(rel));
        Some((bytes, mime))
    }
}

/// The `file://` base URL for a directory, with a trailing slash so a relative
/// subresource href concatenates correctly.
fn dir_base_url(dir: &Path) -> String {
    format!("file://{}/", dir.display())
}

/// A best-effort MIME type for a subresource extension (the small set a captured
/// page references). Unknown extensions get `application/octet-stream`.
fn mime_for(ext: &str) -> String {
    let m = match ext {
        "css" => "text/css",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "html" | "htm" => "text/html",
        "js" => "text/javascript",
        _ => "application/octet-stream",
    };
    m.to_string()
}

/// The lowercased extension of a path (no leading dot), or empty.
fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// The basename of a vault-relative path.
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// The directory containing `abs` (its parent), or `abs` itself if it has none.
fn parent_dir(abs: &Path) -> PathBuf {
    abs.parent().map(Path::to_path_buf).unwrap_or_else(|| abs.to_path_buf())
}

/// The extracted-text sidecar path for a source file:
/// `<source-filename>.<ext>.md` beside it. `None` if no filename. Existence
/// is checked by the caller.
fn sidecar_path(abs: &Path) -> Option<PathBuf> {
    let name = abs.file_name()?.to_string_lossy().into_owned();
    Some(abs.with_file_name(format!("{name}.md")))
}

/// Whether an extension is a renderable raster/vector image.
fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico")
}

/// Whether an extension is a source/code/text type the editor renders directly
/// (read-only, plain). Markdown is handled separately (it gets decorations).
fn is_code_ext(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "json" | "toml" | "yaml" | "yml"
            | "sh" | "bash" | "zsh" | "c" | "h" | "cpp" | "hpp" | "cc" | "go" | "rb"
            | "java" | "kt" | "swift" | "php" | "lua" | "sql" | "css" | "scss" | "xml"
            | "txt" | "csv" | "ini" | "cfg" | "conf" | "log" | "tex"
    )
}

/// A human label for a known source type behind a placeholder card (when no
/// sidecar exists yet). Falls back to the bare extension.
fn source_label(ext: &str) -> String {
    let label = match ext {
        "pdf" => "PDF",
        "docx" | "doc" => "Word document",
        "odt" => "OpenDocument text",
        "pptx" | "ppt" => "presentation",
        "xlsx" | "xls" => "spreadsheet",
        "epub" => "EPUB",
        "m4a" | "mp3" | "wav" | "flac" | "ogg" | "aac" => "audio",
        "mp4" | "mov" | "mkv" | "webm" => "video",
        "zip" | "tar" | "gz" => "archive",
        "" => "file",
        other => return other.to_string(),
    };
    label.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_heading_section() {
        let body = "# A\nintro\n\n## B\nbody b\n\n## C\nbody c\n";
        let sliced = slice_heading(body, "#B").expect("section B");
        assert!(sliced.contains("body b"));
        assert!(!sliced.contains("body c"));
        assert!(!sliced.contains("intro"));
    }

    #[test]
    fn heading_slice_stops_at_same_depth() {
        let body = "## Top\none\n### Sub\ntwo\n## Next\nthree\n";
        let sliced = slice_heading(body, "#Top").expect("section");
        assert!(sliced.contains("one"));
        assert!(sliced.contains("two"), "deeper subsection is included");
        assert!(!sliced.contains("three"), "next same-depth heading ends it");
    }

    #[test]
    fn block_ref_subpath_is_not_sliced() {
        let body = "# A\ntext ^blk\n";
        assert!(slice_heading(body, "#^blk").is_none());
    }

    #[test]
    fn missing_heading_falls_back() {
        assert!(slice_heading("# A\nx\n", "#Nope").is_none());
    }

    #[test]
    fn classifies_extensions() {
        assert!(is_image_ext("png"));
        assert!(is_image_ext("svg"));
        assert!(!is_image_ext("pdf"));
        assert!(is_code_ext("rs"));
        assert!(is_code_ext("json"));
        assert!(!is_code_ext("png"));
    }

    #[test]
    fn sidecar_is_md_suffixed() {
        let p = sidecar_path(Path::new("/v/notes/rm0090.pdf")).unwrap();
        assert_eq!(p, Path::new("/v/notes/rm0090.pdf.md"));
    }

    #[test]
    fn source_label_for_known_types() {
        assert_eq!(source_label("pdf"), "PDF");
        assert_eq!(source_label("m4a"), "audio");
        assert_eq!(source_label("xyz"), "xyz");
    }

    #[test]
    fn markdown_plan_scopes_to_heading() {
        let body = "# A\nintro\n## B\nbody b\n".to_string();
        match markdown_plan(body, Some("#B")) {
            NodePlan::Markdown { text, subpath_note } => {
                assert!(text.contains("body b"));
                assert!(subpath_note.is_none());
            }
            _ => panic!("expected markdown plan"),
        }
    }

    #[test]
    fn markdown_plan_notes_unsliceable_subpath() {
        let body = "# A\nx\n".to_string();
        match markdown_plan(body, Some("#^block-id")) {
            NodePlan::Markdown { subpath_note, .. } => {
                assert_eq!(subpath_note.as_deref(), Some("#^block-id"));
            }
            _ => panic!("expected markdown plan"),
        }
    }
}
