//! Soft line wrapping.
//!
//! Per-buffer-line cache of wrap break positions. Greedy word-boundary
//! wrapping with a char-width approximation — sufficient for monospace text
//! and a reasonable approximation for the markdown live-preview case where
//! heading lines are slightly larger but still mostly monospace.
//!
//! `WrapMap` is invalidated by:
//!   - width changes (`set_width`)
//!   - char-width changes (`set_char_width`, on font size change)
//!   - per-line content changes — detected lazily by `get_or_compute`, which
//!     re-wraps a line whenever the current text hash, width, or font scale no
//!     longer matches its cached entry. (`invalidate_line` exists for explicit
//!     eviction but the per-frame prewrap relies on the hash check instead.)
//!
//! When wrapping is disabled, the map contains a single VLine per buffer line
//! with no breaks; the rest of the view layer is wrap-agnostic.

use smallvec::SmallVec;

/// A run of source bytes whose *rendered* width differs from its raw character
/// count, because a live-preview `Replace` decoration hides or substitutes it:
/// a hidden marker (`cols == 0`, e.g. the `**` of bold or the `<span …>` tag of
/// a color span) or a replaced glyph (`cols == display.chars().count()`, e.g. a
/// list marker rendered as `• `). Offsets are line-local bytes.
///
/// Soft-wrap counts `cols` (not the raw byte span) for these ranges so a line
/// breaks at the column the user actually sees — without this, a long hidden
/// span tag consumes wrap budget it never paints. The span is treated as
/// atomic: a break never lands inside it.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct VisualSpan {
    pub start: u32,
    pub end: u32,
    pub cols: u32,
}

#[derive(Clone, Debug)]
pub struct WrappedLine {
    /// Byte offsets within the buffer line where a visual break starts. An
    /// empty vec means the line fits in one VLine.
    pub breaks: SmallVec<[u32; 4]>,
    /// Width the wrap was computed at; used to detect width-changes that
    /// invalidate the cache.
    pub width: f32,
    /// Font scale the wrap was computed at — heading lines render their text
    /// at `base_char_width * scale`, so they need to break earlier than the
    /// global char_width would suggest. Cached so a scale change (e.g. the
    /// markdown decorator promoted a line to a heading) re-wraps the line.
    pub scale: f32,
    /// Hash of the line text the wrap was computed against. Used to detect
    /// content changes that invalidate the cache even when width is unchanged
    /// (e.g. typing while wrapped).
    pub text_hash: u64,
    /// Per-VLine byte ranges (start, end) within the buffer line. Computed
    /// from `breaks`; cached for fast lookup. `vlines[i].0..vlines[i].1` is
    /// the slice of the buffer line on visual row i.
    pub vlines: SmallVec<[(u32, u32); 4]>,
}

impl Default for WrappedLine {
    fn default() -> Self {
        Self {
            breaks: SmallVec::new(),
            width: 0.0,
            scale: 1.0,
            text_hash: 0,
            vlines: SmallVec::new(),
        }
    }
}

impl WrappedLine {
    /// Number of visual lines for this buffer line (≥ 1).
    pub fn visual_count(&self) -> usize {
        self.vlines.len().max(1)
    }

    /// Return the (vline_index, local_byte_offset_within_vline) for a buffer-
    /// line-local byte offset.
    pub fn vline_at_byte(&self, local_byte: usize) -> (usize, usize) {
        if self.vlines.is_empty() {
            return (0, local_byte);
        }
        for (i, (start, end)) in self.vlines.iter().enumerate() {
            let s = *start as usize;
            let e = *end as usize;
            if local_byte >= s && local_byte <= e {
                return (i, local_byte - s);
            }
        }
        let last = self.vlines.len() - 1;
        let (s, e) = (self.vlines[last].0 as usize, self.vlines[last].1 as usize);
        (last, local_byte.min(e).saturating_sub(s))
    }

