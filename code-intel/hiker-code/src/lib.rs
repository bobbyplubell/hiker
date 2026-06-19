//! hiker-code — code intelligence: the SCIP-consumer `DerivedNodeSource` adapter + (later) LSP,
//! plus code tooling. The pure adapter holds no hiker-UI dependency, so the standalone
//! spec-engine CLI can use it. See `docs/hiker-code.md`.
//!
//! Next: `scip_adapter` — parse a `.scip`, build the graph (range-containment for call edges,
//! `relationships` for impl/type edges), implement `DerivedNodeSource`.

pub mod churn;
pub mod governance;
pub mod scip_adapter;
pub mod seeds;
pub use churn::{churn_report, churn_window, ChurnReport, CommitChurn, FileChurn, SpecChurn};
pub use governance::{GovState, Governance};
pub use scip_adapter::{
    collapse, crate_qualified_sym, index_short_forms, short_sym, symbol_changed_vs, CodeGraph,
    CollapsedGraph, GraphNode, ScipAdapter,
};
pub use seeds::{comment_seeds, CommentCrawl, CommentSeed};
