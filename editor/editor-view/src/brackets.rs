//! Bracket matching decoration provider. See SPEC §9.10, IMPLEMENTATION §16.6.1.
//!
//! For each cursor in the selection, if the character immediately before or
//! after the cursor is a known bracket, scan up to `max_scan` characters in
//! the matching direction looking for a balanced partner. Emit a `Mark`
//! decoration on both brackets when matched, or on the unmatched bracket only
//! when no partner is found within the scan window.

use editor_core::{Color, Decoration, DecorationSet, EditorState, MarkStyle, RangeSet, Rope};

const MATCH_COLOR: Color = Color::rgba(100, 180, 240, 80);
const WARN_COLOR: Color = Color::rgba(240, 100, 100, 90);

/// A pair of bracket characters that should be matched against each other.
#[derive(Clone, Copy, Debug)]
pub struct BracketPair {
    pub open: char,
    pub close: char,
}

impl BracketPair {
    pub const fn new(open: char, close: char) -> Self {
        Self { open, close }
    }
}

/// Default bracket pairs: `()`, `[]`, `{}`.
pub const DEFAULT_BRACKETS: &[BracketPair] = &[
    BracketPair::new('(', ')'),
    BracketPair::new('[', ']'),
    BracketPair::new('{', '}'),
];

/// Direction we're scanning relative to the seed bracket.
#[derive(Clone, Copy)]
enum Dir {
    Forward,
    Backward,
}

/// Classification of a bracket character relative to a pair table.
struct BracketHit {
    /// The pair this bracket belongs to.
    pair: BracketPair,
    /// True if the character is the *opening* side of `pair`.
    is_open: bool,
}

fn classify(c: char, pairs: &[BracketPair]) -> Option<BracketHit> {
    for p in pairs {
        if c == p.open {
            return Some(BracketHit { pair: *p, is_open: true });
        }
        if c == p.close {
            return Some(BracketHit { pair: *p, is_open: false });
        }
    }
    None
}

/// Read the char starting at byte offset `byte`. Returns `(char, len_in_bytes)`.
fn char_at(doc: &Rope, byte: usize) -> Option<(char, usize)> {
    if byte >= doc.len_bytes() {
        return None;
    }
    let end = doc.next_char_boundary(byte);
    let s = doc.slice(byte..end).to_string();
    s.chars().next().map(|c| (c, end - byte))
}

/// Read the char immediately *before* byte offset `byte`. Returns
/// `(char, start_byte_of_char)`.
fn char_before(doc: &Rope, byte: usize) -> Option<(char, usize)> {
    if byte == 0 {
        return None;
    }
    let start = doc.prev_char_boundary(byte);
    let s = doc.slice(start..byte).to_string();
    s.chars().next().map(|c| (c, start))
}

/// Scan from `from_byte` in `dir` looking for a balanced partner of `seed`.
/// `seed.is_open == true` => scan forward for the matching close.
/// `seed.is_open == false` => scan backward for the matching open.
/// Returns `Some(byte_offset_of_partner)` if found within `max_scan` chars.
fn find_partner(
    doc: &Rope,
    seed: &BracketHit,
    from_byte: usize,
    dir: Dir,
    pairs: &[BracketPair],
    max_scan: usize,
) -> Option<usize> {
    let mut depth: usize = 1;
    let mut scanned: usize = 0;
    let mut cursor = from_byte;
    let total = doc.len_bytes();

    loop {
        if scanned >= max_scan {
            return None;
        }
        match dir {
            Dir::Forward => {
                if cursor >= total {
                    return None;
                }
                let (c, len) = char_at(doc, cursor)?;
                let char_start = cursor;
                cursor += len;
                scanned += 1;
                if let Some(hit) = classify(c, pairs) {
                    if hit.pair.open == seed.pair.open && hit.pair.close == seed.pair.close {
                        if hit.is_open {
                            depth += 1;
                        } else {
                            depth -= 1;
                            if depth == 0 {
                                return Some(char_start);
                            }
                        }
                    }
                }
            }
            Dir::Backward => {
                if cursor == 0 {
                    return None;
                }
                let (c, char_start) = char_before(doc, cursor)?;
                cursor = char_start;
                scanned += 1;
                if let Some(hit) = classify(c, pairs) {
                    if hit.pair.open == seed.pair.open && hit.pair.close == seed.pair.close {
                        if !hit.is_open {
                            depth += 1;
                        } else {
                            depth -= 1;
                            if depth == 0 {
                                return Some(char_start);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// For each cursor, attempt to highlight matching (or unmatched) brackets
/// adjacent to the caret.
pub fn bracket_match_decorations(
    state: &EditorState,
    pairs: &[BracketPair],
    max_scan: usize,
) -> DecorationSet {
    let doc = &state.doc;
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    for sel in state.selection.ranges() {
        // We act on the head of each selection range as the caret position.
        let caret = sel.head.offset();
        process_caret(doc, caret, pairs, max_scan, &mut entries);
    }

    // Dedup overlapping identical ranges (same caret span hit by multiple
    // selections, or after/before classifications colliding).
    entries.sort_by_key(|e| e.0.start);
    entries.dedup_by(|a, b| a.0 == b.0);
    RangeSet::from_iter(entries)
}

fn process_caret(
    doc: &Rope,
    caret: usize,
    pairs: &[BracketPair],
    max_scan: usize,
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
) {
    // Prefer the char AFTER the caret (CodeMirror/VSCode-style), then the char
    // BEFORE. Only act on a single seed per caret.
    if let Some((c, len)) = char_at(doc, caret) {
        if let Some(hit) = classify(c, pairs) {
            emit_for_seed(doc, &hit, caret, len, pairs, max_scan, entries);
            return;
        }
    }
    if let Some((c, start)) = char_before(doc, caret) {
        if let Some(hit) = classify(c, pairs) {
            let len = caret - start;
            emit_for_seed(doc, &hit, start, len, pairs, max_scan, entries);
        }
    }
}

fn emit_for_seed(
    doc: &Rope,
    seed: &BracketHit,
    seed_start: usize,
    seed_len: usize,
    pairs: &[BracketPair],
    max_scan: usize,
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
) {
    let (scan_from, dir) = if seed.is_open {
        (seed_start + seed_len, Dir::Forward)
    } else {
        (seed_start, Dir::Backward)
    };
    let seed_range = seed_start..seed_start + seed_len;

    if let Some(partner_start) = find_partner(doc, seed, scan_from, dir, pairs, max_scan) {
        let partner_end = doc.next_char_boundary(partner_start);
        entries.push((seed_range, mark(MATCH_COLOR)));
        entries.push((partner_start..partner_end, mark(MATCH_COLOR)));
    } else {
        entries.push((seed_range, mark(WARN_COLOR)));
    }
}

fn mark(bg: Color) -> Decoration {
    Decoration::Mark(MarkStyle {
        bg: Some(bg),
        ..MarkStyle::default()
    })
}
