use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

const SAMPLE: &str = "// minimal egui_editor demo\nfn main() {\n    println!(\"hello world\");\n    for i in 0..10 {\n        println!(\"{i}\");\n    }\n}\n";

struct App {
    state: EditorState,
    view: ViewState,
}

impl Default for App {
    fn default() -> Self {
        Self {
            state: EditorState::new(SAMPLE),
            view: ViewState::default(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            EditorWidget::new(&mut self.state, &mut self.view).show(ui);
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 600.0])
            .with_title("egui_editor — minimal"),
        ..Default::default()
    };
    eframe::run_native(
        "egui_editor",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            Ok(Box::new(App::default()))
        }),
    )
}
