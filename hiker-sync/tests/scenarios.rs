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
//! 2. disjoint concurrent edits converge — sync-content-encryption-aes256 + sync-three-way-merge
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
/// DISJOINT lineages — the `cp -r` + `.hiker` deleted-then-reseeded shape.
/// Their watermarks are meaningless to each other, so a cross-lineage delta
/// (`export_since(our_watermark)`) returns the peer's WHOLE doc and applying it would
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

    // Both replicas carry BOTH disjoint edits (text 3-way merge).
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
        b.status_of_path(path),
        Some(SyncStatus::Blocked),
        "B's doc is Blocked"
    );
    let _ = &doc_b;
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
    let _ = LocalDocId(doc_b.clone()); // (sanity ref; status keyed by path now)
    assert_eq!(
        b.status_of_path(path),
        Some(SyncStatus::Blocked),
        "B is blocked after round 1"
    );
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one persistent blocked entry: {blocked:?}");
    let fork_path = blocked[0].path.clone();
    assert_eq!(fork_path, path, "blocked record carries the path");
    assert_eq!(
        blocked[0].peer_fingerprint,
        a.fingerprint(),
        "blocked record names the peer we forked against"
    );

    // The user chooses keep-theirs on B for that path.
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepTheirs);

    // Round 2: the fork branch consumes the decision and adopts A's lineage.
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let _a = server2.await.unwrap();

    assert!(r2.blocked.is_empty(), "round 2 no longer blocks: {r2:?}");
    assert!(
        r2.converged.iter().any(|p| p == &fork_path),
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
/// that dials) makes ITS version canonical and PUSHES its canonical text so the peer
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

    let _ = LocalDocId(doc_b.clone());
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one fork recorded: {blocked:?}");
    let fork_path = blocked[0].path.clone();

    // The user chooses keep-mine on B (B is the side that dials = the resolver).
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepMine);

    // Round 2: B dials and pushes its base; A (the responder) adopts it.
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();

    assert!(r2.blocked.is_empty(), "round 2 no longer blocks: {r2:?}");
    assert!(
        r2.converged.iter().any(|p| p == &fork_path),
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
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Bound), "B bound after keep-mine");
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

    let path_b = b.blocked_docs()[0].path.clone();
    let path_a = a.blocked_docs()[0].path.clone();

    // BOTH set keep-mine.
    a.set_fork_resolution(path_a, Resolution::KeepMine);
    b.set_fork_resolution(path_b, Resolution::KeepMine);

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
    let fork_path = b.blocked_docs()[0].path.clone();

    // Resolve keep-both, then round 2.
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepBoth);
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

// --- 4e. Same-region conflict on a BOUND doc: detection, block, resolution -

/// Establish a shared (bound) lineage for `path`/`seed`: A creates it, B adopts
/// A's base in one round. Returns the two nodes, their oplogs, B's doc id, and
/// the dirs (kept alive). After this both materialize `seed` and B is Bound.
async fn bound_pair(
    key: &ContentKey,
    path: &str,
    seed: &str,
) -> (
    SyncNode,
    Arc<OpLog>,
    String,
    SyncNode,
    Arc<OpLog>,
    String,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let (mut a, oplog_a, da) = mk_node(key);
    let (mut b, oplog_b, db) = mk_node(key);
    enroll_each_other(&a, &b);
    // Both devices independently hold the seed (each records a seed content_hash
    // in its own `.ops` history) — the real steady-state shape. First contact
    // with identical content adopts the canonical lineage and binds both sides.
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();

    // Drive bidirectional rounds until BOTH sides are Bound on the shared
    // lineage. Identical content binds via the canonical/non-canonical adoption
    // dance (`min(fingerprint)` owns the lineage), which can take a couple of
    // passes either direction depending on which device is canonical — so loop
    // rather than assume one round settles it regardless of the random
    // fingerprints.
    for _ in 0..4 {
        let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server0 = spawn_responder(a, Duration::from_secs(3));
        b.sync_once(&addr0).await.unwrap();
        a = server0.await.unwrap();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();

        if a.status_of_path(path) == Some(SyncStatus::Bound)
            && b.status_of_path(path) == Some(SyncStatus::Bound)
        {
            break;
        }
    }
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, seed);
    assert_eq!(a.status_of_path(path), Some(SyncStatus::Bound), "A bound on shared lineage");
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Bound), "B bound on shared lineage");
    (a, oplog_a, doc_a, b, oplog_b, doc_b, da, db)
}

/// REGRESSION guard for the desired merge behavior: two BOUND devices edit
/// DISJOINT regions concurrently — the same-region gate must NOT block; the
/// steady-state text merge lands both edits. (Companion to the
/// `same_region_concurrent_edits_block` test: the boundary between auto-merge
/// and block.) [sync-conflict-detect-same-region]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_disjoint_edits_still_auto_merge() {
    let key = ContentKey::generate();
    let path = "notes/disjoint.md";
    let seed = "HEAD base\nmiddle\nTAIL base\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;

    // Disjoint edits: A edits HEAD, B edits TAIL.
    oplog_a
        .apply_user_text(&doc_a, "HEAD ALPHA\nmiddle\nTAIL base\n")
        .unwrap();
    oplog_b
        .apply_user_text(&doc_b, "HEAD base\nmiddle\nTAIL OMEGA\n")
        .unwrap();

    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr_a).await.unwrap();
    assert!(rb.blocked.is_empty(), "disjoint edits must NOT block: {rb:?}");
    let mut a = server1.await.unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr_b).await.unwrap();
    assert!(ra.blocked.is_empty(), "disjoint edits must NOT block: {ra:?}");
    let _b = server2.await.unwrap();

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converge");
    assert!(text_a.contains("ALPHA") && text_a.contains("OMEGA"), "both edits survive: {text_a:?}");
}

/// Two BOUND devices edit the SAME region concurrently — the gate must BLOCK
/// (reason `same-region`) instead of silently interleaving. The peer delta
/// is held, NOT folded into accepted (B stays at its own text), and the block
/// is recorded persistently for the UI. [sync-conflict-detect-same-region,
/// sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_concurrent_edits_block() {
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;

    // Both rewrite the SAME line ("body line") — overlapping ranges.
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    let _a = server.await.unwrap();

    assert!(
        rb.blocked.iter().any(|(p, reason)| p == path && reason == "same-region"),
        "same-region edit blocks: {rb:?}"
    );
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Blocked), "B blocked");
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one persistent blocked entry: {blocked:?}");
    assert_eq!(blocked[0].reason, "same-region");
    // The delta was HELD, not folded: B stays at its own text.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        b_text,
        "the peer delta was held — B's content unchanged (no silent interleave)"
    );
}

/// REGRESSION (silent data loss): two BOUND devices BOTH diverge, but the shared
/// base has aged OUT of the peer's bounded `recent_history_hashes` window — so
/// `same_region_verdict` returns `NoSharedBase`. The old code grouped
/// `NoSharedBase` with `CleanMerge` and applied the peer text; with no
/// reconstructable base `apply_remote_update` falls back to `base = ours`, and
/// `three_way_merge(ours, ours, theirs) == theirs` silently OVERWRITES our
/// divergent edits. The fix BLOCKS instead (reason `same-region`, resolvable),
/// per `sync.md` ("no common base → fork conflict, never silently merged").
///
/// The base is forced out of the window by driving A's doc forward past the
/// 32-hash `RECENT_HISTORY_WINDOW` WITHOUT syncing those edits to B, then having
/// B diverge with its own content. When B pulls A: B's current isn't in A's
/// (now seed-free) recent window, A's current isn't in B's history, AND no hash
/// B holds is in A's recent 32 → both-diverged-no-shared-base. The assertion
/// that matters: OURS (B's text) is NOT silently lost.
/// [sync-conflict-detect-same-region, sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_diverged_no_shared_base_blocks_not_silent_loss() {
    let key = ContentKey::generate();
    let path = "notes/aged-out.md";
    let seed = "title\nshared base line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;

    // Drive A's doc forward well past RECENT_HISTORY_WINDOW (32) distinct
    // versions WITHOUT syncing them to B, so the shared seed hash falls out of
    // A's recent-history window. Each edit is distinct text → distinct
    // content_hash → a fresh accepted op in A's history.
    for i in 0..40 {
        oplog_a
            .apply_user_text(&doc_a, &format!("title\nA private version {i}\n"))
            .unwrap();
    }
    let a_text = "title\nA's final divergent line\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();

    // B diverges from the shared base on its own — a genuine concurrent edit B
    // never sent to A. B's history still contains the seed, but A's manifest no
    // longer carries it (aged out), so no shared base is reconstructable.
    let b_text = "title\nB's divergent line that must survive\n";
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // B pulls A. Both diverged, no shared base in A's window → BLOCK, never a
    // silent fast-forward to A's text.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    let _a = server.await.unwrap();

    assert!(
        rb.blocked.iter().any(|(p, reason)| p == path && reason == "same-region"),
        "both-diverged-no-shared-base must BLOCK (not silently fast-forward): {rb:?}"
    );
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Blocked), "B blocked");
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one persistent blocked entry: {blocked:?}");
    assert_eq!(blocked[0].reason, "same-region");
    // THE assertion that matters: OUR text was NOT overwritten by theirs.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        b_text,
        "B's divergent edit must survive — no silent overwrite by A's text"
    );
    assert_ne!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "B must NOT have been silently fast-forwarded to A's content"
    );
}

/// Same-region block → keep-theirs: BOTH devices converge to A's (theirs)
/// content, and the block clears without re-conflicting on later rounds.
/// [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_keep_theirs_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let fork_path = b.blocked_docs()[0].path.clone();

    // Resolve keep-theirs; round 2 (B pulls + resolves).
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-theirs no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B adopted A's text");

    // Round 3: A pulls B's decisive op so it converges too.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not block on convergence: {ra:?}");
    let b = server3.await.unwrap();
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text, "A converged to theirs");

    // Extra round must NOT re-block either side.
    let addr4 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server4 = spawn_responder(a, Duration::from_secs(2));
    let mut b = b;
    let r4 = b.sync_once(&addr4).await.unwrap();
    assert!(r4.blocked.is_empty(), "no re-block on a later round: {r4:?}");
    let _a = server4.await.unwrap();
}

/// The REALISTIC bidirectional flow the single-direction tests miss: in real
/// auto-sync BOTH devices are dialers, so BOTH independently detect a same-region
/// overlap and BOTH block. Resolving on ONE side must converge AND auto-clear the
/// OTHER side's independent block once the content converges — not leave it
/// blocked until the user also clicks there. Regression for
/// `clear_stale_block`. [sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_both_sides_block_then_one_resolves_clears_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // BOTH devices run a round (each is a dialer in turn) and BOTH block.
    // B dials A:
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    assert!(!b.blocked_docs().is_empty(), "B blocked on its round");
    // A dials B:
    let addrb = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb = spawn_responder(b, Duration::from_secs(3));
    a.sync_once(&addrb).await.unwrap();
    let mut b = serverb.await.unwrap();
    assert!(
        !a.blocked_docs().is_empty(),
        "A independently blocks too — the realistic both-dialers case"
    );

    // User resolves keep-theirs on B ONLY (A gets no decision).
    let fork_path = b.blocked_docs()[0].path.clone();
    b.set_fork_resolution(fork_path, Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(b.blocked_docs().is_empty(), "B resolved + unblocked");
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "B adopted A's text"
    );

    // A is STILL blocked with no decision. Its NEXT round must AUTO-CLEAR the
    // now-stale block because the content has converged (this is the bug:
    // before `clear_stale_block`, A stayed blocked forever even on force-sync).
    let addrb2 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb2 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addrb2).await.unwrap();
    let _b = serverb2.await.unwrap();
    assert!(ra.blocked.is_empty(), "A's round does not re-block: {ra:?}");
    assert!(
        a.blocked_docs().is_empty(),
        "A's stale block auto-cleared once the conflict converged out-of-band"
    );
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        a_text,
        "A converged to the resolved text"
    );
}

/// Same-region block → keep-mine: BOTH devices converge to B's (ours) content;
/// our text wins forward on the shared lineage. No re-block. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_keep_mine_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let fork_path = b.blocked_docs()[0].path.clone();

    // Resolve keep-mine; round 2 (B re-asserts ours forward).
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepMine);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-mine no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_text, "B keeps ours");

    // Round 3: A pulls B's decisive op → converges to ours.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not block: {ra:?}");
    let mut b = server3.await.unwrap();
    let a_final = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(a_final, b_text, "A converged to B's (keep-mine) content: {a_final:?}");
    assert_eq!(a_final.matches("EDITED BY B").count(), 1, "B's text once on A");
    assert_eq!(a_final.matches("EDITED BY A").count(), 0, "A's divergence discarded");

    // Extra round, no re-block.
    let addr4 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server4 = spawn_responder(a, Duration::from_secs(2));
    let r4 = b.sync_once(&addr4).await.unwrap();
    assert!(r4.blocked.is_empty(), "no re-block on a later round: {r4:?}");
    let _a = server4.await.unwrap();
}

/// Same-region block → keep-both: A's text wins at the path, B's text survives
/// as a `conflict-` sibling note; both devices converge. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_keep_both_preserves_local_copy() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    let fork_path = b.blocked_docs()[0].path.clone();

    // Resolve keep-both; round 2.
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepBoth);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let _a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-both no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");

    // Original path holds A's content; B's survives as a conflict sibling.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_text,
        "original path adopted A's content"
    );
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

// --- 4c-bis. Delete-vs-edit: block + Keep-deleted / Keep-edit ---------------

/// Two BOUND devices: A edits the doc while B deletes it (concurrent). The gate
/// must BLOCK (reason `delete-vs-edit`) on the puller rather than silently let
/// the delete win or the edit resurrect. Covers BOTH directions by pulling each
/// way. [sync-conflict-delete-vs-edit]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_blocks_both_directions() {
    let key = ContentKey::generate();
    let path = "notes/dve.md";
    let seed = "title\nbody line\n";

    // Direction 1: A (peer/server) deleted, B (puller) edited → B blocks.
    {
        let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
            bound_pair(&key, path, seed).await;
        oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
        oplog_b.apply_user_text(&doc_b, "title\nbody EDITED BY B\n").unwrap();
        let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr).await.unwrap();
        let _a = server.await.unwrap();
        assert!(
            rb.blocked.iter().any(|(p, r)| p == path && r == "delete-vs-edit"),
            "peer-deleted + we-edited must block delete-vs-edit: {rb:?}"
        );
        assert_eq!(b.status_of_path(path), Some(SyncStatus::Blocked), "B blocked");
        // The delete was HELD, not folded: B stays live at its own edit.
        let mb = oplog_b.materialize_accepted(&doc_b).unwrap();
        assert!(!mb.tombstone, "B's edit not silently deleted");
        assert_eq!(mb.text, "title\nbody EDITED BY B\n");
    }

    // Direction 2: A (peer/server) edited, B (puller) deleted → B blocks.
    {
        let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
            bound_pair(&key, path, seed).await;
        oplog_a.apply_user_text(&doc_a, "title\nbody EDITED BY A\n").unwrap();
        oplog_b.tombstone_document(&doc_b, &Author::User).unwrap();
        let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr).await.unwrap();
        let _a = server.await.unwrap();
        assert!(
            rb.blocked.iter().any(|(p, r)| p == path && r == "delete-vs-edit"),
            "we-deleted + peer-edited must block delete-vs-edit: {rb:?}"
        );
        // The peer edit was HELD, not folded: B stays tombstoned at its own delete.
        assert!(
            oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
            "B's delete not silently resurrected by the peer edit"
        );
    }
}

/// REGRESSION guard: a fast-forward delete (the peer deleted a version we
/// already hold, and we did NOT concurrently edit) must STILL auto-apply (→
/// trash) and NOT block. [sync-conflict-delete-vs-edit]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fast_forward_delete_auto_applies_no_block() {
    let key = ContentKey::generate();
    let path = "notes/ffdel.md";
    let seed = "title\nbody line\n";
    let (mut a, oplog_a, doc_a, mut b, _oplog_b, doc_b, _da, db) =
        bound_pair(&key, path, seed).await;

    // Only A deletes; B stays at the shared base (no concurrent edit).
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    let md_path = db.path().join(path);
    assert!(md_path.exists(), "B's .md exists before the delete syncs");

    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    let _a = server.await.unwrap();

    assert!(rb.blocked.is_empty(), "a fast-forward delete must NOT block: {rb:?}");
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Bound), "B stays bound");
    assert!(
        _oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "the delete auto-applied (B tombstoned)"
    );
    assert!(!md_path.exists(), "the ghost .md was trashed (Phase-3 trash path)");
}

