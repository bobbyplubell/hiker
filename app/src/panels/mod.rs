//! Per-tab-kind body renderers. Each module renders the *content* of one
//! tab kind; the dock tab strip + close button + title come from
//! `crate::dock::viewer`. Buffer-scoped chrome (editor toolbar, status
//! bar) lives inside `buffer::show` since it's only meaningful when a
//! buffer is the active tab.

pub mod agent;
pub mod buffer;
pub mod changes;
pub mod cluster_graph;
pub mod cluster_review;
pub mod backlinks;
pub mod diff_view;
pub mod discovery_pane;
pub mod graph;
pub mod home;
pub mod indexer_detail;
pub mod patch_review;
pub mod plugins;
pub mod preview_common;
pub mod properties;
pub mod queue;
pub mod related;
pub mod search;
pub mod settings;
pub mod snapshot_preview;
pub mod staging_preview;
pub mod trash_preview;
