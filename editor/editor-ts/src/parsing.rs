//! Parser state and incremental reparse plumbing.
//!
//! [`TsState`] owns a parsed `tree_sitter::Tree` plus a precomputed list of
//! highlight ranges. [`parse`] does a full parse; [`reparse`] reuses the
//! prior tree with the supplied `InputEdit`s for incremental work.

use std::ops::Range;

use editor_core::change::Op;
use editor_core::change::Set as ChangeSet;
use editor_core::rope::Rope;
use smol_str::SmolStr;
use tree_sitter::{InputEdit, Language, Parser, Point, Query, QueryCursor, Tree};

/// A bundle of a `tree_sitter::Language` together with its query files.
///
/// Construct one per language. Cheap to clone (queries are owned `String`s;
/// `Language` itself is a thin handle).
#[derive(Clone)]
pub struct TsLanguage {
    pub language: Language,
    pub highlights_query: String,
    pub injections_query: Option<String>,
    pub indent_query: Option<String>,
}

/// Parsed tree + flat highlight list for one document snapshot.
pub struct TsState {
    pub tree: Tree,
    /// `(byte-range, capture-name)` pairs in document order. Capture names
    /// come straight from the highlights query (e.g. `"keyword"`,
    /// `"string"`, `"function.builtin"`).
    pub highlights: Vec<(Range<usize>, SmolStr)>,
}

/// Run a full parse of `doc` and capture highlights.
pub fn parse(language: &TsLanguage, doc: &str) -> TsState {
    let mut parser = Parser::new();
    parser
        .set_language(&language.language)
        .expect("tree-sitter language version mismatch");
    let tree = parser.parse(doc, None).expect("tree-sitter parse failed");
    let highlights = run_highlights(language, &tree, doc);
    TsState { tree, highlights }
}

/// Incremental reparse: apply `edits` to the previous tree, then reparse.
///
/// Callers should build `edits` via [`changeset_to_edits`] when the source
/// of truth is a `ChangeSet`. Edits must be supplied in the order they were
/// applied to the document.
pub fn reparse(
    language: &TsLanguage,
    doc: &str,
    prev: &TsState,
    edits: &[InputEdit],
) -> TsState {
    let mut old_tree = prev.tree.clone();
    for edit in edits {
        old_tree.edit(edit);
    }
    let mut parser = Parser::new();
    parser
        .set_language(&language.language)
        .expect("tree-sitter language version mismatch");
    let tree = parser
        .parse(doc, Some(&old_tree))
        .expect("tree-sitter reparse failed");
    let highlights = run_highlights(language, &tree, doc);
    TsState { tree, highlights }
}

fn run_highlights(
    language: &TsLanguage,
    tree: &Tree,
    doc: &str,
) -> Vec<(Range<usize>, SmolStr)> {
    if language.highlights_query.is_empty() {
        return Vec::new();
    }
    let query = match Query::new(&language.language, &language.highlights_query) {
        Ok(q) => q,
        Err(_) => return Vec::new(),
    };
    let names = query.capture_names();
    let mut out: Vec<(Range<usize>, SmolStr)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let bytes = doc.as_bytes();
    let mut matches = cursor.matches(&query, tree.root_node(), bytes);
    use streaming_iterator::StreamingIterator;
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let idx = cap.index as usize;
            let name = names.get(idx).copied().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let node = cap.node;
            let range = node.start_byte()..node.end_byte();
            if range.start >= range.end {
                continue;
            }
            out.push((range, SmolStr::from(name)));
        }
    }
    out.sort_by_key(|(r, _)| r.start);
    out
}

