//! Block-zone painting: hatched fills, solid fills, expander bars,
//! block-text and block-widget placeholders.

use std::sync::Arc;

use editor_core::decoration::ActionButton;

use editor_core::decoration::ActionButtonStyle;

use editor_core::decoration::ActionTone;

use editor_core::decoration::BlockDeco;

use editor_core::decoration::BlockKind;

use editor_core::decoration::BlockSide;

use editor_core::decoration::BlockTextLine;

use editor_core::decoration::BlockWidget;

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;
use editor_view::viewport::ClickAction;
use editor_view::viewport::ClickRect;
use editor_view::viewport::ClickZone;
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Stroke};

use super::to_egui_color;

/// Shared painting context for block-zone helpers. Bundles the
/// per-frame rendering environment (canvas, metrics, the click-zone
/// sink) threaded through every paint_* call so the helpers can be
/// methods on `self` and stay below per-fn arg limits. The caller
/// builds this once and invokes [`BlockPaint::paint_zone`] per side.
pub(super) struct BlockPaint<'a> {
    pub(super) ui: &'a egui::Ui,
    pub(super) painter: &'a egui::Painter,
    pub(super) font_size: f32,
    pub(super) line_height: f32,
    pub(super) text_origin_x: f32,
    pub(super) hatched_default: Color,
    pub(super) click_zones: &'a mut Vec<ClickZone>,
    pub(super) widget_rect: Rect,
}

/// Identifies which block zone to paint: the decoration layers to scan,
/// the line's byte extent, the side (above/below the line), and the
/// rect the zone occupies.
pub(super) struct BlockZone<'a> {
    pub(super) layers: &'a [editor_core::decoration::Set],
    pub(super) line_byte_start: usize,
    pub(super) line_byte_end: usize,
    pub(super) side: BlockSide,
    pub(super) rect: Rect,
}

