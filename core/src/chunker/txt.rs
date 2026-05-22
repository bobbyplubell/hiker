//! Plain-text chunker. See docs/txt-ingest.md.
//!
//! Pipeline (applied in order):
//! - Layer 1: split on blank-line runs (paragraphs) — implicit baseline.
//! - Layer 2: heuristic structure detection (virtual headings via ALL-CAPS or
//!   setext underlines, plus code-region exclusion).
//! - Layer 3: sentence-aware packing within sections to a ~1200-char soft
//!   cap, with an abbreviation allowlist so `Mr. Smith` stays one sentence.
//!
//! Three guardrails keep Layer 2 from going feral: max one ALL-CAPS heading
//! promotion per rolling 5-line window, the period+space sentence rule from
//! Layer 3, and code-region exclusion.

use super::{Chunk, Chunker};

/// Same soft cap as the markdown chunker (see `chunker::markdown`); kept in
/// sync intentionally so `.md` and `.txt` produce comparable chunk sizes.
const SOFT_SIZE_LIMIT: usize = 1200;

/// Stateless plain-text chunker. Implements [`Chunker`] so the ingest pipeline
/// can dispatch by extension.
// status: txt-chunker-paragraph-splits
pub struct Txt;

impl Chunker for Txt {
    fn chunk(&self, source: &str) -> Vec<Chunk> {
        chunk(source)
    }
}

#[derive(Debug, Clone, Copy)]
struct LineSpec {
    /// Range of the line content, EXCLUDING the trailing newline.
    start: usize,
    end_no_nl: usize,
    /// Range INCLUDING trailing newline (or end-of-file).
    end_with_nl: usize,
}

#[derive(Debug, Clone)]
struct HeadingMark {
    /// Line index of the heading text.
    line: usize,
    /// 1 = H1 (setext `===`), 2 = H2 (ALL-CAPS, setext `---`).
    level: u8,
    title: String,
    /// Number of lines this heading consumes (1 for ALL-CAPS, 2 for setext —
    /// the title line plus the underline).
    span: usize,
}

#[derive(Debug, Clone)]
struct Section {
    heading_path: Option<String>,
    /// Inclusive line index of section body start.
    body_start_line: usize,
    /// Exclusive line index of section body end.
    body_end_line: usize,
}

/// Per-call chunker state. Holds the source and lazily-built tables; methods
/// drive the pipeline stages. Wrapping the work in `self`-methods keeps each
/// stage focused without splitting the file into single-call free helpers.
struct ChunkerCtx<'a> {
    source: &'a str,
    lines: Vec<LineSpec>,
    code_mask: Vec<bool>,
    headings: Vec<HeadingMark>,
    sections: Vec<Section>,
    chunks: Vec<Chunk>,
    next_index: u32,
}

/// Split a `.txt` source into chunks. See module docs for the pipeline.
// status: txt-chunker-sentence-pack
// status: txt-chunker-structure-heuristics
// status: txt-chunker-guardrails
pub fn chunk(source: &str) -> Vec<Chunk> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let mut ctx = ChunkerCtx {
        source,
        lines: Vec::new(),
        code_mask: Vec::new(),
        headings: Vec::new(),
        sections: Vec::new(),
        chunks: Vec::new(),
        next_index: 0,
    };
    ctx.split_lines();
    ctx.detect_code_regions();
    ctx.detect_headings();
    ctx.build_sections();
    ctx.emit_all_sections();
    ctx.chunks
}

