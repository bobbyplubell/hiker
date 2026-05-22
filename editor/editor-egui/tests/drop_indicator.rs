//! Drop-cursor indicator during text drag-and-drop. SPEC §9.19 /
//! IMPLEMENTATION §16.6.11.
//!
//! The painter renders a thin vertical caret at `drop_caret` while the
//! view is in `DragState::DraggingSelection`. Visual position is hard to
//! assert headlessly; this test exercises the code path and confirms no
//! panic / no harness failure when the state is active.

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::DragState;
use editor_view::viewport::ViewState;
#[test]
fn drop_indicator_renders_without_panic() {
    let mut state = EditorState::new("hello world\nsecond line\n");
    // Simulate an in-progress text drag with the drop caret at byte 5.
    let mut view = ViewState {
        drag: DragState::DraggingSelection { drop_caret: 5 },
        ..ViewState::default()
    };

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(400.0, 200.0))
        .build_ui(|ui| {
            EditorWidget::new(&mut state, &mut view).show(ui);
        });
    harness.run();
}
