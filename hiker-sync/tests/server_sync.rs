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
// Path is the cross-device identity (sync-path-identity); no LogicalId is
// negotiated — both clients reach the same doc via its vault-relative path.
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
        report_a.bound.iter().any(|p| p == doc_path),
        "A pushed the bound doc: {report_a:?}"
    );

    // B pulls everything past its cursor, decrypts, and converges.
    let report_b = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(
        report_b.converged.iter().any(|p| p == doc_path),
        "B converged via the relay: {report_b:?}"
    );

    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, expected, "B materialized A's relayed content");

    serve.abort();

    // --- Zero-knowledge: the stored bytes are ciphertext, not plaintext. ---
    // Re-open the hub's on-disk store and read what it actually persisted.
    let store = FileBlobStore::open(&server_data).unwrap();
    let blind = blind_id(&content_key, doc_path);
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

    // A makes an offline edit, pushes it.
    oplog_a.apply_user_text(&doc_a, "seed line\nDUPE-MARKER body\nthird relayed\n").unwrap();
    let expected = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(20)).await.unwrap() });

    node_a.sync_via_server(&server_addr).await.unwrap();
    let rb = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(rb.converged.iter().any(|p| p == doc_path), "B converged via the relay: {rb:?}");
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
    let _ = doc_a;

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
    // B holds its OWN doc at a different path; A's doc lives at a different
    // path, so the two derive disjoint blind_ids and never share a blob stream.
    // (Per sync-path-identity, the path IS the cross-device key.)

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(15)).await.unwrap() });

    node_a.sync_via_server(&server_addr).await.unwrap();
    // B syncs its OWN doc only; it pulls nothing under A's blind_id because
    // their paths differ.
    let rb = node_b.sync_via_server(&server_addr).await.unwrap();
    assert!(
        !rb.converged.iter().any(|p| p == "notes/a-shared.md"),
        "B never converged A's foreign path: {rb:?}"
    );
    assert!(
        !rb.bound.iter().any(|p| p == "notes/a-shared.md"),
        "B never pushed A's foreign path: {rb:?}"
    );

    serve.abort();

    // B's own doc is untouched — A's secret never interleaved into it.
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, own, "B's own doc keeps its own content exactly: {got_b:?}");
    assert!(!got_b.contains("SHARED secret"), "A's content did not leak into B's own doc");
}

// --- Server-side blob GC after rename ------------------------------------------

/// `sync-rename-blob-rotation`: when a rename op flows through the relay, the
/// receiving device sends a `DeleteBlob` for the old blind_id so the hub GCs
/// the orphan stream and resets its per-device cursors against it. A subsequent
/// re-push of an unrelated blob under the OLD blind_id is still possible (the
/// store is content-agnostic), but the receiver's pull cursor against that id
/// is now at 0, so a redundant blob arrives as fresh — proving the cursor was
/// indeed cleared.
///
/// Test shape: simulate the receiver's GC kick by calling
/// `BlobStore::delete(old_blind)` directly on the hub's on-disk store
/// (deterministic; no need to drive the live rename merge through libp2p just
/// to assert the side effect). This is the exact API path the production
/// `Message::DeleteBlob` handler uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_blob_drops_stream_and_cursor() {
    let content_key = ContentKey::generate();
    let server_dir = tempfile::tempdir().unwrap();

    // Seed two blobs under the OLD blind_id (the rename source) and one under
    // the NEW blind_id (the rename target), plus a per-device cursor against
    // the OLD blind_id.
    let old_path = "notes/old-name.md";
    let new_path = "notes/new-name.md";
    let blind_old = blind_id(&content_key, old_path);
    let blind_new = blind_id(&content_key, new_path);
    {
        let mut store = FileBlobStore::open(server_dir.path()).unwrap();
        store.push(&blind_old, 1, content_key.encrypt(b"old-1"));
        store.push(&blind_old, 2, content_key.encrypt(b"old-2"));
        store.push(&blind_new, 1, content_key.encrypt(b"new-1"));
        store.set_device_cursor("DEV-A", &blind_old, 2).unwrap();
        store.set_device_cursor("DEV-A", &blind_new, 1).unwrap();
        store.set_device_cursor("DEV-B", &blind_old, 1).unwrap();
    }

    // GC: the receiver's `Message::DeleteBlob` handler routes to this method.
    {
        let mut store = FileBlobStore::open(server_dir.path()).unwrap();
        store.delete(&blind_old);
    }

    // Reload and verify the OLD stream is gone, the NEW stream is intact, AND
    // every device's cursor against the OLD blind_id is cleared. Cursors
    // against unrelated blind_ids stay put.
    let store = FileBlobStore::open(server_dir.path()).unwrap();
    assert!(
        store.pull(&blind_old, 0).is_empty(),
        "old blind_id stream GC'd"
    );
    assert_eq!(store.latest_seq(&blind_old), None, "old blind_id has no head");
    let new_blobs = store.pull(&blind_new, 0);
    assert_eq!(new_blobs.len(), 1, "new blind_id stream untouched");
    assert_eq!(new_blobs[0].0, 1);

    assert_eq!(
        store.device_cursor("DEV-A", &blind_old),
        None,
        "DEV-A's cursor against OLD blind_id cleared"
    );
    assert_eq!(
        store.device_cursor("DEV-B", &blind_old),
        None,
        "DEV-B's cursor against OLD blind_id cleared"
    );
    assert_eq!(
        store.device_cursor("DEV-A", &blind_new),
        Some(1),
        "DEV-A's cursor against NEW blind_id preserved"
    );

    // Idempotent: a repeat GC is a successful no-op.
    {
        let mut store = FileBlobStore::open(server_dir.path()).unwrap();
        store.delete(&blind_old);
        store.delete("never-existed-blind-id");
    }
    let _ = (old_path, new_path);
}

// The wire-level handler for `Message::DeleteBlob` is exercised indirectly:
// the receiver-side rename-rotation kick lives in `apply_delta_from_peer`
// (`hiker-sync/src/transport/lineage.rs`) and routes through the `Hub`'s
// request handler to `BlobStore::delete`, which the test above asserts
// against. Protocol serde round-trip for `DeleteBlob`/`DeleteBlobAck` is
// covered in `protocol::tests::message_round_trips`.
