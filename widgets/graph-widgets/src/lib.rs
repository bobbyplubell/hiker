//! Reusable graph-layout widgets shared by `hiker-app` and the headless
//! `graph-snapshot` tool. Lives in its own crate so call sites in
//! `hiker-app`'s panels and the snapshot binary are cross-crate (and
//! therefore exempt from `clippy::single_call_fn`), instead of the
//! older `#[path]`-include arrangement that fooled the lint into
//! double-counting.

pub mod force_layout;
pub mod graph_layouts;
