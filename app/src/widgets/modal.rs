//! Modal renderer. Drained by the top-level app each frame; only one
//! modal can be open at a time (matches the TS UI's behaviour).

use eframe::egui;

use crate::editor_pane;
use crate::state::{AppState, Modal, ToastLevel};

/// The display half of a confirm modal: the text shown and whether the
/// confirm button uses the danger style. Bundled so `confirm_dialog`
/// takes the prompt as one value, separate from the `ctx` it renders
/// into and the `intent` it fires on accept.
struct ConfirmPrompt {
    title: String,
    body: String,
    confirm_label: String,
    cancel_label: String,
    danger: bool,
}

impl AppState {
pub fn modal(&mut self, ctx: &egui::Context) {
    let app = self;
    let Some(modal) = app.session.modal.take() else { return };
    match modal {
        Modal::Confirm {
            title,
            body,
            confirm_label,
            cancel_label,
            danger,
            intent,
        } => {
            app.confirm_dialog(
                ctx,
                ConfirmPrompt {
                    title,
                    body,
                    confirm_label,
                    cancel_label,
                    danger,
                },
                intent,
            );
        }
        Modal::DirtyClose { path, tab_id } => {
            app.dirty_close_dialog(ctx, path, tab_id);
        }
        Modal::Recovery { entries } => {
            app.recovery_dialog(ctx, entries);
        }
        Modal::ConfirmDelete { path } => {
            app.confirm_delete_dialog(ctx, path);
        }
        Modal::DiskDrift { path, in_buffer_text } => {
            app.disk_drift_dialog(ctx, path, in_buffer_text);
        }
        Modal::PathConflict { path, recorded_id, current_path_id, target } => {
            app.path_conflict_dialog(ctx, path, recorded_id, current_path_id, target);
        }
    }
}
}

/// Resolve a `pre-write-drift-check` failure. Offers three branches that
/// mirror the TS UI's drift modal: keep mine (force-overwrite), take
/// theirs (reload from disk, discard local edits), or open the dirty-
/// buffer diff so the user can merge by hand.
impl AppState {
fn disk_drift_dialog(
    &mut self,
    ctx: &egui::Context,
    path: String,
    in_buffer_text: String,
) {
    let app = self;
    #[derive(Clone, Copy)]
    enum Choice {
        KeepMine,
        TakeTheirs,
        OpenDiff,
        Cancel,
    }
    let mut decision: Option<Choice> = None;
    let mut open = true;
    egui::Window::new("File changed on disk")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} was modified outside hiker after you loaded it.",
                path
            ));
            ui.label("Pick how to resolve the conflict before saving:");
            ui.add_space(8.0);
            if ui.button("Open diff…").clicked() {
                decision = Some(Choice::OpenDiff);
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui.button("Take theirs (reload)").clicked() {
                    decision = Some(Choice::TakeTheirs);
                }
                let btn = egui::Button::new(
                    egui::RichText::new("Keep mine (overwrite)")
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b));
                if ui.add(btn).clicked() {
                    decision = Some(Choice::KeepMine);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(Choice::Cancel);
                }
            });
        });

    match decision {
        Some(Choice::KeepMine) => {
            if let Err(err) = app.force_save(&path, &in_buffer_text) {
                app.push_toast(
                    format!("Save failed: {err}"),
                    ToastLevel::Error,
                );
            }
        }
        Some(Choice::TakeTheirs) => {
            if let Err(err) = editor_pane::reload_from_disk(app, &path) {
                app.push_toast(
                    format!("Reload failed: {err}"),
                    ToastLevel::Error,
                );
            }
        }
        Some(Choice::OpenDiff) => {
            // Open the path in an editor tab with diff-against-disk mode
            // already on. Diff is a mode of the editor tab (per
            // `diff-as-mode`); the same tab the user edits in renders the
            // diff as a decoration layer.
            use crate::tab::{BufferSource, DiffSource, Tab, TabKind};
            let p = path.clone();
            if let Some(existing) = app.session.tabs.iter().position(|t| {
                matches!(
                    &t.kind,
                    TabKind::Editor { buffer: BufferSource::Vault { path: q }, .. } if q == &p
                )
            }) {
                let tab = &mut app.session.tabs[existing];
                if let TabKind::Editor { diff, .. } = &mut tab.kind {
                    *diff = Some(DiffSource::Disk { path: p });
                }
                app.session.active_tab = Some(tab.id);
            } else {
                let id = app.next_tab_id();
                // Vault buffer with the on-disk diff already active.
                app.session.tabs.push(Tab {
                    id,
                    kind: TabKind::Editor {
                        buffer: BufferSource::Vault { path: p.clone() },
                        diff: Some(DiffSource::Disk { path: p }),
                    },
                    sticky: true,
                });
                app.session.active_tab = Some(id);
            }
        }
        Some(Choice::Cancel) => {}
        None if !open => {}
        None => {
            app.session.modal = Some(Modal::DiskDrift {
                path,
                in_buffer_text,
            });
        }
    }
}
}

