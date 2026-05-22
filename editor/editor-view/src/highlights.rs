//! Active-line and trailing-whitespace highlight decorations.
//! See SPEC §9.11, §9.17 and IMPLEMENTATION §16.6.2, §16.6.8.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::LineStyle;

use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;
/// Soft bluish tint for the line(s) containing a selection head.
const ACTIVE_LINE_BG: Color = Color::rgba(120, 120, 140, 25);

/// Reddish tint for trailing whitespace runs.
const TRAILING_WS_BG: Color = Color::rgba(240, 130, 130, 60);

/// Emit a `Line { bg: ACTIVE_LINE_BG }` decoration for every buffer line that
/// contains a selection head.
pub fn active_line_decorations(state: &EditorState) -> DecorationSet {
    let doc = &state.doc;
    let total_bytes = doc.len_bytes();
    let total_lines = doc.len_lines();
    if total_lines == 0 {
        return RangeSet::empty();
    }

    let mut lines: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for r in state.selection.ranges() {
        let head = r.head.offset().min(total_bytes);
        let line = doc.byte_to_line(head);
        if line < total_lines {
            lines.insert(line);
        }
    }

    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::with_capacity(lines.len());
    for line in lines {
        let line_start = doc.line_to_byte(line);
        let line_end = if line + 1 < total_lines {
            doc.line_to_byte(line + 1)
        } else {
            total_bytes
        };
        let range = if line_start == line_end {
            line_start..line_start + 1
        } else {
            line_start..line_end
        };
        entries.push((
            range,
            Decoration::Line(LineStyle {
                bg: Some(ACTIVE_LINE_BG),
                ..LineStyle::default()
            }),
        ));
    }

    RangeSet::from_iter(entries)
}

/// For each line that ends with one or more whitespace characters before its
/// newline, emit a `Mark { bg: TRAILING_WS_BG }` over that run.
pub fn trailing_whitespace_decorations(
    state: &EditorState,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let doc = &state.doc;
    let total_lines = doc.len_lines();
    if total_lines == 0 {
        return RangeSet::empty();
    }

    let line_range = match viewport {
        Some(vp) => crate::viewport_lines(doc, vp),
        None => 0..total_lines,
    };

    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for line in line_range {
        let line_start = doc.line_to_byte(line);
        let text = doc.line_str(line);
        // Strip trailing newline characters from consideration; we only mark
        // whitespace that sits before the line terminator.
        let bytes = text.as_bytes();
        let mut end = bytes.len();
        while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        let mut ws_start = end;
        while ws_start > 0 {
            let b = bytes[ws_start - 1];
            if b == b' ' || b == b'\t' {
                ws_start -= 1;
            } else {
                break;
            }
        }
        if ws_start < end {
            entries.push((
                (line_start + ws_start)..(line_start + end),
                Decoration::Mark(MarkStyle {
                    bg: Some(TRAILING_WS_BG),
                    ..MarkStyle::default()
                }),
            ));
        }
    }

    RangeSet::from_iter(entries)
}
