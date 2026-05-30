//! Inbound side of the sync session — the `run` event loop that answers
//! request-response messages from a dialer, manifest construction, and the
//! per-doc resolution helpers that feed the responder dispatch.
//!
//! Every method here is a `&self` (read-mostly) `impl SyncNode` continuation
//! plus the `&mut self` swarm-drive entry points; nothing in this file defines
//! its own items (per the impl-split exemption in
//! `scripts/check-splits.py`). The dialer side lives in [`super::dialer`].

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use libp2p::request_response;
use libp2p::swarm::SwarmEvent;
use libp2p::{mdns, PeerId};

use hiker_core::oplog::meta::{Filter, OpStatus};

use crate::crypto;
use crate::identity::{LocalDocId, SyncStatus};
use crate::server::BlobStore;
use crate::protocol::{Manifest, ManifestEntry, Message};
use crate::Error;

use super::{SyncBehaviourEvent, SyncNode, RECENT_HISTORY_WINDOW};

impl SyncNode {
    /// Build this vault's manifest: one [`ManifestEntry`] per document, with its
    /// current content hash and a bounded recent history-hash window. Path is
    /// the cross-device identity, so each entry is keyed by path alone — no
    /// logical id rides the wire. [sync-path-identity]
    pub(super) fn build_manifest(&self) -> Result<Manifest, Error> {
        let doc_ids = self
            .oplog
            .list_doc_ids()
            .map_err(|e| Error::Transport(format!("list docs: {e}")))?;
        let mut entries = Vec::with_capacity(doc_ids.len());
        for doc_id in doc_ids {
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
                .recent_doc_history_hashes(&doc_id, RECENT_HISTORY_WINDOW)
                .map_err(|e| Error::Transport(format!("history {doc_id}: {e}")))?;
            // Collect prior paths from the doc's accepted `Rename { from }` ops
            // so a peer can follow a sequential rename: if its local replica is
            // still at one of these prior paths, it pulls the delta against the
            // new path and the rename op rides in via `apply_remote_update`,
            // which repoints `doc-index.db`. [sync-path-identity]
            let prior_paths: Vec<String> = self
                .oplog
                .query_metadata(&Filter {
                    doc_id: Some(doc_id.clone()),
                    status: Some(OpStatus::Accepted),
                    ..Filter::default()
                })
                .map_err(|e| Error::Transport(format!("query renames {doc_id}: {e}")))?
                .into_iter()
                .filter_map(|op| op.rename_from)
                .collect();
            entries.push(ManifestEntry {
                path,
                current_hash,
                recent_history_hashes: history,
                prior_paths,
            });
        }
        Ok(Manifest { entries })
    }

    /// The local content hash for a doc id (blake3 of `materialize(accepted)`).
    pub(super) fn current_hash(&self, doc_id: &str) -> Result<String, Error> {
        let text = self
            .oplog
            .materialize_accepted(doc_id)
            .map_err(|e| Error::Transport(format!("materialize {doc_id}: {e}")))?
            .text;
        Ok(blake3::hash(text.as_bytes()).to_hex().to_string())
    }

    pub(super) fn history_set(&self, doc_id: &str) -> Result<HashSet<String>, Error> {
        self.oplog
            .doc_history_hashes(doc_id)
            .map_err(|e| Error::Transport(format!("history {doc_id}: {e}")))
    }

