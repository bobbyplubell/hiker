//! Block-zone painting: hatched fills, solid fills, expander bars,
//! block-text and block-widget placeholders.

use std::sync::Arc;

use editor_core::{BlockDeco, BlockKind, BlockSide, BlockTextLine, BlockWidget, Color, Decoration};
use editor_view::{ClickAction, ClickRect, ClickZone};
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Stroke};

use super::to_egui_color;

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_block_zone(
    ui: &egui::Ui,
    painter: &egui::Painter,
    layers: &[editor_core::DecorationSet],
    line_byte_start: usize,
    line_byte_end: usize,
    side: BlockSide,
    rect: Rect,
    font_size: f32,
    line_height: f32,
    text_origin_x: f32,
    hatched_default: Color,
    click_zones: &mut Vec<ClickZone>,
    widget_rect: Rect,
) {
    enum Item<'a> {
        Block(&'a BlockDeco),
        Widget(&'a Arc<dyn BlockWidget>),
    }
    let mut items: Vec<Item<'_>> = Vec::new();
    for layer in layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
            if range.start < line_byte_start || range.start > line_byte_end {
                continue;
            }
            match deco {
                Decoration::Block(b) if b.side == side => items.push(Item::Block(b)),
                Decoration::BlockWidget { side: s, widget } if *s == side => {
                    items.push(Item::Widget(widget));
                }
                _ => {}
            }
        }
    }
    if items.is_empty() {
        paint_hatched(painter, rect, to_egui_color(hatched_default));
        return;
    }
    let mut y = rect.min.y;
    for item in items {
        match item {
            Item::Block(b) => {
                let b_rect = Rect::from_min_max(
                    Pos2::new(rect.min.x, y),
                    Pos2::new(rect.max.x, y + b.height),
                );
                paint_block_kind(
                    ui, painter, &b.kind, b_rect, font_size, line_height,
                    text_origin_x, hatched_default, click_zones, widget_rect,
                );
                y += b.height;
            }
            Item::Widget(w) => {
                let h = w.measure(font_size, rect.width());
                let b_rect = Rect::from_min_max(
                    Pos2::new(rect.min.x, y),
                    Pos2::new(rect.max.x, y + h),
                );
                paint_block_widget_placeholder(
                    ui, painter, w, b_rect, font_size, click_zones, widget_rect,
                );
                y += h;
            }
        }
    }
}

