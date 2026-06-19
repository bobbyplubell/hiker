//! The file-tree "Add to board…" surface: the board-then-column picker
//! (data gathering and render) and the card-add commit. Split from
//! `panels::board` (the board tab itself) because this surface is driven
//! from the file tree's note-row menu and drag targets, not from an open
//! board tab — it reads the narrow `activity::SurfaceCtx` service handles
//! rather than a board pane.
//
// status: board-add-card

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use hiker_theme as theme;

/// One board for the "Add to board…" picker: path, title, column names.
pub type PickerEntry = (String, String, Vec<String>);

/// Gather every board-doc + its columns, the set of board paths `note_rel`
/// is already a card on, and whether `note_rel` is itself a board-doc.
/// Read-only; runs on menu open. Used by the file-tree "Add to board…" verb,
/// reading the vault + read-store + layered off the narrow `activity::SurfaceCtx`.
///
/// status: board-add-card
pub fn picker_context_ctx(
    ctx: &crate::activity::SurfaceCtx<'_>,
    note_rel: &str,
) -> (Vec<PickerEntry>, std::collections::HashSet<String>, bool) {
    picker_context_parts(
        ctx.vault,
        &ctx.services.read_store,
        &ctx.services.layered,
        note_rel,
        Some(ctx.services.kinds.as_ref()),
    )
}

/// Shared body for `picker_context_ctx`. Takes the services it needs by
/// handle so the narrow `activity::SurfaceCtx` can drive it without an `&AppState`.
fn picker_context_parts(
    vault: &hiker_core::vault::Vault,
    read_store: &std::sync::Mutex<hiker_core::store::Store>,
    log: &hiker_core::editing::LayeredDoc,
    note_rel: &str,
    kinds: Option<&hiker_core::kinds::Registry>,
) -> (Vec<PickerEntry>, std::collections::HashSet<String>, bool) {
    let Ok(store) = read_store.lock() else {
        return (Vec::new(), std::collections::HashSet::new(), false);
    };
    let is_board = vault
        .read_file(note_rel)
        .ok()
        .map(|s| hiker_core::boards::parse_board_for(note_rel, &s, kinds).is_ok())
        .unwrap_or(false);
    let mut boards: Vec<PickerEntry> = Vec::new();
    for item in hiker_core::boards::list(vault, &store, log, kinds).unwrap_or_default() {
        let columns = vault
            .read_file(&item.rel_path)
            .ok()
            .and_then(|s| hiker_core::boards::parse_board_for(&item.rel_path, &s, kinds).ok())
            .map(|b| b.columns.into_iter().map(|c| c.name).collect::<Vec<_>>())
            .unwrap_or_default();
        boards.push((item.rel_path, item.title, columns));
    }
    let membership: std::collections::HashSet<String> =
        hiker_core::boards::containing_note_with_paths(vault, &store, log, note_rel, kinds)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.board_doc_rel)
            .collect();
    (boards, membership, is_board)
}

/// Render the board → column nested picker, recording the user's pick.
/// Used by the file-tree "Add to board…" verb.
///
/// status: board-add-card
pub fn column_picker(
    ui: &mut egui::Ui,
    boards: &[PickerEntry],
    membership: &std::collections::HashSet<String>,
    pick: &mut Option<(String, String)>,
) {
    for (rel, title, columns) in boards {
        let already = membership.contains(rel);
        ui.menu_button(title, |ui| {
            if already {
                ui.label(
                    egui::RichText::new("Already on this board")
                        .color(theme::muted())
                        .small(),
                );
            }
            for col in columns {
                if ui.add_enabled(!already, egui::Button::new(col)).clicked() {
                    *pick = Some((rel.clone(), col.clone()));
                    ui.close();
                }
            }
        });
    }
}

/// Append `note_rel` as a card to `board_rel`'s `column` via the core
/// `add_card` op (layered-doc user-save + lazy id-stamp). Runs synchronously on
/// the frame's tokio runtime; the board view re-reads on its next paint.
/// Used by the file-tree "Add to board…" verb.
///
/// status: board-add-card
pub fn add_card(app: &mut AppState, board_rel: &str, column: &str, note_rel: &str) {
    let log = app.vault_session.services.layered.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let kinds = app.vault_session.services.kinds.clone();
    let read_store = app.vault_session.services.read_store.clone();
    let board_rel = board_rel.to_string();
    let column = column.to_string();
    let note_rel = note_rel.to_string();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let result: Result<(), hiker_core::errors::HikerError> = handle.block_on(async {
        // status: board-card-references / derived-status-rule
        // Under path-as-identity the card holds only the source's vault
        // path. The preview runs under a SCOPED read-store lock (it backs
        // the one-sprint membership check on this drop-target / "Add to
        // board…" card-add path) so no guard is held across the await;
        // the commit then rides the same user-save + re-index pair the
        // core op uses.
        let new_src = {
            let store = read_store.lock().map_err(|_| {
                hiker_core::errors::HikerError::Io("read store poisoned".into())
            })?;
            hiker_core::boards::add_card_preview(
                &vault,
                &store,
                Some(kinds.as_ref()),
                &board_rel,
                &column,
                &note_rel,
            )?
        };
        let Some(new_src) = new_src else {
            // Already a card anywhere on this board: idempotent no-op.
            return Ok(());
        };
        hiker_core::ops::op_writes::user_save(&log, &vault, &board_rel, &new_src)?;
        let _ = jobs
            .send(hiker_core::indexer::IndexJob::Upsert {
                rel_path: board_rel.clone(),
                force: false,
            })
            .await;
        Ok(())
    });
    match result {
        Ok(()) => app.push_toast("Added to board".to_string(), ToastLevel::Info),
        Err(e) => app.push_toast(format!("Add to board failed: {e}"), ToastLevel::Error),
    }
}
