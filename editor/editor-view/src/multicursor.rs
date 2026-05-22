//! Multi-cursor commands: add cursor at click, add next occurrence of the
//! current selection, vertical column expansion.

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;
/// Add a cursor at `pos` to the existing selection.
pub fn add_cursor(state: &EditorState, pos: usize) -> Selection {
    let mut ranges: Vec<SelRange> = state.selection.ranges().to_vec();
    ranges.push(SelRange::point(pos));
    let main = ranges.len() - 1;
    Selection::from_ranges(ranges, main)
}

/// Add a cursor one line above/below the main range's head, preserving column.
pub fn add_vertical_cursor(state: &EditorState, down: bool) -> Selection {
    let main = state.selection.main();
    let head = main.head.offset();
    let line = state.doc.byte_to_line(head);
    let line_start = state.doc.line_to_byte(line);
    let col = head - line_start;

    let new_line_idx = if down {
        line + 1
    } else if line == 0 {
        return state.selection.clone();
    } else {
        line - 1
    };
    if new_line_idx >= state.doc.len_lines() {
        return state.selection.clone();
    }
    let new_line_start = state.doc.line_to_byte(new_line_idx);
    let new_line_len = state.doc.line_len_bytes(new_line_idx);
    let mut new_head = new_line_start + col.min(new_line_len);
    while new_head > new_line_start
        && new_head < state.doc.len_bytes()
        && (state.doc.byte_at(new_head) & 0b1100_0000) == 0b1000_0000
    {
        new_head -= 1;
    }

    let mut ranges: Vec<SelRange> = state.selection.ranges().to_vec();
    ranges.push(SelRange::point(new_head));
    let main_idx = ranges.len() - 1;
    Selection::from_ranges(ranges, main_idx)
}

/// Add the next occurrence of the main selection's text as a new cursor.
/// If the main selection is empty, selects the word at the cursor first.
pub fn add_next_occurrence(state: &EditorState) -> Selection {
    let main = state.selection.main();
    if main.is_empty() {
        // Promote to word selection.
        use unicode_segmentation::UnicodeSegmentation;
        let pos = main.head.offset();
        let line = state.doc.byte_to_line(pos);
        let line_start = state.doc.line_to_byte(line);
        let text = state.doc.line_str(line);
        let local = pos - line_start;
        for (i, w) in text.unicode_word_indices() {
            let end = i + w.len();
            if local >= i && local <= end {
                return Selection::from_range(SelRange::new(line_start + i, line_start + end));
            }
        }
        return state.selection.clone();
    }
    let needle = state.doc.slice(main.range()).to_string();
    if needle.is_empty() {
        return state.selection.clone();
    }
    let haystack = state.doc.to_string();
    // Find the next occurrence strictly after the current main range's end.
    let from = main.end();
    let next = haystack[from..]
        .find(&needle)
        .map(|i| from + i)
        .or_else(|| haystack.find(&needle));
    let Some(start) = next else {
        return state.selection.clone();
    };
    if state
        .selection
        .ranges()
        .iter()
        .any(|r| r.start() == start && r.end() == start + needle.len())
    {
        return state.selection.clone();
    }
    let mut ranges: Vec<SelRange> = state.selection.ranges().to_vec();
    ranges.push(SelRange::new(start, start + needle.len()));
    let main_idx = ranges.len() - 1;
    Selection::from_ranges(ranges, main_idx)
}

/// Find all occurrences of the main selection (if non-empty, short, and
/// word-shaped) within `viewport`. Used for VSCode-style "highlight matches
/// of selected text" — viewport-scoped to stay cheap.
pub fn selection_occurrences(
    state: &EditorState,
    viewport: std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    let main = state.selection.main();
    if main.is_empty() {
        return Vec::new();
    }
    let needle_range = main.range();
    let needle_len = needle_range.end - needle_range.start;
    if needle_len == 0 || needle_len > 200 {
        return Vec::new();
    }
    let needle = state.doc.slice(needle_range.clone()).to_string();
    if needle.chars().any(|c| c == '\n') {
        return Vec::new();
    }
    let start = viewport.start.min(state.doc.len_bytes());
    let end = viewport.end.min(state.doc.len_bytes());
    if start >= end {
        return Vec::new();
    }
    let haystack = state.doc.slice(start..end).to_string();
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = haystack[cursor..].find(&needle) {
        let abs_start = start + cursor + found;
        let abs_end = abs_start + needle_len;
        // Skip the user's own selection ranges.
        if !state
            .selection
            .ranges()
            .iter()
            .any(|r| r.start() == abs_start && r.end() == abs_end)
        {
            out.push(abs_start..abs_end);
        }
        cursor += found + needle_len.max(1);
    }
    out
}

