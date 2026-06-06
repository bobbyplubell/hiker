//! First-contact lineage adoption verbs: bringing two independently-seeded
//! Yrs lineages onto one shared base before any deltas flow. Two
//! independently-seeded Yrs Docs at the same path do not auto-merge — identical
//! bytes interleave because the lineages share no history — so at first
//! contact the receiving device adopts the canonical replica's base rather
//! than applying the peer's update onto its own Doc. [sync-lineage-adoption]
//!
//! Pure `impl SyncNode` continuation; no items of its own. The dispatch into
//! these verbs lives in [`super::dialer`] and [`super::fork`]; the steady-state
//! delta path is also here because it shares the same `Rename`-detection seam
//! that needs the server-side blob GC kick at [`SyncNode::apply_delta_from_peer`].

use libp2p::PeerId;

use hiker_core::oplog::shapes::Author;

use crate::crypto;
use crate::identity::LocalDocId;
use crate::protocol::Message;
use crate::Error;

use super::SyncNode;

impl SyncNode {
    /// Request the peer's canonical base for the doc at `path` and adopt it
    /// locally. Path is the cross-device identity; the responder resolves it via
    /// `doc_id_for_path`. [sync-lineage-adoption, sync-path-identity]
    pub(super) async fn adopt_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
    ) -> Result<(), Error> {
        let req = Message::StateRequest {
            path: path.to_string(),
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
            .map_err(|e| Error::Apply(format!("adopt_lineage: {e}")))?;
        Ok(())
    }

    /// Request the peer's canonical base for the doc at `path` and adopt it
    /// locally DISCARDING our local divergence — the keep-theirs fork-resolution
    /// path. Unlike [`adopt_from_peer`](Self::adopt_from_peer), the local branch
    /// does not survive: the doc materializes exactly the peer's content.
    /// [sync-blocked-state]
    pub(super) async fn adopt_theirs_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
    ) -> Result<(), Error> {
        let req = Message::StateRequest {
            path: path.to_string(),
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
            .map_err(|e| Error::Apply(format!("adopt_lineage_theirs: {e}")))?;
        Ok(())
    }

    /// Push OUR canonical Yrs base to the peer so it adopts it — the "keep mine"
    /// converge half. Sends `export_state(local)` as the canonical base for the
    /// doc at `path`; the peer replaces its diverged doc with it, establishing a
    /// shared lineage on OUR side's base. Our own doc is untouched (the push
    /// only reads `export_state`). The base rides the Noise-encrypted channel to
    /// the enrolled peer and is never logged.
    /// [sync-blocked-state, sync-lineage-adoption, sync-path-identity]
    pub(super) async fn push_adopt_to_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
    ) -> Result<(), Error> {
        let state = self
            .oplog
            .export_state(&local.0)
            .map_err(|e| Error::Transport(format!("export_state: {e}")))?;
        let req = Message::PushAdopt {
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
    ///
    /// On a path-changing op (a `Rename` rode in with the delta) this also
    /// rotates the server-side blob stream: the old blind_id (HMAC of the OLD
    /// path) is now orphaned on the hub, so this device kicks a `DeleteBlob`
    /// request on the same peer channel. The P2P path's `peer_id` IS the hub
    /// when this node is talking to the relay; for direct LAN peers the call
    /// goes nowhere useful but is harmless (an unknown-blind-id delete is a
    /// successful no-op). [sync-rename-blob-rotation]
    pub(super) async fn apply_delta_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
    ) -> Result<(), Error> {
        let state_vector = self
            .oplog
            .state_vector_bytes(&local.0)
            .map_err(|e| Error::Transport(format!("state_vector_bytes: {e}")))?;
        let req = Message::DeltaRequest {
            path: path.to_string(),
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
            .map_err(|e| Error::Apply(format!("apply_remote_update: {e}")))?;
        // Blind-id rotation on rename: if applying the delta updated the doc's
        // path, the old blind_id (HMAC of the OLD path) is now orphaned on the
        // hub. Issue a `DeleteBlob` so the server GCs the stream + cursors. The
        // request is idempotent — an unknown blind_id is a successful no-op,
        // which is also why this is safe to send on a direct LAN peer
        // connection (it has no blobs at that id; it just acks). The hub clears
        // both its blob log and its per-device cursors for the old id, and the
        // new path's blind_id naturally starts a fresh stream on the next push.
        // [sync-rename-blob-rotation]
        if let Ok(Some(current_path)) = self.oplog.path_for_doc(&local.0)
            && current_path != path
        {
            let key = self.content_key.get();
            let old_blind = crypto::blind_id(&key, path);
            let del = Message::DeleteBlob {
                blind_id: old_blind.clone(),
            };
            match self.request(peer_id, del).await {
                Ok(Message::DeleteBlobAck { .. }) => {
                    tracing::debug!(
                        old_path = %path,
                        new_path = %current_path,
                        old_blind_id = %old_blind,
                        "sync: GC'd old blind_id stream after rename"
                    );
                }
                Ok(other) => {
                    tracing::debug!(
                        old_blind_id = %old_blind,
                        ?other,
                        "sync: unexpected reply to DeleteBlob; old stream may linger until peer GC"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        old_blind_id = %old_blind,
                        "sync: DeleteBlob failed; old stream may linger until peer GC"
                    );
                }
            }
        }
        Ok(())
    }

    /// Create a fresh empty local document at `path` to hold an adopted lineage.
    pub(super) fn create_local_for(&self, path: &str) -> Result<LocalDocId, Error> {
        let doc_id = self
            .oplog
            .create_document(path, "note", "", &Author::User)
            .map_err(|e| Error::Transport(format!("create_document: {e}")))?;
        Ok(LocalDocId(doc_id))
    }
}
