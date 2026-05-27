//! Zero-knowledge store-and-forward through the `hiker-syncd` hub.
//!
//! An in-process [`Hub`] over a file-backed store on TCP loopback relays
//! ciphertext between two clients that never talk directly. The clients are
//! already bound to a shared logical id (binding happens via the P2P manifest
//! exchange / enrollment, not through the zero-knowledge hub, which only sees
//! opaque blobs) — pre-seeded here with the [`SyncNode::bind_for_test`] helper
//! plus a one-time lineage adoption that stands in for a prior P2P round.
//!
//! The test proves two things: (1) client B converges to client A's content
//! purely through the relay, and (2) the bytes the server stored are ciphertext
//! — not the plaintext document, and only the content key recovers it. That is
//! the zero-knowledge guarantee. [sync-zero-knowledge-server, sync-blind-id]

use std::sync::Arc;
use std::time::Duration;

use hiker_core::oplog::shapes::Author;
use hiker_core::oplog::OpLog;
use hiker_sync::config::Settings;
use hiker_sync::crypto::{blind_id, ContentKey, DeviceKeypair, SharedContentKey};
use hiker_sync::identity::{LocalDocId, LogicalId};
use hiker_sync::server::{BlobStore, FileBlobStore, Hub};
use hiker_sync::transport::{EnrolledPeers, SyncNode};

fn open_vault() -> (tempfile::TempDir, Arc<OpLog>) {
    let dir = tempfile::tempdir().unwrap();
    let oplog = OpLog::open(dir.path()).unwrap();
    (dir, Arc::new(oplog))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_relays_ciphertext_and_b_converges() {
    // One shared vault content key for both clients (the server never sees it).
    let content_key = ContentKey::generate();
    let logical = LogicalId(ulid::Ulid::new().to_string());
    let doc_path = "notes/shared.md";

    // Device keypairs + fingerprints.
    let kp_server = DeviceKeypair::generate();
    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_server = kp_server.fingerprint();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    // --- The hub: file-backed store, both clients enrolled, TCP loopback. ---
    let server_dir = tempfile::tempdir().unwrap();
    let server_data = server_dir.path().to_path_buf();
    let mut server = Hub::new(
        kp_server,
        &server_data,
        vec![fp_a.clone(), fp_b.clone()],
    )
    .unwrap();
    let server_addr = server.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();

    // --- Client A: a doc with content, bound to the shared logical id. ---
    let (_dir_a, oplog_a) = open_vault();
    let doc_a = oplog_a
        .create_document(doc_path, "note", "first line\n", &Author::User)
        .unwrap();
    oplog_a
        .apply_user_text(&doc_a, "first line\nsecond line\n")
        .unwrap();

    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_a.enroll_peer(fp_server.clone()).unwrap();
    node_a.bind_for_test(LocalDocId(doc_a.clone()), logical.clone());

    // --- Client B: a separate vault that already shares A's lineage. ---
    // The shared lineage stands in for a prior P2P bind/adopt; the server path
    // assumes docs are already bound. B adopts A's base, then both are bound to
    // the one logical id, so a later server-relayed delta merges cleanly.
    let (_dir_b, oplog_b) = open_vault();
    let doc_b = oplog_b
        .create_document(doc_path, "note", "", &Author::User)
        .unwrap();
    let base_a = oplog_a.export_state(&doc_a).unwrap();
    oplog_b.adopt_lineage(&doc_b, &base_a).unwrap();
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        "first line\nsecond line\n",
        "B adopted A's base lineage"
    );

    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_b.enroll_peer(fp_server.clone()).unwrap();
    node_b.bind_for_test(LocalDocId(doc_b.clone()), logical.clone());

    // A makes a new offline edit B has never seen.
    oplog_a
        .apply_user_text(&doc_a, "first line\nsecond line\nthird line\n")
        .unwrap();
    let expected = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    // Drive the hub while the clients sync.
    let serve = tokio::spawn(async move {
        server.run(Duration::from_secs(15)).await.unwrap();
    });

    // A pushes its state to the hub (and pulls its own back, a no-op merge).
    let report_a = node_a.sync_via_server(&server_addr).await.unwrap();
    assert!(
        report_a.bound.contains(&logical),
        "A pushed the bound doc: {report_a:?}"
    );

    // B pulls everything past its cursor, decrypts, and converges.
    let report_b = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(
        report_b.converged.contains(&logical),
        "B converged via the relay: {report_b:?}"
    );

    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, expected, "B materialized A's relayed content");

    serve.abort();

    // --- Zero-knowledge: the stored bytes are ciphertext, not plaintext. ---
    // Re-open the hub's on-disk store and read what it actually persisted.
    let store = FileBlobStore::open(&server_data).unwrap();
    let blind = blind_id(&content_key, &logical.0);
    let stored = store.pull(&blind, 0);
    assert!(!stored.is_empty(), "the hub stored A's pushed blob(s)");

    let plaintext_marker = b"third line";
    for (_seq, ciphertext) in &stored {
        // No stored blob contains the plaintext document text.
        assert!(
            !ciphertext
                .windows(plaintext_marker.len())
                .any(|w| w == plaintext_marker),
            "stored blob must not contain plaintext"
        );
        // Only the content key recovers a valid Yrs update; a wrong key fails.
        assert!(
            content_key.decrypt(ciphertext).is_ok(),
            "content key decrypts the stored ciphertext"
        );
        let wrong = ContentKey::from_bytes([0u8; 32]);
        assert!(
            wrong.decrypt(ciphertext).is_err(),
            "a wrong key cannot decrypt the stored ciphertext"
        );
    }
}

