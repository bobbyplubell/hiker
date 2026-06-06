//! Sync service: the live `hiker-sync` engine wired into the app.
//!
//! Owns the `SyncNode` (built from this vault's `Arc<OpLog>`, a user-scope
//! device keypair + content key from [`KeyStore`], and a `Settings` mapped
//! from the vault's `[sync]` `SyncSection`). The node's swarm-driving methods
//! take `&mut self`, so the node lives behind an `Arc<tokio::Mutex<…>>` shared
//! between the background responder/discovery task and the UI-spawned
//! `force_sync`/`discover` calls. The UI thread never `.await`s the node lock;
//! all async work is spawned onto the tokio runtime.
//!
//! Human-readable progress lines are pushed onto the `sync_events` ring on
//! `VaultEvents` via an unbounded channel, mirroring the indexer-progress
//! relay shape.
//!
//! ## Content-key convergence
//!
//! Cross-device content-key agreement is automatic (`sync-vault-key-inband`):
//! each vault still generates its own key locally, but on first contact the two
//! enrolled devices compare content-key fingerprints in the Hello handshake and
//! the non-canonical side adopts the canonical device's key in-band over the
//! authenticated Noise channel — both then encrypt/decrypt deltas under one key.
//! The key lives behind a shared, persist-through [`SharedContentKey`] handle
//! held by both this service and the [`SyncNode`], so an adopted (or manually
//! imported) key updates the live node AND writes through the user-scope
//! `KeyStore`. The manual Copy/Import on the Sync page remains as a fallback.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use hiker_core::config::sections::{SyncMode as CoreSyncMode, SyncSection};
use hiker_sync::config::{Settings, SyncMode};
use hiker_sync::crypto::{ContentKey, DeviceKeypair, SharedContentKey};
use hiker_sync::identity::{BlockedDoc, DeviceFingerprint, Resolution};
use hiker_sync::transport::{EnrolledPeers, PeerCandidate, SyncNode, SyncReport};

use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// User-scope secret store for a vault's sync identity (`sync-secrets-user-scope`).
///
/// The device keypair and content key live under the platform data dir, keyed
/// by the vault's STABLE id (`core::vault::stable_id`, stored inside the vault)
/// — NEVER inside the vault itself, so a synced vault can't carry the key that
/// decrypts it. Keying by the stable id (rather than the absolute path) means
/// moving/renaming the vault directory keeps its sync identity + keys. Layout:
///
/// ```text
/// <data_dir>/hiker/sync/<vault-id>/
///   device.key    # protobuf-encoded Ed25519 keypair
///   content.key   # raw 32-byte AES-256 content key
/// ```
///
/// Both are generated on first sync-enable and loaded thereafter.
pub struct KeyStore {
    dir: PathBuf,
}

impl KeyStore {
    /// The user-scope sync-secrets directory for a vault, under the platform
    /// data dir. Falls back to a `.hiker-sync-secrets` dir in CWD only if the
    /// platform data dir can't be resolved (headless/test environments).
    pub fn dir_for_vault(vault_path: &Path) -> PathBuf {
        let base = directories::ProjectDirs::from("", "", "hiker")
            .map(|p| p.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".hiker-sync-secrets"));
        let sync_base = base.join("sync");
        // Key by the vault's stable id (lives inside the vault, survives moves
        // / renames). Fall back to a path hash only if the id can't be
        // resolved or created (e.g. a non-existent path).
        let id = hiker_core::vault::stable_id(vault_path)
            .unwrap_or_else(|_| vault_path_hash(vault_path));
        let dir = sync_base.join(&id);
        // One-time migration: a pre-vault-id install keyed the store by the
        // path hash. If that legacy dir exists and the id dir doesn't, move it
        // so the device identity + content key carry over rather than
        // regenerating (which would silently de-sync this device).
        let legacy = sync_base.join(vault_path_hash(vault_path));
        if legacy != dir && legacy.exists() && !dir.exists() {
            let _ = std::fs::rename(&legacy, &dir);
        }
        dir
    }

    /// Open (without creating) a key store at an explicit directory. Used by
    /// tests; production code uses [`KeyStore::for_vault`].
    pub const fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The key store for a vault, rooted at the user-scope data dir.
    pub fn for_vault(vault_path: &Path) -> Self {
        Self::at(Self::dir_for_vault(vault_path))
    }

    fn device_path(&self) -> PathBuf {
        self.dir.join("device.key")
    }

    fn content_path(&self) -> PathBuf {
        self.dir.join("content.key")
    }

    /// Marker sidecar for whether the content key is "established" — deliberately
    /// set (manual import) or converged in-band, vs the fresh key a brand-new
    /// device auto-generated. Its mere PRESENCE means established; absence means
    /// fresh. Lives next to `content.key` in the user-scope store, never the
    /// synced vault. [sync-content-key-confirm-on-change]
    fn established_path(&self) -> PathBuf {
        self.dir.join("content.established")
    }

    /// Whether the persisted content key is established (the marker exists).
    /// [sync-content-key-confirm-on-change]
    pub fn content_established(&self) -> bool {
        self.established_path().exists()
    }

    /// Load the device keypair, generating + persisting a fresh one on first
    /// use. The on-disk form is the libp2p protobuf encoding.
    pub fn load_or_generate_device(&self) -> std::io::Result<DeviceKeypair> {
        let path = self.device_path();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(kp) = DeviceKeypair::from_protobuf(&bytes) {
                return Ok(kp);
            }
            // Corrupt/unreadable key material: regenerate rather than wedge.
            tracing::warn!(path = %path.display(), "sync: device.key unreadable, regenerating");
        }
        let kp = DeviceKeypair::generate();
        let bytes = kp
            .to_protobuf()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::create_dir_all(&self.dir)?;
        write_secret(&path, &bytes)?;
        Ok(kp)
    }

    /// Load the vault content key, generating + persisting a fresh one on first
    /// use. The on-disk form is the raw 32 key bytes.
    ///
    /// Generated locally per vault; cross-device agreement happens at runtime via
    /// the automatic in-band transfer (`sync-vault-key-inband`), which adopts the
    /// canonical device's key and persists it back here through
    /// [`store_content`](Self::store_content). See the module-level note.
    pub fn load_or_generate_content(&self) -> std::io::Result<ContentKey> {
        let path = self.content_path();
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                return Ok(ContentKey::from_bytes(arr));
            }
            tracing::warn!(path = %path.display(), "sync: content.key wrong length, regenerating");
        }
        let key = ContentKey::generate();
        std::fs::create_dir_all(&self.dir)?;
        write_secret(&path, key.as_bytes())?;
        Ok(key)
    }

    /// Overwrite the persisted content key with `key` AND its `established` flag
    /// (the write-through both the manual import and the in-band auto-transfer
    /// route through), so an imported / adopted key + its deliberate-ness survive
    /// a restart. The key is written 0600 via [`write_secret`], same as the
    /// generated key; the established marker is a zero-byte sidecar whose presence
    /// IS the flag. [sync-content-key-confirm-on-change]
    pub fn store_content(&self, key: &ContentKey, established: bool) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_secret(&self.content_path(), key.as_bytes())?;
        let marker = self.established_path();
        if established {
            // Marker presence IS the flag; an empty file is enough.
            write_secret(&marker, &[])?;
        } else if marker.exists() {
            let _ = std::fs::remove_file(&marker);
        }
        Ok(())
    }

    /// The local-only device-alias sidecar path: a `{ fingerprint: name }` JSON
    /// map next to the key material. Aliases are NOT synced (`sync-config-section`
    /// keeps `[sync].devices` to fingerprints only) — they're a user-scope
    /// convenience label.
    fn aliases_path(&self) -> PathBuf {
        self.dir.join("aliases.json")
    }

    /// Load the device-alias map, or an empty map if absent/unreadable.
    pub fn load_aliases(&self) -> std::collections::HashMap<String, String> {
        match std::fs::read(self.aliases_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => std::collections::HashMap::new(),
        }
    }

    /// Persist the device-alias map. Not a secret (it's just labels), so a
    /// plain write; but it lives in the user-scope key-store dir, never the
    /// synced vault.
    pub fn store_aliases(
        &self,
        aliases: &std::collections::HashMap<String, String>,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let bytes = serde_json::to_vec_pretty(aliases)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(self.aliases_path(), bytes)
    }
}

