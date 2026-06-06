//! Editor completion sources hosted by the egui app.
//!
//! Currently provides a wikilink autocomplete that fires on `[[` and
//! offers vault paths whose basename matches the partial typed after the
//! opening brackets. Ranking is delegated to the shared core in
//! `editor_view::autocomplete::rank` (the same one the standalone pickers
//! and the chat `@`-mention use) — the wikilink source owns only the
//! `[[`/`]]` trigger + close-fixup + shortest-unambiguous insert form.

use std::sync::Arc;

use editor_core::state::Editor;
use editor_view::autocomplete::CompletionItem;
use editor_view::autocomplete::CompletionKind;
use editor_view::autocomplete::CompletionSource;
use editor_view::autocomplete::RankCandidate;
use hiker_core::vault::Vault;
use smol_str::SmolStr;

use crate::autocomplete::vault_source::Scope;
use crate::autocomplete::vault_source::VaultSource;

/// How many ranked wikilink candidates to surface in the popup.
const WIKILINK_RESULT_CAP: usize = 20;

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

        // Enumerate + rank through the shared `VaultSource` (notes-only
        // scope) so wikilink uses the one definition of "linkable vault
        // item" and the one ranking core. The per-candidate insert form is
        // the shortest-unambiguous path-form per `wikilink-autocomplete`:
        // bare basename when unique vault-wide, otherwise the minimal
        // folder-prefix path that disambiguates.
        let source = VaultSource::new(self.vault.clone(), Scope::NotesOnly);
        source.ranked_with(query, WIKILINK_RESULT_CAP, |rel, paths| {
            wikilink_candidate(rel, paths, suffix, query_start, replace_end)
        })
    }

    /// status: bug-wikilink-edit-reopens-popup
    fn reopens_in_context(&self, state: &Editor, pos: usize) -> bool {
        // Cheap-ish: the same look-back `matches` gates on. Lets the
        // popup re-open when the user types inside an existing
        // `[[wikilink]]` (which doesn't re-fire the `[` trigger).
        wikilink_query_start(&state.doc.to_string(), pos).is_some()
    }
}

/// Build the wikilink [`RankCandidate`] for one path `rel` (given the full
/// vault `paths` for disambiguation): scored on its basename (weighted above
/// the folder prefix by the shared core) with the committed insert being the
/// shortest-unambiguous path-form plus the close-fixup `suffix`, replacing
/// `query_start..replace_end`.
fn wikilink_candidate(
    rel: &str,
    paths: &[String],
    suffix: &str,
    query_start: usize,
    replace_end: usize,
) -> RankCandidate {
    let basename = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".md");
    let insert_form = hiker_core::wikilink::shortest_unambiguous(paths, rel);
    RankCandidate {
        label: SmolStr::from(rel),
        basename: Some(SmolStr::from(basename)),
        item: CompletionItem {
            label: SmolStr::from(basename),
            detail: Some(SmolStr::from(rel)),
            insert: SmolStr::from(format!("{insert_form}{suffix}")),
            replace_range: Some(query_start..replace_end),
            kind: CompletionKind::Wikilink,
        },
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
    use super::{close_fixup, wikilink_candidate, wikilink_query_start};

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

    #[test]
    fn candidate_insert_carries_suffix_and_unique_basename() {
        // Unique basename → bare form; suffix appended for the unclosed case.
        let paths = vec!["notes/architecture.md".to_string()];
        let cand = wikilink_candidate(&paths[0], &paths, "]]", 2, 2);
        assert_eq!(cand.item.insert.as_str(), "architecture]]");
        assert_eq!(cand.item.label.as_str(), "architecture");
        assert_eq!(cand.item.replace_range, Some(2..2));
    }

    #[test]
    fn candidate_disambiguates_colliding_basenames() {
        // Two `index.md` collide → shortest-unambiguous extends the prefix.
        let paths = vec!["a/index.md".to_string(), "b/index.md".to_string()];
        let a = wikilink_candidate(&paths[0], &paths, "", 0, 0);
        let b = wikilink_candidate(&paths[1], &paths, "", 0, 0);
        assert_eq!(a.item.insert.as_str(), "a/index");
        assert_eq!(b.item.insert.as_str(), "b/index");
    }
}

