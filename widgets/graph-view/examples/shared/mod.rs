//! Shared synthetic fixture for the graph-view projection examples.
//!
//! `#[path]`-included by both `snapshot.rs` (headless PNG) and `demo.rs`
//! (interactive eframe) so they render the *same* clustered graph with the
//! *same* deterministic positions — the snapshots and the live demo then agree.
//! Depends only on `hiker_graph_view` + `egui`.

#![allow(dead_code)] // each example uses a subset of these helpers.

use hiker_graph_view::graph_view::{
    LayoutConfig, NodeDescriptor, NodeShape, Palette, Source, Style,
};
use hiker_graph::{LayoutKind, LayoutTree};

/// One central cluster plus a ring of `RING` clusters, each `PER_CLUSTER` nodes.
const RING: usize = 5;
const CLUSTERS: usize = RING + 1; // +1 central cluster at the focus.
const PER_CLUSTER: usize = 12;

/// A clustered synthetic graph: a central cluster at the origin (so the lens
/// magnifies it) ringed by `RING` outer clusters (which the lens compresses),
/// densely linked within a cluster and sparsely between clusters, with fixed
/// (deterministic) positions so every render is reproducible.
pub struct SyntheticGraph {
    positions: Vec<egui::Vec2>,
    edges: Vec<(u32, u32)>,
}

impl SyntheticGraph {
    pub fn new() -> Self {
        let mut positions = Vec::with_capacity(CLUSTERS * PER_CLUSTER);
        let mut edges = Vec::new();

        // Deterministic LCG — no rng dependency, identical across runs.
        let mut state: u64 = 0xDEAD_BEEF_1234_5678;
        let mut rnd = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as u32) as f32 / (u32::MAX as f32)
        };

        // Cluster 0 sits at the origin (the focus); clusters 1..=RING spread on
        // a ring so the lens has clear central + off-centre structure to warp.
        let radius = 320.0;
        for c in 0..CLUSTERS {
            let (cx, cy) = if c == 0 {
                (0.0, 0.0)
            } else {
                let ang = (c - 1) as f32 / RING as f32 * std::f32::consts::TAU;
                (ang.cos() * radius, ang.sin() * radius)
            };
            let base = (c * PER_CLUSTER) as u32;
            for i in 0..PER_CLUSTER {
                // Tight blob around the cluster centre.
                let jx = (rnd() - 0.5) * 120.0;
                let jy = (rnd() - 0.5) * 120.0;
                positions.push(egui::vec2(cx + jx, cy + jy));
                // Intra-cluster edges: chain + a couple of cross links.
                if i > 0 {
                    edges.push((base + i as u32 - 1, base + i as u32));
                }
                if i > 2 {
                    edges.push((base + i as u32 - 3, base + i as u32));
                }
            }
        }

        // Spokes: central cluster hub → each ring cluster hub.
        for c in 1..CLUSTERS {
            edges.push((0, (c * PER_CLUSTER) as u32));
        }
        // Ring: consecutive ring-cluster hubs, closing the loop.
        for c in 1..CLUSTERS {
            let next = if c + 1 < CLUSTERS { c + 1 } else { 1 };
            edges.push(((c * PER_CLUSTER) as u32, (next * PER_CLUSTER) as u32));
        }

        Self { positions, edges }
    }

    /// The deterministic node positions — assign these straight to
    /// `State::positions` so no layout worker runs (fully reproducible).
    pub fn positions(&self) -> Vec<egui::Vec2> {
        self.positions.clone()
    }
}

impl Source for SyntheticGraph {
    fn node_count(&self) -> usize {
        self.positions.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let (fill, active) = match style.palette {
            Palette::Flat { node, active } => (node, active),
            Palette::Policy { cluster, leaf, .. } => (leaf, cluster),
        };
        positions
            .iter()
            .enumerate()
            .map(|(index, &world_pos)| {
                // First node of each cluster gets the accent colour + bigger
                // radius so clusters read as hubs.
                let is_hub = index % PER_CLUSTER == 0;
                NodeDescriptor {
                    index,
                    world_pos,
                    radius: if is_hub { 7.0 } else { 4.0 },
                    shape: NodeShape::Circle,
                    fill: if is_hub { active } else { fill },
                    resting_stroke: egui::Stroke::NONE,
                    hover_stroke: egui::Stroke::new(2.0, active),
                    label: None,
                    label_min_zoom: 0.0,
                    click_path: None,
                    tooltip: None,
                }
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.edges.clone()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        // Not used — the examples assign deterministic positions directly and
        // never trigger a tree layout.
        LayoutTree {
            n: 0,
            children: Vec::new(),
            roots: Vec::new(),
            depth: Vec::new(),
            subtree_leaves: Vec::new(),
        }
    }

    fn preview_for(&self, _index: usize) -> Option<(String, String)> {
        None
    }
}

/// World-space layout sizing (unused by the examples since positions are fixed,
/// but `recompute_layout` would want it).
pub const fn layout_config() -> LayoutConfig {
    LayoutConfig {
        area: 800.0,
        seed_box: 800.0,
    }
}

/// A small hardcoded directed acyclic graph (a shallow hierarchy with a couple
/// of cross-rank edges) used to exercise the [`LayoutKind::Layered`] layout.
/// Positions are produced by the real layered engine via
/// `State::recompute_layout`, so this fixture only supplies topology.
///
/// ```text
///            0
///          / | \
///         1  2  3
///        /|     |\
///       4 5     6 7
///        \ \   / /
///          \ 8 /
/// ```
pub struct LayeredGraph {
    n: usize,
    edges: Vec<(u32, u32)>,
}

impl LayeredGraph {
    pub fn new() -> Self {
        let edges = vec![
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 4),
            (1, 5),
            (3, 6),
            (3, 7),
            (4, 8),
            (5, 8),
            (6, 8),
            (7, 8),
            (2, 8),
        ];
        Self { n: 9, edges }
    }
}

impl Source for LayeredGraph {
    fn node_count(&self) -> usize {
        self.n
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let (fill, active) = match style.palette {
            Palette::Flat { node, active } => (node, active),
            Palette::Policy { cluster, leaf, .. } => (leaf, cluster),
        };
        positions
            .iter()
            .enumerate()
            .map(|(index, &world_pos)| NodeDescriptor {
                index,
                world_pos,
                radius: 14.0,
                shape: NodeShape::Circle,
                fill: if index == 0 { active } else { fill },
                resting_stroke: egui::Stroke::NONE,
                hover_stroke: egui::Stroke::new(2.0, active),
                label: None,
                label_min_zoom: 0.0,
                click_path: None,
                tooltip: None,
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.edges.clone()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        // Unused: the layered layout reads node_count + edges, not a tree.
        LayoutTree {
            n: 0,
            children: Vec::new(),
            roots: Vec::new(),
            depth: Vec::new(),
            subtree_leaves: Vec::new(),
        }
    }

    fn preview_for(&self, _index: usize) -> Option<(String, String)> {
        None
    }
}
