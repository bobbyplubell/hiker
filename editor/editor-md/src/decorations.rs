//! Markdown live-preview decoration provider.
//!
//! Parses the document with pulldown-cmark and emits `MarkStyle`, `LineStyle`,
//! and `Replace` decorations describing how the source should render.
//!
//! "Reveal source on cursor line" rule: any line that contains the main
//! selection's head renders raw (no Replace decorations on it). Other lines
//! hide syntax markers.

use editor_core::{
    Color, Decoration, DecorationSet, EditorState, LineStyle, MarkStyle, RangeSet, Theme,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use smol_str::SmolStr;

pub type MarkdownDecoration = Decoration;

pub const COLOR_LINK: Color = Color::rgb(86, 156, 214);
pub const COLOR_CODE_BG: Color = Color::rgba(120, 120, 120, 30);
pub const COLOR_QUOTE_BG: Color = Color::rgba(120, 120, 120, 20);
pub const COLOR_QUOTE_BAR: Color = Color::rgb(140, 140, 160);
pub const COLOR_HEADING_RULE: Color = Color::rgba(120, 120, 140, 50);

#[derive(Clone, Copy)]
struct MdPalette {
    link: Color,
    code_bg: Color,
    quote_bg: Color,
    quote_bar: Color,
    heading_rule: Color,
}

impl MdPalette {
    fn from_theme(theme: Option<&Theme>) -> Self {
        match theme {
            None => Self {
                link: COLOR_LINK,
                code_bg: COLOR_CODE_BG,
                quote_bg: COLOR_QUOTE_BG,
                quote_bar: COLOR_QUOTE_BAR,
                heading_rule: COLOR_HEADING_RULE,
            },
            Some(t) => Self {
                link: t.markdown.link,
                code_bg: t.markdown.code_bg,
                quote_bg: t.markdown.quote_bg,
                quote_bar: t.markdown.quote_bar,
                heading_rule: COLOR_HEADING_RULE,
            },
        }
    }
}

pub fn markdown_decorations(state: &EditorState, theme: Option<&Theme>) -> DecorationSet {
    let pal = MdPalette::from_theme(theme);
    let cursor = state.selection.main().head.offset();
    let cursor_line = state.doc.byte_to_line(cursor);
    let text = state.doc.to_string();
    let doc_len = text.len();

    // Detect a YAML frontmatter block at the very top so we can (a)
    // exclude pulldown-cmark's structural events inside it (otherwise the
    // closing `---` reads as a Setext H2 underline for the last YAML key,
    // promoting it to heading size + bold) and (b) style the whole block
    // as plain monospace.
    let frontmatter_range: Option<std::ops::Range<usize>> = detect_frontmatter_range(&text);

    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_GFM);
    let parser = Parser::new_ext(&text, opts).into_offset_iter();

    let line_of = |byte: usize| -> usize {
        state.doc.byte_to_line(byte.min(doc_len))
    };
    let on_cursor_line = |range: std::ops::Range<usize>| -> bool {
        let start_line = line_of(range.start);
        let end_line = line_of(range.end.saturating_sub(1).max(range.start));
        cursor_line >= start_line && cursor_line <= end_line
    };

    let mut stack: Vec<(Tag, std::ops::Range<usize>)> = Vec::new();
    let in_frontmatter = |r: &std::ops::Range<usize>| -> bool {
        match &frontmatter_range {
            Some(fm) => r.start < fm.end && r.end > fm.start,
            None => false,
        }
    };

    // Apply a single Mark over the frontmatter to force monospace + plain
    // styling — this layer paints under whatever the per-event handlers
    // emit, but those handlers skip events inside the range so there's
    // nothing to override.
    if let Some(fm) = frontmatter_range.as_ref() {
        entries.push((
            fm.clone(),
            Decoration::Mark(MarkStyle {
                monospace: true,
                font_scale: Some(1.0),
                ..MarkStyle::default()
            }),
        ));
    }

    for (event, byte_range) in parser {
        if in_frontmatter(&byte_range) {
            continue;
        }
        match event {
            Event::Start(tag) => {
                stack.push((tag.clone(), byte_range.clone()));
                handle_start(&tag, &byte_range, &text, &pal, &on_cursor_line, &mut entries);
            }
            Event::End(end_tag) => {
                if let Some((tag, start_range)) = stack.pop() {
                    let span = start_range.start..byte_range.end;
                    handle_end(
                        &tag, &end_tag, span, &byte_range, &text, &pal, &on_cursor_line, &mut entries,
                    );
                }
            }
            Event::Code(_) => {
                // Inline code: style the whole span and hide the backticks.
                let inner = strip_marker(&text, &byte_range, '`');
                if let Some(inner) = inner {
                    entries.push((
                        inner.clone(),
                        Decoration::Mark(MarkStyle {
                            monospace: true,
                            bg: Some(pal.code_bg),
                            ..MarkStyle::default()
                        }),
                    ));
                    if !on_cursor_line(byte_range.clone()) {
                        entries.push((
                            byte_range.start..inner.start,
                            Decoration::Replace { display: None },
                        ));
                        entries.push((
                            inner.end..byte_range.end,
                            Decoration::Replace { display: None },
                        ));
                    }
                }
            }
            Event::TaskListMarker(checked) if !on_cursor_line(byte_range.clone()) => {
                let glyph = if checked { "[x] " } else { "[ ] " };
                entries.push((
                    byte_range.clone(),
                    Decoration::Replace { display: Some(SmolStr::from(glyph)) },
                ));
            }
            Event::Rule => {
                entries.push((
                    byte_range.clone(),
                    Decoration::Line(LineStyle {
                        bg: Some(pal.heading_rule),
                        ..LineStyle::default()
                    }),
                ));
            }
            _ => {}
        }
    }

    RangeSet::from_iter(entries)
}

