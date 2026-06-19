//! Source-Control activity (G3b) — the VSCode-style git panel, wired to the
//! G3a [`GitSyncEngine`](crate::git_sync::GitSyncEngine) verbs. A sidebar
//! `Activity` alongside Files/Trails/etc. (registered in
//! `crate::activity::builtin_activities`). It only does anything when
//! `[git].enabled` is set over a vault that is already a git repo (so a
//! `GitSyncEngine` exists in `services.git_sync`); otherwise it renders a
//! short "git not enabled" hint and offers no actions.
//!
//! The two modes differ exactly as the mode table says: `manual` is the full
//! VSCode model (grouped **Staged Changes** / **Changes** with per-file
//! stage/unstage/discard + a commit box), while `integrated` auto-commits on
//! save, so its SC surface is a flat changed-files list + sync/status only
//! (no staging, no commit box). [git-manual-mode] [git-integrated-mode]
//!
//! Refresh model (mirrors `panels/git_diff.rs`): the engine's `status()` walks
//! the index/workdir, so it's read once and CACHED in the activity's `State`,
//! never per frame. A manual **Refresh** button and every mutating verb
//! (stage/commit/sync/...) re-read it. The libgit2 calls are short and run
//! inline like `git_diff.rs`'s `diff_paths`; the UI thread isn't parked on a
//! network round (push/pull happen on an explicit button, and the outcome is
//! folded into a toast + a re-read).
//!
//! G5 added per-hunk staging in manual mode: a file row's per-hunk toggle
//! expands its working-vs-HEAD diff inline (`hunk_patch::build_hunks`), and
//! each hunk carries **Stage hunk / Unstage hunk / Discard hunk** actions that
//! apply that hunk's one-hunk patch through the engine
//! (`stage_hunk`/`unstage_hunk`/`discard_hunk`). A **Discard all changes**
//! action (behind a confirm) reverts every unstaged path, and amend is
//! surfaced in the commit box. A file row still opens the whole-file diff via
//! the existing `git_diff` panel. Conflicts route to the in-editor marker
//! resolver (opening the file activates `panels/buffer/conflict_overlay`);
//! once resolved + saved, a **Finalize merge** action calls
//! `finalize_merge_if_clean()`.
//!
//! Still deferred: the dirty-diff gutter's hover Stage/Revert (G4-adjacent)
//! needs editor gutter click-zones (editor-submodule work), separate from this
//! app-side per-hunk-in-diff-view feature.

pub mod hunk_patch;
pub mod logic;

use std::sync::Arc;

use eframe::egui;

use egui_workbench::activity::{Activity, View};
use hiker_core::config::vcs::GitMode;
use hiker_theme as theme;

use crate::activity::{AppCtx, SurfaceCtx};
use crate::git_sync::{
    GitStatus, GitSyncEngine, PullOutcome, SubmoduleStatusRow, SyncOutcome,
};
use crate::icons;
use crate::state::ToastLevel;
use hunk_patch::DiffHunk;
use logic::ModeControls;

/// Context radius for the per-hunk diff view (lines shown around each change).
const HUNK_CONTEXT: usize = 3;

/// Per-activity UI state for the Source-Control sidebar. Owned by
/// `AppState::source_control_state`. Holds the cached `status()` read (so the
/// index/workdir walk runs on Refresh / after a verb, not per frame) plus the
/// transient commit-message buffer. `loaded` distinguishes "not yet read" from
/// "read and found clean".
#[derive(Default)]
pub struct State {
    /// Cached status from the last `status()` read; `None` until first read.
    status: Option<GitStatus>,
    /// `true` once a `status()` read has completed (success or recorded error).
    loaded: bool,
    /// Last `status()` / verb error, surfaced in place of the file lists.
    error: Option<String>,
    /// Commit-message buffer (manual mode only).
    commit_message: String,
    /// Amend toggle for the next commit (manual mode only).
    amend: bool,
    /// The file whose per-hunk diff is expanded inline (manual mode), or `None`
    /// when no hunk view is open. Click a file's "hunks" toggle to set it.
    hunk_file: Option<String>,
    /// Cached per-hunk diff for `hunk_file` (recomputed on open / after a verb).
    hunks: Vec<DiffHunk>,
    /// A failure computing the hunk view, surfaced in place of the hunk list.
    hunk_error: Option<String>,
    /// Pending "Discard all changes" confirmation: `true` once the user clicked
    /// the action, until they confirm or cancel.
    confirm_discard_all: bool,
}

