//! The [`GitBackend`] trait + its libgit2 implementation ([`Libgit2Backend`]).
//!
//! All `git2` use lives here. The trait verbs are what the sync orchestration
//! drives (`git.md`); the impl translates each into libgit2 calls and maps
//! every `git2::Error` into a [`GitError`] so nothing git2-shaped escapes.

use std::path::{Path, PathBuf};

use git2::{
    AnnotatedCommit, ApplyLocation, Cred, Diff, FetchOptions, IndexAddOption, PushOptions,
    RemoteCallbacks, Repository, Signature,
};

use crate::meta::{CommitInfo, Trailers};
use crate::{GitError, Result};

/// How the working tree relates to a known commit (`detect working-tree
/// divergence from a known state`, used by manual-mode HEAD-move detection in
/// `git-tolerate-head-move`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Divergence {
    /// HEAD still points at the known commit and the working tree is clean —
    /// nothing changed under hiker since it last looked.
    Unchanged,
    /// HEAD moved, the working tree has uncommitted changes, or both — the set
    /// of vault-relative paths whose on-disk content differs from the known
    /// commit's tree. The orchestration folds each as an external edit.
    Diverged { changed_paths: Vec<String> },
}

/// How one path changed between the two sides of a diff
/// (`diff-paths-trait-method`). Plain Rust mirror of the libgit2 delta kinds
/// the consumers care about; everything else collapses into `Modified`.
/// `Ord` so `(path, status)` rows sort deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeStatus {
    /// The path exists only on the new side (incl. untracked workdir files).
    Added,
    /// The path's content differs between the sides.
    Modified,
    /// The path exists only on the old side.
    Deleted,
    /// The path moved (a byte-similar delete+add pair, collapsed by rename
    /// detection); reported at the *new* path.
    Renamed,
}

impl ChangeStatus {
    /// Map a libgit2 delta kind onto the plain status, or `None` for deltas
    /// that aren't a change (unmodified / ignored / unreadable).
    const fn from_delta(delta: git2::Delta) -> Option<Self> {
        use git2::Delta;
        match delta {
            Delta::Added | Delta::Copied | Delta::Untracked => Some(Self::Added),
            Delta::Deleted => Some(Self::Deleted),
            Delta::Renamed => Some(Self::Renamed),
            Delta::Modified | Delta::Typechange | Delta::Conflicted => Some(Self::Modified),
            Delta::Unmodified | Delta::Ignored | Delta::Unreadable => None,
        }
    }
}

/// The result of letting git reconcile an inbound fetch (`git-merge-via-git`).
/// Plain Rust — no `git2` type crosses the boundary. The git transport lets
/// git's own merge engine produce correct 2-parent topology rather than hiker
/// fabricating merge commits ("Inbound merge" in `git.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Local already contains the fetched head — nothing to do.
    UpToDate,
    /// A clean reconcile (fast-forward or clean 3-way). The `String` is the new
    /// HEAD commit sha; for a 3-way merge that's a real 2-parent merge commit.
    Merged(String),
    /// Conflicts. The `Vec` is the vault-relative (forward-slash) paths with
    /// conflicts. `MERGE_HEAD` is left set and the working tree carries git's
    /// zdiff3 conflict markers; NO commit was made — the user resolves on disk
    /// (via the marker resolver), then the orchestration calls `finalize_merge`
    /// (or `abort_merge`).
    Conflicted(Vec<String>),
}

/// The current branch's name and how far it is ahead/behind its configured
/// upstream (`@{u}`). Plain Rust — the Source-Control header reads it directly.
/// [git-branch-status]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchStatus {
    /// The short branch name HEAD points at (e.g. `main`), or `None` on a
    /// detached HEAD / unborn branch.
    pub branch: Option<String>,
    /// Commits the local branch is ahead of its upstream (local-only commits).
    /// `0` when up to date; the count is also `0` when there is no upstream.
    pub ahead: usize,
    /// Commits the local branch is behind its upstream (remote-only commits).
    /// `0` when up to date or there is no upstream.
    pub behind: usize,
    /// Whether the branch has a configured upstream (`branch.<name>.remote`).
    /// When `false`, `ahead`/`behind` are both `0` (nothing to compare against).
    pub has_upstream: bool,
}

/// Per-submodule status the Source-Control view surfaces
/// (`git-nested-repo-submodule`). Plain Rust — no `git2` type crosses. A
/// submodule with all flags `false` is clean and at its pinned commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleStatus {
    /// Vault-relative path of the submodule (e.g. `code`).
    pub path: String,
    /// The submodule's working tree is an empty placeholder — never checked
    /// out (the freshly-cloned CODE-IN-VAULT broken state). The view offers an
    /// "update submodules" action to populate it.
    pub uninitialized: bool,
    /// The submodule's working tree (or its own index) has uncommitted changes
    /// — the user has nested-repo work in flight.
    pub dirty: bool,
    /// The submodule's checked-out HEAD differs from the gitlink the vault
    /// commit pins (it advanced or rolled back relative to the recorded
    /// pointer).
    pub advanced: bool,
}

/// The git verbs the sync orchestration sits above (`git-backend-trait`). Plain
/// Rust types only; the concrete backend ([`Libgit2Backend`]) hides libgit2.
/// Swappable to `gix` later without touching the orchestration.
pub trait GitBackend {
    /// Open the repo at `vault_root`, initializing it (with an initial empty
    /// state) if there is none. Idempotent.
    fn open_or_init(vault_root: &Path) -> Result<Self>
    where
        Self: Sized;

    /// Ensure `.gitignore` ignores `.hiker/` (`git-ignores-hiker`): the `.ops`
    /// history, `.pending` edits, and `index.db` are hiker-local and never
    /// tracked. Idempotent — appends the rule only when absent.
    fn ensure_hiker_ignored(&self) -> Result<()>;

    /// Stage `paths` (vault-relative) and commit them with `trailers`
    /// (`git-commit-on-save`, `git-attribution-trailer`). `subject` is the
    /// commit subject line. When `amend` is set, the commit replaces HEAD
    /// instead of adding a child (the debounce-window `--amend`-coalesce —
    /// policy decides when, this just performs it). Returns the new commit sha.
    /// A no-op (nothing staged differs from HEAD) returns `Ok(None)`.
    fn commit_paths(
        &self,
        paths: &[String],
        subject: &str,
        trailers: &Trailers,
        amend: bool,
    ) -> Result<Option<String>>;

    /// Commit a pure rename (`git-observed-rename-commit`): the new path
    /// carries the *old* content (byte-identical to HEAD at the old path), so
    /// `git log --follow` / `-M` match with certainty. The caller has already
    /// moved the file on disk. Carries a `Hiker-Rename: <from> -> <to>`
    /// trailer. Returns the commit sha, or `Ok(None)` if nothing changed.
    fn commit_rename(
        &self,
        from: &str,
        to: &str,
        trailers: &Trailers,
    ) -> Result<Option<String>>;

    /// Fetch from `remote` and report the merge-base relationship without
    /// touching the working tree (`git-push-pull-rounds` — the orchestration
    /// decides how an inbound divergence feeds the 3-way merge; this just
    /// fetches and classifies). Returns the changed paths between the local
    /// HEAD and the fetched remote head, so the orchestration can fold each.
    fn pull(&self, remote: &str) -> Result<Divergence>;

    /// Push the current branch to `remote` (`git-push-pull-rounds`). Never
    /// force-pushes. A rejected non-fast-forward surfaces as [`GitError::Push`]
    /// so the orchestration pulls + merges first.
    ///
    /// [`GitError::Push`]: crate::GitError::Push
    fn push(&self, remote: &str) -> Result<()>;

