//! Drag-and-drop of selected text. SPEC.md §7.3 / IMPLEMENTATION.md §16.5.7.
//!
//! Verifies the state machine in `command::handle_mouse_with_mods`:
//! - mouse Down inside an existing non-empty selection does NOT move the
//!   caret (it arms a possible text drag),
//! - a subsequent Drag past the pixel threshold transitions to a real text
//!   drag with a tracking drop caret,
//! - mouse Up with the drop caret outside the original range moves the
//!   text and selects the inserted copy.

use editor_core::change::Set as ChangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;

use editor_core::transaction::Transaction;
use editor_view::command::handle_mouse_with_mods;
use editor_view::command::Action;
use editor_view::events::Modifiers;
use editor_view::events::MouseButton;
use editor_view::events::MouseEvent;
use editor_view::viewport::DragState;
use editor_view::viewport::ViewState;
/// Build a view sized so the x-to-byte mapping in `view_to_buffer` is
/// roughly one pixel per character (gutter = 0, char_w ≈ font_size * 0.55).
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

/// Convert a byte column on line 0 to an x coordinate that `view_to_buffer`
/// will map back to the same byte (mono-width approximation).
fn x_for_col(view: &ViewState, col: usize) -> f32 {
    let char_w = view.font_size * 0.55;
    view.gutter_width + col as f32 * char_w
}

fn state_with_selection(text: &str, range: std::ops::Range<usize>) -> EditorState {
    let st = EditorState::new(text);
    let sel = Selection::from_range(SelRange::new(range.start, range.end));
    st.apply(Transaction::new(ChangeSet::empty(st.doc.len_bytes())).with_selection(sel))
}

fn apply(action: Action, state: &mut EditorState) {
    if let Action::Replace { state: next, .. } = action {
        *state = next;
    }
}

#[test]
fn drag_inside_selection_to_start_moves_text() {
    // "hello world" with "world" selected (bytes 6..11). The user presses
    // the mouse inside the selection (byte 8), drags to byte 0, and releases.
    // Expected: the text "world" is removed from 6..11 and inserted at 0,
    // yielding "worldhello " with the new selection covering "world".
    let mut state = state_with_selection("hello world", 6..11);
    let mut view = make_view();
    view.sync_to(&state);

    let y = 4.0; // anywhere inside line 0
    let mods = Modifiers::default();

    // 1. Mouse down at byte 8 (inside selection). Should NOT move caret.
    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 8), y, click_count: 1 };
    let act = handle_mouse_with_mods(&state, &mut view, &down, mods);
    assert!(matches!(act, Action::None), "mouse-down inside selection must not replace state");
    assert!(matches!(view.drag, DragState::MaybeDraggingSelection { .. }));
    // Selection is unchanged.
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (6, 11));

    // 2. Drag to byte 0 — well past the 4px threshold.
    let drag = MouseEvent::Drag { x: x_for_col(&view, 0), y, button: MouseButton::Left };
    let act = handle_mouse_with_mods(&state, &mut view, &drag, mods);
    apply(act, &mut state);
    match view.drag {
        DragState::DraggingSelection { drop_caret } => assert_eq!(drop_caret, 0),
        other => panic!("expected DraggingSelection, got {:?}", other),
    }

    // 3. Mouse up at byte 0 — commit the move.
    let up = MouseEvent::Up { button: MouseButton::Left, x: x_for_col(&view, 0), y };
    let act = handle_mouse_with_mods(&state, &mut view, &up, mods);
    apply(act, &mut state);
    assert_eq!(view.drag, DragState::Idle);

    assert_eq!(state.doc.to_string(), "worldhello ");
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (0, 5));
}

#[test]
fn drag_drop_inside_original_range_cancels() {
    // Press inside selection, drag a few pixels, release still inside the
    // original range -> no state change.
    let state = state_with_selection("hello world", 6..11);
    let mut view = make_view();
    view.sync_to(&state);
    let y = 4.0;
    let mods = Modifiers::default();

    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 8), y, click_count: 1 };
    let _ = handle_mouse_with_mods(&state, &mut view, &down, mods);
    // Move enough to enter DraggingSelection but stay inside the selection
    // (byte 9 is still inside 6..11).
    let drag = MouseEvent::Drag { x: x_for_col(&view, 9) + 10.0, y, button: MouseButton::Left };
    let _ = handle_mouse_with_mods(&state, &mut view, &drag, mods);
    assert!(matches!(view.drag, DragState::DraggingSelection { .. }));
    let up = MouseEvent::Up { button: MouseButton::Left, x: x_for_col(&view, 9), y };
    let act = handle_mouse_with_mods(&state, &mut view, &up, mods);
    assert!(matches!(act, Action::None));
    assert_eq!(state.doc.to_string(), "hello world");
    assert_eq!(view.drag, DragState::Idle);
}

#[test]
fn mouse_down_outside_selection_still_moves_caret() {
    // Pressing outside an existing selection collapses to a single caret
    // and arms a MaybeSelecting drag, as before.
    let mut state = state_with_selection("hello world", 6..11);
    let mut view = make_view();
    view.sync_to(&state);
    let down = MouseEvent::Down { button: MouseButton::Left, x: x_for_col(&view, 2), y: 4.0, click_count: 1 };
    let act = handle_mouse_with_mods(&state, &mut view, &down, Modifiers::default());
    apply(act, &mut state);
    assert!(matches!(view.drag, DragState::MaybeSelecting { lo: 2, hi: 2 }));
    let sel = state.selection.main();
    assert_eq!((sel.start(), sel.end()), (2, 2));
}