impl AppState {
fn confirm_delete_dialog(&mut self, ctx: &egui::Context, path: String) {
    let app = self;
    let mut decision: Option<bool> = None;
    let mut open = true;
    egui::Window::new("Delete note")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Move {} to trash?", path));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
                let btn = egui::Button::new(
                    egui::RichText::new("Move to trash")
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b));
                if ui.add(btn).clicked() {
                    decision = Some(true);
                }
            });
        });

    match decision {
        Some(true) => app.apply_confirm_delete(&path),
        Some(false) => {}
        None if !open => {}
        None => {
            app.session.modal = Some(Modal::ConfirmDelete { path });
        }
    }
}

fn apply_confirm_delete(&mut self, rel: &str) {
    let app = self;
    let store_mutex = app.vault_session.services.read_store.clone();
    let watcher = app.vault_session.services.watcher.clone();
    let trash = hiker_core::trash::Trash::open(&app.vault_session.vault_root);
    let mut store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Err(err) = hiker_core::vault::delete_note(
        &app.vault_session.vault,
        &mut store,
        Some(watcher.as_ref()),
        &trash,
        rel,
    ) {
        drop(store);
        app.push_toast(format!("Delete failed: {}", err), ToastLevel::Error);
        return;
    }
    drop(store);

    // Close any open tabs for the deleted path + drop its buffer.
    let to_close: Vec<crate::tab::TabId> = app
        .session
        .tabs
        .iter()
        .filter(|t| t.buffer_path() == Some(rel))
        .map(|t| t.id)
        .collect();
    for id in to_close {
        editor_pane::close_tab(app, id);
    }
    app.session.buffers.remove(rel);
    let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    app.session.sidebar.dir_cache.remove(parent);
    app.push_toast(format!("Moved {} to trash", rel), ToastLevel::Info);
}
}

impl AppState {
fn confirm_dialog(
    &mut self,
    ctx: &egui::Context,
    prompt: ConfirmPrompt,
    intent: crate::state::ConfirmIntent,
) {
    let ConfirmPrompt { title, body, confirm_label, cancel_label, danger } = prompt;
    let app = self;
    let mut open = true;
    let mut decision: Option<bool> = None;

    egui::Window::new(&title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(&body);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(&cancel_label).clicked() {
                    decision = Some(false);
                }
                let confirm_btn = egui::Button::new(
                    egui::RichText::new(&confirm_label)
                        .color(egui::Color32::WHITE)
                        .strong(),
                )
                .fill(if danger {
                    egui::Color32::from_rgb(0xc0, 0x39, 0x2b)
                } else {
                    egui::Color32::from_rgb(0x3d, 0x59, 0x9c)
                });
                if ui.add(confirm_btn).clicked() {
                    decision = Some(true);
                }
            });
        });

    match decision {
        Some(true) => app.apply_confirm(intent),
        Some(false) | None if !open => {}
        _ => {
            // Reinsert the modal — user hasn't decided yet.
            app.session.modal = Some(Modal::Confirm {
                title,
                body,
                confirm_label,
                cancel_label,
                danger,
                intent,
            });
        }
    }
}
}

