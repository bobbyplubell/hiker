//! Integration tests for auto-pair transform (SPEC §9.8).

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;
use editor_view::pairs::autopair_transform;
use editor_view::pairs::DEFAULT_PAIRS;
#[test]
fn typing_open_paren_at_empty_cursor_inserts_pair() {
    let state = EditorState::new("");
    // cursor at 0 by default (single point).
    let tx = autopair_transform(&state, "(").expect("should produce a transaction");
    let new_state = state.apply(tx);
    assert_eq!(new_state.doc.to_string(), "()");
    let sel = new_state.selection.main();
    assert!(sel.is_empty());
    assert_eq!(sel.start(), 1, "cursor should land between '(' and ')'");
}

#[test]
fn typing_open_paren_with_selected_text_is_skipped() {
    let mut state = EditorState::new("hello");
    state.selection = Selection::from_range(SelRange::new(0, 5));
    let result = autopair_transform(&state, "(");
    assert!(
        result.is_none(),
        "autopair should return None when selection is non-empty"
    );
}

#[test]
fn non_opener_returns_none() {
    let state = EditorState::new("abc");
    assert!(autopair_transform(&state, "x").is_none());
}

#[test]
fn multi_char_input_returns_none() {
    let state = EditorState::new("");
    assert!(autopair_transform(&state, "((").is_none());
}

#[test]
fn all_default_pairs_work() {
    for pair in DEFAULT_PAIRS {
        let state = EditorState::new("");
        let input = pair.open.to_string();
        let tx = autopair_transform(&state, &input).expect("should produce tx");
        let new_state = state.apply(tx);
        let mut expected = String::new();
        expected.push(pair.open);
        expected.push(pair.close);
        assert_eq!(new_state.doc.to_string(), expected);
        assert_eq!(new_state.selection.main().start(), pair.open.len_utf8());
    }
}

#[test]
fn multi_cursor_autopair() {
    // Cursors sit before whitespace / at end so the no-close-when-text-right
    // rule doesn't suppress auto-close (offsets 1, 3, 5 in "a b c").
    let mut state = EditorState::new("a b c");
    state.selection = Selection::from_ranges(
        vec![
            SelRange::point(1),
            SelRange::point(3),
            SelRange::point(5),
        ],
        0,
    );
    let tx = autopair_transform(&state, "[").expect("should produce tx");
    let new_state = state.apply(tx);
    assert_eq!(new_state.doc.to_string(), "a[] b[] c[]");
    // Each cursor should land between '[' and ']'.
    let ranges = new_state.selection.ranges();
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[0].start(), 2);
    assert_eq!(ranges[1].start(), 6);
    assert_eq!(ranges[2].start(), 10);
}

#[test]
fn typing_close_after_autopair_skips_instead_of_doubling() {
    use editor_view::pairs::autopair_skip;
    let mut state = EditorState::new("");
    state.selection = Selection::single(0);
    let tx = autopair_transform(&state, "(").expect("autopair fires");
    state = state.apply(tx);
    assert_eq!(state.doc.to_string(), "()");
    let skip_tx = autopair_skip(&state, Some(2usize), ")").expect("skip fires");
    assert!(skip_tx.changes.is_identity(), "skip must not edit text");
    let next = state.apply(skip_tx);
    assert_eq!(next.doc.to_string(), "()");
    assert_eq!(next.selection.main().head.offset(), 2);
}

#[test]
fn skip_does_not_fire_for_non_close_chars() {
    use editor_view::pairs::autopair_skip;
    let state = EditorState::new("()");
    assert!(autopair_skip(&state, Some(2), "x").is_none());
}

#[test]
fn skip_does_not_fire_without_marker() {
    use editor_view::pairs::autopair_skip;
    let state = EditorState::new("()");
    assert!(autopair_skip(&state, None, ")").is_none());
}

#[test]
fn backtick_does_not_autoclose_with_text_to_right() {
    // Cursor at offset 0, "code" to the right: typing ` should NOT insert a
    // closer (the owner is surrounding existing text). Falls through to a
    // plain insert handled by the caller.
    let mut state = EditorState::new("code");
    state.selection = Selection::single(0);
    assert!(
        autopair_transform(&state, "`").is_none(),
        "backtick should not auto-close when non-whitespace is to the right"
    );
}

#[test]
fn backtick_autocloses_with_whitespace_to_right() {
    // Whitespace to the right counts as "no text"; auto-close still fires.
    let mut state = EditorState::new(" rest");
    state.selection = Selection::single(0);
    let tx = autopair_transform(&state, "`").expect("autopair fires before whitespace");
    let new_state = state.apply(tx);
    assert_eq!(new_state.doc.to_string(), "`` rest");
    assert_eq!(new_state.selection.main().start(), 1);
}

#[test]
fn backtick_autocloses_at_document_end() {
    // The bare empty/end-of-doc case still auto-closes.
    let state = EditorState::new("");
    let tx = autopair_transform(&state, "`").expect("autopair fires at doc end");
    let new_state = state.apply(tx);
    assert_eq!(new_state.doc.to_string(), "``");
    assert_eq!(new_state.selection.main().start(), 1);
}

#[test]
fn autoclose_suppressed_for_all_pairs_with_text_to_right() {
    for pair in DEFAULT_PAIRS {
        let mut state = EditorState::new("x");
        state.selection = Selection::single(0);
        assert!(
            autopair_transform(&state, &pair.open.to_string()).is_none(),
            "{} should not auto-close before non-whitespace",
            pair.open
        );
    }
}
