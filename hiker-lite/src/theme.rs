//! Light theme for hiker-lite. Mirrors the palette hiker proper uses so the
//! two apps feel like siblings, without pulling in hiker's app crate.

use eframe::egui;

pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.visuals = egui::Visuals::light();
    style.visuals.window_fill = egui::Color32::from_rgb(0xfa, 0xfb, 0xfc);
    style.visuals.panel_fill = egui::Color32::from_rgb(0xf4, 0xf6, 0xf8);
    style.visuals.faint_bg_color = egui::Color32::from_rgb(0xec, 0xef, 0xf3);
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(0xff, 0xff, 0xff);
    style.visuals.code_bg_color = egui::Color32::from_rgb(0xee, 0xf1, 0xf5);

    style.visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0xd6, 0xda, 0xe0));
    style.visuals.widgets.noninteractive.fg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x1f, 0x24, 0x2c));
    style.visuals.widgets.inactive.fg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x4a, 0x52, 0x5e));

    let accent = egui::Color32::from_rgb(0x2f, 0x6f, 0xed);
    style.visuals.selection.bg_fill =
        egui::Color32::from_rgba_unmultiplied(0x2f, 0x6f, 0xed, 0x70);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, accent);
    style.visuals.hyperlink_color = accent;

    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);

    let mut text_styles = style.text_styles.clone();
    if let Some(body) = text_styles.get_mut(&egui::TextStyle::Body) {
        body.size = 14.0;
    }
    if let Some(mono) = text_styles.get_mut(&egui::TextStyle::Monospace) {
        mono.size = 13.0;
    }
    style.text_styles = text_styles;

    ctx.set_style(style);
}