/// A user-picked action collected during the render pass, applied after the
/// scroll/closures release their borrows — same deferral shape the trails and
/// git_diff surfaces use. Each variant maps to one engine verb (or, for
/// `OpenFile` / `OpenDiff`, a `ctx.defer` into `&mut AppState`).
enum Action {
    Stage(Vec<String>),
    Unstage(Vec<String>),
    Discard(Vec<String>),
    /// Discard EVERY unstaged change (the "Discard all changes" action), after
    /// the inline confirm.
    DiscardAll,
    /// Toggle the inline per-hunk diff view for a file (close it if already open
    /// on that path).
    ViewHunks(String),
    /// Stage / unstage / discard a single hunk by its one-hunk patch text. The
    /// `file` is carried so the hunk view recomputes against the right path.
    StageHunk { file: String, patch: String },
    UnstageHunk { file: String, patch: String },
    DiscardHunk { file: String, patch: String },
    Commit { sync_after: bool },
    Sync,
    Pull,
    Push,
    Fetch,
    FinalizeMerge,
    UpdateSubmodules,
    /// Open a file as a plain buffer (used for conflicted files → the editor's
    /// marker resolver activates on the conflict markers).
    OpenFile(String),
    /// Open a file's working-vs-HEAD diff via the existing `git_diff` panel.
    OpenDiff(String),
}

// ---- Activity impl ----------------------------------------------------

/// Zero-sized `Activity` descriptor for the Source-Control panel. State lives
/// in `AppState::source_control_state`; the surface reaches the engine via
/// `ctx.services.git_sync` and the mode via `ctx.config`.
pub struct SourceControl;

impl Activity<dyn AppCtx> for SourceControl {
    fn id(&self) -> &'static str {
        "source-control"
    }
    fn label(&self) -> &'static str {
        "Source Control"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Diff)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&SourceControlSidebar]
    }
}

struct SourceControlSidebar;

impl View<dyn AppCtx> for SourceControlSidebar {
    fn id(&self) -> &'static str {
        "source-control"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-source-control-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}

// ---- Render -----------------------------------------------------------

/// Render the SC body. Gated on `[git].enabled` + an engine: with neither,
/// a short hint and no actions. Otherwise: header (branch + sync/fetch/pull/
/// push), changed-files (grouped or flat per mode), commit box (manual), and
/// submodule rows. Verbs are collected then applied once at the end.
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    // Gate: the engine exists only when `[git].enabled` over a git repo
    // (bootstrap builds it under exactly that condition). No engine → hint.
    let Some(engine) = ctx.services.git_sync.clone() else {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add(icons::ICONS.image(icons::Icon::Diff)).on_hover_text("Source Control");
            ui.label(
                egui::RichText::new("Git not enabled for this vault").color(theme::muted()).small(),
            );
        });
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Set [git] enabled = true in Settings (over a vault that is already a git repo) to use Source Control.",
            )
            .color(theme::muted())
            .italics()
            .small(),
        );
        return;
    };

    let mode = git_mode(ctx);
    let controls = ModeControls::for_mode(mode);

    // Take the cached state out for the frame so the closures can mutate it.
    let mut st = std::mem::take(state_mut(ctx));
    if !st.loaded {
        reload_status(&engine, &mut st);
    }

    let mut action: Option<Action> = None;
    header(ui, &st, mode, &mut action);
    ui.separator();

    if let Some(err) = &st.error {
        ui.colored_label(error_color(), err.clone());
    } else if let Some(status) = st.status.clone() {
        conflict_section(ui, &status, &mut action);
        if logic::working_tree_clean(&status) && status.conflicted.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Nothing to commit — working tree clean")
                    .color(theme::muted())
                    .small()
                    .italics(),
            );
        }
        if controls.staging {
            let view = HunkView { file: st.hunk_file.as_deref(), hunks: &st.hunks, error: st.hunk_error.as_deref() };
            staged_section(ui, &status, view, &mut action);
            changes_section(ui, &status, view, &mut action);
            discard_all_row(ui, &mut st, &status, &mut action);
        } else {
            flat_changes_section(ui, &status, &mut action);
        }
        if controls.commit_box {
            commit_box(ui, &mut st, &status, &mut action);
        }
        submodule_section(ui, &status, &mut action);
    }

    *state_mut(ctx) = st;
    if let Some(action) = action {
        apply_action(ctx, &engine, action);
    }
}

