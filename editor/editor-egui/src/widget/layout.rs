//! Line layout + measurement: building `Segment`s from buffer text +
//! decorations, then laying them out into per-segment galleys with x positions.

use std::sync::Arc;

use editor_core::{Color, Decoration, InlineWidget, MarkStyle};
use egui::{
    epaint::text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId,
};
use smol_str::SmolStr;

/// A single visual line built from buffer text + overlapping decorations.
#[derive(Clone)]
pub(super) struct LineLayout {
    pub(super) segments: Vec<Segment>,
    pub(super) base_font_size: f32,
    pub(super) base_color: Color32,
}

#[derive(Clone)]
pub(super) struct Segment {
    pub(super) display: SmolStr,
    pub(super) buffer_range: std::ops::Range<usize>,
    pub(super) style: MarkStyle,
    pub(super) is_replacement: bool,
    /// When present, this segment renders an inline widget placeholder of the
    /// widget's measured size rather than text. v1 limitation: the egui
    /// adapter does not call into the widget for painting; instead it draws a
    /// styled rect with a small "widget" label.
    pub(super) widget: Option<Arc<dyn InlineWidget>>,
}

/// Per-segment measured galleys + x positions for one frame.
#[derive(Clone)]
pub(super) struct LineMeasured {
    pub(super) galleys: Vec<Arc<egui::Galley>>,
    pub(super) x_starts: Vec<f32>,
    /// Width used to advance for this segment; equals galley width for text,
    /// or the widget's measured width for inline-widget segments.
    pub(super) seg_widths: Vec<f32>,
    pub(super) total_width: f32,
    /// Mirrors `LineLayout::segments[i].buffer_range.start - line_start`.
    pub(super) seg_buffer_starts: Vec<usize>,
    pub(super) seg_buffer_ends: Vec<usize>,
    pub(super) seg_is_replacement: Vec<bool>,
}

impl LineMeasured {
    pub(super) fn x_at_buffer_offset(&self, line_local_byte: usize) -> f32 {
        for (i, &start) in self.seg_buffer_starts.iter().enumerate() {
            let end = self.seg_buffer_ends[i];
            if line_local_byte < start {
                return self.x_starts[i];
            }
            if line_local_byte <= end {
                let seg_x = self.x_starts[i];
                if self.seg_is_replacement[i] {
                    if line_local_byte == end {
                        return seg_x + self.seg_widths[i];
                    }
                    return seg_x;
                }
                // Walk display chars to find x within the galley.
                let g = &self.galleys[i];
                let display = g.text();
                let local_in_seg = line_local_byte - start;
                let safe = local_in_seg.min(display.len());
                let char_idx = display[..safe].chars().count();
                let ccursor = egui::text::CCursor::new(char_idx);
                return seg_x + g.pos_from_cursor(ccursor).min.x;
            }
        }
        self.total_width
    }
}

impl LineLayout {
    pub(super) fn measure(&self, ui: &egui::Ui) -> LineMeasured {
        let mut galleys = Vec::with_capacity(self.segments.len());
        let mut x_starts = Vec::with_capacity(self.segments.len());
        let mut seg_widths = Vec::with_capacity(self.segments.len());
        let mut seg_buffer_starts = Vec::with_capacity(self.segments.len());
        let mut seg_buffer_ends = Vec::with_capacity(self.segments.len());
        let mut seg_is_replacement = Vec::with_capacity(self.segments.len());
        let base = self.line_base();
        let mut x = 0.0f32;
        for seg in &self.segments {
            let g = segment_galley(ui, seg, self.base_font_size, self.base_color);
            let w = if let Some(widget) = &seg.widget {
                widget.measure(self.base_font_size).0.max(g.size().x)
            } else {
                g.size().x
            };
            x_starts.push(x);
            seg_widths.push(w);
            x += w;
            galleys.push(g);
            seg_buffer_starts.push(seg.buffer_range.start - base);
            seg_buffer_ends.push(seg.buffer_range.end - base);
            seg_is_replacement.push(seg.is_replacement);
        }
        LineMeasured {
            galleys,
            x_starts,
            seg_widths,
            total_width: x,
            seg_buffer_starts,
            seg_buffer_ends,
            seg_is_replacement,
        }
    }

    fn line_base(&self) -> usize {
        self.segments.first().map(|s| s.buffer_range.start).unwrap_or(0)
    }
}