/// Write a secret file with owner-only perms on unix (0o600); best-effort
/// elsewhere.
fn write_secret(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

/// Stable short hash of a vault path for the key-store subdir name. Reuses the
/// crate-wide blake3 `hash_string` (no extra dep) and truncates to 16 hex
/// chars — collision-irrelevant since it's only a per-vault directory label.
fn vault_path_hash(vault_path: &Path) -> String {
    let full = hiker_core::hash_string(&vault_path.to_string_lossy());
    full[..full.len().min(16)].to_string()
}

/// Map the vault's TOML `[sync]` section to the `hiker-sync` library config.
/// The two types are deliberately distinct (one is the on-disk TOML shape, the
/// other the lib's runtime config); this is the one-way bridge.
pub fn section_to_config(section: &SyncSection) -> Settings {
    Settings {
        enabled: section.enabled,
        mode: match section.mode {
            CoreSyncMode::Peer => SyncMode::Peer,
            CoreSyncMode::Server => SyncMode::Server,
            CoreSyncMode::Both => SyncMode::Both,
        },
        server_url: section.server_url.clone(),
        discovery: section.discovery,
        devices: section.devices.clone(),
        // This device's self-set name (carried on the handshake) and the learned
        // peer-name map (seeds the node, then updated at runtime). An empty
        // `device_name` maps to `None` (unnamed). [sync-device-name]
        device_name: Some(section.device_name.clone()).filter(|n| !n.is_empty()),
        device_names: section.device_names.clone(),
    }
}

/// Default LAN listen address when `[sync]` doesn't pin one: any interface,
/// OS-assigned TCP port.
pub const DEFAULT_LISTEN_ADDR: &str = "/ip4/0.0.0.0/tcp/0";

/// One delivered fork-diff fetch result: the forked doc's vault-relative path
/// and the peer's current text (`Ok`) or a user-facing error (`Err`). Drained
/// into the Sync page's fork-diff cache each frame. [sync-fork-diff]
pub type ForkDiffResult = (String, Result<String, String>);

/// The sender half of the fork-diff result channel — fed by the spawned
/// fetch task, drained by `main::drain_fork_diff_results`. [sync-fork-diff]
pub type ForkDiffSender = UnboundedSender<ForkDiffResult>;

/// A point-in-time view of the service for the UI header grid. Cheap to clone;
/// taken without holding the node lock across a frame.
#[derive(Clone)]
pub struct SyncSnapshot {
    pub enabled: bool,
    pub mode: SyncMode,
    pub server_url: String,
    /// Whether LAN discovery is on — the page uses this for a config-sanity
    /// warning (peer mode + discovery off can never find a peer).
    pub discovery: bool,
    pub fingerprint: String,
    /// THIS device's self-set human name (`[sync].device_name`), shown + edited
    /// on the Sync page. Empty when unnamed. [sync-device-name]
    pub device_name: String,
    pub last_sync_ms: Option<i64>,
    pub last_report: Option<SyncReport>,
    /// A surfaced last-error string for the page to render in red — set when a
    /// round failed in a user-actionable way (notably a content-key mismatch).
    /// Cleared by a subsequent successful round.
    pub last_error: Option<String>,
    /// Forked/blocked docs for the page's Conflicts section. Mirrored off the
    /// node so the render path never locks it.
    pub blocked: Vec<BlockedDoc>,
    /// The content key as bs58 (a SECRET — the page gates showing it). Cached so
    /// rendering it doesn't lock the node.
    pub content_key_b58: String,
    /// Enrolled LAN candidates currently visible via mDNS `(fingerprint, addr)`.
    /// Mirrored off the node so the page can show "discovered on LAN (enrolled)"
    /// without locking the node on render. [sync-mdns-discovery]
    pub discovered: Vec<(String, String)>,
    /// Hiker peers seen on the LAN whose fingerprint ISN'T enrolled
    /// `(peer_id, addr, fingerprint)`. Mirrored off the node for the page's
    /// "seen on LAN (not enrolled)" hint — they can't sync, but showing them
    /// tells the user a hiker instance is reachable and needs enrolling. The
    /// `fingerprint` (derived from the `PeerId` in the node) is `Some` when the
    /// page can offer a one-click enroll and `None` when it can't be derived.
    /// [sync-mdns-discovery]
    pub seen_unenrolled: Vec<(String, String, Option<String>)>,
    /// Set to a peer device fingerprint when our ESTABLISHED content key differs
    /// from that peer's and we declined to silently switch — the signal the Sync
    /// page renders a confirm action for (the manual Copy/Import is the accept
    /// path for now). `None` when no key change is held.
    /// [sync-content-key-confirm-on-change]
    pub pending_content_key_change: Option<String>,
}

impl SyncSnapshot {
    /// Count of items that NEED THE USER — drives the Sync tab attention badge
    /// and the "is anything wrong" gate. Sums blocked (forked) docs, a held
    /// content-key change, and a present surfaced last-error. Zero means
    /// healthy (no badge). status: sync-attention-badge
    pub fn attention_count(&self) -> usize {
        self.blocked.len()
            + usize::from(self.pending_content_key_change.is_some())
            + usize::from(self.last_error.is_some())
    }
}

/// Shared mutable state the spawned tasks and the UI read/update. Held behind a
/// std `Mutex` (never locked across an `.await`).
///
/// This is the ONLY thing the Sync page reads each frame (via `state_snapshot`).
/// The page must never touch the node's async mutex on the render path — the
/// responder/auto-sync task holds that lock for whole run windows / rounds, so a
/// `blocking_lock` from the egui thread would stall the UI. So node-derived
/// values the page needs (the blocked-doc list, the content-key string) are
/// MIRRORED here by the background task instead of read live.
#[derive(Default)]
struct Shared {
    last_sync_ms: Option<i64>,
    last_report: Option<SyncReport>,
    /// Discovered LAN candidates from the most recent discovery window. Used to
    /// pick dial addresses for `force_sync` on the peer path.
    discovered: Vec<PeerCandidate>,
    /// The concrete bound listen address once the responder task has it.
    listen_addr: Option<String>,
    /// A surfaced, user-actionable last-error (notably a content-key mismatch).
    /// Set when a round fails in a way the user can act on; cleared on the next
    /// round that ran cleanly.
    last_error: Option<String>,
    /// Forked/blocked docs, mirrored from the node by the responder loop so the
    /// page renders them without locking the node. [sync-blocked-state]
    blocked: Vec<BlockedDoc>,
    /// The content key as bs58, cached so the page renders it without locking the
    /// node. Set at construct and on import.
    content_key_b58: String,
    /// Learned `fingerprint -> name` map mirrored from the node after each round,
    /// so the page renders a peer's self-reported synced name without locking the
    /// node. A local `aliases.json` override wins over an entry here.
    /// [sync-device-name]
    learned_names: std::collections::HashMap<String, String>,
    /// Unenrolled hiker peers seen on the LAN `(peer_id, addr, fingerprint)`,
    /// mirrored from the node by the responder loop so the page renders them
    /// without locking the node. The `fingerprint` is the one-click-enroll
    /// target when present. [sync-mdns-discovery]
    seen_unenrolled: Vec<(String, String, Option<String>)>,
    /// The peer device fingerprint of a HELD content-key change: our established
    /// key differs from this peer's and we declined to silently switch. The Sync
    /// page renders a confirm action (for now, the manual Copy/Import is the
    /// accept path); cleared on a successful import or once keys match.
    /// [sync-content-key-confirm-on-change]
    pending_content_key_change: Option<String>,
}

/// Targets for one sync round: the server flag/url. The LAN peer list is NOT
/// snapshotted here — `run_sync_round` reads it LIVE from the node
/// (`discovered_peers()`, classified against the live enrolled set on read)
/// under the node lock it already takes, so a round kicked right after enrolling
/// a peer sees that peer immediately rather than waiting for the responder loop
/// to re-fold the lagging `Shared` snapshot. [sync-mdns-discovery]
struct RoundTargets {
    uses_server: bool,
    server_url: String,
}

/// The live sync engine for a vault session.
pub struct SyncService {
    node: Arc<tokio::sync::Mutex<SyncNode>>,
    fingerprint: String,
    config: Settings,
    /// The vault this service syncs — kept so the service can re-key its
    /// [`KeyStore`] (content-key import) and persist config (`enroll`/`unenroll`)
    /// without the page passing the path back in.
    vault_root: PathBuf,
    /// Local-only device aliases (`{ fingerprint: name }`), loaded from the
    /// key-store sidecar on construct. Not synced. Behind a `Mutex` so the UI
    /// thread can rename without `&mut self`.
    aliases: Mutex<std::collections::HashMap<String, String>>,
    /// THIS device's self-set human name (`[sync].device_name`), seeded from
    /// config at construct. Behind a `Mutex` so the page can rename via `&self`
    /// (mirrors the `aliases` pattern); the persisted config value is the source
    /// of truth the next engine build reads. [sync-device-name]
    device_name: Mutex<String>,
    /// The live enrolled-peer set, the SAME instance the [`SyncNode`] holds (an
    /// `Arc`-shared std mutex). The page's "Enrolled devices" list reads it via
    /// [`enrolled_devices`](Self::enrolled_devices), and enroll/unenroll mutate
    /// it directly — synchronously, off the node lock — so the display and the
    /// running swarm's connection-auth gate update immediately.
    enrolled: EnrolledPeers,
    /// The live content-key handle — the SAME instance the [`SyncNode`] holds.
    /// `set` updates it in place AND persists through the user-scope `KeyStore`,
    /// so both the manual import and the in-band auto-transfer (which the node
    /// runs over the authenticated channel) write to disk consistently.
    /// [sync-vault-key-inband]
    content_key: SharedContentKey,
    shared: Arc<Mutex<Shared>>,
    /// Progress-line sink drained into the `sync_events` ring each frame.
    events_tx: UnboundedSender<String>,
    /// Result sink for the on-demand "view diff" fork-content fetch
    /// (`fetch_fork_diff`): `(path, Ok(their_text) | Err(message))`, drained
    /// into the Sync page's fork-diff cache each frame (mirrors `events_tx`).
    /// [sync-fork-diff]
    fork_diff_tx: ForkDiffSender,
    /// Per-service kill switch. Cancelling it breaks the responder loop, which
    /// drops the swarm (closing the TCP listener and stopping mDNS), so a live
    /// `[sync].enabled = false` flip stops the engine immediately rather than
    /// waiting for a vault reopen. The responder loop also breaks on the
    /// session-wide cancel, so this is purely the *additional* in-session stop.
    cancel: CancellationToken,
    /// Wake signal for the debounced poke-on-commit task: each local commit
    /// calls [`notify_local_change`](Self::notify_local_change) →
    /// `notify_one()` (cheap, callable from the UI thread). The spawned task
    /// (see [`spawn_poke_task`](Self::spawn_poke_task)) coalesces a burst, then
    /// dials each discovered enrolled peer with a content-free poke so they pull
    /// promptly. [sync-poke-on-commit]
    local_change: Arc<Notify>,
}

impl SyncService {
    /// Build the service: load/generate the user-scope secrets, construct the
    /// `SyncNode`, and enroll any devices already listed in config. Does NOT
    /// start listening — the caller (bootstrap) spawns the responder task.
    pub fn new(
        vault_root: &Path,
        oplog: Arc<hiker_core::oplog::OpLog>,
        section: &SyncSection,
        events_tx: UnboundedSender<String>,
        fork_diff_tx: ForkDiffSender,
    ) -> std::io::Result<Self> {
        let store = KeyStore::for_vault(vault_root);
        let keypair = store.load_or_generate_device()?;
        let content_key = store.load_or_generate_content()?;
        // Whether the persisted key was deliberately set / converged, vs the
        // fresh one a brand-new device auto-generated. Gates in-band adoption.
        // [sync-content-key-confirm-on-change]
        let content_established = store.content_established();
        let aliases = store.load_aliases();
        let config = section_to_config(section);
        let fingerprint = keypair.fingerprint().0;
        // Cache the key string up-front (before the key moves into the node) so
        // the page can render it without ever locking the node.
        let content_key_b58 = content_key.to_b58();

        // The enrolled-peer set is shared with the node: create it here, enroll
        // the config-persisted devices into it, then hand the SAME instance to
        // the node so the responder gate, discovery, and this service's
        // enroll/unenroll/list all operate on one live map. Enrolling here
        // (before the node is built) means the validated set is populated before
        // the first connection-auth check. [sync-key-swap-enrollment]
        let enrolled = EnrolledPeers::new();
        for fp in &config.devices {
            if let Err(e) = enrolled.enroll(DeviceFingerprint(fp.clone())) {
                tracing::warn!(fingerprint = %fp, error = %e, "sync: skipping invalid enrolled device");
            }
        }

        // Build the shared content-key handle with a persist hook that writes
        // through the user-scope `KeyStore`. Both the manual Copy/Import and the
        // node's in-band auto-transfer route `set` through this one handle, so an
        // adopted key always lands on disk and is seen by both sides.
        // [sync-vault-key-inband]
        let persist_root = vault_root.to_path_buf();
        let content_key = SharedContentKey::with_persist(
            content_key,
            content_established,
            Arc::new(move |k: &ContentKey, established: bool| {
                if let Err(e) =
                    KeyStore::for_vault(&persist_root).store_content(k, established)
                {
                    tracing::warn!(error = %e, "sync: failed to persist adopted content key");
                }
            }),
        );
        let node = SyncNode::new(
            oplog,
            content_key.clone(),
            keypair,
            config.clone(),
            enrolled.clone(),
        );
        let device_name = config.device_name.clone().unwrap_or_default();

        // Config-sanity warning (`sync-config-sanity-warning`): sync is enabled
        // but the configuration can never reach a peer — peer mode with LAN
        // discovery OFF and no `server_url` — so every round would be a silent
        // no-op. Surface it once, both to the log and the events ring (the Sync
        // page renders its own version from the snapshot bits).
        if let Some(reason) = config_unreachable_warning(&config) {
            tracing::warn!(target: "hiker::sync", "{reason}");
            let _ = events_tx.send(format!("sync: {reason}"));
        }

        Ok(Self {
            node: Arc::new(tokio::sync::Mutex::new(node)),
            fingerprint,
            config,
            vault_root: vault_root.to_path_buf(),
            aliases: Mutex::new(aliases),
            device_name: Mutex::new(device_name),
            enrolled,
            content_key,
            shared: Arc::new(Mutex::new(Shared {
                content_key_b58,
                ..Default::default()
            })),
            events_tx,
            fork_diff_tx,
            cancel: CancellationToken::new(),
            local_change: Arc::new(Notify::new()),
        })
    }

    /// Stop the engine now: cancel the responder loop so the swarm (TCP
    /// listener + mDNS) is dropped. Idempotent. Called by the frame-loop
    /// kill switch when `[sync].enabled` goes false. [sync-disable-kill-switch]
    pub fn shutdown(&self) {
        self.cancel.cancel();
        self.log("sync: stopped (disabled)");
    }

    /// The service's kill-switch token, for the responder loop to also break on.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// This device's fingerprint (the string a peer must enroll to sync).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Enrolled device fingerprints — the LIVE set shared with the node, so a
    /// just-enrolled (or un-enrolled) device is reflected immediately without a
    /// service rebuild. Read off a std mutex; never touches the node lock.
    pub fn enrolled_devices(&self) -> Vec<String> {
        self.enrolled.fingerprints()
    }

    /// Whether sync is enabled for this vault.
    pub const fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Record the user's resolution decision for a forked document. The decision
    /// is consumed by the fork branch on the NEXT sync round (keep-mine offers
    /// our lineage; keep-theirs / keep-both adopt the peer's), so this kicks a
    /// round so the choice takes effect promptly. [sync-blocked-state]
    pub fn resolve_fork(
        &self,
        path: &str,
        resolution: Resolution,
        rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let verb = match resolution {
            Resolution::KeepMine => "keep mine",
            Resolution::KeepTheirs => "keep theirs",
            Resolution::KeepBoth => "keep both",
            Resolution::KeepDeleted => "keep deleted",
            Resolution::KeepEdit => "keep edit",
        };
        self.log(format!("sync: resolving conflict ({verb}) — syncing now"));
        // Spawn the node mutation off the egui thread (the responder/auto-sync
        // task can hold the node lock for a whole round). `set_fork_resolution`
        // only touches an in-node std mutex, so it's quick once the lock frees.
        let node = self.node.clone();
        let path = path.to_string();
        rt.spawn(async move {
            node.lock().await.set_fork_resolution(path, resolution);
        });
        // Kick a round so the decision is acted on without waiting for the
        // periodic tick. `force_sync` itself spawns, so the egui thread doesn't
        // block here either.
        self.force_sync(rt);
    }

    /// Fold the node's continuously-tracked enrolled LAN candidates into the
    /// shared store the round target-selection reads. Called by the responder
    /// loop after each `run` window. Replaces the LAN candidate set with the
    /// node's current live view so expired peers drop out; the manual `discover`
    /// window writes the same field, and the next responder window reconciles
    /// it back to the live set. The node lock is acquired by the caller (the
    /// `cands` are read while it holds the lock); this only touches the std
    /// `Mutex`, never across an `.await`.
    pub fn fold_discovered(&self, cands: Vec<PeerCandidate>) {
        if let Ok(mut s) = self.shared.lock() {
            s.discovered = cands;
        }
    }

    /// Mirror the node's current unenrolled-seen LAN peers into `Shared` so the
    /// page renders the "seen on LAN (not enrolled)" hint without locking the
    /// node. Called by the responder loop alongside `fold_discovered` (the
    /// caller already holds the node lock to read the list); only touches the
    /// std `Mutex`. [sync-mdns-discovery]
    pub fn fold_seen_unenrolled(&self, seen: Vec<(String, String, Option<String>)>) {
        if let Ok(mut s) = self.shared.lock() {
            s.seen_unenrolled = seen;
        }
    }

    /// Mirror the node's current blocked-doc list into `Shared` so the page can
    /// render the Conflicts section without locking the node. Called by the
    /// responder loop after each `run` window (the caller already holds the node
    /// lock to read `blocked`); only touches the std `Mutex`. [sync-blocked-state]
    pub fn fold_blocked(&self, blocked: Vec<BlockedDoc>) {
        if let Ok(mut s) = self.shared.lock() {
            s.blocked = blocked;
        }
    }

    /// The shared node handle, for the spawned responder/discovery task.
    pub fn node(&self) -> Arc<tokio::sync::Mutex<SyncNode>> {
        self.node.clone()
    }

    /// A clone of the progress-line sender, for the spawned task.
    pub fn events_tx(&self) -> UnboundedSender<String> {
        self.events_tx.clone()
    }

    fn log(&self, line: impl Into<String>) {
        let _ = self.events_tx.send(line.into());
    }

    /// Record the bound listen address (set by the responder task once known).
    pub fn set_listen_addr(&self, addr: String) {
        if let Ok(mut s) = self.shared.lock() {
            s.listen_addr = Some(addr);
        }
    }

    /// Validate a peer fingerprint, persist it into `[sync].devices`, and enroll
    /// it on the live shared set. The egui thread NEVER blocks on the node: the
    /// fingerprint is validated as part of [`EnrolledPeers::enroll`] (it
    /// recomputes the `PeerId`), the config write-back is plain file I/O, and
    /// the enroll mutates the shared std-mutex map DIRECTLY — no spawn, no node
    /// lock. Because the node holds the SAME `EnrolledPeers` instance, the
    /// running swarm's connection-auth gate sees the new device immediately, and
    /// [`enrolled_devices`](Self::enrolled_devices) reflects it on the next
    /// frame. Returns an error string on a bad fingerprint or a failed persist.
    pub fn enroll_device(
        &self,
        vault_root: &Path,
        fp: &str,
        rt: &Arc<tokio::runtime::Runtime>,
    ) -> Result<(), String> {
        let fp = fp.trim();
        if fp.is_empty() {
            return Err("empty fingerprint".to_string());
        }
        let fingerprint = DeviceFingerprint(fp.to_string());
        // Enroll into the live shared set first — this validates the fingerprint
        // (recomputes the PeerId) and inserts. On a bad fingerprint we bail
        // before persisting, so a typo never lands in config.
        self.enrolled
            .enroll(fingerprint)
            .map_err(|e| e.to_string())?;
        // Persist into config.sync.devices (skip duplicates). Plain file I/O —
        // no node lock.
        let mut devices = self.config.devices.clone();
        if !devices.iter().any(|d| d == fp) {
            devices.push(fp.to_string());
        }
        let value = serde_json::Value::Array(
            devices
                .iter()
                .map(|d| serde_json::Value::String(d.clone()))
                .collect(),
        );
        hiker_core::config::Config::set(
            hiker_core::config::SettingsScope::Vault,
            "sync.devices",
            &value,
            vault_root,
        )
        .map_err(|e| format!("persist sync.devices: {e}"))?;
        self.log(format!("sync: enrolled device {fp}"));
        // Kick a round now so enrolling a peer converges promptly instead of
        // waiting for the next ~15s auto tick. With the read-time discovery
        // classification, a peer already seen over mDNS is now a round target
        // immediately — `run_sync_round` reads `discovered_peers()` live.
        // `force_sync` spawns, so the egui thread never blocks. [sync-mdns-discovery]
        self.force_sync(rt);
        Ok(())
    }

    /// Un-enroll a device: drop it from `[sync].devices` (persisted via the same
    /// `Config::set` path `enroll_device` uses), remove it from the live shared
    /// enrolled set, and forget any local alias. The egui thread never blocks on
    /// the node: the un-enroll mutates the shared std-mutex map DIRECTLY (no
    /// spawn, no node lock) and the config + alias writes are plain file I/O.
    /// Because the node holds the same instance, the swarm's auth gate and the
    /// displayed list both update immediately. Idempotent for a device that
    /// isn't enrolled.
    pub fn unenroll_device(
        &self,
        vault_root: &Path,
        fp: &str,
        _rt: &Arc<tokio::runtime::Runtime>,
    ) -> Result<(), String> {
        let fp = fp.trim();
        if fp.is_empty() {
            return Err("empty fingerprint".to_string());
        }
        // Validate synchronously (lock-free) for immediate feedback.
        let fingerprint = DeviceFingerprint(fp.to_string());
        hiker_sync::crypto::validate_fingerprint(&fingerprint).map_err(|e| e.to_string())?;
        // Remove from the live shared set directly (std mutex, no node lock).
        self.enrolled.unenroll(&fingerprint);
        // Persist the pruned device list.
        let devices: Vec<String> = self
            .config
            .devices
            .iter()
            .filter(|d| d.as_str() != fp)
            .cloned()
            .collect();
        let value = serde_json::Value::Array(
            devices
                .iter()
                .map(|d| serde_json::Value::String(d.clone()))
                .collect(),
        );
        hiker_core::config::Config::set(
            hiker_core::config::SettingsScope::Vault,
            "sync.devices",
            &value,
            vault_root,
        )
        .map_err(|e| format!("persist sync.devices: {e}"))?;
        // Forget the alias (best-effort persist — the device is gone either way).
        if let Ok(mut map) = self.aliases.lock() {
            if map.remove(fp).is_some() {
                let _ = KeyStore::for_vault(&self.vault_root).store_aliases(&map);
            }
        }
        self.log(format!("sync: un-enrolled device {fp}"));
        Ok(())
    }

    /// Import a content key (bs58) from another of the user's own devices: parse
    /// it and set it on the shared content-key handle. This remains as a manual
    /// FALLBACK alongside the automatic in-band transfer (`sync-vault-key-inband`)
    /// — both route through the same handle, so the swap updates the live node in
    /// place AND persists through the user-scope `KeyStore` consistently. Returns
    /// a friendly error on a malformed key.
    pub fn import_content_key(
        &self,
        b58: &str,
        _rt: &Arc<tokio::runtime::Runtime>,
    ) -> Result<(), String> {
        // Parse + validate synchronously (lock-free) so a malformed key errors
        // immediately.
        let key = ContentKey::from_b58(b58).map_err(|e| e.to_string())?;
        // Route through the shared handle: updates the live node's key in place,
        // marks it ESTABLISHED (a manual import is a deliberate choice — and the
        // accept-the-change path for a held convergence), AND persists it via the
        // KeyStore hook. The node holds the SAME handle, so no node-lock spawn is
        // needed — the egui thread never blocks.
        // [sync-content-key-confirm-on-change]
        self.content_key.adopt(key.clone());
        // Refresh the cached string the page renders, and clear any held
        // pending-key-change now that the user has explicitly chosen a key.
        if let Ok(mut s) = self.shared.lock() {
            s.content_key_b58 = key.to_b58();
            s.pending_content_key_change = None;
        }
        self.log("sync: imported content key (devices can now decrypt each other)");
        Ok(())
    }

    /// THIS device's self-set name (`[sync].device_name`), empty when unnamed.
    /// Read off the live `Mutex` (updated by [`set_device_name`](Self::set_device_name)),
    /// never the node lock. [sync-device-name]
    pub fn device_name(&self) -> String {
        self.device_name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// Set (or clear, when `name` is empty) THIS device's self-set name and
    /// persist it to `[sync].device_name` (vault scope), so it's carried on the
    /// next handshake and survives a restart. Updates the live `Mutex` so the
    /// snapshot reflects it immediately. The running node reads the name from
    /// its own config at construct; a change takes effect for the engine on the
    /// next reconcile/rebuild — the persisted value is the source of truth
    /// either way. Plain file I/O via `&self`; never touches the node lock.
    /// [sync-device-name]
    pub fn set_device_name(&self, vault_root: &Path, name: &str) -> Result<(), String> {
        let name = name.trim();
        hiker_core::config::Config::set(
            hiker_core::config::SettingsScope::Vault,
            "sync.device_name",
            &serde_json::Value::String(name.to_string()),
            vault_root,
        )
        .map_err(|e| format!("persist sync.device_name: {e}"))?;
        if let Ok(mut n) = self.device_name.lock() {
            *n = name.to_string();
        }
        self.log(format!("sync: this device is now named \"{name}\""));
        Ok(())
    }

    /// The learned synced name for a peer fingerprint, if one was reported in a
    /// handshake (read off the mirrored `Shared` map — never the node lock).
    /// This is the synced name; a local `aliases.json` override wins over it in
    /// [`display_name`](Self::display_name). [sync-device-name]
    pub fn learned_name(&self, fp: &str) -> Option<String> {
        self.shared
            .lock()
            .ok()
            .and_then(|s| s.learned_names.get(fp).cloned())
    }

    /// The display name for a peer fingerprint, honoring the precedence the spec
    /// defines: a local `aliases.json` override wins; else the learned synced
    /// name; else `None` (the caller falls back to a truncated fingerprint).
    /// [sync-device-name]
    pub fn display_name(&self, fp: &str) -> Option<String> {
        self.device_alias(fp).or_else(|| self.learned_name(fp))
    }

    /// Mirror the node's learned `fingerprint -> name` map into `Shared` so the
    /// page renders peer names without locking the node. Called by the responder
    /// loop alongside the other folds, and after a `force_sync`/`connect_to`
    /// round. Only touches the std `Mutex`. [sync-device-name]
    pub fn fold_learned_names(&self, names: std::collections::HashMap<String, String>) {
        if let Ok(mut s) = self.shared.lock() {
            s.learned_names = names;
        }
    }

    /// The local alias for a device fingerprint, if one was set.
    pub fn device_alias(&self, fp: &str) -> Option<String> {
        self.aliases.lock().ok().and_then(|m| m.get(fp).cloned())
    }

    /// Set (or clear, when `name` is empty) a local alias for a device, and
    /// persist the sidecar. Local-only — never enters the synced config.
    pub fn set_alias(&self, fp: &str, name: &str) {
        let fp = fp.trim();
        if fp.is_empty() {
            return;
        }
        if let Ok(mut map) = self.aliases.lock() {
            let name = name.trim();
            if name.is_empty() {
                map.remove(fp);
            } else {
                map.insert(fp.to_string(), name.to_string());
            }
            let _ = KeyStore::for_vault(&self.vault_root).store_aliases(&map);
        }
    }

    /// Manual peer fallback: dial an explicit multiaddr and run one sync round
    /// against it, spawned on the runtime. The peer must already be enrolled —
    /// the transport's auth gate drops a connection from an un-enrolled peer.
    /// This is the escape hatch when mDNS finds nothing. Result/errors land in
    /// the events log.
    pub fn connect_to(&self, addr: &str, rt: &Arc<tokio::runtime::Runtime>) {
        let node = self.node.clone();
        let shared = self.shared.clone();
        let events_tx = self.events_tx.clone();
        let content_key = self.content_key.clone();
        let addr = addr.trim().to_string();
        if addr.is_empty() {
            let _ = events_tx.send("sync: enter a peer address first".to_string());
            return;
        }
        rt.spawn(async move {
            let _ = events_tx.send(format!("sync: dialing peer {addr}"));
            let mut node = node.lock().await;
            if let Err(e) = node.dial(&addr).await {
                let _ = events_tx.send(format!("sync: dial error — {e}"));
                return;
            }
            match node.sync_once(&addr).await {
                Ok(report) => {
                    let _ = events_tx.send(format!(
                        "sync: done — {} bound, {} converged, {} blocked",
                        report.bound.len(),
                        report.converged.len(),
                        report.blocked.len()
                    ));
                    for (path, reason) in &report.blocked {
                        let _ = events_tx.send(format!("sync: conflict — {path} ({reason})"));
                    }
                    // Surface a content-key adoption or a held (pending) key
                    // change. [sync-content-key-confirm-on-change]
                    if let Some(peer) = &report.adopted_content_key_from {
                        let _ = events_tx.send(format!("sync: adopted {peer}'s vault content key"));
                    }
                    if let Some(peer) = &report.pending_content_key_change {
                        tracing::warn!(peer = %peer, "sync: peer uses a different content key — confirm to adopt");
                        let _ = events_tx.send(format!(
                            "sync: peer {peer} uses a different content key — confirm to adopt it (Sync page), or Copy/Import to match"
                        ));
                    }
                    let pending = report.pending_content_key_change.clone();
                    // Mirror the node's learned peer names from this round's
                    // handshake (read off the guard we still hold). [sync-device-name]
                    let learned = node.learned_device_names();
                    if let Ok(mut s) = shared.lock() {
                        s.last_sync_ms = Some(now_ms());
                        s.last_report = Some(report);
                        s.last_error = None;
                        // Reflect any in-band content-key adoption. [sync-vault-key-inband]
                        s.content_key_b58 = content_key.get().to_b58();
                        s.learned_names = learned;
                        s.pending_content_key_change = pending;
                    }
                }
                Err(e) => {
                    let line = friendly_round_error(&e.to_string());
                    let _ = events_tx.send(format!("sync: error — {line}"));
                    if let Ok(mut s) = shared.lock() {
                        s.last_error = Some(line);
                    }
                }
            }
        });
    }

    /// Fetch a forked document's CURRENT text from the peer it forked against,
    /// so the Sync page can show a read-only "view diff" before the user
    /// resolves the fork. Resolves the peer's live addr from the discovered set
    /// (by fingerprint, read off the cheap `Shared` snapshot — no node lock on
    /// the UI thread), then spawns the dial + `fetch_doc_text` on the runtime.
    /// The result `(path, Ok(their_text) | Err(message))` rides back to the UI
    /// via the fork-diff channel, mirroring the `sync_events` relay. A peer that
    /// isn't currently discovered yields a clear "not reachable" error rather
    /// than a hang — it must be online for a fork to be diffable. The fetch is
    /// read-only: it never binds, adopts, or changes sync state. [sync-fork-diff]
    pub fn fetch_fork_diff(
        &self,
        path: &str,
        peer_fingerprint: &str,
        rt: &Arc<tokio::runtime::Runtime>,
    ) {
        let path = path.to_string();
        // Resolve the peer's live LAN address by fingerprint off the mirrored
        // discovered set (never the node lock on the UI thread).
        let addr = self.shared.lock().ok().and_then(|s| {
            s.discovered
                .iter()
                .find(|c| c.fingerprint.0 == peer_fingerprint)
                .map(|c| c.addr.clone())
        });
        let Some(addr) = addr else {
            // Not reachable: deliver an actionable error rather than hanging.
            let _ = self.fork_diff_tx.send((
                path,
                Err("peer not reachable — it must be online (discovered on the \
                     LAN) to diff this fork"
                    .to_string()),
            ));
            return;
        };

        let node = self.node.clone();
        let fork_diff_tx = self.fork_diff_tx.clone();
        let events_tx = self.events_tx.clone();
        rt.spawn(async move {
            let _ = events_tx.send(format!("sync: fetching peer's version of {path} for diff"));
            let result = node.lock().await.fetch_doc_text(&addr, &path).await;
            let payload = match result {
                Ok(text) => Ok(text),
                Err(e) => Err(friendly_round_error(&e.to_string())),
            };
            let _ = fork_diff_tx.send((path, payload));
        });
    }

    /// A point-in-time snapshot for the UI header grid. All fields come from the
    /// cheap `Shared` std-mutex snapshot — the render path NEVER locks the node.
    pub fn state_snapshot(&self) -> SyncSnapshot {
        let (
            last_sync_ms,
            last_report,
            last_error,
            blocked,
            content_key_b58,
            discovered,
            seen_unenrolled,
            pending_content_key_change,
        ) = self
            .shared
            .lock()
            .map(|s| {
                (
                    s.last_sync_ms,
                    s.last_report.clone(),
                    s.last_error.clone(),
                    s.blocked.clone(),
                    s.content_key_b58.clone(),
                    s.discovered
                        .iter()
                        .map(|c| (c.fingerprint.0.clone(), c.addr.clone()))
                        .collect(),
                    s.seen_unenrolled.clone(),
                    s.pending_content_key_change.clone(),
                )
            })
            .unwrap_or((None, None, None, Vec::new(), String::new(), Vec::new(), Vec::new(), None));
        SyncSnapshot {
            enabled: self.enabled(),
            mode: self.config.mode,
            server_url: self.config.server_url.clone(),
            discovery: self.config.discovery,
            fingerprint: self.fingerprint().to_string(),
            device_name: self.device_name(),
            last_sync_ms,
            last_report,
            last_error,
            blocked,
            content_key_b58,
            discovered,
            seen_unenrolled,
            pending_content_key_change,
        }
    }

    /// Target selection for one sync round: server (`sync_via_server`) when mode
    /// is Server/Both with a `server_url`. The LAN peer list is read live inside
    /// `run_sync_round` (not here), so it reflects the enrolled set at round time.
    /// Read off config only — never touches the node lock.
    fn round_targets(&self) -> RoundTargets {
        let server_url = self.config.server_url.clone();
        let uses_server = matches!(self.config.mode, SyncMode::Server | SyncMode::Both)
            && !server_url.is_empty();
        RoundTargets {
            uses_server,
            server_url,
        }
    }

    /// The shared core of every sync round — the button (`force_sync`) and the
    /// auto-driver both call this so behavior stays identical. Acquires the node
    /// lock once, runs the server path and/or dials each known enrolled LAN peer
    /// via `sync_once`, and folds the per-peer reports. On the LAN path with no
    /// known peers it returns `Ok(None)` (a benign no-op, not an error) so a
    /// periodic round with nothing to do stays quiet.
    ///
    /// `Ok(Some(report))` is a round that ran; `Ok(None)` is "nothing to do";
    /// `Err` is a real failure. The caller decides what to log.
    async fn run_sync_round(
        node: &Arc<tokio::sync::Mutex<SyncNode>>,
        targets: RoundTargets,
    ) -> Result<Option<SyncReport>, String> {
        if targets.uses_server {
            let mut node = node.lock().await;
            return node
                .sync_via_server(&targets.server_url)
                .await
                .map(Some)
                .map_err(|e| e.to_string());
        }
        // LAN path: read the live enrolled-discovered peer list under the node
        // lock we're about to hold for the round, so a round kicked right after
        // an enroll sees the just-enrolled peer (the node classifies discovery
        // against the live enrolled set on read — no waiting for the responder
        // loop to re-fold the `Shared` snapshot). [sync-mdns-discovery]
        let mut node = node.lock().await;
        let peers: Vec<String> = node
            .discovered_peers()
            .into_iter()
            .map(|c| c.addr)
            .collect();
        if peers.is_empty() {
            return Ok(None);
        }
        // Sync against each known enrolled peer in turn, folding reports. A peer
        // whose round returns `Err` must not be silently dropped on a
        // partial-success round: fold `(peer_addr, reason)` into the report's
        // `errored` so the failure survives for later UI surfacing, in addition
        // to keeping the `last_err` used for the all-peers-failed return below.
        // status: bug-sync-partial-failure-swallowed
        let mut folded = SyncReport::default();
        let mut last_err: Option<String> = None;
        for addr in &peers {
            match node.sync_once(addr).await {
                Ok(r) => {
                    folded.bound.extend(r.bound);
                    folded.converged.extend(r.converged);
                    folded.blocked.extend(r.blocked);
                    folded.errored.extend(r.errored);
                }
                Err(e) => {
                    let reason = e.to_string();
                    folded.errored.push((addr.clone(), reason.clone()));
                    last_err = Some(reason);
                }
            }
        }
        match last_err {
            Some(e) if folded.bound.is_empty() && folded.converged.is_empty() => Err(e),
            _ => Ok(Some(folded)),
        }
    }

    /// Run one auto-sync round inline (the caller is the responder loop, which
    /// already interleaves node-lock turns). Quiet by design: a no-op round
    /// (`Ok(None)`) or a round that bound/converged/blocked nothing emits no
    /// progress line, so periodic ticks don't spam the log. Only a round that
    /// actually did something — or an error — speaks. Records `last_sync_ms`/
    /// `last_report` whenever a round ran so the UI's "last sync" stays fresh.
    pub async fn auto_sync_round(&self) {
        let targets = self.round_targets();
        match Self::run_sync_round(&self.node, targets).await {
            Ok(Some(report)) => {
                let did_something = !report.bound.is_empty()
                    || !report.converged.is_empty()
                    || !report.blocked.is_empty()
                    || !report.errored.is_empty();
                if did_something {
                    self.report_line(&report);
                }
                // Surface a content-key adoption or a held (pending) key change
                // before the report is moved into the snapshot.
                // [sync-content-key-confirm-on-change]
                self.surface_content_key_outcome(&report);
                self.record_report(report);
                self.clear_last_error();
                // An in-band content-key adoption may have happened on the node;
                // reflect it in the page's cached key string. [sync-vault-key-inband]
                self.refresh_content_key_cache();
                // A peer may have reported its name in this round's handshake;
                // mirror the node's learned map for the page. [sync-device-name]
                let learned = self.node.lock().await.learned_device_names();
                self.fold_learned_names(learned);
            }
            Ok(None) => {}
            Err(e) => {
                let line = friendly_round_error(&e);
                self.log(format!("sync: error — {line}"));
                self.set_last_error(line);
            }
        }
    }

    /// Signal that a document was just committed (saved) on THIS device, so the
    /// debounced poke task wakes and nudges enrolled peers to pull promptly
    /// rather than waiting for their own ~15s poll tick. Cheap and non-blocking
    /// — a single `notify_one()` — so it's safe to call straight from the UI
    /// thread at every commit site. A no-op when no poke task is running (sync
    /// disabled) or when there are no enrolled peers (the task finds none to
    /// dial). [sync-poke-on-commit]
    // status: sync-poke-on-commit
    pub fn notify_local_change(&self) {
        self.local_change.notify_one();
    }

    /// Spawn the debounced poke-on-commit task (the SENDING side of
    /// `sync-poke-on-commit`). It loops on the `local_change` notify, coalesces
    /// a burst of commits with a short sleep, then dials each discovered
    /// enrolled peer with a content-free [`SyncNode::poke`] so they run their
    /// existing pull path now. Failures are logged and non-fatal; with no peers
    /// it's a quiet no-op. Respects the per-service cancel token (the kill
    /// switch / vault swap stops it with the rest of the engine). Called by
    /// bootstrap right after the service is built, on the tokio runtime.
    // status: sync-poke-on-commit
    pub fn spawn_poke_task(&self) {
        // Gate on sync being enabled — a disabled service never builds a swarm
        // or listens, so there's nothing to poke from. (bootstrap also only
        // builds the service when enabled, but this keeps the task honest if
        // called in another context.)
        if !self.config.enabled {
            return;
        }
        let node = self.node.clone();
        let notify = self.local_change.clone();
        let cancel = self.cancel.clone();
        let events_tx = self.events_tx.clone();
        // Coalesce a burst of rapid saves into one poke round.
        const DEBOUNCE: Duration = Duration::from_millis(300);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    () = notify.notified() => {
                        // Coalesce: a burst of commits within the window collapses
                        // into one poke round. Don't hold the node lock across
                        // this sleep — only lock it to dial, below.
                        tokio::time::sleep(DEBOUNCE).await;
                        // Snapshot the enrolled-discovered peers under one lock
                        // turn, then dial each. The node lock here is the same
                        // contention the auto-sync dialer already has with the
                        // responder window — fine. [sync-poke-on-commit]
                        let mut node = node.lock().await;
                        let peers: Vec<String> = node
                            .discovered_peers()
                            .into_iter()
                            .map(|c| c.addr)
                            .collect();
                        for addr in peers {
                            if let Err(e) = node.poke(&addr).await {
                                tracing::warn!(peer = %addr, error = %e, "sync: poke failed");
                                let _ = events_tx.send(format!("sync: poke {addr} failed — {e}"));
                            }
                        }
                    }
                }
            }
        });
    }

    /// Refresh the page's cached content-key string from the live shared handle.
    /// Called after a round so an in-band auto-transfer (`sync-vault-key-inband`)
    /// — which adopts the canonical device's key on the node side — is reflected
    /// in what the Sync page shows. Cheap: reads the shared handle, never the
    /// node lock.
    fn refresh_content_key_cache(&self) {
        let b58 = self.content_key.get().to_b58();
        if let Ok(mut s) = self.shared.lock() {
            s.content_key_b58 = b58;
        }
    }

    /// Surface a round's content-key convergence outcome to the user: emit a
    /// progress line for a fresh-key adoption, or — when an ESTABLISHED key
    /// declined to switch — emit a warning AND record the held change as a
    /// pending-key-change snapshot bit the Sync page can render a confirm action
    /// for (the manual Copy/Import is the accept path for now). A round that
    /// neither adopted nor held leaves any prior pending change in place (it
    /// clears on a successful import or once the keys match — i.e. an
    /// `Unchanged` outcome). [sync-content-key-confirm-on-change]
    fn surface_content_key_outcome(&self, report: &SyncReport) {
        if let Some(peer) = &report.adopted_content_key_from {
            self.log(format!("sync: adopted {peer}'s vault content key"));
        }
        if let Some(peer) = &report.pending_content_key_change {
            tracing::warn!(peer = %peer, "sync: peer uses a different content key — confirm to adopt");
            self.log(format!(
                "sync: peer {peer} uses a different content key — confirm to adopt it (Sync page), or Copy/Import to match"
            ));
            if let Ok(mut s) = self.shared.lock() {
                s.pending_content_key_change = Some(peer.clone());
            }
        } else {
            // The keys matched (or we adopted) — no change is being held this
            // round, so clear any stale pending marker.
            if let Ok(mut s) = self.shared.lock() {
                s.pending_content_key_change = None;
            }
        }
    }

    /// Stash a surfaced last-error for the page to render in red.
    fn set_last_error(&self, line: String) {
        if let Ok(mut s) = self.shared.lock() {
            s.last_error = Some(line);
        }
    }

    /// Clear the surfaced last-error after a clean round.
    fn clear_last_error(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.last_error = None;
        }
    }

    /// Push the human-readable summary lines for a round that did something.
    fn report_line(&self, report: &SyncReport) {
        self.log(format!(
            "sync: done — {} bound, {} converged, {} blocked",
            report.bound.len(),
            report.converged.len(),
            report.blocked.len()
        ));
        for (path, reason) in &report.blocked {
            self.log(format!("sync: conflict — {path} ({reason})"));
        }
        // Per-doc / per-peer failures that did NOT abort the round — surface
        // each so a partial failure isn't silent. [bug-sync-partial-failure-swallowed]
        for (label, reason) in &report.errored {
            self.log(format!("sync: skipped — {label} ({reason})"));
        }
    }

    /// Stash a completed round's report + timestamp for the UI header.
    fn record_report(&self, report: SyncReport) {
        if let Ok(mut s) = self.shared.lock() {
            s.last_sync_ms = Some(now_ms());
            s.last_report = Some(report);
        }
    }

    /// Kick off a sync pass on the tokio runtime (does not block the UI). The
    /// manual "Sync now" button. If a `server_url` is configured →
    /// `sync_via_server`. Otherwise dial each known enrolled LAN peer via
    /// `sync_once`. Unlike the silent auto-driver, the button always speaks:
    /// it announces the kickoff and reports a no-op as the "run Discover first"
    /// hint, so a manual click never looks like it did nothing. Progress +
    /// result land in the events ring and `last_report`/`last_sync_ms`.
    pub fn force_sync(&self, rt: &Arc<tokio::runtime::Runtime>) {
        let node = self.node.clone();
        let shared = self.shared.clone();
        let events_tx = self.events_tx.clone();
        let content_key = self.content_key.clone();
        let targets = self.round_targets();

        rt.spawn(async move {
            let _ = events_tx.send("sync: starting".to_string());
            match Self::run_sync_round(&node, targets).await {
                Ok(Some(report)) => {
                    let _ = events_tx.send(format!(
                        "sync: done — {} bound, {} converged, {} blocked",
                        report.bound.len(),
                        report.converged.len(),
                        report.blocked.len()
                    ));
                    for (path, reason) in &report.blocked {
                        let _ = events_tx.send(format!("sync: conflict — {path} ({reason})"));
                    }
                    // Mirror the node's learned peer names (a peer reported its
                    // name in this round's handshake). Brief node lock, off the
                    // UI thread. [sync-device-name]
                    let learned = node.lock().await.learned_device_names();
                    if let Ok(mut s) = shared.lock() {
                        s.last_sync_ms = Some(now_ms());
                        s.last_report = Some(report);
                        s.last_error = None;
                        // Reflect any in-band content-key adoption that ran on the
                        // node this round. [sync-vault-key-inband]
                        s.content_key_b58 = content_key.get().to_b58();
                        s.learned_names = learned;
                    }
                }
                Ok(None) => {
                    let _ = events_tx
                        .send("sync: no known LAN peers — run Discover first".to_string());
                }
                Err(e) => {
                    let line = friendly_round_error(&e);
                    let _ = events_tx.send(format!("sync: error — {line}"));
                    if let Ok(mut s) = shared.lock() {
                        s.last_error = Some(line);
                    }
                }
            }
        });
    }

    /// Run the manual, time-boxed mDNS discovery window on the runtime. Found
    /// candidates are stored for the next `force_sync` LAN pass.
    pub fn discover(&self, window: Duration, rt: &Arc<tokio::runtime::Runtime>) {
        let node = self.node.clone();
        let shared = self.shared.clone();
        let events_tx = self.events_tx.clone();
        rt.spawn(async move {
            let _ = events_tx.send(format!(
                "sync: discovering for {}s",
                window.as_secs()
            ));
            let found = {
                let mut node = node.lock().await;
                node.start_discovery(window).await
            };
            match found {
                Ok(cands) => {
                    let _ = events_tx.send(format!(
                        "sync: discovery found {} enrolled peer(s)",
                        cands.len()
                    ));
                    for c in &cands {
                        let _ = events_tx
                            .send(format!("sync: peer {} at {}", c.fingerprint.0, c.addr));
                    }
                    if let Ok(mut s) = shared.lock() {
                        s.discovered = cands;
                    }
                }
                Err(e) => {
                    let _ = events_tx.send(format!("sync: discovery error — {e}"));
                }
            }
        });
    }
}

