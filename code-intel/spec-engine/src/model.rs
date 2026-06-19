//! The typed node/edge graph model: authored (vault) + derived (source) nodes in one graph.
//! `node-model-authored-vs-derived`, `node-kind-vs-origin`, `typed-edges`.

use serde::{Deserialize, Serialize};

/// Identifier of a bound source — e.g. a repo's portable, git-derived id (`repo-id-git-derived`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

/// A handle to a node within a source — e.g. a SCIP symbol moniker. Opaque to the engine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeHandle {
    pub source: SourceId,
    pub id: String,
}

/// Where a node came from. Independent of `kind`: no kind is reserved to either
/// origin. See `node-kind-vs-origin`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// Function/method call (SCIP: reference occurrence by range containment).
    Calls,
    /// Type reference (extends/implements/field type).
    TypeRef,
    /// Import/use.
    Imports,
    /// Interface/trait implementation (SCIP: `relationships[].is_implementation`).
    Implements,
    /// A generic reference / hyperlink between non-code nodes — a neutral edge kind for sources
    /// whose relations aren't call/type/import/impl. Carries ZIM article hyperlinks today, and is
    /// the home for future wiki / doc cross-references (`<a href>`, issue links, …).
    Link,
}

/// C4-model resolution (`spec-resolution-c4`): the altitude at which a spec relates to code, and the
/// grain at which its drift is computed. Coarser = *less* sensitive (structure / API surface), finer =
/// catches body edits. Declared per spec (frontmatter); the project sets a default. One dial controls
/// both anchor coarsening and the [`crate::DerivedNodeSource::fingerprint_at`] level.
/// Ordered coarse → fine (`Context < Container < Component < Code`), so `min`/`max` clamp altitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    /// Product / user-facing behavior. Governs via children; effectively doesn't drift on code churn.
    Context,
    /// A crate / deployable unit. Drifts only when the crate's public symbol surface changes.
    Container,
    /// A module / type. Drifts on member add / remove / rename, not on body edits.
    Component,
    /// A single symbol (fn / method / field). Drifts on a real (AST-normalized) body change.
    #[default]
    Code,
}

impl Resolution {
    /// Parse a frontmatter token (`context`/`container`/`component`/`code`); unknown → `Code`.
    pub fn parse(s: &str) -> Resolution {
        match s.trim().to_ascii_lowercase().as_str() {
            "context" => Resolution::Context,
            "container" => Resolution::Container,
            "component" => Resolution::Component,
            _ => Resolution::Code,
        }
    }

    /// The resolution a link of `relation` drifts at, honoring the **relation floor**
    /// (`spec-resolution-c4`): `implements`/`verifies` make body-level claims, so they are always
    /// `Code` — a coarser dial on them would be a claim that refutes itself ("this implements the
    /// invariant, but only re-verify when the crate's symbol list changes"). The bug-row relations
    /// `manifests-in`/`verifies-fix` (`tracker-relation-links`) are body-level claims too — "this
    /// code carries this bug" / "this test vouches for the fix" — so they share the `Code` floor.
    /// `touches` is structural: it takes the spec's `declared` resolution (doc frontmatter /
    /// project default), clamped no finer than `Component`. This keeps "set everything coarse"
    /// out of reach for the relations that carry the guarantees; coarsening them requires
    /// demoting the relation itself, a visible semantic edit.
    pub fn for_relation(relation: &str, declared: Option<Resolution>) -> Resolution {
        match relation {
            "implements" | "verifies" | "manifests-in" | "verifies-fix" => Resolution::Code,
            _ => declared.unwrap_or(Resolution::Component).min(Resolution::Component),
        }
    }
}
