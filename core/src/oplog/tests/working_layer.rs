//! Working-layer + scenario tests for the op-log: the editable buffer
//! (`materialize(accepted + working)`), `commit_working`, and the
//! user-edits-while-the-agent-edits scenarios that exercise accept / reject /
//! commit interleaving (per `op-log.md`'s "Layered document model"). Split out
//! of the parent `tests` module to keep each test file within the file-length
//! budget; `user_ctx` is shared from the parent, the rest of the fixtures
//! (`LINES_DOC`, `line_start`, `assert_accepted_and_disk`) are local since only
//! these scenarios use them.

use super::super::shapes::Author;
use super::super::*;
use super::user_ctx;
use tempfile::tempdir;

#[test]
fn every_accepted_version_is_loadable_after_edit_and_save() {
    // status: op-log-history-materialization
    // Mirrors the editor flow (working edit → commit_working) and the version
    // dropdown reading it back: every accepted op the dropdown lists
    // (`doc_history`) must be reconstructable via `materialize_at`, else the
    // snapshot tab shows "Couldn't load the buffer".
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "version one\n", &Author::User)
        .unwrap();
    // Editor: edit the working layer, then Save.
    log.apply_working_edit(&doc_id, "version one\n".len(), 0, "version two\n").unwrap();
    assert!(log.commit_working(&doc_id).unwrap());

    let history = log.doc_history(&doc_id, 20).unwrap();
    assert!(history.len() >= 2, "create + save at least");
    for row in &history {
        assert!(
            log.materialize_at(&doc_id, &row.op_id).unwrap().is_some(),
            "dropdown version {} (kind {:?}) must be loadable",
            row.op_id,
            row.op_kind,
        );
    }
}

#[test]
fn working_edit_shows_in_buffer_not_on_disk() {
    // status: op-log-working-layer
    // An uncommitted user edit is visible in the editable buffer
    // (`materialize(accepted + working)`) but never on disk until commit.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    assert!(!log.has_working_edits(&doc_id).unwrap());
    // Replace "world" (bytes 6..11) with "earth".
    log.apply_working_edit(&doc_id, 6, 5, "earth").unwrap();
    assert!(log.has_working_edits(&doc_id).unwrap());
    // The editable buffer reflects the working edit...
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, "hello earth\n");
    // ...but accepted and the on-disk .md do NOT.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello world\n");
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello world\n");
    // Commit folds it into accepted and onto disk.
    assert!(log.commit_working(&doc_id).unwrap());
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello earth\n");
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello earth\n");
}

#[test]
fn commit_working_folds_into_accepted() {
    // status: op-log-working-layer
    // status: op-log-atomic-write
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    log.apply_working_edit(&doc_id, 6, 5, "earth").unwrap();
    let working_text = log.materialize_working(&doc_id).unwrap().text;
    assert!(log.commit_working(&doc_id).unwrap());
    // accepted == the working text, and the .md on disk matches.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, working_text);
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, working_text);
    // The buffer is clean again.
    assert!(!log.has_working_edits(&doc_id).unwrap());
    // A second commit with nothing pending is a no-op.
    assert!(!log.commit_working(&doc_id).unwrap());
}

#[test]
fn discard_working_reverts_to_accepted() {
    // status: op-log-working-layer
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    log.apply_working_edit(&doc_id, 6, 5, "earth").unwrap();
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, "hello earth\n");
    log.discard_working(&doc_id).unwrap();
    assert!(!log.has_working_edits(&doc_id).unwrap());
    // The buffer reverts to accepted; nothing was ever persisted.
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, "hello world\n");
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello world\n");
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello world\n");
}

#[test]
fn review_view_overlays_pending_on_working() {
    // status: op-log-working-layer
    // status: op-log-layered-model
    // User edits region A; agent stages a pending op in disjoint region B.
    // materialize_review shows BOTH; materialize_working shows only A.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "alpha AAA omega BBB end\n", &Author::User)
        .unwrap();
    // Region A: user replaces "AAA" (bytes 6..9) with "aaa".
    log.apply_working_edit(&doc_id, 6, 3, "aaa").unwrap();
    // Region B: agent stages "BBB" -> "bbb" (disjoint).
    log.stage_pending(
        &doc_id,
        &[EditSpec { old_str: Some("BBB".into()), new_str: "bbb".into() }],
        &user_ctx(),
    )
    .unwrap();
    // The editable buffer shows only the user's A edit.
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "alpha aaa omega BBB end\n"
    );
    // The review overlay shows BOTH the user's A edit and the agent's B edit.
    assert_eq!(
        log.materialize_review(&doc_id, Some("sess-1")).unwrap().text,
        "alpha aaa omega bbb end\n"
    );
}

