//! The zero-knowledge relay hub: the [`BlobStore`] storage seam, its in-memory
//! ([`MemBlobStore`]) and file-backed ([`FileBlobStore`]) implementations, and
//! the [`Hub`] that wraps a store behind the libp2p request-response
//! transport. [sync-decoupled-server, sync-zero-knowledge-server]
//!
//! The server never holds the vault content key, so it cannot decrypt. It
//! degrades to the only thing it can do on ciphertext: an **append-only
//! encrypted-blob log per document, keyed by blind id, with a per-device
//! cursor** (store-and-forward). Clients push sequenced encrypted text blobs;
//! a device pulls everything past its cursor, decrypts, and text-merges.
//! All merge logic stays on the client. The hub only moves opaque
//! `(blind_id, seq, ciphertext)` triples — never a key, a path, or plaintext.
//! [sync-zero-knowledge-server, sync-blind-id]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use libp2p::request_response::{self};
use libp2p::swarm::SwarmEvent;
use libp2p::{PeerId, Swarm};

use crate::crypto::{self, DeviceKeypair};
use crate::identity::DeviceFingerprint;
use crate::protocol::Message;
use crate::transport::{build_swarm, parse_addr, SyncBehaviour, SyncBehaviourEvent};
use crate::Error;

/// An append-only, blind-id-keyed encrypted-blob log with per-device cursors.
/// The server stores only ciphertext; it never sees keys, paths, or content.
pub trait BlobStore {
    /// Append a sequenced encrypted blob under `blind_id`. Idempotent on
    /// `(blind_id, seq)` — re-pushing the same sequence is a no-op so retries
    /// are safe.
    fn push(&mut self, blind_id: &str, seq: u64, ciphertext: Vec<u8>);

    /// Return all `(seq, ciphertext)` for `blind_id` with `seq > after_seq`,
    /// ascending by `seq`. This is the cursor pull.
    fn pull(&self, blind_id: &str, after_seq: u64) -> Vec<(u64, Vec<u8>)>;

    /// The highest stored `seq` for `blind_id`, or `None` if empty — a device's
    /// cursor watermark after a full pull.
    fn latest_seq(&self, blind_id: &str) -> Option<u64>;

    /// Record that `device` has caught up to `seq` for `blind_id` (per-device
    /// store-and-forward cursor). The default is a no-op — only a persistent
    /// store ([`FileBlobStore`]) needs to survive cursors across restarts; an
    /// in-memory store re-pulls from a live client's own cursor and text-merges
    /// the redundancy idempotently. [sync-zero-knowledge-server]
    fn set_cursor(&self, _device: &str, _blind_id: &str, _seq: u64) -> Result<(), Error> {
        Ok(())
    }

    /// Drop every stored blob at `blind_id` AND reset all per-device cursors
    /// against it — the server-side GC the receiver triggers after applying a
    /// `Rename` op that rotated the doc's path. Idempotent: an unknown
    /// `blind_id` is a successful no-op. [sync-rename-blob-rotation]
    fn delete(&mut self, blind_id: &str);
}

/// In-memory [`BlobStore`]: a `HashMap<blind_id, Vec<(seq, ciphertext)>>` kept
/// sorted by `seq`. Cheap and fully working; the server wave and tests use it.
#[derive(Debug, Default)]
pub struct MemBlobStore {
    logs: HashMap<String, Vec<(u64, Vec<u8>)>>,
}

impl MemBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BlobStore for MemBlobStore {
    fn push(&mut self, blind_id: &str, seq: u64, ciphertext: Vec<u8>) {
        let log = self.logs.entry(blind_id.to_string()).or_default();
        match log.binary_search_by_key(&seq, |(s, _)| *s) {
            // Already present: idempotent no-op.
            Ok(_) => {}
            Err(pos) => log.insert(pos, (seq, ciphertext)),
        }
    }

    fn pull(&self, blind_id: &str, after_seq: u64) -> Vec<(u64, Vec<u8>)> {
        let Some(log) = self.logs.get(blind_id) else {
            return Vec::new();
        };
        log.iter()
            .filter(|(s, _)| *s > after_seq)
            .cloned()
            .collect()
    }

    fn latest_seq(&self, blind_id: &str) -> Option<u64> {
        self.logs.get(blind_id).and_then(|log| log.last().map(|(s, _)| *s))
    }

    fn delete(&mut self, blind_id: &str) {
        self.logs.remove(blind_id);
    }
}

