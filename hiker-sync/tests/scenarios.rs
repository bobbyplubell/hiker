//! Wave 4 — broad end-to-end scenario suite for `hiker-sync`.
//!
//! Every scenario drives REAL [`SyncNode`]s over a genuine libp2p stack (TCP +
//! Noise + yamux + request-response/cbor) on `127.0.0.1` loopback, against real
//! [`OpLog`] vaults in temp dirs, with real AES-256-GCM content encryption. No
//! transport is mocked. The only thing standing between these and a true
//! multi-process / multi-host run is that both endpoints live in one process and
//! talk over the loopback interface rather than a physical LAN.
//!
//! The scenarios map onto `docs/sync.md` slugs:
//!
//! 1. fresh two-device sync — sync-negotiated-doc-ids, sync-lineage-adoption
//! 2. disjoint concurrent edits converge — sync-content-encryption-aes256 + CRDT
//! 3. fast-forward, no prompt — sync-enrollment-hash-classification
//! 4. true fork → Blocked — sync-blocked-state
//! 5. rename safety — sync-path-matching-key
//! 6. server store-and-forward, zero-knowledge — sync-zero-knowledge-server, sync-blind-id
//! 7. enrollment gate — sync-key-swap-enrollment / sync-noise-channel
//! 8. multi-document session — sync-stream-muxing
//! 9. (stretch) three-device transitivity

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hiker_core::oplog::meta::{Filter, OpStatus};
use hiker_core::oplog::shapes::Author;
use hiker_core::oplog::OpLog;
use hiker_sync::config::Settings;
use hiker_sync::crypto::{blind_id, ContentKey, DeviceKeypair, SharedContentKey};
use hiker_sync::enroll::{classify, Classification};
use hiker_sync::identity::SyncStatus;
use hiker_sync::server::{BlobStore, FileBlobStore, Hub};
use hiker_sync::transport::{EnrolledPeers, SyncNode};

// --- shared helpers --------------------------------------------------------

/// A fresh, empty temp-vault [`OpLog`] behind an `Arc` (the responder loop and
/// the test body both hold it).
fn open_vault() -> (tempfile::TempDir, Arc<OpLog>) {
    let dir = tempfile::tempdir().unwrap();
    let oplog = OpLog::open(dir.path()).unwrap();
    (dir, Arc::new(oplog))
}

/// Build a [`SyncNode`] over a fresh temp vault, sharing `content_key`. Returns
/// the node, its `Arc<OpLog>` (to drive edits directly), its temp dir (kept
/// alive for the test's lifetime), and its device keypair fingerprint pieces via
/// the node's own `fingerprint()`.
fn mk_node(content_key: &ContentKey) -> (SyncNode, Arc<OpLog>, tempfile::TempDir) {
    let (dir, oplog) = open_vault();
    let node = SyncNode::new(
        Arc::clone(&oplog),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        DeviceKeypair::generate(),
        Settings::default(),
        EnrolledPeers::new(),
    );
    (node, oplog, dir)
}

/// Mutually enroll two nodes' fingerprints (the out-of-band swap that
/// authenticates the Noise channel).
fn enroll_each_other(a: &SyncNode, b: &SyncNode) {
    a.enroll_peer(b.fingerprint()).unwrap();
    b.enroll_peer(a.fingerprint()).unwrap();
}

/// blake3 hex of a string — the same content hash the manifest / `classify` use.
fn hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn set(items: &[&str]) -> HashSet<String> {
    items.iter().map(ToString::to_string).collect()
}

/// True if any accepted op on `doc_id` is `sync:`-authored — the fingerprint of
/// a delta / base that rode in over the receive path.
fn has_sync_authored_op(oplog: &OpLog, doc_id: &str) -> bool {
    oplog
        .query_metadata(&Filter {
            doc_id: Some(doc_id.to_string()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .iter()
        .any(|op| matches!(&op.author, Author::Sync(_)))
}

/// Spawn `node`'s responder loop for `window`, returning the join handle. The
/// node is moved in; the caller drives a dialer against the returned address.
fn spawn_responder(mut node: SyncNode, window: Duration) -> tokio::task::JoinHandle<SyncNode> {
    tokio::spawn(async move {
        node.run(window).await.unwrap();
        node
    })
}

// --- 0. Independent-lineage identical content must not duplicate ----------

/// Regression for the worst sync correctness bug: two vaults seeded
/// INDEPENDENTLY (separate `create_document` calls) hold the SAME text on
/// DISJOINT Yrs lineages — the `cp -r` + `.hiker` deleted-then-reseeded shape.
/// Their state vectors are meaningless to each other, so a cross-lineage delta
/// (`export_since(our_sv)`) returns the peer's WHOLE doc and applying it would
/// INSERT a second copy of the body — doubling the note.
///
/// The fix establishes a shared lineage before any delta: on `Identical`, the
/// non-canonical side adopts the canonical side's base and only then binds, so
/// the steady-state delta path always runs on a shared lineage. After a full
/// bidirectional sync both vaults must materialize the ORIGINAL single text,
/// never doubled — and a later edit must propagate exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_lineages_identical_content_do_not_duplicate() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/dup.md";
    let text = "alpha line\nbeta line\ngamma line\n";
    // INDEPENDENT seeds: each vault creates its own doc with its own lineage
    // (distinct client ids) over identical bytes — exactly two reseeded copies.
    let doc_a = oplog_a.create_document(path, "note", text, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", text, &Author::User).unwrap();

    // Both start at the original single text.
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, text);
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, text);

    // Drive a full sync to steady state. One round each direction settles the
    // canonical/adoption + bind handshake regardless of which side is canonical
    // (the canonical side does nothing on its own pull and is bound by the
    // non-canonical side's post-adoption BindRequest). Run twice so a
    // canonical-pulls-first ordering also fully converges.
    for _ in 0..2 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        b.sync_once(&addr_a).await.unwrap();
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();
    }

    // THE ASSERTION: neither side doubled the body. Before the fix at least one
    // side materializes `text` twice (the cross-lineage delta re-inserted it).
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        text,
        "A's content must not be doubled"
    );
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        text,
        "B's content must not be doubled"
    );

    // Edit on one side, sync again: the edit propagates exactly once, both
    // converge, still no duplication.
    let edited = "alpha line\nbeta line\ngamma line\nDELTA line\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a2 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a2).await.unwrap();
    let _a = server_a2.await.unwrap();

    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, edited, "B picked up A's edit exactly once: {got_b:?}");
    assert_eq!(
        got_b.matches("DELTA line").count(),
        1,
        "the edited line appears exactly once (no duplication): {got_b:?}"
    );
}

// === Wave 5 — enrollment flows + lineage / no-duplication correctness ======
//
// These guard the just-fixed bugs: content doubling across independent
// lineages, asymmetric enrollment (a peer that connected to us must still
// surface so the user can enroll it), and enroll-from-discovered promotion.
// Every scenario drives convergence DETERMINISTICALLY via explicit `sync_once`
// against a bounded responder `run` — no wall-clock waits, no auto-sync tick,
// no live mDNS multicast. Scenario "mutual enrollment converges" is already
// covered by `fresh_two_device_sync` below (A holds a doc, B empty, mutual
// enroll + shared content key, B `sync_once`→A converges) so it is not
// duplicated here.

// --- E1. One-sided enrollment is refused ----------------------------------

/// A enrolls B, but B does NOT enroll A. A dials B and tries to sync: B's
/// connection-auth gate drops A (un-enrolled), so A's `sync_once` fails and B's
/// vault never converges. The complement of `unenrolled_peer_cannot_sync`
/// (which has the puller un-swapped); here the puller IS enrolled but the
/// RESPONDER hasn't enrolled it. [sync-key-swap-enrollment, sync-noise-channel]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_sided_enrollment_is_refused() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);

    // Only A enrolls B; B does NOT enroll A. A can dial out (B is enrolled on
    // A), but B drops A's connection as un-enrolled.
    a.enroll_peer(b.fingerprint()).unwrap();

    let path = "notes/asym.md";
    oplog_b
        .create_document(path, "note", "B's private note\n", &Author::User)
        .unwrap();
    let seed_a = "A's own note\n";
    let doc_a = oplog_a.create_document("notes/a-own.md", "note", seed_a, &Author::User).unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(4));

    // A pulls from B: B refuses the un-enrolled peer, so the round errors.
    let result = a.sync_once(&addr_b).await;
    assert!(
        result.is_err(),
        "one-sided enrollment must not sync (B drops un-enrolled A): {result:?}"
    );

    let _b = server_b.await.unwrap();

    // A converged nothing — its vault is unchanged (still only its own note).
    assert!(
        oplog_a.doc_id_for_path(path).unwrap().is_none(),
        "A did not receive B's doc over a refused connection"
    );
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        seed_a,
        "A's own content is untouched"
    );
}

// --- E2. Asymmetric → surface → recover ------------------------------------

