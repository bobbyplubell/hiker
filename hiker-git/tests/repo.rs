//! Integration tests for the libgit2 [`GitBackend`] on temp repos.
//!
//! These exercise the verbs the git transport drives (`git.md`): commit-on-save
//! with a `Hiker-Author` trailer, an observed pure-rename commit with a
//! `Hiker-Rename` trailer that follows, working-tree divergence detection
//! (manual-mode HEAD-move fold), and `log`/`show` inspection. No network: push/
//! pull are not exercised here (they need a remote) — the local verbs are.

use std::fs;
use std::path::Path;

use hiker_git::meta::{Author, Trailers};
use hiker_git::repo::{ChangeStatus, Divergence, GitBackend, Libgit2Backend, MergeOutcome};
use hiker_git::GitError;

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, body).unwrap();
}

#[test]
fn commit_on_save_writes_author_trailer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    backend.ensure_hiker_ignored().unwrap();

    write(root, "note.md", "hello world\n");
    let trailers = Trailers::authored(Author::User);
    let sha = backend
        .commit_paths(&["note.md".into()], "Save note.md", &trailers, false)
        .unwrap()
        .expect("a commit was produced");

    let log = backend.log(10).unwrap();
    assert_eq!(log.len(), 1, "exactly one commit");
    assert_eq!(log[0].sha, sha);
    assert_eq!(log[0].subject, "Save note.md");
    assert_eq!(log[0].trailers.author, Author::User, "Hiker-Author trailer present + parsed");
}

#[test]
fn agent_author_trailer_round_trips_through_log() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    write(root, "a.md", "x\n");
    backend
        .commit_paths(&["a.md".into()], "agent edit", &Trailers::authored(Author::Agent("sess-9".into())), false)
        .unwrap()
        .unwrap();

    let log = backend.log(10).unwrap();
    assert_eq!(log[0].trailers.author, Author::Agent("sess-9".into()));
}

#[test]
fn hiker_dir_is_gitignored() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    backend.ensure_hiker_ignored().unwrap();

    let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|l| l.trim() == ".hiker/"), "gitignore has .hiker/ rule");

    // Idempotent: a second call adds no duplicate rule.
    backend.ensure_hiker_ignored().unwrap();
    let again = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert_eq!(again.matches(".hiker/").count(), 1, "rule not duplicated");

    // A staged-everything commit must not include the .hiker/ contents.
    write(root, ".hiker/ops/note.md.ops", "history\n");
    write(root, "note.md", "body\n");
    backend
        .commit_paths(&[], "initial", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    assert!(backend.show(&backend.head_sha().unwrap().unwrap(), "note.md").unwrap().is_some());
    assert!(
        backend.show(&backend.head_sha().unwrap().unwrap(), ".hiker/ops/note.md.ops").unwrap().is_none(),
        ".hiker/ content must not be committed"
    );
}

#[test]
fn observed_rename_is_pure_rename_commit_that_follows() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    let content = "stable byte-identical content\nline two\n";
    write(root, "old.md", content);
    backend
        .commit_paths(&["old.md".into()], "create", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Observe the move: the file moves on disk, bytes unchanged.
    fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
    let trailers = Trailers::renamed(Author::User, "old.md".into(), "new.md".into());
    backend.commit_rename("old.md", "new.md", &trailers).unwrap().unwrap();

    // The rename commit carries the Hiker-Rename trailer.
    let log = backend.log(10).unwrap();
    let rename_commit = &log[0];
    assert_eq!(
        rename_commit.trailers.rename,
        Some(("old.md".to_string(), "new.md".to_string())),
        "Hiker-Rename trailer present"
    );

    // The content survived byte-identical at the new path, and the old path is
    // gone — this is exactly the shape `git log --follow` / `-M` matches with
    // certainty (`git-observed-rename-commit`).
    let head = backend.head_sha().unwrap().unwrap();
    assert_eq!(backend.show(&head, "new.md").unwrap().as_deref(), Some(content));
    assert!(backend.show(&head, "old.md").unwrap().is_none());
}

#[test]
fn divergence_detects_working_tree_change_from_known_head() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    write(root, "doc.md", "v1\n");
    let sha = backend
        .commit_paths(&["doc.md".into()], "v1", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Clean against the commit we just made.
    assert_eq!(backend.divergence_from(Some(&sha)).unwrap(), Divergence::Unchanged);

    // Simulate an external editor / a HEAD-move-equivalent disk edit.
    write(root, "doc.md", "v2 edited outside hiker\n");
    match backend.divergence_from(Some(&sha)).unwrap() {
        Divergence::Diverged { changed_paths } => {
            assert!(changed_paths.contains(&"doc.md".to_string()), "doc.md flagged: {changed_paths:?}");
        }
        Divergence::Unchanged => panic!("expected divergence after an external edit"),
    }
}

