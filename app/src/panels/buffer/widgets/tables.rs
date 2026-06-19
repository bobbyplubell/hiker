//! Natively-painted pipe-table widget (`widget-table-render`).
//!
//! A GFM pipe table renders as a `BlockWidget` that paints itself directly —
//! filled rects for the header background and grid rules, lines for cell
//! borders, positioned text runs honoring each column's alignment — via the
//! egui-free [`BlockPaint`] retained-paint hook (`widget-block-native-paint`),
//! not an SVG → texture round-trip. Re-laying out on zoom / DPR / soft-wrap
//! width is free (no re-raster); there is deliberately no `hiker-render` table
//! engine.
//!
//! This module owns the raw-source → rows/cells parse, the grid layout (column
//! widths from content, wrapped cells), the height measure, and the paint-list
//! build. The thin `table_widget_decorations` provider in the parent module
//! wires detection (`editor_md::tables::table_spans`) to a [`TableWidget`] and
//! reuses the shared block-widget reveal/hide plumbing.

use std::sync::Arc;

use editor_core::decoration::{
    BlockPaint, BlockWidget, ChildItem, ChildKind, ChildRect, ChildTexture, Color, Decoration,
    Set as DecorationSet, StyledRun, TextAlign, WidgetClickRegion,
};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::diagrams::{mermaid_span_in_str, wavedrom_span_in_str};
use editor_md::embeds::image_span_in_str;
use editor_md::equations::{MathKind as SpanMathKind, math_spans_in_str};
use editor_md::tables::{ColumnAlign, table_spans};

use self::image_loader::CellImageResolver;
use super::disk_cache::DiagramCacheCtx;
use super::inline::{Colors as InlineColors, parse_runs};
use super::render::{
    MathKind, MermaidColors, RenderedWidget, WaveDromColors, render_image, render_math,
    render_mermaid, render_wavedrom,
};

pub mod cell_edit;
pub mod image_loader;

/// Build the table-widget decoration layer for the current editor state.
///
/// Mirrors `mermaid_widget_decorations`: for each on-screen pipe table
/// (viewport-scoped via `table_spans`), applies reveal-on-cursor (cursor inside
/// the table's byte range or a selection overlap shows the raw source), and
/// otherwise hides the source lines + emits a natively-painted `BlockWidget`.
/// A malformed table (no `|---|` rule row) is never detected — its tinted
/// source remains (`widget-render-error-fallback`). Unlike math / mermaid this
/// path does no SVG raster: the [`TableWidget`] paints itself via `paint_list`
/// (`widget-block-native-paint`). status: widget-table-render
pub fn table_widget_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    inputs: TableProviderInputs<'_>,
) -> DecorationSet {
    let TableProviderInputs { font_px, dpr, cache, images, views, editing_table } = inputs;
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    let doc = state.doc.to_string();
    for span in table_spans(state, viewport) {
        // Reveal: cursor anywhere inside the table block or a selection overlap
        // shows the raw pipe-and-dash source (`widget-reveal-block`); otherwise
        // hide the source lines and render the grid in place. BUT a table whose
        // cell is in active in-place edit suppresses its whole-table reveal — the
        // table stays fully rendered while one cell edits in a popover, the whole
        // point of `widget-table-cell-edit-inplace`. (The caret sits in the edited
        // cell's source, which would otherwise trigger reveal.)
        let in_cell_edit = editing_table == Some(span.byte_range.start);
        let revealed = !in_cell_edit
            && (super::placement::cursor_inside(state, &span.byte_range)
                || super::placement::selection_overlaps(state, &span.byte_range));
        if revealed {
            continue;
        }
        let src = &doc[span.byte_range.clone()];
        // The ephemeral per-table overflow mode + scroll offset, keyed by the
        // table's byte-range start (default Fit when absent / no resolver).
        // status: widget-table-overflow-scroll
        let view = views
            .and_then(|m| m.get(&span.byte_range.start).copied())
            .unwrap_or_default();
        let render = TableRenderInputs { font_px, dpr, cache, images, view };
        let Some(widget) = TableWidget::from_source(src, theme, &span.aligns, render) else {
            continue; // malformed → fall back to the tinted source
        };
        super::placement::emit_block_widget(
            state,
            &span.byte_range,
            Arc::new(widget),
            total_lines,
            &line_byte_end,
            &mut entries,
        );
    }
    RangeSet::from_iter(entries)
}

/// Per-table ephemeral overflow state, keyed by each table's byte-range start
/// (the stable per-table identity within a buffer). Lives host-side on the
/// `Buffer`; the provider reads it to resolve each table's
/// [`TableViewState`]. status: widget-table-overflow-scroll
pub type TableViewMap = std::collections::HashMap<usize, TableViewState>;

/// The per-call render inputs for [`table_widget_decorations`], grouped so the
/// provider stays under the argument cap: font / dpr / diagram cache / image
/// resolver, plus the borrowed ephemeral per-table overflow [`TableViewMap`]
/// (`None` in hosts without one — every table is Fit). status: widget-table-render
#[derive(Clone, Copy)]
pub struct TableProviderInputs<'a> {
    pub font_px: f32,
    pub dpr: f32,
    pub cache: Option<&'a DiagramCacheCtx>,
    pub images: Option<&'a CellImageResolver>,
    pub views: Option<&'a TableViewMap>,
    /// The byte start of the table whose cell is in active in-place edit
    /// (`widget-table-cell-edit-inplace`), if any. That table suppresses its
    /// whole-table reveal so it stays rendered while one cell edits in a popover;
    /// `None` (every host without an in-progress cell edit) keeps the normal
    /// cursor-in reveal.
    pub editing_table: Option<usize>,
}

/// Build this frame's table edit-target entries (`widget-table-cell-edit`): for
/// every on-screen pipe table (viewport-scoped), the whole-widget id (its render
/// `content_hash`) → the table block start, plus each TEXT cell's
/// [`table_cell_id`] → the byte offset at the END of that cell's content (caret
/// ready to append). The buffer panel merges these into the shared
/// `WidgetEditTargets`, so a text-cell click routes through the existing
/// `place_caret_for_block_click` and lands the caret in that cell — which
/// triggers reveal so the source shows for editing.
///
/// BLOCK cells (math / diagram / image — [`cell_edit::cell_is_block`]) are
/// excluded: a plain click on one must NOT move the caret, because a caret
/// inside the table span reveals it (collapses the grid to pipe source) and the
/// in-place edit's double-click entry (`widget-table-cell-edit-inplace`) would
/// lose the race to that reveal — click #1 collapsed the table, so click #2
/// found no cell zone and landed in raw text. Block cells enter edit via
/// double-click / the right-click menu instead; a plain click leaves the table
/// rendered.
///
/// Raster-free and cache-independent (re-parses the on-screen tables), so the
/// ids match the widget layer's `widget_id()` / `click_regions()` even on frames
/// served from the decoration cache. `viewport` scopes the scan like the provider.
pub fn table_edit_targets(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
) -> Vec<(u64, usize)> {
    let mut out: Vec<(u64, usize)> = Vec::new();
    let doc = state.doc.to_string();
    for span in table_spans(state, viewport) {
        let src = &doc[span.byte_range.clone()];
        let Some(widget) = TableWidget::from_source_meta(src, theme, &span.aligns, font_px) else {
            continue;
        };
        let base = span.byte_range.start;
        // Whole-widget fallback: a click off any cell still enters edit.
        out.push((widget.content_hash, base));
        for (i, range) in widget.cell_ranges.iter().enumerate() {
            if cell_edit::cell_is_block(&src[range.clone()]) {
                continue;
            }
            out.push((table_cell_id(widget.content_hash, i), base + range.end));
        }
    }
    out
}

