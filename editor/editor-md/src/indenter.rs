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

/// Number of spaces in one indentation step. Mirrors the tab-width the
/// editor-view Tab handler inserts (`indent_tab`), so list indent/outdent
/// nests by the same visual amount a plain Tab would.
const INDENT_WIDTH: usize = 4;

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
    // Only handle a single, empty (caret) selection range on a list line.
    let info = cursor_list_line(state)?;
    let cursor = state.selection.main().head.offset();

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

/// If the main cursor sits on a list-item line, build a transaction that
/// increases the list's nesting by one indentation step (inserting
/// [`INDENT_WIDTH`] spaces before the existing indentation). Returns `None`
/// for non-list lines so the caller falls back to plain Tab insertion.
pub fn markdown_indent_on_tab(state: &EditorState) -> Option<Transaction> {
    let info = cursor_list_line(state)?;
    let insertion = " ".repeat(INDENT_WIDTH);
    let at = info.line_start;
    let edits = vec![(at..at, insertion)];
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    // Leave the selection unset so it maps through the change — the cursor
    // shifts right by INDENT_WIDTH along with the line content.
    Some(Transaction::new(changes).with_edit_type(EditType::Indent))
}

/// If the main cursor sits on a list-item line that carries leading
/// indentation, build a transaction that decreases the nesting by one step
/// (removing up to [`INDENT_WIDTH`] leading spaces, or a single leading tab).
/// Returns `None` for non-list lines, or list lines with no leading
/// indentation (outdent at column 0 is a no-op).
pub fn markdown_outdent_on_shift_tab(state: &EditorState) -> Option<Transaction> {
    let info = cursor_list_line(state)?;
    let indent = info.indent.as_bytes();
    let remove = if indent.first() == Some(&b'\t') {
        1
    } else {
        let mut n = 0;
        while n < INDENT_WIDTH && n < indent.len() && indent[n] == b' ' {
            n += 1;
        }
        n
    };
    if remove == 0 {
        return None;
    }
    let start = info.line_start;
    let edits = vec![(start..start + remove, String::new())];
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Some(Transaction::new(changes).with_edit_type(EditType::Indent))
}

/// If the caret sits immediately after a list-item bullet and the pasted
/// text's first line opens with the same marker token, return the pasted text
/// with that leading `<marker> ` prefix stripped — so inserting it doesn't
/// double the bullet the buffer line already shows. Returns `None` (paste
/// verbatim) when the caret isn't right after a bullet, or the pasted text's
/// first line doesn't open with the same marker.
pub fn markdown_strip_bullet_on_paste(state: &EditorState, pasted: &str) -> Option<String> {
    let info = cursor_list_line(state)?;
    let cursor = state.selection.main().head.offset();

    // The caret must sit exactly at the content start — right after the
    // bullet's marker and its trailing space(s). Pasting mid-prose or before
    // the marker leaves the text untouched.
    let content_start = info.line_start + info.indent.len() + info.marker_with_space_len;
    if cursor != content_start {
        return None;
    }

    // Parse the pasted first line as a list line; only strip when it carries
    // the same marker token the buffer line uses (e.g. both `-`).
    let first_line = pasted.split_inclusive('\n').next().unwrap_or(pasted);
    let pasted_list = IndentScan.parse_list_line(first_line, 0)?;
    if pasted_list.marker != info.marker {
        return None;
    }

    // Strip the pasted line's own `<indent><marker> ` prefix so only the
    // content rides in after the buffer's existing bullet.
    let strip_to = pasted_list.indent.len() + pasted_list.marker_with_space_len;
    Some(pasted[strip_to..].to_string())
}

/// Resolve the main caret to the list line it sits on, or `None` when the
/// selection is non-trivial or the line is not a list item. Shared by the
/// Tab indent and Shift-Tab outdent paths.
fn cursor_list_line(state: &EditorState) -> Option<ListLine> {
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
    IndentScan.parse_list_line(&line_text, line_start)
}

/// `IndentProvider` impl that delegates to the markdown indent helpers.
#[derive(Debug, Default, Clone)]
pub struct MarkdownIndent;

impl IndentProvider for MarkdownIndent {
    fn on_enter(&self, state: &EditorState) -> Option<Transaction> {
        markdown_indent_on_enter(state)
    }

    fn on_tab(&self, state: &EditorState) -> Option<Transaction> {
        markdown_indent_on_tab(state)
    }

    fn on_shift_tab(&self, state: &EditorState) -> Option<Transaction> {
        markdown_outdent_on_shift_tab(state)
    }

    fn on_paste(&self, state: &EditorState, pasted: &str) -> Option<String> {
        markdown_strip_bullet_on_paste(state, pasted)
    }
}
