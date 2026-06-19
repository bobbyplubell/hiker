//! Host-agnostic embedded buffer view: render an *editable* view of a vault
//! note's shared `Editor` at an arbitrary `egui` rect, owned by whatever host
//! is drawing it (a canvas card, a board Markdown view, a split pane) — not
//! only the dedicated buffer tab.
//!
//! The model is **one shared editor, many views** (`embedded-buffer-view`):
//! the note's document + selection + undo live once in `session.buffers[path]`,
//! while each embedding site owns its own scroll / zoom / wrap / galley cache
//! via an [`EmbeddedView`]. [`show_embedded_buffer`] renders the shared editor
//! through the editor widget, drains its `transactions_out`, and runs the
//! layered-doc editor binding for `path` — so edits reach the document's `working`
//! layer even when no buffer tab is open, making save / autosave / agent-review
//! / dirty-tracking work regardless of which host did the editing.
//!
//! The embed is the chrome-free editor *body*. The chrome the tab adds
//! (minimap, gutter, scrollbar, wikilink hover cards, diff overlay, fold
//! handling, view-option toggles) is deliberately not part of it — a host that
//! wants those wraps them itself.

use eframe::egui;

use editor_egui::widget::PaintCache;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

use crate::panels::buffer::decorations::{rebuild_editor_layers, DecoRebuildCtx};
use crate::panels::buffer::editor_binding;
use crate::state::AppState;

/// Per-embedding-site editor state: one host owns one of these per place it
/// shows a note. Holds the view (scroll / zoom / wrap / viewport) and paint
/// cache, plus the embed's own decoration cache so its live-preview layers
/// memoize independently of the tab's. The shared document lives on the
/// buffer's `Editor`, never here.
pub struct EmbeddedView {
    /// Viewport + layout state for this embedding site.
    pub view: ViewState,
    /// Per-view galley cache, reused across frames by the widget.
    pub paint_cache: PaintCache,
    /// Embed-owned decoration memo, keyed the same way the tab's is. Kept here
    /// so two views of the same note (a card + a tab) don't fight over one
    /// cache.
    decorations: crate::buffer::DecorationCache,
    /// Mirror of the doc text as of the last render, handed to the decoration
    /// rebuild as its `loaded_text` so the index-diff layer is a no-op for the
    /// embed (the embed has no gutter; this keeps it from marking every line as
    /// changed against an empty baseline).
    doc_mirror: String,
}

impl Default for EmbeddedView {
    fn default() -> Self {
        let mut view = ViewState {
            font_size: 14.0,
            ..ViewState::default()
        };
        view.wrap_map.set_enabled(true);
        view.hide_gutter = true;
        Self {
            view,
            paint_cache: PaintCache::default(),
            decorations: crate::buffer::DecorationCache::default(),
            doc_mirror: String::new(),
        }
    }
}