#[test]
fn accept_preserves_working_edits() {
    // status: op-log-working-layer
    // The headline data-loss fix: accepting an agent op in region B must not
    // discard the user's uncommitted edit in disjoint region A.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "alpha AAA omega BBB end\n", &Author::User)
        .unwrap();
    // Region A: uncommitted user edit.
    log.apply_working_edit(&doc_id, 6, 3, "aaa").unwrap();
    // Region B: stage + accept an agent op.
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("BBB".into()), new_str: "bbb".into() }],
            &user_ctx(),
        )
        .unwrap();
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    // The working buffer still has the user's A edit AND the accepted B edit.
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "alpha aaa omega bbb end\n"
    );
    assert!(log.has_working_edits(&doc_id).unwrap());
    // accepted has only B (disk = accepted; working is uncommitted).
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "alpha AAA omega bbb end\n"
    );
    // Committing folds A in too; the on-disk .md then has both.
    assert!(log.commit_working(&doc_id).unwrap());
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "alpha aaa omega bbb end\n"
    );
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "alpha aaa omega bbb end\n");
}

#[test]
fn external_edit_preserves_working_edits() {
    // status: op-log-working-layer
    // status: op-log-external-edit-sync
    // An external (on-disk) edit in region B must show through to the buffer
    // and survive the next commit — not be diffed away — while the user's
    // uncommitted region-A edit is preserved.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "alpha AAA omega BBB end\n", &Author::User)
        .unwrap();
    // Region A: uncommitted user edit (only in `working`, not on disk).
    log.apply_working_edit(&doc_id, 6, 3, "aaa").unwrap();
    // Region B: an external edit lands on disk (e.g. Syncthing) — reconciled
    // into `accepted` with the full new disk text.
    assert!(log
        .apply_external_edit(&doc_id, "alpha AAA omega bbb end\n")
        .unwrap());
    // The working buffer shows BOTH the user's A edit and the external B edit.
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "alpha aaa omega bbb end\n"
    );
    // accepted (= disk) carries the external B edit but not the uncommitted A.
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "alpha AAA omega bbb end\n"
    );
    // Committing folds A in without clobbering the external B.
    assert!(log.commit_working(&doc_id).unwrap());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "alpha aaa omega bbb end\n"
    );
}

#[test]
fn commit_after_accept_lands_both() {
    // status: op-log-working-layer
    // status: op-log-atomic-write
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "alpha AAA omega BBB end\n", &Author::User)
        .unwrap();
    log.apply_working_edit(&doc_id, 6, 3, "aaa").unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("BBB".into()), new_str: "bbb".into() }],
            &user_ctx(),
        )
        .unwrap();
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert!(log.commit_working(&doc_id).unwrap());
    assert!(!log.has_working_edits(&doc_id).unwrap());
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "alpha aaa omega bbb end\n"
    );
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "alpha aaa omega bbb end\n");
}

// ── user-edit x agent-edit merge scenarios ──────────────────────────────
//
// The substrate proof behind the editor-binding fix: a user `working` edit and
// an agent `pending` edit in different regions of a multi-line note coexist in
// `review`, fold into `working` on accept, then onto disk on commit. Per
// `op-log-layered-model` / `op-log-merge-auto`.

/// A 5-line note so a test can target one line by its byte offset.
const LINES_DOC: &str = "line one\nline two\nline three\nline four\nline five\n";

/// Byte offset of the start of 0-based line `n` in [`LINES_DOC`].
fn line_start(n: usize) -> usize {
    LINES_DOC.split_inclusive('\n').take(n).map(str::len).sum()
}

/// Assert `materialize_accepted` and the on-disk `a.md` both equal `expected`
/// (the canonical-disk invariant) for the scenario tests below.
fn assert_accepted_and_disk(log: &OpLog, dir: &std::path::Path, doc_id: &str, expected: &str) {
    assert_eq!(log.materialize_accepted(doc_id).unwrap().text, expected);
    assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), expected);
}

