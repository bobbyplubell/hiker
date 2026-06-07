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
    BlockPaint, BlockWidget, Color, Decoration, Set as DecorationSet, TextAlign, WidgetClickRegion,
};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::tables::{ColumnAlign, table_spans};

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
/// Line height as a multiple of font size.
const LINE_H_RATIO: f32 = 1.35;

/// One parsed table: header cells, body rows, and per-column alignment. Built
/// from the raw pipe-and-dash source by [`parse_table`]. `header_ends` /
/// `row_ends` carry, per cell, the byte offset *within the table source* just
/// past that cell's content (its trimmed-content end) — the caret target for a
/// cell click (`widget-table-cell-edit`), parallel to `header` / `rows`.
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    aligns: Vec<ColumnAlign>,
    header_ends: Vec<usize>,
    row_ends: Vec<Vec<usize>>,
}

/// Theme-derived colors for the painted grid (`widget-render-theme-color`).
#[derive(Clone, Copy)]
struct TableColors {
    text: Color,
    header_bg: Color,
    rule: Color,
}

const fn table_colors(theme: Option<&Theme>) -> TableColors {
    match theme {
        Some(t) => TableColors {
            text: t.palette.fg,
            header_bg: t.markdown.code_bg,
            rule: with_alpha(t.palette.fg, 80),
        },
        None => TableColors {
            text: Color::rgb(40, 40, 40),
            header_bg: Color::rgba(170, 120, 220, 25),
            rule: Color::rgba(120, 120, 120, 120),
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
        let table = parse_table(source, aligns)?;
        let colors = table_colors(theme);
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
        let mut consider = |cells: &[String]| {
            for (i, cell) in cells.iter().enumerate().take(cols) {
                let longest_word = cell
                    .split_whitespace()
                    .map(str::chars)
                    .map(Iterator::count)
                    .max()
                    .unwrap_or(0);
                let nat = cell.chars().count() as f32 * char_w + pad;
                let flr = longest_word as f32 * char_w + pad;
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
        if nat_sum <= total_width || nat_sum <= 0.0 {
            return natural;
        }
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

    /// Greedy word-wrap `text` into the visual lines it occupies in a column of
    /// `width` logical pt at `font_px`. Whitespace is collapsed to single
    /// spaces; a word wider than the column keeps its own line (no mid-word
    /// break — `column_widths` sizes columns to the longest word). Always at
    /// least one line (a single empty line for empty input). This is the single
    /// source of truth for both the height measure ([`wrapped_line_count`]) and
    /// the painted runs ([`paint_list`]), so they never disagree.
    fn wrap_lines(text: &str, width: f32, font_px: f32) -> Vec<String> {
        let char_w = font_px * CHAR_W_RATIO;
        let avail = (width - CELL_PAD_X * 2.0).max(char_w);
        let cols = (avail / char_w).floor().max(1.0) as usize;
        let mut lines: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in text.split_whitespace() {
            let wlen = word.chars().count();
            if cur_len == 0 {
                cur.push_str(word);
                cur_len = wlen;
            } else if cur_len + 1 + wlen <= cols {
                cur.push(' ');
                cur.push_str(word);
                cur_len += 1 + wlen;
            } else {
                lines.push(std::mem::take(&mut cur));
                cur.push_str(word);
                cur_len = wlen;
            }
        }
        if lines.is_empty() || !cur.is_empty() {
            lines.push(cur);
        }
        lines
    }

    /// Number of visual lines `text` occupies in a column of `width` logical pt
    /// at `font_px`. Always at least one. Delegates to [`wrap_lines`] so the
    /// measured height matches the lines actually painted.
    fn wrapped_line_count(text: &str, width: f32, font_px: f32) -> usize {
        Self::wrap_lines(text, width, font_px).len()
    }

    /// Height (logical pt) of the row whose cells are `cells`, at `widths`.
    fn row_height(cells: &[String], widths: &[f32], font_px: f32) -> f32 {
        let line_h = font_px * LINE_H_RATIO;
        let max_lines = cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let w = widths.get(i).copied().unwrap_or(0.0);
                Self::wrapped_line_count(c, w, font_px)
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
                let (anchor_x, text_align) = match align {
                    ColumnAlign::Right => (x + col_w - CELL_PAD_X, TextAlign::Right),
                    ColumnAlign::Center => (x + col_w * 0.5, TextAlign::Center),
                    ColumnAlign::Left | ColumnAlign::None => (x + CELL_PAD_X, TextAlign::Left),
                };
                if !cell.is_empty() {
                    // Wrap to the column width and paint one run per visual
                    // line (same wrap as the height measure), stacking lines
                    // down the cell so long content never spills into its
                    // neighbor. status: widget-table-render
                    let top = y + CELL_PAD_Y + (line_h - font_size) * 0.5;
                    for (li, line) in Self::wrap_lines(cell, col_w, font_size).iter().enumerate() {
                        if line.is_empty() {
                            continue;
                        }
                        list.push(BlockPaint::Text {
                            x: anchor_x,
                            y: top + li as f32 * line_h,
                            text: line.as_str().into(),
                            color: self.colors.text,
                            font_scale: 1.0,
                            align: text_align,
                        });
                    }
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
fn parse_table(source: &str, aligns: &[ColumnAlign]) -> Option<Table> {
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
    // Split a raw line into trimmed cell texts + their source-relative
    // content-end offsets.
    let split_abs = |&(raw, line_start): &(&str, usize)| -> (Vec<String>, Vec<usize>) {
        let mut texts = Vec::new();
        let mut ends = Vec::new();
        for (text, end) in split_row_cells(raw) {
            texts.push(text);
            ends.push(line_start + end);
        }
        (texts, ends)
    };
    let (header, header_ends) = split_abs(&lines[0]);
    let mut rows = Vec::new();
    let mut row_ends = Vec::new();
    for line in &lines[2..] {
        let (texts, ends) = split_abs(line);
        rows.push(texts);
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
    use editor_core::selection::Selection;

    const FONT: f32 = 15.0;
    const DPR: f32 = 1.0;

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
        let t = parse_table(src, &[ColumnAlign::None, ColumnAlign::None]).expect("a table");
        assert_eq!(t.header, vec!["a", "b"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0], vec!["1", "2"]);
    }

    #[test]
    fn rejects_source_without_rule_row() {
        let src = "| a | b |\n| 1 | 2 |\n";
        assert!(parse_table(src, &[]).is_none(), "no delimiter row → not a table");
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
            list.iter().any(|p| matches!(p, BlockPaint::Text { .. })),
            "cell text runs present"
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
                BlockPaint::Text { align, .. } => Some(*align),
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
    fn long_cell_wraps_into_multiple_runs() {
        // status: widget-table-render — a cell longer than its (overflow-shrunk)
        // column paints as several stacked text runs, not one overflowing run,
        // and the measured height reserves room for every line.
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let src = format!("| h | k |\n|---|---|\n| {long} | x |\n");
        let w = TableWidget::from_source(&src, None, &[ColumnAlign::None, ColumnAlign::None], FONT)
            .expect("a table widget");
        let width = 200.0;
        let runs: Vec<(f32, f32)> = w
            .paint_list(FONT, width)
            .unwrap()
            .iter()
            .filter_map(|p| match p {
                BlockPaint::Text { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
            .collect();
        // The long body cell must occupy more than one line.
        let line_count = TableWidget::wrapped_line_count(
            long,
            w.column_widths(FONT, width)[0],
            FONT,
        );
        assert!(line_count > 1, "the long cell should wrap to multiple lines");
        // Distinct y positions among the runs prove lines are stacked, not piled.
        let distinct_ys = {
            let mut ys: Vec<f32> = runs.iter().map(|&(_, y)| y).collect();
            ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
            ys.dedup();
            ys.len()
        };
        assert!(distinct_ys >= line_count, "stacked runs at distinct y per wrapped line");
        // Every painted run sits within the measured table height.
        let total_h = w.measure(FONT, width);
        let line_h = FONT * LINE_H_RATIO;
        assert!(
            runs.iter().all(|&(_, y)| y + line_h <= total_h + 1e-3),
            "all wrapped runs fit inside the measured height",
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
            TableWidget::wrapped_line_count(word, widths[0], FONT),
            1,
            "the unbreakable label fits on one line in its protected column",
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
        assert_eq!(widget.table.header[0], "a|b", "pipe unescaped in text");
        let off = span.byte_range.start + widget.cell_ends[0];
        assert_eq!(&src[..off], "| a\\|b", "offset is just past the source `b`");
    }
}
