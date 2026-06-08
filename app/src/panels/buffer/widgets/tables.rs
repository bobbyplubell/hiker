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
    BlockPaint, BlockWidget, Color, Decoration, Set as DecorationSet, StyledRun, TextAlign,
    WidgetClickRegion,
};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::tables::{ColumnAlign, table_spans};

use super::inline::{Colors as InlineColors, parse_runs};
use super::{cursor_inside, emit_block_widget, selection_overlaps};

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
    font_px: f32,
    _dpr: f32,
) -> DecorationSet {
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
        // hide the source lines and render the grid in place.
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if revealed {
            continue;
        }
        let src = &doc[span.byte_range.clone()];
        let Some(widget) = TableWidget::from_source(src, theme, &span.aligns, font_px) else {
            continue; // malformed → fall back to the tinted source
        };
        emit_block_widget(
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

/// Build this frame's table edit-target entries (`widget-table-cell-edit`): for
/// every on-screen pipe table (viewport-scoped), the whole-widget id (its render
/// `content_hash`) → the table block start, plus each cell's [`table_cell_id`] →
/// the byte offset at the END of that cell's content (caret ready to append).
/// The buffer panel merges these into the shared `WidgetEditTargets`, so a cell
/// click routes through the existing `place_caret_for_block_click` and lands the
/// caret in that cell — which triggers reveal so the source shows for editing.
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
        let Some(widget) = TableWidget::from_source(src, theme, &span.aligns, font_px) else {
            continue;
        };
        let base = span.byte_range.start;
        // Whole-widget fallback: a click off any cell still enters edit.
        out.push((widget.content_hash, base));
        for (i, &rel_end) in widget.cell_ends.iter().enumerate() {
            out.push((table_cell_id(widget.content_hash, i), base + rel_end));
        }
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

/// One table cell: its inline-markdown styled runs (markers stripped, ready for
/// [`BlockPaint::RichText`]). The egui-free width / wrap measure reads the runs
/// directly (per-run style multipliers: bold / monospace render wider), so the
/// reserve and the painted galley agree without a separately-cached string.
struct Cell {
    runs: Vec<StyledRun>,
}

impl Cell {
    fn parse(raw: &str, colors: InlineColors) -> Self {
        Self { runs: parse_runs(raw, colors) }
    }


    /// Style-aware natural (unwrapped) content width in logical pt, excluding
    /// cell padding. Sums each run's width with its style multiplier (bold /
    /// monospace render wider than the flat `CHAR_W_RATIO`), then applies
    /// [`WIDTH_SAFETY`] so the reserve sits slightly above the real galley and
    /// the painter doesn't wrap at the boundary.
    fn natural_content_width(&self, char_w: f32) -> f32 {
        let raw: f32 = self.runs.iter().map(|r| run_width(r, char_w)).sum();
        raw * WIDTH_SAFETY
    }

    /// Style-aware floor (longest unbreakable word) content width in logical pt,
    /// excluding padding. A word's style follows the run it lives in; a word
    /// spanning runs (no whitespace between them, e.g. a bold name touching a
    /// plain suffix) takes the max factor across the runs it overlaps, an
    /// over-reserve that's safe (the painter never needs more). Applies
    /// [`WIDTH_SAFETY`] like the natural measure.
    fn floor_content_width(&self, char_w: f32) -> f32 {
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

/// One parsed table: header cells, body rows, and per-column alignment. Built
/// from the raw pipe-and-dash source by [`parse_table`]. `header_ends` /
/// `row_ends` carry, per cell, the byte offset *within the table source* just
/// past that cell's content (its trimmed-content end) — the caret target for a
/// cell click (`widget-table-cell-edit`), parallel to `header` / `rows`.
struct Table {
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
    aligns: Vec<ColumnAlign>,
    header_ends: Vec<usize>,
    row_ends: Vec<Vec<usize>>,
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
        },
    }
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

/// A natively-painted pipe-table block widget. Full-width own-height block; its
/// height is derived from the wrapped cell content at the paint-time width.
pub struct TableWidget {
    table: Table,
    colors: TableColors,
    /// Content hash over (source, theme colors, font) — the diff / cache key.
    content_hash: u64,
    /// Per-cell caret target (row-major, matching [`TableWidget::cell_boxes`] /
    /// the paint order): byte offset *within the table source* at the end of the
    /// cell's content. A cell click lands the caret here. status: widget-table-cell-edit
    cell_ends: Vec<usize>,
}

impl TableWidget {
    /// Build a table widget from the raw block source, or `None` if the source
    /// isn't a well-formed pipe table (no rule row). A `None` keeps the tinted
    /// source visible (`widget-render-error-fallback`).
    pub fn from_source(
        source: &str,
        theme: Option<&Theme>,
        aligns: &[ColumnAlign],
        font_px: f32,
    ) -> Option<Self> {
        let colors = table_colors(theme);
        let table = parse_table(source, aligns, colors.inline)?;
        let content_hash = hash_table(source, &colors, font_px);
        // Flatten the per-cell content-end offsets in the same row-major order
        // `cell_boxes` / `paint_list` iterate (header first, then each row).
        let mut cell_ends = Vec::with_capacity(table.header_ends.len());
        cell_ends.extend_from_slice(&table.header_ends);
        for r in &table.row_ends {
            cell_ends.extend_from_slice(r);
        }
        Some(Self { table, colors, content_hash, cell_ends })
    }

    /// Per-cell layout boxes in logical points (row-major: header, then each
    /// body row), plus the table's total height. The box geometry is derived
    /// purely from [`column_widths`](Self::column_widths) + [`row_height`](Self::row_height),
    /// so it stays in lockstep with `paint_list` and with `cell_ends`. Used by
    /// `click_regions` to place per-cell hit zones. status: widget-table-cell-edit
    fn cell_boxes(&self, font_size: f32, width: f32) -> (Vec<(f32, f32, f32, f32)>, f32) {
        let widths = self.column_widths(font_size, width);
        let mut boxes = Vec::with_capacity(self.cell_ends.len());
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

    /// Column widths (logical pt) at `total_width`. Each column carries two
    /// measures: its *natural* width (longest full line) and its *floor* (the
    /// longest single word — unbreakable, since cells wrap on word boundaries).
    /// When the table fits, columns get their natural width; when it overflows,
    /// only the flexible part (natural − floor) is shrunk to fit, so no column
    /// ever drops below its longest word and that word can't spill into the
    /// next column. If even the floors don't fit, every column keeps its floor
    /// and the table extends past the content box rather than overlapping.
    fn column_widths(&self, font_px: f32, total_width: f32) -> Vec<f32> {
        let cols = self.col_count();
        let char_w = font_px * CHAR_W_RATIO;
        let pad = CELL_PAD_X * 2.0;
        let mut natural = vec![0.0_f32; cols];
        let mut floor = vec![0.0_f32; cols];
        let mut consider = |cells: &[Cell]| {
            for (i, cell) in cells.iter().enumerate().take(cols) {
                let nat = cell.natural_content_width(char_w) + pad;
                let flr = cell.floor_content_width(char_w) + pad;
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
        let nat_sum: f32 = natural.iter().sum();
        if nat_sum <= 0.0 {
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
        let max_lines = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = widths.get(i).copied().unwrap_or(0.0);
                Self::cell_line_count(c, w, font_px)
            })
            .max()
            .unwrap_or(1);
        max_lines as f32 * line_h + CELL_PAD_Y * 2.0
    }
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
        // fills 1:1), y/h against the table's total height. status: widget-table-cell-edit
        if width <= 0.0 {
            return Vec::new();
        }
        let (boxes, total_h) = self.cell_boxes(font_size, width);
        if total_h <= 0.0 {
            return Vec::new();
        }
        boxes
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| WidgetClickRegion {
                x: x / width,
                y: y / total_h,
                w: w / width,
                h: h / total_h,
                id: table_cell_id(self.content_hash, i),
            })
            .collect()
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
                let align = self
                    .table
                    .aligns
                    .get(i)
                    .copied()
                    .unwrap_or(ColumnAlign::None);
                let text_align = match align {
                    ColumnAlign::Right => TextAlign::Right,
                    ColumnAlign::Center => TextAlign::Center,
                    ColumnAlign::Left | ColumnAlign::None => TextAlign::Left,
                };
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
fn parse_table(source: &str, aligns: &[ColumnAlign], inline: InlineColors) -> Option<Table> {
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
    // source-relative content-end offsets.
    let split_abs = |&(raw, line_start): &(&str, usize)| -> (Vec<Cell>, Vec<usize>) {
        let mut cells = Vec::new();
        let mut ends = Vec::new();
        for (text, end) in split_row_cells(raw) {
            cells.push(Cell::parse(&text, inline));
            ends.push(line_start + end);
        }
        (cells, ends)
    };
    let (header, header_ends) = split_abs(&lines[0]);
    let mut rows = Vec::new();
    let mut row_ends = Vec::new();
    for line in &lines[2..] {
        let (cells, ends) = split_abs(line);
        rows.push(cells);
        row_ends.push(ends);
    }
    Some(Table { header, rows, aligns: aligns.to_vec(), header_ends, row_ends })
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
    split_row_cells(line).into_iter().map(|(text, _)| text).collect()
}

/// Split a table row into its cells, returning each cell's trimmed text and the
/// byte offset *within `line`* just past the cell's last content character (its
/// trimmed-content end) — the caret target for a click on that cell
/// (`widget-table-cell-edit`). Mirrors GFM: a single optional leading and
/// trailing pipe is dropped; `\|` is unescaped in the text. The end offset is in
/// source bytes — escapes precede any trailing whitespace, so unescaping never
/// shifts it.
fn split_row_cells(line: &str) -> Vec<(String, usize)> {
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
            // End offset = segment end minus its trailing whitespace (computed on
            // the source slice, which includes any backslashes — those sit before
            // the trailing whitespace, so they don't move the end).
            let span = &content[s..e];
            let trailing_ws = span.len() - span.trim_end().len();
            (raw.trim().to_string(), e - trailing_ws)
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
mod tests {
    use super::*;
    use super::super::inline::runs_text;
    use editor_core::selection::Selection;

    const FONT: f32 = 15.0;
    const DPR: f32 = 1.0;
    const INLINE: InlineColors = InlineColors {
        text: Color::rgb(40, 40, 40),
        link: Color::rgb(0, 90, 200),
        code_bg: Color::rgba(120, 120, 120, 30),
    };

    /// The visible text of each cell (markers stripped) for a row of [`Cell`]s.
    fn cell_texts(cells: &[Cell]) -> Vec<String> {
        cells.iter().map(|c| runs_text(&c.runs)).collect()
    }

    /// A plain (unstyled) cell from `text` — a single plain run — for the
    /// style-free wrap / height assertions.
    fn plain_cell(text: &str) -> Cell {
        Cell { runs: vec![StyledRun::plain(text, INLINE.text)] }
    }

    /// (block widgets, hide lines) for the table provider, mirroring the
    /// mermaid provider's `mermaid_counts`.
    fn table_counts(state: &EditorState) -> (usize, usize) {
        let set = table_widget_decorations(state, None, None, FONT, DPR);
        let mut block = 0;
        let mut hides = 0;
        for (_, d) in set.iter_all() {
            match d {
                Decoration::BlockWidget { .. } => block += 1,
                Decoration::Line(s) if s.hide => hides += 1,
                _ => {}
            }
        }
        (block, hides)
    }

    #[test]
    fn provider_emits_block_when_cursor_elsewhere() {
        // status: widget-table-render — a well-formed pipe table away from the
        // cursor hides its source lines and renders a block widget.
        let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
        let state = EditorState::new(src);
        let (block, hides) = table_counts(&state);
        assert_eq!(block, 1, "one table block widget when cursor is away");
        assert_eq!(hides, 3, "all three source rows hidden");
    }

    #[test]
    fn provider_reveals_source_when_cursor_inside() {
        // status: widget-table-render / widget-reveal-block — cursor inside the
        // table reveals the raw source (no hides, no block).
        let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
        let mut state = EditorState::new(src);
        state.selection = Selection::single(src.find("| 1 ").unwrap());
        let (block, hides) = table_counts(&state);
        assert_eq!(block, 0, "revealed table emits no in-place block");
        assert_eq!(hides, 0, "and hides nothing, so the source stays visible");
    }

    #[test]
    fn provider_emits_nothing_for_malformed_table() {
        // status: widget-render-error-fallback — no `|---|` rule row → not a
        // GFM table → no detection → no widget, tinted source remains.
        let src = "intro\n\n| a | b |\n| 1 | 2 |\n\nmore\n";
        let state = EditorState::new(src);
        let (block, hides) = table_counts(&state);
        assert_eq!(block, 0, "a malformed table emits no widget");
        assert_eq!(hides, 0, "and hides nothing");
    }

    #[test]
    fn parses_header_and_rows() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let t = parse_table(src, &[ColumnAlign::None, ColumnAlign::None], INLINE).expect("a table");
        assert_eq!(cell_texts(&t.header), vec!["a", "b"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(cell_texts(&t.rows[0]), vec!["1", "2"]);
    }

    #[test]
    fn rejects_source_without_rule_row() {
        let src = "| a | b |\n| 1 | 2 |\n";
        assert!(parse_table(src, &[], INLINE).is_none(), "no delimiter row → not a table");
    }

    #[test]
    fn cell_carries_inline_styled_runs() {
        // status: widget-table-render — a `**bold**` cell parses to a styled run
        // (markers stripped), not a literal `**bold**` string.
        let src = "| h |\n|---|\n| **bold** and `code` |\n";
        let t = parse_table(src, &[ColumnAlign::None], INLINE).expect("a table");
        let cell = &t.rows[0][0];
        assert_eq!(runs_text(&cell.runs), "bold and code", "visible text has markers stripped");
        assert!(cell.runs.iter().any(|r| r.text == "bold" && r.bold), "{:?}", cell.runs);
        assert!(cell.runs.iter().any(|r| r.text == "code" && r.code), "{:?}", cell.runs);
    }

    #[test]
    fn paint_emits_rich_text_for_styled_cell() {
        // status: widget-table-render — a styled cell paints as a RichText block
        // (the wrapping multi-format galley), not flat per-line Text runs.
        let src = "| a |\n|---|\n| *italic* x |\n";
        let w = TableWidget::from_source(src, None, &[ColumnAlign::None], FONT).expect("a widget");
        let list = w.paint_list(FONT, 400.0).unwrap();
        let rich: Vec<&Vec<StyledRun>> = list
            .iter()
            .filter_map(|p| match p {
                BlockPaint::RichText { runs, .. } => Some(runs),
                _ => None,
            })
            .collect();
        assert!(!rich.is_empty(), "styled cells render as RichText");
        assert!(
            rich.iter().any(|runs| runs.iter().any(|r| r.italic)),
            "the italic run survives into the paint list",
        );
        // No literal markdown markers leak into any painted run.
        assert!(
            rich.iter().all(|runs| runs.iter().all(|r| !r.text.contains('*'))),
            "markers stripped in paint",
        );
    }

    #[test]
    fn widget_measures_positive_height_and_paints() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let w = TableWidget::from_source(src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .expect("a table widget");
        let h = w.measure(FONT, 400.0);
        assert!(h > 0.0, "non-empty table has positive height");
        let list = w.paint_list(FONT, 400.0).expect("a paint list");
        // Header bg rect + at least the text runs + rules.
        assert!(
            list.iter().any(|p| matches!(p, BlockPaint::Rect { .. })),
            "header background rect present"
        );
        assert!(
            list.iter().any(|p| matches!(p, BlockPaint::RichText { .. })),
            "cell rich-text runs present"
        );
        assert!(
            list.iter().any(|p| matches!(p, BlockPaint::Line { .. })),
            "grid rules present"
        );
    }

    #[test]
    fn alignment_drives_text_anchor() {
        let src = "| l | r |\n|:--|--:|\n| 1 | 2 |\n";
        let w = TableWidget::from_source(src, None, &[ColumnAlign::Left, ColumnAlign::Right], FONT)
            .expect("a table widget");
        let list = w.paint_list(FONT, 400.0).unwrap();
        let aligns: Vec<TextAlign> = list
            .iter()
            .filter_map(|p| match p {
                BlockPaint::RichText { align, .. } => Some(*align),
                _ => None,
            })
            .collect();
        assert!(aligns.contains(&TextAlign::Left), "left column left-aligned");
        assert!(aligns.contains(&TextAlign::Right), "right column right-aligned");
    }

    #[test]
    fn content_hash_changes_with_source() {
        let a = TableWidget::from_source("| a |\n|---|\n| 1 |\n", None, &[ColumnAlign::None], FONT)
            .unwrap();
        let b = TableWidget::from_source("| a |\n|---|\n| 2 |\n", None, &[ColumnAlign::None], FONT)
            .unwrap();
        assert_ne!(a.widget_id(), b.widget_id(), "different bodies hash apart");
    }

    #[test]
    fn long_cell_reserves_height_for_wrapped_lines() {
        // status: widget-table-render — a cell longer than its (overflow-shrunk)
        // column wraps; the painter owns the real wrap (one RichText per cell),
        // so the egui-free height measure must reserve room for the multi-line
        // wrap it estimates, and bound the cell's max_width to its column.
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let src = format!("| h | k |\n|---|---|\n| {long} | x |\n");
        let w = TableWidget::from_source(&src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .expect("a table widget");
        let width = 200.0;
        let col0 = w.column_widths(FONT, width)[0];
        let line_count = TableWidget::cell_line_count(&plain_cell(long), col0, FONT);
        assert!(line_count > 1, "the long cell should wrap to multiple lines");

        // The body row's reserved height covers every estimated wrapped line.
        let header_h = TableWidget::row_height(&w.table.header, &w.column_widths(FONT, width), FONT);
        let total_h = w.measure(FONT, width);
        let line_h = FONT * LINE_H_RATIO;
        assert!(
            total_h - header_h >= line_count as f32 * line_h,
            "body row height ({}) reserves room for {line_count} wrapped lines",
            total_h - header_h,
        );

        // The long cell paints as a single RichText bounded to its column width.
        let max_widths: Vec<f32> = w
            .paint_list(FONT, width)
            .unwrap()
            .iter()
            .filter_map(|p| match p {
                BlockPaint::RichText { max_width, .. } => Some(*max_width),
                _ => None,
            })
            .collect();
        assert!(
            max_widths.iter().any(|&mw| mw <= col0 + 1e-3),
            "a cell's rich-text max_width is bounded by its column ({col0})",
        );
    }

    #[test]
    fn narrow_column_never_shrinks_below_its_longest_word() {
        // status: widget-table-render — under overflow shrink, a column whose
        // content is a single unbreakable word keeps at least that word's
        // width, so the word can't spill into the next column.
        let word = "Postprocess";
        // A wide second column forces overflow shrinking at a modest width.
        let src = format!(
            "| label | detail |\n|---|---|\n| {word} | this is a deliberately long second column to force the table to overflow and shrink |\n"
        );
        let w = TableWidget::from_source(&src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .expect("a table widget");
        let widths = w.column_widths(FONT, 240.0);
        let char_w = FONT * CHAR_W_RATIO;
        let word_floor = word.chars().count() as f32 * char_w + CELL_PAD_X * 2.0;
        assert!(
            widths[0] >= word_floor - 1e-3,
            "label column ({}) stays >= its longest word's width ({word_floor})",
            widths[0],
        );
        // And that single-word cell therefore renders on one line.
        assert_eq!(
            TableWidget::cell_line_count(&plain_cell(word), widths[0], FONT),
            1,
            "the unbreakable label fits on one line in its protected column",
        );
    }

    #[test]
    fn fitting_table_stretches_to_full_width() {
        // status: widget-table-render (Fix 1) — when the natural content fits, the
        // columns absorb the slack so the table fills the available width exactly,
        // instead of leaving empty space to the right.
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let w = TableWidget::from_source(src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .expect("a table widget");
        let total = 900.0;
        let widths = w.column_widths(FONT, total);
        let sum: f32 = widths.iter().sum();
        assert!((sum - total).abs() < 1e-2, "columns fill the full width: sum {sum} != {total}");
        // Slack is distributed proportionally, so a tiny two-column table splits
        // the stretch roughly evenly (both columns hold the same content).
        assert!((widths[0] - widths[1]).abs() < 1.0, "equal columns stretch equally");
    }

    #[test]
    fn bold_cell_reserves_more_width_than_plain() {
        // status: widget-table-render (Fix 2) — a bold cell measures wider than the
        // same text plain, so its column reserves enough that the real (faux-bold)
        // galley fits on one line instead of wrapping its last glyph.
        let char_w = FONT * CHAR_W_RATIO;
        let bold = Cell::parse("**WaveDrom**", INLINE);
        let plain = Cell::parse("WaveDrom", INLINE);
        assert_eq!(runs_text(&bold.runs), "WaveDrom", "markers stripped");
        assert!(
            bold.natural_content_width(char_w) > plain.natural_content_width(char_w),
            "bold ({}) reserves more than plain ({})",
            bold.natural_content_width(char_w),
            plain.natural_content_width(char_w),
        );
        // And a `code` (monospace) run reserves more still than bold prose.
        let codey = Cell::parse("`Vec<T>`", INLINE);
        let prose = Cell::parse("Vec<T>", INLINE);
        assert!(
            codey.natural_content_width(char_w) > prose.natural_content_width(char_w),
            "monospace code reserves more than plain proportional text",
        );
    }

    #[test]
    fn bold_header_stays_on_one_line_at_full_width() {
        // status: widget-table-render (Fix 1 + 2) — the real regression: a bold
        // "WaveDrom" header in a 3-column table, stretched to a wide page, must get
        // a column wide enough that the style-aware measure keeps it on one line.
        let src = "| WaveDrom | You write | Renders as |\n|---|---|---|\n\
                   | **WaveDrom** | `wavedrom` | a timing diagram |\n";
        let w = TableWidget::from_source(
            src,
            None,
            &[ColumnAlign::None, ColumnAlign::None, ColumnAlign::None],
            FONT,
        )
        .expect("a table widget");
        let widths = w.column_widths(FONT, 1000.0);
        let bold = Cell::parse("**WaveDrom**", INLINE);
        assert_eq!(
            TableWidget::cell_line_count(&bold, widths[0], FONT),
            1,
            "the bold WaveDrom header fits on one line in its widened column",
        );
    }

    #[test]
    fn cell_click_targets_land_at_end_of_cell_content() {
        // status: widget-table-cell-edit — each cell's edit target is the byte
        // offset just past its content (caret ready to append); cells are
        // row-major (header a, bb; then body 1, 22).
        let src = "| a | bb |\n|---|---|\n| 1 | 22 |\n";
        let state = EditorState::new(src);
        let span = table_spans(&state, None).into_iter().next().expect("a table span");
        let widget =
            TableWidget::from_source(&src[span.byte_range.clone()], None, &span.aligns, FONT)
                .expect("a table widget");
        let hash = widget.content_hash;

        let map: std::collections::HashMap<u64, usize> =
            table_edit_targets(&state, None, None, FONT).into_iter().collect();

        for (i, cell) in ["a", "bb", "1", "22"].iter().enumerate() {
            let id = table_cell_id(hash, i);
            let off = *map.get(&id).unwrap_or_else(|| panic!("cell {cell} target present"));
            assert!(
                src[..off].ends_with(cell),
                "cell {cell} target (offset {off}) lands at end of its content; got …{:?}",
                &src[off.saturating_sub(3)..off],
            );
        }
        // Whole-widget fallback → the table block start.
        assert_eq!(map.get(&hash), Some(&span.byte_range.start), "whole-widget → table start");
    }

    #[test]
    fn click_regions_one_per_cell_and_tagged() {
        // status: widget-table-cell-edit — one normalized region per cell, ids
        // tagged TABLE_CELL_TAG with the mermaid (61) / wikilink (62) bits clear,
        // matching `table_cell_id` per index.
        let src = "| a | bb |\n|---|---|\n| 1 | 22 |\n";
        let w = TableWidget::from_source(src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .unwrap();
        let regions = w.click_regions(FONT, 400.0);
        assert_eq!(regions.len(), 4, "2 header + 2 body cells");
        for (i, r) in regions.iter().enumerate() {
            assert_eq!(r.id, table_cell_id(w.content_hash, i), "id matches index");
            assert_ne!(r.id & TABLE_CELL_TAG, 0, "table-cell tag set");
            assert_eq!(r.id & editor_md::links::WIKILINK_WIDGET_TAG, 0, "wikilink bit clear");
            assert_eq!(r.id & (1 << 61), 0, "mermaid bit clear");
            assert!(r.x >= 0.0 && r.x + r.w <= 1.0 + 1e-3, "x normalized");
            assert!(r.y >= 0.0 && r.y + r.h <= 1.0 + 1e-3, "y normalized");
        }
    }

    #[test]
    fn cell_offsets_survive_escaped_pipe() {
        // status: widget-table-cell-edit — `\|` inside a cell is unescaped in the
        // text but the caret offset stays in source bytes (the backslash sits
        // before the trailing whitespace, so it doesn't shift the end).
        let src = "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n";
        let state = EditorState::new(src);
        let span = table_spans(&state, None).into_iter().next().expect("a table span");
        let widget =
            TableWidget::from_source(&src[span.byte_range.clone()], None, &span.aligns, FONT)
                .expect("a table widget");
        assert_eq!(runs_text(&widget.table.header[0].runs), "a|b", "pipe unescaped in text");
        let off = span.byte_range.start + widget.cell_ends[0];
        assert_eq!(&src[..off], "| a\\|b", "offset is just past the source `b`");
    }
}
