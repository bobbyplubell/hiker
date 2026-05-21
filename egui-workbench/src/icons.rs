//! Tiny SVG icon set for built-in workbench chrome (the "all tabs"
//! dropdown trigger, the panel-area maximise/minimise toggle, etc.).
//! Kept crate-internal because hosts already supply their own icon set
//! via `WorkbenchBehavior::activity_items` / `tab_style`; the assets
//! here exist only for buttons the workbench draws itself.
//!
//! `egui_extras::install_image_loaders` must be called by the host
//! before any of these render — the workbench example sets that up
//! once at startup.

use egui;

macro_rules! icon {
    ($name:ident, $file:literal) => {
        pub(crate) fn $name() -> egui::Image<'static> {
            static BYTES: &[u8] = include_bytes!(concat!("../assets/", $file));
            egui::Image::new(egui::ImageSource::Bytes {
                uri: concat!("bytes://egui_workbench-icon-", $file).into(),
                bytes: egui::load::Bytes::Static(BYTES),
            })
            .fit_to_exact_size(egui::vec2(14.0, 14.0))
        }
    };
}

icon!(chevron_down, "chevron_down.svg");
icon!(chevron_up, "chevron_up.svg");