/// What a right-click / wheel on a rendered table resolves to
/// (`widget-table-overflow-scroll`): the table's byte-range start (the
/// `buffer.table_overflow` key the menu toggles + the wheel scrolls) and its
/// natural (intrinsic) layout width (so the buffer panel can clamp the inset
/// scroll offset to `[0, natural − inset]`).
#[derive(Clone, Copy, Debug)]
pub struct TableOverflowTarget {
    pub byte_start: usize,
    pub natural_width: f32,
}

/// Build this frame's table overflow-interaction targets: for every on-screen
/// pipe table (viewport-scoped), map its whole-widget click id (the render
/// `content_hash`, i.e. `widget_id()`) to its [`TableOverflowTarget`]. The buffer
/// panel hit-tests the table's whole-widget click zone against the secondary
/// click (→ open the Fit ⇄ Scrollable menu) and the hovered wheel (→ scroll the
/// inset), then resolves the id here to the table's mode key + clamp bound.
///
/// Raster-free (uses [`TableWidget::from_source_meta`], no diagram blit) and
/// cache-independent, so the ids match the widget layer's `widget_id()` even on
/// frames served from the decoration cache. `viewport` scopes the scan like the
/// provider. status: widget-table-overflow-scroll
pub fn table_overflow_targets(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
) -> std::collections::HashMap<u64, TableOverflowTarget> {
    let mut out = std::collections::HashMap::new();
    let doc = state.doc.to_string();
    for span in table_spans(state, viewport) {
        let src = &doc[span.byte_range.clone()];
        let Some(widget) = TableWidget::from_source_meta(src, theme, &span.aligns, font_px) else {
            continue;
        };
        out.insert(
            widget.content_hash,
            TableOverflowTarget {
                byte_start: span.byte_range.start,
                natural_width: widget.natural_width(font_px),
            },
        );
    }
    out
}

/// Padding (logical pt) on each side of a cell's text within its column.
const CELL_PAD_X: f32 = 8.0;
/// Vertical padding (logical pt) above + below a row's text.
const CELL_PAD_Y: f32 = 4.0;
/// Stroke width (logical pt) for grid rules / borders.
const RULE_W: f32 = 1.0;
/// Approximate width of one character as a fraction of font size. Used to size
/// columns + wrap cells without a font handle (the widget is egui-free; the
/// painter owns the real font). A monospace-ish overestimate keeps text inside
/// its cell rather than clipping.
const CHAR_W_RATIO: f32 = 0.52;
/// Width multiplier for **bold** runs. The painter renders bold via a faux-bold
/// double-paint (`editor-egui` registers no bold family), so a bold run lays out
/// a touch wider than its plain glyphs; reserve for it so a bold header like
/// `WaveDrom` doesn't overflow `max_width` and wrap. Tuned against the real
/// `editor-egui` painter via `tools/table-snapshot`.
const BOLD_W_FACTOR: f32 = 1.12;
/// Width multiplier for inline `code` runs. They render in the Monospace family,
/// whose glyphs are wider than the proportional `CHAR_W_RATIO` estimate; without
/// this a code cell (e.g. a fenced-block label) overflows its column and wraps.
/// Tuned against the real painter via `tools/table-snapshot`.
const CODE_W_FACTOR: f32 = 1.25;
/// Small safety margin on every style-aware width measure, so the egui-free
/// estimate reserves slightly more than the real galley needs — the painter then
/// fits the content on one line rather than wrapping at the column boundary.
const WIDTH_SAFETY: f32 = 1.06;
/// Line height as a multiple of font size.
const LINE_H_RATIO: f32 = 1.35;

/// Per-column intrinsic-width cap as a multiple of font size. A block cell
/// (math / mermaid / wavedrom / image) reserves at least its scaled width, but
/// capped here so one wide block can't blow out the grid — the block then
/// scales-to-fit the (capped) column, aspect preserved. ~22 body characters
/// wide; comfortably holds a typical formula, a small diagram, or a thumbnail.
/// status: widget-table-render
const BLOCK_W_CAP: f32 = 22.0;
/// Minimum displayable width (multiple of font size) a block cell's column may
/// shrink to under overflow — keeps a math cell legible rather than collapsing
/// it to a sliver. status: widget-table-render
const BLOCK_MIN_W: f32 = 4.0;

/// Per-table overflow mode (`widget-table-overflow-scroll`), an ephemeral UI
/// flip toggled from the table's right-click context menu — deliberately NOT
/// encoded in the markdown (that would re-break round-trip), so every table
/// starts [`Fit`](TableOverflow::Fit) each session.
///
/// - [`Fit`](TableOverflow::Fit): the default — columns wrap / auto-size to the
///   available doc width (`column_widths`'s stretch-to-fill + overflow-shrink).
///   Byte-identical to the pre-overflow behavior.
/// - [`Scrollable`](TableOverflow::Scrollable): lay the grid out at its NATURAL
///   width (no stretch, no shrink — columns get their intrinsic widths, the total
///   may exceed the doc width), then clip it to the doc-width inset and offset it
///   by the table's horizontal scroll. The editor never scrolls horizontally;
///   the overflow is confined to the table's own inset viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TableOverflow {
    #[default]
    Fit,
    Scrollable,
}

/// The ephemeral per-table view state the overflow escape hatch needs
/// (`widget-table-overflow-scroll`): the [`TableOverflow`] mode plus, when
/// Scrollable, the inset's horizontal scroll offset in logical points (clamped
/// to `[0, natural_width − inset_width]` by the buffer panel that feeds wheel
/// deltas). Lives host-side keyed by the table's byte-range start; the provider
/// passes the resolved value into [`TableWidget::from_source`]. Default = Fit at
/// offset 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TableViewState {
    pub mode: TableOverflow,
    /// Horizontal scroll offset (logical pt) of the Scrollable inset; ignored
    /// (and held at 0) in Fit mode.
    pub h_offset: f32,
}

/// A cell's rendered block content (Phase B): an owned rasterized texture plus
/// the dpr it was rendered at, so the egui-free sizing math can recover the
/// block's intrinsic *logical* size (`physical / dpr`). Kind-agnostic: math,
/// mermaid, wavedrom, and image cells all carry the same `RenderedWidget` shape.
/// status: widget-table-render
struct CellBlock {
    rendered: RenderedWidget,
    dpr: f32,
}

impl CellBlock {
    /// Intrinsic logical (point) width of the block — physical px ÷ dpr.
    fn intrinsic_w(&self) -> f32 {
        self.rendered.width as f32 / self.dpr
    }

    /// Intrinsic logical (point) height of the block.
    fn intrinsic_h(&self) -> f32 {
        self.rendered.height as f32 / self.dpr
    }

    /// Scaled draw height when fit (aspect-preserved) into `content_w` logical
    /// pt of column space: a block wider than the column shrinks (height scales
    /// with it); a narrower block keeps its natural height (never upscaled).
    /// Mirrors `texture_cache::letterbox` so the reserved row matches the paint.
    fn scaled_height(&self, content_w: f32) -> f32 {
        let (iw, ih) = (self.intrinsic_w(), self.intrinsic_h());
        if content_w > 0.0 && iw > content_w {
            ih * (content_w / iw)
        } else {
            ih
        }
    }
}

/// One table cell: either Phase-A inline-markdown styled runs (markers stripped,
/// painted as [`BlockPaint::RichText`]) or a Phase-B rendered block (a math
/// formula, a mermaid / wavedrom diagram, or an image). The egui-free width /
/// wrap measure reads text runs directly (per-run style multipliers) or a
/// block's intrinsic size, so the reserve and the painted output agree.
/// status: widget-table-render
struct Cell {
    runs: Vec<StyledRun>,
    /// `Some` when this cell's source is a single renderable block (math /
    /// diagram / image): the rendered texture. `None` keeps the cell a pure text
    /// (Phase-A) cell.
    block: Option<CellBlock>,
}