/// v1 placeholder render for a block widget decoration: a colored rect with a
/// "widget" label. Real per-widget painting is deferred.
fn paint_block_widget_placeholder(
    ui: &egui::Ui,
    painter: &egui::Painter,
    widget: &Arc<dyn BlockWidget>,
    rect: Rect,
    font_size: f32,
    click_zones: &mut Vec<ClickZone>,
    widget_rect: Rect,
) {
    let visuals = ui.style().visuals.clone();
    let bg = if visuals.dark_mode {
        Color32::from_rgba_unmultiplied(70, 80, 110, 80)
    } else {
        Color32::from_rgba_unmultiplied(210, 220, 240, 220)
    };
    let fg = visuals.weak_text_color();
    painter.rect_filled(rect, 3.0, bg);
    let label = "widget";
    let font_id = FontId::new(font_size * 0.85, FontFamily::Proportional);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, fg));
    let pos = Pos2::new(
        rect.min.x + 6.0,
        rect.min.y + (rect.height() - galley.size().y) * 0.5,
    );
    painter.galley(pos, galley, fg);

    if widget.handles_click() {
        click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: rect.min.x - widget_rect.min.x,
                y_min: rect.min.y - widget_rect.min.y,
                x_max: rect.max.x - widget_rect.min.x,
                y_max: rect.max.y - widget_rect.min.y,
            },
            action: ClickAction::WidgetClick(widget.widget_id()),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_block_kind(
    ui: &egui::Ui,
    painter: &egui::Painter,
    kind: &BlockKind,
    rect: Rect,
    font_size: f32,
    line_height: f32,
    text_origin_x: f32,
    hatched_default: Color,
    click_zones: &mut Vec<ClickZone>,
    widget_rect: Rect,
) {
    match kind {
        BlockKind::Hatched(c) => {
            let color = if c.a == 0 { hatched_default } else { *c };
            paint_hatched(painter, rect, to_egui_color(color));
        }
        BlockKind::Solid(c) => {
            painter.rect_filled(rect, 0.0, to_egui_color(*c));
        }
        BlockKind::Text { lines } => {
            paint_block_text(ui, painter, lines, rect, text_origin_x, font_size, line_height);
        }
        BlockKind::Expander { id, label, collapsed } => {
            paint_expander(
                ui, painter, rect, *id, label.as_str(), *collapsed,
                font_size, click_zones, widget_rect,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_expander(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    id: u64,
    label: &str,
    collapsed: bool,
    font_size: f32,
    click_zones: &mut Vec<ClickZone>,
    widget_rect: Rect,
) {
    let visuals = ui.style().visuals.clone();
    let bg = if visuals.dark_mode {
        Color32::from_rgba_unmultiplied(60, 60, 80, 80)
    } else {
        Color32::from_rgba_unmultiplied(220, 224, 232, 220)
    };
    let fg = visuals.weak_text_color();
    painter.rect_filled(rect, 4.0, bg);
    // Top + bottom thin border to give it a "button-bar" feel.
    let border = visuals.weak_text_color().gamma_multiply(0.4);
    painter.line_segment(
        [Pos2::new(rect.min.x, rect.min.y), Pos2::new(rect.max.x, rect.min.y)],
        Stroke::new(0.5, border),
    );
    painter.line_segment(
        [Pos2::new(rect.min.x, rect.max.y), Pos2::new(rect.max.x, rect.max.y)],
        Stroke::new(0.5, border),
    );

    let glyph = if collapsed { ">" } else { "v" };
    let text = format!("  {glyph}  {label}");
    let font_id = FontId::new(font_size, FontFamily::Proportional);
    let galley = ui.fonts(|f| f.layout_no_wrap(text, font_id, fg));
    let pos = Pos2::new(
        rect.min.x + 8.0,
        rect.min.y + (rect.height() - galley.size().y) * 0.5,
    );
    painter.galley(pos, galley, fg);

    // Record the entire bar as a click zone; coordinates are widget-local.
    let local_min_x = rect.min.x - widget_rect.min.x;
    let local_min_y = rect.min.y - widget_rect.min.y;
    let local_max_x = rect.max.x - widget_rect.min.x;
    let local_max_y = rect.max.y - widget_rect.min.y;
    click_zones.push(ClickZone {
        rect: ClickRect {
            x_min: local_min_x,
            y_min: local_min_y,
            x_max: local_max_x,
            y_max: local_max_y,
        },
        action: ClickAction::ToggleFold(id),
    });
}

fn paint_hatched(painter: &egui::Painter, rect: Rect, color: Color32) {
    // Draw 45° stripes inside `rect`. Painter is already clipped to the widget
    // rect; manual clamp keeps the lines inside `rect` specifically.
    let stride = 8.0;
    let stroke = Stroke::new(1.0, color);
    let w = rect.width();
    let h = rect.height();
    let mut t = -h;
    while t < w {
        // Line from (rect.min.x + t, rect.min.y) to (rect.min.x + t + h, rect.max.y),
        // clamped to rect horizontally.
        let raw_x1 = rect.min.x + t;
        let raw_x2 = rect.min.x + t + h;
        let (x1, y1, x2, y2) = clip_line_to_rect(
            raw_x1, rect.min.y, raw_x2, rect.max.y, rect,
        );
        if (x2 - x1).abs() + (y2 - y1).abs() > 0.5 {
            painter.line_segment([Pos2::new(x1, y1), Pos2::new(x2, y2)], stroke);
        }
        t += stride;
    }
}

fn clip_line_to_rect(
    mut x1: f32,
    mut y1: f32,
    mut x2: f32,
    mut y2: f32,
    rect: Rect,
) -> (f32, f32, f32, f32) {
    // Parametric clip on the segment for x in [rect.min.x, rect.max.x].
    let dx = x2 - x1;
    let dy = y2 - y1;
    if dx.abs() < f32::EPSILON {
        return (x1, y1, x2, y2);
    }
    // t at x = rect.min.x
    let t_left = (rect.min.x - x1) / dx;
    let t_right = (rect.max.x - x1) / dx;
    let (t_min, t_max) = if dx >= 0.0 { (t_left, t_right) } else { (t_right, t_left) };
    let t0 = t_min.max(0.0);
    let t1 = t_max.min(1.0);
    if t0 > t1 {
        return (x1, y1, x1, y1);
    }
    let nx1 = x1 + dx * t0;
    let ny1 = y1 + dy * t0;
    let nx2 = x1 + dx * t1;
    let ny2 = y1 + dy * t1;
    x1 = nx1;
    y1 = ny1;
    x2 = nx2;
    y2 = ny2;
    (x1, y1, x2, y2)
}

fn paint_block_text(
    ui: &egui::Ui,
    painter: &egui::Painter,
    lines: &[BlockTextLine],
    rect: Rect,
    text_origin_x: f32,
    font_size: f32,
    line_height: f32,
) {
    if lines.is_empty() {
        return;
    }
    let row_h = line_height;
    let mut y = rect.min.y;
    let visuals = ui.style().visuals.clone();
    let default_fg = visuals.text_color();
    for line in lines {
        if y + row_h > rect.max.y + 0.5 {
            break;
        }
        let line_bg = line.bg.map(to_egui_color);
        if let Some(bg) = line_bg {
            let r = Rect::from_min_max(
                Pos2::new(rect.min.x, y),
                Pos2::new(rect.max.x, y + row_h),
            );
            painter.rect_filled(r, 0.0, bg);
        }
        // Intraline mark backgrounds.
        let font_id = FontId::new(font_size, FontFamily::Monospace);
        let fg = line.fg.map(to_egui_color).unwrap_or(default_fg);
        let galley = ui.fonts(|f| f.layout_no_wrap(line.text.to_string(), font_id.clone(), fg));
        // Paint mark backgrounds using prefix-galley measurement.
        for (range, mark_bg) in &line.marks {
            let safe_start = range.start.min(line.text.len());
            let safe_end = range.end.min(line.text.len());
            if safe_start >= safe_end {
                continue;
            }
            let pre1 = ui.fonts(|f| {
                f.layout_no_wrap(line.text[..safe_start].to_string(), font_id.clone(), fg)
            });
            let pre2 = ui.fonts(|f| {
                f.layout_no_wrap(line.text[..safe_end].to_string(), font_id.clone(), fg)
            });
            let x_start = text_origin_x + pre1.size().x;
            let x_end = text_origin_x + pre2.size().x;
            let r = Rect::from_min_max(
                Pos2::new(x_start, y),
                Pos2::new(x_end, y + row_h),
            );
            painter.rect_filled(r, 0.0, to_egui_color(*mark_bg));
        }
        painter.galley(Pos2::new(text_origin_x, y + (row_h - galley.size().y) * 0.5), galley, fg);
        y += row_h;
    }
}