#[test]
fn user_edit_below_agent_line_both_survive_accept_and_commit() {
    // status: op-log-layered-model
    // status: op-log-merge-auto
    // Agent edits line two; user edits line four (BELOW it) — repro of the
    // original bug. Both must coexist in review, fold into working on accept,
    // land on disk on commit; accepted carries only the agent edit pre-commit.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    // Agent stages line two: "line two" → "LINE TWO".
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("line two".into()), new_str: "LINE TWO".into() }],
            &user_ctx(),
        )
        .unwrap();
    // User edits line four (below): replace "four" with "FOUR".
    let four = line_start(3) + "line ".len();
    log.apply_working_edit(&doc_id, four, "four".len(), "FOUR").unwrap();
    // Review shows BOTH; working shows only the user's line-four edit.
    assert_eq!(
        log.materialize_review(&doc_id, Some("sess-1")).unwrap().text,
        "line one\nLINE TWO\nline three\nline FOUR\nline five\n"
    );
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "line one\nline two\nline three\nline FOUR\nline five\n"
    );
    // accepted (= disk) has neither yet.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, LINES_DOC);
    // Accept the agent op: accepted now carries ONLY the agent edit.
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "line one\nLINE TWO\nline three\nline four\nline five\n"
    );
    // working still carries the user edit on top of the accepted agent edit.
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "line one\nLINE TWO\nline three\nline FOUR\nline five\n"
    );
    // Commit folds the user edit into accepted + disk; both edits present.
    assert!(log.commit_working(&doc_id).unwrap());
    let both = "line one\nLINE TWO\nline three\nline FOUR\nline five\n";
    assert_accepted_and_disk(&log, dir.path(), &doc_id, both);
}

#[test]
fn user_edit_above_agent_line_both_survive_accept_and_commit() {
    // status: op-log-layered-model
    // status: op-log-merge-auto
    // Mirror of the below case: user edits line two; agent edits line four
    // (BELOW it), so the user's edit is ABOVE the agent's.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("line four".into()), new_str: "LINE FOUR".into() }],
            &user_ctx(),
        )
        .unwrap();
    let two = line_start(1) + "line ".len();
    log.apply_working_edit(&doc_id, two, "two".len(), "TWO").unwrap();
    assert_eq!(
        log.materialize_review(&doc_id, Some("sess-1")).unwrap().text,
        "line one\nline TWO\nline three\nLINE FOUR\nline five\n"
    );
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "line one\nline TWO\nline three\nline four\nline five\n"
    );
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "line one\nline two\nline three\nLINE FOUR\nline five\n"
    );
    assert!(log.commit_working(&doc_id).unwrap());
    let both = "line one\nline TWO\nline three\nLINE FOUR\nline five\n";
    assert_accepted_and_disk(&log, dir.path(), &doc_id, both);
}

#[test]
fn user_edit_disjoint_line_with_agent_multi_line_edits() {
    // status: op-log-layered-model
    // status: op-log-merge-auto
    // The agent edits MULTIPLE lines (two and four) in one batch; the user
    // edits a disjoint line (one). All three coexist in review, fold into
    // working on accept-all, and land on disk on commit.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[
                EditSpec { old_str: Some("line two".into()), new_str: "LINE TWO".into() },
                EditSpec { old_str: Some("line four".into()), new_str: "LINE FOUR".into() },
            ],
            &user_ctx(),
        )
        .unwrap();
    assert_eq!(out.op_ids.len(), 2);
    // User edits line one (disjoint, above both agent edits).
    let one = line_start(0) + "line ".len();
    log.apply_working_edit(&doc_id, one, "one".len(), "ONE").unwrap();
    assert_eq!(
        log.materialize_review(&doc_id, Some("sess-1")).unwrap().text,
        "line ONE\nLINE TWO\nline three\nLINE FOUR\nline five\n"
    );
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "line ONE\nline two\nline three\nline four\nline five\n"
    );
    // Accept both agent ops.
    for op in &out.op_ids {
        log.accept_pending(&doc_id, op).unwrap();
    }
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "line one\nLINE TWO\nline three\nLINE FOUR\nline five\n"
    );
    assert!(log.commit_working(&doc_id).unwrap());
    let all = "line ONE\nLINE TWO\nline three\nLINE FOUR\nline five\n";
    assert_accepted_and_disk(&log, dir.path(), &doc_id, all);
}

