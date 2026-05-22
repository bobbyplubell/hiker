//! Panel rendering for the egui adapter. SPEC §9.21 + §9.13,
//! IMPLEMENTATION §16.6.13 + §16.6.4.
//!
//! The widget reserves vertical strips at the top / bottom of its rect for
//! registered panels (see `PanelStack::heights`). After painting the text
//! area, the widget calls [`paint_panels`] which lays out each panel in its
//! reserved strip.

use editor_view::panels::PanelKind;

use editor_view::panels::PanelPlacement;

use editor_view::viewport::ViewState;
use egui::{Pos2, Rect};

/// Paint all registered panels into the strips reserved at the top and
/// bottom of `widget_rect`. Top panels stack downward from `widget_rect.top()`;
/// bottom panels stack upward from `widget_rect.bottom()`.
pub fn paint_panels(
    ui: &mut egui::Ui,
    view: &mut ViewState,
    widget_rect: Rect,
    _top_h: f32,
    _bottom_h: f32,
) {
    let panels = view.panels.panels.clone();
    let mut top_cursor = widget_rect.top();
    let mut bottom_cursor = widget_rect.bottom();

    for panel in panels {
        let rect = match panel.placement {
            PanelPlacement::Top => {
                let r = Rect::from_min_max(
                    Pos2::new(widget_rect.left(), top_cursor),
                    Pos2::new(widget_rect.right(), top_cursor + panel.height),
                );
                top_cursor += panel.height;
                r
            }
            PanelPlacement::Bottom => {
                let r = Rect::from_min_max(
                    Pos2::new(widget_rect.left(), bottom_cursor - panel.height),
                    Pos2::new(widget_rect.right(), bottom_cursor),
                );
                bottom_cursor -= panel.height;
                r
            }
        };
        let mut pp = PanelPainter { ui, view };
        match &panel.kind {
            PanelKind::Search => pp.paint_search_panel(rect),
            PanelKind::Label(text) => pp.paint_label_panel(rect, text.as_str()),
        }
    }
}

struct PanelPainter<'a, 'b> {
    ui: &'a mut egui::Ui,
    view: &'b mut ViewState,
}

impl<'a, 'b> PanelPainter<'a, 'b> {
fn paint_label_panel(&mut self, rect: Rect, text: &str) {
    let visuals = self.ui.visuals().clone();
    let painter = self.ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.faint_bg_color);
    painter.text(
        Pos2::new(rect.left() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(12.0),
        visuals.text_color(),
    );
}

fn paint_search_panel(&mut self, rect: Rect) {
    let visuals = self.ui.visuals().clone();
    let painter = self.ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.window_fill);
    painter.line_segment(
        [Pos2::new(rect.left(), rect.top()), Pos2::new(rect.right(), rect.top())],
        egui::Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.5)),
    );

    let mut child = self.ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(egui::vec2(6.0, 4.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let mut sub = PanelPainter { ui: &mut child, view: self.view };
    sub.search_panel_widgets();
}

fn search_panel_widgets(&mut self) {
    let ui = &mut *self.ui;
    let view = &mut *self.view;
    let query_resp = ui.add(
        egui::TextEdit::singleline(&mut view.search.query)
            .desired_width(140.0)
            .hint_text("Find"),
    );
    let _ = ui.add(
        egui::TextEdit::singleline(&mut view.search.replacement)
            .desired_width(140.0)
            .hint_text("Replace"),
    );

    let case = view.search.flags.case_sensitive;
    if ui.selectable_label(case, "Aa").on_hover_text("Match case").clicked() {
        view.search.flags.case_sensitive = !case;
    }
    let ww = view.search.flags.whole_word;
    if ui.selectable_label(ww, "Ab|").on_hover_text("Whole word").clicked() {
        view.search.flags.whole_word = !ww;
    }
    let rx = view.search.flags.regex;
    if ui.selectable_label(rx, ".*").on_hover_text("Regex").clicked() {
        view.search.flags.regex = !rx;
    }

    if ui.button("Prev").clicked() {
        view.search.prev();
    }
    if ui.button("Next").clicked() {
        view.search.next();
    }
    let _ = ui.button("Replace");
    let _ = ui.button("Replace All");

    let total = view.search.matches.len();
    let cur = view
        .search
        .current_idx
        .map(|i| i + 1)
        .unwrap_or(0);
    ui.label(format!("{cur} of {total}"));

    if ui.button("x").on_hover_text("Close").clicked() {
        view.search.close();
    }

    // Enter inside the query field is a no-op here; the host re-runs search
    // each frame from `view.search.query` when it changes.
    let _ = query_resp;
}
}