// --- W5. Server store-and-forward path safety: idempotent re-pull ----------

/// Two clients that already share a lineage push/pull edits via the hub and
/// converge with NO duplication; then a client RE-PULLS the same blobs and does
/// NOT double content. Idempotency is the corruption probe here: the hub's
/// append-only log returns the same ciphertext on a re-pull from a reset cursor,
/// and `apply_remote_update` must merge those already-known Yrs ops as a no-op
/// (not re-insert the body). We force the re-pull by resetting B's pull cursor
/// (`reset_server_cursor_for_test`) so it re-fetches from seq 0 — exactly the
/// "client re-pulls the same blobs" case. [sync-zero-knowledge-server]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_idempotent_repull_does_not_double() {
    let content_key = ContentKey::generate();
    let logical = LogicalId(ulid::Ulid::new().to_string());
    let doc_path = "notes/relay-idem.md";

    let kp_server = DeviceKeypair::generate();
    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_server = kp_server.fingerprint();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    let server_dir = tempfile::tempdir().unwrap();
    let server_data = server_dir.path().to_path_buf();
    let mut server = Hub::new(kp_server, &server_data, vec![fp_a.clone(), fp_b.clone()]).unwrap();
    let server_addr = server.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();

    // Client A: a doc with a uniquely-markered body.
    let (_dir_a, oplog_a) = open_vault();
    let doc_a = oplog_a.create_document(doc_path, "note", "seed line\n", &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, "seed line\nDUPE-MARKER body\n").unwrap();
    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_a.enroll_peer(fp_server.clone()).unwrap();
    node_a.bind_for_test(LocalDocId(doc_a.clone()), logical.clone());

    // Client B: shares A's lineage (prior P2P bind/adopt stand-in).
    let (_dir_b, oplog_b) = open_vault();
    let doc_b = oplog_b.create_document(doc_path, "note", "", &Author::User).unwrap();
    oplog_b.adopt_lineage(&doc_b, &oplog_a.export_state(&doc_a).unwrap()).unwrap();
    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_b.enroll_peer(fp_server.clone()).unwrap();
    node_b.bind_for_test(LocalDocId(doc_b.clone()), logical.clone());

    // A makes an offline edit, pushes it.
    oplog_a.apply_user_text(&doc_a, "seed line\nDUPE-MARKER body\nthird relayed\n").unwrap();
    let expected = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(20)).await.unwrap() });

    node_a.sync_via_server(&server_addr).await.unwrap();
    let rb = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(rb.converged.contains(&logical), "B converged via the relay: {rb:?}");
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, expected, "B materialized A's relayed content exactly");
    assert_eq!(got_b.matches("DUPE-MARKER").count(), 1, "marker once after first pull: {got_b:?}");

    // RE-PULL: reset B's cursor so it re-fetches the SAME blobs from seq 0, then
    // sync again twice. Idempotent merge must not double the body.
    node_b.reset_server_cursor_for_test();
    node_b.sync_via_server(&server_addr).await.unwrap();
    node_b.sync_via_server(&server_addr).await.unwrap();

    let after = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(after, expected, "re-pull did not change B's content: {after:?}");
    assert_eq!(after.matches("DUPE-MARKER").count(), 1, "marker STILL once after re-pull: {after:?}");
    assert_eq!(
        after.matches("third relayed").count(),
        1,
        "relayed line not doubled by re-pull: {after:?}"
    );

    serve.abort();
}

