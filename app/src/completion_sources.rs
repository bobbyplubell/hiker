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
use editor_view::autocomplete::rank;
use hiker_core::vault::Vault;
use smol_str::SmolStr;

use crate::autocomplete::vault_source::Scope;
use crate::autocomplete::vault_source::VaultSource;
use crate::code_sources::CodeCompletionProvider;

/// How many ranked wikilink candidates to surface in the popup.
const WIKILINK_RESULT_CAP: usize = 20;

/// The `code:` namespace prefix that switches `[[` completion from vault
/// notes to code symbols (`[[code:<repo_id>/<symbol>]]`). status: spec-code-link
const CODE_PREFIX: &str = "code:";

/// Wikilink completion: opens after the user types `[[` and offers a
/// ranked list of vault notes by basename match against the chars typed
/// since.
///
/// When the typed query starts with `code:` it instead offers
/// `[[code:<repo_id>/<symbol>]]` candidates: repo_ids first, then — once a
/// repo and `/` are typed — the repo's code symbols. Driven by the optional
/// [`CodeCompletionProvider`]; `None` (preview/test buffers) keeps note-only
/// behavior. status: spec-code-link
pub struct WikilinkSource {
    pub vault: Arc<Vault>,
    /// Authoring helper for `[[code:` links; `None` disables the code branch.
    pub code: Option<Arc<CodeCompletionProvider>>,
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

        // A `#` in the open-link body switches from the note picker to the
        // anchor picker: `[[Page#` offers the page's headings, `[[Page#^`
        // offers its blocks (injecting a fresh marker on an un-anchored pick).
        // The page part before the `#` names the target note (empty = the
        // current buffer, a same-document `[[#…` anchor).
        // status: wikilink-block-anchor-autoinject
        if let Some(hash) = query.find('#') {
            return self.anchor_matches(state, &doc, query_start, pos, hash);
        }

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

        // `[[code:…` switches to the code-symbol source (repo_ids, then a
        // repo's symbols). Keeps note autocomplete untouched for every other
        // prefix. status: spec-code-link
        if let Some(rest) = query.strip_prefix(CODE_PREFIX) {
            if let Some(code) = &self.code {
                return code_matches(code, rest, suffix, query_start, replace_end);
            }
            return Vec::new();
        }

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

impl WikilinkSource {
    /// Build heading / block completions for an anchor-mode link. `hash` is the
    /// byte offset of the `#` within the open-link query (relative to
    /// `query_start`). Resolves the page part to a note body — the current
    /// buffer when the page is empty (`[[#…`) — then offers that note's headings
    /// (`#`) or blocks (`#^`). status: wikilink-block-anchor-autoinject
    fn anchor_matches(
        &self,
        state: &Editor,
        doc: &str,
        query_start: usize,
        pos: usize,
        hash: usize,
    ) -> Vec<CompletionItem> {
        let query = &doc[query_start..pos.min(doc.len())];
        let page = query[..hash].trim();
        let anchor = &query[hash + 1..];
        // The anchor text already typed begins right after the `#`. The replace
        // range covers that text (so re-typing replaces a partial anchor), plus
        // the close-fixup of whatever follows the caret.
        let anchor_start = query_start + hash + 1;
        let after = &doc[pos.min(doc.len())..];
        let (consume, suffix) = close_fixup(after);
        let replace_end = pos + consume;

        // Resolve the target note's body: a same-document `[[#…` reads the
        // current buffer; otherwise the page resolves against the vault.
        let body = match self.resolve_page_body(state, page) {
            Some(b) => b,
            None => return Vec::new(),
        };

        // `#^` → block picker; a plain `#` → heading picker.
        if let Some(block_query) = anchor.strip_prefix('^') {
            block_items(&body, block_query, anchor_start, replace_end, suffix)
        } else {
            heading_items(&body, anchor, anchor_start, replace_end, suffix)
        }
    }

