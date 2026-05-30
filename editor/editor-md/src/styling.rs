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
/// Background painted behind `==highlight==` spans (a soft highlighter amber).
pub const COLOR_HIGHLIGHT_BG: Color = Color::rgba(232, 207, 91, 90);

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

        // Byte ranges where the `==highlight==` / color-span scans must not
        // fire: inline code and fenced/indented code blocks. Collected as we
        // walk the event stream, then handed to `scan_marks` below.
        let mut protected: Vec<std::ops::Range<usize>> = Vec::new();
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
                        if matches!(tag, Tag::CodeBlock(_)) {
                            protected.push(span.clone());
                        }
                        self.handle_end(&tag, end_tag, span);
                    }
                }
                Event::Code(_) => {
                    protected.push(byte_range.clone());
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

        // Non-CommonMark inline extensions (`==highlight==`, colored `<span>`s).
        // Run after the pulldown pass so they can skip code regions the parser
        // already classified.
        self.scan_highlights(&protected);
        self.scan_color_spans(&protected);
    }

    /// True if `span` overlaps any protected (code) range or the frontmatter.
    fn is_protected(&self, span: &std::ops::Range<usize>, protected: &[std::ops::Range<usize>]) -> bool {
        self.in_frontmatter(span)
            || protected.iter().any(|p| span.start < p.end && span.end > p.start)
    }

    /// Style `==highlight==` spans: an amber background over the inner text,
    /// with the `==` markers hidden when the cursor is off the line (matching
    /// how emphasis / strong markers reveal on the cursor line).
    fn scan_highlights(&mut self, protected: &[std::ops::Range<usize>]) {
        let bytes = self.text.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if !(bytes[i] == b'=' && bytes[i + 1] == b'=') {
                i += 1;
                continue;
            }
            let open = i;
            let inner_start = i + 2;
            // Find the closing `==` on the same line.
            let mut j = inner_start;
            let mut close = None;
            while j + 1 < bytes.len() && bytes[j] != b'\n' {
                if bytes[j] == b'=' && bytes[j + 1] == b'=' {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            match close {
                Some(c) if c > inner_start && !self.is_protected(&(open..c + 2), protected) => {
                    self.entries.push((
                        inner_start..c,
                        Decoration::Mark(MarkStyle {
                            bg: Some(COLOR_HIGHLIGHT_BG),
                            ..MarkStyle::default()
                        }),
                    ));
                    if !self.on_cursor_line(open..c + 2) {
                        self.entries.push((open..inner_start, Decoration::Replace { display: None }));
                        self.entries.push((c..c + 2, Decoration::Replace { display: None }));
                    }
                    i = c + 2;
                }
                _ => i += 1,
            }
        }
    }

    /// Style `<span style="color:#rrggbb">…</span>` spans: paint the inner text
    /// with the parsed color and hide the surrounding tags off the cursor line.
    fn scan_color_spans(&mut self, protected: &[std::ops::Range<usize>]) {
        const OPEN: &str = "<span style=\"color:";
        let text = self.text;
        let mut from = 0;
        while let Some(rel) = text[from..].find(OPEN) {
            let open = from + rel;
            let val_start = open + OPEN.len();
            // Opening tag: `…color:VALUE">`.
            let Some(q_rel) = text[val_start..].find('"') else { break };
            let value = &text[val_start..val_start + q_rel];
            let after_q = val_start + q_rel;
            if !text[after_q..].starts_with("\">") {
                from = val_start;
                continue;
            }
            let inner_start = after_q + 2;
            let Some(close_rel) = text[inner_start..].find("</span>") else { break };
            let inner_end = inner_start + close_rel;
            let close_end = inner_end + "</span>".len();
            let span = open..close_end;
            if let Some(color) = parse_hex_color(value.trim())
                && !self.is_protected(&span, protected)
            {
                self.entries.push((
                    inner_start..inner_end,
                    Decoration::Mark(MarkStyle { fg: Some(color), ..MarkStyle::default() }),
                ));
                if !self.on_cursor_line(span.clone()) {
                    self.entries.push((open..inner_start, Decoration::Replace { display: None }));
                    self.entries.push((inner_end..close_end, Decoration::Replace { display: None }));
                }
            }
            from = close_end.max(val_start);
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
                // A lone `-`/`=` being typed as a new (sub-)list item makes
                // pulldown parse it as a Setext underline of the line above.
                // Suppress heading scale/bold so the preceding item stops
                // flashing at heading size while the sub-item is formed; a real
                // Setext heading carries its title text in the same range, so it
                // keeps its styling.
                if self.setext_heading_in_list(range) {
                    return;
                }
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
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => info.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => return,
                };
                self.style_fenced_code_block(range, &lang);
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

    fn style_fenced_code_block(&mut self, range: &std::ops::Range<usize>, lang: &str) {
        let block_active = self.on_cursor_line(range.clone());
        let line_starts = self.collect_line_starts(range);
        let pal = self.pal;
        let mut body_start: Option<usize> = None;
        let mut body_end: usize = range.start;
        for (idx, &ls) in line_starts.iter().enumerate() {
            let line_text = read_line_at(self.text, ls);
            let line_end = ls + line_text.len();
            // A line is a fence delimiter only when it consists solely of the
            // fence run (plus optional info string on the block's opening line,
            // plus trailing whitespace). The opening line — the first line of
            // pulldown-cmark's block range — may carry an info string
            // (` ```rust `); every other fence line must be the bare run.
            // Lines that merely *contain* a triple-backtick alongside other
            // content (Splunk inline comments, prose) are body, never fences,
            // so an unterminated block's trailing body lines aren't mistaken
            // for a closer and multi-block documents pair independently.
            let is_fence = is_fence_line(line_text, idx == 0);
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
                if body_start.is_none() {
                    body_start = Some(ls);
                }
                body_end = line_end;
            }
        }
        // Syntax-highlight pass over the block body. Skipped if the language
        // isn't recognised (tokenize_block returns empty) or there's no body.
        if let Some(start) = body_start {
            if body_end > start && body_end <= self.text.len() {
                let content = &self.text[start..body_end];
                for (range, color) in crate::syntax::tokenize_block(lang, content, start) {
                    self.entries.push((
                        range,
                        Decoration::Mark(MarkStyle {
                            monospace: true,
                            fg: Some(color),
                            ..MarkStyle::default()
                        }),
                    ));
                }
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

    /// True when a heading event is actually a lone `-`/`=` being typed as a new
    /// (sub-)list item, which pulldown parses as a one-character Setext
    /// underline of the line above — not a genuine heading.
    ///
    /// Indenting a new bullet beneath a list item makes the buffer transiently
    /// read `- item one\n  -\n`; pulldown then continues the paragraph into a
    /// Setext H2 whose source range spans both the title line and the lone
    /// underline (e.g. `item one\n  -`). The discriminator is the *underline*
    /// (the heading range's last line): a real Setext underline is always a run
    /// of 3+ `-`/`=` (people write `---`/`===`), whereas the nascent-list shape
    /// is a single `-`/`=` (plus optional indentation / trailing space). A
    /// one-character underline is never an intentional Setext heading, so
    /// suppressing it stops the preceding list item flashing at heading size
    /// while the sub-item is formed. ATX (`# title`) is excluded by the leading
    /// `#`; frontmatter is already suppressed upstream by `in_frontmatter`.
    fn setext_heading_in_list(&self, range: &std::ops::Range<usize>) -> bool {
        // ATX (`# title`) is never a Setext misparse.
        if self.leading_hash_count(range.start) > 0 {
            return false;
        }
        let src = &self.text[range.start..range.end.min(self.text.len())];
        // The underline is the final non-empty line of the heading's source.
        let underline = src.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        let trimmed = underline.trim();
        // A genuine underline is a run of 3+ identical chars; the nascent-list
        // misparse is a lone single `-`/`=`.
        trimmed == "-" || trimmed == "="
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

/// True when `line` is a code-fence delimiter: after trimming surrounding
/// whitespace it begins with a run of 3+ identical fence characters
/// (`` ` `` or `~`) and the remainder is a valid info string for that
/// position. A *closing* fence (`allow_info == false`) must be the bare run
/// — nothing may follow it. An *opening* fence (`allow_info == true`) may
/// carry an info string, but a backtick info string may not itself contain
/// a backtick (CommonMark), which is what keeps inline `` ```x``` `` lines
/// out of the fence path.
fn is_fence_line(line: &str, allow_info: bool) -> bool {
    let trimmed = line.trim();
    let fence_char = match trimmed.as_bytes().first() {
        Some(b'`') => '`',
        Some(b'~') => '~',
        _ => return false,
    };
    let run = trimmed.chars().take_while(|&c| c == fence_char).count();
    if run < 3 {
        return false;
    }
    let info = trimmed[run..].trim();
    if info.is_empty() {
        return true;
    }
    // Non-empty trailing content: only an opening fence may carry it, and a
    // backtick fence's info string may never contain a backtick.
    allow_info && !(fence_char == '`' && info.contains('`'))
}

/// Parse a `#rgb` / `#rrggbb` CSS hex color into a [`Color`]. Returns `None`
/// for any other form (named colors, `rgb()`, etc.) — the color button only
/// ever emits hex, so unknown forms simply render unstyled.
fn parse_hex_color(value: &str) -> Option<Color> {
    let h = value.strip_prefix('#')?;
    let byte = |r: std::ops::Range<usize>| u8::from_str_radix(h.get(r)?, 16).ok();
    match h.len() {
        6 => Some(Color::rgb(byte(0..2)?, byte(2..4)?, byte(4..6)?)),
        3 => {
            // `#rgb` → each nibble doubled (`#abc` == `#aabbcc`).
            let nib = |i: usize| u8::from_str_radix(h.get(i..i + 1)?, 16).ok().map(|v| v * 17);
            Some(Color::rgb(nib(0)?, nib(1)?, nib(2)?))
        }
        _ => None,
    }
}

fn read_line_at(text: &str, line_start: usize) -> &str {
    let bytes = text.as_bytes();
    let mut end = line_start;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    &text[line_start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::decoration::Decoration;
    use editor_core::state::Editor as EditorState;

    /// Run the markdown decoration pass over `src` with the cursor at the very
    /// start of the document (so off-line suppression applies to later lines).
    fn decos(src: &str) -> Vec<Decoration> {
        let state = EditorState::new(src);
        let set = markdown_decorations(&state, None);
        set.iter_all().map(|(_, d)| d.clone()).collect()
    }

    /// True if any decoration applies heading scale/bold styling: either a
    /// `Line` with a `height_scale` above 1.0 or a bold `Mark` with a
    /// `font_scale` above 1.0. Heading styling is the only producer of those.
    fn has_heading(decos: &[Decoration]) -> bool {
        decos.iter().any(|d| match d {
            Decoration::Line(l) => l.height_scale.is_some_and(|s| s > 1.0),
            Decoration::Mark(m) => m.bold && m.font_scale.is_some_and(|s| s > 1.0),
            _ => false,
        })
    }

    #[test]
    fn atx_heading_is_styled() {
        // Regression guard: a real ATX heading must still get heading styling.
        assert!(has_heading(&decos("## Heading\n")), "ATX heading must be styled");
    }

    #[test]
    fn setext_h2_underline_styled_at_top_level() {
        // Regression guard: a genuine top-level Setext heading (`Title\n-----`)
        // is not inside a list and must keep its H2 styling.
        assert!(
            has_heading(&decos("Title\n-----\n")),
            "top-level Setext H2 heading must be styled"
        );
    }

    #[test]
    fn setext_h1_underline_styled_at_top_level() {
        assert!(
            has_heading(&decos("Title\n=====\n")),
            "top-level Setext H1 heading must be styled"
        );
    }

    #[test]
    fn setext_underline_from_list_subitem_not_heading() {
        // While typing an indented sub-bullet the buffer is `- item one\n  -\n`.
        // pulldown lazily reparses the item paragraph as a Setext H2 spanning
        // the list line; the preceding item must NOT render at heading size.
        assert!(
            !has_heading(&decos("- item one\n  -\n")),
            "list sub-item underline must not style a heading"
        );
    }

    #[test]
    fn setext_underline_from_list_subitem_trailing_space_not_heading() {
        // Pressing space after the bullet (`- `) leaves a trailing space on the
        // nascent underline line; it must still be treated as a list sub-item.
        assert!(
            !has_heading(&decos("- item one\n  - ")),
            "list sub-item underline with trailing space must not style a heading"
        );
    }

    #[test]
    fn setext_underline_from_list_subitem_cursor_off_line() {
        // Same trap, with another line following so the cursor (at offset 0) is
        // clearly off the misparsed heading line.
        assert!(
            !has_heading(&decos("- item one\n  -\nmore\n")),
            "list sub-item underline must not style a heading"
        );
    }

    #[test]
    fn setext_eq_underline_from_list_subitem_not_heading() {
        // The `=` underline (would-be H1) variant inside a list.
        assert!(
            !has_heading(&decos("- item one\n  =\n")),
            "list sub-item `=` underline must not style a heading"
        );
    }

    #[test]
    fn setext_underline_from_ordered_list_subitem_not_heading() {
        assert!(
            !has_heading(&decos("1. item one\n   -\n")),
            "ordered list sub-item underline must not style a heading"
        );
    }
}
