//! egui-facing adapter over `hiker_graph`'s pure tree / radial layouts.
//!
//! The layouts are egui-agnostic (they live in `hiker-graph` and produce
//! `hiker_graph::Vec2`). This module wraps the position functions so they
//! return `egui::Vec2`. The vector-free pieces (`LayoutKind`, `LayoutTree`,
//! `bfs_tree`, `dfs_tree`) are used unchanged — callers import them from
//! `hiker_graph` directly.

use eframe::egui::Vec2;
use hiker_graph::LayoutTree;

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
