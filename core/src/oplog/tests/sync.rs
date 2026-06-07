//! Tests for the multi-device sync substrate verbs (`op-log-multi-device-sync`,
//! `sync-content-hash-column`, `sync-lineage-adoption`): the `content_hash`
//! column + `doc_history_hashes`, the plain-bytes export/import round-trip
//! (lineage adoption then incremental delta), the inbound `apply_remote_update`
//! no-op contract, and the local-divergence-preserving adoption. Split out of
//! the parent `tests` module to keep each file within the budget.

use super::super::shapes::Author;
use super::super::*;
use tempfile::tempdir;

/// blake3 hex of a string — mirrors the substrate's `content_hash` so a test
/// can assert the recorded hash matches a materialized text.
fn hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Apply `src`'s current document TEXT into `dst` as the text substrate's
/// receive path: ship the source's `export_since` (whole-file text), its
/// tombstone flag, and its content-hash history window (the merge-base anchor) —
/// exactly what the transport hands `apply_remote_update` now that the wire
/// carries text, not Yrs deltas. Returns whether `dst` advanced.
fn sync_text(dst: &OpLog, dst_doc: &str, src: &OpLog, src_doc: &str, device: &str) -> bool {
    let dst_sv = dst.state_vector_bytes(dst_doc).unwrap();
    let text = src.export_since(src_doc, &dst_sv).unwrap();
    let tombstone = src.materialize_accepted(src_doc).unwrap().tombstone;
    let peer_hashes = src.doc_history_hashes(src_doc).unwrap();
    dst.apply_remote_update(dst_doc, &text, tombstone, device, &peer_hashes)
        .unwrap()
}

#[test]
fn content_hash_populated_and_history_set_accumulates() {
    // status: sync-content-hash-column
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "v1\n", &Author::User)
        .unwrap();
    // Two more accepted states via whole-file saves.
    assert!(log.apply_user_text(&doc_id, "v2\n").unwrap());
    assert!(log.apply_user_text(&doc_id, "v3\n").unwrap());

    // Every accepted row carries the materialized hash as of that op.
    let hist = log.doc_history(&doc_id, 10).unwrap();
    assert!(
        hist.iter().all(|m| m.content_hash.is_some()),
        "every accepted row must carry a content_hash"
    );

    // The history set is exactly the three materialized states.
    let hashes = log.doc_history_hashes(&doc_id).unwrap();
    let expected: std::collections::HashSet<String> =
        ["v1\n", "v2\n", "v3\n"].iter().map(|t| hash(t)).collect();
    assert_eq!(hashes, expected);
}

#[test]
fn recent_history_hashes_are_newest_first_and_stable() {
    // status: bug-sync-history-hashset-truncation-nondet
    //
    // The sync manifest ships only a bounded recent window of a doc's content
    // hashes (`recent_doc_history_hashes(doc, N)`), which the transport later
    // truncates/compares. For two devices to classify lineage the same way
    // every round, that window must be the *most-recent* N by recency and be
    // identical across repeated calls — a `HashSet`'s unspecified iteration
    // order broke both. This asserts the ordered-Vec contract directly at the
    // op-log boundary (the transport-level regression test drives the manifest
    // path; this one pins the substrate verb it rests on).
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "v0\n", &Author::User)
        .unwrap();

    // Apply well over a typical window (32) of distinct accepted states, in a
    // known order. Each whole-file save advances accepted and stamps a fresh
    // content_hash row.
    const TOTAL: usize = 40;
    let mut order: Vec<String> = vec![hash("v0\n")]; // the create's hash, oldest
    for i in 1..=TOTAL {
        let text = format!("v{i}\n");
        assert!(log.apply_user_text(&doc_id, &text).unwrap(), "edit {i} should advance");
        order.push(hash(&text));
    }
    // Newest-first expectation: reverse insertion order.
    let mut newest_first = order.clone();
    newest_first.reverse();

    // The full (unbounded) recent list is newest-first and exact.
    let all = log
        .recent_doc_history_hashes(&doc_id, order.len() + 10)
        .unwrap();
    assert_eq!(all, newest_first, "recent hashes must be newest-first by recency");
    assert_eq!(all.first(), Some(&hash(&format!("v{TOTAL}\n"))), "newest leads");
    assert_eq!(all.last(), Some(&hash("v0\n")), "oldest (the create) trails");

    // A bounded window of 32 is exactly the most-recent 32, and stable across
    // repeated calls (the property the HashSet path could not guarantee).
    const WINDOW: usize = 32;
    let win_a = log.recent_doc_history_hashes(&doc_id, WINDOW).unwrap();
    let win_b = log.recent_doc_history_hashes(&doc_id, WINDOW).unwrap();
    assert_eq!(win_a.len(), WINDOW);
    assert_eq!(win_a, win_b, "the recent window must be deterministic across calls");
    assert_eq!(win_a, newest_first[..WINDOW], "window is the most-recent N by recency");
}

#[test]
fn rejected_op_carries_no_content_hash() {
    // A rejected op never lands in `accepted`, so it has no materialized
    // content and contributes nothing to the history set.
    // status: sync-content-hash-column
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let ctx = ProducerCtx {
        author: Author::Agent("claude-code".to_string()),
        surface: "mcp-tool-call".to_string(),
        session_id: Some("sess-1".to_string()),
    };
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("world".to_string()), new_str: "earth".to_string() }],
            &ctx,
        )
        .unwrap();
    log.reject_pending(&doc_id, &out.op_ids[0]).unwrap();
    // Only the `create` op contributed a hash.
    let hashes = log.doc_history_hashes(&doc_id).unwrap();
    assert_eq!(hashes, std::iter::once(hash("hello world\n")).collect());
}

/// Seed a second vault's doc to the same logical content as `a`, then bind it
/// to `a`'s lineage via `adopt_lineage(export_state(a))`. Returns the two logs
/// and the (independent) doc ids — exactly the enrollment flow.
fn shared_lineage() -> (tempfile::TempDir, OpLog, String, tempfile::TempDir, OpLog, String) {
    let seed = "# Shared\n\nline one\nline two\n";
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
    // B adopts A's canonical lineage (identical content → pure adoption).
    let canonical = log_a.export_state(&doc_a).unwrap();
    log_b.adopt_lineage(&doc_b, &canonical).unwrap();
    assert_eq!(
        log_b.materialize_accepted(&doc_b).unwrap().text,
        log_a.materialize_accepted(&doc_a).unwrap().text,
        "after adoption B must equal A"
    );
    (dir_a, log_a, doc_a, dir_b, log_b, doc_b)
}