/// Header: branch + ahead/behind summary, then the network affordances.
/// Surfacing of a conflicted sync state happens in `apply_action` (a toast +
/// the conflict section), so the header itself is just the controls.
fn header(ui: &mut egui::Ui, st: &State, mode: GitMode, action: &mut Option<Action>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add(icons::ICONS.image(icons::Icon::Diff)).on_hover_text("Source Control");
        let summary = st
            .status
            .as_ref()
            .map_or_else(|| "\u{2026}".to_string(), logic::branch_summary);
        ui.label(egui::RichText::new(summary).strong());
    });
    let mode_label = match mode {
        GitMode::Manual => "manual",
        GitMode::Integrated => "integrated (auto-commit on save)",
    };
    ui.label(egui::RichText::new(mode_label).color(theme::muted()).small());
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Sync").on_hover_text("Pull then push").clicked() {
            *action = Some(Action::Sync);
        }
        if ui.button("Fetch").on_hover_text("Pull (fetch + merge)").clicked() {
            *action = Some(Action::Fetch);
        }
        if ui.button("Pull").on_hover_text("Fetch + merge from the remote").clicked() {
            *action = Some(Action::Pull);
        }
        if ui.button("Push").on_hover_text("Push the current branch").clicked() {
            *action = Some(Action::Push);
        }
        if ui.button("Refresh").on_hover_text("Re-read git status").clicked() {
            // A no-network re-read: the empty-paths Stage arm just reloads
            // `status()` (see `apply_action`).
            *action = Some(Action::Stage(Vec::new()));
        }
    });
}

/// The conflict section — only shown mid-merge. Lists each conflicted path
/// with an **Open** (→ the in-editor marker resolver) and offers a
/// **Finalize merge** once the user has resolved + saved. [git-conflict-inline-markers]
fn conflict_section(ui: &mut egui::Ui, status: &GitStatus, action: &mut Option<Action>) {
    if status.conflicted.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!("Merge conflicts ({})", status.conflicted.len()))
            .color(error_color())
            .strong(),
    );
    ui.label(
        egui::RichText::new(
            "Open each file to resolve its markers in the editor, then Finalize merge.",
        )
        .color(theme::muted())
        .small()
        .italics(),
    );
    for path in &status.conflicted {
        ui.horizontal(|ui| {
            ui.add(icons::ICONS.image(icons::Icon::Warning).tint(error_color()));
            if ui
                .add(egui::Label::new(egui::RichText::new(path).small()).sense(egui::Sense::click()))
                .on_hover_text("Open to resolve conflict markers")
                .clicked()
            {
                *action = Some(Action::OpenFile(path.clone()));
            }
        });
    }
    if ui.button("Finalize merge").on_hover_text("Commit the resolved merge").clicked() {
        *action = Some(Action::FinalizeMerge);
    }
    ui.separator();
}

/// Borrowed view of the inline per-hunk diff state, threaded into the manual
/// staging sections so a row can render its expanded hunk list.
#[derive(Clone, Copy)]
struct HunkView<'a> {
    /// The file whose hunk view is open (`None` = none expanded).
    file: Option<&'a str>,
    /// The cached hunks for `file`.
    hunks: &'a [DiffHunk],
    /// A failure computing the hunks, shown in place of the list.
    error: Option<&'a str>,
}

/// The **Staged Changes** group (manual mode): per-file unstage (-) and a
/// stage-none/unstage-all, plus a clickable row → its diff. A staged row can
/// expand its per-hunk view to **Unstage hunk** individual hunks.
fn staged_section(ui: &mut egui::Ui, status: &GitStatus, view: HunkView<'_>, action: &mut Option<Action>) {
    let header_extra = |ui: &mut egui::Ui| {
        if !status.staged.is_empty()
            && ui.small_button("\u{2212} all").on_hover_text("Unstage all").clicked()
        {
            *action = Some(Action::Unstage(status.staged.iter().map(|c| c.path.clone()).collect()));
        }
    };
    group_header(ui, "Staged Changes", status.staged.len(), header_extra);
    for c in &status.staged {
        file_row(ui, &c.path, logic::change_glyph(c.change), true, view, action);
    }
}