impl Cell {
    /// Parse a cell's raw source. A cell whose trimmed source is exactly one
    /// renderable block — a math span (`$…$` / `$$…$$`), a one-line
    /// ```` ```mermaid ``` ```` / ```` ```wavedrom ``` ```` fence, or an
    /// `![alt](path)` image — renders as a Phase-B texture child (cache-backed);
    /// any other cell stays Phase-A inline-markdown runs. Each detector re-runs
    /// the renderer-unaware `editor-md` `&str` span scan over the cell source
    /// (no new detection logic).
    fn parse(raw: &str, ctx: CellCtx<'_>) -> Self {
        // A block cell carries the raw source as runs too, so the egui-free text
        // path (and edit reveal) still has the source text; the block texture
        // supersedes it at paint time.
        let block = detect_block(raw, ctx);
        Self { runs: parse_runs(raw, ctx.colors), block }
    }


    /// Style-aware natural (unwrapped) content width in logical pt, excluding
    /// cell padding. A block cell reserves its intrinsic width capped at
    /// [`BLOCK_W_CAP`] · font; a text cell sums each run's style-multiplied
    /// width (bold / monospace render wider) then applies [`WIDTH_SAFETY`].
    fn natural_content_width(&self, char_w: f32, font_px: f32) -> f32 {
        if let Some(b) = &self.block {
            return b.intrinsic_w().min(BLOCK_W_CAP * font_px);
        }
        let raw: f32 = self.runs.iter().map(|r| run_width(r, char_w)).sum();
        raw * WIDTH_SAFETY
    }

    /// Style-aware floor (longest unbreakable word) content width in logical pt,
    /// excluding padding. A word's style follows the run it lives in; a word
    /// spanning runs (no whitespace between them, e.g. a bold name touching a
    /// plain suffix) takes the max factor across the runs it overlaps, an
    /// over-reserve that's safe (the painter never needs more). Applies
    /// [`WIDTH_SAFETY`] like the natural measure.
    fn floor_content_width(&self, char_w: f32, font_px: f32) -> f32 {
        if let Some(b) = &self.block {
            // A block can shrink to fit, but not below a min-displayable width.
            return b.intrinsic_w().min(BLOCK_MIN_W * font_px);
        }
        // Build the visible text with per-char style factors, then scan words.
        let mut chars: Vec<(char, f32)> = Vec::new();
        for run in &self.runs {
            let factor = run_factor(run);
            for ch in run.text.chars() {
                chars.push((ch, factor));
            }
        }
        let mut longest = 0.0_f32;
        let mut cur = 0.0_f32;
        for (ch, factor) in chars {
            if ch.is_whitespace() {
                cur = 0.0;
            } else {
                cur += char_w * factor;
                if cur > longest {
                    longest = cur;
                }
            }
        }
        longest * WIDTH_SAFETY
    }
}

/// Per-cell render inputs threaded from the provider into [`Cell::parse`]: the
/// inline-markdown palette plus everything a Phase-B block render needs (math
/// glyph color, font size, dpr, the disk/mem cache). Grouped into one struct so
/// the cell parser stays under the per-fn arg limit and the no-block (edit-
/// target) path can pass a render-free [`CellCtx`] (`cache: None`, which still
/// renders math from the live pipeline — the mem cache makes it cheap).
/// status: widget-table-render
#[derive(Clone, Copy)]
struct CellCtx<'a> {
    colors: InlineColors,
    math_fg: [u8; 4],
    /// Theme-derived mermaid draw colors for a ```` ```mermaid ``` ```` cell.
    /// status: widget-table-render
    mermaid_colors: MermaidColors,
    /// Theme-derived WaveDrom draw colors for a ```` ```wavedrom ``` ```` cell.
    /// status: widget-table-render
    wavedrom_colors: WaveDromColors,
    /// Vault-bound image loader for an `![alt](path)` cell, or `None` in a
    /// read-only / non-vault host (those cells stay source). status: widget-table-render
    images: Option<&'a CellImageResolver>,
    font_px: f32,
    dpr: f32,
    cache: Option<&'a DiagramCacheCtx>,
    /// When false, block (math / diagram / image) cells are NOT rendered — the
    /// cell stays text. The raster-free edit-target path uses this:
    /// `content_hash` / `cell_ends` don't depend on the rendered block, so it
    /// skips the rasterize entirely. status: widget-table-cell-edit
    render_blocks: bool,
}

