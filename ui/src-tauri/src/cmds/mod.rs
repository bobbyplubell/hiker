//! Tauri command implementations, split by feature surface. Each
//! submodule contains a cohesive group of `#[tauri::command]` functions
//! (and their command-shaped helpers / DTOs); shared state and the
//! `invoke_handler!` registration live in `crate::lib`.
//!
//! The split mirrors the existing slug taxonomy (`cluster-*`,
//! `staging-*`, `trail-*`, …) and the banner-comment seams that used to
//! divide `lib.rs`. See the parent module docs for the cross-module
//! visibility conventions (every `VaultSession` field commands need is
//! `pub(crate)`; helpers shared across submodules are `pub(crate)` on
//! `crate`).

pub(crate) mod activity;
pub(crate) mod autosave;
pub(crate) mod bootstrap;
pub(crate) mod changes;
pub(crate) mod cluster;
pub(crate) mod indexer;
pub(crate) mod mcp;
pub(crate) mod mutations;
pub(crate) mod queue;
pub(crate) mod search;
pub(crate) mod settings;
pub(crate) mod staging;
pub(crate) mod trails;
pub(crate) mod vault;
pub(crate) mod vault_home;
pub(crate) mod watcher_router;