/// The incoming-connection surfacing + enroll-from-discovered + recovery path.
/// A enrolls B but B doesn't enroll A. After A connects to B, A appears in B's
/// `seen_unenrolled()` (the "surface a peer that only revealed itself via the
/// connection" fix — otherwise mutual enrollment couldn't complete from the
/// UI). Then the user enrolls A on B, and a subsequent sync converges.
/// [sync-key-swap-enrollment, sync-noise-channel, sync-mdns-discovery]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asymmetric_enrollment_surfaces_then_recovers() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);

    // Asymmetric to start: A enrolls B, B does not enroll A.
    a.enroll_peer(b.fingerprint()).unwrap();
    let a_fp = a.fingerprint();

    let path = "notes/recover.md";
    let text = "content A wants to share once B enrolls it\n";
    let doc_a = oplog_a.create_document(path, "note", text, &Author::User).unwrap();

    // Round 1: A dials B. B's responder gets A's (un-enrolled) connection,
    // surfaces it into its discovery set, then drops it. A's sync errors.
    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(4));
    let r1 = a.sync_once(&addr_b).await;
    assert!(r1.is_err(), "round 1 refused (B hasn't enrolled A): {r1:?}");
    let mut b = server_b.await.unwrap();

    // The fix: A is now visible in B's unenrolled-seen surface even though only
    // the inbound CONNECTION (not an mDNS announce) revealed it, and the
    // surfaced fingerprint matches A's real device fingerprint. (The window may
    // also fold in unrelated hiker instances via live mDNS on a busy LAN, so we
    // assert A is PRESENT with the right fingerprint rather than that it's the
    // only entry — the deterministic part is the connection-surfaced A.)
    let seen = b.seen_unenrolled();
    assert!(
        seen.iter().any(|(_, _, fp)| fp.as_deref() == Some(a_fp.0.as_str())),
        "A surfaced as seen-unenrolled on B with its real fingerprint: {seen:?}"
    );
    assert!(
        b.discovered_peers().is_empty(),
        "A is not yet a dial candidate (no peer is enrolled on B): {:?}",
        b.discovered_peers()
    );

    // The user enrolls A on B (from the surfaced fingerprint). Now mutual.
    b.enroll_peer(a_fp).unwrap();

    // Round 2: A dials B again; the connection now authenticates and converges.
    let addr_b2 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b2 = spawn_responder(b, Duration::from_secs(4));
    let r2 = a.sync_once(&addr_b2).await.unwrap();
    let b = server_b2.await.unwrap();

    // A is the puller here and already holds the doc, so the converge happens on
    // B's side when B next pulls. Drive B→A so B adopts A's content.
    assert!(r2.blocked.is_empty(), "round 2 not blocked: {r2:?}");
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(4));
    let mut b = b;
    let rb = b.sync_once(&addr_a).await.unwrap();
    let _a = server_a.await.unwrap();

    assert_eq!(rb.converged.len(), 1, "B converged after enrolling A: {rb:?}");
    let doc_b = oplog_b
        .doc_id_for_path(path)
        .unwrap()
        .expect("B received the doc after recovery");
    let want = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        want,
        "B converged to A's content once enrollment was mutual"
    );
}

// --- E3. Un-enroll stops syncing on the live gate --------------------------

/// A↔B mutually enrolled + converged on a doc. Un-enrolling B on A (dropping it
/// from the shared `EnrolledPeers`) takes effect on the LIVE connection-auth
/// gate: a subsequent B→A `sync_once` is refused, so B never receives A's new
/// edit. Exercises that un-enroll is visible to the running swarm immediately,
/// no rebuild. [sync-key-swap-enrollment]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unenroll_stops_syncing() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/revoked.md";
    let seed = "shared baseline\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    // Round 1: converge B to A over a shared lineage.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr_a).await.unwrap();
    assert_eq!(r1.converged.len(), 1, "B converged at first: {r1:?}");
    let mut a = server_a.await.unwrap();

    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);

    // A un-enrolls B on the live shared set, then makes a new edit.
    a.unenroll_peer(&b.fingerprint()).unwrap();
    let edited = "shared baseline\nA's post-revocation edit\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    // Round 2: B tries to pull A's delta, but A now drops B as un-enrolled.
    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a2 = spawn_responder(a, Duration::from_secs(4));
    let r2 = b.sync_once(&addr_a2).await;
    let _a = server_a2.await.unwrap();

    assert!(
        r2.is_err(),
        "after un-enroll the live gate refuses B: {r2:?}"
    );
    // B never saw A's new edit.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        seed,
        "B's content is frozen at the pre-revocation baseline"
    );
}

// --- E4. Enroll-from-discovered makes a seen peer a target -----------------

/// e2e flavor of the read-time-classification fix (the unit test
/// `enrolling_a_seen_peer_promotes_it_to_candidate` covers the synthetic
/// mechanism). With real nodes: record B as discovered on A while un-enrolled,
/// so B sits in A's `seen_unenrolled` but is NOT a dial candidate; then enroll
/// B and assert it appears in A's `discovered_peers()` with NO new mDNS event —
/// a round would now target it. [sync-mdns-discovery, sync-key-swap-enrollment]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_promotes_already_seen_peer_for_next_round() {
    let key = ContentKey::generate();
    let (a, _oplog_a, _da) = mk_node(&key);
    let (b, _oplog_b, _db) = mk_node(&key);
    let b_fp = b.fingerprint();

    // B is seen on the LAN at an address, but A hasn't enrolled it yet.
    a.record_discovered_for_test(&b_fp, "/ip4/127.0.0.1/tcp/40123");
    let seen = a.seen_unenrolled();
    assert_eq!(seen.len(), 1, "B is seen-unenrolled: {seen:?}");
    assert_eq!(seen[0].2.as_deref(), Some(b_fp.0.as_str()), "B's fingerprint surfaced");
    assert!(
        a.discovered_peers().is_empty(),
        "B is not yet a dial candidate while un-enrolled"
    );

    // Enroll B — NO second mDNS event. Read-time classification alone promotes
    // it: it leaves the seen-unenrolled surface and becomes a dial candidate, so
    // the next round would target it.
    a.enroll_peer(b_fp.clone()).unwrap();
    assert!(
        a.seen_unenrolled().is_empty(),
        "B left the seen-unenrolled surface on enroll alone"
    );
    let candidates = a.discovered_peers();
    assert_eq!(candidates.len(), 1, "B promoted to a dial candidate: {candidates:?}");
    assert_eq!(
        candidates[0].fingerprint, b_fp,
        "the candidate is B, classified live from the same discovery map"
    );
}

// --- L1. One-side edit fast-forwards across independent lineages -----------

/// A & B start with IDENTICAL content on INDEPENDENT lineages (each
/// `create_document` the same text — distinct client ids). A edits the note,
/// then they sync. B must converge to A's edited text EXACTLY, the edited body
/// appearing ONCE (not doubled by a cross-lineage delta), and as a clean
/// fast-forward (no Blocked). This is the independent-lineage no-duplication
/// guarantee carried through a one-sided edit. [sync-lineage-adoption]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_side_edit_fast_forwards_without_duplication() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/ff-dup.md";
    let text = "one\ntwo\nthree\n";
    // INDEPENDENT seeds over identical bytes — two reseeded copies, disjoint SVs.
    let doc_a = oplog_a.create_document(path, "note", text, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", text, &Author::User).unwrap();

    // A edits BEFORE any sync, so first contact is identical-history-but-ahead.
    let edited = "one\ntwo\nthree\nMARKER edit by A\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    // Two bidirectional rounds settle canonical/adoption + bind regardless of
    // which side is canonical (mirrors the regression test's drive loop).
    for _ in 0..2 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr_a).await.unwrap();
        assert!(rb.blocked.is_empty(), "B side never forks: {rb:?}");
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        let ra = a.sync_once(&addr_b).await.unwrap();
        assert!(ra.blocked.is_empty(), "A side never forks: {ra:?}");
        b = server_b.await.unwrap();
    }

    // B converged to A's edited text EXACTLY, with the edit appearing once.
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, edited, "B fast-forwarded to A's exact edited text: {got_b:?}");
    assert_eq!(
        got_b.matches("MARKER edit by A").count(),
        1,
        "the edited line is present exactly once (no cross-lineage doubling): {got_b:?}"
    );
    // A's own content is unchanged and equally un-doubled.
    let got_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(got_a, edited, "A keeps its edited text: {got_a:?}");
    assert_eq!(got_a.matches("MARKER edit by A").count(), 1, "no doubling on A: {got_a:?}");
}

// --- L2. Post-convergence disjoint edits, each appearing once --------------

/// After A & B share a lineage, A edits region X and B edits region Y
/// (disjoint). A bidirectional sync lands BOTH edits on BOTH sides, each
/// appearing EXACTLY ONCE — the steady-state delta path over a shared lineage
/// must not duplicate. [sync-content-encryption-aes256, sync-stream-muxing]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_convergence_disjoint_edits_appear_once() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/disjoint-once.md";
    let seed = "HEAD base\nmiddle\nTAIL base\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    // Round 0: establish a shared lineage (B adopts A's base).
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    let r0 = b.sync_once(&addr0).await.unwrap();
    assert_eq!(r0.converged.len(), 1, "shared lineage established: {r0:?}");
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);

    // Disjoint edits: A edits HEAD (region X), B edits TAIL (region Y), each with
    // a unique marker so we can count occurrences.
    oplog_a
        .apply_user_text(&doc_a, "HEAD ALPHA-marker\nmiddle\nTAIL base\n")
        .unwrap();
    oplog_b
        .apply_user_text(&doc_b, "HEAD base\nmiddle\nTAIL OMEGA-marker\n")
        .unwrap();

    // Bidirectional sync over the shared lineage.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr_a).await.unwrap();
    assert!(rb.blocked.is_empty(), "B side not blocked: {rb:?}");
    let mut a = server1.await.unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr_b).await.unwrap();
    assert!(ra.blocked.is_empty(), "A side not blocked: {ra:?}");
    let _b = server2.await.unwrap();

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converge to one merged text");
    for text in [&text_a, &text_b] {
        assert_eq!(
            text.matches("ALPHA-marker").count(),
            1,
            "A's edit appears exactly once (no duplication): {text:?}"
        );
        assert_eq!(
            text.matches("OMEGA-marker").count(),
            1,
            "B's edit appears exactly once (no duplication): {text:?}"
        );
    }
}

