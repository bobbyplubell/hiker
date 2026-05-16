//! Higher-level reshape + ops-framework operations. Each submodule
//! contains `impl Trees` methods grouped by their op family.
//!
//! All SQL goes through the helpers re-exported by `super::storage`; no
//! file in this directory imports `rusqlite::*` directly.

pub(super) mod drop;
pub(super) mod edit;
pub(super) mod folder_rename;
pub(super) mod merge;
pub(super) mod move_node;
pub(super) mod rollup;
pub(super) mod split;
pub(super) mod summarize;
