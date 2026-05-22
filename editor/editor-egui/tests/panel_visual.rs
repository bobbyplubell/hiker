//! Visual integration tests for the panel framework + search panel UI.
//! SPEC §9.21 + §9.13.

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::panels::Panel;
use editor_view::panels::PanelKind;
use editor_view::panels::PanelPlacement;
use editor_view::viewport::ViewState;
const fn harness_size() -> egui::Vec2 {
    egui::vec2(800.0, 600.0)
}

#[test]
fn opening_search_registers_panel_and_persists() {
    let mut state = EditorState::new("hello world\nhello again\n");
    let mut view = ViewState::default();
    view.search.open();
    view.search.set_query("hello");

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
        harness.run();
    }

    assert!(view.search.active, "search should remain active");
    let has_search_panel = view
        .panels
        .panels
        .iter()
        .any(|p| matches!(p.kind, PanelKind::Search));
    assert!(has_search_panel, "search panel should have been auto-registered");
    assert!(
        !view.search.matches.is_empty(),
        "expected matches for 'hello' but got none"
    );
}

#[test]
fn closing_search_removes_panel() {
    let mut state = EditorState::new("abc\n");
    let mut view = ViewState::default();
    view.search.open();

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }

    assert!(view
        .panels
        .panels
        .iter()
        .any(|p| matches!(p.kind, PanelKind::Search)));

    view.search.close();
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }

    let has_search_panel = view
        .panels
        .panels
        .iter()
        .any(|p| matches!(p.kind, PanelKind::Search));
    assert!(!has_search_panel, "search panel should be removed on close");
}

#[test]
fn label_panel_reserves_height_via_text_rect() {
    let mut state = EditorState::new("line\n");
    let mut view = ViewState::default();
    view.panels.panels.push(Panel {
        id: 42,
        placement: PanelPlacement::Bottom,
        height: 24.0,
        kind: PanelKind::Label("status".into()),
    });

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(harness_size())
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }

    // text area height = allocated widget height - bottom panel (24)
    // The kittest harness allocates the available rect (not exactly 600px tall),
    // so we just assert that the panel height was deducted.
    assert!(view.height > 0.0);
    assert!(view.width > 0.0);
    // Bottom panel is reserved: text rect bottom == widget bottom - 24, so
    // text height must be at most (harness height - 24).
    assert!(view.height <= 600.0 - 24.0 + 0.5, "view.height was {}", view.height);
}
