//! Frame painting: walks the visible band, paints per-line backgrounds,
//! selections, gutter, segments, cursors, and drop carets. Pulls geometry from
//! the heightmap built during the measure phase.

use std::sync::Arc;

use editor_core::{
    BlockSide, Color, Decoration, EditorState, InlineWidget, LineStyle,
};
use editor_view::view::DragState;
use editor_view::{ClickAction, ClickRect, ClickZone, ViewState};
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Stroke};

use super::blocks::paint_block_zone;
use super::layout::{build_line_layout, LineLayout, LineMeasured};
use super::{
    compute_metrics_fingerprint, hash_str, to_egui_color, CachedRow, HeightMapExt, PaintCache,
};

pub(super) struct PaintCtx<'a> {
    pub(super) ui: &'a mut egui::Ui,
    pub(super) painter: egui::Painter,
    pub(super) rect: Rect,
    pub(super) text_origin_x: f32,
    pub(super) base_font_id: FontId,
    pub(super) text_color: Color32,
    pub(super) selection_color: Color32,
    pub(super) cursor_color: Color32,
    pub(super) gutter_color: Color32,
    pub(super) hatched_default: Color,
    pub(super) has_focus: bool,
}

pub(super) fn paint(
    ui: &mut egui::Ui,
    state: &EditorState,
    view: &mut ViewState,
    cache: &mut PaintCache,
    rect: Rect,
    has_focus: bool,
) {
    // Bump the paint-cache frame counter once per paint, so per-row hits
    // refresh `last_used_frame` and we can evict rows that fell off-screen.
    cache.frame = cache.frame.wrapping_add(1);
    // Evict rows untouched for more than ~120 frames (≈2 seconds at 60 Hz).
    cache.evict_stale(120);

    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);

    // Placeholder text: when the doc is empty and a placeholder is set, paint
    // it dimmed at the text origin and return early. See SPEC §9.12.
    if state.doc.is_empty() {
        if let Some(placeholder) = view.placeholder.clone() {
            let text_origin_x =
                rect.left() + if view.hide_gutter { 4.0 } else { view.gutter_width };
            let font_id = FontId::new(view.font_size, FontFamily::Monospace);
            let dim = visuals.weak_text_color();
            painter.text(
                Pos2::new(text_origin_x, rect.top()),
                egui::Align2::LEFT_TOP,
                placeholder.as_str(),
                font_id,
                dim,
            );
            if has_focus {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            return;
        }
    }

    let hatched_default = if visuals.dark_mode {
        Color::rgba(180, 180, 200, 30)
    } else {
        Color::rgba(120, 120, 140, 45)
    };
    let mut ctx = PaintCtx {
        ui,
        painter,
        rect,
        text_origin_x: rect.left() + if view.hide_gutter { 4.0 } else { view.gutter_width },
        base_font_id: FontId::new(view.font_size, FontFamily::Monospace),
        text_color: visuals.text_color(),
        // egui's `visuals.selection.bg_fill` is opaque and tuned for
        // filling button shapes — used directly here it would obscure
        // the glyphs underneath. We want a translucent tint instead.
        // `linear_multiply(0.5)` was the previous attempt, but Color32
        // is premultiplied, so multiplying scales the alpha too —
        // dropping it to ~50% of an already-dark color on a dark
        // background made the selection visually disappear. Build a
        // fixed-alpha overlay from the theme accent instead.
        selection_color: {
            let s = visuals.selection.bg_fill;
            Color32::from_rgba_unmultiplied(s.r(), s.g(), s.b(), 110)
        },
        cursor_color: visuals.text_color(),
        gutter_color: visuals.weak_text_color(),
        hatched_default,
        has_focus,
    };

    for line_idx in view.visible_lines() {
        if line_idx >= state.doc.len_lines() {
            break;
        }
        paint_visible_line(&mut ctx, state, view, cache, line_idx);
    }

    if let DragState::DraggingSelection { drop_caret } = view.drag {
        paint_drop_caret(&mut ctx, state, view, drop_caret);
    }

    if !view.hide_gutter {
        let sep_x = rect.left() + view.gutter_width - 2.0;
        ctx.painter.line_segment(
            [Pos2::new(sep_x, rect.top()), Pos2::new(sep_x, rect.bottom())],
            Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.3)),
        );
    }
    if has_focus {
        ctx.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// Paint a thin vertical "drop indicator" caret at `drop_caret` (byte
/// offset). Used during text drag-and-drop to show where a release would
/// insert the dragged text. SPEC §9.19.
fn paint_drop_caret(
    ctx: &mut PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    drop_caret: usize,
) {
    if state.doc.len_lines() == 0 {
        return;
    }
    let clamped = drop_caret.min(state.doc.len_bytes());
    let line = state.doc.byte_to_line(clamped);
    if line >= state.doc.len_lines() {
        return;
    }
    let row_top_y = ctx.rect.top() + view.line_top_y(line);
    let above_h = view.height_map.block_above(line);
    let text_top_y = row_top_y + above_h;
    let row_h = view.height_map.text_height(line).max(view.line_height);

    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);
    let local = clamped.saturating_sub(line_start).min(line_text.len());
    let prefix = &line_text[..local];
    let font_id = ctx.base_font_id.clone();
    let galley = ctx
        .ui
        .fonts(|f| f.layout_no_wrap(prefix.to_string(), font_id, Color32::WHITE));
    let x = ctx.text_origin_x + galley.size().x;

    let color = Color32::from_rgb(0, 150, 200);
    let r = Rect::from_min_max(
        Pos2::new(x - 0.75, text_top_y),
        Pos2::new(x + 0.75, text_top_y + row_h),
    );
    ctx.painter.rect_filled(r, 0.0, color);
}

