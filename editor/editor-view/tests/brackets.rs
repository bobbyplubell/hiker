//! Tests for `bracket_match_decorations`. See SPEC §9.10.

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_core::selection::Selection;
use editor_view::brackets::bracket_match_decorations;
use editor_view::brackets::DEFAULT_BRACKETS;
fn set_caret(state: &mut EditorState, byte: usize) {
    state.selection = Selection::single(byte);
}

fn collect(state: &EditorState) -> Vec<(std::ops::Range<usize>, Decoration)> {
    bracket_match_decorations(state, DEFAULT_BRACKETS, 1024)
        .iter_all()
        .map(|(r, d)| (r, d.clone()))
        .collect()
}

fn bg_of(d: &Decoration) -> Option<(u8, u8, u8, u8)> {
    if let Decoration::Mark(m) = d {
        m.bg.map(|c| (c.r, c.g, c.b, c.a))
    } else {
        None
    }
}

const MATCH: (u8, u8, u8, u8) = (100, 180, 240, 80);
const WARN: (u8, u8, u8, u8) = (240, 100, 100, 90);

#[test]
fn caret_after_open_paren_matches_close() {
    // "(foo)" — cursor right after '(' at byte 1.
    let mut state = EditorState::new("(foo)");
    set_caret(&mut state, 1);

    let decos = collect(&state);
    assert_eq!(decos.len(), 2, "expected open and close highlighted: {decos:?}");
    // Both should be MATCH-colored.
    for (_, d) in &decos {
        assert_eq!(bg_of(d), Some(MATCH));
    }
    let ranges: Vec<_> = decos.iter().map(|(r, _)| r.clone()).collect();
    assert!(ranges.contains(&(0..1)), "missing open paren range: {ranges:?}");
    assert!(ranges.contains(&(4..5)), "missing close paren range: {ranges:?}");
}

#[test]
fn unmatched_bracket_emits_warning() {
    // Mismatched: caret right after `(` but there's no closing paren.
    let mut state = EditorState::new("(foo");
    set_caret(&mut state, 1);

    let decos = collect(&state);
    assert_eq!(decos.len(), 1, "expected only the unmatched bracket: {decos:?}");
    assert_eq!(decos[0].0, 0..1);
    assert_eq!(bg_of(&decos[0].1), Some(WARN));
}

#[test]
fn no_adjacent_bracket_produces_no_decorations() {
    let mut state = EditorState::new("hello world");
    set_caret(&mut state, 3);

    let decos = collect(&state);
    assert!(decos.is_empty(), "expected no decorations, got {decos:?}");
}

#[test]
fn nested_brackets_match_outer() {
    // "(a(b)c)" — caret after outer '(' at byte 1 should match outer ')' at 6.
    let mut state = EditorState::new("(a(b)c)");
    set_caret(&mut state, 1);

    let decos = collect(&state);
    assert_eq!(decos.len(), 2, "expected matched outer pair: {decos:?}");
    let ranges: Vec<_> = decos.iter().map(|(r, _)| r.clone()).collect();
    assert!(ranges.contains(&(0..1)));
    assert!(ranges.contains(&(6..7)), "should match outer ')', got {ranges:?}");
    for (_, d) in &decos {
        assert_eq!(bg_of(d), Some(MATCH));
    }
}
