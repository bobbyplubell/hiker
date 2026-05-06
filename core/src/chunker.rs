//! Heading-bounded chunking for v1. See docs/index.md.
//!
//! Strategy:
//! - Strip a YAML frontmatter block if present at the very start of the file.
//! - Walk the markdown with pulldown-cmark using offset events so we can track
//!   the byte range each block occupies in the original (post-frontmatter) text.
//! - Headings (H1–H6) start a new chunk and update the heading_path breadcrumb.
//! - Within a heading section, accumulate blocks until the chunk would exceed
//!   `SOFT_SIZE_LIMIT` characters; then start a new chunk inside the same
//!   heading, carrying the breadcrumb forward.
//! - Code blocks are never split: a code block always lives in a single chunk
//!   even if it busts the soft cap.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const SOFT_SIZE_LIMIT: usize = 1200;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub index: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
    /// "Section > Subsection" breadcrumb of the enclosing heading, or None for
    /// content above any heading.
    pub heading_path: Option<String>,
}

/// Split a markdown source into chunks. Frontmatter is stripped before
/// chunking; byte offsets in the returned chunks are relative to the original
/// `source` (so callers can still index into the input string).
pub fn chunk_markdown(source: &str) -> Vec<Chunk> {
    let (body_start, body) = strip_frontmatter(source);

    let mut state = ChunkBuilder::new(body_start);
    let parser = Parser::new_ext(body, Options::all()).into_offset_iter();

    // Track active block bounds. We only flush when we cross a heading boundary
    // or when a top-level block ends and the accumulated chunk has grown past
    // the soft size limit.
    let mut heading_stack: Vec<String> = Vec::new();
    let mut in_heading_text = false;
    let mut current_heading_buf = String::new();
    // Depth of nested Tag::* opens; we only consider a block "top-level"
    // (eligible for a soft-size cut) when this returns to 0.
    let mut block_depth: i32 = 0;
    // Track when we are inside a fenced/indented code block so we never
    // mid-split it.
    let mut in_code_block = false;
    // Pending block range — set when a top-level Start event fires, consumed on
    // the matching End.
    let mut pending_range: Option<(usize, usize)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(tag) => {
                if block_depth == 0 {
                    pending_range = Some((range.start, range.end));
                }
                block_depth += 1;
                match tag {
                    Tag::Heading { level, .. } => {
                        // Heading boundary: flush whatever's pending into a chunk
                        // *before* updating the breadcrumb.
                        state.flush_chunk();
                        in_heading_text = true;
                        current_heading_buf.clear();
                        // Trim the heading_stack to one shallower than this level
                        // before we push, so an H2 after an H3 pops back up.
                        let depth = heading_level_depth(level);
                        heading_stack.truncate(depth.saturating_sub(1));
                        // Seed the chunk with the heading line's own markdown so
                        // chunks containing nothing but a heading still survive
                        // the empty-trim filter at flush time.
                        let slice = &body[range.start..range.end];
                        state.append_range(range.start, range.end, slice);
                        // Suppress the End-side append for this heading.
                        pending_range = None;
                    }
                    Tag::CodeBlock(_) => {
                        in_code_block = true;
                    }
                    _ => {}
                }
            }
            Event::End(tag_end) => {
                block_depth -= 1;
                match tag_end {
                    TagEnd::Heading(_) => {
                        in_heading_text = false;
                        let title = current_heading_buf.trim().to_string();
                        heading_stack.push(title);
                        let breadcrumb = breadcrumb_from(&heading_stack);
                        state.set_heading_path(Some(breadcrumb));
                        // After the heading line itself, mark the chunk as having
                        // a fresh size budget (the title bytes already counted).
                    }
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        if block_depth == 0 {
                            if let Some((s, e)) = pending_range.take() {
                                state.append_block(s, e, body, body_start);
                            }
                            // Even if we've blown past the soft cap, never split
                            // mid-code-block — but after the code block closes,
                            // we *can* break for the next block.
                        }
                    }
                    _ => {}
                }
                if block_depth == 0 {
                    if let Some((s, e)) = pending_range.take() {
                        // Heading ranges are added on Start; everything else is
                        // appended here.
                        if !matches!(tag_end, TagEnd::Heading(_)) {
                            state.append_block(s, e, body, body_start);
                        }
                        if !in_code_block && state.size() >= SOFT_SIZE_LIMIT {
                            state.flush_chunk();
                        }
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if in_heading_text {
                    current_heading_buf.push_str(&t);
                }
            }
            _ => {}
        }
    }

    state.flush_chunk();
    state.finish()
}

