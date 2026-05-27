//! Buffer lifecycle helpers — open a file as a buffer tab, save, close.
//!
//! Mirrors the open/commit/dirty machinery from
//! `ui/src/app/openFile.ts` + `ui/src/app/editor.ts` collapsed into Rust:
//! single function for "open this rel path as a buffer tab," obeying the
//! preview-slot rule.

use crate::buffer::Buffer;
use crate::state::{nav_push, AppState, NavTarget, ToastLevel};
use crate::tab::{Tab, TabId, TabKind};

/// Load the live vault buffer for `rel` into the buffer map if it isn't already
/// cached. Returns `false` (after a toast) when the file can't be read. The
/// buffer opens on its disk text (= `materialize_accepted` when the doc is
/// seeded); the per-frame editor binding then keeps `editor.doc` equal to
/// `materialize_working` and renders pending agent ops as the suggestion
/// overlay (per `op-log-editor-binding`).
///
/// `pub(crate)` so non-tab hosts (e.g. the board pane's in-tab Markdown view,
/// `board-view-toggle`) can load the buffer before rendering the editor
/// widget inline without opening a separate buffer tab.
pub(crate) fn ensure_vault_buffer_loaded(state: &mut AppState, rel: &str) -> bool {
    if state.session.buffers.contains_key(rel) {
        return true;
    }
    match state.vault_session.vault.read_file_with_hash(rel) {
        Ok((contents, hash)) => {
            let cfg_guard = state.vault_session.config.read().ok();
            let buf = Buffer::with_config_and_vault(
                rel.to_string(),
                &contents,
                hash,
                cfg_guard.as_deref(),
                Some(state.vault_session.vault.clone()),
            );
            drop(cfg_guard);
            state.session.buffers.insert(rel.to_string(), buf);
            true
        }
        Err(err) => {
            state.push_toast(format!("Failed to open {}: {}", rel, err), ToastLevel::Error);
            false
        }
    }
}

/// Open the file at `rel` as a buffer tab. If `sticky`, the tab is created
/// sticky (Mod-click / "Keep open" / drag); otherwise it lands in the
/// preview slot, replacing any prior preview tab.
pub fn open_file(state: &mut AppState, rel: &str, sticky: bool) {
    // Navigation history: skip when we're already navigating via
    // back/forward (the index points at this entry already).
    if !state.session.nav.locked {
        nav_push(state, rel);
    }

    // If the path is already an open tab, focus it and (if it was preview
    // and the request is sticky) promote it.
    if let Some(existing_id) = state.find_buffer_tab(rel) {
        state.session.active_tab = Some(existing_id);
        if sticky && state.session.preview_tab == Some(existing_id) {
            state.promote_preview();
        }
        return;
    }

    // Load contents into memory if not already cached.
    if !ensure_vault_buffer_loaded(state, rel) {
        return;
    }

    // Replace preview slot if a non-sticky open and a preview exists.
    if !sticky {
        if let Some(prev_id) = state.session.preview_tab {
            // Swap the preview tab's kind/payload to the new path, keeping
            // the same id so dock positioning stays put.
            if let Some(tab) = state.tab_by_id_mut(prev_id) {
                tab.kind = TabKind::vault_buffer(rel.to_string());
                tab.sticky = false;
            }
            state.session.active_tab = Some(prev_id);
            return;
        }
    }

    // Only allocate a fresh tab id on the branch that actually keeps it;
    // the preview-reuse branch above returns without using one.
    let tab_id = state.next_tab_id();
    let tab = Tab {
        id: tab_id,
        kind: TabKind::vault_buffer(rel.to_string()),
        sticky,
    };
    state.session.tabs.push(tab);
    state.session.active_tab = Some(tab_id);
    if !sticky {
        state.session.preview_tab = Some(tab_id);
    }
}

impl AppState {
    fn find_buffer_tab(&self, rel: &str) -> Option<TabId> {
    self.session.tabs.iter().find_map(|t| {
        if t.kind.vault_path() == Some(rel) && t.kind.diff_source().is_none() {
            Some(t.id)
        } else {
            None
        }
    })
    }
}

