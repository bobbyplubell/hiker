//! Server-mediated store-and-forward sync path. The zero-knowledge hub can't
//! decrypt content, so it degrades to an append-only encrypted-blob log per
//! document with a per-device cursor; clients push sequenced encrypted
//! whole-file TEXT blobs and pull everything past their cursor. All merge logic
//! (the 3-way text merge) stays on the client. [sync-zero-knowledge-server]
//!
//! Pure `impl SyncNode` continuation; no items of its own. The hub itself
//! lives in [`crate::server`] (its [`crate::server::BlobStore`] is what this
//! file talks to over the wire); the LAN peer paths live in
//! [`super::dialer`] / [`super::responder`].

use libp2p::PeerId;

use crate::crypto;
use crate::identity::{LocalDocId, SyncStatus};
use crate::protocol::Message;
use crate::Error;

use super::{parse_addr, SyncNode, SyncReport};

impl SyncNode {
    /// The offline-catch-up path: sync every already-bound document through the
    /// zero-knowledge hub at `server_addr`, which only relays opaque ciphertext.
    ///
    /// Binding itself never happens here — the hub can't classify or negotiate
    /// ids on ciphertext, so this path assumes docs are already bound via the
    /// P2P manifest exchange / enrollment. The two clients never talk directly;
    /// the server store-and-forwards. For each bound doc:
    /// [sync-zero-knowledge-server, sync-content-encryption-aes256]
    ///
    /// 1. **Push** — encrypt the doc's current whole-file TEXT under the content
    ///    key and `UpdateBlob` it to the hub keyed by `blind_id(content_key,
    ///    path)`, at this device's next monotonic per-blind-id seq. Shipping the
    ///    full text is idempotent: a peer merging it (or a re-push) re-runs the
    ///    text merge, and identical content is a no-op.
    /// 2. **Pull** — `CursorRequest` everything past our cursor for that blind
    ///    id, decrypt each blob, and `apply_remote_update` it (a 3-way text merge
    ///    that, with no shared-base window over the relay, fast-forwards to the
    ///    peer's text). Our own pushed text decrypts to our own content and
    ///    merges as a no-op; a peer's text converges our replica. The cursor
    ///    advances to the batch's high-water seq.
    ///
    /// The server is dialed as an enrolled peer (its fingerprint must be enrolled
    /// on this node), so the same Noise + enrollment gate as the P2P path
    /// authenticates the hub connection. [sync-noise-channel]
    pub async fn sync_via_server(&mut self, server_addr: &str) -> Result<SyncReport, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(server_addr)?;
        let server_id = self.connect(addr).await?;

        // Snapshot every (doc_id, path) pair from the oplog up front. Path is
        // the cross-device identity: blind_id derives from path so two devices
        // independently compute the same blind id for the same path.
        // [sync-path-identity, sync-blind-id]
        let docs: Vec<(LocalDocId, String)> = {
            let ids = self
                .oplog
                .list_doc_ids()
                .map_err(|e| Error::Transport(format!("list docs: {e}")))?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(path) = self
                    .oplog
                    .path_for_doc(&id)
                    .map_err(|e| Error::Transport(format!("path for {id}: {e}")))?
                {
                    out.push((LocalDocId(id), path));
                }
            }
            out
        };

        let mut report = SyncReport::default();
        for (local, path) in docs {
            // A blocked doc streams nothing in either direction. [sync-blocked-state]
            if self.status_of_path(&path) == Some(SyncStatus::Blocked) {
                continue;
            }
            let blind = {
                let key = self.content_key.get();
                crypto::blind_id(&key, &path)
            };

            // 1. Push our current state as the next monotonic blob.
            self.push_state(server_id, &local, &blind).await?;
            report.bound.push(path.clone());

            // 2. Pull + apply everything past our cursor.
            if self.pull_and_apply(server_id, &local, &blind).await? {
                report.converged.push(path);
            }
        }
        Ok(report)
    }

    /// Push the local doc's current whole-file TEXT to the hub as one
    /// content-encrypted `UpdateBlob` at this device's next per-blind-id seq.
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
        // The zero-knowledge hub ships opaque whole-file TEXT blobs and never the
        // peer's content-hash history, so the receiver has no shared-base window
        // here: pass an empty `peer_hashes`, which makes `apply_remote_update`
        // fall back to `base = ours` → a fast-forward to the peer's text. That is
        // exactly the server convergence contract (the relay carries an already-
        // bound doc strictly ahead); an identical re-pull is an idempotent no-op.
        // A delete over the relay is out of scope for the store-and-forward path
        // (no tombstone flag on the blob), so `peer_tombstone = false`.
        let empty_hashes = std::collections::HashSet::new();
        let mut advanced = false;
        let mut high = after;
        for (seq, ciphertext) in blobs {
            let peer_text = self.content_key.get().decrypt(&ciphertext)?;
            if self
                .oplog
                .apply_remote_update(&local.0, &peer_text, false, &device_id, &empty_hashes)
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
}