fn paint_visible_line(ctx: &mut PaintCtx<'_>, state: &EditorState, view: &mut ViewState, cache: &mut PaintCache, line_idx: usize) {
    let row_top_y = ctx.rect.top() + view.line_top_y(line_idx);
    let above_h = view.height_map.block_above(line_idx);
    let below_h = view.height_map.block_below(line_idx);
    let line_top_y = row_top_y + above_h;
    let row_height = view.row_height(line_idx);
    let line_text = state.doc.line_str(line_idx);
    let line_byte_start = state.doc.line_to_byte(line_idx);
    let line_byte_end = line_byte_start + line_text.len();
    let is_hidden = view.height_map.text_height(line_idx) <= 0.5;

    if above_h > 0.0 {
        let zone_rect = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), row_top_y),
            Pos2::new(ctx.rect.right(), row_top_y + above_h),
        );
        paint_block_zone(
            ctx.ui, &ctx.painter, &view.decorations.layers,
            line_byte_start, line_byte_end, BlockSide::Above,
            zone_rect, view.font_size, view.line_height,
            ctx.text_origin_x, ctx.hatched_default,
            &mut view.click_zones, ctx.rect,
        );
    }

    if !is_hidden {
        let vlines: Vec<(usize, usize)> = if view.wrap_map.enabled() {
            view.wrap_map
                .peek(line_idx)
                .map(|w| w.vlines.iter().map(|(s, e)| (*s as usize, *e as usize)).collect())
                .unwrap_or_else(|| vec![(0usize, line_text.len())])
        } else {
            vec![(0usize, line_text.len())]
        };
        let vline_count = vlines.len().max(1) as f32;
        // For scaled lines (markdown headings) the heightmap allocates
        // `scale * line_height` vertical space. Each vline gets an equal
        // share so the gutter number + segment text center inside the
        // line's actual extent rather than hugging the top.
        let row_h = (view.height_map.text_height(line_idx) / vline_count)
            .max(view.line_height);
        for (vi, (vs, ve)) in vlines.iter().enumerate() {
            let vline_top_y = line_top_y + (vi as f32) * row_h;
            let vline_byte_start = line_byte_start + *vs;
            let vline_byte_end = line_byte_start + *ve;
            let vline_text = &line_text[*vs..*ve];
            let is_first_vline = vi == 0;
            paint_text_row(
                ctx, state, view, cache, line_idx, vline_text,
                vline_byte_start, vline_byte_end, vline_top_y, row_h, is_first_vline,
            );
        }
    }

    if below_h > 0.0 {
        let zone_top = if is_hidden { line_top_y } else { line_top_y + row_height };
        let zone_rect = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), zone_top),
            Pos2::new(ctx.rect.right(), zone_top + below_h),
        );
        paint_block_zone(
            ctx.ui, &ctx.painter, &view.decorations.layers,
            line_byte_start, line_byte_end, BlockSide::Below,
            zone_rect, view.font_size, view.line_height,
            ctx.text_origin_x, ctx.hatched_default,
            &mut view.click_zones, ctx.rect,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text_row(
    ctx: &mut PaintCtx<'_>,
    state: &EditorState,
    view: &mut ViewState,
    cache: &mut PaintCache,
    line_idx: usize,
    vline_text: &str,
    vline_byte_start: usize,
    vline_byte_end: usize,
    vline_top_y: f32,
    row_h: f32,
    is_first_vline: bool,
) {
    // Fingerprint inputs that, if any changes, invalidate the cached layout
    // for this row. text_hash catches in-place edits to a same-length line;
    // doc_id catches buffer mutations; sel_line catches cursor-on-line
    // reveal decorations; layers_sig catches changed decoration sets;
    // metrics catches font/size/width changes.
    let text_hash = hash_str(vline_text);
    let doc_id = state.doc.content_id() as u64;
    let sel_line = state
        .doc
        .byte_to_line(state.selection.main().head.offset().min(state.doc.len_bytes()))
        as u64;
    let layers_sig = view.decorations.signature;
    let metrics = compute_metrics_fingerprint(view);
    let key = (line_idx, vline_byte_start);

    // Cache lookup. On a hit we clone the stored layout/measured (cheap —
    // galleys are Arc<Galley>, segments hold SmolStr + Arc<dyn InlineWidget>).
    let frame = cache.frame;
    let hit = cache
        .entries
        .get(&key)
        .filter(|e| {
            e.text_hash == text_hash
                && e.doc_id == doc_id
                && e.sel_line == sel_line
                && e.layers_sig == layers_sig
                && e.metrics == metrics
        })
        .map(|e| (e.layout.clone(), e.measured.clone()));

    let (layout, measured) = if let Some((l, m)) = hit {
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.last_used_frame = frame;
        }
        (l, m)
    } else {
        // Build fresh.
        let layout = build_line_layout(
            vline_text, vline_byte_start, &view.decorations.layers,
            view.font_size, ctx.text_color,
        );
        let measured = layout.measure(ctx.ui);
        cache.entries.insert(
            key,
            CachedRow {
                last_used_frame: cache.frame,
                text_hash,
                doc_id,
                sel_line,
                layers_sig,
                metrics,
                layout: layout.clone(),
                measured: measured.clone(),
            },
        );
        (layout, measured)
    };

    if is_first_vline {
        paint_line_bgs(ctx, state, view, line_idx, vline_byte_start, vline_byte_end, vline_top_y, row_h);
        if !view.hide_gutter {
            paint_gutter(ctx, view, line_idx, vline_byte_start, vline_byte_end, vline_top_y, row_h);
        }
    } else if let Some(bg) = wrapped_continuation_bg(state, view, line_idx) {
        let r = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), vline_top_y),
            Pos2::new(ctx.rect.right(), vline_top_y + row_h),
        );
        ctx.painter.rect_filled(r, 0.0, bg);
    }
    paint_selections(
        ctx, state, view, line_idx, vline_byte_start, vline_byte_end,
        vline_top_y, row_h, &measured, vline_text.len(),
    );
    paint_segments(ctx, &layout, &measured, vline_top_y, row_h, &mut view.click_zones);
    paint_cursors(ctx, state, vline_byte_start, vline_byte_end, vline_top_y, row_h, &measured);
}