    pub fn vline_range(&self, vline: usize) -> (usize, usize) {
        if let Some((s, e)) = self.vlines.get(vline) {
            (*s as usize, *e as usize)
        } else {
            (0, 0)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct WrapMap {
    lines: Vec<WrappedLine>,
    /// Width in pixels available for the text content (i.e. widget width
    /// minus gutter). `0.0` means uninitialized.
    width: f32,
    /// Approximate monospace char width in pixels. `0.0` means uninitialized.
    char_width: f32,
    /// Whether wrapping is on at all. When false, every line is treated as a
    /// single VLine with no breaks.
    enabled: bool,
    /// Fingerprint of the inputs that determine every line's wrap, restricted
    /// to ones whose change implies *some* off-viewport line could now wrap
    /// differently: doc content_id, the full-document decoration epoch, and
    /// width/char_width/enabled. Viewport-scoped layers are deliberately
    /// excluded — they only cover visible lines.
    ///
    /// Compared by [`Self::walk_range`] against the inputs of the previous
    /// prewrap pass: if it matches and the line count is unchanged, only the
    /// union of last and current visible bands needs rescanning.
    last_geo_key: u64,
    last_total_lines: usize,
    last_viewport: (usize, usize),
}

impl WrapMap {
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            self.invalidate_all();
        }
    }

    pub const fn width(&self) -> f32 {
        self.width
    }

    pub fn set_width(&mut self, w: f32) {
        if (self.width - w).abs() > 0.5 {
            self.width = w;
            self.invalidate_all();
        }
    }

    pub fn set_char_width(&mut self, cw: f32) {
        if (self.char_width - cw).abs() > 0.01 {
            self.char_width = cw;
            self.invalidate_all();
        }
    }

    pub const fn char_width(&self) -> f32 {
        self.char_width
    }

    pub fn invalidate_all(&mut self) {
        self.lines.clear();
        // Force the next `walk_range` to take the full-rescan branch — any
        // cached partial-walk state is meaningless once the per-line entries
        // are gone.
        self.last_geo_key = 0;
        self.last_total_lines = 0;
        self.last_viewport = (usize::MAX, usize::MAX);
    }

    /// Decide which line range `prewrap_visible` must rescan this frame, given
    /// the geometry inputs that drive every line's wrap.
    ///
    /// On a pure scroll (geo_key + line count unchanged from last walk), the
    /// only lines whose spans could have shifted are those whose
    /// viewport-scoped decoration coverage just changed — i.e. the union of
    /// last frame's and this frame's visible band. Lines outside that union
    /// were covered by the same set of layers in both frames (full-doc layers,
    /// which are stable while `geo_key` is stable; no viewport-scoped layer
    /// covered them in either frame), so their cached wraps are still valid.
    ///
    /// On any geometry change (doc edit, full-doc decoration change, width /
    /// char-width / enabled change, line-count change) the full document is
    /// returned and the cached walk state is reset.
    ///
    /// Always updates the stored last-walk state so the *next* call sees
    /// today's inputs as "last frame's".
    pub fn walk_range(
        &mut self,
        geo_key: u64,
        total_lines: usize,
        viewport: (usize, usize),
    ) -> std::ops::Range<usize> {
        let full = geo_key != self.last_geo_key
            || total_lines != self.last_total_lines
            || self.last_viewport == (usize::MAX, usize::MAX);
        let range = if full {
            0..total_lines
        } else {
            let (lo_a, hi_a) = self.last_viewport;
            let (lo_b, hi_b) = viewport;
            lo_a.min(lo_b)..hi_a.max(hi_b).min(total_lines)
        };
        self.last_geo_key = geo_key;
        self.last_total_lines = total_lines;
        self.last_viewport = viewport;
        range
    }

    pub fn invalidate_line(&mut self, line: usize) {
        if line < self.lines.len() {
            self.lines[line] = WrappedLine::default();
        }
    }

    pub fn ensure_capacity(&mut self, line_count: usize) {
        if self.lines.len() != line_count {
            self.lines.resize(line_count, WrappedLine::default());
        }
    }

    /// Get the wrap info for `line`, computing it if needed. The cache is
    /// invalidated by width, char-width, font scale, OR per-line content
    /// changes. `scale` is the line's effective font scale (e.g. headings
    /// > 1.0) — caller computes it from the decoration layers covering the
    /// > line so the wrap accounts for the actual rendered character width.
    ///
    /// `text` is borrowed, not produced by a closure: the hash below reads it
    /// unconditionally, so a lazy closure never avoided materializing it. The
    /// caller already holds the line string (it scans the same line for spans),
    /// so passing it through avoids a redundant per-line rope slice + alloc on
    /// every scroll frame.
    pub fn get_or_compute(
        &mut self,
        line: usize,
        text: &str,
        scale: f32,
        spans: &[VisualSpan],
    ) -> &WrappedLine {
        self.ensure_capacity(line + 1);
        // Hash the text AND the visual spans: the spans encode the line's
        // live-preview reveal state (markers hidden off the cursor line, shown
        // on it), so moving the cursor onto/off the line must re-wrap even
        // though the underlying bytes are unchanged.
        let h = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            spans.hash(&mut hasher);
            hasher.finish()
        };
        let dirty = {
            let w = &self.lines[line];
            w.vlines.is_empty()
                || (w.width - self.width).abs() > 0.5
                || (w.scale - scale).abs() > 0.001
                || w.text_hash != h
        };
        if dirty {
            let mut new_w =
                compute_wraps(text, self.char_width, self.width, self.enabled, scale, spans);
            new_w.text_hash = h;
            self.lines[line] = new_w;
        }
        &self.lines[line]
    }