// --- L3. Idempotent re-sync (no drift, no doubling) ------------------------

/// Two already-converged (shared-lineage) nodes `sync_once` again with NO
/// changes. Content must be byte-identical afterward on both sides — repeated
/// rounds never drift or double. [sync-lineage-adoption]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_resync_no_drift() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/idem.md";
    let text = "stable line one\nSTABLE-marker\nstable line three\n";
    let doc_a = oplog_a.create_document(path, "note", text, &Author::User).unwrap();

    // Round 0: converge B to A (shared lineage).
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, text);

    // Capture the converged texts, then re-sync several times with NO changes.
    let before_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let before_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    for _ in 0..3 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr_a).await.unwrap();
        assert!(rb.blocked.is_empty(), "no-op re-sync never blocks: {rb:?}");
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        let ra = a.sync_once(&addr_b).await.unwrap();
        assert!(ra.blocked.is_empty(), "no-op re-sync never blocks: {ra:?}");
        b = server_b.await.unwrap();
    }

    // Byte-identical to the converged state — no drift, no doubling.
    let after_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let after_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(after_a, before_a, "A did not drift across re-syncs: {after_a:?}");
    assert_eq!(after_b, before_b, "B did not drift across re-syncs: {after_b:?}");
    assert_eq!(after_a, text, "A still the original single text");
    assert_eq!(after_b, text, "B still the original single text");
    assert_eq!(after_a.matches("STABLE-marker").count(), 1, "no doubling on A: {after_a:?}");
    assert_eq!(after_b.matches("STABLE-marker").count(), 1, "no doubling on B: {after_b:?}");
}

// --- 1. Fresh two-device sync (P2P) ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_two_device_sync() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/fresh.md";
    let doc_a = oplog_a
        .create_document(path, "note", "alpha line\nbeta line\n", &Author::User)
        .unwrap();

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));

    let report = b.sync_once(&addr).await.unwrap();
    assert_eq!(report.bound.len(), 1, "one doc bound: {report:?}");
    assert_eq!(report.converged.len(), 1, "one doc converged: {report:?}");
    assert!(report.blocked.is_empty(), "nothing blocked: {report:?}");

    // B's vault now holds a doc at the path that materializes A's exact text.
    let doc_b = oplog_b
        .doc_id_for_path(path)
        .unwrap()
        .expect("B has a doc at the synced path");
    let want = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, want);

    // A second round: A edits, B pulls the incremental delta over the now-shared
    // lineage. That receive path records a `sync:`-authored op-metadata row on B
    // (the adopt reconciliation itself lands as a `user` op by design, so the
    // provenance row appears on the streaming delta, not the initial base).
    let mut a = server.await.unwrap();
    oplog_a
        .apply_user_text(&doc_a, "alpha line\nbeta line\ngamma line\n")
        .unwrap();
    let want2 = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let report2 = b.sync_once(&addr2).await.unwrap();
    assert!(report2.blocked.is_empty(), "delta not blocked: {report2:?}");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, want2);
    assert!(
        has_sync_authored_op(&oplog_b, &doc_b),
        "B carries a sync:-authored op after the streamed delta"
    );

    server2.abort();
}

// --- 2. Disjoint concurrent edits converge (no conflict) ------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disjoint_concurrent_edits_converge() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/concurrent.md";
    // A seeds a multi-line doc with distinct head/tail regions.
    let seed = "HEAD original\nmiddle\nTAIL original\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    // Round 0: establish a shared lineage (B adopts A's base, both bound).
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    let r0 = b.sync_once(&addr_a).await.unwrap();
    assert_eq!(r0.converged.len(), 1, "shared lineage established: {r0:?}");
    let mut a = server0.await.unwrap();

    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);

    // Now disjoint edits: A edits the HEAD region, B edits the TAIL region.
    oplog_a
        .apply_user_text(&doc_a, "HEAD edited-by-A\nmiddle\nTAIL original\n")
        .unwrap();
    oplog_b
        .apply_user_text(&doc_b, "HEAD original\nmiddle\nTAIL edited-by-B\n")
        .unwrap();

    // Bidirectional sync over the shared lineage: B pulls A's delta, then A
    // pulls B's. Both should end with BOTH edits and no Blocked.
    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr_a2).await.unwrap();
    assert!(rb.blocked.is_empty(), "B side not blocked: {rb:?}");
    let mut a = server1.await.unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr_b).await.unwrap();
    assert!(ra.blocked.is_empty(), "A side not blocked: {ra:?}");
    let _b = server2.await.unwrap();

    // Both replicas carry BOTH disjoint edits (CRDT positional merge).
    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    for text in [&text_a, &text_b] {
        assert!(text.contains("HEAD edited-by-A"), "A's edit present in {text:?}");
        assert!(text.contains("TAIL edited-by-B"), "B's edit present in {text:?}");
    }
    assert_eq!(text_a, text_b, "both converge to one merged text");
}

// --- 3. Fast-forward, no prompt -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_forward_no_prompt() {
    // First, the unit-level classification: B's current hash sits in A's
    // history (A moved ahead), so A sees a clean peer-adopts fast-forward and B
    // (the puller) sees adopt-peer — never a fork.
    let base = "line one\n";
    let ahead = "line one\nline two\n";
    assert_eq!(
        classify(&hash(ahead), &set(&[&hash(ahead), &hash(base)]), &hash(base), &set(&[&hash(base)])),
        Classification::FastForwardPeerAdopts,
        "the ahead side sees the peer as a prior version"
    );
    assert_eq!(
        classify(&hash(base), &set(&[&hash(base)]), &hash(ahead), &set(&[&hash(ahead), &hash(base)])),
        Classification::FastForwardAdoptPeer,
        "the behind side adopts the ahead peer"
    );

    // Now the live sync. A and B converge on `base`, then A edits ahead so B's
    // current content is strictly a prior version of A's lineage.
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/ff.md";
    let doc_a = oplog_a.create_document(path, "note", base, &Author::User).unwrap();

    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();

    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    let b_snapshot = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(b_snapshot, base, "B is at the shared base");

    // A moves ahead; B's snapshot hash is now in A's history.
    oplog_a.apply_user_text(&doc_a, ahead).unwrap();
    assert!(
        oplog_a.doc_history_hashes(&doc_a).unwrap().contains(&hash(&b_snapshot)),
        "B's prior content is in A's lineage history"
    );

    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr1).await.unwrap();
    let _a = server1.await.unwrap();

    assert!(report.blocked.is_empty(), "fast-forward must not block: {report:?}");
    assert_eq!(report.converged.len(), 1, "B fast-forwarded: {report:?}");
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        ahead,
        "B fast-forwarded to A's ahead content"
    );
}

// --- 4. True fork → Blocked -----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn true_fork_is_blocked() {
    // Unit assertion: same shared seed in history, but divergent currents with
    // no cross-history overlap is a fork.
    let seed = "shared seed\n";
    let a_text = "shared seed\nA's divergent branch\n";
    let b_text = "shared seed\nB's divergent branch\n";
    assert_eq!(
        classify(
            &hash(b_text),
            &set(&[&hash(b_text), &hash(seed)]),
            &hash(a_text),
            &set(&[&hash(a_text), &hash(seed)]),
        ),
        Classification::Fork,
        "divergent currents over a shared seed, no overlap, is a fork"
    );

    // Live sync: two vaults seeded to the SAME initial text (a common content
    // hash in each history), then each edited independently with no ancestry
    // overlap. Neither is bound — this is first contact.
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr).await.unwrap();
    let _a = server.await.unwrap();

    // The doc is Blocked, reported as a fork, and NOTHING merged.
    assert!(
        report.blocked.iter().any(|(p, reason)| p == path && reason == "fork"),
        "fork reported in SyncReport.blocked: {report:?}"
    );
    assert!(report.converged.is_empty(), "no silent convergence: {report:?}");
    assert_eq!(
        b.status_of(&hiker_sync::identity::LocalDocId(doc_b.clone())),
        Some(SyncStatus::Blocked),
        "B's doc is Blocked"
    );
    // Each side keeps its own divergent content; no interleave happened.
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text);
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_text);
}

// --- 4b. Fork resolution: keep-theirs converges ---------------------------

/// A forked doc, then resolved with `KeepTheirs`: on the NEXT round the blocked
/// side adopts the peer's lineage, converges to the peer's content, and its
/// persistent blocked entry clears. Driven deterministically (two explicit
/// `sync_once` rounds against a responder, no wall-clock waits). [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_keep_theirs_converges() {
    use hiker_sync::identity::{LocalDocId, Resolution};

    // The two branches edit the SAME region of the shared seed (a genuine
    // conflict, no clean disjoint merge), so a keep-theirs adoption resolves to
    // the peer's canonical content rather than interleaving both. [sync-blocked-state]
    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: first contact forks. B blocks and records it persistently.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    assert!(
        r1.blocked.iter().any(|(p, reason)| p == path && reason == "fork"),
        "round 1 forks: {r1:?}"
    );
    let local_b = LocalDocId(doc_b.clone());
    assert_eq!(
        b.status_of(&local_b),
        Some(SyncStatus::Blocked),
        "B is blocked after round 1"
    );
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one persistent blocked entry: {blocked:?}");
    let logical = blocked[0].logical_id.clone();
    assert_eq!(blocked[0].path, path, "blocked record carries the path");
    assert_eq!(
        blocked[0].peer_fingerprint,
        a.fingerprint(),
        "blocked record names the peer we forked against"
    );

    // The user chooses keep-theirs on B for that logical id.
    b.set_fork_resolution(logical.clone(), Resolution::KeepTheirs);

    // Round 2: the fork branch consumes the decision and adopts A's lineage.
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let _a = server2.await.unwrap();

    assert!(r2.blocked.is_empty(), "round 2 no longer blocks: {r2:?}");
    assert!(
        r2.converged.iter().any(|l| l == &logical),
        "round 2 converged the resolved doc: {r2:?}"
    );

    // B converged to A's content, and the persistent blocked entry cleared.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "B adopted A's (keep-theirs) content"
    );
    assert!(
        b.blocked_docs().is_empty(),
        "the blocked record cleared on resolution: {:?}",
        b.blocked_docs()
    );
}

