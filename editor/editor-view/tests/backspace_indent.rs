//! Backspace over leading indentation deletes a whole tab-width group when
//! the run of spaces before the caret is a tab-width multiple, and a single
//! character otherwise. Regression guard for the bug where Tab inserted four
//! spaces but Backspace only ever removed one.

use editor_core::selection::{SelRange, Selection};
use editor_core::state::Editor as EditorState;
use editor_view::command::{handle, Action};
use editor_view::events::{InputEvent, Key, KeyEvent, Modifiers, NamedKey};
use editor_view::viewport::ViewState;

fn backspace(state: &EditorState) -> EditorState {
    let mut view = ViewState::default();
    view.sync_to(state);
    let ev = InputEvent::Key(KeyEvent {
        key: Key::Named(NamedKey::Backspace),
        mods: Modifiers::default(),
        repeat: false,
    });
    match handle(state, &mut view, &ev) {
        Action::Replace { state: next, .. } => next,
        _ => panic!("expected Action::Replace from Backspace"),
    }
}

fn at(text: &str, caret: usize) -> EditorState {
    let mut s = EditorState::new(text);
    s.selection = Selection::from_range(SelRange::point(caret));
    s
}

#[test]
fn backspace_deletes_full_tab_width_group() {
    // Caret after exactly one tab-width (4) of leading spaces.
    let state = at("    text\n", 4);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "text\n");
    assert_eq!(next.selection.main().head.offset(), 0);
}

#[test]
fn backspace_deletes_one_group_at_a_time() {
    // Two tab-widths of indentation; one Backspace removes the inner group.
    let state = at("        text\n", 8);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "    text\n");
    assert_eq!(next.selection.main().head.offset(), 4);
}

#[test]
fn backspace_partial_indent_deletes_single_space() {
    // Three leading spaces is not a tab-width multiple: delete one space so the
    // caret lands on a tab-width boundary, not a whole group.
    let state = at("   text\n", 3);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "  text\n");
    assert_eq!(next.selection.main().head.offset(), 2);
}

#[test]
fn backspace_after_content_deletes_single_char() {
    // Caret after content (not in leading indentation): single-char delete,
    // grouping must not kick in even though four spaces precede the caret.
    let state = at("ab    cd\n", 6);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "ab   cd\n");
    assert_eq!(next.selection.main().head.offset(), 5);
}

#[test]
fn backspace_on_indented_list_marker_groups() {
    // Leading indentation before a list bullet still groups.
    let state = at("    - item\n", 4);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "- item\n");
}

#[test]
fn backspace_single_char_word() {
    let state = at("hello\n", 5);
    let next = backspace(&state);
    assert_eq!(next.doc.to_string(), "hell\n");
}
