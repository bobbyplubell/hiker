//! Buffer lifecycle helpers — open a file as a buffer tab, save, close.
//!
//! Mirrors the open/commit/dirty machinery from
//! `ui/src/app/openFile.ts` + `ui/src/app/editor.ts` collapsed into Rust:
//! single function for "open this rel path as a buffer tab," obeying the
//! preview-slot rule.

use crate::buffer::Buffer;
use crate::state::{nav_push, AppState, ToastLevel};
use crate::tab::{Tab, TabId, TabKind};

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
    if !state.session.buffers.contains_key(rel) {
        match state.vault_session.vault.read_file_with_hash(rel) {
            Ok((contents, hash)) => {
                let cfg_guard = state.vault_session.config.read().ok();
                let mut buf = Buffer::with_config_and_vault(
                    rel.to_string(),
                    &contents,
                    hash,
                    cfg_guard.as_deref(),
                    Some(state.vault_session.vault.clone()),
                );
                drop(cfg_guard);
                // Apply pending `edit_note` proposals into the live buffer
                // and snapshot the pre-apply disk text as `agent_base`. The
                // inline diff overlay then renders `DiffLayer(agent_base,
                // current, Agent)` so the agent's changes appear as a diff
                // the user can accept-all / reject-all via the file pill.
                // Per `patch-review-buffer-hydration`.
                buf.hydrate_pending_proposals(
                    state.vault_session.services.staging.as_ref(),
                );
                state.session.buffers.insert(rel.to_string(), buf);
            }
            Err(err) => {
                state.push_toast(
                    format!("Failed to open {}: {}", rel, err),
                    ToastLevel::Error,
                );
                return;
            }
        }
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

/// Load a read-only preview buffer (snapshot blob / staging proposal /
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
        BufferSource::Snapshot { change_id, .. } => {
            let id = change_id.parse::<i64>().ok()?;
            let bytes = state
                .vault_session
                .services
                .changes
                .content_at(id)
                .ok()
                .flatten()?;
            String::from_utf8(bytes).ok()?
        }
        BufferSource::StagingProposal { proposal_id, .. } => state
            .vault_session
            .services
            .staging
            .content(proposal_id)
            .ok()?,
        BufferSource::Trash { trash_path, .. } => std::fs::read_to_string(trash_path).ok()?,
        BufferSource::Vault { .. } => return None,
    };
    let cfg_guard = state.vault_session.config.read().ok();
    // Read-only buffer fronting a non-vault `BufferSource` (snapshot blob,
    // staging proposal, trash entry). `read_only = true` no-ops editing
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
    let Some(idx) = state.session.nav.idx else {
        return;
    };
    let next = idx as i32 + delta;
    if next < 0 || next as usize >= state.session.nav.history.len() {
        return;
    }
    let next_idx = next as usize;
    let path = state.session.nav.history[next_idx].clone();
    state.session.nav.idx = Some(next_idx);
    state.session.nav.locked = true;
    open_file(state, &path, /* sticky */ false);
    state.session.nav.locked = false;
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