#[test]
fn amend_coalesces_into_one_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    write(root, "n.md", "first\n");
    backend
        .commit_paths(&["n.md".into()], "save", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    // A rapid second save within the debounce window amends rather than adding.
    write(root, "n.md", "second\n");
    backend
        .commit_paths(&["n.md".into()], "save", &Trailers::authored(Author::User), true)
        .unwrap()
        .unwrap();

    assert_eq!(backend.log(10).unwrap().len(), 1, "amend collapsed into one commit");
    let head = backend.head_sha().unwrap().unwrap();
    assert_eq!(backend.show(&head, "n.md").unwrap().as_deref(), Some("second\n"));
}

#[test]
fn no_op_commit_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    write(root, "n.md", "x\n");
    backend
        .commit_paths(&["n.md".into()], "first", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    // Committing the same content again is a no-op.
    let again = backend
        .commit_paths(&["n.md".into()], "again", &Trailers::authored(Author::User), false)
        .unwrap();
    assert_eq!(again, None, "an identical-tree commit is a no-op");
}

#[test]
fn diff_paths_between_revs_resolves_head_and_short_shas() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    write(root, "kept.md", "unchanged\n");
    write(root, "edited.md", "v1\n");
    write(root, "removed.md", "going away\n");
    let base = backend
        .commit_paths(&[], "base", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    write(root, "edited.md", "v2\n");
    write(root, "added.md", "new file\n");
    fs::remove_file(root.join("removed.md")).unwrap();
    backend
        .commit_paths(&[], "head", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    let expected = vec![
        ("added.md".to_string(), ChangeStatus::Added),
        ("edited.md".to_string(), ChangeStatus::Modified),
        ("removed.md".to_string(), ChangeStatus::Deleted),
    ];
    // Full sha → symbolic HEAD.
    assert_eq!(backend.diff_paths(&base, Some("HEAD")).unwrap(), expected);
    // Short sha → short sha (rev resolution, not just 40-hex oids).
    let head = backend.head_sha().unwrap().unwrap();
    assert_eq!(backend.diff_paths(&base[..7], Some(&head[..7])).unwrap(), expected);
    // An unknown rev is an error, not an empty diff.
    assert!(backend.diff_paths("no-such-rev", Some("HEAD")).is_err());
}

#[test]
fn diff_paths_against_workdir_sees_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    backend.ensure_hiker_ignored().unwrap();

    write(root, "edited.md", "v1\n");
    write(root, "removed.md", "going away\n");
    backend
        .commit_paths(&[], "base", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Uncommitted working-tree changes: an edit, an untracked file, a delete,
    // and hiker-local state that must never appear in the diff.
    write(root, "edited.md", "v2 on disk only\n");
    write(root, "added.md", "untracked\n");
    fs::remove_file(root.join("removed.md")).unwrap();
    write(root, ".hiker/ops/edited.md.ops", "local history\n");

    assert_eq!(
        backend.diff_paths("HEAD", None).unwrap(),
        vec![
            ("added.md".to_string(), ChangeStatus::Added),
            ("edited.md".to_string(), ChangeStatus::Modified),
            ("removed.md".to_string(), ChangeStatus::Deleted),
        ],
        "tree-to-workdir diff sees the edit, the untracked add, and the delete — never .hiker/"
    );
}

#[test]
fn diff_paths_reports_a_move_as_renamed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();

    let content = "stable byte-identical content\nline two\n";
    write(root, "old.md", content);
    let base = backend
        .commit_paths(&[], "base", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    fs::rename(root.join("old.md"), root.join("new.md")).unwrap();
    backend
        .commit_paths(&[], "moved", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    assert_eq!(
        backend.diff_paths(&base, Some("HEAD")).unwrap(),
        vec![("new.md".to_string(), ChangeStatus::Renamed)],
        "a byte-identical delete+add pair collapses to one Renamed row at the new path"
    );
}

/// Submodule mode (`[git] submodules = "submodule"`): a repo nested in the
/// vault is declared in `.gitmodules` and folded into the vault commit as a
/// gitlink, so the code commit pointer travels with vault push/pull.
/// [git-nested-repo-submodule]
#[test]
fn submodule_mode_declares_and_tracks_a_nested_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // A nested repo at <vault>/sub with its own commit (a HEAD to pin).
    let sub = root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let sub_backend = Libgit2Backend::open_or_init(&sub).unwrap();
    write(&sub, "code.rs", "fn main() {}\n");
    sub_backend
        .commit_paths(&["code.rs".into()], "sub init", &Trailers::authored(Author::User), false)
        .unwrap()
        .expect("nested repo commit");

    // The vault repo with submodule tracking ON.
    let mut backend = Libgit2Backend::open_or_init(root).unwrap();
    backend.ensure_hiker_ignored().unwrap();
    backend.set_submodule_tracking(true);
    backend.ensure_submodules_registered().unwrap();

    write(root, "plan.md", "# plan\n");
    let sha = backend
        .commit_paths(&[], "vault state", &Trailers::authored(Author::User), false)
        .unwrap()
        .expect("vault commit");

    // The nested repo is a declared submodule, and `.gitmodules` travels in the
    // vault commit (so the gitlink pointer is part of what push/pull moves).
    assert!(
        backend.submodule_paths().unwrap().iter().any(|p| p == "sub"),
        "nested repo declared as a submodule",
    );
    let committed_gitmodules = backend
        .show(&sha, ".gitmodules")
        .unwrap()
        .expect(".gitmodules committed");
    assert!(committed_gitmodules.contains("path = sub"), "{committed_gitmodules}");
}

/// Skip mode (the default) leaves a nested repo independent: no `.gitmodules`,
/// no declared submodule, the subtree excluded from the vault commit.
/// [git-nested-repo-submodule]
#[test]
fn skip_mode_default_leaves_nested_repo_untracked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let sub = root.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let sub_backend = Libgit2Backend::open_or_init(&sub).unwrap();
    write(&sub, "code.rs", "x\n");
    sub_backend
        .commit_paths(&["code.rs".into()], "sub", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Default backend — submodule tracking left off.
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    write(root, "plan.md", "# plan\n");
    backend
        .commit_paths(&[], "vault", &Trailers::authored(Author::User), false)
        .unwrap()
        .expect("vault commit");

    assert!(
        backend.submodule_paths().unwrap().is_empty(),
        "skip mode declares no submodules",
    );
    assert!(!root.join(".gitmodules").exists(), "skip mode writes no .gitmodules");
}

/// Prefix-sharing nested paths (`a/sub` and `a/sub-extra`) must each get their
/// OWN `.gitmodules` stanza + `submodule.<name>.url`. The old substring probe
/// (`gm.contains("path = a/sub")`) found that text inside the `a/sub-extra`
/// stanza and skipped `a/sub` — leaving its gitlink unresolvable on a fresh
/// clone. Registration is now line-exact. [git-nested-repo-submodule]
#[test]
fn submodule_registration_does_not_collide_on_shared_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Two nested repos whose paths share a prefix: `a/sub` ⊂ `a/sub-extra`.
    for rel in ["a/sub", "a/sub-extra"] {
        let nested = root.join(rel);
        fs::create_dir_all(&nested).unwrap();
        let nb = Libgit2Backend::open_or_init(&nested).unwrap();
        write(&nested, "code.rs", "fn main() {}\n");
        nb.commit_paths(&["code.rs".into()], "init", &Trailers::authored(Author::User), false)
            .unwrap()
            .expect("nested commit");
    }

    let mut backend = Libgit2Backend::open_or_init(root).unwrap();
    backend.ensure_hiker_ignored().unwrap();
    backend.set_submodule_tracking(true);
    backend.ensure_submodules_registered().unwrap();

    // BOTH paths declared (the prefix-shorter one is not swallowed by the longer).
    let mut declared = backend.submodule_paths().unwrap();
    declared.sort();
    assert_eq!(
        declared,
        vec!["a/sub".to_string(), "a/sub-extra".to_string()],
        "both prefix-sharing repos declared as submodules",
    );

    // Each has its own resolvable url in the vault config (so a fresh clone's
    // `submodule update --init` can find both).
    let cfg = std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap(), "config", "--get", "submodule.a/sub.url"])
        .output()
        .unwrap();
    assert!(cfg.status.success(), "submodule.a/sub.url is set");
    assert!(!cfg.stdout.is_empty(), "submodule.a/sub.url has a value");
}

