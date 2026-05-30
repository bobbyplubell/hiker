//! Editor completion sources hosted by the egui app.
//!
//! Currently provides a wikilink autocomplete that fires on `[[` and
//! offers vault paths whose basename matches the partial typed after the
//! opening brackets.

use std::sync::Arc;

use editor_core::state::Editor;
use editor_view::autocomplete::CompletionItem;
use editor_view::autocomplete::CompletionKind;
use editor_view::autocomplete::CompletionSource;
use hiker_core::vault::Vault;
use smol_str::SmolStr;

/// Wikilink completion: opens after the user types `[[` and offers a
/// ranked list of vault notes by basename match against the chars typed
/// since.
pub struct WikilinkSource {
    pub vault: Arc<Vault>,
}

impl CompletionSource for WikilinkSource {
    fn triggers(&self) -> &[char] {
        &['[']
    }

    fn matches(&self, state: &Editor, pos: usize) -> Vec<CompletionItem> {
        let doc = state.doc.to_string();
        let Some(query_start) = wikilink_query_start(&doc, pos) else {
            return Vec::new();
        };
        let query = &doc[query_start..pos.min(doc.len())];

        // Bracket auto-pairing (`pairs.rs`) inserts the closing `]]` the
        // moment the user types `[[`, so the buffer is usually already
        // `[[query]]` with the caret before the `]]`. If we appended our
        // own `]]` we'd get `[[name]]]]`. Detect a closing `]]` (or a
        // single `]`) immediately after the caret and (a) extend the
        // replaced range to swallow it, (b) drop our appended `]]` when a
        // full `]]` is already there. When nothing follows (auto-pair off,
        // or the close was deleted) we still append `]]` so the link is
        // well-formed. status: bug-wikilink-autocomplete-double-close
        let after = &doc[pos.min(doc.len())..];
        let (consume, suffix) = close_fixup(after);
        let replace_end = pos + consume;

        // Walk vault, score basename matches. Cheap for thousands of notes;
        // for larger vaults we'd cache the path list and only rebuild on
        // watcher events. Insert form is the shortest-unambiguous path-form
        // per `wikilink-autocomplete`: bare basename when unique vault-wide,
        // otherwise the minimal folder-prefix path that disambiguates.
        let paths = self.vault.walk_indexable_files("").unwrap_or_default();
        let needle = query.to_lowercase();
        let mut items: Vec<(i32, CompletionItem)> = Vec::new();
        for rel in paths.iter().take(5000) {
            let basename = rel
                .rsplit('/')
                .next()
                .unwrap_or(rel)
                .trim_end_matches(".md");
            let bn_lower = basename.to_lowercase();
            let score = self.score_basename(&bn_lower, &needle);
            if score <= 0 && !needle.is_empty() {
                continue;
            }
            let insert_form = hiker_core::wikilink::shortest_unambiguous(&paths, rel);
            items.push((
                score,
                CompletionItem {
                    label: SmolStr::from(basename),
                    detail: Some(SmolStr::from(rel.as_str())),
                    insert: SmolStr::from(format!("{insert_form}{suffix}")),
                    replace_range: Some(query_start..replace_end),
                    kind: CompletionKind::Wikilink,
                },
            ));
        }
        items.sort_by_key(|x| std::cmp::Reverse(x.0));
        items.truncate(20);
        items.into_iter().map(|(_, item)| item).collect()
    }

    /// status: bug-wikilink-edit-reopens-popup
    fn reopens_in_context(&self, state: &Editor, pos: usize) -> bool {
        // Cheap-ish: the same look-back `matches` gates on. Lets the
        // popup re-open when the user types inside an existing
        // `[[wikilink]]` (which doesn't re-fire the `[` trigger).
        wikilink_query_start(&state.doc.to_string(), pos).is_some()
    }
}

