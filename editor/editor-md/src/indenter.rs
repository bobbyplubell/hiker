//! Markdown-aware indent handling for the Enter key.
//!
//! Implements SPEC §9.14 / IMPLEMENTATION §16.6.5: when the user presses
//! Enter inside a list item, continue the list on the next line (carrying
//! over leading whitespace and the marker). Pressing Enter on an empty
//! list item ("escape") removes the marker and inserts a blank line.

use editor_core::change::Set as ChangeSet;
use editor_core::transaction::EditType;

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;

use editor_core::transaction::Transaction;
use editor_view::viewport::IndentProvider;

/// Classification of a single line for list-continuation purposes.
struct ListLine {
    /// Byte offset of the line's start.
    line_start: usize,
    /// Bytes of indentation (spaces / tabs) preceding the marker.
    indent: String,
    /// The marker token (e.g. `-`, `*`, `+`, `1.`).
    marker: String,
    /// Width in bytes of the marker plus trailing spaces (so
    /// `line_start + indent.len() + marker_with_space_len` is the start of
    /// the content / cursor area).
    marker_with_space_len: usize,
    /// Total content length of the line excluding the trailing newline.
    line_content_len: usize,
}

/// Single-use wrapper so the list-line parser can be a `self` method
/// (avoids `clippy::single_call_fn` for a helper that's only called from
/// `markdown_indent_on_enter`).
struct IndentScan;

impl IndentScan {
fn parse_list_line(&self, line: &str, line_start: usize) -> Option<ListLine> {
    // Strip a single trailing newline for analysis.
    let stripped = line.strip_suffix('\n').unwrap_or(line);
    let stripped = stripped.strip_suffix('\r').unwrap_or(stripped);

    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let indent_end = i;

    // Bullet marker?
    let marker_bytes: Option<usize> = if i < bytes.len()
        && (bytes[i] == b'-' || bytes[i] == b'*' || bytes[i] == b'+')
    {
        Some(1)
    } else {
        // Ordered marker: one or more digits followed by `.`.
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i > start && i < bytes.len() && bytes[i] == b'.' {
            Some(i + 1 - start)
        } else {
            None
        }
    };

    let marker_len = marker_bytes?;
    let marker_end = indent_end + marker_len;
    if marker_end > bytes.len() {
        return None;
    }

    // The marker must be followed either by EOL or by a space/tab. If by EOL
    // (no trailing space), this is still considered an empty list item.
    let mut after = marker_end;
    let mut had_space = false;
    while after < bytes.len() && (bytes[after] == b' ' || bytes[after] == b'\t') {
        had_space = true;
        after += 1;
    }
    // If there's content after the marker but no separating space, this is
    // not a list item (e.g. `1.2` is not a list).
    if after < bytes.len() && !had_space {
        return None;
    }
    // A bare marker with no trailing space (e.g. `-` at EOL) is treated as
    // an empty list item.
    let marker_with_space_len = (after - indent_end).max(marker_len);

    Some(ListLine {
        line_start,
        indent: stripped[..indent_end].to_string(),
        marker: stripped[indent_end..marker_end].to_string(),
        marker_with_space_len,
        line_content_len: stripped.len(),
    })
}
}

/// If the main cursor sits on a list-item line, build a transaction that
/// either continues the list (Enter inside content) or escapes it (Enter on
/// an empty item). Returns `None` for non-list lines.
pub fn markdown_indent_on_enter(state: &EditorState) -> Option<Transaction> {
    // Only handle a single, empty (caret) selection range.
    if state.selection.ranges().len() != 1 {
        return None;
    }
    let main = state.selection.main();
    if !main.is_empty() {
        return None;
    }
    let cursor = main.head.offset();
    let line = state.doc.byte_to_line(cursor);
    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);
    let info = IndentScan.parse_list_line(&line_text, line_start)?;

    let content_start = info.line_start + info.indent.len() + info.marker_with_space_len;
    let line_end = info.line_start + info.line_content_len;

    // Empty list item: line is just indent + marker (+ trailing spaces).
    // Cursor anywhere on such a line escapes the list.
    let is_empty_item = content_start >= line_end;

    let doc_len = state.doc.len_bytes();
    if is_empty_item {
        // Delete from line_start to line_end (the marker + indent) and
        // insert a newline. The cursor lands at the start of the new blank.
        let edit_range = info.line_start..line_end;
        let edits = vec![(edit_range, "\n".to_string())];
        let changes = ChangeSet::of(doc_len, edits);
        let new_caret = info.line_start + 1;
        let sel = Selection::from_range(SelRange::new(new_caret, new_caret));
        return Some(
            Transaction::new(changes)
                .with_edit_type(EditType::Input)
                .with_selection(sel),
        );
    }

    // Continue: insert "\n<indent><marker> " at the cursor position. For
    // ordered markers we keep the same number (simpler v1 behavior); the
    // user can renumber manually.
    let mut insertion = String::with_capacity(2 + info.indent.len() + info.marker.len());
    insertion.push('\n');
    insertion.push_str(&info.indent);
    insertion.push_str(&info.marker);
    insertion.push(' ');

    let edits = vec![(cursor..cursor, insertion.clone())];
    let changes = ChangeSet::of(doc_len, edits);
    let new_caret = cursor + insertion.len();
    let sel = Selection::from_range(SelRange::new(new_caret, new_caret));
    Some(
        Transaction::new(changes)
            .with_edit_type(EditType::Input)
            .with_selection(sel),
    )
}

/// `IndentProvider` impl that delegates to [`markdown_indent_on_enter`].
#[derive(Debug, Default, Clone)]
pub struct MarkdownIndent;

impl IndentProvider for MarkdownIndent {
    fn on_enter(&self, state: &EditorState) -> Option<Transaction> {
        markdown_indent_on_enter(state)
    }
}