// ---------------------------------------------------------------------------
// G1: push, merge cluster, branch_status, staging ops, update_submodules.
// A local on-disk path is a valid (anonymous) git "remote", so these exercise
// the network verbs without any network.
// ---------------------------------------------------------------------------

/// `push` sends the current branch to a (local) remote; the pushed commit is
/// then visible in the remote repo. [git-push-pull-rounds]
#[test]
fn push_sends_current_branch_to_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    fs::create_dir_all(&local).unwrap();
    // A bare remote accepts pushes to its checked-out-able refs cleanly.
    git2::Repository::init_bare(&remote).unwrap();

    let local_be = Libgit2Backend::open_or_init(&local).unwrap();
    write(&local, "note.md", "hello\n");
    let sha = local_be
        .commit_paths(&[], "note", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    local_be.push(remote.to_str().unwrap()).unwrap();

    // The remote now holds the pushed commit.
    let remote_be = Libgit2Backend::open_or_init(&remote).unwrap();
    assert_eq!(remote_be.show(&sha, "note.md").unwrap().as_deref(), Some("hello\n"));
}

/// `push` with an empty remote is `NoRemote`. [git-push-pull-rounds]
#[test]
fn push_with_no_remote_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let local_be = Libgit2Backend::open_or_init(tmp.path()).unwrap();
    write(tmp.path(), "x.md", "x\n");
    local_be.commit_paths(&[], "x", &Trailers::authored(Author::User), false).unwrap().unwrap();
    assert!(matches!(local_be.push("").unwrap_err(), GitError::NoRemote));
}

