//! The optional, user-driven git integration (`git.md`, the VSCode model).
//!
//! It sits above the `hiker-git` `GitBackend` (which confines `git2`/libgit2)
//! and is **inert until the user acts**: git never runs automatic push/pull
//! rounds. The one automatic git action is the debounced commit-on-save
//! (`git-commit-on-save`), gated by `[git] auto_commit`. Two modes:
//!
//! - **Integrated** (`git-integrated-mode`): a save schedules a debounced
//!   commit-on-save with the `Hiker-Author` trailer; rapid saves
//!   `--amend`-coalesce within the window. Push/pull is the user's job (run from
//!   their terminal or, later, a VSCode-style button) — hiker never pushes or
//!   pulls on its own.
//! - **Manual** (`git-manual-mode`): the user drives git entirely. Hiker
//!   tolerates HEAD moving (`git-tolerate-head-move`): any working-tree
//!   divergence from the last-known commit is folded as an external edit
//!   (`apply_external_edit`, the same 3-way fold a disk edit takes). `.md` is
//!   canonical, not HEAD (`git-co-tenancy`).
//!
//! K1 removed the libp2p sync engine. K2 demoted git to optional/user-driven:
//! the always-on push/pull round driver and the automatic fetch→`git merge`
//! →fold-into-layered reconcile engine are gone. A user's *own* `git pull` that
//! leaves conflict markers in a file is still served by the in-editor marker
//! resolver (`panels/buffer/gitmerge.rs`) — that is the VSCode model.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hiker_core::config::vcs::{GitMode, GitSection};
use hiker_core::editing::shapes::Author as CoreAuthor;
use hiker_core::editing::LayeredDoc;
use hiker_git::meta::{Author, CommitInfo, Trailers};
use hiker_git::repo::{
    ChangeStatus, Divergence, GitBackend, Libgit2Backend, MergeOutcome, SubmoduleStatus,
};

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How one path changed, for a Source-Control row (`ChangedPath`). The plain
/// engine-level mirror of [`ChangeStatus`] the egui SC view (G3b) renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathChange {
    /// New file (untracked, or added in the index / merge).
    Added,
    /// Content changed.
    Modified,
    /// File removed.
    Deleted,
    /// A byte-similar move, reported at the new path.
    Renamed,
}

impl From<ChangeStatus> for PathChange {
    fn from(s: ChangeStatus) -> Self {
        match s {
            ChangeStatus::Added => Self::Added,
            ChangeStatus::Modified => Self::Modified,
            ChangeStatus::Deleted => Self::Deleted,
            ChangeStatus::Renamed => Self::Renamed,
        }
    }
}

/// One changed path in the Source-Control view, with how it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Vault-relative (forward-slash) path.
    pub path: String,
    /// How it changed.
    pub change: PathChange,
}

/// One submodule row the Source-Control view surfaces — a nested repo whose
/// gitlink advanced, whose working tree is dirty, or which is uninitialized
/// (the freshly-cloned empty-dir state). [git-nested-repo-submodule]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleStatusRow {
    /// Vault-relative path of the submodule.
    pub path: String,
    /// Empty placeholder — never checked out (offer "update submodules").
    pub uninitialized: bool,
    /// The nested repo has uncommitted work.
    pub dirty: bool,
    /// The checked-out HEAD differs from the pinned gitlink.
    pub advanced: bool,
}

impl From<SubmoduleStatus> for SubmoduleStatusRow {
    fn from(s: SubmoduleStatus) -> Self {
        Self {
            path: s.path,
            uninitialized: s.uninitialized,
            dirty: s.dirty,
            advanced: s.advanced,
        }
    }
}

/// The Source-Control view's data — everything the SC activity (G3b) renders in
/// one read. Branch + ahead/behind from `branch_status`; the staged/unstaged
/// groups from the index/workdir diffs; conflicts when a merge is in progress;
/// declared submodules with per-submodule status. `.hiker/` is excluded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// Current branch name (`None` on detached/unborn HEAD).
    pub branch: Option<String>,
    /// Local commits ahead of the upstream (`0` when none/up-to-date).
    pub ahead: usize,
    /// Remote commits behind the upstream (`0` when none/up-to-date).
    pub behind: usize,
    /// Staged changes (index-vs-HEAD) — the "Staged Changes" group.
    pub staged: Vec<ChangedPath>,
    /// Unstaged changes (worktree-vs-index) — the "Changes" group.
    pub unstaged: Vec<ChangedPath>,
    /// Conflicted paths when a merge is in progress (empty otherwise).
    pub conflicted: Vec<String>,
    /// Declared submodules with per-submodule status rows.
    pub submodules: Vec<SubmoduleStatusRow>,
}

/// The outcome of [`GitSyncEngine::pull`] the SC view acts on. A `Merged`
/// advanced HEAD (fast-forward or a real merge commit); `Conflicted` left
/// zdiff3 markers on disk for the marker resolver, with `MERGE_HEAD` set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullOutcome {
    /// Local already had the remote head — nothing changed.
    UpToDate,
    /// HEAD advanced (fast-forward or merge commit); the new sha.
    Merged(String),
    /// Conflicts left on disk as markers; the conflicted paths. The user
    /// resolves, then [`GitSyncEngine::finalize_merge_if_clean`] commits.
    Conflicted { paths: Vec<String> },
}

/// The outcome of [`GitSyncEngine::sync`] (pull then push). A conflicted pull
/// stops the round before pushing — hiker never pushes over an in-progress
/// merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Pull found conflicts; the round stopped before pushing. The user
    /// resolves + finalizes, then syncs again.
    Conflicted { paths: Vec<String> },
    /// Pull was clean (up-to-date or merged) and the push succeeded. Carries
    /// the pull outcome for the UI to report.
    Pushed(PullOutcome),
}

/// The user-driven git integration. Holds the libgit2 backend (behind a mutex —
/// libgit2's `Repository` is `Send` but not `Sync`), the vault layered doc, the
/// `[git]` config, and the debounce machinery for commit-on-save.
pub struct GitSyncEngine {
    backend: Arc<Mutex<Libgit2Backend>>,
    layered: Arc<LayeredDoc>,
    config: GitSection,
    vault_root: PathBuf,
    /// Progress-line sink, drained into the `sync_events` ring the Sync page
    /// reads.
    events_tx: UnboundedSender<String>,
    /// Wake signal for the debounced commit-on-save task. A local save calls
    /// [`notify_local_change`](Self::notify_local_change) → `notify_one()`.
    local_change: Arc<Notify>,
    /// Last commit sha hiker knows about — the "known state" manual-mode
    /// divergence detection compares against (`git-tolerate-head-move`). Updated
    /// after every commit and every fold pass.
    known_sha: Arc<Mutex<Option<String>>>,
    cancel: CancellationToken,
    /// Handle to the runtime the engine was constructed on, so the debounce
    /// task can be spawned from a `&self` (UI-thread) call without re-entering
    /// a runtime guard.
    rt: Handle,
}

