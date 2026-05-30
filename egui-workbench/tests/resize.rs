//! Layout invariants for the side bars. Locks in the regressions we
//! hit while building the chat side bar: resize snap-back from content
//! inflation, mirrored width drift on idle frames, etc.

use egui_workbench::tab::Document;

use egui_workbench::side_bar::Side;

use egui_workbench::tab::UiContext;

use egui_workbench::workspace::Workbench;

use egui_workbench::behavior::Host;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TestTab;

impl Document for TestTab {
    fn title(&self) -> egui::WidgetText {
        "tab".into()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
struct TestMode;

/// Inflatable side-bar content: a fixed-width 400px label, wider than
/// the default 260 side bar. Reproduces the snap-back source.
struct WideBehavior;

impl Host<TestTab, TestMode> for WideBehavior {
    fn pane_ui(&mut self, _ui: &mut egui::Ui, _tab: &mut TestTab, _ctx: UiContext<'_>) {}

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, _mode: &TestMode) {
        ui.add_sized([400.0, 20.0], egui::Label::new("wide content"));
    }

    fn secondary_side_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_sized([400.0, 20.0], egui::Label::new("wide content"));
    }

    fn activity_items(&self) -> Vec<egui_workbench::activity_bar::Item<TestMode>> {
        vec![egui_workbench::activity_bar::Item {
            label: "Mode".into(),
            icon: None,
            mode: TestMode,
            badge: None,
        }]
    }
}

fn build_harness(
    size: egui::Vec2,
) -> egui_kittest::Harness<'static, Workbench<TestTab, TestMode>> {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    // Open the panel into the splittable primary region (the activity-bar
    // highlight follows the focused pane). [feature-multi-region-sidebar]
    wb.open_primary_panel(TestMode);
    wb.secondary_side_bar.visible = true;
    egui_kittest::Harness::builder()
        .with_size(size)
        .build_state(
            |ctx, wb| {
                wb.ui(ctx, &mut WideBehavior);
            },
            wb,
        )
}

#[test]
fn default_widths_match_sidebar_defaults() {
    let mut harness = build_harness(egui::vec2(1200.0, 800.0));
    harness.run();

    let wb = harness.state();
    assert_eq!(wb.primary_side_bar.width, 260.0);
    assert_eq!(wb.secondary_side_bar.width, 260.0);
    assert_eq!(wb.primary_side_bar.side, Side::Left);
    assert_eq!(wb.secondary_side_bar.side, Side::Right);
}

#[test]
fn idle_frames_do_not_change_width() {
    let mut harness = build_harness(egui::vec2(1200.0, 800.0));
    for _ in 0..10 {
        harness.run();
    }

    let wb = harness.state();
    // Without the body-rect cursor pin in `side_bar.rs`, the 400px
    // label would inflate the side bars across frames.
    assert!(
        wb.primary_side_bar.width <= 260.0 + 0.5,
        "primary drifted: {}",
        wb.primary_side_bar.width,
    );
    assert!(
        wb.secondary_side_bar.width <= 260.0 + 0.5,
        "secondary drifted: {}",
        wb.secondary_side_bar.width,
    );
}

/// egui SidePanel stores its current width in a `PanelState` keyed by
/// the panel id. To simulate "user dragged to width W" we write
/// directly into that store; egui will read it back on the next frame.
fn force_panel_state_width<S>(
    harness: &mut egui_kittest::Harness<'_, S>,
    panel_id: &str,
    width: f32,
) {
    let id = egui::Id::new(panel_id);
    harness.ctx.data_mut(|d| {
        d.insert_persisted(
            id,
            egui::containers::panel::PanelState {
                rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 0.0)),
            },
        );
    });
}

#[test]
fn narrow_width_holds_against_inflatable_content() {
    // Models the snap-back bug. Set egui's stored width below the
    // chat-style wide content's natural min and assert the panel
    // doesn't bounce back to fit the content over the next 20 frames.
    let mut harness = build_harness(egui::vec2(1200.0, 800.0));
    harness.run();
    force_panel_state_width(&mut harness, "egui_workbench::primary_side_bar", 100.0);
    force_panel_state_width(&mut harness, "egui_workbench::secondary_side_bar", 100.0);

    for _ in 0..20 {
        harness.run();
    }

    let wb = harness.state();
    assert!(
        (wb.secondary_side_bar.width - 100.0).abs() < 1.0,
        "secondary snapped back to {} (expected ~100)",
        wb.secondary_side_bar.width,
    );
    assert!(
        (wb.primary_side_bar.width - 100.0).abs() < 1.0,
        "primary snapped back to {} (expected ~100)",
        wb.primary_side_bar.width,
    );
}

#[test]
fn min_width_floor_sticks() {
    let mut harness = build_harness(egui::vec2(1200.0, 800.0));
    harness.run();
    let min = harness.state().secondary_side_bar.min_width;
    force_panel_state_width(&mut harness, "egui_workbench::secondary_side_bar", min);

    for _ in 0..20 {
        harness.run();
    }

    let wb = harness.state();
    assert!(
        (wb.secondary_side_bar.width - min).abs() < 1.0,
        "min-width panel inflated to {} (min is {})",
        wb.secondary_side_bar.width,
        min,
    );
}

#[test]
fn out_of_range_width_is_clamped() {
    let mut harness = build_harness(egui::vec2(1200.0, 800.0));
    harness.run();

    force_panel_state_width(&mut harness, "egui_workbench::secondary_side_bar", 10.0);
    harness.run();
    harness.run();
    let min = harness.state().secondary_side_bar.min_width;
    assert!(
        harness.state().secondary_side_bar.width >= min - 0.5,
        "below-min width survived: {} (min {})",
        harness.state().secondary_side_bar.width,
        min,
    );

    force_panel_state_width(&mut harness, "egui_workbench::secondary_side_bar", 5000.0);
    harness.run();
    harness.run();
    let max = harness.state().secondary_side_bar.max_width;
    assert!(
        harness.state().secondary_side_bar.width <= max + 0.5,
        "above-max width survived: {} (max {})",
        harness.state().secondary_side_bar.width,
        max,
    );
}
