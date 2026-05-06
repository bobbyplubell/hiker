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
pub struct TxtChunker;

impl Chunker for TxtChunker {
    fn chunk(&self, source: &str) -> Vec<Chunk> {
        chunk_txt(source)
    }
}

/// Split a `.txt` source into chunks. See module docs for the pipeline.
// status: txt-chunker-sentence-pack
// status: txt-chunker-structure-heuristics
// status: txt-chunker-guardrails
pub fn chunk_txt(source: &str) -> Vec<Chunk> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let lines = split_lines(source);
    let code_mask = detect_code_regions(&lines, source);
    let headings = detect_headings(&lines, source, &code_mask);
    let sections = build_sections(&lines, &headings, source);

    let mut chunks = Vec::new();
    let mut next_index: u32 = 0;
    for sec in &sections {
        emit_section(&mut chunks, &mut next_index, source, sec, &code_mask, &lines);
    }
    chunks
}

#[derive(Debug, Clone, Copy)]
struct LineSpec {
    /// Range of the line content, EXCLUDING the trailing newline.
    start: usize,
    end_no_nl: usize,
    /// Range INCLUDING trailing newline (or end-of-file).
    end_with_nl: usize,
}

fn split_lines(source: &str) -> Vec<LineSpec> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let after = if end < bytes.len() { end + 1 } else { end };
        out.push(LineSpec {
            start,
            end_no_nl: end,
            end_with_nl: after,
        });
        i = after;
    }
    out
}

fn line_text<'a>(source: &'a str, l: &LineSpec) -> &'a str {
    &source[l.start..l.end_no_nl]
}

fn is_blank_line(source: &str, l: &LineSpec) -> bool {
    line_text(source, l).trim().is_empty()
}

/// Mark lines that are part of a code-shaped region (3+ consecutive lines
/// satisfying either pattern (a) or (b) in docs/txt-ingest.md). Blank lines
/// don't break a code run on their own — but they don't extend it either:
/// detection requires consecutive non-blank lines.
fn detect_code_regions(lines: &[LineSpec], source: &str) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if is_blank_line(source, &lines[i]) {
            i += 1;
            continue;
        }
        // Count consecutive non-blank lines from i.
        let mut j = i;
        while j < n && !is_blank_line(source, &lines[j]) {
            j += 1;
        }
        let run = &lines[i..j];
        let indented = run
            .iter()
            .all(|l| line_starts_with_indent(line_text(source, l)));
        let symbol_heavy = run
            .iter()
            .all(|l| symbol_count(line_text(source, l)) >= 3);
        if run.len() >= 3 && (indented || symbol_heavy) {
            for k in i..j {
                mask[k] = true;
            }
        }
        i = j;
    }
    mask
}

fn line_starts_with_indent(text: &str) -> bool {
    if text.starts_with('\t') {
        return true;
    }
    let leading_spaces = text.bytes().take_while(|&b| b == b' ').count();
    leading_spaces >= 4
}