#[test]
fn reject_agent_op_preserves_user_edit() {
    // status: op-log-status-states
    // status: op-log-layered-model
    // Rejecting the agent op drops its change from review while the user's
    // disjoint working edit is untouched.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("line two".into()), new_str: "LINE TWO".into() }],
            &user_ctx(),
        )
        .unwrap();
    let four = line_start(3) + "line ".len();
    log.apply_working_edit(&doc_id, four, "four".len(), "FOUR").unwrap();
    // Reject the agent op.
    log.reject_pending(&doc_id, &out.op_ids[0]).unwrap();
    // review == working now (no pending left); the user edit survives.
    let user_only = "line one\nline two\nline three\nline FOUR\nline five\n";
    assert_eq!(log.materialize_review(&doc_id, Some("sess-1")).unwrap().text, user_only);
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, user_only);
    assert!(log.pending_ops(&doc_id).unwrap().is_empty());
    // Commit lands only the user edit; the agent's change never reaches disk.
    assert!(log.commit_working(&doc_id).unwrap());
    assert_accepted_and_disk(&log, dir.path(), &doc_id, user_only);
}

#[test]
fn user_deletes_line_while_agent_edits_another() {
    // status: op-log-layered-model
    // status: op-log-merge-auto
    // The user DELETES a whole line (line three, including its newline) while
    // the agent edits a different line (line one). Accept + commit reflect
    // both: the deletion and the agent's edit.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("line one".into()), new_str: "LINE ONE".into() }],
            &user_ctx(),
        )
        .unwrap();
    // User deletes line three entirely: bytes [line_start(2), line_start(3)).
    let del_start = line_start(2);
    let del_len = line_start(3) - del_start;
    log.apply_working_edit(&doc_id, del_start, del_len, "").unwrap();
    assert_eq!(
        log.materialize_working(&doc_id).unwrap().text,
        "line one\nline two\nline four\nline five\n"
    );
    assert_eq!(
        log.materialize_review(&doc_id, Some("sess-1")).unwrap().text,
        "LINE ONE\nline two\nline four\nline five\n"
    );
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    // accepted has the agent edit but still all five lines (deletion uncommitted).
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "LINE ONE\nline two\nline three\nline four\nline five\n"
    );
    assert!(log.commit_working(&doc_id).unwrap());
    let both = "LINE ONE\nline two\nline four\nline five\n";
    assert_accepted_and_disk(&log, dir.path(), &doc_id, both);
}

#[test]
fn two_agent_ops_accept_one_reject_other_with_user_edit() {
    // status: op-log-status-states
    // status: op-log-per-hunk-accept-reject
    // Two agent ops in different regions (lines two and four) plus a user edit
    // (line one). Accept the line-two op, reject the line-four op: the accepted
    // one folds into working, the rejected one vanishes, the user edit stays.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", LINES_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[
                EditSpec { old_str: Some("line two".into()), new_str: "LINE TWO".into() },
                EditSpec { old_str: Some("line four".into()), new_str: "LINE FOUR".into() },
            ],
            &user_ctx(),
        )
        .unwrap();
    let one = line_start(0) + "line ".len();
    log.apply_working_edit(&doc_id, one, "one".len(), "ONE").unwrap();
    // Accept op[0] (line two), reject op[1] (line four).
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    log.reject_pending(&doc_id, &out.op_ids[1]).unwrap();
    assert!(log.pending_ops(&doc_id).unwrap().is_empty());
    // accepted carries only the accepted line-two op.
    assert_eq!(
        log.materialize_accepted(&doc_id).unwrap().text,
        "line one\nLINE TWO\nline three\nline four\nline five\n"
    );
    // working = accepted agent edit + the user's line-one edit; the rejected
    // line-four op is gone.
    let working = "line ONE\nLINE TWO\nline three\nline four\nline five\n";
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, working);
    assert_eq!(log.materialize_review(&doc_id, Some("sess-1")).unwrap().text, working);
    assert!(log.commit_working(&doc_id).unwrap());
    assert_accepted_and_disk(&log, dir.path(), &doc_id, working);
}

