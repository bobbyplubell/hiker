//! Renderer for the tooltip primitive (SPEC §9.5, IMPLEMENTATION §16.5.2).
//!
//! Walks `ViewState::tooltips` and paints each entry in a floating
//! `egui::Area` above the editor. v1 renders both `Text` and `Markdown`
//! content as plain text — there is no inline markdown rendering inside
//! tooltips yet (hosts that need rich content can pre-format to text).

use editor_core::state::Editor as EditorState;
use editor_view::popup::Tooltip;
use editor_view::popup::TooltipAnchor;
use editor_view::popup::TooltipContent;
use editor_view::popup::TooltipPlacement;
use editor_view::viewport::ViewState;
use egui::{Area, Frame, Id, Order, Pos2, Rect};

/// Paint every tooltip in `view.tooltips` over the editor. Must be called
/// AFTER the main `paint()` call so tooltips draw on top.
pub fn paint_tooltips(
    ui: &mut egui::Ui,
    view: &ViewState,
    state: &EditorState,
    widget_rect: Rect,
) {
    let painter = TooltipPainter { view, state, widget_rect };
    for tip in &view.tooltips {
        painter.paint_one(ui, tip);
    }
}

struct TooltipPainter<'a> {
    view: &'a ViewState,
    state: &'a EditorState,
    widget_rect: Rect,
}

impl<'a> TooltipPainter<'a> {
    fn paint_one(&self, ui: &mut egui::Ui, tip: &Tooltip) {
        let anchor_local = match self.resolve_anchor(tip) {
            Some(p) => p,
            None => return,
        };
        let anchor_screen = Pos2::new(
            self.widget_rect.min.x + anchor_local.x,
            self.widget_rect.min.y + anchor_local.y,
        );
        let text = match &tip.content {
            TooltipContent::Text(s) | TooltipContent::Markdown(s) => s.as_str(),
        };

        // Build & measure first so Smart placement can decide above/below before
        // we commit the pivot. We probe with a hidden Area-less layout pass via
        // the fonts; a generous default for line wrap keeps text readable.
        let est_height = self.estimate_height(ui, text);

        let placement = self.resolve_placement(tip.placement, anchor_screen, est_height);
        let pivot = match placement {
            TooltipPlacement::Above => egui::Align2::LEFT_BOTTOM,
            TooltipPlacement::Below | TooltipPlacement::Smart => egui::Align2::LEFT_TOP,
        };
        let y_offset = match placement {
            TooltipPlacement::Above => -4.0,
            TooltipPlacement::Below | TooltipPlacement::Smart => self.view.line_height + 4.0,
        };
        let area_pos = Pos2::new(anchor_screen.x, anchor_screen.y + y_offset);

        let id = Id::new(("editor_tooltip", tip.id));
        let widget_w = self.widget_rect.width();
        Area::new(id)
            .order(Order::Foreground)
            .fixed_pos(area_pos)
            .pivot(pivot)
            .interactable(false)
            .show(ui.ctx(), |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(widget_w.clamp(120.0, 360.0));
                    ui.label(text);
                });
            });
    }

    fn resolve_anchor(&self, tip: &Tooltip) -> Option<Pos2> {
        match tip.anchor {
            TooltipAnchor::Coords { x, y } => Some(Pos2::new(x, y)),
            TooltipAnchor::BufferPos { byte } => {
                let byte = byte as usize;
                let doc_len = self.state.doc.len_bytes();
                if byte > doc_len {
                    return None;
                }
                let line = self.state.doc.byte_to_line(byte);
                // y: top of the line's text row, relative to widget top.
                let y = self.view.text_top_y(line);
                // x: we don't have full per-segment measurement here; approximate
                // using a monospace advance × column-within-line. This is good
                // enough for tooltip placement (the area pivot tolerates small
                // offsets). Fine-grained x positioning can come later when the
                // line layout is exposed from the renderer.
                let line_start = self.state.doc.line_to_byte(line);
                let col_bytes = byte.saturating_sub(line_start);
                let advance = self.view.font_size * 0.6;
                let x = self.view.content_origin_x() + col_bytes as f32 * advance;
                Some(Pos2::new(x, y))
            }
        }
    }

    fn resolve_placement(
        &self,
        placement: TooltipPlacement,
        anchor_screen: Pos2,
        est_height: f32,
    ) -> TooltipPlacement {
        match placement {
            TooltipPlacement::Above | TooltipPlacement::Below => placement,
            TooltipPlacement::Smart => {
                let bottom_of_below = anchor_screen.y + est_height + 12.0;
                if bottom_of_below > self.widget_rect.max.y {
                    TooltipPlacement::Above
                } else {
                    TooltipPlacement::Below
                }
            }
        }
    }

    fn estimate_height(&self, ui: &egui::Ui, text: &str) -> f32 {
        let row_h = ui.fonts(|f| f.row_height(&egui::FontId::proportional(self.view.font_size)));
        let lines = text.lines().count().max(1) as f32;
        lines * row_h + 12.0
    }
}
