//! Git diff-summary tab (`diff-summary-panel`): a read-only viewer over the
//! vault repo's commit history. Pick a base rev (and optionally a head rev —
//! default is the working tree), see which paths changed with their
//! [`ChangeStatus`], and click a row to open the file with the
//! `DiffSource::GitRef` overlay. Viewer only — no merge / branch / PR verbs;
//! the data comes from the git transport engine's read pass-throughs
//! (`recent_commits` / `diff_paths`). Row gestures follow the interaction
//! grammar (`interaction.md`): hover wash + pointer signal the open, click
//! opens into the preview slot, mod-click opens sticky, right-click is a
//! menu, and an openable row drags its vault-relative path.

use eframe::egui;

use hiker_git::meta::CommitInfo;
use hiker_git::repo::ChangeStatus;

use crate::state::AppState;
use crate::tab::{BufferSource, DiffSource, Tab, TabKind};
use hiker_theme as theme;

/// Diff-summary tab local state: the picked revs plus the cached commit list
/// and file list. The diff recomputes only when the picks change (or on
/// Refresh), never per frame — the workdir diff walks the tree.
#[derive(Default)]
pub struct State {
    /// Base rev (the diff's left side). Seeded from HEAD on first open.
    base: Option<String>,
    /// Head rev (the right side). `None` = the working tree.
    head: Option<String>,
    /// Cached commit list for the rev pickers (newest first).
    log: Vec<CommitInfo>,
    /// Cached `(path, status)` rows for the current picks.
    rows: Vec<(String, ChangeStatus)>,
    /// The `(base, head)` pair `rows` was computed for.
    loaded_for: Option<(String, Option<String>)>,
    /// Last log/diff failure, rendered in place of the list.
    error: Option<String>,
}

/// A row action picked this frame, applied after the scroll closure releases
/// its borrows (same deferral pattern as the Changes tab). Copy-path is
/// handled inline by the menu's custom entry (it needs the live `ui`).
enum RowAction {
    /// Open the file as a plain buffer tab.
    Open { path: String },
    /// Open the file with the `GitRef` diff overlay against the base rev.
    OpenDiff { path: String, sticky: bool },
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Git diff");
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(
            "Changed paths between a base revision and the working tree (or another revision). Click a row to see its hunks.",
        )
        .color(theme::muted())
        .small(),
    );
    ui.add_space(6.0);

    // No engine, no page: the viewer reads through the git engine, present
    // only when `[git] enabled = true` over a vault that is already a git repo.
    let Some(git) = app.vault_session.services.git_sync.clone() else {
        ui.label(
            egui::RichText::new(
                "Git isn't enabled for this vault. Set [git] enabled = true in Settings (over a git repo) to browse diffs here.",
            )
            .color(theme::muted())
            .italics(),
        );
        return;
    };

    // Take the panel state out of `AppState` for the frame so the render
    // closures can borrow `app` (vault-root checks, hover previews) freely.
    let mut st = std::mem::take(&mut app.panels.git_diff);

    // Lazy-load the commit list on first open; Refresh re-reads it and forces
    // the diff to recompute.
    if st.log.is_empty() && st.error.is_none() {
        reload_log(&git, &mut st);
    }
    ui.horizontal(|ui| {
        rev_pickers(ui, &mut st);
        if ui.button("Refresh").on_hover_text("Re-read the commit list and the diff").clicked() {
            reload_log(&git, &mut st);
        }
    });
    ui.add_space(6.0);

    // Recompute the file list when the picks changed.
    if let Some(base) = st.base.clone() {
        let want = (base.clone(), st.head.clone());
        if st.loaded_for.as_ref() != Some(&want) {
            match git.diff_paths(&base, st.head.as_deref()) {
                Ok(rows) => {
                    st.rows = rows;
                    st.error = None;
                }
                Err(e) => {
                    st.rows.clear();
                    st.error = Some(e);
                }
            }
            st.loaded_for = Some(want);
        }
    }

    let mut pending: Option<RowAction> = None;
    if let Some(err) = &st.error {
        ui.colored_label(egui::Color32::RED, err.clone());
    } else if st.base.is_none() {
        ui.label(
            egui::RichText::new("(no commits yet — save a note to mint the first one)")
                .color(theme::muted())
                .italics(),
        );
    } else if st.rows.is_empty() {
        ui.label(
            egui::RichText::new("(no changes between the picked revisions)")
                .color(theme::muted())
                .italics(),
        );
    } else {
        egui::ScrollArea::vertical()
            .id_salt("git-diff-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (path, status) in &st.rows {
                    if let Some(action) = changed_file_row(ui, app, path, *status) {
                        pending = Some(action);
                    }
                }
            });
    }

    let base_rev = st.base.clone();
    app.panels.git_diff = st;
    if let Some(action) = pending {
        apply_row_action(app, action, base_rev.as_deref());
    }
}

