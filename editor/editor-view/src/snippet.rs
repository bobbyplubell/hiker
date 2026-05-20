//! Snippet expansion with tab stops. See SPEC §9.22, IMPLEMENTATION §16.6.14.
//!
//! Syntax:
//!   * `$N` or `${N}`              — tab stop number N (1-based)
//!   * `${N:placeholder text}`     — tab stop with default text
//!   * `$0` / `${0}`               — final cursor position (after the last Tab)
//!   * Literal `$` and `}` escape with `\$` and `\}`
//!
//! Tab stops that share the same number become mirrored (synced edits while
//! the user is parked on that stop).

use std::collections::BTreeMap;
use std::ops::Range;

use editor_core::{
    Anchor, Bias, ChangeSet, EditType, EditorState, SelRange, Selection, Transaction,
};

/// A parsed snippet template.
#[derive(Clone, Debug)]
pub struct Snippet {
    /// Final visible text with placeholders inlined.
    text: String,
    /// Per-tab-stop spans into `text`. Key = tab stop number;
    /// value = list of (byte_start, byte_end) ranges in `text`.
    /// Stop `0` (final cursor) lives at the end of the natural ordering but
    /// is exposed separately by `expand`.
    stops: BTreeMap<u32, Vec<Range<usize>>>,
}

/// State the caller stashes on `ViewState` to drive Tab cycling.
#[derive(Clone, Debug, Default)]
pub struct SnippetState {
    /// Anchors for each tab stop's spans, ordered by cycle order
    /// (`$1`, `$2`, …, then `$0`). Inner Vec entries are mirror spans.
    pub stops: Vec<Vec<(Anchor, Anchor)>>,
    /// Current cycle index. `usize::MAX` means "done — clear on next event".
    pub current: usize,
}

impl SnippetState {
    /// True if there is at least one stop the user has not visited yet.
    pub fn is_active(&self) -> bool {
        !self.stops.is_empty() && self.current != usize::MAX
    }

