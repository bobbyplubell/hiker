//! The v1 sync wire protocol — message *types* only, no networking.
//!
//! These are the payloads carried over the muxed substreams of one
//! authenticated connection (`sync-stream-muxing`): a control substream
//! (hello, manifest, enrollment, handshake) plus one substream per document
//! (lineage base + update blobs). The transport that frames and ships them is
//! [`crate::transport`] (Wave 2); the server that store-and-forwards the
//! encrypted blobs is [`crate::server`] (Wave 3).
//!
//! The session flow these messages drive: `Hello` handshake → exchange
//! [`Manifest`]s → [`crate::enroll::classify`] each path match → bind /
//! adopt / block → stream [`Message::LineageBase`] then [`Message::UpdateBlob`].
//! See `docs/sync.md` "Transport".

use serde::{Deserialize, Serialize};

/// One manifest row: a device's view of a single document at first contact.
/// `path` is the one-time matching key (`sync-path-matching-key`); the hashes
/// feed [`crate::enroll::classify`]. `logical_id` is set once the document is
/// already bound on the sending device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Vault-relative path — the one-time bind matching key.
    pub path: String,
    /// blake3 of `materialize(accepted)` as of now.
    pub current_hash: String,
    /// Recent `content_hash` history from `op_metadata`, for fast-forward
    /// classification.
    pub recent_history_hashes: Vec<String>,
    /// The shared logical id, if this document is already bound.
    pub logical_id: Option<String>,
}

/// A device's full document manifest, exchanged after the hello handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<ManifestEntry>,
}