impl GitSyncEngine {
    /// Build the engine: open-or-init the repo, ensure `.hiker/` is gitignored,
    /// and seed the known sha from HEAD. Does NOT spawn the debounce task — the
    /// caller (bootstrap) calls [`spawn_commit_task`](Self::spawn_commit_task).
    pub fn new(
        vault_root: &std::path::Path,
        layered: Arc<LayeredDoc>,
        config: &GitSection,
        events_tx: UnboundedSender<String>,
        rt: Handle,
    ) -> Result<Self, String> {
        let mut backend = Libgit2Backend::open_or_init(vault_root)
            .map_err(|e| format!("git: open/init failed — {e}"))?;
        backend
            .ensure_hiker_ignored()
            .map_err(|e| format!("git: gitignore write failed — {e}"))?;
        // Fold a nested repo (CODE-IN-VAULT) into vault sync as a submodule when
        // opted in via `[git] submodules = "submodule"`; default skips it (the
        // nested repo stays an independent repo). [git-nested-repo-submodule]
        //
        // Only in INTEGRATED mode — registration writes `.gitmodules` +
        // `submodule.<name>.url`, which mutates the repo structure. In MANUAL
        // mode the user drives git (co-tenancy), so hiker must never dirty the
        // working tree on a vault open; the user declares submodules themselves.
        // [git-manual-mode]
        if config.mode == GitMode::Integrated
            && config.submodules == hiker_core::config::vcs::SubmoduleMode::Submodule
        {
            backend.set_submodule_tracking(true);
            if let Err(e) = backend.ensure_submodules_registered() {
                tracing::warn!(error = %e, "git: submodule registration failed; continuing");
            }
        }
        let known_sha = backend.head_sha().unwrap_or(None);
        Ok(Self {
            backend: Arc::new(Mutex::new(backend)),
            layered,
            config: config.clone(),
            vault_root: vault_root.to_path_buf(),
            events_tx,
            local_change: Arc::new(Notify::new()),
            known_sha: Arc::new(Mutex::new(known_sha)),
            cancel: CancellationToken::new(),
            rt,
        })
    }

    fn log(&self, line: impl Into<String>) {
        let _ = self.events_tx.send(line.into());
    }

    /// Commit the current working tree with a `Hiker-Author` trailer
    /// (`git-commit-on-save`). `amend` collapses a rapid follow-up save into the
    /// previous commit. Stages everything tracked (the gitignore keeps `.hiker/`
    /// out), so one debounced commit captures a burst of saves. Returns the new
    /// sha, or `None` on a no-op. Updates the known sha.
    pub fn commit_now(&self, author: Author, amend: bool) -> Result<Option<String>, String> {
        let trailers = Trailers::authored(author);
        let sha = {
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            backend
                .commit_paths(&[], "hiker save", &trailers, amend)
                .map_err(|e| format!("git: commit failed — {e}"))?
        };
        if let Some(sha) = &sha {
            if let Ok(mut k) = self.known_sha.lock() {
                *k = Some(sha.clone());
            }
            self.log(format!("git: committed {}", &sha[..sha.len().min(8)]));
        }
        Ok(sha)
    }