#[test]
fn export_import_round_trip_streams_an_edit() {
    // status: op-log-multi-device-sync
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // A edits; B pulls A's whole-file text and merges it.
    assert!(log_a.apply_user_text(&doc_a, "# Shared\n\nline one EDITED\nline two\n").unwrap());
    let advanced = sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA");
    assert!(advanced, "the peer text carried a new version");

    // B now materializes to A's text…
    assert_eq!(
        log_b.materialize_accepted(&doc_b).unwrap().text,
        log_a.materialize_accepted(&doc_a).unwrap().text,
    );
    // …and the `.md` is the projection of accepted.
    let on_disk = std::fs::read_to_string(_db.path().join("shared.md")).unwrap();
    assert_eq!(on_disk, log_a.materialize_accepted(&doc_a).unwrap().text);

    // The receive wrote a `sync:deviceA`-authored row.
    let hist = log_b.doc_history(&doc_b, 20).unwrap();
    assert!(
        hist.iter().any(|m| matches!(&m.author, Author::Sync(d) if d == "deviceA")),
        "expected a sync:deviceA row in B's history"
    );
}

#[test]
fn disk_edit_and_peer_edit_both_survive_merge() {
    // Acceptance case 13 (op-log.md §External-edit sync): a doc that differs on
    // a PEER *and* on local DISK at the same time. The ordering invariant folds
    // the offline disk edit into `accepted` as an `author=external` op (what
    // `reconcile_disk` produces) BEFORE the peer's update is applied, so the
    // inbound merge is a standard text three-way: base + local-external +
    // remote. Disjoint edits both survive, both devices converge, and the local
    // disk edit propagates back to the peer.
    // status: op-log-startup-disk-reconcile
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
    // base: "# Shared\n\nline one\nline two\n"

    // B's offline disk edit, folded the way the startup reconcile folds it
    // (`apply_external_edit`) — a disjoint region ("line one") from A's edit.
    assert!(log_b
        .apply_external_edit(&doc_b, "# Shared\n\nline one [LOCAL DISK]\nline two\n")
        .unwrap());

    // The peer (A) diverged from the *same base* on a disjoint region ("line two").
    assert!(log_a
        .apply_user_text(&doc_a, "# Shared\n\nline one\nline two [PEER]\n")
        .unwrap());

    // Reconcile-before-sync: B's disk edit is already in `accepted` (above); the
    // first sync round now pulls A's text and merges it in.
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));

    // BOTH edits survive on B — disjoint regions merge to the union.
    let merged_b = log_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(
        merged_b, "# Shared\n\nline one [LOCAL DISK]\nline two [PEER]\n",
        "the local disk edit and the peer edit both survive the merge"
    );

    // The disk edit is an `author=external` op (recorded before the merge); the
    // peer edit is a `sync:deviceA` op.
    let hist_b = log_b.doc_history(&doc_b, 20).unwrap();
    assert!(
        hist_b.iter().any(|m| matches!(m.author, Author::External)),
        "the disk edit is an author=external op"
    );
    assert!(
        hist_b
            .iter()
            .any(|m| matches!(&m.author, Author::Sync(d) if d == "deviceA")),
        "the peer edit is a sync:deviceA op"
    );

    // Convergence: push B's combined state back to A — A ends byte-identical to
    // B, so the local disk edit propagated to the peer rather than only merging
    // locally.
    assert!(sync_text(&log_a, &doc_a, &log_b, &doc_b, "deviceB"));
    assert_eq!(
        log_a.materialize_accepted(&doc_a).unwrap().text,
        merged_b,
        "both devices converge to the same merged text"
    );
}

#[test]
fn three_peers_editing_disjoint_lines_converge() {
    // FAITHFUL text-model successor to the Yrs `three_peers_editing_the_same_line`
    // CRDT-interleave test. Under the text substrate the wire carries whole-file
    // TEXT reconciled by a 3-way merge over the content-hash base: three peers
    // editing DISJOINT regions all survive and converge to one deterministic text
    // regardless of delivery order (the auto-merge half of the spec). Three peers
    // editing the SAME region is a CONFLICT the transport BLOCKS — never a silent
    // interleave — and is covered by `same_region_edits_verdict_conflict`; we do
    // NOT re-test that here. This pins convergence + no-loss for the disjoint
    // case, the property the text substrate genuinely guarantees.
    // status: op-log-multi-device-sync
    // status: op-log-sync-substrate
    let seed = "line A\nline B\nline C\n";
    let dir_a = tempdir().unwrap();
    let log_a = OpLog::open(dir_a.path()).unwrap();
    let doc_a = log_a.create_document("s.md", "note", seed, &Author::User).unwrap();
    let canonical = log_a.export_state(&doc_a).unwrap();
    // B and C each adopt A's canonical text (the enrollment flow) — three
    // replicas of one doc on the shared content-hash base.
    let mk = |canon: &[u8]| {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path()).unwrap();
        let doc = log.create_document("s.md", "note", seed, &Author::User).unwrap();
        log.adopt_lineage(&doc, canon).unwrap();
        (dir, log, doc)
    };
    let (_db, log_b, doc_b) = mk(&canonical);
    let (_dc, log_c, doc_c) = mk(&canonical);

    // Each peer edits a DISJOINT line, concurrently (all against the same base).
    assert!(log_a.apply_user_text(&doc_a, "line A [A]\nline B\nline C\n").unwrap());
    assert!(log_b.apply_user_text(&doc_b, "line A\nline B [B]\nline C\n").unwrap());
    assert!(log_c.apply_user_text(&doc_c, "line A\nline B\nline C [C]\n").unwrap());

    // Deliver each peer's text to the others in DIFFERENT orders — proving
    // order-independent convergence. Each apply merges over the shared base.
    sync_text(&log_a, &doc_a, &log_b, &doc_b, "B");
    sync_text(&log_a, &doc_a, &log_c, &doc_c, "C");
    sync_text(&log_b, &doc_b, &log_c, &doc_c, "C");
    sync_text(&log_b, &doc_b, &log_a, &doc_a, "A");
    sync_text(&log_c, &doc_c, &log_a, &doc_a, "A");
    sync_text(&log_c, &doc_c, &log_b, &doc_b, "B");
    // One more pass each direction so a peer that merged a partial union also
    // picks up the third edit it hadn't seen when first applied.
    sync_text(&log_a, &doc_a, &log_b, &doc_b, "B");
    sync_text(&log_a, &doc_a, &log_c, &doc_c, "C");
    sync_text(&log_b, &doc_b, &log_a, &doc_a, "A");
    sync_text(&log_c, &doc_c, &log_a, &doc_a, "A");

    let ta = log_a.materialize_accepted(&doc_a).unwrap().text;
    let tb = log_b.materialize_accepted(&doc_b).unwrap().text;
    let tc = log_c.materialize_accepted(&doc_c).unwrap().text;

    // Convergence: all three replicas agree despite different delivery orders.
    assert_eq!(ta, tb, "A and B converge: {ta:?} vs {tb:?}");
    assert_eq!(tb, tc, "B and C converge: {tb:?} vs {tc:?}");
    // No contribution lost: every peer's disjoint edit survives the merge.
    assert!(
        ta.contains("[A]") && ta.contains("[B]") && ta.contains("[C]"),
        "all three concurrent disjoint edits survive: {ta:?}"
    );
}