/// If the buffer line has a Line bg, continuation vlines (wrap rows after the
/// first) should also paint that bg so the highlight runs through.
fn wrapped_continuation_bg(state: &EditorState, view: &ViewState, line_idx: usize) -> Option<Color32> {
    let line_byte_start = state.doc.line_to_byte(line_idx);
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_start + 1) {
            if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                if state.doc.byte_to_line(range.start) == line_idx {
                    return Some(to_egui_color(*c));
                }
            }
        }
    }
    None
}

fn paint_line_bgs(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
) {
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
            if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                if state.doc.byte_to_line(range.start) == line_idx {
                    let r = Rect::from_min_max(
                        Pos2::new(ctx.rect.left(), line_top_y),
                        Pos2::new(ctx.rect.right(), line_top_y + row_height),
                    );
                    ctx.painter.rect_filled(r, 0.0, to_egui_color(*c));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_selections(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
    measured: &LineMeasured,
    line_text_len: usize,
) {
    for r in state.selection.ranges() {
        let (s, e) = (r.start(), r.end());
        if e < line_byte_start || s > line_byte_end {
            continue;
        }
        let local_start = s.saturating_sub(line_byte_start);
        let local_end = (e - line_byte_start).min(line_text_len);
        if !r.is_empty() {
            let x_start = measured.x_at_buffer_offset(local_start);
            let x_end = measured.x_at_buffer_offset(local_end);
            let sel = Rect::from_min_max(
                Pos2::new(ctx.text_origin_x + x_start, line_top_y),
                Pos2::new(ctx.text_origin_x + x_end, line_top_y + row_height),
            );
            ctx.painter.rect_filled(sel, 0.0, ctx.selection_color);
        }
        if e > line_byte_end && line_idx + 1 < state.doc.len_lines() {
            let x_end = measured.total_width;
            let extra = Rect::from_min_max(
                Pos2::new(ctx.text_origin_x + x_end, line_top_y),
                Pos2::new(ctx.text_origin_x + x_end + view.font_size * 0.5, line_top_y + row_height),
            );
            ctx.painter.rect_filled(extra, 0.0, ctx.selection_color);
        }
    }
}

fn paint_gutter(
    ctx: &PaintCtx<'_>,
    view: &mut ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
) {
    let num = (line_idx + 1).to_string();
    let num_galley = ctx
        .ui
        .fonts(|f| f.layout_no_wrap(num, ctx.base_font_id.clone(), ctx.gutter_color));
    let num_x = ctx.rect.left() + view.gutter_width - num_galley.size().x - 6.0;
    let num_y = line_top_y + (row_height - num_galley.size().y) * 0.5;
    ctx.painter
        .galley(Pos2::new(num_x, num_y), num_galley, ctx.gutter_color);

    if let Some(ch) = collect_fold_chevron(&view.decorations.layers, line_byte_start, line_byte_end) {
        // Draw chevron as a small filled triangle via Painter::add(Shape::convex_polygon)
        // — Unicode triangle glyphs aren't reliably present in egui's bundled
        // fonts (would render as the missing-glyph box).
        let size = ctx.base_font_id.size * 0.42;
        let cx = ctx.rect.left() + 9.0;
        let cy = line_top_y + row_height * 0.5;
        let points = if ch.collapsed {
            // ▶ — points right
            vec![
                Pos2::new(cx - size * 0.5, cy - size * 0.6),
                Pos2::new(cx - size * 0.5, cy + size * 0.6),
                Pos2::new(cx + size * 0.6, cy),
            ]
        } else {
            // ▼ — points down
            vec![
                Pos2::new(cx - size * 0.6, cy - size * 0.4),
                Pos2::new(cx + size * 0.6, cy - size * 0.4),
                Pos2::new(cx, cy + size * 0.6),
            ]
        };
        ctx.painter.add(egui::Shape::convex_polygon(
            points,
            ctx.gutter_color,
            Stroke::NONE,
        ));
        view.click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: 0.0,
                y_min: line_top_y - ctx.rect.min.y,
                x_max: 18.0,
                y_max: line_top_y - ctx.rect.min.y + row_height,
            },
            action: ClickAction::ToggleFold(ch.id),
        });
    }
}

