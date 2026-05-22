//! Tests for atomic decoration ranges: motion commands that would land
//! strictly inside a `Decoration::Replace` (or a `Mark` with `atomic = true`)
//! range must snap to the appropriate boundary instead.

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::selection::Selection;
use editor_view::motion;
use editor_view::motion::Direction;
use smol_str::SmolStr;

#[test]
fn move_char_right_skips_replace_range() {
    // Text: a b c [ Z Z Z ] x y z
    // idx:  0 1 2 3 4 5 6 7 8 9 10
    // Atomic Replace covers bytes 3..8 (the "[ZZZ]" region).
    let text = "abc[ZZZ]xyz";
    let mut state = EditorState::new(text);
    // Place cursor just before '['.
    state = state.apply(
        editor_core::transaction::Transaction::new(editor_core::change::Set::empty(state.doc.len_bytes()))
            .with_selection(Selection::single(3)),
    );

    let set: DecorationSet = RangeSet::from_iter([(
        3..8,
        Decoration::Replace { display: Some(SmolStr::new_static("…")) },
    )]);
    let layers = vec![set];

    let sel = motion::move_char(&state, Direction::Right, false, &layers);
    let head = sel.main().head.offset();
    assert_eq!(head, 8, "cursor should snap past the atomic range, not land inside it");
}

#[test]
fn move_char_left_skips_into_replace_range() {
    let text = "abc[ZZZ]xyz";
    let mut state = EditorState::new(text);
    // Place cursor just after ']'.
    state = state.apply(
        editor_core::transaction::Transaction::new(editor_core::change::Set::empty(state.doc.len_bytes()))
            .with_selection(Selection::single(8)),
    );

    let set: DecorationSet = RangeSet::from_iter([(
        3..8,
        Decoration::Replace { display: Some(SmolStr::new_static("…")) },
    )]);
    let layers = vec![set];

    let sel = motion::move_char(&state, Direction::Left, false, &layers);
    let head = sel.main().head.offset();
    assert_eq!(head, 3, "left motion should snap to the start of the atomic range");
}

#[test]
fn atomic_mark_is_respected() {
    let text = "abc[ZZZ]xyz";
    let mut state = EditorState::new(text);
    state = state.apply(
        editor_core::transaction::Transaction::new(editor_core::change::Set::empty(state.doc.len_bytes()))
            .with_selection(Selection::single(3)),
    );

    let style = MarkStyle { atomic: true, ..MarkStyle::default() };
    let set: DecorationSet = RangeSet::from_iter([(3..8, Decoration::Mark(style))]);
    let layers = vec![set];

    let sel = motion::move_char(&state, Direction::Right, false, &layers);
    assert_eq!(sel.main().head.offset(), 8);
}

#[test]
fn non_atomic_mark_does_not_snap() {
    let text = "abc[ZZZ]xyz";
    let mut state = EditorState::new(text);
    state = state.apply(
        editor_core::transaction::Transaction::new(editor_core::change::Set::empty(state.doc.len_bytes()))
            .with_selection(Selection::single(3)),
    );

    // No `atomic` flag set — motion proceeds one byte at a time.
    let style = MarkStyle { bold: true, ..MarkStyle::default() };
    let set: DecorationSet = RangeSet::from_iter([(3..8, Decoration::Mark(style))]);
    let layers = vec![set];

    let sel = motion::move_char(&state, Direction::Right, false, &layers);
    assert_eq!(sel.main().head.offset(), 4);
}
