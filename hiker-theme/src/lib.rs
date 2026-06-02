//! Hiker's egui theme. Approximates the CSS tokens from
//! `ui/src/style/tokens.css` — light palette to match the editor's
//! `light_default` theme used for markdown decorations.
//!
//! Shared egui theme/style (status: crawler-shared-theme), extracted from
//! `app/src/theme.rs`. Depends ONLY on egui; consumed by both `hiker-app` and
//! `hiker-crawler` so the two match colors/fonts/spacing.

/// Zero-sized handle for installing the app theme. A struct (rather than
/// a free `install` fn) so it's an inherent method, exempt from
/// `single_call_fn`.
pub struct Theme;

impl Theme {
    pub fn install(self, ctx: &egui::Context) {
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
        // Selection highlight: accent blue at ~44 % opacity (unmultiplied).
        // The old value used pre-multiplied RGBA with very diluted channels,
        // producing a near-invisible tint on the light panel background (#f4f6f8).
        // This raises contrast so selections are clearly readable across all
        // text inputs and the editor without being visually heavy.
        style.visuals.selection.bg_fill =
            egui::Color32::from_rgba_unmultiplied(0x2f, 0x6f, 0xed, 0x70);
        style.visuals.selection.stroke = egui::Stroke::new(1.0, accent);
        style.visuals.hyperlink_color = accent;

        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);

        // Body text a touch larger than egui's default — markdown editing
        // wants more readability than UI controls.
        let mut text_styles = style.text_styles.clone();
        if let Some(body) = text_styles.get_mut(&egui::TextStyle::Body) {
            body.size = 14.0;
        }
        if let Some(monospace) = text_styles.get_mut(&egui::TextStyle::Monospace) {
            monospace.size = 13.0;
        }
        style.text_styles = text_styles;

        ctx.set_style(style);
    }
}

/// Subtle border / divider colour used by panels and the tab strip.
pub const fn divider() -> egui::Color32 {
    egui::Color32::from_rgb(0xd6, 0xda, 0xe0)
}

/// Slightly-darker highlight for the active tab / selected row.
pub const fn active_bg() -> egui::Color32 {
    egui::Color32::from_rgb(0xe2, 0xe8, 0xf0)
}

/// Hover background tint.
pub const fn hover_bg() -> egui::Color32 {
    egui::Color32::from_rgb(0xea, 0xee, 0xf4)
}

/// Accent colour for dirty markers, focus rings, etc.
pub const fn accent() -> egui::Color32 {
    egui::Color32::from_rgb(0x2f, 0x6f, 0xed)
}

/// Muted text colour for secondary labels (vault path, status bar).
pub const fn muted() -> egui::Color32 {
    egui::Color32::from_rgb(0x6a, 0x73, 0x7d)
}

/// Amber used for in-line warning glyphs and matching warning text
/// (stale-buffer hint, index-offline hint, tool-error chat badges).
pub const fn warn() -> egui::Color32 {
    egui::Color32::from_rgb(0xc4, 0x86, 0x00)
}
