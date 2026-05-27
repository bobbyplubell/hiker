//! Soft-wrap integration tests.

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

const fn long_line() -> &'static str {
    // ~150 chars of one buffer line. Wrap at ~80px wide widget = several VLines.
    "the quick brown fox jumps over the lazy dog the quick brown fox jumps over the lazy dog the quick brown fox jumps over the lazy dog\n"
}

#[test]
fn wrap_off_gives_single_vline_per_buffer_line() {
    let mut state = EditorState::new(long_line());
    let mut view = ViewState::default();
    // wrap disabled by default
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    // Without wrap, the wrap map should report 1 visual line (or be empty —
    // either is fine since the painter wouldn't iterate vlines).
    if let Some(w) = view.wrap_map.peek(0) {
        assert_eq!(w.visual_count(), 1, "wrap-off: should be 1 vline, got {}", w.visual_count());
    }
}

#[test]
fn wrap_on_produces_multiple_vlines_for_long_line() {
    let mut state = EditorState::new(long_line());
    let mut view = ViewState::default();
    view.wrap_map.set_enabled(true);

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 200.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    let w = view.wrap_map.peek(0).expect("wrap entry for line 0");
    assert!(w.visual_count() > 1, "long line should wrap: got {} vlines", w.visual_count());
}

/// Regression: pressing Enter while wrap is on emptied/shifted a visible line
/// in the same frame as paint. `measure()` took its stale (edit-frame) early
/// return, which skipped re-wrapping, so the painter sliced the now-empty line
/// text with the previous frame's longer vline range and panicked with
/// "end byte index N is out of bounds of ``". The fix keeps the wrap cache in
/// sync every frame, before that early return.
#[test]
fn enter_in_wrapped_list_does_not_panic() {
    // A list item; line 0 is 8 bytes ("- abcdef"), line 1 empty.
    let mut state = EditorState::new("- abcdef\n");
    let mut view = ViewState::default();
    view.wrap_map.set_enabled(true);

    {
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(|ui| {
            EditorWidget::new(&mut state, &mut view).show(ui);
        });
    // Frame 1: click near the start of line 0 to grant focus and put the caret
    // a few bytes in, while caching line 0's vline range against the full
    // 8-byte text. Splitting anywhere before byte 8 (next frame) leaves line 0
    // shorter than that cached range.
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: egui::pos2(70.0, 10.0),
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: egui::pos2(70.0, 10.0),
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    harness.run();

    // Frame 2: Enter is applied during this frame's input handling, so it is an
    // "edit frame" (pre_doc_id != doc_id) — exactly the path that panicked.
    harness.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    harness.input_mut().events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: false,
        repeat: false,
        modifiers: Default::default(),
    });
    harness.run();
    }

    // Reaching here without a panic is the assertion; sanity-check the split.
    assert_eq!(state.doc.len_lines(), 3, "Enter should have added a line");
}

#[test]
fn wrap_reflows_when_width_changes() {
    let mut state = EditorState::new(long_line());
    let mut view = ViewState::default();
    view.wrap_map.set_enabled(true);

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(300.0, 200.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    let initial_vlines = view.wrap_map.peek(0).map(editor_view::wrapping::WrappedLine::visual_count).unwrap_or(1);
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 200.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    let after = view.wrap_map.peek(0).map(editor_view::wrapping::WrappedLine::visual_count).unwrap_or(1);
    assert!(
        after < initial_vlines,
        "wider widget should reduce vline count: {initial_vlines} → {after}"
    );
}