impl EmbeddedView {
    /// A fresh embedded view with defaults (gutter hidden, wrap on).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Per-call render options for an embedded buffer view.
pub struct EmbedOpts {
    /// When true, the view never captures edits — the editor widget no-ops
    /// doc-mutating input (`ViewState::read_only`) and the transactions sink /
    /// layered-doc binding are skipped. Non-editing previews pass `true`.
    pub read_only: bool,
    /// When true, render with the live-preview markdown decoration providers
    /// (wikilink pills, callouts, math, rendered widgets, ...). When false, the
    /// view renders as plain monospace text (code / plain files).
    pub markdown: bool,
    /// Editor body font size in logical points (per-view zoom).
    pub font_size: f32,
    /// When true, the view requests egui keyboard focus this frame if it doesn't
    /// already hold it — so a host that just created the embed (e.g. a canvas
    /// card entering inline-edit mode) can let the user type without a priming
    /// click. A host that wants click-to-focus passes `false`.
    pub focus: bool,
}

impl Default for EmbedOpts {
    fn default() -> Self {
        Self { read_only: false, markdown: true, font_size: 14.0, focus: false }
    }
}

/// What an [`show_embedded_buffer`] frame produced.
#[derive(Default)]
pub struct EmbedResponse {
    /// The editor widget held egui keyboard focus this frame.
    pub has_focus: bool,
    /// The widget applied at least one doc-mutating edit from user input this
    /// frame (i.e. the user typed). Always `false` for a read-only view.
    pub edited: bool,
}

/// Render an editable view of `session.buffers[path]`'s shared `Editor` at the
/// current `ui` rect, using `embed`'s own view + paint cache. Loads the buffer
/// if not already present (`ensure_vault_buffer_loaded`), drains the editor's
/// `transactions_out`, and runs the layered-doc editor binding (`editor_binding::run`)
/// for `path` — so edits reach `working` even with no buffer tab open.
///
/// Borrows are sequential: the widget holds `&mut buffer.editor` only inside the
/// inner block, which ends before `editor_binding::run` takes `&mut app`. Two
/// `&mut Editor` are never held at once.
///
/// status: embedded-buffer-view
pub fn show_embedded_buffer(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    embed: &mut EmbeddedView,
    opts: &EmbedOpts,
) -> EmbedResponse {
    // Embedded views render every frame and are best-effort: a missing or
    // unresolvable note draws nothing rather than spamming an error toast on
    // every frame (e.g. a hover preview over a trail waypoint whose source note
    // can't be resolved). Tab opens use the toasting `ensure_vault_buffer_loaded`.
    if crate::editor_pane::try_ensure_vault_buffer_loaded(app, path).is_err() {
        return EmbedResponse::default();
    }

    embed.view.read_only = opts.read_only;
    embed.view.font_size = opts.font_size;

    // Wikilink live-title resolver: built off an Arc clone so it borrows
    // neither `app` nor the buffer, and is usable inside the rebuild closure.
    let resolve_title =
        crate::panels::buffer::wikilink_nav::title_resolver(
            app.vault_session.services.read_store.clone(),
        );
    let theme_owned = editor_core::theme::light_default();
    let dpr = ui.ctx().pixels_per_point();

    // Persisted diagram cache for this vault buffer (`widget-render-disk-cache`),
    // built before the `buffer` borrow so it can ride the rebuild closure owned.
    let diagram_cache = crate::panels::buffer::diagram_cache_ctx(app);

    let Some(buffer) = app.session.buffers.get_mut(path) else {
        return EmbedResponse::default();
    };

    // Keep the embed's doc mirror current so the index-diff decoration layer
    // (unconditional in the shared rebuild) compares equal and stays empty —
    // the embed has no gutter to paint markers into.
    embed.doc_mirror = buffer.editor.doc.to_string();

    // Forward half of the editor binding: a fresh per-frame sink collecting the
    // change set behind every doc edit the widget applies from user input. Left
    // empty (and unread) for read-only views.
    let mut txns: Vec<editor_core::transaction::Transaction> = Vec::new();

    let response = render_widget(ui, RenderCtx {
        editor: &mut buffer.editor,
        view: &mut embed.view,
        paint_cache: &mut embed.paint_cache,
        decorations: &mut embed.decorations,
        loaded_text: &embed.doc_mirror,
        theme: Some(&theme_owned),
        resolve_title: &resolve_title,
        markdown: opts.markdown,
        dpr,
        txns: (!opts.read_only).then_some(&mut txns),
        diagram_cache,
    });
    let editor_rect = response.rect;
    // Focus-on-create: a host entering an edit (canvas inline-edit) wants the
    // caret live immediately. egui's editor widget self-focuses on click; this
    // covers the no-priming-click case. status: canvas-inline-edit
    if opts.focus && !response.has_focus() {
        response.request_focus();
    }
    // `buffer` borrow ends here; the binding can take `&mut app` freely.

    if !opts.read_only {
        editor_binding::run(app, path, &txns);
    }

    // Diagram preview-while-typing: when this embed is an editable markdown view,
    // float the same math / Mermaid / WaveDrom preview the buffer tab gets so the
    // canvas (and any future embedding host) shows a live render of the span the
    // caret is on. The dedicated buffer tab does NOT use this primitive, so this
    // never double-fires there. status: canvas-inline-edit
    if !opts.read_only && opts.markdown {
        show_embed_edit_preview(app, ui.ctx(), path, embed, editor_rect, &theme_owned, dpr);
    }

    EmbedResponse {
        has_focus: response.has_focus(),
        edited: !txns.is_empty(),
    }
}

/// Float the live edit-preview overlay for an embedded markdown editor. Reads the
/// shared buffer's `Editor` immutably and `app.panels.edit_preview` mutably as
/// disjoint direct `AppState` field accesses (the same split the buffer panel's
/// `show_edit_preview` uses), with the embed's own `ViewState` supplying the
/// scroll-correct geometry. Gated `true` here — the caller already checked
/// `!read_only && markdown`. status: canvas-inline-edit
fn show_embed_edit_preview(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    embed: &EmbeddedView,
    editor_rect: egui::Rect,
    theme: &editor_core::theme::Theme,
    dpr: f32,
) {
    use crate::panels::buffer::widgets::edit_preview::{self, PreviewInputs};
    let cache = crate::panels::buffer::diagram_cache_ctx(app);
    let Some(buffer) = app.session.buffers.get(path) else {
        return;
    };
    let inputs = PreviewInputs {
        state: &buffer.editor,
        view: &embed.view,
        editor_rect,
        theme: Some(theme),
        font_px: embed.view.font_size,
        dpr,
        gated: true,
        cache: cache.as_ref(),
    };
    edit_preview::show(&mut app.panels.edit_preview, ctx, &inputs);
}

/// Disjoint borrows the embed render needs, bundled to keep
/// [`render_widget`] under the `too_many_arguments` cap.
struct RenderCtx<'a> {
    editor: &'a mut editor_core::state::Editor,
    view: &'a mut ViewState,
    paint_cache: &'a mut PaintCache,
    decorations: &'a mut crate::buffer::DecorationCache,
    loaded_text: &'a str,
    theme: Option<&'a editor_core::theme::Theme>,
    resolve_title: &'a editor_md::links::TitleResolver<'a>,
    markdown: bool,
    dpr: f32,
    txns: Option<&'a mut Vec<editor_core::transaction::Transaction>>,
    /// Persisted diagram-cache context, or `None` when `[render]
    /// cache_diagrams` is off. status: widget-render-disk-cache
    diagram_cache: Option<crate::panels::buffer::widgets::disk_cache::DiagramCacheCtx>,
}

