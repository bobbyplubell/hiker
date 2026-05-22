//! Structural minimap widget.
//!
//! Renders a narrow strip showing one colored bar per document line, where
//! the bar's color reflects the line's structural role (heading, code,
//! quote, plain) and its width reflects the visible (non-whitespace) line
//! length. A translucent rectangle marks the slice of the document
//! currently visible in the editor; click or drag scrolls the host editor
//! to that position.
//!
//! Classification is driven entirely by `ViewState::decorations`, the same
//! layers the editor paints from — so any decoration provider (markdown,
//! diff, search…) the host has already wired up automatically participates.
//! The minimap reads but does not produce decorations.

use egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_view::viewport::ViewState;

/// What a given doc line looks like structurally. Higher variants beat
/// lower ones when multiple decorations overlap the same line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LineKind {
    Hidden,
    Plain,
    Quote,
    Code,
    Emphasis,
    Heading,
}

/// Visual + behavior knobs. All sizes are pixels. Defaults match the
/// previous hard-coded look so a freshly-defaulted `Options`
/// renders identically to the original widget.
#[derive(Clone, Debug)]
pub struct Options {
    pub width: f32,
    pub bar_padding_left: f32,
    pub bar_padding_right: f32,
    pub bar_corner_radius: f32,
    pub min_bar_width: f32,
    /// Vertical gap between consecutive bars, in pixels (fractional).
    pub bar_gap: f32,
    pub colored: bool,
    pub show_section_rules: bool,
    pub show_viewport: bool,
    pub show_left_edge: bool,
    pub color_heading: Color32,
    pub color_code: Color32,
    pub color_emphasis: Color32,
    pub color_quote: Color32,
    pub color_plain: Color32,
    pub color_background: Color32,
    pub color_section_rule: Color32,
    pub color_viewport: Color32,
    pub color_viewport_hover: Color32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            width: 72.0,
            bar_padding_left: 5.0,
            bar_padding_right: 5.0,
            bar_corner_radius: 1.0,
            min_bar_width: 2.0,
            bar_gap: 0.5,
            colored: true,
            show_section_rules: true,
            show_viewport: true,
            show_left_edge: true,
            color_heading: Color32::from_rgba_premultiplied(60, 122, 220, 240),
            color_code: Color32::from_rgba_premultiplied(60, 149, 197, 220),
            color_emphasis: Color32::from_rgba_premultiplied(201, 138, 60, 220),
            color_quote: Color32::from_rgba_premultiplied(122, 133, 165, 160),
            color_plain: Color32::from_rgba_premultiplied(106, 111, 128, 180),
            color_background: Color32::from_rgba_premultiplied(0, 0, 0, 20),
            color_section_rule: Color32::from_rgba_premultiplied(0, 0, 0, 28),
            color_viewport: Color32::from_rgba_premultiplied(60, 100, 180, 28),
            color_viewport_hover: Color32::from_rgba_premultiplied(60, 100, 180, 50),
        }
    }
}

impl LineKind {
    const fn color(self, opts: &Options) -> Color32 {
        if !opts.colored {
            return match self {
                LineKind::Hidden => Color32::TRANSPARENT,
                _ => opts.color_plain,
            };
        }
        match self {
            LineKind::Hidden => Color32::TRANSPARENT,
            LineKind::Plain => opts.color_plain,
            LineKind::Quote => opts.color_quote,
            LineKind::Code => opts.color_code,
            LineKind::Emphasis => opts.color_emphasis,
            LineKind::Heading => opts.color_heading,
        }
    }
}

pub struct Widget<'a> {
    state: &'a EditorState,
    view: &'a mut ViewState,
    opts: Options,
}

impl<'a> Widget<'a> {
    pub fn new(state: &'a EditorState, view: &'a mut ViewState) -> Self {
        Self { state, view, opts: Options::default() }
    }

    /// Backwards-compatible shortcut for `with_options({ width, ..default })`.
    pub const fn with_width(mut self, width: f32) -> Self {
        self.opts.width = width;
        self
    }

