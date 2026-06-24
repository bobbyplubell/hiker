//! Per-tab-kind body renderers. Each module renders the *content* of one
//! tab kind; the dock tab strip + close button + title come from
//! `crate::dock::viewer`. Buffer-scoped chrome (editor toolbar, status
//! bar) lives inside `buffer::show` since it's only meaningful when a
//! buffer is the active tab.

pub mod buffer;
pub mod command_palette;
pub mod cluster_graph;
pub mod cluster_thumbnail;
pub mod code_governance;
pub mod code_graph;
pub mod entity_graph;
pub mod git_diff;
pub mod project_config;
pub mod board;
pub mod board_close;
pub mod board_metrics;
pub mod board_picker;
pub mod boards_index;
pub mod canvas;
pub mod charts_tab;
pub mod graph;
pub mod graph_data;
pub mod graph_find;
pub mod graph_nav;
pub mod graph_spec;
pub mod home;
pub mod indexer_detail;
pub mod link_graph_preview;
pub mod patch_review;
pub mod properties;
pub mod queue;
pub mod rules;
pub mod settings;
pub mod zim;
