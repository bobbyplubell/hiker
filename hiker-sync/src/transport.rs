//! libp2p transport + the peer sync-session state machine (Wave 2).
//!
//! # Composition
//!
//! One authenticated connection carries many muxed substreams. The libp2p
//! [`Behaviour`](SyncBehaviour) this module composes via
//! `#[derive(NetworkBehaviour)]`:
//!
//! - **TCP** transport (`sync-tcp-transport-choice` — not QUIC; TCP reaches
//!   through locked-down enterprise firewalls more reliably).
//! - **Noise** for mutual endpoint authentication from the enrolled static
//!   device keys (no PKI/CA) plus hop confidentiality and replay protection.
//!   The Noise static key IS the device identity: it comes straight from
//!   [`crate::crypto::DeviceKeypair`], so the remote `PeerId` is what
//!   enrollment authenticates against. [sync-noise-channel]
//! - **yamux** stream multiplexing — request-response already rides concurrent
//!   substreams over the one connection, so per-document streaming never
//!   head-of-line-blocks the control exchange. [sync-stream-muxing]
//! - **mdns** for the manual, time-boxed LAN discovery window. Discovery only
//!   supplies candidates; a connection still authenticates against the enrolled
//!   fingerprints. [sync-mdns-discovery]
//! - **request-response** (CBOR codec) for the framed [`crate::protocol::Message`]
//!   exchange.
//!
//! The malware-adjacent P2P behaviors (`kad`, `dcutr`, `relay`, `autonat`) are
//! compiled out and banned in `deny.toml`. [sync-banned-p2p-features]
//!
//! # Session flow
//!
//! The session is dialer-driven: [`SyncNode::sync_once`] dials a peer, then runs
//! a sequence of request-response round trips while the peer's [`SyncNode::run`]
//! loop answers each request:
//!
//! 1. `Hello` ↔ `HelloAck` — exchange + record fingerprints (Noise already
//!    authenticated them).
//! 2. `ManifestRequest` → `Manifest` — pull the peer's document manifest.
//! 3. per remote entry: resolve/mint the shared logical id, run
//!    [`crate::enroll::classify`], and act:
//!    - `Identical` → bind, no transfer.
//!    - `FastForwardAdoptPeer` (we behind / fresh) → `StateRequest` → adopt the
//!      peer's `LineageBase`. [sync-lineage-adoption]
//!    - `FastForwardPeerAdopts` / already-shared lineage → `DeltaRequest` →
//!      `apply_remote_update` the decrypted delta. [sync-content-encryption-aes256]
//!    - `Fork` → mark the doc `Blocked`, record it, stream nothing.
//!      [sync-blocked-state]
//!
//! The public surface returns plain Rust types only — no libp2p `Swarm` /
//! `PeerId` / `Multiaddr`-typed error escapes the crate (`Multiaddr` is a
//! re-exported address newtype the caller passes in/out; no behaviour type
//! leaks).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use libp2p::request_response::{self, OutboundRequestId, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{mdns, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm};

use hiker_core::oplog::shapes::Author;
use hiker_core::oplog::OpLog;

use crate::config::Settings;
use crate::crypto::{self, ContentKey, DeviceKeypair, SharedContentKey};
use crate::enroll::{self, Classification};
use crate::identity::{
    Binding, BindingTable, BlockedDoc, DeviceFingerprint, LocalDocId, LogicalId, Resolution,
    SyncStatus,
};
use crate::protocol::{Manifest, ManifestEntry, Message};
use crate::server::{BlobStore, MemBlobStore};
use crate::Error;

/// The wire protocol version sent in `Hello`.
const PROTOCOL_VERSION: u32 = 1;

/// How many recent `content_hash` values a manifest entry carries for
/// fast-forward classification. Bounded so a long-lived document's manifest row
/// stays small.
const RECENT_HISTORY_WINDOW: usize = 32;

/// The request-response application protocol id.
const SYNC_PROTOCOL: &str = "/hiker-sync/1";

/// A discovered LAN peer candidate surfaced by the mDNS window: an enrolled
/// device's fingerprint plus a dialable address (a libp2p multiaddr string).
/// Plain data — no libp2p type escapes. [sync-mdns-discovery]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCandidate {
    /// The enrolled device fingerprint the candidate's `PeerId` maps to.
    pub fingerprint: DeviceFingerprint,
    /// A dialable multiaddr advertised over mDNS, as a string (e.g.
    /// `/ip4/192.168.1.5/tcp/40123`).
    pub addr: String,
}

/// The enrolled-peer set, shared out of the node lock so the app can list /
/// enroll / unenroll without taking the node's `tokio::Mutex` (which the
/// responder/auto-sync loop can hold for a whole round). Both [`SyncNode`] and
/// the app's sync service hold a clone of the SAME instance (an `Arc` of a std
/// `Mutex`), so an enroll on the app side is visible to the running swarm's
/// connection-auth gate immediately — no rebuild, no waiting behind a round.
///
/// Keyed by `PeerId` (derived from the swapped fingerprint via
/// [`crypto::fingerprint_to_peer_id`]) so a connection authenticates in one
/// lookup; the fingerprint is kept alongside for reporting / discovery and for
/// the UI's enrolled list. Recomputing the `PeerId` on insert is the validation
/// point — and the reason the app can't mutate the map directly:
/// `fingerprint_to_peer_id` is `pub(crate)`, so [`EnrolledPeers::enroll`] is the
/// public seam. [sync-key-swap-enrollment]
#[derive(Clone)]
pub struct EnrolledPeers {
    inner: Arc<Mutex<HashMap<PeerId, DeviceFingerprint>>>,
}

impl Default for EnrolledPeers {
    fn default() -> Self {
        Self::new()
    }
}

impl EnrolledPeers {
    /// An empty enrolled set.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enroll a peer by its out-of-band-swapped device fingerprint. Computes the
    /// `PeerId` (the validation point) and inserts the mapping. A malformed
    /// fingerprint is rejected with [`Error::InvalidFingerprint`].
    pub fn enroll(&self, fingerprint: DeviceFingerprint) -> Result<(), Error> {
        let peer_id = crypto::fingerprint_to_peer_id(&fingerprint)?;
        self.inner.lock().unwrap().insert(peer_id, fingerprint);
        Ok(())
    }

    /// Remove a peer by its fingerprint (matched on recomputed `PeerId`). A
    /// fingerprint that isn't enrolled — or one that doesn't even decode — is a
    /// no-op rather than an error, so un-enroll is always safe to call.
    pub fn unenroll(&self, fingerprint: &DeviceFingerprint) {
        if let Ok(peer_id) = crypto::fingerprint_to_peer_id(fingerprint) {
            self.inner.lock().unwrap().remove(&peer_id);
        }
    }

    /// Whether a connected `PeerId` is enrolled — the connection-auth check.
    pub fn contains(&self, peer: &PeerId) -> bool {
        self.inner.lock().unwrap().contains_key(peer)
    }

    /// The enrolled peer's fingerprint for a `PeerId`, if mapped.
    pub fn fingerprint_of(&self, peer: &PeerId) -> Option<DeviceFingerprint> {
        self.inner.lock().unwrap().get(peer).cloned()
    }

    /// The enrolled fingerprint strings — the live set the UI lists.
    pub fn fingerprints(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .map(|fp| fp.0.clone())
            .collect()
    }
}

/// Build the sibling path for a keep-both conflict copy: `<stem> (conflict
/// <tag>).<ext>`, where `tag` is a short slice of the peer fingerprint (or, if
/// empty, the current unix-ms timestamp). The copy lands next to the original
/// in the same directory so it's an obvious neighbor in the vault.
/// [sync-blocked-state]
fn conflict_copy_path(path: &str, peer_tag: &str) -> String {
    // Short, filename-safe tag: the peer fingerprint head, else a timestamp.
    let tag: String = if peer_tag.is_empty() {
        now_ms().to_string()
    } else {
        peer_tag
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(8)
            .collect()
    };
    // Split into dir / file, then stem / ext, preserving the directory prefix.
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    format!("{dir}{stem} (conflict {tag}){ext}")
}

/// Current unix time in milliseconds — the conflict-copy fallback tag.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse a multiaddr string into the libp2p type, mapping a parse error into the
/// crate's plain [`Error`] so the libp2p error type never escapes.
pub(crate) fn parse_addr(addr: &str) -> Result<Multiaddr, Error> {
    addr.parse()
        .map_err(|e| Error::Transport(format!("invalid multiaddr {addr:?}: {e}")))
}

/// The composed libp2p behaviour: framed [`Message`] request-response plus mDNS
/// LAN discovery. The derive macro generates a `SyncBehaviourEvent` enum over
/// the two sub-behaviours' events.
///
/// Shared by both the peer [`SyncNode`] and the hub [`crate::server::Hub`]
/// — the transport is role-agnostic, only the topology differs. [sync-libp2p-transport]
#[derive(NetworkBehaviour)]
pub(crate) struct SyncBehaviour {
    /// Framed [`Message`] exchange. Both request and response are a `Message`,
    /// so one codec carries the whole control + per-document protocol.
    pub(crate) rr: request_response::cbor::Behaviour<Message, Message>,
    /// Manual, time-boxed LAN discovery. [sync-mdns-discovery]
    pub(crate) mdns: mdns::tokio::Behaviour,
}

