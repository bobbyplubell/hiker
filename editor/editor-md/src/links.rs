//! Wikilink decorations: `[[Target]]` / `[[Target|Alias]]`.
//!
//! When the cursor is not on the same line, the whole `[[…]]` span is replaced
//! with the alias (or target) text and styled like a link. When the cursor is
//! on the line, the raw markdown stays visible for easy editing.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
use smol_str::SmolStr;

/// Wikilink color — blue-ish, distinct from the markdown link color.
pub const COLOR_WIKILINK: Color = Color::rgb(86, 156, 214);

pub fn wikilink_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let link_color = theme.map(|t| t.markdown.link).unwrap_or(COLOR_WIKILINK);
    let text = state.doc.to_string();
    let doc_len = text.len();
    let cursor = state.selection.main().head.offset();
    let cursor_line = state.doc.byte_to_line(cursor.min(doc_len));
    let line_of = |b: usize| state.doc.byte_to_line(b.min(doc_len));

    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    let bytes = text.as_bytes();
    let (scan_start, scan_end) = match viewport {
        Some(vp) => (vp.start.min(bytes.len()), vp.end.min(bytes.len())),
        None => (0, bytes.len()),
    };
    let mut i = scan_start;
    while i + 1 < scan_end {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            // Find closing `]]` on the same logical span (no newlines inside).
            let mut j = i + 2;
            let mut closed = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'\n' {
                    break;
                }
                if bytes[j] == b']' && bytes[j + 1] == b']' {
                    closed = Some(j);
                    break;
                }
                j += 1;
            }
            let Some(close_start) = closed else {
                i += 1;
                continue;
            };
            let inner_start = i + 2;
            let inner_end = close_start;
            let full_end = close_start + 2;
            let inner = &text[inner_start..inner_end];
            if inner.is_empty() || inner.contains(']') {
                i = full_end;
                continue;
            }

            // Split on '|' for alias.
            let (target, alias_range_in_inner) = if let Some(pipe) = inner.find('|') {
                let alias = (pipe + 1)..inner.len();
                (&inner[..pipe], alias)
            } else {
                (inner, 0..inner.len())
            };
            let alias_text = &inner[alias_range_in_inner.clone()];
            let display_text: &str = if alias_text.is_empty() { target } else { alias_text };

            let span_line_start = line_of(i);
            let span_line_end = line_of(full_end.saturating_sub(1).max(i));
            let on_cursor =
                cursor_line >= span_line_start && cursor_line <= span_line_end;

            if !on_cursor {
                entries.push((
                    i..full_end,
                    Decoration::Replace {
                        display: Some(SmolStr::from(display_text)),
                    },
                ));
            }

            // Always emit a Mark on the alias-or-target text inside the span,
            // so when revealed the displayed text is still styled.
            let alias_byte_range = (inner_start + alias_range_in_inner.start)
                ..(inner_start + alias_range_in_inner.end);
            entries.push((
                alias_byte_range,
                Decoration::Mark(MarkStyle {
                    fg: Some(link_color),
                    underline: true,
                    ..MarkStyle::default()
                }),
            ));

            i = full_end;
            continue;
        }
        i += 1;
    }

    RangeSet::from_iter(entries)
}
