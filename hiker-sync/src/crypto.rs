//! Content encryption, blind ids, and device identity.
//!
//! Two secrets live here (both user-scope, never in the synced vault —
//! `sync-secrets-user-scope`):
//!
//! - The per-vault **content key** ([`ContentKey`]): a 256-bit AES-GCM key
//!   shared by all enrolled devices. Each Yrs `update_v2` is encrypted with it
//!   on the client before it leaves the device, which is what makes the server
//!   zero-knowledge. [sync-content-encryption-aes256]
//! - The per-device **static keypair** ([`DeviceKeypair`]): Ed25519, used to
//!   authenticate the Noise channel. Its [`DeviceFingerprint`] is swapped out
//!   of band to enroll. [sync-key-swap-enrollment]
//!
//! The server keys blobs by a **blind id** — `HMAC(content_key, logical_id)` —
//! so it sees random-looking ids, never human paths. [sync-blind-id]
//!
//! `aes-gcm` and `libp2p`'s identity types are confined to this module; the
//! public surface returns plain `Vec<u8>` / `String` / fixed-array newtypes.

use std::sync::{Arc, Mutex};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

use crate::Error;
use crate::identity::DeviceFingerprint;

/// AES-256-GCM nonce length in bytes (96-bit, the standard GCM nonce).
const NONCE_LEN: usize = 12;

type HmacSha256 = Hmac<Sha256>;

/// The per-vault 256-bit content key. Encrypts every update blob with
/// AES-256-GCM and derives blind ids. Opaque newtype over the raw key bytes;
/// the `aes_gcm` cipher never escapes this module.
#[derive(Clone)]
pub struct ContentKey([u8; 32]);