// --- 4b'. Fork resolution: keep-mine resolves BOTH sides -------------------

/// keep-mine now converges BOTH devices in one click: the resolver (the side
/// that dials) makes ITS version canonical and PUSHES its Yrs base so the peer
/// adopts it (discarding the peer's divergence — that's what "keep mine"
/// means). After the round: the peer materializes the resolver's content
/// exactly (its marker present once, the other's absent), the resolver's
/// content is unchanged, both are bound, and nothing is doubled.
///
/// Because the peer adopts the resolver's EXACT base, both sides now share that
/// lineage. The multi-round discipline that the original keep-mine bug evaded:
/// after resolving we run several MORE rounds AND make a follow-up edit on the
/// resolver, asserting it propagates to the peer exactly once over the now
/// shared lineage (no deferred cross-lineage interleave). [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_keep_mine_resolves_both_sides() {
    use hiker_sync::identity::{LocalDocId, Resolution};

    // Same-region conflicting edits over a shared seed, on independent lineages.
    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: first contact forks; B (the dialer) records it.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let _r1 = b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    let local_b = LocalDocId(doc_b.clone());
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one fork recorded: {blocked:?}");
    let logical = blocked[0].logical_id.clone();

    // The user chooses keep-mine on B (B is the side that dials = the resolver).
    b.set_fork_resolution(logical.clone(), Resolution::KeepMine);

    // Round 2: B dials and pushes its base; A (the responder) adopts it.
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();

    assert!(r2.blocked.is_empty(), "round 2 no longer blocks: {r2:?}");
    assert!(
        r2.converged.iter().any(|l| l == &logical),
        "round 2 converged the resolved doc: {r2:?}"
    );

    // The PEER (A) converged to the resolver's (B's) content: B's marker present
    // exactly once, A's own marker gone.
    let doc_a_text = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(doc_a_text, b_text, "A (peer) adopted B's keep-mine content: {doc_a_text:?}");
    assert_eq!(doc_a_text.matches("edited by B").count(), 1, "B's content once on A");
    assert_eq!(doc_a_text.matches("edited by A").count(), 0, "A's divergence discarded");

    // The resolver's content is unchanged, both bound, block cleared.
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_text, "B unchanged");
    assert_eq!(b.status_of(&local_b), Some(SyncStatus::Bound), "B bound after keep-mine");
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());

    // MULTI-ROUND DISCIPLINE: several more rounds must NOT corrupt (the deferred
    // cross-lineage interleave only surfaces on a later round). Content stable.
    for _ in 0..3 {
        let addr_n = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_n = spawn_responder(a, Duration::from_secs(2));
        let rn = b.sync_once(&addr_n).await.unwrap();
        assert!(rn.blocked.is_empty(), "extra round never re-blocks: {rn:?}");
        a = server_n.await.unwrap();

        let addr_m = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_m = spawn_responder(b, Duration::from_secs(2));
        let rm = a.sync_once(&addr_m).await.unwrap();
        assert!(rm.blocked.is_empty(), "extra round never re-blocks: {rm:?}");
        b = server_m.await.unwrap();
    }
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        b_text,
        "A byte-stable across extra rounds (no deferred interleave)"
    );
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_text, "B byte-stable");

    // FOLLOW-UP EDIT on the resolver propagates to the peer exactly once over the
    // now-shared lineage — proof the push-adopt established a real shared lineage.
    let b_edited = "title\nbody edited by B\nFOLLOWUP by B\n";
    oplog_b.apply_user_text(&doc_b, b_edited).unwrap();
    let addr_e = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_e = spawn_responder(b, Duration::from_secs(3));
    let re = a.sync_once(&addr_e).await.unwrap();
    let _b = server_e.await.unwrap();
    assert!(re.blocked.is_empty(), "follow-up delta not blocked: {re:?}");
    let a_after = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(a_after, b_edited, "A picked up B's follow-up edit: {a_after:?}");
    assert_eq!(
        a_after.matches("FOLLOWUP by B").count(),
        1,
        "follow-up edit appears exactly once (shared lineage, no interleave): {a_after:?}"
    );
}

// --- 4b''. Fork resolution: both sides keep-mine converges deterministically

/// BOTH sides set keep-mine. Whoever pushes first wins: its `PushAdopt` makes
/// the other adopt its base AND clears the other's pending keep-mine (so the
/// adopter doesn't push back). The result converges to ONE version with no
/// duplication and no flapping — content stable across extra rounds. We don't
/// assert WHICH side wins (no strict canonical tiebreak), only that they agree
/// on a single version that contains exactly one of the two markers and is
/// stable. [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_both_keep_mine_converges_deterministically() {
    use hiker_sync::identity::Resolution;

    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: both forks (B dials A; A also forks on its own round below). B
    // records its fork.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    // A forks too (its own dial round) so it has a recorded block + logical id.
    let addr_b0 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b0 = spawn_responder(b, Duration::from_secs(3));
    a.sync_once(&addr_b0).await.unwrap();
    let mut b = server_b0.await.unwrap();

    let logical_b = b.blocked_docs()[0].logical_id.clone();
    let logical_a = a.blocked_docs()[0].logical_id.clone();

    // BOTH set keep-mine.
    a.set_fork_resolution(logical_a, Resolution::KeepMine);
    b.set_fork_resolution(logical_b, Resolution::KeepMine);

    // Drive both directions for a few rounds. The first push wins and clears the
    // other's pending keep-mine, so they converge without flapping.
    for _ in 0..3 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(2));
        let rb = b.sync_once(&addr_a).await.unwrap();
        assert!(rb.blocked.is_empty(), "B round never re-blocks: {rb:?}");
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(2));
        let ra = a.sync_once(&addr_b).await.unwrap();
        assert!(ra.blocked.is_empty(), "A round never re-blocks: {ra:?}");
        b = server_b.await.unwrap();
    }

    // They converged to ONE version: identical text, exactly one of the two
    // markers, the other absent — no duplication.
    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converged to one version: a={text_a:?} b={text_b:?}");
    let winner_is_a = text_a == a_text;
    let winner_is_b = text_a == b_text;
    assert!(winner_is_a || winner_is_b, "converged to one of the two versions: {text_a:?}");
    let total_markers =
        text_a.matches("edited by A").count() + text_a.matches("edited by B").count();
    assert_eq!(total_markers, 1, "exactly one marker present, no duplication: {text_a:?}");
}

// --- 4c. Fork resolution: keep-both preserves a conflict copy --------------

/// A forked doc resolved with `KeepBoth`: the local version is preserved as a
/// sibling conflict-copy note (via the op-log create path, so it's indexed),
/// and the original path adopts the peer's content. Both versions survive.
/// [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_keep_both_preserves_local_copy() {
    use hiker_sync::identity::Resolution;

    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: fork → B blocks.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let logical = b.blocked_docs()[0].logical_id.clone();

    // Resolve keep-both, then round 2.
    b.set_fork_resolution(logical.clone(), Resolution::KeepBoth);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let _a = server2.await.unwrap();

    assert!(r2.blocked.is_empty(), "keep-both no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "blocked record cleared");

    // The original path now holds A's content (keep-theirs half).
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "original path adopted A's content"
    );

    // A sibling conflict copy preserves B's local version, as a real indexed
    // note (created via the op-log create path).
    let copy = oplog_b
        .list_doc_ids()
        .unwrap()
        .into_iter()
        .find(|id| {
            oplog_b
                .path_for_doc(id)
                .unwrap()
                .map(|p| p.contains("conflict"))
                .unwrap_or(false)
        })
        .expect("a conflict-copy note exists");
    assert_eq!(
        oplog_b.materialize_accepted(&copy).unwrap().text,
        b_text,
        "the conflict copy preserves B's local version"
    );
}

// --- 4d. Generic no-corruption guard: extra rounds after any resolution ----