#[test]
fn bug_sync_commit_working_races_remote_apply() {
    // status: bug-sync-commit-working-races-remote-apply
    //
    // commit_working reads `materialize(working).text` under one locked() block
    // and then calls commit_text_edit (which acquires its own lock and diffs
    // the captured text against the *current* accepted). If a remote/external
    // edit lands between the two lock acquisitions, the diff vs. the just-
    // updated accepted reverts the peer's bytes — silent data loss.
    //
    // We use the #[cfg(test)] `commit_working_test_hook` to deterministically
    // schedule an `apply_external_edit` into the lock-gap, then assert the
    // committed accepted contains BOTH edits (a real three-way merge). The
    // test FAILS today because the peer's line-3 edit is reverted.
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tempdir().unwrap();
    let log = Arc::new(OpLog::open(dir.path()).unwrap());
    let original = "line one\nline two\nline three\n";
    let doc_id = log
        .create_document("a.md", "note", original, &Author::User)
        .unwrap();

    // User edits line 2: insert " MODIFIED-BY-USER" after "line two".
    let insert_at = "line one\nline two".len();
    log.apply_working_edit(&doc_id, insert_at, 0, " MODIFIED-BY-USER")
        .unwrap();
    let user_working = "line one\nline two MODIFIED-BY-USER\nline three\n";
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, user_working);

    // Peer edit: same `original` accepted, but extends line 3 (disjoint from
    // the user's line-2 change).
    let peer_disk = "line one\nline two\nline three EXTENDED-BY-PEER\n";

    // Hook fires *after* commit_working reads the working text and releases
    // the first lock, but *before* commit_text_edit takes its own lock — the
    // exact window the bug needs. Inside the hook we launch a thread that
    // applies the external edit (taking the lock while commit_working is
    // paused), and wait for it to complete before returning. Once we return,
    // commit_text_edit acquires the lock and diffs the (now stale) user
    // working text against the (now peer-extended) accepted.
    let start = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));
    let log_for_hook = Arc::clone(&log);
    let start_hook = Arc::clone(&start);
    let done_hook = Arc::clone(&done);
    let peer_disk_owned = peer_disk.to_string();
    let doc_id_for_hook = doc_id.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let log_for_thread = Arc::clone(&log_for_hook);
        let start_thread = Arc::clone(&start_hook);
        let done_thread = Arc::clone(&done_hook);
        let peer_owned = peer_disk_owned.clone();
        let doc_id_thread = doc_id_for_hook.clone();
        let handle = thread::spawn(move || {
            // Sync with the hook: only fire the external edit once the hook
            // has released the commit_working lock.
            start_thread.wait();
            log_for_thread
                .apply_external_edit(&doc_id_thread, &peer_owned)
                .unwrap();
            done_thread.wait();
        });
        // Signal the worker to apply the external edit, then wait until it's
        // done so commit_text_edit observes the peer-advanced accepted.
        start_hook.wait();
        done_hook.wait();
        handle.join().unwrap();
    });
    *log.commit_working_test_hook.lock().unwrap() = Some(hook);

    log.commit_working(&doc_id).unwrap();

    // Both edits should have landed (real three-way merge). Today, the peer's
    // line-3 extension is reverted by the user save.
    let expected = "line one\nline two MODIFIED-BY-USER\nline three EXTENDED-BY-PEER\n";
    let final_text = log.materialize_accepted(&doc_id).unwrap().text;
    assert_eq!(
        final_text, expected,
        "commit_working raced apply_external_edit — peer's edit was reverted",
    );
}

