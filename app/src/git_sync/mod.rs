//! The git transport engine, behind the sync seam (`sync.md`
//! `sync-transport-seam`, `git.md`).
//!
//! This is the git side of the pluggable transport: it sits above the
//! `hiker-git` `GitBackend` (which confines `git2`/libgit2) and below the
//! same triggers the libp2p engine uses (startup / interval / poke for
//! push-pull; save for commit-on-save). Two modes:
//!
//! - **Integrated** (`git-integrated-mode`): hiker drives commit + push/pull. A
//!   save schedules a debounced commit-on-save (`git-commit-on-save`) with the
//!   `Hiker-Author` trailer; rapid saves `--amend`-coalesce within the window.
//!   Push/pull runs on the sync triggers (`git-push-pull-rounds`); an inbound
//!   divergence feeds [`hiker_core::merge`] + the unified conflict surface,
//!   never git's own markers.
//! - **Manual** (`git-manual-mode`): the user drives git. Hiker tolerates HEAD
//!   moving (`git-tolerate-head-move`): any working-tree divergence from the
//!   last-known commit is folded as an external edit (`apply_external_edit`,
//!   the same 3-way fold a disk edit takes). Hiker never pushes/pulls/rebases
//!   here (`git-manual-commit-policy`); `.md` is canonical, not HEAD
//!   (`git-co-tenancy`).
//!
//! The engine implements [`hiker_sync::seam::Transport`] so the orchestration
//! drives it through the same verb surface as the libp2p engine.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hiker_core::config::vcs::{GitMode, GitSection};
use hiker_core::oplog::shapes::Author as CoreAuthor;
use hiker_core::oplog::OpLog;
use hiker_git::meta::{Author, Trailers};
use hiker_git::repo::{Divergence, GitBackend, Libgit2Backend};
use hiker_sync::seam::{Transport, TransportKind};

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The git transport engine. Holds the libgit2 backend (behind a mutex —
/// libgit2's `Repository` is `Send` but not `Sync`), the vault op log, the
/// `[git]` config, and the debounce machinery for commit-on-save.
pub struct GitSyncEngine {
    backend: Arc<Mutex<Libgit2Backend>>,
    oplog: Arc<OpLog>,
    config: GitSection,
    vault_root: PathBuf,
    /// Progress-line sink, drained into the same `sync_events` ring the libp2p
    /// engine uses (the Sync page reads one feed).
    events_tx: UnboundedSender<String>,
    /// Wake signal for the debounced commit-on-save task. A local save calls
    /// [`notify_local_change`](Transport::notify_local_change) → `notify_one()`.
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
        oplog: Arc<OpLog>,
        config: &GitSection,
        events_tx: UnboundedSender<String>,
        rt: Handle,
    ) -> Result<Self, String> {
        let backend = Libgit2Backend::open_or_init(vault_root)
            .map_err(|e| format!("git: open/init failed — {e}"))?;
        backend
            .ensure_hiker_ignored()
            .map_err(|e| format!("git: gitignore write failed — {e}"))?;
        let known_sha = backend.head_sha().unwrap_or(None);
        Ok(Self {
            backend: Arc::new(Mutex::new(backend)),
            oplog,
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

    /// Whether a push/pull remote is configured — git is a bidirectional sync
    /// only then (`sync-single-bidirectional-transport`). Commit-only local
    /// versioning (empty remote) is not a sync path.
    #[must_use]
    pub fn has_remote(&self) -> bool {
        !self.config.remote.trim().is_empty()
    }

    /// The engine's kill-switch token, for bootstrap's responder loop to break
    /// on alongside the session cancel.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
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
    /// moved the file on disk via the op-log `move_note` path.
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

    /// One push/pull round (`git-push-pull-rounds`), integrated mode only: pull
    /// then push. An inbound divergence is folded through the op log's 3-way
    /// merge (`fold_divergence`), never via `git merge` — so git conflict
    /// markers are never left in a file. A no-op when no remote is set.
    pub fn push_pull_round(&self) -> Result<(), String> {
        if self.config.mode == GitMode::Manual {
            // Manual mode never pulls/pushes (`git-manual-commit-policy`); its
            // only coupling is the HEAD-move fold below.
            return self.manual_reconcile();
        }
        if !self.has_remote() {
            return Ok(());
        }
        let remote = self.config.remote.clone();
        let divergence = {
            let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
            backend.pull(&remote).map_err(|e| format!("git: pull failed — {e}"))?
        };
        if let Divergence::Diverged { changed_paths } = divergence {
            self.fold_divergence(&changed_paths);
        }
        let backend = self.backend.lock().map_err(|_| "git: backend lock poisoned")?;
        match backend.push(&remote) {
            Ok(()) => Ok(()),
            Err(e) => {
                // A rejected non-fast-forward means the remote advanced again;
                // surface it — the next round pulls + folds first. We never
                // force-push (`git-co-tenancy`).
                self.log(format!("git: push deferred — {e}"));
                Ok(())
            }
        }
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

    /// Fold each changed path's on-disk content into the op log via
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
            match self.oplog.doc_id_for_path(rel) {
                Ok(Some(doc_id)) => match self.oplog.apply_external_edit(&doc_id, &disk) {
                    Ok(true) => self.log(format!("git: folded inbound change to {rel}")),
                    Ok(false) => {} // no-op echo
                    Err(e) => self.log(format!("git: fold failed for {rel} — {e}")),
                },
                Ok(None) => {
                    // No op-log doc yet — a brand-new file from the peer / git
                    // history. Register it with the inbound content authored
                    // `external` so it's tracked and indexed.
                    match self.oplog.create_document(rel, "note", &disk, &CoreAuthor::External) {
                        Ok(_) => self.log(format!("git: bound new inbound document {rel}")),
                        Err(e) => self.log(format!("git: could not bind {rel} — {e}")),
                    }
                }
                Err(e) => self.log(format!("git: doc lookup failed for {rel} — {e}")),
            }
        }
    }

    /// Spawn the debounced commit-on-save task (integrated mode, the SENDING
    /// side of `git-commit-on-save`). Loops on the `local_change` notify,
    /// coalesces a burst with the configured debounce, then commits (amending
    /// within the window), and pushes if a remote is set. Respects the cancel
    /// token. A no-op in manual mode unless `auto_commit` is on and the user
    /// hasn't committed (`git-manual-commit-policy`).
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

    /// One debounced commit-on-save pass. In manual mode, only commits when the
    /// user hasn't already (we never compete — `git-manual-commit-policy`): a
    /// fresh divergence means the user left it uncommitted, so we may commit it;
    /// if HEAD already moved (the user committed), we fold instead and don't
    /// re-commit. In integrated mode we always commit the save burst and push.
    fn commit_for_save_burst(&self) {
        if self.config.mode == GitMode::Manual {
            // Manual: reconcile any HEAD move first, then auto-commit the user's
            // uncommitted working tree only if they left it uncommitted.
            let _ = self.manual_reconcile();
            // After reconcile, anything still dirty is the user's own
            // uncommitted edit — commit it on their behalf (auto_commit is on).
            match self.commit_now(Author::User, false) {
                Ok(_) => {}
                Err(e) => self.log(e),
            }
            return;
        }
        match self.commit_now(Author::User, false) {
            Ok(Some(_)) => {
                if self.has_remote() {
                    if let Err(e) = self.push_pull_round() {
                        self.log(e);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => self.log(e),
        }
    }
}

impl Transport for GitSyncEngine {
    fn kind(&self) -> TransportKind {
        TransportKind::Git
    }

    fn is_bidirectional(&self) -> bool {
        // git is a bidirectional sync only with a push/pull remote in
        // integrated mode; manual mode never pushes/pulls.
        self.config.mode == GitMode::Integrated && self.has_remote()
    }

    fn notify_local_change(&self) {
        self.local_change.notify_one();
    }

    fn shutdown(&self) {
        self.cancel.cancel();
        self.log("git: stopped (disabled)");
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
        let oplog = Arc::new(OpLog::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode,
            remote: remote.to_string(),
            ..GitSection::default()
        };
        let engine = GitSyncEngine::new(
            dir.path(),
            oplog,
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
            .oplog
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
        let oplog = Arc::new(OpLog::open(dir.path()).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let section = GitSection {
            mode: GitMode::Manual,
            auto_commit: false,
            ..GitSection::default()
        };
        let engine =
            Arc::new(GitSyncEngine::new(dir.path(), oplog, &section, tx, Handle::current()).unwrap());
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
        // A tracked document with op-log history.
        let doc_id = engine
            .oplog
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

        let accepted = engine.oplog.materialize_accepted(&doc_id).unwrap().text;
        assert!(accepted.contains("user added a line"), "folded the user's edit: {accepted:?}");
        assert!(!accepted.contains("<<<<<<<"), "no git conflict markers left in the file");
        drop(dir);
    }

    #[tokio::test]
    async fn push_pull_round_is_noop_without_remote() {
        let (engine, dir) = build(GitMode::Integrated, "");
        // No remote configured — a round does nothing and never errors.
        assert!(engine.push_pull_round().is_ok());
        assert!(!engine.is_bidirectional(), "no remote ⇒ not a bidirectional sync");
        drop(dir);
    }

    #[tokio::test]
    async fn manual_mode_is_never_bidirectional() {
        let (engine, dir) = build(GitMode::Manual, "git@example.com:vault.git");
        // Even with a remote, manual mode never pushes/pulls.
        assert!(!engine.is_bidirectional());
        drop(dir);
    }
}
