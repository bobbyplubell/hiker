use super::*;
use hiker_sync::crypto::DeviceKeypair;
use hiker_sync::transport::SyncNode;

/// The auto-sync round CORE (`run_sync_round` — the same path the periodic /
/// on-discovery driver in bootstrap calls) drives convergence between two
/// real libp2p nodes, with NO manual `sync_once` in the test body. Node A
/// holds a doc and runs its responder loop; B's round dials A's listen addr
/// and converges. This is the auto path proving it converges on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_round_core_converges_two_nodes_without_manual_sync() {
    use hiker_sync::config::Settings;
    use hiker_sync::crypto::{ContentKey, SharedContentKey};

    let content_key = ContentKey::generate();

    // Node A: source of truth with one edited doc.
    let dir_a = tempfile::tempdir().unwrap();
    let oplog_a = Arc::new(hiker_core::oplog::OpLog::open(dir_a.path()).unwrap());
    let doc_path = "notes/auto.md";
    let doc_a = oplog_a
        .create_document(doc_path, "note", "alpha\n", &hiker_core::oplog::shapes::Author::User)
        .unwrap();
    oplog_a.apply_user_text(&doc_a, "alpha\nbeta\n").unwrap();

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

    // Node B: empty, behind a shared mutex like the live service holds it.
    let dir_b = tempfile::tempdir().unwrap();
    let oplog_b = Arc::new(hiker_core::oplog::OpLog::open(dir_b.path()).unwrap());
    let node_b_inner = SyncNode::new(
        Arc::clone(&oplog_b),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    node_b_inner.enroll_peer(fp_a.clone()).unwrap();

    // A listens; drive its responder loop concurrently.
    let bound = node_a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = tokio::spawn(async move {
        let _ = node_a.run(Duration::from_secs(15)).await;
    });

    // Seed B's discovery map with A at its bound addr, as mDNS would — the
    // LAN round reads `discovered_peers()` live (classified against the live
    // enrolled set on read), so A must be in the map AND enrolled (it is).
    node_b_inner.record_discovered_for_test(&fp_a, &bound);
    let node_b = Arc::new(tokio::sync::Mutex::new(node_b_inner));

    // Drive convergence through the auto-sync round CORE — NOT `sync_once`.
    let targets = RoundTargets {
        uses_server: false,
        server_url: String::new(),
    };
    let report = SyncService::run_sync_round(&node_b, targets)
        .await
        .expect("round ok")
        .expect("round ran (peer known)");
    assert_eq!(report.bound.len(), 1, "one doc bound: {report:?}");
    assert_eq!(report.converged.len(), 1, "one doc converged: {report:?}");

    let doc_b = oplog_b
        .doc_id_for_path(doc_path)
        .unwrap()
        .expect("B has the synced doc");
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        "alpha\nbeta\n",
        "B converged to A's text via the auto-round core"
    );

    server.abort();
}

/// A LAN round with no known peers is a benign no-op (`Ok(None)`), not an
/// error — this is what keeps periodic auto-rounds silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lan_round_with_no_peers_is_silent_noop() {
    use hiker_sync::config::Settings;
    use hiker_sync::crypto::{ContentKey, SharedContentKey};

    let dir = tempfile::tempdir().unwrap();
    let oplog = Arc::new(hiker_core::oplog::OpLog::open(dir.path()).unwrap());
    let node = Arc::new(tokio::sync::Mutex::new(SyncNode::new(
        oplog,
        SharedContentKey::new(ContentKey::generate()),
        DeviceKeypair::generate(),
        Settings::default(),
        EnrolledPeers::new(),
    )));
    let targets = RoundTargets {
        uses_server: false,
        server_url: String::new(),
    };
    let out = SyncService::run_sync_round(&node, targets).await.unwrap();
    assert!(out.is_none(), "no peers → Ok(None), the silent no-op");
}

