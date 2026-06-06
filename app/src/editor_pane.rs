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
    match try_ensure_vault_buffer_loaded(state, rel) {
        Ok(()) => true,
        Err(err) => {
            state.push_toast(format!("Failed to open {}: {}", rel, err), ToastLevel::Error);
            false
        }
    }
}

/// The non-toasting half of [`ensure_vault_buffer_loaded`]: load the buffer or
/// return the read error to the caller. Used by passive, best-effort hosts
/// (hover previews, embedded canvas/board views) that render every frame and
/// must NOT spam an error toast when their target note is missing or its path
/// can't be resolved — they simply draw nothing. Tab opens go through
/// [`ensure_vault_buffer_loaded`], which surfaces the failure as a toast.
pub(crate) fn try_ensure_vault_buffer_loaded(
    state: &mut AppState,
    rel: &str,
) -> Result<(), hiker_core::errors::HikerError> {
    if state.session.buffers.contains_key(rel) {
        return Ok(());
    }
    // Open-time disk-reconcile (`op-log.md` §External-edit sync): the per-doc
    // backstop for a change the in-session watcher dropped (a suppressed-write
    // window, a notify-queue overflow). Fold any disk-vs-`accepted` drift in as
    // one `author=external` op BEFORE the buffer's text is read below, so the
    // rope and the editor binding load from the now-reconciled `accepted`. The
    // underlying `apply_external_edit` is hash-gated: byte-identical disk is a
    // no-op (no op minted, no rewrite), so this stays cheap on a clean open.
    // A new path with no doc yet resolves to `Ok(false)` and is a no-op; the
    // `ensure_doc` call below still mints its document.
    // status: op-log-open-time-disk-reconcile
    if let Err(e) = hiker_core::ops::op_writes::external_edit(
        &state.vault_session.services.oplog,
        &state.vault_session.vault,
        rel,
    ) {
        tracing::warn!(path = %rel, error = %e, "op-log: open-time disk reconcile failed (non-fatal)");
    }
    match state.vault_session.vault.read_file_with_hash(rel) {
        Ok((contents, hash)) => {
            // Ensure the op log has a document for this path before the editor
            // binding runs. A note created after the bootstrap walk (New Note
            // button, tree new-file, wikilink-create) has no doc yet; without
            // one the forward binding never mirrors typing into `working` and
            // the save fails with "no op-log document". Existing files were
            // seeded at vault open, so this is a no-op for them.
            if let Err(e) = hiker_core::ops::op_writes::ensure_doc(
                &state.vault_session.services.oplog,
                &state.vault_session.vault,
                rel,
            ) {
                tracing::warn!(path = %rel, error = %e, "op-log: failed to ensure document on open");
            }
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
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Open the file at `rel` as a buffer tab. If `sticky`, the tab is created
/// sticky (Mod-click / "Keep open" / drag); otherwise it lands in the
/// preview slot, replacing any prior preview tab.
pub fn open_file(state: &mut AppState, rel: &str, sticky: bool) {
    // A `.canvas` file opens in the spatial canvas view, never as a raw-JSON text
    // buffer. Route it to the canvas open path so back/forward navigation (and
    // any other `open_file` caller, e.g. the tree's Open verb) lands on the
    // canvas editor instead of its JSON. `canvas::open` owns its own nav push and
    // respects `nav.locked`, so it isn't double-recorded. status: canvas-nav-stack
    if rel.ends_with(".canvas") {
        crate::panels::canvas::open(state, rel);
        return;
    }
    // A cluster-tree note opens in its force-graph view, not as raw markdown — it
    // has no in-buffer view (unlike a board, which has a Markdown toggle), so a
    // plain buffer would just show its YAML. The kind-routing sibling of the
    // `.canvas` branch: every `open_file` caller (vault sidebar, backlinks,
    // related, appears-in, wikilinks) lands on the graph. status: cluster-tree-open-routing
    if rel.ends_with(".md")
        && let Some(tree_id) = state.vault_session.services.trees.tree_id_at_path(rel)
    {
        let for_build = tree_id.clone();
        state.find_or_open_tab(
            |k| matches!(k, TabKind::ClusterGraph { tree_id: existing } if existing == &tree_id),
            move || TabKind::ClusterGraph { tree_id: for_build },
        );
        return;
    }
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
    let tab = Tab::new(tab_id, TabKind::vault_buffer(rel.to_string()), sticky);
    state.session.tabs.push(tab);
    state.session.active_tab = Some(tab_id);
    if !sticky {
        state.session.preview_tab = Some(tab_id);
    }
}

/// DRIVE seam for linked viz tabs: open `rel` as a buffer tab placed in the
/// workbench editor `group`, rather than the viz tab's own preview slot. A
/// graph/canvas tab with `link.target == Some(Group(g))` routes its
/// node-clicks here so the note lands in the linked group.
///
/// Placement works through the one chokepoint: we point the workbench's
/// focused group at `group` before delegating to [`open_file`], so the
/// per-frame `sync_workbench_tabs` opens the freshly-added `Session::tabs`
/// entry into that group (new workbench tabs open into the focused group).
/// An already-open note is focused in place wherever it lives — the same
/// "find-or-focus" posture `open_file` has. status: tab-linking
pub fn open_file_in_group(
    state: &mut AppState,
    rel: &str,
    group: egui_workbench::workspace::GroupId,
    sticky: bool,
) {
    state.workbench.editor_area.set_focused_group(group);
    open_file(state, rel, sticky);
}

/// Resolve a viz tab's FOLLOW `source` to the vault path of the note active
/// in the referenced group/tab, if any. `Group(g)` reads the active tab in
/// that editor group each frame; `Tab(id)` reads that specific tab. Returns
/// `None` when the link is unset, the group is gone, or the active tab there
/// isn't note-backed. status: tab-linking
pub fn followed_note_path(state: &AppState, source: Option<crate::tab::LinkRef>) -> Option<String> {
    use crate::tab::LinkRef;
    let tab_id = match source? {
        LinkRef::Group(g) => state.active_tab_in_group(g)?,
        LinkRef::Tab(id) => id,
    };
    state
        .tab_by_id(tab_id)
        .and_then(|t| t.buffer_path())
        .map(std::string::ToString::to_string)
}

/// Resolve a viz tab's DRIVE `target` to a concrete workbench `GroupId`.
/// `Tab(id)` resolves to the group that tab currently lives in. Returns
/// `None` when the link is unset or the referenced group/tab is gone.
/// status: tab-linking
pub fn drive_target_group(
    state: &AppState,
    target: Option<crate::tab::LinkRef>,
) -> Option<egui_workbench::workspace::GroupId> {
    use crate::tab::LinkRef;
    match target? {
        LinkRef::Group(g) => Some(g),
        LinkRef::Tab(id) => state.group_of_tab(id),
    }
}

/// Human label for an editor `group`: the title of its active tab, else a
/// "Group N" fallback by traversal index. Used by the link-control menu.
/// status: tab-linking
fn group_label(state: &AppState, group: egui_workbench::workspace::GroupId, idx: usize) -> String {
    state
        .active_tab_in_group(group)
        .and_then(|id| state.tab_by_id(id))
        .map_or_else(|| format!("Group {}", idx + 1), super::tab::Tab::label)
}

/// Render the link-control popup body for the viz tab `tab_id`: a Source
/// (FOLLOW) and Target (DRIVE) picker over the current editor groups, plus a
/// clear option for each. Selecting a group writes the tab's `link`. Shared
/// by the graph and canvas tab headers so both surfaces wire links the same
/// way. status: tab-linking
pub fn link_menu_ui(ui: &mut eframe::egui::Ui, app: &mut AppState, tab_id: TabId) {
    use crate::tab::LinkRef;
    let groups = app.workbench.groups();
    let own_group = app.group_of_tab(tab_id);
    let current = app.tab_by_id(tab_id).map(|t| t.link).unwrap_or_default();

    let mut set_source: Option<Option<LinkRef>> = None;
    let mut set_target: Option<Option<LinkRef>> = None;

    ui.label(
        eframe::egui::RichText::new("Follow (highlight active note in)")
            .small()
            .color(hiker_theme::muted()),
    );
    for (idx, g) in groups.iter().enumerate() {
        // Linking a viz tab to its own group is a no-op loop; skip it.
        if own_group == Some(*g) {
            continue;
        }
        let selected = current.source == Some(LinkRef::Group(*g));
        let prefix = if selected { "* " } else { "  " };
        if ui.button(format!("{prefix}{}", group_label(app, *g, idx))).clicked() {
            set_source = Some(Some(LinkRef::Group(*g)));
            ui.close();
        }
    }
    if current.source.is_some() && ui.button("  Clear follow").clicked() {
        set_source = Some(None);
        ui.close();
    }

    ui.separator();
    ui.label(
        eframe::egui::RichText::new("Drive (open clicked notes in)")
            .small()
            .color(hiker_theme::muted()),
    );
    for (idx, g) in groups.iter().enumerate() {
        if own_group == Some(*g) {
            continue;
        }
        let selected = current.target == Some(LinkRef::Group(*g));
        let prefix = if selected { "* " } else { "  " };
        if ui.button(format!("{prefix}{}", group_label(app, *g, idx))).clicked() {
            set_target = Some(Some(LinkRef::Group(*g)));
            ui.close();
        }
    }
    if current.target.is_some() && ui.button("  Clear drive").clicked() {
        set_target = Some(None);
        ui.close();
    }

    if set_source.is_some() || set_target.is_some() {
        if let Some(tab) = app.tab_by_id_mut(tab_id) {
            if let Some(s) = set_source {
                tab.link.source = s;
            }
            if let Some(t) = set_target {
                tab.link.target = t;
            }
        }
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
        BufferSource::HistoryVersion { op_id, path } => {
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
        NavTarget::HistoryVersion { path, op_id } => {
            set_active_tab_kind(state, TabKind::version_preview(path.clone(), op_id.clone()));
        }
        NavTarget::ZimArticle { zim_path, article } => {
            // `nav.locked` is set by `nav_go`, so this in-place restore doesn't
            // re-push onto the stack.
            crate::panels::zim::restore_nav(state, zim_path, article.clone());
        }
    }
}

/// Open a historical version in the *active* tab, in place, and record it on
/// the nav stack so Back returns to the live file. The active tab's content
/// swaps to the read-only history-version view; the live buffer keeps its own
/// buffer-map key, so reverting (Back / "Live") is lossless.
pub fn open_version_in_tab(state: &mut AppState, path: &str, op_id: &str) {
    if !state.session.nav.locked {
        state.session.nav.push(NavTarget::HistoryVersion {
            path: path.to_string(),
            op_id: op_id.to_string(),
        });
    }
    set_active_tab_kind(state, TabKind::version_preview(path.to_string(), op_id.to_string()));
}

/// Open a trashed item as a read-only preview in the editor's preview
/// slot (reused like a non-sticky file open). `feature-trash-panel`.
pub fn open_trash_in_tab(state: &mut AppState, trash_path: &str, original_path: &str) {
    use crate::tab::BufferSource;
    let source = BufferSource::Trash {
        trash_path: trash_path.to_string(),
        original_path: original_path.to_string(),
    };
    if ensure_readonly_buffer_loaded(state, &source).is_none() {
        state.push_toast("Can't preview trashed item", crate::state::ToastLevel::Error);
        return;
    }
    let kind = TabKind::trash_preview(trash_path.to_string(), original_path.to_string());
    if let Some(prev_id) = state.session.preview_tab {
        if let Some(tab) = state.tab_by_id_mut(prev_id) {
            tab.kind = kind;
            tab.sticky = false;
        }
        state.session.active_tab = Some(prev_id);
        return;
    }
    let tab_id = state.next_tab_id();
    state.session.tabs.push(Tab::new(tab_id, kind, false));
    state.session.active_tab = Some(tab_id);
    state.session.preview_tab = Some(tab_id);
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
    let text = buffer.current_text();
    // Path-form wikilinks (per `wikilink-path-form`) are stored as-typed —
    // no save-time normalize step. What the user typed reaches disk verbatim.
    let log = &state.vault_session.services.oplog;
    // Fidelity invariant (`op-log-binding-fidelity-invariant`): the forward
    // mirror keeps `materialize(working)` byte-equal to the editor's text, so
    // the commit folds exactly the buffer the user sees. Asserted here at Save
    // (debug-only, zero release cost) to catch any forward-mirror drift in
    // tests/dev before it commits a stale `working`.
    debug_assert_eq!(
        log.materialize_working(&doc_id)
            .map(|c| c.text)
            .ok()
            .as_deref(),
        Some(text.as_str()),
        "op-log-binding-fidelity-invariant: materialize(working) must equal the editor's text at Save"
    );
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
            // Nudge enrolled peers to pull this just-committed change promptly
            // instead of waiting for their own poll tick. Cheap, non-blocking,
            // and a no-op when sync is off / there are no peers.
            // status: sync-poke-on-commit
            if let Some(sync) = &state.vault_session.services.sync {
                sync.notify_local_change();
            }
            state.push_toast(format!("Saved {}", rel), ToastLevel::Info);
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

    // If no other tab references this buffer, drop it from memory — *unless*
    // it has unsaved work. A note kept open only by a non-tab host (a canvas
    // card editing it via `embedded-buffer-view`) survives tab close while
    // dirty: autosave commits it, and once clean + tabless it's dropped on a
    // later close. Without this guard, closing the tab while a card edits the
    // same shared `Editor` would evict the buffer (and its unsaved edits) out
    // from under the card. status: embedded-buffer-view-lifecycle
    if let Some(path) = removed.kind.vault_path().map(std::string::ToString::to_string) {
        let still_open =
            state.session.tabs.iter().any(|t| t.kind.vault_path() == Some(&path));
        let dirty = state
            .session
            .buffers
            .get(&path)
            .is_some_and(crate::buffer::Buffer::is_dirty);
        if !still_open && !dirty {
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
        state.clusters_state.review_panes.remove(&id);
    }

    // Drop the ZIM viewer pane (archive handle + render texture), which
    // lives in a UI-thread-local store. status: zim-view
    if matches!(&removed.kind, TabKind::ZimView { .. }) {
        crate::panels::zim::forget(id);
    }

    // Drop a canvas tab's per-tab pane (parsed doc + view widget) and its
    // node-content panes (editor / htmlview state), the latter in a
    // UI-thread-local store keyed by tab + node id. status: canvas-tab
    if let TabKind::Canvas { path } = &removed.kind {
        // Snapshot the pane's view state (camera pan/zoom + per-card scroll/zoom)
        // into the session map keyed by path BEFORE dropping the pane, so a
        // reopen in the same session restores it. status: canvas-view-state-persist
        let path = path.clone();
        crate::panels::canvas::capture_view(state, id, &path);
        state.panels.canvases.remove(&id);
        crate::panels::canvas::content::forget(id);
        crate::panels::canvas::edit::forget(id);
    }
}

/// Close a tab; if the underlying buffer is dirty, surface the dirty-close
/// modal instead of closing immediately.
pub fn close_tab_with_dirty_guard(app: &mut AppState, tab_id: TabId) {
    let dirty_path = app
        .tab_by_id(tab_id)
        .and_then(|t| t.buffer_path().map(str::to_string))
        .filter(|p| {
            app.session
                .buffers
                .get(p)
                .map(crate::buffer::Buffer::is_dirty)
                .unwrap_or(false)
        });
    if let Some(path) = dirty_path {
        app.session.modal = Some(crate::state::Modal::DirtyClose { path, tab_id });
    } else {
        close_tab(app, tab_id);
    }
}
