//! Visual snapshot tests via `egui_kittest`'s snapshot feature.
//!
//! These tests are `#[ignore]`d by default because they require a
//! baseline PNG on disk under `tests/snapshots/`. The recipe:
//!
//! 1. First-time generation:
//!    `UPDATE_SNAPSHOTS=1 cargo test -p egui_workbench --test snapshots -- --ignored`
//! 2. Visually review the generated `.png` files, then commit them.
//! 3. CI runs `cargo test -p egui_workbench --test snapshots -- --ignored`
//!    to diff against the committed baselines.

use egui_workbench::tab::Document;

use egui_workbench::workspace::OpenTabOptions;

use egui_workbench::tab::UiContext;

use egui_workbench::workspace::Workbench;

use egui_workbench::behavior::Host;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SnapTab {
    title: String,
}

impl Document for SnapTab {
    fn title(&self) -> egui::WidgetText {
        self.title.clone().into()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum SnapMode {
    Files,
}

struct SnapBehavior;

impl Host<SnapTab, SnapMode> for SnapBehavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut SnapTab, _ctx: UiContext<'_>) {
        ui.label(&tab.title);
    }
    fn side_bar_ui(&mut self, ui: &mut egui::Ui, _mode: &SnapMode) {
        ui.label("Files");
    }
    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("ready");
    }
}

fn run_and_snapshot(name: &str, build: impl FnOnce(&mut Workbench<SnapTab, SnapMode>)) {
    let mut wb = Workbench::<SnapTab, SnapMode>::new();
    build(&mut wb);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build(|ctx: &egui::Context| {
            let mut beh = SnapBehavior;
            wb.ui(ctx, &mut beh);
        });
    harness.run();
    harness.snapshot(name);
}

#[test]
#[ignore = "snapshot tests require baseline PNGs; run with --ignored after UPDATE_SNAPSHOTS=1"]
fn default_layout_snapshot() {
    run_and_snapshot("default_layout", |_| {});
}

#[test]
#[ignore = "snapshot tests require baseline PNGs; run with --ignored after UPDATE_SNAPSHOTS=1"]
fn with_side_bars_open_snapshot() {
    run_and_snapshot("with_side_bars_open", |wb| {
        wb.primary_side_bar.visible = true;
        wb.secondary_side_bar.visible = true;
        wb.activity_bar.set_active(Some(SnapMode::Files));
    });
}

#[test]
#[ignore = "snapshot tests require baseline PNGs; run with --ignored after UPDATE_SNAPSHOTS=1"]
fn with_panel_area_open_snapshot() {
    run_and_snapshot("with_panel_area_open", |wb| {
        wb.open_panel_tab(
            SnapTab { title: "Terminal".into() },
            &OpenTabOptions::default(),
        );
        wb.open_panel_tab(
            SnapTab { title: "Output".into() },
            &OpenTabOptions::default(),
        );
    });
}