// --- file-backed store -----------------------------------------------------

/// One blob frame on disk: a `u32-le` length prefix over the frame body, where
/// the body is the `u64-le` seq followed by the ciphertext. The whole frame is
/// appended and fsynced in one shot; a crash mid-append can only leave a torn
/// *trailing* frame, which the loader tolerates by stopping at the first
/// short/undecodable frame — the same discipline as `core::oplog::store`'s
/// `append_op` / `load_ops`. [sync-zero-knowledge-server]
fn encode_frame(seq: u64, ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
    let body_len = 8 + ciphertext.len();
    let len = u32::try_from(body_len)
        .map_err(|_| Error::Transport("blob frame exceeds u32 length".to_string()))?;
    let mut frame = Vec::with_capacity(4 + body_len);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(ciphertext);
    Ok(frame)
}

/// A file-backed [`BlobStore`]: one append-only log file per blind id under a
/// data dir, plus a per-device cursor file. Holds ONLY opaque
/// `(blind_id, seq, ciphertext)` — never a content key, path, or plaintext.
///
/// On-disk layout under `<data_dir>`:
///
/// ```text
/// blobs/<blind_id>.log     append-only [u32-le len | u64-le seq | ciphertext] frames
/// cursors/<device>.cursor  per-device "<blind_id> <seq>\n" lines (highest pulled seq)
/// ```
///
/// The `blind_id` is already a 64-char hex HMAC tag, so it is a safe, opaque
/// filename — the server never derives a name from a path or key. [sync-blind-id]
#[derive(Debug)]
pub struct FileBlobStore {
    data_dir: PathBuf,
    /// In-memory mirror of each blind id's `(seq, ciphertext)` log, loaded from
    /// disk on first touch. The disk log is the source of truth; this caches it
    /// so `pull` / `latest_seq` don't re-read the file each call.
    logs: HashMap<String, Vec<(u64, Vec<u8>)>>,
}

impl FileBlobStore {
    /// Open (creating if needed) a file-backed store rooted at `data_dir`. The
    /// `blobs/` subdir's existing logs are lazily loaded per blind id on first
    /// access, tolerating a torn trailing frame.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, Error> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(data_dir.join("blobs"))
            .map_err(|e| Error::Transport(format!("create blobs dir: {e}")))?;
        fs::create_dir_all(data_dir.join("cursors"))
            .map_err(|e| Error::Transport(format!("create cursors dir: {e}")))?;
        Ok(Self {
            data_dir,
            logs: HashMap::new(),
        })
    }

    fn log_path(&self, blind_id: &str) -> PathBuf {
        self.data_dir.join("blobs").join(format!("{blind_id}.log"))
    }

    /// Load a blind id's frames from disk, stopping at the first torn/undecodable
    /// trailing frame. Missing file → empty.
    fn load_log(&self, blind_id: &str) -> Vec<(u64, Vec<u8>)> {
        let path = self.log_path(blind_id);
        let Ok(bytes) = fs::read(&path) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 4 <= bytes.len() {
            let len = u32::from_le_bytes([
                bytes[pos],
                bytes[pos + 1],
                bytes[pos + 2],
                bytes[pos + 3],
            ]) as usize;
            let start = pos + 4;
            let end = start + len;
            // Torn trailing frame, or a body too short to even hold the seq.
            if end > bytes.len() || len < 8 {
                break;
            }
            let seq = u64::from_le_bytes([
                bytes[start],
                bytes[start + 1],
                bytes[start + 2],
                bytes[start + 3],
                bytes[start + 4],
                bytes[start + 5],
                bytes[start + 6],
                bytes[start + 7],
            ]);
            let ciphertext = bytes[start + 8..end].to_vec();
            out.push((seq, ciphertext));
            pos = end;
        }
        // Frames are written in push order but a device may push out of order;
        // keep the in-memory view sorted by seq for the cursor pull.
        out.sort_by_key(|(s, _)| *s);
        out
    }

    /// The cached log for a blind id, loading from disk on first touch.
    fn log(&mut self, blind_id: &str) -> &mut Vec<(u64, Vec<u8>)> {
        if !self.logs.contains_key(blind_id) {
            let loaded = self.load_log(blind_id);
            self.logs.insert(blind_id.to_string(), loaded);
        }
        self.logs.get_mut(blind_id).expect("just inserted")
    }

    fn cursor_path(&self, device: &str) -> PathBuf {
        self.data_dir
            .join("cursors")
            .join(format!("{device}.cursor"))
    }

    /// Read a device's recorded pull cursor for `blind_id`, if any. Used by the
    /// hub to track per-device store-and-forward catch-up across restarts.
    pub fn device_cursor(&self, device: &str, blind_id: &str) -> Option<u64> {
        let path = self.cursor_path(device);
        let contents = fs::read_to_string(path).ok()?;
        for line in contents.lines() {
            if let Some((bid, seq)) = line.split_once(' ')
                && bid == blind_id
            {
                return seq.trim().parse().ok();
            }
        }
        None
    }

    /// Record that `device` has pulled up to `seq` for `blind_id`. Rewrites the
    /// device's cursor file atomically (write-temp + rename + fsync).
    pub fn set_device_cursor(
        &self,
        device: &str,
        blind_id: &str,
        seq: u64,
    ) -> Result<(), Error> {
        let path = self.cursor_path(device);
        // Read-modify-write the small per-device map.
        let mut map: HashMap<String, u64> = HashMap::new();
        if let Ok(contents) = fs::read_to_string(&path) {
            for line in contents.lines() {
                if let Some((bid, s)) = line.split_once(' ')
                    && let Ok(v) = s.trim().parse()
                {
                    map.insert(bid.to_string(), v);
                }
            }
        }
        map.insert(blind_id.to_string(), seq);
        let mut body = String::new();
        for (bid, s) in &map {
            body.push_str(&format!("{bid} {s}\n"));
        }
        let tmp = path.with_extension("cursor.tmp");
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| Error::Transport(format!("open cursor tmp: {e}")))?;
            f.write_all(body.as_bytes())
                .map_err(|e| Error::Transport(format!("write cursor: {e}")))?;
            f.sync_all()
                .map_err(|e| Error::Transport(format!("fsync cursor: {e}")))?;
        }
        fs::rename(&tmp, &path).map_err(|e| Error::Transport(format!("rename cursor: {e}")))?;
        Ok(())
    }
}

