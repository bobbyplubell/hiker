//! The typed node/edge graph model: authored (vault) + derived (source) nodes in one graph.
//! `node-model-authored-vs-derived`, `node-kind-vs-origin`, `typed-edges`.

/// Identifier of a bound source — e.g. a repo's portable, git-derived id (`repo-id-git-derived`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

/// A handle to a node within a source — e.g. a SCIP symbol moniker. Opaque to the engine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeHandle {
    pub source: SourceId,
    pub id: String,
}

/// Where a node came from. Independent of `kind`: a `kind: epic` may be authored
/// (vault) or derived (a read-only Jira mirror). See `node-kind-vs-origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Created/edited/deleted in the vault (frontmatter `kind`). Mutable.
    Authored,
    /// Surfaced read-only by a [`crate::DerivedNodeSource`]. Heals/drifts with its source.
    Derived,
}

/// A node in the one typed graph.
#[derive(Debug, Clone)]
pub struct Node {
    pub handle: NodeHandle,
    /// "spec" | "epic" | "story" | "sprint" | "code:function" | "code:class" | …
    pub kind: String,
    pub origin: Origin,
    pub name: String,
}

/// A typed edge `(from, relation, to)`. Invariant (`typed-edges`): an **authored** edge always
/// originates from an authored node — enforced at insert time by the engine, not here.
#[derive(Debug, Clone)]
pub struct Edge {
    pub from: NodeHandle,
    /// "implements" | "parent" | "belongs_to" | "calls" | "touches" | …
    pub relation: String,
    pub to: NodeHandle,
}

/// Kinds of derived edge a source can expose, for [`crate::DerivedNodeSource::neighbors`] filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Function/method call (SCIP: reference occurrence by range containment).
    Calls,
    /// Type reference (extends/implements/field type).
    TypeRef,
    /// Import/use.
    Imports,
    /// Interface/trait implementation (SCIP: `relationships[].is_implementation`).
    Implements,
}
