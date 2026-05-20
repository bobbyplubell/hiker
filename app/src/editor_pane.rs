//! Buffer lifecycle helpers — open a file as a buffer tab, save, close.
//!
//! Mirrors the open/commit/dirty machinery from
//! `ui/src/app/openFile.ts` + `ui/src/app/editor.ts` collapsed into Rust:
//! single function for "open this rel path as a buffer tab," obeying the
//! preview-slot rule.

use crate::buffer::Buffer;
use crate::state::{nav_push, note_visited, AppState, ToastLevel};
use crate::tab::{Tab, TabId, TabKind};

/// Open the file at `rel` as a buffer tab. If `sticky`, the tab is created
/// sticky (Mod-click / "Keep open" / drag); otherwise it lands in the
/// preview slot, replacing any prior preview tab.
pub fn open_file(state: &mut AppState, rel: &str, sticky: bool) {
    // Trail-tracking: every open_file counts as a "visit" regardless of
    // whether we end up reusing an existing tab.
    note_visited(state, rel);
    // Navigation history: skip when we're already navigating via
    // back/forward (the index points at this entry already).
    if !state.session.nav.locked {
        nav_push(state, rel);
    }

    // If the path is already an open tab, focus it and (if it was preview
    // and the request is sticky) promote it.
    if let Some(existing_id) = find_buffer_tab(state, rel) {
        state.session.active_tab = Some(existing_id);
        if sticky && state.session.preview_tab == Some(existing_id) {
            promote_preview(state);
        }
        return;
    }

    // Load contents into memory if not already cached.
    if !state.session.buffers.contains_key(rel) {
        match state.vault_session.vault.read_file_with_hash(rel) {
            Ok((contents, hash)) => {
                let cfg_guard = state.vault_session.config.read().ok();
                let buf = Buffer::with_config_and_vault(
                    rel.to_string(),
                    contents,
                    hash,
                    cfg_guard.as_deref(),
                    Some(state.vault_session.vault.clone()),
                );
                drop(cfg_guard);
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
                tab.kind = TabKind::Buffer { path: rel.to_string() };
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
        kind: TabKind::Buffer { path: rel.to_string() },
        sticky,
    };
    state.session.tabs.push(tab);
    state.session.active_tab = Some(tab_id);
    if !sticky {
        state.session.preview_tab = Some(tab_id);
    }
}

fn find_buffer_tab(state: &AppState, rel: &str) -> Option<TabId> {
    state.session.tabs.iter().find_map(|t| match &t.kind {
        TabKind::Buffer { path } if path == rel => Some(t.id),
        _ => None,
    })
}

/// Move the navigation cursor by `delta` (-1 = back, +1 = forward) and
/// re-open the buffer at that position. Sets `nav_locked` while running so
/// the resulting `open_file` doesn't push a new history entry.
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
    open_file(state, &path, /* sticky */ true);
    state.session.nav.locked = false;
}

/// Promote the current preview tab to sticky.
pub fn promote_preview(state: &mut AppState) {
    let Some(id) = state.session.preview_tab else { return };
    if let Some(tab) = state.tab_by_id_mut(id) {
        tab.sticky = true;
    }
    state.session.preview_tab = None;
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
    if !buffer.is_dirty() {
        return Ok(());
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
        Err(hiker_core::error::HikerError::DiskDrift { .. }) => {
            state.session.modal = Some(crate::state::Modal::DiskDrift {
                path: rel.to_string(),
                in_buffer_text: text,
            });
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Force-overwrite a drifted file: re-read the current disk hash so the
/// write-checked path accepts our text. Used by the "Keep mine" branch of
/// the drift modal.
pub fn force_save(state: &mut AppState, rel: &str, text: &str) -> Result<(), String> {
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
        contents,
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
    if let TabKind::Buffer { path } = &removed.kind {
        let still_open = state.session.tabs.iter().any(|t| matches!(&t.kind, TabKind::Buffer { path: p } if p == path));
        if !still_open {
            state.session.buffers.remove(path);
        }
    }
}