/// Load a read-only preview buffer (snapshot blob / pending proposal /
/// trash entry) into `state.session.buffers` under its composite key.
/// Idempotent: re-calling for the same source is a no-op once loaded.
/// Returns the storage key callers use to look the buffer up later.
pub fn ensure_readonly_buffer_loaded(
    state: &mut AppState,
    source: &crate::tab::BufferSource,
) -> Option<String> {
    use crate::tab::BufferSource;
    let key = crate::buffer::buffer_key_for_source(source);
    if state.session.buffers.contains_key(&key) {
        return Some(key);
    }
    let contents = match source {
        BufferSource::Snapshot { op_id, path } => {
            // The version's content materialized from the op log at `op_id`.
            let log = state.vault_session.services.oplog.as_ref();
            hiker_core::ops::op_writes::content_at_op(log, path, op_id)
                .ok()
                .flatten()?
        }
        BufferSource::PendingProposal { proposal_id, target_path } => {
            // The proposal content is the op-log pending-op materialization:
            // `materialize(accepted + just this op)`. Read through the op-log
            // seam rather than a legacy pending store.
            let log = state.vault_session.services.oplog.as_ref();
            hiker_core::ops::op_writes::proposal_materializations(
                log,
                target_path,
                proposal_id,
            )
            .ok()
            .flatten()
            .map(|(_accepted, proposed)| proposed)?
        }
        BufferSource::Trash { trash_path, .. } => std::fs::read_to_string(trash_path).ok()?,
        BufferSource::Vault { .. } => return None,
    };
    let cfg_guard = state.vault_session.config.read().ok();
    // Read-only buffer fronting a non-vault `BufferSource` (snapshot blob,
    // pending proposal, trash entry). `read_only = true` no-ops editing
    // commands; the save path already short-circuits non-`Vault` sources.
    let buf = {
        let path = source.path().to_string();
        let hash = hiker_core::hash_string(&contents);
        let mut buf = Buffer::with_config_and_vault(
            path,
            &contents,
            hash,
            cfg_guard.as_deref(),
            Some(state.vault_session.vault.clone()),
        );
        buf.source = source.clone();
        buf.view.read_only = true;
        buf
    };
    drop(cfg_guard);
    state.session.buffers.insert(key.clone(), buf);
    Some(key)
}

/// Move the navigation cursor by `delta` (-1 = back, +1 = forward) and
/// re-open the buffer at that position. Sets `nav_locked` while running so
/// the resulting `open_file` doesn't push a new history entry.
///
/// `sticky = false` so back/forward behaves like a single click on the
/// path: an existing tab is focused as-is, otherwise the preview slot is
/// reused (or a new preview tab is opened). Previously this passed
/// `sticky = true`, which silently promoted the target to a sticky tab —
/// new buffers landed permanently after each Back press and the user
/// ended up with a strip full of regular tabs instead of preview reuse.
pub fn nav_go(state: &mut AppState, delta: i32) {
    let target = match delta.cmp(&0) {
        std::cmp::Ordering::Less => state.session.nav.back(),
        std::cmp::Ordering::Greater => state.session.nav.forward(),
        std::cmp::Ordering::Equal => None,
    };
    let Some(target) = target else { return };
    // `locked` so the restoration's `open_file` / tab swap doesn't push a new
    // nav entry on top of the one we just moved to.
    state.session.nav.locked = true;
    navigate_to(state, &target);
    state.session.nav.locked = false;
}

/// Restore a nav target into the active editor view.
fn navigate_to(state: &mut AppState, target: &NavTarget) {
    match target {
        NavTarget::File(path) => {
            // Backing out of a snapshot/preview we swapped into the active tab:
            // revert that tab in place rather than focusing / opening a separate
            // tab (so the round-trip lands back exactly where it started).
            if revert_active_preview_to_file(state, path) {
                return;
            }
            open_file(state, path, /* sticky */ false);
        }
        NavTarget::Snapshot { path, op_id } => {
            set_active_tab_kind(state, TabKind::snapshot_preview(path.clone(), op_id.clone()));
        }
    }
}