/// The explicit regression net for the whole deferred-cross-lineage-interleave
/// class of bug: for EACH of keep-mine / keep-theirs / keep-both, after the
/// resolution round run 3+ MORE bidirectional rounds and assert the content is
/// byte-stable and the surviving marker(s) appear exactly once. The original
/// keep-mine bug only surfaced on a LATER round (a cross-lineage delta pulled
/// the peer's whole doc and interleaved it), so a single post-resolution
/// assertion would have missed it — this drives the rounds that catch it.
/// [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_resolution_no_corruption_on_extra_rounds() {
    use hiker_sync::identity::Resolution;

    for resolution in [
        Resolution::KeepMine,
        Resolution::KeepTheirs,
        Resolution::KeepBoth,
    ] {
        // B is the dialer / resolver throughout. Same-region conflict on
        // independent lineages.
        let seed = "title\nbody line\n";
        let a_text = "title\nbody edited by A\n";
        let b_text = "title\nbody edited by B\n";

        let key = ContentKey::generate();
        let (mut a, oplog_a, _da) = mk_node(&key);
        let (mut b, oplog_b, _db) = mk_node(&key);
        enroll_each_other(&a, &b);

        let path = "notes/forked.md";
        let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
        let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
        oplog_a.apply_user_text(&doc_a, a_text).unwrap();
        oplog_b.apply_user_text(&doc_b, b_text).unwrap();

        // Round 1: fork; B records it.
        let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server = spawn_responder(a, Duration::from_secs(3));
        b.sync_once(&addr).await.unwrap();
        let mut a = server.await.unwrap();
        let logical = b.blocked_docs()[0].logical_id.clone();

        // The expected single converged text per resolution:
        // - keep-mine  → B's content wins on both sides.
        // - keep-theirs/keep-both → A's content wins at the original path.
        let (expected, marker) = match resolution {
            Resolution::KeepMine => (b_text, "edited by B"),
            _ => (a_text, "edited by A"),
        };

        // Resolution round.
        b.set_fork_resolution(logical.clone(), resolution);
        let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server2 = spawn_responder(a, Duration::from_secs(3));
        let r2 = b.sync_once(&addr2).await.unwrap();
        let mut a = server2.await.unwrap();
        assert!(r2.blocked.is_empty(), "{resolution:?}: resolution round not blocked: {r2:?}");

        // 3+ extra bidirectional rounds — where a deferred interleave would bite.
        for _ in 0..3 {
            let addr_n = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
            let server_n = spawn_responder(a, Duration::from_secs(2));
            let rb = b.sync_once(&addr_n).await.unwrap();
            assert!(rb.blocked.is_empty(), "{resolution:?}: extra round re-blocked: {rb:?}");
            a = server_n.await.unwrap();

            let addr_m = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
            let server_m = spawn_responder(b, Duration::from_secs(2));
            let ra = a.sync_once(&addr_m).await.unwrap();
            assert!(ra.blocked.is_empty(), "{resolution:?}: extra round re-blocked: {ra:?}");
            b = server_m.await.unwrap();
        }

        // Both sides byte-stable at the expected single text, marker once.
        let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
        let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
        assert_eq!(text_a, expected, "{resolution:?}: A == expected: {text_a:?}");
        assert_eq!(text_b, expected, "{resolution:?}: B == expected: {text_b:?}");
        assert_eq!(
            text_a.matches(marker).count(),
            1,
            "{resolution:?}: marker appears exactly once on A: {text_a:?}"
        );
        assert_eq!(
            text_b.matches(marker).count(),
            1,
            "{resolution:?}: marker appears exactly once on B: {text_b:?}"
        );
    }
}

// --- 5. Rename safety (headline scenario) ---------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_does_not_fork_identity() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let old_path = "notes/foo.md";
    let new_path = "notes/bar.md";
    let seed = "rename me\nbody line\n";
    let doc_a = oplog_a.create_document(old_path, "note", seed, &Author::User).unwrap();

    // Round 0: bind A↔B on notes/foo.md (B adopts A's base).
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();

    let doc_b = oplog_b.doc_id_for_path(old_path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);
    let logical_for_b = b
        .bindings()
        .logical_for(&hiker_sync::identity::LocalDocId(doc_b.clone()))
        .cloned()
        .expect("B bound the doc");

    // A renames foo -> bar (a meta.path op on the shared lineage). B, still at
    // the old path, makes a body edit.
    oplog_a.rename_document(&doc_a, new_path, &Author::User).unwrap();
    oplog_b
        .apply_user_text(&doc_b, "rename me\nbody line\nB's addition\n")
        .unwrap();

    // Sync: B pulls A's delta (which carries the rename) over the shared
    // lineage. Identity is the binding, so B does NOT mint a fresh doc for the
    // new path — the rename rides the same logical id.
    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr1).await.unwrap();
    let _a = server1.await.unwrap();

    assert!(report.blocked.is_empty(), "rename must not fork/block: {report:?}");

    // B's same local doc now reports the NEW path (the rename op applied to its
    // Yrs meta.path) — identity survived the rename, no second doc minted.
    assert_eq!(
        oplog_b.path_for_doc(&doc_b).unwrap().as_deref(),
        Some(new_path),
        "B's existing doc moved to the new path — identity is the binding"
    );
    // The binding is unchanged: still the one logical id.
    assert_eq!(
        b.bindings()
            .logical_for(&hiker_sync::identity::LocalDocId(doc_b.clone()))
            .cloned(),
        Some(logical_for_b),
        "B kept the same logical identity across the rename"
    );
    // B's own edit is still present (the rename didn't clobber the body).
    assert!(
        oplog_b
            .materialize_accepted(&doc_b)
            .unwrap()
            .text
            .contains("B's addition"),
        "B's edit survived the converge"
    );
    // No phantom second doc was minted at the new path.
    let all_docs = oplog_b.list_doc_ids().unwrap();
    assert_eq!(all_docs.len(), 1, "exactly one logical doc on B: {all_docs:?}");
}

// --- 6. Server-mediated store-and-forward, zero-knowledge -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_store_and_forward_zero_knowledge() {
    use hiker_sync::identity::{LocalDocId, LogicalId};

    let key = ContentKey::generate();
    let logical = LogicalId(ulid::Ulid::new().to_string());
    let path = "notes/hub.md";

    let kp_server = DeviceKeypair::generate();
    let fp_server = kp_server.fingerprint();

    // The hub: file-backed, both clients enrolled.
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    a.enroll_peer(fp_server.clone()).unwrap();
    b.enroll_peer(fp_server.clone()).unwrap();

    let server_dir = tempfile::tempdir().unwrap();
    let server_data = server_dir.path().to_path_buf();
    let mut server = Hub::new(
        kp_server,
        &server_data,
        vec![a.fingerprint(), b.fingerprint()],
    )
    .unwrap();
    let server_addr = server.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();

    // A holds a doc with content; B shares the lineage (a prior P2P bind/adopt
    // stand-in, exactly as the server path assumes — the hub can't negotiate
    // ids on ciphertext). Both are bound to the one logical id.
    let doc_a = oplog_a.create_document(path, "note", "alpha\n", &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, "alpha\nbeta\n").unwrap();
    a.bind_for_test(LocalDocId(doc_a.clone()), logical.clone());

    let doc_b = oplog_b.create_document(path, "note", "", &Author::User).unwrap();
    oplog_b.adopt_lineage(&doc_b, &oplog_a.export_state(&doc_a).unwrap()).unwrap();
    b.bind_for_test(LocalDocId(doc_b.clone()), logical.clone());

    // A makes an offline edit B has never seen.
    oplog_a.apply_user_text(&doc_a, "alpha\nbeta\nGAMMA marker\n").unwrap();
    let want = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(10)).await.unwrap() });

    // A pushes while B is "offline" (not connected). Then B pulls and converges.
    let ra = a.sync_via_server(&server_addr).await.unwrap();
    assert!(ra.bound.contains(&logical), "A pushed: {ra:?}");
    let rb = b.sync_via_server(&server_addr).await.unwrap();
    assert!(rb.converged.contains(&logical), "B converged via the hub: {rb:?}");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, want);

    serve.abort();

    // Zero-knowledge: re-open the on-disk store; stored bytes are ciphertext.
    let store = FileBlobStore::open(&server_data).unwrap();
    let blind = blind_id(&key, &logical.0);
    let stored = store.pull(&blind, 0);
    assert!(!stored.is_empty(), "the hub stored A's pushed blob(s)");
    let plaintext_marker = b"GAMMA marker";
    for (_seq, ciphertext) in &stored {
        assert!(
            !ciphertext.windows(plaintext_marker.len()).any(|w| w == plaintext_marker),
            "stored blob must not contain plaintext"
        );
        assert!(key.decrypt(ciphertext).is_ok(), "the content key decrypts it");
        let wrong = ContentKey::from_bytes([0u8; 32]);
        assert!(wrong.decrypt(ciphertext).is_err(), "a wrong key cannot");
    }
}

// --- 7. Enrollment gate ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unenrolled_peer_cannot_sync() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);

    // Only A enrolls B; B does NOT enroll A (no bidirectional swap). A will drop
    // B's connection as a non-enrolled peer, and B's connect() rejects A too.
    a.enroll_peer(b.fingerprint()).unwrap();

    let path = "notes/secret.md";
    oplog_a
        .create_document(path, "note", "confidential\n", &Author::User)
        .unwrap();

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(4));

    // B's dialer checks the connected peer against its (empty) enrolled set and
    // refuses — no sync occurs.
    let result = b.sync_once(&addr).await;
    assert!(
        result.is_err(),
        "an un-swapped peer must not sync: {result:?}"
    );

    // B's vault stayed empty — nothing converged.
    assert!(
        oplog_b.doc_id_for_path(path).unwrap().is_none(),
        "B did not receive the doc"
    );

    server.abort();
}

// --- 8. Multi-document session --------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_document_session() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let docs = [
        ("notes/one.md", "first doc body\n"),
        ("notes/two.md", "second doc body\n"),
        ("notes/three.md", "third doc body\n"),
    ];
    for (p, text) in &docs {
        oplog_a.create_document(p, "note", text, &Author::User).unwrap();
    }

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));

    let report = b.sync_once(&addr).await.unwrap();
    assert_eq!(report.converged.len(), 3, "all three converged: {report:?}");
    assert!(report.blocked.is_empty(), "nothing blocked: {report:?}");
    server.abort();

    // All three texts landed on B.
    for (p, text) in &docs {
        let doc_b = oplog_b
            .doc_id_for_path(p)
            .unwrap()
            .unwrap_or_else(|| panic!("B missing {p}"));
        assert_eq!(
            oplog_b.materialize_accepted(&doc_b).unwrap().text,
            *text,
            "doc {p} converged"
        );
    }
}

