//! End-to-end LAN sync over real libp2p (TCP + Noise + yamux + request-response).
//!
//! Two in-process [`SyncNode`]s, each over its own temp-vault [`OpLog`], enroll
//! each other's device fingerprints and share one content key. Node A creates
//! and edits a document; node B dials A and runs [`SyncNode::sync_once`]. The
//! test asserts B's vault converges to A's document text, exercising the whole
//! Wave-2 path: a genuine libp2p connection, the Hello + Manifest exchange,
//! enrollment + classification, bind, lineage adoption, and — on a second
//! round after A edits again — the content-encrypted delta-streaming receive
//! path that records a `sync:`-authored `op_metadata` row.

use std::sync::Arc;
use std::time::Duration;

use hiker_core::oplog::meta::{Filter, OpStatus};
use hiker_core::oplog::shapes::Author;
use hiker_core::oplog::OpLog;
use hiker_sync::config::Settings;
use hiker_sync::crypto::{ContentKey, DeviceKeypair, SharedContentKey};
use hiker_sync::transport::{EnrolledPeers, SyncNode};

fn open_vault() -> (tempfile::TempDir, Arc<OpLog>) {
    let dir = tempfile::tempdir().unwrap();
    let oplog = OpLog::open(dir.path()).unwrap();
    (dir, Arc::new(oplog))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_converge_a_document_over_libp2p() {
    // Shared vault content key (transferred in-band after the fingerprint swap;
    // here we just hand both nodes the same key).
    let content_key = ContentKey::generate();

    // --- Node A: the source of truth. ---
    let (_dir_a, oplog_a) = open_vault();
    let doc_path = "notes/shared.md";
    let doc_a = oplog_a
        .create_document(doc_path, "note", "first line\n", &Author::User)
        .unwrap();
    // Edit it so A's lineage has real history (a fast-forward base for B).
    oplog_a
        .apply_user_text(&doc_a, "first line\nsecond line\n")
        .unwrap();

    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    // A enrolls B (out-of-band fingerprint swap).
    node_a.enroll_peer(fp_b.clone()).unwrap();

    // --- Node B: starts empty. ---
    let (_dir_b, oplog_b) = open_vault();
    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    // B enrolls A.
    node_b.enroll_peer(fp_a.clone()).unwrap();

    // A listens on an OS-assigned port; capture the concrete address.
    let bound = node_a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();

    // Drive A's responder loop concurrently while B syncs (both rounds).
    let server = tokio::spawn(async move {
        node_a.run(Duration::from_secs(15)).await.unwrap();
    });

    // --- Round 1: B has no replica → bind + adopt A's canonical lineage. ---
    let report = node_b.sync_once(&bound).await.unwrap();
    assert_eq!(report.bound.len(), 1, "one doc bound: {report:?}");
    assert_eq!(report.converged.len(), 1, "one doc converged: {report:?}");
    assert!(report.blocked.is_empty(), "nothing blocked: {report:?}");

    // B now has a local doc at the same path that materializes A's text.
    let doc_b = oplog_b
        .doc_id_for_path(doc_path)
        .unwrap()
        .expect("B has a doc at the synced path");
    let got = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(
        got, "first line\nsecond line\n",
        "B converged to A's document text"
    );

    // --- Round 2: A edits again; B pulls the delta over the shared lineage. ---
    // Drive A's OpLog directly (the same Arc the responder serves from).
    oplog_a
        .apply_user_text(&doc_a, "first line\nsecond line\nthird line\n")
        .unwrap();
    let expected = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let report2 = node_b.sync_once(&bound).await.unwrap();
    assert_eq!(report2.converged.len(), 1, "delta converged: {report2:?}");
    assert!(report2.blocked.is_empty(), "nothing blocked: {report2:?}");

    let got2 = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got2, expected, "B converged to A's edited text via the delta");

    // The delta rode in through the receive path: B's op history for that doc
    // carries a `sync:`-authored op (the `apply_remote_update` metadata row).
    let history = oplog_b
        .query_metadata(&Filter {
            doc_id: Some(doc_b.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        history
            .iter()
            .any(|op| matches!(&op.author, Author::Sync(_))),
        "B has a sync:-authored op after delta streaming: {history:?}"
    );

    server.abort();
}

/// Device-name transfer (`sync-device-name`): each node carries a self-set
/// `[sync].device_name`; after one `sync_once` BOTH sides have learned the
/// other's name from the `Hello`/`HelloAck` handshake, keyed by the peer's
/// enrolled fingerprint. The dialer learns the responder's name (via `HelloAck`)
/// and the responder learns the dialer's (via `Hello`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn device_name_transfers_on_handshake_both_directions() {
    let content_key = ContentKey::generate();

    let (_dir_a, oplog_a) = open_vault();
    let (_dir_b, oplog_b) = open_vault();

    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    // A names itself "desktop", B names itself "laptop".
    let cfg_a = Settings {
        device_name: Some("desktop".into()),
        ..Settings::default()
    };
    let cfg_b = Settings {
        device_name: Some("laptop".into()),
        ..Settings::default()
    };

    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        cfg_a,
        EnrolledPeers::new(),
    );
    node_a.enroll_peer(fp_b.clone()).unwrap();

    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        cfg_b,
        EnrolledPeers::new(),
    );
    node_b.enroll_peer(fp_a.clone()).unwrap();

    let bound = node_a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    // A short responder window: the single round below completes well within it,
    // and the test reads A's learned map only after the window returns A back.
    let server = tokio::spawn(async move {
        node_a.run(Duration::from_secs(2)).await.unwrap();
        // Hand A back so we can read its learned map post-round.
        node_a
    });

    // B dials A: the Hello carries B's "laptop"; the HelloAck carries A's "desktop".
    let _ = node_b.sync_once(&bound).await.unwrap();

    // B learned A's self-reported name from the HelloAck.
    let b_learned = node_b.learned_device_names();
    assert_eq!(
        b_learned.get(&fp_a.0).map(String::as_str),
        Some("desktop"),
        "B learned A's name from the HelloAck: {b_learned:?}"
    );

    // A learned B's self-reported name from the Hello (read after the run window).
    let node_a = server.await.unwrap();
    let a_learned = node_a.learned_device_names();
    assert_eq!(
        a_learned.get(&fp_b.0).map(String::as_str),
        Some("laptop"),
        "A learned B's name from the Hello: {a_learned:?}"
    );
}

