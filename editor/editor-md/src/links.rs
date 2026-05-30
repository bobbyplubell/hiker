//! Wikilink decorations: `[[Target]]`.
//!
//! When the cursor is off the link's line the whole `[[…]]` span collapses to
//! a styled link pill; with the cursor on the line the raw markdown stays
//! visible for editing (the standard live-preview reveal).
//!
//! Under the path-form (`wikilink-path-form`) the body is the target verbatim
//! — a bare basename (`Name`) or a vault-relative path without the `.md`
//! extension (`folder/sub/Name`). There is no `|display` alias half.
//!
//! A host may pass a `resolve` closure mapping a link target to the note's
//! *current* title (its basename, or frontmatter `title` once that's wired).
//! When present, links render as clickable [`WikilinkWidget`] pills carrying
//! the live title — a click emits a `WidgetClick` tagged with
//! [`WIKILINK_WIDGET_TAG`] so the host can resolve the target and open it.
//! A target the resolver can't place renders in a distinct unresolved style.
//! Without a resolver (read-only previews, the standalone example) links
//! fall back to a plain non-clickable `Replace`.

use editor_core::decoration::Color;
use editor_core::decoration::Decoration;
use editor_core::decoration::InlineWidget;
use editor_core::decoration::InlineWidgetDisplay;
use editor_core::decoration::MarkStyle;
use editor_core::decoration::Set as DecorationSet;
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use smol_str::SmolStr;
use std::sync::Arc;

/// Wikilink color — blue-ish, distinct from the markdown link color.
pub const COLOR_WIKILINK: Color = Color::rgb(86, 156, 214);
/// Unresolved-link color — a muted red, so a dangling ULID or unmatched name
/// reads as broken rather than as a normal link. [wikilink-unresolved]
pub const COLOR_WIKILINK_UNRESOLVED: Color = Color::rgb(224, 108, 117);

/// Tag bit OR-ed into a wikilink widget's `widget_id`. The low bits carry the
/// link's full-span start byte so the host can re-parse the link at that
/// offset on click. The tag lets the buffer panel tell wikilink `WidgetClick`s
/// apart from other inline-widget click consumers (patch-review, diff hunks).
pub const WIKILINK_WIDGET_TAG: u64 = 1 << 62;

/// Resolver closure: target (a vault path or bare basename) → the note's
/// current display title, or `None` when the target can't be resolved.
pub type TitleResolver<'a> = dyn Fn(&str) -> Option<String> + 'a;

/// Clickable inline pill for a resolved/unresolved wikilink. Renders as
/// textual inline content (`display()` is `Some`) and reports clicks so the
/// host can open the target. status: wikilink-render-live-title
struct WikilinkWidget {
    text: SmolStr,
    fg: Color,
    bg: Option<Color>,
    id: u64,
}

impl InlineWidget for WikilinkWidget {
    fn measure(&self, _font_size: f32) -> (f32, f32) {
        // Width comes from the rendered title galley (the layout takes the max
        // of this and the galley width); 0 lets the text drive the advance.
        (0.0, 0.0)
    }
    fn handles_click(&self) -> bool {
        true
    }
    fn widget_id(&self) -> u64 {
        self.id
    }
    fn display(&self) -> Option<InlineWidgetDisplay> {
        Some(InlineWidgetDisplay {
            text: self.text.clone(),
            bg: self.bg,
            fg: Some(self.fg),
            strikethrough: false,
        })
    }
}

pub fn wikilink_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    resolve: Option<&TitleResolver<'_>>,
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

            // Path-form (`wikilink-path-form`): the entire body is the target;
            // no `|alias` half. The display falls back to the target verbatim
            // when no resolver provides a title.
            let target = inner.trim();
            let display_text: &str = target;
            // Range of the styled text inside the brackets — used for the
            // alias Mark below; under path-form it's the whole body.
            let alias_range_in_inner: std::ops::Range<usize> = 0..inner.len();

            let span_line_start = line_of(i);
            let span_line_end = line_of(full_end.saturating_sub(1).max(i));
            let on_cursor =
                cursor_line >= span_line_start && cursor_line <= span_line_end;

            if !on_cursor {
                match resolve {
                    Some(resolve) => {
                        // Live-title: prefer the resolver's current title, then
                        // the stored display, then the raw target. A `None`
                        // result renders unresolved. [wikilink-render-live-title]
                        let resolved = resolve(target);
                        let (label, fg, bg) = match &resolved {
                            Some(title) => (
                                title.as_str(),
                                link_color,
                                Some(pill_bg(link_color)),
                            ),
                            None => (display_text, COLOR_WIKILINK_UNRESOLVED, None),
                        };
                        let label = if label.is_empty() { target } else { label };
                        entries.push((
                            i..full_end,
                            Decoration::InlineWidget {
                                widget: Arc::new(WikilinkWidget {
                                    text: SmolStr::from(label),
                                    fg,
                                    bg,
                                    id: WIKILINK_WIDGET_TAG | i as u64,
                                }),
                                atomic: true,
                            },
                        ));
                    }
                    None => {
                        entries.push((
                            i..full_end,
                            Decoration::Replace {
                                display: Some(SmolStr::from(display_text)),
                            },
                        ));
                    }
                }
            }

            // Always emit a Mark on the alias-or-target text inside the span,
            // so when revealed (cursor on line) the displayed text is styled.
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

/// Faint pill background tinted from the link color (low alpha).
const fn pill_bg(c: Color) -> Color {
    Color::rgba(c.r, c.g, c.b, 36)
}