    /// Compute the reply to one inbound [`Message`] request from `peer`. The
    /// responder is stateless across requests beyond the OpLog: every request
    /// names the document by vault path. [sync-path-identity]
    pub(super) fn handle_request(&self, peer: &PeerId, req: Message) -> Result<Message, Error> {
        match req {
            // Record the dialer's self-reported name (keyed by its enrolled
            // fingerprint, not the raw peer id), then reply with our own name so
            // the dialer learns ours on the same round trip. [sync-device-name]
            Message::Hello { device_name, content_key_fp, .. } => {
                let peer_fp = self.peer_fingerprint(peer);
                self.record_device_name(&peer_fp, device_name.as_deref());
                self.record_peer_content_key_fp(&peer_fp, &content_key_fp);
                Ok(Message::HelloAck {
                    device_fingerprint: self.fingerprint.0.clone(),
                    content_key_fp: self.content_key.fingerprint(),
                    device_name: self.config.device_name.clone(),
                })
            }
            // Serve the content key to an enrolled peer over the (already
            // Noise-encrypted) channel — the canonical-device half of the
            // in-band transfer. `peer` is already enrollment-gated by the
            // responder. The raw bytes are NEVER logged.
            //
            // Refuse if the peer's preceding `Hello` reported the SAME
            // `content_key_fp` as ours — they have already converged on the key
            // and have no legitimate need to fetch it again. A buggy or
            // malicious enrolled peer could otherwise refresh the raw key on
            // demand. Unknown fingerprint (no Hello recorded) is permitted: the
            // legitimate dialer flow always Hellos first, but a tighter
            // gate-on-unknown would break any future shape where a probe
            // precedes a Hello. (bug-sync-content-key-request-no-throttle)
            // [sync-vault-key-inband]
            Message::ContentKeyRequest => {
                let peer_fp = self.peer_fingerprint(peer);
                let our_fp = self.content_key.fingerprint();
                if self.peer_content_key_fp(&peer_fp).as_deref() == Some(our_fp.as_str()) {
                    return Err(Error::Transport(
                        "content key already converged — refusing to re-serve".into(),
                    ));
                }
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
            Message::StateRequest { path } => {
                // The peer wants our canonical base for the doc at `path`.
                // Resolve directly via `doc_id_for_path` — path IS the identity.
                let Some(local) = self.local_doc_for_path(&path)? else {
                    return Err(Error::Transport(format!(
                        "state requested for unknown path {path}"
                    )));
                };
                let state = self
                    .oplog
                    .export_state(&local.0)
                    .map_err(|e| Error::Transport(format!("export_state: {e}")))?;
                // Serving a StateRequest means the peer is about to adopt our
                // base → the lineage is now shared from our side as well, so
                // future rounds with this peer can use the delta path safely.
                // [sync-path-identity, sync-lineage-adoption]
                self.mark_bound(&path);
                Ok(Message::LineageBase { path, state })
            }
            Message::DeltaRequest { path, state_vector } => {
                let Some(local) = self.local_doc_for_path(&path)? else {
                    return Err(Error::Transport(format!(
                        "delta requested for unknown path {path}"
                    )));
                };
                let delta = self
                    .oplog
                    .export_since(&local.0, &state_vector)
                    .map_err(|e| Error::Transport(format!("export_since: {e}")))?;
                let key = self.content_key.get();
                let ciphertext = key.encrypt(&delta);
                let blind_id = crypto::blind_id(&key, &path);
                // A DeltaRequest implies the peer is already on a shared
                // lineage with us; mirror that fact in our own status so we
                // don't re-classify this path on a subsequent round.
                self.mark_bound(&path);
                Ok(Message::UpdateBlob {
                    blind_id,
                    seq: 0,
                    ciphertext,
                })
            }
            // The pusher's "keep mine" converge: it has made ITS version
            // canonical and pushed its exact Yrs base. We (the peer) adopt that
            // base — replacing our diverged doc, discarding our local branch
            // ("keep mine" means the pusher's version wins) — then clear any
            // block / pending resolution we had for the doc at `path`. Clearing
            // OUR resolution is what prevents a flap: if we also had a
            // keep-mine queued, we no longer push back next round. Resolve the
            // local doc by `path`; if we have none there yet, create one to
            // hold the adopted lineage. Because we adopt the pusher's EXACT
            // base, both sides now share its lineage → later deltas are safe
            // (no interleave). `peer` is already enrollment-gated. The pushed
            // `state` rides the Noise channel and is never logged.
            // [sync-blocked-state, sync-lineage-adoption]
            Message::PushAdopt { path, state } => {
                let _ = peer; // enrollment already gated the connection.
                let local = match self.local_doc_for_path(&path)? {
                    Some(existing) => existing,
                    None => self.create_local_for(&path)?,
                };
                let device_id = self.peer_fingerprint(peer).0;
                self.oplog
                    .adopt_lineage_theirs(&local.0, &state, &device_id)
                    .map_err(|e| Error::Transport(format!("adopt_lineage_theirs: {e}")))?;
                self.mark_bound(&path);
                self.clear_blocked(&path);
                Ok(Message::PushAdoptAck { path })
            }
            // Server-side blob GC on rename: an enrolled peer asks us (acting
            // as the hub) to drop all blobs at a blind_id whose stream has
            // rotated. The connection is already enrollment-gated. Idempotent —
            // an unknown blind_id is a successful no-op (the caller's view of
            // "GC the old stream" is satisfied either way).
            // [sync-rename-blob-rotation]
            Message::DeleteBlob { blind_id } => {
                let _ = peer; // enrollment already gated the connection.
                self.blobs.lock().unwrap().delete(&blind_id);
                Ok(Message::DeleteBlobAck { blind_id })
            }
            other => Err(Error::Transport(format!(
                "unexpected request on responder: {other:?}"
            ))),
        }
    }

    /// Resolve a vault-relative path to a local doc id.
    pub(super) fn local_doc_for_path(&self, path: &str) -> Result<Option<LocalDocId>, Error> {
        Ok(self
            .oplog
            .doc_id_for_path(path)
            .map_err(|e| Error::Transport(format!("doc_id_for_path: {e}")))?
            .map(LocalDocId))
    }

    /// Try to identify a local doc that is the rename source for a manifest
    /// entry at a path we don't yet have. Returns the matching local doc when
    /// its `current_hash` appears in the peer's recent history (proving they
    /// share lineage), OR our doc's recent history overlaps the peer's
    /// (covering the case where the peer is the renamer and we're slightly
    /// behind on its body). Iterating `list_doc_ids` is O(#docs) — fine for the
    /// path manifest sizes the protocol targets. [sync-path-identity]
    pub(super) fn find_rename_source(
        &self,
        entry: &ManifestEntry,
    ) -> Result<Option<LocalDocId>, Error> {
        // First, the authoritative signal: the peer's manifest entry carries the
        // doc's prior paths (every `Rename { from }` op in its history). If our
        // local replica still lives at one of those, we KNOW it's the same
        // document — no heuristic needed.
        for prior in &entry.prior_paths {
            if let Some(local) = self.local_doc_for_path(prior)? {
                return Ok(Some(local));
            }
        }
        // Fallback: content-hash overlap probe for older peers that don't send
        // `prior_paths` (backward-compat).
        let theirs_history: HashSet<&str> = entry
            .recent_history_hashes
            .iter()
            .map(String::as_str)
            .collect();
        let doc_ids = self
            .oplog
            .list_doc_ids()
            .map_err(|e| Error::Transport(format!("list docs: {e}")))?;
        for id in doc_ids {
            // Only consider docs that are currently bound and at SOME path
            // (skip mid-creation unmaterialized rows).
            let Some(path) = self
                .oplog
                .path_for_doc(&id)
                .map_err(|e| Error::Transport(format!("path_for_doc: {e}")))?
            else {
                continue;
            };
            if path == entry.path {
                continue;
            }
            if self.status_of_path(&path) != Some(SyncStatus::Bound) {
                continue;
            }
            let ours_current = self.current_hash(&id)?;
            let ours_history = self.history_set(&id)?;
            // We share lineage if either side's current is in the other's
            // history (the same overlap classify uses to detect fast-forwards).
            if theirs_history.contains(ours_current.as_str()) {
                return Ok(Some(LocalDocId(id)));
            }
            if ours_history.contains(&entry.current_hash) {
                return Ok(Some(LocalDocId(id)));
            }
        }
        Ok(None)
    }

    /// Drive the swarm event loop as a responder, answering inbound requests
    /// from enrolled peers until `window` elapses. Used by a listening node
    /// while a peer drives `sync_once`; also the basis for always-on serving.
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
}
