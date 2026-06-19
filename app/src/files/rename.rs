//! Inline-rename machinery for the files sidebar: the egui-memory draft
//! lifecycle behind the in-tree rename `TextEdit` row (start / draft /
//! commit-on-focus-loss / Esc-cancel, per `docs/interaction.md`
//! [inline-edit-lifecycle]), the commit path through the indexer-driven
//! `move_note`, and the post-move bookkeeping shared with drag-drop moves:
//! repointing open buffers/tabs and landing the observed-rename git commit
//! (`git-observed-rename-commit`).

use eframe::egui;

use hiker_core::vault::EntryKind;

use crate::state::AppState;

use super::sidebar::basename_of;

pub(super) fn commit_rename(app: &mut AppState, from: &str, draft: &str) {
    let draft = draft.trim();
    if draft.is_empty() || draft == basename_of(from) {
        return;
    }
    // A rename is a basename change, never a move: reject path separators and
    // `..` so `../x` or `sub/x` can't silently relocate the note out of its
    // directory (`rename-basename-only`).
    if let Err(reason) = validate_rename_basename(draft) {
        app.push_toast(
            format!("Rename failed: {reason}"),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    let parent = from.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let to = if parent.is_empty() {
        draft.to_string()
    } else {
        format!("{parent}/{draft}")
    };
    // Route through the indexer-driven `move_note` (layered-doc rename +
    // referrer rewrites + watcher suppression), not the bare
    // `vault::move_note`.
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let from_owned = from.to_string();
    let to_owned = to.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .block_on(async { hiker_core::ops::file::move_note(&watcher, &jobs, &from_owned, &to_owned).await }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    if let Err(err) = result {
        app.push_toast(
            format!("Rename failed: {err}"),
            crate::state::ToastLevel::Error,
        );
        return;
    }
    app.file_tree_state.invalidate_dir(parent);
    if let Err(reason) = repoint_open_buffer(app, from, &to) {
        // The on-disk move already happened (collision-guarded by the vault),
        // but an open buffer already lives at the destination key with its own
        // (possibly unsaved) state. Refuse to clobber it rather than silently
        // drop those edits — surface it like the on-disk collision does.
        app.push_toast(
            format!("Renamed -> {to}, but {reason}"),
            crate::state::ToastLevel::Error,
        );
        commit_observed_rename(app, from, &to);
        return;
    }
    commit_observed_rename(app, from, &to);
    app.push_toast(format!("Renamed -> {to}"), crate::state::ToastLevel::Info);
}

/// Reject a rename draft that isn't a bare basename: a rename changes the
/// file's name in place, it must not move it across directories. Path
/// separators (`/`, `\`) and the parent-dir component `..` are the escape
/// vectors (`../x`, `sub/x`); `.` is meaningless as a whole name too.
/// Returns `Err(reason)` describing the rejection for the toast.
/// Would repointing a buffer from `from` to `to` clobber a *different* buffer
/// already open at `to`? `buffer_exists` reports whether a buffer is open at a
/// key. Repointing onto an occupied destination key would silently drop that
/// buffer's in-memory `working` state, so this refuses it (the in-memory
/// analogue of the on-disk vault collision guard). A no-op move (`from == to`)
/// never collides. Pure so the collision decision is unit-testable without a
/// full `AppState`.
fn check_buffer_collision(
    from: &str,
    to: &str,
    buffer_exists: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if from != to && buffer_exists(to) {
        return Err(format!(
            "a buffer is already open at {to} — its unsaved edits weren't touched"
        ));
    }
    Ok(())
}

fn validate_rename_basename(draft: &str) -> Result<(), &'static str> {
    if draft.contains('/') || draft.contains('\\') {
        return Err("name can't contain a path separator");
    }
    if draft == ".." || draft == "." {
        return Err("invalid name");
    }
    Ok(())
}

/// When the git transport is active, land a dedicated pure-rename commit for an
/// observed move (`git-observed-rename-commit`) so `git log --follow` recovers
/// it. A no-op when git sync isn't the active transport — the libp2p path and
/// the no-transport path are untouched.
pub(super) fn commit_observed_rename(app: &AppState, from: &str, to: &str) {
    if let Some(git) = &app.vault_session.services.git_sync {
        git.commit_observed_rename(from, to);
    }
}

/// Move any loaded buffer + open editor tabs from `from` to `to` after a
/// move/rename so the open view keeps tracking the file.
///
/// Refuses (and leaves the source buffer in place) when a *different* buffer is
/// already open at the destination key: blindly inserting would drop that
/// buffer's in-memory `working` state. The on-disk vault collision guard only
/// protects the file on disk, not the in-memory buffer map, so this is the
/// in-memory analogue of that guard (`rename-buffer-collision`).
pub(super) fn repoint_open_buffer(app: &mut AppState, from: &str, to: &str) -> Result<(), String> {
    check_buffer_collision(from, to, |k| app.session.buffers.contains_key(k))?;
    if let Some(buf) = app.session.buffers.remove(from) {
        let mut moved = buf;
        moved.path = to.to_string();
        app.session.buffers.insert(to.to_string(), moved);
    }
    for tab in &mut app.session.tabs {
        if let crate::tab::TabKind::Editor {
            buffer: crate::tab::BufferSource::Vault { path },
            ..
        } = &mut tab.kind
            && path == from
        {
            *path = to.to_string();
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
struct RenameMem {
    path: String,
    draft: String,
    just_opened: bool,
}

fn mem_id() -> egui::Id {
    egui::Id::new("sidebar-files-rename")
}

/// Active inline-rename draft for `path`, if any.
pub(super) fn rename_draft_for(ui: &egui::Ui, path: &str) -> Option<String> {
    ui.ctx().memory(|m| {
        m.data
            .get_temp::<RenameMem>(mem_id())
            .filter(|r| r.path == path)
            .map(|r| r.draft.clone())
    })
}

/// Enter inline-rename mode (egui-memory side): seed the draft + flag the
/// row to grab focus next frame.
pub(super) fn start_rename(ui: &egui::Ui, path: &str) {
    let draft = basename_of(path).to_string();
    ui.ctx().memory_mut(|m| {
        m.data.insert_temp(
            mem_id(),
            RenameMem {
                path: path.to_string(),
                draft,
                just_opened: true,
            },
        );
    });
}

/// Draw the inline-rename TextEdit row, returning the committed draft on
/// Enter or focus loss (Esc cancels; otherwise `None`). Manages focus +
/// egui-memory draft lifecycle. See [`rename_edit_outcome`].
pub(super) fn rename_text_edit(
    ui: &mut egui::Ui,
    path: &str,
    kind: &EntryKind,
    depth: usize,
    mut draft: String,
) -> Option<String> {
    let outcome = ui.horizontal(|ui| {
        ui.add_space((depth as f32) * 12.0);
        ui.add(match kind {
            EntryKind::Dir => crate::icons::ICONS.image(crate::icons::Icon::Folder),
            EntryKind::File => crate::icons::ICONS.image(crate::icons::Icon::File),
        });
        let id = egui::Id::new(("rename-edit", path));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut draft)
                .id(id)
                .desired_width(ui.available_width()),
        );
        // First frame: focus + clear the just-opened flag.
        let just_opened = ui.ctx().memory(|m| {
            m.data
                .get_temp::<RenameMem>(mem_id())
                .map(|r| r.path == path && r.just_opened)
                .unwrap_or(false)
        });
        if just_opened {
            resp.request_focus();
            ui.ctx().memory_mut(|m| {
                if let Some(mut r) = m.data.get_temp::<RenameMem>(mem_id())
                    && r.path == path
                {
                    r.just_opened = false;
                    m.data.insert_temp(mem_id(), r);
                }
            });
        }
        if resp.changed() {
            ui.ctx().memory_mut(|m| {
                m.data.insert_temp(
                    mem_id(),
                    RenameMem {
                        path: path.to_string(),
                        draft: draft.clone(),
                        just_opened: false,
                    },
                );
            });
        }
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let outcome = rename_edit_outcome(resp.lost_focus(), esc);
        if outcome != RenameEditOutcome::Editing {
            // Drop the draft if it still belongs to this row.
            ui.ctx().memory_mut(|m| {
                if m.data
                    .get_temp::<RenameMem>(mem_id())
                    .map(|r| r.path == path)
                    .unwrap_or(false)
                {
                    m.data.remove::<RenameMem>(mem_id());
                }
            });
        }
        (outcome == RenameEditOutcome::Commit).then(|| draft.clone())
    });
    outcome.inner
}

/// What an inline-rename frame resolved to. Focus-loss COMMITS (Enter
/// surrenders focus, so it commits too; so does clicking elsewhere); Esc is
/// the only cancel — per `docs/interaction.md` [inline-edit-lifecycle],
/// matching the board card editor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RenameEditOutcome {
    /// Still editing — keep the draft.
    Editing,
    /// Commit the draft (Enter or any other focus loss).
    Commit,
    /// Discard the draft (Esc).
    Cancel,
}

/// Resolve the rename lifecycle for one frame from the TextEdit's focus
/// state + the Esc key. Esc wins over a simultaneous focus loss (in egui,
/// Esc surrenders focus, so both arrive on the same frame).
const fn rename_edit_outcome(lost_focus: bool, esc_pressed: bool) -> RenameEditOutcome {
    if esc_pressed {
        RenameEditOutcome::Cancel
    } else if lost_focus {
        RenameEditOutcome::Commit
    } else {
        RenameEditOutcome::Editing
    }
}

#[cfg(test)]
mod rename_guard_tests {
    use super::{check_buffer_collision, validate_rename_basename};
    use std::collections::HashSet;

    /// A rename is a basename change: path separators and `..` must be rejected
    /// so they can't silently relocate the note out of its directory
    /// (`rename-basename-only`).
    #[test]
    fn rejects_path_escape_drafts() {
        assert!(validate_rename_basename("../escape").is_err());
        assert!(validate_rename_basename("sub/child").is_err());
        assert!(validate_rename_basename("win\\child").is_err());
        assert!(validate_rename_basename("..").is_err());
        assert!(validate_rename_basename(".").is_err());
    }

    /// A plain basename (including dotfiles and dotted names) is accepted.
    #[test]
    fn accepts_plain_basenames() {
        assert!(validate_rename_basename("notes.md").is_ok());
        assert!(validate_rename_basename(".hidden").is_ok());
        assert!(validate_rename_basename("a.b.c").is_ok());
    }

    /// A buffer already open at the destination must not be silently dropped:
    /// the collision is surfaced, mirroring the on-disk collision guard
    /// (`rename-buffer-collision`).
    #[test]
    fn refuses_to_clobber_open_destination_buffer() {
        let open: HashSet<String> = ["dst.md".to_string()].into_iter().collect();
        let exists = |k: &str| open.contains(k);
        assert!(check_buffer_collision("src.md", "dst.md", &exists).is_err());
    }

    /// No buffer at the destination → the repoint proceeds.
    #[test]
    fn allows_repoint_to_free_destination() {
        let open: HashSet<String> = ["src.md".to_string()].into_iter().collect();
        let exists = |k: &str| open.contains(k);
        assert!(check_buffer_collision("src.md", "dst.md", &exists).is_ok());
    }

    /// A no-op move onto its own key is never a collision.
    #[test]
    fn noop_move_is_not_a_collision() {
        let exists = |_k: &str| true;
        assert!(check_buffer_collision("same.md", "same.md", &exists).is_ok());
    }
}

#[cfg(test)]
mod rename_lifecycle_tests {
    use super::{rename_edit_outcome, RenameEditOutcome};

    /// Any focus loss commits — Enter (which surrenders focus) and a plain
    /// click-away arrive identically; the latter used to cancel
    /// (`bug-rename-focus-loss-cancels`).
    #[test]
    fn focus_loss_commits() {
        assert_eq!(rename_edit_outcome(true, false), RenameEditOutcome::Commit);
    }

    /// Esc is the only cancel, and wins over the focus loss it triggers on
    /// the same frame.
    #[test]
    fn esc_cancels_even_with_focus_loss() {
        assert_eq!(rename_edit_outcome(true, true), RenameEditOutcome::Cancel);
        assert_eq!(rename_edit_outcome(false, true), RenameEditOutcome::Cancel);
    }

    /// No focus change, no Esc → still editing.
    #[test]
    fn otherwise_keeps_editing() {
        assert_eq!(rename_edit_outcome(false, false), RenameEditOutcome::Editing);
    }
}
