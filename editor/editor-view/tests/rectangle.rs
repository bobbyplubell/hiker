//! Rectangular (column) selection. SPEC §9.15 / IMPLEMENTATION §16.6.6.
//!
//! Alt+mouse-drag creates one SelRange per buffer line in the y-span of
//! the drag, each spanning the same horizontal x-range.

use editor_core::state::Editor as EditorState;
use editor_view::command::handle_mouse_with_mods;
use editor_view::command::Action;
use editor_view::events::Modifiers;
use editor_view::events::MouseButton;
use editor_view::events::MouseEvent;
use editor_view::viewport::DragState;
use editor_view::viewport::ViewState;
fn x_for_col(view: &ViewState, col: usize) -> f32 {
    // Mirror the x->byte mapper's fallback glyph width (`font_size * 0.6`)
    // used when no measured monospace width has been seeded on the wrap map.
    let char_w = view.font_size * 0.6;
    view.gutter_width + col as f32 * char_w
}

#[test]
fn alt_drag_creates_one_range_per_row_at_same_columns() {
    let text = "hello world\nhello world\nhello world\nhello world\nhello world\n";
    let mut state = EditorState::new(text);
    let mut view = ViewState {
        gutter_width: 0.0,
        font_size: 14.0,
        line_height: 18.0,
        width: 400.0,
        height: 200.0,
        ..ViewState::default()
    };
    view.sync_to(&state);

    // Down at column 5 on line 0 (y=0) with Alt held — start RectangleSelecting.
    let alt = Modifiers { ctrl: false, alt: true, shift: false, meta: false };
    let down = MouseEvent::Down {
        button: MouseButton::Left,
        x: x_for_col(&view, 5),
        y: 0.0,
        click_count: 1,
    };
    let act = handle_mouse_with_mods(&state, &mut view, &down, alt);
    if let Action::Replace { state: s, .. } = act {
        state = s;
    }
    assert!(matches!(view.drag, DragState::RectangleSelecting { .. }));

    // Drag down past several lines (line_height=18; y=60 ≈ line 3) and to col 9.
    let drag = MouseEvent::Drag {
        x: x_for_col(&view, 9),
        y: 60.0,
        button: MouseButton::Left,
    };
    let act = handle_mouse_with_mods(&state, &mut view, &drag, alt);
    if let Action::Replace { state: s, .. } = act {
        state = s;
    }

    let ranges = state.selection.ranges();
    assert!(ranges.len() >= 2, "expected multiple ranges, got {}", ranges.len());

    // Each range should span the same byte columns on its line: cols 5..9.
    for r in ranges {
        let line = state.doc.byte_to_line(r.start());
        let line_start = state.doc.line_to_byte(line);
        let local_start = r.start() - line_start;
        let local_end = r.end() - line_start;
        assert_eq!(local_start, 5, "range {:?} on line {} should start at col 5", r, line);
        assert_eq!(local_end, 9, "range {:?} on line {} should end at col 9", r, line);
    }
}