/// If `raw`'s trimmed source is exactly ONE renderable block — math, a one-line
/// mermaid / wavedrom fence, or an `![alt](path)` image — render it to a texture
/// child; otherwise `None` (the cell stays a Phase-A text cell). Each detector
/// re-runs the renderer-unaware `editor-md` `&str` span scan over the cell
/// source (no new detection logic), and a render failure also yields `None` so
/// the cell falls back to tinted source (`widget-render-error-fallback`). The
/// kinds are mutually exclusive (a cell is one fence / one image / one formula),
/// so the order only decides which `None`-returning probe runs first.
/// status: widget-table-render
fn detect_block(raw: &str, ctx: CellCtx<'_>) -> Option<CellBlock> {
    if !ctx.render_blocks {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    detect_math_block(trimmed, ctx)
        .or_else(|| detect_mermaid_block(trimmed, ctx))
        .or_else(|| detect_wavedrom_block(trimmed, ctx))
        .or_else(|| detect_image_block(trimmed, ctx))
}

/// A pure math cell (exactly one `$…$` / `$$…$$` span filling `trimmed`).
fn detect_math_block(trimmed: &str, ctx: CellCtx<'_>) -> Option<CellBlock> {
    let spans = math_spans_in_str(trimmed);
    // Exactly one span covering the whole trimmed cell → a pure math cell.
    let span = match spans.as_slice() {
        [only] if only.byte_range == (0..trimmed.len()) => only,
        _ => return None,
    };
    let kind = match span.kind {
        SpanMathKind::Inline => MathKind::Inline,
        SpanMathKind::Display => MathKind::Display,
    };
    let inner = &trimmed[span.inner_range.clone()];
    let rendered = render_math(inner, kind, ctx.font_px, ctx.dpr, ctx.math_fg, "", ctx.cache)?;
    Some(CellBlock { rendered, dpr: ctx.dpr })
}

/// A pure mermaid cell: a one-line ```` ```mermaid <src>``` ```` fence (the only
/// form a single-line pipe-table cell can hold) filling `trimmed`.
/// status: widget-table-render
fn detect_mermaid_block(trimmed: &str, ctx: CellCtx<'_>) -> Option<CellBlock> {
    let inner = mermaid_span_in_str(trimmed)?;
    let rendered = render_mermaid(inner, ctx.font_px, ctx.dpr, ctx.mermaid_colors, ctx.cache)?;
    Some(CellBlock { rendered, dpr: ctx.dpr })
}

/// A pure wavedrom cell: a one-line ```` ```wavedrom <src>``` ```` fence filling
/// `trimmed`. status: widget-table-render
fn detect_wavedrom_block(trimmed: &str, ctx: CellCtx<'_>) -> Option<CellBlock> {
    let inner = wavedrom_span_in_str(trimmed)?;
    let rendered = render_wavedrom(inner, ctx.font_px, ctx.dpr, ctx.wavedrom_colors, ctx.cache)?;
    Some(CellBlock { rendered, dpr: ctx.dpr })
}

/// A pure image cell: a single `![alt](path)` filling `trimmed`, the path
/// vault-resolved + loaded through the sandbox. `None` (stays source) when no
/// resolver is present, the path doesn't resolve, or the decode fails.
/// status: widget-table-render
fn detect_image_block(trimmed: &str, ctx: CellCtx<'_>) -> Option<CellBlock> {
    let path = image_span_in_str(trimmed)?;
    let resolver = ctx.images?;
    let resolved = resolver.resolve(path)?;
    let rendered = render_image(&resolved.bytes, &resolved.key, ctx.dpr, ctx.cache)?;
    Some(CellBlock { rendered, dpr: ctx.dpr })
}

/// Per-character width multiplier for a run's style (bold faux-bold / monospace
/// `code` both wider than plain proportional). `code` dominates `bold` when a run
/// is somehow both.
const fn run_factor(run: &StyledRun) -> f32 {
    if run.code {
        CODE_W_FACTOR
    } else if run.bold {
        BOLD_W_FACTOR
    } else {
        1.0
    }
}

/// Style-aware width (logical pt, no padding) of one run's text on a single line.
fn run_width(run: &StyledRun, char_w: f32) -> f32 {
    run.text.chars().count() as f32 * char_w * run_factor(run)
}

/// Map a column's GFM alignment to the painter's [`TextAlign`]. Shared by the
/// `paint_list` and composite cell-text paths so both honor column alignment.
fn column_text_align(aligns: &[ColumnAlign], col: usize) -> TextAlign {
    match aligns.get(col).copied().unwrap_or(ColumnAlign::None) {
        ColumnAlign::Right => TextAlign::Right,
        ColumnAlign::Center => TextAlign::Center,
        ColumnAlign::Left | ColumnAlign::None => TextAlign::Left,
    }
}

/// One parsed table: header cells, body rows, and per-column alignment. Built
/// from the raw pipe-and-dash source by [`parse_table`]. `header_ranges` /
/// `row_ranges` carry, per cell, the byte range *within the table source* of
/// that cell's trimmed content — the editable source region for an in-place cell
/// edit (`widget-table-cell-edit-inplace`); its `end` alone is the caret target
/// for a cell click (`widget-table-cell-edit`). Parallel to `header` / `rows`.
struct Table {
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    aligns: Vec<ColumnAlign>,
    header_ranges: Vec<std::ops::Range<usize>>,
    row_ranges: Vec<Vec<std::ops::Range<usize>>>,
}

/// Theme-derived colors for the painted grid (`widget-render-theme-color`).
/// `inline` carries the per-run palette the cells' inline-markdown parse needs
/// (base text, link, code background) so a `**bold**` / `[link]` cell renders in
/// the same colors as surrounding prose.
#[derive(Clone, Copy)]
struct TableColors {
    text: Color,
    header_bg: Color,
    rule: Color,
    inline: InlineColors,
    /// Math-glyph foreground for a cell's rendered formula (matches surrounding
    /// prose), threaded into [`render_math`]. status: widget-table-render
    math_fg: Color,
}

const fn table_colors(theme: Option<&Theme>) -> TableColors {
    match theme {
        Some(t) => TableColors {
            text: t.palette.fg,
            header_bg: t.markdown.code_bg,
            rule: with_alpha(t.palette.fg, 80),
            inline: InlineColors {
                text: t.palette.fg,
                link: t.markdown.link,
                code_bg: t.markdown.code_bg,
            },
            math_fg: t.palette.fg,
        },
        None => TableColors {
            text: Color::rgb(40, 40, 40),
            header_bg: Color::rgba(170, 120, 220, 25),
            rule: Color::rgba(120, 120, 120, 120),
            inline: InlineColors {
                text: Color::rgb(40, 40, 40),
                link: Color::rgb(0, 90, 200),
                code_bg: Color::rgba(120, 120, 120, 30),
            },
            math_fg: Color::rgb(40, 40, 40),
        },
    }
}

/// The cell-parse render context derived from a table's baked colors + render
/// inputs. Shared by [`TableWidget::from_source`] and the parser. The diagram
/// colors + image resolver ride in [`BlockInputs`] (theme-derived, host-owned)
/// so this stays a `const fn` and the no-block edit-target path can pass empties.
const fn cell_ctx<'a>(
    colors: &TableColors,
    font_px: f32,
    dpr: f32,
    cache: Option<&'a DiagramCacheCtx>,
    render_blocks: bool,
    blocks: BlockInputs<'a>,
) -> CellCtx<'a> {
    let fg = colors.math_fg;
    CellCtx {
        colors: colors.inline,
        math_fg: [fg.r, fg.g, fg.b, fg.a],
        mermaid_colors: blocks.mermaid_colors,
        wavedrom_colors: blocks.wavedrom_colors,
        images: blocks.images,
        font_px,
        dpr,
        cache,
        render_blocks,
    }
}

/// Theme-derived block-render inputs threaded from the provider into the cell
/// parser: the mermaid / wavedrom draw colors and the vault image resolver. Kept
/// apart from the `const`-baked [`TableColors`] because the diagram colors come
/// from a non-`const` theme map and the resolver is a borrowed host handle.
/// status: widget-table-render
#[derive(Clone, Copy)]
struct BlockInputs<'a> {
    mermaid_colors: MermaidColors,
    wavedrom_colors: WaveDromColors,
    images: Option<&'a CellImageResolver>,
}

const fn with_alpha(c: Color, a: u8) -> Color {
    Color::rgba(c.r, c.g, c.b, a)
}

/// Tag bit OR-ed into a table-cell widget-click id. Distinct from
/// `editor_md::links::WIKILINK_WIDGET_TAG` (bit 62) and `MERMAID_REGION_TAG`
/// (bit 61) so the buffer panel tells per-cell edit clicks apart by registry
/// membership with no bit collisions. status: widget-table-cell-edit
pub const TABLE_CELL_TAG: u64 = 1 << 60;

/// Collision-free id for cell `index` (row-major: header cells, then each body
/// row left→right) of the table whose render carries `content_hash`. Mixes the
/// two into the low 60 bits and OR-s [`TABLE_CELL_TAG`], leaving the mermaid
/// (bit 61) and wikilink (bit 62) tags clear. Mirrors `mod::region_click_id`.
const fn table_cell_id(content_hash: u64, index: usize) -> u64 {
    let mixed = content_hash
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (index as u64).wrapping_add(0x1234_5678);
    TABLE_CELL_TAG | (mixed & ((1 << 60) - 1))
}

/// Render inputs for [`TableWidget::from_source`], grouped so the constructor
/// stays under the argument cap and the new ephemeral overflow
/// [`TableViewState`](view) rides alongside the existing font / dpr / cache /
/// image-resolver inputs. status: widget-table-render
#[derive(Clone, Copy)]
pub struct TableRenderInputs<'a> {
    pub font_px: f32,
    pub dpr: f32,
    pub cache: Option<&'a DiagramCacheCtx>,
    pub images: Option<&'a CellImageResolver>,
    /// The table's resolved ephemeral overflow mode + inset scroll offset
    /// (`widget-table-overflow-scroll`); `TableViewState::default()` = Fit.
    pub view: TableViewState,
}

/// A natively-painted pipe-table block widget. Full-width own-height block; its
/// height is derived from the wrapped cell content at the paint-time width.
pub struct TableWidget {
    table: Table,
    colors: TableColors,
    /// Content hash over (source, theme colors, font) — the diff / cache key.
    content_hash: u64,
    /// Per-cell source range (row-major, matching [`TableWidget::cell_boxes`] /
    /// the paint order): the byte range *within the table source* of the cell's
    /// trimmed content. The `end` is the cell click's caret target
    /// (`widget-table-cell-edit`); the whole `start..end` is the editable region
    /// an in-place cell edit binds to (`widget-table-cell-edit-inplace`).
    cell_ranges: Vec<std::ops::Range<usize>>,
    /// Whether ANY cell carries Phase-B block content (a rendered math / mermaid
    /// / wavedrom / image texture). When false the table paints via the
    /// byte-identical `paint_list` path (Phase A); when true it paints as a
    /// `composite` (native text children +
    /// texture block children). status: widget-table-render
    has_block: bool,
    /// Ephemeral per-table overflow mode + inset scroll offset
    /// (`widget-table-overflow-scroll`). `Fit` (default) keeps the byte-identical
    /// fit/wrap path; `Scrollable` lays the grid out at natural width, clipped to
    /// the doc-width inset and offset by `view.h_offset`.
    view: TableViewState,
}

