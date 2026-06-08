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

    let log = OpLog::open(td.path()).unwrap();
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
    let log = OpLog::open(td.path()).unwrap();

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
    // `overlay::span_delta_round_trips` proptest with a through-the-OpLog case.
    let td = TempDir::new().unwrap();
    let log = OpLog::open(td.path()).unwrap();
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