/// Build the role-agnostic libp2p swarm shared by the peer node and the hub:
/// TCP + Noise (keyed by `keypair`) + yamux, carrying the CBOR request-response
/// [`Message`] behaviour and mDNS. The Noise static key IS the device identity,
/// so the remote `PeerId` is exactly what enrollment authenticates against.
/// [sync-noise-channel, sync-tcp-transport-choice]
pub(crate) fn build_swarm(
    keypair: libp2p::identity::Keypair,
) -> Result<Swarm<SyncBehaviour>, Error> {
    let local_peer_id = keypair.public().to_peer_id();
    let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| Error::Transport(format!("tcp/noise setup: {e}")))?
        .with_behaviour(|_kp| {
            let rr = request_response::cbor::Behaviour::<Message, Message>::new(
                [(StreamProtocol::new(SYNC_PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default(),
            );
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            Ok(SyncBehaviour { rr, mdns })
        })
        .map_err(|e| Error::Transport(format!("behaviour setup: {e}")))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();
    Ok(swarm)
}

/// The outcome of a [`SyncNode::sync_once`] run — test-drivable and the shape
/// the later scenario suite builds on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Logical ids that became (or already were) bound this session.
    pub bound: Vec<LogicalId>,
    /// Logical ids whose local replica converged to the peer (adopted a base or
    /// applied a delta).
    pub converged: Vec<LogicalId>,
    /// `(path, reason)` for each document left unsynced — a fork is `"fork"`.
    pub blocked: Vec<(String, String)>,
}

/// A peer sync node: owns the vault's [`OpLog`], the content + device keys, the
/// binding table, config, the enrolled-peer set, and a per-doc sync-status map,
/// plus an (in-memory) [`BlobStore`] for the server-mediated path. The libp2p
/// `Swarm` is built lazily on first `listen`/`dial`/`sync_once` and held here so
/// the event loop and the one-shot driver share it.
pub struct SyncNode {
    oplog: Arc<OpLog>,
    /// The vault content key, shared (and persist-through) with the caller — the
    /// app's sync service holds the SAME handle, so an in-band auto-transfer or
    /// a manual import on either side is seen by both and written to disk once.
    /// Used for encrypt / decrypt / blind-id. [sync-vault-key-inband]
    content_key: SharedContentKey,
    keypair: DeviceKeypair,
    fingerprint: DeviceFingerprint,
    bindings: Mutex<BindingTable>,
    config: Settings,
    /// Enrolled peers — a clone of the SAME shared set the app's sync service
    /// holds, so an app-side enroll/unenroll is visible to this node's
    /// connection-auth gate and discovery immediately (no rebuild, no node
    /// lock). [sync-key-swap-enrollment]
    enrolled: EnrolledPeers,
    /// Per-document sync status (`Bound` / `PendingBind` / `Blocked`).
    status: Mutex<HashMap<LocalDocId, SyncStatus>>,
    /// Persistent record of every forked (blocked) document, keyed by logical
    /// id — the surface the UI lists and resolves. Distinct from the round
    /// report's `blocked` (which is the LAST round only): an entry persists
    /// until the doc converges or the user resolves it. [sync-blocked-state]
    blocked: Mutex<HashMap<LogicalId, BlockedDoc>>,
    /// User resolution decisions for blocked docs, keyed by logical id. The
    /// fork branch consults this on the NEXT round: an entry makes it act
    /// (keep-mine / keep-theirs / keep-both) instead of re-blocking. Empty by
    /// default, so forks block unchanged when the user hasn't chosen.
    /// [sync-blocked-state]
    resolutions: Mutex<HashMap<LogicalId, Resolution>>,
    /// Server-mediated store-and-forward log (unused on the LAN path; held so
    /// the same node can drive the server path in a later wave). [server::BlobStore]
    blobs: Mutex<MemBlobStore>,
    /// Per-blind-id outgoing push sequence: the next `seq` this device will
    /// stamp on an `UpdateBlob` it pushes to the hub. Monotonic per blind id so
    /// the server's append-only log orders one device's pushes; other devices'
    /// pushes interleave by their own seqs and Yrs merges them commutatively.
    server_push_seq: Mutex<HashMap<String, u64>>,
    /// Per-blind-id pull cursor: the highest `seq` this device has already
    /// pulled + applied from the hub. The store-and-forward catch-up watermark.
    /// [sync-zero-knowledge-server]
    server_pull_cursor: Mutex<HashMap<String, u64>>,
    /// ALL LAN peers seen via mDNS, keyed by `PeerId` with the address they
    /// advertised — enrolled or not. The single source of truth for discovery,
    /// folded from the `Discovered`/`Expired` events the swarm surfaces while
    /// the responder [`run`](Self::run) loop drives it (and the one-shot
    /// [`start_discovery`](Self::start_discovery) window).
    ///
    /// Crucially, the enrolled/unenrolled split is NOT frozen here at mDNS-event
    /// time: [`discovered_peers`](Self::discovered_peers) and
    /// [`seen_unenrolled`](Self::seen_unenrolled) classify against the LIVE
    /// [`EnrolledPeers`] set on every READ. So enrolling a peer that's already
    /// sitting in this map promotes it to a dial candidate immediately, with no
    /// second mDNS event to wait for. [sync-mdns-discovery]
    discovered: Mutex<HashMap<PeerId, Multiaddr>>,
    /// `PeerId`s of unenrolled peers we've already emitted a one-time "seen on
    /// LAN" log line for, so the responder loop doesn't repeat it every window.
    seen_unenrolled_logged: Mutex<HashSet<PeerId>>,
    /// Unenrolled peers seen for the first time and not yet drained by the
    /// caller for a one-time log line. The responder loop drains this via
    /// [`take_newly_seen_unenrolled`](Self::take_newly_seen_unenrolled) after
    /// each window and emits a progress line per entry.
    newly_seen_unenrolled: Mutex<Vec<PeerId>>,
    /// Set true whenever a `run` window folded in an enrolled peer not already
    /// in `discovered`. The caller polls + clears it via
    /// [`take_newly_discovered`](Self::take_newly_discovered) to trigger an
    /// immediate sync round on first sight of a peer.
    newly_discovered: Mutex<bool>,
    /// The libp2p swarm, built lazily.
    swarm: Option<Swarm<SyncBehaviour>>,
}

impl SyncNode {
    /// Construct a node over a vault's [`OpLog`] with its content + device keys
    /// and `[sync]` config, sharing the given [`EnrolledPeers`] set AND the
    /// [`SharedContentKey`] handle with the caller (the app's sync service holds
    /// the same instances, so enroll/unenroll or a content-key swap on either
    /// side is seen by both). The swarm is not built until the first
    /// `listen`/`dial`/`sync_once`.
    pub fn new(
        oplog: Arc<OpLog>,
        content_key: SharedContentKey,
        keypair: DeviceKeypair,
        config: Settings,
        enrolled: EnrolledPeers,
    ) -> Self {
        let fingerprint = keypair.fingerprint();
        Self {
            oplog,
            content_key,
            keypair,
            fingerprint,
            bindings: Mutex::new(BindingTable::new()),
            config,
            enrolled,
            status: Mutex::new(HashMap::new()),
            blocked: Mutex::new(HashMap::new()),
            resolutions: Mutex::new(HashMap::new()),
            blobs: Mutex::new(MemBlobStore::new()),
            server_push_seq: Mutex::new(HashMap::new()),
            server_pull_cursor: Mutex::new(HashMap::new()),
            discovered: Mutex::new(HashMap::new()),
            seen_unenrolled_logged: Mutex::new(HashSet::new()),
            newly_seen_unenrolled: Mutex::new(Vec::new()),
            newly_discovered: Mutex::new(false),
            swarm: None,
        }
    }

    /// This node's own device fingerprint (the one a peer must enroll).
    pub fn fingerprint(&self) -> DeviceFingerprint {
        self.fingerprint.clone()
    }

    /// Enroll a peer by its out-of-band-swapped device fingerprint. Only
    /// connections whose authenticated `PeerId` maps back to an enrolled
    /// fingerprint proceed — discovery never bypasses this. [sync-key-swap-enrollment]
    ///
    /// A malformed fingerprint is rejected with [`Error::InvalidFingerprint`].
    pub fn enroll_peer(&self, fingerprint: DeviceFingerprint) -> Result<(), Error> {
        self.enrolled.enroll(fingerprint)
    }

    /// A handle to this node's shared enrolled-peer set, so the caller can hold
    /// the SAME instance and mutate it without the node lock.
    pub fn enrolled_handle(&self) -> EnrolledPeers {
        self.enrolled.clone()
    }

    /// Remove a peer from the enrolled set, dropping its `PeerId` mapping. After
    /// this the peer's connections are no longer authorized and it stops being
    /// an auto-dial target — [`discovered_peers`](Self::discovered_peers)
    /// classifies against the live enrolled set on read, so dropping the
    /// enrollment is enough to remove it as a candidate (it reappears in
    /// [`seen_unenrolled`](Self::seen_unenrolled) if it's still on the LAN). A
    /// fingerprint that wasn't enrolled is a no-op.
    pub fn unenroll_peer(&self, fingerprint: &DeviceFingerprint) -> Result<(), Error> {
        self.enrolled.unenroll(fingerprint);
        Ok(())
    }

    /// Replace the vault content key (the in-band key transfer lands it here
    /// after the fingerprint swap authenticates the channel). Routes through the
    /// shared handle, so it also persists and is seen by the app's sync service.
    /// [sync-vault-key-inband]
    pub fn set_content_key(&self, key: ContentKey) {
        self.content_key.set(key);
    }

    /// The current vault content key (clone), for export to another of the
    /// user's own devices via the manual content-key swap. The returned key is
    /// a SECRET — see [`ContentKey::to_b58`].
    pub fn content_key(&self) -> ContentKey {
        self.content_key.get()
    }

    /// The shared content-key handle, so the caller holds the SAME instance and
    /// a key swap on either side is mutual. [sync-vault-key-inband]
    pub fn content_key_handle(&self) -> SharedContentKey {
        self.content_key.clone()
    }

    /// A snapshot of this node's binding table (for inspection / tests).
    pub fn bindings(&self) -> BindingTable {
        self.bindings.lock().unwrap().clone()
    }

    /// Pre-seed the discovery map with a peer at a multiaddr, as if mDNS had
    /// surfaced it — the test seam for the LAN round path, which reads
    /// [`discovered_peers`](Self::discovered_peers) live. Whether the peer shows
    /// up as a dial candidate still depends on it being enrolled (read-time
    /// classification), exactly as in production. A malformed multiaddr is a
    /// no-op.
    pub fn record_discovered_for_test(&self, peer_fingerprint: &DeviceFingerprint, addr: &str) {
        let (Ok(peer_id), Ok(addr)) = (
            crypto::fingerprint_to_peer_id(peer_fingerprint),
            parse_addr(addr),
        ) else {
            return;
        };
        self.record_discovered([(peer_id, addr)]);
    }

    /// Pre-seed a `(local_doc_id, logical_id)` binding directly and flip the doc
    /// to `Bound`, without a live P2P manifest round.
    ///
    /// The server-mediated path ([`sync_via_server`](Self::sync_via_server))
    /// assumes documents are already bound — binding itself happens via the P2P
    /// manifest exchange / enrollment, not through the zero-knowledge hub (which
    /// only ever sees opaque ciphertext, so it cannot classify or negotiate ids).
    /// This is the seam an enrolled device's bind table is restored from, and
    /// what tests use to stand up the server path without a peer round.
    pub fn bind_for_test(&self, local: LocalDocId, logical: LogicalId) {
        self.bind_local(local, logical);
    }

    /// Reset the per-blind-id server pull cursors so the next
    /// [`sync_via_server`](Self::sync_via_server) re-fetches every blob from seq
    /// 0 — the test seam for the idempotent re-pull case (a client that pulls
    /// the same store-and-forward blobs again must not double content, since
    /// `apply_remote_update` merges already-known Yrs ops as a no-op).
    pub fn reset_server_cursor_for_test(&self) {
        self.server_pull_cursor.lock().unwrap().clear();
    }

    /// This node's `[sync]` configuration (mode, discovery toggle, enrolled
    /// device list as loaded from the vault config).
    pub const fn config(&self) -> &Settings {
        &self.config
    }

    /// Buffer a content-encrypted update for the store-and-forward server path:
    /// encrypt `update` under the content key and append it to the local
    /// [`MemBlobStore`] under the logical id's blind id. The server-mediated
    /// transport (Wave 3) flushes this log to the hub; on the LAN path direct
    /// peer streaming is used instead. [sync-zero-knowledge-server]
    pub fn buffer_update(&self, logical_id: &LogicalId, seq: u64, update: &[u8]) {
        let key = self.content_key.get();
        let blind = crypto::blind_id(&key, &logical_id.0);
        let ciphertext = key.encrypt(update);
        self.blobs.lock().unwrap().push(&blind, seq, ciphertext);
    }

    /// Pull buffered encrypted updates for a logical id past `after_seq` and
    /// decrypt them — the receiving half of the store-and-forward path. Returns
    /// `(seq, plaintext_update)` ascending; a tampered/foreign-key blob fails
    /// with [`Error::Decrypt`]. [sync-zero-knowledge-server]
    pub fn drain_buffered(
        &self,
        logical_id: &LogicalId,
        after_seq: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, Error> {
        let key = self.content_key.get();
        let blind = crypto::blind_id(&key, &logical_id.0);
        let blobs = self.blobs.lock().unwrap();
        blobs
            .pull(&blind, after_seq)
            .into_iter()
            .map(|(seq, ct)| key.decrypt(&ct).map(|pt| (seq, pt)))
            .collect()
    }

    /// The current sync status of a local document, if tracked.
    pub fn status_of(&self, local: &LocalDocId) -> Option<SyncStatus> {
        self.status.lock().unwrap().get(local).copied()
    }

    /// A snapshot of every document currently blocked by a fork — the surface
    /// the Sync page lists and resolves. Persistent across rounds (unlike the
    /// round report's `blocked`), so a doc that forked two rounds ago is still
    /// here until it converges or the user resolves it. [sync-blocked-state]
    pub fn blocked_docs(&self) -> Vec<BlockedDoc> {
        self.blocked.lock().unwrap().values().cloned().collect()
    }

    /// Record the user's resolution decision for a forked document. Consumed by
    /// the fork branch on the NEXT round: instead of re-blocking it adopts the
    /// peer (keep-theirs / keep-both) or offers our lineage (keep-mine). No
    /// decision (the default) leaves the fork blocked. [sync-blocked-state]
    pub fn set_fork_resolution(&self, logical_id: LogicalId, resolution: Resolution) {
        self.resolutions
            .lock()
            .unwrap()
            .insert(logical_id, resolution);
    }

    /// Record a fork as persistently blocked. Idempotent on the logical id.
    fn record_blocked(&self, logical: &LogicalId, path: &str, peer: &DeviceFingerprint) {
        self.blocked.lock().unwrap().insert(
            logical.clone(),
            BlockedDoc {
                logical_id: logical.clone(),
                path: path.to_string(),
                reason: "fork".to_string(),
                peer_fingerprint: peer.clone(),
            },
        );
    }

    /// The logical id of a previously-recorded fork at `path`, if any. A forked
    /// doc never binds, so reusing this id across rounds keeps the user's
    /// resolution decision (keyed by that id) addressable. [sync-blocked-state]
    fn blocked_logical_for_path(&self, path: &str) -> Option<LogicalId> {
        self.blocked
            .lock()
            .unwrap()
            .values()
            .find(|b| b.path == path)
            .map(|b| b.logical_id.clone())
    }

    /// Clear a blocked record and its resolution decision — called when the doc
    /// converges or its fork is resolved.
    fn clear_blocked(&self, logical: &LogicalId) {
        self.blocked.lock().unwrap().remove(logical);
        self.resolutions.lock().unwrap().remove(logical);
    }

    /// The fingerprint of the enrolled peer for a connection, falling back to
    /// the raw peer id string when the mapping is missing (shouldn't happen on
    /// an enrolled connection, but keeps the record honest either way).
    fn peer_fingerprint(&self, peer_id: &PeerId) -> DeviceFingerprint {
        self.enrolled
            .fingerprint_of(peer_id)
            .unwrap_or_else(|| DeviceFingerprint(peer_id.to_string()))
    }

    /// The LAN candidates that are CURRENTLY enrolled: every mDNS-discovered
    /// peer whose `PeerId` maps to an enrolled fingerprint at the moment of the
    /// call. Classified against the live [`EnrolledPeers`] set on read (not at
    /// mDNS-event time), so enrolling a peer that's already in the discovery map
    /// makes it a dial candidate immediately — no second mDNS event needed. The
    /// always-on counterpart to a one-shot [`start_discovery`](Self::start_discovery)
    /// window; auto-sync reads this to pick dial targets. [sync-mdns-discovery]
    pub fn discovered_peers(&self) -> Vec<PeerCandidate> {
        self.discovered
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(peer_id, addr)| {
                self.enrolled.fingerprint_of(peer_id).map(|fp| PeerCandidate {
                    fingerprint: fp,
                    addr: addr.to_string(),
                })
            })
            .collect()
    }

    /// Whether a new enrolled peer appeared since the last call, clearing the
    /// flag. The auto-sync driver polls this after each responder window to
    /// trigger an immediate round on first sight of a peer rather than waiting
    /// for the next periodic tick.
    pub fn take_newly_discovered(&self) -> bool {
        std::mem::replace(&mut self.newly_discovered.lock().unwrap(), false)
    }

    /// Fold an mDNS `Discovered` event into the single discovery map — EVERY
    /// peer, enrolled or not (the enrolled/unenrolled split is decided on read,
    /// not here). Flags `newly_discovered` when a not-yet-tracked peer that is
    /// currently enrolled appears (so the driver fires a prompt round on first
    /// sight), and pushes a first-seen unenrolled peer onto `newly_seen_unenrolled`
    /// (drained for a one-time log line). A peer that was already seen unenrolled
    /// and is enrolled later relies on the periodic tick / the explicit kick the
    /// enroll path does — not on a re-announce. Shared by the responder path and
    /// `start_discovery`. [sync-mdns-discovery]
    fn record_discovered(&self, peers: impl IntoIterator<Item = (PeerId, Multiaddr)>) {
        let mut discovered = self.discovered.lock().unwrap();
        let mut logged = self.seen_unenrolled_logged.lock().unwrap();
        for (peer_id, addr) in peers {
            // New peer id (or one that re-appeared after expiry) the first time
            // we see it. Address churn for a known peer just updates the entry.
            let first_sight = discovered.insert(peer_id, addr).is_none();
            if self.enrolled.contains(&peer_id) {
                // Enrolled now: a first sight flags a prompt round.
                if first_sight {
                    *self.newly_discovered.lock().unwrap() = true;
                }
            } else if logged.insert(peer_id) {
                // Not enrolled: surface a one-time "seen on LAN" line so the user
                // knows a hiker instance is reachable but needs enrolling. The
                // enrollment gate is unchanged; this peer still can't sync.
                self.newly_seen_unenrolled.lock().unwrap().push(peer_id);
            }
        }
    }

    /// Drain the `PeerId`s of unenrolled peers seen for the first time since the
    /// last call. The responder loop polls this after each window and emits a
    /// one-time "discovered un-enrolled peer …" progress line per entry, so the
    /// user sees that a hiker instance is on the LAN but needs enrolling.
    pub fn take_newly_seen_unenrolled(&self) -> Vec<String> {
        std::mem::take(&mut *self.newly_seen_unenrolled.lock().unwrap())
            .into_iter()
            .map(|p| p.to_string())
            .collect()
    }

    /// LAN peers seen via mDNS whose fingerprint is NOT enrolled, as
    /// `(peer_id, addr, fingerprint)` strings — the always-on visibility surface
    /// the page reads (mirrored into `Shared`, never locked on render). These
    /// can't sync; they're shown so the user knows a hiker instance is reachable
    /// but needs enrolling.
    ///
    /// The fingerprint is derived from the `PeerId` via
    /// [`crypto::peer_id_to_fingerprint`] (our keys are Ed25519, so an
    /// identity-multihash `PeerId` carries the public key verbatim). It's
    /// `Some` for a peer we can offer a one-click enroll for, `None` for a
    /// `PeerId` we can't invert. Deriving it here keeps libp2p types off the
    /// render path — the page enrolls with the `String` we hand it.
    /// [sync-mdns-discovery]
    pub fn seen_unenrolled(&self) -> Vec<(String, String, Option<String>)> {
        self.discovered
            .lock()
            .unwrap()
            .iter()
            .filter(|(peer_id, _)| !self.enrolled.contains(peer_id))
            .map(|(peer_id, addr)| {
                let fp = crypto::peer_id_to_fingerprint(peer_id).map(|f| f.0);
                (peer_id.to_string(), addr.to_string(), fp)
            })
            .collect()
    }

    /// Drop expired peers from the single discovery map (clearing the
    /// one-time-log marker so a peer that re-appears later logs again).
    fn record_expired(&self, peers: impl IntoIterator<Item = (PeerId, Multiaddr)>) {
        let mut discovered = self.discovered.lock().unwrap();
        let mut logged = self.seen_unenrolled_logged.lock().unwrap();
        for (peer_id, _addr) in peers {
            discovered.remove(&peer_id);
            logged.remove(&peer_id);
        }
    }

    // --- swarm lifecycle -------------------------------------------------

    /// Build the libp2p swarm if not already built: TCP + Noise (keyed by the
    /// device keypair) + yamux, carrying the CBOR request-response behaviour and
    /// mDNS. Idempotent.
    fn ensure_swarm(&mut self) -> Result<(), Error> {
        if self.swarm.is_some() {
            return Ok(());
        }
        let keypair = self.keypair.libp2p_keypair().clone();
        self.swarm = Some(build_swarm(keypair)?);
        Ok(())
    }

    const fn swarm_mut(&mut self) -> &mut Swarm<SyncBehaviour> {
        self.swarm
            .as_mut()
            .expect("swarm built by ensure_swarm before use")
    }

    /// Start listening on `addr`; returns the concrete bound address (e.g. with
    /// the OS-assigned port resolved from `/tcp/0`).
    pub async fn listen(&mut self, addr: &str) -> Result<String, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(addr)?;
        self.swarm_mut()
            .listen_on(addr)
            .map_err(|e| Error::Transport(format!("listen: {e}")))?;
        // Drive the swarm until the first NewListenAddr resolves the real port.
        loop {
            match self.swarm_mut().select_next_some().await {
                SwarmEvent::NewListenAddr { address, .. } => return Ok(address.to_string()),
                SwarmEvent::ListenerError { error, .. } => {
                    return Err(Error::Transport(format!("listener error: {error}")));
                }
                _ => {}
            }
        }
    }

    /// Dial `addr`. Returns once the dial is queued; connection establishment
    /// (and its enrollment check) happens in the event loop.
    pub async fn dial(&mut self, addr: &str) -> Result<(), Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(addr)?;
        self.swarm_mut()
            .dial(addr)
            .map_err(|e| Error::Transport(format!("dial: {e}")))?;
        Ok(())
    }

    /// Whether a connected `PeerId` is enrolled. A non-enrolled peer's
    /// connection is dropped — discovery never bypasses enrollment.
    /// [sync-noise-channel]
    fn is_enrolled(&self, peer: &PeerId) -> bool {
        self.enrolled.contains(peer)
    }

    // --- manifest --------------------------------------------------------

    /// Build this vault's manifest: one [`ManifestEntry`] per document, with its
    /// current content hash, a bounded recent history-hash window, and its
    /// logical id if already bound. [sync-path-matching-key]
    fn build_manifest(&self) -> Result<Manifest, Error> {
        let doc_ids = self
            .oplog
            .list_doc_ids()
            .map_err(|e| Error::Transport(format!("list docs: {e}")))?;
        let bindings = self.bindings.lock().unwrap();
        let mut entries = Vec::with_capacity(doc_ids.len());
        for doc_id in doc_ids {
            let local = LocalDocId(doc_id.clone());
            // A doc with no path row is unmaterialized / mid-creation — skip it
            // rather than emit a pathless manifest row that can't path-match.
            let Some(path) = self
                .oplog
                .path_for_doc(&doc_id)
                .map_err(|e| Error::Transport(format!("path for {doc_id}: {e}")))?
            else {
                continue;
            };
            let text = self
                .oplog
                .materialize_accepted(&doc_id)
                .map_err(|e| Error::Transport(format!("materialize {doc_id}: {e}")))?
                .text;
            let current_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
            let history: Vec<String> = self
                .oplog
                .doc_history_hashes(&doc_id)
                .map_err(|e| Error::Transport(format!("history {doc_id}: {e}")))?
                .into_iter()
                .take(RECENT_HISTORY_WINDOW)
                .collect();
            entries.push(ManifestEntry {
                path,
                current_hash,
                recent_history_hashes: history,
                logical_id: bindings.logical_for(&local).map(|l| l.0.clone()),
            });
        }
        Ok(Manifest { entries })
    }

    /// The local content hash for a doc id (blake3 of `materialize(accepted)`).
    fn current_hash(&self, doc_id: &str) -> Result<String, Error> {
        let text = self
            .oplog
            .materialize_accepted(doc_id)
            .map_err(|e| Error::Transport(format!("materialize {doc_id}: {e}")))?
            .text;
        Ok(blake3::hash(text.as_bytes()).to_hex().to_string())
    }

    fn history_set(&self, doc_id: &str) -> Result<HashSet<String>, Error> {
        self.oplog
            .doc_history_hashes(doc_id)
            .map_err(|e| Error::Transport(format!("history {doc_id}: {e}")))
    }

    // --- responder -------------------------------------------------------

    /// Compute the reply to one inbound [`Message`] request from `peer`. The
    /// responder is stateless across requests beyond the binding table / OpLog:
    /// every request names the logical id or carries enough to resolve it.
    fn handle_request(&self, peer: &PeerId, req: Message) -> Result<Message, Error> {
        match req {
            Message::Hello { .. } => Ok(Message::HelloAck {
                device_fingerprint: self.fingerprint.0.clone(),
                content_key_fp: self.content_key.fingerprint(),
            }),
            // Serve the content key to an enrolled peer over the (already
            // Noise-encrypted) channel — the canonical-device half of the
            // in-band transfer. `peer` is already enrollment-gated by the
            // responder. The raw bytes are NEVER logged. [sync-vault-key-inband]
            Message::ContentKeyRequest => {
                let _ = peer; // enrollment already gated the connection.
                Ok(Message::ContentKeyResponse {
                    key: self.content_key.get().as_bytes().to_vec(),
                })
            }
            Message::ManifestRequest => Ok(Message::Manifest(self.build_manifest()?)),
            // Serve a read-only snapshot of one document's current accepted text
            // to an enrolled peer (the connection is already enrollment-gated).
            // The requester diffs it against its own version to preview a fork
            // before resolving it — this neither binds nor mutates anything; an
            // unknown path replies with empty text (the peer simply has nothing
            // there). [sync-fork-diff]
            Message::DocContentRequest { path } => {
                let _ = peer; // enrollment already gated the connection.
                let text = match self.local_doc_for_path(&path)? {
                    Some(local) => self
                        .oplog
                        .materialize_accepted(&local.0)
                        .map_err(|e| Error::Transport(format!("materialize {path}: {e}")))?
                        .text,
                    None => String::new(),
                };
                Ok(Message::DocContentResponse { text })
            }
            Message::StateRequest { logical_id } => {
                // The peer wants our canonical base for this logical id. Resolve
                // it to a local doc; if we aren't bound yet, bind by minting the
                // proposed id against the matching local doc (the dialer already
                // chose the canonical id deterministically).
                let local = self.local_for_logical(&logical_id);
                let Some(local) = local else {
                    return Err(Error::Transport(format!(
                        "state requested for unbound logical id {logical_id}"
                    )));
                };
                let state = self
                    .oplog
                    .export_state(&local.0)
                    .map_err(|e| Error::Transport(format!("export_state: {e}")))?;
                Ok(Message::LineageBase { logical_id, state })
            }
            Message::DeltaRequest {
                logical_id,
                state_vector,
            } => {
                let Some(local) = self.local_for_logical(&logical_id) else {
                    return Err(Error::Transport(format!(
                        "delta requested for unbound logical id {logical_id}"
                    )));
                };
                let delta = self
                    .oplog
                    .export_since(&local.0, &state_vector)
                    .map_err(|e| Error::Transport(format!("export_since: {e}")))?;
                let key = self.content_key.get();
                let ciphertext = key.encrypt(&delta);
                let blind_id = crypto::blind_id(&key, &logical_id);
                Ok(Message::UpdateBlob {
                    blind_id,
                    seq: 0,
                    ciphertext,
                })
            }
            // The pusher's "keep mine" converge: it has made ITS version
            // canonical and pushed its exact Yrs base. We (the peer) adopt that
            // base — replacing our diverged doc, discarding our local branch
            // ("keep mine" means the pusher's version wins) — then bind the
            // logical id and clear any block / pending resolution we had for it.
            // Clearing OUR resolution is what prevents a flap: if we also had a
            // keep-mine queued, we no longer push back next round. Resolve the
            // local doc by `path`; if we have none there yet, create one to hold
            // the adopted lineage. Because we adopt the pusher's EXACT base, both
            // sides now share its lineage → later deltas are safe (no
            // interleave). `peer` is already enrollment-gated. The pushed `state`
            // rides the Noise channel and is never logged.
            // [sync-blocked-state, sync-lineage-adoption]
            Message::PushAdopt {
                logical_id,
                path,
                state,
            } => {
                let _ = peer; // enrollment already gated the connection.
                let local = match self.local_doc_for_path(&path)? {
                    Some(existing) => existing,
                    None => self.create_local_for(&path)?,
                };
                let device_id = self.peer_fingerprint(peer).0;
                self.oplog
                    .adopt_lineage_theirs(&local.0, &state, &device_id)
                    .map_err(|e| Error::Transport(format!("adopt_lineage_theirs: {e}")))?;
                let logical = LogicalId(logical_id.clone());
                self.bind_local(local, logical.clone());
                self.clear_blocked(&logical);
                Ok(Message::PushAdoptAck { logical_id })
            }
            // Bind handshake messages: the responder records the binding the
            // dialer proposes and acks. `peer` is already enrolled.
            Message::BindRequest { path, logical_id } => {
                let _ = peer; // enrollment already gated the connection.
                if let Some(local) = self.local_doc_for_path(&path)? {
                    self.bind_local(local, LogicalId(logical_id.clone()));
                }
                Ok(Message::BindAck { logical_id })
            }
            other => Err(Error::Transport(format!(
                "unexpected request on responder: {other:?}"
            ))),
        }
    }

    /// Resolve a logical id to its local doc id via the binding table.
    fn local_for_logical(&self, logical_id: &str) -> Option<LocalDocId> {
        self.bindings
            .lock()
            .unwrap()
            .local_for(&LogicalId(logical_id.to_string()))
            .cloned()
    }

    /// Resolve a vault-relative path to a local doc id.
    fn local_doc_for_path(&self, path: &str) -> Result<Option<LocalDocId>, Error> {
        Ok(self
            .oplog
            .doc_id_for_path(path)
            .map_err(|e| Error::Transport(format!("doc_id_for_path: {e}")))?
            .map(LocalDocId))
    }

    /// Record a binding and flip the doc's status to `Bound`.
    fn bind_local(&self, local: LocalDocId, logical: LogicalId) {
        self.bindings.lock().unwrap().bind(Binding {
            local_doc_id: local.clone(),
            logical_id: logical,
        });
        self.status.lock().unwrap().insert(local, SyncStatus::Bound);
    }

    /// Drive the swarm event loop as a responder, answering inbound requests
    /// from enrolled peers until `window` elapses. Used by a listening node
    /// while a peer drives [`sync_once`]; also the basis for always-on serving.
    /// Non-enrolled connections are dropped. [sync-noise-channel]
    pub async fn run(&mut self, window: Duration) -> Result<(), Error> {
        self.ensure_swarm()?;
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => return Ok(()),
                event = self.swarm_mut().select_next_some() => {
                    self.handle_swarm_event(event);
                }
            }
        }
    }

    /// Handle one swarm event on the responder path: enrollment-gate new
    /// connections and answer request-response requests.
    fn handle_swarm_event(&mut self, event: SwarmEvent<SyncBehaviourEvent>) {
        match event {
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. }
                if !self.is_enrolled(&peer_id) =>
            {
                // A peer dialed us but we haven't enrolled it: authenticated by
                // Noise but not trusted. Surface it in the discovery set first —
                // so it shows under "Seen on LAN (not enrolled)" with an Enroll
                // button even when mDNS is asymmetric and only the *connection*
                // revealed the peer (otherwise mutual enrollment can't complete
                // from the UI). Then drop it; we don't serve un-enrolled peers.
                // [sync-noise-channel, sync-discovered-peers]
                let addr = endpoint.get_remote_address().clone();
                self.record_discovered([(peer_id, addr)]);
                tracing::warn!(
                    peer = %peer_id,
                    "sync: dropping connection from un-enrolled peer — enroll its fingerprint on THIS device to let it sync"
                );
                let _ = self.swarm_mut().disconnect_peer_id(peer_id);
            }
            SwarmEvent::Behaviour(SyncBehaviourEvent::Rr(
                request_response::Event::Message {
                    peer,
                    message:
                        request_response::Message::Request {
                            request, channel, ..
                        },
                    ..
                },
            )) => {
                // Compute the reply WITHOUT `?`-bailing: a handler error (or a
                // request from a peer we haven't enrolled) becomes an explicit
                // `Message::Error` reply rather than a dropped channel, so the
                // dialer surfaces the real reason instead of an opaque
                // "connection closed before a response" — and one bad request
                // never tears down this responder window.
                let reply = if !self.is_enrolled(&peer) {
                    tracing::warn!(peer = %peer, "sync: refusing request from un-enrolled peer");
                    Message::Error {
                        reason: "not enrolled on the remote device — enroll this device's \
                                 fingerprint there to sync"
                            .to_string(),
                    }
                } else {
                    match self.handle_request(&peer, request) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "sync: request handler error; replying with error");
                            Message::Error { reason: e.to_string() }
                        }
                    }
                };
                // Send-response only fails if the channel timed out; a dropped
                // response surfaces to the peer as an outbound failure.
                let _ = self.swarm_mut().behaviour_mut().rr.send_response(channel, reply);
            }
            // Continuously fold mDNS discovery into the always-on candidate set
            // so auto-sync has targets without a manual Discover window. Gated
            // on `[sync].discovery`: with it off we don't track LAN peers (and
            // so never auto-dial them). [sync-mdns-discovery]
            SwarmEvent::Behaviour(SyncBehaviourEvent::Mdns(mdns::Event::Discovered(peers)))
                if self.config.discovery =>
            {
                self.record_discovered(peers);
            }
            SwarmEvent::Behaviour(SyncBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                self.record_expired(peers);
            }
            _ => {}
        }
    }

    // --- dialer / one-shot ----------------------------------------------

    /// Dial `peer`, run the full Hello + Manifest + classify + adopt/stream
    /// flow, and return a [`SyncReport`]. The peer must be running [`run`] (or
    /// otherwise driving its swarm) to answer. The active node here is the
    /// "puller": it converges its own replicas toward the peer.
    pub async fn sync_once(&mut self, peer: &str) -> Result<SyncReport, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(peer)?;
        let peer_id = self.connect(addr).await?;

        // 1. Hello handshake — exchange device + content-key fingerprints.
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_fingerprint: self.fingerprint.0.clone(),
            content_key_fp: self.content_key.fingerprint(),
        };
        let peer_content_key_fp = match self.request(peer_id, hello).await? {
            Message::HelloAck { content_key_fp, .. } => content_key_fp,
            other => {
                return Err(Error::Transport(format!("expected HelloAck, got {other:?}")));
            }
        };

        // 1b. In-band content-key convergence, BEFORE any docs — so subsequent
        // content-encrypted deltas + blind-ids match on both sides.
        // [sync-vault-key-inband]
        self.converge_content_key(peer_id, &peer_content_key_fp).await?;

        // 2. Pull the peer's manifest.
        let manifest = match self.request(peer_id, Message::ManifestRequest).await? {
            Message::Manifest(m) => m,
            other => {
                return Err(Error::Transport(format!("expected Manifest, got {other:?}")));
            }
        };

        // 3. Classify + act per entry.
        let mut report = SyncReport::default();
        for entry in manifest.entries {
            self.sync_entry(peer_id, entry, &mut report).await?;
        }
        Ok(report)
    }

    /// Dial `peer`, Hello-handshake, and fetch the current accepted text of one
    /// document by its vault-relative `path` — the read-only "view diff" probe
    /// for a forked document. Returns the peer's `materialize(accepted).text`
    /// for `path` (empty when the peer has no doc there). The peer must be
    /// running [`run`](Self::run) (or otherwise driving its swarm) to answer.
    ///
    /// This is a pure read: it does not bind, classify, adopt, or stream — it
    /// neither touches our local doc nor changes any sync status. The text rides
    /// the Noise-encrypted channel, gated on enrollment like every other
    /// request. [sync-fork-diff]
    // status: sync-fork-diff
    pub async fn fetch_doc_text(&mut self, peer: &str, path: &str) -> Result<String, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(peer)?;
        let peer_id = self.connect(addr).await?;

        // Hello first, like every dialer flow, so the peer records our
        // fingerprint and the request rides an established session.
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_fingerprint: self.fingerprint.0.clone(),
            content_key_fp: self.content_key.fingerprint(),
        };
        match self.request(peer_id, hello).await? {
            Message::HelloAck { .. } => {}
            other => {
                return Err(Error::Transport(format!("expected HelloAck, got {other:?}")));
            }
        }

        let req = Message::DocContentRequest {
            path: path.to_string(),
        };
        match self.request(peer_id, req).await? {
            Message::DocContentResponse { text } => Ok(text),
            other => Err(Error::Transport(format!(
                "expected DocContentResponse, got {other:?}"
            ))),
        }
    }

    /// Converge on ONE shared vault content key over the authenticated channel,
    /// so both enrolled devices encrypt/decrypt deltas under the same key and
    /// the manual Copy/Import step is unnecessary. Run right after the Hello
    /// exchange (before any docs). [sync-vault-key-inband]
    ///
    /// - If our content-key fingerprint already matches the peer's → both
    ///   already share a key; do nothing.
    /// - Else pick a deterministic key owner: `canonical = min(our device
    ///   fingerprint, peer device fingerprint)`. If WE are non-canonical, request
    ///   the canonical device's key in-band and adopt it (the shared handle
    ///   persists it). If WE are canonical, do nothing — the peer requests from
    ///   us on its own round.
    ///
    /// The deterministic rule means exactly one side adopts; after first contact
    /// in both directions both hold the canonical device's key.
    // status: sync-vault-key-inband
    async fn converge_content_key(
        &mut self,
        peer_id: PeerId,
        peer_content_key_fp: &str,
    ) -> Result<(), Error> {
        // Already the same key — nothing to transfer.
        if self.content_key.fingerprint() == peer_content_key_fp {
            return Ok(());
        }
        // Deterministic owner by device fingerprint (peer's via the enrolled set,
        // falling back to its peer-id string if the mapping is somehow missing).
        let peer_fp = self
            .enrolled
            .fingerprint_of(&peer_id)
            .map(|fp| fp.0)
            .unwrap_or_else(|| peer_id.to_string());
        let canonical_is_us = self.fingerprint().0 < peer_fp;
        if canonical_is_us {
            // We own the key this round; the peer will request it from us.
            return Ok(());
        }
        // We are non-canonical: pull the canonical device's key in-band and adopt
        // it. The raw bytes ride the Noise-encrypted channel and are NEVER logged.
        let key = match self.request(peer_id, Message::ContentKeyRequest).await? {
            Message::ContentKeyResponse { key } => key,
            other => {
                return Err(Error::Transport(format!(
                    "expected ContentKeyResponse, got {other:?}"
                )));
            }
        };
        if key.len() != 32 {
            return Err(Error::InvalidKey(format!(
                "in-band content key must be 32 bytes, got {}",
                key.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&key);
        // Routes through the shared handle: updates in place AND persists.
        self.content_key.set(ContentKey::from_bytes(arr));
        Ok(())
    }

    /// Process one remote manifest entry: bind, then adopt or stream.
    async fn sync_entry(
        &mut self,
        peer_id: PeerId,
        entry: ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        // Resolve our local replica. Identity is the binding once established, so
        // a doc the peer already bound is resolved by its logical id first — that
        // is what survives a rename: the peer's `meta.path` moved, but our local
        // replica is still the same logical document, just at the old path. Only
        // an as-yet-unbound entry falls back to the one-time path matching key.
        // [sync-path-matching-key, sync-negotiated-doc-ids]
        let local = match entry
            .logical_id
            .as_ref()
            .and_then(|lid| self.local_for_logical(lid))
        {
            Some(bound_local) => Some(bound_local),
            None => self.local_doc_for_path(&entry.path)?,
        };

        // Pick the canonical logical id. If either side is already bound, reuse
        // it; else mint a fresh ULID. The dialer chooses, then proposes it to
        // the peer so both sides agree. [sync-negotiated-doc-ids]
        //
        // A doc that forked on a prior round is never bound, so without this it
        // would mint a *fresh* id every round and the user's resolution decision
        // (keyed by the recorded blocked entry's logical id) would never match.
        // Reuse the recorded fork's logical id by path so the decision lands.
        // [sync-blocked-state]
        let our_logical = local
            .as_ref()
            .and_then(|l| self.bindings.lock().unwrap().logical_for(l).cloned());
        let logical = match (our_logical, entry.logical_id.clone()) {
            (Some(ours), _) => ours,
            (None, Some(theirs)) => LogicalId(theirs),
            (None, None) => self
                .blocked_logical_for_path(&entry.path)
                .unwrap_or_else(|| LogicalId(ulid::Ulid::new().to_string())),
        };

        match local {
            // We have no local replica of this path: it's new to us. Create a
            // placeholder doc at the path, bind it, and adopt the peer's
            // canonical lineage. [sync-lineage-adoption]
            None => {
                let local = self.create_local_for(&entry.path)?;
                self.bind_local(local.clone(), logical.clone());
                self.propose_bind(peer_id, &entry.path, &logical).await?;
                self.adopt_from_peer(peer_id, &local, &logical).await?;
                report.bound.push(logical.clone());
                report.converged.push(logical);
            }
            // We already share a lineage with the peer (bound from a prior
            // contact): identity is settled, so enrollment classification no
            // longer applies. Updates stream both directions — pull the
            // incremental delta and let Yrs merge it. Concurrent disjoint edits
            // merge positionally (not a fork — a fork has no shared lineage),
            // and a peer-side rename rides in as the `meta.path` op on that same
            // lineage. A blocked doc streams nothing until the user resolves it.
            // [sync-blocked-state, sync-stream-muxing]
            Some(local) if self.bindings.lock().unwrap().is_bound(&local) => {
                if self.status_of(&local) == Some(SyncStatus::Blocked) {
                    report.blocked.push((entry.path.clone(), "fork".to_string()));
                    return Ok(());
                }
                self.apply_delta_from_peer(peer_id, &local, &logical).await?;
                report.bound.push(logical.clone());
                report.converged.push(logical);
            }
            // First contact for a doc we hold locally: classify against the
            // peer's content-hash history before any merge. [sync-enrollment-hash-classification]
            Some(local) => {
                let ours_current = self.current_hash(&local.0)?;
                let ours_history = self.history_set(&local.0)?;
                let theirs_history: HashSet<String> =
                    entry.recent_history_hashes.iter().cloned().collect();
                let class = enroll::classify(
                    &ours_current,
                    &ours_history,
                    &entry.current_hash,
                    &theirs_history,
                );
                if matches!(class, Classification::Fork) {
                    // Diagnostic: a fork means our content differs from the peer's
                    // with no shared content-history. For vaults that should be
                    // identical copies this is usually a UNIFORM difference (line
                    // endings, a trailing newline, a BOM, frontmatter on one
                    // side) — surface the hashes + our materialized length so the
                    // cause is visible instead of an opaque "everything forked".
                    let our_text =
                        self.oplog.materialize_accepted(&local.0).map(|m| m.text).unwrap_or_default();
                    // Escaped prefix so line endings / a leading BOM / frontmatter
                    // are visible — compare this line across the two instances'
                    // logs for the same path to see exactly where they diverge.
                    let head: String =
                        our_text.chars().take(48).collect::<String>().escape_default().to_string();
                    tracing::warn!(
                        path = %entry.path,
                        ours = %&ours_current[..ours_current.len().min(12)],
                        theirs = %&entry.current_hash[..entry.current_hash.len().min(12)],
                        our_bytes = our_text.len(),
                        ours_hist = ours_history.len(),
                        theirs_hist = theirs_history.len(),
                        head = %head,
                        "sync: fork — content differs with no shared history"
                    );
                }
                self.act_on_classification(peer_id, &local, &logical, &entry, class, report)
                    .await?;
            }
        }
        Ok(())
    }

    /// Act on the enrollment classification for a doc we already hold locally.
    async fn act_on_classification(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
        entry: &ManifestEntry,
        class: Classification,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        match class {
            Classification::Identical => {
                // Same content, but two independently-seeded vaults have
                // DISJOINT Yrs lineages (different client ids over the same
                // bytes). Binding now and letting a later round take the
                // steady-state delta path would be a correctness bug: our state
                // vector is meaningless to a disjoint-lineage peer, so its
                // `export_since` returns its ENTIRE doc and applying it inserts a
                // SECOND copy of the body alongside ours (the duplication bug).
                //
                // The cure is to establish a SHARED lineage before any delta.
                // Pick the canonical side deterministically by device
                // fingerprint so both sides agree without negotiating; the
                // non-canonical side adopts the canonical base (content-safe —
                // the content is identical) and only THEN binds. The canonical
                // side does nothing this round: the peer will classify us as
                // `FastForwardAdoptPeer`, adopt us, and send a `BindRequest`,
                // which binds our side post-adoption (shared lineage).
                let peer_fp = self
                    .enrolled
                    .fingerprint_of(&peer_id)
                    .map(|fp| fp.0)
                    .unwrap_or_else(|| peer_id.to_string());
                let canonical_is_us = self.fingerprint().0 < peer_fp;
                if canonical_is_us {
                    // We are canonical: do NOT bind, do NOT pull. The peer adopts
                    // us and binds us via its `BindRequest` once the lineage is
                    // shared. Clearing a stale fork record is still safe.
                    self.clear_blocked(logical);
                } else {
                    // We are non-canonical: adopt the peer's base to establish a
                    // shared lineage (identical content, so nothing is lost),
                    // then bind. Only after this is the delta path safe.
                    //
                    // Propose the bind FIRST so the canonical peer resolves the
                    // logical id to its local doc before it serves our
                    // `StateRequest` (the responder resolves a `StateRequest` via
                    // its binding table, so the bind must precede the state pull).
                    self.propose_bind(peer_id, &entry.path, logical).await?;
                    self.adopt_from_peer(peer_id, local, logical).await?;
                    self.bind_local(local.clone(), logical.clone());
                    self.clear_blocked(logical);
                    report.bound.push(logical.clone());
                    report.converged.push(logical.clone());
                }
            }
            Classification::FastForwardAdoptPeer => {
                // First contact and we are behind: there is no shared lineage to
                // merge a delta onto yet, so adopt the peer's canonical base and
                // re-express our (fast-forward: none) divergence on it. Once
                // bound, later rounds take the steady-state delta path above.
                // [sync-lineage-adoption]
                //
                // Propose the bind FIRST so the canonical peer resolves the
                // logical id before serving our `StateRequest` (the responder
                // resolves a state pull via its binding table).
                self.propose_bind(peer_id, &entry.path, logical).await?;
                self.adopt_from_peer(peer_id, local, logical).await?;
                self.bind_local(local.clone(), logical.clone());
                self.clear_blocked(logical);
                report.bound.push(logical.clone());
                report.converged.push(logical.clone());
            }
            Classification::FastForwardPeerAdopts => {
                // The peer is behind: WE are canonical. Do NOT bind and do NOT
                // pull this round — binding now would make us eligible for the
                // steady-state delta path while our lineage is still disjoint
                // from the peer's, and a `DeltaRequest` across disjoint lineages
                // re-inserts the peer's whole body (the duplication bug). Instead
                // the behind peer classifies us as `FastForwardAdoptPeer` on its
                // own round, adopts our base (establishing a shared lineage), and
                // sends a `BindRequest`; we bind then — post-adoption, on the now
                // shared lineage. [sync-lineage-adoption]
                self.clear_blocked(logical);
            }
            Classification::Fork => {
                // A true fork. If the user picked a resolution on a prior round,
                // act on it now instead of re-blocking; otherwise block + record
                // it for the UI. [sync-blocked-state]
                self.resolve_fork(peer_id, local, logical, entry, report)
                    .await?;
            }
        }
        Ok(())
    }

    /// Handle a detected fork for a doc we hold locally: consume any pending
    /// resolution decision, or block + record it for the UI. With no decision
    /// set (the default) this blocks unchanged. Each resolution converges in a
    /// single round: keep-theirs / keep-both adopt the peer's lineage; keep-mine
    /// PUSHES our base for the peer to adopt (see the `KeepMine` arm), so all
    /// three resolve both sides on one click. [sync-blocked-state]
    async fn resolve_fork(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
        entry: &ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let decision = self.resolutions.lock().unwrap().get(logical).copied();
        match decision {
            None => {
                // No decision: block the doc, stream nothing, and record it
                // persistently for the UI. [sync-blocked-state]
                self.status
                    .lock()
                    .unwrap()
                    .insert(local.clone(), SyncStatus::Blocked);
                let peer = self.peer_fingerprint(&peer_id);
                self.record_blocked(logical, &entry.path, &peer);
                report.blocked.push((entry.path.clone(), "fork".to_string()));
            }
            Some(Resolution::KeepTheirs) => {
                // Adopt the peer's lineage, discarding our local divergence: the
                // user chose the peer's version. Bind, propose the id so the peer
                // resolves the StateRequest, then pull + adopt-theirs. Fully
                // convergent to the peer's content on this device.
                self.bind_local(local.clone(), logical.clone());
                self.propose_bind(peer_id, &entry.path, logical).await?;
                self.adopt_theirs_from_peer(peer_id, local, logical).await?;
                self.clear_blocked(logical);
                report.bound.push(logical.clone());
                report.converged.push(logical.clone());
            }
            Some(Resolution::KeepBoth) => {
                // Preserve the local version as a conflict copy alongside the
                // note (a normal indexed note via the op-log create path), THEN
                // keep-theirs: adopt the peer's lineage at the original path,
                // discarding the local branch there (it survives as the copy).
                let peer = self.peer_fingerprint(&peer_id);
                self.write_conflict_copy(local, &entry.path, &peer)?;
                self.bind_local(local.clone(), logical.clone());
                self.propose_bind(peer_id, &entry.path, logical).await?;
                self.adopt_theirs_from_peer(peer_id, local, logical).await?;
                self.clear_blocked(logical);
                report.bound.push(logical.clone());
                report.converged.push(logical.clone());
            }
            Some(Resolution::KeepMine) => {
                // Our version is canonical — and we converge BOTH sides in one
                // click by PUSHING our base so the peer adopts it. Our content is
                // unchanged; we bind our existing doc to the logical id (no pull,
                // no merge — `bind_local` only records the binding), then send the
                // peer our exact Yrs base (`export_state`) for it to adopt
                // (discarding its divergence — that's what "keep mine" means).
                //
                // This is lineage-safe precisely BECAUSE the peer adopts OUR
                // actual base: after the push both sides are on our lineage →
                // shared → the steady-state delta path is now safe (no
                // cross-lineage interleave). This is the crucial difference from
                // the old broken keep-mine, which bound without the peer adopting
                // (a disjoint-lineage bind that doubled on the next delta).
                //
                // The peer also clears any keep-mine it had queued when it
                // adopts, so whoever pushes first wins with no flapping (see
                // `PushAdopt` handler). [sync-blocked-state, sync-lineage-adoption]
                self.bind_local(local.clone(), logical.clone());
                self.propose_bind(peer_id, &entry.path, logical).await?;
                self.push_adopt_to_peer(peer_id, local, logical, &entry.path)
                    .await?;
                self.clear_blocked(logical);
                report.bound.push(logical.clone());
                report.converged.push(logical.clone());
            }
        }
        Ok(())
    }

    /// Write the local replica's current content to a sibling note in the vault
    /// as a fresh, indexed document — the keep-both conflict copy. Routed
    /// through the op-log `create_document` path so it shows up like any other
    /// note (its own logical id, indexed, materialized `.md`). Named
    /// `<stem> (conflict <peer-or-timestamp>).<ext>`. [sync-blocked-state]
    fn write_conflict_copy(
        &self,
        local: &LocalDocId,
        path: &str,
        peer: &DeviceFingerprint,
    ) -> Result<(), Error> {
        let text = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Transport(format!("materialize for conflict copy: {e}")))?
            .text;
        let copy_path = conflict_copy_path(path, &peer.0);
        self.oplog
            .create_document(&copy_path, "note", &text, &Author::User)
            .map_err(|e| Error::Transport(format!("create conflict copy: {e}")))?;
        Ok(())
    }

    /// Tell the peer the logical id we bound a path to, so both sides agree.
    async fn propose_bind(
        &mut self,
        peer_id: PeerId,
        path: &str,
        logical: &LogicalId,
    ) -> Result<(), Error> {
        let req = Message::BindRequest {
            path: path.to_string(),
            logical_id: logical.0.clone(),
        };
        match self.request(peer_id, req).await? {
            Message::BindAck { .. } => Ok(()),
            other => Err(Error::Transport(format!("expected BindAck, got {other:?}"))),
        }
    }

    /// Request the peer's canonical base for `logical` and adopt it locally.
    /// [sync-lineage-adoption]
    async fn adopt_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
    ) -> Result<(), Error> {
        let req = Message::StateRequest {
            logical_id: logical.0.clone(),
        };
        let state = match self.request(peer_id, req).await? {
            Message::LineageBase { state, .. } => state,
            other => {
                return Err(Error::Transport(format!(
                    "expected LineageBase, got {other:?}"
                )));
            }
        };
        self.oplog
            .adopt_lineage(&local.0, &state)
            .map_err(|e| Error::Transport(format!("adopt_lineage: {e}")))?;
        Ok(())
    }

    /// Request the peer's canonical base for `logical` and adopt it locally
    /// DISCARDING our local divergence — the keep-theirs fork-resolution path.
    /// Unlike [`adopt_from_peer`], the local branch does not survive: the doc
    /// materializes exactly the peer's content. [sync-blocked-state]
    async fn adopt_theirs_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
    ) -> Result<(), Error> {
        let req = Message::StateRequest {
            logical_id: logical.0.clone(),
        };
        let state = match self.request(peer_id, req).await? {
            Message::LineageBase { state, .. } => state,
            other => {
                return Err(Error::Transport(format!(
                    "expected LineageBase, got {other:?}"
                )));
            }
        };
        let device_id = self.peer_fingerprint(&peer_id).0;
        self.oplog
            .adopt_lineage_theirs(&local.0, &state, &device_id)
            .map_err(|e| Error::Transport(format!("adopt_lineage_theirs: {e}")))?;
        Ok(())
    }

    /// Push OUR canonical Yrs base to the peer so it adopts it — the "keep mine"
    /// converge half. Sends `export_state(local)` as the canonical base for
    /// `logical` at `path`; the peer replaces its diverged doc with it,
    /// establishing a shared lineage on OUR side's base. Our own doc is
    /// untouched (the push only reads `export_state`). The base rides the
    /// Noise-encrypted channel to the enrolled peer and is never logged.
    /// [sync-blocked-state, sync-lineage-adoption]
    async fn push_adopt_to_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
        path: &str,
    ) -> Result<(), Error> {
        let state = self
            .oplog
            .export_state(&local.0)
            .map_err(|e| Error::Transport(format!("export_state: {e}")))?;
        let req = Message::PushAdopt {
            logical_id: logical.0.clone(),
            path: path.to_string(),
            state,
        };
        match self.request(peer_id, req).await? {
            Message::PushAdoptAck { .. } => Ok(()),
            other => Err(Error::Transport(format!(
                "expected PushAdoptAck, got {other:?}"
            ))),
        }
    }

    /// Pull the peer's incremental update past our state vector and apply it via
    /// the receive path — the steady-state streaming case once both sides share
    /// the lineage. The update is content-decrypted, then `apply_remote_update`
    /// records a `sync:<peer>`-authored op. [sync-content-encryption-aes256]
    async fn apply_delta_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        logical: &LogicalId,
    ) -> Result<(), Error> {
        let state_vector = self
            .oplog
            .state_vector_bytes(&local.0)
            .map_err(|e| Error::Transport(format!("state_vector_bytes: {e}")))?;
        let req = Message::DeltaRequest {
            logical_id: logical.0.clone(),
            state_vector,
        };
        let ciphertext = match self.request(peer_id, req).await? {
            Message::UpdateBlob { ciphertext, .. } => ciphertext,
            other => {
                return Err(Error::Transport(format!(
                    "expected UpdateBlob, got {other:?}"
                )));
            }
        };
        let update = self.content_key.get().decrypt(&ciphertext)?;
        // Tag the op with the peer's enrolled fingerprint as the sync device id.
        let device_id = self
            .enrolled
            .fingerprint_of(&peer_id)
            .map(|fp| fp.0)
            .unwrap_or_else(|| peer_id.to_string());
        self.oplog
            .apply_remote_update(&local.0, &update, &device_id)
            .map_err(|e| Error::Transport(format!("apply_remote_update: {e}")))?;
        Ok(())
    }

    /// Create a fresh empty local document at `path` to hold an adopted lineage.
    fn create_local_for(&self, path: &str) -> Result<LocalDocId, Error> {
        let doc_id = self
            .oplog
            .create_document(path, "note", "", &Author::User)
            .map_err(|e| Error::Transport(format!("create_document: {e}")))?;
        Ok(LocalDocId(doc_id))
    }

    // --- discovery -------------------------------------------------------

    /// Run the manual, time-boxed mDNS discovery window: drive the swarm for
    /// `window`, collecting addresses for *enrolled* peers discovered on the
    /// LAN. Discovery only supplies candidates; a connection still authenticates
    /// against the enrolled fingerprints, so a discovered stranger never
    /// appears here. [sync-mdns-discovery]
    pub async fn start_discovery(
        &mut self,
        window: Duration,
    ) -> Result<Vec<PeerCandidate>, Error> {
        // The mDNS window is opt-in per vault; a disabled config never advertises
        // or browses. [sync-mdns-discovery]
        if !self.config.discovery {
            return Ok(Vec::new());
        }
        self.ensure_swarm()?;
        let deadline = tokio::time::Instant::now() + window;
        let mut found: Vec<PeerCandidate> = Vec::new();
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => return Ok(found),
                event = self.swarm_mut().select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(SyncBehaviourEvent::Mdns(
                            mdns::Event::Discovered(peers),
                        )) => {
                            // Fold into the continuous set too, so the manual
                            // window and the always-on tracker share state.
                            let peers: Vec<_> = peers.into_iter().collect();
                            self.record_discovered(peers.iter().cloned());
                            for (peer_id, addr) in peers {
                                if let Some(fp) = self.enrolled.fingerprint_of(&peer_id) {
                                    let cand = PeerCandidate {
                                        fingerprint: fp,
                                        addr: addr.to_string(),
                                    };
                                    if !found.contains(&cand) {
                                        found.push(cand);
                                    }
                                }
                            }
                        }
                        SwarmEvent::Behaviour(SyncBehaviourEvent::Mdns(
                            mdns::Event::Expired(peers),
                        )) => self.record_expired(peers),
                        _ => {}
                    }
                }
            }
        }
    }

    // --- server-mediated store-and-forward ------------------------------

    /// The offline-catch-up path: sync every already-bound document through the
    /// zero-knowledge hub at `server_addr`, which only relays opaque ciphertext.
    ///
    /// Binding itself never happens here — the hub can't classify or negotiate
    /// ids on ciphertext, so this path assumes docs are already bound via the
    /// P2P manifest exchange / enrollment (see [`bind_for_test`](Self::bind_for_test)).
    /// The two clients never talk directly; the server store-and-forwards.
    /// For each bound doc: [sync-zero-knowledge-server, sync-content-encryption-aes256]
    ///
    /// 1. **Push** — encrypt the doc's current Yrs state under the content key
    ///    and `UpdateBlob` it to the hub keyed by `blind_id(content_key,
    ///    logical_id)`, at this device's next monotonic per-blind-id seq. The
    ///    full state is itself a valid v2 update; Yrs `apply_update` is
    ///    idempotent, so a peer merging it (or a re-push) is harmless.
    /// 2. **Pull** — `CursorRequest` everything past our cursor for that blind
    ///    id, decrypt each blob, and `apply_remote_update` it. Our own pushed
    ///    blobs decrypt to already-known ops and merge as no-ops; a peer's blobs
    ///    converge our replica. The cursor advances to the batch's high-water seq.
    ///
    /// The server is dialed as an enrolled peer (its fingerprint must be enrolled
    /// on this node), so the same Noise + enrollment gate as the P2P path
    /// authenticates the hub connection. [sync-noise-channel]
    pub async fn sync_via_server(&mut self, server_addr: &str) -> Result<SyncReport, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(server_addr)?;
        let server_id = self.connect(addr).await?;

        // Snapshot the bound docs up front so we don't hold the binding lock
        // across the awaits below.
        let bound: Vec<(LocalDocId, LogicalId)> = self
            .bindings
            .lock()
            .unwrap()
            .iter()
            .map(|(l, g)| (l.clone(), g.clone()))
            .collect();

        let mut report = SyncReport::default();
        for (local, logical) in bound {
            // A blocked doc streams nothing in either direction. [sync-blocked-state]
            if self.status_of(&local) == Some(SyncStatus::Blocked) {
                continue;
            }
            let blind = {
                let key = self.content_key.get();
                crypto::blind_id(&key, &logical.0)
            };

            // 1. Push our current state as the next monotonic blob.
            self.push_state(server_id, &local, &blind).await?;
            report.bound.push(logical.clone());

            // 2. Pull + apply everything past our cursor.
            if self.pull_and_apply(server_id, &local, &blind).await? {
                report.converged.push(logical);
            }
        }
        Ok(report)
    }

    /// Push the local doc's current Yrs state to the hub as one content-encrypted
    /// `UpdateBlob` at this device's next per-blind-id seq.
    async fn push_state(
        &mut self,
        server_id: PeerId,
        local: &LocalDocId,
        blind: &str,
    ) -> Result<(), Error> {
        let state = self
            .oplog
            .export_state(&local.0)
            .map_err(|e| Error::Transport(format!("export_state: {e}")))?;
        let ciphertext = self.content_key.get().encrypt(&state);
        let seq = {
            let mut seqs = self.server_push_seq.lock().unwrap();
            let next = seqs.get(blind).copied().unwrap_or(0) + 1;
            seqs.insert(blind.to_string(), next);
            next
        };
        let req = Message::UpdateBlob {
            blind_id: blind.to_string(),
            seq,
            ciphertext,
        };
        match self.request(server_id, req).await? {
            Message::PushAck { .. } => Ok(()),
            other => Err(Error::Transport(format!("expected PushAck, got {other:?}"))),
        }
    }

    /// Pull every blob past our cursor for `blind`, decrypt, and apply. Returns
    /// `true` if any applied update advanced local state.
    async fn pull_and_apply(
        &mut self,
        server_id: PeerId,
        local: &LocalDocId,
        blind: &str,
    ) -> Result<bool, Error> {
        let after = self
            .server_pull_cursor
            .lock()
            .unwrap()
            .get(blind)
            .copied()
            .unwrap_or(0);
        let req = Message::CursorRequest {
            blind_id: blind.to_string(),
            after_seq: after,
        };
        let blobs = match self.request(server_id, req).await? {
            Message::BlobBatch { blobs, .. } => blobs,
            other => {
                return Err(Error::Transport(format!("expected BlobBatch, got {other:?}")));
            }
        };
        let device_id = self
            .enrolled
            .fingerprint_of(&server_id)
            .map(|fp| fp.0)
            .unwrap_or_else(|| server_id.to_string());
        let mut advanced = false;
        let mut high = after;
        for (seq, ciphertext) in blobs {
            let update = self.content_key.get().decrypt(&ciphertext)?;
            if self
                .oplog
                .apply_remote_update(&local.0, &update, &device_id)
                .map_err(|e| Error::Transport(format!("apply_remote_update: {e}")))?
            {
                advanced = true;
            }
            high = high.max(seq);
        }
        self.server_pull_cursor
            .lock()
            .unwrap()
            .insert(blind.to_string(), high);
        Ok(advanced)
    }

    // --- low-level request/response driver -------------------------------

    /// Drive the swarm until an outbound connection to `addr` establishes, then
    /// verify the peer is enrolled. Returns the authenticated `PeerId`.
    /// [sync-noise-channel]
    async fn connect(&mut self, addr: Multiaddr) -> Result<PeerId, Error> {
        self.swarm_mut()
            .dial(addr)
            .map_err(|e| Error::Transport(format!("dial: {e}")))?;
        loop {
            match self.swarm_mut().select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    if !self.is_enrolled(&peer_id) {
                        let _ = self.swarm_mut().disconnect_peer_id(peer_id);
                        return Err(Error::Transport(
                            "connected peer is not enrolled".to_string(),
                        ));
                    }
                    return Ok(peer_id);
                }
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    return Err(Error::Transport(format!("dial failed: {error}")));
                }
                _ => {}
            }
        }
    }

    /// Send one request to `peer_id` and drive the swarm until its response (or
    /// an outbound failure) arrives. One request in flight at a time keeps the
    /// dialer's state machine sequential; yamux still muxes the substreams.
    async fn request(&mut self, peer_id: PeerId, msg: Message) -> Result<Message, Error> {
        let want: OutboundRequestId =
            self.swarm_mut().behaviour_mut().rr.send_request(&peer_id, msg);
        loop {
            match self.swarm_mut().select_next_some().await {
                SwarmEvent::Behaviour(SyncBehaviourEvent::Rr(
                    request_response::Event::Message {
                        message:
                            request_response::Message::Response {
                                request_id,
                                response,
                            },
                        ..
                    },
                )) if request_id == want => {
                    // A responder that couldn't serve the request replies with
                    // `Message::Error` instead of dropping the channel — surface
                    // its reason rather than treating it as an unexpected message.
                    if let Message::Error { reason } = response {
                        return Err(Error::Transport(format!("peer refused: {reason}")));
                    }
                    return Ok(response);
                }
                SwarmEvent::Behaviour(SyncBehaviourEvent::Rr(
                    request_response::Event::OutboundFailure {
                        request_id, error, ..
                    },
                )) if request_id == want => {
                    return Err(Error::Transport(format!(
                        "request failed: {error} — if this repeats, make sure THIS device's \
                         fingerprint is enrolled on the peer"
                    )));
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::crypto::{self, ContentKey, DeviceKeypair, SharedContentKey};

    fn mk_node(config: Settings) -> SyncNode {
        let dir = tempfile::tempdir().unwrap();
        let oplog = Arc::new(OpLog::open(dir.path()).unwrap());
        // Keep the temp dir alive for the node's lifetime via a leak — the
        // node only reads the oplog handle, and these are pure in-memory
        // discovery-bookkeeping tests with no swarm built.
        std::mem::forget(dir);
        SyncNode::new(
            oplog,
            SharedContentKey::new(ContentKey::generate()),
            DeviceKeypair::generate(),
            config,
            EnrolledPeers::new(),
        )
    }

    /// A fresh enrolled peer's `(PeerId, fingerprint)` for feeding the
    /// discovery folders directly (the mechanism the responder loop drives).
    fn enrolled_peer(node: &SyncNode) -> (PeerId, DeviceFingerprint) {
        let fp = DeviceKeypair::generate().fingerprint();
        node.enroll_peer(fp.clone()).unwrap();
        let peer_id = crypto::fingerprint_to_peer_id(&fp).unwrap();
        (peer_id, fp)
    }

    fn addr() -> Multiaddr {
        "/ip4/192.168.1.5/tcp/40123".parse().unwrap()
    }

    /// `record_discovered` tracks an enrolled peer continuously and flags it as
    /// newly seen exactly once; `take_newly_discovered` clears the flag.
    #[test]
    fn continuous_discovery_tracks_enrolled_and_flags_new() {
        let node = mk_node(Settings::default());
        let (peer_id, _fp) = enrolled_peer(&node);

        node.record_discovered([(peer_id, addr())]);
        assert_eq!(node.discovered_peers().len(), 1, "enrolled peer tracked");
        assert!(node.take_newly_discovered(), "first sight flags newly-discovered");
        assert!(!node.take_newly_discovered(), "flag cleared after read");

        // Re-seeing the same peer (address churn) doesn't re-flag.
        node.record_discovered([(peer_id, addr())]);
        assert!(!node.take_newly_discovered(), "known peer doesn't re-flag");
        assert_eq!(node.discovered_peers().len(), 1, "no duplicate entry");
    }

    /// A non-enrolled discovered peer is never tracked — discovery never
    /// bypasses enrollment. [sync-mdns-discovery]
    #[test]
    fn continuous_discovery_drops_non_enrolled() {
        let node = mk_node(Settings::default());
        let stranger = crypto::fingerprint_to_peer_id(
            &DeviceKeypair::generate().fingerprint(),
        )
        .unwrap();

        node.record_discovered([(stranger, addr())]);
        assert!(node.discovered_peers().is_empty(), "stranger not tracked");
        assert!(!node.take_newly_discovered(), "stranger doesn't flag");
    }

    /// Un-enrolling a peer drops it from the enrolled set and from the
    /// continuous discovery candidates, so it stops being an auto-dial target.
    #[test]
    fn unenroll_peer_drops_enrollment_and_candidate() {
        let node = mk_node(Settings::default());
        let (peer_id, fp) = enrolled_peer(&node);
        node.record_discovered([(peer_id, addr())]);
        assert_eq!(node.discovered_peers().len(), 1);
        assert!(node.is_enrolled(&peer_id), "enrolled before");

        node.unenroll_peer(&fp).unwrap();
        assert!(!node.is_enrolled(&peer_id), "no longer enrolled");
        assert!(node.discovered_peers().is_empty(), "candidate dropped too");

        // Un-enrolling an unknown fingerprint is a no-op (not an error).
        let other = DeviceKeypair::generate().fingerprint();
        node.unenroll_peer(&other).unwrap();
    }

    /// A non-enrolled discovered peer is recorded in the unenrolled-seen set
    /// (for UI visibility) and reported once via `take_newly_seen_unenrolled`,
    /// while still being kept OUT of the enrolled dial candidates.
    #[test]
    fn unenrolled_peer_is_seen_but_not_a_dial_candidate() {
        let node = mk_node(Settings::default());
        let stranger = crypto::fingerprint_to_peer_id(
            &DeviceKeypair::generate().fingerprint(),
        )
        .unwrap();

        node.record_discovered([(stranger, addr())]);
        // Not a dial candidate (enrollment gate unchanged).
        assert!(node.discovered_peers().is_empty(), "stranger not a dial candidate");
        // But visible in the unenrolled-seen surface.
        let seen = node.seen_unenrolled();
        assert_eq!(seen.len(), 1, "stranger recorded as seen-unenrolled");
        assert_eq!(seen[0].0, stranger.to_string(), "peer id surfaced");

        // First-seen reported exactly once.
        let first = node.take_newly_seen_unenrolled();
        assert_eq!(first.len(), 1, "first sight reported once");
        assert!(
            node.take_newly_seen_unenrolled().is_empty(),
            "no repeat without a new peer"
        );

        // Re-seeing the same unenrolled peer doesn't re-report it.
        node.record_discovered([(stranger, addr())]);
        assert!(
            node.take_newly_seen_unenrolled().is_empty(),
            "known unenrolled peer doesn't re-report"
        );
    }

    /// Enrolling a previously-seen unenrolled peer promotes it from the
    /// unenrolled-seen set to the enrolled dial candidates IMMEDIATELY — with NO
    /// second mDNS event. This is the read-time-classification fix: discovery
    /// keeps a single map and `discovered_peers()` / `seen_unenrolled()` split
    /// it against the LIVE enrolled set on read, so an enroll reclassifies an
    /// already-seen peer the moment the next round reads `discovered_peers()`.
    #[test]
    fn enrolling_a_seen_peer_promotes_it_to_candidate() {
        let node = mk_node(Settings::default());
        let fp = DeviceKeypair::generate().fingerprint();
        let peer_id = crypto::fingerprint_to_peer_id(&fp).unwrap();

        // Seen before enrollment: in the unenrolled-seen surface, not a candidate.
        node.record_discovered([(peer_id, addr())]);
        assert_eq!(node.seen_unenrolled().len(), 1);
        assert!(node.discovered_peers().is_empty());

        // Enroll only — NO new mDNS event. Read-time classification alone must
        // promote it: now a dial candidate, no longer "seen".
        node.enroll_peer(fp).unwrap();
        assert!(
            node.seen_unenrolled().is_empty(),
            "no longer in seen set after enroll, with no new mDNS event"
        );
        assert_eq!(
            node.discovered_peers().len(),
            1,
            "promoted to a dial candidate on enroll alone — no re-announce"
        );
    }

    /// An mDNS `Expired` event drops the peer from the continuous set.
    #[test]
    fn continuous_discovery_drops_on_expiry() {
        let node = mk_node(Settings::default());
        let (peer_id, _fp) = enrolled_peer(&node);

        node.record_discovered([(peer_id, addr())]);
        assert_eq!(node.discovered_peers().len(), 1);
        node.record_expired([(peer_id, addr())]);
        assert!(node.discovered_peers().is_empty(), "expired peer dropped");
    }
}