/// The v1 protocol message set. Tagged for self-describing framing; the
/// transport encodes these (serde) onto the control / per-document substreams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// First message on the control substream: protocol version, the sending
    /// device's fingerprint (already authenticated by Noise), and the NON-SECRET
    /// fingerprint of its vault content key. The content-key fingerprint lets
    /// the peer detect whether both devices already share a key (skip transfer)
    /// or differ (the non-canonical side requests the canonical device's key
    /// in-band). [sync-vault-key-inband]
    Hello {
        protocol_version: u32,
        device_fingerprint: String,
        content_key_fp: String,
    },

    /// Acknowledge a [`Message::Hello`] (the responder's hello reply on the
    /// request-response control exchange). Carries the responder's content-key
    /// fingerprint so the dialer learns whether their keys match. [sync-vault-key-inband]
    HelloAck {
        device_fingerprint: String,
        content_key_fp: String,
    },

    /// Ask an enrolled peer for the vault content key over the
    /// already-Noise-encrypted channel. Sent by the non-canonical device when
    /// the Hello exchange shows the two devices' content-key fingerprints
    /// differ. The reply is a [`Message::ContentKeyResponse`]. [sync-vault-key-inband]
    ContentKeyRequest,

    /// The vault content key as its raw 32 bytes, in reply to a
    /// [`Message::ContentKeyRequest`]. The bytes ride the Noise-encrypted
    /// channel to a verified-enrolled peer (enrollment is the consent), so they
    /// are not re-wrapped in the content layer. NEVER logged, never written into
    /// the synced vault. [sync-vault-key-inband]
    ContentKeyResponse { key: Vec<u8> },

    /// Ask the peer for its full document manifest. The reply is a
    /// [`Message::Manifest`].
    ManifestRequest,

    /// Ask an enrolled peer for the current text of one document by its
    /// vault-relative path, so the requester can show a read-only "view diff"
    /// of a forked document before resolving it. The peer materializes its
    /// accepted ops for the path and replies with [`Message::DocContentResponse`].
    /// The text rides the Noise-encrypted channel; serving it neither binds nor
    /// mutates either side. [sync-fork-diff]
    DocContentRequest { path: String },

    /// The peer's current accepted text for a [`Message::DocContentRequest`]'s
    /// path — `materialize(accepted).text`. Read-only preview content; the
    /// requester diffs it against its own version and never adopts it.
    /// [sync-fork-diff]
    DocContentResponse { text: String },

    /// The sender's full document manifest.
    Manifest(Manifest),

    /// Ask the peer for the canonical Yrs base of a logical id (full
    /// `export_state`), so the requester can [`adopt`][adopt] it as a fresh
    /// shared lineage. The reply is a [`Message::LineageBase`].
    ///
    /// [adopt]: crate::transport::SyncNode
    StateRequest { logical_id: String },

    /// Ask the peer for the incremental update past a state-vector watermark
    /// once both sides already share a lineage. `state_vector` is the
    /// requester's `state_vector_bytes`. The reply is a [`Message::UpdateBlob`]
    /// (its `ciphertext` is `content_key.encrypt(export_since(...))`).
    /// [sync-content-encryption-aes256]
    DeltaRequest {
        logical_id: String,
        state_vector: Vec<u8>,
    },

    /// Request to bind a local path to a shared logical id (the binder
    /// proposes the id it will use).
    BindRequest { path: String, logical_id: String },

    /// Acknowledge a bind, confirming the shared logical id.
    BindAck { logical_id: String },

    /// The canonical replica's Yrs base (`encode_state_as_update_v2`) that an
    /// adopting device takes as the Doc for this logical id. Opaque bytes —
    /// the Yrs type never crosses the boundary. [sync-lineage-adoption]
    LineageBase { logical_id: String, state: Vec<u8> },

    /// A sequenced, content-encrypted Yrs update blob, keyed by blind id.
    /// `ciphertext` is AES-256-GCM output from [`crate::crypto::ContentKey`].
    /// [sync-zero-knowledge-server]
    UpdateBlob {
        blind_id: String,
        seq: u64,
        ciphertext: Vec<u8>,
    },

    /// Pull request to the store-and-forward server: everything past a device's
    /// cursor for one blind id. [sync-zero-knowledge-server]
    CursorRequest { blind_id: String, after_seq: u64 },

    /// The store-and-forward server's reply to a [`Message::CursorRequest`]: the
    /// stored `(seq, ciphertext)` blobs for one blind id past the device's
    /// cursor, ascending by `seq`. The server only ever moves opaque ciphertext;
    /// it never holds a content key. [sync-zero-knowledge-server]
    BlobBatch {
        blind_id: String,
        blobs: Vec<(u64, Vec<u8>)>,
    },

    /// The server's acknowledgement of an [`Message::UpdateBlob`] push: the
    /// highest sequence now stored for that blind id (the pusher's cursor
    /// watermark). [sync-zero-knowledge-server]
    PushAck { blind_id: String, latest_seq: u64 },

    /// Push OUR canonical Yrs base to an enrolled peer so the peer ADOPTS it —
    /// the one-click "keep mine" fork-resolution converge. The pusher's version
    /// wins: the peer replaces its diverged doc with `state` (our full
    /// `export_state`), establishing a SHARED lineage so subsequent deltas are
    /// safe. `path` resolves the peer's local doc (it may not yet share a logical
    /// id with us). `state` is the canonical v2 base; it rides the
    /// Noise-encrypted channel to a verified-enrolled peer (enrollment is the
    /// consent), so it is not re-wrapped in the content layer, NEVER logged, and
    /// NEVER written into the synced vault. The reply is a
    /// [`Message::PushAdoptAck`]. [sync-blocked-state, sync-lineage-adoption]
    PushAdopt {
        logical_id: String,
        path: String,
        state: Vec<u8>,
    },

    /// Acknowledge a [`Message::PushAdopt`]: the peer adopted our base, bound the
    /// logical id, and cleared its own block / pending resolution for it.
    /// [sync-blocked-state]
    PushAdoptAck { logical_id: String },

    /// A responder's error reply to any request it couldn't serve (a handler
    /// error, or a request from a peer it hasn't enrolled). Sent INSTEAD of
    /// dropping the response channel, so the dialer surfaces the real reason
    /// rather than an opaque "connection closed before a response" failure.
    Error { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips() {
        let msgs = vec![
            Message::Hello {
                protocol_version: 1,
                device_fingerprint: "DEV-ABC".into(),
                content_key_fp: "ckfp-aaaa".into(),
            },
            Message::HelloAck {
                device_fingerprint: "DEV-XYZ".into(),
                content_key_fp: "ckfp-bbbb".into(),
            },
            Message::ContentKeyRequest,
            Message::ContentKeyResponse {
                key: vec![1, 2, 3, 4],
            },
            Message::ManifestRequest,
            Message::DocContentRequest {
                path: "notes/a.md".into(),
            },
            Message::DocContentResponse {
                text: "hello\nworld\n".into(),
            },
            Message::Manifest(Manifest {
                entries: vec![ManifestEntry {
                    path: "notes/a.md".into(),
                    current_hash: "h0".into(),
                    recent_history_hashes: vec!["h-1".into()],
                    logical_id: None,
                }],
            }),
            Message::StateRequest {
                logical_id: "G1".into(),
            },
            Message::DeltaRequest {
                logical_id: "G1".into(),
                state_vector: vec![4, 5, 6],
            },
            Message::BindRequest {
                path: "notes/a.md".into(),
                logical_id: "G1".into(),
            },
            Message::BindAck {
                logical_id: "G1".into(),
            },
            Message::LineageBase {
                logical_id: "G1".into(),
                state: vec![1, 2, 3],
            },
            Message::UpdateBlob {
                blind_id: "bf00".into(),
                seq: 7,
                ciphertext: vec![9, 9, 9],
            },
            Message::CursorRequest {
                blind_id: "bf00".into(),
                after_seq: 3,
            },
            Message::BlobBatch {
                blind_id: "bf00".into(),
                blobs: vec![(4, vec![1, 2]), (5, vec![3, 4])],
            },
            Message::PushAck {
                blind_id: "bf00".into(),
                latest_seq: 5,
            },
            Message::PushAdopt {
                logical_id: "G1".into(),
                path: "notes/a.md".into(),
                state: vec![1, 2, 3],
            },
            Message::PushAdoptAck {
                logical_id: "G1".into(),
            },
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), m);
        }
    }
}