    pub fn peek(&self, line: usize) -> Option<&WrappedLine> {
        self.lines.get(line).filter(|w| !w.vlines.is_empty())
    }

    /// Total visual line count across all buffer lines. Caller must ensure
    /// all lines have been wrapped (via `get_or_compute`).
    pub fn total_visual_lines(&self) -> usize {
        self.lines.iter().map(WrappedLine::visual_count).sum()
    }
}

/// Greedy word-boundary wrap. Returns a `WrappedLine` with `breaks` + `vlines`
/// populated. When `enabled` is false (or width / char_width unset), produces
/// a single VLine spanning the whole text. `scale` scales the effective
/// char width up (heading lines render larger than the base monospace cell,
/// so they fit fewer characters per visual row).
///
/// `spans` describes ranges whose rendered width differs from their raw char
/// count (live-preview hidden markers / replaced glyphs); see [`VisualSpan`].
/// Each is counted as `cols` visible columns and treated as an atomic unit so a
/// break never lands inside one. `breaks`/`vlines` stay in *real* (source) byte
/// coordinates regardless — the renderer slices the real line by these ranges
/// and re-applies the decorations.
pub fn compute_wraps(
    text: &str,
    char_width: f32,
    max_width: f32,
    enabled: bool,
    scale: f32,
    spans: &[VisualSpan],
) -> WrappedLine {
    if !enabled || char_width <= 0.0 || max_width <= 0.0 {
        let mut vlines: SmallVec<[(u32, u32); 4]> = SmallVec::new();
        vlines.push((0, text.len() as u32));
        return WrappedLine {
            breaks: SmallVec::new(),
            vlines,
            width: max_width,
            scale,
            text_hash: 0,
        };
    }
    let scale = scale.max(0.01);
    let effective_cw = char_width * scale;
    let max_chars = ((max_width / effective_cw).floor() as usize).max(1);

    // Visible columns of a span that starts exactly at byte `i`, plus the
    // source byte length to advance — or `None` when `i` isn't a span start.
    let span_at = |i: usize| -> Option<(usize, usize)> {
        spans
            .iter()
            .find(|s| s.start as usize == i)
            .map(|s| (s.cols as usize, (s.end - s.start) as usize))
    };
    // Visible columns in the real byte range [from, to) — char count, with each
    // fully-contained span's source chars swapped for its `cols`. Used to
    // re-tally the new row after a deferred (word-boundary) break.
    let cols_in = |from: usize, to: usize| -> usize {
        let mut c = text[from..to].chars().count();
        for s in spans {
            let (ss, se) = (s.start as usize, s.end as usize);
            if ss >= from && se <= to {
                c = c - text[ss..se].chars().count() + s.cols as usize;
            }
        }
        c
    };

    let mut breaks: SmallVec<[u32; 4]> = SmallVec::new();
    let mut vlines: SmallVec<[(u32, u32); 4]> = SmallVec::new();
    let mut row_start: usize = 0;
    let mut row_cols: usize = 0;
    let mut last_space_byte: Option<usize> = None;

    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        // A unit is either a whole atomic span or a single char.
        let (unit_cols, unit_len, is_space) = match span_at(i) {
            Some((cols, len)) => (cols, len, false),
            None => {
                let ch = text[i..].chars().next().unwrap();
                (1, ch.len_utf8(), ch == ' ' || ch == '\t')
            }
        };
        if is_space {
            last_space_byte = Some(i);
        }
        if row_cols + unit_cols > max_chars {
            // Prefer the last space strictly before the current unit; otherwise
            // hard-break right before it (never inside a span).
            let break_byte = match last_space_byte {
                Some(sp) if sp >= row_start && sp < i => sp + 1,
                _ => i,
            };
            if break_byte > row_start {
                vlines.push((row_start as u32, break_byte as u32));
                breaks.push(break_byte as u32);
                row_start = break_byte;
                row_cols = cols_in(break_byte, i) + unit_cols;
                last_space_byte = None;
            } else {
                row_cols += unit_cols;
            }
        } else {
            row_cols += unit_cols;
        }
        i += unit_len;
    }

    vlines.push((row_start as u32, text.len() as u32));
    if vlines.is_empty() {
        vlines.push((0, 0));
    }

    WrappedLine { breaks, vlines, width: max_width, scale, text_hash: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_returns_single_vline() {
        let w = compute_wraps("hello world this is long", 7.0, 50.0, false, 1.0, &[]);
        assert_eq!(w.visual_count(), 1);
    }

    #[test]
    fn empty_line_one_vline() {
        let w = compute_wraps("", 7.0, 100.0, true, 1.0, &[]);
        assert_eq!(w.visual_count(), 1);
    }

    #[test]
    fn short_line_one_vline() {
        let w = compute_wraps("hello", 7.0, 200.0, true, 1.0, &[]);
        assert_eq!(w.visual_count(), 1);
        assert_eq!(w.vline_range(0), (0, 5));
    }

    #[test]
    fn wraps_at_word_boundary() {
        // 7px/char, 60px width → ~8 chars per line.
        // "hello world this is" → breaks at " world", " this", " is"
        let w = compute_wraps("hello world this is", 7.0, 60.0, true, 1.0, &[]);
        assert!(w.visual_count() >= 2);
        // First VLine should end at the space after "hello" or similar.
        let (start, end) = w.vline_range(0);
        assert_eq!(start, 0);
        let first_slice = &"hello world this is"[start..end];
        assert!(!first_slice.contains("this is"), "first slice: {first_slice:?}");
    }

    #[test]
    fn break_inside_long_word_when_no_space() {
        // "abcdefghij" with 4-char width → must break inside the word.
        let w = compute_wraps("abcdefghij", 7.0, 28.0, true, 1.0, &[]);
        assert!(w.visual_count() >= 2);
    }

    #[test]
    fn hidden_span_does_not_consume_wrap_width() {
        // A line whose visible text is short ("red") but whose source carries a
        // long hidden color-span tag must NOT wrap: the hidden bytes contribute
        // zero columns. 7px/char, 280px → 40 cols/row.
        let src = "<span style=\"color:#2e5e3a\">red</span>";
        let open_end = src.find('>').unwrap() + 1; // end of opening tag
        let close_start = src.find("</span>").unwrap();
        let spans = [
            VisualSpan { start: 0, end: open_end as u32, cols: 0 },
            VisualSpan { start: close_start as u32, end: src.len() as u32, cols: 0 },
        ];
        // Without span-awareness the 38-char source would exceed... actually fit
        // 40 cols; shrink the width so the raw length WOULD wrap but the visible
        // 3 cols don't. 280px/38chars vs a 70px width → 10 cols/row.
        let raw = compute_wraps(src, 7.0, 70.0, true, 1.0, &[]);
        let visible = compute_wraps(src, 7.0, 70.0, true, 1.0, &spans);
        assert!(raw.visual_count() > 1, "raw source overflows the narrow width");
        assert_eq!(visible.visual_count(), 1, "the 3 visible columns fit on one row");
    }

    #[test]
    fn replaced_span_counts_display_columns() {
        // A list marker `1234567. ` (9 source chars) rendered as `- ` (2 cols).
        // Width = 6 cols/row. With the source counted it wraps; as 2 cols it
        // fits with the following short word.
        let src = "1234567. ab";
        let spans = [VisualSpan { start: 0, end: 9, cols: 2 }];
        let w = compute_wraps(src, 7.0, 42.0, true, 1.0, &spans);
        // "- ab" == 4 visible cols ≤ 6 → single row, range covers the real bytes.
        assert_eq!(w.visual_count(), 1);
        assert_eq!(w.vline_range(0), (0, src.len()));
    }

    #[test]
    fn vline_at_byte_finds_correct_row() {
        let w = compute_wraps("hello world this is", 7.0, 60.0, true, 1.0, &[]);
        // byte 0 is in vline 0 at local 0
        assert_eq!(w.vline_at_byte(0), (0, 0));
        // last byte should land in the final vline
        let last_idx = "hello world this is".len();
        let (vline, _) = w.vline_at_byte(last_idx);
        assert_eq!(vline, w.visual_count() - 1);
    }

    #[test]
    fn scaled_line_wraps_at_fewer_chars() {
        // 7px base char width, 280px max → 40 chars/row at scale 1.0.
        // Text is 21 chars: fits on one line at scale 1.0.
        // At scale=2.0 effective char width is 14px → 20 chars/row → needs to break.
        let base = compute_wraps("abcdefghij klmnopqrst", 7.0, 280.0, true, 1.0, &[]);
        let scaled = compute_wraps("abcdefghij klmnopqrst", 7.0, 280.0, true, 2.0, &[]);
        assert_eq!(base.visual_count(), 1, "base 1.0 scale fits on one line");
        assert!(
            scaled.visual_count() > base.visual_count(),
            "scale=2.0 should wrap into more vlines: {} vs {}",
            scaled.visual_count(),
            base.visual_count()
        );
    }

    #[test]
    fn scale_change_invalidates_cache() {
        // Build a WrapMap, compute once at scale 1.0, then bump scale to 2.0
        // and verify the new wrap differs from the cached one.
        let mut map = WrapMap::default();
        map.set_enabled(true);
        map.set_width(280.0);
        map.set_char_width(7.0);
        let text = "abcdefghij klmnopqrst".to_string();
        let one = map.get_or_compute(0, &text, 1.0, &[]).visual_count();
        let two = map.get_or_compute(0, &text, 2.0, &[]).visual_count();
        assert_eq!(one, 1, "scale 1.0 single line");
        assert!(two > 1, "scale 2.0 should re-wrap into multiple vlines, got {two}");
    }
}
