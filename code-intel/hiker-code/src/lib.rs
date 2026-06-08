//! hiker-code — code intelligence: the SCIP-consumer `DerivedNodeSource` adapter + (later) LSP,
//! plus code tooling. The pure adapter holds no hiker-UI dependency, so the standalone
//! spec-engine CLI can use it. See `docs/hiker-code.md`.
//!
//! Next: `scip_adapter` — parse a `.scip`, build the graph (range-containment for call edges,
//! `relationships` for impl/type edges), implement `DerivedNodeSource`.

pub mod scip_adapter;
pub use scip_adapter::{CodeGraph, GraphNode, ScipAdapter};
