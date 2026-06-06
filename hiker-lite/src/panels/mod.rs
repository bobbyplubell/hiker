//! Side-panel widgets for the lite shell: fuzzy file search, the vault file
//! tree, find-and-replace across the active buffer, and a hex viewer for
//! binary files. Each panel is a self-contained egui widget the workbench
//! mounts into a dock region.

pub mod file_search;
pub mod filetree;
pub mod find_replace;
pub mod hex_view;