    /// The body text of the note named by `page`: the live current-buffer text
    /// for an empty page (same-document anchor), otherwise the on-disk body of
    /// the resolved vault note. `None` when a non-empty page resolves to no
    /// single note. status: wikilink-block-anchor-autoinject
    fn resolve_page_body(&self, state: &Editor, page: &str) -> Option<String> {
        if page.is_empty() {
            return Some(state.doc.to_string());
        }
        let paths = self.vault.walk_indexable_files("").unwrap_or_default();
        match hiker_core::wikilink::resolve_path(
            &paths,
            page,
            hiker_core::wikilink::AmbiguityPolicy::LexFirst,
            None,
        ) {
            hiker_core::wikilink::Resolution::Resolved(p) => self.vault.read_file(&p).ok(),
            _ => None,
        }
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

/// Build block completion candidates for the note `body`, ranked against the
/// already-typed block-anchor query (the text after `#^`). Each candidate's
/// label is the block preview; an already-anchored block inserts its existing
/// `^id`, an un-anchored block inserts a fresh content-addressed `^id` that the
/// app-layer reconciler injects into the target note on commit.
/// status: wikilink-block-anchor-autoinject
fn block_items(
    body: &str,
    query: &str,
    anchor_start: usize,
    replace_end: usize,
    suffix: &str,
) -> Vec<CompletionItem> {
    let blocks = hiker_core::wikilink::scan_blocks(body);
    let candidates: Vec<RankCandidate> = blocks
        .iter()
        .map(|b| {
            let id = b
                .existing_id
                .clone()
                .unwrap_or_else(|| hiker_core::wikilink::fresh_block_id(body, &b.range));
            let insert = format!("^{id}{suffix}");
            RankCandidate {
                label: SmolStr::from(b.preview.as_str()),
                basename: None,
                item: CompletionItem {
                    label: SmolStr::from(b.preview.as_str()),
                    detail: Some(SmolStr::from(format!("^{id}"))),
                    insert: SmolStr::from(insert),
                    replace_range: Some(anchor_start..replace_end),
                    kind: CompletionKind::Wikilink,
                },
            }
        })
        .collect();
    rank(query, candidates, WIKILINK_RESULT_CAP)
}

/// Build heading completion candidates for the note `body`, ranked against the
/// already-typed heading query (the text after `#`). The insert is the
/// heading's GitHub slug so a click later resolves it (`wikilink-headings-
/// blocks`). status: wikilink-block-anchor-autoinject
fn heading_items(
    body: &str,
    query: &str,
    anchor_start: usize,
    replace_end: usize,
    suffix: &str,
) -> Vec<CompletionItem> {
    let candidates: Vec<RankCandidate> = scan_headings(body)
        .into_iter()
        .map(|(text, slug)| {
            let insert = format!("{slug}{suffix}");
            RankCandidate {
                label: SmolStr::from(text.as_str()),
                basename: None,
                item: CompletionItem {
                    label: SmolStr::from(text.as_str()),
                    detail: Some(SmolStr::from(format!("#{slug}"))),
                    insert: SmolStr::from(insert),
                    replace_range: Some(anchor_start..replace_end),
                    kind: CompletionKind::Wikilink,
                },
            }
        })
        .collect();
    rank(query, candidates, WIKILINK_RESULT_CAP)
}

/// Enumerate a note body's ATX headings as `(text, slug)` pairs, skipping
/// fenced code blocks. The slug is the GitHub anchor slug the read side matches
/// against (`hiker_core::wikilink::heading_slug`).
fn scan_headings(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in body.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes)
            && let Some(rest) = trimmed[hashes..].strip_prefix(' ')
        {
            let text = rest.trim().to_string();
            let slug = hiker_core::wikilink::heading_slug(&text);
            if !slug.is_empty() {
                out.push((text, slug));
            }
        }
    }
    out
}

/// Dispatch a `[[code:<rest>` completion. `rest` is everything typed after the
/// `code:` prefix. The FIRST `/` (mirroring `wikilink::parse_code_target`)
/// separates the committed repo from the partial symbol:
///
/// - no `/` yet → still choosing a repo: rank repo_ids, insert `code:<repo>/`
///   so the link continues into the symbol stage.
/// - `<repo>/<partial>` → repo chosen: bind that repo (loads its `.scip` once,
///   cached) and rank its symbols, inserting the full `code:<repo>/<moniker>`.
///
/// Binding is deferred to the symbol stage on purpose: the bare-`code:` /
/// repo-partial stage only needs the cheap project-note scan. status: spec-code-link
fn code_matches(
    code: &CodeCompletionProvider,
    rest: &str,
    suffix: &str,
    query_start: usize,
    replace_end: usize,
) -> Vec<CompletionItem> {
    match rest.split_once('/') {
        None => repo_candidates(&code.repos(), rest, suffix, query_start, replace_end),
        Some((repo_id, partial)) => {
            let Some(adapter) = code.adapter(repo_id) else { return Vec::new() };
            let entities: Vec<(String, String, String)> = adapter
                .entities()
                .map(|(m, k, n)| (m.clone(), k.to_string(), n.to_string()))
                .collect();
            symbol_candidates(&entities, repo_id, partial, suffix, query_start, replace_end)
        }
    }
}

/// Rank `repos` (`(repo_id, label)`) against the partially-typed repo id and
/// build completion items that insert `code:<repo_id>/` — continuing the link
/// into the symbol stage (no `suffix` yet; the link isn't done). Pure. status: spec-code-link
fn repo_candidates(
    repos: &[(String, String)],
    partial: &str,
    _suffix: &str,
    query_start: usize,
    replace_end: usize,
) -> Vec<CompletionItem> {
    let candidates: Vec<RankCandidate> = repos
        .iter()
        .map(|(repo_id, label)| RankCandidate {
            label: SmolStr::from(repo_id.as_str()),
            basename: None,
            item: CompletionItem {
                label: SmolStr::from(repo_id.as_str()),
                detail: Some(SmolStr::from(label.as_str())),
                // Insert continues the link; choosing a repo leaves the caret
                // ready to type the symbol. No close `suffix`: still mid-link.
                insert: SmolStr::from(format!("{CODE_PREFIX}{repo_id}/")),
                replace_range: Some(query_start..replace_end),
                kind: CompletionKind::Variable,
            },
        })
        .collect();
    rank(partial, candidates, WIKILINK_RESULT_CAP)
}

/// Rank a repo's `entities` (`(moniker, kind, name)`) against the partial
/// symbol and build items that insert the full `code:<repo_id>/<moniker>` plus
/// the close-fixup `suffix`, so accepting yields a valid `[[code:repo/moniker]]`.
/// Ranks on the human `name`; the inserted handle is the stable `moniker`. Pure
/// (no adapter / SCIP types) → unit-testable. status: spec-code-link
fn symbol_candidates(
    entities: &[(String, String, String)],
    repo_id: &str,
    partial: &str,
    suffix: &str,
    query_start: usize,
    replace_end: usize,
) -> Vec<CompletionItem> {
    let candidates: Vec<RankCandidate> = entities
        .iter()
        .map(|(moniker, kind, name)| RankCandidate {
            label: SmolStr::from(name.as_str()),
            basename: None,
            item: CompletionItem {
                label: SmolStr::from(name.as_str()),
                detail: Some(SmolStr::from(symbol_detail(kind, moniker))),
                insert: SmolStr::from(format!("{CODE_PREFIX}{repo_id}/{moniker}{suffix}")),
                replace_range: Some(query_start..replace_end),
                kind: completion_kind_of(kind),
            },
        })
        .collect();
    rank(partial, candidates, WIKILINK_RESULT_CAP)
}

/// `detail` line for a symbol candidate: the bare entity kind (`function`,
/// `type`, …) followed by the moniker's last path-ish segment as a locator.
fn symbol_detail(kind: &str, moniker: &str) -> String {
    let kind = kind.strip_prefix("code:").unwrap_or(kind);
    // Show a short tail of the moniker so colliding names are distinguishable.
    let tail = moniker
        .rsplit(|c| c == '/' || c == ' ')
        .find(|s| !s.is_empty())
        .unwrap_or(moniker);
    format!("{kind} · {tail}")
}

/// Map a `code:*` entity kind to a popup [`CompletionKind`] icon. Members
/// (function/method/macro) read as `Function`; everything else as `Variable`.
fn completion_kind_of(kind: &str) -> CompletionKind {
    match kind {
        "code:function" | "code:method" | "code:macro" => CompletionKind::Function,
        _ => CompletionKind::Variable,
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
mod code_tests {
    use editor_view::autocomplete::CompletionKind;

    use super::{completion_kind_of, repo_candidates, symbol_candidates, symbol_detail};

    fn ents() -> Vec<(String, String, String)> {
        // (moniker, kind, name) — a couple that share the partial "par".
        vec![
            (
                "scip rust core 1.0 parse_target().".to_string(),
                "code:function".to_string(),
                "parse_target".to_string(),
            ),
            (
                "scip rust core 1.0 Parser#".to_string(),
                "code:type".to_string(),
                "Parser".to_string(),
            ),
            (
                "scip rust core 1.0 unrelated().".to_string(),
                "code:function".to_string(),
                "unrelated".to_string(),
            ),
        ]
    }

    #[test]
    fn repo_candidates_insert_continues_link() {
        let repos = vec![
            ("alpha".to_string(), "code/alpha".to_string()),
            ("beta".to_string(), "code/beta".to_string()),
        ];
        // Partial "al" ranks alpha first; insert continues into the symbol stage.
        let items = repo_candidates(&repos, "al", "]]", 2, 4);
        assert_eq!(items[0].label.as_str(), "alpha");
        assert_eq!(items[0].insert.as_str(), "code:alpha/");
        assert_eq!(items[0].detail.as_deref(), Some("code/alpha"));
        assert_eq!(items[0].replace_range, Some(2..4));
        assert_eq!(items[0].kind, CompletionKind::Variable);
    }

    #[test]
    fn repo_candidates_empty_partial_lists_all() {
        let repos = vec![("alpha".to_string(), "a".to_string())];
        let items = repo_candidates(&repos, "", "", 0, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert.as_str(), "code:alpha/");
    }

    #[test]
    fn symbol_candidates_rank_by_name_and_insert_full_moniker() {
        let entities = ents();
        // Partial "par" matches parse_target + Parser, drops "unrelated".
        let items = symbol_candidates(&entities, "myrepo", "par", "]]", 2, 4);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"parse_target"));
        assert!(labels.contains(&"Parser"));
        assert!(!labels.contains(&"unrelated"));
        // The inserted handle is the full `code:repo/<moniker>` + close fixup —
        // a valid `[[code:repo/moniker]]` once the brackets close.
        let parse = items.iter().find(|i| i.label == "parse_target").unwrap();
        assert_eq!(
            parse.insert.as_str(),
            "code:myrepo/scip rust core 1.0 parse_target().]]"
        );
        assert_eq!(parse.replace_range, Some(2..4));
        assert_eq!(parse.kind, CompletionKind::Function);
        let parser = items.iter().find(|i| i.label == "Parser").unwrap();
        assert_eq!(parser.kind, CompletionKind::Variable); // a type
    }

    #[test]
    fn symbol_insert_parses_back_to_repo_and_moniker() {
        // Round-trip: the produced link must split via the real parser.
        let entities = ents();
        let items = symbol_candidates(&entities, "myrepo", "parse", "", 0, 0);
        let parse = items.iter().find(|i| i.label == "parse_target").unwrap();
        let inner = parse.insert.as_str(); // no close suffix in this case
        let (repo, symbol) = hiker_core::wikilink::parse_code_target(inner)
            .expect("inserted code: string must parse");
        assert_eq!(repo, "myrepo");
        assert_eq!(symbol, "scip rust core 1.0 parse_target().");
    }

    #[test]
    fn detail_shows_kind_and_moniker_tail() {
        assert_eq!(symbol_detail("code:function", "scip rust core 1.0 foo()."), "function · foo().");
        assert_eq!(symbol_detail("code:type", "a/b/Bar#"), "type · Bar#");
    }

    #[test]
    fn kind_mapping() {
        assert_eq!(completion_kind_of("code:function"), CompletionKind::Function);
        assert_eq!(completion_kind_of("code:method"), CompletionKind::Function);
        assert_eq!(completion_kind_of("code:macro"), CompletionKind::Function);
        assert_eq!(completion_kind_of("code:type"), CompletionKind::Variable);
        assert_eq!(completion_kind_of("code:field"), CompletionKind::Variable);
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

/// Anchor-mode completion (`[[Page#` headings, `[[Page#^` blocks) on a
/// `WikilinkSource` driven directly through `matches`, against a vault whose
/// target note carries headings and blocks (one block already anchored).
#[cfg(test)]
mod anchor_tests {
    use std::sync::Arc;

    use editor_core::state::Editor;
    use editor_view::autocomplete::CompletionSource;
    use hiker_core::vault::Vault;
    use tempfile::TempDir;

    use super::WikilinkSource;

    const TARGET_BODY: &str =
        "# Intro\n\nFirst paragraph here.\n\nAlready tagged block. ^kept\n\n## Details\n";

    fn source_for(target_body: &str) -> (TempDir, WikilinkSource) {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("target.md"), target_body).unwrap();
        let vault = Arc::new(Vault::open(td.path()).unwrap());
        (td, WikilinkSource { vault, code: None })
    }

    /// Build an editor whose text is `doc` with the caret at the end.
    fn editor_at_end(doc: &str) -> Editor {
        let mut ed = Editor::new(doc);
        ed.selection = editor_core::selection::Selection::single(doc.len());
        ed
    }

    #[test]
    fn hash_offers_target_headings() {
        let (_td, src) = source_for(TARGET_BODY);
        // Caret before the auto-paired `]]`, so the insert carries no own close.
        let doc = "[[target#]]";
        let caret = doc.find("]]").unwrap();
        let mut ed = Editor::new(doc);
        ed.selection = editor_core::selection::Selection::single(caret);
        let items = src.matches(&ed, caret);
        let inserts: Vec<&str> = items.iter().map(|i| i.insert.as_str()).collect();
        // Heading slugs inserted (auto-paired `]]` already present → no suffix).
        assert!(inserts.contains(&"intro"), "offers Intro heading slug, got {inserts:?}");
        assert!(inserts.contains(&"details"), "offers Details heading slug");
    }

    #[test]
    fn hash_caret_offers_blocks_reusing_existing_id() {
        let (_td, src) = source_for(TARGET_BODY);
        // Caret before an auto-paired `]]`, mirroring the real editing buffer.
        let doc = "[[target#^]]";
        let caret = doc.find("]]").unwrap();
        let mut ed = Editor::new(doc);
        ed.selection = editor_core::selection::Selection::single(caret);
        let items = src.matches(&ed, caret);
        let inserts: Vec<&str> = items.iter().map(|i| i.insert.as_str()).collect();
        // The already-tagged block reuses its `^kept` id verbatim.
        assert!(inserts.contains(&"^kept"), "reuses existing block id, got {inserts:?}");
        // The un-anchored "First paragraph here." block gets a fresh `^id` whose
        // id re-derives to the same content hash the reconciler will look up.
        let para_block_id = {
            let blocks = hiker_core::wikilink::scan_blocks(TARGET_BODY);
            let para = blocks.iter().find(|b| b.preview == "First paragraph here.").unwrap();
            hiker_core::wikilink::fresh_block_id(TARGET_BODY, &para.range)
        };
        assert!(
            inserts.contains(&format!("^{para_block_id}").as_str()),
            "un-anchored block offered with its content-addressed id, got {inserts:?}",
        );
    }

    #[test]
    fn same_document_hash_caret_offers_current_buffer_blocks() {
        // An empty page (`[[#^`) targets the current buffer; blocks come from the
        // editor text, not a vault note.
        let (_td, src) = source_for("# Other\n");
        let doc = "Local block here.\n\n[[#^";
        let items = src.matches(&editor_at_end(doc), doc.len());
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"Local block here."),
            "same-doc anchor enumerates current-buffer blocks, got {labels:?}",
        );
    }
}
