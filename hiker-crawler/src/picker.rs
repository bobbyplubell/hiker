//! The element picker model (`crawler-element-picker`).
//!
//! The bridge from "a live page" to "a selection set". A click hands the
//! engine a point; it returns a [`crate::engine::Hit`] with ranked selector
//! candidates and the node's HTML/text. The user labels each pick with a field
//! name (`title`, `body`, `date`, …); the set of `{ field, selector }` pairs is
//! the extractor spec every emit target consumes (`crawler-emit-targets`).

use serde::{Deserialize, Serialize};

/// One labelled selection: a field name bound to the chosen selector for it,
/// plus a sample of what it captured (for preview / LLM authoring context).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    /// The field name the captured value lands under (`title`, `body`, …).
    pub name: String,
    /// The chosen CSS selector (one of the hit's ranked candidates).
    pub selector: String,
    /// Whether the selector matches many nodes (a list/repeat field → drives
    /// hub/list crawl `next_urls`) rather than a single value.
    pub repeat: bool,
    /// A sample of the captured text, for the preview pane and LLM context.
    pub sample: String,
}

/// How the frontier is fed for this job (`crawler-link-strategy`). A per-job
/// choice that maps onto `extract.md`'s `crawl-modes`; hiker's frontier loop is
/// unchanged. See [`crate::emit::crawl_params`] for the mode/depth mapping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkStrategy {
    /// A frozen set of URLs — pasted or harvested by a repeat-selector pick
    /// over the JS-rendered page. Follow nothing (`crawl_mode: list, depth: 0`).
    StaticList,
    /// Follow links from the seed within scope patterns up to a depth cap
    /// (`crawl_mode: deep`).
    #[default]
    Dynamic,
    /// The chosen extractor owns discovery, emitting `next_urls`; scope only
    /// guards them.
    PluginDriven,
}

/// The full picked spec for a site: the page it was built against, the labelled
/// fields, and the link-following strategy. The deterministic input to every
/// emitter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selection {
    /// The URL the selection was authored against (the crawl seed).
    pub seed_url: String,
    /// The labelled fields, in author order.
    pub fields: Vec<Field>,
    /// How the frontier is fed (`crawler-link-strategy`).
    pub link: LinkStrategy,
}

impl Selection {
    /// A fresh selection anchored to `seed_url`.
    #[must_use]
    pub fn new(seed_url: impl Into<String>) -> Self {
        Self {
            seed_url: seed_url.into(),
            fields: Vec::new(),
            link: LinkStrategy::default(),
        }
    }

    /// Add a labelled field to the selection.
    pub fn push(&mut self, field: Field) {
        self.fields.push(field);
    }

    /// Whether any field is a repeat/list field — the signal that this site is
    /// a listing/hub page whose matches seed a crawl rather than a single clip.
    #[must_use]
    pub fn has_repeat(&self) -> bool {
        self.fields.iter().any(|f| f.repeat)
    }
}

#[cfg(test)]
mod tests {
    use super::{Field, LinkStrategy, Selection};

    fn field(name: &str, selector: &str, repeat: bool) -> Field {
        Field {
            name: name.to_owned(),
            selector: selector.to_owned(),
            repeat,
            sample: String::new(),
        }
    }

    #[test]
    fn new_anchors_seed_and_defaults_to_dynamic() {
        let sel = Selection::new("https://example.com");
        assert_eq!(sel.seed_url, "https://example.com");
        assert!(sel.fields.is_empty());
        assert_eq!(sel.link, LinkStrategy::Dynamic);
    }

    #[test]
    fn push_appends_fields_in_author_order() {
        let mut sel = Selection::new("https://example.com");
        sel.push(field("title", "h1", false));
        sel.push(field("body", ".content", false));
        assert_eq!(sel.fields.len(), 2);
        assert_eq!(sel.fields[0].name, "title");
        assert_eq!(sel.fields[1].selector, ".content");
    }

    #[test]
    fn has_repeat_is_true_when_any_field_repeats() {
        let mut sel = Selection::new("https://example.com");
        sel.push(field("title", "h1", false));
        assert!(!sel.has_repeat());
        sel.push(field("links", ".card a", true));
        assert!(sel.has_repeat());
    }
}