/// Find the byte offset where the wikilink query begins — i.e. just
/// after the most recent `[[` opener on the caret's line — or `None`
/// when the caret isn't inside an open `[[ … ` context. Returns `None`
/// if a `]` appears between the opener and the caret (the link is
/// already closed / we're past it). Shared by `matches` (to slice the
/// query) and `reopens_in_context` (to decide whether to re-open).
fn wikilink_query_start(doc: &str, pos: usize) -> Option<usize> {
    if pos < 2 {
        return None;
    }
    let end = pos.min(doc.len());
    let bytes = doc.as_bytes();
    let line_start = doc[..end].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut open: Option<usize> = None;
    let mut i = pos.saturating_sub(2);
    while i >= line_start {
        if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b'[' {
            open = Some(i + 2);
            break;
        }
        if bytes.get(i).copied() == Some(b']') {
            return None;
        }
        if i == line_start {
            break;
        }
        i -= 1;
    }
    let query_start = open?;
    // Past the link if a `]` sits between the opener and the caret.
    if doc[query_start..end].contains(']') {
        return None;
    }
    Some(query_start)
}

/// Decide how an accepted wikilink completion should land relative to an
/// existing auto-paired close. `after` is the text immediately after the
/// caret. Returns `(bytes_of_after_to_also_replace, suffix_to_append)`:
///
/// - `]]` already there (auto-pair) → swallow both, append nothing.
/// - a lone `]` → swallow it, append one `]` to complete the pair.
/// - nothing → append `]]` so the link is well-formed.
///
/// This is what stops `[[` auto-pairing from yielding `[[name]]]]`.
/// status: bug-wikilink-autocomplete-double-close
fn close_fixup(after: &str) -> (usize, &'static str) {
    if after.starts_with("]]") {
        (2, "")
    } else if after.starts_with(']') {
        (1, "]")
    } else {
        (0, "]]")
    }
}

#[cfg(test)]
mod tests {
    use super::{close_fixup, wikilink_query_start};

    #[test]
    fn close_fixup_swallows_autopaired_close() {
        // `[[query]]` with caret before `]]` → consume 2, append none.
        assert_eq!(close_fixup("]]"), (2, ""));
        assert_eq!(close_fixup("]] trailing"), (2, ""));
    }

    #[test]
    fn close_fixup_completes_lone_close() {
        assert_eq!(close_fixup("]"), (1, "]"));
        assert_eq!(close_fixup("] more"), (1, "]"));
    }

    #[test]
    fn close_fixup_appends_when_unclosed() {
        assert_eq!(close_fixup(""), (0, "]]"));
        assert_eq!(close_fixup("word"), (0, "]]"));
    }

    #[test]
    fn query_start_finds_opener() {
        // "see [[foo" — caret at end (len 9); query starts after `[[`.
        let doc = "see [[foo";
        assert_eq!(wikilink_query_start(doc, doc.len()), Some(6));
    }

    #[test]
    fn query_start_none_outside_link() {
        assert_eq!(wikilink_query_start("plain text", 10), None);
    }

    #[test]
    fn query_start_none_when_closed_before_caret() {
        // Caret after the close — not an open context.
        let doc = "[[foo]] bar";
        assert_eq!(wikilink_query_start(doc, doc.len()), None);
    }

    #[test]
    fn query_start_bounded_to_line() {
        // Opener on a previous line doesn't count.
        let doc = "[[foo\nbar";
        assert_eq!(wikilink_query_start(doc, doc.len()), None);
    }
}

impl WikilinkSource {
    fn score_basename(&self, name: &str, needle: &str) -> i32 {
    if needle.is_empty() {
        return 1;
    }
    if name == needle {
        return 1000;
    }
    if name.starts_with(needle) {
        return 500;
    }
    if name.contains(needle) {
        return 200;
    }
    // Subsequence match (each needle char appears in order in name).
    let mut ni = needle.bytes();
    let mut next = ni.next();
    for b in name.bytes() {
        if let Some(c) = next {
            if c == b {
                next = ni.next();
            }
        }
    }
    if next.is_none() { 50 } else { 0 }
    }
}