/// The **Changes** group (manual mode) over the unstaged paths: per-file
/// stage (+) / discard, and a stage-all/discard-all in the header. An unstaged
/// row can expand its per-hunk view to **Stage hunk** / **Discard hunk**.
fn changes_section(ui: &mut egui::Ui, status: &GitStatus, view: HunkView<'_>, action: &mut Option<Action>) {
    let header_extra = |ui: &mut egui::Ui| {
        if !status.unstaged.is_empty()
            && ui.small_button("+ all").on_hover_text("Stage all").clicked()
        {
            *action =
                Some(Action::Stage(status.unstaged.iter().map(|c| c.path.clone()).collect()));
        }
    };
    group_header(ui, "Changes", status.unstaged.len(), header_extra);
    for c in &status.unstaged {
        file_row(ui, &c.path, logic::change_glyph(c.change), false, view, action);
    }
}

/// Integrated-mode flat changed-files list (staged + unstaged unioned, no
/// staging affordances — commits are automatic). Each row opens its diff.
fn flat_changes_section(ui: &mut egui::Ui, status: &GitStatus, action: &mut Option<Action>) {
    let mut rows: Vec<(&str, &str)> = Vec::new();
    for c in status.staged.iter().chain(&status.unstaged) {
        rows.push((c.path.as_str(), logic::change_glyph(c.change)));
    }
    group_header(ui, "Changes", rows.len(), |_ui| {});
    if rows.is_empty() {
        ui.label(egui::RichText::new("No changes").color(theme::muted()).small().italics());
    }
    for (path, glyph) in rows {
        // No per-file actions in integrated mode (auto-commit handles it).
        flat_file_row(ui, path, glyph, action);
    }
}

/// One changed-file row in a manual group: glyph + clickable name (→ diff) +
/// per-file action buttons + a per-hunk toggle. `staged` selects unstage (-)
/// vs stage (+)/discard. When this row's hunk view is open (`view.file ==
/// path`), the per-hunk list renders right below it.
fn file_row(
    ui: &mut egui::Ui,
    path: &str,
    glyph: &str,
    staged: bool,
    view: HunkView<'_>,
    action: &mut Option<Action>,
) {
    let expanded = view.file == Some(path);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(glyph).monospace().small().color(theme::accent()));
        if ui
            .add(egui::Label::new(egui::RichText::new(path).small()).sense(egui::Sense::click()))
            .on_hover_text("Open working-vs-HEAD diff")
            .clicked()
        {
            *action = Some(Action::OpenDiff(path.to_string()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if staged {
                if ui.small_button("\u{2212}").on_hover_text("Unstage").clicked() {
                    *action = Some(Action::Unstage(vec![path.to_string()]));
                }
            } else {
                if ui.small_button("\u{2715}").on_hover_text("Discard changes").clicked() {
                    *action = Some(Action::Discard(vec![path.to_string()]));
                }
                if ui.small_button("+").on_hover_text("Stage").clicked() {
                    *action = Some(Action::Stage(vec![path.to_string()]));
                }
            }
            // Per-hunk view toggle. A rename/delete has no per-hunk patch worth
            // showing, but the toggle is harmless (the hunk list comes back
            // empty and self-collapses).
            let toggle = if expanded { "\u{25BE}" } else { "\u{25B8}" };
            if ui.small_button(toggle).on_hover_text("Per-hunk staging").clicked() {
                *action = Some(Action::ViewHunks(path.to_string()));
            }
        });
    });
    if expanded {
        hunk_view(ui, path, staged, view, action);
    }
}

