//! `VaultSource` — the single definition of "linkable / insertable vault
//! item." Enumerates vault paths (notes, and optionally other indexed
//! sources) and ranks them through the shared core in
//! `editor_view::autocomplete::rank`. Consumed by wikilink completion and
//! the standalone canvas/board pickers so "what can I link / insert" is one
//! definition, not re-derived per surface.
//!
//! status: autocomplete-vault-source

use std::sync::Arc;

use editor_view::autocomplete::CandidateSource;
use editor_view::autocomplete::CompletionItem;
use editor_view::autocomplete::CompletionKind;
use editor_view::autocomplete::RankCandidate;
use editor_view::autocomplete::rank;
use hiker_core::vault::Vault;
use smol_str::SmolStr;

/// Which vault entries a [`VaultSource`] enumerates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Notes only (`.md`/`.markdown`) — the wikilink surface.
    NotesOnly,
    /// Notes plus every other indexed source (`.txt`, …) — the canvas /
    /// board pickers.
    NotesAndSources,
}

/// Enumerates vault candidates and ranks them via the shared core. Cheap to
/// construct (holds an `Arc<Vault>`); the vault walk happens per
/// `candidates` call. For large vaults the path list could be cached and
/// rebuilt on watcher events — out of scope here.
pub struct VaultSource {
    vault: Arc<Vault>,
    scope: Scope,
}

impl VaultSource {
    /// A source over `vault` restricted to `scope`.
    #[must_use]
    pub const fn new(vault: Arc<Vault>, scope: Scope) -> Self {
        Self { vault, scope }
    }

    /// The vault's relative indexable paths, filtered to the scope. Notes
    /// are `.md`/`.markdown`; sources additionally include `.txt` etc.
    fn paths(&self) -> Vec<String> {
        let all = self.vault.walk_indexable_files("").unwrap_or_default();
        match self.scope {
            Scope::NotesAndSources => all,
            Scope::NotesOnly => all.into_iter().filter(|p| is_note(p)).collect(),
        }
    }

    /// Enumerate this source's scoped paths, build one [`RankCandidate`] per
    /// path via `to_candidate(rel, all_paths)`, and return the top `limit`
    /// ranked [`CompletionItem`]s. `all_paths` is passed so callers that need
    /// vault-wide context (e.g. the wikilink shortest-unambiguous form) can
    /// see sibling paths. This is the shared enumerate-then-`rank` seam every
    /// vault surface builds on. [autocomplete-vault-source]
    #[must_use]
    pub fn ranked_with<F>(&self, query: &str, limit: usize, to_candidate: F) -> Vec<CompletionItem>
    where
        F: Fn(&str, &[String]) -> RankCandidate,
    {
        let paths = self.paths();
        let candidates: Vec<RankCandidate> = paths
            .iter()
            .take(5000)
            .map(|rel| to_candidate(rel, &paths))
            .collect();
        rank(query, candidates, limit)
    }
}

/// `.md`/`.markdown` extension check (case-insensitive).
fn is_note(rel: &str) -> bool {
    rel.rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
}

/// The basename of a relative path with any `.md`/`.markdown`/`.txt`
/// extension stripped — the label shown in pickers and the field the
/// shared core weights above the folder prefix.
fn basename_of(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.trim_end_matches(".markdown")
        .trim_end_matches(".md")
        .trim_end_matches(".txt")
}

impl CandidateSource for VaultSource {
    fn candidates(&self, query: &str, limit: usize) -> Vec<CompletionItem> {
        // Standalone picker / mention default: label = basename, insert =
        // full relative path, detail = full path. Wikilink supplies its own
        // candidate via `ranked_with` (it needs the shortest-unambiguous
        // insert form + a replace range).
        self.ranked_with(query, limit, |rel, _paths| {
            let basename = basename_of(rel);
            RankCandidate {
                label: SmolStr::from(rel),
                basename: Some(SmolStr::from(basename)),
                item: CompletionItem {
                    label: SmolStr::from(basename),
                    detail: Some(SmolStr::from(rel)),
                    insert: SmolStr::from(rel),
                    replace_range: None,
                    kind: CompletionKind::Wikilink,
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{basename_of, is_note};

    #[test]
    fn note_extension_detection() {
        assert!(is_note("a/b.md"));
        assert!(is_note("c.markdown"));
        assert!(!is_note("d.txt"));
        assert!(!is_note("noext"));
    }

    #[test]
    fn basename_strips_folder_and_extension() {
        assert_eq!(basename_of("notes/architecture.md"), "architecture");
        assert_eq!(basename_of("plain.txt"), "plain");
        assert_eq!(basename_of("deep/path/file.markdown"), "file");
    }
}