    /// Read the recent commit log (newest first, capped at `limit`) for
    /// inspection / the activity-feed projection (`git-parallel-history`).
    fn log(&self, limit: usize) -> Result<Vec<CommitInfo>>;

    /// Read the content of `path` at `rev` (`git show <rev>:<path>`), for
    /// inspection / version preview. `rev` is anything `git rev-parse`
    /// accepts — `HEAD`, a full or short sha, a ref name. `None` if the path
    /// didn't exist there.
    fn show(&self, rev: &str, path: &str) -> Result<Option<String>>;

    /// The paths that differ between `base_rev` and `head_rev`, each with how
    /// it changed (`diff-paths-trait-method`). Revs resolve like [`show`]'s
    /// (`HEAD`, full/short shas, ref names). `head_rev = None` diffs
    /// `base_rev` against the **working directory** (index + untracked files,
    /// `.hiker/` excluded) — the HEAD-vs-worktree case the diff-summary panel
    /// opens on. Renames are detected: a byte-similar delete+add pair
    /// collapses to one `Renamed` row at the new path.
    ///
    /// [`show`]: GitBackend::show
    fn diff_paths(
        &self,
        base_rev: &str,
        head_rev: Option<&str>,
    ) -> Result<Vec<(String, ChangeStatus)>>;

    /// Detect whether the working tree diverged from `known_sha`
    /// (`git-tolerate-head-move`): HEAD moved or files changed on disk. When
    /// `known_sha` is `None`, compares against the current HEAD (pure
    /// dirty-tree check). The manual-mode reconcile calls this to find external
    /// edits to fold.
    fn divergence_from(&self, known_sha: Option<&str>) -> Result<Divergence>;

    /// The current HEAD commit sha, or `None` on an unborn branch (a fresh repo
    /// with no commits yet).
    fn head_sha(&self) -> Result<Option<String>>;
}

/// libgit2-backed [`GitBackend`]. Owns one [`Repository`] handle.
pub struct Libgit2Backend {
    repo: Repository,
    root: PathBuf,
    /// Whether a nested repo under the vault is folded into vault sync as a
    /// git **submodule** (declared in `.gitmodules`, the vault commit records
    /// its HEAD as a gitlink that travels with push/pull) rather than SKIPPED
    /// (left an independent repo, excluded from the vault tree). Opt-in via
    /// `[git] submodules = "submodule"`; default `false` (skip) preserves the
    /// one-vault-one-repo posture. [git-nested-repo-submodule]
    track_submodules: bool,
}

impl Libgit2Backend {
    fn map_open(e: &git2::Error) -> GitError {
        GitError::Open(e.message().to_string())
    }

    fn map_commit(e: &git2::Error) -> GitError {
        GitError::Commit(e.message().to_string())
    }

    fn map_read(e: &git2::Error) -> GitError {
        GitError::Read(e.message().to_string())
    }