/// Render the expanded per-hunk diff for `path`: each hunk's lines (colored
/// +/-/context) plus its Stage / Unstage / Discard action (per the row's
/// `staged` group). The action hands the hunk's one-hunk patch text to the
/// engine verb. [git-staging-ops]
fn hunk_view(
    ui: &mut egui::Ui,
    path: &str,
    staged: bool,
    view: HunkView<'_>,
    action: &mut Option<Action>,
) {
    ui.indent("sc-hunks", |ui| {
        if let Some(err) = view.error {
            ui.colored_label(error_color(), err);
            return;
        }
        if view.hunks.is_empty() {
            ui.label(egui::RichText::new("(no hunks)").color(theme::muted()).small().italics());
            return;
        }
        for (i, hunk) in view.hunks.iter().enumerate() {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(&hunk.header).monospace().small().color(theme::muted()));
            for line in &hunk.lines {
                let color = match line.as_bytes().first() {
                    Some(b'+') => egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                    Some(b'-') => egui::Color32::from_rgb(0xb9, 0x3a, 0x3a),
                    _ => theme::muted(),
                };
                ui.label(egui::RichText::new(line).monospace().small().color(color));
            }
            ui.horizontal(|ui| {
                let file = path.to_string();
                let patch = hunk.patch.clone();
                if staged {
                    if ui.small_button("Unstage hunk").clicked() {
                        *action = Some(Action::UnstageHunk { file, patch });
                    }
                } else {
                    if ui.small_button("Stage hunk").clicked() {
                        *action = Some(Action::StageHunk { file: file.clone(), patch: patch.clone() });
                    }
                    if ui.small_button("Discard hunk").on_hover_text("Revert this hunk on disk").clicked() {
                        *action = Some(Action::DiscardHunk { file, patch });
                    }
                }
            });
            if i + 1 < view.hunks.len() {
                ui.separator();
            }
        }
    });
}

/// The **Discard all changes** action (manual mode): a destructive
/// revert-to-HEAD of every unstaged path, behind an inline confirm so it can't
/// be a one-click accident. Shown only when there are unstaged changes.
fn discard_all_row(ui: &mut egui::Ui, st: &mut State, status: &GitStatus, action: &mut Option<Action>) {
    if status.unstaged.is_empty() {
        st.confirm_discard_all = false;
        return;
    }
    ui.add_space(2.0);
    if st.confirm_discard_all {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Discard ALL unstaged changes?").color(error_color()).small(),
            );
            if ui.small_button("Confirm").clicked() {
                *action = Some(Action::DiscardAll);
            }
            if ui.small_button("Cancel").clicked() {
                st.confirm_discard_all = false;
            }
        });
    } else if ui
        .small_button("Discard all changes")
        .on_hover_text("Revert every unstaged change to its HEAD content")
        .clicked()
    {
        st.confirm_discard_all = true;
    }
}

/// One changed-file row in the integrated flat list: glyph + clickable name
/// (→ diff). No staging buttons (commits are automatic).
fn flat_file_row(ui: &mut egui::Ui, path: &str, glyph: &str, action: &mut Option<Action>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(glyph).monospace().small().color(theme::accent()));
        if ui
            .add(egui::Label::new(egui::RichText::new(path).small()).sense(egui::Sense::click()))
            .on_hover_text("Open working-vs-HEAD diff")
            .clicked()
        {
            *action = Some(Action::OpenDiff(path.to_string()));
        }
    });
}

/// A group title row: `Title (n)` with an optional right-aligned action block.
fn group_header(ui: &mut egui::Ui, title: &str, count: usize, extra: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{title} ({count})")).strong().small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), extra);
    });
}

/// The commit box (manual mode): a multiline message, an Amend toggle, and
/// Commit / Commit & Sync buttons. Commit is disabled while nothing is staged
/// (and the message empty), matching the engine no-op.
fn commit_box(ui: &mut egui::Ui, st: &mut State, status: &GitStatus, action: &mut Option<Action>) {
    ui.separator();
    ui.add_space(2.0);
    ui.add(
        egui::TextEdit::multiline(&mut st.commit_message)
            .hint_text("Commit message")
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        ui.checkbox(&mut st.amend, "Amend").on_hover_text("Replace the previous commit");
    });
    let has_staged = !status.staged.is_empty();
    let can_commit = (has_staged || st.amend) && !st.commit_message.trim().is_empty();
    ui.horizontal(|ui| {
        ui.add_enabled_ui(can_commit, |ui| {
            if ui.button("Commit").clicked() {
                *action = Some(Action::Commit { sync_after: false });
            }
            if ui.button("Commit & Sync").on_hover_text("Commit then pull+push").clicked() {
                *action = Some(Action::Commit { sync_after: true });
            }
        });
    });
    if !has_staged && !st.amend {
        ui.label(
            egui::RichText::new("Stage changes to commit").color(theme::muted()).small().italics(),
        );
    }
}

