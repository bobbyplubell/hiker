//! Tests for widget event semantics (SPEC §3.10, IMPLEMENTATION §16.5.6).
//!
//! A custom `InlineWidget` decoration drawn over a single byte range must
//! reach the click sink as `ClickAction::WidgetClick(id)` when clicked.

use std::sync::Arc;

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_core::decoration::InlineWidget;

use editor_core::rangeset::RangeSet;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ClickAction;
use editor_view::viewport::ViewState;
/// Minimal widget impl used only by this test. Returns a stable id and reports
/// `handles_click() == true` so the painter registers a click zone.
struct TestWidget {
    id: u64,
}

impl InlineWidget for TestWidget {
    fn measure(&self, font_size: f32) -> (f32, f32) {
        // Roughly six monospace ems wide; tall enough to span one line.
        (font_size * 6.0, font_size)
    }
    fn handles_click(&self) -> bool {
        true
    }
    fn widget_id(&self) -> u64 {
        self.id
    }
}

#[test]
fn inline_widget_click_emits_widget_click_action() {
    let target_id: u64 = 0xC0FFEE;
    let mut state = EditorState::new("hello world\n");
    let mut view = ViewState::default();
    let mut clicks: Vec<ClickAction> = Vec::new();

    // Place the widget over bytes 0..5 ("hello").
    let widget: Arc<dyn InlineWidget> = Arc::new(TestWidget { id: target_id });
    let deco = Decoration::InlineWidget { widget, atomic: true };
    let set = RangeSet::from_iter(vec![(0..5, deco)]);

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_ui(|ui| {
                view.decorations.clear();
                view.decorations.push(set.clone());
                EditorWidget::new(&mut state, &mut view)
                    .with_click_sink(&mut clicks)
                    .show(ui);
            });
        harness.run();

        // The widget sits just past the gutter on the first line; click well
        // inside its bounds. Default gutter_width=56, font_size=14 so the
        // widget spans roughly x=56..56+(14*6)=140 on the first line.
        let click_x = 90.0;
        let click_y = 10.0;
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: egui::pos2(click_x, click_y),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: egui::pos2(click_x, click_y),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        });
        harness.run();
    }

    let saw_click = clicks
        .iter()
        .any(|a| matches!(a, ClickAction::WidgetClick(id) if *id == target_id));
    assert!(
        saw_click,
        "expected WidgetClick({target_id}); got {clicks:?}",
    );
}