#[test]
fn delete_vs_concurrent_edit_verdict_conflict_both_directions() {
    // status: sync-conflict-delete-vs-edit
    //
    // BEHAVIORAL INVERSION of the retired
    // `offline_delete_versus_concurrent_peer_edit_converges_delete_wins`: a
    // delete concurrent with an edit no longer silently lets the delete win — it
    // is a CONFLICT that must block. This pins the core verdict in BOTH
    // directions (peer deleted + we edited; we deleted + peer edited). The
    // transport block-then-resolve + convergence is covered by the
    // `delete_vs_edit_*` scenarios in `hiker-sync/tests/scenarios.rs`.
    use super::super::sync::DeleteVsEdit;

    // Direction 1: PEER tombstoned, WE edited. B is the local device; A is the
    // peer. B edits its text; A (the peer) is tombstoned. B's verdict on A's
    // incoming state must be Conflict.
    {
        let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
        // B (local, live) edits past the shared base.
        let b_text = "# Shared\n\nline one EDITED BY B\nline two\n";
        assert!(log_b.apply_user_text(&doc_b, b_text).unwrap());
        // A (peer) deletes — tombstone keeps A's last-known text (the base).
        log_a.tombstone_document(&doc_a, &Author::User).unwrap();
        let peer = log_a.materialize_accepted(&doc_a).unwrap();
        let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
        let verdict = log_b
            .delete_vs_edit_verdict(&doc_b, &peer.text, peer.tombstone, &peer_hashes)
            .unwrap();
        assert_eq!(
            verdict,
            DeleteVsEdit::Conflict,
            "peer-deleted + we-edited must block, not silently delete-win"
        );
    }

    // Direction 2: WE tombstoned, PEER edited. B (local) deletes; A (peer) edits.
    // B's verdict on A's incoming (live, edited) state must be Conflict.
    {
        let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
        // B (local) deletes.
        log_b.tombstone_document(&doc_b, &Author::User).unwrap();
        // A (peer, live) edits past the shared base.
        let a_text = "# Shared\n\nline one EDITED BY A\nline two\n";
        assert!(log_a.apply_user_text(&doc_a, a_text).unwrap());
        let peer = log_a.materialize_accepted(&doc_a).unwrap();
        let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
        let verdict = log_b
            .delete_vs_edit_verdict(&doc_b, &peer.text, peer.tombstone, &peer_hashes)
            .unwrap();
        assert_eq!(
            verdict,
            DeleteVsEdit::Conflict,
            "we-deleted + peer-edited must block, not silently delete-win"
        );
    }
}

#[test]
fn fast_forward_delete_verdict_not_a_conflict() {
    // status: sync-conflict-delete-vs-edit
    //
    // REGRESSION guard: a *sequential* delete (the peer deleted a version we
    // already hold, and we did NOT concurrently edit) is a fast-forward delete,
    // NOT a conflict. The verdict must be NotApplicable so the existing delta
    // path auto-applies it (→ the Phase-3 trash move) rather than blocking.
    use super::super::sync::DeleteVsEdit;
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // B stays at the shared base (no concurrent edit). A deletes the version B
    // already holds. A's tombstone keeps the base text, so the live side (B) ==
    // the base → fast-forward delete.
    log_a.tombstone_document(&doc_a, &Author::User).unwrap();
    let peer = log_a.materialize_accepted(&doc_a).unwrap();
    let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
    let verdict = log_b
        .delete_vs_edit_verdict(&doc_b, &peer.text, peer.tombstone, &peer_hashes)
        .unwrap();
    assert_eq!(
        verdict,
        DeleteVsEdit::NotApplicable,
        "a fast-forward delete (no concurrent edit) must NOT block — it auto-applies → trash"
    );

    // And the actual apply still tombstones B (the auto-apply path the verdict
    // defers to), confirming the fast-forward delete lands. The peer is
    // tombstoned, so `sync_text` ships `peer_tombstone = true`.
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));
    assert!(
        log_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "the fast-forward delete auto-applied (B tombstoned)"
    );
}

#[test]
fn delete_vs_edit_verdict_not_applicable_when_neither_or_both_tombstoned() {
    // status: sync-conflict-delete-vs-edit
    //
    // The delete-vs-edit gate needs EXACTLY one tombstoned side. Two plain edits
    // (neither tombstoned) is the same-region detector's job, not this one; a
    // converged delete (both tombstoned) is idempotent, no conflict.
    use super::super::sync::DeleteVsEdit;

    // Neither tombstoned (two live edits) → NotApplicable.
    {
        let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
        assert!(log_b
            .apply_user_text(&doc_b, "# Shared\n\nline one B\nline two\n")
            .unwrap());
        assert!(log_a
            .apply_user_text(&doc_a, "# Shared\n\nline one A\nline two\n")
            .unwrap());
        let peer = log_a.materialize_accepted(&doc_a).unwrap();
        let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
        assert_eq!(
            log_b
                .delete_vs_edit_verdict(&doc_b, &peer.text, peer.tombstone, &peer_hashes)
                .unwrap(),
            DeleteVsEdit::NotApplicable,
            "two live edits are not a delete-vs-edit (same-region's job)"
        );
    }

    // Both tombstoned (converged delete) → NotApplicable.
    {
        let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
        log_b.tombstone_document(&doc_b, &Author::User).unwrap();
        log_a.tombstone_document(&doc_a, &Author::User).unwrap();
        let peer = log_a.materialize_accepted(&doc_a).unwrap();
        let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
        assert_eq!(
            log_b
                .delete_vs_edit_verdict(&doc_b, &peer.text, peer.tombstone, &peer_hashes)
                .unwrap(),
            DeleteVsEdit::NotApplicable,
            "both-tombstoned is a converged delete, not a conflict"
        );
    }
}

