//! The [`GitBackend`] trait + its libgit2 implementation ([`Libgit2Backend`]).
//!
//! All `git2` use lives here. The trait verbs are what the sync orchestration
//! drives (`git.md`); the impl translates each into libgit2 calls and maps
//! every `git2::Error` into a [`GitError`] so nothing git2-shaped escapes.

use std::path::{Path, PathBuf};

use git2::{
    AnnotatedCommit, Cred, FetchOptions, IndexAddOption, PushOptions, RemoteCallbacks, Repository,
    Signature,
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
    fn push(&self, remote: &str) -> Result<()>;

    /// Read the recent commit log (newest first, capped at `limit`) for
    /// inspection / the activity-feed projection (`git-parallel-history`).
    fn log(&self, limit: usize) -> Result<Vec<CommitInfo>>;

    /// Read the content of `path` at commit `sha` (`git show <sha>:<path>`),
    /// for inspection / version preview. `None` if the path didn't exist there.
    fn show(&self, sha: &str, path: &str) -> Result<Option<String>>;

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

    /// Stage `paths` into the index and write the tree. Returns the tree oid.
    fn stage_tree(&self, paths: &[String]) -> Result<git2::Oid> {
        let mut index = self.repo.index().map_err(|e| Self::map_commit(&e))?;
        if paths.is_empty() {
            // Stage everything that changed (used by amend-coalesce paths).
            index
                .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
                .map_err(|e| Self::map_commit(&e))?;
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
        let mut paths = Vec::new();
        diff.foreach(
            &mut |delta, _| {
                if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                    paths.push(p.to_string_lossy().into_owned());
                }
                true
            },
            None,
            None,
            None,
        )
        .map_err(|e| Self::map_read(&e))?;
        paths.sort();
        paths.dedup();
        Ok(paths)
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

impl GitBackend for Libgit2Backend {
    fn open_or_init(vault_root: &Path) -> Result<Self> {
        let repo = match Repository::open(vault_root) {
            Ok(r) => r,
            Err(_) => Repository::init(vault_root).map_err(|e| Self::map_open(&e))?,
        };
        Ok(Self { repo, root: vault_root.to_path_buf() })
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
            Err(_) => return Ok(Divergence::Unchanged),
        };
        let fetched: AnnotatedCommit =
            self.repo.reference_to_annotated_commit(&fetch_head).map_err(|e| Self::map_read(&e))?;
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

    fn show(&self, sha: &str, path: &str) -> Result<Option<String>> {
        let oid = git2::Oid::from_str(sha)
            .map_err(|_| GitError::InvalidPath(format!("bad sha {sha}")))?;
        let commit = self.repo.find_commit(oid).map_err(|e| Self::map_read(&e))?;
        let tree = commit.tree().map_err(|e| Self::map_read(&e))?;
        let entry = match tree.get_path(Path::new(path)) {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        let obj = entry.to_object(&self.repo).map_err(|e| Self::map_read(&e))?;
        let Some(blob) = obj.as_blob() else { return Ok(None) };
        Ok(Some(String::from_utf8_lossy(blob.content()).into_owned()))
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
