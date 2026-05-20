//! Renderer for the tooltip primitive (SPEC §9.5, IMPLEMENTATION §16.5.2).
//!
//! Walks `ViewState::tooltips` and paints each entry in a floating
//! `egui::Area` above the editor. v1 renders both `Text` and `Markdown`
//! content as plain text — there is no inline markdown rendering inside
//! tooltips yet (hosts that need rich content can pre-format to text).

use editor_core::EditorState;
use editor_view::{Tooltip, TooltipAnchor, TooltipContent, TooltipPlacement, ViewState};
use egui::{Area, Frame, Id, Order, Pos2, Rect};

/// Paint every tooltip in `view.tooltips` over the editor. Must be called
/// AFTER the main `paint()` call so tooltips draw on top.
pub fn paint_tooltips(
    ui: &mut egui::Ui,
    view: &ViewState,
    state: &EditorState,
    widget_rect: Rect,
) {
    for tip in &view.tooltips {
        paint_one(ui, view, state, widget_rect, tip);
    }
}

fn paint_one(
    ui: &mut egui::Ui,
    view: &ViewState,
    state: &EditorState,
    widget_rect: Rect,
    tip: &Tooltip,
) {
    let anchor_local = match resolve_anchor(view, state, tip) {
        Some(p) => p,
        None => return,
    };
    let anchor_screen = Pos2::new(
        widget_rect.min.x + anchor_local.x,
        widget_rect.min.y + anchor_local.y,
    );
    let text = match &tip.content {
        TooltipContent::Text(s) | TooltipContent::Markdown(s) => s.as_str(),
    };

    // Build & measure first so Smart placement can decide above/below before
    // we commit the pivot. We probe with a hidden Area-less layout pass via
    // the fonts; a generous default for line wrap keeps text readable.
    let est_height = estimate_height(ui, text, view.font_size);

    let placement = resolve_placement(tip.placement, anchor_screen, est_height, widget_rect);
    let pivot = match placement {
        TooltipPlacement::Above => egui::Align2::LEFT_BOTTOM,
        TooltipPlacement::Below | TooltipPlacement::Smart => egui::Align2::LEFT_TOP,
    };
    let y_offset = match placement {
        TooltipPlacement::Above => -4.0,
        TooltipPlacement::Below | TooltipPlacement::Smart => view.line_height + 4.0,
    };
    let area_pos = Pos2::new(anchor_screen.x, anchor_screen.y + y_offset);

    let id = Id::new(("editor_tooltip", tip.id));
    Area::new(id)
        .order(Order::Foreground)
        .fixed_pos(area_pos)
        .pivot(pivot)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(widget_rect.width().clamp(120.0, 360.0));
                ui.label(text);
            });
        });
}

fn resolve_anchor(view: &ViewState, state: &EditorState, tip: &Tooltip) -> Option<Pos2> {
    match tip.anchor {
        TooltipAnchor::Coords { x, y } => Some(Pos2::new(x, y)),
        TooltipAnchor::BufferPos { byte } => {
            let byte = byte as usize;
            let doc_len = state.doc.len_bytes();
            if byte > doc_len {
                return None;
            }
            let line = state.doc.byte_to_line(byte);
            // y: top of the line's text row, relative to widget top.
            let y = view.text_top_y(line);
            // x: we don't have full per-segment measurement here; approximate
            // using a monospace advance × column-within-line. This is good
            // enough for tooltip placement (the area pivot tolerates small
            // offsets). Fine-grained x positioning can come later when the
            // line layout is exposed from the renderer.
            let line_start = state.doc.line_to_byte(line);
            let col_bytes = byte.saturating_sub(line_start);
            let advance = view.font_size * 0.6;
            let x = if view.hide_gutter { 4.0 } else { view.gutter_width }
                + col_bytes as f32 * advance;
            Some(Pos2::new(x, y))
        }
    }
}

fn resolve_placement(
    placement: TooltipPlacement,
    anchor_screen: Pos2,
    est_height: f32,
    widget_rect: Rect,
) -> TooltipPlacement {
    match placement {
        TooltipPlacement::Above | TooltipPlacement::Below => placement,
        TooltipPlacement::Smart => {
            let bottom_of_below = anchor_screen.y + est_height + 12.0;
            if bottom_of_below > widget_rect.max.y {
                TooltipPlacement::Above
            } else {
                TooltipPlacement::Below
            }
        }
    }
}

fn estimate_height(ui: &egui::Ui, text: &str, font_size: f32) -> f32 {
    let row_h = ui.fonts(|f| f.row_height(&egui::FontId::proportional(font_size)));
    let lines = text.lines().count().max(1) as f32;
    lines * row_h + 12.0
}