/// End-to-end typing tests for the `[[` → autocomplete flow, driven through the
/// real editor input pipeline (`command::handle`) on a `Buffer` built exactly
/// as the app builds one — with the `WikilinkSource` attached. Guards both the
/// auto-pair nesting (`[[` must produce `[[]]`, not `[[]`) and that the popup
/// actually opens on a buffer carrying content (the restored-note case).
#[cfg(test)]
mod typing_tests {
    use std::sync::Arc;

    use editor_view::command::{self, Action};
    use editor_view::events::InputEvent;
    use hiker_core::vault::Vault;
    use tempfile::TempDir;

    use crate::buffer::Buffer;

    /// A vault with a couple of notes plus a `target.md` the wikilink can
    /// resolve, and a `Buffer` for `host` built the way every open path builds
    /// one (vault handle supplied → `WikilinkSource` registered).
    fn buffer_in_vault(host: &str, host_contents: &str) -> (TempDir, Buffer) {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("target.md"), "# Target\n").unwrap();
        std::fs::write(td.path().join("other.md"), "# Other\n").unwrap();
        std::fs::write(td.path().join(host), host_contents).unwrap();
        let vault = Arc::new(Vault::open(td.path()).unwrap());
        let buf = Buffer::with_config_and_vault(
            host.to_string(),
            host_contents,
            hiker_core::hash_string(host_contents),
            None,
            Some(vault),
        );
        (td, buf)
    }

    /// Feed one `InputEvent::Text` through the real command pipeline and fold
    /// the resulting state back into the buffer, exactly as the widget does.
    fn type_text(buf: &mut Buffer, s: &str) {
        let action = command::handle(&buf.editor, &mut buf.view, &InputEvent::Text(s.into()));
        if let Action::Replace { state, .. } = action {
            buf.editor = state;
        }
    }

    fn type_open_brackets(buf: &mut Buffer) {
        // Two separate `[` text events — the real per-keystroke sequence.
        type_text(buf, "[");
        type_text(buf, "[");
    }

    #[test]
    fn typing_double_bracket_pairs_to_four_and_opens_completion() {
        let (_td, mut buf) = buffer_in_vault("note.md", "");
        type_open_brackets(&mut buf);
        assert_eq!(buf.editor.doc.to_string(), "[[]]", "`[[` must auto-pair to `[[]]`");
        assert_eq!(
            buf.editor.selection.main().head.offset(),
            2,
            "caret sits between the inner brackets",
        );
        assert!(buf.view.completion.active, "completion popup should open on `[[`");
        assert!(!buf.view.completion.items.is_empty(), "popup offers vault notes");
    }

    #[test]
    fn double_bracket_opens_completion_on_a_note_with_existing_content() {
        // The restored-note case: a buffer carrying prior content (frontmatter +
        // body). Typing `[[` at the end of the body must still open the popup.
        let body = "---\ntitle: Old Note\n---\n\nSome existing prose here.\n";
        let (_td, mut buf) = buffer_in_vault("old.md", body);
        // Move the caret to end of document (where the user types).
        let end = buf.editor.doc.len_bytes();
        buf.editor.selection = editor_core::selection::Selection::single(end);
        type_open_brackets(&mut buf);
        assert!(
            buf.editor.doc.to_string().ends_with("[[]]"),
            "`[[` nests to `[[]]` mid-document, got {:?}",
            buf.editor.doc.to_string(),
        );
        assert!(
            buf.view.completion.active,
            "completion must open on a content-bearing (restored) note too",
        );
    }

    #[test]
    fn wikilink_source_is_registered_when_vault_supplied() {
        let (_td, buf) = buffer_in_vault("note.md", "");
        assert_eq!(
            buf.view.completion_sources.len(),
            1,
            "a vault-backed buffer must carry the wikilink completion source",
        );
    }
}
