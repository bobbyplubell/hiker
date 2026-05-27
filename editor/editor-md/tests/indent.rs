use editor_core::state::Editor as EditorState;
use editor_core::selection::Selection;
use editor_md::indenter::{
    markdown_indent_on_enter, markdown_indent_on_tab, markdown_outdent_on_shift_tab,
    markdown_strip_bullet_on_paste,
};

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

#[test]
fn tab_indents_list_bullet_by_one_step() {
    let text = "- item\n";
    let caret = "- it".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_tab(&state).expect("list line should indent");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "    - item\n");
    // Caret stays on the same character, shifted right by the indent width.
    assert_eq!(next.selection.main().head.offset(), caret + 4);
}

#[test]
fn tab_indents_already_nested_bullet() {
    let text = "    - item\n";
    let caret = "    - item".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_indent_on_tab(&state).expect("nested list line should indent");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "        - item\n");
}

#[test]
fn tab_on_non_list_line_returns_none() {
    let text = "plain prose\n";
    let caret = "plain".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_indent_on_tab(&state).is_none());
}

#[test]
fn shift_tab_outdents_nested_bullet() {
    let text = "    - item\n";
    let caret = "    - it".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_outdent_on_shift_tab(&state).expect("nested list line should outdent");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "- item\n");
    assert_eq!(next.selection.main().head.offset(), caret - 4);
}

#[test]
fn shift_tab_outdents_partial_indent() {
    // Two leading spaces: outdent removes up to a full step, here just the two.
    let text = "  - item\n";
    let caret = "  - item".len();
    let state = state_with_caret(text, caret);
    let tx = markdown_outdent_on_shift_tab(&state).expect("should outdent partial indent");
    let next = state.apply(tx);
    assert_eq!(next.doc.to_string(), "- item\n");
}

#[test]
fn shift_tab_at_column_zero_is_noop() {
    let text = "- item\n";
    let caret = "- item".len();
    let state = state_with_caret(text, caret);
    // No leading indentation to remove: provider declines so the default
    // outdent path (which also no-ops here) runs.
    assert!(markdown_outdent_on_shift_tab(&state).is_none());
}

#[test]
fn shift_tab_on_non_list_line_returns_none() {
    let text = "    plain prose\n";
    let caret = "    plain".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_outdent_on_shift_tab(&state).is_none());
}

#[test]
fn paste_bullet_after_bullet_strips_leading_marker() {
    // Caret sits right after the "- " bullet on an otherwise-empty list line;
    // pasting "- foo" should drop the pasted bullet so only one bullet shows.
    let text = "- \n";
    let caret = "- ".len();
    let state = state_with_caret(text, caret);
    let stripped =
        markdown_strip_bullet_on_paste(&state, "- foo").expect("should strip the doubled bullet");
    assert_eq!(stripped, "foo");
}

#[test]
fn paste_bullet_mid_prose_is_verbatim() {
    // Caret in plain prose (not after a bullet): paste is left untouched.
    let text = "plain prose\n";
    let caret = "plain ".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_strip_bullet_on_paste(&state, "- foo").is_none());
}

#[test]
fn paste_non_bullet_text_after_bullet_is_verbatim() {
    // Caret after a bullet, but the pasted text isn't a list item: untouched.
    let text = "- \n";
    let caret = "- ".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_strip_bullet_on_paste(&state, "foo").is_none());
}

#[test]
fn paste_bullet_not_at_content_start_is_verbatim() {
    // Caret mid-content on a list line (not right after the marker): the user
    // is pasting into existing text, so don't treat the leading "- " as a
    // doubled bullet.
    let text = "- item\n";
    let caret = "- it".len();
    let state = state_with_caret(text, caret);
    assert!(markdown_strip_bullet_on_paste(&state, "- foo").is_none());
}