#[test]
fn disjoint_region_edits_verdict_clean_merge() {
    // status: sync-conflict-detect-same-region
    // REGRESSION guard for the desired merge behavior: two devices edit
    // DISJOINT regions of a shared-lineage doc concurrently — the verdict must
    // be CleanMerge (no block), and the text merge that follows keeps both edits.
    use super::super::sync::SameRegion;
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
    // base on both: "# Shared\n\nline one\nline two\n"

    // A edits line two; B edits line one — disjoint byte ranges.
    let a_text = "# Shared\n\nline one\nline two [A]\n";
    let b_text = "# Shared\n\nline one [B]\nline two\n";
    assert!(log_a.apply_user_text(&doc_a, a_text).unwrap());
    assert!(log_b.apply_user_text(&doc_b, b_text).unwrap());

    // B classifies A's incoming text against B's own accepted + A's history.
    let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
    let verdict = log_b
        .same_region_verdict(&doc_b, a_text, &peer_hashes)
        .unwrap();
    assert_eq!(
        verdict,
        SameRegion::CleanMerge,
        "disjoint-region concurrent edits must NOT block — they auto-merge"
    );

    // And the actual text merge keeps both edits (the behavior the block must
    // not regress).
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));
    let merged = log_b.materialize_accepted(&doc_b).unwrap().text;
    assert!(merged.contains("[A]") && merged.contains("[B]"), "both edits survive: {merged:?}");
}

#[test]
fn same_region_edits_verdict_conflict() {
    // status: sync-conflict-detect-same-region
    // Two devices edit the SAME line of a shared-lineage doc concurrently — the
    // verdict must be Conflict (block), so the change is not silently
    // interleaved.
    use super::super::sync::SameRegion;
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();
    // base on both: "# Shared\n\nline one\nline two\n"

    // Both rewrite "line one" — overlapping byte ranges.
    let a_text = "# Shared\n\nline one EDITED BY A\nline two\n";
    let b_text = "# Shared\n\nline one EDITED BY B\nline two\n";
    assert!(log_a.apply_user_text(&doc_a, a_text).unwrap());
    assert!(log_b.apply_user_text(&doc_b, b_text).unwrap());

    let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
    let verdict = log_b
        .same_region_verdict(&doc_b, a_text, &peer_hashes)
        .unwrap();
    assert_eq!(
        verdict,
        SameRegion::Conflict,
        "same-region concurrent edits must block, not silently interleave"
    );
}

#[test]
fn fast_forward_verdict_clean_merge_without_fetch() {
    // status: sync-conflict-detect-same-region
    // The peer is strictly ahead (we never diverged): theirs == our base on the
    // shared base, so the verdict is CleanMerge even though theirs != ours.
    use super::super::sync::SameRegion;
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // Only A edits; B stays at the base.
    let a_text = "# Shared\n\nline one\nline two ADVANCED\n";
    assert!(log_a.apply_user_text(&doc_a, a_text).unwrap());

    let peer_hashes = log_a.doc_history_hashes(&doc_a).unwrap();
    let verdict = log_b
        .same_region_verdict(&doc_b, a_text, &peer_hashes)
        .unwrap();
    assert_eq!(
        verdict,
        SameRegion::CleanMerge,
        "a strict fast-forward is a clean merge, never a block"
    );
}

#[test]
fn apply_remote_update_noop_when_content_unchanged() {
    // status: op-log-multi-device-sync
    // status: op-log-sync-substrate
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // Applying B's OWN current text back to B carries no new content → no-op.
    assert!(
        !sync_text(&log_b, &doc_b, &log_b, &doc_b, "deviceA"),
        "an update with no new content must return false"
    );
    // A advances; B merges it once (advances), then a re-apply of the SAME peer
    // text is an idempotent no-op (the merge over the now-matching content is a
    // no-op commit).
    assert!(log_a.apply_user_text(&doc_a, "# Shared\n\nchanged\n").unwrap());
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));
    assert!(
        !sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"),
        "the second apply of the same peer text is a no-op"
    );
}

#[test]
fn sync_receive_records_real_op() {
    // status: bug-sync-clock-range-records-local-cid
    // status: op-log-sync-substrate
    //
    // FAITHFUL successor to the Yrs `bug_sync_clock_range_records_local_cid`
    // guard, ported to the text model. Under the text substrate
    // `apply_remote_update` lands the merged text through `commit_text_edit`,
    // which re-expresses the peer's content as localized text ops on OUR
    // `accepted` (a real local commit, authored `sync:<device>`). The Yrs clock
    // range is gone (the columns are vestigial 0s now), so the faithful
    // assertion is that the receive records a REAL accepted op — a
    // `sync:<device>`-authored row whose content hash matches the now-merged
    // accepted content — never a phantom that didn't actually land the bytes.
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // A authors an edit; B pulls A's text and merges it.
    let edited = "# Shared\n\nline one EDITED\nline two\n";
    assert!(log_a.apply_user_text(&doc_a, edited).unwrap());
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));

    // The sync row B recorded for this receive.
    let hist = log_b.doc_history(&doc_b, 50).unwrap();
    let row = hist
        .iter()
        .find(|m| matches!(&m.author, Author::Sync(d) if d == "deviceA"))
        .expect("expected a sync:deviceA-authored row on B");

    // The row describes a real op that landed the merged bytes: its content
    // hash equals the blake3 of B's now-current accepted text (which includes
    // A's edit), so the row corresponds to actual content, not a phantom.
    let merged = log_b.materialize_accepted(&doc_b).unwrap().text;
    assert!(merged.contains("EDITED"), "B's accepted must hold A's merged edit");
    assert_eq!(
        row.content_hash.as_deref(),
        Some(hash(&merged).as_str()),
        "the sync row's content hash must match the merged accepted content",
    );
}

