//! Soft-wrap integration tests.

use editor_core::EditorState;
use editor_egui::EditorWidget;
use editor_view::ViewState;

fn long_line() -> &'static str {
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
    let initial_vlines = view.wrap_map.peek(0).map(|w| w.visual_count()).unwrap_or(1);
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 200.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    let after = view.wrap_map.peek(0).map(|w| w.visual_count()).unwrap_or(1);
    assert!(
        after < initial_vlines,
        "wider widget should reduce vline count: {initial_vlines} → {after}"
    );
}