fn symbol_count(text: &str) -> usize {
    text.bytes()
        .filter(|&b| matches!(b, b';' | b'{' | b'}' | b'(' | b')' | b'='))
        .count()
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

/// Pass over the lines once, marking those that should be promoted to virtual
/// headings. Code-region lines are skipped (per the spec's code-region
/// exclusion guardrail), and ALL-CAPS promotions are throttled to at most
/// one per rolling 5-line window.
fn detect_headings(
    lines: &[LineSpec],
    source: &str,
    code_mask: &[bool],
) -> Vec<HeadingMark> {
    let mut out = Vec::new();
    let n = lines.len();
    // Last index at which we promoted an ALL-CAPS heading; used to enforce
    // the rolling-window guardrail.
    let mut last_caps_promotion: Option<usize> = None;
    let mut i = 0;
    while i < n {
        if code_mask[i] {
            i += 1;
            continue;
        }
        let text = line_text(source, &lines[i]);

        // Setext: this line is non-empty, next line is all `=` (H1) or `-` (H2).
        if i + 1 < n && !is_blank_line(source, &lines[i]) && !code_mask[i + 1] {
            let next_text = line_text(source, &lines[i + 1]).trim();
            if is_setext_underline(next_text, '=') {
                out.push(HeadingMark {
                    line: i,
                    level: 1,
                    title: text.trim().to_string(),
                    span: 2,
                });
                i += 2;
                continue;
            }
            if is_setext_underline(next_text, '-') {
                out.push(HeadingMark {
                    line: i,
                    level: 2,
                    title: text.trim().to_string(),
                    span: 2,
                });
                i += 2;
                continue;
            }
        }

        // ALL-CAPS heading.
        if looks_like_all_caps_heading(text) {
            let allowed = match last_caps_promotion {
                Some(prev) => i.saturating_sub(prev) >= 5,
                None => true,
            };
            if allowed {
                out.push(HeadingMark {
                    line: i,
                    level: 2,
                    title: text.trim().to_string(),
                    span: 1,
                });
                last_caps_promotion = Some(i);
                i += 1;
                continue;
            }
        }

        i += 1;
    }
    out
}

fn is_setext_underline(text: &str, ch: char) -> bool {
    if text.len() < 3 {
        return false;
    }
    text.chars().all(|c| c == ch)
}

fn looks_like_all_caps_heading(text: &str) -> bool {
    let trimmed = text.trim();
    let len = trimmed.chars().count();
    if !(3..=60).contains(&len) {
        return false;
    }
    // Must contain at least one letter, and every letter must be uppercase.
    let mut has_letter = false;
    for c in trimmed.chars() {
        if c.is_alphabetic() {
            has_letter = true;
            if !c.is_uppercase() {
                return false;
            }
        }
    }
    if !has_letter {
        return false;
    }
    // More than one distinct non-space character (rejects `==========`).
    let mut distinct = std::collections::HashSet::new();
    for c in trimmed.chars().filter(|c| !c.is_whitespace()) {
        distinct.insert(c);
    }
    if distinct.len() < 2 {
        return false;
    }
    // Fewer than ~10 words.
    if trimmed.split_whitespace().count() > 10 {
        return false;
    }
    true
}

#[derive(Debug, Clone)]
struct Section {
    heading_path: Option<String>,
    /// Inclusive line index of section body start.
    body_start_line: usize,
    /// Exclusive line index of section body end.
    body_end_line: usize,
}

fn build_sections(
    lines: &[LineSpec],
    headings: &[HeadingMark],
    _source: &str,
) -> Vec<Section> {
    let n = lines.len();
    if headings.is_empty() {
        return vec![Section {
            heading_path: None,
            body_start_line: 0,
            body_end_line: n,
        }];
    }

    let mut sections = Vec::new();
    let mut heading_stack: Vec<String> = Vec::new();

    // Pre-section content (before the first heading).
    if headings[0].line > 0 {
        sections.push(Section {
            heading_path: None,
            body_start_line: 0,
            body_end_line: headings[0].line,
        });
    }

    for (idx, h) in headings.iter().enumerate() {
        // Update heading_stack to reflect this heading's level.
        let depth = h.level as usize;
        heading_stack.truncate(depth.saturating_sub(1));
        heading_stack.push(h.title.clone());
        let breadcrumb = heading_stack.join(" > ");

        let body_start = h.line + h.span;
        let body_end = if idx + 1 < headings.len() {
            headings[idx + 1].line
        } else {
            n
        };
        sections.push(Section {
            heading_path: Some(breadcrumb),
            body_start_line: body_start,
            body_end_line: body_end,
        });
    }

    sections
}

/// Walk the section's body, emitting sentence-packed prose chunks and
/// keeping any code-shaped runs whole.
fn emit_section(
    chunks: &mut Vec<Chunk>,
    next_index: &mut u32,
    source: &str,
    sec: &Section,
    code_mask: &[bool],
    lines: &[LineSpec],
) {
    if sec.body_start_line >= sec.body_end_line {
        return;
    }
    let mut i = sec.body_start_line;
    while i < sec.body_end_line {
        if code_mask[i] {
            // Group consecutive code lines into one chunk (kept whole).
            let start_line = i;
            let mut j = i + 1;
            while j < sec.body_end_line && code_mask[j] {
                j += 1;
            }
            let start_byte = lines[start_line].start;
            let end_byte = lines[j - 1].end_with_nl;
            push_chunk(chunks, next_index, source, start_byte, end_byte, sec.heading_path.clone());
            i = j;
        } else {
            // Group consecutive prose (non-code) lines into one packing run.
            let start_line = i;
            let mut j = i + 1;
            while j < sec.body_end_line && !code_mask[j] {
                j += 1;
            }
            let start_byte = lines[start_line].start;
            let end_byte = lines[j - 1].end_with_nl;
            sentence_pack_range(
                chunks,
                next_index,
                source,
                start_byte,
                end_byte,
                sec.heading_path.clone(),
            );
            i = j;
        }
    }
}

fn push_chunk(
    chunks: &mut Vec<Chunk>,
    next_index: &mut u32,
    source: &str,
    start: usize,
    end: usize,
    heading_path: Option<String>,
) {
    let text = source[start..end].trim().to_string();
    if text.is_empty() {
        return;
    }
    chunks.push(Chunk {
        index: *next_index,
        byte_start: start,
        byte_end: end,
        text,
        heading_path,
    });
    *next_index += 1;
}

fn sentence_pack_range(
    chunks: &mut Vec<Chunk>,
    next_index: &mut u32,
    source: &str,
    range_start: usize,
    range_end: usize,
    heading_path: Option<String>,
) {
    let slice = &source[range_start..range_end];
    if slice.trim().is_empty() {
        return;
    }
    let units = segment_units(slice);
    let mut cur_start: Option<usize> = None;
    let mut cur_end: usize = 0;
    for &(s, e) in &units {
        let abs_s = range_start + s;
        let abs_e = range_start + e;
        if let Some(cs) = cur_start {
            let prospective = abs_e - cs;
            if prospective > SOFT_SIZE_LIMIT {
                push_chunk(chunks, next_index, source, cs, cur_end, heading_path.clone());
                cur_start = None;
            }
        }
        if cur_start.is_none() {
            cur_start = Some(abs_s);
        }
        cur_end = abs_e;
    }
    if let Some(cs) = cur_start {
        push_chunk(chunks, next_index, source, cs, cur_end, heading_path);
    }
}

fn segment_units(source: &str) -> Vec<(usize, usize)> {
    if has_sentence_terminator(source) {
        segment_sentences(source)
    } else {
        segment_lines(source)
    }
}

fn has_sentence_terminator(source: &str) -> bool {
    source.bytes().any(|b| matches!(b, b'.' | b'?' | b'!'))
}

/// Walk `source` finding sentence boundaries per the docs/txt-ingest.md rule:
/// `.`, `?`, `!` followed by whitespace and a capital letter (or end-of-input).
/// Skips boundaries whose preceding word is in the abbreviation allowlist.
fn segment_sentences(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i];
        if matches!(c, b'.' | b'?' | b'!') {
            let term_end = i + 1;
            let mut j = term_end;
            // Allow consecutive terminators ("!!!") to count as one boundary.
            while j < bytes.len() && matches!(bytes[j], b'.' | b'?' | b'!') {
                j += 1;
            }
            let after_terms = j;
            // Whitespace after the terminator(s)?
            let mut k = after_terms;
            while k < bytes.len() && matches!(bytes[k], b' ' | b'\t' | b'\n' | b'\r') {
                k += 1;
            }
            let had_whitespace = k > after_terms;
            let is_eof = k >= bytes.len();
            let next_is_capital = !is_eof && bytes[k].is_ascii_uppercase();
            let is_terminator = is_eof || (had_whitespace && next_is_capital);

            if is_terminator && c == b'.' && is_abbreviation_ending_at(source, i) {
                i = term_end;
                continue;
            }
            // Numbered-list prefix: `^\s*\d+\.` at line start is not a
            // sentence terminator, even though the next token is usually
            // capitalized ("1. Buy milk. 2. Bake bread."). Without this,
            // the period-space-capital rule mid-splits list items.
            if is_terminator && c == b'.' && is_numbered_list_prefix(source, i) {
                i = term_end;
                continue;
            }
            if is_terminator {
                if after_terms > start {
                    out.push((start, after_terms));
                }
                start = k;
                i = k;
                continue;
            }
            i = term_end;
            continue;
        }
        i += 1;
    }
    if start < bytes.len() {
        out.push((start, bytes.len()));
    }
    out
}