fn segment_galley(
    ui: &egui::Ui,
    seg: &Segment,
    base_size: f32,
    base_color: Color32,
) -> Arc<egui::Galley> {
    // Widgets that provide custom `display()` text (e.g. the patch-review
    // intraline `new_str` widget) get measured + laid out as that text;
    // the painter renders the same galley. Widgets with no display fall
    // through to the bordered "widget" placeholder.
    let widget_display = seg.widget.as_ref().and_then(|w| w.display());
    let owned_text;
    let display: &str = if let Some(d) = widget_display.as_ref() {
        owned_text = d.text.clone();
        owned_text.as_str()
    } else if seg.widget.is_some() {
        "widget"
    } else if seg.display.is_empty() && seg.is_replacement {
        ""
    } else if seg.display.is_empty() {
        " "
    } else {
        seg.display.as_str()
    };
    let format = format_for(&seg.style, base_size, base_color);
    let mut job = LayoutJob::single_section(display.to_string(), format);
    job.wrap.max_width = f32::INFINITY;
    ui.fonts(|f| f.layout_job(job))
}

pub(super) fn build_line_layout(
    line_text: &str,
    line_byte_start: usize,
    layers: &[editor_core::DecorationSet],
    base_font_size: f32,
    base_color: Color32,
) -> LineLayout {
    let line_byte_end = line_byte_start + line_text.len();
    let mut events: Vec<DecoEvent> = Vec::new();
    for layer in layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
            let clipped = range.start.max(line_byte_start)..range.end.min(line_byte_end);
            if clipped.start >= clipped.end {
                continue;
            }
            match deco {
                Decoration::Mark(style) => events.push(DecoEvent::Mark(clipped, style.clone())),
                Decoration::Replace { display } => {
                    events.push(DecoEvent::Replace(clipped, display.clone()))
                }
                Decoration::InlineWidget { widget, .. } => {
                    events.push(DecoEvent::Widget(clipped, widget.clone()))
                }
                Decoration::Line(_) | Decoration::Block(_) | Decoration::BlockWidget { .. } => {}
            }
        }
    }

    // Snap a line-local byte index to the nearest valid char boundary at or
    // before it. Decoration ranges occasionally land mid-codepoint when they
    // outlive a buffer edit (the markdown parse is async) — slicing on those
    // raw indices panics on multi-byte chars like em-dash.
    let snap = |mut b: usize| -> usize {
        if b > line_text.len() {
            b = line_text.len();
        }
        while b > 0 && !line_text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };

    let mut boundaries: Vec<usize> = vec![0, line_text.len()];
    for ev in &events {
        match ev {
            DecoEvent::Mark(r, _)
            | DecoEvent::Replace(r, _)
            | DecoEvent::Widget(r, _) => {
                boundaries.push(snap(r.start.saturating_sub(line_byte_start)));
                boundaries.push(snap(r.end.saturating_sub(line_byte_start)));
            }
        }
    }
    boundaries.sort();
    boundaries.dedup();

    let style_at = |start: usize,
                    end: usize|
     -> (
        MarkStyle,
        Option<Option<SmolStr>>,
        Option<Arc<dyn InlineWidget>>,
    ) {
        let abs_start = line_byte_start + start;
        let abs_end = line_byte_start + end;
        let mut merged = MarkStyle::default();
        let mut replacement: Option<Option<SmolStr>> = None;
        let mut widget: Option<Arc<dyn InlineWidget>> = None;
        for ev in &events {
            match ev {
                DecoEvent::Mark(r, s) if r.start <= abs_start && r.end >= abs_end => {
                    merge_mark(&mut merged, s);
                }
                DecoEvent::Replace(r, disp) if r.start <= abs_start && r.end >= abs_end => {
                    replacement = Some(disp.clone());
                }
                DecoEvent::Widget(r, w) if r.start <= abs_start && r.end >= abs_end => {
                    widget = Some(w.clone());
                }
                _ => {}
            }
        }
        (merged, replacement, widget)
    };

    // Collect line-local Replace AND Widget ranges; each becomes ONE
    // consolidated segment so an interior Mark doesn't subdivide and duplicate
    // either the Replace display or the widget placeholder.
    enum Atomic {
        Replace(Option<SmolStr>),
        Widget(Arc<dyn InlineWidget>),
    }
    let mut atomic_ranges: Vec<(usize, usize, Atomic)> = Vec::new();
    for ev in &events {
        match ev {
            DecoEvent::Replace(r, disp) => atomic_ranges.push((
                snap(r.start.saturating_sub(line_byte_start)),
                snap(r.end.saturating_sub(line_byte_start)),
                Atomic::Replace(disp.clone()),
            )),
            DecoEvent::Widget(r, w) => atomic_ranges.push((
                snap(r.start.saturating_sub(line_byte_start)),
                snap(r.end.saturating_sub(line_byte_start)),
                Atomic::Widget(w.clone()),
            )),
            DecoEvent::Mark(_, _) => {}
        }
    }
    atomic_ranges.sort_by_key(|(s, _, _)| *s);

    // Marks-overlapping-range helper: union of all Mark styles whose range
    // intersects [s, e). Used for both Replace consolidated segments and
    // normal text segments.
    let marks_for = |s: usize, e: usize| -> MarkStyle {
        let abs_s = line_byte_start + s;
        let abs_e = line_byte_start + e;
        let mut merged = MarkStyle::default();
        for ev in &events {
            if let DecoEvent::Mark(r, m) = ev {
                if r.end > abs_s && r.start < abs_e {
                    merge_mark(&mut merged, m);
                }
            }
        }
        merged
    };

    let mut segments = Vec::with_capacity(boundaries.len());
    let mut cursor: usize = 0;
    let line_len = line_text.len();

    while cursor < line_len {
        // 1. If cursor is inside an atomic (Replace or Widget) range, emit ONE
        //    consolidated segment.
        if let Some(idx) = atomic_ranges.iter().position(|(s, e, _)| cursor >= *s && cursor < *e)
        {
            let (rs, re, ref atom) = atomic_ranges[idx];
            let style = marks_for(rs, re);
            match atom {
                Atomic::Replace(disp) => segments.push(Segment {
                    display: disp.clone().unwrap_or_default(),
                    buffer_range: (line_byte_start + rs)..(line_byte_start + re),
                    style,
                    is_replacement: true,
                    widget: None,
                }),
                Atomic::Widget(w) => segments.push(Segment {
                    display: SmolStr::default(),
                    buffer_range: (line_byte_start + rs)..(line_byte_start + re),
                    style,
                    is_replacement: true,
                    widget: Some(w.clone()),
                }),
            }
            cursor = re;
            continue;
        }

        // 2. Find the next break: either the next atomic-range start, the next
        //    Mark boundary, or end of line.
        let mut seg_end = line_len;
        for (rs, _, _) in &atomic_ranges {
            if *rs > cursor && *rs < seg_end {
                seg_end = *rs;
            }
        }
        for b in &boundaries {
            if *b > cursor && *b < seg_end {
                seg_end = *b;
            }
        }
        if seg_end <= cursor {
            cursor += 1;
            continue;
        }
        let (style, _, _) = style_at(cursor, seg_end);
        let slice = &line_text[cursor..seg_end];
        segments.push(Segment {
            display: SmolStr::from(slice),
            buffer_range: (line_byte_start + cursor)..(line_byte_start + seg_end),
            style,
            is_replacement: false,
            widget: None,
        });
        cursor = seg_end;
    }
    if segments.is_empty() {
        segments.push(Segment {
            display: SmolStr::default(),
            buffer_range: line_byte_start..line_byte_start,
            style: MarkStyle::default(),
            is_replacement: false,
            widget: None,
        });
    }
    LineLayout { segments, base_font_size, base_color }
}

