//! egui adapter over the egui-agnostic `hiker-graph` layout crate.
//!
//! The layout engines (ForceAtlas2 background worker + tree/radial layouts)
//! live in `hiker-graph` (part of the `hiker-render` submodule) and carry
//! their own `Vec2` so they stay graphics-free. This crate is the thin egui
//! façade `hiker-app`'s panels and the headless `graph-snapshot` tool consume:
//! it re-exports the vector-free types unchanged and wraps the rest so callers
//! keep handing positions in and out as `egui::Vec2`, converting at the
//! boundary. Living in its own crate also keeps the call sites cross-crate (so
//! `clippy::single_call_fn` doesn't double-count them).

pub mod force_layout;
pub mod graph_layouts;
