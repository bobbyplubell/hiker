# Sync

Multi-device sync of a vault's op log (`op-log.md`) across a user's own devices. The op log is the substrate; this doc specs the layer on top — device identity, enrollment, the encrypted transport, and the server. Yrs CRDT merge handles concurrent edits; this layer moves the updates and decides which replicas are the same document.

The headline decisions:

- **Transport-negotiated document identity.** Each device keeps minting its own local ULID `doc_id`; the transport binds local ids to a shared logical lineage, so devices never agree on an id string up front and identity survives renames. [sync-negotiated-doc-ids]
- **Two modes, one protocol.** Direct peer-to-peer on a LAN, or through a decoupled server that runs standalone or in-process alongside the app — the same wire protocol either way, only the topology differs. [sync-p2p-lan, sync-decoupled-server]
- **Two encryption layers; the server is zero-knowledge.** A Noise channel authenticates endpoints and secures the hop; a client-side AES-256-GCM layer encrypts content, so a relay server only ever stores ciphertext and never holds the vault key. [sync-noise-channel, sync-content-encryption-aes256, sync-zero-knowledge-server]
- **libp2p for transport, with the P2P footguns compiled out.** `default-features = false` plus a minimal feature set; `cargo-deny` bans the DHT / hole-punching / relay crates so they never enter the binary. [sync-libp2p-transport, sync-banned-p2p-features]
- **Manual bidirectional key swap to enroll.** Devices exchange public-key fingerprints out of band (Syncthing-Device-ID style); the vault content key transfers in-band after mutual auth. [sync-key-swap-enrollment, sync-vault-key-inband]
- **Enrollment classifies before it merges.** First contact compares content hashes against the `.ops` version history: identical binds, a clean fast-forward auto-adopts, a true fork is Blocked until the user picks a side. [sync-enrollment-hash-classification, sync-blocked-state]


## Identity

A device never agrees on a shared `doc_id` string. Each keeps its local ULID `doc_id` (the `.yrs` / `.pending` / `.ops` filename, the `op_metadata.doc_id` key); the transport maintains a binding from each device's local id to a shared **logical id** and establishes one shared Yrs lineage behind it. [sync-negotiated-doc-ids]

Path is the **one-time matching key**, not the identity. At first contact two unbound documents bind when their vault-relative paths match; after binding, identity is the logical id and a rename (a `meta.path` change) never re-opens the question. This is what lets reorganization (`op-log-reorg-batch`) move notes freely without severing sync identity. [sync-path-matching-key]

Two independently-seeded Yrs Docs do not merge — identical bytes interleave, because the lineages share no history. So a newly-bound device **adopts the canonical lineage** rather than applying the peer's update onto its own Doc: [sync-lineage-adoption]

1. Take the canonical replica's base (`encode_state_as_update_v2` of its Doc) as the Doc for this logical id.
2. Re-apply the local-only divergence as one edit on the shared lineage via the existing external-edit reconciliation (`op-log-external-edit-sync`): diff canonical text → local text, apply as `user` ops.

The adopting device's pre-binding op history (local-only, never synced) collapses into that one reconciliation op.

### Enrollment-time classification

A fork must not be auto-merged: a positional CRDT merge of two genuinely divergent texts interleaves into nonsense (the reason `op-log-merge-conflict` exists). At binding there is no shared lineage to trust, so divergence is classified from the **content-hash history** before any adoption. [sync-enrollment-hash-classification]

| Condition | Meaning | Action |
| --- | --- | --- |
| current hashes equal | identical content | bind; canonical chosen by deterministic rule; no reconcile |
| peer's current hash ∈ our `.ops` history | peer is a prior version of our lineage | fast-forward: peer adopts, no prompt, no loss |
| our current hash ∈ peer's history | we are behind | symmetric fast-forward |
| neither current ∈ the other's history, or both | true fork (or ambiguous) | Blocked |

A hash in history means "content was once identical," not strict ancestry — a revert can recreate an old hash, so a both-directions match is ambiguous and escalates rather than guesses. Hashes are blake3 over `materialize(accepted)`.

The history-hash set comes from a `content_hash` column on `op_metadata` (blake3 of the materialized content as of each accepted op), so "is hash X in this doc's history" is one indexed query. [sync-content-hash-column]

### Blocked documents