    /// A signature for hiker-authored commits. Git's own author/committer
    /// identity is separate from hiker's finer `Hiker-Author` trailer; we use a
    /// stable hiker identity so a commit is attributable even on a host with no
    /// `user.name` configured (which would otherwise fail `signature()`).
    fn signature(&self) -> Result<Signature<'static>> {
        // Prefer the repo's configured identity; fall back to a hiker default.
        if let Ok(sig) = self.repo.signature() {
            // `Repository::signature` borrows config-owned strings; rebuild an
            // owned 'static signature from its parts.
            let name = sig.name().unwrap_or("hiker");
            let email = sig.email().unwrap_or("hiker@localhost");
            return Signature::now(name, email).map_err(|e| Self::map_commit(&e));
        }
        Signature::now("hiker", "hiker@localhost").map_err(|e| Self::map_commit(&e))
    }

    /// Vault-relative directory prefixes (each with a trailing `/`) that hold a
    /// NESTED git repo — a `<dir>/.git` somewhere under the vault root, other
    /// than the vault repo's own `.git`. The walk records each nested repo and
    /// prunes it (never descends into a nested repo or any `.git`), so the cost
    /// is bounded by the vault's directory count outside nested repos. Empty in
    /// the common case (no repo nested in the vault). [self-host-code-in-vault]
    fn nested_repo_prefixes(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_nested_repos(&self.root, &self.root, &mut out);
        out
    }

    /// Enable/disable folding nested repos into vault sync as submodules. Set
    /// by the engine from `[git] submodules` before the first commit; default
    /// (false) keeps the skip posture. [git-nested-repo-submodule]
    pub const fn set_submodule_tracking(&mut self, track: bool) {
        self.track_submodules = track;
    }

    /// Declare every nested repo under the vault as a git submodule so the
    /// vault commit records its HEAD as a proper gitlink (not an undeclared
    /// embedded repo). For each nested repo not yet in `.gitmodules`, append a
    /// stanza (path + `origin` URL, falling back to `./<path>`) and set
    /// `submodule.<name>.url` in the vault repo config (so a fresh clone's
    /// `submodule update --init` resolves). Idempotent — an already-declared
    /// submodule is skipped. No-op without nested repos. [git-nested-repo-submodule]
    pub fn ensure_submodules_registered(&self) -> Result<()> {
        let nested = self.nested_repo_prefixes();
        if nested.is_empty() {
            return Ok(());
        }
        let gm_path = self.root.join(".gitmodules");
        let mut gm = std::fs::read_to_string(&gm_path).unwrap_or_default();
        let mut cfg = self.repo.config().map_err(|e| Self::map_commit(&e))?;
        for prefix in nested {
            let name = prefix.trim_end_matches('/'); // vault-rel path == submodule name
            // Match the `path = <name>` declaration on its OWN line, not as a
            // substring — otherwise a prefix-sharing path (`a/sub` inside the
            // `a/sub-extra` stanza) is wrongly seen as already-declared, gets no
            // stanza/url, yet `stage_all` still adds its gitlink, so a fresh
            // clone's `submodule update --init` can't resolve the URL.
            if Self::gitmodules_declares_path(&gm, name) {
                continue;
            }
            let url = self.nested_origin_url(name);
            if !gm.is_empty() && !gm.ends_with('\n') {
                gm.push('\n');
            }
            gm.push_str(&format!("[submodule \"{name}\"]\n\tpath = {name}\n\turl = {url}\n"));
            cfg.set_str(&format!("submodule.{name}.url"), &url)
                .map_err(|e| Self::map_commit(&e))?;
        }
        std::fs::write(&gm_path, gm)
            .map_err(|e| GitError::Commit(format!("write .gitmodules: {e}")))?;
        Ok(())
    }

    /// Whether `.gitmodules` already declares a submodule whose `path` is exactly
    /// `name`. Each line is trimmed of leading indentation (`.gitmodules` keys
    /// are tab/space-indented) and matched as a whole `path = <name>` entry, so a
    /// prefix-sharing path (`a/sub` vs `a/sub-extra`) never collides.
    /// [git-nested-repo-submodule]
    fn gitmodules_declares_path(gm: &str, name: &str) -> bool {
        gm.lines().any(|line| {
            line.trim()
                .strip_prefix("path")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
                .is_some_and(|val| val.trim() == name)
        })
    }

    /// The `origin` URL of the nested repo at vault-relative `rel`, or a
    /// relative-path fallback (`./<rel>`) when it has none — enough to declare
    /// the submodule; the user repoints the URL for cross-machine clone.
    fn nested_origin_url(&self, rel: &str) -> String {
        Repository::open(self.root.join(rel))
            .ok()
            .and_then(|nested| {
                nested
                    .find_remote("origin")
                    .ok()
                    .and_then(|r| r.url().map(str::to_string))
            })
            .unwrap_or_else(|| format!("./{rel}"))
    }

    /// Initialize + checkout every declared submodule (`git submodule update
    /// --init`) — for populating submodules after a fresh clone or a pull that
    /// advanced a gitlink. Best-effort per submodule. A clone with empty
    /// submodule dirs (the CODE-IN-VAULT broken state) is repaired by this.
    /// [git-nested-repo-submodule]
    pub fn update_submodules(&self) -> Result<()> {
        for mut sm in self.repo.submodules().map_err(|e| Self::map_commit(&e))? {
            sm.update(true, None).map_err(|e| Self::map_commit(&e))?;
        }
        Ok(())
    }

    /// CONSERVATIVE restore for vault-open: init + checkout ONLY the declared
    /// submodules whose working tree is uninitialized (an empty gitlink dir — the
    /// freshly-cloned CODE-IN-VAULT broken state). A populated or dirty submodule
    /// is left strictly alone, so the user's nested-repo work is never clobbered
    /// by reopening the vault. Returns the count of submodules populated.
    ///
    /// Uninitialized-vs-populated is read from libgit2's per-submodule status
    /// (`is_wd_uninitialized` ⇒ the workdir is an empty placeholder); we ignore
    /// dirty-tracking detail (`SubmoduleIgnore::Dirty`) since we only care about
    /// the populated-yet vs not-yet distinction here. [git-nested-repo-submodule]
    pub fn restore_uninitialized_submodules(&self) -> Result<usize> {
        let mut restored = 0usize;
        for mut sm in self.repo.submodules().map_err(|e| Self::map_commit(&e))? {
            let Some(name) = sm.name().map(str::to_string) else {
                continue;
            };
            let status = self
                .repo
                .submodule_status(&name, git2::SubmoduleIgnore::Dirty)
                .map_err(|e| Self::map_commit(&e))?;
            // Only touch a submodule that has NO checkout yet. A populated or
            // dirty submodule keeps its current state — never re-checked-out.
            if !status.is_wd_uninitialized() {
                continue;
            }
            sm.update(true, None).map_err(|e| Self::map_commit(&e))?;
            restored += 1;
        }
        Ok(restored)
    }

    /// Vault-relative paths of every git submodule the vault repo declares
    /// (`.gitmodules` + config). Empty when none are tracked. For the
    /// orchestration / a settings surface to show what's folded in, and the
    /// observable signal that registration took. [git-nested-repo-submodule]
    pub fn submodule_paths(&self) -> Result<Vec<String>> {
        let subs = self.repo.submodules().map_err(|e| Self::map_commit(&e))?;
        Ok(subs
            .iter()
            .filter_map(|s| s.path().to_str().map(str::to_string))
            .collect())
    }

    /// Fetch `remote` and let GIT reconcile the inbound head, producing correct
    /// commit topology (`git-merge-via-git`). Drives the standard merge state
    /// machine via `merge_analysis`:
    ///
    /// - up-to-date → [`MergeOutcome::UpToDate`].
    /// - fast-forward → advance the branch ref + checkout → [`MergeOutcome::Merged`].
    /// - normal 3-way → `repo.merge` populates the index + working tree; on a
    ///   clean index write a **2-parent** merge commit ([local HEAD, fetched])
    ///   and `cleanup_state` → [`MergeOutcome::Merged`]; on a conflicted index
    ///   leave `MERGE_HEAD` set + zdiff3 markers on disk, NO commit →
    ///   [`MergeOutcome::Conflicted`] (the marker resolver finalizes later).
    ///
    /// The merge commit is authored with `trailers` (the same `signature` +
    /// trailer formatting `commit_paths` uses). `git2`-free outcome.
    // status: git-merge-via-git
    pub fn fetch_and_merge(&self, remote: &str, trailers: &Trailers) -> Result<MergeOutcome> {
        let Some(fetched) = self.fetch_head_commit(remote)? else {
            return Ok(MergeOutcome::UpToDate);
        };
        let (analysis, _pref) =
            self.repo.merge_analysis(&[&fetched]).map_err(|e| Self::map_commit(&e))?;

        if analysis.is_up_to_date() {
            return Ok(MergeOutcome::UpToDate);
        }
        if analysis.is_fast_forward() {
            return self.fast_forward_to(&fetched).map(MergeOutcome::Merged);
        }
        // Normal 3-way: let git populate the index + working tree.
        self.repo
            .merge(&[&fetched], None, None)
            .map_err(|e| Self::map_commit(&e))?;
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        if index.has_conflicts() {
            // git already wrote MERGE_HEAD; leave markers on disk, no commit.
            return Ok(MergeOutcome::Conflicted(Self::conflict_paths(&index)?));
        }
        let fetched_commit =
            self.repo.find_commit(fetched.id()).map_err(|e| Self::map_read(&e))?;
        let sha = self.commit_merge(&mut index, &fetched_commit, trailers)?;
        self.repo.cleanup_state().map_err(|e| Self::map_commit(&e))?;
        Ok(MergeOutcome::Merged(sha))
    }

    /// Finalize an in-progress merge after the user resolved conflicts on disk
    /// (`git-conflict-inline-markers`). Stages the resolved working tree, errors
    /// if conflicts remain, then writes the **2-parent** commit ([HEAD,
    /// MERGE_HEAD]) with `trailers` and `cleanup_state`s. Returns the sha, or
    /// `Ok(None)` when there is nothing to commit.
    pub fn finalize_merge(&self, trailers: &Trailers) -> Result<Option<String>> {
        let merge_head = self.merge_head_commit()?.ok_or_else(|| {
            GitError::Commit("finalize_merge with no MERGE_HEAD".into())
        })?;
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        // Stage the resolved working tree (the user's edits removing markers).
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(|e| Self::map_commit(&e))?;
        index.write().map_err(|e| Self::map_commit(&e))?;
        if index.has_conflicts() {
            return Err(GitError::NeedsMerge(Self::conflict_paths(&index)?));
        }
        let head = self.head_commit()?;
        let tree_oid = index.write_tree().map_err(|e| Self::map_commit(&e))?;
        // Nothing to commit if the tree already matches HEAD's.
        if let Some(parent) = &head {
            let parent_tree = parent.tree().map_err(|e| Self::map_commit(&e))?;
            if parent_tree.id() == tree_oid {
                self.repo.cleanup_state().map_err(|e| Self::map_commit(&e))?;
                return Ok(None);
            }
        }
        let sha = self.write_merge_commit(tree_oid, head.as_ref(), &merge_head, trailers)?;
        self.repo.cleanup_state().map_err(|e| Self::map_commit(&e))?;
        Ok(Some(sha))
    }

    /// Abort an in-progress merge (the `git merge --abort` equivalent): reset
    /// hard to HEAD and clear the merge state. Discards the partially-merged
    /// working tree.
    pub fn abort_merge(&self) -> Result<()> {
        let head = self
            .head_commit()?
            .ok_or_else(|| GitError::Commit("abort_merge with no HEAD".into()))?;
        self.repo
            .reset(head.as_object(), git2::ResetType::Hard, None)
            .map_err(|e| Self::map_commit(&e))?;
        self.repo.cleanup_state().map_err(|e| Self::map_commit(&e))
    }

    /// Whether a merge is in progress (`MERGE_HEAD` exists).
    pub fn merge_in_progress(&self) -> Result<bool> {
        Ok(self.merge_head_oid()?.is_some())
    }

    /// Vault-relative paths of the index's conflicted entries (empty when no
    /// merge is in progress / no conflicts remain).
    pub fn merge_conflict_paths(&self) -> Result<Vec<String>> {
        let index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        Self::conflict_paths(&index)
    }

    /// Commit exactly what is already STAGED (the current index), without
    /// re-staging the working tree — the Source-Control "Commit" button
    /// (`git commit`, no `-a`). `subject` is the commit subject; `trailers`
    /// carry the `Hiker-Author`. `amend` replaces HEAD. Returns the new sha, or
    /// `Ok(None)` when the index tree already equals HEAD's (nothing staged to
    /// commit). [git-staging-ops]
    pub fn commit_index(
        &self,
        subject: &str,
        trailers: &Trailers,
        amend: bool,
    ) -> Result<Option<String>> {
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        let tree_oid = index.write_tree().map_err(|e| Self::map_commit(&e))?;
        let message = format!("{subject}{}", trailers.render());
        self.write_commit(tree_oid, &message, amend)
    }

    /// The STAGED changes — paths whose index entry differs from HEAD (`git
    /// diff --cached`), each with how it changed. This is the Source-Control
    /// view's "Staged Changes" group. On an unborn branch (no HEAD) every
    /// indexed path reads as `Added`. `.hiker/` is excluded. Renames are
    /// detected and reported at the new path. [git-staging-ops]
    pub fn diff_tree_to_index(&self) -> Result<Vec<(String, ChangeStatus)>> {
        let head_tree = match self.head_commit()? {
            Some(c) => Some(c.tree().map_err(|e| Self::map_read(&e))?),
            None => None,
        };
        let index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        let mut diff = self
            .repo
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
            .map_err(|e| Self::map_read(&e))?;
        diff.find_similar(None).map_err(|e| Self::map_read(&e))?;
        Ok(Self::collect_changed(&diff)?
            .into_iter()
            .filter(|(p, _)| !p.starts_with(".hiker/"))
            .collect())
    }

    /// The UNSTAGED changes — paths whose working tree differs from the index
    /// (`git diff`), each with how it changed. This is the Source-Control
    /// view's "Changes" group. Untracked files are included (read as `Added`),
    /// `.hiker/` is excluded, and renames are detected at the new path.
    /// [git-staging-ops]
    pub fn diff_index_to_workdir(&self) -> Result<Vec<(String, ChangeStatus)>> {
        let index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let mut diff = self
            .repo
            .diff_index_to_workdir(Some(&index), Some(&mut opts))
            .map_err(|e| Self::map_read(&e))?;
        diff.find_similar(None).map_err(|e| Self::map_read(&e))?;
        Ok(Self::collect_changed(&diff)?
            .into_iter()
            .filter(|(p, _)| !p.starts_with(".hiker/"))
            .collect())
    }

    /// Per-submodule status rows for every declared submodule — what the
    /// Source-Control view surfaces (`uninitialized` / `dirty` / `advanced`).
    /// Best-effort: a submodule whose status can't be read is reported with all
    /// flags `false` so it still appears as a row. [git-nested-repo-submodule]
    pub fn submodule_status_rows(&self) -> Result<Vec<SubmoduleStatus>> {
        let mut rows = Vec::new();
        for sm in self.repo.submodules().map_err(|e| Self::map_commit(&e))? {
            let Some(name) = sm.name().map(str::to_string) else {
                continue;
            };
            let path = sm.path().to_string_lossy().replace('\\', "/");
            let status = self
                .repo
                .submodule_status(&name, git2::SubmoduleIgnore::None)
                .ok();
            let (uninitialized, dirty, advanced) = match status {
                Some(s) => (
                    s.is_wd_uninitialized(),
                    // The nested repo has uncommitted work: modified tracked
                    // files or untracked content in the submodule's workdir.
                    s.is_wd_wd_modified() || s.is_wd_untracked(),
                    // The checked-out submodule HEAD differs from the gitlink
                    // the vault commit pins (the submodule moved forward/back).
                    s.is_wd_modified(),
                ),
                None => (false, false, false),
            };
            rows.push(SubmoduleStatus { path, uninitialized, dirty, advanced });
        }
        Ok(rows)
    }

    /// The parent commit shas of `rev` (in order), for inspecting commit
    /// topology — a merge commit has two. Resolves `rev` like
    /// [`GitBackend::show`].
    pub fn parent_shas(&self, rev: &str) -> Result<Vec<String>> {
        let commit = self.resolve_commit(rev)?;
        Ok(commit.parent_ids().map(|id| id.to_string()).collect())
    }

    /// The current branch name + ahead/behind counts vs the configured upstream
    /// (`@{u}`), for the Source-Control header. A detached HEAD / unborn branch
    /// reports `branch = None`; a branch with no upstream reports
    /// `has_upstream = false` and zero counts. [git-branch-status]
    pub fn branch_status(&self) -> Result<BranchStatus> {
        // The branch HEAD points at (None on detached / unborn).
        let head = match self.repo.head() {
            Ok(h) => h,
            // Unborn branch (no commits yet) — report the symbolic branch name
            // with no upstream rather than failing.
            Err(e)
                if e.code() == git2::ErrorCode::UnbornBranch
                    || e.code() == git2::ErrorCode::NotFound =>
            {
                return Ok(BranchStatus {
                    branch: self.unborn_branch_name(),
                    ahead: 0,
                    behind: 0,
                    has_upstream: false,
                });
            }
            Err(e) => return Err(Self::map_read(&e)),
        };
        if !head.is_branch() {
            // Detached HEAD: no branch, no upstream to compare against.
            return Ok(BranchStatus { branch: None, ahead: 0, behind: 0, has_upstream: false });
        }
        let branch_name = head.shorthand().map(str::to_string);
        let local_oid = match head.target() {
            Some(oid) => oid,
            None => {
                return Ok(BranchStatus {
                    branch: branch_name,
                    ahead: 0,
                    behind: 0,
                    has_upstream: false,
                });
            }
        };

        // The upstream of this branch, if configured.
        let Some(short) = head.shorthand() else {
            return Ok(BranchStatus { branch: branch_name, ahead: 0, behind: 0, has_upstream: false });
        };
        let branch = self.repo.find_branch(short, git2::BranchType::Local).map_err(|e| Self::map_read(&e))?;
        let upstream = match branch.upstream() {
            Ok(u) => u,
            // No upstream configured for this branch.
            Err(_) => {
                return Ok(BranchStatus { branch: branch_name, ahead: 0, behind: 0, has_upstream: false });
            }
        };
        let Some(upstream_oid) = upstream.get().target() else {
            return Ok(BranchStatus { branch: branch_name, ahead: 0, behind: 0, has_upstream: true });
        };

        let (ahead, behind) = self
            .repo
            .graph_ahead_behind(local_oid, upstream_oid)
            .map_err(|e| Self::map_read(&e))?;
        Ok(BranchStatus { branch: branch_name, ahead, behind, has_upstream: true })
    }

    /// Stage `paths` (vault-relative) into the index — the `git add <path>`
    /// equivalent. A path that is gone on disk stages as a delete. Writes the
    /// index; no commit. [git-staging-ops]
    pub fn stage_paths(&self, paths: &[String]) -> Result<()> {
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        for p in paths {
            let rel = Path::new(p);
            if self.root.join(rel).exists() {
                index.add_path(rel).map_err(|e| Self::map_commit(&e))?;
            } else {
                index.remove_path(rel).map_err(|e| Self::map_commit(&e))?;
            }
        }
        index.write().map_err(|e| Self::map_commit(&e))
    }

    /// Unstage `paths` (vault-relative) — the `git reset HEAD -- <path>` /
    /// `git restore --staged` equivalent: reset each path's index entry to its
    /// HEAD state (or remove it from the index entirely when the path doesn't
    /// exist at HEAD, i.e. a newly-added file). The working tree is untouched.
    /// On an unborn branch (no HEAD) every path is removed from the index.
    /// [git-staging-ops]
    pub fn unstage_paths(&self, paths: &[String]) -> Result<()> {
        let head = self.head_commit()?;
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        let head_tree = match &head {
            Some(c) => Some(c.tree().map_err(|e| Self::map_read(&e))?),
            None => None,
        };
        for p in paths {
            let rel = Path::new(p);
            // The path's blob at HEAD, if it exists there.
            let head_entry = head_tree
                .as_ref()
                .and_then(|t| t.get_path(rel).ok());
            match head_entry {
                Some(entry) => {
                    // Restore the index entry to its HEAD blob (mode + oid).
                    let ie = git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: entry.filemode() as u32,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: entry.id(),
                        flags: 0,
                        flags_extended: 0,
                        path: p.replace('\\', "/").into_bytes(),
                    };
                    index.add(&ie).map_err(|e| Self::map_commit(&e))?;
                }
                // Not at HEAD — a staged add; drop it from the index.
                None => {
                    index.remove_path(rel).map_err(|e| Self::map_commit(&e))?;
                }
            }
        }
        index.write().map_err(|e| Self::map_commit(&e))
    }

    /// Discard working-tree changes to `paths` — the `git checkout -- <path>` /
    /// `git restore <path>` equivalent: overwrite each path on disk with its
    /// HEAD content (deleting it when it doesn't exist at HEAD). Destructive:
    /// uncommitted edits to those paths are lost. [git-staging-ops]
    pub fn discard_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let head = self
            .head_commit()?
            .ok_or_else(|| GitError::Commit("discard with no HEAD".into()))?;
        let head_tree = head.tree().map_err(|e| Self::map_read(&e))?;
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        let mut co = git2::build::CheckoutBuilder::new();
        co.force().remove_untracked(false).update_index(true);
        for p in paths {
            // Drop any staged version first so the index matches HEAD for the
            // path, then check the HEAD tree's content back out over the workdir.
            let rel = Path::new(p);
            match head_tree.get_path(rel) {
                Ok(entry) => {
                    let ie = git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: entry.filemode() as u32,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: entry.id(),
                        flags: 0,
                        flags_extended: 0,
                        path: p.replace('\\', "/").into_bytes(),
                    };
                    index.add(&ie).map_err(|e| Self::map_commit(&e))?;
                }
                // Not at HEAD — discarding means removing the working file.
                Err(_) => {
                    let _ = std::fs::remove_file(self.root.join(rel));
                    let _ = index.remove_path(rel);
                }
            }
            co.path(p.as_str());
        }
        index.write().map_err(|e| Self::map_commit(&e))?;
        self.repo
            .checkout_head(Some(&mut co))
            .map_err(|e| Self::map_commit(&e))
    }

    /// Stage a single unified-diff hunk into the index — the `git apply
    /// --cached <hunk>` equivalent. `patch` is a complete unified diff (a `diff
    /// --git` / `---` / `+++` header plus one `@@` hunk) describing a forward
    /// change (HEAD → working). Applies it to the index only; the working tree
    /// is untouched. Per-hunk staging in the diff view. [git-staging-ops]
    pub fn stage_hunk(&self, patch: &str) -> Result<()> {
        self.apply_patch(patch, ApplyLocation::Index)
    }

    /// Unstage a single hunk from the index — the `git apply --cached -R
    /// <hunk>` equivalent: reverse-apply the forward `patch` to the index, so
    /// only that hunk's staged lines are dropped back to their HEAD state while
    /// the rest of the staged file is left intact. The working tree is
    /// untouched. [git-staging-ops]
    pub fn unstage_hunk(&self, patch: &str) -> Result<()> {
        let reversed = crate::hunk::reverse_patch(patch)?;
        self.apply_patch(&reversed, ApplyLocation::Index)
    }

    /// Discard a single hunk from the WORKING TREE — the `git apply -R <hunk>`
    /// equivalent: reverse-apply the forward `patch` to the workdir so that
    /// hunk's edit is reverted on disk while the rest of the file's
    /// uncommitted edits survive. Destructive for that hunk only. The index is
    /// untouched. [git-staging-ops]
    pub fn discard_hunk(&self, patch: &str) -> Result<()> {
        let reversed = crate::hunk::reverse_patch(patch)?;
        self.apply_patch(&reversed, ApplyLocation::WorkDir)
    }

    /// Parse `patch` (a unified diff) into a libgit2 `Diff` and apply it at
    /// `location`. The patch is byte-exact: libgit2's apply matches the hunk's
    /// context against the target, so a stale patch (the file moved on) fails
    /// cleanly with [`GitError::Apply`] rather than corrupting the file.
    fn apply_patch(&self, patch: &str, location: ApplyLocation) -> Result<()> {
        let diff = Diff::from_buffer(patch.as_bytes())
            .map_err(|e| GitError::Apply(format!("parse patch: {}", e.message())))?;
        self.repo
            .apply(&diff, location, None)
            .map_err(|e| GitError::Apply(e.message().to_string()))
    }

    /// Advance the branch ref (and HEAD's working tree) to `target` — the
    /// fast-forward case of [`fetch_and_merge`]. Returns the new sha.
    fn fast_forward_to(&self, target: &AnnotatedCommit) -> Result<String> {
        let target_commit =
            self.repo.find_commit(target.id()).map_err(|e| Self::map_read(&e))?;
        // On an unborn branch (a fresh local repo whose first merge is really a
        // "seed from remote") `repo.head()` fails — resolve the branch ref name
        // HEAD symbolically points at instead, and create it.
        let refname = self.branch_refname();
        match self.repo.find_reference(&refname) {
            Ok(mut reference) => {
                reference
                    .set_target(target.id(), "fast-forward")
                    .map_err(|e| Self::map_commit(&e))?;
            }
            Err(_) => {
                self.repo
                    .reference(&refname, target.id(), true, "fast-forward")
                    .map_err(|e| Self::map_commit(&e))?;
            }
        }
        self.repo.set_head(&refname).map_err(|e| Self::map_commit(&e))?;
        self.repo
            .checkout_head(Some(git2::build::CheckoutBuilder::default().force()))
            .map_err(|e| Self::map_commit(&e))?;
        Ok(target_commit.id().to_string())
    }

    /// The fully-qualified branch ref name HEAD points at, working on both a
    /// born branch (`repo.head()`) and an unborn one (the symbolic target of
    /// `HEAD`, defaulting to `refs/heads/master`).
    fn branch_refname(&self) -> String {
        if let Ok(head) = self.repo.head()
            && let Some(name) = head.name()
        {
            return name.to_string();
        }
        // Unborn branch: read where HEAD symbolically points.
        match self.repo.find_reference("HEAD") {
            Ok(head) => head
                .symbolic_target()
                .unwrap_or("refs/heads/master")
                .to_string(),
            Err(_) => "refs/heads/master".to_string(),
        }
    }

    /// The short branch name HEAD symbolically points at on an unborn branch
    /// (e.g. `main`/`master`), or `None` if it can't be read.
    fn unborn_branch_name(&self) -> Option<String> {
        self.repo
            .find_reference("HEAD")
            .ok()
            .and_then(|h| h.symbolic_target().map(str::to_string))
            .map(|t| t.strip_prefix("refs/heads/").unwrap_or(&t).to_string())
    }

    /// Write the merged tree from `index` and create the 2-parent merge commit
    /// ([local HEAD, fetched]) with `trailers`. Returns the new sha.
    fn commit_merge(
        &self,
        index: &mut git2::Index,
        fetched: &git2::Commit,
        trailers: &Trailers,
    ) -> Result<String> {
        let tree_oid = index.write_tree().map_err(|e| Self::map_commit(&e))?;
        let head = self.head_commit()?;
        self.write_merge_commit(tree_oid, head.as_ref(), fetched, trailers)
    }

    /// Create a 2-parent commit at `tree_oid` with parents `[local, other]`
    /// (`local` omitted on an unborn branch), authored with `trailers`. Reuses
    /// the same `signature` + trailer-message formatting as `commit_paths`.
    fn write_merge_commit(
        &self,
        tree_oid: git2::Oid,
        local: Option<&git2::Commit>,
        other: &git2::Commit,
        trailers: &Trailers,
    ) -> Result<String> {
        let sig = self.signature()?;
        let tree = self.repo.find_tree(tree_oid).map_err(|e| Self::map_commit(&e))?;
        let message = format!("Merge remote changes{}", trailers.render());
        let mut parents: Vec<&git2::Commit> = Vec::new();
        if let Some(local) = local {
            parents.push(local);
        }
        parents.push(other);
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, &message, &tree, &parents)
            .map_err(|e| Self::map_commit(&e))?;
        Ok(oid.to_string())
    }

    /// The `MERGE_HEAD` oid, or `None` when no merge is in progress.
    fn merge_head_oid(&self) -> Result<Option<git2::Oid>> {
        match self.repo.find_reference("MERGE_HEAD") {
            Ok(r) => Ok(r.target()),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(Self::map_read(&e)),
        }
    }

    /// The `MERGE_HEAD` commit, or `None` when no merge is in progress.
    fn merge_head_commit(&self) -> Result<Option<git2::Commit<'_>>> {
        match self.merge_head_oid()? {
            Some(oid) => self
                .repo
                .find_commit(oid)
                .map(Some)
                .map_err(|e| Self::map_read(&e)),
            None => Ok(None),
        }
    }

    /// Vault-relative (forward-slash) paths of an index's conflicted entries.
    fn conflict_paths(index: &git2::Index) -> Result<Vec<String>> {
        let conflicts = index.conflicts().map_err(|e| Self::map_commit(&e))?;
        let mut paths = Vec::new();
        for c in conflicts {
            let c = c.map_err(|e| Self::map_commit(&e))?;
            // Prefer the "our"/"their" side path; fall back to the ancestor.
            let entry = c.our.or(c.their).or(c.ancestor);
            if let Some(entry) = entry {
                let path = String::from_utf8_lossy(&entry.path).replace('\\', "/");
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Whole-tree stage in **submodule mode**: declare nested repos in
    /// `.gitmodules`, stage everything EXCEPT their subtrees, then add each as
    /// an explicit gitlink at its current HEAD. We don't let `add_all` walk
    /// into a nested repo (libgit2's submodule-add is for *new* submodules and
    /// rejects an already-present checkout — `invalid path`); the gitlink index
    /// entry adopts the existing repo directly. The pointer travels with the
    /// vault commit. [git-nested-repo-submodule]
    fn stage_all_with_submodules(&self, index: &mut git2::Index) -> Result<()> {
        self.ensure_submodules_registered()?;
        let nested = self.nested_repo_prefixes();
        let mut skip_nested = |p: &Path, _: &[u8]| -> i32 {
            let rel = p.to_string_lossy().replace('\\', "/");
            i32::from(nested.iter().any(|pre| rel.starts_with(pre.as_str())))
        };
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, Some(&mut skip_nested))
            .map_err(|e| Self::map_commit(&e))?;
        for prefix in &nested {
            self.add_gitlink(index, prefix.trim_end_matches('/'))?;
        }
        Ok(())
    }

    /// Add a gitlink index entry (mode `160000`) at vault-relative `name`
    /// pointing at the nested repo's current HEAD commit — the libgit2-robust
    /// way to record a submodule pointer for an already-present checkout
    /// (`Repository::submodule` only sets up *new* ones). [git-nested-repo-submodule]
    fn add_gitlink(&self, index: &mut git2::Index, name: &str) -> Result<()> {
        let nested = Repository::open(self.root.join(name)).map_err(|e| Self::map_commit(&e))?;
        let head = nested
            .head()
            .and_then(|r| r.peel_to_commit())
            .map_err(|e| Self::map_commit(&e))?
            .id();
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160_000, // GIT_FILEMODE_COMMIT — a gitlink, not a blob/tree
            uid: 0,
            gid: 0,
            file_size: 0,
            id: head,
            flags: 0,
            flags_extended: 0,
            path: name.as_bytes().to_vec(),
        };
        index.add(&entry).map_err(|e| Self::map_commit(&e))
    }

    /// Whole-tree stage in **skip mode**: exclude any nested repo subtree so a
    /// whole-tree `add_all` doesn't swallow it as a gitlink. The nested repo
    /// stays independent (managed via its own remote). [git-nested-repo-submodule]
    fn stage_all_skipping_nested(&self, index: &mut git2::Index) -> Result<()> {
        let nested = self.nested_repo_prefixes();
        if nested.is_empty() {
            return index
                .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
                .map_err(|e| Self::map_commit(&e));
        }
        tracing::warn!(
            repos = ?nested,
            "git transport: excluding nested repo(s) from whole-tree stage \
             (set `[git] submodules = \"submodule\"` to fold them in as submodules)",
        );
        let mut skip_nested = |p: &Path, _: &[u8]| -> i32 {
            let rel = p.to_string_lossy().replace('\\', "/");
            i32::from(nested.iter().any(|pre| rel.starts_with(pre.as_str())))
        };
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, Some(&mut skip_nested))
            .map_err(|e| Self::map_commit(&e))
    }

    /// Stage `paths` into the index and write the tree. Returns the tree oid.
    /// An empty `paths` stages the whole working tree (amend-coalesce paths),
    /// dispatching on the submodule policy; an explicit list stages exactly
    /// those (a missing path is a staged delete).
    fn stage_tree(&self, paths: &[String]) -> Result<git2::Oid> {
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        if paths.is_empty() {
            if self.track_submodules {
                self.stage_all_with_submodules(&mut index)?;
            } else {
                self.stage_all_skipping_nested(&mut index)?;
            }
        } else {
            for p in paths {
                let rel = Path::new(p);
                if self.root.join(rel).exists() {
                    index.add_path(rel).map_err(|e| Self::map_commit(&e))?;
                } else {
                    // A staged delete: the file is gone on disk.
                    index.remove_path(rel).map_err(|e| Self::map_commit(&e))?;
                }
            }
        }
        index.write().map_err(|e| Self::map_commit(&e))?;
        index.write_tree().map_err(|e| Self::map_commit(&e))
    }

    /// Write a commit with `tree_oid` and `message`. `amend` replaces HEAD;
    /// otherwise HEAD (if any) is the sole parent. Returns the new sha, or
    /// `None` when the tree is identical to the parent's (nothing to commit).
    fn write_commit(
        &self,
        tree_oid: git2::Oid,
        message: &str,
        amend: bool,
    ) -> Result<Option<String>> {
        let sig = self.signature()?;
        let tree = self.repo.find_tree(tree_oid).map_err(|e| Self::map_commit(&e))?;
        let head = self.head_commit()?;

        // No-op guard: if the new tree equals the parent tree (and we're not
        // amending to change the message), there's nothing to commit.
        if !amend
            && let Some(parent) = &head
        {
            let parent_tree = parent.tree().map_err(|e| Self::map_commit(&e))?;
            if parent_tree.id() == tree_oid {
                return Ok(None);
            }
        }

        let oid = if amend {
            let head = head.ok_or_else(|| GitError::Commit("amend with no HEAD".into()))?;
            head.amend(Some("HEAD"), Some(&sig), Some(&sig), None, Some(message), Some(&tree))
                .map_err(|e| Self::map_commit(&e))?
        } else {
            let parents: Vec<&git2::Commit> = head.iter().collect();
            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
                .map_err(|e| Self::map_commit(&e))?
        };
        Ok(Some(oid.to_string()))
    }

    /// The current HEAD commit, or `None` on an unborn branch.
    fn head_commit(&self) -> Result<Option<git2::Commit<'_>>> {
        match self.repo.head() {
            Ok(head) => {
                let commit = head
                    .peel_to_commit()
                    .map_err(|e| Self::map_read(&e))?;
                Ok(Some(commit))
            }
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch
                || e.code() == git2::ErrorCode::NotFound =>
            {
                Ok(None)
            }
            Err(e) => Err(Self::map_read(&e)),
        }
    }

    /// The vault-relative paths whose blobs differ between two trees.
    fn changed_paths_between(
        &self,
        old: Option<&git2::Tree>,
        new: Option<&git2::Tree>,
    ) -> Result<Vec<String>> {
        let diff = self
            .repo
            .diff_tree_to_tree(old, new, None)
            .map_err(|e| Self::map_read(&e))?;
        Ok(Self::collect_changed(&diff)?.into_iter().map(|(p, _)| p).collect())
    }

    /// Flatten a computed diff into sorted, deduped `(path, status)` rows. A
    /// rename reports at its new path; non-change deltas (unmodified /
    /// ignored / unreadable) are dropped.
    fn collect_changed(diff: &git2::Diff) -> Result<Vec<(String, ChangeStatus)>> {
        let mut rows = Vec::new();
        diff.foreach(
            &mut |delta, _| {
                if let (Some(p), Some(status)) = (
                    delta.new_file().path().or_else(|| delta.old_file().path()),
                    ChangeStatus::from_delta(delta.status()),
                ) {
                    rows.push((p.to_string_lossy().into_owned(), status));
                }
                true
            },
            None,
            None,
            None,
        )
        .map_err(|e| Self::map_read(&e))?;
        rows.sort();
        rows.dedup();
        Ok(rows)
    }

    /// Resolve `rev` — anything `git rev-parse` accepts (`HEAD`, a full or
    /// short sha, a ref name) — to the commit it names.
    fn resolve_commit(&self, rev: &str) -> Result<git2::Commit<'_>> {
        let obj = self
            .repo
            .revparse_single(rev)
            .map_err(|_| GitError::InvalidPath(format!("unknown rev {rev}")))?;
        obj.peel_to_commit()
            .map_err(|_| GitError::InvalidPath(format!("rev {rev} is not a commit")))
    }

    /// Fetch `remote`'s default-branch refspecs and return the fetched head as
    /// an annotated commit (`None` when there is no `FETCH_HEAD` after the
    /// fetch — an empty remote). Shared by `pull` and `fetch_and_merge` so the
    /// fetch logic lives in one place. Errors map to [`GitError::Pull`].
    fn fetch_head_commit(&self, remote: &str) -> Result<Option<AnnotatedCommit<'_>>> {
        if remote.is_empty() {
            return Err(GitError::NoRemote);
        }
        let mut rmt = self
            .repo
            .remote_anonymous(remote)
            .or_else(|_| self.repo.find_remote(remote))
            .map_err(|e| GitError::Pull(e.message().to_string()))?;
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(self.remote_callbacks());
        // Fetch the default branch refspecs.
        let refspecs: Vec<String> = rmt
            .fetch_refspecs()
            .map(|s| s.iter().flatten().map(str::to_string).collect())
            .unwrap_or_default();
        let specs: Vec<&str> = if refspecs.is_empty() {
            vec!["refs/heads/*:refs/remotes/origin/*"]
        } else {
            refspecs.iter().map(String::as_str).collect()
        };
        rmt.fetch(&specs, Some(&mut fo), None)
            .map_err(|e| GitError::Pull(e.message().to_string()))?;

        let fetch_head = match self.repo.find_reference("FETCH_HEAD") {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        self.repo
            .reference_to_annotated_commit(&fetch_head)
            .map(Some)
            .map_err(|e| Self::map_read(&e))
    }

    /// Build the credential + progress callbacks for a remote operation. Uses
    /// the user's git credential helper + SSH agent (the same auth a plain
    /// `git push` uses) — hiker never stores credentials.
    fn remote_callbacks<'a>(&self) -> RemoteCallbacks<'a> {
        let mut cb = RemoteCallbacks::new();
        cb.credentials(|url, username, allowed| {
            if allowed.is_ssh_key()
                && let Some(user) = username
            {
                return Cred::ssh_key_from_agent(user);
            }
            if allowed.is_user_pass_plaintext() {
                // Defer to the configured credential helper.
                let cfg = git2::Config::open_default()?;
                return Cred::credential_helper(&cfg, url, username);
            }
            if allowed.is_default() {
                return Cred::default();
            }
            Cred::default()
        });
        cb
    }
}