impl TableWidget {
    /// Build a table widget from the raw block source, or `None` if the source
    /// isn't a well-formed pipe table (no rule row). A `None` keeps the tinted
    /// source visible (`widget-render-error-fallback`). `dpr` / `cache` drive
    /// per-cell Phase-B block (math) rendering; a pure-text table ignores them.
    pub fn from_source(
        source: &str,
        theme: Option<&Theme>,
        aligns: &[ColumnAlign],
        render: TableRenderInputs<'_>,
    ) -> Option<Self> {
        let TableRenderInputs { font_px, dpr, cache, images, view } = render;
        let colors = table_colors(theme);
        let blocks = BlockInputs {
            mermaid_colors: super::theme_mermaid_colors(theme),
            wavedrom_colors: super::theme_wavedrom_colors(theme),
            images,
        };
        let ctx = cell_ctx(&colors, font_px, dpr, cache, true, blocks);
        Self::build(source, aligns, &colors, ctx, font_px, view)
    }

    /// Raster-free build for the edit-target path: parses the table WITHOUT
    /// rendering any block (math) cell (`render_blocks: false`). The
    /// `content_hash` (source/theme/font) and `cell_ends` (byte offsets) are
    /// independent of the rendered block, so the ids match the rendering
    /// `from_source` exactly while paying no rasterize. status: widget-table-cell-edit
    fn from_source_meta(
        source: &str,
        theme: Option<&Theme>,
        aligns: &[ColumnAlign],
        font_px: f32,
    ) -> Option<Self> {
        let colors = table_colors(theme);
        // `render_blocks: false` skips all block detection, so the diagram colors
        // / resolver are never read; theme-derive them anyway to keep one shape.
        let blocks = BlockInputs {
            mermaid_colors: super::theme_mermaid_colors(theme),
            wavedrom_colors: super::theme_wavedrom_colors(theme),
            images: None,
        };
        let ctx = cell_ctx(&colors, font_px, 1.0, None, false, blocks);
        // The edit-target path never paints, so the overflow mode is irrelevant;
        // default Fit keeps `content_hash` / `cell_ends` independent of it.
        Self::build(source, aligns, &colors, ctx, font_px, TableViewState::default())
    }

    /// Shared body of the two constructors: parse, hash, flatten cell ends, and
    /// flag whether any cell carries block content.
    fn build(
        source: &str,
        aligns: &[ColumnAlign],
        colors: &TableColors,
        ctx: CellCtx<'_>,
        font_px: f32,
        view: TableViewState,
    ) -> Option<Self> {
        let table = parse_table(source, aligns, ctx)?;
        let content_hash = hash_table(source, colors, font_px);
        // Flatten the per-cell content ranges in the same row-major order
        // `cell_boxes` / `paint_list` iterate (header first, then each row).
        let mut cell_ranges = Vec::with_capacity(table.header_ranges.len());
        cell_ranges.extend_from_slice(&table.header_ranges);
        for r in &table.row_ranges {
            cell_ranges.extend_from_slice(r);
        }
        let has_block = std::iter::once(&table.header)
            .chain(table.rows.iter())
            .any(|row| row.iter().any(|c| c.block.is_some()));
        Some(Self { table, colors: *colors, content_hash, cell_ranges, has_block, view })
    }

    /// The table's intrinsic (natural-layout) total width in logical pt — the sum
    /// of the columns' natural widths. In Scrollable mode this is the width the
    /// grid lays out at (before the inset clip); the buffer panel clamps the
    /// scroll offset against `natural_width − inset_width`.
    /// status: widget-table-overflow-scroll
    pub fn natural_width(&self, font_px: f32) -> f32 {
        self.measure_columns(font_px).0.iter().sum()
    }

    /// The horizontal shift (logical pt, ≤ 0) the composite grid + its cells are
    /// translated by for the Scrollable inset: `−clamp(h_offset, 0, natural −
    /// inset)`. `0` in Fit mode (the grid sits at its natural origin). Shared by
    /// `composite_children` and `click_regions` so the painted geometry and the
    /// cell hit-zones move together. status: widget-table-overflow-scroll
    fn scroll_shift(&self, font_px: f32, inset_width: f32) -> f32 {
        if self.view.mode != TableOverflow::Scrollable {
            return 0.0;
        }
        let inset_w = inset_width.max(1.0);
        let max_off = (self.natural_width(font_px) - inset_w).max(0.0);
        -self.view.h_offset.clamp(0.0, max_off)
    }

    /// Per-cell layout boxes in logical points (row-major: header, then each
    /// body row), plus the table's total height. The box geometry is derived
    /// purely from [`column_widths`](Self::column_widths) + [`row_height`](Self::row_height),
    /// so it stays in lockstep with `paint_list` and with `cell_ends`. Used by
    /// `click_regions` to place per-cell hit zones. status: widget-table-cell-edit
    fn cell_boxes(&self, font_size: f32, width: f32) -> (Vec<(f32, f32, f32, f32)>, f32) {
        let widths = self.column_widths(font_size, width);
        let mut boxes = Vec::with_capacity(self.cell_ranges.len());
        let mut y = 0.0_f32;
        let rows_iter = std::iter::once(&self.table.header).chain(self.table.rows.iter());
        for cells in rows_iter {
            let row_h = Self::row_height(cells, &widths, font_size);
            let mut x = 0.0_f32;
            for i in 0..cells.len() {
                let col_w = widths.get(i).copied().unwrap_or(0.0);
                boxes.push((x, y, col_w, row_h));
                x += col_w;
            }
            y += row_h;
        }
        (boxes, y)
    }

    /// Number of columns, derived from the widest row (header included).
    fn col_count(&self) -> usize {
        let body = self.table.rows.iter().map(Vec::len).max().unwrap_or(0);
        self.table.header.len().max(body).max(1)
    }

    /// Per-column `(natural, floor)` widths (logical pt, padding included). The
    /// *natural* width is the longest full line; the *floor* is the longest
    /// single word (unbreakable, since cells wrap on word boundaries). Both are
    /// raised to a one-character minimum cell, and natural is kept ≥ floor. This
    /// is the mode-independent measure both [`column_widths`](Self::column_widths)
    /// branches start from. status: widget-table-render
    fn measure_columns(&self, font_px: f32) -> (Vec<f32>, Vec<f32>) {
        let cols = self.col_count();
        let char_w = font_px * CHAR_W_RATIO;
        let pad = CELL_PAD_X * 2.0;
        let mut natural = vec![0.0_f32; cols];
        let mut floor = vec![0.0_f32; cols];
        let mut consider = |cells: &[Cell]| {
            for (i, cell) in cells.iter().enumerate().take(cols) {
                let nat = cell.natural_content_width(char_w, font_px) + pad;
                let flr = cell.floor_content_width(char_w, font_px) + pad;
                if nat > natural[i] {
                    natural[i] = nat;
                }
                if flr > floor[i] {
                    floor[i] = flr;
                }
            }
        };
        consider(&self.table.header);
        for row in &self.table.rows {
            consider(row);
        }
        let min_cell = char_w + pad;
        for i in 0..cols {
            floor[i] = floor[i].max(min_cell);
            natural[i] = natural[i].max(floor[i]);
        }
        (natural, floor)
    }

