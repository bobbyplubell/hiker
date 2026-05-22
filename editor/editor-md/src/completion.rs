//! Wikilink completion source (SPEC §9.6).
//!
//! Triggers on `[`; when the user just typed the second `[` of `[[`, returns
//! one [`CompletionItem`] per known page name. The replacement range covers
//! the text between `[[` and the caret so committing extends the link.

use editor_core::state::Editor as EditorState;
use editor_view::autocomplete::CompletionItem;
use editor_view::autocomplete::CompletionKind;
use editor_view::autocomplete::CompletionSource;
use smol_str::SmolStr;

/// Static set of known wiki page names. Hosts wanting live page discovery can
/// build their own `CompletionSource`; this v1 source ships a flat list.
pub struct WikilinkSource {
    pub pages: Vec<SmolStr>,
}

impl WikilinkSource {
    pub const fn new(pages: Vec<SmolStr>) -> Self {
        Self { pages }
    }
}

impl CompletionSource for WikilinkSource {
    fn triggers(&self) -> &[char] {
        &['[']
    }

    fn matches(&self, state: &EditorState, pos: usize) -> Vec<CompletionItem> {
        // Walk backward from the caret to find the most recent `[[` on the
        // same line. If we don't see one before a newline or start-of-doc, no
        // completions apply.
        let doc = state.doc.to_string();
        let bytes = doc.as_bytes();
        if pos > bytes.len() {
            return Vec::new();
        }

        let mut i = pos;
        let mut query_start = pos;
        while i > 0 {
            let b = bytes[i - 1];
            if b == b'\n' || b == b']' {
                return Vec::new();
            }
            if b == b'[' && i >= 2 && bytes[i - 2] == b'[' {
                query_start = i;
                break;
            }
            if b == b'[' {
                // A single stray `[`; no `[[` opener here.
                return Vec::new();
            }
            i -= 1;
        }
        if i == 0 {
            return Vec::new();
        }

        let query = std::str::from_utf8(&bytes[query_start..pos]).unwrap_or("");
        let q_lower = query.to_lowercase();

        self.pages
            .iter()
            .filter(|p| q_lower.is_empty() || p.to_lowercase().contains(&q_lower))
            .map(|page| CompletionItem {
                label: page.clone(),
                detail: None,
                insert: SmolStr::from(format!("{page}]]")),
                replace_range: Some(query_start..pos),
                kind: CompletionKind::Wikilink,
            })
            .collect()
    }
}
