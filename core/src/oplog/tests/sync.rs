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

    // A edits; B asks for "ops since my watermark" and applies them.
    assert!(log_a.apply_user_text(&doc_a, "# Shared\n\nline one EDITED\nline two\n").unwrap());
    let b_sv = log_b.state_vector_bytes(&doc_b).unwrap();
    let delta = log_a.export_since(&doc_a, &b_sv).unwrap();
    let advanced = log_b.apply_remote_update(&doc_b, &delta, "deviceA").unwrap();
    assert!(advanced, "the delta carried new ops");

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
fn apply_remote_update_noop_when_no_new_ops() {
    // status: op-log-multi-device-sync
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // A delta computed against B's *own* current SV carries no ops B lacks.
    let b_sv = log_b.state_vector_bytes(&doc_b).unwrap();
    let empty_delta = log_b.export_since(&doc_b, &b_sv).unwrap();
    assert!(
        !log_b.apply_remote_update(&doc_b, &empty_delta, "deviceA").unwrap(),
        "an update with no new ops must return false"
    );
    // Re-applying an already-known delta is also a no-op (idempotent receive).
    assert!(log_a.apply_user_text(&doc_a, "# Shared\n\nchanged\n").unwrap());
    let b_sv = log_b.state_vector_bytes(&doc_b).unwrap();
    let delta = log_a.export_since(&doc_a, &b_sv).unwrap();
    assert!(log_b.apply_remote_update(&doc_b, &delta, "deviceA").unwrap());
    assert!(
        !log_b.apply_remote_update(&doc_b, &delta, "deviceA").unwrap(),
        "the second apply of the same delta is a no-op"
    );
}

#[test]
fn bug_sync_clock_range_records_local_cid() {
    // status: bug-sync-clock-range-records-local-cid
    //
    // `apply_remote_update` captures `cid = local.accepted.client_id()` and
    // brackets the merge with `state_clock(accepted, cid)` pre/post. But the
    // peer's update authors ops under the *peer's* client_id, so the local
    // cid's clock does not advance — the recorded `(yrs_client_id,
    // yrs_clock_lo, yrs_clock_hi)` describes a zero-width range that does not
    // correspond to any real op. The recorded row should instead reflect the
    // span of ops the update actually introduced (peer's client id, lo<hi).
    let (_da, log_a, doc_a, _db, log_b, doc_b) = shared_lineage();

    // A authors an edit — these ops live under A's client id.
    let edited = "# Shared\n\nline one EDITED\nline two\n";
    assert!(log_a.apply_user_text(&doc_a, edited).unwrap());

    // B applies A's delta.
    let b_sv = log_b.state_vector_bytes(&doc_b).unwrap();
    let delta = log_a.export_since(&doc_a, &b_sv).unwrap();
    assert!(log_b.apply_remote_update(&doc_b, &delta, "deviceA").unwrap());

    // The sync row B recorded for this receive.
    let hist = log_b.doc_history(&doc_b, 50).unwrap();
    let row = hist
        .iter()
        .find(|m| matches!(&m.author, Author::Sync(d) if d == "deviceA"))
        .expect("expected a sync:deviceA-authored row on B");

    // The bug: the captured clock range is zero-width because the local cid's
    // clock didn't advance — the peer's ops landed under the peer's cid.
    assert!(
        row.yrs_clock_hi > row.yrs_clock_lo,
        "recorded clock range is zero-width ({}..{}): the row does not describe \
         any real op (apply_remote_update captured the LOCAL cid's clock, but \
         the peer-authored ops advanced the PEER's clock)",
        row.yrs_clock_lo,
        row.yrs_clock_hi,
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
fn bug_sync_remote_rename_overwrites_collision() {
    // status: bug-sync-remote-rename-overwrites-collision
    //
    // `apply_remote_update` blindly calls `meta::repoint_doc` when a remote
    // update's merge advances `meta.path`. `repoint_doc` silently overwrites a
    // path mapping owned by a *different* local doc — and the subsequent
    // `write_md_file` clobbers that other doc's `.md` on disk. The expected
    // behavior (mirroring `accept_pending`'s pre-check) is to refuse the path
    // repoint and surface a Fork-style block, or route to a conflict-sibling
    // path. Either way, the local doc-X at `notes/foo.md` and its on-disk
    // content must be preserved.
    let dir_local = tempdir().unwrap();
    let log_local = OpLog::open(dir_local.path()).unwrap();
    // doc-X owns `notes/foo.md` locally; its disk content is distinctive.
    let local_x_text = "LOCAL DOC X CONTENT\n";
    let doc_x = log_local
        .create_document("notes/foo.md", "note", local_x_text, &Author::User)
        .unwrap();
    // doc-Y lives at a different path locally; the peer will rename it.
    let local_y_text = "PEER WILL RENAME ME\n";
    let doc_y = log_local
        .create_document("notes/bar.md", "note", local_y_text, &Author::User)
        .unwrap();

    // Peer log: independently has doc-Y at the same path with the same content,
    // adopts the local's lineage for doc-Y, then renames bar.md → foo.md.
    let dir_peer = tempdir().unwrap();
    let log_peer = OpLog::open(dir_peer.path()).unwrap();
    let doc_y_peer = log_peer
        .create_document("notes/bar.md", "note", local_y_text, &Author::User)
        .unwrap();
    let canonical = log_local.export_state(&doc_y).unwrap();
    log_peer.adopt_lineage(&doc_y_peer, &canonical).unwrap();
    // Peer renames its copy of doc-Y to the path that doc-X owns locally.
    log_peer
        .rename_document(&doc_y_peer, "notes/foo.md", &Author::User)
        .unwrap();

    // Local pulls the peer's delta for doc-Y. The delta carries a `meta.path`
    // advance to `notes/foo.md` — which collides with doc-X's path.
    let local_y_sv = log_local.state_vector_bytes(&doc_y).unwrap();
    let delta = log_peer.export_since(&doc_y_peer, &local_y_sv).unwrap();
    let result = log_local.apply_remote_update(&doc_y, &delta, "peer-device");

    // Expected fix: the collision is detected — either by returning an Err, or
    // by NOT performing the destructive overwrite (e.g. routing to a sibling
    // conflict path). Either way, doc-X's on-disk `.md` and path mapping must
    // be preserved.
    let foo_on_disk = std::fs::read_to_string(dir_local.path().join("notes/foo.md"))
        .expect("doc-X's notes/foo.md should still exist on disk");
    assert_eq!(
        foo_on_disk, local_x_text,
        "BUG: doc-X's `.md` at notes/foo.md was overwritten with doc-Y's content. \
         apply_remote_update silently repointed the path mapping and wrote \
         doc-Y's materialized text over doc-X's file. result={:?}",
        result,
    );
    // Optional stronger assertion: the path-index still resolves to doc-X.
    assert_eq!(
        log_local.doc_id_for_path("notes/foo.md").unwrap().as_deref(),
        Some(doc_x.as_str()),
        "BUG: doc_index path mapping for notes/foo.md was silently repointed to doc-Y, \
         orphaning doc-X. result={:?}",
        result,
    );
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
