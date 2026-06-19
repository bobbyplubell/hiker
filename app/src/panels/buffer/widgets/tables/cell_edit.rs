//! In-place table-cell-edit derivation (`widget-table-cell-edit-inplace`): the
//! per-cell editable byte-RANGE targets the overlay editor binds to, and the
//! block-vs-text cell classifier that gates the trigger / picks the menu label.
//!
//! Split out of the parent `tables.rs` (which paints the table) to keep that file
//! under the length budget: this is the cell-EDIT half (what the buffer panel's
//! `table_cell_edit` overlay consumes), distinct from the render / overflow code.
//! It reaches into the parent's private [`TableWidget`] parse + `table_cell_id`
//! (Rust privacy is module-scoped — a child sees the parent's privates), so the
//! ids it mints match the widget layer's `click_regions()` exactly.

use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::diagrams::{mermaid_span_in_str, wavedrom_span_in_str};
use editor_md::embeds::image_span_in_str;
use editor_md::equations::math_spans_in_str;
use editor_md::tables::table_spans;

use super::{table_cell_id, TableWidget};

/// One on-screen table cell's identity + editable source region for in-place
/// cell editing (`widget-table-cell-edit-inplace`): the cell's whole-widget
/// click id (`table_cell_id`), the enclosing table block's byte start (the
/// per-table edit-state key + the reveal-suppression key), and the cell's
/// absolute byte `range` in the document (its trimmed content between the
/// surrounding unescaped `|` — the bytes the overlay editor splices).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableCellTarget {
    pub cell_id: u64,
    pub table_start: usize,
    pub range: std::ops::Range<usize>,
}

/// Build this frame's in-place cell-edit targets: for every on-screen pipe table
/// (viewport-scoped), one [`TableCellTarget`] per cell, row-major, carrying the
/// cell's click id, its table's byte start, and the cell's absolute editable byte
/// range. The buffer panel resolves a right-click / double-click on a cell's
/// whole-widget click zone to its id, looks the target up here, and seeds the
/// overlay editor with `doc[range]`.
///
/// Raster-free (uses `TableWidget::from_source_meta`) and cache-independent, so
/// the ids match the widget layer's `click_regions()` even on cached frames.
/// `viewport` scopes the scan like the provider. status: widget-table-cell-edit-inplace
#[must_use]
pub fn table_cell_targets(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
) -> Vec<TableCellTarget> {
    let mut out: Vec<TableCellTarget> = Vec::new();
    let doc = state.doc.to_string();
    for span in table_spans(state, viewport) {
        let src = &doc[span.byte_range.clone()];
        let Some(widget) = TableWidget::from_source_meta(src, theme, &span.aligns, font_px) else {
            continue;
        };
        let base = span.byte_range.start;
        for (i, range) in widget.cell_ranges.iter().enumerate() {
            out.push(TableCellTarget {
                cell_id: table_cell_id(widget.content_hash, i),
                table_start: base,
                range: (base + range.start)..(base + range.end),
            });
        }
    }
    out
}

/// Whether a cell's source is exactly one renderable block — a math span, a
/// one-line mermaid / wavedrom fence, or an `![alt](path)` image. Detection only
/// (no render / vault resolve), so the in-place cell-edit trigger can choose the
/// "Edit diagram" (block) vs "Edit cell" (text) menu label and decide whether a
/// double-click should enter edit. Mirrors the parent `detect_block`'s
/// mutually-exclusive probe order. status: widget-table-cell-edit-inplace
#[must_use]
pub fn cell_is_block(src: &str) -> bool {
    let trimmed = src.trim();
    if trimmed.is_empty() {
        return false;
    }
    let math = matches!(
        math_spans_in_str(trimmed).as_slice(),
        [only] if only.byte_range == (0..trimmed.len())
    );
    math
        || mermaid_span_in_str(trimmed).is_some()
        || wavedrom_span_in_str(trimmed).is_some()
        || image_span_in_str(trimmed).is_some()
}