/// Recursively record nested-repo directory prefixes under `root`. A directory
/// containing a `.git` entry (other than `root` itself) is a nested repo: push
/// its vault-relative path with a trailing `/` and DO NOT descend into it; any
/// `.git` directory is likewise never entered. Unreadable directories are
/// skipped (best-effort, mirroring the staging path's tolerance).
fn collect_nested_repos(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || entry.file_name() == ".git" {
            continue;
        }
        if path.join(".git").exists() {
            // A nested repository: record its prefix and prune (don't descend).
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(format!("{}/", rel.to_string_lossy().replace('\\', "/")));
            }
        } else {
            collect_nested_repos(root, &path, out);
        }
    }
}

impl GitBackend for Libgit2Backend {
    fn open_or_init(vault_root: &Path) -> Result<Self> {
        let repo = match Repository::open(vault_root) {
            Ok(r) => r,
            Err(_) => Repository::init(vault_root).map_err(|e| Self::map_open(&e))?,
        };
        let backend = Self { repo, root: vault_root.to_path_buf(), track_submodules: false };
        // Conflict markers include the base section (`|||||||`) so the inline
        // resolver has context (`git-conflict-inline-markers`).
        backend
            .repo
            .config()
            .map_err(|e| Self::map_open(&e))?
            .set_str("merge.conflictStyle", "zdiff3")
            .map_err(|e| Self::map_open(&e))?;
        Ok(backend)
    }