/// Delete-vs-edit block → Keep deleted: BOTH devices converge to deleted, the
/// block clears, and a later round does not re-block. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_keep_deleted_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/dve.md";
    let seed = "title\nbody line\n";
    // A deletes, B edits — B is the puller/resolver. Keep-deleted makes B
    // tombstone + trash its own edited file (the live-side trash path), then
    // pushes the tombstone so A converges to deleted too.
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, db) =
        bound_pair(&key, path, seed).await;
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    oplog_b.apply_user_text(&doc_b, "title\nbody EDITED BY B\n").unwrap();
    assert!(db.path().join(path).exists(), "B's edited .md exists before resolve");

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    assert_eq!(b.blocked_docs()[0].reason, "delete-vs-edit");

    // Resolve keep-deleted; round 2 (B resolves + pushes the tombstone to A).
    b.set_fork_resolution(path.to_string(), Resolution::KeepDeleted);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-deleted no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");
    assert!(
        oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B stays deleted"
    );
    // A adopted the tombstone via PushAdopt → A is deleted too, ghost trashed.
    assert!(
        oplog_a.materialize_accepted(&doc_a).unwrap().tombstone,
        "A converged to deleted"
    );
    assert!(
        !db.path().join(path).exists(),
        "B's .md trashed on keep-deleted"
    );

    // Extra round: no re-block on either side.
    let addr3 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(a, Duration::from_secs(2));
    let r3 = b.sync_once(&addr3).await.unwrap();
    assert!(r3.blocked.is_empty(), "no re-block after keep-deleted: {r3:?}");
    let _a = server3.await.unwrap();
}

/// Delete-vs-edit block → Keep edit (resurrect): BOTH devices converge to the
/// live edited document, the block clears, and a later round does not re-block.
/// [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_keep_edit_resurrects_and_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/dve.md";
    let seed = "title\nbody line\n";
    // B deletes, A edits — B is the puller/resolver. Keep-edit must resurrect B
    // to the edited (A's) content.
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, db) =
        bound_pair(&key, path, seed).await;
    let edited = "title\nbody EDITED BY A\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();
    oplog_b.tombstone_document(&doc_b, &Author::User).unwrap();

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    assert_eq!(b.blocked_docs()[0].reason, "delete-vs-edit");

    // Resolve keep-edit; round 2.
    b.set_fork_resolution(path.to_string(), Resolution::KeepEdit);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-edit no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");
    let mb = oplog_b.materialize_accepted(&doc_b).unwrap();
    assert!(!mb.tombstone, "B resurrected (not tombstoned)");
    assert_eq!(mb.text, edited, "B holds the edited content");
    // A adopted B's resurrected lineage via PushAdopt → A is live + edited.
    let ma = oplog_a.materialize_accepted(&doc_a).unwrap();
    assert!(!ma.tombstone, "A converged to live");
    assert_eq!(ma.text, edited, "A holds the edited content");
    assert!(db.path().join(path).exists(), "B's .md is back on disk");

    // Extra round: no re-block.
    let addr3 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(a, Duration::from_secs(2));
    let r3 = b.sync_once(&addr3).await.unwrap();
    assert!(r3.blocked.is_empty(), "no re-block after keep-edit: {r3:?}");
    let _a = server3.await.unwrap();
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
        let fork_path = b.blocked_docs()[0].path.clone();

        // The expected single converged text per resolution:
        // - keep-mine  → B's content wins on both sides.
        // - keep-theirs/keep-both → A's content wins at the original path.
        let (expected, marker) = match resolution {
            Resolution::KeepMine => (b_text, "edited by B"),
            _ => (a_text, "edited by A"),
        };

        // Resolution round.
        b.set_fork_resolution(fork_path.clone(), resolution);
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
    // Path IS the identity (sync-path-identity). Confirm B's local doc lives at
    // the old path before the rename rides in.
    assert_eq!(
        oplog_b.path_for_doc(&doc_b).unwrap().as_deref(),
        Some(old_path),
        "B's local doc starts at the old path"
    );

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

    // Identity IS the path (`op-log-path-identity`): the rename relocated B's
    // doc's files from the old path to the new one, so the doc now resolves at
    // `new_path` and no longer at `old_path` — no phantom second doc was minted.
    let _ = &doc_b;
    assert!(
        oplog_b.doc_id_for_path(new_path).unwrap().is_some(),
        "B's doc now lives at the new path — identity is the binding"
    );
    assert!(
        oplog_b.doc_id_for_path(old_path).unwrap().is_none(),
        "the old path no longer resolves after the rename moved the doc"
    );
    // B's own edit is still present (the rename didn't clobber the body).
    let doc_b = oplog_b.doc_id_for_path(new_path).unwrap().unwrap();
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

// --- 5b. Concurrent rename collision: BLOCK + user resolution ---------------

/// Seed the headline concurrent-rename-collision scenario: A holds `foo.md`
/// (a_content) and B holds `bar.md` (b_content) — different docs, different
/// content, never synced together. Each independently renames its doc to
/// `target.md` while disconnected. Returns the two nodes + oplogs + doc ids +
/// temp dirs (kept alive). After this, B dialing A sees A's manifest entry for
/// `target.md` with `prior_paths = [foo.md]` colliding with B's own local
/// replica at `target.md` (formerly bar) — the LWW-on-path collision.
async fn rename_collision_setup(
    key: &ContentKey,
    a_content: &str,
    b_content: &str,
) -> (
    SyncNode,
    Arc<OpLog>,
    String,
    SyncNode,
    Arc<OpLog>,
    String,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let (a, oplog_a, da) = mk_node(key);
    let (b, oplog_b, db) = mk_node(key);
    enroll_each_other(&a, &b);

    let target_path = "notes/target.md";
    let doc_a = oplog_a
        .create_document("notes/foo.md", "note", a_content, &Author::User)
        .unwrap();
    let doc_b = oplog_b
        .create_document("notes/bar.md", "note", b_content, &Author::User)
        .unwrap();
    oplog_a
        .rename_document(&doc_a, target_path, &Author::User)
        .unwrap();
    oplog_b
        .rename_document(&doc_b, target_path, &Author::User)
        .unwrap();

    // Identity is the path (`op-log-path-identity`): the rename relocated each
    // doc's files to `target_path`, so the doc ids returned are the post-rename
    // paths — the foo/bar paths no longer resolve.
    (
        a,
        oplog_a,
        target_path.to_string(),
        b,
        oplog_b,
        target_path.to_string(),
        da,
        db,
    )
}

/// Find the single `notes/target.conflict-<rand6>.md` sibling doc in `oplog`
/// and return its materialized text. Panics if absent.
fn conflict_copy_text(oplog: &OpLog) -> String {
    let all_paths: Vec<String> = oplog
        .list_doc_ids()
        .unwrap()
        .into_iter()
        .filter_map(|id| oplog.path_for_doc(&id).ok().flatten())
        .collect();
    let copy = all_paths
        .iter()
        .find(|p| {
            p.contains(".conflict-") && p.ends_with(".md") && p.starts_with("notes/target.")
        })
        .unwrap_or_else(|| panic!("expected a target.conflict-<rand6>.md sibling: {all_paths:?}"));
    let copy_id = oplog.doc_id_for_path(copy).unwrap().unwrap();
    oplog.materialize_accepted(&copy_id).unwrap().text
}

/// `sync-concurrent-rename-not-merged`: two devices independently rename
/// DIFFERENT documents to the SAME target path while disconnected. The two
/// disjoint lineages both claim the path — a contended change. Per the spec
/// this is NOT auto-resolved: it BLOCKS (reason `rename-collision`) for the
/// user to pick Keep mine / Keep theirs / Keep both. Nothing is moved, copied,
/// or adopted while blocked.
///
/// INVERSION NOTE: this test previously asserted `report.blocked.is_empty()`
/// and that the loser silently became a `target.conflict-<rand6>.md` copy with
/// A's lineage auto-adopted at the path. The spec now blocks instead of
/// auto-deciding, so the assertions are inverted: the round MUST block, B's doc
/// MUST stay at its own bar content (no adopt), and NO conflict-copy is written
/// until the user resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_rename_to_same_target_blocks_for_resolution() {
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;

    // B dials A → sees A's `target.md` (prior_paths=[foo.md]) colliding with
    // B's own `target.md` (formerly bar). Disjoint lineages → BLOCK.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr).await.unwrap();
    let _a = server.await.unwrap();

    assert!(
        report
            .blocked
            .iter()
            .any(|(p, reason)| p == "notes/target.md" && reason == "rename-collision"),
        "concurrent rename collision must BLOCK for user resolution: {report:?}"
    );
    assert_eq!(
        b.status_of_path("notes/target.md"),
        Some(SyncStatus::Blocked),
        "B's target.md is blocked"
    );
    let blocked = b.blocked_docs();
    assert_eq!(blocked.len(), 1, "one persistent blocked entry: {blocked:?}");
    assert_eq!(blocked[0].reason, "rename-collision");

    // HELD: nothing moved/copied/adopted. B's doc stays at its own bar content;
    // no conflict-copy sibling exists yet.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        b_content,
        "B's doc unchanged while blocked (no silent adopt)"
    );
    let copy_count = oplog_b
        .list_doc_ids()
        .unwrap()
        .into_iter()
        .filter_map(|id| oplog_b.path_for_doc(&id).ok().flatten())
        .filter(|p| p.contains(".conflict-"))
        .count();
    assert_eq!(copy_count, 0, "no conflict-copy written while blocked");

    // A is untouched too.
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        a_content,
        "A's content at target.md unchanged"
    );
}

/// Rename collision → Keep theirs: the PEER's (A's) doc wins `target.md`; OUR
/// (B's) doc moves aside to a `conflict-` sibling. Both devices converge with no
/// data loss and no re-block. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_keep_theirs_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    // Resolve keep-theirs; round 2 (B adopts A at the path, moves its own aside).
    b.set_fork_resolution("notes/target.md".to_string(), Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-theirs no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");

    // target.md holds A's content on B; B's bar survives as a conflict sibling.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_content,
        "B's target.md adopted A's content (theirs won the path)"
    );
    assert_eq!(conflict_copy_text(&oplog_b), b_content, "B's bar preserved as conflict copy");

    // Round 3: A pulls B's conflict-copy doc so it converges too.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not block on convergence: {ra:?}");
    let b = server3.await.unwrap();
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        a_content,
        "A keeps its content at target.md"
    );
    assert_eq!(conflict_copy_text(&oplog_a), b_content, "A pulled B's bar conflict copy");

    // Extra round must NOT re-block.
    let addr4 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server4 = spawn_responder(a, Duration::from_secs(2));
    let mut b = b;
    let r4 = b.sync_once(&addr4).await.unwrap();
    assert!(r4.blocked.is_empty(), "no re-block on a later round: {r4:?}");
    let _a = server4.await.unwrap();
}

/// Rename collision → Keep mine: OUR (B's) doc keeps `target.md`; the PEER's
/// (A's) doc yields to a `conflict-` sibling. We push our base so A adopts ours
/// at the path, and preserve A's content as a conflict copy that A pulls back.
/// Both converge, no re-block. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_keep_mine_converges() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    // Resolve keep-mine; round 2 (B keeps target.md, pushes ours to A, copies theirs).
    b.set_fork_resolution("notes/target.md".to_string(), Resolution::KeepMine);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-mine no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");

    // B keeps its bar at target.md; A's foo preserved as a conflict sibling on B.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        b_content,
        "B keeps its content at target.md (mine won the path)"
    );
    assert_eq!(conflict_copy_text(&oplog_b), a_content, "A's foo preserved as conflict copy on B");
    // A adopted B's bar at target.md via the push.
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        b_content,
        "A adopted B's bar at target.md (push)"
    );

    // Round 3: A pulls B's conflict-copy of A's own foo content.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not block on convergence: {ra:?}");
    let b = server3.await.unwrap();
    assert_eq!(conflict_copy_text(&oplog_a), a_content, "A pulled its foo back as a conflict copy");

    // Extra round must NOT re-block.
    let addr4 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server4 = spawn_responder(a, Duration::from_secs(2));
    let mut b = b;
    let r4 = b.sync_once(&addr4).await.unwrap();
    assert!(r4.blocked.is_empty(), "no re-block on a later round: {r4:?}");
    let _a = server4.await.unwrap();
}

/// Rename collision → Keep both: both docs survive at distinct paths. The
/// winner of the contended path is DETERMINISTIC by fingerprint (`min` keeps
/// the path), so both devices converge to the same assignment regardless of who
/// resolves. No re-block. [sync-conflict-resolve-actions]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_keep_both_converges_deterministically() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;

    // The resolver is B; the deterministic winner of the path is the smaller
    // fingerprint. Compute which content should end up at target.md vs the copy.
    let b_keeps_path = b.fingerprint().0 < a.fingerprint().0;
    let (path_text, copy_text) = if b_keeps_path {
        (b_content, a_content)
    } else {
        (a_content, b_content)
    };

    // Round 1: block.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();

    // Resolve keep-both; round 2.
    b.set_fork_resolution("notes/target.md".to_string(), Resolution::KeepBoth);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let mut a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "keep-both no longer blocks: {r2:?}");
    assert!(b.blocked_docs().is_empty(), "block cleared");

    // The deterministic winner is at target.md on B; the loser is the copy.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        path_text,
        "deterministic winner at target.md on B"
    );
    assert_eq!(conflict_copy_text(&oplog_b), copy_text, "loser preserved as conflict copy on B");

    // Round 3: A converges to the same assignment.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not block on convergence: {ra:?}");
    let b = server3.await.unwrap();
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        path_text,
        "A converges to the same winner at target.md"
    );
    assert_eq!(conflict_copy_text(&oplog_a), copy_text, "A has the same loser as a conflict copy");

    // Extra round must NOT re-block.
    let addr4 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server4 = spawn_responder(a, Duration::from_secs(2));
    let mut b = b;
    let r4 = b.sync_once(&addr4).await.unwrap();
    assert!(r4.blocked.is_empty(), "no re-block on a later round: {r4:?}");
    let _a = server4.await.unwrap();
}

/// Regression guard: an ORDINARY rename (the peer renames a doc to a path
/// NOTHING else claims) must still auto-apply — only a genuine collision with a
/// different local doc at the target blocks. A holds `foo.md`, binds with B,
/// then renames it to `renamed.md` (a path B has no other doc at). B's pull
/// applies the rename on the shared lineage with no block.
/// [sync-concurrent-rename-not-merged]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_colliding_rename_does_not_block() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let old_path = "notes/foo.md";
    let new_path = "notes/renamed.md";
    let seed = "ordinary rename\nbody\n";
    let doc_a = oplog_a
        .create_document(old_path, "note", seed, &Author::User)
        .unwrap();

    // Round 0: bind A↔B on foo.md.
    let addr0 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server0 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr0).await.unwrap();
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(old_path).unwrap().unwrap();

    // A renames to a path B has no other doc at — an ordinary rename.
    oplog_a.rename_document(&doc_a, new_path, &Author::User).unwrap();

    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let report = b.sync_once(&addr1).await.unwrap();
    let _a = server1.await.unwrap();

    assert!(
        report.blocked.is_empty(),
        "an ordinary rename must NOT block: {report:?}"
    );
    // Identity IS the path: the rename relocated B's doc to `new_path`, so the
    // doc resolves there and not at the old path (`op-log-path-identity`).
    let _ = &doc_b;
    assert!(
        oplog_b.doc_id_for_path(new_path).unwrap().is_some(),
        "B's doc moved to the new path (rename auto-applied on the shared lineage)"
    );
    assert!(
        oplog_b.doc_id_for_path(old_path).unwrap().is_none(),
        "the old path no longer resolves after the rename"
    );
    assert_eq!(
        oplog_b.list_doc_ids().unwrap().len(),
        1,
        "no conflict-copy minted for an ordinary rename"
    );
}

// --- 6. Server-mediated store-and-forward, zero-knowledge -----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_store_and_forward_zero_knowledge() {
    // Path is the cross-device identity (sync-path-identity); no LogicalId
    // negotiation rides through the hub.

    let key = ContentKey::generate();
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

    // A holds a doc with content; B shares the lineage (a prior P2P
    // bind/adopt stand-in, exactly as the server path assumes — the hub can't
    // classify on ciphertext). Both reach the doc by its path; the blind_id
    // (HMAC of the content key + path) is the same on both ends, so the hub's
    // append-only log under that blind_id is the shared transfer channel.
    let doc_a = oplog_a.create_document(path, "note", "alpha\n", &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, "alpha\nbeta\n").unwrap();

    let doc_b = oplog_b.create_document(path, "note", "", &Author::User).unwrap();
    oplog_b.adopt_lineage(&doc_b, &oplog_a.export_state(&doc_a).unwrap()).unwrap();

    // A makes an offline edit B has never seen.
    oplog_a.apply_user_text(&doc_a, "alpha\nbeta\nGAMMA marker\n").unwrap();
    let want = oplog_a.materialize_accepted(&doc_a).unwrap().text;

    let serve = tokio::spawn(async move { server.run(Duration::from_secs(10)).await.unwrap() });

    // A pushes while B is "offline" (not connected). Then B pulls and converges.
    let ra = a.sync_via_server(&server_addr).await.unwrap();
    assert!(ra.bound.iter().any(|p| p == path), "A pushed: {ra:?}");
    let rb = b.sync_via_server(&server_addr).await.unwrap();
    assert!(rb.converged.iter().any(|p| p == path), "B converged via the hub: {rb:?}");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, want);

    serve.abort();

    // Zero-knowledge: re-open the on-disk store; stored bytes are ciphertext.
    let store = FileBlobStore::open(&server_data).unwrap();
    let blind = blind_id(&key, path);
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

