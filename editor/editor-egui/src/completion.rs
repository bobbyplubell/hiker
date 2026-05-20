//! Renderer for the autocomplete popup (SPEC §9.6, IMPLEMENTATION §16.5.3).
//!
//! Draws `view.completion` as a floating `egui::Area` immediately below the
//! caret. v1 paints a scrollable list of `label`s with an optional `detail`
//! column; the selected row is highlighted. The popup absorbs no input
//! itself — keys are routed through the normal command pipeline, which
//! intercepts ArrowUp/Down/Enter/Tab/Escape while `completion.active`.

use editor_core::EditorState;
use editor_view::{CompletionKind, ViewState};
use egui::{Area, Color32, Frame, Id, Order, Pos2, Rect, ScrollArea, Stroke};

const MAX_VISIBLE_ITEMS: usize = 8;
const ROW_HEIGHT: f32 = 18.0;
const POPUP_WIDTH: f32 = 280.0;

/// Paint the completion popup, if active. Call AFTER `paint()` so the popup
/// draws above the editor body and any tooltips that share the same layer.
pub fn paint_completion_popup(
    ui: &mut egui::Ui,
    view: &ViewState,
    state: &EditorState,
    widget_rect: Rect,
) {
    if !view.completion.active || view.completion.items.is_empty() {
        return;
    }
    let anchor_screen = caret_screen_pos(view, state, widget_rect);
    let pos = Pos2::new(anchor_screen.x, anchor_screen.y + view.line_height + 2.0);

    let visuals = ui.visuals().clone();
    let bg = visuals.window_fill();
    let stroke = Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.6));

    Area::new(Id::new("editor_completion_popup"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::default()
                .fill(bg)
                .stroke(stroke)
                .corner_radius(4.0)
                .inner_margin(4.0)
                .show(ui, |ui| {
                    ui.set_width(POPUP_WIDTH);
                    let visible = view.completion.items.len().min(MAX_VISIBLE_ITEMS);
                    let max_h = ROW_HEIGHT * visible as f32 + 4.0;
                    ScrollArea::vertical()
                        .max_height(max_h)
                        .show(ui, |ui| paint_rows(ui, view, &visuals));
                });
        });
}

fn paint_rows(ui: &mut egui::Ui, view: &ViewState, visuals: &egui::Visuals) {
    let selected_bg = visuals.selection.bg_fill;
    let text_color = visuals.text_color();
    let detail_color = visuals.weak_text_color();
    for (i, item) in view.completion.items.iter().enumerate() {
        let is_selected = i == view.completion.selected;
        let row = ui.allocate_response(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::hover(),
        );
        let painter = ui.painter();
        if is_selected {
            painter.rect_filled(row.rect, 2.0, selected_bg);
        }
        let icon = kind_icon(item.kind);
        let label_text = format!("{icon}  {}", item.label);
        let label_color = if is_selected { Color32::WHITE } else { text_color };
        painter.text(
            row.rect.left_top() + egui::vec2(6.0, 2.0),
            egui::Align2::LEFT_TOP,
            label_text,
            egui::FontId::proportional(13.0),
            label_color,
        );
        if let Some(detail) = &item.detail {
            painter.text(
                row.rect.right_top() + egui::vec2(-6.0, 2.0),
                egui::Align2::RIGHT_TOP,
                detail.as_str(),
                egui::FontId::proportional(11.0),
                detail_color,
            );
        }
    }
}

fn kind_icon(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Snippet => "s",
        CompletionKind::Variable => "v",
        CompletionKind::Function => "f",
        CompletionKind::Keyword => "k",
        CompletionKind::Wikilink => "w",
        CompletionKind::Text => "t",
    }
}

/// Locate the caret in screen coordinates by walking the height map and
/// approximating column width (matching the painter's mono assumption).
fn caret_screen_pos(view: &ViewState, state: &EditorState, widget_rect: Rect) -> Pos2 {
    let pos = state.selection.main().head.offset().min(state.doc.len_bytes());
    let line = state.doc.byte_to_line(pos);
    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);
    let local = pos.saturating_sub(line_start).min(line_text.len());
    let col_chars = line_text[..local].chars().count();
    let char_w = view.font_size * 0.55;
    let x = widget_rect.min.x + view.gutter_width + col_chars as f32 * char_w;
    let y = widget_rect.min.y + view.line_top_y(line);
    Pos2::new(x, y)
}
