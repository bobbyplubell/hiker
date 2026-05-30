//! `hiker-lite` — a lightweight Notepad++-style text editor built on the
//! standalone editor stack and `egui_workbench`.
//!
//! Phase 1 is native-only. The wasm/OPFS backend follows in Phase 2;
//! everything that touches the filesystem is already routed through the
//! async `Vfs` trait so the swap is mechanical.

mod app;
mod host;
mod panels;
mod theme;
mod vfs;

use app::LiteApp;

fn main() -> eframe::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let _guard = runtime.enter();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_title("hiker-lite"),
        ..Default::default()
    };
    eframe::run_native(
        "hiker-lite",
        options,
        Box::new(|cc| {
            theme::install(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(LiteApp::new(runtime.handle().clone())))
        }),
    )
}