/// Like [`mk_node_with_handle`] but with an EXPLICIT device keypair and an
/// EXPLICIT established flag, so a test can pin which device is canonical
/// (`min(fingerprint)`) and whether its key is deliberately set vs fresh —
/// the two axes the convergence decision turns on.
/// [sync-content-key-confirm-on-change]
fn mk_node_established(
    content_key: ContentKey,
    established: bool,
    keypair: DeviceKeypair,
) -> (SyncNode, Arc<OpLog>, tempfile::TempDir, SharedContentKey) {
    let (dir, oplog) = open_vault();
    let handle = if established {
        SharedContentKey::new_established(content_key)
    } else {
        SharedContentKey::new(content_key)
    };
    let node = SyncNode::new(
        Arc::clone(&oplog),
        handle.clone(),
        keypair,
        Settings::default(),
        EnrolledPeers::new(),
    );
    (node, oplog, dir, handle)
}

/// A FRESH (non-established) key on the non-canonical side adopts the peer's key
/// in-band, marks itself established, and the round surfaces the adoption.
/// [sync-content-key-confirm-on-change]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_key_adopts_peer_and_marks_established() {
    // Pin canonical = B (its fingerprint sorts BEFORE A's), so A is the
    // non-canonical adopter when A dials B.
    let kp_a = DeviceKeypair::generate();
    let kp_b = loop {
        let kp = DeviceKeypair::generate();
        if kp.fingerprint().0 < kp_a.fingerprint().0 {
            break kp;
        }
    };
    let b_fp = kp_b.fingerprint().0.clone();

    let key_a = ContentKey::from_bytes([1u8; 32]);
    let key_b = ContentKey::from_bytes([2u8; 32]);
    let (mut a, oplog_a, _da, handle_a) = mk_node_established(key_a, false, kp_a);
    let (mut b, _ob, _db, handle_b) = mk_node_established(key_b, false, kp_b);
    enroll_each_other(&a, &b);
    assert!(!handle_a.is_established(), "A starts fresh");

    oplog_a.create_document("notes/x.md", "note", "x\n", &Author::User).unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(3));
    let report = a.sync_once(&addr_b).await.expect("round ok");
    let _b = server_b.await.unwrap();

    // A adopted B's (canonical) key, is now established, and the round surfaced it.
    assert_eq!(handle_a.fingerprint(), handle_b.fingerprint(), "A adopted B's key");
    assert!(handle_a.is_established(), "adoption marks A established");
    assert_eq!(
        report.adopted_content_key_from.as_deref(),
        Some(b_fp.as_str()),
        "round surfaces the adoption with the peer fingerprint: {report:?}"
    );
    assert!(report.pending_content_key_change.is_none(), "no pending change");
}

/// An ESTABLISHED key on the non-canonical side is NOT silently switched: the
/// key is unchanged, no `ContentKeyRequest` is made, and the round surfaces a
/// pending-key-change for the user to confirm.
/// [sync-content-key-confirm-on-change]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn established_key_holds_and_surfaces_pending_change() {
    // Pin canonical = B again, so A would be the adopter — except A's key is
    // established, so it must hold instead.
    let kp_a = DeviceKeypair::generate();
    let kp_b = loop {
        let kp = DeviceKeypair::generate();
        if kp.fingerprint().0 < kp_a.fingerprint().0 {
            break kp;
        }
    };
    let b_fp = kp_b.fingerprint().0.clone();

    let key_a = ContentKey::from_bytes([1u8; 32]);
    let key_b = ContentKey::from_bytes([2u8; 32]);
    let fp_a_before = key_a.fingerprint();
    let (mut a, oplog_a, _da, handle_a) = mk_node_established(key_a, true, kp_a);
    let (mut b, _ob, _db, handle_b) = mk_node_established(key_b, false, kp_b);
    enroll_each_other(&a, &b);
    assert!(handle_a.is_established(), "A's key is established");

    oplog_a.create_document("notes/y.md", "note", "y\n", &Author::User).unwrap();

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(3));
    let report = a.sync_once(&addr_b).await.expect("round ok (held, not aborted)");
    let _b = server_b.await.unwrap();

    // A's established key is UNCHANGED — not silently switched to B's.
    assert_eq!(handle_a.fingerprint(), fp_a_before, "A's key unchanged");
    assert_ne!(handle_a.fingerprint(), handle_b.fingerprint(), "keys still differ");
    assert!(handle_a.is_established(), "A stays established");
    // The held change is surfaced with B's fingerprint for the page to confirm.
    assert_eq!(
        report.pending_content_key_change.as_deref(),
        Some(b_fp.as_str()),
        "round surfaces the held key change: {report:?}"
    );
    assert!(report.adopted_content_key_from.is_none(), "nothing adopted");
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

/// A single document that hits a DOC-LEVEL error mid-round must NOT abort the
/// rest of the round: the other documents still sync, the bad one is recorded
/// in `report.errored`, and `sync_once` returns `Ok` (not `Err`).
/// Regression for `bug-sync-round-aborts-on-one-doc`.
///
/// The honest single-doc failure used here is a content-key mismatch on the
/// DELTA path: doc1 is brought onto a shared lineage with a matching key, then
/// B's content key is rotated to a DIFFERENT key while B is the CANONICAL side
/// (so the in-band key convergence is a no-op — the canonical device never
/// requests the peer's key). On the next round A's content-encrypted delta for
/// doc1 fails to decrypt under B's now-different key (`Error::Decrypt`, a
/// doc-level error). doc2 is fresh on A, so it adopts via the UNencrypted
/// lineage base and converges regardless of the key mismatch. No test-only
/// failure hook is added to production code — the mismatch rides the real key
/// state machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_doc_error_does_not_abort_round() {
    // Generate two keypairs and pick B's so it sorts BEFORE A's — B is then the
    // canonical content-key owner and never re-adopts A's key in-band, so a
    // deliberate later key rotation on B sticks for the delta round.
    let kp_a = DeviceKeypair::generate();
    let kp_b = loop {
        let kp = DeviceKeypair::generate();
        if kp.fingerprint().0 < kp_a.fingerprint().0 {
            break kp;
        }
    };

    let shared = ContentKey::from_bytes([9u8; 32]);
    let (dir_a, oplog_a) = open_vault();
    let (dir_b, oplog_b) = open_vault();
    let _da = dir_a;
    let _db = dir_b;

    let handle_b = SharedContentKey::new(ContentKey::from_bytes(*shared.as_bytes()));
    let mut a = SyncNode::new(
        Arc::clone(&oplog_a),
        SharedContentKey::new(ContentKey::from_bytes(*shared.as_bytes())),
        kp_a,
        Settings::default(),
        EnrolledPeers::new(),
    );
    let mut b = SyncNode::new(
        Arc::clone(&oplog_b),
        handle_b.clone(),
        kp_b,
        Settings::default(),
        EnrolledPeers::new(),
    );
    enroll_each_other(&a, &b);

    // doc1 exists on both with identical content → round 1 puts it on a SHARED
    // lineage (the non-canonical side, A, adopts B-or-vice-versa; either way the
    // steady-state delta path becomes live).
    let doc1 = "notes/keyed-doc.md";
    let text1 = "alpha\nbeta\n";
    let doc1_a = oplog_a.create_document(doc1, "note", text1, &Author::User).unwrap();
    oplog_b.create_document(doc1, "note", text1, &Author::User).unwrap();

    // Drive a couple of bidirectional rounds to settle doc1 onto a shared
    // lineage (keys still match here, so this converges cleanly).
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

    // Now diverge: A edits doc1 (its delta will be encrypted under A's key) and
    // creates a brand-new doc2 that B has never seen.
    oplog_a.apply_user_text(&doc1_a, "alpha\nbeta\ngamma\n").unwrap();
    let doc2 = "notes/fresh-doc.md";
    let text2 = "fresh body\n";
    oplog_a.create_document(doc2, "note", text2, &Author::User).unwrap();

    // Rotate B's content key to something A doesn't hold. B is canonical, so the
    // handshake's in-band convergence won't pull A's key back — the keys stay
    // different for the doc phase.
    handle_b.set(ContentKey::from_bytes([3u8; 32]));

    // The round: doc1 takes the delta path and FAILS to decrypt (doc-level);
    // doc2 adopts the unencrypted base and converges. The round must NOT abort.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    let report = b
        .sync_once(&addr_a)
        .await
        .expect("round must NOT abort on one doc's doc-level error");
    let _a = server_a.await.unwrap();

    // The good doc still synced.
    assert!(
        report.converged.contains(&doc2.to_string()),
        "the good doc converged despite the bad one failing: {report:?}"
    );
    let doc2_b = oplog_b
        .doc_id_for_path(doc2)
        .unwrap()
        .expect("B adopted the fresh doc");
    assert_eq!(
        oplog_b.materialize_accepted(&doc2_b).unwrap().text,
        text2,
        "the good doc's content landed on B"
    );

    // The bad doc was recorded, not silently dropped.
    assert!(
        report.errored.iter().any(|(p, _)| p == doc1),
        "the failing doc is recorded in report.errored: {report:?}"
    );
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
/// THE BUG this guards: `apply_remote_update` applied the rename but did NOT
/// relocate B's path-keyed per-doc files, so the doc was left resolvable at the
/// old path while a later manifest path-match on the new path minted a SECOND
/// doc for the same content — content duplication. Under path-as-identity
/// (`op-log-path-identity`) the receive path moves the per-doc files old→new and
/// repoints the history rows, so the doc resolves only at the new path. We re-run
/// several rounds so any deferred second-doc minting would surface.
/// [sync-path-matching-key]
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
    // the shared lineage, both ride the same delta). Identity is the path, so
    // after the rename A's doc is addressed by `new_path`.
    oplog_a.rename_document(&doc_a, new_path, &Author::User).unwrap();
    let _ = &doc_a;
    let doc_a = new_path.to_string();
    let edited = "shared head\nMARKER body\nshared tail\nRENAME-EDIT line\n";
    oplog_a.apply_user_text(&doc_a, edited).unwrap();

    // B pulls the delta carrying the rename + edit over the shared lineage.
    let addr1 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server1 = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr1).await.unwrap();
    assert!(r1.blocked.is_empty(), "rename must not fork/block: {r1:?}");
    let a = server1.await.unwrap();

    // B's existing doc moved to the new path (identity is the path), with the
    // content. The per-doc files relocated, so it resolves at the new path only.
    let _ = &doc_b;
    assert_eq!(
        oplog_b.doc_id_for_path(new_path).unwrap().as_deref(),
        Some(new_path),
        "B's doc now resolves at the new path (files relocated)"
    );
    let doc_b = new_path.to_string();
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, edited, "B has the renamed doc's content exactly: {got_b:?}");
    assert_eq!(got_b.matches("RENAME-EDIT line").count(), 1, "edit once: {got_b:?}");

    // B does NOT still resolve the old path to this doc (no stale duplicate).
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

/// A tombstones a shared doc WHILE B edits the same doc (concurrent). Under the
/// text substrate this is a delete-vs-edit CONFLICT: the spec BLOCKS it for user
/// resolution rather than silently picking a winner (`sync-conflict-delete-vs-edit`).
///
/// INVERSION NOTE: this previously asserted the Yrs-CRDT commutative merge (the
/// tombstone `meta` flag and B's `text` op merging into a deterministic
/// "tombstoned with B's text underneath"). That silent auto-merge is exactly
/// what the spec replaced with block-and-resolve — a delete concurrent with an
/// edit is contended, never auto-converged. So the corruption-probe invariant is
/// now: the conflict BLOCKS (no silent interleave / no data loss), each side
/// HOLDS its own state untouched while blocked, and no phantom doc is minted.
/// [sync-conflict-delete-vs-edit, sync-conflict-block-and-resolve, sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_blocks_without_corruption() {
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
    let mut a = server0.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().unwrap();

    // Concurrent: A tombstones, B edits the body.
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    let b_text = "base line\nB-EDIT marker\n";
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // B pulls A's manifest: the delete-vs-edit conflict BLOCKS (no silent
    // interleave). Re-run extra rounds — a deferred interleave would surface,
    // but the doc stays blocked + each side holds its own state.
    for _ in 0..3 {
        let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr).await.unwrap();
        a = server.await.unwrap();
        // Either this round freshly blocked it, or it stays persistently blocked
        // from a prior round — never silently merged.
        let freshly_blocked = rb
            .blocked
            .iter()
            .any(|(p, r)| p == path && r == "delete-vs-edit");
        assert!(
            freshly_blocked || b.status_of_path(path) == Some(SyncStatus::Blocked),
            "delete-vs-edit must BLOCK, not silently merge: {rb:?}"
        );
    }
    assert_eq!(
        b.status_of_path(path),
        Some(SyncStatus::Blocked),
        "B's doc is blocked delete-vs-edit (held, not folded)"
    );

    // HELD, not corrupted: A stays tombstoned at its base; B stays live at its
    // own edit. Neither side silently adopted the other.
    let mat_a = oplog_a.materialize_accepted(&doc_a).unwrap();
    assert!(mat_a.tombstone, "A stays deleted (its own state, held)");
    let mat_b = oplog_b.materialize_accepted(&doc_b).unwrap();
    assert!(!mat_b.tombstone, "B's edit not silently deleted");
    assert_eq!(mat_b.text, b_text, "B holds exactly its own edit, no interleave");
    assert_eq!(
        mat_b.text.matches("B-EDIT marker").count(),
        1,
        "B's edit appears exactly once: {:?}",
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
/// both sides (text 3-way merge). Extra rounds keep it byte-stable. This
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

// --- W4b. Concurrent SAME-region edits on a shared lineage now BLOCK --------

/// After A & B share a lineage, SAME-region concurrent edits no longer silently
/// interleave: the bound-doc gate detects the byte-range overlap and BLOCKS
/// the doc for user resolution (the behavior change of
/// `sync-conflict-detect-same-region`). A resolution then converges both sides.
/// (This test previously asserted the OLD silent-interleave contract; the slug
/// exists precisely to replace it.) The disjoint-region sibling that must STILL
/// auto-merge is `bound_disjoint_edits_still_auto_merge`.
/// [sync-conflict-detect-same-region, sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_lineage_same_region_edits_block_then_resolve() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/same-region.md";
    let seed = "alpha\nINSERT-HERE\nomega\n";
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;

    // Both edit the SAME line region with distinct markers.
    let a_text = "alpha\nINSERT-HERE AAA-side\nomega\n";
    let b_text = "alpha\nINSERT-HERE BBB-side\nomega\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // Round 1: B pulls A — the same-region overlap BLOCKS instead of merging.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    assert!(
        rb.blocked.iter().any(|(p, r)| p == path && r == "same-region"),
        "same-region edits block, not silently interleave: {rb:?}"
    );
    // Held, not folded: B keeps its own text.
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, b_text);

    // Resolve keep-theirs → both converge to A's content, no re-block.
    let fork_path = b.blocked_docs()[0].path.clone();
    b.set_fork_resolution(fork_path, Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr2).await.unwrap();
    let a = server2.await.unwrap();
    assert!(r2.blocked.is_empty(), "resolution clears the block: {r2:?}");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B adopted A's text");

    // A pulls B's decisive op → converges, no re-block.
    let addr3 = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server3 = spawn_responder(b, Duration::from_secs(3));
    let mut a = a;
    let ra = a.sync_once(&addr3).await.unwrap();
    assert!(ra.blocked.is_empty(), "A does not re-block on convergence: {ra:?}");
    let _b = server3.await.unwrap();
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text, "A converged to theirs");
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
    let fork_path = b.blocked_docs()[0].path.clone();

    // B edits AGAIN while blocked (stale-snapshot trap: a resolution must use
    // this current content, not the content at block time).
    oplog_b.apply_user_text(&doc_b, "title\nbody edited by B\nB-AGAIN while blocked\n").unwrap();

    // keep-theirs: B discards its branch (incl. the while-blocked edit) and
    // adopts A's content.
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepTheirs);
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
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Bound));
    let _ = &doc_b;

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
    let fork_path = b.blocked_docs()[0].path.clone();

    // B edits AGAIN while blocked — this CURRENT content is what keep-mine pushes.
    let b_current = "title\nbody edited by B\nB-CURRENT after block\n";
    oplog_b.apply_user_text(&doc_b, b_current).unwrap();

    b.set_fork_resolution(fork_path.clone(), Resolution::KeepMine);
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

// --- W7. Re-edit after a keep-theirs resolution is a clean merge -----------

