//! Where and whether a rendered block widget is emitted. The reveal
//! predicates ([`line_active`], [`cursor_inside`], [`selection_overlaps`])
//! decide *whether*: a cursor or selection on/in a span suppresses the
//! in-place render so the source shows instead. [`emit_block_widget`] and
//! [`fit_block_height`] decide *where and how tall*: hide the span's source
//! lines, anchor the block widget above them, and scale its height to the
//! content column. Shared by the math / mermaid / wavedrom providers in the
//! parent module, the sibling [`chart`](super::chart) and
//! [`tables`](super::tables) providers, and the decoration-cache reveal
//! fingerprints in `panels::buffer::decorations`.

use std::sync::Arc;

use editor_core::decoration::{BlockSide, BlockWidget, Decoration, LineStyle};
use editor_core::state::Editor as EditorState;

use super::render::RenderedWidget;

/// Own-height (logical points) for a rasterized block widget, scaled to fit the
/// available content `width`: a diagram wider than the column is shrunk to fit
/// (and its height scaled proportionally), so the reserved row matches the
/// letterboxed paint with no excess vertical band. A diagram narrower than the
/// column keeps its natural size (not upscaled). Mirrors the aspect-preserving
/// fit `texture_cache::letterbox` applies at paint time. status: widget-wavedrom-render
pub(super) fn fit_block_height(rendered: &RenderedWidget, dpr: f32, width: f32) -> f32 {
    let natural_w = rendered.width as f32 / dpr;
    let natural_h = rendered.height as f32 / dpr;
    if width > 0.0 && natural_w > width {
        natural_h * (width / natural_w)
    } else {
        natural_h
    }
}

/// Hide every source line of the block span and anchor the block widget to
/// the gap above the span's first line, so the rendered output paints in
/// place of the hidden source (`widget-block-source-hide`). Shared by the
/// display-math and mermaid block paths (and the table provider in
/// [`tables`](super::tables)).
pub(super) fn emit_block_widget(
    state: &EditorState,
    byte_range: &std::ops::Range<usize>,
    widget: Arc<dyn BlockWidget>,
    total_lines: usize,
    line_byte_end: &dyn Fn(usize) -> usize,
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
) {
    let start_line = state.doc.byte_to_line(byte_range.start);
    let end_line = state.doc.byte_to_line(byte_range.end.saturating_sub(1));
    let first_line_start = state.doc.line_to_byte(start_line.min(total_lines.saturating_sub(1)));
    for l in start_line..=end_line {
        if l >= total_lines {
            break;
        }
        let s = state.doc.line_to_byte(l);
        let e = line_byte_end(l);
        entries.push((
            s..e,
            Decoration::Line(LineStyle { hide: true, ..LineStyle::default() }),
        ));
    }
    // The block widget anchors at the line of `range.start`; the painter adds
    // its height as the `Above` gap of that (now zero-height) first line, so it
    // renders exactly where the hidden source block was.
    entries.push((
        first_line_start..first_line_start + 1,
        Decoration::BlockWidget {
            side: BlockSide::Above,
            widget,
        },
    ));
}

/// True if the main cursor's line intersects the span's lines (inline reveal).
pub(in crate::panels::buffer) fn line_active(
    state: &EditorState,
    range: &std::ops::Range<usize>,
) -> bool {
    let doc_len = state.doc.len_bytes();
    let cursor = state.selection.main().head.offset();
    let cursor_line = state.doc.byte_to_line(cursor.min(doc_len));
    let start_line = state.doc.byte_to_line(range.start.min(doc_len));
    let end_line = state
        .doc
        .byte_to_line(range.end.saturating_sub(1).max(range.start).min(doc_len));
    cursor_line >= start_line && cursor_line <= end_line
}

/// True if the main cursor sits anywhere inside the span (delimiters
/// inclusive) — the display-math reveal predicate.
pub(in crate::panels::buffer) fn cursor_inside(
    state: &EditorState,
    range: &std::ops::Range<usize>,
) -> bool {
    let cursor = state.selection.main().head.offset();
    cursor >= range.start && cursor <= range.end
}

/// True if any selection range (multi-cursor included) overlaps the span,
/// mirroring `live-preview-selection-reveal-all`.
pub(in crate::panels::buffer) fn selection_overlaps(
    state: &EditorState,
    range: &std::ops::Range<usize>,
) -> bool {
    state.selection.ranges().iter().any(|r| {
        let (s, e) = (r.start(), r.end());
        if s == e {
            // Empty selection (a bare cursor): treat touching the span as
            // overlap so a click at either delimiter reveals.
            s >= range.start && s <= range.end
        } else {
            s < range.end && e > range.start
        }
    })
}
