//! Obsidian-style callouts. A callout is a blockquote whose first content line
//! starts with `[!type]` (case-insensitive). Supported types: note, warning,
//! tip, info, important. Each gets a distinct background tint and left bar
//! color. These decorations stack on top of the regular blockquote styling.

use editor_core::{
    Color, Decoration, DecorationSet, EditorState, LineStyle, MarkStyle, RangeSet, Theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalloutType {
    Note,
    Warning,
    Tip,
    Info,
    Important,
}

impl CalloutType {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "note" => Some(Self::Note),
            "warning" => Some(Self::Warning),
            "tip" => Some(Self::Tip),
            "info" => Some(Self::Info),
            "important" => Some(Self::Important),
            _ => None,
        }
    }

    fn bg(self) -> Color {
        match self {
            Self::Note => Color::rgba(86, 156, 214, 30),
            Self::Warning => Color::rgba(220, 170, 60, 35),
            Self::Tip => Color::rgba(80, 200, 120, 35),
            Self::Info => Color::rgba(100, 180, 220, 30),
            Self::Important => Color::rgba(220, 90, 110, 35),
        }
    }

    fn themed_bg(self, theme: Option<&Theme>) -> Color {
        match theme {
            None => self.bg(),
            Some(t) => match self {
                Self::Note => t.markdown.callout_note_bg,
                Self::Warning => t.markdown.callout_warning_bg,
                Self::Tip => t.markdown.callout_tip_bg,
                // Info / Important have no dedicated theme slot in v1; reuse the
                // hardcoded defaults so themes don't need to enumerate every
                // possible callout type.
                Self::Info => self.bg(),
                Self::Important => self.bg(),
            },
        }
    }

    fn bar(self) -> Color {
        match self {
            Self::Note => Color::rgb(86, 156, 214),
            Self::Warning => Color::rgb(220, 170, 60),
            Self::Tip => Color::rgb(80, 200, 120),
            Self::Info => Color::rgb(100, 180, 220),
            Self::Important => Color::rgb(220, 90, 110),
        }
    }
}

pub fn callout_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<std::ops::Range<usize>>,
) -> DecorationSet {
    let text = state.doc.to_string();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let line_range = match viewport.as_ref() {
        Some(vp) => editor_view::viewport_lines(&state.doc, vp),
        None => 0..total_lines,
    };
    let scan_end_line = line_range.end;
    let mut line = line_range.start;
    while line < scan_end_line {
        let ls = state.doc.line_to_byte(line);
        let le = line_byte_end(line);
        let raw = &text[ls..le];
        let line_text = raw.strip_suffix('\n').unwrap_or(raw);

        if let Some((marker_start_off, marker_end_off, type_str)) = parse_callout_head(line_text) {
            if let Some(ct) = CalloutType::parse(type_str) {
                // Find the run of consecutive blockquote lines starting at `line`.
                let mut end_line = line;
                while end_line + 1 < total_lines {
                    let ns = state.doc.line_to_byte(end_line + 1);
                    let ne = line_byte_end(end_line + 1);
                    let nraw = &text[ns..ne];
                    let nline = nraw.strip_suffix('\n').unwrap_or(nraw);
                    if is_blockquote_line(nline) {
                        end_line += 1;
                    } else {
                        break;
                    }
                }

                // Emit Line bg for each line in the run.
                for l in line..=end_line {
                    let s = state.doc.line_to_byte(l);
                    let e = line_byte_end(l);
                    entries.push((
                        s..e,
                        Decoration::Line(LineStyle {
                            bg: Some(ct.themed_bg(theme)),
                            ..LineStyle::default()
                        }),
                    ));
                }

                // Color the leading `>` marker on the head line with the bar color.
                let marker_start = ls + marker_start_off;
                let marker_end = ls + marker_end_off;
                entries.push((
                    marker_start..marker_end,
                    Decoration::Mark(MarkStyle {
                        fg: Some(ct.bar()),
                        bold: true,
                        ..MarkStyle::default()
                    }),
                ));

                line = end_line + 1;
                continue;
            }
        }
        line += 1;
    }

    RangeSet::from_iter(entries)
}

/// If `line_text` is a blockquote line whose content begins with `[!type]`,
/// return `(marker_start, marker_end, type_str)`. `marker_start..marker_end`
/// covers the `>` (and trailing space if present).
fn parse_callout_head(line_text: &str) -> Option<(usize, usize, &str)> {
    let bytes = line_text.as_bytes();
    let mut p = 0;
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    if p >= bytes.len() || bytes[p] != b'>' {
        return None;
    }
    let marker_start = p;
    p += 1;
    let mut marker_end = p;
    if p < bytes.len() && bytes[p] == b' ' {
        p += 1;
        marker_end = p;
    }
    // Skip any further spaces inside the quote.
    while p < bytes.len() && bytes[p] == b' ' {
        p += 1;
    }
    // Must begin with `[!`
    if p + 2 >= bytes.len() || bytes[p] != b'[' || bytes[p + 1] != b'!' {
        return None;
    }
    let type_start = p + 2;
    let mut q = type_start;
    while q < bytes.len() && bytes[q] != b']' {
        q += 1;
    }
    if q >= bytes.len() || bytes[q] != b']' {
        return None;
    }
    let type_str = &line_text[type_start..q];
    Some((marker_start, marker_end, type_str))
}

fn is_blockquote_line(line_text: &str) -> bool {
    let bytes = line_text.as_bytes();
    let mut p = 0;
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    p < bytes.len() && bytes[p] == b'>'
}
