//! Search / find-and-replace engine + state + decoration provider.
//!
//! SPEC §9.13, IMPLEMENTATION §16.6.4. This module is the non-UI machinery
//! for the find panel: the [`SearchState`] structure plus pure functions that
//! compute matches, build replacement transactions, and emit highlight
//! decorations. UI rendering is a separate task.

use std::ops::Range;

use editor_core::change::Set as ChangeSet;
use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::transaction::EditType;

use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::transaction::Transaction;
/// Light yellow background for ordinary search matches.
const SEARCH_MATCH_BG: Color = Color::rgba(255, 235, 130, 90);
/// Stronger orange background for the currently-focused match.
const SEARCH_CURRENT_BG: Color = Color::rgba(255, 165, 0, 160);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchFlags {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub in_selection: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub replacement: String,
    pub flags: SearchFlags,
    pub matches: Vec<Range<usize>>,
    pub current_idx: Option<usize>,
}

impl SearchState {
    pub const fn open(&mut self) {
        self.active = true;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.matches.clear();
        self.current_idx = None;
    }

    pub fn set_query(&mut self, q: impl Into<String>) {
        self.query = q.into();
    }

    pub fn next(&mut self) {
        if self.matches.is_empty() {
            self.current_idx = None;
            return;
        }
        self.current_idx = Some(match self.current_idx {
            Some(i) => (i + 1) % self.matches.len(),
            None => 0,
        });
    }

    pub fn prev(&mut self) {
        if self.matches.is_empty() {
            self.current_idx = None;
            return;
        }
        self.current_idx = Some(match self.current_idx {
            Some(0) | None => self.matches.len() - 1,
            Some(i) => i - 1,
        });
    }
}

/// Compute all matches for `query` in `state.doc`, honoring `flags`.
pub fn run_search(state: &EditorState, query: &str, flags: SearchFlags) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let doc = state.doc.to_string();
    let (scan_start, scan_end) = if flags.in_selection {
        let r = state.selection.main().range();
        if r.is_empty() {
            return Vec::new();
        }
        (r.start, r.end.min(doc.len()))
    } else {
        (0, doc.len())
    };
    let haystack = &doc[scan_start..scan_end];

    let raw: Vec<Range<usize>> = if flags.regex {
        let final_pat = if flags.case_sensitive {
            query.to_string()
        } else {
            format!("(?i){query}")
        };
        match regex::Regex::new(&final_pat) {
            Ok(re) => re.find_iter(haystack).map(|m| m.start()..m.end()).collect(),
            Err(_) => Vec::new(),
        }
    } else if flags.case_sensitive {
        haystack
            .match_indices(query)
            .map(|(i, m)| i..i + m.len())
            .collect()
    } else {
        // Case-insensitive: lowercase both, search the lowercased haystack.
        // Because Unicode lowercasing can change byte lengths, we map matches
        // from the lowercased haystack back to original byte offsets.
        let lower_h: String = haystack.to_lowercase();
        let lower_n: String = query.to_lowercase();
        if lower_n.is_empty() {
            Vec::new()
        } else {
            let mut map: Vec<usize> = Vec::with_capacity(lower_h.len() + 1);
            for (orig_byte, ch) in haystack.char_indices() {
                let lower_len: usize = ch.to_lowercase().map(char::len_utf8).sum();
                for _ in 0..lower_len {
                    map.push(orig_byte);
                }
            }
            map.push(haystack.len());

            let mut out = Vec::new();
            for (i, m) in lower_h.match_indices(&lower_n) {
                let s = *map.get(i).unwrap_or(&haystack.len());
                let e = *map.get(i + m.len()).unwrap_or(&haystack.len());
                out.push(s..e);
            }
            out
        }
    };

    let mut out: Vec<Range<usize>> = raw
        .into_iter()
        .map(|r| (r.start + scan_start)..(r.end + scan_start))
        .collect();

    if flags.whole_word {
        let bytes = doc.as_bytes();
        out.retain(|r| {
            let before_ok = r.start == 0 || !is_word_byte(bytes[r.start - 1]);
            let after_ok = r.end >= bytes.len() || !is_word_byte(bytes[r.end]);
            before_ok && after_ok
        });
    }
    out
}

const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Build a transaction that replaces the current match with
/// `search.replacement`. Returns `None` when there is no current match or the
/// search has no replacement target.
pub fn replace_current(state: &EditorState, search: &SearchState) -> Option<Transaction> {
    let idx = search.current_idx?;
    let m = search.matches.get(idx)?.clone();
    let edits = vec![(m, search.replacement.clone())];
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Some(Transaction::new(changes).with_edit_type(EditType::Other))
}

/// Build a transaction that replaces every match with `search.replacement`.
pub fn replace_all(state: &EditorState, search: &SearchState) -> Option<Transaction> {
    if search.matches.is_empty() {
        return None;
    }
    let mut edits: Vec<(Range<usize>, String)> = search
        .matches
        .iter()
        .cloned()
        .map(|r| (r, search.replacement.clone()))
        .collect();
    edits.sort_by_key(|(r, _)| r.start);
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Some(Transaction::new(changes).with_edit_type(EditType::Other))
}

/// Emit a `Mark` decoration per match. The match at `current_idx` gets the
/// stronger highlight color.
pub fn search_decorations(state: &EditorState, search: &SearchState) -> DecorationSet {
    let total = state.doc.len_bytes();
    let mut entries: Vec<(Range<usize>, Decoration)> = Vec::with_capacity(search.matches.len());
    for (i, m) in search.matches.iter().enumerate() {
        let start = m.start.min(total);
        let end = m.end.min(total).max(start);
        if start == end {
            continue;
        }
        let bg = if Some(i) == search.current_idx {
            SEARCH_CURRENT_BG
        } else {
            SEARCH_MATCH_BG
        };
        entries.push((
            start..end,
            Decoration::Mark(MarkStyle { bg: Some(bg), ..MarkStyle::default() }),
        ));
    }
    entries.sort_by_key(|(r, _)| r.start);
    RangeSet::from_iter(entries)
}