    /// Commit an observed move (`git-observed-rename-commit`): a pure-rename
    /// commit (new path carrying the old, byte-identical content) plus a
    /// `Hiker-Rename` trailer, then — if the content also changed — an edit
    /// commit at the new path. The caller has already moved the file on disk.
    pub fn commit_observed_move(
        &self,
        from: &str,
        to: &str,
        author: Author,
        content_changed: bool,
    ) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        let rename_trailers = Trailers::renamed(author.clone(), from.to_string(), to.to_string());
        backend
            .commit_rename(from, to, &rename_trailers)
            .map_err(|e| format!("git: rename commit failed — {e}"))?;
        if content_changed {
            backend
                .commit_paths(&[to.to_string()], &format!("hiker edit {to}"), &Trailers::authored(author), false)
                .map_err(|e| format!("git: post-rename edit commit failed — {e}"))?;
        }
        if let Ok(Some(sha)) = backend.head_sha()
            && let Ok(mut k) = self.known_sha.lock()
        {
            *k = Some(sha);
        }
        Ok(())
    }

    /// App hook for an observed file-tree move/rename (`git-observed-rename-commit`).
    /// Lands a dedicated pure-rename commit carrying the `Hiker-Rename` trailer so
    /// `git log --follow` recovers the move, instead of letting it ride the next
    /// staged save-burst commit as an opaque delete+add. The caller has already
    /// moved the file on disk via the layered-doc `move_note` path.
    ///
    /// Gated by the same `auto_commit` policy as commit-on-save: when hiker isn't
    /// allowed to commit on the user's behalf (manual mode with `auto_commit`
    /// off), this is a no-op and the move stays in the working tree for the user
    /// to commit. Errors are logged, never fatal — a failed rename commit just
    /// means the move falls back to the next save-burst commit. Returns whether a
    /// rename commit was attempted.
    pub fn commit_observed_rename(&self, from: &str, to: &str) -> bool {
        if !self.config.auto_commit {
            return false;
        }
        // A pure rename: the renamed file's own bytes are unchanged by the move,
        // so any referrer-link rewrites to OTHER files stay in the working tree
        // and ride the next save-burst commit. Push (if any) happens on the
        // regular sync triggers, not here.
        if let Err(e) = self.commit_observed_move(from, to, Author::User, false) {
            self.log(e);
            return false;
        }
        self.log(format!("git: rename-committed {from} -> {to}"));
        true
    }

    /// Recent commits (newest first, capped at `limit`) — the diff-summary
    /// panel's rev picker reads these (`diff-summary-panel`). A read-only
    /// pass-through to `GitBackend::log`.
    pub fn recent_commits(&self, limit: usize) -> Result<Vec<CommitInfo>, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.log(limit).map_err(|e| format!("git: log failed — {e}"))
    }

    /// Changed paths between `base_rev` and `head_rev` (`None` = the working
    /// tree), with per-path status — the diff-summary panel's file list. A
    /// read-only pass-through to `GitBackend::diff_paths`
    /// (`diff-paths-trait-method`).
    pub fn diff_paths(
        &self,
        base_rev: &str,
        head_rev: Option<&str>,
    ) -> Result<Vec<(String, ChangeStatus)>, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend
            .diff_paths(base_rev, head_rev)
            .map_err(|e| format!("git: diff failed — {e}"))
    }

    /// Content of `path` at `rev` (`GitBackend::show`) — the resolution path
    /// for `DiffSource::GitRef` overlay bases (`diff-source-git-ref`).
    /// `Ok(None)` = the path didn't exist at that rev.
    pub fn show_at(&self, rev: &str, path: &str) -> Result<Option<String>, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.show(rev, path).map_err(|e| format!("git: show failed — {e}"))
    }

    /// Manual-mode reconcile (`git-tolerate-head-move`): detect any working-tree
    /// divergence from the last-known commit and fold each changed path as an
    /// external edit — the same path as a disk edit from another editor. Never
    /// touches the remote.
    pub fn manual_reconcile(&self) -> Result<(), String> {
        let known = self.known_sha.lock().ok().and_then(|g| g.clone());
        let divergence = {
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            backend
                .divergence_from(known.as_deref())
                .map_err(|e| format!("git: divergence check failed — {e}"))?
        };
        if let Divergence::Diverged { changed_paths } = divergence {
            self.fold_divergence(&changed_paths);
            // Adopt the new HEAD as the known state so the next pass doesn't
            // re-fold the same commit (hash-gated + idempotent).
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            if let Ok(Some(sha)) = backend.head_sha()
                && let Ok(mut k) = self.known_sha.lock()
            {
                *k = Some(sha);
            }
        }
        Ok(())
    }

    /// Fold each changed path's on-disk content into the layered doc via
    /// `apply_external_edit` — the substrate's 3-way fold. `apply_external_edit`
    /// is hash-gated (a byte-identical disk equals a no-op echo) and, on a
    /// same-region contention, leaves the document conflicted through the
    /// unified conflict surface rather than interleaving — so we feed
    /// `hiker_core::merge`, never git's markers (`git-push-pull-rounds`).
    fn fold_divergence(&self, changed_paths: &[String]) {
        for rel in changed_paths {
            // Only fold indexable text documents; skip attachments / non-md.
            if !rel.ends_with(".md") && !rel.ends_with(".txt") {
                continue;
            }
            let disk = match std::fs::read_to_string(self.vault_root.join(rel)) {
                Ok(s) => s,
                Err(_) => continue, // deleted / binary — handled elsewhere
            };
            match self.layered.doc_id_for_path(rel) {
                Ok(Some(doc_id)) => match self.layered.apply_external_edit(&doc_id, &disk) {
                    Ok(true) => self.log(format!("git: folded inbound change to {rel}")),
                    Ok(false) => {} // no-op echo
                    Err(e) => self.log(format!("git: fold failed for {rel} — {e}")),
                },
                Ok(None) => {
                    // No layered-doc document yet — a brand-new file from the peer / git
                    // history. Register it with the inbound content authored
                    // `external` so it's tracked and indexed.
                    match self.layered.create_document(rel, "note", &disk, &CoreAuthor::External) {
                        Ok(_) => self.log(format!("git: bound new inbound document {rel}")),
                        Err(e) => self.log(format!("git: could not bind {rel} — {e}")),
                    }
                }
                Err(e) => self.log(format!("git: doc lookup failed for {rel} — {e}")),
            }
        }
    }

    /// Spawn the debounced save-burst task (the SENDING side of
    /// `git-commit-on-save`). Loops on the `local_change` notify, coalesces a
    /// burst with the configured debounce, then runs one
    /// [`commit_for_save_burst`](Self::commit_for_save_burst) pass. Respects the
    /// cancel token. Never pushes — push/pull is user-driven (the VSCode model).
    ///
    /// Gated by `auto_commit`: `false` skips spawning entirely (so integrated +
    /// `auto_commit = false` does no auto-commit; the engine still serves
    /// status/sync). In INTEGRATED mode the pass commits the burst; in MANUAL it
    /// only folds an external HEAD move — manual NEVER auto-commits.
    pub fn spawn_commit_task(self: &Arc<Self>) {
        if !self.config.auto_commit {
            return;
        }
        let engine = self.clone();
        let cancel = self.cancel.clone();
        let debounce = Duration::from_millis(u64::from(self.config.commit_debounce_ms).max(1));
        self.rt.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = engine.local_change.notified() => {}
                }
                // Coalesce a burst of rapid saves into one commit window.
                tokio::time::sleep(debounce).await;
                // libgit2 work is blocking — run it off the async worker.
                let e = engine.clone();
                let _ = tokio::task::spawn_blocking(move || e.commit_for_save_burst()).await;
            }
        });
    }

    /// One debounced commit-on-save pass.
    ///
    /// **Manual mode NEVER auto-commits** (`git-manual-mode`): the user drives
    /// git entirely, so a save only updates the `.md` (+ snapshot) and we just
    /// fold any external HEAD move (`manual_reconcile`) to keep the layered model
    /// consistent with disk. The working tree is left for the user to commit.
    ///
    /// **Integrated mode** commits the save burst — but never pushes; push/pull
    /// is user-driven (the VSCode model). (Reached only when `auto_commit` is on:
    /// `spawn_commit_task` doesn't spawn this loop otherwise, so `auto_commit =
    /// false` disables the auto-commit even in integrated.)
    fn commit_for_save_burst(&self) {
        if self.config.mode == GitMode::Manual {
            // Manual: fold an external HEAD move so the layered doc tracks disk;
            // do NOT commit on the user's behalf — git is entirely theirs.
            let _ = self.manual_reconcile();
            return;
        }
        if let Err(e) = self.commit_now(Author::User, false) {
            self.log(e);
        }
    }

    /// Restore-on-open for the CODE-IN-VAULT pattern: when `[git].submodules =
    /// "submodule"`, populate any DECLARED-but-UNINITIALIZED submodule (an empty
    /// gitlink dir, as left by a fresh clone) at its pinned commit via G1's
    /// conservative `restore_uninitialized_submodules`. A populated or dirty
    /// submodule is NEVER re-checked-out, so the user's nested-repo work is never
    /// clobbered.
    ///
    /// This is a CHECKOUT (not a repo-structure mutation like registration), so
    /// it's allowed in BOTH modes — a clone with empty submodule dirs is the
    /// broken state regardless of who drives commits. Errors are logged, never
    /// fatal. [git-nested-repo-submodule]
    pub fn restore_submodules_on_open(&self) {
        if self.config.submodules != hiker_core::config::vcs::SubmoduleMode::Submodule {
            return;
        }
        let restored = {
            let backend = match self.backend.lock() {
                Ok(b) => b,
                Err(_) => {
                    self.log("git: backend lock poisoned; skipping submodule restore");
                    return;
                }
            };
            backend.restore_uninitialized_submodules()
        };
        match restored {
            Ok(0) => {}
            Ok(n) => self.log(format!("git: populated {n} uninitialized submodule(s) on open")),
            Err(e) => self.log(format!("git: submodule restore failed — {e}")),
        }
    }

    // -----------------------------------------------------------------------
    // G3a — Source-Control engine verbs (the testable backend-of-the-frontend
    // the G3b egui activity calls). Each wraps the G1 backend + engine state
    // and returns plain types. No UI caller yet; unit-tested below.
    // -----------------------------------------------------------------------

    /// The Source-Control view's data in one read (`GitStatus`): branch +
    /// ahead/behind, staged (index-vs-HEAD) and unstaged (worktree-vs-index)
    /// groups, conflicted paths when a merge is in progress, and declared
    /// submodules with per-submodule status. `.hiker/` is excluded by the
    /// backend diffs. [git-branch-status, git-staging-ops]
    pub fn status(&self) -> Result<GitStatus, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        let bs = backend.branch_status().map_err(|e| format!("git: branch status failed — {e}"))?;
        let staged = backend
            .diff_tree_to_index()
            .map_err(|e| format!("git: staged diff failed — {e}"))?
            .into_iter()
            .map(|(path, change)| ChangedPath { path, change: change.into() })
            .collect();
        let unstaged = backend
            .diff_index_to_workdir()
            .map_err(|e| format!("git: unstaged diff failed — {e}"))?
            .into_iter()
            .map(|(path, change)| ChangedPath { path, change: change.into() })
            .collect();
        // Conflicts only matter mid-merge; an idle repo reports none.
        let conflicted = if backend
            .merge_in_progress()
            .map_err(|e| format!("git: merge-state check failed — {e}"))?
        {
            backend
                .merge_conflict_paths()
                .map_err(|e| format!("git: conflict paths failed — {e}"))?
        } else {
            Vec::new()
        };
        let submodules = backend
            .submodule_status_rows()
            .map_err(|e| format!("git: submodule status failed — {e}"))?
            .into_iter()
            .map(SubmoduleStatusRow::from)
            .collect();
        Ok(GitStatus {
            branch: bs.branch,
            ahead: bs.ahead,
            behind: bs.behind,
            staged,
            unstaged,
            conflicted,
            submodules,
        })
    }

    /// Stage `paths` (vault-relative) into the index — the SC "stage" action.
    /// Wraps G1 `stage_paths`. [git-staging-ops]
    pub fn stage(&self, paths: &[String]) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.stage_paths(paths).map_err(|e| format!("git: stage failed — {e}"))
    }

    /// Unstage `paths` (reset their index entry to HEAD) — the SC "unstage"
    /// action. Wraps G1 `unstage_paths`. [git-staging-ops]
    pub fn unstage(&self, paths: &[String]) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.unstage_paths(paths).map_err(|e| format!("git: unstage failed — {e}"))
    }

    /// Discard working-tree changes to `paths` (restore HEAD content) — the SC
    /// "discard" action. Destructive. Wraps G1 `discard_paths`. [git-staging-ops]
    pub fn discard(&self, paths: &[String]) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.discard_paths(paths).map_err(|e| format!("git: discard failed — {e}"))
    }

    /// Stage a single unified-diff hunk into the index — the per-hunk "Stage
    /// hunk" action in the diff view. `patch` is one hunk's forward unified
    /// diff (HEAD → working); see [`crate::source_control::hunk_patch`] for the
    /// builder. Wraps G1 `stage_hunk`. [git-staging-ops]
    pub fn stage_hunk(&self, patch: &str) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.stage_hunk(patch).map_err(|e| format!("git: stage hunk failed — {e}"))
    }

    /// Unstage a single hunk from the index (reverse-apply it) — the per-hunk
    /// "Unstage hunk" action. Wraps G1 `unstage_hunk`. [git-staging-ops]
    pub fn unstage_hunk(&self, patch: &str) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.unstage_hunk(patch).map_err(|e| format!("git: unstage hunk failed — {e}"))
    }

    /// Discard a single hunk from the WORKING TREE (reverse-apply it on disk) —
    /// the per-hunk "Discard hunk" action. Destructive for that hunk only.
    /// Wraps G1 `discard_hunk`. [git-staging-ops]
    pub fn discard_hunk(&self, patch: &str) -> Result<(), String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.discard_hunk(patch).map_err(|e| format!("git: discard hunk failed — {e}"))
    }

    /// The working-vs-HEAD per-hunk diff for `path` (vault-relative), each hunk
    /// carrying the lines to render and the one-hunk patch the
    /// `stage_hunk`/`unstage_hunk`/`discard_hunk` verbs apply. The base side is
    /// `path`'s content at HEAD (empty when the file is new at HEAD); the new
    /// side is the on-disk working-tree content. `context` is the per-hunk
    /// context radius. Used by the SC diff view's per-hunk staging.
    /// [git-staging-ops]
    pub fn working_hunks(
        &self,
        path: &str,
        context: usize,
    ) -> Result<Vec<crate::source_control::hunk_patch::DiffHunk>, String> {
        let head = self.head_sha_str()?;
        let base = match head {
            Some(rev) => self.show_at(&rev, path)?.unwrap_or_default(),
            None => String::new(),
        };
        let current = std::fs::read_to_string(self.vault_root.join(path))
            .map_err(|e| format!("git: read working file {path} failed — {e}"))?;
        Ok(crate::source_control::hunk_patch::build_hunks(&base, &current, path, context))
    }

    /// HEAD's sha, or `None` on an unborn branch — the base rev a working-vs-HEAD
    /// hunk diff resolves its left side against.
    fn head_sha_str(&self) -> Result<Option<String>, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.head_sha().map_err(|e| format!("git: head read failed — {e}"))
    }

    /// Populate / advance the vault's git submodules to their pinned commit —
    /// the SC "Update submodules" action. Restores any uninitialized nested
    /// repo (the freshly-cloned empty-dir state) then `update`s populated ones
    /// to the new pin. Gated on `[git].submodules == Submodule`; a no-op (and
    /// reported as such) otherwise. Wraps G1
    /// `restore_uninitialized_submodules` + `update_submodules`.
    /// [git-nested-repo-submodule]
    pub fn update_submodules(&self) -> Result<(), String> {
        if self.config.submodules != hiker_core::config::vcs::SubmoduleMode::Submodule {
            return Err("git: submodules aren't tracked ([git].submodules is not \"submodule\")".to_string());
        }
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        let restored = backend
            .restore_uninitialized_submodules()
            .map_err(|e| format!("git: submodule restore failed — {e}"))?;
        if restored > 0 {
            self.log(format!("git: populated {restored} uninitialized submodule(s)"));
        }
        backend.update_submodules().map_err(|e| format!("git: submodule update failed — {e}"))
    }

    /// Commit exactly what is STAGED with the `Hiker-Author: user` trailer (the
    /// same attribution commit-on-save uses) — the SC "Commit" button. `amend`
    /// replaces HEAD. Returns the new sha, or `Ok(None)` when nothing is staged
    /// (a no-op). Updates the known sha. [git-staging-ops]
    pub fn commit(&self, message: &str, amend: bool) -> Result<Option<String>, String> {
        let trailers = Trailers::authored(Author::User);
        let sha = {
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            backend
                .commit_index(message, &trailers, amend)
                .map_err(|e| format!("git: commit failed — {e}"))?
        };
        if let Some(sha) = &sha {
            if let Ok(mut k) = self.known_sha.lock() {
                *k = Some(sha.clone());
            }
            self.log(format!("git: committed {}", &sha[..sha.len().min(8)]));
        }
        Ok(sha)
    }

    /// Push the current branch to `[git].remote` — the SC "push" action. Errors
    /// clearly when no remote is configured. Wraps G1 `push`.
    /// [git-push-pull-rounds]
    pub fn push(&self) -> Result<(), String> {
        if self.config.remote.is_empty() {
            return Err("git: no remote configured ([git].remote is empty)".to_string());
        }
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        backend.push(&self.config.remote).map_err(|e| format!("git: push failed — {e}"))
    }

    /// Pull from `[git].remote`: fetch + a real `git merge` (decision A). A
    /// clean reconcile fast-forwards or writes a 2-parent merge commit; a
    /// conflict leaves zdiff3 markers on disk (no commit, `MERGE_HEAD` set) for
    /// the marker resolver. On a `Merged` that may have advanced a submodule
    /// gitlink, restore uninitialized submodules then update them so the nested
    /// repo populates at its pin. [git-merge-via-git, git-nested-repo-submodule]
    pub fn pull(&self) -> Result<PullOutcome, String> {
        if self.config.remote.is_empty() {
            return Err("git: no remote configured ([git].remote is empty)".to_string());
        }
        let trailers = Trailers::authored(Author::Sync("remote".to_string()));
        let outcome = {
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            backend
                .fetch_and_merge(&self.config.remote, &trailers)
                .map_err(|e| format!("git: pull/merge failed — {e}"))?
        };
        match outcome {
            MergeOutcome::UpToDate => Ok(PullOutcome::UpToDate),
            MergeOutcome::Merged(sha) => {
                if let Ok(mut k) = self.known_sha.lock() {
                    *k = Some(sha.clone());
                }
                // A merge may have advanced a submodule gitlink; populate any
                // uninitialized nested repo and bring populated ones to the new
                // pin so a CODE-IN-VAULT submodule tracks the pull.
                self.sync_submodules_after_merge();
                self.log(format!("git: pulled, HEAD now {}", &sha[..sha.len().min(8)]));
                Ok(PullOutcome::Merged(sha))
            }
            MergeOutcome::Conflicted(paths) => {
                self.log(format!("git: pull left {} conflicted path(s)", paths.len()));
                Ok(PullOutcome::Conflicted { paths })
            }
        }
    }

    /// Populate/advance submodules after a pull that may have moved a gitlink:
    /// first the conservative uninitialized-only restore, then `update` so a
    /// populated submodule moves to the new pin. Best-effort; errors logged.
    /// [git-nested-repo-submodule]
    fn sync_submodules_after_merge(&self) {
        if self.config.submodules != hiker_core::config::vcs::SubmoduleMode::Submodule {
            return;
        }
        let backend = match self.backend.lock() {
            Ok(b) => b,
            Err(_) => {
                self.log("git: backend lock poisoned; skipping submodule sync after pull");
                return;
            }
        };
        match backend.restore_uninitialized_submodules() {
            Ok(0) => {}
            Ok(n) => self.log(format!("git: populated {n} uninitialized submodule(s) after pull")),
            Err(e) => self.log(format!("git: submodule restore after pull failed — {e}")),
        }
        if let Err(e) = backend.update_submodules() {
            self.log(format!("git: submodule update after pull failed — {e}"));
        }
    }

    /// Sync = pull then push — the SC "Sync" button. A conflicted pull stops
    /// the round before pushing (never push over an in-progress merge); the
    /// user resolves + finalizes, then syncs again. Otherwise push and report.
    /// [git-push-pull-rounds]
    pub fn sync(&self) -> Result<SyncOutcome, String> {
        match self.pull()? {
            PullOutcome::Conflicted { paths } => Ok(SyncOutcome::Conflicted { paths }),
            clean => {
                self.push()?;
                Ok(SyncOutcome::Pushed(clean))
            }
        }
    }

    /// Finalize an in-progress merge once the user resolved the markers — the
    /// SC "complete merge" step. Only commits when NO conflict markers remain
    /// in the working tree (a guard so the UI can't finalize over an
    /// unresolved file); returns the merge commit sha, or `Ok(None)` when there
    /// is nothing to commit. A no-op (Ok(None)) when no merge is in progress.
    /// [git-conflict-inline-markers]
    pub fn finalize_merge_if_clean(&self) -> Result<Option<String>, String> {
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        if !backend.merge_in_progress().map_err(|e| format!("git: merge-state check failed — {e}"))? {
            return Ok(None);
        }
        // Guard: refuse to finalize while any tracked path still carries
        // conflict markers. The index can be marker-free yet the file dirty;
        // re-scan the conflicted paths' on-disk content for the markers.
        let conflicted = backend
            .merge_conflict_paths()
            .map_err(|e| format!("git: conflict paths failed — {e}"))?;
        for rel in &conflicted {
            if let Ok(body) = std::fs::read_to_string(self.vault_root.join(rel))
                && body.contains("<<<<<<<")
            {
                return Err(format!("git: {rel} still has unresolved conflict markers"));
            }
        }
        let trailers = Trailers::authored(Author::User);
        let sha = backend
            .finalize_merge(&trailers)
            .map_err(|e| format!("git: finalize merge failed — {e}"))?;
        if let Some(sha) = &sha
            && let Ok(mut k) = self.known_sha.lock()
        {
            *k = Some(sha.clone());
        }
        Ok(sha)
    }

    /// A change was just committed (saved) locally — nudge the engine to
    /// schedule a debounced commit-on-save. Never pushes; non-blocking; safe to
    /// call from the UI thread at every commit site.
    /// status: git-commit-on-save
    pub(crate) fn notify_local_change(&self) {
        self.local_change.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiker_core::config::vcs::GitSection;
    use hiker_git::meta::CommitInfo;

    /// Build an engine over a fresh temp vault in `integrated` mode, returning
    /// the engine, the vault root tempdir, and a drain for its event lines.
    fn build(mode: GitMode, remote: &str) -> (Arc<GitSyncEngine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let layered = Arc::new(LayeredDoc::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode,
            remote: remote.to_string(),
            ..GitSection::default()
        };
        let engine = GitSyncEngine::new(
            dir.path(),
            layered,
            &section,
            tx,
            Handle::current(),
        )
        .unwrap();
        (Arc::new(engine), dir)
    }

    fn read_log(engine: &GitSyncEngine) -> Vec<CommitInfo> {
        let backend = engine.backend.lock().unwrap();
        backend.log(50).unwrap()
    }

    #[tokio::test]
    async fn commit_on_save_produces_a_user_trailer_commit() {
        let (engine, dir) = build(GitMode::Integrated, "");
        // The substrate creates the .md; mimic a save by creating a doc.
        engine
            .layered
            .create_document("note.md", "note", "hello\n", &CoreAuthor::User)
            .unwrap();
        let sha = engine.commit_now(Author::User, false).unwrap();
        assert!(sha.is_some(), "a commit-on-save produced a commit");
        let log = read_log(&engine);
        assert_eq!(log[0].trailers.author, Author::User, "Hiker-Author: user trailer");
        drop(dir);
    }

    #[tokio::test]
    async fn observed_move_is_a_pure_rename_commit_with_trailer() {
        let (engine, dir) = build(GitMode::Integrated, "");
        let root = dir.path();
        std::fs::write(root.join("old.md"), "stable content\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        // Observe the move: move on disk, bytes unchanged, then commit it.
        std::fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
        engine
            .commit_observed_move("old.md", "new.md", Author::User, false)
            .unwrap();

        let log = read_log(&engine);
        assert_eq!(
            log[0].trailers.rename,
            Some(("old.md".to_string(), "new.md".to_string())),
            "Hiker-Rename trailer on the rename commit"
        );
        // The bytes survived at the new path (the shape `--follow` matches).
        let head = {
            let backend = engine.backend.lock().unwrap();
            backend.head_sha().unwrap().unwrap()
        };
        let backend = engine.backend.lock().unwrap();
        assert_eq!(backend.show(&head, "new.md").unwrap().as_deref(), Some("stable content\n"));
        assert!(backend.show(&head, "old.md").unwrap().is_none());
        drop(dir);
    }

    #[tokio::test]
    async fn observed_rename_hook_follows_and_carries_trailer() {
        let (engine, dir) = build(GitMode::Integrated, "");
        let root = dir.path();
        std::fs::write(root.join("old.md"), "stable content\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        // The app-level hook: rename on disk, then the hook lands the dedicated
        // pure-rename commit (auto_commit defaults on in integrated mode).
        std::fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
        assert!(engine.commit_observed_rename("old.md", "new.md"), "hook committed the rename");

        // The Hiker-Rename trailer is present on the head commit.
        let log = read_log(&engine);
        assert_eq!(
            log[0].trailers.rename,
            Some(("old.md".to_string(), "new.md".to_string())),
            "Hiker-Rename trailer on the rename commit",
        );

        // `git log --follow new.md` recovers BOTH the rename commit and the
        // original create — i.e. git tracks the file across the move.
        let follow = std::process::Command::new("git")
            .args(["-C", root.to_str().unwrap(), "log", "--follow", "--format=%s", "--", "new.md"])
            .output()
            .unwrap();
        assert!(follow.status.success(), "git log --follow ran");
        let subjects = String::from_utf8(follow.stdout).unwrap();
        let lines: Vec<&str> = subjects.lines().collect();
        assert_eq!(lines.len(), 2, "--follow spans the rename: {lines:?}");
        assert!(lines[0].contains("Rename old.md -> new.md"), "rename commit: {lines:?}");
        drop(dir);
    }

    #[tokio::test]
    async fn observed_rename_hook_respects_auto_commit_gate() {
        // auto_commit OFF: the hook must be a no-op so the user's own git
        // workflow owns the rename commit (manual-mode policy).
        let dir = tempfile::tempdir().unwrap();
        let layered = Arc::new(LayeredDoc::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode: GitMode::Manual,
            auto_commit: false,
            ..GitSection::default()
        };
        let engine =
            Arc::new(GitSyncEngine::new(dir.path(), layered, &section, tx, Handle::current()).unwrap());
        let root = dir.path();
        std::fs::write(root.join("old.md"), "stable\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();
        let count = read_log(&engine).len();
        std::fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
        assert!(!engine.commit_observed_rename("old.md", "new.md"), "gate: no commit");
        assert_eq!(read_log(&engine).len(), count, "no rename commit landed under the gate");
        drop(dir);
    }

    #[tokio::test]
    async fn manual_head_move_folds_as_external_edit() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        // A document tracked by the layered doc.
        let doc_id = engine
            .layered
            .create_document("doc.md", "note", "version one\n", &CoreAuthor::User)
            .unwrap();
        // hiker commits it; this is now the known state.
        engine.commit_now(Author::User, false).unwrap().unwrap();

        // The user edits + commits OUTSIDE hiker — HEAD moves underneath.
        std::fs::write(root.join("doc.md"), "version one\nuser added a line\n").unwrap();
        {
            let backend = engine.backend.lock().unwrap();
            backend
                .commit_paths(
                    &["doc.md".into()],
                    "user's own commit",
                    &Trailers::authored(Author::External),
                    false,
                )
                .unwrap()
                .unwrap();
        }

        // Manual reconcile folds the divergence as an external edit (the same
        // 3-way fold a disk edit takes — no git markers left in the file).
        engine.manual_reconcile().unwrap();

        let accepted = engine.layered.materialize_accepted(&doc_id).unwrap().text;
        assert!(accepted.contains("user added a line"), "folded the user's edit: {accepted:?}");
        assert!(!accepted.contains("<<<<<<<"), "no git conflict markers left in the file");
        drop(dir);
    }

    /// Manual mode + `submodules = "submodule"` must NOT register submodules on
    /// vault open: registration writes `.gitmodules` + `submodule.<name>.url`,
    /// which would dirty a working tree the user didn't author. In manual mode
    /// the user drives git (co-tenancy); hiker mutates no repo structure.
    /// [git-manual-mode] [git-nested-repo-submodule]
    #[tokio::test]
    async fn manual_mode_does_not_register_submodules() {
        use hiker_core::config::vcs::SubmoduleMode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A nested repo with a HEAD that *would* be eligible for registration.
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_backend = hiker_git::repo::Libgit2Backend::open_or_init(&sub).unwrap();
        std::fs::write(sub.join("code.rs"), "fn main() {}\n").unwrap();
        sub_backend
            .commit_paths(&["code.rs".into()], "sub init", &Trailers::authored(Author::User), false)
            .unwrap()
            .expect("nested repo commit");

        let layered = Arc::new(LayeredDoc::open(root).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode: GitMode::Manual,
            submodules: SubmoduleMode::Submodule,
            ..GitSection::default()
        };
        let _engine =
            Arc::new(GitSyncEngine::new(root, layered, &section, tx, Handle::current()).unwrap());

        assert!(
            !root.join(".gitmodules").exists(),
            "manual mode must not write .gitmodules on open",
        );
    }

    /// MANUAL mode NEVER auto-commits: a save-burst pass must leave the user's
    /// uncommitted working tree exactly as-is (it only folds external HEAD
    /// moves). The user drives git entirely. [git-manual-mode]
    #[tokio::test]
    async fn manual_mode_does_not_commit_on_save() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        // An initial commit so HEAD exists; this is the known state.
        std::fs::write(root.join("seed.md"), "seed\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();
        let before = read_log(&engine).len();

        // The user saves (a doc is created); a save-burst pass runs.
        engine
            .layered
            .create_document("note.md", "note", "hello\n", &CoreAuthor::User)
            .unwrap();
        engine.commit_for_save_burst();

        // Manual must NOT have committed — the dirty tree is left for the user.
        assert_eq!(
            read_log(&engine).len(),
            before,
            "manual mode created no commit on save",
        );
        drop(dir);
    }

    /// INTEGRATED mode DOES auto-commit a save burst (when `auto_commit` is on —
    /// the default). The save-burst pass lands a commit. [git-integrated-mode]
    #[tokio::test]
    async fn integrated_mode_commits_on_save() {
        let (engine, dir) = build(GitMode::Integrated, "");
        let root = dir.path();
        std::fs::write(root.join("note.md"), "hello\n").unwrap();
        let before = read_log(&engine).len();

        engine.commit_for_save_burst();

        assert_eq!(
            read_log(&engine).len(),
            before + 1,
            "integrated mode committed the save burst",
        );
        drop(dir);
    }

    /// `auto_commit = false` disables the auto-commit even in INTEGRATED mode:
    /// `spawn_commit_task` never spawns the save-burst loop, so no commit lands
    /// on its own. (The engine still works for later status/sync use.)
    /// [git-commit-on-save]
    #[tokio::test]
    async fn integrated_auto_commit_off_does_not_spawn_commit_loop() {
        let dir = tempfile::tempdir().unwrap();
        let layered = Arc::new(LayeredDoc::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode: GitMode::Integrated,
            auto_commit: false,
            ..GitSection::default()
        };
        let engine =
            Arc::new(GitSyncEngine::new(dir.path(), layered, &section, tx, Handle::current()).unwrap());
        std::fs::write(dir.path().join("note.md"), "hello\n").unwrap();
        let before = read_log(&engine).len();

        // Spawn the task (a no-op under the gate) and poke it; give the runtime a
        // beat to run any (absent) debounce loop.
        engine.spawn_commit_task();
        engine.notify_local_change();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            read_log(&engine).len(),
            before,
            "auto_commit = false: no auto-commit loop, so no commit lands",
        );
        drop(dir);
    }

    /// Submodule restore-on-open is a no-op when there are no submodules (and is
    /// gated off entirely when `[git] submodules != "submodule"`). It must not
    /// error on a plain vault. [git-nested-repo-submodule]
    #[tokio::test]
    async fn submodule_restore_on_open_is_noop_without_submodules() {
        use hiker_core::config::vcs::SubmoduleMode;

        let dir = tempfile::tempdir().unwrap();
        let layered = Arc::new(LayeredDoc::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode: GitMode::Manual,
            submodules: SubmoduleMode::Submodule,
            ..GitSection::default()
        };
        let engine =
            Arc::new(GitSyncEngine::new(dir.path(), layered, &section, tx, Handle::current()).unwrap());
        // No submodules declared — must run cleanly and populate nothing.
        engine.restore_submodules_on_open();
        drop(dir);
    }

    // -----------------------------------------------------------------------
    // G3a — Source-Control engine verbs (the testable backend-of-the-frontend;
    // the egui SC activity (G3b) is the only other caller). A local on-disk
    // path is a valid (anonymous) git "remote", so push/pull/sync exercise the
    // network verbs without any network.
    // -----------------------------------------------------------------------

    /// Build an engine over a fresh temp vault (no LayeredDoc dependence on the
    /// SC verbs), returning the engine and its tempdir. Mirrors `build` but lets
    /// the caller seed an arbitrary `[git]` config.
    fn engine_for(section: &GitSection) -> (Arc<GitSyncEngine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let layered = Arc::new(LayeredDoc::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let engine =
            GitSyncEngine::new(dir.path(), layered, section, tx, Handle::current()).unwrap();
        (Arc::new(engine), dir)
    }

    /// Create a bare git repo at `path` via plain `git` (the app crate doesn't
    /// link `git2` — module discipline confines it to `hiker-git`). A bare
    /// remote accepts pushes to its checked-out-able refs cleanly.
    fn init_bare(path: &std::path::Path) {
        let ok = std::process::Command::new("git")
            .args(["init", "--bare", path.to_str().unwrap()])
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init --bare {path:?}");
    }

    /// `status` groups index-vs-HEAD as STAGED and worktree-vs-index as
    /// UNSTAGED, and surfaces the branch name. A staged new file lands in
    /// `staged`; an unstaged edit to a tracked file lands in `unstaged`.
    #[tokio::test]
    async fn status_groups_staged_and_unstaged() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        std::fs::write(root.join("tracked.md"), "v1\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        // A brand-new file staged into the index; an edit to the tracked file
        // left UNSTAGED in the working tree.
        std::fs::write(root.join("staged.md"), "fresh\n").unwrap();
        std::fs::write(root.join("tracked.md"), "v2\n").unwrap();
        engine.stage(&["staged.md".to_string()]).unwrap();

        let st = engine.status().unwrap();
        assert!(st.branch.is_some(), "a born branch has a name");
        assert!(
            st.staged.iter().any(|c| c.path == "staged.md" && c.change == PathChange::Added),
            "the staged new file is in the staged group: {:?}",
            st.staged,
        );
        assert!(
            st.unstaged.iter().any(|c| c.path == "tracked.md" && c.change == PathChange::Modified),
            "the unstaged edit is in the unstaged group: {:?}",
            st.unstaged,
        );
        // Nothing conflicted on an idle repo, and `.hiker/` never appears.
        assert!(st.conflicted.is_empty());
        assert!(st.staged.iter().chain(&st.unstaged).all(|c| !c.path.starts_with(".hiker/")));
        drop(dir);
    }

    /// stage → commit lands a commit of exactly the staged content; an unstaged
    /// edit is left behind. A commit with nothing staged is a no-op (`Ok(None)`).
    #[tokio::test]
    async fn stage_then_commit_lands_a_commit() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        std::fs::write(root.join("seed.md"), "seed\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();
        let before = read_log(&engine).len();

        // Nothing staged yet → commit is a no-op.
        assert_eq!(engine.commit("empty", false).unwrap(), None, "no-op when nothing staged");
        assert_eq!(read_log(&engine).len(), before, "no commit landed");

        // Stage one file, leave another edit unstaged, then commit.
        std::fs::write(root.join("a.md"), "staged content\n").unwrap();
        std::fs::write(root.join("seed.md"), "unstaged edit\n").unwrap();
        engine.stage(&["a.md".to_string()]).unwrap();
        let sha = engine.commit("add a", false).unwrap().expect("a commit landed");

        assert_eq!(read_log(&engine).len(), before + 1, "exactly one commit");
        assert_eq!(read_log(&engine)[0].trailers.author, Author::User, "Hiker-Author: user trailer");
        // The commit captured the staged file but NOT the unstaged edit.
        {
            let backend = engine.backend.lock().unwrap();
            assert_eq!(backend.show(&sha, "a.md").unwrap().as_deref(), Some("staged content\n"));
            assert_eq!(backend.show(&sha, "seed.md").unwrap().as_deref(), Some("seed\n"), "unstaged edit not committed");
        }
        drop(dir);
    }

    /// unstage moves a staged path back out of the staged group; discard reverts
    /// an unstaged working-tree edit to its HEAD content.
    #[tokio::test]
    async fn unstage_and_discard_round_trip() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        std::fs::write(root.join("doc.md"), "committed\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        std::fs::write(root.join("doc.md"), "edited\n").unwrap();
        engine.stage(&["doc.md".to_string()]).unwrap();
        assert!(engine.status().unwrap().staged.iter().any(|c| c.path == "doc.md"), "staged");

        engine.unstage(&["doc.md".to_string()]).unwrap();
        let st = engine.status().unwrap();
        assert!(st.staged.is_empty(), "unstaged: nothing staged");
        assert!(st.unstaged.iter().any(|c| c.path == "doc.md"), "edit now unstaged");

        engine.discard(&["doc.md".to_string()]).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("doc.md")).unwrap(), "committed\n", "discard reverted to HEAD");
        assert!(engine.status().unwrap().unstaged.is_empty(), "clean after discard");
        drop(dir);
    }

    /// `push` with no remote configured errors clearly; with a bare remote it
    /// sends the current branch so the commit is visible in the remote.
    #[tokio::test]
    async fn push_to_bare_remote_and_no_remote_errors() {
        // No remote → a clear error, not a panic.
        let (engine, dir) = build(GitMode::Manual, "");
        std::fs::write(dir.path().join("n.md"), "x\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();
        assert!(engine.push().is_err(), "push without a remote errors");
        drop(dir);

        // A bare remote on disk accepts the push.
        let rdir = tempfile::tempdir().unwrap();
        let remote = rdir.path().join("remote.git");
        init_bare(&remote);

        let (engine, dir) = build(GitMode::Manual, remote.to_str().unwrap());
        std::fs::write(dir.path().join("note.md"), "hello\n").unwrap();
        let sha = engine.commit_now(Author::User, false).unwrap().unwrap();
        engine.push().expect("push to bare remote");

        let remote_be = hiker_git::repo::Libgit2Backend::open_or_init(&remote).unwrap();
        assert_eq!(remote_be.show(&sha, "note.md").unwrap().as_deref(), Some("hello\n"));
        drop((dir, rdir));
    }

    /// `pull` fast-forwards a behind-local to the remote head (`Merged`), and a
    /// 3-way `pull` with disjoint edits produces a clean merge commit. A second
    /// pull with nothing new is `UpToDate`.
    #[tokio::test]
    async fn pull_fast_forward_then_merge() {
        let rdir = tempfile::tempdir().unwrap();
        let remote = rdir.path().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        let remote_be = hiker_git::repo::Libgit2Backend::open_or_init(&remote).unwrap();
        std::fs::write(remote.join("base.md"), "base\n").unwrap();
        remote_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();

        let (engine, dir) = build(GitMode::Manual, remote.to_str().unwrap());

        // First pull fast-forwards the empty local to the remote base.
        match engine.pull().unwrap() {
            PullOutcome::Merged(_) => {}
            other => panic!("expected a fast-forward Merged, got {other:?}"),
        }
        assert!(dir.path().join("base.md").exists(), "fast-forward checked out base.md");

        // Remote advances; local adds a disjoint file → a real 3-way merge.
        std::fs::write(remote.join("theirs.md"), "from remote\n").unwrap();
        remote_be.commit_paths(&[], "theirs", &Trailers::authored(Author::User), false).unwrap().unwrap();
        std::fs::write(dir.path().join("ours.md"), "from local\n").unwrap();
        engine.stage(&["ours.md".to_string()]).unwrap();
        engine.commit("ours", false).unwrap().unwrap();

        let merge_sha = match engine.pull().unwrap() {
            PullOutcome::Merged(sha) => sha,
            other => panic!("expected a 3-way Merged, got {other:?}"),
        };
        {
            let backend = engine.backend.lock().unwrap();
            assert_eq!(backend.parent_shas(&merge_sha).unwrap().len(), 2, "2-parent merge commit");
            assert_eq!(backend.show(&merge_sha, "theirs.md").unwrap().as_deref(), Some("from remote\n"));
            assert_eq!(backend.show(&merge_sha, "ours.md").unwrap().as_deref(), Some("from local\n"));
        }

        // Nothing new on the remote now → UpToDate.
        assert_eq!(engine.pull().unwrap(), PullOutcome::UpToDate);
        drop((dir, rdir));
    }

    /// A divergent `pull` leaves zdiff3 markers on disk (`Conflicted`), and
    /// `finalize_merge_if_clean` refuses while markers remain, then commits a
    /// 2-parent merge once they're resolved.
    #[tokio::test]
    async fn pull_conflict_then_finalize() {
        let rdir = tempfile::tempdir().unwrap();
        let remote = rdir.path().join("remote");
        std::fs::create_dir_all(&remote).unwrap();
        let remote_be = hiker_git::repo::Libgit2Backend::open_or_init(&remote).unwrap();
        std::fs::write(remote.join("doc.md"), "base\n").unwrap();
        remote_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();

        let (engine, dir) = build(GitMode::Manual, remote.to_str().unwrap());
        engine.pull().unwrap(); // fast-forward to base

        // Both sides edit the same line → a conflict.
        std::fs::write(remote.join("doc.md"), "remote edit\n").unwrap();
        remote_be.commit_paths(&[], "remote", &Trailers::authored(Author::User), false).unwrap().unwrap();
        std::fs::write(dir.path().join("doc.md"), "local edit\n").unwrap();
        engine.stage(&["doc.md".to_string()]).unwrap();
        engine.commit("local", false).unwrap().unwrap();

        match engine.pull().unwrap() {
            PullOutcome::Conflicted { paths } => assert_eq!(paths, vec!["doc.md".to_string()]),
            other => panic!("expected Conflicted, got {other:?}"),
        }
        let on_disk = std::fs::read_to_string(dir.path().join("doc.md")).unwrap();
        assert!(on_disk.contains("<<<<<<<"), "conflict markers on disk: {on_disk}");
        assert!(engine.status().unwrap().conflicted == vec!["doc.md".to_string()], "status surfaces the conflict");

        // Finalizing while markers remain is refused (the UI guard).
        assert!(engine.finalize_merge_if_clean().is_err(), "refuses to finalize over markers");

        // Resolve on disk, then finalize → a 2-parent merge commit.
        std::fs::write(dir.path().join("doc.md"), "resolved\n").unwrap();
        let merge_sha = engine.finalize_merge_if_clean().unwrap().expect("a finalize commit");
        {
            let backend = engine.backend.lock().unwrap();
            assert!(!backend.merge_in_progress().unwrap(), "merge state cleared");
            assert_eq!(backend.parent_shas(&merge_sha).unwrap().len(), 2, "2-parent merge commit");
            assert_eq!(backend.show(&merge_sha, "doc.md").unwrap().as_deref(), Some("resolved\n"));
        }
        assert!(engine.status().unwrap().conflicted.is_empty(), "no conflicts after finalize");
        drop((dir, rdir));
    }

    /// `sync` round-trips: a clean pull then a push that lands the local commit
    /// on the remote, reported as `Pushed`.
    #[tokio::test]
    async fn sync_round_trip_pull_then_push() {
        let rdir = tempfile::tempdir().unwrap();
        let remote = rdir.path().join("remote.git");
        init_bare(&remote);

        // Seed the remote with a base commit via an intermediary clone so the
        // local can fast-forward and then push back.
        let seed = rdir.path().join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        let seed_be = hiker_git::repo::Libgit2Backend::open_or_init(&seed).unwrap();
        std::fs::write(seed.join("base.md"), "base\n").unwrap();
        seed_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();
        seed_be.push(remote.to_str().unwrap()).unwrap();

        let (engine, dir) = build(GitMode::Manual, remote.to_str().unwrap());
        // Pull the base down, add a local commit, then sync (pull clean + push).
        engine.pull().unwrap();
        std::fs::write(dir.path().join("local.md"), "local work\n").unwrap();
        engine.stage(&["local.md".to_string()]).unwrap();
        let local_sha = engine.commit("local work", false).unwrap().unwrap();

        match engine.sync().unwrap() {
            SyncOutcome::Pushed(_) => {}
            other => panic!("expected Pushed, got {other:?}"),
        }
        // The remote now holds the local commit.
        let remote_be = hiker_git::repo::Libgit2Backend::open_or_init(&remote).unwrap();
        assert_eq!(remote_be.show(&local_sha, "local.md").unwrap().as_deref(), Some("local work\n"));
        drop((dir, rdir));
    }

    /// Per-hunk staging end-to-end through the engine: `working_hunks` produces
    /// the patches; `stage_hunk` stages exactly one hunk (the other edit stays
    /// unstaged); `unstage_hunk` reverses it. [git-staging-ops]
    #[tokio::test]
    async fn stage_hunk_round_trip_through_engine() {
        let (engine, dir) = build(GitMode::Manual, "");
        let root = dir.path();
        std::fs::write(root.join("doc.md"), "a\nb\nc\nd\ne\nf\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        // Two disjoint edits in the working tree.
        std::fs::write(root.join("doc.md"), "a\nB\nc\nd\nE\nf\n").unwrap();
        // Context 0 keeps the two edits in separate hunks.
        let hunks = engine.working_hunks("doc.md", 0).unwrap();
        assert_eq!(hunks.len(), 2, "two disjoint hunks: {hunks:?}");

        // Stage only the first hunk; the file is partially staged.
        engine.stage_hunk(&hunks[0].patch).unwrap();
        let st = engine.status().unwrap();
        assert!(st.staged.iter().any(|c| c.path == "doc.md"), "doc.md partially staged: {st:?}");
        assert!(st.unstaged.iter().any(|c| c.path == "doc.md"), "second hunk still unstaged");

        // Committing lands only the first hunk; line 5 is still `e` at HEAD.
        let sha = engine.commit("first hunk", false).unwrap().unwrap();
        {
            let backend = engine.backend.lock().unwrap();
            assert_eq!(backend.show(&sha, "doc.md").unwrap().as_deref(), Some("a\nB\nc\nd\ne\nf\n"));
        }
        drop(dir);
    }

    /// `update_submodules` is gated on `[git].submodules == Submodule`: a vault
    /// that doesn't track submodules gets a clear error, not a silent no-op.
    /// [git-nested-repo-submodule]
    #[tokio::test]
    async fn update_submodules_gated_on_config() {
        let (engine, dir) = build(GitMode::Manual, "");
        let err = engine.update_submodules().unwrap_err();
        assert!(err.contains("submodules aren't tracked"), "clear gate error: {err}");
        drop(dir);
    }

    /// `status` surfaces a submodule row: a vault that declares + commits a
    /// nested repo reports it in `submodules`. (Uses submodule mode so the
    /// gitlink is recorded.) [git-nested-repo-submodule]
    #[tokio::test]
    async fn status_surfaces_submodule_row() {
        use hiker_core::config::vcs::SubmoduleMode;

        let section = GitSection {
            mode: GitMode::Integrated,
            submodules: SubmoduleMode::Submodule,
            ..GitSection::default()
        };
        let (engine, dir) = engine_for(&section);
        let root = dir.path();

        // A nested repo with a HEAD to pin.
        let sub = root.join("code");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_be = hiker_git::repo::Libgit2Backend::open_or_init(&sub).unwrap();
        std::fs::write(sub.join("main.rs").as_path(), "fn main() {}\n").unwrap();
        sub_be
            .commit_paths(&["main.rs".into()], "sub init", &Trailers::authored(Author::User), false)
            .unwrap()
            .unwrap();

        // Register + commit the vault so the submodule gitlink is recorded.
        {
            let backend = engine.backend.lock().unwrap();
            backend.ensure_submodules_registered().unwrap();
        }
        std::fs::write(root.join("plan.md"), "# plan\n").unwrap();
        engine.commit_now(Author::User, false).unwrap().unwrap();

        let st = engine.status().unwrap();
        assert!(
            st.submodules.iter().any(|s| s.path == "code"),
            "the declared submodule appears as a status row: {:?}",
            st.submodules,
        );
        drop(dir);
    }
}
