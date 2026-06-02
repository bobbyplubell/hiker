//! Browser-style swipe-progress overlay. While a two-finger horizontal
//! swipe is accumulating in `keybinds::handle`, paint a
//! chevron + progress fill on the appropriate edge of the central area
//! so the user gets feedback before the threshold trips.
//!
//! Implements the spec slug `navigation-swipe-visual-feedback`. Greys
//! out when there's no history in the swipe direction so a "would
//! commit but nowhere to go" gesture is visually distinct from a real
//! one.

use eframe::egui;

use crate::state::AppState;
use hiker_theme as theme;

const THRESHOLD: f32 = 120.0;
const PILL_WIDTH: f32 = 56.0;
const PILL_HEIGHT: f32 = 96.0;
const EDGE_MARGIN: f32 = 8.0;

impl AppState {
    pub fn swipe_indicator_overlay(&self, ctx: &egui::Context) {
    let state = self;
    let accum = state.session.nav.swipe_accum_x;
    if accum.abs() < 0.5 {
        return;
    }
    let progress = (accum.abs() / THRESHOLD).clamp(0.0, 1.0);
    // Swipe direction: positive dx (content moving right) → going back;
    // negative → going forward. Pill renders on the side the next page
    // is coming *from* — i.e. swipe-right paints the back arrow on the
    // left edge, swipe-left paints forward arrow on the right edge.
    let going_back = accum > 0.0;

    // Greyed when we have no history in this direction.
    let direction_active = if going_back {
        crate::state::nav_can_back(state)
    } else {
        crate::state::nav_can_forward(state)
    };

    let screen = ctx.screen_rect();
    let mid_y = screen.center().y;
    let pill_rect = if going_back {
        egui::Rect::from_min_size(
            egui::pos2(
                screen.min.x + EDGE_MARGIN,
                mid_y - PILL_HEIGHT * 0.5,
            ),
            egui::vec2(PILL_WIDTH, PILL_HEIGHT),
        )
    } else {
        egui::Rect::from_min_size(
            egui::pos2(
                screen.max.x - PILL_WIDTH - EDGE_MARGIN,
                mid_y - PILL_HEIGHT * 0.5,
            ),
            egui::vec2(PILL_WIDTH, PILL_HEIGHT),
        )
    };

    let layer = egui::LayerId::new(egui::Order::Foreground, egui::Id::new("swipe-indicator"));
    let painter = ctx.layer_painter(layer);

    // Background pill: semi-transparent dark fill, light stroke. Alpha
    // ramps in with progress so it feels like it's growing in opacity,
    // not just position.
    let alpha = ((0.35 + 0.45 * progress) * 255.0).round() as u8;
    let bg_color = if direction_active {
        egui::Color32::from_rgba_unmultiplied(20, 20, 20, alpha)
    } else {
        egui::Color32::from_rgba_unmultiplied(60, 60, 60, (alpha as f32 * 0.7) as u8)
    };
    painter.rect_filled(pill_rect, 12.0, bg_color);
    painter.rect_stroke(
        pill_rect,
        12.0,
        egui::Stroke::new(1.0, theme::divider()),
        egui::StrokeKind::Inside,
    );

    // Progress fill: a vertical bar inside the pill that grows from the
    // outer edge toward the chevron as the swipe accumulates. At 100%
    // the bar fills the whole pill; on commit the cooldown holds the
    // accumulator at the threshold so the bar flashes full.
    let bar_inset = 6.0;
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(pill_rect.min.x + bar_inset, pill_rect.min.y + bar_inset),
        egui::vec2(
            (pill_rect.width() - bar_inset * 2.0) * progress,
            pill_rect.height() - bar_inset * 2.0,
        ),
    );
    let armed = state.session.nav.swipe_armed_dir.is_some();
    let committed = state.session.nav.swipe_last_commit_dir.is_some();
    let bar_color = if committed {
        // Released past threshold — saturated accent.
        theme::accent()
    } else if armed && direction_active {
        // Held past threshold but still mid-gesture — preview the
        // accent so the user knows release will commit.
        let a = theme::accent();
        egui::Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 200)
    } else if direction_active {
        egui::Color32::from_rgba_unmultiplied(180, 180, 180, 220)
    } else {
        egui::Color32::from_rgba_unmultiplied(120, 120, 120, 160)
    };
    painter.rect_filled(bar_rect, 8.0, bar_color);

    // Chevron stroke. Sits on the inner edge (toward the screen center)
    // so the user reads pill-then-chevron-then-page. Drawn as three
    // line segments instead of a text glyph because egui's bundled
    // font doesn't ship the unicode chevron glyphs and an SVG asset
    // at this size would alias visibly — the path stroke renders
    // crisp at any pixel density.
    let glyph_color = if direction_active {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(180)
    };
    let cx = pill_rect.center().x;
    let cy = pill_rect.center().y;
    let half_h = 14.0;
    let half_w = 9.0;
    let stroke = egui::Stroke::new(5.0, glyph_color);
    let (p0, p1, p2) = if going_back {
        (
            egui::pos2(cx + half_w, cy - half_h),
            egui::pos2(cx - half_w, cy),
            egui::pos2(cx + half_w, cy + half_h),
        )
    } else {
        (
            egui::pos2(cx - half_w, cy - half_h),
            egui::pos2(cx + half_w, cy),
            egui::pos2(cx - half_w, cy + half_h),
        )
    };
    painter.line_segment([p0, p1], stroke);
    painter.line_segment([p1, p2], stroke);
    }
}