impl<'a> ChunkerCtx<'a> {
    fn line_text(&self, l: &LineSpec) -> &'a str {
        &self.source[l.start..l.end_no_nl]
    }

    fn is_blank_line(&self, l: &LineSpec) -> bool {
        self.line_text(l).trim().is_empty()
    }

    fn split_lines(&mut self) {
        let bytes = self.source.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let start = i;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'\n' {
                end += 1;
            }
            let after = if end < bytes.len() { end + 1 } else { end };
            self.lines.push(LineSpec {
                start,
                end_no_nl: end,
                end_with_nl: after,
            });
            i = after;
        }
    }

    fn detect_code_regions(&mut self) {
        let n = self.lines.len();
        self.code_mask = vec![false; n];
        let mut i = 0;
        while i < n {
            if self.is_blank_line(&self.lines[i]) {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < n && !self.is_blank_line(&self.lines[j]) {
                j += 1;
            }
            let run = &self.lines[i..j];
            let indented = run.iter().all(|l| {
                let text = self.line_text(l);
                text.starts_with('\t')
                    || text.bytes().take_while(|&b| b == b' ').count() >= 4
            });
            let symbol_heavy = run.iter().all(|l| {
                self.line_text(l)
                    .bytes()
                    .filter(|&b| matches!(b, b';' | b'{' | b'}' | b'(' | b')' | b'='))
                    .count()
                    >= 3
            });
            if run.len() >= 3 && (indented || symbol_heavy) {
                for slot in self.code_mask.iter_mut().take(j).skip(i) {
                    *slot = true;
                }
            }
            i = j;
        }
    }

    fn is_setext_underline(&self, text: &str, ch: char) -> bool {
        if text.len() < 3 {
            return false;
        }
        text.chars().all(|c| c == ch)
    }

    fn try_setext_heading(&mut self, i: usize) -> Option<usize> {
        let n = self.lines.len();
        if i + 1 >= n || self.is_blank_line(&self.lines[i]) || self.code_mask[i + 1] {
            return None;
        }
        let text = self.line_text(&self.lines[i]);
        let next_text = self.line_text(&self.lines[i + 1]).trim();
        let level = if self.is_setext_underline(next_text, '=') {
            1
        } else if self.is_setext_underline(next_text, '-') {
            2
        } else {
            return None;
        };
        self.headings.push(HeadingMark {
            line: i,
            level,
            title: text.trim().to_string(),
            span: 2,
        });
        Some(i + 2)
    }

    fn looks_like_all_caps_heading(&self, text: &str) -> bool {
        let trimmed = text.trim();
        let len = trimmed.chars().count();
        if !(3..=60).contains(&len) {
            return false;
        }
        let mut has_letter = false;
        let mut all_upper = true;
        for c in trimmed.chars() {
            if c.is_alphabetic() {
                has_letter = true;
                if !c.is_uppercase() {
                    all_upper = false;
                    break;
                }
            }
        }
        let mut distinct = std::collections::HashSet::new();
        for c in trimmed.chars().filter(|c| !c.is_whitespace()) {
            distinct.insert(c);
        }
        let few_words = trimmed.split_whitespace().count() <= 10;
        has_letter && all_upper && distinct.len() >= 2 && few_words
    }

    fn try_caps_heading(&mut self, i: usize, last_caps_promotion: &mut Option<usize>) -> bool {
        let text = self.line_text(&self.lines[i]);
        if !self.looks_like_all_caps_heading(text) {
            return false;
        }
        let allowed = match *last_caps_promotion {
            Some(prev) => i.saturating_sub(prev) >= 5,
            None => true,
        };
        if !allowed {
            return false;
        }
        self.headings.push(HeadingMark {
            line: i,
            level: 2,
            title: text.trim().to_string(),
            span: 1,
        });
        *last_caps_promotion = Some(i);
        true
    }

    fn detect_headings(&mut self) {
        let n = self.lines.len();
        let mut last_caps_promotion: Option<usize> = None;
        let mut i = 0;
        while i < n {
            if self.code_mask[i] {
                i += 1;
                continue;
            }
            if let Some(next) = self.try_setext_heading(i) {
                i = next;
                continue;
            }
            if self.try_caps_heading(i, &mut last_caps_promotion) {
                i += 1;
                continue;
            }
            i += 1;
        }
    }

    fn build_sections(&mut self) {
        let n = self.lines.len();
        if self.headings.is_empty() {
            self.sections.push(Section {
                heading_path: None,
                body_start_line: 0,
                body_end_line: n,
            });
            return;
        }
        let mut heading_stack: Vec<String> = Vec::new();
        if self.headings[0].line > 0 {
            self.sections.push(Section {
                heading_path: None,
                body_start_line: 0,
                body_end_line: self.headings[0].line,
            });
        }
        for idx in 0..self.headings.len() {
            let h = &self.headings[idx];
            let depth = h.level as usize;
            heading_stack.truncate(depth.saturating_sub(1));
            heading_stack.push(h.title.clone());
            let breadcrumb = heading_stack.join(" > ");
            let body_start = h.line + h.span;
            let body_end = if idx + 1 < self.headings.len() {
                self.headings[idx + 1].line
            } else {
                n
            };
            self.sections.push(Section {
                heading_path: Some(breadcrumb),
                body_start_line: body_start,
                body_end_line: body_end,
            });
        }
    }

    fn emit_all_sections(&mut self) {
        let sections = std::mem::take(&mut self.sections);
        for sec in &sections {
            self.emit_section(sec);
        }
    }

    fn emit_section(&mut self, sec: &Section) {
        if sec.body_start_line >= sec.body_end_line {
            return;
        }
        let mut i = sec.body_start_line;
        while i < sec.body_end_line {
            if self.code_mask[i] {
                let start_line = i;
                let mut j = i + 1;
                while j < sec.body_end_line && self.code_mask[j] {
                    j += 1;
                }
                let start_byte = self.lines[start_line].start;
                let end_byte = self.lines[j - 1].end_with_nl;
                self.push_chunk(start_byte, end_byte, sec.heading_path.clone());
                i = j;
            } else {
                let start_line = i;
                let mut j = i + 1;
                while j < sec.body_end_line && !self.code_mask[j] {
                    j += 1;
                }
                let start_byte = self.lines[start_line].start;
                let end_byte = self.lines[j - 1].end_with_nl;
                self.sentence_pack_range(start_byte, end_byte, sec.heading_path.clone());
                i = j;
            }
        }
    }

    fn sentence_pack_range(
        &mut self,
        range_start: usize,
        range_end: usize,
        heading_path: Option<String>,
    ) {
        let slice = &self.source[range_start..range_end];
        if slice.trim().is_empty() {
            return;
        }
        let units = self.segment_units(slice);
        let mut cur_start: Option<usize> = None;
        let mut cur_end: usize = 0;
        for (s, e) in units {
            let abs_s = range_start + s;
            let abs_e = range_start + e;
            if let Some(cs) = cur_start {
                let prospective = abs_e - cs;
                if prospective > SOFT_SIZE_LIMIT {
                    self.push_chunk(cs, cur_end, heading_path.clone());
                    cur_start = None;
                }
            }
            if cur_start.is_none() {
                cur_start = Some(abs_s);
            }
            cur_end = abs_e;
        }
        if let Some(cs) = cur_start {
            self.push_chunk(cs, cur_end, heading_path);
        }
    }

    /// Segment a slice into sentence-or-line units. If any sentence terminator
    /// (`.`, `?`, `!`) is present, walk sentences per docs/txt-ingest.md (with
    /// the abbreviation allowlist and numbered-list carve-out); otherwise fall
    /// back to packing non-blank lines.
    fn segment_units(&self, slice: &str) -> Vec<(usize, usize)> {
        let has_term = slice.bytes().any(|b| matches!(b, b'.' | b'?' | b'!'));
        if has_term {
            self.segment_sentences(slice)
        } else {
            self.segment_lines(slice)
        }
    }

    fn segment_sentences(&self, slice: &str) -> Vec<(usize, usize)> {
        let bytes = slice.as_bytes();
        let mut out = Vec::new();
        let mut seg_start = 0usize;
        let mut p = 0usize;
        while p < bytes.len() {
            let c = bytes[p];
            if !matches!(c, b'.' | b'?' | b'!') {
                p += 1;
                continue;
            }
            let term_end = p + 1;
            let mut q = term_end;
            while q < bytes.len() && matches!(bytes[q], b'.' | b'?' | b'!') {
                q += 1;
            }
            let after_terms = q;
            let mut k = after_terms;
            while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
                k += 1;
            }
            let had_ws = k > after_terms;
            let is_eof = k >= bytes.len();
            let next_cap = !is_eof && bytes[k].is_ascii_uppercase();
            let is_terminator = is_eof || (had_ws && next_cap);

            if is_terminator
                && c == b'.'
                && (self.is_abbrev_at(slice, bytes, p) || self.is_list_prefix_at(bytes, p))
            {
                p = term_end;
                continue;
            }
            if is_terminator {
                if after_terms > seg_start {
                    out.push((seg_start, after_terms));
                }
                seg_start = k;
                p = k;
                continue;
            }
            p = term_end;
        }
        if seg_start < bytes.len() {
            out.push((seg_start, bytes.len()));
        }
        out
    }

    // status: txt-abbreviation-allowlist
    fn is_abbrev_at(&self, slice: &str, bytes: &[u8], p: usize) -> bool {
        let mut wstart = p;
        while wstart > 0 {
            let prev = bytes[wstart - 1];
            if matches!(prev, b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            wstart -= 1;
        }
        let word = &slice[wstart..=p];
        abbreviations::ALL.iter().any(|a| word.eq_ignore_ascii_case(a))
    }

    /// Numbered-list prefix carve-out: `^\s*\d+\.` at line start is not a
    /// sentence terminator (else list items split mid-line).
    fn is_list_prefix_at(&self, bytes: &[u8], p: usize) -> bool {
        let mut line_start = p;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        let mut r = line_start;
        while r < p && matches!(bytes[r], b' ' | b'\t') {
            r += 1;
        }
        let digits_start = r;
        while r < p && bytes[r].is_ascii_digit() {
            r += 1;
        }
        r == p && r > digits_start
    }

    fn segment_lines(&self, slice: &str) -> Vec<(usize, usize)> {
        let bytes = slice.as_bytes();
        let mut out = Vec::new();
        let mut p = 0usize;
        while p < bytes.len() {
            let line_start = p;
            let mut line_end = line_start;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }
            let after = if line_end < bytes.len() {
                line_end + 1
            } else {
                line_end
            };
            if !slice[line_start..line_end].trim().is_empty() {
                out.push((line_start, after));
            }
            p = after;
        }
        out
    }

    fn push_chunk(&mut self, start: usize, end: usize, heading_path: Option<String>) {
        let text = self.source[start..end].trim().to_string();
        if text.is_empty() {
            return;
        }
        self.chunks.push(Chunk {
            index: self.next_index,
            byte_start: start,
            byte_end: end,
            text,
            heading_path,
        });
        self.next_index += 1;
    }
}