impl ContentKey {
    /// Wrap raw 32-byte key material.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Generate a fresh random content key from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Borrow the raw key bytes (for at-rest storage in the OS keychain).
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A short, NON-SECRET fingerprint of the key: the head of `blake3(key
    /// bytes)` as hex. Preimage-resistant, so exposing it never reveals the key.
    /// Used only to detect "same key vs different key" over the authenticated
    /// channel — the handshake compares fingerprints to decide whether the
    /// in-band content-key transfer is needed. [sync-vault-key-inband]
    pub fn fingerprint(&self) -> String {
        // 8 bytes of the blake3 digest → 16 hex chars. Collision-irrelevant
        // here (it only gates "do we already share a key").
        let digest = blake3::hash(&self.0);
        let mut s = String::with_capacity(16);
        for b in &digest.as_bytes()[..8] {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// The bs58 encoding of the 32 key bytes — the human-transferable form for
    /// the manual content-key swap (`sync-vault-key-inband`'s manual stand-in).
    /// This is a SECRET; only show it over a trusted channel between a user's
    /// own devices. Reuses `bs58` (already a dep for fingerprints) so no base64
    /// crate is pulled in.
    pub fn to_b58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }

    /// Parse a bs58-encoded content key produced by [`to_b58`](Self::to_b58).
    /// Rejects anything that doesn't decode to exactly 32 bytes with
    /// [`Error::InvalidKey`].
    pub fn from_b58(s: &str) -> Result<Self, Error> {
        let bytes = bs58::decode(s.trim())
            .into_vec()
            .map_err(|e| Error::InvalidKey(format!("content key not valid bs58: {e}")))?;
        if bytes.len() != 32 {
            return Err(Error::InvalidKey(format!(
                "content key must be 32 bytes, got {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Encrypt `plaintext` with AES-256-GCM under a fresh random 96-bit nonce.
    /// The nonce is **prepended** to the ciphertext: the returned blob is
    /// `nonce (12 bytes) || ciphertext+tag`. Self-contained, so [`decrypt`]
    /// needs no out-of-band nonce.
    ///
    /// [`decrypt`]: ContentKey::decrypt
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new(self.0.as_ref().into());
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        // AES-GCM encryption only fails on absurd input sizes; treat it as
        // infallible for our (Yrs-update-sized) payloads.
        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .expect("AES-256-GCM encryption of a bounded payload cannot fail");

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        blob
    }

    /// Decrypt a blob produced by [`encrypt`]. Splits off the prepended 96-bit
    /// nonce and verifies the GCM tag. Returns [`Error::Decrypt`] on a wrong
    /// key or tampered/truncated ciphertext (GCM does not distinguish these),
    /// or [`Error::MalformedBlob`] if the blob is shorter than the nonce.
    ///
    /// [`encrypt`]: ContentKey::encrypt
    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, Error> {
        if blob.len() < NONCE_LEN {
            return Err(Error::MalformedBlob);
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(self.0.as_ref().into());
        let nonce = Nonce::from_slice(nonce_bytes);
        cipher.decrypt(nonce, ciphertext).map_err(|_| Error::Decrypt)
    }
}

impl std::fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key bytes.
        f.write_str("ContentKey(<redacted>)")
    }
}

/// A shared, persist-through handle to the vault content key.
///
/// Wraps `Arc<Mutex<ContentKey>>` plus an optional persist hook, so the node and
/// the app's sync service hold a clone of the SAME key (mirroring the
/// [`crate::transport::EnrolledPeers`] sharing pattern). [`set`](Self::set)
/// updates the key in place AND calls the persist hook, so the in-band transfer
/// (`sync-vault-key-inband`) and the manual import both write through to
/// at-rest storage consistently. The node uses this for encrypt / decrypt /
/// blind-id, so swapping the key takes effect for the whole session at once.
#[derive(Clone)]
pub struct SharedContentKey {
    inner: Arc<Mutex<ContentKey>>,
    /// Called with the new key on every [`set`](Self::set). The app wires this
    /// to write through its user-scope `KeyStore`; tests can leave it unset.
    persist: Option<Arc<dyn Fn(&ContentKey) + Send + Sync>>,
}

impl SharedContentKey {
    /// A handle with no persist hook — the key lives only in memory (tests, or
    /// a node whose caller persists separately).
    pub fn new(key: ContentKey) -> Self {
        Self {
            inner: Arc::new(Mutex::new(key)),
            persist: None,
        }
    }

    /// A handle whose [`set`](Self::set) also calls `persist` with the new key —
    /// the write-through used by the app's sync service to keep the on-disk
    /// content key in step with the in-memory one.
    pub fn with_persist(
        key: ContentKey,
        persist: Arc<dyn Fn(&ContentKey) + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(key)),
            persist: Some(persist),
        }
    }

    /// The current key (clone). Used for export and for the encrypt / decrypt /
    /// blind-id call sites.
    pub fn get(&self) -> ContentKey {
        self.inner.lock().unwrap().clone()
    }

    /// Replace the key in place AND persist it (if a hook is set). The seam both
    /// the in-band auto-transfer and the manual import route through.
    pub fn set(&self, key: ContentKey) {
        if let Some(persist) = &self.persist {
            persist(&key);
        }
        *self.inner.lock().unwrap() = key;
    }

    /// The non-secret fingerprint of the current key — the handshake comparison
    /// value. [sync-vault-key-inband]
    pub fn fingerprint(&self) -> String {
        self.inner.lock().unwrap().fingerprint()
    }
}

/// The server's per-document key: `hex(HMAC-SHA256(content_key, logical_id))`.
/// Deterministic and key-dependent, so the server can group a document's blobs
/// without ever learning the human path. [sync-blind-id]
pub fn blind_id(key: &ContentKey, logical_id: &str) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(&key.0)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(logical_id.as_bytes());
    let tag = mac.finalize().into_bytes();
    hex_encode(&tag)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A per-device static Ed25519 keypair for Noise channel authentication. Wraps
/// `libp2p::identity::Keypair`; the libp2p type never escapes this module.
pub struct DeviceKeypair {
    inner: libp2p::identity::Keypair,
}

impl DeviceKeypair {
    /// Generate a fresh Ed25519 device keypair.
    pub fn generate() -> Self {
        Self {
            inner: libp2p::identity::Keypair::generate_ed25519(),
        }
    }

    /// Reconstruct from a protobuf-encoded keypair (as stored at rest).
    pub fn from_protobuf(bytes: &[u8]) -> Result<Self, Error> {
        let inner = libp2p::identity::Keypair::from_protobuf_encoding(bytes)
            .map_err(|e| Error::InvalidKey(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Protobuf-encode the keypair for at-rest storage (OS keychain).
    pub fn to_protobuf(&self) -> Result<Vec<u8>, Error> {
        self.inner
            .to_protobuf_encoding()
            .map_err(|e| Error::InvalidKey(e.to_string()))
    }

    /// The short, checksummed device fingerprint (Syncthing-Device-ID flavor):
    /// `bs58(pubkey_bytes)` with a short bs58 checksum suffix derived from a
    /// blake3 hash of the public key. Stable for a given key, swapped out of
    /// band to authenticate. [sync-key-swap-enrollment]
    pub fn fingerprint(&self) -> DeviceFingerprint {
        DeviceFingerprint(fingerprint_for_pubkey(&self.inner.public()))
    }

    /// The underlying libp2p keypair, for the transport's Noise static-key
    /// config. The Noise static key IS the device identity, so the remote
    /// `PeerId` derived from this key is exactly what enrollment authenticates
    /// against. Crate-internal: the libp2p type never escapes the public API.
    /// [sync-noise-channel]
    pub(crate) const fn libp2p_keypair(&self) -> &libp2p::identity::Keypair {
        &self.inner
    }
}

/// Render a public key as a checksummed fingerprint string.
fn fingerprint_for_pubkey(public: &libp2p::identity::PublicKey) -> String {
    let pk_bytes = public.encode_protobuf();
    let body = bs58::encode(&pk_bytes).into_string();

    // 4-byte checksum over the key bytes, bs58-encoded, appended after a
    // separator — recomputable by the receiving device to catch typos.
    let digest = blake3::hash(&pk_bytes);
    let check = bs58::encode(&digest.as_bytes()[..4]).into_string();
    format!("{body}-{check}")
}

/// Recover the libp2p [`PeerId`] a [`DeviceFingerprint`] denotes.
///
/// The fingerprint body is `bs58(public_key_protobuf)`; a libp2p `PeerId` is the
/// identity-multihash of that same protobuf. So an enrolled fingerprint maps
/// deterministically to the `PeerId` the remote presents over Noise, which is
/// how the transport authenticates a connection against the enrolled set
/// without the libp2p type leaking into the public enrollment API.
/// [sync-noise-channel]
///
/// Returns [`Error::InvalidFingerprint`] if the body doesn't decode to a valid
/// public key or the checksum suffix doesn't recompute.
///
/// [`PeerId`]: libp2p::PeerId
pub(crate) fn fingerprint_to_peer_id(
    fingerprint: &DeviceFingerprint,
) -> Result<libp2p::PeerId, Error> {
    let s = &fingerprint.0;
    let (body, check) = s
        .rsplit_once('-')
        .ok_or_else(|| Error::InvalidFingerprint(s.clone()))?;
    let pk_bytes = bs58::decode(body)
        .into_vec()
        .map_err(|_| Error::InvalidFingerprint(s.clone()))?;
    // Recompute the checksum suffix; a mismatch means a typo / truncation.
    let digest = blake3::hash(&pk_bytes);
    let expect_check = bs58::encode(&digest.as_bytes()[..4]).into_string();
    if check != expect_check {
        return Err(Error::InvalidFingerprint(s.clone()));
    }
    let public = libp2p::identity::PublicKey::try_decode_protobuf(&pk_bytes)
        .map_err(|_| Error::InvalidFingerprint(s.clone()))?;
    Ok(public.to_peer_id())
}

/// Derive the [`DeviceFingerprint`] a libp2p [`PeerId`] denotes — the inverse of
/// [`fingerprint_to_peer_id`].
///
/// Our device keys are Ed25519, whose public key is small enough that libp2p
/// embeds it directly in the `PeerId` as an *identity* multihash (code `0x00`):
/// the multihash digest IS the protobuf-encoded public key. So for our own
/// peers the `PeerId` is fully invertible — recover the public key from the
/// digest and re-encode it via [`fingerprint_for_pubkey`].
///
/// Returns `None` for a `PeerId` we can't invert: one whose multihash isn't the
/// identity code (a SHA-256-hashed id of some larger key — not ours, and a hash
/// can't be reversed), or whose digest doesn't decode to a valid public key.
/// This is the seam the UI uses to offer a one-click enroll for a discovered
/// peer (it shows the derived fingerprint for the user to verify); a peer whose
/// fingerprint can't be derived simply gets no button. [sync-mdns-discovery]
///
/// [`PeerId`]: libp2p::PeerId
pub(crate) fn peer_id_to_fingerprint(peer_id: &libp2p::PeerId) -> Option<DeviceFingerprint> {
    let mh = peer_id.as_ref();
    // Only an identity multihash carries the public key verbatim; a hashed id
    // (any other code) is one-way and can't be inverted back to a fingerprint.
    const IDENTITY_MULTIHASH_CODE: u64 = 0x00;
    if mh.code() != IDENTITY_MULTIHASH_CODE {
        return None;
    }
    let public = libp2p::identity::PublicKey::try_decode_protobuf(mh.digest()).ok()?;
    Some(DeviceFingerprint(fingerprint_for_pubkey(&public)))
}

/// Validate a device fingerprint WITHOUT touching the live node: decode its
/// body + verify its checksum suffix by recovering the `PeerId` it denotes.
/// This is the lock-free pre-check the app runs on the egui thread before
/// spawning the node-side enroll/unenroll (which itself needs the node lock);
/// it gives immediate error feedback on a malformed fingerprint while the
/// actual node mutation happens off-thread. Returns [`Error::InvalidFingerprint`]
/// on a bad body or mismatched checksum.
pub fn validate_fingerprint(fingerprint: &DeviceFingerprint) -> Result<(), Error> {
    fingerprint_to_peer_id(fingerprint).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_a() -> ContentKey {
        ContentKey::from_bytes([7u8; 32])
    }
    fn key_b() -> ContentKey {
        ContentKey::from_bytes([9u8; 32])
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = key_a();
        let plaintext = b"a yrs update_v2 blob".to_vec();
        let blob = key.encrypt(&plaintext);
        // Nonce is prepended, so the blob is longer than plaintext + tag.
        assert!(blob.len() > plaintext.len() + NONCE_LEN);
        let back = key.decrypt(&blob).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn fresh_nonce_makes_ciphertext_nondeterministic() {
        let key = key_a();
        let a = key.encrypt(b"same input");
        let b = key.encrypt(b"same input");
        assert_ne!(a, b, "random nonce must make blobs differ");
        assert_eq!(key.decrypt(&a).unwrap(), key.decrypt(&b).unwrap());
    }

    #[test]
    fn tampered_blob_fails() {
        let key = key_a();
        let mut blob = key.encrypt(b"secret");
        // Flip a bit in the ciphertext body (past the nonce).
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(matches!(key.decrypt(&blob), Err(Error::Decrypt)));
    }

    #[test]
    fn wrong_key_fails() {
        let blob = key_a().encrypt(b"secret");
        assert!(matches!(key_b().decrypt(&blob), Err(Error::Decrypt)));
    }

    #[test]
    fn short_blob_is_malformed() {
        let key = key_a();
        assert!(matches!(key.decrypt(&[0u8; 4]), Err(Error::MalformedBlob)));
    }

    #[test]
    fn content_key_b58_round_trips() {
        let key = ContentKey::from_bytes([42u8; 32]);
        let s = key.to_b58();
        let back = ContentKey::from_b58(&s).unwrap();
        assert_eq!(back.as_bytes(), key.as_bytes(), "b58 round-trip preserves key");

        // Whitespace around a pasted string is tolerated.
        let padded = format!("  {s}\n");
        assert_eq!(
            ContentKey::from_b58(&padded).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn content_key_from_b58_rejects_wrong_length() {
        // bs58 of 16 zero bytes decodes fine but is the wrong length.
        let short = bs58::encode([0u8; 16]).into_string();
        assert!(matches!(ContentKey::from_b58(&short), Err(Error::InvalidKey(_))));
        // Too long.
        let long = bs58::encode([0u8; 33]).into_string();
        assert!(matches!(ContentKey::from_b58(&long), Err(Error::InvalidKey(_))));
        // Not valid bs58 at all (contains an invalid char).
        assert!(matches!(ContentKey::from_b58("0OIl"), Err(Error::InvalidKey(_))));
    }

    #[test]
    fn content_key_fingerprint_is_stable_and_distinguishes_keys() {
        let a = key_a();
        // Stable for a given key.
        assert_eq!(a.fingerprint(), a.fingerprint(), "fingerprint is stable");
        assert_eq!(a.fingerprint(), key_a().fingerprint(), "same bytes → same fp");
        // 8 digest bytes → 16 hex chars.
        assert_eq!(a.fingerprint().len(), 16);
        assert!(a.fingerprint().chars().all(|c| c.is_ascii_hexdigit()));
        // Different keys → different fingerprints.
        assert_ne!(a.fingerprint(), key_b().fingerprint(), "different key → different fp");
        // Non-secret: the fingerprint is a hash, not the key bytes verbatim.
        assert_ne!(a.fingerprint(), hex_encode(a.as_bytes()), "fp is not the raw key");
    }

    #[test]
    fn blind_id_is_deterministic_and_key_dependent() {
        let a = blind_id(&key_a(), "logical-123");
        let a2 = blind_id(&key_a(), "logical-123");
        assert_eq!(a, a2, "same key + id must be stable");

        let b = blind_id(&key_b(), "logical-123");
        assert_ne!(a, b, "different key must give different blind id");

        let c = blind_id(&key_a(), "logical-456");
        assert_ne!(a, c, "different logical id must give different blind id");

        // hex of a 32-byte SHA-256 HMAC.
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn shared_content_key_set_updates_and_persists() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A persist hook that records the last-persisted fingerprint + call count.
        let calls = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(String::new()));
        let calls_c = calls.clone();
        let last_c = last.clone();
        let shared = SharedContentKey::with_persist(
            key_a(),
            Arc::new(move |k: &ContentKey| {
                calls_c.fetch_add(1, Ordering::SeqCst);
                *last_c.lock().unwrap() = k.fingerprint();
            }),
        );

        // Initial state: no persist on construct, fingerprint matches key_a.
        assert_eq!(calls.load(Ordering::SeqCst), 0, "construct doesn't persist");
        assert_eq!(shared.fingerprint(), key_a().fingerprint());

        // set() swaps the key in place AND fires the persist hook with the new key.
        shared.set(key_b());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "set persists once");
        assert_eq!(shared.fingerprint(), key_b().fingerprint(), "key swapped in place");
        assert_eq!(*last.lock().unwrap(), key_b().fingerprint(), "persisted the new key");
        assert_eq!(shared.get().as_bytes(), key_b().as_bytes(), "get reflects the swap");
    }

    #[test]
    fn shared_content_key_without_persist_is_fine() {
        let shared = SharedContentKey::new(key_a());
        shared.set(key_b());
        assert_eq!(shared.get().as_bytes(), key_b().as_bytes(), "set works with no hook");
    }

    #[test]
    fn fingerprint_is_stable_for_a_key() {
        let kp = DeviceKeypair::generate();
        let f1 = kp.fingerprint();
        let f2 = kp.fingerprint();
        assert_eq!(f1, f2);
        assert!(f1.0.contains('-'), "fingerprint carries a checksum suffix");
    }

    #[test]
    fn distinct_keys_have_distinct_fingerprints() {
        let a = DeviceKeypair::generate().fingerprint();
        let b = DeviceKeypair::generate().fingerprint();
        assert_ne!(a, b);
    }

    #[test]
    fn validate_fingerprint_accepts_real_and_rejects_garbage() {
        // A real fingerprint validates without the node lock.
        let fp = DeviceKeypair::generate().fingerprint();
        assert!(validate_fingerprint(&fp).is_ok());

        // No checksum separator.
        assert!(matches!(
            validate_fingerprint(&DeviceFingerprint("nochecksum".to_string())),
            Err(Error::InvalidFingerprint(_))
        ));
        // Tampered checksum suffix.
        let mut tampered = fp.0.clone();
        tampered.pop();
        tampered.push(if fp.0.ends_with('x') { 'y' } else { 'x' });
        assert!(matches!(
            validate_fingerprint(&DeviceFingerprint(tampered)),
            Err(Error::InvalidFingerprint(_))
        ));
    }

    #[test]
    fn peer_id_to_fingerprint_round_trips() {
        // A generated Ed25519 device fingerprint maps to a PeerId and back to
        // the same fingerprint (identity multihash, fully invertible).
        let fp = DeviceKeypair::generate().fingerprint();
        let peer_id = fingerprint_to_peer_id(&fp).unwrap();
        assert_eq!(
            peer_id_to_fingerprint(&peer_id),
            Some(fp),
            "peer_id_to_fingerprint inverts fingerprint_to_peer_id for an Ed25519 key"
        );
    }

    #[test]
    fn peer_id_to_fingerprint_none_for_hashed_peer_id() {
        // A SHA-256-hashed PeerId (non-identity multihash) can't be inverted.
        let kp = DeviceKeypair::generate();
        let public = kp.libp2p_keypair().public();
        let hashed = libp2p::PeerId::from_public_key(&public);
        // from_public_key uses an identity hash for keys small enough to embed
        // (ours), so force a hashed id by re-wrapping the digest under SHA-256.
        let sha = libp2p::multihash::Multihash::<64>::wrap(
            0x12, // sha2-256
            blake3::hash(hashed.to_bytes().as_slice()).as_bytes(),
        )
        .unwrap();
        let hashed_peer = libp2p::PeerId::from_multihash(sha).unwrap();
        assert_eq!(peer_id_to_fingerprint(&hashed_peer), None);
    }

    #[test]
    fn keypair_protobuf_round_trip_preserves_fingerprint() {
        let kp = DeviceKeypair::generate();
        let bytes = kp.to_protobuf().unwrap();
        let restored = DeviceKeypair::from_protobuf(&bytes).unwrap();
        assert_eq!(kp.fingerprint(), restored.fingerprint());
    }
}