/// Submodule rows: each declared submodule with its state label and, when an
/// update would help (uninitialized / moved-off-pin), an **Update submodules**
/// action. Read-only beyond that (no add/deinit UI). [git-nested-repo-submodule]
fn submodule_section(ui: &mut egui::Ui, status: &GitStatus, action: &mut Option<Action>) {
    if status.submodules.is_empty() {
        return;
    }
    ui.separator();
    group_header(ui, "Submodules", status.submodules.len(), |_ui| {});
    let mut any_update_useful = false;
    for row in &status.submodules {
        if logic::submodule_update_useful(row) {
            any_update_useful = true;
        }
        submodule_row(ui, row);
    }
    if any_update_useful
        && ui
            .button("Update submodules")
            .on_hover_text("Populate / advance submodules to their pinned commit")
            .clicked()
    {
        *action = Some(Action::UpdateSubmodules);
    }
}

/// One submodule status row: path + a state label, colored by severity.
fn submodule_row(ui: &mut egui::Ui, row: &SubmoduleStatusRow) {
    let label = logic::submodule_state_label(row);
    let color = if row.uninitialized || row.advanced {
        theme::warn()
    } else if row.dirty {
        theme::accent()
    } else {
        theme::muted()
    };
    ui.horizontal(|ui| {
        ui.add(icons::ICONS.image(icons::Icon::Folder));
        ui.label(egui::RichText::new(&row.path).small());
        ui.label(egui::RichText::new(label).color(color).small());
    });
}

// ---- Effects ----------------------------------------------------------

/// Apply a collected action against the engine, route errors/outcomes to
/// toasts, and re-read `status()` so the lists reflect the change. Open verbs
/// route through `ctx.defer` into `&mut AppState` (the only place that can
/// open a tab).
fn apply_action(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, action: Action) {
    match action {
        // A bare re-read (Refresh / the empty stage-all no-op): just reload.
        Action::Stage(paths) if paths.is_empty() => reload_into_state(ctx, engine),
        Action::Stage(paths) => run_then_reload(ctx, engine, engine.stage(&paths), "Staged"),
        Action::Unstage(paths) => run_then_reload(ctx, engine, engine.unstage(&paths), "Unstaged"),
        Action::Discard(paths) => {
            run_then_reload(ctx, engine, engine.discard(&paths), "Discarded changes")
        }
        Action::DiscardAll => do_discard_all(ctx, engine),
        Action::ViewHunks(path) => toggle_hunks(ctx, engine, &path),
        Action::StageHunk { file, patch } => {
            run_then_reload(ctx, engine, engine.stage_hunk(&patch), "Staged hunk");
            recompute_hunks_for(ctx, engine, &file);
        }
        Action::UnstageHunk { file, patch } => {
            run_then_reload(ctx, engine, engine.unstage_hunk(&patch), "Unstaged hunk");
            recompute_hunks_for(ctx, engine, &file);
        }
        Action::DiscardHunk { file, patch } => {
            run_then_reload(ctx, engine, engine.discard_hunk(&patch), "Discarded hunk");
            recompute_hunks_for(ctx, engine, &file);
        }
        Action::Commit { sync_after } => do_commit(ctx, engine, sync_after),
        Action::Sync | Action::Fetch | Action::Pull => do_sync_like(ctx, engine, &action),
        Action::Push => run_then_reload(ctx, engine, engine.push(), "Pushed"),
        Action::FinalizeMerge => do_finalize(ctx, engine),
        Action::UpdateSubmodules => do_update_submodules(ctx, engine),
        Action::OpenFile(path) => {
            ctx.defer(move |app| crate::editor_pane::open_file(app, &path, false));
        }
        Action::OpenDiff(path) => open_diff(ctx, engine, &path),
    }
}

