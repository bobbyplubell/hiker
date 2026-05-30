//! Transclusion decorations: `![[Target]]` and
//! `![[Target#section]]`.
//!
//! When the cursor is not on the same line as a transclusion, the entire
//! `![[…]]` span is replaced with a chip-style preview (`"📄 <target>"`),
//! plus a Mark to color the underlying text when revealed. No block widget
//! is rendered — when block widget machinery lands, this should switch to
//! `Block { side: Above, kind: Widget }`.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
use smol_str::SmolStr;

/// Transclusion chip color — muted gray, distinct from wikilink blue.
pub const COLOR_TRANSCLUSION: Color = Color::rgb(140, 140, 150);

pub fn transclusion_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    // Transclusions don't have a dedicated theme slot; fall back to the
    // theme's dim/quote_bar color if a theme is provided.
    let fg = theme.map(|t| t.palette.dim).unwrap_or(COLOR_TRANSCLUSION);
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
    while i + 2 < scan_end {
        if bytes[i] == b'!' && bytes[i + 1] == b'[' && bytes[i + 2] == b'[' {
            let mut j = i + 3;
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
            let inner_start = i + 3;
            let inner_end = close_start;
            let full_end = close_start + 2;
            let inner = &text[inner_start..inner_end];
            if inner.is_empty() || inner.contains(']') {
                i = full_end;
                continue;
            }

            // Target is everything before an optional `#section` or `|alias`.
            let target_end = inner
                .find(['#', '|'])
                .unwrap_or(inner.len());
            let target = &inner[..target_end];

            let span_line_start = line_of(i);
            let span_line_end = line_of(full_end.saturating_sub(1).max(i));
            let on_cursor = cursor_line >= span_line_start && cursor_line <= span_line_end;

            if !on_cursor {
                let display = format!("[{target}]");
                entries.push((
                    i..full_end,
                    Decoration::Replace {
                        display: Some(SmolStr::from(display)),
                    },
                ));
            }

            entries.push((
                i..full_end,
                Decoration::Mark(MarkStyle {
                    fg: Some(fg),
                    atomic: true,
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