/// A clean 3-way merge: local and remote each commit a disjoint file on top of
/// a shared base. `fetch_and_merge` lets git reconcile → `Merged`, the result
/// is a real 2-parent merge commit, and both sides' files are present.
/// [git-merge-via-git]
#[test]
fn fetch_and_merge_clean_three_way_makes_a_two_parent_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();

    let remote_be = Libgit2Backend::open_or_init(&remote).unwrap();
    write(&remote, "base.md", "shared base\n");
    remote_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();
    let base_sha = remote_be.head_sha().unwrap().unwrap();

    let local_be = Libgit2Backend::open_or_init(&local).unwrap();
    // First fetch fast-forwards local to the shared base.
    assert_eq!(
        local_be.fetch_and_merge(remote.to_str().unwrap(), &Trailers::authored(Author::User)).unwrap(),
        MergeOutcome::Merged(base_sha.clone()),
    );

    // Remote adds theirs.md; local adds ours.md (disjoint).
    write(&remote, "theirs.md", "from remote\n");
    remote_be.commit_paths(&[], "theirs", &Trailers::authored(Author::User), false).unwrap().unwrap();
    write(&local, "ours.md", "from local\n");
    let local_sha = local_be
        .commit_paths(&[], "ours", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    let outcome = local_be
        .fetch_and_merge(remote.to_str().unwrap(), &Trailers::authored(Author::Sync("origin".into())))
        .unwrap();
    let merge_sha = match outcome {
        MergeOutcome::Merged(sha) => sha,
        other => panic!("expected a clean Merged, got {other:?}"),
    };
    assert!(!local_be.merge_in_progress().unwrap(), "merge state cleaned up");
    assert_eq!(local_be.parent_shas(&merge_sha).unwrap().len(), 2, "2-parent merge commit");
    assert!(local_be.parent_shas(&merge_sha).unwrap().contains(&local_sha), "local parent");
    assert_eq!(local_be.show(&merge_sha, "ours.md").unwrap().as_deref(), Some("from local\n"));
    assert_eq!(local_be.show(&merge_sha, "theirs.md").unwrap().as_deref(), Some("from remote\n"));
    assert_eq!(local_be.show(&merge_sha, "base.md").unwrap().as_deref(), Some("shared base\n"));
}

/// A fast-forward (local strictly behind the remote): `fetch_and_merge` returns
/// `Merged` and HEAD advances; an already-current fetch is `UpToDate`.
/// [git-merge-via-git]
#[test]
fn fetch_and_merge_fast_forwards_then_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();

    let remote_be = Libgit2Backend::open_or_init(&remote).unwrap();
    write(&remote, "a.md", "one\n");
    remote_be.commit_paths(&[], "c1", &Trailers::authored(Author::User), false).unwrap().unwrap();
    let c1 = remote_be.head_sha().unwrap().unwrap();

    let local_be = Libgit2Backend::open_or_init(&local).unwrap();
    let url = remote.to_str().unwrap();
    assert_eq!(
        local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap(),
        MergeOutcome::Merged(c1.clone()),
    );

    write(&remote, "b.md", "two\n");
    remote_be.commit_paths(&[], "c2", &Trailers::authored(Author::User), false).unwrap().unwrap();
    let c2 = remote_be.head_sha().unwrap().unwrap();
    assert_eq!(
        local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap(),
        MergeOutcome::Merged(c2.clone()),
        "fast-forward to remote head",
    );
    assert_eq!(local_be.head_sha().unwrap().unwrap(), c2, "HEAD advanced");
    assert_eq!(local_be.parent_shas(&c2).unwrap(), vec![c1], "single-parent (no synthetic merge)");
    assert_eq!(
        local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap(),
        MergeOutcome::UpToDate,
    );
}