/// Commit the staged content with the buffered message; on success clear the
/// message and (when `sync_after`) run a sync round. Reloads status either way.
fn do_commit(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, sync_after: bool) {
    let (message, amend) = {
        let st = state_ref(ctx);
        (st.commit_message.clone(), st.amend)
    };
    match engine.commit(&message, amend) {
        Ok(Some(_)) => {
            {
                let st = state_mut(ctx);
                st.commit_message.clear();
                st.amend = false;
            }
            toast(ctx, "Committed", ToastLevel::Info);
            if sync_after {
                do_sync_like(ctx, engine, &Action::Sync);
                return;
            }
        }
        Ok(None) => toast(ctx, "Nothing staged to commit", ToastLevel::Warn),
        Err(e) => toast(ctx, e, ToastLevel::Error),
    }
    reload_into_state(ctx, engine);
}

/// Sync / Pull / Fetch all route through the engine's `sync`/`pull` and fold
/// the outcome into a toast. A conflicted outcome is surfaced clearly (the
/// conflict section appears after the reload) — never a silent failure.
fn do_sync_like(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, action: &Action) {
    match action {
        Action::Sync => match engine.sync() {
            Ok(SyncOutcome::Pushed(p)) => toast(ctx, pull_outcome_msg(&p, true), ToastLevel::Info),
            Ok(SyncOutcome::Conflicted { paths }) => {
                toast(ctx, conflict_msg(paths.len()), ToastLevel::Warn);
            }
            Err(e) => toast(ctx, e, ToastLevel::Error),
        },
        Action::Pull | Action::Fetch => match engine.pull() {
            Ok(PullOutcome::Conflicted { paths }) => {
                toast(ctx, conflict_msg(paths.len()), ToastLevel::Warn);
            }
            Ok(other) => toast(ctx, pull_outcome_msg(&other, false), ToastLevel::Info),
            Err(e) => toast(ctx, e, ToastLevel::Error),
        },
        _ => {}
    }
    reload_into_state(ctx, engine);
}

/// Finalize the in-progress merge, refusing (with the engine's error) while
/// markers remain. On success the conflict section clears on the reload.
fn do_finalize(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>) {
    match engine.finalize_merge_if_clean() {
        Ok(Some(_)) => toast(ctx, "Merge finalized", ToastLevel::Info),
        Ok(None) => toast(ctx, "No merge in progress", ToastLevel::Warn),
        Err(e) => toast(ctx, e, ToastLevel::Error),
    }
    reload_into_state(ctx, engine);
}

/// Update submodules via the standalone engine verb (G5): restore any
/// uninitialized nested repo then advance populated ones to their pin — no
/// network, no pull. [git-nested-repo-submodule]
fn do_update_submodules(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>) {
    run_then_reload(ctx, engine, engine.update_submodules(), "Submodules updated");
}

/// Discard ALL unstaged changes after the confirm: gather every unstaged path
/// from the cached status and discard them in one go. Clears the confirm flag.
fn do_discard_all(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>) {
    let paths: Vec<String> = {
        let st = state_ref(ctx);
        st.status
            .as_ref()
            .map(|s| s.unstaged.iter().map(|c| c.path.clone()).collect())
            .unwrap_or_default()
    };
    state_mut(ctx).confirm_discard_all = false;
    if paths.is_empty() {
        toast(ctx, "Nothing to discard", ToastLevel::Warn);
        reload_into_state(ctx, engine);
        return;
    }
    run_then_reload(ctx, engine, engine.discard(&paths), "Discarded all changes");
}

/// Toggle the inline per-hunk diff view for `path`: close it if already open on
/// that path, otherwise open it and compute the hunks. [git-staging-ops]
fn toggle_hunks(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, path: &str) {
    let already = state_ref(ctx).hunk_file.as_deref() == Some(path);
    if already {
        let st = state_mut(ctx);
        st.hunk_file = None;
        st.hunks.clear();
        st.hunk_error = None;
        return;
    }
    state_mut(ctx).hunk_file = Some(path.to_string());
    recompute_hunks_for(ctx, engine, path);
}

