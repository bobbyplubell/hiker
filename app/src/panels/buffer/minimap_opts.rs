//! Adapter from the vault's `MinimapConfig` (TOML-backed settings) to the
//! egui-side `minimap::Options` the widget consumes, plus the small hex
//! color parser the mapping needs. Lives beside the buffer panel because the
//! panel is the only place that materializes minimap options each frame.

use editor_egui::minimap::Options as MinimapOptions;

/// Read a `MinimapConfig` directly into the egui-side `MinimapOptions`.
/// Trait methods on `&self` are exempt from `clippy::single_call_fn` even
/// when only one caller materializes them.
pub(crate) trait MinimapOptionsExt {
    fn to_minimap_options(&self) -> MinimapOptions;
}

impl MinimapOptionsExt for hiker_core::config::sections::MinimapConfig {
    fn to_minimap_options(&self) -> MinimapOptions {
        use hiker_core::config::sections::MinimapStyle;
        MinimapOptions {
            style: match self.style {
                MinimapStyle::Glyphs => editor_egui::minimap::Style::Glyphs,
                MinimapStyle::Bars => editor_egui::minimap::Style::Bars,
            },
            width: self.width as f32,
            bar_padding_left: self.bar_padding_left as f32,
            bar_padding_right: self.bar_padding_right as f32,
            bar_corner_radius: self.bar_corner_radius as f32,
            min_bar_width: self.min_bar_width as f32,
            bar_gap: (self.bar_gap_tenths as f32) / 10.0,
            colored: self.colored,
            show_section_rules: self.show_section_rules,
            show_viewport: self.show_viewport,
            show_left_edge: self.show_left_edge,
            color_heading: parse_hex_color(&self.color_heading),
            color_code: parse_hex_color(&self.color_code),
            color_emphasis: parse_hex_color(&self.color_emphasis),
            color_quote: parse_hex_color(&self.color_quote),
            color_plain: parse_hex_color(&self.color_plain),
            color_background: parse_hex_color(&self.color_background),
            color_section_rule: parse_hex_color(&self.color_section_rule),
            color_viewport: parse_hex_color(&self.color_viewport),
            color_viewport_hover: parse_hex_color(&self.color_viewport_hover),
            // Selection / search marks aren't user-tunable knobs (they
            // reflect transient editor state, not the structural palette),
            // so they ride the built-in defaults.
            ..MinimapOptions::default()
        }
    }
}

/// Parse `#RRGGBB` / `#RRGGBBAA` into an egui `Color32`. Falls back to
/// fully-opaque magenta on a malformed value so a bad config entry is
/// visually obvious instead of silently transparent.
fn parse_hex_color(s: &str) -> egui::Color32 {
    let bytes = s.as_bytes();
    if !matches!(bytes.first(), Some(b'#')) {
        return egui::Color32::from_rgb(0xff, 0x00, 0xff);
    }
    let hex = &s[1..];
    let hex_byte = |i: usize| -> Option<u8> { u8::from_str_radix(hex.get(i..i + 2)?, 16).ok() };
    match hex.len() {
        6 => {
            let (Some(r), Some(g), Some(b)) = (hex_byte(0), hex_byte(2), hex_byte(4)) else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgb(r, g, b)
        }
        8 => {
            let (Some(r), Some(g), Some(b), Some(a)) =
                (hex_byte(0), hex_byte(2), hex_byte(4), hex_byte(6))
            else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgba_unmultiplied(r, g, b, a)
        }
        _ => egui::Color32::from_rgb(0xff, 0x00, 0xff),
    }
}