/// A divergent conflict: `fetch_and_merge` returns `Conflicted`, leaves
/// `MERGE_HEAD` + zdiff3 markers on disk (no commit); after the user resolves,
/// `finalize_merge` writes a 2-parent commit and clears merge state.
/// [git-merge-via-git, git-conflict-inline-markers]
#[test]
fn fetch_and_merge_conflict_then_finalize() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();

    let remote_be = Libgit2Backend::open_or_init(&remote).unwrap();
    write(&remote, "doc.md", "base line\n");
    remote_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();

    let local_be = Libgit2Backend::open_or_init(&local).unwrap();
    let url = remote.to_str().unwrap();
    local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap();

    write(&remote, "doc.md", "remote edit\n");
    remote_be.commit_paths(&[], "remote edit", &Trailers::authored(Author::User), false).unwrap().unwrap();
    write(&local, "doc.md", "local edit\n");
    local_be.commit_paths(&[], "local edit", &Trailers::authored(Author::User), false).unwrap().unwrap();

    match local_be.fetch_and_merge(url, &Trailers::authored(Author::Sync("origin".into()))).unwrap() {
        MergeOutcome::Conflicted(paths) => assert_eq!(paths, vec!["doc.md".to_string()]),
        other => panic!("expected Conflicted, got {other:?}"),
    }
    assert!(local_be.merge_in_progress().unwrap(), "MERGE_HEAD left set");
    assert_eq!(local_be.merge_conflict_paths().unwrap(), vec!["doc.md".to_string()]);

    // git wrote zdiff3 conflict markers into the working file (no commit yet).
    let on_disk = fs::read_to_string(local.join("doc.md")).unwrap();
    assert!(on_disk.contains("<<<<<<<"), "conflict markers present: {on_disk}");
    assert!(on_disk.contains("|||||||"), "zdiff3 base section present: {on_disk}");

    // The user resolves on disk, then finalizes → a 2-parent merge commit.
    write(&local, "doc.md", "resolved by hand\n");
    let merge_sha = local_be
        .finalize_merge(&Trailers::authored(Author::User))
        .unwrap()
        .expect("a finalize commit");
    assert!(!local_be.merge_in_progress().unwrap(), "merge state cleared");
    assert_eq!(local_be.parent_shas(&merge_sha).unwrap().len(), 2, "2-parent merge commit");
    assert_eq!(
        local_be.show(&merge_sha, "doc.md").unwrap().as_deref(),
        Some("resolved by hand\n"),
    );
}

/// `finalize_merge` with no merge in progress (`MERGE_HEAD` absent) errors —
/// it's only callable to complete a conflicted merge. The `NeedsMerge` guard
/// inside is defensive (git's `add` resolves conflicted index entries, so a
/// realistic resolve-then-finalize never hits it). [git-conflict-inline-markers]
#[test]
fn finalize_merge_without_merge_in_progress_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let be = Libgit2Backend::open_or_init(tmp.path()).unwrap();
    write(tmp.path(), "a.md", "x\n");
    be.commit_paths(&[], "c1", &Trailers::authored(Author::User), false).unwrap().unwrap();
    assert!(!be.merge_in_progress().unwrap());
    assert!(matches!(
        be.finalize_merge(&Trailers::authored(Author::User)).unwrap_err(),
        GitError::Commit(_),
    ));
}

/// `abort_merge` after a conflict resets to HEAD and clears the merge state.
/// [git-merge-via-git]
#[test]
fn abort_merge_clears_in_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote");
    let local = tmp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();

    let remote_be = Libgit2Backend::open_or_init(&remote).unwrap();
    write(&remote, "doc.md", "base\n");
    remote_be.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();
    let local_be = Libgit2Backend::open_or_init(&local).unwrap();
    let url = remote.to_str().unwrap();
    local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap();

    write(&remote, "doc.md", "remote\n");
    remote_be.commit_paths(&[], "remote", &Trailers::authored(Author::User), false).unwrap().unwrap();
    write(&local, "doc.md", "local\n");
    let local_sha = local_be
        .commit_paths(&[], "local", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    assert!(matches!(
        local_be.fetch_and_merge(url, &Trailers::authored(Author::User)).unwrap(),
        MergeOutcome::Conflicted(_),
    ));
    assert!(local_be.merge_in_progress().unwrap());

    local_be.abort_merge().unwrap();
    assert!(!local_be.merge_in_progress().unwrap(), "abort cleared MERGE_HEAD");
    assert_eq!(local_be.head_sha().unwrap().unwrap(), local_sha, "HEAD back at local commit");
    assert_eq!(fs::read_to_string(local.join("doc.md")).unwrap(), "local\n", "working tree reset");
}

/// `branch_status`: a fresh born branch has its name, no upstream, zero counts.
/// [git-branch-status]
#[test]
fn branch_status_no_upstream_reports_branch_and_zero_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let be = Libgit2Backend::open_or_init(tmp.path()).unwrap();
    write(tmp.path(), "a.md", "x\n");
    be.commit_paths(&[], "c1", &Trailers::authored(Author::User), false).unwrap().unwrap();

    let st = be.branch_status().unwrap();
    assert!(st.branch.is_some(), "a born branch has a name");
    assert!(!st.has_upstream, "no upstream configured");
    assert_eq!((st.ahead, st.behind), (0, 0));
}