#[test]
fn adopt_lineage_preserves_local_divergence() {
    // status: sync-lineage-adoption
    // B has a local edit before binding; adopting A's lineage must keep both
    // the canonical content and B's local change (re-expressed as a user op).
    let seed = "# Shared\n\nalpha\nbeta\n";
    let dir_a = tempdir().unwrap();
    let log_a = OpLog::open(dir_a.path()).unwrap();
    let doc_a = log_a
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();
    // A diverges canonically: appends a line B has never seen.
    let canonical_text = "# Shared\n\nalpha\nbeta\ngamma (from A)\n";
    assert!(log_a.apply_user_text(&doc_a, canonical_text).unwrap());

    let dir_b = tempdir().unwrap();
    let log_b = OpLog::open(dir_b.path()).unwrap();
    let doc_b = log_b
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();
    // B's local-only divergence: edits a line A's canonical text still has.
    let local_b = "# Shared\n\nalpha LOCAL-B\nbeta\n";
    assert!(log_b.apply_user_text(&doc_b, local_b).unwrap());

    // B adopts A's canonical lineage.
    let canonical = log_a.export_state(&doc_a).unwrap();
    log_b.adopt_lineage(&doc_b, &canonical).unwrap();

    // The merged result carries A's canonical addition AND B's local change.
    let merged = log_b.materialize_accepted(&doc_b).unwrap().text;
    assert!(merged.contains("gamma (from A)"), "lost A's canonical content: {merged:?}");
    assert!(merged.contains("LOCAL-B"), "lost B's local divergence: {merged:?}");
    // Disk reflects the merged accepted state.
    let on_disk = std::fs::read_to_string(dir_b.path().join("shared.md")).unwrap();
    assert_eq!(on_disk, merged);
}

#[test]
fn sync_text_receive_is_path_inert() {
    // status: bug-sync-remote-rename-overwrites-collision
    // status: op-log-sync-substrate
    //
    // FAITHFUL text-model successor to the Yrs `bug_sync_remote_rename_overwrites_collision`
    // guard. The former bug was that `apply_remote_update` repointed `meta.path`
    // from a Yrs `meta.path` op riding in the delta, clobbering a DIFFERENT
    // local doc's `.md` at the target path. Under the text substrate the wire
    // carries only TEXT — no path move can ride in — so `apply_remote_update` is
    // PATH-INERT: it merges the peer's text into the named doc and NEVER touches
    // any other doc's path or `.md`. (A concurrent rename collision is now the
    // transport's job: the manifest path-identity routes it to a Fork block,
    // covered by `concurrent_rename_to_same_target_blocks_for_resolution` in the
    // scenarios suite.) This pins the no-collateral-overwrite invariant at the
    // substrate boundary.
    let dir_local = tempdir().unwrap();
    let log_local = OpLog::open(dir_local.path()).unwrap();
    // doc-X owns `notes/foo.md` locally; its disk content is distinctive.
    let local_x_text = "LOCAL DOC X CONTENT\n";
    let doc_x = log_local
        .create_document("notes/foo.md", "note", local_x_text, &Author::User)
        .unwrap();
    // doc-Y lives at its own path locally; a peer shares its lineage.
    let local_y_text = "PEER SHARES ME\n";
    let doc_y = log_local
        .create_document("notes/bar.md", "note", local_y_text, &Author::User)
        .unwrap();

    // Peer independently holds doc-Y at the same path + content, adopts the
    // local's text, then edits its copy.
    let dir_peer = tempdir().unwrap();
    let log_peer = OpLog::open(dir_peer.path()).unwrap();
    let doc_y_peer = log_peer
        .create_document("notes/bar.md", "note", local_y_text, &Author::User)
        .unwrap();
    let canonical = log_local.export_state(&doc_y).unwrap();
    log_peer.adopt_lineage(&doc_y_peer, &canonical).unwrap();
    let peer_edit = "PEER SHARES ME\nplus a peer edit\n";
    log_peer.apply_user_text(&doc_y_peer, peer_edit).unwrap();

    // Local pulls the peer's TEXT for doc-Y and merges it into doc-Y. The text
    // wire conveys no path — doc-X at notes/foo.md cannot be touched.
    assert!(sync_text(&log_local, &doc_y, &log_peer, &doc_y_peer, "peer-device"));

    // doc-X's `.md` and content are untouched.
    let foo_on_disk = std::fs::read_to_string(dir_local.path().join("notes/foo.md"))
        .expect("doc-X's notes/foo.md should still exist on disk");
    assert_eq!(
        foo_on_disk, local_x_text,
        "doc-X's `.md` at notes/foo.md must be untouched by a text receive on doc-Y",
    );
    // Both paths still resolve to their own docs; nothing was repointed.
    assert_eq!(
        log_local.doc_id_for_path("notes/foo.md").unwrap().as_deref(),
        Some("notes/foo.md"),
        "doc-X's path mapping is intact",
    );
    assert_eq!(
        log_local.doc_id_for_path("notes/bar.md").unwrap().as_deref(),
        Some("notes/bar.md"),
        "doc-Y remains resolvable at its own path",
    );
    // doc-Y merged the peer's edit (the receive did its actual job).
    assert!(
        log_local.materialize_accepted(&doc_y).unwrap().text.contains("plus a peer edit"),
        "doc-Y merged the peer's text",
    );
    let _ = doc_x;
}