/// Open a historical snapshot in the *active* tab, in place, and record it on
/// the nav stack so Back returns to the live file. The active tab's content
/// swaps to the read-only snapshot view; the live buffer keeps its own
/// buffer-map key, so reverting (Back / "Live") is lossless.
pub fn open_snapshot_in_tab(state: &mut AppState, path: &str, op_id: &str) {
    if !state.session.nav.locked {
        state.session.nav.push(NavTarget::Snapshot {
            path: path.to_string(),
            op_id: op_id.to_string(),
        });
    }
    set_active_tab_kind(state, TabKind::snapshot_preview(path.to_string(), op_id.to_string()));
}

/// Swap the active tab back to the live vault buffer for `path`, in place, and
/// record it on the nav stack (the version-dropdown "Live" pick).
pub fn open_live_in_tab(state: &mut AppState, path: &str) {
    if !ensure_vault_buffer_loaded(state, path) {
        return;
    }
    if !state.session.nav.locked {
        state.session.nav.push(NavTarget::File(path.to_string()));
    }
    set_active_tab_kind(state, TabKind::vault_buffer(path.to_string()));
}

/// Swap the active tab's kind in place. No-op when there's no active tab.
fn set_active_tab_kind(state: &mut AppState, kind: TabKind) {
    if let Some(active) = state.session.active_tab
        && let Some(tab) = state.session.tabs.iter_mut().find(|t| t.id == active)
    {
        tab.kind = kind;
    }
}

/// If the active tab is a read-only preview (snapshot / proposal) of `path`,
/// revert it to the live vault buffer in place and return `true`. Used by
/// Back so leaving a snapshot lands back on the same tab's live buffer.
/// (`vault_path()` only matches `Vault` sources, so a snapshot tab is matched
/// by its buffer source's own path instead.)
fn revert_active_preview_to_file(state: &mut AppState, path: &str) -> bool {
    use crate::tab::BufferSource;
    let Some(active) = state.session.active_tab else { return false };
    // Immutable check first so the borrow is released before `ensure_*` takes
    // `&mut state`.
    let is_preview_of_path = state
        .session
        .tabs
        .iter()
        .find(|t| t.id == active)
        .is_some_and(|t| {
            matches!(
                &t.kind,
                TabKind::Editor { buffer, .. }
                    if buffer.path() == path && !matches!(buffer, BufferSource::Vault { .. })
            )
        });
    if !is_preview_of_path {
        return false;
    }
    // Make sure the live buffer exists before swapping the tab to it (it may
    // not if the preview was opened as a fresh tab from Home / Changes).
    if !ensure_vault_buffer_loaded(state, path) {
        return false;
    }
    set_active_tab_kind(state, TabKind::vault_buffer(path.to_string()));
    true
}

impl AppState {
    /// Promote the current preview tab to sticky.
    pub fn promote_preview(&mut self) {
    let Some(id) = self.session.preview_tab else { return };
    if let Some(tab) = self.tab_by_id_mut(id) {
        tab.sticky = true;
    }
    self.session.preview_tab = None;
    }
}