/// keep-theirs resolves a fork → B adopts A's lineage, so AFTERWARD they SHARE a
/// lineage. Then BOTH edit divergently again. This must NOT re-fork or corrupt:
/// because the lineage is now shared, the subsequent divergent edits are a text
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
    let fork_path = b.blocked_docs()[0].path.clone();
    b.set_fork_resolution(fork_path.clone(), Resolution::KeepTheirs);
    let addr2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server2 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr2).await.unwrap();
    let a = server2.await.unwrap();
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B on A's content");

    // NOW both edit divergently again. Post-keep-theirs the lineage is shared,
    // so this is a text merge (disjoint regions here), NOT a new fork.
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

// === poke-on-commit transport round-trip ===================================

/// The transport-level half of `sync-poke-on-commit`: with two mutually
/// enrolled nodes, A's `poke(addr_b)` sets B's `poked` flag over the wire
/// (Hello → HelloAck, then SyncPoke → SyncPokeAck), and B's `take_poked()`
/// returns `true` exactly once then `false`. The poke carries no content — it
/// only wakes B's existing pull path (here, the flag the bootstrap driver
/// drains to fire an `auto_sync_round`). [sync-poke-on-commit]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poke_sets_peer_poked_flag_once() {
    let key = ContentKey::generate();
    let (mut a, _oplog_a, _da) = mk_node(&key);
    let (mut b, _oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    // No poke yet: B's flag starts clear.
    assert!(!b.take_poked(), "poked flag starts clear");

    let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_b = spawn_responder(b, Duration::from_secs(3));

    // A pokes B — lightweight nudge, no manifest / delta.
    a.poke(&addr_b).await.unwrap();

    let b = server_b.await.unwrap();
    // B saw the poke exactly once: true, then cleared.
    assert!(b.take_poked(), "poke set B's poked flag");
    assert!(!b.take_poked(), "poked flag cleared after one read");
}

// === Wave 7 — both-devices-as-dialers convergence + stale-block clearing =====
//
// Real auto-sync makes BOTH peers dialers, so on a genuine conflict BOTH
// independently detect it and BOTH block. Resolving on ONE side must (a) converge
// the content on both AND (b) auto-clear the OTHER side's independent block once
// the contention is gone out-of-band — the user should not have to also click on
// the second device. The single-direction conflict tests above never exercise
// this: only one device ever blocks there.
//
// These scenarios model both-devices-as-dialers via `sync_until_settled`, which
// alternates A→B then B→A rounds to a fixed point, and assert on BOTH sides:
// materialized content equal AND `blocked_docs()` empty at quiescence — the two
// checks the older tests omitted.

/// Drive realistic auto-sync to QUIESCENCE: alternate A-dials-B then B-dials-A
/// rounds (both devices are dialers, the real shape) until a fixed point —
/// neither side's round reports a new block AND both materialized texts are
/// equal AND neither side still lists a `blocked_docs()` entry — or the
/// `max_rounds` cap is hit (asserts it settled within the cap). Each side is
/// passed as a `(oplog, doc_id)` pair so the fixed-point check can materialize
/// and compare both sides' content. Returns the two nodes so the caller keeps
/// driving / asserting.
///
/// This is the core both-dialers methodology piece: a single resolution on ONE
/// device must propagate through these alternating rounds to converge the content
/// AND clear the OTHER device's independent stale block.
async fn sync_until_settled<'a>(
    mut a: SyncNode,
    mut b: SyncNode,
    side_a: (&'a Arc<OpLog>, &'a str),
    side_b: (&'a Arc<OpLog>, &'a str),
    max_rounds: usize,
) -> (SyncNode, SyncNode) {
    let (oplog_a, doc_a) = side_a;
    let (oplog_b, doc_b) = side_b;
    let mut settled = false;
    for round in 0..max_rounds {
        // A dials B.
        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        let ra = a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();

        // B dials A.
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr_a).await.unwrap();
        a = server_a.await.unwrap();

        // Fixed point: this full pair reported no NEW blocks, neither side still
        // holds a persistent block, and both sides agree. Under the text
        // substrate a DELETED doc keeps a per-device last-known text (a
        // recoverable trash artifact, not canonical content), so two converged
        // tombstones agree even when those artifacts differ — convergence for a
        // deleted doc is "both tombstoned", per `sync-conflict-delete-vs-edit`.
        // A live doc must converge on identical text.
        let mat_a = oplog_a.materialize_accepted(doc_a).unwrap();
        let mat_b = oplog_b.materialize_accepted(doc_b).unwrap();
        let agree = if mat_a.tombstone && mat_b.tombstone {
            true
        } else {
            !mat_a.tombstone && !mat_b.tombstone && mat_a.text == mat_b.text
        };
        let no_new_blocks = ra.blocked.is_empty() && rb.blocked.is_empty();
        let no_persistent_blocks = a.blocked_docs().is_empty() && b.blocked_docs().is_empty();
        if no_new_blocks && no_persistent_blocks && agree {
            tracing::info!(round, "sync_until_settled: reached quiescence");
            settled = true;
            break;
        }
    }
    assert!(
        settled,
        "sync did not reach quiescence within {max_rounds} rounds: \
         a_blocked={:?} b_blocked={:?} mat_a={:?} mat_b={:?}",
        a.blocked_docs(),
        b.blocked_docs(),
        oplog_a.materialize_accepted(doc_a).unwrap(),
        oplog_b.materialize_accepted(doc_b).unwrap(),
    );
    (a, b)
}

/// Make BOTH bound devices independently detect + block a same-region conflict
/// (each is a dialer in turn), then return the nodes still blocked. Shared setup
/// for the same-region both-sides scenarios. After this both `blocked_docs()` are
/// non-empty for `path`.
async fn both_block_same_region(
    a: SyncNode,
    b: SyncNode,
) -> (SyncNode, SyncNode) {
    // B dials A → B blocks.
    let mut b = b;
    let mut a = a;
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr).await.unwrap();
    a = server.await.unwrap();
    assert!(!b.blocked_docs().is_empty(), "B blocked on its round");
    // A dials B → A independently blocks too.
    let addrb = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb = spawn_responder(b, Duration::from_secs(3));
    a.sync_once(&addrb).await.unwrap();
    b = serverb.await.unwrap();
    assert!(
        !a.blocked_docs().is_empty(),
        "A independently blocks too — the realistic both-dialers case"
    );
    (a, b)
}

/// SAME-REGION, both-sides, driven through the quiescence helper: BOTH block,
/// the user resolves keep-theirs on ONE side, and the alternating rounds converge
/// BOTH sides AND clear BOTH blocks. (The hand-rolled variant
/// `same_region_both_sides_block_then_one_resolves_clears_both` already covers
/// this; this drives it through `sync_until_settled` as the methodology baseline.)
/// [sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_both_block_resolve_one_settles_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/contended.md";
    let seed = "title\nbody line\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    let (a, b) = both_block_same_region(a, b).await;

    // User resolves keep-theirs on B ONLY (A gets no decision).
    b.set_fork_resolution(path.to_string(), Resolution::KeepTheirs);

    // Drive to quiescence: B's resolution converges content to A's, and A's
    // independent stale block auto-clears once its content converges.
    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    // BOTH sides: content equal to the resolved (theirs = A's) text AND no block.
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text, "A == resolved text");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B == resolved text");
    assert!(a.blocked_docs().is_empty(), "A's stale block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
}

// --- delete-vs-edit, both-sides ----------------------------------------------

/// Make BOTH bound devices independently detect + block a delete-vs-edit
/// conflict. A deletes, B edits (or symmetric, per the seeded oplogs). Each dials
/// in turn so BOTH record a `delete-vs-edit` block. Returns the nodes still
/// blocked.
async fn both_block_delete_vs_edit(a: SyncNode, b: SyncNode) -> (SyncNode, SyncNode) {
    let mut b = b;
    let mut a = a;
    // B dials A → B blocks delete-vs-edit.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    a = server.await.unwrap();
    assert!(
        rb.blocked.iter().any(|(_, r)| r == "delete-vs-edit"),
        "B blocks delete-vs-edit on its round: {rb:?}"
    );
    // A dials B → A independently blocks delete-vs-edit too.
    let addrb = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addrb).await.unwrap();
    b = serverb.await.unwrap();
    assert!(
        ra.blocked.iter().any(|(_, r)| r == "delete-vs-edit"),
        "A independently blocks delete-vs-edit too (both-dialers): {ra:?}"
    );
    (a, b)
}

/// DELETE-VS-EDIT, both-sides, Keep-edit: A deletes while B edits; BOTH dial and
/// BOTH block delete-vs-edit. The user resolves Keep-edit on ONE side (B). Driving
/// to quiescence must converge BOTH to the live edited doc AND clear BOTH blocks —
/// including A's independent block, which has NO queued decision and must
/// auto-clear once the content converges out-of-band (the stale-block fix for the
/// delete-vs-edit re-eval path). [sync-conflict-delete-vs-edit,
/// sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_both_block_keep_edit_settles_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/dve-both.md";
    let seed = "title\nbody line\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, db) = bound_pair(&key, path, seed).await;
    // A deletes, B edits — a genuine concurrent delete-vs-edit.
    let edited = "title\nbody EDITED BY B\n";
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    oplog_b.apply_user_text(&doc_b, edited).unwrap();

    let (a, b) = both_block_delete_vs_edit(a, b).await;

    // User resolves Keep-edit on B ONLY. A stays blocked with no decision.
    b.set_fork_resolution(path.to_string(), Resolution::KeepEdit);

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 10).await;

    // BOTH converge to the LIVE edited doc (not tombstoned), with the edit text.
    let ma = oplog_a.materialize_accepted(&doc_a).unwrap();
    let mb = oplog_b.materialize_accepted(&doc_b).unwrap();
    assert!(!ma.tombstone, "A resurrected to live");
    assert!(!mb.tombstone, "B stays live");
    assert_eq!(ma.text, edited, "A holds the edited content");
    assert_eq!(mb.text, edited, "B holds the edited content");
    // BOTH blocks cleared — A's stale block auto-cleared (no decision queued there).
    assert!(a.blocked_docs().is_empty(), "A's stale delete-vs-edit block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
    assert!(db.path().join(path).exists(), "B's .md is back on disk (resurrected)");
}

/// DELETE-VS-EDIT, both-sides, Keep-deleted: A deletes while B edits; BOTH dial
/// and BOTH block. The user resolves Keep-deleted on ONE side (B). Driving to
/// quiescence converges BOTH to deleted AND clears BOTH blocks — A's independent
/// block (no decision) auto-clears once the tombstone converges. [sync-conflict-delete-vs-edit]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_both_block_keep_deleted_settles_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/dve-both-del.md";
    let seed = "title\nbody line\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, db) = bound_pair(&key, path, seed).await;
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    oplog_b.apply_user_text(&doc_b, "title\nbody EDITED BY B\n").unwrap();
    assert!(db.path().join(path).exists(), "B's edited .md exists before resolve");

    let (a, b) = both_block_delete_vs_edit(a, b).await;

    // Keep-deleted on B ONLY; A stays blocked with no decision.
    b.set_fork_resolution(path.to_string(), Resolution::KeepDeleted);

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 10).await;

    // BOTH converge to deleted (tombstoned), BOTH blocks cleared.
    assert!(oplog_a.materialize_accepted(&doc_a).unwrap().tombstone, "A deleted");
    assert!(oplog_b.materialize_accepted(&doc_b).unwrap().tombstone, "B deleted");
    assert!(a.blocked_docs().is_empty(), "A's stale block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
    assert!(!db.path().join(path).exists(), "B's .md trashed on keep-deleted");
}

// --- rename-collision, both-sides --------------------------------------------

/// Make BOTH devices independently detect + block a concurrent-rename collision.
/// Each renamed a DIFFERENT doc onto `target.md`; each dials in turn so BOTH
/// record a `rename-collision` block. Returns the nodes still blocked.
async fn both_block_rename_collision(a: SyncNode, b: SyncNode) -> (SyncNode, SyncNode) {
    let mut b = b;
    let mut a = a;
    let target = "notes/target.md";
    // B dials A → B blocks rename-collision.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    a = server.await.unwrap();
    assert!(
        rb.blocked.iter().any(|(p, r)| p == target && r == "rename-collision"),
        "B blocks rename-collision on its round: {rb:?}"
    );
    // A dials B → A independently blocks rename-collision too.
    let addrb = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addrb).await.unwrap();
    b = serverb.await.unwrap();
    assert!(
        ra.blocked.iter().any(|(p, r)| p == target && r == "rename-collision"),
        "A independently blocks rename-collision too (both-dialers): {ra:?}"
    );
    (a, b)
}

/// RENAME-COLLISION, both-sides, Keep-theirs: both rename different docs onto
/// `target.md`; BOTH dial and BOTH block. The user resolves Keep-theirs on ONE
/// side (B): the PEER's (A's) doc wins the path, B's own moves to a conflict
/// sibling. Driving to quiescence converges BOTH on the path assignment AND
/// clears BOTH blocks — A's independent block (no decision) auto-clears once the
/// collision is gone out-of-band (the stale-block fix for the rename-collision
/// re-eval path). [sync-concurrent-rename-not-merged, sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_both_block_keep_theirs_settles_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;
    let target = "notes/target.md";

    let (a, b) = both_block_rename_collision(a, b).await;

    // Keep-theirs on B ONLY: A's doc wins target.md, B moves its bar aside. A has
    // no decision — its independent block must auto-clear once the collision is gone.
    b.set_fork_resolution(target.to_string(), Resolution::KeepTheirs);

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 12).await;

    // BOTH converge on the path assignment: A's content at target.md on both.
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        a_content,
        "A keeps its content at target.md"
    );
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        a_content,
        "B's target.md adopted A's content (theirs won the path)"
    );
    // B's bar survives as a conflict copy.
    assert_eq!(conflict_copy_text(&oplog_b), b_content, "B's bar preserved as conflict copy");
    // BOTH blocks cleared (A's auto-cleared with no decision).
    assert!(a.blocked_docs().is_empty(), "A's stale rename-collision block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
}

/// RENAME-COLLISION, both-sides, Keep-both: both rename different docs onto
/// `target.md`; BOTH block. The user resolves Keep-both on ONE side; the
/// deterministic (`min(fingerprint)`) winner keeps the path, the loser becomes a
/// conflict sibling — so both devices agree on the assignment. Driving to
/// quiescence converges BOTH on the assignment AND clears BOTH blocks.
/// [sync-concurrent-rename-not-merged]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_both_block_keep_both_settles_both() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;
    let target = "notes/target.md";

    // Deterministic winner of the path: smaller fingerprint.
    let b_keeps_path = b.fingerprint().0 < a.fingerprint().0;
    let (path_text, copy_text) = if b_keeps_path {
        (b_content, a_content)
    } else {
        (a_content, b_content)
    };

    let (a, b) = both_block_rename_collision(a, b).await;

    b.set_fork_resolution(target.to_string(), Resolution::KeepBoth);

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 12).await;

    // BOTH converge to the same deterministic assignment.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        path_text,
        "deterministic winner at target.md on B"
    );
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        path_text,
        "A converges to the same winner at target.md"
    );
    assert_eq!(conflict_copy_text(&oplog_b), copy_text, "loser preserved as conflict copy on B");
    assert_eq!(conflict_copy_text(&oplog_a), copy_text, "A has the same loser as a conflict copy");
    assert!(a.blocked_docs().is_empty(), "A's stale block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
}

// --- fork, both-sides --------------------------------------------------------

/// FORK (disjoint lineages over a shared seed), both-sides: both independently
/// fork and BOTH block. The user resolves keep-theirs on ONE side (B). Driving to
/// quiescence converges BOTH to A's content AND clears BOTH blocks — A's
/// independent fork block auto-clears once it re-classifies as a fast-forward
/// against B's now-adopted (A's) lineage. [sync-blocked-state]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_both_block_keep_theirs_settles_both() {
    use hiker_sync::identity::Resolution;
    let seed = "title\nbody line\n";
    let a_text = "title\nbody edited by A\n";
    let b_text = "title\nbody edited by B\n";

    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/forked-both.md";
    let doc_a = oplog_a.create_document(path, "note", seed, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", seed, &Author::User).unwrap();
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    // BOTH fork: B dials A, then A dials B.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr).await.unwrap();
    let mut a = server.await.unwrap();
    assert!(rb.blocked.iter().any(|(p, r)| p == path && r == "fork"), "B forks: {rb:?}");
    let addrb = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let serverb = spawn_responder(b, Duration::from_secs(3));
    let ra = a.sync_once(&addrb).await.unwrap();
    let b = serverb.await.unwrap();
    assert!(ra.blocked.iter().any(|(p, r)| p == path && r == "fork"), "A independently forks: {ra:?}");

    // Keep-theirs on B ONLY. A has no decision.
    let b = b;
    b.set_fork_resolution(path.to_string(), Resolution::KeepTheirs);

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 10).await;

    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text, "A keeps its content");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B adopted A's content");
    assert!(a.blocked_docs().is_empty(), "A's stale fork block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's fork block cleared: {:?}", b.blocked_docs());
}

// --- deeper multi-step sequences through the helper --------------------------