    /// Column widths (logical pt) at `total_width`, mode-dependent
    /// (`widget-table-overflow-scroll`):
    ///
    /// - **Scrollable:** return the raw *natural* widths — no stretch-to-fill, no
    ///   overflow-shrink. The grid lays out at its intrinsic width (which may
    ///   exceed `total_width`); the composite path clips it to the doc-width inset
    ///   and offsets it by the scroll. `total_width` is ignored here.
    /// - **Fit (default):** when the table fits, columns get their natural width
    ///   stretched proportionally to fill `total_width`; when it overflows, only
    ///   the flexible part (natural − floor) is shrunk so no column drops below
    ///   its longest word (that word can't spill into the next column). If even
    ///   the floors don't fit, every column keeps its floor and the table extends
    ///   past the content box rather than overlapping.
    fn column_widths(&self, font_px: f32, total_width: f32) -> Vec<f32> {
        let cols = self.col_count();
        let (natural, floor) = self.measure_columns(font_px);
        let nat_sum: f32 = natural.iter().sum();
        if nat_sum <= 0.0 {
            return natural;
        }
        // Scrollable: natural widths verbatim (no fit-stretch, no shrink). The
        // inset clip + scroll offset live in the composite path, not here.
        if self.view.mode == TableOverflow::Scrollable {
            return natural;
        }
        if nat_sum <= total_width {
            // Content fits: distribute the slack so the table fills the available
            // width exactly. Slack goes proportionally to each column's natural
            // width, so text-heavy columns grow more (a narrow label column stays
            // tight, the prose columns absorb most of the stretch).
            let slack = total_width - nat_sum;
            return (0..cols)
                .map(|i| natural[i] + slack * (natural[i] / nat_sum))
                .collect();
        }
        // Overflow: shrink only the flexible part (natural − floor) toward the
        // floors so no column drops below its longest word.
        let floor_sum: f32 = floor.iter().sum();
        let avail_flex = total_width - floor_sum;
        if avail_flex <= 0.0 {
            return floor;
        }
        let flex_sum = nat_sum - floor_sum;
        let scale = if flex_sum > 0.0 { avail_flex / flex_sum } else { 0.0 };
        (0..cols)
            .map(|i| floor[i] + (natural[i] - floor[i]) * scale)
            .collect()
    }

    /// Style-aware number of visual lines a [`Cell`] occupies in a column of
    /// `width` logical pt at `font_px`. Greedy word-wrap, but each word's width
    /// uses its run's style multiplier (bold / monospace wider) just like
    /// [`column_widths`], so the height estimate agrees with the style-aware
    /// width reserve: with the wider columns those styles earn, a bold/code cell
    /// estimates *fewer* (never more) wraps than the flat ratio would, so height
    /// never under-reserves and clips. Always at least one line. The painter
    /// still owns the true wrap; this only sizes the reserved box.
    fn cell_line_count(cell: &Cell, width: f32, font_px: f32) -> usize {
        if cell.block.is_some() {
            // A block cell contributes height through `row_height`'s block
            // branch (its scaled texture height), not the text-line estimate.
            return 1;
        }
        let char_w = font_px * CHAR_W_RATIO;
        let avail = (width - CELL_PAD_X * 2.0).max(char_w);
        // Split the visible text into words, each carrying its widest style
        // factor (a word straddling runs takes the max, matching the floor
        // measure's safe over-reserve). Whitespace collapses to single spaces.
        let words = Self::styled_words(cell);
        let space_w = char_w; // a single inter-word space, plain width
        let mut lines = 1usize;
        let mut cur = 0.0_f32;
        for (wlen, factor) in words {
            let ww = wlen as f32 * char_w * factor;
            if cur <= 0.0 {
                cur = ww;
            } else if cur + space_w + ww <= avail {
                cur += space_w + ww;
            } else {
                lines += 1;
                cur = ww;
            }
        }
        lines
    }

    /// The visible words of a cell as `(char_count, style_factor)`, where a word
    /// spanning multiple runs takes the max style factor across the runs it
    /// touches (a safe over-reserve). Whitespace separates words.
    fn styled_words(cell: &Cell) -> Vec<(usize, f32)> {
        let mut words: Vec<(usize, f32)> = Vec::new();
        let mut len = 0usize;
        let mut factor = 1.0_f32;
        for run in &cell.runs {
            let rf = run_factor(run);
            for ch in run.text.chars() {
                if ch.is_whitespace() {
                    if len > 0 {
                        words.push((len, factor));
                        len = 0;
                        factor = 1.0;
                    }
                } else {
                    len += 1;
                    factor = factor.max(rf);
                }
            }
        }
        if len > 0 {
            words.push((len, factor));
        }
        words
    }