A true fork sets the document's sync status to **Blocked**: no updates stream in either direction for that document until the user resolves it. The block is per-document — the rest of the vault keeps syncing. [sync-blocked-state]

Resolution verbs reuse the `drift-conflict-modal` shape:

- **Keep mine** — my side is canonical and the resolution converges BOTH devices in one click: the resolver pushes its canonical Yrs base (`PushAdopt`) and the peer adopts it, discarding its own divergence (that is what "keep mine" means). Because the peer adopts the resolver's exact base, both sides land on one shared lineage, so subsequent deltas are safe. If both devices set keep-mine, whoever pushes first wins and clears the other's pending decision, so it converges to one version with no flapping.
- **Keep theirs** — symmetric: the resolver adopts the peer's lineage, discarding its own divergence.
- **Keep both** — one side canonical; the other lands as a sibling conflict-copy note (its own fresh logical id, indexable like any note), then the original path adopts the peer's content.

Before resolving, the user can **View diff**: a forked document holds the local content but not the peer's (forks are detected from content hashes, so the peer's body was never fetched), so the peer's current text is fetched on demand over the authenticated channel and shown as a read-only unified diff against the local version. The fetch never mutates the local doc or changes sync state; the peer must be online (discovered on the LAN) to diff. [sync-fork-diff]

On resolution the document flips to bound and streaming begins.


## Enrollment and keys

Two secrets: a per-device static keypair (channel auth) and a per-vault 256-bit content key (content encryption, shared by all enrolled devices).

Pairing is a **manual bidirectional swap of device public-key fingerprints** — each device shows a short checksummed fingerprint (copy-paste or QR, the Syncthing Device ID model); the user enters A's into B and B's into A. The out-of-band swap is what authenticates the Noise channel, so there is no MITM window. [sync-key-swap-enrollment]

The high-entropy **vault content key is never typed or displayed**. Each device generates its own per-vault key, so two devices start with different keys; they converge automatically. In the `Hello` handshake each side carries a **non-secret content-key fingerprint** (a truncated blake3 of the key bytes — preimage-resistant, so it never reveals the key). If the fingerprints match, both already share a key and nothing transfers. If they differ, a deterministic owner is picked (`canonical = min(device fingerprint)`); the non-canonical side requests the canonical device's key and adopts it in-band, before any document deltas, so all content-encrypted blobs and blind ids then match. The 32 raw key bytes ride the already-encrypted, mutually-authenticated Noise channel to a verified-enrolled peer — enrollment is the consent (the SSH/Signal model) — and are never logged or written into the synced vault. The manual Copy/Import of the key on the Sync page remains as a fallback. [sync-vault-key-inband]

Both secrets live **user-scope, never in the synced vault** — the same rule as `[llm].api_key`, so a synced vault can't carry the key that decrypts it. Stored in the platform data dir, keyed by the vault's **stable id** (`core::vault::vault_id`, a ULID at `.hiker/vault-id`) rather than its absolute path — so moving or renaming the vault directory keeps its device identity and content key instead of silently regenerating them. [sync-secrets-user-scope, sync-vault-stable-id]


## Transport

libp2p, configured to exactly the pieces this design uses and nothing else: `default-features = false` with `tcp`, `noise`, `yamux`, `mdns`, `request-response` (or `stream`), `tokio`, `macros`. The transport is role-agnostic — the same protocol runs P2P and server. [sync-libp2p-transport]

The malware-adjacent P2P behaviors are **compiled out and kept out**: no `kad` (DHT), `dcutr` (hole-punching), `relay`, or `autonat`. `cargo-deny` bans those crates by name, so a future feature flip that would pull one in fails CI — the guarantee is enforced, not habitual, and visible to an enterprise reviewer in the dependency graph / SBOM. [sync-banned-p2p-features]

One authenticated connection carries many **muxed substreams** (yamux): a control substream (manifest, enrollment, state-vector handshake) plus one substream per document, streamed concurrently. A large document never head-of-line-blocks a small edit or the control channel. Safe because cross-document Yrs merges are commutative and idempotent — interleaving on separate streams can't corrupt state. [sync-stream-muxing]

**TCP, not QUIC.** QUIC's built-in muxing/encryption are attractive, but it rides UDP, which enterprise firewalls more often block or throttle; plain TCP is the more reliable reach on locked-down networks. [sync-tcp-transport-choice]


## Modes