/// DEEPER: conflict → resolve → both edit again in DISJOINT regions → drive →
/// converge with NO spurious block. A same-region conflict is resolved keep-theirs
/// (both-sides), then BOTH devices make fresh edits to DISJOINT regions of the now
/// shared-lineage doc. Driving to quiescence must merge both disjoint edits with
/// no re-block on either side — proving the post-resolution lineage is genuinely
/// shared and steady-state text merge works. [sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_resolve_then_disjoint_edits_converge_no_block() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/seq-disjoint.md";
    let seed = "HEAD base\nMID line\nTAIL base\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    // Same-region conflict on the MID line.
    oplog_a.apply_user_text(&doc_a, "HEAD base\nMID edited by A\nTAIL base\n").unwrap();
    oplog_b.apply_user_text(&doc_b, "HEAD base\nMID edited by B\nTAIL base\n").unwrap();

    let (a, b) = both_block_same_region(a, b).await;
    b.set_fork_resolution(path.to_string(), Resolution::KeepTheirs);
    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;
    let resolved = "HEAD base\nMID edited by A\nTAIL base\n";
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, resolved, "A resolved");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, resolved, "B resolved");

    // Now BOTH edit DISJOINT regions (A the HEAD, B the TAIL) on the shared lineage.
    oplog_a.apply_user_text(&doc_a, "HEAD ALPHA-mark\nMID edited by A\nTAIL base\n").unwrap();
    oplog_b.apply_user_text(&doc_b, "HEAD base\nMID edited by A\nTAIL OMEGA-mark\n").unwrap();

    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converge to one merged text: a={text_a:?} b={text_b:?}");
    for t in [&text_a, &text_b] {
        assert_eq!(t.matches("ALPHA-mark").count(), 1, "A's disjoint edit once: {t:?}");
        assert_eq!(t.matches("OMEGA-mark").count(), 1, "B's disjoint edit once: {t:?}");
    }
    assert!(a.blocked_docs().is_empty(), "no spurious block on A: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "no spurious block on B: {:?}", b.blocked_docs());
}

/// DEEPER: same-region conflict resolved Keep-both (both-sides) → the
/// `conflict-` sibling note ALSO syncs to the other device. After both-sides
/// resolution the original path holds A's content on both, B's losing text
/// survives as a `conflict-` sibling on B, and driving to quiescence propagates
/// that sibling note to A as well (it's a normal indexed note). Both converge on
/// the original path AND on the sibling, no block. [sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_region_keep_both_sibling_syncs_to_peer() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/seq-keepboth.md";
    let seed = "title\nbody line\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    let a_text = "title\nbody EDITED BY A\n";
    let b_text = "title\nbody EDITED BY B\n";
    oplog_a.apply_user_text(&doc_a, a_text).unwrap();
    oplog_b.apply_user_text(&doc_b, b_text).unwrap();

    let (a, b) = both_block_same_region(a, b).await;
    // Keep-both on B: A's text wins the path, B's survives as a conflict sibling.
    b.set_fork_resolution(path.to_string(), Resolution::KeepBoth);

    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 10).await;

    // Original path: A's content on BOTH.
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, a_text, "A at the path");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_text, "B adopted A at the path");
    // The conflict sibling preserves B's losing text on B...
    let sibling_text = |oplog: &OpLog| -> Option<String> {
        oplog
            .list_doc_ids()
            .unwrap()
            .into_iter()
            .find(|id| {
                oplog
                    .path_for_doc(id)
                    .unwrap()
                    .map(|p| p.contains(".conflict-"))
                    .unwrap_or(false)
            })
            .map(|id| oplog.materialize_accepted(&id).unwrap().text)
    };
    assert_eq!(sibling_text(&oplog_b).as_deref(), Some(b_text), "B holds its losing text as a sibling");
    // ...and that sibling note syncs to A as a normal indexed note.
    assert_eq!(
        sibling_text(&oplog_a).as_deref(),
        Some(b_text),
        "the conflict sibling propagated to A"
    );
    assert!(a.blocked_docs().is_empty(), "no block on A: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "no block on B: {:?}", b.blocked_docs());
}

// --- restart re-hydration: stale block must auto-clear on re-eval ------------

/// Build a fresh [`SyncNode`] over an EXISTING vault (the same `Arc<OpLog>`),
/// modelling an app RESTART: the durable `blocked.json` is re-hydrated, so the
/// node comes up with the doc's status forced `Blocked` again (see
/// `SyncNode::new`). Shares `content_key` and a fresh keypair pinned so the node
/// is non-canonical/canonical as the caller needs is not required here. Used to
/// exercise the blocked re-eval paths (`resolve_delete_vs_edit` /
/// `resolve_rename_collision`) when the conflict has ALREADY converged
/// out-of-band — the persisted block is now stale and must auto-clear.
fn rebuild_node(content_key: &ContentKey, oplog: &Arc<OpLog>, keypair: DeviceKeypair) -> SyncNode {
    SyncNode::new(
        Arc::clone(oplog),
        SharedContentKey::new(ContentKey::from_bytes(*content_key.as_bytes())),
        keypair,
        Settings::default(),
        EnrolledPeers::new(),
    )
}

/// REGRESSION (`bug-sync-delete-vs-edit-stale-block-on-peer`): a delete-vs-edit
/// block that has CONVERGED out-of-band (resolved on the other device) must
/// auto-clear when the blocked side re-evaluates — not re-block forever. We model
/// the realistic restart: B blocks delete-vs-edit and persists it, the conflict
/// is then resolved+converged on the shared lineage, and B is REBUILT (its block
/// re-hydrates with status forced `Blocked`). On B's next round — routed to
/// `resolve_delete_vs_edit` with NO queued decision because the status is
/// Blocked — the re-eval sees the verdict is no longer a conflict (both sides
/// agree) and AUTO-CLEARS the stale block instead of re-blocking.
/// [sync-conflict-delete-vs-edit, sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_vs_edit_stale_block_auto_clears_on_reeval() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let path = "notes/dve-stale.md";
    let seed = "title\nbody line\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) = bound_pair(&key, path, seed).await;
    let kp_b = b.fingerprint();
    let _ = kp_b;
    // A deletes, B edits → both detect delete-vs-edit.
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    oplog_b.apply_user_text(&doc_b, "title\nbody EDITED BY B\n").unwrap();
    let (a, b) = both_block_delete_vs_edit(a, b).await;
    assert!(!b.blocked_docs().is_empty(), "B blocked before resolution");

    // Resolve keep-deleted on B; converge so BOTH reach the tombstone state.
    b.set_fork_resolution(path.to_string(), Resolution::KeepDeleted);
    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;
    assert!(oplog_a.materialize_accepted(&doc_a).unwrap().tombstone, "A converged to deleted");
    assert!(oplog_b.materialize_accepted(&doc_b).unwrap().tombstone, "B converged to deleted");
    drop((a, b));

    // RESTART B: a fresh node over the SAME vault re-hydrates the (now stale)
    // block from disk — status comes up forced Blocked even though the content
    // already converged to deleted out-of-band. Manually re-record the block to
    // model a persisted block that survived (the converge above cleared it; we
    // simulate the restart-with-stale-block by recording it again, no decision).
    let mut b2 = rebuild_node(&key, &oplog_b, DeviceKeypair::generate());
    let mut a2 = rebuild_node(&key, &oplog_a, DeviceKeypair::generate());
    enroll_each_other(&a2, &b2);
    b2.record_blocked_for_test(path, "delete-vs-edit", &a2.fingerprint());
    assert_eq!(
        b2.status_of_path(path),
        Some(SyncStatus::Blocked),
        "rebuilt B comes up Blocked (re-hydrated stale block)"
    );

    // B2 dials A2 → routed to resolve_delete_vs_edit with NO decision. The
    // conflict is gone (both deleted), so the re-eval must AUTO-CLEAR, not re-block.
    let addr = a2.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a2, Duration::from_secs(3));
    let rb = b2.sync_once(&addr).await.unwrap();
    let _a2 = server.await.unwrap();
    assert!(rb.blocked.is_empty(), "stale delete-vs-edit block does not re-block: {rb:?}");
    assert!(
        b2.blocked_docs().is_empty(),
        "stale delete-vs-edit block auto-cleared on re-eval: {:?}",
        b2.blocked_docs()
    );
}

/// REGRESSION (`bug-sync-rename-collision-stale-block-on-peer`): a
/// rename-collision block that no longer collides (resolved + converged
/// out-of-band) must auto-clear on re-eval rather than re-block forever. Modelled
/// via a restart: B blocks the collision, it is resolved so the lineages converge
/// at the path, then B is rebuilt with the (now stale) block re-hydrated and
/// dials A — routed to `resolve_rename_collision` with NO decision. The re-eval
/// sees the collision is gone (our doc no longer disjoint from the peer's at the
/// path) and AUTO-CLEARS. [sync-concurrent-rename-not-merged, sync-conflict-block-and-resolve]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_collision_stale_block_auto_clears_on_reeval() {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let a_content = "this is foo's content from A\n";
    let b_content = "this is bar's content from B\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        rename_collision_setup(&key, a_content, b_content).await;
    let target = "notes/target.md";
    let (a, b) = both_block_rename_collision(a, b).await;

    // Resolve keep-theirs on B; converge so both share A's lineage at target.md.
    b.set_fork_resolution(target.to_string(), Resolution::KeepTheirs);
    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 12).await;
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, a_content, "B adopted A at target");
    drop((a, b));

    // RESTART B with a re-hydrated (now stale) rename-collision block.
    let mut b2 = rebuild_node(&key, &oplog_b, DeviceKeypair::generate());
    let mut a2 = rebuild_node(&key, &oplog_a, DeviceKeypair::generate());
    enroll_each_other(&a2, &b2);
    b2.record_blocked_for_test(target, "rename-collision", &a2.fingerprint());
    assert_eq!(b2.status_of_path(target), Some(SyncStatus::Blocked), "rebuilt B Blocked");

    // B2 dials A2 → routed to resolve_rename_collision, no decision. The lineages
    // already converged at the path, so the collision is gone → auto-clear.
    let addr = a2.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a2, Duration::from_secs(3));
    let rb = b2.sync_once(&addr).await.unwrap();
    let _a2 = server.await.unwrap();
    assert!(rb.blocked.is_empty(), "stale rename-collision block does not re-block: {rb:?}");
    assert!(
        b2.blocked_docs().is_empty(),
        "stale rename-collision block auto-cleared on re-eval: {:?}",
        b2.blocked_docs()
    );
}

// ===========================================================================
// Wave 7 — structured (NON-NOTE) document types over the op-log sync path.
//
// Canvas (`.canvas` JSON), cluster-trees (markdown + `hiker.*` frontmatter),
// trails (markdown + `hiker.waypoints` YAML), and kanban boards all sync as
// their serialized bytes EXACTLY like a note: at the sync layer a doc is just
// bytes at a vault path, and the same-region conflict gate runs a char-level
// Myers diff over that text (see `core::oplog::doc::spans_overlap`). These
// scenarios exercise the REAL serialized shapes (not toy strings) so we have
// coverage that the byte-level merge + same-region detection behaves sensibly
// on JSON / YAML content, where edits are nested inside arrays/maps rather than
// on prose lines.
//
// Each structured type contributes four scenarios, all driven through the same
// both-devices-are-dialers helpers (`bound_pair`, `sync_until_settled`):
//   1. round-trip      — edit on one device converges byte-equal to the other
//   2. disjoint merge  — non-overlapping structural edits auto-merge, NO block
//   3. same-region     — both edit the SAME element → BLOCK, resolve, converge
//   4. delete          — tombstone one structured doc → propagates to the peer
//
// The seeds below are realistic serializations per:
//   canvas  — hiker-canvas/core/src/model.rs (JSON Canvas 1.0)
//   tree    — core/src/trees/store.rs (`hiker.kind: cluster-tree`)
//   trail   — core/src/trails/mod.rs (`hiker.kind: trail`, recursive waypoints)
//   kanban  — docs/kanban.md (`hiker.kind: board`, columns/cards)
// They are kept here as plain `&str` (the sync layer needs only the bytes), so
// hiker-sync gains no dependency on those crates.

/// A structured doc fixture: the path it lives at, a realistic serialized seed,
/// two DISJOINT edits (touch non-overlapping byte regions — must auto-merge),
/// and two SAME-element edits (touch one overlapping region — must block).
struct StructuredDoc {
    path: &'static str,
    seed: &'static str,
    /// Disjoint: device-A edit and device-B edit hit different byte ranges.
    disjoint_a: &'static str,
    disjoint_b: &'static str,
    /// A unique marker present in each disjoint edit, to assert it survived once.
    disjoint_marker_a: &'static str,
    disjoint_marker_b: &'static str,
    /// Same element: both edits rewrite the SAME byte region (a real conflict).
    same_a: &'static str,
    same_b: &'static str,
}

// --- canvas: JSON Canvas 1.0, pretty-printed (multi-line, tab-indented) -----
//
// Two text nodes and one edge. Disjoint edit A repositions node `n1` (its `x`);
// disjoint edit B adds a brand-new EDGE to the `edges` array — non-overlapping
// regions of the JSON. Same-region edit moves the SAME node `n1`'s coords two
// different ways.
const CANVAS: StructuredDoc = StructuredDoc {
    path: "diagrams/architecture.canvas",
    seed: "{\n\
\t\"nodes\": [\n\
\t\t{\n\
\t\t\t\"id\": \"n1\",\n\
\t\t\t\"x\": 0,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Ingest pipeline\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"n2\",\n\
\t\t\t\"x\": 400,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Vector store\"\n\
\t\t}\n\
\t],\n\
\t\"edges\": [\n\
\t\t{\n\
\t\t\t\"id\": \"e1\",\n\
\t\t\t\"fromNode\": \"n1\",\n\
\t\t\t\"toNode\": \"n2\"\n\
\t\t}\n\
\t]\n\
}\n",
    // A moves n1: x 0 -> 64. (Edits only the n1 `x` value region.)
    disjoint_a: "{\n\
\t\"nodes\": [\n\
\t\t{\n\
\t\t\t\"id\": \"n1\",\n\
\t\t\t\"x\": 64,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Ingest pipeline\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"n2\",\n\
\t\t\t\"x\": 400,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Vector store\"\n\
\t\t}\n\
\t],\n\
\t\"edges\": [\n\
\t\t{\n\
\t\t\t\"id\": \"e1\",\n\
\t\t\t\"fromNode\": \"n1\",\n\
\t\t\t\"toNode\": \"n2\"\n\
\t\t}\n\
\t]\n\
}\n",
    // B adds a second edge e2 (a new element in the `edges` array region).
    disjoint_b: "{\n\
\t\"nodes\": [\n\
\t\t{\n\
\t\t\t\"id\": \"n1\",\n\
\t\t\t\"x\": 0,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Ingest pipeline\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"n2\",\n\
\t\t\t\"x\": 400,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Vector store\"\n\
\t\t}\n\
\t],\n\
\t\"edges\": [\n\
\t\t{\n\
\t\t\t\"id\": \"e1\",\n\
\t\t\t\"fromNode\": \"n1\",\n\
\t\t\t\"toNode\": \"n2\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"e2\",\n\
\t\t\t\"fromNode\": \"n2\",\n\
\t\t\t\"toNode\": \"n1\"\n\
\t\t}\n\
\t]\n\
}\n",
    disjoint_marker_a: "\"x\": 64",
    disjoint_marker_b: "\"id\": \"e2\"",
    // Both move the SAME node n1's x to DIFFERENT values (same byte region).
    same_a: "{\n\
\t\"nodes\": [\n\
\t\t{\n\
\t\t\t\"id\": \"n1\",\n\
\t\t\t\"x\": 100,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Ingest pipeline\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"n2\",\n\
\t\t\t\"x\": 400,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Vector store\"\n\
\t\t}\n\
\t],\n\
\t\"edges\": [\n\
\t\t{\n\
\t\t\t\"id\": \"e1\",\n\
\t\t\t\"fromNode\": \"n1\",\n\
\t\t\t\"toNode\": \"n2\"\n\
\t\t}\n\
\t]\n\
}\n",
    same_b: "{\n\
\t\"nodes\": [\n\
\t\t{\n\
\t\t\t\"id\": \"n1\",\n\
\t\t\t\"x\": 200,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Ingest pipeline\"\n\
\t\t},\n\
\t\t{\n\
\t\t\t\"id\": \"n2\",\n\
\t\t\t\"x\": 400,\n\
\t\t\t\"y\": 0,\n\
\t\t\t\"width\": 240,\n\
\t\t\t\"height\": 120,\n\
\t\t\t\"type\": \"text\",\n\
\t\t\t\"text\": \"Vector store\"\n\
\t\t}\n\
\t],\n\
\t\"edges\": [\n\
\t\t{\n\
\t\t\t\"id\": \"e1\",\n\
\t\t\t\"fromNode\": \"n1\",\n\
\t\t\t\"toNode\": \"n2\"\n\
\t\t}\n\
\t]\n\
}\n",
};