impl AppState {
fn dirty_close_dialog(
    &mut self,
    ctx: &egui::Context,
    path: String,
    tab_id: crate::tab::TabId,
) {
    let app = self;
    let mut decision: Option<DirtyChoice> = None;
    let mut open = true;

    egui::Window::new("Unsaved changes")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "{} has unsaved changes. What would you like to do?",
                path
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision = Some(DirtyChoice::Cancel);
                }
                if ui.button("Discard & close").clicked() {
                    decision = Some(DirtyChoice::Discard);
                }
                let save_btn = egui::Button::new(
                    egui::RichText::new("Save & close").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(0x3d, 0x59, 0x9c));
                if ui.add(save_btn).clicked() {
                    decision = Some(DirtyChoice::Save);
                }
            });
        });

    match decision {
        Some(DirtyChoice::Save) => {
            if let Err(err) = editor_pane::save_buffer(app, &path) {
                app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                app.session.modal = Some(Modal::DirtyClose { path, tab_id });
                return;
            }
            editor_pane::close_tab(app, tab_id);
        }
        Some(DirtyChoice::Discard) => {
            // Drop the buffer's in-memory edits by removing it; close_tab
            // would otherwise leave it (other tabs may reference it).
            if !app.session.tabs.iter().any(|t| {
                t.id != tab_id
                    && t.buffer_path() == Some(&path)
            }) {
                app.session.buffers.remove(&path);
            }
            editor_pane::close_tab(app, tab_id);
        }
        Some(DirtyChoice::Cancel) => {
            // User declined; drop the modal.
        }
        None if !open => {
            // X-closed counts as cancel.
        }
        None => {
            // No decision yet — keep the modal up.
            app.session.modal = Some(Modal::DirtyClose { path, tab_id });
        }
    }
}
}

enum DirtyChoice {
    Save,
    Discard,
    Cancel,
}

impl AppState {
fn recovery_dialog(
    &mut self,
    ctx: &egui::Context,
    mut entries: Vec<hiker_core::autosave::RecoveredEntry>,
) {
    let app = self;
    let mut bulk_decision: Option<BulkChoice> = None;
    let mut per_row: Vec<RowChoice> = Vec::new();

    egui::Window::new("Restore unsaved changes?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                "Hiker has autosaved copies of these buffers from a previous session. \
                 Restore brings the autosaved text back into the buffer; Discard drops it.",
            );
            ui.add_space(8.0);
            for (i, entry) in entries.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&entry.path).monospace());
                    if ui.button("Restore").clicked() {
                        per_row.push(RowChoice { idx: i, restore: true });
                    }
                    if ui.button("Discard").clicked() {
                        per_row.push(RowChoice { idx: i, restore: false });
                    }
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Restore all").clicked() {
                    bulk_decision = Some(BulkChoice::RestoreAll);
                }
                if ui.button("Discard all").clicked() {
                    bulk_decision = Some(BulkChoice::DiscardAll);
                }
                if ui.button("Decide later").clicked() {
                    bulk_decision = Some(BulkChoice::Defer);
                }
            });
        });

    if let Some(bulk) = bulk_decision {
        match bulk {
            BulkChoice::RestoreAll => {
                for entry in entries.drain(..) {
                    apply_restore(app, &entry);
                }
            }
            BulkChoice::DiscardAll => {
                for entry in entries.drain(..) {
                    apply_discard(app, &entry.path);
                }
            }
            BulkChoice::Defer => {
                // Leave the sidecars on disk; close the modal so the
                // user can keep working.
            }
        }
        return;
    }

    // Apply per-row decisions (in reverse index order so indexes stay
    // valid while we drain).
    per_row.sort_by_key(|c| std::cmp::Reverse(c.idx));
    for choice in per_row {
        if choice.idx >= entries.len() {
            continue;
        }
        let entry = entries.remove(choice.idx);
        if choice.restore {
            apply_restore(app, &entry);
        } else {
            apply_discard(app, &entry.path);
        }
    }

    if !entries.is_empty() {
        app.session.modal = Some(Modal::Recovery { entries });
    }
}
}

enum BulkChoice {
    RestoreAll,
    DiscardAll,
    Defer,
}

struct RowChoice {
    idx: usize,
    restore: bool,
}

fn apply_restore(app: &mut AppState, entry: &hiker_core::autosave::RecoveredEntry) {
    let text = match std::str::from_utf8(&entry.autosaved_content) {
        Ok(s) => s.to_string(),
        Err(_) => {
            app.push_toast(
                format!("Skipped non-UTF8 autosave: {}", entry.path),
                crate::state::ToastLevel::Warn,
            );
            return;
        }
    };
    // The loaded_hash is whatever's on disk *now* so the buffer is
    // immediately marked dirty (current_hash != loaded_hash) and the user
    // sees an unsaved buffer they can Save or Revert.
    let on_disk_hash = entry.on_disk_hash.clone().unwrap_or_default();
    // Plain disk-backed buffer (no config / vault completion sources).
    let buffer = crate::buffer::Buffer::with_config_and_vault(
        entry.path.clone(),
        &text,
        on_disk_hash,
        None,
        None,
    );
    app.session.buffers.insert(entry.path.clone(), buffer);
    crate::editor_pane::open_file(app, &entry.path, /* sticky */ true);
    let autosave = app.vault_session.services.autosave.clone();
    if let Err(err) = autosave.clear(&entry.path) {
        tracing::warn!(error = %err, "autosave clear after restore failed");
    }
}