#[test]
fn commit_working_preserves_peer_edit_during_race() {
    // Regression: commit_working must pass (base, ours=working, theirs=peer)
    // to three_way_merge. With overlapping spans the peer wins per the merge
    // policy, so the peer's bytes survive in accepted instead of being
    // silently overwritten by the user's stale working text.
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tempdir().unwrap();
    let log = Arc::new(OpLog::open(dir.path()).unwrap());
    let original = "line one\nline two\nline three\n";
    let doc_id = log
        .create_document("a.md", "note", original, &Author::User)
        .unwrap();

    // User REPLACES "line two" with "LINE-TWO-USER" (a replacement span).
    let start = "line one\n".len();
    let removed = "line two".len();
    log.apply_working_edit(&doc_id, start, removed, "LINE-TWO-USER")
        .unwrap();
    let user_working = "line one\nLINE-TWO-USER\nline three\n";
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, user_working);

    // Peer ALSO replaces "line two" with "LINE-TWO-PEER" — true span overlap.
    let peer_disk = "line one\nLINE-TWO-PEER\nline three\n";

    let start = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));
    let log_for_hook = Arc::clone(&log);
    let start_hook = Arc::clone(&start);
    let done_hook = Arc::clone(&done);
    let peer_disk_owned = peer_disk.to_string();
    let doc_id_for_hook = doc_id.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let log_for_thread = Arc::clone(&log_for_hook);
        let start_thread = Arc::clone(&start_hook);
        let done_thread = Arc::clone(&done_hook);
        let peer_owned = peer_disk_owned.clone();
        let doc_id_thread = doc_id_for_hook.clone();
        let handle = thread::spawn(move || {
            start_thread.wait();
            log_for_thread
                .apply_external_edit(&doc_id_thread, &peer_owned)
                .unwrap();
            done_thread.wait();
        });
        start_hook.wait();
        done_hook.wait();
        handle.join().unwrap();
    });
    *log.commit_working_test_hook.lock().unwrap() = Some(hook);

    log.commit_working(&doc_id).unwrap();

    let final_accepted = log.materialize_accepted(&doc_id).unwrap().text;
    assert_eq!(
        final_accepted, peer_disk,
        "peer's edit must be preserved when commit_working races an overlapping peer edit",
    );
    assert!(
        !final_accepted.contains("LINE-TWO-USER"),
        "user's overlapping span should drop per peer-wins policy: accepted={final_accepted:?}",
    );
}

#[test]
fn bug_sync_per_hunk_accept_cross_op_deps() {
    // Bug: stage_pending falls back to producing op #2 against
    // `accepted + prior session pending` when op #2's anchor isn't in
    // `accepted`. The Yrs update is encoded with before_sv =
    // accepted.state_vector, so accepting op #2 alone (skipping op #1) either
    // silently lands a drifted edit or fails — per-hunk independence breaks.
    //
    // Scenario:
    //   accepted = "alpha\nbeta\n"
    //   op #1: insert INSERT line between alpha and beta (anchored in accepted)
    //   op #2: replace "INSERT" -> "REPLACED" (anchor exists ONLY in pending
    //          view → forces fallback path in stage_pending lines 436-439)
    //   Accept ONLY op #2 (skip op #1).
    //
    // Acceptable safe outcomes:
    //   - accept returns Err (per-hunk Accept disabled) AND accepted unchanged
    //   - accept returns Ok and accepted is unchanged ("alpha\nbeta\n")
    //
    // Bug manifestation: accept returns Ok with corrupted text (e.g.
    // "alphaREPLACED\nbeta\n"), or returns a generic Drift error that today
    // would surface to the user as a confusing "anchor drift" rather than
    // "depends on op #1".
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "alpha\nbeta\n", &Author::User)
        .unwrap();
    // Stage op #1 first so it's in state.pending under the session, then
    // stage op #2 separately under the same session — that's what forces
    // op #2 down the fallback path (its "INSERT" anchor is absent from
    // `accepted` but present in `accepted + prior session pending`).
    let out1 = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("alpha\nbeta\n".into()),
                new_str: "alpha\nINSERT\nbeta\n".into(),
            }],
            &user_ctx(),
        )
        .unwrap();
    assert_eq!(out1.op_ids.len(), 1);
    let out2 = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("INSERT".into()),
                new_str: "REPLACED".into(),
            }],
            &user_ctx(),
        )
        .unwrap();
    assert_eq!(out2.op_ids.len(), 1, "op #2 should stage via fallback path");
    let op2_id = out2.op_ids[0].clone();

    // Accept ONLY op #2; skip op #1.
    let result = log.accept_pending(&doc_id, &op2_id);
    let materialized = log.materialize_accepted(&doc_id).unwrap().text;
    eprintln!(
        "accept_pending(op2) result = {:?}, materialized accepted = {:?}",
        result, materialized
    );

    // Safe outcomes:
    //   - Err returned AND accepted unchanged, OR
    //   - Ok returned AND accepted unchanged.
    // Bug outcomes: Ok with mangled text, or Err with corrupted accepted, or
    // an Err whose only signal is generic "drift" (which the spec wants to
    // become a clean DependsOn). Today no DependsOn variant exists, so we
    // assert: if Ok was returned, accepted must equal the original.
    match result {
        Ok(()) => {
            assert_eq!(
                materialized, "alpha\nbeta\n",
                "BUG: accept_pending of dependent op #2 without op #1 silently \
                 corrupted accepted — got {materialized:?}"
            );
        }
        Err(e) => {
            // Accepted must still be unchanged after a failed accept.
            assert_eq!(
                materialized, "alpha\nbeta\n",
                "accept_pending errored but accepted was mutated anyway: \
                 err={e:?}, accepted={materialized:?}"
            );
            // And the error should be a per-hunk dependency error, not a
            // generic anchor/drift one — names the blocker so the caller
            // (UI or agent) can accept/reject it first.
            match &e {
                super::super::error::Error::DependsOn { op_id, predecessors } => {
                    assert_eq!(op_id, &op2_id);
                    assert_eq!(predecessors.len(), 1);
                    assert_eq!(predecessors[0], out1.op_ids[0]);
                }
                _ => panic!(
                    "BUG: accept_pending of dependent op #2 returned a generic \
                     error instead of a clean depends-on signal: {e:?}"
                ),
            }
        }
    }
}