// --- cluster-tree: markdown + `hiker.kind: cluster-tree` frontmatter --------
//
// A flat node list under `hiker.nodes` (children implied by `parent`). Disjoint
// edit A renames leaf `n1`; disjoint edit B renames leaf `n2` (different list
// entries, distinct byte regions). Same-region edits rename the SAME leaf `n1`.
const TREE: StructuredDoc = StructuredDoc {
    path: "cluster-trees/semantic.md",
    seed: "---\n\
hiker:\n\
  kind: cluster-tree\n\
  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n\
  name: Semantic\n\
  source: review:confirm\n\
  state: draft\n\
  created_at_ms: 1779732983626\n\
  nodes:\n\
  - id: root\n\
    kind: cluster\n\
    name: 'Vault: notes'\n\
    confidence: 1.0\n\
  - id: n1\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-1\n\
      path: notes/alpha.md\n\
    name: Alpha cluster\n\
    confidence: 0.9\n\
  - id: n2\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-2\n\
      path: notes/beta.md\n\
    name: Beta cluster\n\
    confidence: 0.8\n\
---\n\
<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n",
    disjoint_a: "---\n\
hiker:\n\
  kind: cluster-tree\n\
  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n\
  name: Semantic\n\
  source: review:confirm\n\
  state: draft\n\
  created_at_ms: 1779732983626\n\
  nodes:\n\
  - id: root\n\
    kind: cluster\n\
    name: 'Vault: notes'\n\
    confidence: 1.0\n\
  - id: n1\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-1\n\
      path: notes/alpha.md\n\
    name: Alpha RENAMED-A\n\
    confidence: 0.9\n\
  - id: n2\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-2\n\
      path: notes/beta.md\n\
    name: Beta cluster\n\
    confidence: 0.8\n\
---\n\
<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n",
    disjoint_b: "---\n\
hiker:\n\
  kind: cluster-tree\n\
  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n\
  name: Semantic\n\
  source: review:confirm\n\
  state: draft\n\
  created_at_ms: 1779732983626\n\
  nodes:\n\
  - id: root\n\
    kind: cluster\n\
    name: 'Vault: notes'\n\
    confidence: 1.0\n\
  - id: n1\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-1\n\
      path: notes/alpha.md\n\
    name: Alpha cluster\n\
    confidence: 0.9\n\
  - id: n2\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-2\n\
      path: notes/beta.md\n\
    name: Beta RENAMED-B\n\
    confidence: 0.8\n\
---\n\
<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n",
    disjoint_marker_a: "Alpha RENAMED-A",
    disjoint_marker_b: "Beta RENAMED-B",
    // Both rename the SAME leaf n1 differently.
    same_a: "---\n\
hiker:\n\
  kind: cluster-tree\n\
  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n\
  name: Semantic\n\
  source: review:confirm\n\
  state: draft\n\
  created_at_ms: 1779732983626\n\
  nodes:\n\
  - id: root\n\
    kind: cluster\n\
    name: 'Vault: notes'\n\
    confidence: 1.0\n\
  - id: n1\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-1\n\
      path: notes/alpha.md\n\
    name: Alpha PICKED-BY-A\n\
    confidence: 0.9\n\
  - id: n2\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-2\n\
      path: notes/beta.md\n\
    name: Beta cluster\n\
    confidence: 0.8\n\
---\n\
<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n",
    same_b: "---\n\
hiker:\n\
  kind: cluster-tree\n\
  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n\
  name: Semantic\n\
  source: review:confirm\n\
  state: draft\n\
  created_at_ms: 1779732983626\n\
  nodes:\n\
  - id: root\n\
    kind: cluster\n\
    name: 'Vault: notes'\n\
    confidence: 1.0\n\
  - id: n1\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-1\n\
      path: notes/alpha.md\n\
    name: Alpha PICKED-BY-B\n\
    confidence: 0.9\n\
  - id: n2\n\
    parent: root\n\
    kind: leaf\n\
    note:\n\
      id: note-2\n\
      path: notes/beta.md\n\
    name: Beta cluster\n\
    confidence: 0.8\n\
---\n\
<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n",
};

// --- trail: markdown + recursive `hiker.waypoints` YAML ---------------------
//
// A trail-doc with two root waypoints, the first carrying a nested child
// subtree. Disjoint edit A appends a child to the FIRST root's subtree; disjoint
// edit B appends a NEW second-root sibling — different subtrees, distinct byte
// regions. Same-region edits both repoint the SAME waypoint's `path`.
const TRAIL: StructuredDoc = StructuredDoc {
    path: "trails/research.md",
    seed: "---\n\
hiker:\n\
  kind: trail\n\
  waypoints:\n\
  - path: trails/research/r1--AAAAAA.md\n\
    waypoints:\n\
    - path: trails/research/c1--BBBBBB.md\n\
  - path: trails/research/r2--DDDDDD.md\n\
---\n\
trail prose body\n",
    // A adds a child under the FIRST root's subtree.
    disjoint_a: "---\n\
hiker:\n\
  kind: trail\n\
  waypoints:\n\
  - path: trails/research/r1--AAAAAA.md\n\
    waypoints:\n\
    - path: trails/research/c1--BBBBBB.md\n\
    - path: trails/research/c2--ADDEDA.md\n\
  - path: trails/research/r2--DDDDDD.md\n\
---\n\
trail prose body\n",
    // B adds a new root-level sibling waypoint at the END.
    disjoint_b: "---\n\
hiker:\n\
  kind: trail\n\
  waypoints:\n\
  - path: trails/research/r1--AAAAAA.md\n\
    waypoints:\n\
    - path: trails/research/c1--BBBBBB.md\n\
  - path: trails/research/r2--DDDDDD.md\n\
  - path: trails/research/r3--ADDEDB.md\n\
---\n\
trail prose body\n",
    disjoint_marker_a: "c2--ADDEDA",
    disjoint_marker_b: "r3--ADDEDB",
    // Both repoint the SAME first child waypoint's path differently.
    same_a: "---\n\
hiker:\n\
  kind: trail\n\
  waypoints:\n\
  - path: trails/research/r1--AAAAAA.md\n\
    waypoints:\n\
    - path: trails/research/c1--PICKEDA.md\n\
  - path: trails/research/r2--DDDDDD.md\n\
---\n\
trail prose body\n",
    same_b: "---\n\
hiker:\n\
  kind: trail\n\
  waypoints:\n\
  - path: trails/research/r1--AAAAAA.md\n\
    waypoints:\n\
    - path: trails/research/c1--PICKEDB.md\n\
  - path: trails/research/r2--DDDDDD.md\n\
---\n\
trail prose body\n",
};

// --- kanban board: markdown + `hiker.kind: board` columns/cards YAML --------
//
// Three columns (Todo / Doing / Done). Disjoint edit A adds a card to `Todo`;
// disjoint edit B adds a card to `Done` — different columns, distinct byte
// regions. Same-region edits both rewrite the SAME existing card's path.
const KANBAN: StructuredDoc = StructuredDoc {
    path: "boards/roadmap.md",
    seed: "---\n\
hiker:\n\
  kind: board\n\
  columns:\n\
  - name: Todo\n\
    cards:\n\
    - { path: research/raptor-paper.md }\n\
  - name: Doing\n\
    cards:\n\
    - { path: work/migration.md }\n\
  - name: Done\n\
    cards: []\n\
---\n\
# Q3 Roadmap\n\
\n\
Board prose framing.\n",
    // A appends a card to the Todo column.
    disjoint_a: "---\n\
hiker:\n\
  kind: board\n\
  columns:\n\
  - name: Todo\n\
    cards:\n\
    - { path: research/raptor-paper.md }\n\
    - { path: inbox/CARD-ADDED-A.md }\n\
  - name: Doing\n\
    cards:\n\
    - { path: work/migration.md }\n\
  - name: Done\n\
    cards: []\n\
---\n\
# Q3 Roadmap\n\
\n\
Board prose framing.\n",
    // B appends a card to the (empty) Done column.
    disjoint_b: "---\n\
hiker:\n\
  kind: board\n\
  columns:\n\
  - name: Todo\n\
    cards:\n\
    - { path: research/raptor-paper.md }\n\
  - name: Doing\n\
    cards:\n\
    - { path: work/migration.md }\n\
  - name: Done\n\
    cards:\n\
    - { path: archive/CARD-ADDED-B.md }\n\
---\n\
# Q3 Roadmap\n\
\n\
Board prose framing.\n",
    disjoint_marker_a: "CARD-ADDED-A",
    disjoint_marker_b: "CARD-ADDED-B",
    // Both repoint the SAME Doing card's path differently.
    same_a: "---\n\
hiker:\n\
  kind: board\n\
  columns:\n\
  - name: Todo\n\
    cards:\n\
    - { path: research/raptor-paper.md }\n\
  - name: Doing\n\
    cards:\n\
    - { path: work/migration-PICKEDA.md }\n\
  - name: Done\n\
    cards: []\n\
---\n\
# Q3 Roadmap\n\
\n\
Board prose framing.\n",
    same_b: "---\n\
hiker:\n\
  kind: board\n\
  columns:\n\
  - name: Todo\n\
    cards:\n\
    - { path: research/raptor-paper.md }\n\
  - name: Doing\n\
    cards:\n\
    - { path: work/migration-PICKEDB.md }\n\
  - name: Done\n\
    cards: []\n\
---\n\
# Q3 Roadmap\n\
\n\
Board prose framing.\n",
};

// --- scenario 1: round-trip per type ----------------------------------------

