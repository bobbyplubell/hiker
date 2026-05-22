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
        // Look backwards for the most recent `[[` opener on the same line.
        let doc = state.doc.to_string();
        let bytes = doc.as_bytes();
        if pos < 2 {
            return Vec::new();
        }
        // Bound the look-back to the current line.
        let line_start = doc[..pos.min(doc.len())]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut open: Option<usize> = None;
        let mut i = pos.saturating_sub(2);
        while i >= line_start {
            if i + 1 < bytes.len() && bytes[i] == b'[' && bytes[i + 1] == b'[' {
                open = Some(i + 2);
                break;
            }
            if bytes.get(i).copied() == Some(b']') {
                // A close before the opener — not a wikilink context.
                return Vec::new();
            }
            if i == line_start {
                break;
            }
            i -= 1;
        }
        let Some(query_start) = open else {
            return Vec::new();
        };
        let query = &doc[query_start..pos.min(doc.len())];
        // If the query contains `]` we're past the wikilink; bail.
        if query.contains(']') {
            return Vec::new();
        }

        // Walk vault, score basename matches. Cheap for thousands of notes;
        // for larger vaults we'd cache the path list and only rebuild on
        // watcher events.
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
            items.push((
                score,
                CompletionItem {
                    label: SmolStr::from(basename),
                    detail: Some(SmolStr::from(rel.as_str())),
                    insert: SmolStr::from(format!("{basename}]]")),
                    replace_range: Some(query_start..pos),
                    kind: CompletionKind::Wikilink,
                },
            ));
        }
        items.sort_by_key(|x| std::cmp::Reverse(x.0));
        items.truncate(20);
        items.into_iter().map(|(_, item)| item).collect()
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
