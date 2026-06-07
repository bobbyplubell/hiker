//! `hiker-sync` — multi-device sync of a vault's op log.
//!
//! This crate is the layer on top of the op-log substrate (`core::oplog`,
//! specced in `docs/op-log.md`): device identity, enrollment, the encrypted
//! transport, and the relay server. The substrate ships whole-file TEXT + a
//! version hash between replicas (`op-log-sync-substrate`) — no CRDT on the
//! wire; this crate moves those text blobs, authenticates endpoints, encrypts
//! content, and decides which local replicas are the same logical document,
//! reconciling concurrent edits via one 3-way text merge. See `docs/sync.md`
//! for the full design.
//!
//! # Module discipline
//!
//! The `libp2p` and `aes-gcm` dependencies are **confined to this crate**, the
//! same rule `core::oplog` applies to `rusqlite`. The public API returns
//! plain Rust types only — no `libp2p` swarm/identity type and no `aes_gcm`
//! cipher ever crosses the crate boundary. Wire
//! payloads are `Vec<u8>` blobs; identities are newtype `String`s; keys are
//! opaque newtypes over fixed byte arrays. A consumer (`app`, `cli`,
//! `hiker-syncd`) links `hiker-sync` and never transitively sees libp2p in its
//! own surface.
//!
//! # Wave status
//!
//! Waves 1–2 are implemented: the pure modules ([`crypto`], [`enroll`],
//! [`identity`], [`config`], [`protocol`]) plus the libp2p [`transport`] and the
//! [`transport::SyncNode`] peer sync-session state machine (real TCP + Noise +
//! yamux + request-response, plus mDNS discovery). The relay hub
//! ([`server`]) wiring and the `hiker-syncd` binary land in Wave 3 (search for
//! `WAVE 3` markers); the in-memory [`server::MemBlobStore`] is real and used
//! now.

pub mod config;
pub mod crypto;
pub mod enroll;
pub mod identity;
pub mod protocol;
pub mod seam;
pub mod server;
pub mod transport;

/// Crate-wide error type. Every fallible public surface returns this so a
/// consumer never has to match on a libp2p or aes-gcm error directly.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Content-layer decryption failed (bad key, truncated blob, or tampering).
    /// AES-256-GCM authentication does not distinguish these by design, so the
    /// message is deliberately coarse.
    #[error("content decryption failed (bad key or tampered ciphertext)")]
    Decrypt,

    /// A ciphertext blob was shorter than the prepended nonce.
    #[error("ciphertext blob too short to contain a nonce")]
    MalformedBlob,

    /// A device fingerprint string failed to decode or its checksum mismatched.
    #[error("invalid device fingerprint: {0}")]
    InvalidFingerprint(String),

    /// Key material (vault content key or device key) was the wrong length.
    #[error("invalid key material: {0}")]
    InvalidKey(String),

    /// Serialization / deserialization of a wire message failed.
    #[error("protocol serialization error: {0}")]
    Protocol(#[from] serde_json::Error),

    /// A transport / network operation failed (Wave 2+). Connection-level: a
    /// round that hits this can't make progress on subsequent documents either,
    /// so the dialer aborts the peer's round on a `Transport` error.
    #[error("transport error: {0}")]
    Transport(String),

    /// A DOC-LEVEL failure applying or adopting one document's state into the
    /// local op-log (e.g. a rename collision onto an occupied path, or a base
    /// the op-log refused). Unlike [`Transport`](Self::Transport) this is scoped
    /// to the one document — the connection is still healthy — so the dialer
    /// records it against that path and CONTINUES the round with the remaining
    /// documents rather than aborting.
    #[error("apply error: {0}")]
    Apply(String),
}
