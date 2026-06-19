//! The host-data contract between a graph consumer and the engine: the
//! [`Source`] trait a caller implements over its own storage, the per-frame
//! [`NodeDescriptor`]s it produces, the common [`Toggles`] and world-space
//! [`LayoutConfig`], the hover [`PreviewCache`], and the serde-free view
//! [`Snapshot`] the app persists across restarts.

use std::collections::HashMap;

use hiker_graph::{LayoutKind, LayoutTree};

use super::styling::Style;

/// View toggles common to every graph. Caller-specific toggles (the vault
/// "Orphans", the cluster "Leaves", the review tab's "Live preview") live on
/// the caller and are surfaced through the `extra_toggles` argument of
/// [`State::view_options_menu`](super::State::view_options_menu).
#[derive(Clone, Copy)]
pub struct Toggles {
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_preview: bool,
}

/// Hover-preview card text, refreshed only when the hovered node changes so
/// we don't re-read the note body every frame.
#[derive(Default)]
pub struct PreviewCache {
    pub(super) hovered_index: Option<usize>,
    pub(super) title: Option<String>,
    pub(super) body: Option<String>,
}

/// Per-node draw + hit-test descriptor produced by a [`Source`] each frame.
/// The caller computes `fill`/`radius`/`shape` from its own data and the
/// active [`Style`]; the engine never hardcodes a coloring scheme.
pub struct NodeDescriptor {
    /// Index into `positions` — also the hover/preview identity.
    pub index: usize,
    pub world_pos: egui::Vec2,
    /// Base radius in world units, before `node_scale`/zoom.
    pub radius: f32,
    pub shape: NodeShape,
    pub fill: egui::Color32,
    pub resting_stroke: egui::Stroke,
    pub hover_stroke: egui::Stroke,
    pub label: Option<String>,
    /// Labels draw only at or above this zoom (0.0 = always).
    pub label_min_zoom: f32,
    /// Multiplier on the base label font size, so high-level nodes (e.g. crates /
    /// modules) can render larger text than leaves. `1.0` = the base size.
    pub label_scale: f32,
    /// A small status-badge dot painted at the node's top-right in this color —
    /// a *mark* layered over the fill, not a recolor, so it stays legible
    /// whatever the fill encodes (the code graph flags planned/partial-spec
    /// nodes with it). Painter-drawn at the FULL LOD tier only; dots/markers
    /// are too small to carry a second mark. status: code-graph-status-badge
    pub badge: Option<egui::Color32>,
    /// The badge's top-LEFT twin — a second, independent mark channel so a node
    /// can carry both (the code graph puts open-bug counts here, status on the
    /// right shoulder). Same Painter/LOD treatment as [`Self::badge`].
    /// status: code-graph-bug-badge
    pub bug_badge: Option<egui::Color32>,
    /// `Some` makes the node clickable; the path is returned from
    /// [`State::ui`](super::State::ui) for the caller to open.
    pub click_path: Option<String>,
    /// Hover tooltip text (the cluster graph shows node names; the vault
    /// graph passes `None`).
    pub tooltip: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NodeShape {
    Circle,
    Square,
}

/// World-space layout sizing. The vault graph and cluster graph settled on
/// different scales (1000² vs 800² boxes), so each caller passes its own.
#[derive(Clone, Copy)]
pub struct LayoutConfig {
    /// Area handed to the tree layouts.
    pub area: f32,
    /// Full width of the random scatter box for the force seed.
    pub seed_box: f32,
}

/// The caller-supplied bridge from domain data to the engine. Vault and
/// cluster panels each implement it over their own storage.
pub trait Source {
    /// Total node count (length of the `positions` vector). Includes nodes
    /// the caller hides in [`Source::nodes`] (orphans / leaves) so edge and
    /// layout indices stay stable.
    fn node_count(&self) -> usize;

    /// Build the visible node descriptors for this frame. `positions` is the
    /// engine's current layout; the caller reads `positions[i]` for each node
    /// it emits and skips its own hidden nodes.
    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor>;

    /// Edges as `positions`-index pairs. Used both for drawing and as the
    /// force-worker topology.
    fn edges(&self) -> Vec<(u32, u32)>;

    /// Spanning/parent tree for a tree layout. The vault graph BFS/DFS-es a
    /// spanning tree per kind; the cluster graph uses its parent tree for
    /// all. Only called for non-force kinds.
    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree;

    /// `(title, body)` for the hover-preview card of node `index`. Called
    /// once per hover change. Returns `None` to suppress the card.
    fn preview_for(&self, index: usize) -> Option<(String, String)>;

    /// Per-edge stroke color for edge `index` (its position in
    /// [`Source::edges`]). The default `None` falls back to the style's
    /// single `edge_color` — the engine never hardcodes a kind→color scheme;
    /// a typed source (the vault graph's wikilink / board-membership /
    /// trail-membership edges) supplies its own per-kind hues.
    /// status: vault-graph-edge-toggles
    fn edge_color(&self, index: usize) -> Option<egui::Color32> {
        let _ = index;
        None
    }

    /// A stable per-node identity that survives a rebuild, used by the
    /// force-directed layout to map old node positions onto the new graph so a
    /// re-cluster / vault-rebuild *morphs* smoothly instead of reshuffling: a
    /// retained key keeps (and is anchored toward) its prior position; a node
    /// whose key is new settles in fresh.
    ///
    /// The default returns `None` for every node, which opts out entirely —
    /// the layout then falls back to today's fresh random scatter on every
    /// rebuild, byte-identical to the pre-anchor behaviour.
    fn node_key(&self, index: usize) -> Option<String> {
        let _ = index;
        None
    }
}

/// A plain, serde-free snapshot of a [`State`](super::State)'s persistable VIEW
/// (the bits the app rides on its tab-state store across restart): the warm-seed
/// node positions keyed by [`Source::node_key`], the affine pan/zoom, the
/// projection (as a string discriminant + its two scalars), the focus mode
/// (string discriminant), the common toggles, and the LOD thresholds. Everything
/// non-serializable or recomputed (the layout worker, edge routes, hover-preview
/// cache, fly-to, the `Mobius` nav, GPU handles) is intentionally absent — see
/// [`State::view_snapshot`](super::State::view_snapshot).
///
/// The discriminant strings keep `hiker_projection`'s `ProjectionKind`/`FocusMode`
/// off the app↔serde boundary, so neither this crate nor `hiker_projection` needs
/// serde. status: graph-view-state-persist
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// `node_key -> (world_x, world_y)`.
    pub positions: HashMap<String, (f32, f32)>,
    pub pan_x: f32,
    pub pan_y: f32,
    pub zoom: f32,
    /// `"affine"` | `"fisheye"` | `"poincare"`.
    pub projection_kind: String,
    pub projection_strength: f32,
    pub projection_size_falloff: f32,
    /// `"center"` | `"cursor"` | `"selection"`.
    pub focus_mode: String,
    pub show_labels: bool,
    pub show_edges: bool,
    pub show_preview: bool,
    pub lod_full_mag: f32,
    pub lod_marker_mag: f32,
}