/// Save the buffer at `rel` to disk. Updates loaded_hash on success.
/// Returns Ok(()) even for clean buffers (no-op); errors only when write
/// fails. Drift conflicts (file changed on disk between load and save)
/// open the `Modal::DiskDrift` resolution dialog and return Ok(()) — the
/// modal owns the next-step decision (`pre-write-drift-check`).
///
/// On success, also records the write to the changelog (`changes.db`) so
/// the status-bar version dropdown and activity feed see a snapshot —
/// without this, every user save vanished from history and the dropdown
/// was empty for buffers the user had been editing all session.
pub fn save_buffer(state: &mut AppState, rel: &str) -> Result<(), String> {
    let Some(buffer) = state.session.buffers.get(rel) else {
        return Err("buffer not found".to_string());
    };
    // Read-only preview buffers (snapshot / staging / trash) have no save
    // path — their verbs are Restore / Accept / Reject in the toolbar.
    if !matches!(&buffer.source, crate::tab::BufferSource::Vault { .. }) {
        return Ok(());
    }
    if !buffer.is_dirty() {
        return Ok(());
    }
    // Per `patch-review-hydrate-dehydrate`: refuse to save while there are
    // unresolved hydrated proposals — accepting each hunk is what writes
    // the `changes.db` audit row, and bypassing that via Save would skip
    // the audit. The user must walk the hunks (or Reject all) first.
    if !buffer.hydrated_proposals.is_empty() {
        let n = buffer.hydrated_proposals.len();
        return Err(format!(
            "{} pending agent {} for this buffer — accept or reject each hunk before saving",
            n,
            if n == 1 { "proposal" } else { "proposals" },
        ));
    }
    let text = buffer.current_text();
    let expected = buffer.loaded_hash.clone();

    // Pre-write baseline: capture the on-disk state before we overwrite
    // it so rollback has a row to land on. `ensure_baseline` is a no-op
    // if a row already exists for this path.
    {
        let changes = state.vault_session.services.changes.clone();
        if let Ok((pre_text, pre_hash)) = state.vault_session.vault.read_file_with_hash(rel)
            && let Err(e) = changes.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
        {
            tracing::warn!(error = %e, path = %rel, "changes: ensure_baseline failed (save_buffer)");
        }
    }

    match state.vault_session.vault.write_file_checked(rel, &expected, &text) {
        Ok(new_hash) => {
            let c = state.vault_session.services.changes.clone();
            if let Err(e) = c.append(hiker_core::changes::ChangeAppend {
                    path: rel,
                    op: hiker_core::changes::ChangeOp::Modified,
                    author: "user",
                    content_hash: Some(&new_hash),
                    content: Some(text.as_bytes()),
                    rename_from: None,
                    metadata: serde_json::json!({}),
                })
            {
                tracing::warn!(error = %e, path = %rel, "changes: append failed (save_buffer)");
            }
            if let Some(b) = state.session.buffers.get_mut(rel) {
                b.loaded_hash = new_hash;
                b.loaded_text = text.clone();
            }
            state.push_toast(format!("Saved {}", rel), ToastLevel::Info);
            Ok(())
        }
        Err(hiker_core::errors::HikerError::DiskDrift { .. }) => {
            state.session.modal = Some(crate::state::Modal::DiskDrift {
                path: rel.to_string(),
                in_buffer_text: text,
            });
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

impl AppState {
/// Force-overwrite a drifted file: re-read the current disk hash so the
/// write-checked path accepts our text. Used by the "Keep mine" branch of
/// the drift modal.
pub fn force_save(&mut self, rel: &str, text: &str) -> Result<(), String> {
    let state = self;
    // Re-read the on-disk hash so the second `write_file_checked` call
    // succeeds. If the file vanished entirely we still want to write.
    let pre_state = state.vault_session.vault.read_file_with_hash(rel).ok();
    let current_hash = pre_state
        .as_ref()
        .map(|(_, h)| h.clone())
        .unwrap_or_default();
    {
        let changes = state.vault_session.services.changes.clone();
        if let Some((pre_text, pre_hash)) = pre_state.as_ref()
            && let Err(e) = changes.ensure_baseline(rel, "user", pre_text.as_bytes(), pre_hash)
        {
            tracing::warn!(error = %e, path = %rel, "changes: ensure_baseline failed (force_save)");
        }
    }
    let new_hash = state
        .vault_session
        .vault
        .write_file_checked(rel, &current_hash, text)
        .map_err(|e| e.to_string())?;
    let c = state.vault_session.services.changes.clone();
    if let Err(e) = c.append(hiker_core::changes::ChangeAppend {
            path: rel,
            op: hiker_core::changes::ChangeOp::Modified,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(text.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"forced": true}),
        })
    {
        tracing::warn!(error = %e, path = %rel, "changes: append failed (force_save)");
    }
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
    // covers the snapshot / staging / trash buffers stored under the
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
