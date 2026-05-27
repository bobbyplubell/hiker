//! Auto-pair brackets / quotes (SPEC §9.8, IMPLEMENTATION §16.5.4).
//!
//! When the user types a configured opener at an empty cursor, this transform
//! inserts the matching closer and positions the cursor between the pair.
//! Multi-cursor friendly: all (empty) selection ranges get paired. Auto-close
//! is suppressed when non-whitespace text sits immediately to the right of the
//! cursor (`has_text_to_right`), so wrapping/typing before existing text never
//! injects a stray closer. Typing the closer right before an auto-inserted one
//! types over it instead of doubling (`autopair_skip`).
//!
//! Per-language config is deferred (see SPEC §9.8).

use editor_core::change::Set as ChangeSet;
use editor_core::transaction::EditType;

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;

use editor_core::transaction::Transaction;
/// A single auto-pair definition.
#[derive(Clone, Copy, Debug)]
pub struct AutoPair {
    pub open: char,
    pub close: char,
}

/// The default set of pairs: `()`, `[]`, `{}`, `""`, `` `` ``.
pub const DEFAULT_PAIRS: &[AutoPair] = &[
    AutoPair { open: '(', close: ')' },
    AutoPair { open: '[', close: ']' },
    AutoPair { open: '{', close: '}' },
    AutoPair { open: '"', close: '"' },
    AutoPair { open: '`', close: '`' },
];

/// If the user is typing a known close char AND the cursor is sitting right
/// before an auto-inserted close char of the same kind, return a transaction
/// that *only* moves the cursor past the existing close (no insert) — the
/// "skip-over-close" UX from VSCode/IntelliJ/CM6.
///
/// `skip_marker` is the byte position immediately AFTER the most recent
/// auto-inserted close char. Pass `view.autopair_skip_at`. Caller should
/// `take()` it so this method only fires once per pairing.
pub fn autopair_skip(
    state: &EditorState,
    skip_marker: Option<usize>,
    inserted: &str,
) -> Option<Transaction> {
    let marker = skip_marker?;
    let mut chars = inserted.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // Must be a known close char.
    if !DEFAULT_PAIRS.iter().any(|p| p.close == first) {
        return None;
    }
    let main = state.selection.main();
    if !main.is_empty() {
        return None;
    }
    let cursor = main.head.offset();
    let close_len = first.len_utf8();
    // Cursor must be sitting just before the marker's recorded close char.
    if cursor + close_len != marker {
        return None;
    }
    if cursor >= state.doc.len_bytes() {
        return None;
    }
    if state.doc.byte_at(cursor) != first as u8 {
        return None;
    }
    let new_sel = Selection::single(cursor + close_len);
    let changes = ChangeSet::empty(state.doc.len_bytes());
    Some(
        Transaction::new(changes)
            .with_selection(new_sel)
            .with_edit_type(EditType::Other),
    )
}

/// True when the character immediately to the right of `cursor` is
/// non-whitespace. Used to suppress auto-close when wrapping/typing before
/// existing text (the bare cursor-at-end / cursor-before-whitespace case still
/// auto-closes). Document end counts as "no text to the right".
fn has_text_to_right(state: &EditorState, cursor: usize) -> bool {
    let doc = &state.doc;
    if cursor >= doc.len_bytes() {
        return false;
    }
    let end = doc.next_char_boundary(cursor);
    doc.slice(cursor..end)
        .to_string()
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace())
}

/// If `inserted` is a single auto-pair opener and every selection range is
/// empty, produce a transaction that inserts `<open><close>` at every cursor
/// and places each cursor between the pair.
///
/// Returns `None` otherwise (caller should fall through to a normal insert).
pub fn autopair_transform(state: &EditorState, inserted: &str) -> Option<Transaction> {
    // Exactly one character.
    let mut chars = inserted.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let pair = DEFAULT_PAIRS.iter().find(|p| p.open == first)?;

    // All ranges must be empty.
    let ranges = state.selection.ranges();
    if ranges.iter().any(|r| !r.is_empty()) {
        return None;
    }

    // Don't auto-close when there is non-whitespace text immediately to the
    // right of any cursor: surrounding/typing before existing text shouldn't
    // inject a stray closer. Fall through to a plain insert instead.
    if ranges.iter().any(|r| has_text_to_right(state, r.start())) {
        return None;
    }

    // Build the (range, replacement) list. Insertion text is "<open><close>".
    let mut insertion = String::with_capacity(first.len_utf8() + pair.close.len_utf8());
    insertion.push(pair.open);
    insertion.push(pair.close);
    let open_len = pair.open.len_utf8();

    // Collect & sort cursor positions (dedup overlapping points).
    let mut positions: Vec<usize> = ranges.iter().map(editor_core::selection::SelRange::start).collect();
    positions.sort_unstable();
    positions.dedup();

    let edits: Vec<(std::ops::Range<usize>, String)> = positions
        .iter()
        .map(|&pos| (pos..pos, insertion.clone()))
        .collect();

    let doc_len = state.doc.len_bytes();
    let changes = ChangeSet::of(doc_len, edits);

    // Compute resulting cursor positions: each cursor lands between open and
    // close. After applying N insertions of length `insertion.len()` before
    // position P (counting strictly-before insertions), the new cursor is at
    // (original P) + (#prior insertions) * insertion.len() + open_len.
    let ins_len = insertion.len();
    let new_ranges: Vec<SelRange> = positions
        .iter()
        .enumerate()
        .map(|(i, &pos)| {
            let new_pos = pos + i * ins_len + open_len;
            SelRange::point(new_pos)
        })
        .collect();

    let selection = if new_ranges.len() == 1 {
        Selection::single(new_ranges[0].start())
    } else {
        Selection::from_ranges(new_ranges, 0)
    };

    Some(
        Transaction::new(changes)
            .with_selection(selection)
            .with_edit_type(EditType::Input),
    )
}