#[test]
fn bug_sync_adopt_lineage_discards_working() {
    // status: bug-sync-adopt-lineage-discards-working
    //
    // `adopt_lineage` reads `local_text` from `accepted` only and then drops
    // `working`. The doc-comment claims uncommitted edits "fold back in via
    // the merge" — they don't, because `working` was never read. A user with
    // uncommitted typing at the moment a peer's canonical lineage is adopted
    // silently loses that typing.
    let seed = "alpha\nbeta\ngamma\n";
    let dir_local = tempdir().unwrap();
    let log_local = OpLog::open(dir_local.path()).unwrap();
    let doc_local = log_local
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();

    // Uncommitted user typing on the working overlay: modifies line 2.
    // Replace "beta" (4 bytes, starting at offset 6) with "beta MODIFIED-BY-USER".
    let accepted_text = log_local.materialize_accepted(&doc_local).unwrap().text;
    let beta_start = accepted_text.find("beta").expect("seed contains beta");
    log_local
        .apply_working_edit(&doc_local, beta_start, "beta".len(), "beta MODIFIED-BY-USER")
        .unwrap();
    // Sanity: working materializes with the edit; accepted is still the seed.
    assert_eq!(
        log_local.materialize_working(&doc_local).unwrap().text,
        "alpha\nbeta MODIFIED-BY-USER\ngamma\n"
    );
    assert_eq!(log_local.materialize_accepted(&doc_local).unwrap().text, seed);

    // Peer's canonical state: same seed, but with a disjoint edit on line 3.
    let dir_peer = tempdir().unwrap();
    let log_peer = OpLog::open(dir_peer.path()).unwrap();
    let doc_peer = log_peer
        .create_document("shared.md", "note", seed, &Author::User)
        .unwrap();
    let peer_text = "alpha\nbeta\ngamma EXTENDED-BY-PEER\n";
    assert!(log_peer.apply_user_text(&doc_peer, peer_text).unwrap());
    let canonical = log_peer.export_state(&doc_peer).unwrap();

    // Adopt the peer's lineage. The three-way merge should see:
    //   base   = "alpha\nbeta\ngamma\n"          (shared seed)
    //   ours   = "alpha\nbeta MODIFIED-BY-USER\ngamma\n"   (working overlay)
    //   theirs = "alpha\nbeta\ngamma EXTENDED-BY-PEER\n"   (peer canonical)
    // → merged: "alpha\nbeta MODIFIED-BY-USER\ngamma EXTENDED-BY-PEER\n"
    log_local.adopt_lineage(&doc_local, &canonical).unwrap();

    let after = log_local.materialize_accepted(&doc_local).unwrap().text;
    assert!(
        after.contains("EXTENDED-BY-PEER"),
        "lost peer's canonical edit: {after:?}"
    );
    assert!(
        after.contains("MODIFIED-BY-USER"),
        "lost user's uncommitted working edit: {after:?} \
         (bug: adopt_lineage reads from accepted only and drops `working`, \
         so the user's in-progress typing is silently discarded instead of \
         being folded into the three-way merge as the doc-comment claims)"
    );
}

#[test]
fn apply_remote_tombstone_moves_local_md_to_trash_and_restore_recovers() {
    // bug-sync-remote-delete-leaves-ghost-file: a delete on device A syncs to B
    // as a tombstone flag. Before the fix, B's `apply_remote_update` advanced
    // the doc to tombstoned but `write_md_file` early-returned, leaving B's
    // `.md` on disk as a stale ghost (editable → could resurrect the doc). The
    // fix moves the lingering `.md` to TRASH (recoverable), referencing the
    // doc_id — consistent with the offline-delete reconcile — then restore
    // rebinds and recovers history.
    use crate::trash::{Kind, Trash};

    let (_da, log_a, doc_a, dir_b, log_b, doc_b) = shared_lineage();
    let md_path = dir_b.path().join("shared.md");
    assert!(md_path.exists(), "B's .md exists before the remote delete");

    // A deletes the doc; B receives the tombstone (text + tombstone flag).
    log_a.tombstone_document(&doc_a, &Author::User).unwrap();
    let advanced = sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA");
    assert!(advanced, "the tombstone receive advanced B");

    // The doc is tombstoned on B…
    assert!(
        log_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B's doc is tombstoned after applying the remote delete"
    );
    // …the ghost `.md` is gone from the vault…
    assert!(
        !md_path.exists(),
        "the local .md was removed (no ghost left to resurrect the doc)"
    );
    // …and it landed in trash with a doc_id-referencing entry.
    let trash = Trash::open(dir_b.path());
    let entries = trash.list().unwrap();
    assert_eq!(entries.len(), 1, "exactly one trash entry was created");
    let e = &entries[0];
    assert_eq!(e.original_path, "shared.md");
    assert_eq!(e.kind, Kind::File);
    assert_eq!(
        e.doc_id.as_deref(),
        Some(doc_b.as_str()),
        "the trash entry references the doc_id so restore rebinds + recovers history"
    );
    // The recoverable artifact is the last-known content (mirrors offline delete).
    let trashed = std::fs::read_to_string(trash.entry_path(e)).unwrap();
    assert_eq!(trashed, "# Shared\n\nline one\nline two\n");

    // Restore: file back at its path, same doc_id, tombstone cleared, history
    // intact — the inverse of the offline-delete trash round-trip.
    let restored = std::fs::rename(trash.entry_path(e), &md_path);
    restored.unwrap();
    log_b
        .restore_document(&doc_b, "shared.md", &Author::External)
        .unwrap();
    assert!(md_path.exists(), "the .md is back on disk after restore");
    assert!(
        !log_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "tombstone cleared after restore"
    );
    assert_eq!(
        log_b.path_for_doc(&doc_b).unwrap().as_deref(),
        Some("shared.md"),
        "path → doc_id rebound to the SAME doc_id (history not orphaned)"
    );
    // History intact: the original create + the sync tombstone + the restore are
    // all still keyed under the same doc_id.
    let hist = log_b.doc_history(&doc_b, 50).unwrap();
    assert!(
        hist.iter().any(|m| matches!(&m.author, Author::Sync(d) if d == "deviceA")),
        "the remote-delete op is retained in history under the same doc_id"
    );
}