/// A peer discovered over mDNS while UN-enrolled is NOT a round target, but
/// enrolling it (with NO new mDNS event) makes the very next round target it
/// — `run_sync_round` reads `discovered_peers()` live, classified against the
/// live enrolled set on read. This is the app-level guard for the "enroll
/// reclassifies an already-seen peer" fix. [sync-mdns-discovery]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_promotes_already_seen_peer_for_next_round() {
    use hiker_sync::config::Settings;
    use hiker_sync::crypto::{ContentKey, SharedContentKey};

    let fp_peer = DeviceKeypair::generate().fingerprint();
    let addr = "/ip4/127.0.0.1/tcp/40123";

    let dir = tempfile::tempdir().unwrap();
    let oplog = Arc::new(hiker_core::oplog::OpLog::open(dir.path()).unwrap());
    let enrolled = EnrolledPeers::new();
    let node_inner = SyncNode::new(
        oplog,
        SharedContentKey::new(ContentKey::generate()),
        DeviceKeypair::generate(),
        Settings::default(),
        enrolled.clone(),
    );
    // Seen over mDNS while NOT enrolled.
    node_inner.record_discovered_for_test(&fp_peer, addr);
    let node = Arc::new(tokio::sync::Mutex::new(node_inner));

    // Round 1: peer is seen but un-enrolled → no round target → silent no-op.
    let out = SyncService::run_sync_round(
        &node,
        RoundTargets { uses_server: false, server_url: String::new() },
    )
    .await
    .unwrap();
    assert!(out.is_none(), "un-enrolled seen peer is not a round target");

    // Enroll it — no second mDNS event. The shared set is what the node reads.
    enrolled.enroll(fp_peer).unwrap();

    // Round 2: now the peer IS a target, so the round actually attempts the
    // dial (no addr is listening, so it fails) rather than the no-op above —
    // proving the just-enrolled, already-seen peer was reclassified live.
    let out = SyncService::run_sync_round(
        &node,
        RoundTargets { uses_server: false, server_url: String::new() },
    )
    .await;
    assert!(
        matches!(out, Err(_)),
        "enrolled-already-seen peer is now a target (dial attempted, fails with no listener): {out:?}"
    );
}

#[test]
fn key_store_round_trips_device_and_content() {
    let tmp = tempfile::tempdir().unwrap();
    let store = KeyStore::at(tmp.path().join("ks"));

    // First call generates + persists.
    let kp1 = store.load_or_generate_device().unwrap();
    let ck1 = store.load_or_generate_content().unwrap();
    let fp1 = kp1.fingerprint();
    let ck1_bytes = *ck1.as_bytes();

    // Second call loads the same material back.
    let kp2 = store.load_or_generate_device().unwrap();
    let ck2 = store.load_or_generate_content().unwrap();
    assert_eq!(kp2.fingerprint(), fp1, "device key persists across loads");
    assert_eq!(*ck2.as_bytes(), ck1_bytes, "content key persists across loads");

    // Files live where we expect, NOT inside any vault.
    assert!(tmp.path().join("ks").join("device.key").exists());
    assert!(tmp.path().join("ks").join("content.key").exists());
}

#[test]
fn store_content_overwrites_persisted_key() {
    let tmp = tempfile::tempdir().unwrap();
    let store = KeyStore::at(tmp.path().join("ks"));
    let _generated = store.load_or_generate_content().unwrap();

    let imported = ContentKey::from_bytes([3u8; 32]);
    store.store_content(&imported).unwrap();
    // Re-loading reads the imported key, not the original generated one.
    let back = store.load_or_generate_content().unwrap();
    assert_eq!(*back.as_bytes(), [3u8; 32], "imported key persisted");
}