    /// Height (logical pt) of the row whose cells are `cells`, at `widths`.
    /// Measured egui-free off each cell's styled runs (markers already stripped)
    /// via [`cell_line_count`](Self::cell_line_count), whose per-word width
    /// honors bold / monospace styling — the same factors [`column_widths`] uses
    /// to reserve the column. Because wider styles widen their column, the line
    /// estimate is conservative (never more wraps than reserved), so the row
    /// height doesn't under-reserve and clip. The painter owns the true wrap.
    /// status: widget-table-render
    fn row_height(cells: &[Cell], widths: &[f32], font_px: f32) -> f32 {
        let line_h = font_px * LINE_H_RATIO;
        // Text path: tallest wrapped text cell (block cells count as 1 line and
        // contribute height through the block branch below instead).
        let max_lines = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = widths.get(i).copied().unwrap_or(0.0);
                Self::cell_line_count(c, w, font_px)
            })
            .max()
            .unwrap_or(1);
        let text_h = max_lines as f32 * line_h + CELL_PAD_Y * 2.0;
        // Block path: a math/diagram cell fits (aspect-preserved) into its
        // column's content width; its scaled height (plus padding) may exceed
        // the text rows, so the row grows to hold it. status: widget-table-render
        let block_h = cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let b = c.block.as_ref()?;
                let content_w = (widths.get(i).copied().unwrap_or(0.0) - CELL_PAD_X * 2.0).max(1.0);
                Some(b.scaled_height(content_w) + CELL_PAD_Y * 2.0)
            })
            .fold(0.0_f32, f32::max);
        text_h.max(block_h)
    }

    /// Build the composite child render-items for a table with block cells
    /// (`widget-table-render`): one Native child carrying the whole grid chrome
    /// (header bg, rules, borders) at the widget origin, then per cell either a
    /// Native RichText child (text cells, Phase A) or a Texture child (a
    /// rendered math block) positioned in the cell's content box. The grid
    /// geometry comes from the SAME `column_widths` / `row_height` solve the
    /// `paint_list` path uses, so a composite table lays out identically to a
    /// plain one plus the block cells. status: widget-table-render
    fn composite_children(&self, font_size: f32, width: f32) -> Vec<ChildItem> {
        let widths = self.column_widths(font_size, width);
        let mut children: Vec<ChildItem> = Vec::new();
        let (total_h, cell_layout) = self.layout_cells(&widths, font_size);
        let total_w: f32 = widths.iter().sum();

        // Scrollable inset (`widget-table-overflow-scroll`): shift every child
        // LEFT by the clamped scroll offset and clip them to the doc-width inset
        // box, so a natural-width grid wider than `width` shows only the inset's
        // slice (earlier columns scroll off the left, later columns hidden off
        // the right). Fit leaves both at their no-op identity (`x_shift = 0`,
        // `clip = None`), so its children are byte-identical to before.
        let x_shift = self.scroll_shift(font_size, width);
        let clip = if self.view.mode == TableOverflow::Scrollable {
            Some(ChildRect { x: 0.0, y: 0.0, w: width.max(1.0), h: total_h })
        } else {
            None
        };

        // Chrome as one native child spanning the whole table box.
        children.push(ChildItem {
            rect: ChildRect { x: x_shift, y: 0.0, w: total_w, h: total_h },
            kind: ChildKind::Native(self.grid_chrome(&widths, total_w, total_h, font_size)),
            clip,
        });

        // Per-cell content children.
        let rows_iter = std::iter::once(&self.table.header).chain(self.table.rows.iter());
        let mut cell_idx = 0usize;
        for cells in rows_iter {
            for (i, cell) in cells.iter().enumerate() {
                let (cx, cy, cw, ch) = cell_layout[cell_idx];
                cell_idx += 1;
                let content_w = (cw - CELL_PAD_X * 2.0).max(1.0);
                let content_x = cx + CELL_PAD_X + x_shift;
                let content_y = cy + CELL_PAD_Y;
                let content_h = (ch - CELL_PAD_Y * 2.0).max(1.0);
                if let Some(b) = &cell.block {
                    let geo = CellGeo { x: content_x, y: content_y, w: content_w, h: content_h };
                    children.push(Self::block_child(b, geo, clip));
                } else if !cell.runs.is_empty() {
                    let geo = CellGeo { x: content_x, y: content_y, w: content_w, h: ch };
                    children.push(self.text_child(cell, i, geo, clip));
                }
            }
        }
        children
    }

    /// Flatten the grid into per-cell boxes `(x, y, w, h)` (row-major) plus the
    /// table's total height — the shared geometry the composite children sit in.
    fn layout_cells(&self, widths: &[f32], font_size: f32) -> (f32, Vec<(f32, f32, f32, f32)>) {
        let mut boxes = Vec::with_capacity(self.cell_ranges.len());
        let mut y = 0.0_f32;
        for cells in std::iter::once(&self.table.header).chain(self.table.rows.iter()) {
            let row_h = Self::row_height(cells, widths, font_size);
            let mut x = 0.0_f32;
            for i in 0..cells.len() {
                let col_w = widths.get(i).copied().unwrap_or(0.0);
                boxes.push((x, y, col_w, row_h));
                x += col_w;
            }
            y += row_h;
        }
        (y, boxes)
    }

    /// The grid chrome (header background, per-row rules, top rule, vertical
    /// column separators) as a native paint list relative to the table box's
    /// top-left — identical primitives to the `paint_list` path's chrome.
    fn grid_chrome(&self, widths: &[f32], total_w: f32, total_h: f32, font_size: f32) -> Vec<BlockPaint> {
        let mut list: Vec<BlockPaint> = Vec::new();
        let header_h = Self::row_height(&self.table.header, widths, font_size);
        list.push(BlockPaint::Rect {
            x: 0.0,
            y: 0.0,
            w: total_w,
            h: header_h,
            color: self.colors.header_bg,
        });
        let mut y = 0.0_f32;
        for cells in std::iter::once(&self.table.header).chain(self.table.rows.iter()) {
            y += Self::row_height(cells, widths, font_size);
            list.push(BlockPaint::Line {
                from: (0.0, y),
                to: (total_w, y),
                width: RULE_W,
                color: self.colors.rule,
            });
        }
        list.push(BlockPaint::Line {
            from: (0.0, 0.0),
            to: (total_w, 0.0),
            width: RULE_W,
            color: self.colors.rule,
        });
        let mut x = 0.0_f32;
        for col_w in widths.iter().chain(std::iter::once(&0.0_f32)) {
            list.push(BlockPaint::Line {
                from: (x, 0.0),
                to: (x, total_h),
                width: RULE_W,
                color: self.colors.rule,
            });
            x += col_w;
        }
        list
    }

    /// A text cell's composite child: a single-element Native paint list holding
    /// the cell's wrapping `RichText`, in a child rect that IS the cell content
    /// box (so the RichText anchors at the child's top-left). Mirrors the
    /// `paint_list` per-cell `RichText` (same runs, `max_width`, alignment).
    /// `clip` (`Some` only for a Scrollable table) confines it to the inset.
    fn text_child(&self, cell: &Cell, col: usize, geo: CellGeo, clip: Option<ChildRect>) -> ChildItem {
        let text_align = column_text_align(&self.table.aligns, col);
        ChildItem {
            rect: ChildRect { x: geo.x, y: geo.y, w: geo.w, h: geo.h },
            kind: ChildKind::Native(vec![BlockPaint::RichText {
                x: 0.0,
                y: 0.0,
                runs: cell.runs.clone(),
                max_width: geo.w.max(1.0),
                align: text_align,
            }]),
            clip,
        }
    }

    /// A block cell's composite child: the rendered texture, letterboxed into the
    /// cell content box (the painter preserves aspect, centering). The cell row
    /// is already sized (via `row_height`'s block branch) to hold the scaled
    /// height, so the texture fits without excess band. `clip` (`Some` only for a
    /// Scrollable table) confines it to the inset. status: widget-table-render
    fn block_child(b: &CellBlock, geo: CellGeo, clip: Option<ChildRect>) -> ChildItem {
        ChildItem {
            rect: ChildRect { x: geo.x, y: geo.y, w: geo.w, h: geo.h },
            kind: ChildKind::Texture(ChildTexture {
                rgba: b.rendered.rgba.clone(),
                width: b.rendered.width,
                height: b.rendered.height,
                id: b.rendered.content_hash,
            }),
            clip,
        }
    }
}

/// One composite cell child's content-box geometry (logical pt, widget-relative,
/// already horizontally shifted for a Scrollable inset). Bundles the four
/// coordinates so the child builders stay under the argument cap.
/// status: widget-table-overflow-scroll
#[derive(Clone, Copy)]
struct CellGeo {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl BlockWidget for TableWidget {
    fn measure(&self, font_size: f32, width: f32) -> f32 {
        let widths = self.column_widths(font_size, width);
        let mut h = Self::row_height(&self.table.header, &widths, font_size);
        for row in &self.table.rows {
            h += Self::row_height(row, &widths, font_size);
        }
        h
    }

    fn widget_id(&self) -> u64 {
        self.content_hash
    }

    fn handles_click(&self) -> bool {
        // A body click enters edit (place the caret in the hidden source),
        // matching display math / mermaid. status: widget-block-click-to-edit
        true
    }

    fn click_regions(&self, font_size: f32, width: f32) -> Vec<WidgetClickRegion> {
        // One normalized hit region per cell, in the same order as `cell_ends`,
        // so a cell click resolves to that cell's caret target. Normalized to
        // the painted box: x/w against `width` (the content box the painter
        // fills 1:1), y/h against the table's total height. In Scrollable mode the
        // grid is laid out at natural width and shifted left by the clamped scroll
        // offset, so the cell x's are shifted to match what's painted in the inset
        // (a cell scrolled out of the inset maps outside [0,1] and can't be hit).
        // status: widget-table-cell-edit / widget-table-overflow-scroll
        if width <= 0.0 {
            return Vec::new();
        }
        let (boxes, total_h) = self.cell_boxes(font_size, width);
        if total_h <= 0.0 {
            return Vec::new();
        }
        let x_shift = self.scroll_shift(font_size, width);
        boxes
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| WidgetClickRegion {
                x: (x + x_shift) / width,
                y: y / total_h,
                w: w / width,
                h: h / total_h,
                id: table_cell_id(self.content_hash, i),
            })
            .collect()
    }

    fn composite(&self, font_size: f32, width: f32) -> Option<Vec<ChildItem>> {
        // The composite (container) path is taken when EITHER a cell hosts a
        // rendered block (math / diagram / image), OR the table is Scrollable —
        // Scrollable needs the per-child clip + scroll offset, which only the
        // composite path carries (`paint_list` can't clip). A plain Fit table
        // returns `None` so its byte-identical `paint_list` is used instead.
        // status: widget-table-render / widget-table-overflow-scroll
        if !self.has_block && self.view.mode != TableOverflow::Scrollable {
            return None;
        }
        Some(self.composite_children(font_size, width))
    }

