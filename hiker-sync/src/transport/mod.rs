//! libp2p transport + the peer sync-session state machine.
//!
//! This module owns the public types (`SyncNode`, `SyncReport`, `EnrolledPeers`,
//! `PeerCandidate`) and the swarm/behaviour wiring. The state-machine surface
//! is split across sibling files by **role**, because the dialer and responder
//! halves of the same connection have wildly different concerns (initiating vs
//! answering, optimistic vs defensive, mutating-self vs read-mostly) and the
//! server-mediated store-and-forward path is yet another shape again:
//!
//! - [`responder`] — inbound side: `run` event loop, `handle_request` dispatch,
//!   manifest construction, rename-source resolution.
//! - [`dialer`] — outbound side: `sync_once` round, manifest walk, per-entry
//!   classification, content-key convergence, and the low-level
//!   request/response driver.
//! - [`lineage`] — first-contact lineage adoption verbs (`adopt_from_peer`,
//!   `adopt_theirs_from_peer`, `push_adopt_to_peer`, `apply_delta_from_peer`,
//!   `create_local_for`).
//! - [`fork`] — fork classification action + resolution (`act_on_classification`,
//!   `resolve_fork`, `write_conflict_copy`) and the conflict-copy naming.
//! - [`server`] — the server-mediated (relay store-and-forward) sync path.
//!
//! Each sibling is a pure `impl SyncNode` continuation (no module-level item
//! definitions of its own), the legitimate big-type-split shape called out in
//! `scripts/check-splits.py`.
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
//! The session is dialer-driven: `SyncNode::sync_once` dials a peer, then runs
//! a sequence of request-response round trips while the peer's `SyncNode::run`
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
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{mdns, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm};

use hiker_core::oplog::OpLog;

use crate::config::Settings;
use crate::crypto::{self, ContentKey, DeviceKeypair, SharedContentKey};
use crate::identity::{BlockStore, BlockedDoc, DeviceFingerprint, Resolution, SyncStatus};
// Note: path is the cross-device identity; no `LogicalId` rides the wire here.
// [sync-path-identity]
use crate::protocol::Message;
use crate::server::{BlobStore, MemBlobStore};
use crate::Error;

mod dialer;
mod fork;
mod lineage;
mod responder;
mod server;

/// The wire protocol version sent in `Hello`.
pub(crate) const PROTOCOL_VERSION: u32 = 1;

/// How many recent `content_hash` values a manifest entry carries for
/// fast-forward classification. Bounded so a long-lived document's manifest row
/// stays small.
pub(crate) const RECENT_HISTORY_WINDOW: usize = 32;

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
    /// Vault paths that successfully streamed this round (bound + on a shared
    /// lineage with the peer). Keyed by path — the cross-device identity.
    /// [sync-path-identity]
    pub bound: Vec<String>,
    /// Vault paths whose local replica converged to the peer (adopted a base or
    /// applied a delta).
    pub converged: Vec<String>,
    /// `(path, reason)` for each document left unsynced — a fork is `"fork"`.
    pub blocked: Vec<(String, String)>,
    /// `(path, reason)` for each document that hit a DOC-LEVEL error this round
    /// (e.g. a decrypt failure or a per-doc apply failure) and was SKIPPED so
    /// the round could continue with the remaining docs. A transport-level
    /// failure (the connection itself broke) aborts the round instead and never
    /// lands here. Empty on a clean round.
    pub errored: Vec<(String, String)>,
    /// Set to the peer's device fingerprint when this round's content-key
    /// convergence was HELD because our key is established (deliberately set)
    /// and the peer's differs — we did NOT silently switch. The app surfaces it
    /// (and the Sync page later renders a confirm action; the manual import is
    /// the accept-the-change path for now). `None` when the key matched or was
    /// freshly auto-adopted. [sync-content-key-confirm-on-change]
    pub pending_content_key_change: Option<String>,
    /// Set to the peer's device fingerprint when a FRESH (non-established) key
    /// auto-adopted the peer's key in-band this round — surfaced so the user
    /// sees the adoption happened. `None` otherwise.
    /// [sync-content-key-confirm-on-change]
    pub adopted_content_key_from: Option<String>,
}

