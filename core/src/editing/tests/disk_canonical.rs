//! The op-log-stays-byte-identical-to-disk invariant (`op-log-disk-canonical`):
//! seeding an existing file verifies-and-skips rather than rewriting it (so a
//! first open never churns a note's mtime), the seed path refuses bytes that
//! diverge from disk, and folding external edits of every shape leaves
//! `materialize(accepted)` byte-equal to the folded text. Split out of the
//! parent `tests` module to keep each file within the length budget.

use super::super::error::Error;
use super::super::shapes::Author;
use super::super::*;
use crate::ops::op_writes as bridge;
use crate::vault::Vault;
use tempfile::TempDir;

#[test]
fn bootstrap_does_not_rewrite_files_on_disk() {
    // First-open seeding must NOT rewrite the user's notes over themselves: the
    // bytes are already canonical, and a rewrite would churn every note's mtime
    // (re-stamping the whole vault on first open). Seeding goes through
    // `seed_document`, which verifies bytes against disk rather than writing.
    use std::time::{Duration, SystemTime};

    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let note = td.path().join("a.md");
    std::fs::write(&note, "# A\nbody\n").unwrap();

    // Backdate the file's mtime to a fixed instant well in the past, then
    // assert bootstrap leaves it exactly there — a rewrite would bump it to now.
    let backdated = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&note)
        .unwrap()
        .set_modified(backdated)
        .unwrap();
    let before = std::fs::metadata(&note).unwrap().modified().unwrap();

    let log = LayeredDoc::open(td.path()).unwrap();
    assert_eq!(bridge::bootstrap(&vault, &log).unwrap(), 1, "note seeded");

    // The doc is registered with the on-disk bytes…
    let id = log.doc_id_for_path("a.md").unwrap().expect("a.md mapped");
    assert_eq!(log.materialize_accepted(&id).unwrap().text, "# A\nbody\n");
    // …and the file on disk was never touched.
    let after = std::fs::metadata(&note).unwrap().modified().unwrap();
    assert_eq!(before, after, "bootstrap must not rewrite the note (mtime churn)");
}

#[test]
fn seed_document_errors_when_disk_differs() {
    // The seed path verifies the bytes it would write against disk and refuses
    // (rather than silently overwriting) when they diverge.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.md"), "on-disk\n").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();

    let err = log
        .seed_document("a.md", "note", "different\n", &Author::User)
        .expect_err("seed must reject bytes that don't match disk");
    assert!(
        matches!(err, Error::SeedMismatch { .. }),
        "expected SeedMismatch, got {err:?}"
    );
}

#[test]
fn apply_external_edit_round_trips_across_edit_shapes() {
    // The op-log-must-never-diverge-from-disk invariant, exercised end to end:
    // folding successive external (disk) edits of many shapes into a tracked
    // doc must leave `materialize(accepted)` byte-equal to each folded text.
    // Sequential so each shape diffs against the previous (real fold deltas,
    // not first-write fulls). Backs the runtime `FoldRoundTrip` guard and the
    // `overlay::span_delta_round_trips` proptest with a through-the-LayeredDoc case.
    let td = TempDir::new().unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();
    let id = log
        .create_document("a.md", "note", "start\n", &Author::User)
        .unwrap();

    let shapes = [
        "start\nappended line\n",                 // pure append
        "prepended\nstart\nappended line\n",      // prepend
        "prepended\nstart\nappended line\n",      // no-op (byte-identical)
        "prepended\nSTART\nappended line\n",      // mid-line replace
        "prepended\nappended line\n",             // interior delete
        "totally different content entirely\n",   // full replace
        "café → multibyte é text\n",               // multibyte boundaries
        "",                                        // empty
        "back to one line\n",                      // grow from empty
    ];
    for (i, shape) in shapes.iter().enumerate() {
        log.apply_external_edit(&id, shape)
            .unwrap_or_else(|e| panic!("fold shape {i} ({shape:?}) errored: {e}"));
        assert_eq!(
            &log.materialize_accepted(&id).unwrap().text,
            shape,
            "materialize(accepted) must equal folded shape {i}"
        );
    }
}

#[test]
fn tombstone_then_reopen_stays_deleted() {
    // Regression (finding 1 — tombstone resurrection / silent data loss).
    // A logical delete must have a durable on-disk effect: the doc's `.md` is
    // absent from its vault path. `load_accepted` reads content straight off the
    // `.md` and the tombstone flag is in-memory-only, so a doc whose `.md`
    // SURVIVED on disk would resurrect as a LIVE note on the next
    // `LayeredDoc::open`. Here the delete tombstones AND removes the file (the
    // contract the delete paths now uphold), so reopening a fresh LayeredDoc must
    // see the doc as gone — never materialize it as live.
    let td = TempDir::new().unwrap();
    let note = td.path().join("a.md");

    {
        let log = LayeredDoc::open(td.path()).unwrap();
        let id = log
            .create_document("a.md", "note", "live body\n", &Author::User)
            .unwrap();
        assert!(note.exists(), "create writes the .md");

        // Delete = tombstone (in-memory) + the file's durable removal from its
        // vault path. (The trees/vault delete paths do the fs removal; here we
        // perform the equivalent removal directly.)
        log.tombstone_document(&id, &Author::User).unwrap();
        std::fs::remove_file(&note).unwrap();
    }

    // Reopen a fresh LayeredDoc — no in-memory tombstone carries over. With the
    // `.md` absent, the doc must read as UNKNOWN (deleted), NOT resurrect live.
    let reopened = LayeredDoc::open(td.path()).unwrap();
    assert_eq!(
        reopened.doc_id_for_path("a.md").unwrap(),
        None,
        "deleted doc must not exist after reopen"
    );
    let err = reopened
        .materialize_accepted("a.md")
        .expect_err("a deleted doc must not materialize as a live note");
    assert!(
        matches!(err, Error::UnknownDoc(_)),
        "expected UnknownDoc (stays deleted), got {err:?}"
    );
}

