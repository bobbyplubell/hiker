//! Shared force/tree graph rendering engine, extracted from the hiker app so
//! the vault link-graph, the cluster-tree graph, and the `graph-snapshot`
//! tool can all drive one render path. The crate owns pan/zoom, layout (the
//! background force worker plus inline tree-position math), the view-options
//! menu, and the node/edge/label/hover/preview paint loop.
//!
//! A caller supplies a [`Source`](graph_view::source::Source) adapting its own
//! data into per-frame [`NodeDescriptor`](graph_view::source::NodeDescriptor)s
//! plus edge/layout-tree topology, and two host callbacks: the preview-card
//! painter passed to [`State::ui`](graph_view::State::ui) and the eye
//! [`egui::Image`] passed to
//! [`State::view_options_menu`](graph_view::State::view_options_menu). That
//! keeps the engine free of the app's icon registry and preview-card painter.

pub mod force_graph;
pub mod graph_view;