/// Generic round-trip: A creates the structured doc, B adopts it (shared
/// lineage via `bound_pair`), then A edits it and the edit converges to B
/// byte-equal, no block. Asserts on BOTH sides. The `kind` tag rides in `meta`
/// but the sync path treats the body as plain bytes — this proves a non-note
/// serialized body survives the adopt + delta path intact.
async fn roundtrip_structured(doc: &StructuredDoc) {
    let key = ContentKey::generate();
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, doc.path, doc.seed).await;

    // Both materialize the exact seed bytes after binding.
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, doc.seed);
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, doc.seed);

    // A edits; drive to quiescence so B picks it up.
    oplog_a.apply_user_text(&doc_a, doc.disjoint_a).unwrap();
    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;
    drop((a, b));

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, doc.disjoint_a, "A holds its edited structured body");
    assert_eq!(text_b, text_a, "B converged byte-equal to A's structured body");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_round_trip_converges() {
    roundtrip_structured(&CANVAS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_round_trip_converges() {
    roundtrip_structured(&TREE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trail_round_trip_converges() {
    roundtrip_structured(&TRAIL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_round_trip_converges() {
    roundtrip_structured(&KANBAN).await;
}

// --- scenario 2: disjoint structural edits auto-merge (NO block) ------------

/// Generic disjoint-merge: A and B (bound on a shared lineage) make DISJOINT
/// structural edits (different byte regions of the JSON/YAML). Driving to
/// quiescence must auto-merge BOTH edits with NO block on either side — the
/// desired merge behavior for disjoint edits. Asserts both markers survive
/// exactly once on BOTH sides and `blocked_docs()` is empty.
async fn disjoint_structured_auto_merges(doc: &StructuredDoc) {
    let key = ContentKey::generate();
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, doc.path, doc.seed).await;

    oplog_a.apply_user_text(&doc_a, doc.disjoint_a).unwrap();
    oplog_b.apply_user_text(&doc_b, doc.disjoint_b).unwrap();

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    // No block on either side — disjoint edits must merge, not conflict.
    assert!(a.blocked_docs().is_empty(), "A not blocked on disjoint edit: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B not blocked on disjoint edit: {:?}", b.blocked_docs());

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converge to one merged structured body");
    for text in [&text_a, &text_b] {
        assert_eq!(
            text.matches(doc.disjoint_marker_a).count(),
            1,
            "A's structural edit ({}) present exactly once: {text:?}",
            doc.disjoint_marker_a
        );
        assert_eq!(
            text.matches(doc.disjoint_marker_b).count(),
            1,
            "B's structural edit ({}) present exactly once: {text:?}",
            doc.disjoint_marker_b
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_disjoint_edits_auto_merge() {
    disjoint_structured_auto_merges(&CANVAS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_disjoint_edits_auto_merge() {
    disjoint_structured_auto_merges(&TREE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trail_disjoint_edits_auto_merge() {
    disjoint_structured_auto_merges(&TRAIL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_disjoint_edits_auto_merge() {
    disjoint_structured_auto_merges(&KANBAN).await;
}

// --- scenario 3: same-region structural conflict blocks, then resolves ------

/// Generic same-region conflict: A and B (bound) both edit the SAME structural
/// element (same node coords / same waypoint path / same card path) DIFFERENTLY.
/// Each device, dialing in turn, must independently BLOCK with reason
/// `same-region` (proving the char-level overlap detection fires on JSON/YAML
/// content the same way it does on prose). Then the user resolves keep-theirs on
/// ONE side and driving to quiescence converges BOTH sides to the peer's content
/// AND clears BOTH blocks. Asserts on BOTH sides throughout.
async fn same_region_structured_blocks_then_resolves(doc: &StructuredDoc) {
    use hiker_sync::identity::Resolution;
    let key = ContentKey::generate();
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, doc.path, doc.seed).await;

    oplog_a.apply_user_text(&doc_a, doc.same_a).unwrap();
    oplog_b.apply_user_text(&doc_b, doc.same_b).unwrap();

    // BOTH devices independently detect the same-region overlap and BLOCK.
    let (a, b) = both_block_same_region(a, b).await;
    assert!(
        a.blocked_docs().iter().any(|d| d.path == doc.path && d.reason == "same-region"),
        "A blocked same-region on structured content: {:?}",
        a.blocked_docs()
    );
    assert!(
        b.blocked_docs().iter().any(|d| d.path == doc.path && d.reason == "same-region"),
        "B blocked same-region on structured content: {:?}",
        b.blocked_docs()
    );
    // The peer delta was HELD, not folded — each side stays at its own edit.
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        doc.same_b,
        "B held its own structured edit (no silent interleave)"
    );

    // Resolve keep-theirs on B ONLY; drive to quiescence. Both converge to A's
    // (theirs, from B's view) content and BOTH blocks clear.
    b.set_fork_resolution(doc.path.to_string(), Resolution::KeepTheirs);
    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 12).await;

    assert!(a.blocked_docs().is_empty(), "A's block cleared: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B's block cleared: {:?}", b.blocked_docs());
    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, doc.same_a, "A keeps its (theirs = A's) resolved content");
    assert_eq!(text_b, doc.same_a, "B adopted A's content on keep-theirs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_same_node_edit_blocks_then_resolves() {
    same_region_structured_blocks_then_resolves(&CANVAS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_same_node_edit_blocks_then_resolves() {
    same_region_structured_blocks_then_resolves(&TREE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trail_same_waypoint_edit_blocks_then_resolves() {
    same_region_structured_blocks_then_resolves(&TRAIL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_same_card_edit_blocks_then_resolves() {
    same_region_structured_blocks_then_resolves(&KANBAN).await;
}

// --- scenario 4: delete a structured doc propagates as a tombstone ----------

/// Generic delete-propagation: A and B share a lineage on the structured doc,
/// then A tombstones it. Driving to quiescence the tombstone propagates to B
/// (B's `materialize_accepted` reports `tombstone`), with NO block, and the
/// tombstone is stable across extra rounds (no resurrection). Asserts on BOTH
/// sides. Mirrors `tombstone_propagates_over_shared_lineage` for non-note bytes.
async fn delete_structured_propagates(doc: &StructuredDoc) {
    let key = ContentKey::generate();
    let (mut a, oplog_a, doc_a, mut b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, doc.path, doc.seed).await;
    assert!(!oplog_b.materialize_accepted(&doc_b).unwrap().tombstone, "B alive at first");

    // A tombstones the structured doc.
    oplog_a.tombstone_document(&doc_a, &Author::User).unwrap();
    assert!(oplog_a.materialize_accepted(&doc_a).unwrap().tombstone, "A tombstoned");

    // Round 1: B pulls the tombstone delta.
    let addr = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr).await.unwrap();
    assert!(r1.blocked.is_empty(), "tombstone delta not blocked: {r1:?}");
    let a = server.await.unwrap();
    assert!(
        oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B sees the tombstone after the synced delete of a structured doc"
    );

    // Extra rounds: tombstone is stable on BOTH sides (no resurrection).
    let (_a, _b) = drive_bidirectional(a, b, 3).await;
    assert!(
        oplog_a.materialize_accepted(&doc_a).unwrap().tombstone,
        "A stays tombstoned across extra rounds"
    );
    assert!(
        oplog_b.materialize_accepted(&doc_b).unwrap().tombstone,
        "B stays tombstoned across extra rounds"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_delete_propagates() {
    delete_structured_propagates(&CANVAS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_delete_propagates() {
    delete_structured_propagates(&TREE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trail_delete_propagates() {
    delete_structured_propagates(&TRAIL).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_delete_propagates() {
    delete_structured_propagates(&KANBAN).await;
}

// --- scenario 2 (probe): COMPACT single-line JSON still merges disjoint edits
//
// The same-region gate runs a CHAR-LEVEL Myers diff (`core::oplog::doc::
// spans_overlap`), NOT a line-level one. A natural worry for `.canvas` is that a
// tool serializing the whole canvas on ONE line would make every edit "the same
// line" and thus spuriously conflict. This probe proves it does NOT: with a
// COMPACT single-line canvas, A bumps node n1's `x` and B bumps node n2's `x`
// (two distinct byte spans on the same physical line). They must auto-merge with
// NO block — confirming the detection is byte-range fine-grained, so structured
// docs do not need a coarser/finer special case for compact serializations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_compact_single_line_disjoint_edits_auto_merge() {
    let key = ContentKey::generate();
    let path = "diagrams/compact.canvas";
    let seed = "{\"nodes\":[{\"id\":\"n1\",\"x\":0,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"A\"},{\"id\":\"n2\",\"x\":400,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"B\"}],\"edges\":[]}\n";
    // A bumps n1.x (0 -> 64); B bumps n2.x (400 -> 480). Distinct byte spans on
    // the same single line.
    let edit_a = "{\"nodes\":[{\"id\":\"n1\",\"x\":64,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"A\"},{\"id\":\"n2\",\"x\":400,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"B\"}],\"edges\":[]}\n";
    let edit_b = "{\"nodes\":[{\"id\":\"n1\",\"x\":0,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"A\"},{\"id\":\"n2\",\"x\":480,\"y\":0,\"width\":240,\"height\":120,\"type\":\"text\",\"text\":\"B\"}],\"edges\":[]}\n";

    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;
    oplog_a.apply_user_text(&doc_a, edit_a).unwrap();
    oplog_b.apply_user_text(&doc_b, edit_b).unwrap();

    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    assert!(a.blocked_docs().is_empty(), "A not blocked (char-level detection): {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B not blocked (char-level detection): {:?}", b.blocked_docs());
    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(text_a, text_b, "both converge to one merged compact JSON");
    // Both distinct value edits survived on a single line.
    assert!(text_a.contains("\"n1\",\"x\":64"), "n1 moved by A survives: {text_a:?}");
    assert!(text_a.contains("\"n2\",\"x\":480"), "n2 moved by B survives: {text_a:?}");
}

// === bug-sync-new-note-first-content-not-synced — repro scenarios ===========
//
// User-reported real-app bug: create a NEW note on A → type content → save. The
// note IS created on peer B, but EMPTY — the first content commit does not
// propagate. A second save (re-dirty + save) DOES propagate. So the FIRST
// content-after-create fails to reach B; the SECOND succeeds.
//
// These scenarios isolate the mechanism at the sync layer. Three plausible
// shapes (a)/(b)/(c) per the bug brief; the FAILING one localizes the cause.

/// (a) FIRST-CONTACT THEN CONTENT, no prior sync. A creates the doc empty, then
/// (the first save) applies the content — all BEFORE B has ever synced. B then
/// pulls for the first time. Because the content is already in A's `accepted`
/// before the round, the `LineageBase` A serves (`export_state`) carries the
/// content, so a single settle must land it on B. This is the create-then-save-
/// then-first-sync ordering; it is expected to PASS (content present at base).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_note_first_content_first_contact_after_save() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/fresh-create.md";
    // Create empty (the new-note create), then the FIRST save lands content —
    // both before B ever syncs.
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();
    let content = "first typed content\nsecond line\n";
    oplog_a.apply_user_text(&doc_a, content).unwrap();
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, content);

    // B's very first pull: no local replica → create_local_for + adopt_from_peer
    // → StateRequest/LineageBase → adopt_lineage. The base carries the content.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    let rb = b.sync_once(&addr_a).await.unwrap();
    let _a = server_a.await.unwrap();

    assert!(rb.blocked.is_empty(), "first contact must not block: {rb:?}");
    let doc_b = oplog_b
        .doc_id_for_path(path)
        .unwrap()
        .expect("B has the doc after first contact");
    let got = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got, content, "B materializes A's content on first contact: {got:?}");
}

/// (b) CREATE-TIME EMPTY SYNC, THEN CONTENT. A creates empty; B syncs and adopts
/// the EMPTY base (B now Bound at empty). THEN A applies the first content. B
/// pulls again over the now-shared lineage (the bound-doc delta path:
/// `export_since(B's SV)`). The content delta MUST reach B in this single round.
///
/// THIS is the real-app ordering: a fresh note often round-trips to the peer
/// (debounced poke / poll) while still empty — the create is visible — and only
/// then does the user's first typed content commit. If the SV watermark after
/// adopting an empty base does not let `export_since` deliver the subsequent
/// content delta, the FIRST content save is silently dropped on B. This is the
/// scenario the brief flags as the likely culprit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_note_empty_synced_then_first_content() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/empty-then-content.md";
    // A creates the note EMPTY (the new-note create, before any typing).
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();

    // Round 1: B adopts A's EMPTY base and binds. Both sides now share the
    // lineage at empty content.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    let r1 = b.sync_once(&addr_a).await.unwrap();
    let mut a = server_a.await.unwrap();
    assert!(r1.blocked.is_empty(), "empty first contact must not block: {r1:?}");

    let doc_b = oplog_b
        .doc_id_for_path(path)
        .unwrap()
        .expect("B has the (empty) doc after round 1");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, "", "B at empty base");
    assert_eq!(b.status_of_path(path), Some(SyncStatus::Bound), "B bound at empty");

    // The FIRST content save on A (this is what the user types after the note
    // exists on both devices).
    let content = "the first typed content\nthat must sync\n";
    oplog_a.apply_user_text(&doc_a, content).unwrap();
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, content);

    // Round 2: B pulls the content delta over the shared lineage (bound path).
    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a2 = spawn_responder(a, Duration::from_secs(3));
    let r2 = b.sync_once(&addr_a2).await.unwrap();
    let _a = server_a2.await.unwrap();
    assert!(r2.blocked.is_empty(), "content delta must not block: {r2:?}");

    // THE ASSERTION: B has the content after the FIRST content save — no second
    // commit needed. If the bug reproduces here, B is still empty.
    let got = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(
        got, content,
        "B must materialize A's FIRST content over the shared lineage (not empty): {got:?}"
    );
}

/// (c) DRIVE-TO-QUIESCENCE variant of (b): create empty, settle, type content,
/// settle again — does ONE settle after the first content commit converge B, or
/// does it need a SECOND commit? `sync_until_settled` runs alternating rounds;
/// it asserts quiescence (equal text both sides) within the cap. If the first
/// content needs a second commit to propagate, this fails to settle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_note_empty_then_content_settles_in_one() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/quiesce-create.md";
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();

    // Settle the empty create across both devices.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a).await.unwrap();
    let a = server_a.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().expect("B has empty doc");

    // First content save, then drive to quiescence — must converge to `content`.
    let content = "quiescence content line\n";
    oplog_a.apply_user_text(&doc_a, content).unwrap();
    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;

    let got = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got, content, "B converged to A's first content within one settle: {got:?}");
}

/// (b') VARIANT: both A and B independently hold the path at EMPTY (e.g. an
/// empty `.md` reached B out-of-band), neither bound. A then types its first
/// content. On B's first-contact pull, B has a LOCAL empty replica, so the
/// dialer takes the `Some(local)` + `None` status → `classify` path rather than
/// `create_local_for`/adopt. A is ahead of empty (empty is in A's history), so
/// it must be a clean FastForwardAdoptPeer — B adopts A's content, never a fork,
/// never stuck empty. This models the new-note-first-content with a pre-existing
/// empty local replica on the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_note_first_content_peer_holds_empty_replica() {
    let key = ContentKey::generate();
    let (a, oplog_a, _da) = mk_node(&key);
    let (b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/both-empty.md";
    // Both independently create the path EMPTY (distinct lineages, identical
    // empty bytes). Neither has synced.
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "note", "", &Author::User).unwrap();

    // A types its FIRST content (the first save), before any sync.
    let content = "first content over a pre-existing empty peer replica\n";
    oplog_a.apply_user_text(&doc_a, content).unwrap();

    // Drive to quiescence: B must adopt A's content (A is strictly ahead of the
    // shared empty seed). One settle, no second commit, no fork.
    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;
    assert!(a.blocked_docs().is_empty(), "A not blocked: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B not blocked: {:?}", b.blocked_docs());

    let got = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got, content, "B converged to A's first content (not empty): {got:?}");
}

/// REGRESSION for `bug-canvas-dirty-save-and-delete-sync`: a `.canvas` document
/// whose committed JSON DELETES a node syncs to the peer in ONE settle — no
/// "second change" required.
///
/// Before the fix the canvas wrote each edit straight to `accepted` per-edit
/// (and a stuck dirty buffer meant the delete often needed a follow-up change to
/// propagate). The canvas now rides the SAME op-log working/commit model as a
/// note: a node-delete is an ordinary committed text edit (the canonical JSON
/// minus that node), so it travels over the existing op-log sync exactly like a
/// note edit. This pins that: a canvas's delete-committed JSON converges in one
/// `sync_until_settled`. The canvas JSON is treated as opaque document text by
/// the transport (kind is irrelevant to the text merge), so a node-removal is
/// just a localized text delete over the canonical serialization.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_node_delete_syncs_in_one_settle() {
    let key = ContentKey::generate();
    let path = "boards/plan.canvas";
    // Two-node canvas in canonical (tab-indented, stable-key) JSON shape — the
    // committed starting document on both bound devices.
    let two_nodes = "{\n\t\"nodes\": [\n\t\t{\n\t\t\t\"id\": \"n1\",\n\t\t\t\"type\": \"file\",\n\t\t\t\"file\": \"a.md\",\n\t\t\t\"x\": 0,\n\t\t\t\"y\": 0,\n\t\t\t\"width\": 300,\n\t\t\t\"height\": 200\n\t\t},\n\t\t{\n\t\t\t\"id\": \"n2\",\n\t\t\t\"type\": \"file\",\n\t\t\t\"file\": \"b.md\",\n\t\t\t\"x\": 400,\n\t\t\t\"y\": 0,\n\t\t\t\"width\": 300,\n\t\t\t\"height\": 200\n\t\t}\n\t],\n\t\"edges\": []\n}";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, two_nodes).await;

    // A deletes node n2 (the canvas commits the canonical JSON minus n2) — the
    // shape `commit_working` folds to `accepted` after a delete edit.
    let one_node = "{\n\t\"nodes\": [\n\t\t{\n\t\t\t\"id\": \"n1\",\n\t\t\t\"type\": \"file\",\n\t\t\t\"file\": \"a.md\",\n\t\t\t\"x\": 0,\n\t\t\t\"y\": 0,\n\t\t\t\"width\": 300,\n\t\t\t\"height\": 200\n\t\t}\n\t],\n\t\"edges\": []\n}";
    oplog_a.apply_user_text(&doc_a, one_node).unwrap();

    // One settle: B must converge to the deleted-node JSON, no second change.
    let (a, b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;
    assert!(a.blocked_docs().is_empty(), "A not blocked: {:?}", a.blocked_docs());
    assert!(b.blocked_docs().is_empty(), "B not blocked: {:?}", b.blocked_docs());

    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, one_node, "B converged to the delete-committed canvas JSON: {got_b:?}");
    assert!(!got_b.contains("n2"), "the deleted node is gone on the peer");
    assert!(got_b.contains("n1"), "the surviving node remains on the peer");
}

// --- Save = commit working + sync (op-log-working-layer) -------------------

/// The default sync-on-save flow: an UNSAVED (`working`-only) edit does NOT reach
/// the peer — only `accepted` syncs — and once an explicit save (`commit_working`)
/// folds it, it propagates. Over the REAL transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_only_edit_propagates_only_after_save() {
    let key = ContentKey::generate();
    let path = "notes/onsave.md";
    let seed = "shared baseline\n";
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, path, seed).await;

    // An unsaved edit in `working` only — `accepted` is untouched, so it does not
    // cross the wire.
    oplog_a
        .apply_working_edit(&doc_a, seed.len(), 0, "uncommitted edit\n")
        .unwrap();
    assert_eq!(
        oplog_a.materialize_accepted(&doc_a).unwrap().text,
        seed,
        "accepted unchanged while the edit is working-only"
    );

    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        seed,
        "the working-only edit does NOT reach the peer before a save"
    );

    // An explicit save folds `working` into `accepted`; it then propagates.
    assert!(oplog_a.commit_working(&doc_a).unwrap(), "the working edit commits on save");
    let want = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(want, "shared baseline\nuncommitted edit\n");
    let (_a, _b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        want,
        "after the explicit save, the edit reaches the peer"
    );
}

// === Canvas sync corruption repro =========================================
//
// A `.canvas` file rides the SAME whole-file-as-text substrate as a
// note (`docs/canvas.md` [canvas-doc-kind]) — the JSON text IS the document; a
// canvas edit re-serializes the model and commits the new full text, diffed
// into localized spans exactly like a note save (`apply_user_text`). So these
// tests drive the canvas binding faithfully by committing canonical-JSON text.
//
// The reported corruption is digit-level: numeric coordinates gain spliced /
// duplicated digits (`5828` -> `582828`, `8116` -> `811116`) and garbage gets
// prepended to byte 0 (`1113872{`). That is the fingerprint of two DISJOINT
// lineages interleaving near-identical numeric-dense bytes. Every coordinate in
// these fixtures is <= 4 digits, so a digit run longer than 4 is unambiguous
// corruption — the invariant the checker enforces.

#[derive(Clone)]
struct CNode {
    id: String,
    file: String,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// Serialize canvas nodes to canonical-shaped JSON (tab-indented, stable key
/// order), mirroring `Canvas::to_canonical_json`'s output shape so the localized
/// diff sees realistic JSON token boundaries. Edges are empty.
fn canvas_json(nodes: &[CNode]) -> String {
    let mut s = String::from("{\n\t\"nodes\": [\n");
    for (i, n) in nodes.iter().enumerate() {
        s.push_str("\t\t{\n");
        s.push_str(&format!("\t\t\t\"id\": \"{}\",\n", n.id));
        s.push_str(&format!("\t\t\t\"x\": {},\n", n.x));
        s.push_str(&format!("\t\t\t\"y\": {},\n", n.y));
        s.push_str(&format!("\t\t\t\"width\": {},\n", n.w));
        s.push_str(&format!("\t\t\t\"height\": {},\n", n.h));
        s.push_str(&format!("\t\t\t\"file\": \"{}\",\n", n.file));
        s.push_str("\t\t\t\"type\": \"file\"\n");
        s.push_str(if i + 1 < nodes.len() { "\t\t},\n" } else { "\t\t}\n" });
    }
    s.push_str("\t],\n\t\"edges\": []\n}\n");
    s
}

/// A grid of `n` file nodes; every coordinate is <= 4 digits (x,y) and width /
/// height are 3 digits, so any 5+ digit run in the synced output is corruption.
fn grid_canvas(n: usize) -> Vec<CNode> {
    (0..n)
        .map(|i| CNode {
            id: format!("n{i}"),
            file: format!("notes/node-{i}.md"),
            x: 100 + (i % 5) as i64 * 300,
            y: 100 + (i / 5) as i64 * 200,
            w: 260,
            h: 140,
        })
        .collect()
}

/// Longest run of consecutive ASCII digits in `s`. With <=4-digit fixtures, a
/// result > 4 means digits were spliced/duplicated into a number (the reported
/// `5828` -> `582828` corruption).
fn max_digit_run(s: &str) -> usize {
    let mut max = 0usize;
    let mut cur = 0usize;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            cur += 1;
            max = max.max(cur);
        } else {
            cur = 0;
        }
    }
    max
}

/// Assert `text` is an uncorrupted canvas: parses as JSON, holds exactly
/// `expect_nodes` nodes, and has no spliced/duplicated digits.
fn assert_canvas_intact(text: &str, expect_nodes: usize, who: &str) {
    let run = max_digit_run(text);
    assert!(
        run <= 4,
        "{who}: digit-splice corruption — {run}-digit run in synced canvas (all fixture \
         coords are <=4 digits). Text:\n{text}"
    );
    let v: serde_json::Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("{who}: synced canvas is not valid JSON ({e}):\n{text}"));
    let got = v["nodes"].as_array().map_or(0, Vec::len);
    assert_eq!(got, expect_nodes, "{who}: node count changed (corruption):\n{text}");
}

/// Baseline: two vaults INDEPENDENTLY seed the SAME canvas (disjoint lineages),
/// then sync to convergence. Mirrors `independent_lineages_identical_content_do_not_duplicate`
/// but for a numeric-dense `.canvas`. Both sides must converge to the single
/// canvas with no digit-splice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_independent_identical_lineages_do_not_corrupt() {
    let key = ContentKey::generate();
    let (a, oplog_a, _da) = mk_node(&key);
    let (b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "boards/graph.canvas";
    let nodes = grid_canvas(20);
    let json = canvas_json(&nodes);
    let doc_a = oplog_a.create_document(path, "canvas", &json, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "canvas", &json, &Author::User).unwrap();

    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_canvas_intact(&text_a, 20, "A");
    assert_canvas_intact(&text_b, 20, "B");
    assert_eq!(text_a, text_b, "both converge to one canvas");
}

/// Two bound (shared-lineage) devices move DIFFERENT nodes concurrently, then
/// sync. Disjoint-region edits must auto-merge with no digit-splice. This
/// exercises the char-level `multi_span_delta` localization on numeric-dense
/// JSON, where a naive char diff can align digits ACROSS distinct number tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_concurrent_disjoint_node_moves_do_not_corrupt() {
    let key = ContentKey::generate();
    let (a, oplog_a, _da) = mk_node(&key);
    let (b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "boards/graph.canvas";
    let nodes = grid_canvas(20);
    let doc_a = oplog_a
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();
    let doc_b = oplog_b
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();

    // Establish a shared lineage first.
    let (a, b) = sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;

    // Concurrent disjoint moves: A moves node 2, B moves node 17.
    let mut na = nodes.clone();
    na[2].x = 9001;
    na[2].y = 8002;
    oplog_a.apply_user_text(&doc_a, &canvas_json(&na)).unwrap();

    let mut nb = nodes.clone();
    nb[17].x = 7003;
    nb[17].y = 6004;
    oplog_b.apply_user_text(&doc_b, &canvas_json(&nb)).unwrap();

    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_canvas_intact(&text_a, 20, "A");
    assert_canvas_intact(&text_b, 20, "B");
    assert_eq!(text_a, text_b, "both converge after disjoint concurrent moves");
    // Both edits survived.
    assert!(text_a.contains("9001") && text_a.contains("7003"), "both moves present: {text_a}");
}

/// Independent identical canvases, then concurrent edits land DURING the
/// multi-round adoption handshake (one direction synced, then BOTH edit before
/// the handshake completes). The dangerous window: a side bound while lineages
/// are still disjoint would take a whole-doc cross-lineage delta. Whatever the
/// outcome (converge or clean block), neither side may end digit-spliced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_edits_during_adoption_handshake_do_not_corrupt() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "boards/graph.canvas";
    let nodes = grid_canvas(20);
    let doc_a = oplog_a
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();
    let doc_b = oplog_b
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();

    // ONE direction only: B dials A (B adopts A on the Identical handshake; A is
    // canonical and not yet bound — the half-open window).
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a).await.unwrap();
    a = server_a.await.unwrap();

    // Now BOTH edit concurrently, mid-handshake.
    let mut na = nodes.clone();
    na[1].x = 9001;
    oplog_a.apply_user_text(&doc_a, &canvas_json(&na)).unwrap();
    let mut nb = nodes.clone();
    nb[18].x = 7003;
    oplog_b.apply_user_text(&doc_b, &canvas_json(&nb)).unwrap();

    // Drive several full rounds; either converges or blocks, but must not corrupt.
    for _ in 0..8 {
        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();

        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        b.sync_once(&addr_a).await.unwrap();
        a = server_a.await.unwrap();
    }

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_canvas_intact(&text_a, 20, "A");
    assert_canvas_intact(&text_b, 20, "B");
}

/// Two vaults INDEPENDENTLY author DIFFERENT canvases at the same path (disjoint
/// lineages, no shared history) — the "both ran a layout command" case. This is
/// a genuine fork: it must BLOCK for resolution, never silently interleave
/// the two numeric-dense JSONs into spliced digits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_independent_divergent_lineages_block_not_corrupt() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "boards/graph.canvas";
    // Same node set but different layouts → different content, disjoint lineages.
    let mut nodes_a = grid_canvas(20);
    nodes_a[3].x = 9001;
    let mut nodes_b = grid_canvas(20);
    nodes_b[3].x = 7003;
    let doc_a = oplog_a
        .create_document(path, "canvas", &canvas_json(&nodes_a), &Author::User)
        .unwrap();
    let doc_b = oplog_b
        .create_document(path, "canvas", &canvas_json(&nodes_b), &Author::User)
        .unwrap();

    // A few rounds; a genuine fork blocks rather than merges.
    let mut blocked_somewhere = false;
    for _ in 0..4 {
        let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_a = spawn_responder(a, Duration::from_secs(3));
        let rb = b.sync_once(&addr_a).await.unwrap();
        a = server_a.await.unwrap();
        blocked_somewhere |= !rb.blocked.is_empty();

        let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
        let server_b = spawn_responder(b, Duration::from_secs(3));
        let ra = a.sync_once(&addr_b).await.unwrap();
        b = server_b.await.unwrap();
        blocked_somewhere |= !ra.blocked.is_empty();
    }

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    // The invariant that matters: no silent digit-splice on either side.
    assert_canvas_intact(&text_a, 20, "A");
    assert_canvas_intact(&text_b, 20, "B");
    assert!(
        blocked_somewhere || !a.blocked_docs().is_empty() || !b.blocked_docs().is_empty(),
        "a genuine canvas fork must block for resolution, not silently merge"
    );
}

/// REPRO for the reported canvas corruption: one client edits + saves REPEATEDLY
/// (each save commits + pokes) while the peer is idle, and the commits land while
/// the lineage-adoption handshake is still settling. If a side gets bound while
/// the lineages are still disjoint, the editing side's next push is a whole-doc
/// cross-lineage delta that interleaves the near-identical numeric JSON
/// (digit-duplication, e.g. `5828` -> `582828`). Models BOTH directions of the
/// dial since canonicality is by fingerprint and we don't control which side
/// wins. Must converge clean.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_edits_during_unsettled_handshake_do_not_corrupt() {
    let key = ContentKey::generate();
    // Independent identical seeds = disjoint lineages, the realistic "same board
    // already on both devices".
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "boards/graph.canvas";
    let nodes = grid_canvas(20);
    let doc_a = oplog_a
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();
    let doc_b = oplog_b
        .create_document(path, "canvas", &canvas_json(&nodes), &Author::User)
        .unwrap();

    // A "drag": nudge a node each step, save (commit), then run ONE sync round
    // in alternating directions — edits keep landing BEFORE the multi-round
    // handshake reaches quiescence.
    let mut cur = nodes.clone();
    for i in 0..8 {
        cur[1].x = 200 + i as i64;
        cur[7].y = 300 + i as i64;
        oplog_a.apply_user_text(&doc_a, &canvas_json(&cur)).unwrap();

        if i % 2 == 0 {
            // B pulls A.
            let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
            let server_a = spawn_responder(a, Duration::from_secs(3));
            b.sync_once(&addr_a).await.unwrap();
            a = server_a.await.unwrap();
        } else {
            // A pushes to B (A dials B).
            let addr_b = b.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
            let server_b = spawn_responder(b, Duration::from_secs(3));
            a.sync_once(&addr_b).await.unwrap();
            b = server_b.await.unwrap();
        }

        // Corruption is visible the instant it lands — check every step.
        let ta = oplog_a.materialize_accepted(&doc_a).unwrap().text;
        let tb = oplog_b.materialize_accepted(&doc_b).unwrap().text;
        assert_canvas_intact(&ta, 20, &format!("A@step{i}"));
        assert_canvas_intact(&tb, 20, &format!("B@step{i}"));
    }

    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 12).await;
    let ta = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let tb = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_canvas_intact(&ta, 20, "A-final");
    assert_canvas_intact(&tb, 20, "B-final");
    assert_eq!(ta, tb, "both converge to the same canvas");
}