// --- 9. (Stretch) Three-device transitivity -------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn three_device_transitivity() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    let (mut c, oplog_c, _dc) = mk_node(&key);
    enroll_each_other(&a, &b);
    enroll_each_other(&b, &c);

    let path = "notes/relayed.md";
    let want = "content that must reach C via B\n";
    oplog_a.create_document(path, "note", want, &Author::User).unwrap();

    // A↔B: B pulls A's doc.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr_a).await.unwrap();
    assert_eq!(rb.converged.len(), 1, "B got A's doc: {rb:?}");
    let _a = server_a.await.unwrap();

    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, want);

    // B↔C: C pulls from B (which now holds A's content).
    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(3));
    let rc = c.sync_once(&addr_b).await.unwrap();
    assert_eq!(rc.converged.len(), 1, "C got the doc via B: {rc:?}");
    let _b = server_b.await.unwrap();

    let doc_c = oplog_c.doc_id_for_path(path).unwrap().unwrap();
    assert_eq!(
        oplog_c.materialize_accepted(&doc_c).unwrap().text,
        want,
        "C converged to A's content transitively through B"
    );
}

// --- discovery API surface (mDNS multicast is unreliable in sandboxes) -----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_window_is_time_boxed_and_opt_in() {
    let key = ContentKey::generate();

    // Discovery disabled → returns immediately with no candidates, regardless
    // of the window. [sync-mdns-discovery]
    let (dir, oplog) = open_vault();
    let mut off = SyncNode::new(
        Arc::clone(&oplog),
        SharedContentKey::new(ContentKey::from_bytes(*key.as_bytes())),
        DeviceKeypair::generate(),
        Settings { discovery: false, ..Settings::default() },
        EnrolledPeers::new(),
    );
    let found = off.start_discovery(Duration::from_secs(5)).await.unwrap();
    assert!(found.is_empty(), "disabled discovery yields nothing");
    drop(dir);

    // Discovery enabled → the window is bounded; it returns within the deadline
    // even when no enrolled peer is found (multicast can't be relied on in a
    // sandbox, so we assert the time-box, not convergence — convergence is
    // driven by explicit dials in the other scenarios).
    let (_dir2, oplog2) = open_vault();
    let mut on = SyncNode::new(
        Arc::clone(&oplog2),
        SharedContentKey::new(ContentKey::from_bytes(*key.as_bytes())),
        DeviceKeypair::generate(),
        Settings::default(),
        EnrolledPeers::new(),
    );
    let started = std::time::Instant::now();
    let found = on.start_discovery(Duration::from_millis(700)).await.unwrap();
    let elapsed = started.elapsed();
    // Only enrolled peers ever appear; none enrolled here.
    assert!(found.is_empty(), "no enrolled candidate on loopback");
    assert!(
        elapsed < Duration::from_secs(5),
        "discovery window honored its deadline: {elapsed:?}"
    );
}

// --- 10. Automatic in-band content-key transfer ----------------------------

/// Build a [`SyncNode`] over a fresh temp vault with an EXPLICIT content key
/// (so two nodes can start with DIFFERENT keys). Returns the node, its oplog,
/// its temp dir, and the shared content-key handle (so the test can read the
/// adopted key's fingerprint after a round). Mirrors [`mk_node`] but exposes the
/// handle and lets the caller pick the key.
fn mk_node_with_handle(
    content_key: ContentKey,
) -> (SyncNode, Arc<OpLog>, tempfile::TempDir, SharedContentKey) {
    let (dir, oplog) = open_vault();
    let handle = SharedContentKey::new(content_key);
    let node = SyncNode::new(
        Arc::clone(&oplog),
        handle.clone(),
        DeviceKeypair::generate(),
        Settings::default(),
        EnrolledPeers::new(),
    );
    (node, oplog, dir, handle)
}

/// Two mutually-enrolled nodes start with DIFFERENT content keys but the SAME
/// document content. Before this feature a content-encrypted delta fails to
/// decrypt across the two keys ("reached peer but content didn't decrypt"); the
/// in-band transfer makes the non-canonical side adopt the canonical device's
/// key over the authenticated channel during the Hello exchange, so both end up
/// holding ONE key and the delta path converges. [sync-vault-key-inband]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_key_auto_transfers_on_first_contact() {
    // DISTINCT content keys — the pre-feature failure condition.
    let key_a = ContentKey::from_bytes([1u8; 32]);
    let key_b = ContentKey::from_bytes([2u8; 32]);
    assert_ne!(
        key_a.fingerprint(),
        key_b.fingerprint(),
        "the two devices start with different keys"
    );

    let (mut a, oplog_a, _da, handle_a) = mk_node_with_handle(key_a);
    let (mut b, oplog_b, _db, handle_b) = mk_node_with_handle(key_b);
    enroll_each_other(&a, &b);

    // Same document content on both, seeded independently.
    let path = "notes/keyed.md";
    let text = "alpha line\nbeta line\n";
    let doc_a = oplog_a.create_document(path, "note", text, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", text, &Author::User).unwrap();

    // Drive a full bidirectional sync to steady state. The in-band key transfer
    // runs in the Hello phase of each `sync_once`; whichever side is
    // non-canonical adopts the canonical device's key when it dials. Run twice so
    // either canonical ordering fully settles the key + lineage handshake.
    for _ in 0..2 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        b.sync_once(&addr_a).await.unwrap();
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();
    }

    // THE ASSERTION: both devices now hold the SAME content key (the
    // non-canonical side adopted the canonical device's key in-band).
    assert_eq!(
        handle_a.fingerprint(),
        handle_b.fingerprint(),
        "both devices converged on one content key"
    );

    // And a subsequent edit's content-encrypted delta decrypts + converges (no
    // "didn't decrypt" error, content not doubled).
    let edited = "alpha line\nbeta line\ngamma line\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a2 = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr_a2).await.expect("delta round must not error");
    let _a = server_a2.await.unwrap();

    assert!(report.blocked.is_empty(), "delta not blocked: {report:?}");
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, edited, "B picked up A's edit: {got_b:?}");
    assert_eq!(
        got_b.matches("gamma line").count(),
        1,
        "the edited line appears exactly once (no duplication): {got_b:?}"
    );
}

/// Two nodes that ALREADY share one content key see matching content-key
/// fingerprints in the Hello exchange, so the in-band transfer is a no-op: the
/// key never changes on either side. [sync-vault-key-inband]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matching_content_keys_no_transfer() {
    let shared = ContentKey::from_bytes([7u8; 32]);
    let fp_before = shared.fingerprint();

    let (mut a, oplog_a, _da, handle_a) = mk_node_with_handle(
        ContentKey::from_bytes(*shared.as_bytes()),
    );
    let (mut b, _oplog_b, _db, handle_b) = mk_node_with_handle(
        ContentKey::from_bytes(*shared.as_bytes()),
    );
    enroll_each_other(&a, &b);

    let path = "notes/same-key.md";
    oplog_a.create_document(path, "note", "shared\n", &Author::User).unwrap();

    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a).await.unwrap();
    let _a = server_a.await.unwrap();

    // Neither side's key changed — the fingerprints matched, so no key was
    // requested or swapped.
    assert_eq!(handle_a.fingerprint(), fp_before, "A's key unchanged");
    assert_eq!(handle_b.fingerprint(), fp_before, "B's key unchanged");
}

// === Wave 6 — adversarial data-corruption / data-loss probes ===============
//
// Each names a way sync could corrupt or lose data: a synced rename creating a
// duplicate doc, a tombstone failing to propagate, a mid-session create
// duplicating, same-region concurrent edits losing content, idempotent server
// re-pulls doubling, edit-while-forked then resolving with stale content, a
// re-fork after a prior resolution, and byte-exact round-trips of tricky
// content (empty / no-newline / CRLF / BOM / multibyte). Every state change is
// followed by extra rounds + byte-stability re-asserts (deferred cross-lineage
// corruption only shows on a later round), and correctness uses BOTH exact
// `materialize_accepted` equality AND `matches(marker).count() == 1`.

/// Drive a full bidirectional sync to steady state. Runs `rounds` B→A then A→B
/// pairs; asserts neither side ever blocks. Consumes and returns the two nodes
/// so the caller keeps driving.
async fn drive_bidirectional(
    mut a: SyncNode,
    mut b: SyncNode,
    rounds: usize,
) -> (SyncNode, SyncNode) {
    for _ in 0..rounds {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr_a).await.unwrap();
        assert!(rb.blocked.is_empty(), "B round blocked: {rb:?}");
        a = server_a.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        let ra = a.sync_once(&addr_b).await.unwrap();
        assert!(ra.blocked.is_empty(), "A round blocked: {ra:?}");
        b = server_b.await.unwrap();
    }
    (a, b)
}

// --- W1. Synced rename does not duplicate or fork --------------------------