    fn paint_list(&self, font_size: f32, width: f32) -> Option<Vec<BlockPaint>> {
        let widths = self.column_widths(font_size, width);
        let total_w: f32 = widths.iter().sum();
        let line_h = font_size * LINE_H_RATIO;
        let mut list: Vec<BlockPaint> = Vec::new();

        // Header background strip.
        let header_h = Self::row_height(&self.table.header, &widths, font_size);
        list.push(BlockPaint::Rect {
            x: 0.0,
            y: 0.0,
            w: total_w,
            h: header_h,
            color: self.colors.header_bg,
        });

        // Per-row text + the horizontal rule under each row.
        let mut y = 0.0_f32;
        let rows_iter = std::iter::once(&self.table.header).chain(self.table.rows.iter());
        let mut row_tops: Vec<f32> = Vec::new();
        for cells in rows_iter {
            row_tops.push(y);
            let row_h = Self::row_height(cells, &widths, font_size);
            let mut x = 0.0_f32;
            for (i, cell) in cells.iter().enumerate() {
                let col_w = widths.get(i).copied().unwrap_or(0.0);
                let text_align = column_text_align(&self.table.aligns, i);
                if !cell.runs.is_empty() {
                    // One wrapping rich-text block per cell: the painter owns the
                    // real wrap (the egui-free height measure above only
                    // approximates it via character counts), and lays the styled
                    // runs as a single multi-format galley inside the cell's
                    // content box so inline markdown (bold/italic/code/strike/
                    // link) renders in place. The block is anchored at the cell's
                    // left content edge and aligned within `max_width`, so the
                    // column's alignment is honored without spilling into the
                    // neighbor. status: widget-table-render
                    let max_width = (col_w - CELL_PAD_X * 2.0).max(font_size);
                    let top = y + CELL_PAD_Y + (line_h - font_size) * 0.5;
                    list.push(BlockPaint::RichText {
                        x: x + CELL_PAD_X,
                        y: top,
                        runs: cell.runs.clone(),
                        max_width,
                        align: text_align,
                    });
                }
                x += col_w;
            }
            y += row_h;
            list.push(BlockPaint::Line {
                from: (0.0, y),
                to: (total_w, y),
                width: RULE_W,
                color: self.colors.rule,
            });
        }
        let total_h = y;

        // Outer border + top rule + vertical column separators.
        list.push(BlockPaint::Line {
            from: (0.0, 0.0),
            to: (total_w, 0.0),
            width: RULE_W,
            color: self.colors.rule,
        });
        let mut x = 0.0_f32;
        for col_w in widths.iter().chain(std::iter::once(&0.0_f32)) {
            list.push(BlockPaint::Line {
                from: (x, 0.0),
                to: (x, total_h),
                width: RULE_W,
                color: self.colors.rule,
            });
            x += col_w;
        }

        Some(list)
    }
}

/// Parse the raw pipe-table block into header + body rows + alignments, or
/// `None` if there's no `|---|` delimiter (rule) row — i.e. it isn't a GFM
/// table. The second non-blank line must be the delimiter row.
fn parse_table(source: &str, aligns: &[ColumnAlign], ctx: CellCtx<'_>) -> Option<Table> {
    // Non-blank raw lines paired with their byte offset within `source`, so each
    // cell's content-end can be reported as a source-relative offset
    // (`widget-table-cell-edit`). `split_inclusive` keeps the newline, so the
    // running offset stays exact.
    let mut lines: Vec<(&str, usize)> = Vec::new();
    let mut off = 0usize;
    for raw in source.split_inclusive('\n') {
        if !raw.trim().is_empty() {
            lines.push((raw, off));
        }
        off += raw.len();
    }
    if lines.len() < 2 {
        return None;
    }
    if !is_delimiter_row(lines[1].0) {
        return None;
    }
    // Split a raw line into parsed cells (inline markdown → styled runs) + their
    // source-relative content `start..end` ranges (the editable cell region).
    let split_abs = |&(raw, line_start): &(&str, usize)| -> (Vec<Cell>, Vec<std::ops::Range<usize>>) {
        let mut cells = Vec::new();
        let mut ranges = Vec::new();
        for cell in split_row_cells(raw) {
            cells.push(Cell::parse(&cell.text, ctx));
            ranges.push((line_start + cell.start)..(line_start + cell.end));
        }
        (cells, ranges)
    };
    let (header, header_ranges) = split_abs(&lines[0]);
    let mut rows = Vec::new();
    let mut row_ranges = Vec::new();
    for line in &lines[2..] {
        let (cells, ranges) = split_abs(line);
        rows.push(cells);
        row_ranges.push(ranges);
    }
    Some(Table { header, rows, aligns: aligns.to_vec(), header_ranges, row_ranges })
}

/// Whether `line` is a GFM delimiter row: every cell is dashes with optional
/// leading/trailing colons (`:--`, `:-:`, `--:`, `---`).
fn is_delimiter_row(line: &str) -> bool {
    let cells = split_cells(line);
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| {
        let bytes = c.trim().as_bytes();
        !bytes.is_empty()
            && bytes.iter().all(|&b| b == b'-' || b == b':')
            && bytes.contains(&b'-')
    })
}

/// Split a table row into its cell strings (text only), stripping the optional
/// leading / trailing pipes and unescaping `\|`. Thin wrapper over
/// [`split_row_cells`] so the cell text + count stay identical to the
/// offset-tracking path the caret targets use.
fn split_cells(line: &str) -> Vec<String> {
    split_row_cells(line).into_iter().map(|c| c.text).collect()
}

/// One cell of a split table row: its trimmed text plus the byte offsets *within
/// the row line* of its trimmed content's start and end. The `start..end` slice
/// of the source is the cell's editable region (`widget-table-cell-edit-inplace`)
/// — the bytes between the surrounding unescaped `|`, leading/trailing whitespace
/// trimmed, escapes preserved (so `\|` round-trips). `end` alone is the
/// click-to-edit caret target (`widget-table-cell-edit`).
#[derive(Clone)]
struct SplitCell {
    text: String,
    start: usize,
    end: usize,
}

/// Split a table row into its cells, returning each cell's trimmed text and the
/// byte offsets *within `line`* of its trimmed-content start and end — the
/// editable source range for an in-place cell edit, and (the end alone) the caret
/// target for a click on that cell (`widget-table-cell-edit`). Mirrors GFM: a
/// single optional leading and trailing pipe is dropped; `\|` is unescaped in the
/// text. The offsets are in source bytes — escapes precede any trailing
/// whitespace, so unescaping never shifts the end.
fn split_row_cells(line: &str) -> Vec<SplitCell> {
    let content = line.strip_suffix('\n').unwrap_or(line);
    // Raw segments split on unescaped '|', each keeping its byte range
    // [start, end) within `content`.
    let mut segs: Vec<(String, usize, usize)> = Vec::new();
    let mut text = String::new();
    let mut start = 0usize;
    let mut escaped = false;
    let mut idx = 0usize;
    for ch in content.chars() {
        let len = ch.len_utf8();
        if escaped {
            text.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            segs.push((std::mem::take(&mut text), start, idx));
            start = idx + len;
        } else {
            text.push(ch);
        }
        idx += len;
    }
    segs.push((std::mem::take(&mut text), start, idx));
    // Drop the empty cell a leading / trailing pipe creates.
    if segs.len() > 1 && segs[0].0.trim().is_empty() {
        segs.remove(0);
    }
    if segs.len() > 1 && segs.last().is_some_and(|s| s.0.trim().is_empty()) {
        segs.pop();
    }
    segs.into_iter()
        .map(|(raw, s, e)| {
            // Trim the cell's whitespace on the SOURCE slice (it includes any
            // backslashes, which sit before the trailing whitespace and inside the
            // leading whitespace, so trimming never lands mid-escape): the start
            // moves past the leading whitespace, the end before the trailing.
            let span = &content[s..e];
            let leading_ws = span.len() - span.trim_start().len();
            let trailing_ws = span.len() - span.trim_end().len();
            let end = e - trailing_ws;
            // A whitespace-only cell collapses to an empty range at its end.
            let start = (s + leading_ws).min(end);
            SplitCell { text: raw.trim().to_string(), start, end }
        })
        .collect()
}

/// Content hash over the table source + baked colors + font size. Stable across
/// frames so the decoration cache and the painter's `widget_id` agree, changing
/// only when the source, theme, or font size does (`widget-render-cache`).
fn hash_table(source: &str, colors: &TableColors, font_px: f32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    for c in [colors.text, colors.header_bg, colors.rule] {
        (c.r, c.g, c.b, c.a).hash(&mut h);
    }
    font_px.to_bits().hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests;