fn paint_segments(
    ctx: &PaintCtx<'_>,
    layout: &LineLayout,
    measured: &LineMeasured,
    line_top_y: f32,
    row_height: f32,
    click_zones: &mut Vec<ClickZone>,
) {
    for (idx, seg) in layout.segments.iter().enumerate() {
        let g = &measured.galleys[idx];
        let seg_x = ctx.text_origin_x + measured.x_starts[idx];
        let seg_w = measured.seg_widths[idx];
        let seg_y = line_top_y + (row_height - g.size().y) * 0.5;
        let fg = seg.style.fg.map(to_egui_color).unwrap_or(ctx.text_color);

        if let Some(widget) = &seg.widget {
            paint_inline_widget_placeholder(
                ctx, widget, seg_x, line_top_y, seg_w, row_height,
                g, seg_y, click_zones,
            );
            continue;
        }

        if let Some(bg) = seg.style.bg {
            let bg_rect = Rect::from_min_max(
                Pos2::new(seg_x, line_top_y),
                Pos2::new(seg_x + seg_w, line_top_y + row_height),
            );
            ctx.painter.rect_filled(bg_rect, 0.0, to_egui_color(bg));
        }
        ctx.painter.galley(Pos2::new(seg_x, seg_y), g.clone(), fg);
        if seg.style.bold {
            ctx.painter.galley(Pos2::new(seg_x + 0.5, seg_y), g.clone(), fg);
        }
        if seg.style.underline {
            let y = seg_y + g.size().y * 0.92;
            ctx.painter.line_segment(
                [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                Stroke::new(1.0, fg),
            );
        }
        if seg.style.strikethrough {
            let y = seg_y + g.size().y * 0.5;
            ctx.painter.line_segment(
                [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                Stroke::new(1.0, fg),
            );
        }
    }
}

/// v1 placeholder render for an inline widget decoration: a styled rect of
/// the widget's measured size plus a tiny "widget" label. Real per-widget
/// painting is deferred to a future trait method.
#[allow(clippy::too_many_arguments)]
fn paint_inline_widget_placeholder(
    ctx: &PaintCtx<'_>,
    widget: &Arc<dyn InlineWidget>,
    seg_x: f32,
    line_top_y: f32,
    seg_w: f32,
    row_height: f32,
    label_galley: &Arc<egui::Galley>,
    label_y: f32,
    click_zones: &mut Vec<ClickZone>,
) {
    let visuals = ctx.ui.style().visuals.clone();
    let bg = if visuals.dark_mode {
        Color32::from_rgba_unmultiplied(70, 80, 110, 80)
    } else {
        Color32::from_rgba_unmultiplied(210, 220, 240, 220)
    };
    let border = visuals.weak_text_color().gamma_multiply(0.5);
    let rect = Rect::from_min_max(
        Pos2::new(seg_x, line_top_y),
        Pos2::new(seg_x + seg_w, line_top_y + row_height),
    );
    ctx.painter.rect_filled(rect, 3.0, bg);
    ctx.painter
        .rect_stroke(rect, 3.0, Stroke::new(0.5, border), egui::StrokeKind::Inside);
    ctx.painter
        .galley(Pos2::new(seg_x + 2.0, label_y), label_galley.clone(), border);

    if widget.handles_click() {
        click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: seg_x - ctx.rect.min.x,
                y_min: line_top_y - ctx.rect.min.y,
                x_max: seg_x + seg_w - ctx.rect.min.x,
                y_max: line_top_y + row_height - ctx.rect.min.y,
            },
            action: ClickAction::WidgetClick(widget.widget_id()),
        });
    }
}

fn paint_cursors(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
    measured: &LineMeasured,
) {
    for r in state.selection.ranges() {
        let head = r.head.offset();
        if head < line_byte_start || head > line_byte_end {
            continue;
        }
        let local = head - line_byte_start;
        let x = measured.x_at_buffer_offset(local);
        let cursor_rect = Rect::from_min_max(
            Pos2::new(ctx.text_origin_x + x - 0.5, line_top_y),
            Pos2::new(ctx.text_origin_x + x + 1.0, line_top_y + row_height),
        );
        let color = if ctx.has_focus { ctx.cursor_color } else { ctx.gutter_color };
        ctx.painter.rect_filled(cursor_rect, 0.0, color);
    }
}

fn collect_fold_chevron(
    layers: &[editor_core::DecorationSet],
    line_start: usize,
    line_end: usize,
) -> Option<editor_core::FoldChevron> {
    for layer in layers {
        for (range, deco) in layer.iter_overlapping(line_start..line_end + 1) {
            if let Decoration::Line(ls) = deco {
                if range.start == line_start {
                    if let Some(ch) = ls.fold_chevron {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}