/// A & B share a lineage for `notes/a.md`. A renames it to `notes/b.md` AND
/// edits. After sync B must have the doc at the NEW path with the content, NOT
/// still hold the old path, hold exactly ONE doc, and crucially
/// `doc_id_for_path("notes/b.md")` must resolve to that same doc.
///
/// THE BUG this guards: `apply_remote_update` applied the rename to the Yrs
/// `meta.path` and rewrote the `.md`, but did NOT repoint B's `doc-index.db`.
/// So `path_for_doc` (reads the Yrs Doc) reported the new path while
/// `doc_id_for_path(new_path)` (reads the index) returned `None`. A later
/// manifest path-match on the new path would then mint a SECOND doc for the
/// same content — content duplication. Fixed by repointing the path index in
/// the receive path when a synced op moves `meta.path`. We re-run several
/// rounds so any deferred second-doc minting would surface. [sync-path-matching-key]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synced_rename_does_not_duplicate_or_fork() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let old_path = "notes/a.md";
    let new_path = "notes/b.md";
    let seed = "shared head\nMARKER body\nshared tail\n";
    let doc_a = oplog_a.create_document(old_path, "note", seed, &Author::User).unwrap();

    // Round 0: B adopts A's base — now a shared lineage at notes/a.md.
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    let r0 = b.sync_once(&addr0).await.unwrap();
    assert_eq!(r0.converged.len(), 1, "shared lineage established: {r0:?}");
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(old_path).unwrap().unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);

    // A renames a.md -> b.md AND edits the body (a meta.path op + a text op on
    // the shared lineage, both ride the same delta).
    oplog_a.rename_document(&doc_a, new_path, &Author::User).unwrap();
    let edited = "shared head\nMARKER body\nshared tail\nRENAME-EDIT line\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    // B pulls the delta carrying the rename + edit over the shared lineage.
    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr1).await.unwrap();
    assert!(r1.blocked.is_empty(), "rename must not fork/block: {r1:?}");
    let a = server1.await.unwrap();

    // B's existing doc moved to the new path (Yrs meta.path), with the content.
    assert_eq!(
        oplog_b.path_for_doc(&doc_b).unwrap().as_deref(),
        Some(new_path),
        "B's doc reports the new path"
    );
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, edited, "B has the renamed doc's content exactly: {got_b:?}");
    assert_eq!(got_b.matches("RENAME-EDIT line").count(), 1, "edit once: {got_b:?}");

    // THE ASSERTION the bug failed: the path INDEX resolves the new path to the
    // SAME doc (so a later manifest match reuses it, never minting a second).
    assert_eq!(
        oplog_b.doc_id_for_path(new_path).unwrap().as_deref(),
        Some(doc_b.as_str()),
        "doc_id_for_path(new_path) resolves to B's same doc (index repointed)"
    );
    // B does NOT still resolve the old path to this doc.
    assert!(
        oplog_b.doc_id_for_path(old_path).unwrap().is_none(),
        "B no longer maps the old path"
    );
    // Exactly ONE doc on B — no duplicate minted.
    assert_eq!(
        oplog_b.list_doc_ids().unwrap().len(),
        1,
        "exactly one doc on B (no duplicate from a stale path index)"
    );

    // Drive several more rounds: a deferred second-doc minting (the bug's late
    // form) would appear on a later round when B re-classifies the new path.
    let (a2, b2) = drive_bidirectional(a, b, 3).await;
    let _a = a2;
    let _b = b2;
    assert_eq!(
        oplog_b.list_doc_ids().unwrap().len(),
        1,
        "still exactly one doc on B after extra rounds"
    );
    let final_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(final_b, edited, "B byte-stable after extra rounds: {final_b:?}");
    assert_eq!(final_b.matches("RENAME-EDIT line").count(), 1, "no doubling: {final_b:?}");
}

// --- W2a. Tombstone propagates over a shared lineage -----------------------

/// A & B share a lineage. A tombstones the doc. After sync B's
/// `materialize_accepted` reports the tombstone — the delete propagated, no
/// corruption. Extra rounds keep it tombstoned (no resurrection). [sync-content-encryption-aes256]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstone_propagates_over_shared_lineage() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/doomed.md";
    let seed = "content to delete\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    // Round 0: shared lineage.
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();
    assert!(!oplog_b.materialize_accepted(&doc_b).unwrap().tombstone, "B alive at first");

    // A tombstones; B pulls the delta.
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    assert!(oplog_a.materialize_accepted(&doc_a).unwrap().tombstone, "A tombstoned");

    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr1).await.unwrap();
    assert!(r1.blocked.is_empty(), "tombstone delta not blocked: {r1:?}");
    let a = server1.await.unwrap();

    assert!(
        oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B sees the tombstone after the synced delete"
    );

    // Extra rounds: the tombstone is byte-stable (no resurrection, no second doc).
    let (_a, _b) = drive_bidirectional(a, b, 3).await;
    assert!(
        oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B stays tombstoned across extra rounds"
    );
    assert_eq!(oplog_b.list_doc_ids().unwrap().len(), 1, "no duplicate doc on B");
}

// --- W2b. Delete-vs-edit: defined, non-corrupting outcome ------------------

/// A tombstones a shared doc WHILE B edits the same doc (concurrent). After a
/// bidirectional sync both sides must reach a DEFINED, identical state with no
/// silent duplication or interleave. We assert convergence (both sides equal)
/// and document the outcome: the tombstone is a `meta.tombstone=true` CRDT op
/// and B's edit is a `text` CRDT op — disjoint Yrs fields, so they merge
/// commutatively. The defined result is "tombstoned" (the delete wins as the
/// meta flag), with B's text still present underneath but the doc reading as
/// deleted. The invariant under test is convergence + no corruption, not which
/// of delete/edit "wins". [sync-content-encryption-aes256, sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_converges_without_corruption() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/conflict-delete.md";
    let seed = "base line\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    // Round 0: shared lineage.
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();

    // Concurrent: A tombstones, B edits the body (different Yrs fields).
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    oplog_b.apply_user_text(&doc_b, "base line\nB-EDIT marker\n").unwrap();

    // Bidirectional sync to settle the merge.
    let (a, b) = drive_bidirectional(a, b, 3).await;
    let _a = a;
    let _b = b;

    // DEFINED OUTCOME: both sides converge to the same materialized state.
    let mat_a = oplog_a.materialize_accepted(&doc_a).unwrap();
    let mat_b = oplog_b.materialize_accepted(&doc_b).unwrap();
    assert_eq!(mat_a.tombstone, mat_b.tombstone, "both agree on tombstone flag");
    assert_eq!(mat_a.text, mat_b.text, "both converge to one text (no interleave)");
    // The delete wins as the meta flag (the documented outcome).
    assert!(mat_a.tombstone, "delete-vs-edit resolves to tombstoned");
    // No duplication: B's edit appears at most once (not interleaved twice).
    assert!(
        mat_b.text.matches("B-EDIT marker").count() <= 1,
        "B's edit is not duplicated: {:?}",
        mat_b.text
    );
    // Exactly one doc per side — neither minted a phantom.
    assert_eq!(oplog_a.list_doc_ids().unwrap().len(), 1, "one doc on A");
    assert_eq!(oplog_b.list_doc_ids().unwrap().len(), 1, "one doc on B");
}

// --- W3. New doc created mid-session propagates exactly once ---------------

/// A & B already syncing a doc. A creates a BRAND-NEW note after the first
/// sync. On the next sync it propagates to B via the None/create+adopt path,
/// exactly once, no fork, no duplicate, content exact — and B then holds two
/// docs total (the original + the new one). Extra rounds keep it stable. [sync-lineage-adoption]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_doc_mid_session_propagates_once() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let first_path = "notes/first.md";
    let doc_a1 = oplog_a.create_document(first_path, "note", "first body\n", &Author::User).unwrap();

    // Round 0: B converges the first doc.
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();
    assert!(oplog_b.doc_id_for_path(first_path).unwrap().is_some(), "B has first doc");

    // A creates a brand-new note mid-session.
    let second_path = "notes/second.md";
    let new_text = "FRESH body\nsecond line\n";
    let _doc_a2 = oplog_a.create_document(second_path, "note", new_text, &Author::User).unwrap();

    // B pulls: the new doc is None-on-B → create+adopt, exactly once.
    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr1).await.unwrap();
    assert!(r1.blocked.is_empty(), "new doc not blocked: {r1:?}");
    let a = server1.await.unwrap();

    let doc_b2 = oplog_b
        .doc_id_for_path(second_path)
        .unwrap()
        .expect("B received the new doc");
    let got = oplog_b.materialize_accepted(&doc_b2).unwrap().text;
    assert_eq!(got, new_text, "B has the new doc's content exactly: {got:?}");
    assert_eq!(got.matches("FRESH body").count(), 1, "new content once: {got:?}");
    assert_eq!(oplog_b.list_doc_ids().unwrap().len(), 2, "B holds exactly two docs");

    // Extra rounds: no duplicate doc minted for the new path on a later round.
    let _ = doc_a1;
    let (_a, _b) = drive_bidirectional(a, b, 3).await;
    assert_eq!(oplog_b.list_doc_ids().unwrap().len(), 2, "still two docs on B");
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b2).unwrap().text,
        new_text,
        "new doc byte-stable"
    );
}

// --- W4a. Concurrent disjoint-region edits on a shared lineage -------------

/// After A & B share a lineage, disjoint-region edits on each survive once on
/// both sides (CRDT positional merge). Extra rounds keep it byte-stable. This
/// complements `post_convergence_disjoint_edits_appear_once` by driving 3 extra
/// rounds after convergence to catch deferred interleave. [sync-content-encryption-aes256]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_lineage_disjoint_edits_survive_once_stable() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/disjoint-stable.md";
    let seed = "HEAD seed\nmiddle\nTAIL seed\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();

    oplog_a.apply_user_text(&doc_a, "HEAD AAA-mark\nmiddle\nTAIL seed\n").unwrap();
    oplog_b.apply_user_text(&doc_b, "HEAD seed\nmiddle\nTAIL BBB-mark\n").unwrap();

    // Several bidirectional rounds — converge then keep stable.
    let (a, b) = drive_bidirectional(a, b, 4).await;
    let _a = a;
    let _b = b;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "converge to one merged text: a={text_a:?} b={text_b:?}");
    for t in [&text_a, &text_b] {
        assert!(t.contains("AAA-mark"), "A's edit present: {t:?}");
        assert!(t.contains("BBB-mark"), "B's edit present: {t:?}");
        assert_eq!(t.matches("AAA-mark").count(), 1, "A's edit once: {t:?}");
        assert_eq!(t.matches("BBB-mark").count(), 1, "B's edit once: {t:?}");
    }
}

