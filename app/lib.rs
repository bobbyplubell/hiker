//! Library facade over the hiker desktop app's module tree.
//!
//! The app ships as the `hiker` binary (`src/main.rs`), which owns the eframe
//! entry point and the `HikerApp` eframe wiring. This library target exists so
//! *other workspace crates* — specifically the headless `tools/profile-buffer`
//! performance profiler — can drive the app's real internal code paths (notably
//! `panels::buffer::decorations::rebuild_editor_layers`) without reimplementing
//! them. It re-declares the same module tree as a library target; Cargo compiles
//! the bin and lib as independent crates, so the binary is unaffected. Only the
//! profiler links against this lib.
//!
//! Why this file lives at `app/lib.rs` (outside `src/`): the repo's structural
//! lint scripts (`scripts/check-splits.py`, `check-lengths.py`) scan `app/src`.
//! A `src/lib.rs` would (a) make `app/src` a module-root, retroactively flagging
//! every top-level `app/src/*.rs` as a "sibling-only shard" against this new
//! root, and (b) be subject to the public-surface-density rule. Sitting beside
//! `src/` (referenced via `#[path]`) keeps it out of those scans entirely, so
//! the bin's existing structure is unchanged. The lib is wired up in
//! `app/Cargo.toml`'s `[lib] path`.
//!
//! Visibility posture: only the modules the profiler actually reaches are `pub`
//! (`buffer`, and the `panels::buffer::{decorations, widgets}` path); every
//! other module stays crate-internal (`pub(crate)`) so the app's existing
//! crate-private item names aren't promoted into a public API surface (which
//! would trip pedantic lints like `module_name_repetitions` on app code this
//! tool has no business touching).

// The app's modules carry binary-shaped dead-code (UI verbs reached only at
// runtime via egui, fields populated by the eframe loop) that reads as unused
// when the tree is compiled as a library with no binary entry point pulling on
// it. The bin target is the real consumer and is warning-clean; silence the
// lib-only dead-code noise here so the profiler build stays quiet.
#![allow(dead_code)]

#[path = "src/actions.rs"]
pub(crate) mod actions;
#[path = "src/activity/mod.rs"]
pub(crate) mod activity;
#[path = "src/appears_in/mod.rs"]
pub(crate) mod appears_in;
#[path = "src/autocomplete/mod.rs"]
pub(crate) mod autocomplete;
#[path = "src/backlinks/mod.rs"]
pub(crate) mod backlinks;
#[path = "src/buffer.rs"]
pub mod buffer;
#[path = "src/buffer_view.rs"]
pub(crate) mod buffer_view;
#[path = "src/bootstrap.rs"]
pub(crate) mod bootstrap;
#[path = "src/canvas_activity/mod.rs"]
pub(crate) mod canvas_activity;
#[path = "src/projects_activity/mod.rs"]
pub(crate) mod projects_activity;
#[path = "src/spec_panel.rs"]
pub(crate) mod spec_panel;
#[path = "src/code_sources.rs"]
pub(crate) mod code_sources;
#[path = "src/charts.rs"]
pub(crate) mod charts;
#[path = "src/clusters/mod.rs"]
pub(crate) mod clusters;
#[path = "src/command_center.rs"]
pub(crate) mod command_center;
#[path = "src/completion_sources.rs"]
pub(crate) mod completion_sources;
#[path = "src/context/mod.rs"]
pub(crate) mod context;
#[path = "src/editor_pane.rs"]
pub(crate) mod editor_pane;
#[path = "src/files/mod.rs"]
pub(crate) mod files;
#[path = "src/icons.rs"]
pub(crate) mod icons;
#[path = "src/item_menu.rs"]
pub(crate) mod item_menu;
#[path = "src/keybinds.rs"]
pub(crate) mod keybinds;
#[path = "src/os_open.rs"]
pub(crate) mod os_open;
#[path = "src/panels/mod.rs"]
pub mod panels;
#[path = "src/git_sync/mod.rs"]
pub(crate) mod git_sync;
#[path = "src/profiling.rs"]
pub(crate) mod profiling;
#[path = "src/related/mod.rs"]
pub(crate) mod related;
#[path = "src/search/mod.rs"]
pub(crate) mod search;
#[path = "src/side_panel_persist.rs"]
pub(crate) mod side_panel_persist;
#[path = "src/sidebar/mod.rs"]
pub(crate) mod sidebar;
#[path = "src/source_control/mod.rs"]
pub(crate) mod source_control;
#[path = "src/state.rs"]
pub(crate) mod state;
#[path = "src/tab.rs"]
pub(crate) mod tab;
#[path = "src/titlebar.rs"]
pub(crate) mod titlebar;
#[path = "src/toolbar.rs"]
pub(crate) mod toolbar;
#[path = "src/trails/mod.rs"]
pub(crate) mod trails;
#[path = "src/trash/mod.rs"]
pub(crate) mod trash;
#[path = "src/vault_view/mod.rs"]
pub(crate) mod vault_view;
#[path = "src/widgets/mod.rs"]
pub(crate) mod widgets;
#[path = "src/workbench_host.rs"]
pub(crate) mod workbench_host;
