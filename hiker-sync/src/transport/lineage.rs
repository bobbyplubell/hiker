//! First-contact lineage adoption verbs: bringing two independently-seeded
//! lineages onto one shared base before any deltas flow. Two
//! independently-seeded documents at the same path do not auto-merge — identical
//! bytes would interleave because the lineages share no history — so at first
//! contact the receiving device adopts the canonical replica's text rather
//! than applying the peer's update onto its own text. [sync-lineage-adoption]
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
        // A StateRequest/LineageBase fork keep-theirs adopts the peer's LIVE
        // content (a content fork is a divergence, not a delete), so tombstone is
        // false here. A keep-deleted converge instead rides `PushAdopt`, which
        // carries the delete flag explicitly. [sync-conflict-delete-vs-edit]
        self.oplog
            .adopt_lineage_theirs(&local.0, &state, false, &device_id)
            .map_err(|e| Error::Apply(format!("adopt_lineage_theirs: {e}")))?;
        Ok(())
    }

    /// Push OUR canonical text to the peer so it adopts it — the "keep mine"
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
        // Carry our tombstone flag so a keep-deleted converge pushes the DELETE
        // (text alone can't — a tombstoned doc keeps its last-known text). The
        // peer tombstones rather than resurrecting that text.
        // [sync-conflict-delete-vs-edit]
        let tombstone = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Transport(format!("materialize for push-adopt: {e}")))?
            .tombstone;
        let req = Message::PushAdopt {
            path: path.to_string(),
            state,
            tombstone,
        };
        match self.request(peer_id, req).await? {
            Message::PushAdoptAck { .. } => Ok(()),
            other => Err(Error::Transport(format!(
                "expected PushAdoptAck, got {other:?}"
            ))),
        }
    }

    /// Pull the peer's current document TEXT and reconcile it via the receive
    /// path — the steady-state streaming case once both sides share a logical
    /// document. The payload is content-decrypted whole-file text (not a
    /// delta), then `apply_remote_update` runs the 3-way TEXT merge over the
    /// content-hash merge-base and records a `sync:<peer>`-authored op.
    /// `peer_hashes` is the peer's recent content-hash window (from the manifest
    /// entry) — it anchors the merge-base; `peer_tombstone` carries the peer's
    /// delete flag (text alone can't, since a tombstone keeps the last-known
    /// text). [sync-content-encryption-aes256, sync-three-way-merge,
    /// sync-conflict-delete-vs-edit]
    ///
    /// When the manifest path differs from our local doc's current path, the
    /// peer RENAMED the doc (text carries no path move, so the rename rides the
    /// manifest path-identity). After landing the text we apply the rename
    /// explicitly with `rename_document`, then rotate the server-side blob
    /// stream: the old blind_id (HMAC of the OLD path) is now orphaned on the
    /// hub, so this device kicks a `DeleteBlob` on the same channel. The request
    /// is idempotent (an unknown blind_id is a successful no-op), so it is safe
    /// on a direct LAN peer too. [sync-rename-blob-rotation, sync-path-identity]
    pub(super) async fn apply_delta_from_peer(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
        peer_hashes: &std::collections::HashSet<String>,
        peer_tombstone: bool,
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
        let peer_text = self.content_key.get().decrypt(&ciphertext)?;
        // Tag the op with the peer's enrolled fingerprint as the sync device id.
        let device_id = self
            .enrolled
            .fingerprint_of(&peer_id)
            .map(|fp| fp.0)
            .unwrap_or_else(|| peer_id.to_string());
        self.oplog
            .apply_remote_update(&local.0, &peer_text, peer_tombstone, &device_id, peer_hashes)
            .map_err(|e| Error::Apply(format!("apply_remote_update: {e}")))?;
        // A peer rename: text carries no path move, so the rename is conveyed by
        // the manifest listing the doc at a NEW path. Our local doc still lives
        // at its old path (`local.0`); relabel it to the manifest path so both
        // replicas share the path-identity. Lineage-safe under path-as-identity
        // (`op-log-path-identity`): `rename_document` relocates the per-doc files
        // and repoints history; a no-op when the paths already match.
        if local.0 != path {
            self.oplog
                .rename_document(&local.0, path, &Author::User)
                .map_err(|e| Error::Apply(format!("apply rename: {e}")))?;
            // Move the on-disk `.md` to the new path so the projection follows
            // the rename (the op-log records the logical move; the file move is
            // the caller's per `rename_document`).
            let root = self.oplog.vault_root().to_path_buf();
            let from_abs = root.join(&local.0);
            let to_abs = root.join(path);
            if from_abs.exists() {
                if let Some(parent) = to_abs.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&from_abs, &to_abs);
            }
        }
        // Blind-id rotation on rename: the old blind_id (HMAC of the OLD path) is
        // now orphaned on the hub. Under path-as-identity (`op-log-path-identity`)
        // the rename relocated the per-doc files, so the path we requested under
        // (`local.0`) NO LONGER RESOLVES — that absence is the rename signal.
        // Issue a `DeleteBlob` so the server GCs the stream + cursors. The
        // request is idempotent — an unknown blind_id is a successful no-op,
        // which is also why this is safe to send on a direct LAN peer
        // connection (it has no blobs at that id; it just acks). The hub clears
        // both its blob log and its per-device cursors for the old id, and the
        // new path's blind_id naturally starts a fresh stream on the next push.
        // [sync-rename-blob-rotation]
        let renamed_away = matches!(self.oplog.doc_id_for_path(&local.0), Ok(None));
        if renamed_away {
            let key = self.content_key.get();
            let old_blind = crypto::blind_id(&key, &local.0);
            let del = Message::DeleteBlob {
                blind_id: old_blind.clone(),
            };
            match self.request(peer_id, del).await {
                Ok(Message::DeleteBlobAck { .. }) => {
                    tracing::debug!(
                        old_path = %local.0,
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