#[test]
fn alias_store_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let store = KeyStore::at(tmp.path().join("ks"));

    // Empty before anything is written.
    assert!(store.load_aliases().is_empty());

    let mut map = std::collections::HashMap::new();
    map.insert("DEV-ABC".to_string(), "phone".to_string());
    map.insert("DEV-XYZ".to_string(), "laptop".to_string());
    store.store_aliases(&map).unwrap();

    let back = store.load_aliases();
    assert_eq!(back.get("DEV-ABC").map(String::as_str), Some("phone"));
    assert_eq!(back.get("DEV-XYZ").map(String::as_str), Some("laptop"));
    assert_eq!(back.len(), 2);

    // Sidecar lives in the key-store dir, not any vault.
    assert!(tmp.path().join("ks").join("aliases.json").exists());
}

#[test]
fn friendly_round_error_maps_decrypt() {
    let decrypt = "content decryption failed (bad key or tampered ciphertext)";
    let mapped = friendly_round_error(decrypt);
    assert!(mapped.contains("different content keys"), "decrypt → key hint");
    // A non-decrypt error passes through unchanged.
    let other = "transport error: dial failed";
    assert_eq!(friendly_round_error(other), other);
}

#[test]
fn distinct_vaults_get_distinct_key_dirs() {
    // Two real vaults get distinct stable ids → distinct key dirs.
    let a_vault = tempfile::tempdir().unwrap();
    let b_vault = tempfile::tempdir().unwrap();
    let a = KeyStore::dir_for_vault(a_vault.path());
    let b = KeyStore::dir_for_vault(b_vault.path());
    assert_ne!(a, b, "different vaults get different key dirs");
}

#[test]
fn vault_key_dir_survives_move() {
    // The key dir is keyed by the in-vault stable id, so a vault that moves
    // to a new path (carrying its `.hiker/vault-id`) resolves to the SAME
    // key dir — keys are retained across the move instead of regenerating.
    let v1 = tempfile::tempdir().unwrap();
    let before = KeyStore::dir_for_vault(v1.path());
    // Simulate a move: a different path carrying the same vault-id file.
    let v2 = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(v2.path().join(".hiker")).unwrap();
    std::fs::copy(
        v1.path().join(".hiker/vault-id"),
        v2.path().join(".hiker/vault-id"),
    )
    .unwrap();
    let after = KeyStore::dir_for_vault(v2.path());
    assert_eq!(before, after, "same vault id → same key dir after a move");
}

#[test]
fn section_maps_to_lib_config() {
    let section = SyncSection {
        enabled: true,
        mode: CoreSyncMode::Both,
        server_url: "/dns4/hub.example/tcp/4001".to_string(),
        discovery: false,
        devices: vec!["DEV-ABC".to_string()],
        device_name: "laptop".to_string(),
        device_names: std::collections::HashMap::from([(
            "DEV-ABC".to_string(),
            "phone".to_string(),
        )]),
    };
    let cfg = section_to_config(&section);
    assert!(cfg.enabled);
    assert_eq!(cfg.mode, SyncMode::Both);
    assert_eq!(cfg.server_url, "/dns4/hub.example/tcp/4001");
    assert!(!cfg.discovery);
    assert_eq!(cfg.devices, vec!["DEV-ABC".to_string()]);
    // device_name maps to Some when non-empty; learned map carries through.
    assert_eq!(cfg.device_name.as_deref(), Some("laptop"));
    assert_eq!(cfg.device_names.get("DEV-ABC").map(String::as_str), Some("phone"));

    // An empty device_name maps to None (unnamed).
    let unnamed = SyncSection::default();
    assert!(section_to_config(&unnamed).device_name.is_none());

    // Each core mode maps to its lib counterpart.
    for (core, lib) in [
        (CoreSyncMode::Peer, SyncMode::Peer),
        (CoreSyncMode::Server, SyncMode::Server),
        (CoreSyncMode::Both, SyncMode::Both),
    ] {
        let s = SyncSection {
            mode: core,
            ..SyncSection::default()
        };
        assert_eq!(section_to_config(&s).mode, lib);
    }
}
