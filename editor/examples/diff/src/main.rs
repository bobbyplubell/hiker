use editor_core::diff::lines as diff_lines;
use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

const LEFT: &str = "fn greet(name: &str) {\n    println!(\"Hello, {name}!\");\n    println!(\"Welcome.\");\n    println!(\"How are you?\");\n}\n\nfn main() {\n    greet(\"world\");\n}\n";

const RIGHT: &str = "fn greet(name: &str) -> String {\n    format!(\"Hello, {name}! Welcome.\")\n}\n\nfn main() {\n    let g = greet(\"world\");\n    println!(\"{g}\");\n}\n";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Unified,
    SideBySide,
}

struct App {
    mode: Mode,
    left_state: EditorState,
    right_state: EditorState,
    left_view: ViewState,
    right_view: ViewState,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: Mode::Unified,
            left_state: EditorState::new(LEFT),
            right_state: EditorState::new(RIGHT),
            left_view: ViewState::default(),
            right_view: ViewState::default(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let left_text = self.left_state.doc.to_string();
        let right_text = self.right_state.doc.to_string();
        let hunks = diff_lines(&left_text, &right_text);
        let line_height = self.right_view.line_height.max(self.left_view.line_height).max(18.0);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.mode, Mode::Unified, "Unified");
                ui.radio_value(&mut self.mode, Mode::SideBySide, "Side-by-side");
                ui.separator();
                let active = hunks
                    .iter()
                    .filter(|h| !matches!(h.kind, editor_core::diff::HunkKind::Context))
                    .count();
                ui.label(format!("{active} hunks  ·  edit either pane to update live"));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.mode {
            Mode::Unified => {
                self.right_view.decorations.clear();
                self.right_view.decorations.push(editor_diff::view::unified_decorations(
                    &self.right_state.doc,
                    &left_text,
                    &hunks,
                    line_height,
                    None,
                ));
                EditorWidget::new(&mut self.right_state, &mut self.right_view).show(ui);
            }
            Mode::SideBySide => {
                let (left_set, right_set) = editor_diff::view::alignment_decorations(
                    &self.left_state.doc,
                    &self.right_state.doc,
                    &hunks,
                    line_height,
                    None,
                );
                self.left_view.decorations.clear();
                self.left_view.decorations.push(left_set);
                self.right_view.decorations.clear();
                self.right_view.decorations.push(right_set);

                let avail = ui.available_size();
                let half_w = (avail.x - 8.0) * 0.5;
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(half_w, avail.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            EditorWidget::new(&mut self.left_state, &mut self.left_view).show(ui);
                        },
                    );
                    ui.separator();
                    ui.allocate_ui_with_layout(
                        egui::vec2(half_w, avail.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            EditorWidget::new(&mut self.right_state, &mut self.right_view).show(ui);
                        },
                    );
                });
                let sync = self.left_view.scroll_y.max(self.right_view.scroll_y);
                self.left_view.scroll_y = sync;
                self.right_view.scroll_y = sync;
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("egui_editor — diff"),
        ..Default::default()
    };
    eframe::run_native(
        "egui_editor diff",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            Ok(Box::new(App::default()))
        }),
    )
}