    pub const fn with_options(mut self, opts: Options) -> Self {
        self.opts = opts;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let kinds = self.classify_lines();
        let metrics = self.measure_lines();
        let opts = self.opts.clone();
        let state = &*self.state;
        let view = &mut *self.view;
        let height = ui.available_height().max(0.0);
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(opts.width, height), Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        painter.rect_filled(rect, 0.0, opts.color_background);
        if opts.show_left_edge {
            // Reuse the section-rule color for the gutter rule — it's the
            // same "subtle separator" role, so users only tune one knob to
            // change both.
            painter.line_segment(
                [rect.left_top(), rect.left_bottom()],
                Stroke::new(1.0, opts.color_section_rule),
            );
        }

        let line_count = state.doc.len_lines();
        if line_count == 0 || height <= 1.0 {
            return response;
        }

        // Project on the real content axis from `height_map`. After the
        // host editor's per-frame `prewrap_visible` + `apply_line_height_
        // decorations`, the height map's `total_height()`, `y_at_text()`,
        // and `text_height()` reflect soft-wrap multipliers, heading
        // scale, block widgets and hidden lines for every buffer line —
        // so the minimap stays in lockstep with what the user actually
        // sees in the editor (and `scroll_y` is in the same units).
        let line_h = view.line_height.max(1.0);
        let total_content = view
            .height_map
            .total_height()
            .max(line_count as f32 * line_h)
            .max(1.0);
        let scale = rect.height() / total_content;

        let max_visible = metrics
            .iter()
            .map(|m| m.visible)
            .max()
            .unwrap_or(1)
            .max(1) as f32;

        let bar_pad_l = opts.bar_padding_left;
        let bar_pad_r = opts.bar_padding_right;
        let usable_w = (rect.width() - bar_pad_l - bar_pad_r).max(1.0);
        let bar_radius = CornerRadius::same(opts.bar_corner_radius.clamp(0.0, 8.0) as u8);

        let mut prev_y_px: f32 = -1.0;
        for (line, m) in metrics.iter().enumerate().take(line_count) {
            let kind = kinds.get(line).copied().unwrap_or(LineKind::Plain);
            if kind == LineKind::Hidden {
                continue;
            }
            let m = *m;
            // Use the real per-line geometry so wrapped lines occupy
            // proportionally more vertical space in the minimap, just
            // like they do in the editor viewport.
            let line_full_h = view.height_map.text_height(line).max(0.0);
            if line_full_h <= 0.0 {
                continue;
            }
            let y_px = rect.top() + view.height_map.y_at_text(line) * scale;
            let h_px = (line_full_h * scale).max(1.0);
            if h_px < 1.0 && y_px - prev_y_px < 1.0 {
                continue;
            }
            prev_y_px = y_px;

            if kind == LineKind::Heading && opts.show_section_rules {
                let y = (y_px - 1.0).max(rect.top());
                painter.hline(
                    (rect.left() + bar_pad_l - 1.0)..=(rect.right() - bar_pad_r + 1.0),
                    y,
                    Stroke::new(1.0, opts.color_section_rule),
                );
            }

            if m.visible == 0 && m.indent == 0 {
                continue;
            }

            let indent_frac = (m.indent as f32) / max_visible;
            let visible_frac = (m.visible as f32) / max_visible;
            let bar_x = rect.left() + bar_pad_l + indent_frac * usable_w;
            let bar_w = (visible_frac * usable_w).max(opts.min_bar_width);
            let bar_h = (h_px - opts.bar_gap).max(1.0);
            let bar_rect = Rect::from_min_size(Pos2::new(bar_x, y_px), Vec2::new(bar_w, bar_h));
            painter.rect_filled(bar_rect, bar_radius, kind.color(&opts));
        }

        // Thumb + click both project on the real content axis. `scroll_y`
        // and `view.height` are in the same units as `total_content`, so
        // `scroll_y / total_content` is the fraction of the document that
        // sits above the viewport and `view.height / total_content` is
        // the fraction visible — these reflect soft-wrap and tall
        // lines (headings, blocks) the same way the editor does.
        if opts.show_viewport {
            let active = response.hovered() || response.dragged();
            let thumb_fill = if active {
                opts.color_viewport_hover
            } else {
                opts.color_viewport
            };
            let thumb_stroke = {
                // Brighten the stroke relative to the fill so the
                // viewport box reads as a framed rect, not a faint blur.
                let a = (thumb_fill.a() as f32 * 2.2).clamp(0.0, 255.0) as u8;
                Color32::from_rgba_unmultiplied(thumb_fill.r(), thumb_fill.g(), thumb_fill.b(), a)
            };
            let frac_top = (view.scroll_y / total_content).clamp(0.0, 1.0);
            let frac_h = (view.height / total_content).clamp(0.0, 1.0);
            let vp_y = rect.top() + frac_top * rect.height();
            let vp_h = (frac_h * rect.height()).max(8.0);
            let vp_rect = Rect::from_min_size(
                Pos2::new(rect.left() + 1.0, vp_y),
                Vec2::new(rect.width() - 1.0, vp_h),
            );
            painter.rect_filled(vp_rect, CornerRadius::same(2), thumb_fill);
            painter.rect_stroke(
                vp_rect,
                CornerRadius::same(2),
                Stroke::new(1.0, thumb_stroke),
                egui::StrokeKind::Inside,
            );
        }

        // Scroll-on-press: react the moment the primary button goes down
        // anywhere on the strip. `clicked()` doesn't fire until release
        // and `dragged()` waits for the drag threshold — neither matches
        // VSCode's "snap immediately" feel. `is_pointer_button_down_on`
        // is true for every frame the button is held inside the widget,
        // so this also covers the drag-to-scroll case naturally.
        if let Some(pos) = response.interact_pointer_pos()
            && response.is_pointer_button_down_on()
        {
            let frac = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
            let target_center = frac * total_content;
            let mut target = target_center - view.height * 0.5;
            let max_scroll = (total_content - view.height).max(0.0);
            if target < 0.0 {
                target = 0.0;
            }
            if target > max_scroll {
                target = max_scroll;
            }
            view.scroll_y = target;
        }

        response
    }
}