/// Re-read the commit list and seed the base pick from HEAD when unset (or
/// when the picked rev fell out of the list, e.g. after an amend-coalesce).
fn reload_log(git: &crate::git_sync::GitSyncEngine, st: &mut State) {
    match git.recent_commits(50) {
        Ok(log) => {
            let known = |pick: &Option<String>| {
                pick.as_ref().is_some_and(|sha| log.iter().any(|c| &c.sha == sha))
            };
            if !known(&st.base) {
                st.base = log.first().map(|c| c.sha.clone());
            }
            if st.head.is_some() && !known(&st.head) {
                st.head = None;
            }
            st.log = log;
            st.error = None;
        }
        Err(e) => st.error = Some(e),
    }
    st.loaded_for = None;
}

/// One picker line: `Base: <commit> against: <Working tree | commit>`.
fn rev_pickers(ui: &mut egui::Ui, st: &mut State) {
    let pick_label = |pick: &Option<String>| {
        pick.as_ref()
            .and_then(|sha| st.log.iter().find(|c| &c.sha == sha))
            .map_or_else(|| "(no commits)".to_string(), rev_label)
    };
    ui.label(egui::RichText::new("Base").small().color(theme::muted()));
    egui::ComboBox::from_id_salt("git-diff-base")
        .selected_text(pick_label(&st.base))
        .show_ui(ui, |ui| {
            for c in &st.log {
                let selected = st.base.as_deref() == Some(c.sha.as_str());
                if ui.selectable_label(selected, rev_label(c)).clicked() {
                    st.base = Some(c.sha.clone());
                }
            }
        });
    ui.label(egui::RichText::new("against").small().color(theme::muted()));
    let head_text = match &st.head {
        None => "Working tree".to_string(),
        some => pick_label(some),
    };
    egui::ComboBox::from_id_salt("git-diff-head")
        .selected_text(head_text)
        .show_ui(ui, |ui| {
            if ui.selectable_label(st.head.is_none(), "Working tree").clicked() {
                st.head = None;
            }
            for c in &st.log {
                let selected = st.head.as_deref() == Some(c.sha.as_str());
                if ui.selectable_label(selected, rev_label(c)).clicked() {
                    st.head = Some(c.sha.clone());
                }
            }
        });
}

/// Picker text for a commit: short sha + truncated subject.
fn rev_label(c: &CommitInfo) -> String {
    const SUBJECT_MAX: usize = 40;
    let subject: String = c.subject.chars().take(SUBJECT_MAX).collect();
    let ellipsis = if c.subject.chars().count() > SUBJECT_MAX { "\u{2026}" } else { "" };
    format!("{} \u{b7} {subject}{ellipsis}", &c.sha[..c.sha.len().min(8)])
}