fn heading_level_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn breadcrumb_from(stack: &[String]) -> String {
    stack.join(" > ")
}

/// Strip a YAML frontmatter block delimited by `---` lines at the very start of
/// the file. Returns the byte offset where the body begins (relative to the
/// original source) and the body slice.
fn strip_frontmatter(source: &str) -> (usize, &str) {
    if !source.starts_with("---\n") && !source.starts_with("---\r\n") {
        return (0, source);
    }
    let after_open = if source.starts_with("---\r\n") { 5 } else { 4 };
    // Look for a closing `---` line.
    let rest = &source[after_open..];
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find("---") {
        let abs = search_from + idx;
        // Must be at a line start (preceded by \n or be at the very start).
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        if !at_line_start {
            search_from = abs + 1;
            continue;
        }
        // After the `---`, must be end-of-input or a newline.
        let after = abs + 3;
        let valid_end = after >= rest.len()
            || rest.as_bytes()[after] == b'\n'
            || (rest.as_bytes()[after] == b'\r'
                && rest.len() > after + 1
                && rest.as_bytes()[after + 1] == b'\n');
        if valid_end {
            // Skip the closing `---` line and its newline.
            let mut body_start_in_rest = after;
            if body_start_in_rest < rest.len() && rest.as_bytes()[body_start_in_rest] == b'\r' {
                body_start_in_rest += 1;
            }
            if body_start_in_rest < rest.len() && rest.as_bytes()[body_start_in_rest] == b'\n' {
                body_start_in_rest += 1;
            }
            let body_offset = after_open + body_start_in_rest;
            return (body_offset, &source[body_offset..]);
        }
        search_from = abs + 1;
    }
    // Unterminated frontmatter — bail and treat the whole file as body.
    (0, source)
}

struct ChunkBuilder {
    body_start_offset: usize,
    next_index: u32,
    chunks: Vec<Chunk>,
    cur_byte_start: Option<usize>,
    cur_byte_end: usize,
    cur_text: String,
    cur_heading_path: Option<String>,
}

impl ChunkBuilder {
    fn new(body_start_offset: usize) -> Self {
        Self {
            body_start_offset,
            next_index: 0,
            chunks: Vec::new(),
            cur_byte_start: None,
            cur_byte_end: 0,
            cur_text: String::new(),
            cur_heading_path: None,
        }
    }

    fn size(&self) -> usize {
        self.cur_text.len()
    }

    fn set_heading_path(&mut self, path: Option<String>) {
        self.cur_heading_path = path;
    }

    fn append_block(&mut self, body_start: usize, body_end: usize, body: &str, _body_offset: usize) {
        let slice = &body[body_start..body_end];
        self.append_range(body_start, body_end, slice);
    }

    fn append_range(&mut self, body_start: usize, body_end: usize, slice: &str) {
        let abs_start = body_start + self.body_start_offset;
        let abs_end = body_end + self.body_start_offset;
        if self.cur_byte_start.is_none() {
            self.cur_byte_start = Some(abs_start);
            self.cur_text.clear();
        }
        self.cur_byte_end = abs_end;
        if !slice.is_empty() {
            // Preserve the source as-is — pulldown-cmark's offset events give
            // the original markdown bytes for each block, which is what we want
            // to embed (markers and all).
            if !self.cur_text.is_empty() && !self.cur_text.ends_with('\n') {
                self.cur_text.push('\n');
            }
            self.cur_text.push_str(slice.trim_end_matches('\n'));
        }
    }

    fn flush_chunk(&mut self) {
        if self.cur_byte_start.is_none() {
            return;
        }
        let text = self.cur_text.trim().to_string();
        if text.is_empty() {
            self.cur_byte_start = None;
            self.cur_text.clear();
            return;
        }
        self.chunks.push(Chunk {
            index: self.next_index,
            byte_start: self.cur_byte_start.unwrap(),
            byte_end: self.cur_byte_end,
            text,
            heading_path: self.cur_heading_path.clone(),
        });
        self.next_index += 1;
        self.cur_byte_start = None;
        self.cur_text.clear();
    }