// --- W5b. Pulling a blob for an unbound doc is safe (no interleave) --------

/// A client that has NOT bound a logical id (so it doesn't share that lineage)
/// must not corrupt anything via the server path. `sync_via_server` iterates
/// only the client's OWN bound docs, so an unbound logical id is simply never
/// pulled — there is nothing to interleave. We assert the client pushes/pulls
/// only its bound doc and the foreign blob the hub holds is left untouched in
/// the client's vault (its unbound doc keeps its own content). [sync-zero-knowledge-server]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_unbound_doc_is_not_interleaved() {
    let content_key = ContentKey::generate();
    let logical_shared = LogicalId(ulid::Ulid::new().to_string());

    let kp_server = DeviceKeypair::generate();
    let kp_a = DeviceKeypair::generate();
    let kp_b = DeviceKeypair::generate();
    let fp_server = kp_server.fingerprint();
    let fp_a = kp_a.fingerprint();
    let fp_b = kp_b.fingerprint();

    let server_dir = tempfile::tempdir().unwrap();
    let server_data = server_dir.path().to_path_buf();
    let mut server = Hub::new(kp_server, &server_data, vec![fp_a.clone(), fp_b.clone()]).unwrap();
    let server_addr = server.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();

    // A holds a doc bound to `logical_shared` and pushes it to the hub.
    let (_dir_a, oplog_a) = open_vault();
    let doc_a = oplog_a.create_document("notes/a-shared.md", "note", "A SHARED secret\n", &Author::User).unwrap();
    let mut node_a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_a.enroll_peer(fp_server.clone()).unwrap();
    node_a.bind_for_test(LocalDocId(doc_a.clone()), logical_shared.clone());

    // B has a DIFFERENT doc that is NOT bound to `logical_shared` (no shared
    // lineage with A's doc). Its own content must be preserved.
    let (_dir_b, oplog_b) = open_vault();
    let own = "B's OWN private content\n";
    let doc_b = oplog_b.create_document("notes/b-own.md", "note", own, &Author::User).unwrap();
    let mut node_b = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_b.enroll_peer(fp_server.clone()).unwrap();
    // B does NOT bind its doc to logical_shared. (It's unbound for sync.)

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(15)).await.unwrap() });

    node_a.sync_via_server(&server_addr).await.unwrap();
    // B syncs: it has no bound docs, so it pulls nothing for the foreign blob.
    let rb = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(rb.converged.is_empty(), "B converged nothing (no bound doc): {rb:?}");
    assert!(rb.bound.is_empty(), "B pushed nothing (no bound doc): {rb:?}");

    serve.abort();

    // B's own doc is untouched — A's secret never interleaved into it.
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, own, "B's unbound doc keeps its own content exactly: {got_b:?}");
    assert!(!got_b.contains("SHARED secret"), "A's content did not leak into B's unbound doc");
}
