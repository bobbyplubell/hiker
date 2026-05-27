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