    fn ensure_hiker_ignored(&self) -> Result<()> {
        let gitignore = self.root.join(".gitignore");
        let rule = ".hiker/";
        let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == rule) {
            return Ok(());
        }
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(rule);
        updated.push('\n');
        std::fs::write(&gitignore, updated)
            .map_err(|e| GitError::Open(format!("write .gitignore: {e}")))?;
        Ok(())
    }

    fn commit_paths(
        &self,
        paths: &[String],
        subject: &str,
        trailers: &Trailers,
        amend: bool,
    ) -> Result<Option<String>> {
        let tree_oid = self.stage_tree(paths)?;
        let message = format!("{subject}{}", trailers.render());
        self.write_commit(tree_oid, &message, amend)
    }

    fn commit_rename(
        &self,
        from: &str,
        to: &str,
        trailers: &Trailers,
    ) -> Result<Option<String>> {
        // The caller moved the file on disk already; stage both the removed old
        // path and the added new path so the tree records delete-old + add-new
        // (git's rename representation). `-M`/`--follow` recover the move at
        // read time because the bytes are identical (`git-observed-rename-
        // commit`).
        let subject = format!("Rename {from} -> {to}");
        self.commit_paths(&[from.to_string(), to.to_string()], &subject, trailers, false)
    }

    fn pull(&self, remote: &str) -> Result<Divergence> {
        let Some(fetched) = self.fetch_head_commit(remote)? else {
            return Ok(Divergence::Unchanged);
        };
        let local_head = self.head_commit()?;
        let local_tree = match &local_head {
            Some(c) => Some(c.tree().map_err(|e| Self::map_read(&e))?),
            None => None,
        };
        let fetched_commit = self
            .repo
            .find_commit(fetched.id())
            .map_err(|e| Self::map_read(&e))?;
        let fetched_tree = fetched_commit.tree().map_err(|e| Self::map_read(&e))?;

        // Up to date?
        if local_head.as_ref().map(git2::Commit::id) == Some(fetched.id()) {
            return Ok(Divergence::Unchanged);
        }
        let changed =
            self.changed_paths_between(local_tree.as_ref(), Some(&fetched_tree))?;
        if changed.is_empty() {
            Ok(Divergence::Unchanged)
        } else {
            Ok(Divergence::Diverged { changed_paths: changed })
        }
    }

    fn push(&self, remote: &str) -> Result<()> {
        if remote.is_empty() {
            return Err(GitError::NoRemote);
        }
        let head = self.repo.head().map_err(|e| GitError::Push(e.message().to_string()))?;
        let branch = head.shorthand().unwrap_or("master");
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        let mut rmt = self
            .repo
            .remote_anonymous(remote)
            .or_else(|_| self.repo.find_remote(remote))
            .map_err(|e| GitError::Push(e.message().to_string()))?;
        let mut po = PushOptions::new();
        po.remote_callbacks(self.remote_callbacks());
        rmt.push(&[&refspec], Some(&mut po))
            .map_err(|e| GitError::Push(e.message().to_string()))
    }

    fn log(&self, limit: usize) -> Result<Vec<CommitInfo>> {
        let mut walk = self.repo.revwalk().map_err(|e| Self::map_read(&e))?;
        if walk.push_head().is_err() {
            // Unborn branch — no commits.
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for oid in walk.flatten().take(limit) {
            let commit = self.repo.find_commit(oid).map_err(|e| Self::map_read(&e))?;
            let message = commit.message().unwrap_or_default();
            let subject = message.lines().next().unwrap_or_default().to_string();
            out.push(CommitInfo {
                sha: oid.to_string(),
                subject,
                author_name: commit.author().name().unwrap_or("").to_string(),
                time_unix: commit.time().seconds(),
                trailers: Trailers::parse(message),
            });
        }
        Ok(out)
    }

    fn show(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let commit = self.resolve_commit(rev)?;
        let tree = commit.tree().map_err(|e| Self::map_read(&e))?;
        let entry = match tree.get_path(Path::new(path)) {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        let obj = entry.to_object(&self.repo).map_err(|e| Self::map_read(&e))?;
        let Some(blob) = obj.as_blob() else { return Ok(None) };
        Ok(Some(String::from_utf8_lossy(blob.content()).into_owned()))
    }

    // status: diff-paths-trait-method
    fn diff_paths(
        &self,
        base_rev: &str,
        head_rev: Option<&str>,
    ) -> Result<Vec<(String, ChangeStatus)>> {
        let base = self.resolve_commit(base_rev)?.tree().map_err(|e| Self::map_read(&e))?;
        let mut diff = match head_rev {
            // Rev ↔ rev: a plain tree-to-tree diff.
            Some(rev) => {
                let head = self.resolve_commit(rev)?.tree().map_err(|e| Self::map_read(&e))?;
                self.repo.diff_tree_to_tree(Some(&base), Some(&head), None)
            }
            // Rev ↔ working tree. Diff through the index so a staged-but-
            // uncommitted change still counts, and include untracked files so
            // a new note not yet committed reads as `Added`. Gitignore still
            // applies; `.hiker/` is filtered below as belt-and-braces (same
            // posture as `divergence_from`).
            None => {
                let mut opts = git2::DiffOptions::new();
                opts.include_untracked(true).recurse_untracked_dirs(true);
                self.repo.diff_tree_to_workdir_with_index(Some(&base), Some(&mut opts))
            }
        }
        .map_err(|e| Self::map_read(&e))?;
        // Collapse byte-similar delete+add pairs into one `Renamed` delta so a
        // moved file reads as a move, not a Deleted/Added pair.
        diff.find_similar(None).map_err(|e| Self::map_read(&e))?;
        let changed = Self::collect_changed(&diff)?;
        Ok(changed.into_iter().filter(|(p, _)| !p.starts_with(".hiker/")).collect())
    }

    fn divergence_from(&self, known_sha: Option<&str>) -> Result<Divergence> {
        let head = self.head_commit()?;
        let head_sha = head.as_ref().map(|c| c.id().to_string());

        // HEAD moved relative to the known commit?
        let head_moved = match (known_sha, &head_sha) {
            (Some(k), Some(h)) => k != h,
            (Some(_), None) => true,
            (None, _) => false,
        };

        // Working-tree dirtiness against HEAD, expressed as changed paths.
        let mut statuses_opts = git2::StatusOptions::new();
        statuses_opts.include_untracked(true).recurse_untracked_dirs(true);
        let statuses = self
            .repo
            .statuses(Some(&mut statuses_opts))
            .map_err(|e| Self::map_read(&e))?;
        let mut changed: Vec<String> = statuses
            .iter()
            .filter_map(|e| e.path().map(str::to_string))
            .filter(|p| !p.starts_with(".hiker/"))
            .collect();

        // If HEAD moved away from the known commit, the tree at the known sha
        // also differs — surface those paths too so the fold sees the full set.
        if head_moved
            && let Some(k) = known_sha
            && let Ok(oid) = git2::Oid::from_str(k)
            && let Ok(known) = self.repo.find_commit(oid)
        {
            let known_tree = known.tree().map_err(|e| Self::map_read(&e))?;
            let head_tree = match &head {
                Some(c) => Some(c.tree().map_err(|e| Self::map_read(&e))?),
                None => None,
            };
            let between = self.changed_paths_between(Some(&known_tree), head_tree.as_ref())?;
            changed.extend(between);
        }
        changed.sort();
        changed.dedup();

        if !head_moved && changed.is_empty() {
            Ok(Divergence::Unchanged)
        } else {
            Ok(Divergence::Diverged { changed_paths: changed })
        }
    }

    fn head_sha(&self) -> Result<Option<String>> {
        Ok(self.head_commit()?.map(|c| c.id().to_string()))
    }
}