    fn finish(mut self) -> Vec<Chunk> {
        self.flush_chunk();
        self.chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_no_chunks() {
        let chunks = chunk_markdown("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn whitespace_only_input_produces_no_chunks() {
        let chunks = chunk_markdown("   \n\n\t\n");
        assert!(chunks.is_empty());
    }

    #[test]
    fn no_headings_yields_one_chunk() {
        let src = "Just a paragraph.\n\nAnd another.\n";
        let chunks = chunk_markdown(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("Just a paragraph"));
        assert!(chunks[0].text.contains("And another"));
        assert!(chunks[0].heading_path.is_none());
        assert_eq!(chunks[0].index, 0);
    }

    #[test]
    fn each_heading_starts_a_new_chunk() {
        let src = "# A\n\nbody a\n\n# B\n\nbody b\n";
        let chunks = chunk_markdown(src);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading_path.as_deref(), Some("A"));
        assert_eq!(chunks[1].heading_path.as_deref(), Some("B"));
        assert_eq!(chunks[0].index, 0);
        assert_eq!(chunks[1].index, 1);
    }

    #[test]
    fn nested_headings_build_breadcrumb() {
        let src = "# A\n\n## A1\n\nbody.\n\n## A2\n\nbody.\n\n# B\n\nbody.\n";
        let chunks = chunk_markdown(src);
        let paths: Vec<_> = chunks
            .iter()
            .map(|c| c.heading_path.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(paths, vec!["A", "A > A1", "A > A2", "B"]);
    }

    #[test]
    fn content_above_any_heading_has_no_breadcrumb() {
        let src = "intro paragraph\n\n# First\n\nbody\n";
        let chunks = chunk_markdown(src);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].heading_path.is_none());
        assert_eq!(chunks[1].heading_path.as_deref(), Some("First"));
    }

    #[test]
    fn frontmatter_is_stripped() {
        let src = "---\ntitle: hello\ntags: [x]\n---\n\n# Real Heading\n\nbody\n";
        let chunks = chunk_markdown(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path.as_deref(), Some("Real Heading"));
        assert!(!chunks[0].text.contains("title: hello"));
        // byte_start should be past the frontmatter.
        assert!(chunks[0].byte_start >= src.find("# Real").unwrap());
    }

    #[test]
    fn unterminated_frontmatter_falls_through() {
        // No closing `---` — we treat the whole file as body and parse it.
        let src = "---\ntitle: hello\n\n# Heading\n\nbody\n";
        let chunks = chunk_markdown(src);
        // pulldown-cmark sees `---` followed by `title:` etc. as a setext-ish
        // structure; we don't assert the exact chunk shape, only that we don't
        // panic and produce *something*.
        assert!(!chunks.is_empty());
    }

    #[test]
    fn long_section_splits_at_soft_cap() {
        let mut src = String::from("# Big\n\n");
        // 10 paragraphs of ~200 chars each → 2000+ chars total.
        for i in 0..10 {
            src.push_str(&format!("paragraph {i} ").repeat(20));
            src.push_str("\n\n");
        }
        let chunks = chunk_markdown(&src);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks within one heading, got {}",
            chunks.len()
        );
        // Every chunk under "Big" carries the heading forward.
        for c in &chunks {
            assert_eq!(c.heading_path.as_deref(), Some("Big"));
        }
    }

    #[test]
    fn code_block_is_never_split() {
        let mut src = String::from("# Code\n\n");
        // Pad to push past the soft cap.
        src.push_str(&"prelude. ".repeat(150));
        src.push_str("\n\n```\n");
        // A 2000-char code block, single fenced section.
        src.push_str(&"x".repeat(2000));
        src.push_str("\n```\n\nepilogue.\n");
        let chunks = chunk_markdown(&src);
        // Find the chunk that contains the code fence.
        let code_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.text.contains("```"))
            .collect();
        assert_eq!(code_chunks.len(), 1, "code block must live in exactly one chunk");
        // The full 2000 x's must be present in that one chunk.
        assert!(code_chunks[0].text.contains(&"x".repeat(2000)));
    }

    #[test]
    fn chunk_indexes_are_contiguous_and_zero_based() {
        let src = "# A\n\nbody\n\n# B\n\nbody\n\n# C\n\nbody\n";
        let chunks = chunk_markdown(src);
        let indexes: Vec<_> = chunks.iter().map(|c| c.index).collect();
        assert_eq!(indexes, vec![0, 1, 2]);
    }

    #[test]
    fn byte_offsets_point_into_original_source() {
        let src = "---\nx: 1\n---\n\n# Hi\n\nbody.\n";
        let chunks = chunk_markdown(src);
        // The first chunk's byte range, sliced out of the original src,
        // should at minimum overlap with the heading text.
        let c = &chunks[0];
        let slice = &src[c.byte_start..c.byte_end.min(src.len())];
        assert!(slice.contains("Hi") || slice.contains("# Hi"));
    }
}
