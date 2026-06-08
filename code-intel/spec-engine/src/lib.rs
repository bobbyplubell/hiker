//! spec-engine — the backend-agnostic typed node/edge graph + the `DerivedNodeSource` port.
//!
//! The orthogonal core: one typed graph linking **authored** vault nodes (specs, epics,
//! stories…) to **derived** source nodes (code symbols, Jira issues…). It depends only on the
//! [`DerivedNodeSource`] port — never on SCIP/LSP/sem directly.
//!
//! See `docs/spec-engine.md`.

pub mod link_store;
pub mod model;
pub mod port;

pub use link_store::{DriftReport, Link, LinkStore};
pub use model::{Edge, EdgeKind, Node, NodeHandle, Origin, SourceId};
pub use port::{DerivedNodeSource, Fingerprint, SourceCaps, SourceLoc};