enum DecoEvent {
    Mark(std::ops::Range<usize>, MarkStyle),
    Replace(std::ops::Range<usize>, Option<SmolStr>),
    Widget(std::ops::Range<usize>, Arc<dyn InlineWidget>),
}

fn merge_mark(dst: &mut MarkStyle, src: &MarkStyle) {
    if src.bold { dst.bold = true; }
    if src.italic { dst.italic = true; }
    if src.strikethrough { dst.strikethrough = true; }
    if src.underline { dst.underline = true; }
    if src.monospace { dst.monospace = true; }
    if src.fg.is_some() { dst.fg = src.fg; }
    if src.bg.is_some() { dst.bg = src.bg; }
    if src.font_scale.is_some() { dst.font_scale = src.font_scale; }
}

fn format_for(style: &MarkStyle, base_size: f32, base_color: Color32) -> TextFormat {
    let size = base_size * style.font_scale.unwrap_or(1.0);
    // `style.monospace` is the *signal* that this run is code-shaped.
    // Both branches resolve to `Monospace` for now because the wrap
    // calculator below uses monospace `char_width` and mixing
    // proportional runs into a monospace wrap budget produces visible
    // misalignment. Custom font families (per `editor.font_*` settings)
    // are loaded at startup via `egui::Context::set_fonts` and routed
    // through here once that lands.
    let family = if style.monospace {
        FontFamily::Monospace
    } else {
        FontFamily::Monospace
    };
    let fg = style.fg.map(super::to_egui_color).unwrap_or(base_color);
    TextFormat {
        font_id: FontId::new(size, family),
        color: fg,
        italics: style.italic,
        // We draw bg/underline/strike manually in the painter so they pick up
        // the segment's measured width, not the glyph rect.
        ..Default::default()
    }
}

// Re-export Color for super module accessibility consistency.
#[allow(dead_code)]
pub(super) type _ColorAlias = Color;
