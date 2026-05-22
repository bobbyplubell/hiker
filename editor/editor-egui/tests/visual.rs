//! Headless tests via `egui_kittest`. We don't snapshot pixels (cross-machine
//! font / GPU divergence is its own can of worms); we drive the widget and
//! assert on observable state — height map, decoration layers, click sink.

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_md::folds::fold_decorations;
use editor_md::folds::fold_regions;
use editor_md::styling::markdown_decorations;
use editor_view::viewport::ClickAction;
use editor_view::viewport::ViewState;
const fn harness_size() -> egui::Vec2 {
    egui::vec2(800.0, 600.0)
}

#[test]
fn empty_editor_renders_without_panic() {
    let mut state = EditorState::new("hello\nworld\n");
    let mut view = ViewState::default();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }
    assert!(view.line_height > 0.0);
}

#[test]
fn markdown_fold_chevron_click_toggles_state() {
    let mut state = EditorState::new("# Title\nbody1\nbody2\n");
    let mut view = ViewState::default();
    let folds: std::collections::HashSet<u64> = Default::default();
    let mut clicks: Vec<ClickAction> = Vec::new();
    let target_id = fold_regions(&state).into_iter().next().expect("fold region").id;

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                view.decorations.clear();
                view.decorations.push_with_heights(markdown_decorations(&state, None));
                view.decorations.push_with_heights(fold_decorations(&state, &folds));
                EditorWidget::new(&mut state, &mut view)
                    .with_click_sink(&mut clicks)
                    .show(ui);
            });
        harness.run();

        // Click the chevron column (x≈[0,18], y on the first line).
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: egui::pos2(8.0, 10.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        });
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: egui::pos2(8.0, 10.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        });
        harness.run();
    }

    let toggled = clicks
        .iter()
        .any(|a| matches!(a, ClickAction::ToggleFold(id) if *id == target_id));
    assert!(toggled, "expected ToggleFold({target_id}); got {clicks:?}");
}

#[test]
fn collapsed_fold_hides_body_lines_in_height_map() {
    let mut state = EditorState::new("# T\nbody1\nbody2\nbody3\n");
    let mut view = ViewState::default();
    let id = fold_regions(&state)[0].id;
    let mut folds: std::collections::HashSet<u64> = Default::default();
    folds.insert(id);

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                view.decorations.clear();
                view.decorations.push_with_heights(fold_decorations(&state, &folds));
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
        harness.run();
    }

    for line in 1..4 {
        assert_eq!(
            view.height_map.text_height(line),
            0.0,
            "line {line} should be hidden"
        );
    }
    assert!(view.height_map.text_height(0) > 0.0);
}