/// Save the buffer at `rel` to disk. Folds the user's uncommitted `working`
/// layer into `accepted` via `commit_working` (per `op-log.md`'s "Disk write
/// invariant"), which atomically rewrites the `.md`. Returns Ok(()) even for
/// clean buffers (no-op); errors only when the commit fails.
///
/// The agent's `pending` ops are untouched — they live outside `working` and
/// `accepted`, so the save can't carry one to disk: the "saved without
/// reviewing" failure mode is gone. After commit, the buffer's `loaded_hash` /
/// `loaded_text` advance to the committed text so `is_dirty()` clears.
/// Because `working` is CRDT-merged, the old disk-drift modal for user saves
/// is superseded; external-edit reconciliation is handled separately by the
/// watcher (`op_writes::external_edit`).
///
/// On success, the op log records the commit (an accepted `user` op) so the
/// status-bar version dropdown and activity feed see a snapshot.
pub fn save_buffer(state: &mut AppState, rel: &str) -> Result<(), String> {
    let Some(buffer) = state.session.buffers.get(rel) else {
        return Err("buffer not found".to_string());
    };
    // Read-only preview buffers (snapshot / pending / trash) have no save
    // path — their verbs are Restore / Accept / Reject in the toolbar.
    if !matches!(&buffer.source, crate::tab::BufferSource::Vault { .. }) {
        return Ok(());
    }
    if !buffer.is_dirty() {
        return Ok(());
    }
    let log = &state.vault_session.services.oplog;
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(|e| e.to_string())? else {
        return Err(format!("no op-log document for {}", rel));
    };
    // Fold the `working` layer into `accepted` (atomic `.md` rewrite). The
    // forward binding already mirrored the user's typing into `working`, so
    // the committed content is exactly the editable buffer text.
    let mut text = buffer.current_text();

    // Normalize hand-typed / external wikilinks to the durable id form before
    // committing: resolve each name target, stamp it, rewrite to
    // `[[<ulid>|<display>]]`. The cheap parse pre-check keeps the common
    // save (no name-form links) off the store lock + async bridge entirely.
    // status: wikilink-name-normalize
    let needs_normalize = hiker_core::wikilink::parse_links(&text)
        .iter()
        .any(|l| !l.is_id_form() && !l.target.is_empty());
    if needs_normalize {
        if let Some(normalized) = normalize_wikilinks_blocking(state, rel, &text) {
            if normalized != text {
                // Update the working layer + the visible editor doc so the
                // commit folds the id-form text and the cursor stays valid.
                let _ = state
                    .vault_session
                    .services
                    .oplog
                    .apply_user_text(&doc_id, &normalized);
                if let Some(b) = state.session.buffers.get_mut(rel) {
                    b.set_doc_clamping_selection(&normalized);
                }
                text = normalized;
            }
        }
    }
    let log = &state.vault_session.services.oplog;
    match log.commit_working(&doc_id) {
        Ok(_) => {
            let new_hash = hiker_core::hash_string(&text);
            if let Some(b) = state.session.buffers.get_mut(rel) {
                // The committed text is the buffer's clean, in-sync-with-
                // `accepted` baseline; advancing both clears `is_dirty()`.
                b.loaded_hash = new_hash;
                b.loaded_text = text;
            }
            // Auto-reject-on-drift (`op-log-status-states`): the commit just
            // advanced `accepted`, so any pending agent op anchored to the
            // changed region may have drifted. When `[op-log]
            // auto_reject_on_drift` is set, flip those to rejected immediately.
            let auto_reject = state
                .vault_session
                .config
                .read()
                .map(|c| c.op_log.auto_reject_on_drift)
                .unwrap_or(false);
            if auto_reject
                && let Err(e) = hiker_core::ops::op_writes::auto_reject_drifted(
                    &state.vault_session.services.oplog,
                    rel,
                    true,
                )
            {
                tracing::warn!(error = %e, path = %rel, "oplog: auto-reject-on-drift failed");
            }
            state.push_toast(format!("Saved {}", rel), ToastLevel::Info);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Run the async wikilink id-form normalizer (`core::ops::buffer::
/// normalize_wikilinks`) to completion on the frame's tokio runtime. A fresh
/// reader `Store` is opened for the call rather than locking the shared
/// `read_store` across the `.await` (the trail-stamping seam does the same — a
/// `MutexGuard` held across an await point is the anti-pattern the indexer's
/// fresh-connection model exists to avoid). Returns the rewritten text, or
/// `None` (after a warn log) when there's no runtime or the pass errors — in
/// which case the save proceeds with the user's text unchanged.
fn normalize_wikilinks_blocking(state: &AppState, rel: &str, text: &str) -> Option<String> {
    let watcher = state.vault_session.services.watcher.clone();
    let jobs = state.vault_session.services.indexer.job_sender();
    let vault = state.vault_session.vault.clone();
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let mut store = match hiker_core::store::Store::open(vault.root()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %rel, "wikilink normalize: open store failed");
            return None;
        }
    };
    let result = handle.block_on(hiker_core::ops::buffer::normalize_wikilinks(
        &watcher, &jobs, &vault, &mut store, rel, text,
    ));
    match result {
        Ok(out) => Some(out),
        Err(e) => {
            tracing::warn!(error = %e, path = %rel, "wikilink normalize-on-save failed");
            None
        }
    }
}

impl AppState {
/// Force-overwrite a drifted file: re-read the current disk hash so the
/// write-checked path accepts our text. Used by the "Keep mine" branch of
/// the drift modal.
pub fn force_save(&mut self, rel: &str, text: &str) -> Result<(), String> {
    let state = self;
    // Route the forced write through the op log: `user_save` applies the
    // edit to `accepted` and writes the materialized `.md`. No drift check
    // here — the user already chose to overwrite via "Keep mine".
    hiker_core::ops::op_writes::user_save(
        &state.vault_session.services.oplog,
        &state.vault_session.vault,
        rel,
        text,
    )
    .map_err(|e| e.to_string())?;
    let new_hash = hiker_core::hash_string(text);
    if let Some(b) = state.session.buffers.get_mut(rel) {
        b.loaded_hash = new_hash;
        b.loaded_text = text.to_string();
    }
    state.push_toast(format!("Saved {} (forced)", rel), ToastLevel::Info);
    Ok(())
}
}

/// Reload a buffer from disk, discarding the user's in-buffer edits. Used
/// by the "Take theirs" branch of the drift modal.
pub fn reload_from_disk(state: &mut AppState, rel: &str) -> Result<(), String> {
    let (contents, hash) = state
        .vault_session
        .vault
        .read_file_with_hash(rel)
        .map_err(|e| e.to_string())?;
    let cfg_guard = state.vault_session.config.read().ok();
    let buf = crate::buffer::Buffer::with_config_and_vault(
        rel.to_string(),
        &contents,
        hash,
        cfg_guard.as_deref(),
        Some(state.vault_session.vault.clone()),
    );
    drop(cfg_guard);
    state.session.buffers.insert(rel.to_string(), buf);
    state.push_toast(format!("Reloaded {}", rel), ToastLevel::Info);
    Ok(())
}

/// Close a tab by id. If the buffer behind it is dirty, the caller is
/// expected to have shown the dirty-close modal first.
pub fn close_tab(state: &mut AppState, id: TabId) {
    let idx = state.session.tabs.iter().position(|t| t.id == id);
    let Some(idx) = idx else { return };
    let removed = state.session.tabs.remove(idx);

    if state.session.preview_tab == Some(id) {
        state.session.preview_tab = None;
    }
    if state.session.active_tab == Some(id) {
        // Move focus to the neighbour to the right, else left, else none.
        state.session.active_tab = state
            .session
            .tabs
            .get(idx)
            .or_else(|| state.session.tabs.get(idx.wrapping_sub(1)))
            .map(|t| t.id);
    }

    // If no other tab references this buffer, drop it from memory.
    if let Some(path) = removed.kind.vault_path().map(std::string::ToString::to_string) {
        let still_open =
            state.session.tabs.iter().any(|t| t.kind.vault_path() == Some(&path));
        if !still_open {
            state.session.buffers.remove(&path);
        }
    }

    // Drop any read-only preview buffer this tab was the last referrer
    // for. Vault buffers were already removed above by vault_path; this
    // covers the snapshot / pending / trash buffers stored under the
    // composite keys produced by `buffer_key_for_source`.
    if let crate::tab::TabKind::Editor { buffer, .. } = &removed.kind
        && !matches!(buffer, crate::tab::BufferSource::Vault { .. })
    {
        let key = crate::buffer::buffer_key_for_source(buffer);
        let still_used = state.session.tabs.iter().any(|t| {
            matches!(
                &t.kind,
                crate::tab::TabKind::Editor { buffer: b, .. }
                    if crate::buffer::buffer_key_for_source(b) == key
            )
        });
        if !still_used {
            state.session.buffers.remove(&key);
        }
    }

    // Drop the cluster-graph state if a ClusterGraph tab closes.
    // Each graph keeps a petgraph DiGraph + positions Vec which can be
    // large for big clusters; without this they leak.
    if let TabKind::ClusterGraph { tree_id } = &removed.kind {
        state.panels.cluster_graph.remove(tree_id);
    }

    // Drop the cluster-review pane state — owns a draft tree until the
    // user persists, keyed by the closed tab's id.
    if matches!(&removed.kind, TabKind::ClusterReview { .. }) {
        state.panels.clusters.review_panes.remove(&id);
    }
}