#[test]
fn forget_document_prunes_snapshot_history() {
    // Regression (finding 2). `forget_document` must remove the path's snapshot
    // history dir, or a later file created at the SAME vault path inherits the
    // OLD file's snapshots — letting the version dropdown / restore roll the new
    // file back to unrelated content.
    let td = TempDir::new().unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();

    // Old file at notes/x.md, saved a few times so it accrues snapshots.
    let id = log
        .create_document("notes/x.md", "note", "old v1\n", &Author::User)
        .unwrap();
    log.apply_user_text(&id, "old v2\n").unwrap();
    log.apply_user_text(&id, "old v3\n").unwrap();
    assert!(
        !crate::snapshot::list_snapshots(td.path(), "notes/x.md")
            .unwrap()
            .is_empty(),
        "the old file should have accrued snapshots"
    );

    // Forget it (e.g. it became ignored).
    log.forget_document("notes/x.md").unwrap();

    // The snapshot history for that path must be GONE — a fresh file at the same
    // path starts with no inherited history.
    assert!(
        crate::snapshot::list_snapshots(td.path(), "notes/x.md")
            .unwrap()
            .is_empty(),
        "forget_document must prune the path's snapshot history"
    );
}

#[test]
fn register_verify_mismatch_leaves_no_cached_state() {
    // Regression (finding 3). `seed_document` (VerifyExisting) must verify the
    // on-disk bytes BEFORE caching the DocState. On a mismatch it returns
    // SeedMismatch; if it had cached the (wrong) text first, `is_loaded()` would
    // be true afterwards and a later re-seed would be short-circuited — leaving
    // `accepted` permanently diverged from the canonical `.md`. After a failed
    // verify there must be NO cached state, so the doc re-seeds cleanly.
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.md"), "on-disk truth\n").unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();

    let err = log
        .seed_document("a.md", "note", "wrong seed text\n", &Author::User)
        .expect_err("verify must reject divergent seed bytes");
    assert!(matches!(err, Error::SeedMismatch { .. }), "got {err:?}");

    // The failed verify left NO cached DocState — re-seeding is not
    // short-circuited.
    assert!(
        !log.is_loaded("a.md"),
        "a failed verify must not leave a cached (divergent) DocState"
    );

    // Re-seeding with the CORRECT bytes now succeeds and caches the truth.
    log.seed_document("a.md", "note", "on-disk truth\n", &Author::User)
        .expect("re-seed with matching bytes succeeds");
    assert_eq!(
        log.materialize_accepted("a.md").unwrap().text,
        "on-disk truth\n",
        "accepted must equal the canonical .md, not the earlier wrong seed"
    );
}

#[test]
fn pending_rename_collision_detects_in_memory_only_doc() {
    // Regression (finding 6). The rename-collision pre-check in `accept_pending`
    // must also consult the in-memory `docs` cache, not just disk. A doc created
    // this session but not yet flushed to disk lives only in the cache; a rename
    // onto its path would otherwise pass the disk-only check and clobber it.
    let td = TempDir::new().unwrap();
    let log = LayeredDoc::open(td.path()).unwrap();

    // `mover.md` exists on disk and will be renamed.
    let mover = log
        .create_document("mover.md", "note", "mover body\n", &Author::User)
        .unwrap();

    // `target.md` is loaded into the cache but its `.md` is removed from disk,
    // so it exists ONLY in memory (cache-only doc).
    let _target = log
        .create_document("target.md", "note", "target body\n", &Author::User)
        .unwrap();
    std::fs::remove_file(td.path().join("target.md")).unwrap();
    assert!(
        !td.path().join("target.md").exists(),
        "target.md is cache-only now"
    );
    assert!(log.is_loaded("target.md"), "target is loaded in cache");

    // Stage + try to accept a rename of mover.md -> target.md. The disk-only
    // check would miss the cache-only target; the cache check must catch it.
    let ctx = super::user_ctx();
    let staged = log
        .stage_pending_renames(&[("mover.md".to_string(), "target.md".to_string())], &ctx)
        .unwrap();
    let op_id = &staged.op_ids[0];
    let err = log
        .accept_pending(&mover, op_id)
        .expect_err("rename onto a cache-only doc must be refused as a collision");
    assert!(
        matches!(err, Error::Anchor(_)),
        "expected an Anchor collision error, got {err:?}"
    );
}