mod abbreviations {
    /// Small allowlist of abbreviations that end with a period but never
    /// terminate a sentence. Kept short and conservative — the bigger this
    /// list grows, the more false-negatives we accumulate (real sentence
    /// breaks treated as abbreviations). Add only when real content shows the
    /// segmenter splitting wrong.
    pub const ALL: &[&str] = &[
        "Mr.", "Mrs.", "Ms.", "Dr.", "Prof.", "Sr.", "Jr.", "St.",
        "e.g.", "i.e.", "etc.", "vs.", "cf.", "approx.",
        "a.m.", "p.m.", "A.M.", "P.M.",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_produces_no_chunks() {
        assert!(chunk("").is_empty());
    }

    #[test]
    fn whitespace_only_produces_no_chunks() {
        assert!(chunk("   \n\n\t\n").is_empty());
    }

    #[test]
    fn single_short_file_one_chunk() {
        let chunks = chunk("Hello world. This is a note.\n");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("Hello world"));
        assert!(chunks[0].heading_path.is_none());
    }

    #[test]
    fn long_prose_packs_to_multiple_chunks() {
        let mut src = String::new();
        for n in 0..200 {
            src.push_str(&format!("Sentence number {n}. "));
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected multiple chunks, got {}", chunks.len());
        for c in &chunks {
            assert!(
                c.byte_end - c.byte_start <= SOFT_SIZE_LIMIT + 200,
                "chunk too big: {} bytes",
                c.byte_end - c.byte_start
            );
        }
    }

    #[test]
    fn abbreviation_does_not_terminate_sentence() {
        // Force a chunk split by exceeding SOFT_SIZE_LIMIT. The split must
        // never land between "Mr." and "Smith" — the allowlist keeps that
        // pair in the same sentence unit (and therefore the same chunk).
        let mut src = String::new();
        for _ in 0..80 {
            src.push_str("Mr. Smith arrived early and stayed late. ");
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected split, got {}", chunks.len());
        for c in &chunks {
            // No chunk should start with "Smith" — that would mean a split
            // landed between "Mr." and "Smith".
            assert!(
                !c.text.starts_with("Smith"),
                "split landed inside `Mr. Smith`: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn period_inside_word_is_not_a_break() {
        // Force a split; verify no chunk begins with "bar" (which would mean
        // a split landed inside `foo.bar`).
        let mut src = String::new();
        for _ in 0..80 {
            src.push_str("Visit foo.bar today and tomorrow as well. ");
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected split, got {}", chunks.len());
        for c in &chunks {
            assert!(
                !c.text.starts_with("bar"),
                "split landed inside `foo.bar`: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn no_terminator_falls_back_to_line_packing() {
        let src = "let x  one\nlet y  two\nlet z  three\n";
        let chunks = chunk(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("let x"));
    }

    #[test]
    fn numbered_list_prefix_does_not_split_inside_an_item() {
        // The period after `1` is followed by space + capital `B`, which
        // would normally fire the sentence-terminator rule. The numbered-
        // list prefix carve-out suppresses that so list items don't split
        // mid-line. Force a chunk split by repeating the list, then verify
        // no chunk starts with "Buy milk" (which would mean the split
        // landed inside item `1.`).
        let mut src = String::new();
        for _ in 0..60 {
            // Each block: a real capital-starting sentence followed by a
            // newline-anchored list. The carve-out must prevent a split
            // between the list marker (`1.`) and its content ("Buy milk…").
            src.push_str("And then rest came at last.\n1. Buy milk and butter for the week.\n2. Bake bread for tomorrow.\n");
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected split, got {}", chunks.len());
        for c in &chunks {
            assert!(
                !c.text.trim_start().starts_with("Buy milk"),
                "list item split mid-line: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn numbered_list_followed_by_capital_paragraph_breaks_correctly() {
        // After the last item, a real sentence beginning with a capital
        // should still be a sentence break (the carve-out only applies to
        // the period inside the list prefix itself). Force a chunk split
        // and verify a chunk starts at "The next paragraph" — proving the
        // break landed at that sentence boundary rather than mid-item.
        let mut src = String::new();
        for _ in 0..40 {
            src.push_str("1. Buy milk for the household.\n\nThe next paragraph follows here.\n\n");
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected split, got {}", chunks.len());
        let has_paragraph_start = chunks
            .iter()
            .any(|c| c.text.starts_with("The next paragraph"));
        assert!(
            has_paragraph_start,
            "expected a chunk starting at the paragraph break"
        );
    }

    #[test]
    fn lowercase_after_period_does_not_break() {
        // "First half. but lowercase…" — lowercase after the period must
        // NOT be a sentence break. Force a split and verify no chunk
        // begins with "but lowercase".
        let mut src = String::new();
        for _ in 0..80 {
            src.push_str("First half. but lowercase continues all the way through. ");
        }
        let chunks = chunk(&src);
        assert!(chunks.len() >= 2, "expected split, got {}", chunks.len());
        for c in &chunks {
            assert!(
                !c.text.starts_with("but lowercase"),
                "split landed at lowercase-after-period: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn all_caps_line_promoted_to_heading() {
        let src = "INTRODUCTION\n\nfirst sentence here. second sentence.\n";
        let chunks = chunk(src);
        // Pre-section content is empty (heading is at line 0); one section
        // under "INTRODUCTION" with a single chunk.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path.as_deref(), Some("INTRODUCTION"));
    }

    #[test]
    fn setext_underlines_create_h1_and_h2() {
        let src = "Big Title\n=========\n\nintro.\n\nSubtitle\n--------\n\nbody.\n";
        let chunks = chunk(src);
        let paths: Vec<_> = chunks.iter().filter_map(|c| c.heading_path.as_deref()).collect();
        assert!(paths.contains(&"Big Title"), "got paths: {paths:?}");
        assert!(paths.contains(&"Big Title > Subtitle"), "got paths: {paths:?}");
    }

    #[test]
    fn equals_run_alone_is_not_a_setext_without_title() {
        // A line of `=` with nothing above it shouldn't promote anything.
        let src = "=========\n\nbody.\n";
        let chunks = chunk(src);
        for c in &chunks {
            assert!(c.heading_path.is_none());
        }
    }

    #[test]
    fn caps_promotion_throttled_within_5_line_window() {
        // Five consecutive ALL-CAPS lines should promote at most one heading.
        let src = "FIRST CAPS\nSECOND CAPS\nTHIRD CAPS\nFOURTH CAPS\nFIFTH CAPS\n\nbody.\n";
        let chunks = chunk(src);
        let promoted: Vec<_> = chunks.iter().filter(|c| c.heading_path.is_some()).collect();
        // Only one heading should have been promoted; later CAPS lines fall
        // into the section as content.
        let unique_paths: std::collections::HashSet<_> =
            promoted.iter().filter_map(|c| c.heading_path.as_deref()).collect();
        assert_eq!(unique_paths.len(), 1);
        assert!(unique_paths.contains("FIRST CAPS"));
    }

    #[test]
    fn code_shaped_region_excluded_from_heading_promotion_and_kept_whole() {
        // Indented Python-ish snippet with an ALL-CAPS-looking line inside
        // (`SOMETHING` would normally promote). The 4-space indent + 3+ lines
        // makes it a code region.
        let src = "Intro paragraph.\n\n    if (x):\n    SOMETHING = y\n    return z\n\nOutro paragraph.\n";
        let chunks = chunk(src);
        // No heading_path should ever be set — SOMETHING is inside a code
        // region.
        for c in &chunks {
            assert!(
                c.heading_path.is_none(),
                "did not expect heading: {:?}",
                c.heading_path
            );
        }
        // Code region kept whole — there should be a chunk containing all 3
        // code lines.
        let code_chunk = chunks
            .iter()
            .find(|c| c.text.contains("SOMETHING"))
            .expect("code region chunk missing");
        assert!(code_chunk.text.contains("if (x):"));
        assert!(code_chunk.text.contains("return z"));
    }

    #[test]
    fn symbol_heavy_run_excluded_from_heading_promotion() {
        // 3+ lines with at least 3 of `;{}()=` each — counts as code-shaped
        // even without indentation.
        let src = "SOMETHING TODO\nfn foo() { return 1; }\nfn bar() { return 2; }\nfn baz() { return 3; }\n";
        let chunks = chunk(src);
        // The leading ALL-CAPS line is on a non-code line, so it *can*
        // promote. Verify it does.
        assert!(chunks
            .iter()
            .any(|c| c.heading_path.as_deref() == Some("SOMETHING TODO")));
        // The fn lines are code-shaped — they're under SOMETHING TODO but
        // emitted whole.
        let code_chunk = chunks
            .iter()
            .find(|c| c.text.contains("fn foo") && c.text.contains("fn baz"));
        assert!(code_chunk.is_some(), "expected code lines kept together");
    }

    #[test]
    fn byte_offsets_index_into_source() {
        let src = "Alpha sentence. Beta sentence.";
        let chunks = chunk(src);
        for c in &chunks {
            assert!(c.byte_end <= src.len());
        }
    }

    #[test]
    fn chunk_indexes_are_contiguous_and_zero_based() {
        let mut src = String::from("INTRO\n\n");
        for n in 0..150 {
            src.push_str(&format!("S{n} word word word word word. "));
        }
        let chunks = chunk(&src);
        let idx: Vec<_> = chunks.iter().map(|c| c.index).collect();
        let expected: Vec<u32> = (0..chunks.len() as u32).collect();
        assert_eq!(idx, expected);
    }
}
