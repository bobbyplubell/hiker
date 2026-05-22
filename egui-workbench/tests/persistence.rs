//! Persistence round-trip tests for `WorkbenchLayout`.

use egui_workbench::tab::Document;

use egui_workbench::workspace::OpenTabOptions;

use egui_workbench::tab::State;

use egui_workbench::tab::UiContext;

use egui_workbench::workspace::Workbench;

use egui_workbench::behavior::Host;

use egui_workbench::persistence::migrate;
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
struct PTab {
    title: String,
}

impl Document for PTab {
    fn title(&self) -> egui::WidgetText {
        self.title.clone().into()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum PMode {
    Files,
    Search,
}

#[allow(dead_code)]
struct PBehavior;
impl Host<PTab, PMode> for PBehavior {
    fn pane_ui(&mut self, _ui: &mut egui::Ui, _tab: &mut PTab, _ctx: UiContext<'_>) {}
}

#[test]
fn layout_round_trip_preserves_tabs() {
    let mut wb = Workbench::<PTab, PMode>::new();
    wb.activity_bar.set_active(Some(PMode::Files));
    let h1 = wb.open_tab(
        PTab { title: "alpha".into() },
        &OpenTabOptions::default(),
    );
    let h2 = wb.open_tab(
        PTab { title: "beta".into() },
        &OpenTabOptions {
            state: State::Pinned,
            ..OpenTabOptions::default()
        },
    );
    wb.open_panel_tab(PTab { title: "term".into() }, &OpenTabOptions::default());

    let layout = wb.layout();
    let json = serde_json::to_string(&layout).expect("serialise");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let layout2 = migrate(parsed).expect("migrate v1");

    let mut wb2 = Workbench::<PTab, PMode>::new();
    wb2.apply_layout(layout2).expect("apply layout");

    // Editor tabs preserved (including handles).
    let mut editor_handles: Vec<_> = wb2.iter_tabs().map(|(h, _)| h).collect();
    editor_handles.sort();
    let mut expected = vec![h1, h2];
    expected.sort();
    assert_eq!(editor_handles, expected);

    // Pinned state survived.
    assert_eq!(
        wb2.editor_area.state(h2),
        Some(State::Pinned),
        "pinned state must round-trip",
    );

    // Panel tab is also restored.
    assert_eq!(wb2.panel_area.tab_count(), 1);
}

#[test]
fn unknown_schema_version_returns_none() {
    let v = serde_json::json!({ "version": 999 });
    assert!(migrate(v).is_none());
}

#[test]
fn missing_version_returns_none() {
    let v = serde_json::json!({ "primary_side": "Left" });
    assert!(migrate(v).is_none());
}