#[test]
fn bug_sync_working_mirror_cross_lineage_apply() {
    // status: bug-sync-working-mirror-cross-lineage-apply
    //
    // The working Doc is cloned from accepted via `clone_doc`, which mints a
    // fresh client_id. When a peer-authored update advances accepted and the
    // working-mirror path (`apply_remote_update` in sync.rs:150-153) applies
    // the encoded delta onto working, working sees foreign client_ids and the
    // merge can drop, dup, or interleave bytes — exactly the cross-lineage
    // failure mode the spec warns about.
    //
    // Real cross-lineage scenario: two devices share a lineage via
    // adopt_lineage. Device B holds an uncommitted working edit on line 2.
    // Device A authors a disjoint edit on line 3 and ships the delta to B.
    // After B applies the remote update, materialize_working should show
    // BOTH edits (three-way merge). Today the cross-lineage apply doesn't
    // preserve both correctly.
    let seed = "alpha\nbeta\ngamma\n";
    let dir_a = tempdir().unwrap();
    let log_a = OpLog::open(dir_a.path()).unwrap();
    let doc_a = log_a
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();
    let dir_b = tempdir().unwrap();
    let log_b = OpLog::open(dir_b.path()).unwrap();
    let doc_b = log_b
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();
    // B adopts A's canonical lineage so future deltas merge.
    let canonical = log_a.export_state(&doc_a).unwrap();
    log_b.adopt_lineage(&doc_b, &canonical).unwrap();
    assert_eq!(log_b.materialize_accepted(&doc_b).unwrap().text, seed);

    // B: user edits line 2 in working — uncommitted, working-only.
    let line2_end = "alpha\nbeta".len();
    log_b
        .apply_working_edit(&doc_b, line2_end, 0, " MODIFIED-BY-USER")
        .unwrap();
    assert_eq!(
        log_b.materialize_working(&doc_b).unwrap().text,
        "alpha\nbeta MODIFIED-BY-USER\ngamma\n"
    );

    // A: disjoint edit on line 3, then ship the delta to B.
    assert!(log_a
        .apply_user_text(&doc_a, "alpha\nbeta\ngamma EXTENDED-BY-PEER\n")
        .unwrap());
    let b_sv = log_b.state_vector_bytes(&doc_b).unwrap();
    let delta = log_a.export_since(&doc_a, &b_sv).unwrap();
    assert!(log_b
        .apply_remote_update(&doc_b, &delta, "deviceA")
        .unwrap());

    // Accepted on B carries A's peer edit; working should carry BOTH.
    assert_eq!(
        log_b.materialize_accepted(&doc_b).unwrap().text,
        "alpha\nbeta\ngamma EXTENDED-BY-PEER\n"
    );
    let materialized = log_b.materialize_working(&doc_b).unwrap().text;
    dbg!(&materialized);

    let expected = "alpha\nbeta MODIFIED-BY-USER\ngamma EXTENDED-BY-PEER\n";
    assert_eq!(
        materialized, expected,
        "cross-lineage working-mirror apply did not produce a clean three-way merge"
    );
}
