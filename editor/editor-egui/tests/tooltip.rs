//! Tooltip primitive integration test. Drives the widget through two frames
//! with a tooltip queued in `view.tooltips`, asserts that the harness does
//! not panic and that the list is still present after rendering (the widget
//! is read-only w.r.t. tooltips; the host owns the slot).

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::popup::Tooltip;
use editor_view::popup::TooltipAnchor;
use editor_view::popup::TooltipContent;
use editor_view::popup::TooltipPlacement;
use editor_view::viewport::ViewState;
use smol_str::SmolStr;

#[test]
fn tooltip_paints_without_panic() {
    let mut state = EditorState::new("hello\nworld\nfoo bar baz\n");
    let mut view = ViewState::default();
    view.tooltips.push(Tooltip {
        id: 42,
        anchor: TooltipAnchor::BufferPos { byte: 6 }, // start of "world"
        placement: TooltipPlacement::Smart,
        content: TooltipContent::Text(SmolStr::new_static("hint: this is a tooltip")),
    });
    view.tooltips.push(Tooltip {
        id: 43,
        anchor: TooltipAnchor::Coords { x: 100.0, y: 40.0 },
        placement: TooltipPlacement::Above,
        content: TooltipContent::Markdown(SmolStr::new_static("**bold** (rendered as text)")),
    });

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
        harness.run();
    }

    // The widget should not have mutated tooltips — the host owns the list.
    assert_eq!(view.tooltips.len(), 2, "tooltip list preserved after frames");
    assert!(view.line_height > 0.0);
}