impl BlobStore for FileBlobStore {
    fn push(&mut self, blind_id: &str, seq: u64, ciphertext: Vec<u8>) {
        // Idempotent on (blind_id, seq): a re-push of a known seq is dropped, so
        // a client retry never duplicates a frame. Load-on-touch first.
        let already = {
            let log = self.log(blind_id);
            log.binary_search_by_key(&seq, |(s, _)| *s).is_ok()
        };
        if already {
            return;
        }
        // Append the frame to disk (the source of truth), then mirror in memory.
        if let Ok(frame) = encode_frame(seq, &ciphertext) {
            let path = self.log_path(blind_id);
            if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path)
                && f.write_all(&frame).and_then(|_| f.sync_all()).is_ok()
            {
                let log = self.log(blind_id);
                match log.binary_search_by_key(&seq, |(s, _)| *s) {
                    Ok(_) => {}
                    Err(pos) => log.insert(pos, (seq, ciphertext)),
                }
            }
        }
    }

    fn pull(&self, blind_id: &str, after_seq: u64) -> Vec<(u64, Vec<u8>)> {
        // The in-memory mirror is authoritative once loaded; if untouched this
        // call, fall back to a disk read so a fresh-opened store still serves.
        if let Some(log) = self.logs.get(blind_id) {
            return log
                .iter()
                .filter(|(s, _)| *s > after_seq)
                .cloned()
                .collect();
        }
        self.load_log(blind_id)
            .into_iter()
            .filter(|(s, _)| *s > after_seq)
            .collect()
    }

    fn latest_seq(&self, blind_id: &str) -> Option<u64> {
        if let Some(log) = self.logs.get(blind_id) {
            return log.last().map(|(s, _)| *s);
        }
        self.load_log(blind_id).last().map(|(s, _)| *s)
    }

    fn set_cursor(&self, device: &str, blind_id: &str, seq: u64) -> Result<(), Error> {
        self.set_device_cursor(device, blind_id, seq)
    }

    fn delete(&mut self, blind_id: &str) {
        // Drop the cached log mirror.
        self.logs.remove(blind_id);
        // Remove the on-disk log; missing-file is fine (idempotent no-op).
        let log_path = self.log_path(blind_id);
        match fs::remove_file(&log_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, blind_id, "delete blob log: {e}"),
        }
        // Strip this blind_id from every device cursor file. Walking the
        // cursors dir is acceptable — the cursors are small per-device files.
        let cursors_dir = self.data_dir.join("cursors");
        let Ok(entries) = fs::read_dir(&cursors_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("cursor") {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&path) else { continue };
            let mut map: HashMap<String, u64> = HashMap::new();
            for line in contents.lines() {
                if let Some((bid, s)) = line.split_once(' ')
                    && let Ok(v) = s.trim().parse()
                    && bid != blind_id
                {
                    map.insert(bid.to_string(), v);
                }
            }
            let mut body = String::new();
            for (bid, s) in &map {
                body.push_str(&format!("{bid} {s}\n"));
            }
            let tmp = path.with_extension("cursor.tmp");
            if let Ok(mut f) = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                && f.write_all(body.as_bytes()).and_then(|_| f.sync_all()).is_ok()
            {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }
}