/// Run the editor widget for one embed frame: wire the decoration rebuild (the
/// same shared providers the tab uses, gated to markdown-or-plain by
/// `markdown`) and the optional transactions sink, then `show`.
fn render_widget(ui: &mut egui::Ui, ctx: RenderCtx<'_>) -> egui::Response {
    let RenderCtx {
        editor,
        view,
        paint_cache,
        decorations,
        loaded_text,
        theme,
        resolve_title,
        markdown,
        dpr,
        txns,
        diagram_cache,
    } = ctx;
    let font_px = view.font_size;
    let mut deco_ctx = DecoRebuildCtx {
        cache: decorations,
        folds: &EMPTY_FOLDS,
        loaded_text,
        // No dirty-diff gutter in an embedded preview host. status: git-dirty-diff-gutter
        git_head_text: None,
        theme,
        live_preview: markdown,
        render_widgets: markdown,
        is_markdown: markdown,
        code_language: None, // embeds host vault notes, not code files
        dpr,
        font_px,
        chunk_boundaries: false,
        show_whitespace: false,
        highlight_trailing_whitespace: false,
        diff: None,
        conflict: None,
        resolve_title: Some(resolve_title),
        diagram_cache,
        // Embedded buffer view: inline-CSV charts render; external `data:`
        // charts fall back to source (no note-bound resolver). status: widget-chart-render
        chart_resolver: None,
        // No note-bound vault binding here, so `![alt](path)` image cells fall
        // back to source. status: widget-table-render
        image_resolver: None,
        // Embedded buffer view renders tables Fit-only (no overflow toggle).
        // status: widget-table-overflow-scroll
        table_overflow: &EMPTY_TABLE_OVERFLOW,
        // No in-place table cell edit in an embed — the table reveals normally.
        // status: widget-table-cell-edit-inplace
        editing_table: None,
    };
    let mut rebuild = |editor: &editor_core::state::Editor, view: &mut ViewState| {
        rebuild_editor_layers(editor, view, &mut deco_ctx);
    };
    let mut widget = EditorWidget::new(editor, view)
        .with_paint_cache(paint_cache)
        .with_decoration_rebuild(&mut rebuild);
    if let Some(sink) = txns {
        widget = widget.with_transactions_sink(sink);
    }
    widget.show(ui)
}

/// Embeds have no fold chrome, but the shared decoration rebuild reads a fold
/// set for the structural fold / frontmatter-fold layers. A shared empty set
/// avoids allocating one per frame per embed.
static EMPTY_FOLDS: std::sync::LazyLock<std::collections::HashSet<u64>> =
    std::sync::LazyLock::new(std::collections::HashSet::new);

/// Embeds render tables Fit-only — the Scrollable overflow toggle is an
/// editor-only affordance — so a shared empty per-table overflow map avoids a
/// per-frame allocation. status: widget-table-overflow-scroll
static EMPTY_TABLE_OVERFLOW: std::sync::LazyLock<
    crate::panels::buffer::widgets::tables::TableViewMap,
> = std::sync::LazyLock::new(crate::panels::buffer::widgets::tables::TableViewMap::new);