/// Turn a raw round-error string into a user-actionable message. The #1
/// silent-failure trap is two devices with different content keys: a P2P round
/// reaches the peer but the content layer can't decrypt. `hiker-sync`'s
/// `Error::Decrypt` renders as "content decryption failed …"; detect that
/// substring and point the user at Copy/Import content key.
fn friendly_round_error(e: &str) -> String {
    let low = e.to_ascii_lowercase();
    if low.contains("decryption failed") || low.contains("decrypt") {
        "reached peer but content didn't decrypt — the devices likely have \
         different content keys (use Copy/Import content key)"
            .to_string()
    } else {
        e.to_string()
    }
}

/// A one-time config-sanity warning when sync is enabled but the configuration
/// can never reach a peer: peer mode with LAN discovery OFF and no `server_url`,
/// so discovery never browses and there is no server to relay — every round is
/// a silent no-op. Returns `None` when the config has a plausible path to a peer
/// (discovery on, OR a server configured, OR sync disabled). The Sync page
/// renders its own equivalent from the snapshot; this drives the start-time
/// log and events line for `sync-config-sanity-warning`, so a headless or
/// page-never-opened session still warns.
fn config_unreachable_warning(config: &Settings) -> Option<String> {
    if !config.enabled {
        return None;
    }
    let has_server = matches!(config.mode, SyncMode::Server | SyncMode::Both)
        && !config.server_url.is_empty();
    if config.discovery || has_server {
        return None;
    }
    Some(
        "enabled but discovery is off and no server is configured — no peer will \
         ever be found"
            .to_string(),
    )
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}


#[cfg(test)]
mod tests;