/// Per-line "how much of the line is visible content vs. leading
/// whitespace", in bytes. Bytes are a fine proxy at minimap scale and let
/// us avoid a UTF-8 walk per line.
#[derive(Clone, Copy, Default)]
struct LineMetrics {
    indent: u32,
    visible: u32,
}

impl<'a> Widget<'a> {
fn measure_lines(&self) -> Vec<LineMetrics> {
    let state = &*self.state;
    let line_count = state.doc.len_lines();
    let mut out = Vec::with_capacity(line_count);
    for line in 0..line_count {
        let s = state.doc.line_str(line);
        let total = s.trim_end_matches(['\n', '\r']).len() as u32;
        let indent = s
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count() as u32;
        let visible = total.saturating_sub(indent);
        out.push(LineMetrics { indent, visible });
    }
    out
}

/// Walk every decoration layer the host has pushed and assign each line
/// the highest-priority kind that overlaps it. This is the shared path
/// with the editor's syntax pipeline — anything the editor highlights
/// shows up here automatically.
fn classify_lines(&self) -> Vec<LineKind> {
    let state = &*self.state;
    let view = &*self.view;
    let line_count = state.doc.len_lines();
    let mut out = vec![LineKind::Plain; line_count];
    if line_count == 0 {
        return out;
    }
    let doc_len = state.doc.len_bytes();

    let promote = |slot: &mut LineKind, kind: LineKind| {
        if kind > *slot {
            *slot = kind;
        }
    };

    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(0..doc_len) {
            let lo = state.doc.byte_to_line(range.start.min(doc_len));
            let hi = state
                .doc
                .byte_to_line(range.end.saturating_sub(1).max(range.start).min(doc_len));
            let kind = match deco {
                Decoration::Mark(m) => {
                    if m.font_scale.map(|s| s > 1.05).unwrap_or(false) || m.bold {
                        Some(LineKind::Heading)
                    } else if m.monospace {
                        Some(LineKind::Code)
                    } else if m.bg.is_some() {
                        Some(LineKind::Emphasis)
                    } else {
                        None
                    }
                }
                Decoration::Line(l) => {
                    if l.hide {
                        Some(LineKind::Hidden)
                    } else if l.bg.is_some() {
                        Some(LineKind::Quote)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let Some(kind) = kind else { continue };
            for slot in out.iter_mut().take(hi.min(line_count - 1) + 1).skip(lo) {
                if kind == LineKind::Hidden {
                    *slot = LineKind::Hidden;
                } else if *slot != LineKind::Hidden {
                    promote(slot, kind);
                }
            }
        }
    }
    out
}
}
