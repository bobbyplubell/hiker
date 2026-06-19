//! Unit tests for in-place table cell editing (`widget-table-cell-edit-inplace`):
//! the cell-edit trigger gating (block vs text), the cell source-RANGE derivation
//! that the overlay binds to, and the Tab / Shift+Tab cell-nav index math.
//!
//! Split out of `table_cell_edit.rs` to keep that file focused; the parent
//! includes it via `#[cfg(test)] mod tests;`.

use super::next_cell_index;
use crate::panels::buffer::widgets::tables;
use crate::panels::buffer::widgets::tables::cell_edit::{cell_is_block, table_cell_targets};
use editor_core::state::Editor as EditorState;

/// A block cell (math / mermaid / wavedrom / image) is detected as a block (→
/// "Edit diagram" + the double-click fast path); a text cell is not (→ "Edit
/// cell", menu-only). status: widget-table-cell-edit-inplace
#[test]
fn block_vs_text_cell_classification() {
    assert!(cell_is_block("$x^2$"), "inline math is a block cell");
    assert!(cell_is_block("$$ \\frac{a}{b} $$"), "display math is a block cell");
    assert!(cell_is_block("```mermaid graph TD; A-->B```"), "one-line mermaid fence");
    assert!(cell_is_block("![alt](img.png)"), "an image is a block cell");
    assert!(!cell_is_block("plain text"), "plain prose is a text cell");
    assert!(!cell_is_block("**bold** and `code`"), "inline markdown is still a text cell");
    assert!(!cell_is_block("  "), "a blank cell is a text cell");
    assert!(!cell_is_block("see $x$ here"), "math NOT filling the cell stays text");
}

/// `table_cell_targets` reports each cell's editable byte RANGE — the bytes the
/// overlay splices — as the trimmed content between the surrounding unescaped
/// `|`. status: widget-table-cell-edit-inplace
#[test]
fn cell_targets_carry_editable_range() {
    let src = "| a | bb |\n|---|---|\n| 1 | 222 |\n";
    let state = EditorState::new(src);
    let targets = table_cell_targets(&state, None, None, 15.0);
    // Row-major: header a, bb; then body 1, 222.
    let texts: Vec<&str> = targets.iter().map(|t| &src[t.range.clone()]).collect();
    assert_eq!(texts, vec!["a", "bb", "1", "222"], "ranges slice the trimmed cell content");
    // Every target carries the same table start (one table).
    assert!(targets.iter().all(|t| t.table_start == 0), "all cells share the table start");
}

/// The editable range is escape-correct: `\|` inside a cell stays in the range
/// (so it round-trips), and the range trims surrounding whitespace but never
/// lands mid-escape. status: widget-table-cell-edit-inplace
#[test]
fn cell_range_survives_escaped_pipe() {
    let src = "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n";
    let state = EditorState::new(src);
    let targets = table_cell_targets(&state, None, None, 15.0);
    let first = &targets[0];
    assert_eq!(&src[first.range.clone()], "a\\|b", "the escaped pipe stays inside the range");
}

/// An empty cell yields an empty (start == end) editable range parked at its
/// content position, so a fresh edit inserts there cleanly.
/// status: widget-table-cell-edit-inplace
#[test]
fn empty_cell_range_is_collapsed() {
    let src = "| a |  |\n|---|---|\n| 1 | 2 |\n";
    let state = EditorState::new(src);
    let targets = table_cell_targets(&state, None, None, 15.0);
    // Header cell 1 is empty.
    let empty = &targets[1];
    assert_eq!(empty.range.start, empty.range.end, "empty cell → collapsed range");
    assert_eq!(&src[empty.range.clone()], "", "and slices to nothing");
}

/// Tab / Shift+Tab nav walks the table's cells in document order and stops (no
/// wrap) at either end. status: widget-table-cell-edit-inplace
#[test]
fn cell_nav_index_steps_and_stops_at_ends() {
    // Forward from 0 in a 4-cell table.
    assert_eq!(next_cell_index(0, 4, 1), Some(1));
    assert_eq!(next_cell_index(2, 4, 1), Some(3));
    assert_eq!(next_cell_index(3, 4, 1), None, "Tab off the last cell → exit");
    // Backward.
    assert_eq!(next_cell_index(3, 4, -1), Some(2));
    assert_eq!(next_cell_index(0, 4, -1), None, "Shift+Tab off the first cell → exit");
}

/// The splice replaces EXACTLY the resolved cell range with the new text — the
/// same `Set::of` primitive `splice` applies to the buffer, proving the right
/// cell's bytes change and the surrounding `|` structure is preserved.
/// status: widget-table-cell-edit-inplace
#[test]
fn splice_replaces_only_the_cell_range() {
    let src = "| a | bb |\n|---|---|\n| 1 | 222 |\n";
    let state = EditorState::new(src);
    let targets = table_cell_targets(&state, None, None, 15.0);
    // Edit the body cell "222" → "X".
    let cell = targets.iter().find(|t| &src[t.range.clone()] == "222").expect("the 222 cell");
    let set = editor_core::change::Set::of(
        src.len(),
        std::iter::once((cell.range.clone(), "X".to_string())),
    );
    let tx = editor_core::transaction::Transaction::new(set);
    let after = state.apply(tx).doc.to_string();
    assert_eq!(after, "| a | bb |\n|---|---|\n| 1 | X |\n", "only that cell changed, pipes intact");
}

/// A non-editing pass leaves the document byte-identical: `table_cell_targets`
/// (and the provider it shares the parse with) are read-only. The regression
/// guard that the feature never mutates source as a side effect of rendering.
/// status: widget-table-cell-edit-inplace
#[test]
fn non_editing_table_is_byte_identical() {
    let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\ntail\n";
    let state = EditorState::new(src);
    let _ = table_cell_targets(&state, None, None, 15.0);
    let _ = cell_is_block("| a | b |");
    assert_eq!(state.doc.to_string(), src, "deriving targets never mutates the doc");
}

/// Helper sanity: `table_cell_targets` ids match the widget layer's
/// `click_regions` ids so the overlay's cell-rect hit-test resolves.
/// status: widget-table-cell-edit-inplace
#[test]
fn cell_target_ids_match_edit_targets() {
    let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let state = EditorState::new(src);
    let targets = table_cell_targets(&state, None, None, 15.0);
    let edit: std::collections::HashMap<u64, usize> =
        tables::table_edit_targets(&state, None, None, 15.0).into_iter().collect();
    for t in &targets {
        // The click id is present in the shared edit-target map (its caret target
        // is the cell's content end).
        assert_eq!(edit.get(&t.cell_id), Some(&t.range.end), "cell id → content-end caret");
    }
}
