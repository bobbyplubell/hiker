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
use hiker_git::repo::{Divergence, GitBackend, Libgit2Backend};

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
