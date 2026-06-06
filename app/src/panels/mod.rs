//! Per-tab-kind body renderers. Each module renders the *content* of one
//! tab kind; the dock tab strip + close button + title come from
//! `crate::dock::viewer`. Buffer-scoped chrome (editor toolbar, status
//! bar) lives inside `buffer::show` since it's only meaningful when a
//! buffer is the active tab.

pub mod buffer;
pub mod changes;
pub mod command_palette;
pub mod cluster_graph;
pub mod cluster_thumbnail;
pub mod board;
pub mod boards_index;
pub mod canvas;
pub mod graph;
pub mod home;
pub mod indexer_detail;
pub mod patch_review;
pub mod properties;
pub mod queue;
pub mod settings;
pub mod sync;
pub mod zim;