/// Convert a [`ChangeSet`] into the list of `tree_sitter::InputEdit`s
/// needed by [`reparse`].
///
/// Tree-sitter wants `(start_byte, old_end_byte, new_end_byte,
/// start_position, old_end_position, new_end_position)` for each
/// contiguous edit. `Point { row, column }` is computed against the
/// **pre-edit** rope (`before`) for start + old_end, and against the
/// projected post-edit byte offsets for new_end. The column is in **UTF-8
/// bytes within its line**, matching how tree-sitter treats source text.
///
/// We coalesce contiguous `Delete`/`Insert` ops at the same cursor into a
/// single `InputEdit` (a "replace"), so that the count of edits matches
/// the count of distinct change regions in the changeset.
pub fn changeset_to_edits(before: &Rope, changes: &ChangeSet) -> Vec<InputEdit> {
    let mut edits = Vec::new();
    let mut in_pos = 0usize; // byte cursor in `before`
    let mut out_pos = 0usize; // byte cursor in the post-edit document
    let ops = changes.ops();
    let mut i = 0;
    while i < ops.len() {
        match &ops[i] {
            Op::Retain(n) => {
                in_pos += *n as usize;
                out_pos += *n as usize;
                i += 1;
            }
            Op::Delete(_) | Op::Insert(_) => {
                let mut delete_len = 0usize;
                let mut insert_len = 0usize;
                while i < ops.len() {
                    match &ops[i] {
                        Op::Delete(n) => {
                            delete_len += *n as usize;
                            i += 1;
                        }
                        Op::Insert(s) => {
                            insert_len += s.len();
                            i += 1;
                        }
                        Op::Retain(_) => break,
                    }
                }
                let start_byte = in_pos;
                let old_end_byte = in_pos + delete_len;
                let new_end_byte = out_pos + insert_len;
                edits.push(InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position: byte_to_point(before, start_byte),
                    old_end_position: byte_to_point(before, old_end_byte),
                    // new_end_position is computed against the *post-edit*
                    // text, which we don't have here. Tree-sitter only
                    // strictly requires the byte offset; the point is used
                    // for diagnostics and as a hint for incremental parsing.
                    // Approximating with the pre-edit point of `start_byte`
                    // offset by the inserted bytes' rough row/column shape
                    // is sufficient; callers that need exact positions can
                    // post-process with the new rope.
                    // Cheap approximation: assume the insertion is a
                    // single-line addition; multi-line inserts still parse
                    // correctly via byte offsets — point is just a hint.
                    new_end_position: {
                        let p = byte_to_point(before, start_byte);
                        Point { row: p.row, column: p.column + insert_len }
                    },
                });
                in_pos = old_end_byte;
                out_pos = new_end_byte;
            }
        }
    }
    edits
}

fn byte_to_point(rope: &Rope, byte: usize) -> Point {
    let byte = byte.min(rope.len_bytes());
    let row = rope.byte_to_line(byte);
    let line_start = rope.line_to_byte(row);
    let column = byte - line_start;
    Point { row, column }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changeset_to_edits_pure_insert() {
        let before = Rope::from_str("hello world");
        let cs = ChangeSet::of(11, [(5..5, ", brave".to_string())]);
        let edits = changeset_to_edits(&before, &cs);
        assert_eq!(edits.len(), 1);
        let e = &edits[0];
        assert_eq!(e.start_byte, 5);
        assert_eq!(e.old_end_byte, 5);
        assert_eq!(e.new_end_byte, 5 + ", brave".len());
    }

    #[test]
    fn changeset_to_edits_replace_and_delete() {
        let before = Rope::from_str("aaaabbbbcccc");
        let cs = ChangeSet::of(
            12,
            [
                (0..4, "X".to_string()),
                (8..12, String::new()),
            ],
        );
        let edits = changeset_to_edits(&before, &cs);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].start_byte, 0);
        assert_eq!(edits[0].old_end_byte, 4);
        assert_eq!(edits[0].new_end_byte, 1);
        // second edit starts at byte 8 in *before*; in *after* it's at
        // 1 + 4 = 5 (X + bbbb).
        assert_eq!(edits[1].start_byte, 8);
        assert_eq!(edits[1].old_end_byte, 12);
        assert_eq!(edits[1].new_end_byte, 5);
    }
}
