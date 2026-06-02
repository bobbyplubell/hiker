//! PDF text extraction logic — the pure-Rust fast path and the scanned/garbage
//! heuristic behind [`crate::builtin::PdfExtractor`].
//!
//! v1 pulls the text layer out of a PDF with the `pdf-extract` crate (built on
//! `lopdf`) — no external `pdftotext` binary, no bundled poppler/pdfium C
//! dependency — so hiker keeps its single-binary / clean-SBOM posture, the same
//! reasoning that picks `wasmi` over a JIT in `plugins.md`
//! (`extract-pdf-fast-path`).
//!
//! When the fast path yields empty or garbage text — a scanned / image-only PDF
//! with no embedded text layer — [`run`] returns `Ok(None)` so the registry's
//! fallback chain (`extract-fallback-chain`) can hand the source to a
//! higher-fidelity extractor (the deferred marker/docling fallback, or a
//! user-wired [`crate::builtin::CommandExtractor`])
//! (`extract-pdf-scanned-detect`). With no fallback configured, the chain ends
//! in `Ok(None)` and the caller records a skip reason on the sidecar via the
//! Phase-2 per-file skipped-state mechanism in `index.md` — the same path a
//! non-UTF-8 plain-text file takes through the passthrough extractor.
//
// status: extract-pdf-fast-path
// status: extract-pdf-scanned-detect

use std::path::Path;

use crate::ExtractError;

/// Run the PDF fast path over the file at `path`: read the bytes, pull the
/// text layer, and return the cleaned markdown body — or `Ok(None)` when the
/// text layer is empty/garbage (scanned/image-only PDF), the scanned-detect
/// signal that advances the fallback chain. A PDF the parser can't open at all
/// (malformed/encrypted) is an `Err` — a hard failure for this extractor, not
/// a "try the next" signal.
pub(super) fn run(path: &Path) -> Result<Option<String>, ExtractError> {
    let bytes = std::fs::read(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let text = pdf_extract::extract_text_from_mem(&bytes)
        .map_err(|e| ExtractError::Extractor("pdf".into(), e.to_string()))?;
    if looks_scanned(&text) {
        return Ok(None);
    }
    Ok(Some(normalize(&text)))
}

/// Minimum count of word-forming characters below which a PDF's text layer is
/// treated as absent (scanned/image-only). A genuine text PDF clears this in
/// its first line; a scanned page yields zero or a few stray glyphs.
const MIN_WORD_CHARS: usize = 16;

/// Minimum ratio of "plausible text" characters (letters, digits, ASCII
/// punctuation) among the non-whitespace glyphs. A bad font/encoding decode
/// produces a stream of replacement chars / control bytes / private-use-area
/// garbage that falls below this; real prose sits near 1.0.
const MIN_PRINTABLE_RATIO: f64 = 0.6;

/// The scanned/garbage heuristic for `extract-pdf-scanned-detect`: decide
/// whether an extracted text layer is real prose or the empty/garbage output
/// of a scanned (image-only) or badly-encoded PDF.
///
/// Two independent gates, either of which condemns the text:
/// 1. **Too little text.** Fewer than [`MIN_WORD_CHARS`] alphanumeric
///    characters — an image-only page extracts to nothing (or a few stray
///    glyphs from a logo/watermark font), and there is no prose to index.
/// 2. **Too much garbage.** Among the non-whitespace characters, fewer than
///    [`MIN_PRINTABLE_RATIO`] are plausible text characters — a broken
///    font/CMap decode emits replacement chars and private-use codepoints, not
///    readable content.
///
/// Deliberately conservative: it only fires on clearly-unusable output, so a
/// real-but-sparse PDF (a title page, a mostly-figures slide) still extracts.
fn looks_scanned(text: &str) -> bool {
    let word_chars = text.chars().filter(|c| c.is_alphanumeric()).count();
    if word_chars < MIN_WORD_CHARS {
        return true;
    }

    let mut non_ws = 0usize;
    let mut plausible = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        non_ws += 1;
        if is_plausible_text_char(c) {
            plausible += 1;
        }
    }
    if non_ws == 0 {
        return true;
    }
    (plausible as f64 / non_ws as f64) < MIN_PRINTABLE_RATIO
}

/// Whether `c` plausibly belongs to readable extracted text: any Unicode letter
/// or number (covers non-Latin scripts), or ASCII punctuation/symbols.
/// Replacement chars, control codes, and private-use-area codepoints (the
/// signature of a broken decode) are not plausible.
fn is_plausible_text_char(c: char) -> bool {
    c.is_alphanumeric() || (c.is_ascii_graphic() && !c.is_alphanumeric())
}

/// Tidy a raw extracted text layer into a sidecar body: normalize line endings
/// and collapse the runs of blank lines `pdf-extract` emits between pages so
/// the markdown body reads cleanly. Content is otherwise preserved verbatim —
/// no reflow, no heuristics that could drop text.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0u32;
    for line in text.replace("\r\n", "\n").replace('\r', "\n").lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            // Collapse 2+ consecutive blank lines into a single paragraph break.
            if blank_run >= 2 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{looks_scanned, normalize};

    #[test]
    fn empty_text_looks_scanned() {
        assert!(looks_scanned(""));
        assert!(looks_scanned("   \n\n  \t "));
    }

    #[test]
    fn too_few_word_chars_looks_scanned() {
        // A scanned page might extract a stray glyph or two; below the floor.
        assert!(looks_scanned("a b c"));
        assert!(looks_scanned("\u{0c}\u{0c}")); // form feeds between blank pages
    }

    #[test]
    fn garbage_decode_looks_scanned() {
        // Enough chars to clear the word-count gate, but mostly replacement /
        // private-use codepoints from a broken font decode.
        let garbage = "\u{fffd}".repeat(40);
        let with_a_few_letters = format!("abcdefghijklmnop{garbage}");
        assert!(looks_scanned(&with_a_few_letters));
    }

    #[test]
    fn real_prose_is_not_scanned() {
        let prose = "The quick brown fox jumps over the lazy dog. \
                     Extraction produced a clean text layer with real words.";
        assert!(!looks_scanned(prose));
    }

    #[test]
    fn non_latin_prose_is_not_scanned() {
        // Cyrillic / CJK count as alphanumeric and plausible text.
        let text = "Это настоящий текст из PDF документа с реальными словами.";
        assert!(!looks_scanned(text));
    }

    #[test]
    fn normalize_collapses_blank_runs_and_trims() {
        let raw = "line one  \r\n\r\n\r\n\r\nline two\n\n\n";
        let out = normalize(raw);
        // CRLF normalized, blank runs collapsed to a single break, trailing
        // line whitespace trimmed; one terminating newline kept.
        assert_eq!(out, "line one\n\nline two\n");
    }
}