#[test]
fn apply_remote_tombstone_idempotent_does_not_double_trash() {
    // Edge case: re-applying the SAME tombstone (the file is already gone after
    // the first apply) must be a no-op — no second trash entry, no error.
    use crate::trash::Trash;

    let (_da, log_a, doc_a, dir_b, log_b, doc_b) = shared_lineage();
    let md_path = dir_b.path().join("shared.md");

    log_a.tombstone_document(&doc_a, &Author::User).unwrap();

    // First apply: transition live → tombstoned, moves the .md to trash.
    assert!(sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"));
    assert!(!md_path.exists());
    let trash = Trash::open(dir_b.path());
    assert_eq!(trash.list().unwrap().len(), 1, "first apply trashed once");

    // Second apply of the SAME tombstone: B is already tombstoned (both sides
    // deleted), so the receive is a no-op — no transition, no second trash entry.
    assert!(
        !sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA"),
        "re-applying the same tombstone is a no-op"
    );
    assert_eq!(
        trash.list().unwrap().len(),
        1,
        "idempotent re-apply does NOT create a second trash entry"
    );
}

/// REPRO for the reported canvas corruption: edit on BOTH sides with ASYMMETRIC
/// dirty-buffer sync. A `.canvas` rides the whole-file-as-text substrate and
/// a canvas edit REPLACES the entire `working` span (no incremental editor
/// change set — `replace_working`), re-authoring every byte. When a peer delta
/// arrives while that re-authored `working` overlay is live, `apply_remote_update`
/// mirrors the peer's accepted-lineage ops ONTO `working` so the editable buffer
/// stays `accepted + working`. But the full-span replace tombstoned the original
/// content and re-inserted it under a different positional structure, so the
/// peer's ops land misaligned and `materialize_working` interleaves the two
/// near-identical numeric JSONs — the digit-splice (`5828` -> `582828`). The
/// dirty-buffer-ON side pokes frequently, so the OFF side keeps receiving deltas
/// while sitting on a dirty canvas working layer. [sync-canvas-corruption-probe]
#[test]
fn canvas_peer_delta_onto_dirty_working_interleaves_repro() {
    let node = |i: usize, x: i64, y: i64| {
        format!(
            "\t\t{{\n\t\t\t\"id\": \"n{i}\",\n\t\t\t\"x\": {x},\n\t\t\t\"y\": {y},\n\t\t\t\"width\": 260,\n\t\t\t\"height\": 140,\n\t\t\t\"type\": \"text\"\n\t\t}}"
        )
    };
    let canvas = |coords: &[(i64, i64)]| {
        let body = coords
            .iter()
            .enumerate()
            .map(|(i, (x, y))| node(i, *x, *y))
            .collect::<Vec<_>>()
            .join(",\n");
        format!("{{\n\t\"nodes\": [\n{body}\n\t],\n\t\"edges\": []\n}}\n")
    };
    // All fixture coords are <= 4 digits, so a 5+ digit run is corruption.
    let max_digit_run = |s: &str| {
        let mut max = 0usize;
        let mut cur = 0usize;
        for b in s.bytes() {
            if b.is_ascii_digit() {
                cur += 1;
                max = std::cmp::max(max, cur);
            } else {
                cur = 0;
            }
        }
        max
    };

    let base = canvas(&[
        (1000, 1000), (1300, 1000), (1600, 1000),
        (1000, 1300), (1300, 1300), (1600, 1300),
    ]);

    // Shared canvas lineage: A creates, B adopts the identical content.
    let dir_a = tempdir().unwrap();
    let log_a = OpLog::open(dir_a.path()).unwrap();
    let doc_a = log_a.create_document("board.canvas", "canvas", &base, &Author::User).unwrap();
    let dir_b = tempdir().unwrap();
    let log_b = OpLog::open(dir_b.path()).unwrap();
    let doc_b = log_b.create_document("board.canvas", "canvas", &base, &Author::User).unwrap();
    let canonical = log_a.export_state(&doc_a).unwrap();
    log_b.adopt_lineage(&doc_b, &canonical).unwrap();
    assert_eq!(log_b.materialize_accepted(&doc_b).unwrap().text, base, "B adopted A's canvas");

    // B (dirty-buffer OFF) drags node n4 — the canvas full-span working edit,
    // left UNCOMMITTED in `working` (an unsaved canvas edit).
    let b_edit = canvas(&[
        (1000, 1000), (1300, 1000), (1600, 1000),
        (1000, 1300), (4321, 1300), (1600, 1300),
    ]);
    log_b.replace_working(&doc_b, &b_edit).unwrap();
    assert_eq!(log_b.materialize_working(&doc_b).unwrap().text, b_edit, "B's working holds its edit");

    // A (dirty-buffer ON) drags a DIFFERENT node n1 and auto-commits → a delta.
    let a_edit = canvas(&[
        (1000, 1000), (9876, 1000), (1600, 1000),
        (1000, 1300), (1300, 1300), (1600, 1300),
    ]);
    log_a.apply_user_text(&doc_a, &a_edit).unwrap();

    // A's text lands on B WHILE B's dirty canvas working overlay is live.
    sync_text(&log_b, &doc_b, &log_a, &doc_a, "deviceA");

    // The editable buffer the canvas panel renders (and a later save commits).
    // Before the fix, A's `9876` was relocated to byte 0 (`9876{…`) because the
    // full-span working replace tombstoned the whole structure; with the
    // localized working diff it merges in place: valid JSON, both disjoint edits
    // present, no prepended garbage, no digit-splice.
    let working = log_b.materialize_working(&doc_b).unwrap().text;
    assert!(
        working.starts_with('{'),
        "canvas working buffer has content before its opening brace (byte-0 relocation):\n{working}"
    );
    assert!(
        max_digit_run(&working) <= 4,
        "canvas working buffer digit-spliced:\n{working}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&working)
        .unwrap_or_else(|e| panic!("canvas working buffer is not valid JSON ({e}):\n{working}"));
    assert_eq!(parsed["nodes"].as_array().map_or(0, Vec::len), 6, "all 6 nodes intact");
    assert!(working.contains("9876"), "A's disjoint edit merged into B's working overlay");
    assert!(working.contains("4321"), "B's own uncommitted edit survived the merge");
}

/// `commit_working` (an explicit Save) of a freshly-created note races a remote
/// round that lands the SAME first content into `accepted` between
/// `commit_working`'s two locks — the new-note content-doubling regression.
///
/// The note is created EMPTY (`accepted = ""`), the user types the first content
/// into `working`, and Save calls `commit_working`. It captures
/// `base_accepted_text = ""` under its first lock, then re-reads `current_accepted`
/// under the second lock and — because a peer that already adopted A's content
/// synced it back (or A's own prior commit landed) — `current_accepted` is now
/// `content` (`!= base ""`), so it runs
/// `three_way_merge(base="", ours=content, theirs=content)`.
///
/// `three_way_merge` diffs ours-vs-base = `[(0,0,content)]` and theirs-vs-base =
/// `[(0,0,content)]`. The range-overlap test `start < ts+tl && ts < end` is
/// `0 < 0 && 0 < 0` = false for two zero-width insertions at offset 0, so without
/// the identical-twin skip OUR span is re-applied on top of `theirs` (which
/// already contains the content), producing `content + content` = DOUBLED text.
/// The twin skip (matching `spans_overlap`) recognizes the converged edit and
/// drops it.
#[test]
fn commit_working_no_double_when_peer_races_identical_content() {
    use std::sync::Arc;

    let dir = tempdir().unwrap();
    let log = Arc::new(OpLog::open(dir.path()).unwrap());
    // Fresh note, created EMPTY (the new-note create seeds accepted = "").
    let doc_id = log
        .create_document("new.md", "note", "", &Author::User)
        .unwrap();

    // The user types the first content into `working` (the editor forward
    // binding). `accepted` is still empty; the buffer is dirty.
    let content = "first content line\nsecond content line\n";
    log.apply_working_edit(&doc_id, 0, 0, content).unwrap();
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, content);
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "");

    // Between commit_working's two locks, a remote round lands the SAME content
    // into `accepted` (the peer adopted A's content and synced it back). The hook
    // fires after the first lock captured base_accepted_text = "" but before the
    // re-check, so the re-check sees current_accepted = content != base.
    let log_for_hook = Arc::clone(&log);
    let doc_for_hook = doc_id.clone();
    let content_for_hook = content.to_string();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        log_for_hook
            .apply_user_text(&doc_for_hook, &content_for_hook)
            .unwrap();
    });
    *log.commit_working_test_hook.lock().unwrap() = Some(hook);

    log.commit_working(&doc_id).unwrap();

    // The autocommit must leave `accepted` at the single content — not doubled.
    let got = log.materialize_accepted(&doc_id).unwrap().text;
    assert_eq!(
        got, content,
        "commit_working three-way-merged its own first content over an \
         accepted that already advanced to the IDENTICAL content, doubling it: {got:?}"
    );
}