/// The result of one round's in-band content-key convergence (the dialer's
/// [`SyncNode::converge_content_key`] step). Carried back onto the
/// [`SyncReport`] so the app surfaces an adoption or a held key change.
/// [sync-content-key-confirm-on-change]
pub(super) enum ContentKeyOutcome {
    /// Keys already matched, or we are the canonical owner — nothing changed.
    Unchanged,
    /// A fresh (non-established) key auto-adopted the peer's key in-band. Carries
    /// the peer's device fingerprint for the surfaced line.
    Adopted { peer_fp: String },
    /// Our key is established and the peer's differs — we held our key (did NOT
    /// switch) and surface the mismatch for confirmation. Carries the peer's
    /// device fingerprint.
    PendingChange { peer_fp: String },
}

/// A peer sync node: owns the vault's [`OpLog`], the content + device keys, the
/// binding table, config, the enrolled-peer set, and a per-doc sync-status map,
/// plus an (in-memory) [`crate::server::BlobStore`] for the server-mediated
/// path. The libp2p `Swarm` is built lazily on first `listen`/`dial`/`sync_once`
/// and held here so the event loop and the one-shot driver share it.
pub struct SyncNode {
    pub(super) oplog: Arc<OpLog>,
    /// The vault content key, shared (and persist-through) with the caller — the
    /// app's sync service holds the SAME handle, so an in-band auto-transfer or
    /// a manual import on either side is seen by both and written to disk once.
    /// Used for encrypt / decrypt / blind-id. [sync-vault-key-inband]
    pub(super) content_key: SharedContentKey,
    pub(super) keypair: DeviceKeypair,
    pub(super) fingerprint: DeviceFingerprint,
    pub(super) config: Settings,
    /// Enrolled peers — a clone of the SAME shared set the app's sync service
    /// holds, so an app-side enroll/unenroll is visible to this node's
    /// connection-auth gate and discovery immediately (no rebuild, no node
    /// lock). [sync-key-swap-enrollment]
    pub(super) enrolled: EnrolledPeers,
    /// Per-document sync status (`Bound` / `Blocked`), keyed by vault path —
    /// the cross-device identity. [sync-path-identity]
    pub(super) status: Mutex<HashMap<String, SyncStatus>>,
    /// Learned `fingerprint -> name` map: the human name each peer self-reported
    /// in its `Hello`/`HelloAck`. Seeded from `[sync].device_names` at construct
    /// and updated on every handshake (last name a device reports for ITSELF
    /// wins). Read by the app to persist back into config and to render a peer's
    /// name. A device never writes another device's name on its behalf.
    /// [sync-device-name]
    pub(super) device_names: Mutex<HashMap<String, String>>,
    /// Learned `fingerprint -> content_key_fp` map: the content-key fingerprint
    /// each peer self-reported in its `Hello`. Used to gate `ContentKeyRequest`
    /// — a peer whose last-reported fp matches ours has already converged on
    /// the key and must not be served the raw bytes again
    /// (bug-sync-content-key-request-no-throttle).
    /// [sync-vault-key-inband]
    pub(super) peer_content_key_fps: Mutex<HashMap<String, String>>,
    /// Persistent record of every forked (blocked) document, keyed by vault
    /// path — the surface the UI lists and resolves. Distinct from the round
    /// report's `blocked` (which is the LAST round only): an entry persists
    /// until the doc converges or the user resolves it. Hydrated from
    /// [`block_store`](Self::block_store) on construct and written through it on
    /// every record/clear, so a held conflict survives an app restart and
    /// re-surfaces instead of silently clearing.
    /// [sync-blocked-state, sync-path-identity, sync-conflict-block-persistence]
    pub(super) blocked: Mutex<HashMap<String, BlockedDoc>>,
    /// Durable backing for [`blocked`](Self::blocked): the per-vault
    /// `<vault>/.hiker/sync/blocked.json` store. `record_blocked` / `clear_blocked`
    /// write the whole set through this after mutating the in-memory map, and
    /// [`SyncNode::new`] hydrates the map from it — closing the
    /// "block lives only in memory and clears on restart" gap.
    /// [sync-conflict-block-persistence]
    pub(super) block_store: BlockStore,
    /// User resolution decisions for blocked docs, keyed by vault path. The
    /// fork branch consults this on the NEXT round: an entry makes it act
    /// (keep-mine / keep-theirs / keep-both) instead of re-blocking. Empty by
    /// default, so forks block unchanged when the user hasn't chosen.
    /// [sync-blocked-state, sync-path-identity]
    pub(super) resolutions: Mutex<HashMap<String, Resolution>>,
    /// Server-mediated store-and-forward log (unused on the LAN path; held so
    /// the same node can drive the server path in a later wave). [crate::server::BlobStore]
    pub(super) blobs: Mutex<MemBlobStore>,
    /// Per-blind-id outgoing push sequence: the next `seq` this device will
    /// stamp on an `UpdateBlob` it pushes to the hub. Monotonic per blind id so
    /// the server's append-only log orders one device's pushes; other devices'
    /// pushes interleave by their own seqs and the client-side 3-way text merge
    /// reconciles them on pull.
    pub(super) server_push_seq: Mutex<HashMap<String, u64>>,
    /// Per-blind-id pull cursor: the highest `seq` this device has already
    /// pulled + applied from the hub. The store-and-forward catch-up watermark.
    /// [sync-zero-knowledge-server]
    pub(super) server_pull_cursor: Mutex<HashMap<String, u64>>,
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
    pub(super) discovered: Mutex<HashMap<PeerId, Multiaddr>>,
    /// `PeerId`s of unenrolled peers we've already emitted a one-time "seen on
    /// LAN" log line for, so the responder loop doesn't repeat it every window.
    pub(super) seen_unenrolled_logged: Mutex<HashSet<PeerId>>,
    /// Unenrolled peers seen for the first time and not yet drained by the
    /// caller for a one-time log line. The responder loop drains this via
    /// [`take_newly_seen_unenrolled`](Self::take_newly_seen_unenrolled) after
    /// each window and emits a progress line per entry.
    pub(super) newly_seen_unenrolled: Mutex<Vec<PeerId>>,
    /// Set true whenever a `run` window folded in an enrolled peer not already
    /// in `discovered`. The caller polls + clears it via
    /// [`take_newly_discovered`](Self::take_newly_discovered) to trigger an
    /// immediate sync round on first sight of a peer.
    pub(super) newly_discovered: Mutex<bool>,
    /// Set true whenever a `run` window served a [`Message::SyncPoke`] from an
    /// enrolled peer — that peer just committed a change and wants us to pull
    /// promptly. The caller polls + clears it via [`take_poked`](Self::take_poked)
    /// to trigger an immediate sync round, the same shape as `newly_discovered`.
    /// [sync-poke-on-commit]
    pub(super) poked: Mutex<bool>,
    /// The libp2p swarm, built lazily.
    pub(super) swarm: Option<Swarm<SyncBehaviour>>,
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
        let device_names = Mutex::new(config.device_names.clone());
        // Hydrate the blocked-conflict set from its durable per-vault store, so a
        // held conflict (fork / same-region / delete-vs-edit / rename-collision)
        // recorded before the last shutdown re-surfaces on this construct rather
        // than silently clearing. [sync-conflict-block-persistence]
        // status: sync-conflict-block-persistence
        let block_store = BlockStore::for_vault(oplog.vault_root());
        let hydrated_blocks = block_store.load();
        // Seed the per-doc status map so a re-hydrated block reports
        // `SyncStatus::Blocked` from `status_of_path` immediately on restart —
        // consistent with `blocked_docs()`, without waiting for the next round.
        // [sync-conflict-block-persistence]
        let status: HashMap<String, SyncStatus> = hydrated_blocks
            .keys()
            .map(|p| (p.clone(), SyncStatus::Blocked))
            .collect();
        let blocked = Mutex::new(hydrated_blocks);
        Self {
            oplog,
            content_key,
            keypair,
            fingerprint,
            config,
            enrolled,
            status: Mutex::new(status),
            device_names,
            peer_content_key_fps: Mutex::new(HashMap::new()),
            blocked,
            block_store,
            resolutions: Mutex::new(HashMap::new()),
            blobs: Mutex::new(MemBlobStore::new()),
            server_push_seq: Mutex::new(HashMap::new()),
            server_pull_cursor: Mutex::new(HashMap::new()),
            discovered: Mutex::new(HashMap::new()),
            seen_unenrolled_logged: Mutex::new(HashSet::new()),
            newly_seen_unenrolled: Mutex::new(Vec::new()),
            newly_discovered: Mutex::new(false),
            poked: Mutex::new(false),
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

    /// Record a doc as blocked with the given `reason` and peer, the test seam
    /// for the restart-with-stale-block path: it models a persisted block that
    /// re-hydrates on construct (status forced `Blocked`) even though the
    /// conflict has since converged out-of-band. Mirrors the production
    /// [`record_blocked`](Self::record_blocked) the detecting round calls, plus
    /// the status seed `SyncNode::new` does for a hydrated block, so the next
    /// round routes to the doc's blocked re-eval branch exactly as after a real
    /// restart.
    pub fn record_blocked_for_test(&self, path: &str, reason: &str, peer: &DeviceFingerprint) {
        self.record_blocked(path, reason, peer);
        self.status
            .lock()
            .unwrap()
            .insert(path.to_string(), SyncStatus::Blocked);
    }

    /// Reset the per-blind-id server pull cursors so the next
    /// [`sync_via_server`](Self::sync_via_server) re-fetches every blob from seq
    /// 0 — the test seam for the idempotent re-pull case (a client that pulls
    /// the same store-and-forward blobs again must not double content, since
    /// `apply_remote_update` re-merging the peer's identical text is a no-op).
    pub fn reset_server_cursor_for_test(&self) {
        self.server_pull_cursor.lock().unwrap().clear();
    }

    /// This node's `[sync]` configuration (mode, discovery toggle, enrolled
    /// device list as loaded from the vault config).
    pub const fn config(&self) -> &Settings {
        &self.config
    }

    /// A snapshot of the learned `fingerprint -> name` map — the names enrolled
    /// peers have self-reported in their handshakes. The app reads this to
    /// render a peer's synced name and to persist it back into
    /// `[sync].device_names`. [sync-device-name]
    pub fn learned_device_names(&self) -> HashMap<String, String> {
        self.device_names.lock().unwrap().clone()
    }

    /// Record a peer's SELF-reported name into the learned map, keyed by the
    /// peer's fingerprint. Called on every handshake; the last name a device
    /// reports for itself wins. An empty/`None` name is ignored (an unnamed peer
    /// doesn't clobber a previously-learned name). [sync-device-name]
    pub(super) fn record_device_name(&self, fingerprint: &DeviceFingerprint, name: Option<&str>) {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return;
        };
        self.device_names
            .lock()
            .unwrap()
            .insert(fingerprint.0.clone(), name.to_string());
    }