**P2P / LAN.** Two hiker instances on the same network connect directly, authenticate from the swapped fingerprints, and run the protocol with no server. Discovery via mDNS (below). [sync-p2p-lan]

**Decoupled server.** A `hiker-syncd` binary runs the same `hiker-sync` crate in hub topology — many devices, relay plus store. It runs standalone or is spawned in-process by the app (a config flag), so an always-on desktop can be the hub for a phone without a separate process. Cross-network sync is the server's job; P2P is LAN-only, since hole-punching is banned and there is no NAT-traversal path by design. [sync-decoupled-server]

### Zero-knowledge server

The server never holds the vault content key, so it cannot read content. Because it can't decrypt, it can't compute Yrs state-vector deltas either — so it degrades to the only thing it can do on ciphertext: an **append-only encrypted-blob log per document, with a per-device cursor** (store-and-forward). Clients push sequenced encrypted update blobs; a device pulls everything past its cursor, decrypts, and lets Yrs merge them. All CRDT logic stays on the client. [sync-zero-knowledge-server]

The server keys blobs by a **blind id** — `HMAC(vault_key, logical_id)` — not the human path, so it sees random-looking ids and ciphertext, never names or content. It still learns blob count, size, and timing; hiding that (padding / cover traffic) is deferred. [sync-blind-id]


## Encryption

Two layers, both AES-256, each a different job:

