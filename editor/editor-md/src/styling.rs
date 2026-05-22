//! Markdown live-preview decoration provider.
//!
//! Parses the document with pulldown-cmark and emits `MarkStyle`, `LineStyle`,
//! and `Replace` decorations describing how the source should render.
//!
//! "Reveal source on cursor line" rule: any line that contains the main
//! selection's head renders raw (no Replace decorations on it). Other lines
//! hide syntax markers.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::LineStyle;

use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
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


/// State threaded through every decoration-emitting helper during one
/// parse. Owning the entries vec + cursor-line lookup as fields lets the
/// helpers be `self`-methods, which keeps each focused and avoids the
/// `single_call_fn` lint over a host of one-shot helpers.
struct MdScan<'a> {
    text: &'a str,
    pal: MdPalette,
    state: &'a EditorState,
    cursor_line: usize,
    doc_len: usize,
    entries: Vec<(std::ops::Range<usize>, Decoration)>,
    frontmatter_range: Option<std::ops::Range<usize>>,
}

pub fn markdown_decorations(state: &EditorState, theme: Option<&Theme>) -> DecorationSet {
    let pal = match theme {
        None => MdPalette {
            link: COLOR_LINK,
            code_bg: COLOR_CODE_BG,
            quote_bg: COLOR_QUOTE_BG,
            quote_bar: COLOR_QUOTE_BAR,
            heading_rule: COLOR_HEADING_RULE,
        },
        Some(t) => MdPalette {
            link: t.markdown.link,
            code_bg: t.markdown.code_bg,
            quote_bg: t.markdown.quote_bg,
            quote_bar: t.markdown.quote_bar,
            heading_rule: COLOR_HEADING_RULE,
        },
    };
    let cursor = state.selection.main().head.offset();
    let cursor_line = state.doc.byte_to_line(cursor);
    let text = state.doc.to_string();
    let doc_len = text.len();

    let mut scan = MdScan {
        text: &text,
        pal,
        state,
        cursor_line,
        doc_len,
        entries: Vec::new(),
        frontmatter_range: None,
    };
    // Detect a YAML frontmatter block at the very top so we can (a)
    // exclude pulldown-cmark's structural events inside it (otherwise the
    // closing `---` reads as a Setext H2 underline for the last YAML key,
    // promoting it to heading size + bold) and (b) style the whole block
    // as plain monospace.
    scan.frontmatter_range = scan.detect_frontmatter_range();
    scan.run();
    RangeSet::from_iter(scan.entries)
}

impl<'a> MdScan<'a> {
    fn line_of(&self, byte: usize) -> usize {
        self.state.doc.byte_to_line(byte.min(self.doc_len))
    }

    fn on_cursor_line(&self, range: std::ops::Range<usize>) -> bool {
        let start_line = self.line_of(range.start);
        let end_line = self.line_of(range.end.saturating_sub(1).max(range.start));
        self.cursor_line >= start_line && self.cursor_line <= end_line
    }

    const fn in_frontmatter(&self, r: &std::ops::Range<usize>) -> bool {
        match &self.frontmatter_range {
            Some(fm) => r.start < fm.end && r.end > fm.start,
            None => false,
        }
    }

    fn run(&mut self) {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_GFM);

        // Apply a single Mark over the frontmatter to force monospace + plain
        // styling — this layer paints under whatever the per-event handlers
        // emit, but those handlers skip events inside the range so there's
        // nothing to override.
        if let Some(fm) = self.frontmatter_range.clone() {
            self.entries.push((
                fm,
                Decoration::Mark(MarkStyle {
                    monospace: true,
                    font_scale: Some(1.0),
                    ..MarkStyle::default()
                }),
            ));
        }

        // pulldown-cmark borrows from `self.text`; collect events into a
        // local Vec so we can call `&mut self` methods while iterating.
        let events: Vec<_> = Parser::new_ext(self.text, opts)
            .into_offset_iter()
            .collect();

