//! Demo binary for `egui_workbench`.
//!
//! Showcases all the v0.1 building blocks: activity bar, side bar,
//! editor area with Preview/Pinned/Modified tab states, bottom panel
//! area with its own tabs, status bar with host-rendered cells, and
//! layout serialise/deserialise to a JSON file on disk.

use eframe::egui;
use egui_workbench::activity_bar::ActivityBadge;
use egui_workbench::activity_bar::Item;
use egui_workbench::tab::Document;
use egui_workbench::workspace::OpenTabOptions;
use egui_workbench::tab::State;
use egui_workbench::tab::UiContext;
use egui_workbench::workspace::Workbench;
use egui_workbench::behavior::Host;
use serde::{Deserialize, Serialize};

const LAYOUT_PATH: &str = "/tmp/workbench-demo-layout.json";

fn main() -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("egui_workbench demo")
        .with_inner_size([1400.0, 900.0]);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "workbench-demo",
        options,
        Box::new(|_cc| Ok(Box::new(DemoApp::new()))),
    )
}

struct DemoApp {
    workbench: Workbench<DemoTab, DemoMode>,
    status: String,
}

impl Default for DemoApp {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoApp {
    fn new() -> Self {
        let mut workbench = Workbench::<DemoTab, DemoMode>::new();
        workbench.activity_bar.set_active(Some(DemoMode::Explorer));

        // Pinned tab (sorts leftmost; survives Close Others).
        let pinned = workbench.open_tab(
            DemoTab::Doc {
                title: "pinned.md".into(),
                body: "This tab is pinned.".into(),
                dirty: false,
            },
            &OpenTabOptions::default(),
        );
        workbench.pin_tab(pinned, true);

        // Regular tabs.
        workbench.open_tab(DemoTab::Welcome, &OpenTabOptions::default());
        workbench.open_tab(
            DemoTab::Doc {
                title: "notes.md".into(),
                body: "# notes\n\nHello from egui_workbench.".into(),
                dirty: false,
            },
            &OpenTabOptions::default(),
        );

        // Preview tab — opening another Preview will replace this one.
        workbench.open_tab(
            DemoTab::Doc {
                title: "preview.md".into(),
                body: "Preview tab — italic title; replaced by next preview.".into(),
                dirty: false,
            },
            &OpenTabOptions {
                state: State::Preview,
                ..OpenTabOptions::default()
            },
        );

        // Seed panel area with two tool tabs.
        workbench.open_panel_tab(
            DemoTab::Terminal,
            &OpenTabOptions::default(),
        );
        workbench.open_panel_tab(
            DemoTab::Output,
            &OpenTabOptions::default(),
        );
        // Start with panel area visible to make the feature discoverable.
        workbench.panel_area.visible = true;

        Self {
            workbench,
            status: "Ready".into(),
        }
    }
}

impl eframe::App for DemoApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Top toolbar with global demo actions.
        egui::TopBottomPanel::top("demo_toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Toggle panel").clicked() {
                    self.workbench.toggle_panel_area();
                }
                if ui.button("Toggle side bar").clicked() {
                    self.workbench.toggle_primary_side_bar();
                }
                if ui.button("Open preview tab").clicked() {
                    use std::time::{SystemTime, UNIX_EPOCH};
                    let suffix: u32 = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0);
                    self.workbench.open_tab(
                        DemoTab::Doc {
                            title: format!("preview-{suffix}.md"),
                            body: "Another preview tab — italic title.".into(),
                            dirty: false,
                        },
                        &OpenTabOptions {
                            state: State::Preview,
                            ..OpenTabOptions::default()
                        },
                    );
                }
                if ui.button("Save layout").clicked() {
                    let layout = self.workbench.layout();
                    match serde_json::to_string_pretty(&layout) {
                        Ok(json) => match std::fs::write(LAYOUT_PATH, json) {
                            Ok(()) => self.status = format!("Saved layout to {LAYOUT_PATH}"),
                            Err(e) => self.status = format!("Save failed: {e}"),
                        },
                        Err(e) => self.status = format!("Serialise failed: {e}"),
                    }
                }
                if ui.button("Load layout").clicked() {
                    match std::fs::read_to_string(LAYOUT_PATH) {
                        Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
                            Ok(v) => match egui_workbench::persistence::migrate(v) {
                                Some(layout) => match self.workbench.apply_layout(layout) {
                                    Ok(()) => self.status = "Loaded layout".into(),
                                    Err(e) => self.status = format!("Apply failed: {e}"),
                                },
                                None => self.status = "Unknown schema version".into(),
                            },
                            Err(e) => self.status = format!("Parse failed: {e}"),
                        },
                        Err(e) => self.status = format!("Read failed: {e}"),
                    }
                }
            });
        });

        let mut behavior = DemoBehavior {
            status: self.status.clone(),
        };
        self.workbench.ui(ctx, &mut behavior);
    }
}