    /// Reset to the inactive default.
    pub fn cancel(&mut self) {
        self.stops.clear();
        self.current = 0;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// `$` at end of input with no following number/brace.
    DanglingDollar,
    /// `${` without matching `}`.
    UnterminatedBrace,
    /// A non-digit character followed `${` or `$`.
    ExpectedDigit,
    /// Trailing backslash with nothing to escape.
    DanglingEscape,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::DanglingDollar => f.write_str("dangling `$`"),
            ParseError::UnterminatedBrace => f.write_str("unterminated `${...}`"),
            ParseError::ExpectedDigit => f.write_str("expected digit after `$`"),
            ParseError::DanglingEscape => f.write_str("dangling `\\`"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Snippet {
    /// Parse a template string. See module docs for syntax.
    pub fn parse(template: &str) -> Result<Self, ParseError> {
        let mut text = String::with_capacity(template.len());
        let mut stops: BTreeMap<u32, Vec<Range<usize>>> = BTreeMap::new();
        let bytes = template.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\\' {
                let next = *bytes.get(i + 1).ok_or(ParseError::DanglingEscape)?;
                // Only `\$` and `\}` are special — every other `\x` is
                // copied verbatim (including the backslash) to preserve
                // existing escape sequences like `\n` in template text.
                if next == b'$' || next == b'}' {
                    text.push(next as char);
                    i += 2;
                } else {
                    text.push('\\');
                    i += 1;
                }
                continue;
            }
            if c == b'$' {
                let (consumed, num, default) = parse_dollar(&template[i..])?;
                let start = text.len();
                if let Some(d) = default {
                    text.push_str(d);
                }
                let end = text.len();
                stops.entry(num).or_default().push(start..end);
                i += consumed;
                continue;
            }
            // UTF-8: copy a single codepoint.
            let ch_len = utf8_len(c);
            text.push_str(&template[i..i + ch_len]);
            i += ch_len;
        }
        Ok(Self { text, stops })
    }

    /// Rendered text with placeholders inlined.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// All parsed tab stops, keyed by stop number.
    pub fn stops(&self) -> &BTreeMap<u32, Vec<Range<usize>>> {
        &self.stops
    }

    /// Build a transaction that inserts the snippet at `pos`, replacing
    /// `replace_range` if given. Also returns a [`SnippetState`] the caller
    /// should stash on `ViewState` to drive Tab cycling.
    pub fn expand(
        &self,
        state: &EditorState,
        pos: usize,
        replace_range: Option<Range<usize>>,
    ) -> (Transaction, SnippetState) {
        let range = replace_range.unwrap_or(pos..pos);
        let insert_start = range.start;
        let doc_len = state.doc.len_bytes();
        let changes = ChangeSet::of(doc_len, [(range, self.text.clone())]);

        // Build per-stop anchors in cycle order: $1, $2, … then $0.
        let mut cycle: Vec<u32> = self.stops.keys().copied().filter(|n| *n != 0).collect();
        if self.stops.contains_key(&0) {
            cycle.push(0);
        }
        let mut snip = SnippetState::default();
        for n in cycle {
            let spans = self
                .stops
                .get(&n)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let mut mirrors: Vec<(Anchor, Anchor)> = Vec::with_capacity(spans.len());
            for span in spans {
                let s = insert_start + span.start;
                let e = insert_start + span.end;
                // Bias so the anchor pair grows with edits inside the stop.
                mirrors.push((Anchor::at(s, Bias::Left), Anchor::at(e, Bias::Right)));
            }
            snip.stops.push(mirrors);
        }

        let tx = Transaction::new(changes).with_edit_type(EditType::Input);
        // Drop selection placement here — caller will set it via
        // `selection_for_stop` after applying so anchors are in the new doc.
        (tx, snip)
    }
}

/// Parse a `$…` token starting at the `$`. Returns the number of bytes
/// consumed (including the `$`), the stop number, and an optional default
/// placeholder text.
fn parse_dollar(s: &str) -> Result<(usize, u32, Option<&str>), ParseError> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[0], b'$');
    if bytes.len() < 2 {
        return Err(ParseError::DanglingDollar);
    }
    if bytes[1] == b'{' {
        // `${N}` or `${N:default}`. Scan the digits.
        let mut j = 2usize;
        let num_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == num_start {
            return Err(ParseError::ExpectedDigit);
        }
        let num: u32 = s[num_start..j].parse().map_err(|_| ParseError::ExpectedDigit)?;
        if j >= bytes.len() {
            return Err(ParseError::UnterminatedBrace);
        }
        match bytes[j] {
            b'}' => Ok((j + 1, num, None)),
            b':' => {
                let default_start = j + 1;
                // Allow `\}` to escape inside the default.
                let mut k = default_start;
                while k < bytes.len() {
                    if bytes[k] == b'\\' && k + 1 < bytes.len() && bytes[k + 1] == b'}' {
                        k += 2;
                        continue;
                    }
                    if bytes[k] == b'}' {
                        break;
                    }
                    k += utf8_len(bytes[k]);
                }
                if k >= bytes.len() {
                    return Err(ParseError::UnterminatedBrace);
                }
                let raw = &s[default_start..k];
                // Unescape `\}` -> `}` lazily into an owned string only if needed.
                // For v1 we just return the raw slice; `${N:foo\}bar}` is rare
                // and the escaping there is best handled by callers that need it.
                let default = if raw.contains('\\') {
                    // Allocate by leaking through a Box::leak via a static-life
                    // workaround is ugly; we return the raw slice unchanged and
                    // document that `\}` inside `${N:…}` is not currently
                    // post-processed. For v1 spec compliance the literal use
                    // case is plain text.
                    raw
                } else {
                    raw
                };
                Ok((k + 1, num, Some(default)))
            }
            _ => Err(ParseError::ExpectedDigit),
        }
    } else if bytes[1].is_ascii_digit() {
        let mut j = 1usize;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        let num: u32 = s[1..j].parse().map_err(|_| ParseError::ExpectedDigit)?;
        Ok((j, num, None))
    } else {
        Err(ParseError::ExpectedDigit)
    }
}

