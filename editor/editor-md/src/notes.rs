//! Footnote decorations.
//!
//! Two kinds:
//! - Inline reference: `[^name]` (alphanumeric `name`) — rendered as a small
//!   superscript-like chip (smaller font, blue fg, atomic). A `Replace` swaps
//!   the raw text for a styled `[^<name>]`.
//! - Definition line: `[^name]: text` at start of a line — gets a `Line`
//!   decoration with a dim background, and the `[^name]: ` prefix is marked
//!   for styling.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::LineStyle;

use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
use smol_str::SmolStr;

/// Footnote color — blue, distinct from wikilink (slightly cooler).
pub const COLOR_FOOTNOTE: Color = Color::rgb(80, 140, 220);
/// Definition line background — very faint blue tint.
pub const COLOR_FOOTNOTE_DEF_BG: Color = Color::rgba(80, 140, 220, 20);

pub fn footnote_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let text = state.doc.to_string();
    let fg = theme.map(|t| t.markdown.link).unwrap_or(COLOR_FOOTNOTE);
    let def_bg = COLOR_FOOTNOTE_DEF_BG;

    let mut scan = NotesScan {
        state,
        text: &text,
        fg,
        def_bg,
        viewport,
        entries: Vec::new(),
    };
    scan.collect_definitions();
    scan.collect_inline_refs();
    RangeSet::from_iter(scan.entries)
}

/// Per-parse state. Bundling the shared inputs + entry list as fields
/// lets `collect_definitions` / `collect_inline_refs` be `self` methods.
struct NotesScan<'a> {
    state: &'a EditorState,
    text: &'a str,
    fg: Color,
    def_bg: Color,
    viewport: Option<&'a std::ops::Range<usize>>,
    entries: Vec<(std::ops::Range<usize>, Decoration)>,
}

impl<'a> NotesScan<'a> {
fn collect_definitions(&mut self) {
    let state = self.state;
    let text = self.text;
    let fg = self.fg;
    let def_bg = self.def_bg;
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

    let line_range = match viewport {
        Some(vp) => editor_view::viewport_lines(&state.doc, vp),
        None => 0..total_lines,
    };
    for line in line_range {
        let ls = state.doc.line_to_byte(line);
        let le = line_byte_end(line);
        let raw = &text[ls..le];
        let line_text = raw.strip_suffix('\n').unwrap_or(raw);
        let Some(prefix_len) = Self::parse_def_prefix(line_text) else {
            continue;
        };
        entries.push((
            ls..le,
            Decoration::Line(LineStyle {
                bg: Some(def_bg),
                ..LineStyle::default()
            }),
        ));
        entries.push((
            ls..(ls + prefix_len),
            Decoration::Mark(MarkStyle {
                fg: Some(fg),
                bold: true,
                ..MarkStyle::default()
            }),
        ));
    }
}

/// If `line_text` begins with `[^name]: `, return the length of that prefix.
fn parse_def_prefix(line_text: &str) -> Option<usize> {
    let bytes = line_text.as_bytes();
    if bytes.len() < 5 || bytes[0] != b'[' || bytes[1] != b'^' {
        return None;
    }
    let mut p = 2;
    while p < bytes.len() && bytes[p].is_ascii_alphanumeric() {
        p += 1;
    }
    if p == 2 || p >= bytes.len() || bytes[p] != b']' {
        return None;
    }
    p += 1;
    if p >= bytes.len() || bytes[p] != b':' {
        return None;
    }
    p += 1;
    if p < bytes.len() && bytes[p] == b' ' {
        p += 1;
    }
    Some(p)
}

fn collect_inline_refs(&mut self) {
    let state = self.state;
    let text = self.text;
    let fg = self.fg;
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

    let bytes = text.as_bytes();
    let (scan_start, scan_end) = match viewport {
        Some(vp) => (vp.start.min(bytes.len()), vp.end.min(bytes.len())),
        None => (0, bytes.len()),
    };
    let mut i = scan_start;
    while i + 3 < scan_end {
        if bytes[i] == b'[' && bytes[i + 1] == b'^' {
            // Skip if this position is the start of a definition line.
            let line = state.doc.byte_to_line(i);
            let ls = state.doc.line_to_byte(line);
            if i == ls {
                let le = line_byte_end(line);
                let raw = &text[ls..le];
                let line_text = raw.strip_suffix('\n').unwrap_or(raw);
                if Self::parse_def_prefix(line_text).is_some() {
                    i += 1;
                    continue;
                }
            }

            let mut p = i + 2;
            while p < bytes.len() && bytes[p].is_ascii_alphanumeric() {
                p += 1;
            }
            if p == i + 2 || p >= bytes.len() || bytes[p] != b']' {
                i += 1;
                continue;
            }
            let name = &text[(i + 2)..p];
            let full_end = p + 1;
            let display = format!("[^{name}]");
            entries.push((
                i..full_end,
                Decoration::Replace {
                    display: Some(SmolStr::from(display)),
                },
            ));
            entries.push((
                i..full_end,
                Decoration::Mark(MarkStyle {
                    fg: Some(fg),
                    font_scale: Some(0.7),
                    atomic: true,
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