/// The read-only "view diff" probe (`sync-fork-diff`): a dialer fetches the
/// responder's current accepted text for one path via `fetch_doc_text` and gets
/// exactly the responder's `materialize_accepted` text back, WITHOUT binding,
/// adopting, or mutating either side. Asserts the fetched text matches and that
/// the dialer's own forked doc at the same path is unchanged by the fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_doc_text_returns_peer_text_without_mutating_ours() {
    let content_key = ContentKey::generate();

    // Responder A holds the "theirs" version of a forked path.
    let (_dir_a, oplog_a) = open_vault();
    let path = "notes/forked.md";
    let doc_a = oplog_a
        .create_document(path, "note", "theirs line one\n", &Author::User)
        .unwrap();
    oplog_a
        .apply_user_text(&doc_a, "theirs line one\ntheirs line two\n")
        .unwrap();
    let theirs = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    // Dialer B holds a DIVERGENT "ours" version of the same path (a fork).
    let (_dir_b, oplog_b) = open_vault();
    let doc_b = oplog_b
        .create_document(path, "note", "ours line one\n", &Author::User)
        .unwrap();
    oplog_b
        .apply_user_text(&doc_b, "ours line one\nours OWN line\n")
        .unwrap();
    let ours_before = oplog_b.materialize_accepted(&doc_b).unwrap().text;

    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_a.enroll_peer(fp_b).unwrap();

    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_b.enroll_peer(fp_a).unwrap();

    let bound = node_a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = tokio::spawn(async move {
        node_a.run(Duration::from_secs(15)).await.unwrap();
    });

    // B fetches A's text for the path — the "view diff" probe.
    let fetched = node_b.fetch_doc_text(&bound, path).await.unwrap();
    assert_eq!(fetched, theirs, "B fetched A's accepted text verbatim");

    // The fetch is read-only: B's own forked doc is untouched, and no binding /
    // status flip happened (B still has its own divergent text at the path).
    let ours_after = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(ours_after, ours_before, "fetch did NOT mutate our doc");
    assert_ne!(ours_after, theirs, "ours and theirs still diverge (a fork)");

    server.abort();
}
