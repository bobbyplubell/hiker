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