fn apply_discard(app: &mut AppState, path: &str) {
    let autosave = app.vault_session.services.autosave.clone();
    if let Err(err) = autosave.discard(path) {
        app.push_toast(
            format!("Discard failed for {}: {}", path, err),
            crate::state::ToastLevel::Error,
        );
    }
}

// ===========================================================================
// Path-conflict modal (Keep mine / Repoint / Break)
// ===========================================================================

#[derive(Clone, Copy)]
enum ConflictChoice {
    KeepMine,
    Repoint,
    Break,
    Cancel,
}

impl AppState {
/// Resolve a stored double-link `PathConflict`: the recorded path now points
/// at a note with a different ULID than the one recorded. Three branches,
/// reused across every reference surface (`target`):
///   - **Keep mine** — leave the stored reference as-is (it stays a broken/
///     orphan-style card until the note it recorded reappears).
///   - **Repoint** — rewrite the stored path to the note now at `path`,
///     adopting its current identity (one `user_save` op).
///   - **Break** — remove the reference (card / waypoint) entirely.
///
/// status: trail-path-conflict-modal
/// status: board-card-references
fn path_conflict_dialog(
    &mut self,
    ctx: &egui::Context,
    path: String,
    recorded_id: String,
    current_path_id: String,
    target: crate::state::PathConflictTarget,
) {
    let app = self;
    let mut decision: Option<ConflictChoice> = None;
    let mut open = true;
    egui::Window::new("Reference conflict")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "The note at {path} is no longer the one this reference recorded."
            ));
            ui.label(
                egui::RichText::new(format!(
                    "recorded id {recorded_id} - current note id {current_path_id}"
                ))
                .small()
                .monospace()
                .color(egui::Color32::from_rgb(0xb9, 0x6a, 0x6a)),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Keep mine").on_hover_text(
                    "Leave the reference unchanged (stays broken until the recorded note returns)",
                ).clicked() {
                    decision = Some(ConflictChoice::KeepMine);
                }
                if ui.button("Repoint").on_hover_text(
                    "Rewrite the reference to the note now at this path",
                ).clicked() {
                    decision = Some(ConflictChoice::Repoint);
                }
                let break_btn = egui::Button::new(
                    egui::RichText::new("Break").color(egui::Color32::WHITE).strong(),
                )
                .fill(egui::Color32::from_rgb(0xc0, 0x39, 0x2b));
                if ui.add(break_btn).on_hover_text("Remove the reference").clicked() {
                    decision = Some(ConflictChoice::Break);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(ConflictChoice::Cancel);
                }
            });
        });

    match decision {
        Some(ConflictChoice::KeepMine) | Some(ConflictChoice::Cancel) => {}
        Some(ConflictChoice::Repoint) => app.apply_conflict_repoint(&path, &target),
        Some(ConflictChoice::Break) => app.apply_conflict_break(&target),
        None if !open => {}
        None => {
            app.session.modal = Some(Modal::PathConflict {
                path,
                recorded_id,
                current_path_id,
                target,
            });
        }
    }
}

/// "Repoint": rewrite the reference's stored path to the note now at `path`.
/// For a board card this is a board-doc frontmatter `user_save` via the
/// board ops (the card's id stays; only the path half is adopted from the
/// current note — actually a fresh `{id,path}` for the note now there).
fn apply_conflict_repoint(
    &mut self,
    path: &str,
    target: &crate::state::PathConflictTarget,
) {
    use crate::state::PathConflictTarget;
    match target {
        PathConflictTarget::BoardCard { board_rel, card_id } => {
            crate::panels::board::repoint_card(self, board_rel, card_id, path);
        }
    }
}

/// "Break": remove the conflicting reference entirely.
fn apply_conflict_break(&mut self, target: &crate::state::PathConflictTarget) {
    use crate::state::PathConflictTarget;
    match target {
        PathConflictTarget::BoardCard { board_rel, card_id } => {
            crate::panels::board::break_card(self, board_rel, card_id);
        }
    }
}
}