#[derive(Clone, Serialize, Deserialize)]
enum DemoTab {
    Doc {
        title: String,
        body: String,
        dirty: bool,
    },
    Welcome,
    Terminal,
    Output,
}

impl Document for DemoTab {
    fn title(&self) -> egui::WidgetText {
        match self {
            DemoTab::Doc { title, .. } => title.clone().into(),
            DemoTab::Welcome => "Welcome".into(),
            DemoTab::Terminal => "Terminal".into(),
            DemoTab::Output => "Output".into(),
        }
    }
    fn is_dirty(&self) -> bool {
        matches!(self, DemoTab::Doc { dirty: true, .. })
    }
    fn tooltip(&self) -> Option<String> {
        match self {
            DemoTab::Doc { title, .. } => Some(format!("Document: {title}")),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum DemoMode {
    Explorer,
    Search,
    Scm,
    Debug,
}

impl DemoMode {
    const fn label(&self) -> &'static str {
        match self {
            DemoMode::Explorer => "Explorer",
            DemoMode::Search => "Search",
            DemoMode::Scm => "Source Control",
            DemoMode::Debug => "Run and Debug",
        }
    }
}

struct DemoBehavior {
    status: String,
}

impl Host<DemoTab, DemoMode> for DemoBehavior {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut DemoTab, _ctx: UiContext<'_>) {
        match tab {
            DemoTab::Doc { body, dirty, .. } => {
                let resp = ui.text_edit_multiline(body);
                if resp.changed() {
                    *dirty = true;
                }
            }
            DemoTab::Welcome => {
                ui.heading("Welcome to egui_workbench");
                ui.label(
                    "Open the demo toolbar at the top to exercise Preview tabs, \
                    pinned tabs, panel area, and layout save/load.",
                );
            }
            DemoTab::Terminal => {
                ui.label("(Pretend this is a terminal.)");
            }
            DemoTab::Output => {
                ui.label("(Pretend this is the Output panel.)");
            }
        }
    }

    fn tab_context_menu(&mut self, ui: &mut egui::Ui, _tab: &DemoTab) {
        if ui.button("Demo: log").clicked() {
            eprintln!("demo: tab context menu clicked");
            ui.close();
        }
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &DemoMode) {
        ui.label(format!("{} placeholder", mode.label()));
    }

    fn side_bar_title(&self, mode: &DemoMode) -> egui::WidgetText {
        mode.label().into()
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label("Ln 1, Col 1");
                ui.separator();
                ui.label("UTF-8");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(&self.status);
                ui.separator();
                ui.label("workbench-demo");
            });
        });
    }

    fn activity_items(&self) -> Vec<Item<DemoMode>> {
        vec![
            Item {
                mode: DemoMode::Explorer,
                icon: None,
                label: "Explorer".into(),
                badge: None,
            },
            Item {
                mode: DemoMode::Search,
                icon: None,
                label: "Search".into(),
                badge: Some(ActivityBadge::Dot),
            },
            Item {
                mode: DemoMode::Scm,
                icon: None,
                label: "Source Control".into(),
                badge: Some(ActivityBadge::Count(3)),
            },
            Item {
                mode: DemoMode::Debug,
                icon: None,
                label: "Debug".into(),
                badge: None,
            },
        ]
    }
}