    /// Record a peer's self-reported `content_key_fp` from its `Hello`, keyed
    /// by the peer's fingerprint. Consulted on `ContentKeyRequest` to refuse
    /// re-serving the raw key to a peer that has already converged on it
    /// (bug-sync-content-key-request-no-throttle). [sync-vault-key-inband]
    pub(super) fn record_peer_content_key_fp(
        &self,
        fingerprint: &DeviceFingerprint,
        content_key_fp: &str,
    ) {
        self.peer_content_key_fps
            .lock()
            .unwrap()
            .insert(fingerprint.0.clone(), content_key_fp.to_string());
    }

    /// The peer's last-reported `content_key_fp`, if it has sent a `Hello` on
    /// this node's lifetime. [sync-vault-key-inband]
    pub(super) fn peer_content_key_fp(&self, fingerprint: &DeviceFingerprint) -> Option<String> {
        self.peer_content_key_fps
            .lock()
            .unwrap()
            .get(&fingerprint.0)
            .cloned()
    }

    /// Build THIS device's `Hello`, carrying its configured self-set name so the
    /// peer can adopt it into its learned map. [sync-device-name]
    pub(super) fn build_hello(&self) -> Message {
        Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_fingerprint: self.fingerprint.0.clone(),
            content_key_fp: self.content_key.fingerprint(),
            device_name: self.config.device_name.clone(),
        }
    }

    /// Buffer a content-encrypted update for the store-and-forward server path:
    /// encrypt `update` under the content key and append it to the local
    /// [`MemBlobStore`] under `path`'s blind id (`HMAC(content_key, path)`).
    /// The server-mediated transport flushes this log to the hub; on the LAN
    /// path direct peer streaming is used instead.
    /// [sync-zero-knowledge-server, sync-blind-id]
    pub fn buffer_update(&self, path: &str, seq: u64, update: &[u8]) {
        let key = self.content_key.get();
        let blind = crypto::blind_id(&key, path);
        let ciphertext = key.encrypt(update);
        self.blobs.lock().unwrap().push(&blind, seq, ciphertext);
    }

    /// Pull buffered encrypted updates for `path` past `after_seq` and decrypt
    /// them — the receiving half of the store-and-forward path. Returns
    /// `(seq, plaintext_update)` ascending; a tampered/foreign-key blob fails
    /// with [`Error::Decrypt`]. [sync-zero-knowledge-server, sync-blind-id]
    pub fn drain_buffered(
        &self,
        path: &str,
        after_seq: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, Error> {
        let key = self.content_key.get();
        let blind = crypto::blind_id(&key, path);
        let blobs = self.blobs.lock().unwrap();
        blobs
            .pull(&blind, after_seq)
            .into_iter()
            .map(|(seq, ct)| key.decrypt(&ct).map(|pt| (seq, pt)))
            .collect()
    }

    /// The current sync status of a document at `path`, if tracked.
    pub fn status_of_path(&self, path: &str) -> Option<SyncStatus> {
        self.status.lock().unwrap().get(path).copied()
    }

    /// A snapshot of every document currently blocked by a fork — the surface
    /// the Sync page lists and resolves. Persistent across rounds (unlike the
    /// round report's `blocked`), so a doc that forked two rounds ago is still
    /// here until it converges or the user resolves it. [sync-blocked-state]
    pub fn blocked_docs(&self) -> Vec<BlockedDoc> {
        self.blocked.lock().unwrap().values().cloned().collect()
    }

    /// Record the user's resolution decision for a forked document at `path`.
    /// Consumed by the fork branch on the NEXT round: instead of re-blocking it
    /// adopts the peer (keep-theirs / keep-both) or offers our lineage
    /// (keep-mine). No decision (the default) leaves the fork blocked.
    /// [sync-blocked-state, sync-path-identity]
    pub fn set_fork_resolution(&self, path: String, resolution: Resolution) {
        self.resolutions.lock().unwrap().insert(path, resolution);
    }

    /// Record a doc as persistently blocked with the given `reason`
    /// (`"fork"` for a disjoint-lineage fork, `"same-region"` for a bound-doc
    /// overlapping concurrent edit, `"delete-vs-edit"` for a delete concurrent
    /// with an edit, `"rename-collision"` for two devices renaming different
    /// docs onto the same path). Idempotent on the path key. The block is
    /// written through the durable [`block_store`](Self::block_store) so it
    /// survives a restart and re-surfaces. [sync-blocked-state,
    /// sync-conflict-block-and-resolve, sync-conflict-block-persistence]
    pub(super) fn record_blocked(&self, path: &str, reason: &str, peer: &DeviceFingerprint) {
        let mut blocked = self.blocked.lock().unwrap();
        blocked.insert(
            path.to_string(),
            BlockedDoc {
                path: path.to_string(),
                reason: reason.to_string(),
                peer_fingerprint: peer.clone(),
            },
        );
        self.persist_blocked(&blocked);
    }

    /// Write the current blocked set through the durable per-vault store. Called
    /// after every `record_blocked` / `clear_blocked` mutation while the
    /// `blocked` lock is held, so the on-disk file and the in-memory map never
    /// diverge. A persist I/O failure is logged, not fatal — the block is still
    /// held in memory this session and re-recorded next round if it persists.
    /// [sync-conflict-block-persistence]
    fn persist_blocked(&self, blocked: &HashMap<String, BlockedDoc>) {
        if let Err(e) = self.block_store.save(blocked) {
            tracing::warn!(error = %e, "sync: failed to persist blocked-conflict set");
        }
    }

    /// The reason a doc at `path` is currently blocked (`"fork"` /
    /// `"same-region"` / `"delete-vs-edit"` / `"rename-collision"`), if it is
    /// blocked. Lets the dialer route a blocked doc to the right resolution
    /// branch on a later round.
    pub(super) fn blocked_reason(&self, path: &str) -> Option<String> {
        self.blocked
            .lock()
            .unwrap()
            .get(path)
            .map(|b| b.reason.clone())
    }

    /// Clear a blocked record and its resolution decision — called when the doc
    /// converges or its fork is resolved. Removes the durable entry too, so a
    /// resolved/converged block does NOT resurrect on the next restart.
    /// [sync-conflict-block-persistence]
    pub(super) fn clear_blocked(&self, path: &str) {
        let mut blocked = self.blocked.lock().unwrap();
        let removed = blocked.remove(path).is_some();
        if removed {
            self.persist_blocked(&blocked);
        }
        drop(blocked);
        self.resolutions.lock().unwrap().remove(path);
    }

    /// The fingerprint of the enrolled peer for a connection, falling back to
    /// the raw peer id string when the mapping is missing (shouldn't happen on
    /// an enrolled connection, but keeps the record honest either way).
    pub(super) fn peer_fingerprint(&self, peer_id: &PeerId) -> DeviceFingerprint {
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

    /// Whether an enrolled peer poked us (committed a change and asked us to
    /// pull) since the last call, clearing the flag. The sync driver polls this
    /// after each responder window — alongside `take_newly_discovered` — to run
    /// a prompt pull round rather than waiting for the next periodic tick.
    /// [sync-poke-on-commit]
    pub fn take_poked(&self) -> bool {
        std::mem::replace(&mut self.poked.lock().unwrap(), false)
    }

    /// Record an inbound poke (set the `poked` flag). Called by the responder
    /// dispatch when an enrolled peer sends [`Message::SyncPoke`].
    /// [sync-poke-on-commit]
    pub(super) fn record_poked(&self) {
        *self.poked.lock().unwrap() = true;
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
    pub(super) fn record_discovered(&self, peers: impl IntoIterator<Item = (PeerId, Multiaddr)>) {
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
    pub(super) fn record_expired(&self, peers: impl IntoIterator<Item = (PeerId, Multiaddr)>) {
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
    pub(super) fn ensure_swarm(&mut self) -> Result<(), Error> {
        if self.swarm.is_some() {
            return Ok(());
        }
        let keypair = self.keypair.libp2p_keypair().clone();
        self.swarm = Some(build_swarm(keypair)?);
        Ok(())
    }

    pub(super) const fn swarm_mut(&mut self) -> &mut Swarm<SyncBehaviour> {
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
    pub(super) fn is_enrolled(&self, peer: &PeerId) -> bool {
        self.enrolled.contains(peer)
    }

    /// Flip the doc at `path` to `Bound` status. The path-as-identity model
    /// has no separate binding table — being able to resolve `path →
    /// doc_id_for_path` IS the binding. [sync-path-identity]
    pub(super) fn mark_bound(&self, path: &str) {
        self.status
            .lock()
            .unwrap()
            .insert(path.to_string(), SyncStatus::Bound);
    }

    /// Drop a stale block when a doc auto-merges cleanly again — e.g. the
    /// conflict was resolved on the OTHER device and the content has since
    /// converged, so this side's block (which the user never cleared here) must
    /// stop surfacing. Idempotent and cheap when the doc wasn't blocked:
    /// `clear_blocked` only persists if it actually removed an entry.
    /// status: sync-conflict-block-and-resolve
    pub(super) fn clear_stale_block(&self, path: &str) {
        let was_blocked = self.blocked.lock().unwrap().contains_key(path);
        if was_blocked {
            tracing::info!(path = %path, "sync: conflict converged out-of-band — clearing stale block");
        }
        self.mark_bound(path);
        self.clear_blocked(path);
    }

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

    /// `record_device_name` adopts a peer's self-reported name into the learned
    /// map, last-write-wins; an empty/whitespace name is ignored so an unnamed
    /// peer never clobbers a previously-learned name. The map seeds from
    /// `[sync].device_names` at construct. [sync-device-name]
    #[test]
    fn learned_device_names_records_and_seeds() {
        // Seeds from config.
        let cfg = Settings {
            device_names: std::collections::HashMap::from([("DEV-SEED".into(), "seeded".into())]),
            ..Settings::default()
        };
        let node = mk_node(cfg);
        assert_eq!(
            node.learned_device_names().get("DEV-SEED").map(String::as_str),
            Some("seeded")
        );

        let fp = DeviceFingerprint("DEV-PEER".into());
        // First report adopts the name.
        node.record_device_name(&fp, Some("phone"));
        assert_eq!(
            node.learned_device_names().get("DEV-PEER").map(String::as_str),
            Some("phone")
        );
        // Last-write-from-that-device wins (the device renamed itself).
        node.record_device_name(&fp, Some("phone-2"));
        assert_eq!(
            node.learned_device_names().get("DEV-PEER").map(String::as_str),
            Some("phone-2")
        );
        // An empty / whitespace / None report is ignored, not a clobber.
        node.record_device_name(&fp, Some("   "));
        node.record_device_name(&fp, None);
        assert_eq!(
            node.learned_device_names().get("DEV-PEER").map(String::as_str),
            Some("phone-2"),
            "empty/None name does not clobber the learned name"
        );
    }

    /// `build_hello` carries THIS device's configured self-set name. [sync-device-name]
    #[test]
    fn build_hello_carries_configured_device_name() {
        let cfg = Settings {
            device_name: Some("my-desktop".into()),
            ..Settings::default()
        };
        let node = mk_node(cfg);
        match node.build_hello() {
            Message::Hello { device_name, .. } => {
                assert_eq!(device_name.as_deref(), Some("my-desktop"));
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    /// Regression test for `bug-sync-history-hashset-truncation-nondet`:
    ///
    /// `build_manifest` builds each entry's `recent_history_hashes` by calling
    /// `oplog.doc_history_hashes(..)` (returns a `HashSet<String>`) and then
    /// `.into_iter().take(RECENT_HISTORY_WINDOW)`. `HashSet` iteration order is
    /// unspecified, so when a doc's history has more than 32 unique content
    /// hashes, `.take(32)` selects an arbitrary subset rather than the
    /// most-recent 32 by timestamp. Two devices that ought to classify as a
    /// fast-forward then can intermittently classify as a Fork.
    ///
    /// This test creates a doc and applies 64 distinct content states, then
    /// asserts that the manifest's `recent_history_hashes` equals the set of
    /// the most-recent 32 content hashes by timestamp DESC. With the current
    /// HashSet-based code this is virtually certain to fail.
    #[test]
    fn bug_sync_history_hashset_truncation_nondet() {
        use hiker_core::oplog::meta::{Filter, OpStatus};
        use hiker_core::oplog::shapes::Author;

        let node = mk_node(Settings::default());

        let path = "notes/long-history.md";
        let doc_id = node
            .oplog
            .create_document(path, "note", "v0\n", &Author::User)
            .unwrap();

        // Apply 64 distinct content states. Each user-save advances accepted
        // and writes a fresh content_hash row → 64 additional unique hashes on
        // top of the initial seed (the seed contributes one more).
        const TOTAL_EDITS: usize = 64;
        for i in 1..=TOTAL_EDITS {
            let text = format!("v{i}\n");
            let changed = node.oplog.apply_user_text(&doc_id, &text).unwrap();
            assert!(changed, "edit {i} should advance accepted");
        }

        // Compute the expected most-recent-32 content hashes by timestamp DESC,
        // straight from the side table via the canonical accepted-row query.
        let accepted_rows = node
            .oplog
            .query_metadata(&Filter {
                doc_id: Some(doc_id.clone()),
                status: Some(OpStatus::Accepted),
                ..Filter::default()
            })
            .unwrap();
        let mut expected_recent: Vec<String> = accepted_rows
            .into_iter()
            .filter_map(|op| op.content_hash)
            .collect();
        // query_metadata is already ORDER BY timestamp_ms DESC, rowid DESC.
        expected_recent.truncate(RECENT_HISTORY_WINDOW);
        let expected_set: std::collections::HashSet<String> =
            expected_recent.iter().cloned().collect();
        assert_eq!(
            expected_set.len(),
            RECENT_HISTORY_WINDOW,
            "sanity: expected the doc to have at least {RECENT_HISTORY_WINDOW} unique recent hashes"
        );

        // Drive the actual manifest-builder code path the bug lives in.
        let manifest = node.build_manifest().expect("build_manifest");
        let entry = manifest
            .entries
            .iter()
            .find(|e| e.path == path)
            .expect("manifest entry for our doc");

        assert_eq!(
            entry.recent_history_hashes.len(),
            RECENT_HISTORY_WINDOW,
            "manifest carries exactly RECENT_HISTORY_WINDOW recent hashes"
        );

        let got_set: std::collections::HashSet<String> =
            entry.recent_history_hashes.iter().cloned().collect();
        assert_eq!(
            got_set, expected_set,
            "manifest.recent_history_hashes must be the most-recent {RECENT_HISTORY_WINDOW} \
             content hashes by timestamp DESC, but HashSet-then-take selected an arbitrary subset \
             (bug-sync-history-hashset-truncation-nondet)"
        );
    }

    /// Regression test for `bug-sync-content-key-request-no-throttle`:
    ///
    /// The responder unconditionally answers `ContentKeyRequest` with the raw
    /// 32-byte vault content key for any enrolled peer — even one whose
    /// preceding `Hello` reported the SAME `content_key_fp` as ours, i.e. it
    /// has already converged on the key and has no legitimate reason to ask
    /// for it. The dialer-side "only request when fingerprints differ" is a
    /// policy, not enforcement; a buggy or malicious enrolled peer can refresh
    /// the key on demand by simply asking. The responder should refuse.
    #[test]
    fn bug_sync_content_key_request_no_throttle() {
        let node = mk_node(Settings::default());
        let (peer_id, fp) = enrolled_peer(&node);

        // Drive a Hello from the peer with content_key_fp matching ours — the
        // peer claims it has already converged on the same key.
        let our_fp = node.content_key.fingerprint();
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_fingerprint: fp.0.clone(),
            content_key_fp: our_fp.clone(),
            device_name: Some("converged-peer".into()),
        };
        let _ = node
            .handle_request(&peer_id, hello)
            .expect("Hello handled");

        // Now the same peer requests the content key. Since its Hello reported
        // the same content_key_fp, it must NOT be served the raw key.
        let response = node.handle_request(&peer_id, Message::ContentKeyRequest);

        assert!(
            !matches!(response, Ok(Message::ContentKeyResponse { .. })),
            "BUG: responder served content key to a peer that already has it \
             (Hello reported matching content_key_fp), got {response:?}"
        );
    }

    /// Build a node over an explicit (caller-owned) vault dir, so the dir can
    /// be reused across a drop+reconstruct ("restart") in the persistence test.
    fn mk_node_at(vault_root: &std::path::Path) -> SyncNode {
        let oplog = Arc::new(OpLog::open(vault_root).unwrap());
        SyncNode::new(
            oplog,
            SharedContentKey::new(ContentKey::generate()),
            DeviceKeypair::generate(),
            Settings::default(),
            EnrolledPeers::new(),
        )
    }

    /// The core durability guarantee for `sync-conflict-block-persistence`: a
    /// recorded block (path + reason + peer fingerprint) survives an app restart
    /// — modeled by DROPPING the `SyncNode` and reconstructing a fresh one over
    /// the SAME vault dir — and re-surfaces via `blocked_docs()` /
    /// `status_of_path()`. Resolving (clearing) a block removes its persisted
    /// entry, so it does NOT resurrect on the next restart.
    #[test]
    fn blocked_conflict_survives_restart_and_clears_on_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path();
        let peer = DeviceFingerprint("DEV-PEER".into());

        // First session: record a same-region block.
        {
            let node = mk_node_at(vault);
            assert!(node.blocked_docs().is_empty(), "fresh vault has no blocks");
            node.record_blocked("notes/a.md", "same-region", &peer);
            node.mark_bound("notes/a.md"); // a no-op for status here; just exercising
            assert_eq!(node.blocked_docs().len(), 1);
        } // node dropped — the only durable record is now on disk.

        // Restart: a brand-new node over the same vault dir re-hydrates the block.
        {
            let node = mk_node_at(vault);
            let blocks = node.blocked_docs();
            assert_eq!(blocks.len(), 1, "block re-surfaced after restart");
            assert_eq!(blocks[0].path, "notes/a.md");
            assert_eq!(blocks[0].reason, "same-region", "reason persisted");
            assert_eq!(blocks[0].peer_fingerprint, peer, "peer fingerprint persisted");
            assert_eq!(
                node.status_of_path("notes/a.md"),
                Some(SyncStatus::Blocked),
                "hydrated block reports Blocked status without a round"
            );
            assert_eq!(
                node.blocked_reason("notes/a.md").as_deref(),
                Some("same-region"),
                "blocked_reason routes the restart-hydrated block"
            );

            // Resolve (converge / user-resolve) → clears the persisted entry.
            node.clear_blocked("notes/a.md");
            assert!(node.blocked_docs().is_empty(), "cleared in this session");
        } // dropped again.

        // Next restart: the cleared block must NOT resurrect.
        {
            let node = mk_node_at(vault);
            assert!(
                node.blocked_docs().is_empty(),
                "resolved block does not resurrect on the next restart"
            );
        }
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
