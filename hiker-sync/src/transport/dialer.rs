//! Outbound side of the sync session — the dialer's per-round state machine:
//! connect, Hello handshake, content-key convergence, manifest walk + per-entry
//! classification, and the low-level single-in-flight request/response driver.
//!
//! Every method is a `&mut self` `impl SyncNode` continuation (the dialer's
//! state machine must drive the swarm) and the file defines no items of its
//! own. The inbound side lives in [`super::responder`]; the lineage adoption
//! verbs the dispatch calls out to live in [`super::lineage`].

use std::collections::HashSet;

use futures::StreamExt;
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId};

use crate::crypto::ContentKey;
use crate::enroll::{self, Classification};
use crate::identity::SyncStatus;
use crate::protocol::{ManifestEntry, Message};
use crate::Error;

use super::{parse_addr, SyncBehaviourEvent, SyncNode, SyncReport};

impl SyncNode {
    /// Dial `peer`, run the full Hello + Manifest + classify + adopt/stream
    /// flow, and return a [`SyncReport`]. The peer must be running `run` (or
    /// otherwise driving its swarm) to answer. The active node here is the
    /// "puller": it converges its own replicas toward the peer.
    pub async fn sync_once(&mut self, peer: &str) -> Result<SyncReport, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(peer)?;
        let peer_id = self.connect(addr).await?;

        // 1. Hello handshake — exchange device + content-key fingerprints, plus
        // each side's self-set device name. Record the peer's reported name into
        // the learned map. [sync-device-name]
        let hello = self.build_hello();
        let peer_content_key_fp = match self.request(peer_id, hello).await? {
            Message::HelloAck {
                content_key_fp,
                device_name,
                ..
            } => {
                let peer_fp = self.peer_fingerprint(&peer_id);
                self.record_device_name(&peer_fp, device_name.as_deref());
                self.record_peer_content_key_fp(&peer_fp, &content_key_fp);
                content_key_fp
            }
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
    /// running `run` (or otherwise driving its swarm) to answer.
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
        let hello = self.build_hello();
        match self.request(peer_id, hello).await? {
            Message::HelloAck { device_name, .. } => {
                self.record_device_name(&self.peer_fingerprint(&peer_id), device_name.as_deref());
            }
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
    pub(super) async fn converge_content_key(
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

    /// Process one remote manifest entry: resolve by path (path IS the
    /// cross-device identity), then adopt or stream.
    /// [sync-path-identity]
    pub(super) async fn sync_entry(
        &mut self,
        peer_id: PeerId,
        entry: ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        // Resolve our local replica by vault path — the cross-device identity.
        // A peer-side rename rides in as a `meta.path` op on the shared lineage
        // in the delta stream; once both sides are on the shared lineage and
        // streaming, a rename moves both replicas' `meta.path` and the next
        // manifest from each side will list the doc at its new path. There is
        // no separate binding table to consult.
        let local = self.local_doc_for_path(&entry.path)?;

        match local {
            // We have no local replica of this path: either it's genuinely new
            // to us, OR the peer renamed an existing doc we still hold under
            // the old path. Distinguish via content-hash overlap: if any of our
            // bound docs has a `current_hash` that appears in this entry's
            // recent history (i.e. we share lineage with this doc), pull a
            // delta against the new path so the peer-side `Rename { from }` op
            // rides in — `apply_remote_update` then repoints our path mapping.
            // Otherwise this is a fresh adoption. [sync-path-identity,
            // sync-rename-blob-rotation]
            None => {
                if let Some(prior) = self.find_rename_source(&entry)? {
                    self.apply_delta_from_peer(peer_id, &prior, &entry.path).await?;
                    self.mark_bound(&entry.path);
                    report.bound.push(entry.path.clone());
                    report.converged.push(entry.path);
                } else {
                    let local = self.create_local_for(&entry.path)?;
                    self.adopt_from_peer(peer_id, &local, &entry.path).await?;
                    self.mark_bound(&entry.path);
                    report.bound.push(entry.path.clone());
                    report.converged.push(entry.path);
                }
            }
            // We already have a doc at this path. If it's marked Bound from a
            // prior round, it's on the shared lineage — pull the delta. If
            // it's marked Blocked from a prior fork, the user must resolve.
            // Otherwise this is first contact and we classify against the
            // content-hash history.
            // [sync-blocked-state, sync-stream-muxing, sync-enrollment-hash-classification]
            Some(local) => {
                match self.status_of_path(&entry.path) {
                    Some(SyncStatus::Bound) => {
                        self.apply_delta_from_peer(peer_id, &local, &entry.path).await?;
                        report.bound.push(entry.path.clone());
                        report.converged.push(entry.path);
                    }
                    Some(SyncStatus::Blocked) => {
                        // A doc that forked on a prior round runs its
                        // resolution branch again — without this it would block
                        // every round and the user's decision (keyed by path)
                        // would never land. [sync-blocked-state]
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
                        self.act_on_classification(peer_id, &local, &entry, class, report)
                            .await?;
                        // (path-keyed — no logical id rides the wire)
                    }
                    None => {
                        // First contact for a doc we hold locally: classify
                        // against the peer's content-hash history before any
                        // merge. [sync-enrollment-hash-classification]
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
                            let our_text = self
                                .oplog
                                .materialize_accepted(&local.0)
                                .map(|m| m.text)
                                .unwrap_or_default();
                            let head: String = our_text
                                .chars()
                                .take(48)
                                .collect::<String>()
                                .escape_default()
                                .to_string();
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
                        self.act_on_classification(peer_id, &local, &entry, class, report)
                            .await?;
                        // (path-keyed — no logical id rides the wire)
                    }
                }
            }
        }
        Ok(())
    }

    /// Drive the swarm until an outbound connection to `addr` establishes, then
    /// verify the peer is enrolled. Returns the authenticated `PeerId`.
    /// [sync-noise-channel]
    pub(super) async fn connect(&mut self, addr: Multiaddr) -> Result<PeerId, Error> {
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
    pub(super) async fn request(&mut self, peer_id: PeerId, msg: Message) -> Result<Message, Error> {
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