// --- the hub ---------------------------------------------------------------

/// The zero-knowledge relay hub. Runs the same role-agnostic libp2p transport
/// as the peer [`crate::transport::SyncNode`] (Noise-authenticated, enrollment-
/// gated, request-response over TCP+yamux), but instead of an op log it owns a
/// [`BlobStore`]. It accepts pushes and cursor pulls from enrolled devices and
/// store-and-forwards opaque ciphertext between them. It holds NO content key
/// and never decrypts. [sync-decoupled-server, sync-zero-knowledge-server]
pub struct Hub<S: BlobStore = FileBlobStore> {
    keypair: DeviceKeypair,
    store: S,
    /// Enrolled device fingerprints, keyed by the `PeerId` each maps to, so a
    /// connection is gated in one lookup — the same gate as the peer path.
    enrolled: HashMap<PeerId, DeviceFingerprint>,
    swarm: Option<Swarm<SyncBehaviour>>,
}

impl Hub<FileBlobStore> {
    /// Construct a hub over a [`FileBlobStore`] rooted at `data_dir`, accepting
    /// the given enrolled device fingerprints. The swarm is built on first
    /// `listen`. A malformed fingerprint is skipped (logged) rather than failing
    /// construction, so one bad entry in a device file doesn't sink the hub.
    pub fn new(
        keypair: DeviceKeypair,
        data_dir: impl AsRef<Path>,
        enrolled: Vec<DeviceFingerprint>,
    ) -> Result<Self, Error> {
        let store = FileBlobStore::open(data_dir)?;
        let mut server = Self {
            keypair,
            store,
            enrolled: HashMap::new(),
            swarm: None,
        };
        for fp in enrolled {
            server.enroll_device(fp);
        }
        Ok(server)
    }
}

impl<S: BlobStore> Hub<S> {
    /// Construct a hub over an arbitrary [`BlobStore`] (e.g. a [`MemBlobStore`]
    /// for tests). Most callers use [`Hub::new`].
    pub fn with_store(
        keypair: DeviceKeypair,
        store: S,
        enrolled: Vec<DeviceFingerprint>,
    ) -> Self {
        let mut server = Self {
            keypair,
            store,
            enrolled: HashMap::new(),
            swarm: None,
        };
        for fp in enrolled {
            server.enroll_device(fp);
        }
        server
    }

    /// Enroll a device by its out-of-band-swapped fingerprint. Only connections
    /// whose authenticated `PeerId` maps back to an enrolled fingerprint are
    /// served; others are dropped. A malformed fingerprint is logged and
    /// skipped. [sync-key-swap-enrollment]
    pub fn enroll_device(&mut self, fingerprint: DeviceFingerprint) {
        match crypto::fingerprint_to_peer_id(&fingerprint) {
            Ok(peer_id) => {
                self.enrolled.insert(peer_id, fingerprint);
            }
            Err(e) => {
                tracing::warn!(fingerprint = %fingerprint.0, error = %e, "skipping invalid device fingerprint");
            }
        }
    }

    fn ensure_swarm(&mut self) -> Result<(), Error> {
        if self.swarm.is_none() {
            let kp = self.keypair.libp2p_keypair().clone();
            self.swarm = Some(build_swarm(kp)?);
        }
        Ok(())
    }

    const fn swarm_mut(&mut self) -> &mut Swarm<SyncBehaviour> {
        self.swarm
            .as_mut()
            .expect("swarm built by ensure_swarm before use")
    }

    fn is_enrolled(&self, peer: &PeerId) -> bool {
        self.enrolled.contains_key(peer)
    }

