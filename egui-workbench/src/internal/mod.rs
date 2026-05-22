//! Internal helpers — not part of the public API.
//!
//! These modules bridge egui_tiles to the workbench's higher-level
//! concepts (handle indirection, pinned tab enforcement, focused-group
//! detection) and are subject to change without a major version bump.

pub(crate) mod tree_adapter;
