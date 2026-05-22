use editor_core::state::Editor as EditorState;
use editor_core::selection::Selection;
use editor_md::indenter::markdown_indent_on_enter;

fn state_with_caret(text: &str, caret: usize) -> EditorState {
    let mut s = EditorState::new(text);
    s.selection = Selection::single(caret);
    s
}

#[test]
fn enter_inside_dash_item_continues_list() {
    let text = "- item\n";
    // Caret at end of "- item" (before the trailing newline).
    let caret = "- item".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_enter(&state).expect("should produce tx");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "- item\n- \n");
    // Caret should sit after the "- " marker on the new line.
    let expected_caret = "- item\n- ".len();
    assert_eq!(next.selection.main().head.offset(), expected_caret);
}

#[test]
fn enter_on_empty_bullet_escapes_list() {
    let text = "- \n";
    // Caret at end of the marker.
    let caret = "- ".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_enter(&state).expect("should produce tx");
    let next = state.apply(tx);
    // The "- " is removed and replaced with a single newline.
    assert_eq!(next.doc.to_string(), "\n\n");
    assert_eq!(next.selection.main().head.offset(), 1);
}

#[test]
fn enter_inside_ordered_item_continues_with_same_number() {
    // v1 documented behavior: keep the same number ("1. " again). The user
    // can renumber by hand; smart renumbering is a follow-up.
    let text = "1. item\n";
    let caret = "1. item".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_enter(&state).expect("should produce tx");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "1. item\n1. \n");
}

#[test]
fn enter_on_non_list_line_returns_none() {
    let text = "plain prose\n";
    let caret = "plain prose".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_indent_on_enter(&state).is_none());
}

#[test]
fn enter_continues_indented_nested_bullet() {
    let text = "  - nested\n";
    let caret = "  - nested".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_enter(&state).expect("should produce tx");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "  - nested\n  - \n");
}