        let mut stack: Vec<(Tag, std::ops::Range<usize>)> = Vec::new();
        for (event, byte_range) in events {
            if self.in_frontmatter(&byte_range) {
                continue;
            }
            match event {
                Event::Start(tag) => {
                    stack.push((tag.clone(), byte_range.clone()));
                    self.handle_start(&tag, &byte_range);
                }
                Event::End(end_tag) => {
                    if let Some((tag, start_range)) = stack.pop() {
                        let span = start_range.start..byte_range.end;
                        self.handle_end(&tag, end_tag, span);
                    }
                }
                Event::Code(_) => {
                    // Inline code: style the whole span and hide the backticks.
                    let inner = strip_marker(self.text, &byte_range, '`');
                    if let Some(inner) = inner {
                        let code_bg = self.pal.code_bg;
                        self.entries.push((
                            inner.clone(),
                            Decoration::Mark(MarkStyle {
                                monospace: true,
                                bg: Some(code_bg),
                                ..MarkStyle::default()
                            }),
                        ));
                        if !self.on_cursor_line(byte_range.clone()) {
                            self.entries.push((
                                byte_range.start..inner.start,
                                Decoration::Replace { display: None },
                            ));
                            self.entries.push((
                                inner.end..byte_range.end,
                                Decoration::Replace { display: None },
                            ));
                        }
                    }
                }
                Event::TaskListMarker(checked) if !self.on_cursor_line(byte_range.clone()) => {
                    let glyph = if checked { "[x] " } else { "[ ] " };
                    self.entries.push((
                        byte_range.clone(),
                        Decoration::Replace { display: Some(SmolStr::from(glyph)) },
                    ));
                }
                Event::Rule => {
                    let heading_rule = self.pal.heading_rule;
                    self.entries.push((
                        byte_range.clone(),
                        Decoration::Line(LineStyle {
                            bg: Some(heading_rule),
                            ..LineStyle::default()
                        }),
                    ));
                }
                _ => {}
            }
        }
    }

    /// Find the byte range of a leading `---\n…\n---\n` frontmatter block.
    /// The opening `---` must be the very first line of the document. Returns
    /// the inclusive range covering both fences plus the YAML body.
    fn detect_frontmatter_range(&self) -> Option<std::ops::Range<usize>> {
        let text = self.text;
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

    fn handle_start(&mut self, tag: &Tag, range: &std::ops::Range<usize>) {
        match tag {
            Tag::Heading { level, .. } => {
                let scale = self.heading_scale(*level);
                self.entries.push((
                    range.clone(),
                    Decoration::Line(LineStyle {
                        height_scale: Some(scale * 1.0),
                        ..LineStyle::default()
                    }),
                ));
                self.entries.push((
                    range.clone(),
                    Decoration::Mark(MarkStyle {
                        bold: true,
                        font_scale: Some(scale),
                        ..MarkStyle::default()
                    }),
                ));
                if !self.on_cursor_line(range.clone()) {
                    // Only hide `#` markers for ATX headings (`# title`). Setext
                    // headings (`title\n===` / `title\n---`) don't HAVE a prefix
                    // to hide; eating a char would chop the heading text itself.
                    let leading_hashes = self.leading_hash_count(range.start);
                    if leading_hashes > 0 {
                        let prefix_len = leading_hashes + 1; // hashes + the space after
                        self.entries.push((
                            range.start..range.start + prefix_len.min(range.len()),
                            Decoration::Replace { display: None },
                        ));
                    }
                }
            }
            Tag::BlockQuote(_) => {
                self.style_blockquote(range);
            }
            Tag::CodeBlock(kind) => {
                // Only honour fenced code blocks (triple-backtick / triple-tilde).
                // Indented (4-space) blocks are too easy to trigger accidentally
                // in prose and aren't what users mean when they want "code"
                // styling.
                if !matches!(kind, pulldown_cmark::CodeBlockKind::Fenced(_)) {
                    return;
                }
                self.style_fenced_code_block(range);
            }
            _ => {}
        }
    }

    fn style_blockquote(&mut self, range: &std::ops::Range<usize>) {
        let pal = self.pal;
        // Collect line starts first so the inner closure doesn't need to
        // borrow self mutably AND read text from self.
        let mut emit: Vec<(usize, usize, bool, Option<(usize, usize)>)> = Vec::new();
        self.each_line_in(range, |line_start, line_text| {
            let line_end = line_start + line_text.len();
            let bytes = line_text.as_bytes();
            let mut p = 0;
            while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
                p += 1;
            }
            let marker = if p < bytes.len() && bytes[p] == b'>' {
                let marker_start = line_start + p;
                let marker_end = marker_start
                    + if p + 1 < bytes.len() && bytes[p + 1] == b' ' { 2 } else { 1 };
                Some((marker_start, marker_end))
            } else {
                None
            };
            emit.push((line_start, line_end, false, marker));
        });
        for (line_start, line_end, _, marker) in emit {
            // Per-line background (Line decorations are 1:1 with their starting line).
            self.entries.push((
                line_start..line_end + 1,
                Decoration::Line(LineStyle {
                    bg: Some(pal.quote_bg),
                    ..LineStyle::default()
                }),
            ));
            if let Some((marker_start, marker_end)) = marker {
                // Always color the marker.
                self.entries.push((
                    marker_start..marker_end,
                    Decoration::Mark(MarkStyle {
                        fg: Some(pal.quote_bar),
                        ..MarkStyle::default()
                    }),
                ));
                // Replace `>` (or `> `) with a vertical bar when cursor is off this line.
                if !self.on_cursor_line(line_start..line_end + 1) {
                    self.entries.push((
                        marker_start..marker_end,
                        Decoration::Replace { display: Some(SmolStr::from("| ")) },
                    ));
                }
            }
        }
    }

    fn style_fenced_code_block(&mut self, range: &std::ops::Range<usize>) {
        let block_active = self.on_cursor_line(range.clone());
        let line_starts = self.collect_line_starts(range);
        let first_ls = line_starts.first().copied();
        let last_ls = line_starts.last().copied();
        let pal = self.pal;
        for &ls in &line_starts {
            let line_text = read_line_at(self.text, ls);
            let line_end = ls + line_text.len();
            let is_fence = Some(ls) == first_ls || Some(ls) == last_ls;
            if is_fence {
                if !block_active {
                    self.entries.push((
                        ls..line_end + 1,
                        Decoration::Line(LineStyle { hide: true, ..LineStyle::default() }),
                    ));
                } else {
                    self.entries.push((
                        ls..line_end + 1,
                        Decoration::Line(LineStyle {
                            bg: Some(pal.code_bg),
                            ..LineStyle::default()
                        }),
                    ));
                }
            } else {
                self.entries.push((
                    ls..line_end + 1,
                    Decoration::Line(LineStyle {
                        bg: Some(pal.code_bg),
                        ..LineStyle::default()
                    }),
                ));
                self.entries.push((
                    ls..line_end,
                    Decoration::Mark(MarkStyle {
                        monospace: true,
                        ..MarkStyle::default()
                    }),
                ));
            }
        }
    }

    fn handle_end(
        &mut self,
        tag: &Tag,
        _end_tag: TagEnd,
        span: std::ops::Range<usize>,
    ) {
        match tag {
            Tag::Emphasis => {
                let inner = strip_marker(self.text, &span, '*').or_else(|| strip_marker(self.text, &span, '_'));
                if let Some(inner) = inner {
                    self.entries.push((
                        inner.clone(),
                        Decoration::Mark(MarkStyle { italic: true, ..MarkStyle::default() }),
                    ));
                    if !self.on_cursor_line(span.clone()) {
                        self.entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                        self.entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                    }
                }
            }
            Tag::Strong => {
                let inner = strip_marker_double(self.text, &span, "**").or_else(|| strip_marker_double(self.text, &span, "__"));
                if let Some(inner) = inner {
                    self.entries.push((
                        inner.clone(),
                        Decoration::Mark(MarkStyle { bold: true, ..MarkStyle::default() }),
                    ));
                    if !self.on_cursor_line(span.clone()) {
                        self.entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                        self.entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                    }
                }
            }
            Tag::Strikethrough => {
                let inner = strip_marker_double(self.text, &span, "~~");
                if let Some(inner) = inner {
                    self.entries.push((
                        inner.clone(),
                        Decoration::Mark(MarkStyle { strikethrough: true, ..MarkStyle::default() }),
                    ));
                    if !self.on_cursor_line(span.clone()) {
                        self.entries.push((span.start..inner.start, Decoration::Replace { display: None }));
                        self.entries.push((inner.end..span.end, Decoration::Replace { display: None }));
                    }
                }
            }
            Tag::Link { .. } => {
                // [label](url) — style the label, hide the brackets/url.
                if let Some(label_range) = self.find_link_label(&span) {
                    let link = self.pal.link;
                    self.entries.push((
                        label_range.clone(),
                        Decoration::Mark(MarkStyle {
                            fg: Some(link),
                            underline: true,
                            ..MarkStyle::default()
                        }),
                    ));
                    if !self.on_cursor_line(span.clone()) {
                        self.entries.push((span.start..label_range.start, Decoration::Replace { display: None }));
                        self.entries.push((label_range.end..span.end, Decoration::Replace { display: None }));
                    }
                }
            }
            Tag::Item => {
                // Replace just the leading marker ("- ", "* ", "+ ") with a bullet
                // glyph. The leading indent whitespace is preserved so nested
                // items still render shifted right.
                let line_start = self.line_byte_start(span.start);
                let after_indent = self.text[line_start..]
                    .bytes()
                    .take_while(|&b| b == b' ' || b == b'\t')
                    .count();
                let marker_start = line_start + after_indent;
                let marker_end = self.list_marker_end(line_start);
                if marker_end > marker_start && !self.on_cursor_line(line_start..marker_end) {
                    let marker = &self.text[marker_start..marker_end];
                    let is_ordered = marker.chars().next().is_some_and(|c| c.is_ascii_digit());
                    if !is_ordered {
                        self.entries.push((
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

    const fn heading_scale(&self, level: HeadingLevel) -> f32 {
        match level {
            HeadingLevel::H1 => 2.0,
            HeadingLevel::H2 => 1.6,
            HeadingLevel::H3 => 1.4,
            HeadingLevel::H4 => 1.2,
            HeadingLevel::H5 => 1.1,
            HeadingLevel::H6 => 1.05,
        }
    }

    fn leading_hash_count(&self, start: usize) -> usize {
        self.text[start..]
            .bytes()
            .take_while(|&b| b == b'#')
            .count()
    }

    fn find_link_label(&self, range: &std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
        let slice = &self.text[range.clone()];
        let start = slice.find('[')? + 1;
        let close = slice[start..].find(']')? + start;
        Some(range.start + start..range.start + close)
    }

    fn line_byte_start(&self, byte: usize) -> usize {
        self.text[..byte.min(self.text.len())]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn each_line_in<F: FnMut(usize, &str)>(&self, range: &std::ops::Range<usize>, mut f: F) {
        let mut p = range.start;
        while p < range.end && p < self.text.len() {
            let line_text = read_line_at(self.text, p);
            f(p, line_text);
            let advance = line_text.len() + 1; // +1 for the newline (or 0 if EOF)
            p += advance;
        }
    }

    fn collect_line_starts(&self, range: &std::ops::Range<usize>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut p = range.start;
        while p < range.end && p < self.text.len() {
            out.push(p);
            let line_text = read_line_at(self.text, p);
            p += line_text.len() + 1;
        }
        out
    }

    fn list_marker_end(&self, line_start: usize) -> usize {
        let text = self.text;
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

fn read_line_at(text: &str, line_start: usize) -> &str {
    let bytes = text.as_bytes();
    let mut end = line_start;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    &text[line_start..end]
}
