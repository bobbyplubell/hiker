//! Math decorations.
//!
//! Detects inline math `$...$` and block math `$$...$$`. This module only
//! *marks* the regions visually — actual math rendering (KaTeX/MathJax
//! equivalent) is deferred and would require either a host-provided
//! BlockWidget renderer or a native equation layout engine.
//!
//! - Inline `$...$`: `Mark` with monospace font + light bg + violet fg.
//! - Block `$$...$$`: per-line `Line` decoration with a tinted bg covering
//!   every line in the span (including the delimiter lines).

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::LineStyle;

use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
pub const COLOR_MATH_FG: Color = Color::rgb(170, 120, 220);
pub const COLOR_MATH_BG: Color = Color::rgba(170, 120, 220, 25);

pub fn math_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let text = state.doc.to_string();
    // Math has no dedicated theme slot; use code_bg if a theme is provided.
    let fg = theme.map(|t| t.palette.accent).unwrap_or(COLOR_MATH_FG);
    let bg = theme.map(|t| t.markdown.code_bg).unwrap_or(COLOR_MATH_BG);

    let mut scan = MathScan {
        state,
        text: &text,
        fg,
        bg,
        viewport,
        entries: Vec::new(),
    };
    let block_ranges = scan.collect_block_math();
    scan.collect_inline_math(&block_ranges);
    RangeSet::from_iter(scan.entries)
}

struct MathScan<'a> {
    state: &'a EditorState,
    text: &'a str,
    fg: Color,
    bg: Color,
    viewport: Option<&'a std::ops::Range<usize>>,
    entries: Vec<(std::ops::Range<usize>, Decoration)>,
}

impl<'a> MathScan<'a> {
/// Walks the doc finding `$$ ... $$` spans (which may cross multiple lines).
/// Emits one `Line` decoration per line of the span and returns the byte
/// ranges occupied so inline-math scanning can skip them.
fn collect_block_math(&mut self) -> Vec<std::ops::Range<usize>> {
    let state = self.state;
    let text = self.text;
    let bg = self.bg;
    let viewport = self.viewport;
    let entries = &mut self.entries;
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let bytes = text.as_bytes();
    let (scan_start, scan_end) = match viewport {
        Some(vp) => (vp.start.min(bytes.len()), vp.end.min(bytes.len())),
        None => (0, bytes.len()),
    };
    let mut i = scan_start;
    while i + 1 < scan_end {
        if bytes[i] == b'$' && bytes[i + 1] == b'$' {
            let mut j = i + 2;
            let mut closed = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'$' && bytes[j + 1] == b'$' {
                    closed = Some(j);
                    break;
                }
                j += 1;
            }
            let Some(close_start) = closed else {
                break;
            };
            let full_end = close_start + 2;
            let start_line = state.doc.byte_to_line(i);
            let end_line = state.doc.byte_to_line(full_end.saturating_sub(1));
            for l in start_line..=end_line {
                if l >= total_lines {
                    break;
                }
                let s = state.doc.line_to_byte(l);
                let e = line_byte_end(l);
                entries.push((
                    s..e,
                    Decoration::Line(LineStyle {
                        bg: Some(bg),
                        ..LineStyle::default()
                    }),
                ));
            }
            ranges.push(i..full_end);
            i = full_end;
            continue;
        }
        i += 1;
    }
    ranges
}

fn collect_inline_math(&mut self, block_ranges: &[std::ops::Range<usize>]) {
    let text = self.text;
    let fg = self.fg;
    let bg = self.bg;
    let viewport = self.viewport;
    let entries = &mut self.entries;
    let in_block = |off: usize| -> bool {
        block_ranges.iter().any(|r| off >= r.start && off < r.end)
    };
    let bytes = text.as_bytes();
    let (scan_start, scan_end) = match viewport {
        Some(vp) => (vp.start.min(bytes.len()), vp.end.min(bytes.len())),
        None => (0, bytes.len()),
    };
    let mut i = scan_start;
    while i < scan_end {
        if bytes[i] == b'$' && !in_block(i) {
            // Skip `$$`.
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                i += 2;
                continue;
            }
            let mut j = i + 1;
            let mut closed = None;
            while j < bytes.len() {
                if bytes[j] == b'\n' {
                    break;
                }
                if bytes[j] == b'$' {
                    closed = Some(j);
                    break;
                }
                j += 1;
            }
            let Some(close) = closed else {
                i += 1;
                continue;
            };
            if close == i + 1 {
                i = close + 1;
                continue;
            }
            let full_end = close + 1;
            entries.push((
                i..full_end,
                Decoration::Mark(MarkStyle {
                    fg: Some(fg),
                    bg: Some(bg),
                    monospace: true,
                    ..MarkStyle::default()
                }),
            ));
            i = full_end;
            continue;
        }
        i += 1;
    }
}
}
