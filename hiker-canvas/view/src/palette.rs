//! Resolution of a JSON Canvas [`Color`] to concrete egui colors. Preset slots
//! `1..=6` map to a small built-in palette keyed off [`egui::Visuals`]
//! light/dark so a canvas reads correctly in both themes; a `#RRGGBB` hex
//! literal is used verbatim. The core never hard-codes RGB — that mapping lives
//! here, at the render boundary. Backs the `canvas-node-frame` /
//! `canvas-edge-routing` color resolution (those slugs are anchored on their
//! painters in `paint.rs` / `edges.rs`).

use egui::{Color32, Visuals};
use hiker_canvas::color::Color;

/// The stroke + fill a node or edge resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// Border / line color.
    pub stroke: Color32,
    /// Card fill (a translucent tint of the stroke for node cards).
    pub fill: Color32,
}

/// The six preset accent hues, in `1..=6` order (red, orange, yellow, green,
/// cyan, purple) — the conventional JSON Canvas preset ordering.
const PRESET_LIGHT: [Color32; 6] = [
    Color32::from_rgb(0xe0, 0x53, 0x53),
    Color32::from_rgb(0xe0, 0x90, 0x40),
    Color32::from_rgb(0xc0, 0xa0, 0x30),
    Color32::from_rgb(0x4c, 0xb0, 0x55),
    Color32::from_rgb(0x36, 0xa0, 0xb0),
    Color32::from_rgb(0x9a, 0x5c, 0xd0),
];

const PRESET_DARK: [Color32; 6] = [
    Color32::from_rgb(0xff, 0x7b, 0x72),
    Color32::from_rgb(0xff, 0xa6, 0x57),
    Color32::from_rgb(0xe3, 0xc5, 0x4a),
    Color32::from_rgb(0x6c, 0xc6, 0x74),
    Color32::from_rgb(0x56, 0xc2, 0xd6),
    Color32::from_rgb(0xc4, 0x8b, 0xf0),
];

/// Resolve a preset slot (`1..=6`) to its accent color for the active theme.
#[must_use]
pub fn preset_color(slot: u8, dark: bool) -> Color32 {
    let table = if dark { &PRESET_DARK } else { &PRESET_LIGHT };
    let idx = usize::from(slot.clamp(1, 6)) - 1;
    table[idx]
}

/// Resolve a node's optional [`Color`] to its border + card-fill for `visuals`.
/// `None` falls back to the theme's neutral widget colors.
#[must_use]
pub fn resolve_node(color: Option<&Color>, visuals: &Visuals) -> Resolved {
    let dark = visuals.dark_mode;
    let stroke = match color {
        Some(Color::Preset(slot)) => preset_color(*slot, dark),
        Some(Color::Hex(hex)) => parse_hex(hex).unwrap_or(visuals.widgets.noninteractive.fg_stroke.color),
        None => visuals.widgets.noninteractive.bg_stroke.color,
    };
    let fill = match color {
        Some(_) => tint(stroke, visuals.window_fill, 0.14),
        None => visuals.widgets.noninteractive.weak_bg_fill,
    };
    Resolved { stroke, fill }
}

/// Resolve an edge's optional [`Color`] to its line color for `visuals`.
#[must_use]
pub fn resolve_edge(color: Option<&Color>, visuals: &Visuals) -> Color32 {
    match color {
        Some(Color::Preset(slot)) => preset_color(*slot, visuals.dark_mode),
        Some(Color::Hex(hex)) => parse_hex(hex).unwrap_or_else(|| visuals.text_color()),
        // text_color() is a method call, so the lazy form is the right one here.
        None => visuals.widgets.noninteractive.fg_stroke.color,
    }
}

/// Blend `accent` over `base` at opacity `t` (0 = base, 1 = accent).
fn tint(accent: Color32, base: Color32, t: f32) -> Color32 {
    let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    Color32::from_rgb(
        lerp(base.r(), accent.r()),
        lerp(base.g(), accent.g()),
        lerp(base.b(), accent.b()),
    )
}

/// Parse a `#RRGGBB` (or `#RGB`) hex literal into a [`Color32`].
fn parse_hex(hex: &str) -> Option<Color32> {
    let body = hex.strip_prefix('#')?;
    match body.len() {
        6 => {
            let r = u8::from_str_radix(&body[0..2], 16).ok()?;
            let g = u8::from_str_radix(&body[2..4], 16).ok()?;
            let b = u8::from_str_radix(&body[4..6], 16).ok()?;
            Some(Color32::from_rgb(r, g, b))
        }
        3 => {
            let dup = |c: &str| u8::from_str_radix(c, 16).ok().map(|v| v * 17);
            Some(Color32::from_rgb(dup(&body[0..1])?, dup(&body[1..2])?, dup(&body[2..3])?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_hex, preset_color, resolve_edge};
    use egui::{Color32, Visuals};
    use hiker_canvas::color::Color;

    #[test]
    fn hex_parses_six_and_three_digit() {
        assert_eq!(parse_hex("#ff8800"), Some(Color32::from_rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_hex("#f80"), Some(Color32::from_rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_hex("nope"), None);
    }

    #[test]
    fn presets_differ_between_light_and_dark() {
        for slot in 1..=6u8 {
            assert_ne!(preset_color(slot, true), preset_color(slot, false));
        }
    }

    #[test]
    fn hex_edge_color_used_verbatim() {
        let c = resolve_edge(Some(&Color::Hex("#123456".to_owned())), &Visuals::light());
        assert_eq!(c, Color32::from_rgb(0x12, 0x34, 0x56));
    }
}
