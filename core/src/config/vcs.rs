//! Version-control transport config: the `[sync].transport` selector and the
//! `[git]` section. Split from `sections.rs` because the git transport is a
//! cohesive, self-contained concept (paired with `docs/git.md` and the
//! `hiker-git` crate) and `sections.rs` had grown past its file-length budget.
//! The parent module re-exports these alongside the other section types.

use serde::{Deserialize, Serialize};

/// Which transport carries cross-device sync (`sync-transport-seam`). The merge
/// + conflict logic above the seam is transport-agnostic; this selects the
/// mechanism that moves whole-file content + version metadata. `Libp2p` is the
/// default; `Git` selects the integrated/manual git transport (`git.md`,
/// configured by the `[git]` section); `None` is local-only — no bidirectional
/// cross-device sync, though `.ops` history still accrues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTransport {
    #[default]
    Libp2p,
    Git,
    None,
}

/// `[git]` section. Configures the git transport (`git.md`) when
/// `[sync].transport = "git"`. Per-vault. Carries no secrets — credentials for
/// an https remote come from the user's git credential helper / SSH agent, the
/// same way a plain `git push` authenticates, never from this TOML.
///
/// status: git-config-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSection {
    /// `integrated` (hiker drives commit + push/pull) or `manual` (the user
    /// drives git; hiker tolerates HEAD moving and folds divergence as an
    /// external edit). See `git-integrated-mode` / `git-manual-mode`.
    #[serde(default)]
    pub mode: GitMode,
    /// Integrated push/pull target (a git URL). Empty = commit-only local
    /// versioning (no push/pull). Ignored in `manual` mode (hiker never
    /// pushes/pulls there). [git-config-section]
    #[serde(default)]
    pub remote: String,
    /// Commit on save. In `manual` mode, only commits when the user hasn't
    /// (`git-manual-commit-policy`). [git-commit-on-save]
    #[serde(default = "super::sections::yes")]
    pub auto_commit: bool,
    /// Coalesce rapid saves into one commit (debounce window, ms); rapid saves
    /// within the window `--amend`-coalesce. [git-commit-on-save]
    #[serde(default = "default_commit_debounce_ms")]
    pub commit_debounce_ms: u32,
    /// Periodic `git gc` interval (days) to keep packfiles compact.
    #[serde(default = "default_gc_interval_days")]
    pub gc_interval_days: u32,
}

impl Default for GitSection {
    fn default() -> Self {
        Self {
            mode: GitMode::default(),
            remote: String::new(),
            auto_commit: true,
            commit_debounce_ms: default_commit_debounce_ms(),
            gc_interval_days: default_gc_interval_days(),
        }
    }
}

/// Git transport mode. `Integrated` = hiker drives commit + push/pull;
/// `Manual` = the user drives git and hiker cooperates (tolerates HEAD moving,
/// never pushes/pulls/rebases). See `git.md` "Modes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitMode {
    #[default]
    Integrated,
    Manual,
}

const fn default_commit_debounce_ms() -> u32 {
    1500
}

const fn default_gc_interval_days() -> u32 {
    30
}
