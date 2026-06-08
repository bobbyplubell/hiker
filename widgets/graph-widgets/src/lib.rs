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
use hiker_graph::{
    GraphInput, LayeredEngine, LayoutEngine, LayoutTree, RankDir,
};

pub mod force_layout;

#[inline]
fn to_egui(positions: Vec<hiker_graph::Vec2>) -> Vec<Vec2> {
    positions.into_iter().map(|p| Vec2::new(p.x, p.y)).collect()
}

/// Result of a layered (dagre/Sugiyama) layout, with everything converted to
/// `egui::Vec2` at the crate boundary: node positions, the poly-line route for
/// each edge (aligned to the input `edges` order), and the laid-out bounds.
pub struct LayeredResult {
    pub positions: Vec<Vec2>,
    pub edge_routes: Vec<Vec<Vec2>>,
    pub size: Vec2,
}

/// Dagre layered (Sugiyama) layout — see [`hiker_graph::LayeredEngine`]. Builds
/// the engine with the default separations and node size, runs it over a
/// directed `GraphInput`, and converts the vector-free output into
/// `egui::Vec2`. The graph is treated as directed (layered layout is inherently
/// directed); no cluster parents or edge labels are supplied here.
pub fn layered_layout(
    node_count: usize,
    edges: &[(u32, u32)],
    node_sizes: Option<&[Vec2]>,
    rankdir: RankDir,
) -> LayeredResult {
    let sizes: Option<Vec<hiker_graph::Vec2>> =
        node_sizes.map(|s| s.iter().map(|v| hiker_graph::Vec2::new(v.x, v.y)).collect());
    let engine = LayeredEngine {
        rankdir,
        ..LayeredEngine::default()
    };
    let out = engine.layout(&GraphInput {
        node_count,
        edges,
        node_sizes: sizes.as_deref(),
        edge_label_sizes: None,
        node_parents: None,
        directed: true,
    });
    let edge_routes = out
        .edge_routes
        .into_iter()
        .map(|route| route.into_iter().map(|p| Vec2::new(p.x, p.y)).collect())
        .collect();
    LayeredResult {
        positions: to_egui(out.positions),
        edge_routes,
        size: Vec2::new(out.size.x, out.size.y),
    }
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
