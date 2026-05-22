//! Smoke tests for `egui_workbench`. Mirrors the kittest pattern used
//! in hiker's `app/src/smoke_tests.rs`: build a workbench, drive it for
//! a few frames in a headless harness, assert no panic.

use egui_workbench::tab::Document;

use egui_workbench::workspace::OpenTabOptions;

use egui_workbench::tab::State;

use egui_workbench::tab::UiContext;

use egui_workbench::workspace::Workbench;

use egui_workbench::behavior::Host;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TestTab {
    title: String,
}

impl Document for TestTab {
    fn title(&self) -> egui::WidgetText {
        self.title.clone().into()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum TestMode {
    Files,
}

struct TestBehavior;

impl Host<TestTab, TestMode> for TestBehavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut TestTab, _ctx: UiContext<'_>) {
        ui.label(&tab.title);
    }
}

#[test]
fn workbench_ui_runs_clean_for_three_frames() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    wb.open_tab(
        TestTab { title: "alpha".into() },
        &OpenTabOptions::default(),
    );
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .build(|ctx: &egui::Context| {
            let mut beh = TestBehavior;
            wb.ui(ctx, &mut beh);
        });
    for _ in 0..3 {
        harness.run();
    }
}

#[test]
fn pinned_tabs_stay_leftmost() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    let pinned = wb.open_tab(
        TestTab { title: "pinned".into() },
        &OpenTabOptions {
            state: State::Pinned,
            ..OpenTabOptions::default()
        },
    );
    let _r1 = wb.open_tab(
        TestTab { title: "reg1".into() },
        &OpenTabOptions::default(),
    );
    let _r2 = wb.open_tab(
        TestTab { title: "reg2".into() },
        &OpenTabOptions::default(),
    );

    // Run one frame so the enforce_pinned_first pass executes, then
    // drop the harness so the workbench borrow is released.
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build(|ctx: &egui::Context| {
                let mut beh = TestBehavior;
                wb.ui(ctx, &mut beh);
            });
        harness.run();
    }

    // First child in every Tabs container should be the pinned handle.
    let leading = wb.editor_area.leading_handle_per_group();
    assert!(!leading.is_empty(), "expected at least one Tabs container");
    for h in leading {
        assert_eq!(h, pinned, "pinned tab should sort first");
    }
}

#[test]
fn preview_replacement_keeps_only_latest() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    let a = wb.open_tab(
        TestTab { title: "A".into() },
        &OpenTabOptions {
            state: State::Preview,
            ..OpenTabOptions::default()
        },
    );
    let b = wb.open_tab(
        TestTab { title: "B".into() },
        &OpenTabOptions {
            state: State::Preview,
            ..OpenTabOptions::default()
        },
    );
    // Tab A should be gone; B remains.
    let handles: Vec<_> = wb.iter_tabs().map(|(h, _)| h).collect();
    assert!(!handles.contains(&a), "preview A should have been replaced");
    assert!(handles.contains(&b), "preview B should remain");
}

#[test]
fn workbench_with_pinned_and_preview_tabs_runs_clean() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    wb.open_tab(
        TestTab { title: "pinned".into() },
        &OpenTabOptions { state: State::Pinned, ..OpenTabOptions::default() },
    );
    wb.open_tab(
        TestTab { title: "regular".into() },
        &OpenTabOptions::default(),
    );
    wb.open_tab(
        TestTab { title: "preview".into() },
        &OpenTabOptions { state: State::Preview, ..OpenTabOptions::default() },
    );
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .build(|ctx: &egui::Context| {
            let mut beh = TestBehavior;
            wb.ui(ctx, &mut beh);
        });
    for _ in 0..3 {
        harness.run();
    }
}

#[test]
fn panel_area_visible_and_invisible_runs_clean() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    wb.open_panel_tab(
        TestTab { title: "term".into() },
        &OpenTabOptions::default(),
    );
    wb.open_panel_tab(
        TestTab { title: "out".into() },
        &OpenTabOptions::default(),
    );
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build(|ctx: &egui::Context| {
                let mut beh = TestBehavior;
                wb.ui(ctx, &mut beh);
            });
        for _ in 0..3 {
            harness.run();
        }
    }
    wb.panel_area.visible = false;
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 800.0))
            .build(|ctx: &egui::Context| {
                let mut beh = TestBehavior;
                wb.ui(ctx, &mut beh);
            });
        for _ in 0..3 {
            harness.run();
        }
    }
}

#[test]
fn activity_bar_with_no_items_runs_clean() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    // No tabs, no activity items (TestBehavior returns the default
    // empty Vec). The activity bar should still render its chrome
    // without panic.
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .build(|ctx: &egui::Context| {
            let mut beh = TestBehavior;
            wb.ui(ctx, &mut beh);
        });
    for _ in 0..3 {
        harness.run();
    }
}

#[test]
fn close_others_skips_pinned() {
    let mut wb = Workbench::<TestTab, TestMode>::new();
    let pinned = wb.open_tab(
        TestTab { title: "pinned".into() },
        &OpenTabOptions {
            state: State::Pinned,
            ..OpenTabOptions::default()
        },
    );
    let keep = wb.open_tab(
        TestTab { title: "keep".into() },
        &OpenTabOptions::default(),
    );
    let _drop1 = wb.open_tab(
        TestTab { title: "drop1".into() },
        &OpenTabOptions::default(),
    );
    let _drop2 = wb.open_tab(
        TestTab { title: "drop2".into() },
        &OpenTabOptions::default(),
    );

    // Need to render one frame so the tree's Tabs container is populated
    // (insert_tab adds via direct mutation, so children are present
    // immediately — but find_group_of walks the tree, which works either
    // way). No frame strictly required here.
    wb.close_others(keep);

    let handles: Vec<_> = wb.iter_tabs().map(|(h, _)| h).collect();
    assert!(handles.contains(&pinned), "pinned tab must survive close_others");
    assert!(handles.contains(&keep), "the exception tab must survive");
    assert_eq!(handles.len(), 2);
}
