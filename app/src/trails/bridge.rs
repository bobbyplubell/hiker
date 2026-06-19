//! Sync-UI → async-core bridge for the trails sidebar.
//!
//! The trails sidebar renders synchronously (egui frame loop) but every
//! trail mutation is an async `core::trails` verb that writes the
//! trail-doc / waypoint-notes on disk and re-indexes. This module mirrors
//! `crate::panels::board`'s pattern exactly: each verb clones the needed
//! service handles off the narrow `activity::SurfaceCtx`, builds an owned future,
//! and drives it to completion on the current tokio runtime via
//! `Handle::try_current().block_on(...)` — the verbs hold a `!Send`
//! `&mut Store` / vault handle internally, so `block_on` on the UI thread
//! (not `spawn`) is the right shape. The next frame re-reads the trail
//! from disk through `core::trails::get_trail`.
//!
//! Read-only listing / detail (`list`, `get_trail`, `containing_note`)
//! are plain sync core calls and are issued inline at the render site.

use hiker_core::errors::HikerError;
use hiker_core::trails::ops as tops;
use hiker_core::trails::{self, TrailDetail, TrailListItem};

use crate::activity::SurfaceCtx;

/// Drive an owned trails-op future to completion on the current tokio
/// runtime (entered by the egui frame loop). Mirrors `board::run`.
fn run<T, F>(fut: F) -> Result<T, HikerError>
where
    F: std::future::Future<Output = Result<T, HikerError>>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(fut),
        Err(_) => Err(HikerError::Io("no tokio runtime".into())),
    }
}

/// Vault-relative path of the active trail-doc, read from
/// `vault.active_trail` config. `None` when no trail is active or the
/// config lock is poisoned.
pub fn active_trail_rel(ctx: &SurfaceCtx<'_>) -> Option<String> {
    ctx.config.read().ok()?.vault.active_trail.clone()
}

/// Enumerate every trail-doc in the vault (recency unsorted). Empty on a
/// store-lock failure. Read-only; safe to call each frame.
pub fn list(ctx: &SurfaceCtx<'_>) -> Vec<TrailListItem> {
    let Ok(store) = ctx.services.read_store.lock() else {
        return Vec::new();
    };
    trails::list(ctx.vault, &store, &ctx.services.layered).unwrap_or_default()
}

/// Fetch the full detail bundle for `trail_doc_rel` (ordered, resolved
/// waypoints + append cursor + body). `None` on any read/parse error.
pub fn get_trail(ctx: &SurfaceCtx<'_>, trail_doc_rel: &str) -> Option<TrailDetail> {
    let store = ctx.services.read_store.lock().ok()?;
    trails::get_trail(ctx.vault, &store, &ctx.services.layered, trail_doc_rel).ok()
}

/// Pre-compute the cascade size for a remove-waypoint confirm dialog
/// (count includes the target itself). 1 on any error so the confirm
/// still reads sensibly.
pub fn descendant_count(ctx: &SurfaceCtx<'_>, trail_doc_rel: &str, waypoint_path: &str) -> u32 {
    tops::descendant_count(ctx.vault, trail_doc_rel, waypoint_path).unwrap_or(1)
}

/// Create a new trail (default-named, default placement) and return its
/// trail-doc rel-path on success.
pub fn create_trail(ctx: &mut SurfaceCtx<'_>, name: &str) -> Result<String, HikerError> {
    let watcher = ctx.services.watcher.clone();
    let jobs = ctx.services.indexer.job_sender();
    let log = ctx.services.layered.clone();
    let vault = ctx.vault.clone();
    let trails_cfg = ctx
        .config
        .read()
        .map(|c| c.trails.clone())
        .unwrap_or_default();
    let name = name.to_string();
    run(async move {
        tops::create_trail(&watcher, &jobs, &log, &vault, &trails_cfg, &name)
            .await
            .map(|o| o.trail_doc_rel)
    })
}

/// Set (or, with `None`, clear) the trail-doc's append cursor.
pub fn set_append_cursor(
    ctx: &mut SurfaceCtx<'_>,
    trail_doc_rel: &str,
    waypoint_path: Option<&str>,
) -> Result<(), HikerError> {
    let watcher = ctx.services.watcher.clone();
    let jobs = ctx.services.indexer.job_sender();
    let vault = ctx.vault.clone();
    let (trail_doc_rel, waypoint_path) =
        (trail_doc_rel.to_string(), waypoint_path.map(str::to_string));
    run(async move {
        tops::set_append_cursor(&watcher, &jobs, &vault, &trail_doc_rel, waypoint_path.as_deref())
            .await
    })
}

/// Delete a whole trail (trail-doc + companion folder cascade).
pub fn delete_trail(ctx: &mut SurfaceCtx<'_>, trail_doc_rel: &str) -> Result<(), HikerError> {
    let watcher = ctx.services.watcher.clone();
    let jobs = ctx.services.indexer.job_sender();
    let log = ctx.services.layered.clone();
    let vault = ctx.vault.clone();
    let trash = hiker_core::trash::Trash::open(ctx.vault.root());
    let trail_doc_rel = trail_doc_rel.to_string();
    run(async move {
        tops::delete_trail(&watcher, &jobs, &log, &vault, &trash, &trail_doc_rel)
            .await
            .map(|_| ())
    })
}

/// Stamp `hiker.last_activated_at = now` on a trail-doc (the activation
/// recency the dropdown orders by).
pub fn stamp_activated(ctx: &mut SurfaceCtx<'_>, trail_doc_rel: &str) -> Result<(), HikerError> {
    let watcher = ctx.services.watcher.clone();
    let jobs = ctx.services.indexer.job_sender();
    let vault = ctx.vault.clone();
    let trail_doc_rel = trail_doc_rel.to_string();
    run(async move { tops::stamp_last_activated_at(&watcher, &jobs, &vault, &trail_doc_rel).await })
}
