//! Pure presentation helpers for the Source-Control activity (G3b). The
//! egui paint/click path isn't headless-testable, so everything that is a
//! *decision* — which controls a mode shows, how a change/submodule row is
//! labelled, whether the panel is idle — lives here as a plain function and
//! is unit-tested. The `mod.rs` render code calls these and only does the
//! (untestable) egui layout. No egui types leak in here.

use hiker_core::config::vcs::GitMode;

use crate::git_sync::{GitStatus, PathChange, SubmoduleStatusRow};

/// Single-letter glyph + a human label for a [`PathChange`], matching the
/// `git_diff` panel's A/M/D/R vocabulary so a changed file reads the same on
/// either surface.
pub(crate) const fn change_glyph(change: PathChange) -> &'static str {
    match change {
        PathChange::Added => "A",
        PathChange::Modified => "M",
        PathChange::Deleted => "D",
        PathChange::Renamed => "R",
    }
}

/// The set of commit-row controls the header/commit area shows for a mode.
/// `manual` is the full VSCode model (staging + an explicit commit box);
/// `integrated` auto-commits on save, so its SC surface is sync/status only —
/// no staging groups, no commit box. [git-manual-mode] [git-integrated-mode]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeControls {
    /// Split the changed files into Staged / Changes groups with per-file
    /// stage/unstage and a commit box. `false` = a single flat changed list.
    pub staging: bool,
    /// Show the commit-message box + Commit / Commit&Sync / Amend buttons.
    pub commit_box: bool,
}

impl ModeControls {
    /// Resolve the controls for a git mode.
    pub(crate) const fn for_mode(mode: GitMode) -> Self {
        match mode {
            GitMode::Manual => Self { staging: true, commit_box: true },
            GitMode::Integrated => Self { staging: false, commit_box: false },
        }
    }
}

/// Header summary line for a branch + ahead/behind, e.g. `main ↓2 ↑1`, or
/// `(detached)` when the branch is unborn/detached. Pure so the exact text is
/// pinned by a test rather than buried in a paint call.
pub(crate) fn branch_summary(status: &GitStatus) -> String {
    let branch = status.branch.as_deref().unwrap_or("(detached)");
    let mut out = branch.to_string();
    if status.behind > 0 {
        out.push_str(&format!("  \u{2193}{}", status.behind));
    }
    if status.ahead > 0 {
        out.push_str(&format!("  \u{2191}{}", status.ahead));
    }
    out
}

/// A short human description of a submodule row's state, for the row label and
/// hover. The states aren't mutually exclusive; the most actionable one wins
/// (uninitialized → dirty → advanced → clean). [git-nested-repo-submodule]
pub(crate) const fn submodule_state_label(row: &SubmoduleStatusRow) -> &'static str {
    if row.uninitialized {
        "not initialized"
    } else if row.dirty {
        "uncommitted changes"
    } else if row.advanced {
        "moved off pin"
    } else {
        "up to date"
    }
}

/// Whether an "Update submodules" action would do anything useful for this
/// row — i.e. the row is uninitialized or moved off its pin. A merely-dirty
/// submodule is the user's own nested work, not something `update` should
/// clobber, so it doesn't by itself justify the action.
pub(crate) const fn submodule_update_useful(row: &SubmoduleStatusRow) -> bool {
    row.uninitialized || row.advanced
}

/// Whether the whole working tree is clean for the SC view: no staged, no
/// unstaged, no conflicted paths. Drives the "nothing to commit" empty state.
/// Submodules are reported separately, so a dirty submodule alone still reads
/// as a clean working tree here.
pub(crate) fn working_tree_clean(status: &GitStatus) -> bool {
    status.staged.is_empty() && status.unstaged.is_empty() && status.conflicted.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_sync::ChangedPath;

    fn changed(path: &str, change: PathChange) -> ChangedPath {
        ChangedPath { path: path.to_string(), change }
    }

    #[test]
    fn change_glyphs_match_git_diff_vocabulary() {
        assert_eq!(change_glyph(PathChange::Added), "A");
        assert_eq!(change_glyph(PathChange::Modified), "M");
        assert_eq!(change_glyph(PathChange::Deleted), "D");
        assert_eq!(change_glyph(PathChange::Renamed), "R");
    }

    #[test]
    fn manual_mode_shows_staging_and_commit_box() {
        let c = ModeControls::for_mode(GitMode::Manual);
        assert!(c.staging, "manual splits Staged / Changes");
        assert!(c.commit_box, "manual shows the commit box");
    }

    #[test]
    fn integrated_mode_hides_staging_and_commit_box() {
        let c = ModeControls::for_mode(GitMode::Integrated);
        assert!(!c.staging, "integrated is a flat changed list (auto-commit)");
        assert!(!c.commit_box, "integrated commits automatically — no commit box");
    }

    #[test]
    fn branch_summary_renders_branch_and_counts() {
        let mut st = GitStatus { branch: Some("main".into()), ..GitStatus::default() };
        assert_eq!(branch_summary(&st), "main");
        st.ahead = 1;
        st.behind = 2;
        assert_eq!(branch_summary(&st), "main  \u{2193}2  \u{2191}1");
        // Only ahead.
        let st2 = GitStatus { branch: Some("dev".into()), ahead: 3, ..GitStatus::default() };
        assert_eq!(branch_summary(&st2), "dev  \u{2191}3");
    }

    #[test]
    fn branch_summary_handles_detached_head() {
        let st = GitStatus { branch: None, ..GitStatus::default() };
        assert_eq!(branch_summary(&st), "(detached)");
    }

    #[test]
    fn submodule_label_picks_most_actionable_state() {
        let row = |u, d, a| SubmoduleStatusRow {
            path: "code".into(),
            uninitialized: u,
            dirty: d,
            advanced: a,
        };
        assert_eq!(submodule_state_label(&row(true, true, true)), "not initialized");
        assert_eq!(submodule_state_label(&row(false, true, true)), "uncommitted changes");
        assert_eq!(submodule_state_label(&row(false, false, true)), "moved off pin");
        assert_eq!(submodule_state_label(&row(false, false, false)), "up to date");
    }

    #[test]
    fn submodule_update_useful_for_uninitialized_or_advanced_only() {
        let row = |u, d, a| SubmoduleStatusRow {
            path: "code".into(),
            uninitialized: u,
            dirty: d,
            advanced: a,
        };
        assert!(submodule_update_useful(&row(true, false, false)), "uninitialized → update");
        assert!(submodule_update_useful(&row(false, false, true)), "advanced → update");
        assert!(!submodule_update_useful(&row(false, true, false)), "only-dirty → leave alone");
        assert!(!submodule_update_useful(&row(false, false, false)), "clean → nothing to do");
    }

    #[test]
    fn working_tree_clean_only_when_no_changes_or_conflicts() {
        let mut st = GitStatus::default();
        assert!(working_tree_clean(&st), "empty status is clean");
        st.staged.push(changed("a.md", PathChange::Added));
        assert!(!working_tree_clean(&st), "a staged change is not clean");
        let st2 = GitStatus {
            unstaged: vec![changed("b.md", PathChange::Modified)],
            ..GitStatus::default()
        };
        assert!(!working_tree_clean(&st2));
        let st3 = GitStatus { conflicted: vec!["c.md".into()], ..GitStatus::default() };
        assert!(!working_tree_clean(&st3), "a conflict is not clean");
    }
}
