//! The v1 sync wire protocol — message *types* only, no networking.
//!
//! These are the payloads carried over the muxed substreams of one
//! authenticated connection (`sync-stream-muxing`): a control substream
//! (hello, manifest, content-key transfer) plus one substream per document
//! keyed by **vault path** (lineage base + update blobs). The transport that
//! frames and ships them is [`crate::transport`]; the server that
//! store-and-forwards the encrypted blobs is [`crate::server`].
//!
//! The session flow these messages drive: `Hello` handshake → exchange
//! [`Manifest`]s → [`crate::enroll::classify`] each path → adopt / stream
//! delta / block. The transport speaks paths end-to-end; each device keeps
//! its own internal `doc_id` handle for op-log bookkeeping but never exchanges
//! it. [sync-path-identity]
//!
//! See `docs/sync.md` "Transport".

use serde::{Deserialize, Serialize};

/// One manifest row: a device's view of a single document at a vault path.
/// The hashes feed [`crate::enroll::classify`]. The document key is the path
/// itself — there is no separate logical id. [sync-path-identity]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Vault-relative path — the cross-device identity of this document.
    pub path: String,
    /// blake3 of `materialize(accepted)` as of now.
    pub current_hash: String,
    /// Recent `content_hash` history from the side table, for fast-forward
    /// classification.
    pub recent_history_hashes: Vec<String>,
    /// Vault paths this document has previously occupied on the sender (the
    /// `from` of every `Rename` op in its accepted history). The receiver uses
    /// this to follow a rename: if its local replica still lives at one of
    /// these prior paths, the manifest at the new path identifies the same
    /// document, so the receiver pulls the delta against the new path against
    /// its existing doc's state vector and folds in the `Rename` op.
    /// `#[serde(default)]` keeps the field additive (an older peer that omits
    /// it still parses, falling back to never-matched). [sync-path-identity,
    /// sync-rename-blob-rotation]
    #[serde(default)]
    pub prior_paths: Vec<String>,
    /// Whether this document is tombstoned (deleted) on the sender. A deleted
    /// doc keeps its last-known text (so `current_hash` alone can't reveal the
    /// delete), so the manifest carries the flag explicitly: the receiver's
    /// delete-vs-edit gate and the fast-forward-delete auto-apply both need to
    /// know the peer deleted. `#[serde(default)]` keeps it additive (an older
    /// peer that omits it parses as live). [sync-conflict-delete-vs-edit]
    #[serde(default)]
    pub tombstone: bool,
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
    ///
    /// `device_name` is the sender's SELF-set human name (`[sync].device_name`):
    /// the receiver adopts it into its learned `fingerprint -> name` map so it
    /// can render "synced from `laptop`" instead of a fingerprint. `Option` and
    /// `#[serde(default)]` keep it additive — an older peer that omits the field
    /// still parses. [sync-device-name]
    Hello {
        protocol_version: u32,
        device_fingerprint: String,
        content_key_fp: String,
        #[serde(default)]
        device_name: Option<String>,
    },

    /// Acknowledge a [`Message::Hello`] (the responder's hello reply on the
    /// request-response control exchange). Carries the responder's content-key
    /// fingerprint so the dialer learns whether their keys match. [sync-vault-key-inband]
    ///
    /// Like [`Message::Hello`], carries the responder's self-set `device_name`
    /// so the dialer learns the responder's name on the same round trip.
    /// Additive (`Option` + `#[serde(default)]`). [sync-device-name]
    HelloAck {
        device_fingerprint: String,
        content_key_fp: String,
        #[serde(default)]
        device_name: Option<String>,
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

    /// The peer's current accepted state for a [`Message::DocContentRequest`]'s
    /// path — `materialize(accepted)`'s `text` plus its `tombstone` flag.
    /// Read-only preview content; the requester diffs it against its own version
    /// and never adopts it. The `tombstone` flag lets the delete-vs-edit
    /// detection tell a deleted peer doc from a live one (a tombstone keeps the
    /// last-known text, so text alone can't), per `sync-conflict-delete-vs-edit`.
    /// [sync-fork-diff, sync-conflict-delete-vs-edit]
    DocContentResponse { text: String, tombstone: bool },

    /// The sender's full document manifest.
    Manifest(Manifest),

    /// Ask the peer for the canonical TEXT of the document at `path` (its
    /// `export_state` whole-file content), so the requester can adopt / merge it.
    /// The reply is a [`Message::LineageBase`]. Under the text substrate the
    /// bytes are the document text. [sync-path-identity,
    /// sync-lineage-adoption]
    StateRequest { path: String },

    /// Ask the peer for the current TEXT of the document at `path` once both
    /// sides already share a logical document. `state_vector` is the requester's
    /// VESTIGIAL `state_vector_bytes` watermark (a content-hash token — the
    /// responder ignores it and returns the full current text); kept so the
    /// field shape doesn't churn. The reply is a [`Message::UpdateBlob`] (its
    /// `ciphertext` is `content_key.encrypt(export_since(...))` = the whole-file
    /// text). [sync-content-encryption-aes256, sync-path-identity]
    DeltaRequest { path: String, state_vector: Vec<u8> },

    /// The canonical replica's whole-file TEXT (`export_state`) that an adopting
    /// device merges/adopts for `path`. Opaque bytes — the document's `.md` text.
    /// [sync-lineage-adoption]
    LineageBase { path: String, state: Vec<u8> },

    /// A sequenced, content-encrypted whole-file TEXT blob, keyed by blind id.
    /// `ciphertext` is AES-256-GCM output from [`crate::crypto::ContentKey`]
    /// over the document text. [sync-zero-knowledge-server]
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

    /// Push OUR canonical TEXT to an enrolled peer so the peer ADOPTS it — the
    /// one-click "keep mine" fork-resolution converge for the document at
    /// `path`. The pusher's version wins: the peer replaces its diverged doc
    /// with `state` (our full `export_state` whole-file text). `state` is the
    /// document's `.md` text; it rides the Noise-encrypted channel to a
    /// verified-enrolled peer (enrollment is the consent), so it is not
    /// re-wrapped in the content layer, NEVER logged, and NEVER written into the
    /// synced vault. `tombstone` carries the pusher's delete flag (text alone
    /// can't — a tombstoned doc keeps its last-known text), so a keep-deleted
    /// converge pushes the DELETE and the adopting peer tombstones rather than
    /// resurrecting the last-known text. The reply is a [`Message::PushAdoptAck`].
    /// [sync-blocked-state, sync-lineage-adoption, sync-conflict-delete-vs-edit]
    PushAdopt {
        path: String,
        state: Vec<u8>,
        #[serde(default)]
        tombstone: bool,
    },

    /// Acknowledge a [`Message::PushAdopt`]: the peer adopted our base at
    /// `path` and cleared its own block / pending resolution for it.
    /// [sync-blocked-state]
    PushAdoptAck { path: String },

    /// A responder's error reply to any request it couldn't serve (a handler
    /// error, or a request from a peer it hasn't enrolled). Sent INSTEAD of
    /// dropping the response channel, so the dialer surfaces the real reason
    /// rather than an opaque "connection closed before a response" failure.
    Error { reason: String },

    /// Ask the store-and-forward server to GC the blob stream at `blind_id`:
    /// drop every stored `(seq, ciphertext)` for that id AND reset all
    /// per-device cursors against it. Sent by a client whose local replica just
    /// applied a `Rename` op that rotated the doc's path → its old blind_id is
    /// now an orphan stream on the hub. Authenticated by the enrollment gate
    /// like every other request; idempotent on the server (an unknown
    /// `blind_id` is a successful no-op). The reply is a
    /// [`Message::DeleteBlobAck`]. [sync-rename-blob-rotation]
    DeleteBlob { blind_id: String },

    /// Acknowledge a [`Message::DeleteBlob`]: the server dropped the stream
    /// (or had nothing at that id), echoing the `blind_id` for correlation.
    /// [sync-rename-blob-rotation]
    DeleteBlobAck { blind_id: String },

    /// A content-free "wake up and pull" nudge to an enrolled peer: the sender
    /// just committed a local change and wants the peer to run its existing pull
    /// path promptly rather than waiting for its own ~15s poll tick. Carries no
    /// payload — it only sets the peer's `poked` flag so the peer's sync driver
    /// fires an `auto_sync_round`, which is where the actual manifest/delta
    /// exchange happens. The connection is already enrollment-gated. The reply is
    /// a [`Message::SyncPokeAck`]. [sync-poke-on-commit]
    SyncPoke,

    /// Acknowledge a [`Message::SyncPoke`]: the peer recorded the nudge and will
    /// pull on its next driver turn. [sync-poke-on-commit]
    SyncPokeAck,
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
                device_name: Some("laptop".into()),
            },
            Message::HelloAck {
                device_fingerprint: "DEV-XYZ".into(),
                content_key_fp: "ckfp-bbbb".into(),
                device_name: None,
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
                tombstone: false,
            },
            Message::Manifest(Manifest {
                entries: vec![ManifestEntry {
                    path: "notes/a.md".into(),
                    current_hash: "h0".into(),
                    recent_history_hashes: vec!["h-1".into()],
                    prior_paths: vec!["notes/a-old.md".into()],
                    tombstone: false,
                }],
            }),
            Message::StateRequest {
                path: "notes/a.md".into(),
            },
            Message::DeltaRequest {
                path: "notes/a.md".into(),
                state_vector: vec![4, 5, 6],
            },
            Message::LineageBase {
                path: "notes/a.md".into(),
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
                path: "notes/a.md".into(),
                state: vec![1, 2, 3],
                tombstone: false,
            },
            Message::PushAdoptAck {
                path: "notes/a.md".into(),
            },
            Message::DeleteBlob {
                blind_id: "bf00".into(),
            },
            Message::DeleteBlobAck {
                blind_id: "bf00".into(),
            },
            Message::SyncPoke,
            Message::SyncPokeAck,
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), m);
        }
    }

    /// A `Hello` from an older peer that predates `device_name` (the field is
    /// absent on the wire) still parses, with `device_name == None`. This is the
    /// additive backward-compat guarantee. [sync-device-name]
    #[test]
    fn hello_without_device_name_field_parses() {
        let legacy = r#"{"type":"hello","protocol_version":1,"device_fingerprint":"DEV-OLD","content_key_fp":"ckfp"}"#;
        let parsed: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            parsed,
            Message::Hello {
                protocol_version: 1,
                device_fingerprint: "DEV-OLD".into(),
                content_key_fp: "ckfp".into(),
                device_name: None,
            }
        );
    }
}
