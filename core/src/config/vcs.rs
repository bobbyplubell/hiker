//! Version-control config: the `[git]` section. Split from `sections.rs`
//! because git is a cohesive, self-contained concept (paired with `docs/git.md`
//! and the `hiker-git` crate) and `sections.rs` had grown past its file-length
//! budget. The parent module re-exports these alongside the other section types.

use serde::{Deserialize, Serialize};

/// `[git]` section. Configures the optional, user-driven git integration
/// (`git.md`, the VSCode model). Per-vault. Git is inert until the user opts in
/// (`enabled`) over a vault that is already a git repo — hiker never runs
/// automatic push/pull rounds. The one automatic action is commit-on-save,
/// gated by `auto_commit`. Carries no secrets — credentials for a remote come
/// from the user's git credential helper / SSH agent, the same way a plain
/// `git push` authenticates, never from this TOML.
///
/// status: git-config-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSection {
    /// Opt in to hiker's git integration for this vault. Default off — git is
    /// inert (no commit-on-save, no rename commits, no history reads through the
    /// git engine) until the user enables it over a vault that is already a git
    /// repo. Replaces the former `[sync].transport = "git"` selector now that
    /// git is the only (optional, user-driven) integration. [git-config-section]
    #[serde(default = "super::sections::no")]
    pub enabled: bool,
    /// `manual` (the DEFAULT — the user drives git; hiker never auto-commits,
    /// only folds external HEAD moves as external edits) or `integrated` (hiker
    /// may drive the debounced commit-on-save, still gated by `auto_commit`).
    /// See `git-integrated-mode` / `git-manual-mode`. Hiker never runs automatic
    /// push/pull in either mode (the VSCode model). `manual` is the default so a
    /// bare `[git].enabled = true` produces no surprise auto-commits.
    #[serde(default)]
    pub mode: GitMode,
    /// The vault's push/pull target (a git URL). Empty = local-only versioning.
    /// Push/pull is user-driven; hiker never pushes/pulls automatically.
    /// [git-config-section]
    #[serde(default)]
    pub remote: String,
    /// The debounced commit-on-save toggle for `integrated` mode. `true`
    /// (default) lets integrated mode commit a save burst; `false` disables the
    /// auto-commit even in integrated (sync/status still available, just no
    /// automatic commit). Ignored in `manual` mode, which NEVER auto-commits
    /// regardless. [git-commit-on-save]
    #[serde(default = "super::sections::yes")]
    pub auto_commit: bool,
    /// Coalesce rapid saves into one commit (debounce window, ms); rapid saves
    /// within the window `--amend`-coalesce. [git-commit-on-save]
    #[serde(default = "default_commit_debounce_ms")]
    pub commit_debounce_ms: u32,
    /// How a git repo NESTED inside the vault (the CODE-IN-VAULT pattern) is
    /// handled when git is the sync transport: `skip` (default — the nested
    /// repo is independent, excluded from the vault tree, one-vault-one-repo)
    /// or `submodule` (declare it a git submodule so the vault commit records
    /// its HEAD as a gitlink that travels with push/pull). [git-nested-repo-submodule]
    #[serde(default)]
    pub submodules: SubmoduleMode,
}

impl Default for GitSection {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: GitMode::default(),
            remote: String::new(),
            auto_commit: true,
            commit_debounce_ms: default_commit_debounce_ms(),
            submodules: SubmoduleMode::default(),
        }
    }
}

/// How a repo nested inside the vault is folded (or not) into git sync.
/// `Skip` keeps the one-vault-one-repo posture (the nested repo is independent,
/// excluded from the vault tree); `Submodule` declares it in `.gitmodules` so
/// the vault commit pins its HEAD as a gitlink. Opt-in via `[git] submodules`.
/// See `git.md` "Nested repositories". [git-nested-repo-submodule]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleMode {
    #[default]
    Skip,
    Submodule,
}

/// Git transport mode. `Manual` (the default) = the user drives git entirely;
/// hiker never auto-commits and only folds external HEAD moves as external
/// edits (`git-tolerate-head-move`). `Integrated` = hiker drives the debounced
/// commit-on-save (still gated by `auto_commit`). In neither mode does hiker
/// push/pull on its own (push/pull is user-driven — the VSCode model).
///
/// Default is `Manual` so a user who merely flips `[git].enabled = true` gets
/// NO surprise auto-commits: auto behavior is opt-in (deliberately select
/// `integrated`), per the load-bearing "auto is opt-in" requirement.
/// See `git.md` "Modes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitMode {
    Integrated,
    #[default]
    Manual,
}

const fn default_commit_debounce_ms() -> u32 {
    1500
}
