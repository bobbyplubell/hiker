//! "Find / jump to node" candidate sources for the graph views — the popup
//! analogue of a wikilink jump. Both the code-graph view and the vault
//! link-graph view open the shared [`autocomplete_picker`] over one of these
//! sources (Ctrl+F); a pick carries the node's stable id (a SCIP moniker or a
//! note rel-path) in `CompletionItem::insert`, which the view focuses /
//! navigates to.
//!
//! The ranking reuses the shared autocomplete core ([`rank`]) so a popup hit
//! orders the same way the editor's wikilink completion does (exact > prefix >
//! word-boundary > substring), returning the ranked top-`limit` rather than
//! the single first match the old inline search box resolved.
//!
//! [`autocomplete_picker`]: crate::widgets::autocomplete_picker

use editor_view::autocomplete::{
    rank, CandidateSource, CompletionItem, CompletionKind, RankCandidate,
};
use smol_str::SmolStr;

use super::entity_graph::{EntityNode, SPEC_KIND};

/// File basename (final `/`-segment) of a node's `file` path, for the picker
/// detail line. Returns the whole string when there's no separator.
fn file_basename(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file)
}

/// Map an entity kind discriminant (`code:type`, `code:function`, …) to a
/// completion-kind icon hint. Containers read as `Variable`, callables as
/// `Function`, everything else as `Text`.
fn kind_to_completion(kind: &str) -> CompletionKind {
    match kind {
        "code:function" | "code:method" | "code:macro" => CompletionKind::Function,
        "code:type" | "code:module" => CompletionKind::Variable,
        _ => CompletionKind::Text,
    }
}

/// A [`CandidateSource`] over the unified entity graph's nodes (code symbols + spec slugs). Ranks
/// by `name` and carries each node's `id` (SCIP moniker / spec slug) in `insert` so the view can
/// focus it after a pick. Borrows the node slice — built fresh per open, queried per frame while
/// the picker is up.
pub(crate) struct EntityNodeFindSource<'a> {
    nodes: &'a [EntityNode],
}

impl<'a> EntityNodeFindSource<'a> {
    pub(crate) const fn new(nodes: &'a [EntityNode]) -> Self {
        Self { nodes }
    }
}

impl CandidateSource for EntityNodeFindSource<'_> {
    fn candidates(&self, query: &str, limit: usize) -> Vec<CompletionItem> {
        let candidates = self
            .nodes
            .iter()
            .map(|n| {
                let detail = if n.kind == SPEC_KIND {
                    "spec".to_string()
                } else {
                    format!("{} · {}", n.kind, file_basename(&n.file))
                };
                RankCandidate {
                    label: SmolStr::from(n.name.as_str()),
                    basename: None,
                    item: CompletionItem {
                        label: SmolStr::from(n.name.as_str()),
                        detail: Some(SmolStr::from(detail)),
                        insert: SmolStr::from(n.id.as_str()),
                        replace_range: None,
                        kind: kind_to_completion(&n.kind),
                    },
                }
            })
            .collect();
        rank(query, candidates, limit)
    }
}

/// A [`CandidateSource`] over the vault link-graph's note paths. Ranks by the
/// note basename (boosted above the folder prefix, matching wikilink jump) and
/// carries the rel-path in `insert` so the view can navigate to the note.
pub struct VaultNodeFindSource<'a> {
    paths: &'a [String],
}

impl<'a> VaultNodeFindSource<'a> {
    pub const fn new(paths: &'a [String]) -> Self {
        Self { paths }
    }
}

impl CandidateSource for VaultNodeFindSource<'_> {
    fn candidates(&self, query: &str, limit: usize) -> Vec<CompletionItem> {
        let candidates = self
            .paths
            .iter()
            .map(|path| {
                let base = super::graph::basename(path);
                let folder = match path.rfind('/') {
                    Some(i) => &path[..i],
                    None => "",
                };
                RankCandidate {
                    label: SmolStr::from(path.as_str()),
                    basename: Some(SmolStr::from(base.as_str())),
                    item: CompletionItem {
                        label: SmolStr::from(base),
                        detail: (!folder.is_empty()).then(|| SmolStr::from(folder)),
                        insert: SmolStr::from(path.as_str()),
                        replace_range: None,
                        kind: CompletionKind::Wikilink,
                    },
                }
            })
            .collect();
        rank(query, candidates, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, kind: &str, file: &str) -> EntityNode {
        EntityNode {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            start_line: 0,
            lines: 1,
            status: None,
            parent: None,
        }
    }

    /// Entity find: a prefix match ranks above an interior substring, each item's `insert`
    /// carries the node id (not the name), and a spec node reads "spec" in its detail.
    #[test]
    fn entity_find_ranks_prefix_first_and_carries_id() {
        let nodes = vec![
            node("scip:parse_config", "parse_config", "code:function", "src/cfg.rs"),
            node("scip:Parser", "Parser", "code:type", "src/parse.rs"),
            node("scip:reparse", "reparse", "code:function", "src/parse.rs"),
            node("parser-spec", "parser-spec", SPEC_KIND, ""),
        ];
        let src = EntityNodeFindSource::new(&nodes);
        let items = src.candidates("par", 10);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // `Parser` + `parse_config` (prefix) outrank `reparse` (interior).
        assert!(
            labels.iter().position(|l| *l == "reparse")
                > labels.iter().position(|l| *l == "Parser")
        );
        assert!(labels.contains(&"Parser") && labels.contains(&"parse_config"));
        // `insert` is the SCIP id, not the display name.
        let parser = items.iter().find(|i| i.label.as_str() == "Parser").unwrap();
        assert_eq!(parser.insert.as_str(), "scip:Parser");
        assert_eq!(parser.detail.as_deref(), Some("code:type · parse.rs"));
        // A spec node carries the "spec" detail.
        let spec = items.iter().find(|i| i.label.as_str() == "parser-spec").unwrap();
        assert_eq!(spec.detail.as_deref(), Some("spec"));
    }

    /// The result list is capped to `limit`.
    #[test]
    fn entity_find_respects_limit() {
        let nodes: Vec<EntityNode> = (0..20)
            .map(|i| node(&format!("id{i}"), &format!("foo{i}"), "code:function", "f.rs"))
            .collect();
        let src = EntityNodeFindSource::new(&nodes);
        assert_eq!(src.candidates("foo", 5).len(), 5);
    }

    /// Vault find: a basename hit outranks a deep-path-only hit, and `insert`
    /// carries the full rel-path while the label is the basename.
    #[test]
    fn vault_find_basename_beats_folder_and_carries_path() {
        let paths = vec![
            "notes/architecture.md".to_string(),
            "arch-team/meeting.md".to_string(),
        ];
        let src = VaultNodeFindSource::new(&paths);
        let items = src.candidates("arch", 10);
        // The note literally named `architecture` ranks first over the
        // `arch-team/` folder hit.
        assert_eq!(items[0].label.as_str(), "architecture");
        assert_eq!(items[0].insert.as_str(), "notes/architecture.md");
        assert_eq!(items[0].detail.as_deref(), Some("notes"));
    }
}