impl<'a> BlockPaint<'a> {
    pub(super) fn paint_zone(&mut self, zone: &BlockZone<'_>) {
        let &BlockZone { layers, line_byte_start, line_byte_end, side, rect } = zone;
        enum Item<'b> {
            Block(&'b BlockDeco),
            Widget(&'b Arc<dyn BlockWidget>),
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
            self.paint_hatched(rect, to_egui_color(self.hatched_default));
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
                    self.paint_block_kind(&b.kind, b_rect);
                    y += b.height;
                }
                Item::Widget(w) => {
                    let h = w.measure(self.font_size, rect.width());
                    let b_rect = Rect::from_min_max(
                        Pos2::new(rect.min.x, y),
                        Pos2::new(rect.max.x, y + h),
                    );
                    self.paint_block_widget_placeholder(w, b_rect);
                    y += h;
                }
            }
        }
    }

    /// v1 placeholder render for a block widget decoration: a colored rect with a
    /// "widget" label. Real per-widget painting is deferred.
    fn paint_block_widget_placeholder(
        &mut self,
        widget: &Arc<dyn BlockWidget>,
        rect: Rect,
    ) {
        let visuals = self.ui.style().visuals.clone();
        let bg = if visuals.dark_mode {
            Color32::from_rgba_unmultiplied(70, 80, 110, 80)
        } else {
            Color32::from_rgba_unmultiplied(210, 220, 240, 220)
        };
        let fg = visuals.weak_text_color();
        self.painter.rect_filled(rect, 3.0, bg);
        let label = "widget";
        let font_id = FontId::new(self.font_size * 0.85, FontFamily::Proportional);
        let galley = self.ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, fg));
        let pos = Pos2::new(
            rect.min.x + 6.0,
            rect.min.y + (rect.height() - galley.size().y) * 0.5,
        );
        self.painter.galley(pos, galley, fg);

        if widget.handles_click() {
            self.click_zones.push(ClickZone {
                rect: ClickRect {
                    x_min: rect.min.x - self.widget_rect.min.x,
                    y_min: rect.min.y - self.widget_rect.min.y,
                    x_max: rect.max.x - self.widget_rect.min.x,
                    y_max: rect.max.y - self.widget_rect.min.y,
                },
                action: ClickAction::WidgetClick(widget.widget_id()),
            });
        }
    }

    fn paint_block_kind(
        &mut self,
        kind: &BlockKind,
        rect: Rect,
    ) {
        match kind {
            BlockKind::Hatched(c) => {
                let color = if c.a == 0 { self.hatched_default } else { *c };
                self.paint_hatched(rect, to_egui_color(color));
            }
            BlockKind::Solid(c) => {
                self.painter.rect_filled(rect, 0.0, to_egui_color(*c));
            }
            BlockKind::Text { lines } => {
                self.paint_block_text(lines, rect);
            }
            BlockKind::Expander { id, label, collapsed } => {
                self.paint_expander(rect, *id, label.as_str(), *collapsed);
            }
            BlockKind::ActionRow { label, glyph, tone, buttons } => {
                self.paint_action_row(rect, label.as_str(), glyph.as_deref(), *tone, buttons);
            }
        }
    }

    fn paint_action_row(
        &mut self,
        rect: Rect,
        label: &str,
        glyph: Option<&str>,
        tone: ActionTone,
        buttons: &[ActionButton],
    ) {
        let visuals = self.ui.style().visuals.clone();
        let bg = match (tone, visuals.dark_mode) {
            (ActionTone::Normal, true) => Color32::from_rgba_unmultiplied(48, 80, 56, 110),
            (ActionTone::Normal, false) => Color32::from_rgba_unmultiplied(232, 245, 233, 220),
            (ActionTone::Warning, true) => Color32::from_rgba_unmultiplied(90, 80, 50, 110),
            (ActionTone::Warning, false) => Color32::from_rgba_unmultiplied(255, 244, 214, 220),
            (ActionTone::Conflicted, true) => Color32::from_rgba_unmultiplied(70, 70, 70, 110),
            (ActionTone::Conflicted, false) => Color32::from_rgba_unmultiplied(225, 225, 225, 220),
        };
        let fg = match tone {
            ActionTone::Conflicted => visuals.weak_text_color(),
            _ => visuals.text_color(),
        };
        self.painter.rect_filled(rect, 3.0, bg);

        // Label on the left (glyph + text).
        let label_font = FontId::new(self.font_size * 0.9, FontFamily::Proportional);
        let label_text = match glyph {
            Some(g) if !g.is_empty() => format!("{}  {}", g, label),
            _ => label.to_string(),
        };
        let label_galley = self.ui.fonts(|f| f.layout_no_wrap(label_text, label_font.clone(), fg));
        let label_pos = Pos2::new(
            rect.min.x + 8.0,
            rect.min.y + (rect.height() - label_galley.size().y) * 0.5,
        );
        self.painter.galley(label_pos, label_galley, fg);

        // Buttons stacked from the right edge inward. Sized tight so the
        // action row visually reads as a thin strip rather than a chunky
        // toolbar — the editor body is the focus, this is just an
        // affordance attached to it.
        let btn_font = FontId::new(self.font_size * 0.8, FontFamily::Proportional);
        let h_pad: f32 = 6.0;
        let v_pad: f32 = 1.0;
        let gap: f32 = 3.0;
        let mut x = rect.max.x - 6.0;
        for btn in buttons.iter().rev() {
            let btn_fg_enabled = match btn.style {
                ActionButtonStyle::Primary | ActionButtonStyle::Danger => Color32::WHITE,
                ActionButtonStyle::Neutral => visuals.text_color(),
            };
            let btn_fg = if btn.enabled { btn_fg_enabled } else { visuals.weak_text_color() };
            let btn_bg_enabled = match btn.style {
                ActionButtonStyle::Primary => Color32::from_rgb(0x2f, 0x8f, 0x4d),
                ActionButtonStyle::Danger => Color32::from_rgb(0xb9, 0x3a, 0x3a),
                ActionButtonStyle::Neutral => Color32::from_gray(if visuals.dark_mode { 70 } else { 220 }),
            };
            let btn_bg = if btn.enabled {
                btn_bg_enabled
            } else {
                Color32::from_gray(if visuals.dark_mode { 60 } else { 200 })
            };
            let btn_galley = self.ui.fonts(|f| f.layout_no_wrap(btn.label.to_string(), btn_font.clone(), btn_fg));
            let btn_w = btn_galley.size().x + h_pad * 2.0;
            let btn_h = (btn_galley.size().y + v_pad * 2.0).min(rect.height() - 4.0);
            let btn_rect = Rect::from_min_max(
                Pos2::new(x - btn_w, rect.min.y + (rect.height() - btn_h) * 0.5),
                Pos2::new(x, rect.min.y + (rect.height() + btn_h) * 0.5),
            );
            self.painter.rect_filled(btn_rect, 3.0, btn_bg);
            self.painter.galley(
                Pos2::new(
                    btn_rect.min.x + h_pad,
                    btn_rect.min.y + (btn_rect.height() - btn_galley.size().y) * 0.5,
                ),
                btn_galley,
                btn_fg,
            );
            if btn.enabled {
                self.click_zones.push(ClickZone {
                    rect: ClickRect {
                        x_min: btn_rect.min.x - self.widget_rect.min.x,
                        y_min: btn_rect.min.y - self.widget_rect.min.y,
                        x_max: btn_rect.max.x - self.widget_rect.min.x,
                        y_max: btn_rect.max.y - self.widget_rect.min.y,
                    },
                    action: ClickAction::WidgetClick(btn.id),
                });
            }
            x -= btn_w + gap;
        }
    }

    fn paint_expander(
        &mut self,
        rect: Rect,
        id: u64,
        label: &str,
        collapsed: bool,
    ) {
        let visuals = self.ui.style().visuals.clone();
        let bg = if visuals.dark_mode {
            Color32::from_rgba_unmultiplied(60, 60, 80, 80)
        } else {
            Color32::from_rgba_unmultiplied(220, 224, 232, 220)
        };
        let fg = visuals.weak_text_color();
        self.painter.rect_filled(rect, 4.0, bg);
        // Top + bottom thin border to give it a "button-bar" feel.
        let border = visuals.weak_text_color().gamma_multiply(0.4);
        self.painter.line_segment(
            [Pos2::new(rect.min.x, rect.min.y), Pos2::new(rect.max.x, rect.min.y)],
            Stroke::new(0.5, border),
        );
        self.painter.line_segment(
            [Pos2::new(rect.min.x, rect.max.y), Pos2::new(rect.max.x, rect.max.y)],
            Stroke::new(0.5, border),
        );

        let glyph = if collapsed { ">" } else { "v" };
        let text = format!("  {glyph}  {label}");
        let font_id = FontId::new(self.font_size, FontFamily::Proportional);
        let galley = self.ui.fonts(|f| f.layout_no_wrap(text, font_id, fg));
        let pos = Pos2::new(
            rect.min.x + 8.0,
            rect.min.y + (rect.height() - galley.size().y) * 0.5,
        );
        self.painter.galley(pos, galley, fg);

        // Record the entire bar as a click zone; coordinates are widget-local.
        let local_min_x = rect.min.x - self.widget_rect.min.x;
        let local_min_y = rect.min.y - self.widget_rect.min.y;
        let local_max_x = rect.max.x - self.widget_rect.min.x;
        let local_max_y = rect.max.y - self.widget_rect.min.y;
        self.click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: local_min_x,
                y_min: local_min_y,
                x_max: local_max_x,
                y_max: local_max_y,
            },
            action: ClickAction::ToggleFold(id),
        });
    }

    fn paint_hatched(&self, rect: Rect, color: Color32) {
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
            let (x1, y1, x2, y2) = self.clip_line_to_rect(
                raw_x1, rect.min.y, raw_x2, rect.max.y, rect,
            );
            if (x2 - x1).abs() + (y2 - y1).abs() > 0.5 {
                self.painter.line_segment([Pos2::new(x1, y1), Pos2::new(x2, y2)], stroke);
            }
            t += stride;
        }
    }

    fn clip_line_to_rect(
        &self,
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
        &self,
        lines: &[BlockTextLine],
        rect: Rect,
    ) {
        if lines.is_empty() {
            return;
        }
        let row_h = self.line_height;
        let mut y = rect.min.y;
        let visuals = self.ui.style().visuals.clone();
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
                self.painter.rect_filled(r, 0.0, bg);
            }
            // Intraline mark backgrounds.
            let font_id = FontId::new(self.font_size, FontFamily::Monospace);
            let fg = line.fg.map(to_egui_color).unwrap_or(default_fg);
            let galley = self.ui.fonts(|f| f.layout_no_wrap(line.text.to_string(), font_id.clone(), fg));
            // Paint mark backgrounds using prefix-galley measurement.
            for (range, mark_bg) in &line.marks {
                let safe_start = range.start.min(line.text.len());
                let safe_end = range.end.min(line.text.len());
                if safe_start >= safe_end {
                    continue;
                }
                let pre1 = self.ui.fonts(|f| {
                    f.layout_no_wrap(line.text[..safe_start].to_string(), font_id.clone(), fg)
                });
                let pre2 = self.ui.fonts(|f| {
                    f.layout_no_wrap(line.text[..safe_end].to_string(), font_id.clone(), fg)
                });
                let x_start = self.text_origin_x + pre1.size().x;
                let x_end = self.text_origin_x + pre2.size().x;
                let r = Rect::from_min_max(
                    Pos2::new(x_start, y),
                    Pos2::new(x_end, y + row_h),
                );
                self.painter.rect_filled(r, 0.0, to_egui_color(*mark_bg));
            }
            self.painter.galley(Pos2::new(self.text_origin_x, y + (row_h - galley.size().y) * 0.5), galley.clone(), fg);
            // Strike line through the text (removed-diff lines): a thin
            // horizontal rule at the row's vertical center, spanning the
            // rendered glyph width.
            if line.strikethrough {
                let strike_w = galley.size().x;
                if strike_w > 0.0 {
                    let mid_y = y + row_h * 0.5;
                    self.painter.line_segment(
                        [
                            Pos2::new(self.text_origin_x, mid_y),
                            Pos2::new(self.text_origin_x + strike_w, mid_y),
                        ],
                        Stroke::new(1.0, fg),
                    );
                }
            }
            y += row_h;
        }
    }
}