/// True when the period at `period_idx` closes a numbered-list prefix —
/// i.e. the run from the start of the current line up to (but not including)
/// this period is one or more ASCII digits, with optional leading whitespace.
fn is_numbered_list_prefix(source: &str, period_idx: usize) -> bool {
    let bytes = source.as_bytes();
    let mut line_start = period_idx;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut i = line_start;
    while i < period_idx && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    let digits_start = i;
    while i < period_idx && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i == period_idx && i > digits_start
}

// status: txt-abbreviation-allowlist
fn is_abbreviation_ending_at(source: &str, period_idx: usize) -> bool {
    let bytes = source.as_bytes();
    let mut start = period_idx;
    while start > 0 {
        let prev = bytes[start - 1];
        if matches!(prev, b' ' | b'\t' | b'\n' | b'\r') {
            break;
        }
        start -= 1;
    }
    let word = &source[start..=period_idx];
    abbreviations::ALL
        .iter()
        .any(|a| word.eq_ignore_ascii_case(a))
}

fn segment_lines(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let line_start = i;
        let mut line_end = line_start;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        let after = if line_end < bytes.len() { line_end + 1 } else { line_end };
        if !source[line_start..line_end].trim().is_empty() {
            out.push((line_start, after));
        }
        i = after;
    }
    out
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
        assert!(chunk_txt("").is_empty());
    }

    #[test]
    fn whitespace_only_produces_no_chunks() {
        assert!(chunk_txt("   \n\n\t\n").is_empty());
    }

    #[test]
    fn single_short_file_one_chunk() {
        let chunks = chunk_txt("Hello world. This is a note.\n");
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
        let chunks = chunk_txt(&src);
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
        let src = "Mr. Smith arrived early. He was happy.";
        let units = segment_sentences(src);
        assert_eq!(units.len(), 2, "got units {units:?}");
        assert!(src[units[0].0..units[0].1].contains("Mr. Smith arrived early."));
    }

    #[test]
    fn period_inside_word_is_not_a_break() {
        let src = "Visit foo.bar today. The next sentence.";
        let units = segment_sentences(src);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn no_terminator_falls_back_to_line_packing() {
        let src = "let x  one\nlet y  two\nlet z  three\n";
        let chunks = chunk_txt(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("let x"));
    }

    #[test]
    fn numbered_list_prefix_does_not_split_inside_an_item() {
        // The period after `1` is followed by space + capital `B`, which
        // would normally fire the sentence-terminator rule. The numbered-
        // list prefix carve-out suppresses that so list items don't split
        // mid-line. (Items themselves can still bunch into one unit when
        // the following item starts with a digit, not a capital — that's
        // fine for chunking; what matters is no mid-item breaks.)
        let src = "1. Buy milk and butter.\n2. Bake bread.\n";
        let units = segment_sentences(src);
        for &(s, e) in &units {
            let unit_text = &src[s..e];
            // No unit should start with the bare list marker on its own.
            assert!(
                !unit_text.trim_start().starts_with("Buy milk"),
                "list item split mid-line: {unit_text:?}"
            );
        }
    }

    #[test]
    fn numbered_list_followed_by_capital_paragraph_breaks_correctly() {
        // After the last item, a real sentence beginning with a capital
        // should still be a sentence break (the carve-out only applies to
        // the period inside the list prefix itself).
        let src = "1. Buy milk.\n\nThe next paragraph follows.";
        let units = segment_sentences(src);
        assert_eq!(units.len(), 2, "got {units:?}");
    }

    #[test]
    fn lowercase_after_period_does_not_break() {
        let src = "First half. but lowercase continues.";
        let units = segment_sentences(src);
        assert_eq!(units.len(), 1);
    }

    #[test]
    fn all_caps_line_promoted_to_heading() {
        let src = "INTRODUCTION\n\nfirst sentence here. second sentence.\n";
        let chunks = chunk_txt(src);
        // Pre-section content is empty (heading is at line 0); one section
        // under "INTRODUCTION" with a single chunk.
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading_path.as_deref(), Some("INTRODUCTION"));
    }

    #[test]
    fn setext_underlines_create_h1_and_h2() {
        let src = "Big Title\n=========\n\nintro.\n\nSubtitle\n--------\n\nbody.\n";
        let chunks = chunk_txt(src);
        let paths: Vec<_> = chunks.iter().filter_map(|c| c.heading_path.as_deref()).collect();
        assert!(paths.contains(&"Big Title"), "got paths: {paths:?}");
        assert!(paths.contains(&"Big Title > Subtitle"), "got paths: {paths:?}");
    }

    #[test]
    fn equals_run_alone_is_not_a_setext_without_title() {
        // A line of `=` with nothing above it shouldn't promote anything.
        let src = "=========\n\nbody.\n";
        let chunks = chunk_txt(src);
        for c in &chunks {
            assert!(c.heading_path.is_none());
        }
    }

    #[test]
    fn caps_promotion_throttled_within_5_line_window() {
        // Five consecutive ALL-CAPS lines should promote at most one heading.
        let src = "FIRST CAPS\nSECOND CAPS\nTHIRD CAPS\nFOURTH CAPS\nFIFTH CAPS\n\nbody.\n";
        let chunks = chunk_txt(src);
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
        let chunks = chunk_txt(src);
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
        let chunks = chunk_txt(src);
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
        let chunks = chunk_txt(src);
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
        let chunks = chunk_txt(&src);
        let idx: Vec<_> = chunks.iter().map(|c| c.index).collect();
        let expected: Vec<u32> = (0..chunks.len() as u32).collect();
        assert_eq!(idx, expected);
    }
}