/// The canvas forward-binding save path end-to-end: a canvas edit mirrors into
/// `working` via `replace_working` (what `persist_canvas`/`mirror_json_to_working`
/// run), an explicit Save folds it via `commit_working`, and it syncs to the peer.
/// Guards that canvas edits reach `accepted` + the wire on save (the on-save flow,
/// no autocommit).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_forward_binding_save_syncs_to_peer() {
    let key = ContentKey::generate();
    let seed = canvas_json(&grid_canvas(6));
    let (a, oplog_a, doc_a, b, oplog_b, doc_b, _da, _db) =
        bound_pair(&key, "boards/save.canvas", &seed).await;

    // A edits the canvas via the forward binding (the `working` overlay), then
    // SAVES (commit_working) — exactly `persist_canvas` + `save_canvas_document`.
    let mut nodes = grid_canvas(6);
    nodes[2].x = 9001;
    let edited = canvas_json(&nodes);
    oplog_a.replace_working(&doc_a, &edited).unwrap();
    assert_eq!(
        oplog_a.materialize_working(&doc_a).unwrap().text,
        edited,
        "the forward binding put the canvas edit in `working`"
    );
    assert!(
        oplog_a.commit_working(&doc_a).unwrap(),
        "Save commits the canvas working edit into accepted"
    );
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, edited);

    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 6).await;
    assert_eq!(
        oplog_b.materialize_accepted(&doc_b).unwrap().text,
        edited,
        "B received the saved canvas edit"
    );
}

/// One full directional sync `from` dials `to` (responder window short).
async fn dial(from: SyncNode, to: SyncNode) -> (SyncNode, SyncNode) {
    let mut from = from;
    let mut to = to;
    let addr = to.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server = spawn_responder(to, Duration::from_secs(3));
    from.sync_once(&addr).await.unwrap();
    let to = server.await.unwrap();
    (from, to)
}

/// THREE devices, each INDEPENDENTLY seeding the SAME canvas (three disjoint
/// lineages) — the realistic "I already have this board on laptop, phone, and
/// desktop" case. Mesh-sync them in an order that makes a peer adopt one lineage
/// that a later pairwise round abandons, then make concurrent edits and mesh
/// again. The transitive-adoption bug surfaces as a device bound to a stale
/// lineage taking a whole-doc cross-lineage delta → digit-splice. All three must
/// converge with no corruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canvas_three_device_independent_lineages_converge_clean() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    let (mut c, oplog_c, _dc) = mk_node(&key);
    // Enroll all three pairs.
    a.enroll_peer(b.fingerprint()).unwrap();
    a.enroll_peer(c.fingerprint()).unwrap();
    b.enroll_peer(a.fingerprint()).unwrap();
    b.enroll_peer(c.fingerprint()).unwrap();
    c.enroll_peer(a.fingerprint()).unwrap();
    c.enroll_peer(b.fingerprint()).unwrap();

    let path = "boards/graph.canvas";
    let nodes = grid_canvas(20);
    let json = canvas_json(&nodes);
    let doc_a = oplog_a.create_document(path, "canvas", &json, &Author::User).unwrap();
    let doc_b = oplog_b.create_document(path, "canvas", &json, &Author::User).unwrap();
    let doc_c = oplog_c.create_document(path, "canvas", &json, &Author::User).unwrap();

    // Sync B<->C FIRST (they pick a canonical between themselves), then bring A
    // in — so whichever of B/C adopted the other may now have to re-adopt A's
    // lineage, leaving the third device transitively on a lineage that moved.
    let run_mesh = |a: SyncNode, b: SyncNode, c: SyncNode| async move {
        let (b, c) = dial(b, c).await;
        let (c, b) = dial(c, b).await;
        let (a, b) = dial(a, b).await;
        let (b, a) = dial(b, a).await;
        let (a, c) = dial(a, c).await;
        let (c, a) = dial(c, a).await;
        (a, b, c)
    };
    for _ in 0..4 {
        let (na, nb, nc) = run_mesh(a, b, c).await;
        a = na;
        b = nb;
        c = nc;
    }

    // Concurrent edits on all three (disjoint nodes), then mesh again.
    let mut na = nodes.clone();
    na[1].x = 9001;
    oplog_a.apply_user_text(&doc_a, &canvas_json(&na)).unwrap();
    let mut nb = nodes.clone();
    nb[9].x = 7003;
    oplog_b.apply_user_text(&doc_b, &canvas_json(&nb)).unwrap();
    let mut nc = nodes.clone();
    nc[18].x = 5005;
    oplog_c.apply_user_text(&doc_c, &canvas_json(&nc)).unwrap();

    for _ in 0..6 {
        let (na, nb, nc) = run_mesh(a, b, c).await;
        a = na;
        b = nb;
        c = nc;
    }

    let text_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    let text_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    let text_c = oplog_c.materialize_accepted(&doc_c).unwrap().text;
    assert_canvas_intact(&text_a, 20, "A");
    assert_canvas_intact(&text_b, 20, "B");
    assert_canvas_intact(&text_c, 20, "C");
    drop((a, b, c));
}

// --- new-note first-content sync (op-log-working-layer) --------------------
//
//   typing = oplog.apply_working_edit(doc, ..)   (the editor forward binding)
//   save   = oplog.commit_working(doc)           (folds working → accepted)
// Regression guards that a freshly-created note's first SAVED content reaches the
// peer and is not clobbered by the peer's still-empty replica.

/// A creates a note EMPTY; the empty create settles to B (both bound at empty,
/// each with `working = None`). A then types its first content and SAVES it
/// (`commit_working`). Driving to quiescence, B must converge to A's content —
/// the peer's still-empty replica must not clobber it back over the shared
/// lineage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_note_first_saved_content_reaches_peer() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/fresh-create.md";
    // A creates the note EMPTY (the new-note create seeds accepted = "").
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();

    // Settle the empty create to B (both bound at empty, working = None).
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a).await.unwrap();
    let a = server_a.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().expect("B has empty doc");
    assert_eq!(oplog_b.materialize_accepted(&doc_b).unwrap().text, "");

    // A types its first content into `working`, then SAVES it.
    let content = "first content line\nthat must reach the peer\n";
    oplog_a.apply_working_edit(&doc_a, 0, 0, content).unwrap();
    assert!(oplog_a.commit_working(&doc_a).unwrap(), "save folded the content");
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, content);

    // Drive to quiescence — B must converge to A's content, NOT empty.
    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;

    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    let got_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert_eq!(got_a, content, "A must still hold its content (not clobbered empty): {got_a:?}");
    assert_eq!(got_b, content, "B must converge to A's first content (not empty): {got_b:?}");
}

/// A peer pull lands on A while A has UNSAVED typed content in `working` (the
/// editor forward-binding mirrored typing into the overlay). The working-mirror
/// in `apply_remote_update` must reconcile WITHOUT losing the typed content, and
/// A's later save must then fold the typed content into `accepted` — not the
/// empty remote state. This is the working-mirror survival the dirty-buffer
/// corruption fixes (text-level reconcile) guarantee, on the save flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_overlay_survives_remote_empty_pull_then_saves() {
    let key = ContentKey::generate();
    let (mut a, oplog_a, _da) = mk_node(&key);
    let (mut b, oplog_b, _db) = mk_node(&key);
    enroll_each_other(&a, &b);

    let path = "notes/pull-race.md";
    let doc_a = oplog_a.create_document(path, "note", "", &Author::User).unwrap();

    // Settle empty to B.
    let addr_a = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a).await.unwrap();
    let mut a = server_a.await.unwrap();
    let doc_b = oplog_b.doc_id_for_path(path).unwrap().expect("B has empty doc");

    // A types content into `working` but has NOT saved yet (the buffer is dirty;
    // accepted is still empty).
    let content = "typed-but-not-yet-saved\n";
    oplog_a.apply_working_edit(&doc_a, 0, 0, content).unwrap();
    assert_eq!(oplog_a.materialize_working(&doc_a).unwrap().text, content);
    assert_eq!(oplog_a.materialize_accepted(&doc_a).unwrap().text, "");

    // B (still empty) dials A: B's empty state syncs into A while A's accepted is
    // empty and A's working holds the typed content — a remote pull landing on A
    // while the user is mid-edit.
    let addr_a2 = a.listen("/ip4/127.0.0.1/tcp/0").await.unwrap();
    let server_a2 = spawn_responder(a, Duration::from_secs(3));
    b.sync_once(&addr_a2).await.unwrap();
    a = server_a2.await.unwrap();

    // The working overlay must STILL hold the typed content after the remote pull.
    assert_eq!(
        oplog_a.materialize_working(&doc_a).unwrap().text,
        content,
        "A's working overlay must survive the remote empty pull"
    );

    // Now A saves. It must fold the typed content into accepted, not commit empty.
    let committed = oplog_a.commit_working(&doc_a).unwrap();
    let got_a = oplog_a.materialize_accepted(&doc_a).unwrap().text;
    assert!(committed, "save must fold the typed content");
    assert_eq!(got_a, content, "A's accepted must be the typed content, not empty: {got_a:?}");

    // And it must then sync to B.
    let (_a, _b) =
        sync_until_settled(a, b, (&oplog_a, &doc_a), (&oplog_b, &doc_b), 8).await;
    let got_b = oplog_b.materialize_accepted(&doc_b).unwrap().text;
    assert_eq!(got_b, content, "B must converge to the content (not empty): {got_b:?}");
}

// Note: the three-way-merge doubling regression (a peer save racing
// `commit_working`'s two-lock window) needs the crate-private
// `commit_working_test_hook`, so it lives as a core oplog test:
// `core/src/oplog/tests/sync.rs::commit_working_no_double_when_peer_races_identical_content`.
