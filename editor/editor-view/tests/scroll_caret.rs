//! `command::scroll_caret_into_view` keeps the caret within the visible band
//! after an edit. Guards `bug-editor-no-scroll-cursor-into-view-on-edit`: the
//! command sets a deferred flag and the widget calls this once the height map
//! is current. Here we exercise the geometry directly.

use editor_core::selection::Selection;
use editor_core::state::Editor as EditorState;
use editor_view::command::scroll_caret_into_view;
use editor_view::viewport::ViewState;

/// 51 lines ("a\n" x 50 + trailing empty). Line N starts at byte 2*N, so a
/// caret on line N is byte 2*N. Default line height 18px → line N top = 18*N.
fn make_state_and_view() -> (EditorState, ViewState) {
    let state = EditorState::new(&"a\n".repeat(50));
    let mut view = ViewState {
        line_height: 18.0,
        height: 100.0, // ~5.5 lines visible
        width: 400.0,
        ..ViewState::default()
    };
    view.sync_to(&state);
    (state, view)
}

fn caret_on_line(state: &mut EditorState, line: usize) {
    state.selection = Selection::single(line * 2);
}

#[test]
fn scrolls_down_when_caret_below_viewport() {
    let (mut state, mut view) = make_state_and_view();
    view.scroll_y = 0.0;
    caret_on_line(&mut state, 30);

    scroll_caret_into_view(&state, &mut view);

    // bottom of line 30 = 18*30 + 18 = 558; flush to viewport bottom → 558-100.
    assert_eq!(view.scroll_y, 458.0);
}

#[test]
fn scrolls_up_when_caret_above_viewport() {
    let (mut state, mut view) = make_state_and_view();
    view.scroll_y = 500.0;
    caret_on_line(&mut state, 5);

    scroll_caret_into_view(&state, &mut view);

    // top of line 5 = 90; band pulled up to the caret line top.
    assert_eq!(view.scroll_y, 90.0);
}

#[test]
fn no_op_when_caret_already_visible() {
    let (mut state, mut view) = make_state_and_view();
    view.scroll_y = 90.0; // band [90, 190] shows lines 5..~10
    caret_on_line(&mut state, 7); // top 126, bottom 144 — inside the band

    scroll_caret_into_view(&state, &mut view);

    assert_eq!(view.scroll_y, 90.0);
}