/// `branch_status` ahead/behind: local commits past the upstream count as
/// ahead; remote commits count as behind. [git-branch-status]
#[test]
fn branch_status_counts_ahead_and_behind_an_upstream() {
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("remote.git");
    let local = tmp.path().join("local");
    git2::Repository::init_bare(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();

    // Seed a commit, push it, then wire an upstream tracking branch.
    let be = Libgit2Backend::open_or_init(&local).unwrap();
    write(&local, "a.md", "one\n");
    be.commit_paths(&[], "c1", &Trailers::authored(Author::User), false).unwrap().unwrap();
    be.push(remote.to_str().unwrap()).unwrap();

    // Configure origin + upstream the way a real clone would, via plain git.
    let run = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-C", local.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    run(&["remote", "add", "origin", remote.to_str().unwrap()]);
    run(&["fetch", "origin"]);
    let head = std::process::Command::new("git")
        .args(["-C", local.to_str().unwrap(), "symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    let branch = String::from_utf8(head.stdout).unwrap().trim().to_string();
    run(&["branch", "--set-upstream-to", &format!("origin/{branch}")]);

    // Even now: zero ahead/behind.
    let st = Libgit2Backend::open_or_init(&local).unwrap().branch_status().unwrap();
    assert!(st.has_upstream, "upstream now configured");
    assert_eq!((st.ahead, st.behind), (0, 0), "in sync after fetch");

    // Two local commits past the upstream → ahead = 2.
    write(&local, "b.md", "two\n");
    be.commit_paths(&[], "c2", &Trailers::authored(Author::User), false).unwrap().unwrap();
    write(&local, "c.md", "three\n");
    be.commit_paths(&[], "c3", &Trailers::authored(Author::User), false).unwrap().unwrap();
    let st = Libgit2Backend::open_or_init(&local).unwrap().branch_status().unwrap();
    assert_eq!(st.ahead, 2, "two local-only commits");
    assert_eq!(st.behind, 0);
}

/// `stage_paths` / `unstage_paths`: staging an untracked file makes it staged;
/// unstaging removes it from the index again (it isn't at HEAD). A tracked
/// path's edit unstages back to its HEAD blob. [git-staging-ops]
#[test]
fn stage_and_unstage_paths_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let be = Libgit2Backend::open_or_init(root).unwrap();
    write(root, "tracked.md", "v1\n");
    be.commit_paths(&["tracked.md".into()], "init", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // A new untracked file + an edit to the tracked one.
    write(root, "new.md", "fresh\n");
    write(root, "tracked.md", "v2\n");

    // diff_paths(HEAD, workdir) sees both before staging.
    let before = be.diff_paths("HEAD", None).unwrap();
    assert!(before.iter().any(|(p, _)| p == "new.md"));
    assert!(before.iter().any(|(p, _)| p == "tracked.md"));

    // Stage both, then unstage them.
    be.stage_paths(&["new.md".into(), "tracked.md".into()]).unwrap();
    be.unstage_paths(&["new.md".into(), "tracked.md".into()]).unwrap();

    // After unstaging, the index matches HEAD for the tracked path and the new
    // file is no longer indexed — but the working-tree edits remain on disk.
    assert_eq!(fs::read_to_string(root.join("tracked.md")).unwrap(), "v2\n", "workdir untouched");
    assert_eq!(fs::read_to_string(root.join("new.md")).unwrap(), "fresh\n", "workdir untouched");
    // Committing with an empty path-set stages the whole tree, so both still
    // land — proof the unstage didn't touch the working tree, only the index.
    let after = be.diff_paths("HEAD", None).unwrap();
    assert!(after.iter().any(|(p, _)| p == "new.md"), "new.md still a workdir change");
    assert!(after.iter().any(|(p, _)| p == "tracked.md"), "tracked.md still a workdir change");
}

/// `discard_paths`: a tracked-file edit reverts to its HEAD content; a
/// newly-added untracked file is removed from disk. [git-staging-ops]
#[test]
fn discard_paths_restores_head_and_removes_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let be = Libgit2Backend::open_or_init(root).unwrap();
    write(root, "tracked.md", "committed\n");
    be.commit_paths(&["tracked.md".into()], "init", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Edit the tracked file and add a new one; stage them (discard works on
    // both the index and the working tree).
    write(root, "tracked.md", "dirty edit\n");
    write(root, "added.md", "new\n");
    be.stage_paths(&["tracked.md".into(), "added.md".into()]).unwrap();

    be.discard_paths(&["tracked.md".into(), "added.md".into()]).unwrap();

    assert_eq!(
        fs::read_to_string(root.join("tracked.md")).unwrap(),
        "committed\n",
        "tracked edit reverted to HEAD",
    );
    assert!(!root.join("added.md").exists(), "untracked addition removed");
    assert_eq!(be.diff_paths("HEAD", None).unwrap(), vec![], "working tree clean vs HEAD");
}

/// `update_submodules` populates an empty submodule directory at its pinned
/// commit — the CODE-IN-VAULT fresh-clone repair. A non-recursive clone leaves
/// the submodule path empty; `update_submodules` clones + checks out the pinned
/// commit. [git-nested-repo-submodule]
#[test]
fn update_submodules_populates_a_pinned_submodule() {
    let tmp = tempfile::tempdir().unwrap();
    // Helper to run plain git in a repo, asserting success.
    let git = |cwd: &Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-C", cwd.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };

    // A standalone "origin" repo for the submodule's content.
    let sub_origin = tmp.path().join("sub_origin");
    fs::create_dir_all(&sub_origin).unwrap();
    let sub_be = Libgit2Backend::open_or_init(&sub_origin).unwrap();
    write(&sub_origin, "code.rs", "fn main() {}\n");
    sub_be.commit_paths(&["code.rs".into()], "sub init", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // A vault that declares + COMMITS the submodule (the gitlink + .gitmodules
    // travel in the vault commit, like a real CODE-IN-VAULT setup).
    let vault = tmp.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    let vbe = Libgit2Backend::open_or_init(&vault).unwrap();
    write(&vault, "plan.md", "# plan\n");
    vbe.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();
    git(&vault, &["-c", "protocol.file.allow=always", "submodule", "add", sub_origin.to_str().unwrap(), "sub"]);
    git(&vault, &["commit", "-m", "add sub"]);

    // A fresh, NON-recursive clone of the vault — the broken state: the
    // submodule path is empty (uninitialized), the gitlink is recorded.
    let clone = tmp.path().join("clone");
    git(tmp.path(), &["-c", "protocol.file.allow=always", "clone", vault.to_str().unwrap(), clone.to_str().unwrap()]);
    assert!(!clone.join("sub/code.rs").exists(), "fresh clone leaves the submodule empty");

    // update_submodules populates it at the pinned commit (init + checkout).
    let cbe = Libgit2Backend::open_or_init(&clone).unwrap();
    cbe.update_submodules().unwrap();
    assert!(clone.join("sub/code.rs").exists(), "submodule populated at the pinned commit");
}

/// G2 restore-on-open: `restore_uninitialized_submodules` populates ONLY the
/// empty-gitlink (uninitialized) submodule of a fresh clone, then is a no-op
/// once it's populated — it never re-checks-out a populated submodule, so a
/// user's nested-repo work is never clobbered by reopening the vault.
/// [git-nested-repo-submodule]
#[test]
fn restore_uninitialized_submodules_inits_then_leaves_populated_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let git = |cwd: &Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(["-C", cwd.to_str().unwrap()])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };

    // Standalone origin repo for the submodule content.
    let sub_origin = tmp.path().join("sub_origin");
    fs::create_dir_all(&sub_origin).unwrap();
    let sub_be = Libgit2Backend::open_or_init(&sub_origin).unwrap();
    write(&sub_origin, "code.rs", "fn main() {}\n");
    sub_be.commit_paths(&["code.rs".into()], "sub init", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    // Vault that declares + commits the submodule.
    let vault = tmp.path().join("vault");
    fs::create_dir_all(&vault).unwrap();
    let vbe = Libgit2Backend::open_or_init(&vault).unwrap();
    write(&vault, "plan.md", "# plan\n");
    vbe.commit_paths(&[], "base", &Trailers::authored(Author::User), false).unwrap().unwrap();
    git(&vault, &["-c", "protocol.file.allow=always", "submodule", "add", sub_origin.to_str().unwrap(), "sub"]);
    git(&vault, &["commit", "-m", "add sub"]);

    // Fresh non-recursive clone — the broken empty-submodule state.
    let clone = tmp.path().join("clone");
    git(tmp.path(), &["-c", "protocol.file.allow=always", "clone", vault.to_str().unwrap(), clone.to_str().unwrap()]);
    assert!(!clone.join("sub/code.rs").exists(), "fresh clone leaves the submodule empty");

    let cbe = Libgit2Backend::open_or_init(&clone).unwrap();
    // First pass: the uninitialized submodule is populated (count == 1).
    let restored = cbe.restore_uninitialized_submodules().unwrap();
    assert_eq!(restored, 1, "the one uninitialized submodule was populated");
    assert!(clone.join("sub/code.rs").exists(), "submodule checked out at the pin");

    // Simulate the user's own nested-repo work in the now-populated submodule.
    write(&clone, "sub/code.rs", "fn main() { /* user edit */ }\n");

    // Second pass: nothing uninitialized remains, so it's a no-op (count == 0)
    // and the user's edit is left untouched — never re-checked-out.
    let restored2 = cbe.restore_uninitialized_submodules().unwrap();
    assert_eq!(restored2, 0, "a populated submodule is not re-initialized");
    assert_eq!(
        fs::read_to_string(clone.join("sub/code.rs")).unwrap(),
        "fn main() { /* user edit */ }\n",
        "the user's nested-repo edit is left untouched",
    );
}

// ---------------------------------------------------------------------------
// Per-hunk staging (G5): stage_hunk / unstage_hunk / discard_hunk.
//
// A unified-diff patch for a single hunk is applied to the index (stage),
// reverse-applied to the index (unstage), or reverse-applied to the working
// tree (discard). The build/apply logic is what's unit-tested here; the diff
// view's per-hunk buttons (egui) are smoke-run-verified separately.
// ---------------------------------------------------------------------------

/// Patch text for the FIRST of two separate single-line edits in a 6-line file:
/// line 2 `b` → `B`. The hunk carries one line of context on each side, so its
/// context anchors uniquely against the file. (Lines 1-indexed in `@@`.)
const fn first_hunk_patch() -> &'static str {
    "diff --git a/doc.md b/doc.md\nindex 1111111..2222222 100644\n--- a/doc.md\n+++ b/doc.md\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"
}

/// Patch text for the SECOND edit in the same file: line 5 `e` → `E`.
const fn second_hunk_patch() -> &'static str {
    "diff --git a/doc.md b/doc.md\nindex 1111111..2222222 100644\n--- a/doc.md\n+++ b/doc.md\n@@ -4,3 +4,3 @@\n d\n-e\n+E\n f\n"
}

/// Seed a repo with `doc.md` = `a..f`, committed, then both edits applied on
/// disk (`B` and `E`). Returns the backend + root.
fn seed_two_edits(tmp: &tempfile::TempDir) -> Libgit2Backend {
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    write(root, "doc.md", "a\nb\nc\nd\ne\nf\n");
    backend
        .commit_paths(&["doc.md".into()], "seed", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    // Both regions edited in the working tree.
    write(root, "doc.md", "a\nB\nc\nd\nE\nf\n");
    backend
}

#[test]
fn stage_hunk_stages_only_that_hunk() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = seed_two_edits(&tmp);

    // Stage only the first hunk (line 2). The second edit (line 5) stays out.
    backend.stage_hunk(first_hunk_patch()).unwrap();

    let staged = backend.diff_tree_to_index().unwrap();
    assert_eq!(
        staged,
        vec![("doc.md".to_string(), ChangeStatus::Modified)],
        "doc.md is partially staged",
    );
    // The working tree still carries both edits (stage doesn't touch the disk).
    assert_eq!(
        fs::read_to_string(tmp.path().join("doc.md")).unwrap(),
        "a\nB\nc\nd\nE\nf\n",
        "working tree untouched by index-only apply",
    );
    // The unstaged (worktree-vs-index) diff still shows the second hunk's edit
    // is NOT staged: there remains a working-tree-vs-index delta.
    let unstaged = backend.diff_index_to_workdir().unwrap();
    assert_eq!(
        unstaged,
        vec![("doc.md".to_string(), ChangeStatus::Modified)],
        "the second hunk remains unstaged",
    );

    // Commit the index: only the first hunk lands; line 5 is still `e` at HEAD.
    let sha = backend
        .commit_index("stage first hunk", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        backend.show(&sha, "doc.md").unwrap().as_deref(),
        Some("a\nB\nc\nd\ne\nf\n"),
        "exactly the first hunk was committed",
    );
}

#[test]
fn unstage_hunk_reverses_the_stage() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = seed_two_edits(&tmp);

    // Stage both hunks, then unstage just the first.
    backend.stage_hunk(first_hunk_patch()).unwrap();
    backend.stage_hunk(second_hunk_patch()).unwrap();
    backend.unstage_hunk(first_hunk_patch()).unwrap();

    // The index now matches HEAD on line 2 but holds the line-5 edit. Commit
    // and confirm only the second hunk landed.
    let sha = backend
        .commit_index("only second", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        backend.show(&sha, "doc.md").unwrap().as_deref(),
        Some("a\nb\nc\nd\nE\nf\n"),
        "unstage_hunk reversed the first hunk in the index",
    );
}

#[test]
fn discard_hunk_reverts_only_that_hunk_in_the_working_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = seed_two_edits(&tmp);

    // Discard the first hunk on disk: line 2 reverts to `b`, line 5 keeps `E`.
    backend.discard_hunk(first_hunk_patch()).unwrap();

    assert_eq!(
        fs::read_to_string(tmp.path().join("doc.md")).unwrap(),
        "a\nb\nc\nd\nE\nf\n",
        "only the first hunk was reverted on disk; the second edit survives",
    );
}

#[test]
fn stage_hunk_on_a_stale_patch_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let backend = Libgit2Backend::open_or_init(root).unwrap();
    // A file whose content does NOT match the patch's context.
    write(root, "doc.md", "totally\ndifferent\ncontent\n");
    backend
        .commit_paths(&["doc.md".into()], "seed", &Trailers::authored(Author::User), false)
        .unwrap()
        .unwrap();

    let err = backend.stage_hunk(first_hunk_patch()).unwrap_err();
    assert!(matches!(err, GitError::Apply(_)), "a non-matching hunk is a clean Apply error: {err:?}");
}