/// No-hook variant proving the doubling window is reachable through the REAL
/// remote-update path (not just the test hook): a genuine cross-lineage
/// `apply_remote_update` lands the peer's IDENTICAL first content into A's
/// `accepted` AND mirrors it onto A's still-dirty `working`, while A's `working`
/// holds the same uncommitted first content. Without the working-mirror's
/// text-level reconcile + the `three_way_merge` twin skip, A's Save then doubles.
///
/// A and B each create the note (independent lineages, both empty), each types
/// the same first content into `working`, then a remote delta from B lands into
/// A's accepted right before A saves.
#[test]
fn commit_working_no_double_via_real_remote_update_race() {
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let log_a = OpLog::open(dir_a.path()).unwrap();
    let log_b = OpLog::open(dir_b.path()).unwrap();

    let content = "shared first content\n";

    // Both create the note EMPTY on independent lineages.
    let doc_a = log_a.create_document("n.md", "note", "", &Author::User).unwrap();
    let doc_b = log_b.create_document("n.md", "note", "", &Author::User).unwrap();

    // A types content into `working` (uncommitted, accepted still empty).
    log_a.apply_working_edit(&doc_a, 0, 0, content).unwrap();
    assert_eq!(log_a.materialize_accepted(&doc_a).unwrap().text, "");

    // B commits the SAME first content into its accepted (B's own autocommit).
    log_b.apply_working_edit(&doc_b, 0, 0, content).unwrap();
    assert!(log_b.commit_working(&doc_b).unwrap());
    assert_eq!(log_b.materialize_accepted(&doc_b).unwrap().text, content);

    // A pulls B's text: B's content lands in A's accepted via the genuine
    // remote-update path while A's working still holds the uncommitted content.
    sync_text(&log_a, &doc_a, &log_b, &doc_b, "deviceB");
    let accepted_after_pull = log_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(
        accepted_after_pull, content,
        "B's content landed in A's accepted (now != the empty base A's working diffed from)"
    );

    // A's autocommit now runs: it captured base = "" earlier conceptually, but
    // re-reads accepted = content under the commit lock and three-way-merges its
    // own working content over the advanced accepted — doubling it.
    log_a.commit_working(&doc_a).unwrap();
    let got = log_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(
        got, content,
        "A's autocommit doubled its own first content over the remotely-arrived \
         identical content: {got:?}"
    );
}

/// New-note content survives a racing remote that brings the EMPTY base while
/// the user's first content is still in `working`. This is the benign direction
/// of the merge window (the new-note flow): base = "" (the fresh note), ours =
/// content (uncommitted), and a remote round carries the empty base into
/// `accepted` (a no-op against the already-empty accepted). `commit_working`
/// must land the user's content, never drop it. (The OTHER direction — a remote
/// that empties an ESTABLISHED note's `accepted` while the user has NO local
/// divergence — is standard three-way-merge: the remote deletion wins, and
/// `commit_working` correctly does not resurrect it. If an UNWANTED empty lands
/// over content there, the root is an upstream stale-empty adoption, not this
/// layer.)
#[test]
fn commit_working_new_note_content_survives_racing_empty() {
    use std::sync::Arc;

    let dir = tempdir().unwrap();
    let log = Arc::new(OpLog::open(dir.path()).unwrap());
    // Fresh note, created EMPTY; user types the first content into `working`.
    let doc_id = log.create_document("n.md", "note", "", &Author::User).unwrap();
    let content = "the user's first content\nthat must reach accepted\n";
    log.apply_working_edit(&doc_id, 0, 0, content).unwrap();
    assert_eq!(log.materialize_working(&doc_id).unwrap().text, content);
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "");

    // A remote round lands the empty base into accepted between the two locks
    // (idempotent against the already-empty accepted — the new-note shape).
    let log_for_hook = Arc::clone(&log);
    let doc_for_hook = doc_id.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        log_for_hook.apply_user_text(&doc_for_hook, "").unwrap();
    });
    *log.commit_working_test_hook.lock().unwrap() = Some(hook);

    log.commit_working(&doc_id).unwrap();

    let got = log.materialize_accepted(&doc_id).unwrap().text;
    assert_eq!(got, content, "the new note's first content must land in accepted: {got:?}");
}