- **Noise channel** (via libp2p-noise, the `Noise_XX_25519_ChaChaPoly_SHA256` suite it exposes) — mutual endpoint authentication from the enrolled static keys (no PKI / CA) plus hop confidentiality, forward secrecy, and replay protection. The channel cipher is ChaCha20-Poly1305 (libp2p-noise doesn't expose an AES-GCM suite); the AES-256 requirement is met by the content layer below. [sync-noise-channel]
- **Content layer** — each Yrs `update_v2` is AES-256-GCM-encrypted with the vault content key on the **client**, before it leaves the device, and decrypted on receipt. This is what makes the server zero-knowledge; it is applied uniformly so the P2P and server paths push the identical blob and any buffered blob is encrypted at rest. [sync-content-encryption-aes256]


## Discovery

mDNS (libp2p-mdns, part of the always-running swarm) discovers enrolled peers on the LAN. While `[sync].enabled && [sync].discovery`, discovery runs **continuously** so auto-sync (below) always has live peers; the manual "Discover" button stays as an on-demand ~30s rescan. Discovery only supplies IP:port candidates — a connection still won't authenticate unless the device fingerprints were already swapped, so discovery never substitutes for enrollment, and `[sync].discovery = false` turns continuous LAN discovery off entirely. mDNS is LAN-only; cross-network devices use the configured server. [sync-mdns-discovery]

Discovered peers are classified as **sync target vs. not-yet-enrolled at read time** against the live enrolled set — not frozen at mDNS-event time — so enrolling a peer makes it an immediate sync target without waiting for the next announce. (The enrolled set is shared between the UI and the swarm, so enroll / un-enroll apply synchronously to both the displayed list and the connection-auth gate.) The Sync page surfaces both buckets: **Discovered on LAN (enrolled)** (reachable peers a round will sync) and **Seen on LAN (not enrolled)** (hiker instances found but not yet trusted), plus a one-time progress-log line when an un-enrolled peer is first seen. [sync-discovered-peers]

Each **Seen on LAN** row offers one-click **Enroll**: the peer's fingerprint is derived from its `PeerId` (the Ed25519 public key is embedded in the identity-multihash `PeerId`) and shown so the user can verify it matches the other device's fingerprint before enrolling — enrollment stays a verified pairing, not blind trust. Enrolling kicks an immediate sync round. [sync-enroll-from-discovered]

A responder that can't serve a request — a handler error, or a request from a peer it hasn't enrolled — replies with an explicit error the dialer surfaces, and logs a dropped un-enrolled connection, rather than letting the dialer see an opaque "connection closed before a response." This makes a one-sided / missing mutual enrollment diagnosable instead of silent. [sync-key-swap-enrollment]


## Automatic sync

While `[sync].enabled`, peers converge on their own — no manual action. The engine keeps enrolled peers continuously discovered and runs lightweight sync rounds on three triggers: **at startup** (a device that just came online catches up immediately), **when an enrolled peer is discovered**, and on a **short interval** (~15s). Each round exchanges state vectors and transfers only the delta, so a round with no new ops is a cheap, silent no-op — the progress log and toasts fire only when something actually transfers. `[sync].discovery = false` disables continuous LAN discovery and LAN auto-dial (server-mediated auto-sync via `server_url` still runs); the kill switch (`[sync].enabled = false`) stops auto-sync with the rest of the engine. The manual "Sync now" button remains for an on-demand round. Convergence still needs a path: two LAN peers must be online together, or a server must be in the path to relay for a device that's currently offline. [sync-auto-sync]


## `[sync]` config section

Per-vault, in `vault/.hiker/config.toml`. Secrets are excluded — they are user-scope per `sync-secrets-user-scope`. [sync-config-section]

```toml
[sync]
enabled = false              # opt in per vault
mode = "peer"                # "peer" | "server" | "both"
server_url = ""              # when using a relay / hub
discovery = true             # allow the manual mDNS discovery window
devices = []                 # enrolled device fingerprints
```

Embedding sync rides alongside but separate — the content-addressed blob store of `op-log-embeddings-lww-cache`, not the CRDT transport.


## UI

The sync surfaces in the egui app; all of it degrades cleanly when `[sync]` is off.

- **Sync page** — a singleton tab (actions menu → "Sync") showing engine state (enabled, mode, server URL), this device's fingerprint (copyable), the enrolled-device list (rename + remove), the content-key copy/import, the discovered-peer buckets with one-click Enroll (`sync-discovered-peers`, `sync-enroll-from-discovered`), config-sanity warnings, the conflicts section, and last-sync result. Actions: an **Enable sync / Disable sync** button (flips `[sync].enabled` via the config-commit path, same as the Settings toggle), enroll-by-fingerprint, "Sync now", "Discover (30s)", and a manual **"Connect to peer address"** field (dial an explicit multiaddr — an mDNS fallback; the peer must still be enrolled). Below, a recently-synced-items list (op log `author LIKE 'sync:%'`) renders each as its real op alongside a live progress log. When sync is disabled the page shows an "Enable sync" button instead of a dead end. [sync-ui-page]
- **Settings `[sync]` section** — `enabled` / `mode` / `server_url` / `discovery` via the standard settings rows; `devices` is read-only there (enrollment is the Sync page's job). Vault scope; secrets never appear (user-scope per `sync-secrets-user-scope`). [sync-settings-section]
- **Activity provenance** — synced changes appear in the activity feed as their underlying op (Modified / Created / Renamed), not as a distinct "sync" category, tagged with their source device (`sync:<device>`) and isolable via a "Synced" filter pill. [sync-activity-provenance]
- **Enable/disable are both live** — sync is off by default; with `[sync].enabled = false` nothing is constructed (no keys, no swarm, no listener, no mDNS advertising). A per-frame reconcile builds + spawns the engine the moment the toggle goes on, and tears it down (closing the listener and mDNS) the moment it goes off — no vault reopen in either direction. [sync-disable-kill-switch]


## Out of scope

- **NAT traversal / cross-network P2P.** P2P is LAN-only; the server covers off-LAN sync. Hole-punching (`dcutr`) is banned, so there is no direct off-LAN peer path by design.
- **Embedding vector transport.** Specced as the LWW content-addressed cache in `op-log-embeddings-lww-cache`.
- **Cluster-tree sync.** Trees stay in `trees.db` and don't sync until they move to per-tree `.md` files (separate work); once they do, they ride this layer like any document.
- **Encryption at rest of the local vault.** Orthogonal; this doc covers in-transit plus server-at-rest only.


## Deferred

- `sync-quic-transport-deferred` — QUIC transport (native muxing + encryption) once UDP reachability is acceptable, or as an opportunistic upgrade with TCP fallback.
- `sync-metadata-padding-deferred` — blob padding / cover traffic to blunt the server's count / size / timing metadata view.


## Forward refs

- `op-log.md` — the substrate: the Yrs update primitives, the `op_metadata` side table, external-edit reconciliation, and the "Sync substrate" readiness notes this layer rides on.
- `op-log.md` `op-log-embeddings-lww-cache` — embedding sync, a separate transport.
- `settings.md` — config conventions for the `[sync]` section.
- `diff.md` and `op-log-merge-conflict` — the conflict-hunk shape the Blocked resolution reuses.