// --- W4b. Concurrent SAME-region edits on a shared lineage -----------------

/// After A & B share a lineage, SAME-region edits on each converge
/// deterministically with no content loss and no duplication. Because the
/// lineage is shared this is a CRDT merge (NOT a fork) — Yrs orders the two
/// concurrent inserts by client id, so the result contains BOTH inserted
/// markers exactly once and both sides agree. Extra rounds keep it byte-stable.
/// [sync-content-encryption-aes256]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_lineage_same_region_edits_converge() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/same-region.md";
    let seed = "alpha\nINSERT-HERE\nomega\n";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();

    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();

    // Both edit the SAME line region with distinct markers.
    oplog_a.apply_user_text(&doc_a, "alpha\nINSERT-HERE AAA-side\nomega\n").unwrap();
    oplog_b.apply_user_text(&doc_b, "alpha\nINSERT-HERE BBB-side\nomega\n").unwrap();

    let (a, b) = drive_bidirectional(a, b, 4).await;
    let _a = a;
    let _b = b;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    // Converged: both sides identical (shared lineage = CRDT merge, no fork).
    assert_eq!(text_a, text_b, "same-region edits converge: a={text_a:?} b={text_b:?}");
    // No content loss: both concurrent inserts survive exactly once.
    assert_eq!(text_a.matches("AAA-side").count(), 1, "A's insert once: {text_a:?}");
    assert_eq!(text_a.matches("BBB-side").count(), 1, "B's insert once: {text_a:?}");
    // The shared prefix/suffix are not doubled.
    assert_eq!(text_a.matches("alpha").count(), 1, "prefix once: {text_a:?}");
    assert_eq!(text_a.matches("omega").count(), 1, "suffix once: {text_a:?}");
}

// --- W6. Edit-while-forked, then resolve uses current content --------------

/// A doc forks; the user edits THEIR (B's) side AGAIN while it's blocked; then
/// resolves. For keep-theirs B converges to A's content (its own later edit
/// discarded — that's keep-theirs). For keep-mine B's CURRENT (post-block) edit
/// becomes canonical and A adopts it. Both run extra rounds + assert no
/// duplication, proving the resolution used the CURRENT content, not a stale
/// snapshot. [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_while_forked_then_keep_theirs() {
    use hiker_sync::identity::{LocalDocId, Resolution};

    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: fork.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let logical = b.blocked_docs()[0].logical_id.clone();

    // B edits AGAIN while blocked (stale-snapshot trap: a resolution must use
    // this current content, not the content at block time).
    oplog_b.apply_user_text(&doc_b, "title\nbody edited by B\nB-AGAIN while blocked\n").unwrap();

    // keep-theirs: B discards its branch (incl. the while-blocked edit) and
    // adopts A's content.
    b.set_fork_resolution(logical.clone(), Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "resolution not blocked: {r2:?}");

    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "keep-theirs adopts A's content, discarding B's while-blocked edit"
    );
    assert_eq!(b.status_of(&LocalDocId(doc_b.clone())), Some(SyncStatus::Bound));

    // Extra rounds: byte-stable, A's marker once, B's discarded edit absent.
    let (_a, _b) = drive_bidirectional(a, b, 3).await;
    let final_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(final_b, a_text, "B byte-stable on A's content: {final_b:?}");
    assert_eq!(final_b.matches("edited by A").count(), 1, "A marker once: {final_b:?}");
    assert_eq!(final_b.matches("B-AGAIN").count(), 0, "while-blocked edit discarded: {final_b:?}");
}

/// Same setup as above but keep-MINE: B's CURRENT (post-block) edit is the one
/// pushed to A. Proves the resolver pushes its live `export_state`, not a stale
/// snapshot. [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_while_forked_then_keep_mine_uses_current_content() {
    use hiker_sync::identity::Resolution;

    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: fork.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let logical = b.blocked_docs()[0].logical_id.clone();

    // B edits AGAIN while blocked — this CURRENT content is what keep-mine pushes.
    let b_current = "title\nbody edited by B\nB-CURRENT after block\n";
    oplog_b.apply_user_text(&doc_b, b_current).unwrap();

    b.set_fork_resolution(logical.clone(), Resolution::KeepMine);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "resolution not blocked: {r2:?}");

    // A adopted B's CURRENT content (incl. the while-blocked edit), once.
    let got_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(got_a, b_current, "A adopted B's CURRENT content: {got_a:?}");
    assert_eq!(got_a.matches("B-CURRENT after block").count(), 1, "current edit once: {got_a:?}");
    assert_eq!(got_a.matches("edited by A").count(), 0, "A's divergence discarded");

    // Extra rounds: byte-stable on both sides.
    let (_a, _b) = drive_bidirectional(a, b, 3).await;
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, b_current, "A stable");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_current, "B stable");
}

// --- W7. Re-edit after a keep-theirs resolution is a CRDT merge ------------

/// keep-theirs resolves a fork → B adopts A's lineage, so AFTERWARD they SHARE a
/// lineage. Then BOTH edit divergently again. This must NOT re-fork or corrupt:
/// because the lineage is now shared, the subsequent divergent edits are a CRDT
/// merge (converged), not a new fork. We assert convergence + no duplication +
/// byte-stability across extra rounds. [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refork_after_keep_theirs_is_crdt_merge() {
    use hiker_sync::identity::Resolution;

    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: fork; round 2: keep-theirs resolves it → shared lineage on A's base.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let logical = b.blocked_docs()[0].logical_id.clone();
    b.set_fork_resolution(logical.clone(), Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr2).await.unwrap();
    let a = server2.await.unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B on A's content");

    // NOW both edit divergently again. Post-keep-theirs the lineage is shared,
    // so this is a CRDT merge (disjoint regions here), NOT a new fork.
    oplog_a.apply_user_text(&doc_a, "title\nbody edited by A\nA-REFORK tail\n").unwrap();
    oplog_b.apply_user_text(&doc_b, "B-REFORK head\ntitle\nbody edited by A\n").unwrap();

    let (a, b) = drive_bidirectional(a, b, 4).await;
    let _a = a;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "re-edits converge (shared lineage merge): a={text_a:?} b={text_b:?}");
    assert!(b.blocked_docs().is_empty(), "no new fork recorded on B");
    for t in [&text_a, &text_b] {
        assert_eq!(t.matches("A-REFORK tail").count(), 1, "A re-edit once: {t:?}");
        assert_eq!(t.matches("B-REFORK head").count(), 1, "B re-edit once: {t:?}");
    }
}

// --- W8. Byte-exact seed/sync round-trip of tricky content -----------------

/// The "everything forks because of a uniform normalization difference" guard.
/// For each tricky body — empty, no trailing newline, CRLF, leading BOM, and
/// multibyte unicode — two vaults seeded with the IDENTICAL bytes must classify
/// Identical (bind, NOT fork), and a synced edit round-trips byte-exactly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_exact_tricky_content_round_trips() {
    let cases: &[(&str, &str, &str)] = &[
        ("notes/empty.md", "", "ADDED after empty\n"),
        ("notes/no-newline.md", "no trailing newline", "no trailing newline\nNOW with one\n"),
        ("notes/crlf.md", "line one\r\nline two\r\n", "line one\r\nline two\r\nCRLF three\r\n"),
        ("notes/bom.md", "\u{feff}leading bom line\n", "\u{feff}leading bom line\nBOM edit\n"),
        ("notes/unicode.md", "café — naïve 日本語 🦀\n", "café — naïve 日本語 🦀\nMORE 漢字 ✅\n"),
    ];

    for (path, body, edited) in cases {
        let key = ContentKey::generate();
        let (a, oplog_a, _da) = mk_node(&key);
        let (b, oplog_b, _db) = mk_node(&key);
        enroll_each_other(&a, &b);

        // IDENTICAL bytes, seeded independently on each side.
        let doc_a = oplog_a.create_document(path, "note", body, &Author::User).unwrap();
        let doc_b = oplog_b.create_document(path, "note", body, &Author::User).unwrap();
        // Sanity: both materialize the EXACT seed bytes (no normalization).
        assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, *body, "A exact seed {path}");
        assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, *body, "B exact seed {path}");

        // Two bidirectional rounds to settle canonical/adoption + bind. Identical
        // content must classify Identical and bind, NEVER fork.
        let (a, b) = drive_bidirectional(a, b, 2).await;

        // Neither side forked or doubled — content is still the exact seed.
        assert_eq!(
            oplog_a.materialize_accepted(&doc_a).unwrap().text,
            *body,
            "A byte-exact (no fork, no doubling) for {path}"
        );
        assert_eq!(
            oplog_b.materialize_accepted(&doc_b).unwrap().text,
            *body,
            "B byte-exact (no fork, no doubling) for {path}"
        );

        // A synced edit round-trips byte-exactly to B.
        oplog_a.apply_user_text(&doc_a, edited).unwrap();
        let mut a = a;
        let mut b = b;
        let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server = spawn_responder(a, Duration::from_secs(3));
        let r = b.sync_once(&addr).await.unwrap();
        assert!(r.blocked.is_empty(), "edit not blocked for {path}: {r:?}");
        let _a = server.await.unwrap();

        let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
        assert_eq!(got_b, *edited, "B round-trips the edit byte-exactly for {path}: {got_b:?}");
        let _ = b;
    }
}
