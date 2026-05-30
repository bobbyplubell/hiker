//! Renderer for the autocomplete popup (SPEC §9.6, IMPLEMENTATION §16.5.3).
//!
//! Draws `view.completion` as a floating `egui::Area` immediately below the
//! caret. v1 paints a scrollable list of `label`s with an optional `detail`
//! column; the selected row is highlighted. The popup absorbs no input
//! itself — keys are routed through the normal command pipeline, which
//! intercepts ArrowUp/Down/Enter/Tab/Escape while `completion.active`.

use editor_core::state::Editor as EditorState;
use editor_view::autocomplete::CompletionKind;
use editor_view::viewport::ViewState;
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
    let painter = CompletionPainter { view, state, widget_rect };
    painter.paint(ui);
}

struct CompletionPainter<'a> {
    view: &'a ViewState,
    state: &'a EditorState,
    widget_rect: Rect,
}

impl<'a> CompletionPainter<'a> {
    fn paint(&self, ui: &mut egui::Ui) {
        let anchor_screen = self.caret_screen_pos();
        let pos = Pos2::new(anchor_screen.x, anchor_screen.y + self.view.line_height + 2.0);

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
                        let visible = self.view.completion.items.len().min(MAX_VISIBLE_ITEMS);
                        let max_h = ROW_HEIGHT * visible as f32 + 4.0;
                        ScrollArea::vertical()
                            .max_height(max_h)
                            .show(ui, |ui| self.paint_rows(ui, &visuals));
                    });
            });
    }

    fn paint_rows(&self, ui: &mut egui::Ui, visuals: &egui::Visuals) {
        let selected_bg = visuals.selection.bg_fill;
        let text_color = visuals.text_color();
        let detail_color = visuals.weak_text_color();
        let label_font = egui::FontId::proportional(13.0);
        let detail_font = egui::FontId::proportional(11.0);
        for (i, item) in self.view.completion.items.iter().enumerate() {
            let is_selected = i == self.view.completion.selected;
            let row = ui.allocate_response(
                egui::vec2(ui.available_width(), ROW_HEIGHT),
                egui::Sense::hover(),
            );
            // Clip everything painted for this row to the row rect so a
            // long label / detail can never bleed past the card edges
            // (`wikilink-hover-preview` / wikilink autocomplete had detail
            // paths overflowing the 280px popup). Each row gets its own
            // clip-scoped painter.
            let painter = ui.painter().with_clip_rect(row.rect);
            if is_selected {
                painter.rect_filled(row.rect, 2.0, selected_bg);
            }
            let label_color = if is_selected { Color32::WHITE } else { text_color };
            let icon = self.kind_icon(item.kind);
            let label_text = format!("{icon}  {}", item.label);

            // Reserve the right portion for the detail (≈45% of the row),
            // and truncate each side to its budget so label + detail never
            // overlap. The detail is a path — keep its tail (the
            // distinguishing folder/name) and front-ellipsize.
            let inner_w = (row.rect.width() - 12.0).max(0.0);
            let detail_budget = if item.detail.is_some() {
                inner_w * 0.45
            } else {
                0.0
            };
            let label_budget = (inner_w - detail_budget - 8.0).max(0.0);

            let label_galley = ui.fonts(|f| {
                let mut job = egui::text::LayoutJob::simple_singleline(
                    label_text,
                    label_font.clone(),
                    label_color,
                );
                job.wrap = egui::text::TextWrapping::truncate_at_width(label_budget);
                f.layout_job(job)
            });
            painter.galley(
                row.rect.left_top() + egui::vec2(6.0, 2.0),
                label_galley,
                label_color,
            );

            if let Some(detail) = &item.detail {
                let shown = elide_front(detail.as_str(), &detail_font, detail_budget, ui);
                let detail_galley = ui.fonts(|f| {
                    let mut job = egui::text::LayoutJob::simple_singleline(
                        shown,
                        detail_font.clone(),
                        detail_color,
                    );
                    job.wrap = egui::text::TextWrapping::truncate_at_width(detail_budget);
                    f.layout_job(job)
                });
                let x = row.rect.right() - 6.0 - detail_galley.rect.width();
                painter.galley(
                    egui::pos2(x, row.rect.top() + 3.0),
                    detail_galley,
                    detail_color,
                );
            }
        }
    }

    const fn kind_icon(&self, kind: CompletionKind) -> &'static str {
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
    fn caret_screen_pos(&self) -> Pos2 {
        let state = self.state;
        let view = self.view;
        let pos = state.selection.main().head.offset().min(state.doc.len_bytes());
        let line = state.doc.byte_to_line(pos);
        let line_start = state.doc.line_to_byte(line);
        let line_text = state.doc.line_str(line);
        let local = pos.saturating_sub(line_start).min(line_text.len());
        let col_chars = line_text[..local].chars().count();
        let char_w = view.font_size * 0.55;
        let x = self.widget_rect.min.x + view.content_origin_x() + col_chars as f32 * char_w;
        let y = self.widget_rect.min.y + view.line_top_y(line);
        Pos2::new(x, y)
    }
}

/// Front-elide `text` (a path-like detail string) so its tail — the
/// distinguishing folder + name — survives when it doesn't fit in
/// `max_width`. Returns the original when it already fits; otherwise
/// trims leading path segments / chars and prepends `…`-as-ASCII
/// (`...`). Width is measured with `font` via egui's font layout so it
/// matches what gets painted.
fn elide_front(text: &str, font: &egui::FontId, max_width: f32, ui: &egui::Ui) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    let measure = |s: &str| -> f32 {
        ui.fonts(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), egui::Color32::WHITE)
                .rect
                .width()
        })
    };
    if measure(text) <= max_width {
        return text.to_string();
    }
    // Trim from the front by chars until "..." + tail fits.
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    while start < chars.len() {
        let candidate: String = std::iter::once('.')
            .chain(std::iter::once('.'))
            .chain(std::iter::once('.'))
            .chain(chars[start..].iter().copied())
            .collect();
        if measure(&candidate) <= max_width {
            return candidate;
        }
        start += 1;
    }
    "...".to_string()
}