/// Recompute the cached hunks for `path` (after opening the view or applying a
/// hunk verb). Closes the view when the file is now clean.
fn recompute_hunks_for(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, path: &str) {
    if state_ref(ctx).hunk_file.as_deref() != Some(path) {
        return;
    }
    match engine.working_hunks(path, HUNK_CONTEXT) {
        Ok(hunks) => {
            let st = state_mut(ctx);
            if hunks.is_empty() {
                // No working-tree change remains — collapse the view.
                st.hunk_file = None;
            }
            st.hunks = hunks;
            st.hunk_error = None;
        }
        Err(e) => {
            let st = state_mut(ctx);
            st.hunks.clear();
            st.hunk_error = Some(e);
        }
    }
}

/// Open `path`'s working-vs-HEAD diff via the existing `git_diff` panel. The
/// base rev is HEAD (the engine's most recent commit); with no commits yet a
/// plain open is the fallback. Routed through `ctx.defer` (tab ops need
/// `&mut AppState`).
fn open_diff(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>, path: &str) {
    let head = engine.recent_commits(1).ok().and_then(|c| c.first().map(|c| c.sha.clone()));
    let path = path.to_string();
    match head {
        Some(rev) => ctx.defer(move |app| {
            crate::panels::git_diff::open_diff_tab(app, &path, &rev, false);
        }),
        None => ctx.defer(move |app| crate::editor_pane::open_file(app, &path, false)),
    }
}

/// Run a verb `Result`, toast its error or a success line, then reload status.
fn run_then_reload(
    ctx: &mut SurfaceCtx<'_>,
    engine: &Arc<GitSyncEngine>,
    result: Result<(), String>,
    ok_msg: &str,
) {
    match result {
        Ok(()) => toast(ctx, ok_msg, ToastLevel::Info),
        Err(e) => toast(ctx, e, ToastLevel::Error),
    }
    reload_into_state(ctx, engine);
}

// ---- State / engine helpers -------------------------------------------

/// Read `[git].mode` from the shared config (default `manual`).
fn git_mode(ctx: &SurfaceCtx<'_>) -> GitMode {
    ctx.config.read().map(|c| c.git.mode).unwrap_or(GitMode::Manual)
}

/// Mutable handle to the activity's state slice.
fn state_mut<'a>(ctx: &'a mut SurfaceCtx<'_>) -> &'a mut State {
    ctx.state.downcast_mut::<State>().expect("source-control state")
}

/// Immutable handle to the activity's state slice.
fn state_ref<'a>(ctx: &'a SurfaceCtx<'_>) -> &'a State {
    ctx.state.downcast_ref::<State>().expect("source-control state")
}

/// Re-read `status()` into the live state slice (after a mutating verb).
fn reload_into_state(ctx: &mut SurfaceCtx<'_>, engine: &Arc<GitSyncEngine>) {
    let mut st = std::mem::take(state_mut(ctx));
    reload_status(engine, &mut st);
    *state_mut(ctx) = st;
}

/// Read `status()` into `st`, recording the error or clearing it.
fn reload_status(engine: &GitSyncEngine, st: &mut State) {
    match engine.status() {
        Ok(status) => {
            st.status = Some(status);
            st.error = None;
        }
        Err(e) => {
            st.status = None;
            st.error = Some(e);
        }
    }
    st.loaded = true;
}

/// Push a toast onto the shared sink.
fn toast(ctx: &mut SurfaceCtx<'_>, message: impl Into<String>, level: ToastLevel) {
    ctx.toasts.push(crate::state::Toast {
        message: message.into(),
        level,
        created_at: std::time::Instant::now(),
        undo: None,
    });
}

/// Human report for a clean pull outcome (`pushed` distinguishes the sync
/// "...and pushed" tail).
fn pull_outcome_msg(outcome: &PullOutcome, pushed: bool) -> String {
    let base = match outcome {
        PullOutcome::UpToDate => "Already up to date".to_string(),
        PullOutcome::Merged(_) => "Pulled changes".to_string(),
        PullOutcome::Conflicted { .. } => "Conflicts to resolve".to_string(),
    };
    if pushed {
        format!("{base}; pushed")
    } else {
        base
    }
}

/// Conflict toast copy.
fn conflict_msg(n: usize) -> String {
    format!("Pull left {n} conflicted file(s) — resolve in the editor, then Finalize merge")
}

/// Error / conflict accent. The theme exposes `warn`/`accent`/`muted` but no
/// dedicated error token, so the SC surface uses the same red the trails
/// sidebar uses for broken references.
const fn error_color() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}
