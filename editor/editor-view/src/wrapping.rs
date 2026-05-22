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
//!   - per-line content changes (`invalidate_line`, by the painter when it
//!     detects the line's content hash has changed)
//!
//! When wrapping is disabled, the map contains a single VLine per buffer line
//! with no breaks; the rest of the view layer is wrap-agnostic.

use smallvec::SmallVec;

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
    pub fn get_or_compute<F: Fn(usize) -> String>(
        &mut self,
        line: usize,
        line_text: F,
        scale: f32,
    ) -> &WrappedLine {
        self.ensure_capacity(line + 1);
        let text = line_text(line);
        let h = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
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
                compute_wraps(&text, self.char_width, self.width, self.enabled, scale);
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
pub fn compute_wraps(
    text: &str,
    char_width: f32,
    max_width: f32,
    enabled: bool,
    scale: f32,
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
    let bytes = text.as_bytes();
    let mut breaks: SmallVec<[u32; 4]> = SmallVec::new();
    let mut vlines: SmallVec<[(u32, u32); 4]> = SmallVec::new();

    let mut row_start: usize = 0;
    let mut row_char_count: usize = 0;
    let mut last_space_byte: Option<usize> = None;

    let mut i = 0;
    while i < bytes.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if ch == ' ' || ch == '\t' {
            last_space_byte = Some(i);
        }

        if row_char_count + 1 > max_chars {
            // Only consider a space-break that's BEFORE the current position;
            // a space marked at this same `i` (we're currently sitting on it)
            // can't be the break point since `sp + 1 > i`.
            let break_byte = match last_space_byte {
                Some(sp) if sp >= row_start && sp < i => sp + 1,
                _ => i,
            };
            if break_byte > row_start {
                vlines.push((row_start as u32, break_byte as u32));
                breaks.push(break_byte as u32);
                row_start = break_byte;
                // Chars consumed so far on the new row (text[row_start..i] plus
                // the current char we're about to count).
                row_char_count = text[row_start..i].chars().count() + 1;
                last_space_byte = None;
            } else {
                row_char_count += 1;
            }
        } else {
            row_char_count += 1;
        }
        i += ch_len;
    }

    vlines.push((row_start as u32, bytes.len() as u32));
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
        let w = compute_wraps("hello world this is long", 7.0, 50.0, false, 1.0);
        assert_eq!(w.visual_count(), 1);
    }

    #[test]
    fn empty_line_one_vline() {
        let w = compute_wraps("", 7.0, 100.0, true, 1.0);
        assert_eq!(w.visual_count(), 1);
    }

    #[test]
    fn short_line_one_vline() {
        let w = compute_wraps("hello", 7.0, 200.0, true, 1.0);
        assert_eq!(w.visual_count(), 1);
        assert_eq!(w.vline_range(0), (0, 5));
    }

    #[test]
    fn wraps_at_word_boundary() {
        // 7px/char, 60px width → ~8 chars per line.
        // "hello world this is" → breaks at " world", " this", " is"
        let w = compute_wraps("hello world this is", 7.0, 60.0, true, 1.0);
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
        let w = compute_wraps("abcdefghij", 7.0, 28.0, true, 1.0);
        assert!(w.visual_count() >= 2);
    }

    #[test]
    fn vline_at_byte_finds_correct_row() {
        let w = compute_wraps("hello world this is", 7.0, 60.0, true, 1.0);
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
        let base = compute_wraps("abcdefghij klmnopqrst", 7.0, 280.0, true, 1.0);
        let scaled = compute_wraps("abcdefghij klmnopqrst", 7.0, 280.0, true, 2.0);
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
        let one = map.get_or_compute(0, |_| text.clone(), 1.0).visual_count();
        let two = map.get_or_compute(0, |_| text.clone(), 2.0).visual_count();
        assert_eq!(one, 1, "scale 1.0 single line");
        assert!(two > 1, "scale 2.0 should re-wrap into multiple vlines, got {two}");
    }
}
