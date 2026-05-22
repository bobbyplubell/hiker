//! editor-core: state, rope, transactions, selection. No rendering deps.
//!
//! Serialization is opt-in via the `serde` feature.

pub mod compartment;
pub mod sumtree;
pub mod rope;
pub mod change;
pub mod anchor;
pub mod selection;
pub mod rangeset;
pub mod decoration;
pub mod transaction;
pub mod history;
pub mod state;
pub mod diff;
pub mod theme;
pub mod facet;

