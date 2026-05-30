//! Progressive mouse selection by click count, matching VSCode:
//! double-click (click_count == 2) selects the word under the pointer,
//! triple-click (click_count == 3) selects the whole line.
//!
//! Guards the `click_count` match arms in `command::mouse_down`: the egui
//! translation layer computes 1/2/3 from egui's
//! `button_double_clicked` / `button_triple_clicked`, and the view must map
//! 2 -> word and 3 -> line.

use editor_core::state::Editor as EditorState;
use editor_view::command::handle_mouse_with_mods;
use editor_view::command::Action;
use editor_view::events::Modifiers;
use editor_view::events::MouseButton;
use editor_view::events::MouseEvent;
use editor_view::viewport::ViewState;

fn make_view() -> ViewState {
    ViewState {
        gutter_width: 0.0,
        font_size: 14.0,
        line_height: 18.0,
        width: 400.0,
        height: 200.0,
        ..ViewState::default()
    }
}

/// Map a byte column on a given line to an x coordinate that
/// `view_to_buffer` maps back to the same byte (mono-width approximation),
/// and the line index to a y coordinate.
fn x_for_col(view: &ViewState, col: usize) -> f32 {
    let char_w = view.font_size * 0.55;
    view.gutter_width + col as f32 * char_w
}

fn y_for_line(view: &ViewState, line: usize) -> f32 {
    line as f32 * view.line_height + view.line_height / 2.0
}

fn apply(action: Action, state: &mut EditorState) {
    if let Action::Replace { state: next, .. } = action {
        *state = next;
    }
}

#[test]
fn double_click_selects_word() {
    // "hello world" — double-click inside "world" (byte 8) selects 6..11.
    let mut state = EditorState::new("hello world");
    let mut view = make_view();
    view.sync_to(&state);

    let down = MouseEvent::Down {
        button: MouseButton::Left,
        x: x_for_col(&view, 8),
        y: y_for_line(&view, 0),
        click_count: 2,
    };
    let act = handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default());
    apply(act, &mut state);

    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 11), "double-click selects the word");
}

#[test]
fn double_click_word_survives_zero_motion_drag() {
    // A held button emits a no-motion Drag the frame after the press
    // (translate.rs). It must NOT collapse the double-click's word selection
    // back to a caret. Regression for the "word highlights then vanishes" bug.
    let mut state = EditorState::new("hello world");
    let mut view = make_view();
    view.sync_to(&state);
    let (x, y) = (x_for_col(&view, 8), y_for_line(&view, 0));

    let down = MouseEvent::Down { button: MouseButton::Left, x, y, click_count: 2 };
    apply(handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 11), "double-click selects the word");

    // Same coordinates → no motion → selection preserved.
    let drag = MouseEvent::Drag { x, y, button: MouseButton::Left };
    apply(handle_mouse_with_mods(&state, &mut view, &drag, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 11), "no-motion drag keeps the word selected");
}

#[test]
fn double_click_word_survives_jitter_within_word() {
    // The real intermittent bug: a tiny pointer move during the double-click
    // lands the stray Drag on a *different* byte that's still inside the word.
    // Exact-equality guarding missed this; the union must keep the whole word
    // as long as the pointer stays within it.
    let mut state = EditorState::new("hello world");
    let mut view = make_view();
    view.sync_to(&state);
    let y = y_for_line(&view, 0);

    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 8), y, click_count: 2 };
    apply(handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default()), &mut state);
    assert_eq!((state.selection.main().start(), state.selection.main().end()), (6, 11));

    // Jitter one column over — still inside "world" (6..11).
    let drag = MouseEvent::Drag { x: x_for_col(&view, 9), y, button: MouseButton::Left };
    apply(handle_mouse_with_mods(&state, &mut view, &drag, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 11), "jitter inside the word keeps the whole word");
}

#[test]
fn drag_after_double_click_extends_from_word_edge() {
    // Dragging past the word's edge extends from the far edge (word stays
    // wholly selected), rather than collapsing to a point-anchored range.
    let mut state = EditorState::new("hello world foo");
    let mut view = make_view();
    view.sync_to(&state);
    let y = y_for_line(&view, 0);

    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 8), y, click_count: 2 };
    apply(handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default()), &mut state);
    assert_eq!((state.selection.main().start(), state.selection.main().end()), (6, 11));

    // Drag right into "foo": selection runs from the word's left edge (6) to
    // the pointer, past the word's right edge (11). (Exact end byte depends on
    // the test's mono col→byte approximation, so assert it extended past 11.)
    let drag = MouseEvent::Drag { x: x_for_col(&view, 13), y, button: MouseButton::Left };
    apply(handle_mouse_with_mods(&state, &mut view, &drag, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!(sel.start(), 6, "selection keeps the word's left edge");
    assert!(sel.end() > 11, "drag past the word extends beyond its right edge (got {})", sel.end());
}

#[test]
fn double_click_pattern_includes_hyphen() {
    // `editor.double_click_pattern = "[\\w-]+"` makes double-click select
    // hyphenated words whole, instead of stopping at the hyphen.
    let mut state = EditorState::new("foo-bar baz");
    let mut view = make_view();
    view.sync_to(&state);
    view.double_click_re = std::sync::Arc::new(regex::Regex::new(r"[\w-]+").unwrap());

    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 2), y: y_for_line(&view, 0), click_count: 2 };
    apply(handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (0, 7), "hyphen pattern selects the whole foo-bar");
}

#[test]
fn triple_click_pattern_overrides_whole_line() {
    // A triple-click pattern selects the match under the cursor instead of the
    // whole line. `\w+` here selects just the clicked word.
    let mut state = EditorState::new("alpha beta gamma");
    let mut view = make_view();
    view.sync_to(&state);
    view.triple_click_re = std::sync::Arc::new(regex::Regex::new(r"\w+").unwrap());

    // Click inside "beta" (byte 7).
    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 7), y: y_for_line(&view, 0), click_count: 3 };
    apply(handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default()), &mut state);
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 10), "triple-click pattern selects the word, not the line");
}

#[test]
fn triple_click_selects_line() {
    // Multi-line buffer; triple-click on line 1 selects the whole line
    // including its trailing newline (line_start..next_line_start).
    let mut state = EditorState::new("alpha beta\ngamma delta\nepsilon\n");
    let mut view = make_view();
    view.sync_to(&state);

    // Line 1 is "gamma delta\n": bytes 11..23.
    let down = MouseEvent::Down {
        button: MouseButton::Left,
        x: x_for_col(&view, 3),
        y: y_for_line(&view, 1),
        click_count: 3,
    };
    let act = handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default());
    apply(act, &mut state);

    let sel = state.selection.main();
    let selected = &state.doc.to_string()[sel.start()..sel.end()];
    assert_eq!(selected, "gamma delta\n", "triple-click selects the whole line");
}

#[test]
fn triple_click_selects_last_line_without_newline() {
    // Last line has no trailing newline; selection runs to end of buffer.
    let mut state = EditorState::new("alpha\nomega");
    let mut view = make_view();
    view.sync_to(&state);

    let down = MouseEvent::Down {
        button: MouseButton::Left,
        x: x_for_col(&view, 2),
        y: y_for_line(&view, 1),
        click_count: 3,
    };
    let act = handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default());
    apply(act, &mut state);

    let sel = state.selection.main();
    let selected = &state.doc.to_string()[sel.start()..sel.end()];
    assert_eq!(selected, "omega", "triple-click on the last line selects to buffer end");
}