/// Status glyph + color for a changed-file row (the same palette the Changes
/// tab uses for its op kinds). The code graph's diff overlay reuses the colors
/// so "modified" looks the same on a row and on a node (`code-graph-diff-coloring`).
pub(crate) const fn status_glyph(status: ChangeStatus) -> (&'static str, egui::Color32) {
    match status {
        ChangeStatus::Added => ("A", egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
        ChangeStatus::Modified => ("M", egui::Color32::from_rgb(0x2f, 0x6f, 0xb9)),
        ChangeStatus::Deleted => ("D", egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
        ChangeStatus::Renamed => ("R", egui::Color32::from_rgb(0x9a, 0x5f, 0x1f)),
    }
}

/// Render one changed-file row and return the action (if any) the user picked.
///
/// Interaction per `interaction.md`: an openable row signals with the standard
/// hover wash + pointer; click opens the diff into the preview slot; mod-click
/// opens it sticky; right-click is always a menu (Open / Open diff / Copy
/// path). A path missing from the working tree (deleted, or renamed away)
/// has nothing to open into a buffer, so it keeps only the menu — with the
/// open verbs greyed out and carrying the reason.
fn changed_file_row(
    ui: &mut egui::Ui,
    app: &AppState,
    path: &str,
    status: ChangeStatus,
) -> Option<RowAction> {
    const ROW_HEIGHT: f32 = 22.0;
    let openable = app.vault_session.vault_root.join(path).is_file();
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click_and_drag(),
    );
    if openable {
        // An openable row is a drag source like any note row — the
        // vault-relative path payload ([drag-note-payload]).
        crate::widgets::note_row::note_drag_source(ui, &resp, path, path);
        if let Some(c) = theme::open_signal_wash(false, resp.hovered()) {
            ui.painter().rect_filled(rect, 2.0, c);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            // A row referencing a note hover-previews like any other note row.
            if path.ends_with(".md") {
                crate::widgets::preview::register_note_hover(ui, rect, path);
            }
        }
    }
    let (glyph, color) = status_glyph(status);
    let painter = ui.painter();
    painter.text(
        egui::pos2(rect.min.x + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        glyph,
        egui::FontId::monospace(12.0),
        color,
    );
    let text_color = if openable { ui.visuals().text_color() } else { theme::muted() };
    painter.text(
        egui::pos2(rect.min.x + 24.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        path,
        egui::FontId::proportional(13.0),
        text_color,
    );

    let mut action = None;
    if openable && resp.clicked() {
        let sticky = crate::widgets::note_row::open_sticky(ui.input(|i| i.modifiers));
        action = Some(RowAction::OpenDiff { path: path.to_string(), sticky });
    }
    let menu = row_menu(path, openable);
    let mut chosen = None;
    resp.context_menu(|ui| chosen = egui_workbench::menu::show(ui, menu));
    if let Some(picked) = chosen {
        action = Some(picked);
    }
    action
}

/// The row's context menu: Open / Open diff / Copy path. Open verbs grey out
/// (with the reason) when the path isn't in the working tree.
fn row_menu(path: &str, openable: bool) -> egui_workbench::menu::Menu<RowAction> {
    use egui_workbench::menu::{Action, Enabled, Menu};
    let enabled = if openable {
        Enabled::Yes
    } else {
        Enabled::No("not in the working tree".into())
    };
    let copy_path = path.to_owned();
    Menu::new()
        .action_with(
            Action::new("Open", RowAction::Open { path: path.to_string() })
                .enabled(enabled.clone()),
        )
        .action_with(
            Action::new(
                "Open diff",
                RowAction::OpenDiff { path: path.to_string(), sticky: false },
            )
            .enabled(enabled),
        )
        .custom(move |ui| {
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(copy_path.clone());
                ui.close();
            }
            None
        })
}

/// Apply a deferred row action. The menu's plain Open mirrors the universal
/// note-item Open (preview slot); Open diff routes through
/// [`open_diff_tab`] against the panel's picked base rev.
fn apply_row_action(app: &mut AppState, action: RowAction, base_rev: Option<&str>) {
    match action {
        RowAction::Open { path } => crate::editor_pane::open_file(app, &path, false),
        RowAction::OpenDiff { path, sticky } => {
            if let Some(rev) = base_rev {
                open_diff_tab(app, &path, rev, sticky);
            }
        }
    }
}

/// Open `path` as an editor tab with the `GitRef` diff overlay against `rev`
/// (`diff-source-git-ref`), honoring the standard preview/sticky tab model:
/// an existing identical diff tab is focused (and promoted when sticky), a
/// non-sticky open lands in (or replaces) the preview slot, a sticky open
/// gets its own tab. Shared with the code graph's "Open diff" node-menu verb
/// (`code-graph-open-diff-from-node`).
pub(crate) fn open_diff_tab(app: &mut AppState, path: &str, rev: &str, sticky: bool) {
    if !crate::editor_pane::ensure_vault_buffer_loaded(app, path) {
        return;
    }
    if let Some(existing) = app.session.tabs.iter().find(|t| {
        matches!(
            &t.kind,
            TabKind::Editor {
                buffer: BufferSource::Vault { path: p },
                diff: Some(DiffSource::GitRef { rev: r, .. }),
            } if p == path && r == rev
        )
    }) {
        let id = existing.id;
        app.session.active_tab = Some(id);
        if sticky && app.session.preview_tab == Some(id) {
            app.promote_preview();
        }
        return;
    }
    let kind = TabKind::git_diff_preview(path, rev);
    if !sticky && let Some(prev_id) = app.session.preview_tab {
        if let Some(tab) = app.tab_by_id_mut(prev_id) {
            tab.kind = kind;
            tab.sticky = false;
        }
        app.session.active_tab = Some(prev_id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(id, kind, sticky));
    app.session.active_tab = Some(id);
    if !sticky {
        app.session.preview_tab = Some(id);
    }
}
