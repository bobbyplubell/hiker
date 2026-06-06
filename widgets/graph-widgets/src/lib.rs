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
//!
//! The tree/radial position wrappers live here at the crate root rather than in
//! their own module: each is a one-line conversion over the matching
//! `hiker_graph` function, too small to form a cohesive sibling module.

use eframe::egui::Vec2;
use hiker_graph::LayoutTree;

pub mod force_layout;

#[inline]
fn to_egui(positions: Vec<hiker_graph::Vec2>) -> Vec<Vec2> {
    positions.into_iter().map(|p| Vec2::new(p.x, p.y)).collect()
}

/// Radial (one ring per tree depth) layout — see
/// [`hiker_graph::radial_positions`].
pub fn radial_positions(tree: &LayoutTree, area: f32) -> Vec<Vec2> {
    to_egui(hiker_graph::radial_positions(tree, area))
}

/// Top-down tree layout — see [`hiker_graph::vertical_tree_positions`].
pub fn vertical_tree_positions(tree: &LayoutTree, area: f32) -> Vec<Vec2> {
    to_egui(hiker_graph::vertical_tree_positions(tree, area))
}

/// Left-to-right tree layout — see [`hiker_graph::horizontal_tree_positions`].
pub fn horizontal_tree_positions(tree: &LayoutTree, area: f32) -> Vec<Vec2> {
    to_egui(hiker_graph::horizontal_tree_positions(tree, area))
}