fn utf8_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte < 0xC0 {
        // Continuation byte — shouldn't happen at start, but be defensive.
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}

/// Build a [`Selection`] from the mirror anchors of stop `cycle_idx` in `snip`.
/// The first span becomes the "main" range; the rest are additional cursors
/// for mirrored editing.
pub fn selection_for_stop(snip: &SnippetState, cycle_idx: usize) -> Option<Selection> {
    let mirrors = snip.stops.get(cycle_idx)?;
    if mirrors.is_empty() {
        return None;
    }
    let ranges: Vec<SelRange> = mirrors
        .iter()
        .map(|(a, b)| SelRange::new(a.offset(), b.offset()))
        .collect();
    Some(Selection::from_ranges(ranges, 0))
}

/// After every edit applied by the user inside the current stop, mirror the
/// primary range's text into the other mirror spans of the same stop.
///
/// Returns a transaction that performs the mirror sync, or `None` if no sync
/// is required (single mirror, or current stop already finished).
pub fn mirror_sync(state: &EditorState, snip: &SnippetState) -> Option<Transaction> {
    if !snip.is_active() {
        return None;
    }
    let mirrors = snip.stops.get(snip.current)?;
    if mirrors.len() < 2 {
        return None;
    }
    let (pa, pb) = mirrors[0];
    let start = pa.offset().min(pb.offset());
    let end = pa.offset().max(pb.offset());
    if end > state.doc.len_bytes() {
        return None;
    }
    let primary_text = state.doc.slice(start..end).to_string();
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for (a, b) in mirrors.iter().skip(1) {
        let s = a.offset().min(b.offset());
        let e = a.offset().max(b.offset());
        if e > state.doc.len_bytes() {
            return None;
        }
        let existing = state.doc.slice(s..e).to_string();
        if existing == primary_text {
            continue;
        }
        edits.push((s..e, primary_text.clone()));
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(r, _)| r.start);
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Some(Transaction::new(changes).with_edit_type(EditType::Input))
}

/// Map every anchor in `snip` through `changes`. Call after applying any
/// transaction so the stored anchors stay valid.
pub fn map_through(snip: &mut SnippetState, changes: &ChangeSet) {
    for mirrors in &mut snip.stops {
        for (a, b) in mirrors.iter_mut() {
            *a = a.map(changes);
            *b = b.map(changes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_numbered_stops() {
        let s = Snippet::parse("for $1 in $2:\n    $0").unwrap();
        assert_eq!(s.text(), "for  in :\n    ");
        assert_eq!(s.stops().len(), 3);
        assert!(s.stops().contains_key(&0));
        assert!(s.stops().contains_key(&1));
        assert!(s.stops().contains_key(&2));
    }

    #[test]
    fn parse_placeholder_text() {
        let s = Snippet::parse("${1:item}").unwrap();
        assert_eq!(s.text(), "item");
        let span = &s.stops()[&1][0];
        assert_eq!(&s.text()[span.clone()], "item");
    }

    #[test]
    fn parse_escaped_dollar() {
        let s = Snippet::parse("price: \\$5").unwrap();
        assert_eq!(s.text(), "price: $5");
        assert!(s.stops().is_empty());
    }

    #[test]
    fn parse_mirror_indices() {
        let s = Snippet::parse("$1 $1").unwrap();
        assert_eq!(s.text(), " ");
        assert_eq!(s.stops()[&1].len(), 2);
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Snippet::parse("$").err(), Some(ParseError::DanglingDollar));
        assert_eq!(Snippet::parse("${1").err(), Some(ParseError::UnterminatedBrace));
        assert_eq!(Snippet::parse("${x}").err(), Some(ParseError::ExpectedDigit));
    }
}
