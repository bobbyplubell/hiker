//! Small-UX feature smoke tests: active line highlight, placeholder, special
//! chars, trailing whitespace, scroll-past-end. See SPEC §9.11–§9.18.

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;
use editor_view::highlights::active_line_decorations;
use editor_view::whitespace::special_chars_decorations;
use editor_view::highlights::trailing_whitespace_decorations;
use editor_view::whitespace::SpecialCharsFlags;
use editor_view::viewport::ViewState;
#[test]
fn active_line_emits_line_decoration() {
    let mut state = EditorState::new("alpha\nbeta\ngamma\n");
    // Place cursor on line 1 ("beta").
    state.selection = Selection::from_range(SelRange::point(8));
    let set = active_line_decorations(&state);
    assert!(set.iter_all().any(|(_, d)| matches!(d, Decoration::Line(_))));
}

#[test]
fn placeholder_defaults_to_none() {
    let v = ViewState::default();
    assert!(v.placeholder.is_none());
}

#[test]
fn special_chars_produces_replace() {
    let state = EditorState::new("a\tb c\n");
    let flags = SpecialCharsFlags {
        tabs: true,
        spaces: true,
        nbsp: false,
        zero_width: false,
        crlf: false,
    };
    let set = special_chars_decorations(&state, flags);
    assert!(set
        .iter_all()
        .any(|(_, d)| matches!(d, Decoration::Replace { .. })));
}

#[test]
fn trailing_whitespace_produces_mark() {
    let state = EditorState::new("hello   \nworld\n");
    let set = trailing_whitespace_decorations(&state, None);
    assert!(set.iter_all().any(|(_, d)| matches!(d, Decoration::Mark(_))));
}

#[test]
fn scroll_past_end_allows_scroll_past_last_line() {
    use editor_view::command;
    use editor_view::command::Action;
    use editor_view::events::InputEvent;

    let state = EditorState::new("line0\nline1\nline2\n");
    let mut view = ViewState {
        height: 100.0,
        line_height: 18.0,
        scroll_past_end: 0.5,
        ..ViewState::default()
    };
    view.sync_to(&state);

    let total = view.height_map.total_height();
    let baseline_max = (total - view.height).max(0.0);
    // Scroll a huge amount down so we hit the cap.
    let _ = command::handle(&state, &mut view, &InputEvent::Scroll { delta_x: 0.0, delta_y: -10_000.0 });
    assert!(
        view.scroll_y > baseline_max,
        "scroll_past_end should permit scrolling past the baseline max ({} > {})",
        view.scroll_y,
        baseline_max
    );

    // Sanity: equivalent dispatch with scroll_past_end = 0.0 caps at baseline.
    let mut view2 = ViewState {
        height: 100.0,
        line_height: 18.0,
        ..ViewState::default()
    };
    view2.sync_to(&state);
    let _ = command::handle(&state, &mut view2, &InputEvent::Scroll { delta_x: 0.0, delta_y: -10_000.0 });
    let cap2 = (view2.height_map.total_height() - view2.height).max(0.0);
    assert!((view2.scroll_y - cap2).abs() < 0.001, "without past-end should clamp to baseline");
    drop::<Action>(Action::None);
}