/// Find the byte range of a leading `---\n…\n---\n` frontmatter block.
/// The opening `---` must be the very first line of the document. Returns
/// the inclusive range covering both fences plus the YAML body.
fn detect_frontmatter_range(text: &str) -> Option<std::ops::Range<usize>> {
    if !text.starts_with("---\n") && !text.starts_with("---\r\n") {
        return None;
    }
    // Walk lines from the second one looking for a `---` close.
    let mut pos = if text.starts_with("---\r\n") { 5 } else { 4 };
    let bytes = text.as_bytes();
    while pos < bytes.len() {
        let line_start = pos;
        // Find end of this line.
        let nl = bytes[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(bytes.len());
        let line = &text[line_start..nl];
        let stripped = line.trim_end_matches('\r');
        if stripped == "---" {
            // Include the trailing newline if present so the range
            // covers the whole fence row.
            let end = if nl < bytes.len() { nl + 1 } else { nl };
            return Some(0..end);
        }
        pos = if nl < bytes.len() { nl + 1 } else { bytes.len() };
    }
    None
}

fn handle_start(
    tag: &Tag,
    range: &std::ops::Range<usize>,
    text: &str,
    pal: &MdPalette,
    on_cursor_line: &dyn Fn(std::ops::Range<usize>) -> bool,
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
) {
    match tag {
        Tag::Heading { level, .. } => {
            let scale = heading_scale(*level);
            entries.push((
                range.clone(),
                Decoration::Line(LineStyle {
                    height_scale: Some(scale * 1.0),
                    ..LineStyle::default()
                }),
            ));
            entries.push((
                range.clone(),
                Decoration::Mark(MarkStyle {
                    bold: true,
                    font_scale: Some(scale),
                    ..MarkStyle::default()
                }),
            ));
            if !on_cursor_line(range.clone()) {
                // Only hide `#` markers for ATX headings (`# title`). Setext
                // headings (`title\n===` / `title\n---`) don't HAVE a prefix
                // to hide; eating a char would chop the heading text itself.
                let leading_hashes = leading_hash_count(text, range.start);
                if leading_hashes > 0 {
                    let prefix_len = leading_hashes + 1; // hashes + the space after
                    entries.push((
                        range.start..range.start + prefix_len.min(range.len()),
                        Decoration::Replace { display: None },
                    ));
                }
            }
        }
        Tag::BlockQuote(_) => {
            each_line_in(text, range, |line_start, line_text| {
                let line_end = line_start + line_text.len();
                // Per-line background (Line decorations are 1:1 with their starting line).
                entries.push((
                    line_start..line_end + 1,
                    Decoration::Line(LineStyle {
                        bg: Some(pal.quote_bg),
                        ..LineStyle::default()
                    }),
                ));
                // Find the `>` marker (after any leading whitespace).
                let bytes = line_text.as_bytes();
                let mut p = 0;
                while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                    p += 1;
                }
                if p < bytes.len() && bytes[p] == b'>' {
                    let marker_start = line_start + p;
                    let marker_end = marker_start
                        + if p + 1 < bytes.len() && bytes[p + 1] == b' ' { 2 } else { 1 };
                    // Always color the marker.
                    entries.push((
                        marker_start..marker_end,
                        Decoration::Mark(MarkStyle {
                            fg: Some(pal.quote_bar),
                            ..MarkStyle::default()
                        }),
                    ));
                    // Replace `>` (or `> `) with a vertical bar when cursor is off this line.
                    if !on_cursor_line(line_start..line_end + 1) {
                        entries.push((
                            marker_start..marker_end,
                            Decoration::Replace { display: Some(SmolStr::from("| ")) },
                        ));
                    }
                }
            });
        }
        Tag::CodeBlock(kind) => {
            // Only honour fenced code blocks (triple-backtick / triple-tilde).
            // Indented (4-space) blocks are too easy to trigger accidentally
            // in prose and aren't what users mean when they want "code"
            // styling.
            if !matches!(kind, pulldown_cmark::CodeBlockKind::Fenced(_)) {
                return;
            }
            let block_active = on_cursor_line(range.clone());
            let line_starts = collect_line_starts(text, range);
            let first_ls = line_starts.first().copied();
            let last_ls = line_starts.last().copied();
            for &ls in &line_starts {
                let line_text = read_line_at(text, ls);
                let line_end = ls + line_text.len();
                let is_fence = Some(ls) == first_ls || Some(ls) == last_ls;
                if is_fence {
                    if !block_active {
                        entries.push((
                            ls..line_end + 1,
                            Decoration::Line(LineStyle { hide: true, ..LineStyle::default() }),
                        ));
                    } else {
                        entries.push((
                            ls..line_end + 1,
                            Decoration::Line(LineStyle {
                                bg: Some(pal.code_bg),
                                ..LineStyle::default()
                            }),
                        ));
                    }
                } else {
                    entries.push((
                        ls..line_end + 1,
                        Decoration::Line(LineStyle {
                            bg: Some(pal.code_bg),
                            ..LineStyle::default()
                        }),
                    ));
                    entries.push((
                        ls..line_end,
                        Decoration::Mark(MarkStyle {
                            monospace: true,
                            ..MarkStyle::default()
                        }),
                    ));
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_end(
    tag: &Tag,
    _end_tag: &TagEnd,
    span: std::ops::Range<usize>,
    _final_range: &std::ops::Range<usize>,
    text: &str,
    pal: &MdPalette,
    on_cursor_line: &dyn Fn(std::ops::Range<usize>) -> bool,
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
) {
    match tag {
        Tag::Emphasis => {
            let inner = strip_marker(text, &span, '*').or_else(|| strip_marker(text, &span, '_'));
            if let Some(inner) = inner {
                entries.push((
                    inner.clone(),
                    Decoration::Mark(MarkStyle { italic: true, ..MarkStyle::default() }),
                ));
                if !on_cursor_line(span.clone()) {
                    entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                    entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                }
            }
        }
        Tag::Strong => {
            let inner = strip_marker_double(text, &span, "**").or_else(|| strip_marker_double(text, &span, "__"));
            if let Some(inner) = inner {
                entries.push((
                    inner.clone(),
                    Decoration::Mark(MarkStyle { bold: true, ..MarkStyle::default() }),
                ));
                if !on_cursor_line(span.clone()) {
                    entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                    entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                }
            }
        }
        Tag::Strikethrough => {
            let inner = strip_marker_double(text, &span, "~~");
            if let Some(inner) = inner {
                entries.push((
                    inner.clone(),
                    Decoration::Mark(MarkStyle { strikethrough: true, ..MarkStyle::default() }),
                ));
                if !on_cursor_line(span.clone()) {
                    entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                    entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                }
            }
        }
        Tag::Link { .. } => {
            // [label](url) — style the label, hide the brackets/url.
            if let Some(label_range) = find_link_label(text, &span) {
                entries.push((
                    label_range.clone(),
                    Decoration::Mark(MarkStyle {
                        fg: Some(pal.link),
                        underline: true,
                        ..MarkStyle::default()
                    }),
                ));
                if !on_cursor_line(span.clone()) {
                    entries.push((span.start..label_range.start, Decoration::Replace { display: None }));
                    entries.push((label_range.end..span.end, Decoration::Replace { display: None }));
                }
            }
        }
        Tag::Item => {
            // Replace just the leading marker ("- ", "* ", "+ ") with a bullet
            // glyph. The leading indent whitespace is preserved so nested
            // items still render shifted right.
            let line_start = line_byte_start(text, span.start);
            let after_indent = text[line_start..]
                .bytes()
                .take_while(|&b| b == b' ' || b == b'\t')
                .count();
            let marker_start = line_start + after_indent;
            let marker_end = list_marker_end(text, line_start);
            if marker_end > marker_start && !on_cursor_line(line_start..marker_end) {
                let marker = &text[marker_start..marker_end];
                let is_ordered = marker.chars().next().is_some_and(|c| c.is_ascii_digit());
                if !is_ordered {
                    entries.push((
                        marker_start..marker_end,
                        Decoration::Replace {
                            display: Some(SmolStr::from("• ")),
                        },
                    ));
                }
            }
        }
        _ => {}
    }
}

fn heading_scale(level: HeadingLevel) -> f32 {
    match level {
        HeadingLevel::H1 => 2.0,
        HeadingLevel::H2 => 1.6,
        HeadingLevel::H3 => 1.4,
        HeadingLevel::H4 => 1.2,
        HeadingLevel::H5 => 1.1,
        HeadingLevel::H6 => 1.05,
    }
}

fn leading_hash_count(text: &str, start: usize) -> usize {
    text[start..]
        .bytes()
        .take_while(|&b| b == b'#')
        .count()
}

fn strip_marker(text: &str, range: &std::ops::Range<usize>, marker: char) -> Option<std::ops::Range<usize>> {
    let slice = &text[range.clone()];
    let bytes = slice.as_bytes();
    let mut start_off = 0;
    while start_off < bytes.len() && bytes[start_off] == marker as u8 {
        start_off += 1;
    }
    let mut end_off = bytes.len();
    while end_off > start_off && bytes[end_off - 1] == marker as u8 {
        end_off -= 1;
    }
    if start_off >= end_off {
        return None;
    }
    Some(range.start + start_off..range.start + end_off)
}

fn strip_marker_double(text: &str, range: &std::ops::Range<usize>, marker: &str) -> Option<std::ops::Range<usize>> {
    let slice = &text[range.clone()];
    if !slice.starts_with(marker) || !slice.ends_with(marker) || slice.len() < marker.len() * 2 {
        return None;
    }
    Some(range.start + marker.len()..range.end - marker.len())
}

fn find_link_label(text: &str, range: &std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let slice = &text[range.clone()];
    let start = slice.find('[')? + 1;
    let close = slice[start..].find(']')? + start;
    Some(range.start + start..range.start + close)
}

fn line_byte_start(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

fn each_line_in<F: FnMut(usize, &str)>(text: &str, range: &std::ops::Range<usize>, mut f: F) {
    let mut p = range.start;
    while p < range.end && p < text.len() {
        let line_text = read_line_at(text, p);
        f(p, line_text);
        let advance = line_text.len() + 1; // +1 for the newline (or 0 if EOF)
        p += advance;
    }
}

fn collect_line_starts(text: &str, range: &std::ops::Range<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut p = range.start;
    while p < range.end && p < text.len() {
        out.push(p);
        let line_text = read_line_at(text, p);
        p += line_text.len() + 1;
    }
    out
}

fn read_line_at(text: &str, line_start: usize) -> &str {
    let bytes = text.as_bytes();
    let mut end = line_start;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    &text[line_start..end]
}

fn list_marker_end(text: &str, line_start: usize) -> usize {
    let after_indent = text[line_start..]
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let mut p = line_start + after_indent;
    let bytes = text.as_bytes();
    // Ordered: digits then '.' or ')'
    if p < bytes.len() && bytes[p].is_ascii_digit() {
        while p < bytes.len() && bytes[p].is_ascii_digit() {
            p += 1;
        }
        if p < bytes.len() && (bytes[p] == b'.' || bytes[p] == b')') {
            p += 1;
            if p < bytes.len() && bytes[p] == b' ' {
                p += 1;
            }
            return p;
        }
        return line_start;
    }
    // Unordered: -, *, +
    if p < bytes.len() && matches!(bytes[p], b'-' | b'*' | b'+') {
        p += 1;
        if p < bytes.len() && bytes[p] == b' ' {
            p += 1;
        }
        return p;
    }
    line_start
}