    /// Start listening on `addr`; returns the concrete bound address (with the
    /// OS-assigned port resolved from a `/tcp/0`).
    pub async fn listen(&mut self, addr: &str) -> Result<String, Error> {
        self.ensure_swarm()?;
        let addr = parse_addr(addr)?;
        self.swarm_mut()
            .listen_on(addr)
            .map_err(|e| Error::Transport(format!("listen: {e}")))?;
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

    /// Serve enrolled devices forever, store-and-forwarding ciphertext. Never
    /// returns under normal operation; a fatal transport error propagates.
    pub async fn run_forever(&mut self) -> Result<(), Error> {
        self.ensure_swarm()?;
        loop {
            let event = self.swarm_mut().select_next_some().await;
            self.handle_event(event);
        }
    }

    /// Serve for at most `window` (test driver / bounded run).
    pub async fn run(&mut self, window: Duration) -> Result<(), Error> {
        self.ensure_swarm()?;
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                _ = timeout => return Ok(()),
                event = self.swarm_mut().select_next_some() => {
                    self.handle_event(event);
                }
            }
        }
    }

    /// Handle one swarm event: enrollment-gate connections, answer pushes and
    /// cursor pulls.
    fn handle_event(&mut self, event: SwarmEvent<SyncBehaviourEvent>) {
        match event {
            SwarmEvent::ConnectionEstablished { peer_id, .. } if !self.is_enrolled(&peer_id) => {
                // A stranger authenticated by Noise but not enrolled — drop it.
                // [sync-noise-channel]
                tracing::debug!(%peer_id, "dropping connection from non-enrolled device");
                let _ = self.swarm_mut().disconnect_peer_id(peer_id);
            }
            SwarmEvent::Behaviour(SyncBehaviourEvent::Rr(request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            })) => {
                if !self.is_enrolled(&peer) {
                    return; // ignore; the channel drops, no reply.
                }
                let reply = self.handle_request(&peer, request);
                let _ = self
                    .swarm_mut()
                    .behaviour_mut()
                    .rr
                    .send_response(channel, reply);
            }
            _ => {}
        }
    }

    /// Compute the reply to one request. The hub only ever moves opaque
    /// ciphertext: it pushes an `UpdateBlob` into the store and answers a
    /// `CursorRequest` with the stored blobs past the cursor. Anything else is
    /// outside the hub's role and gets a coarse error reply (the client only
    /// drives push/pull against a server).
    fn handle_request(&mut self, peer: &PeerId, req: Message) -> Message {
        match req {
            // The hub is zero-knowledge — it never holds a content key, so it
            // reports an empty content-key fingerprint and never participates in
            // the in-band key transfer (clients drive that peer-to-peer via
            // `sync_once`, not against the hub). [sync-vault-key-inband, sync-zero-knowledge-server]
            // The hub is a relay, not a named user device, so it reports no
            // self-set device name. [sync-device-name, sync-zero-knowledge-server]
            Message::Hello { .. } => Message::HelloAck {
                device_fingerprint: self.keypair.fingerprint().0,
                content_key_fp: String::new(),
                device_name: None,
            },
            Message::UpdateBlob {
                blind_id,
                seq,
                ciphertext,
            } => {
                self.store.push(&blind_id, seq, ciphertext);
                let latest = self.store.latest_seq(&blind_id).unwrap_or(seq);
                // Record the pushing device's cursor so it doesn't re-pull its
                // own blob (best-effort; a failed write just means a redundant
                // pull later, which the text merge handles idempotently).
                let _ = self.record_cursor(peer, &blind_id, latest);
                Message::PushAck {
                    blind_id,
                    latest_seq: latest,
                }
            }
            Message::CursorRequest {
                blind_id,
                after_seq,
            } => {
                let blobs = self.store.pull(&blind_id, after_seq);
                if let Some((high, _)) = blobs.last() {
                    let _ = self.record_cursor(peer, &blind_id, *high);
                }
                Message::BlobBatch { blind_id, blobs }
            }
            // Server-side GC of the blob stream at `blind_id` after a rename
            // rotated the doc's path on the sender. Idempotent — an unknown
            // blind_id is a successful no-op. Connection is already enrollment
            // gated. [sync-rename-blob-rotation]
            Message::DeleteBlob { blind_id } => {
                self.store.delete(&blind_id);
                Message::DeleteBlobAck { blind_id }
            }
            other => {
                tracing::debug!(?other, "hub ignoring non-relay request");
                Message::PushAck {
                    blind_id: String::new(),
                    latest_seq: 0,
                }
            }
        }
    }

    /// Persist a device's cursor when the store is a [`FileBlobStore`]; a no-op
    /// for in-memory stores. Keyed by the enrolled fingerprint so it survives a
    /// `PeerId` churn across reconnects.
    fn record_cursor(&self, peer: &PeerId, blind_id: &str, seq: u64) -> Result<(), Error> {
        let device = self
            .enrolled
            .get(peer)
            .map(|fp| fp.0.clone())
            .unwrap_or_else(|| peer.to_string());
        self.store.set_cursor(&device, blind_id, seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pull_from_cursor() {
        let mut store = MemBlobStore::new();
        store.push("bid", 1, vec![1]);
        store.push("bid", 2, vec![2]);
        store.push("bid", 3, vec![3]);

        // Pull everything past cursor 1.
        let got = store.pull("bid", 1);
        assert_eq!(got, vec![(2, vec![2]), (3, vec![3])]);

        // Pull from the head.
        assert_eq!(store.pull("bid", 0).len(), 3);
        // Caught up.
        assert!(store.pull("bid", 3).is_empty());
        assert_eq!(store.latest_seq("bid"), Some(3));
    }

    #[test]
    fn out_of_order_push_is_sorted() {
        let mut store = MemBlobStore::new();
        store.push("bid", 3, vec![3]);
        store.push("bid", 1, vec![1]);
        store.push("bid", 2, vec![2]);
        let got: Vec<u64> = store.pull("bid", 0).into_iter().map(|(s, _)| s).collect();
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn push_is_idempotent_on_seq() {
        let mut store = MemBlobStore::new();
        store.push("bid", 1, vec![1]);
        store.push("bid", 1, vec![99]); // duplicate seq, ignored
        let got = store.pull("bid", 0);
        assert_eq!(got, vec![(1, vec![1])]);
    }

    #[test]
    fn unknown_blind_id_is_empty() {
        let store = MemBlobStore::new();
        assert!(store.pull("missing", 0).is_empty());
        assert_eq!(store.latest_seq("missing"), None);
    }

    #[test]
    fn file_store_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = FileBlobStore::open(dir.path()).unwrap();
            store.push("bid", 1, vec![10, 11]);
            store.push("bid", 2, vec![20, 21]);
            store.push("bid", 1, vec![99]); // duplicate seq, idempotent no-op
            assert_eq!(store.pull("bid", 1), vec![(2, vec![20, 21])]);
            assert_eq!(store.latest_seq("bid"), Some(2));
        }
        // Re-open: the append-only log replays from disk.
        let store = FileBlobStore::open(dir.path()).unwrap();
        assert_eq!(
            store.pull("bid", 0),
            vec![(1, vec![10, 11]), (2, vec![20, 21])]
        );
        assert_eq!(store.latest_seq("bid"), Some(2));
        assert!(store.pull("missing", 0).is_empty());
    }

    #[test]
    fn file_store_tolerates_torn_trailing_frame() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut store = FileBlobStore::open(dir.path()).unwrap();
            store.push("bid", 1, vec![1, 2, 3]);
            store.push("bid", 2, vec![4, 5, 6]);
        }
        // Simulate a crash mid-append: chop the tail of the log file so the last
        // frame is torn. The loader must return the intact prefix.
        let log = dir.path().join("blobs").join("bid.log");
        let bytes = std::fs::read(&log).unwrap();
        std::fs::write(&log, &bytes[..bytes.len() - 2]).unwrap();

        let store = FileBlobStore::open(dir.path()).unwrap();
        assert_eq!(store.pull("bid", 0), vec![(1, vec![1, 2, 3])]);
        assert_eq!(store.latest_seq("bid"), Some(1));
    }

    #[test]
    fn file_store_records_device_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileBlobStore::open(dir.path()).unwrap();
        assert_eq!(store.device_cursor("DEV-A", "bid"), None);
        store.set_device_cursor("DEV-A", "bid", 5).unwrap();
        store.set_device_cursor("DEV-A", "other", 9).unwrap();
        store.set_device_cursor("DEV-A", "bid", 7).unwrap(); // advances
        assert_eq!(store.device_cursor("DEV-A", "bid"), Some(7));
        assert_eq!(store.device_cursor("DEV-A", "other"), Some(9));
        assert_eq!(store.device_cursor("DEV-B", "bid"), None);
    }
}
