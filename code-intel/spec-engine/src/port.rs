//! The `DerivedNodeSource` port — spec-engine's only backend dependency (`derived-node-source-port`).
//!
//! Source-neutral by design: nothing here is code-specific. Code (SCIP/LSP), infra, … are
//! interchangeable impls. Generality lives in the graph model (`model.rs`), not in this trait;
//! keep it minimal and push domain richness into node `kind` / edge `relation` / attributes.

use crate::model::{EdgeKind, NodeHandle, Resolution, SourceId};

/// Where a derived node lives in its source: file:line for code, but generalizes to URL / row / ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLoc {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Opaque change-fingerprint for drift (`drift-fingerprint`). For code: a structural hash of the
/// definition range. The engine stores it at link time and recompares on reindex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint(pub String);

/// What a source supports — drives reliable-or-absent + capability tiers. A thin source (ctags)
/// may have `resolution` only; SCIP/LSP add `blast_radius` + `drift`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SourceCaps {
    pub resolution: bool,
    pub stable_identity: bool,
    pub drift: bool,
    /// Call/reference blast radius (always available from occurrences).
    pub blast_radius: bool,
    /// Implementation / interface fan-out available. Verified to be indexer-dependent:
    /// scip-python populates `relationships`, rust-analyzer does not (would need moniker recovery).
    pub implementations: bool,
}

/// Read-only, external, graph-shaped source of derived nodes + edges.
///
/// The one backend abstraction the spec-engine depends on. The code project (a repo) is the
/// boundary object: a loader/indexer is its write side, this port is its read side.
pub trait DerivedNodeSource {
    /// Fuzzy-resolve a typed name (or a full id) to a node handle within `scope`.
    fn resolve(&self, query: &str, scope: &SourceId) -> Option<NodeHandle>;
    /// Location of a node's definition (for navigation).
    fn locate(&self, h: &NodeHandle) -> Option<SourceLoc>;
    /// Source text of a node (for preview / highlight / fingerprinting input).
    fn content(&self, h: &NodeHandle) -> Option<String>;
    /// Change-fingerprint of a node (for drift) at `Code` resolution — the symbol's own definition.
    fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint>;
    /// Change-fingerprint at a C4 `resolution` (`spec-resolution-c4`): `Code` = the symbol body;
    /// coarser levels hash structure / API so finer churn doesn't drift. Default ignores resolution
    /// and falls back to [`Self::fingerprint`], so backends opt in incrementally.
    fn fingerprint_at(&self, h: &NodeHandle, _resolution: Resolution) -> Option<Fingerprint> {
        self.fingerprint(h)
    }
    /// Neighbors of a node, filtered to the requested edge kinds (blast radius).
    fn neighbors(&self, h: &NodeHandle, kinds: &[EdgeKind]) -> Vec<NodeHandle>;
    /// What this source supports.
    fn capabilities(&self) -> SourceCaps;
}
