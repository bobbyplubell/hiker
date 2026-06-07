# Sync

Multi-device sync of a vault across a user's own devices. Sync ships **files, not ops**: on save, a device pushes the canonical `.md` (or a diff vs the base) plus a version hash; there is **no CRDT on the wire**. The op log (`op-log.md`) is the local substrate; this doc specs the layer on top — the pluggable transport seam, the one 3-way text merge, the one unified conflict surface, and the default libp2p transport (device identity, enrollment, the encrypted transport, and the server). The git transport is in `git.md`.

Concurrent edits reconcile through a single 3-way text merge; this layer moves whole-file content + version metadata and decides which replicas are the same document. Because the wire carries text, there is no lineage, no `client_id`, no per-device Doc — "same note diverged" is a text merge, "different notes at one path" is a fork conflict.


## Transport seam

Sync is reached behind one pluggable transport trait so the mechanism is swappable and the merge/conflict logic above it is transport-agnostic: [sync-transport-seam]

- **libp2p** (default, this doc) — encrypted file blobs + version metadata over an authenticated P2P/relay channel; zero-knowledge, LAN discovery, turnkey, no account. Fits a store-and-forward blob server cleanly because the payload is already a whole-file blob.
- **integrated git** (`git.md`) — hiker drives `commit` + `push`/`pull`; the user brings a remote. Transparent and interoperable.
- **manual git** (`git.md`) — the user drives git; hiker tolerates HEAD moving and folds changes through the external-edit 3-way.
- **none** — local-only; `.ops` history still accrues.

**One rule: no two bidirectional cross-device syncs at once** (libp2p *and* git-as-sync). libp2p plus git-as-local-versioning (commit-only, no push/pull) is fine — git is then just a second local history, not a second sync path. [sync-single-bidirectional-transport]


## Identity

Documents are identified across devices by their **vault-relative path** — the same path-identity as the local substrate (`op-log-path-identity`). There is no per-device `doc_id` and no `path → id` table; the transport speaks paths and file content. [sync-path-identity]

A rename is an observed content-preserving move (`op-log-observed-move`): the sending device conveys the move (old path → new path, content unchanged) plus a delete signal for the old path; the receiving device applies the move and the document continues syncing under its new path.

**Concurrent rename to the same path is a conflict, not an auto-resolve.** If two devices rename different documents onto the same path while disconnected, the collision Blocks both for user resolution (see "Conflicts") rather than letting one silently win or auto-spawning a conflict-copy. A rare single-user mishap, but the user picks the outcome. [sync-concurrent-rename-not-merged]


## The 3-way text merge

All concurrent reconciliation is one 3-way text merge over `(base, ours, theirs)`: [sync-three-way-merge]

- **base** is the **last-common version** for this `(document, peer)` — recovered from **content-hash history**: the most recent version hash present in *both* devices' `.ops` history for the path (`most_recent_shared_op_id`). This is the analogue of git's merge-base; a wrong base loses edits, so it is the one place to be careful.
- **Disjoint hunks** → merge automatically, no prompt.
- **Same-region hunks** → conflict (surfaced, never silently interleaved).
- **No common base** (the path exists on both devices with no shared version in either history) → **fork conflict** — two genuinely different notes at one path, never merged.

Version hashes are blake3 over `materialize(accepted)`. "Hash X is in this doc's history" is one lookup against the regenerable history query-index (`index.db`, rebuilt from `.ops`); a hash appearing in history means "content was once identical," not strict ancestry — a revert can recreate an old hash, so a both-directions match is ambiguous and escalates rather than guesses. [sync-content-hash-history]

### First-contact classification

At first contact for a path there is no recorded merge-base, so divergence is classified from the content-hash history before anything is applied: [sync-enrollment-hash-classification]

| Condition | Meaning | Action |
| --- | --- | --- |
| current hashes equal | identical content | bind; no merge |
| peer's current hash ∈ our `.ops` history | peer is a prior version of our content | fast-forward: peer adopts ours, no prompt, no loss |
| our current hash ∈ peer's history | we are behind | symmetric fast-forward |
| a shared older hash, both advanced | common base exists | 3-way merge (auto if disjoint, conflict if same-region) |
| neither current ∈ the other's history, no shared base | true fork | Blocked (fork conflict) |

### Device naming

Device identity is the immutable fingerprint; the human-readable **device name** is a separate, self-set label that propagates so the other devices can show "synced from `laptop`" instead of a fingerprint. [sync-device-name]

- **Self-set, vault-scope.** A device names *itself*: `[sync].device_name` holds THIS device's chosen name (per-vault, synced config metadata — not the user-scope key sidecar). A device never names another.
- **Carried on the handshake.** `Hello`/`HelloAck` carry the sending device's `device_name` (optional — a peer that omits it still parses).
- **Learned names map.** Each device keeps `[sync].device_names`, a `fingerprint → name` map, and on every handshake adopts the peer's self-reported name (last-write-from-that-device wins for ITS OWN name). This is what the UI renders for a remote device.
- **Local override.** User-scope `aliases.json` is an optional **local display override**: when set, the local alias wins over the learned synced name; never synced. Precedence: local alias → learned synced name → truncated fingerprint.

The name never gates authentication or affects the fingerprint-based enrollment/auth path.


## Conflicts

A positional merge of two genuinely *contended* changes converges to nonsense rather than intent, so hiker never silently picks a winner: a contended change **Blocks the document and the user resolves it**. Clean, uncontended merges still happen automatically and silently. [sync-conflict-block-and-resolve]

### What blocks vs what merges

| Case | Behavior |
| --- | --- |
| Clean fast-forward (peer strictly ahead) | auto-apply, no prompt [sync-enrollment-hash-classification] |
| Disjoint-region concurrent edits (common base, no overlapping ranges) | auto-merge (3-way), no prompt |
| Identical content | bind, no merge |
| Same-region concurrent edit (overlapping ranges) | **Block** [sync-conflict-detect-same-region] |
| Concurrent rename to the same path | **Block** [sync-concurrent-rename-not-merged] |
| Delete vs concurrent edit | **Block** [sync-conflict-delete-vs-edit] |
| Fork (different content, no shared base) | **Block** [sync-enrollment-hash-classification] |

### Detection

- **Same-region edit** — the 3-way merge over `(base, ours, theirs)`: if our divergent hunks overlap the incoming hunks, it's contended. Non-overlapping hunks merge; overlapping hunks block. Same overlap test the local user-vs-agent `op-log-merge-conflict` uses. [sync-conflict-detect-same-region]
- **Rename collision** — the incoming move's target path already names a different local document. [sync-concurrent-rename-not-merged]
- **Delete vs edit** — a delete of a path concurrent with an edit to it (neither based on the other's version). [sync-conflict-delete-vs-edit]
- **Fork** — first-contact hash classification finds no shared base (per "First-contact classification"). [sync-enrollment-hash-classification]

### Unified conflict surface

Local user-vs-agent overlap, sync same-region conflicts, and (git transport) git merge markers all feed **one** conflict surface — a net consolidation of the prior two conflict UIs. [sync-unified-conflict-surface]

- **Inline markers + ActionRow.** Conflict regions render VS-Code-marker style with inline `ActionRow` buttons (the same `editor-diff/conflict.rs` overlap + keep-theirs machinery the local overlay uses).
- **Gating.** A conflicted-buffer state gates save/index until the user resolves it; the markdown live-preview renders conflict regions raw.
- **Default 2-way** (ours / theirs); diff3 (showing the base) is a later toggle.

### Block semantics

A blocked document **holds the incoming change — it is not folded into `accepted`**: the local doc stays at its current version, no content streams either way for it, and the rest of the vault keeps syncing (per-doc isolation, `bug-sync-round-aborts-on-one-doc`). The peer's version is fetched on demand for preview (`sync-fork-diff`). The block is **persisted** — blocked path, conflict kind, and peer fingerprint survive a restart and re-surface, rather than clearing on memory loss. [sync-blocked-state, sync-conflict-block-persistence]

### Notification

A new block **notifies the user** (activity-feed entry + badge, per "Attention surfacing") rather than waiting to be noticed on the Sync page, whose Conflicts section lists each blocked doc with its kind, the peer, a **View diff**, and the resolution verbs. **View diff** fetches the peer's current text on demand over the authenticated channel (a blocked doc holds ours, not theirs) and renders a read-only unified diff against the local version; the fetch never mutates the doc or sync state, and the peer must be online. [sync-conflict-notify, sync-fork-diff]

### Resolution

The user picks per blocked document; the choice produces a definite `accepted` state that propagates so the *other* device unblocks and converges to the same resolution rather than re-conflicting. Resolution reuses the unified conflict surface (`sync-unified-conflict-surface`, `op-log-merge-conflict`) where the conflict is a text overlap. [sync-conflict-resolve-actions]

| Conflict | Choices |
| --- | --- |
| Same-region edit / fork | **Keep mine** — our text becomes canonical at the path; the peer fast-forwards to it. **Keep theirs** — adopt the peer's text, dropping our divergence. **Keep both** — ours stays canonical at the path; the peer's version lands as a `<stem>.conflict-<short>.<ext>` sibling note. |
| Rename collision | **Keep mine** (our rename wins the path) · **Keep theirs** (their rename wins) · **Keep both** (both documents survive at distinct paths; the loser takes the `conflict-`suffixed path). |
| Delete vs edit | **Keep deleted** (the delete wins) · **Keep edit** (resurrect the document with the edit). |

On resolution the document unblocks and streaming resumes.


## Enrollment and keys

Two secrets: a per-device static keypair (channel auth) and a per-vault 256-bit content key (content encryption, shared by all enrolled devices).

Pairing is a **manual bidirectional swap of device public-key fingerprints** — each device shows a short checksummed fingerprint (copy-paste or QR, the Syncthing Device ID model); the user enters A's into B and B's into A. The out-of-band swap is what authenticates the Noise channel, so there is no MITM window. [sync-key-swap-enrollment]

The high-entropy **vault content key is never typed or displayed**. Each device generates its own per-vault key, so two devices start with different keys; they converge automatically. In the `Hello` handshake each side carries a **non-secret content-key fingerprint** (a truncated blake3 of the key bytes — preimage-resistant, so it never reveals the key). If the fingerprints match, both already share a key and nothing transfers. If they differ, a deterministic owner is picked (`canonical = min(device fingerprint)`); only the non-canonical side ever adopts, and whether it adopts depends on whether its own key is **established** (below). The 32 raw key bytes ride the already-encrypted, mutually-authenticated Noise channel to a verified-enrolled peer — enrollment is the consent (the SSH/Signal model) — and are never logged or written into the synced vault. The manual Copy/Import of the key on the Sync page remains as a fallback. [sync-vault-key-inband]

**A device never silently replaces a deliberately-set key.** Each content key carries an *established* flag (persisted user-scope alongside the key): false for the fresh key a brand-new device just auto-generated, true once the key has been deliberately set — either by a manual Copy/Import or by a completed in-band adoption. On a fingerprint mismatch where this device is non-canonical: [sync-content-key-confirm-on-change]

- **Fresh (non-established) key** → request the canonical device's key in-band and adopt it before any document content, marking the result established. The brand-new-device path; the adoption is surfaced (a progress line "adopted *peer*'s vault content key").
- **Established key** → do **not** silently switch. Hold this device's key for the round and surface the mismatch so the user can confirm rather than have an intentionally-imported key swapped out. Copy/Import is the explicit accept path (sets the key, marks it established). Until then, content across the two keys fails to decrypt and surfaces the existing "different content keys" hint.

Because only the non-canonical side adopts, an established key on the *canonical* device is always preserved. The Sync page renders the held change; a one-click confirm is a later phase.

Both secrets live **user-scope, never in the synced vault** — the same rule as `[llm].api_key`, so a synced vault can't carry the key that decrypts it. Stored in the platform data dir, keyed by the vault's **stable id** (`core::vault::vault_id`, a ULID at `.hiker/vault-id`) rather than its absolute path — so moving or renaming the vault directory keeps its device identity and content key instead of silently regenerating them. [sync-secrets-user-scope, sync-vault-stable-id]


## Transport (libp2p)

libp2p, configured to exactly the pieces this design uses and nothing else: `default-features = false` with `tcp`, `noise`, `yamux`, `mdns`, `request-response` (or `stream`), `tokio`, `macros`. The transport is role-agnostic — the same protocol runs P2P and server. [sync-libp2p-transport]

The malware-adjacent P2P behaviors are **compiled out and kept out**: no `kad` (DHT), `dcutr` (hole-punching), `relay`, or `autonat`. `cargo-deny` bans those crates by name, so a future feature flip that would pull one in fails CI. [sync-banned-p2p-features]

One authenticated connection carries many **muxed substreams** (yamux): a control substream (manifest, enrollment, version handshake) plus one substream per document, streamed concurrently. A large document never head-of-line-blocks a small edit or the control channel. Safe because per-document file transfers are independent. [sync-stream-muxing]

**TCP, not QUIC.** QUIC's built-in muxing/encryption are attractive, but it rides UDP, which enterprise firewalls more often block or throttle; plain TCP is the more reliable reach on locked-down networks. [sync-tcp-transport-choice]


## Modes

**P2P / LAN.** Two hiker instances on the same network connect directly, authenticate from the swapped fingerprints, and run the protocol with no server. Discovery via mDNS (below). [sync-p2p-lan]

**Decoupled server.** A `hiker-syncd` binary runs the same `hiker-sync` crate in hub topology — many devices, relay plus store. It runs standalone or is spawned in-process by the app (a config flag), so an always-on desktop can be the hub for a phone without a separate process. Cross-network sync is the server's job; P2P is LAN-only, since hole-punching is banned and there is no NAT-traversal path by design. [sync-decoupled-server]

### Zero-knowledge server

The server never holds the vault content key, so it cannot read content. It stores what it's given: an **append-only encrypted-file-blob log per document, with a per-device cursor** (store-and-forward). Clients push sequenced encrypted file blobs (whole file, or a diff vs the base); a device pulls everything past its cursor, decrypts, and runs the 3-way merge locally. All merge/conflict logic stays on the client — the file-blob payload fits store-and-forward directly, with no server-side CRDT math. [sync-zero-knowledge-server]

The server keys blobs by a **blind id** — `HMAC(vault_key, path)` — not the human path, so it sees random-looking ids and ciphertext, never names or content. A rename rotates the blind id: the document's blob stream at the old blind id stops growing, and a fresh stream opens at the new blind id; the receiving device GCs the old stream after applying the move. The server still learns blob count, size, and timing; hiding that (padding / cover traffic) is deferred. [sync-blind-id, sync-rename-blob-rotation]


## Encryption

Two layers, both AES-256, each a different job:

- **Noise channel** (via libp2p-noise, the `Noise_XX_25519_ChaChaPoly_SHA256` suite it exposes) — mutual endpoint authentication from the enrolled static keys (no PKI / CA) plus hop confidentiality, forward secrecy, and replay protection. The channel cipher is ChaCha20-Poly1305 (libp2p-noise doesn't expose an AES-GCM suite); the AES-256 requirement is met by the content layer below. [sync-noise-channel]
- **Content layer** — each file blob is AES-256-GCM-encrypted with the vault content key on the **client**, before it leaves the device, and decrypted on receipt. This is what makes the server zero-knowledge; it is applied uniformly so the P2P and server paths push the identical blob and any buffered blob is encrypted at rest. [sync-content-encryption-aes256]


## Discovery

mDNS (libp2p-mdns, part of the always-running swarm) discovers enrolled peers on the LAN. While `[sync].enabled && [sync].discovery`, discovery runs **continuously** so auto-sync (below) always has live peers; the manual "Discover" button stays as an on-demand ~30s rescan. Discovery only supplies IP:port candidates — a connection still won't authenticate unless the device fingerprints were already swapped, so discovery never substitutes for enrollment, and `[sync].discovery = false` turns continuous LAN discovery off entirely. mDNS is LAN-only; cross-network devices use the configured server. [sync-mdns-discovery]

Discovered peers are classified as **sync target vs. not-yet-enrolled at read time** against the live enrolled set — not frozen at mDNS-event time — so enrolling a peer makes it an immediate sync target without waiting for the next announce. The enrolled set is shared between UI and swarm, so enroll / un-enroll apply synchronously to both the displayed list and the connection-auth gate. The Sync page surfaces both buckets: **Discovered on LAN (enrolled)** (reachable peers a round will sync) and **Seen on LAN (not enrolled)** (found but not yet trusted), plus a one-time progress-log line when an un-enrolled peer is first seen. [sync-discovered-peers]

Each **Seen on LAN** row offers one-click **Enroll**: the peer's fingerprint is derived from its `PeerId` (the Ed25519 public key is embedded in the identity-multihash `PeerId`) and shown so the user can verify it matches the other device's fingerprint before enrolling — enrollment stays a verified pairing, not blind trust. Enrolling kicks an immediate sync round. [sync-enroll-from-discovered]

A responder that can't serve a request — a handler error, or a request from a peer it hasn't enrolled — replies with an explicit error the dialer surfaces, and logs a dropped un-enrolled connection, rather than letting the dialer see an opaque "connection closed before a response." This makes a one-sided / missing mutual enrollment diagnosable instead of silent. [sync-key-swap-enrollment]


## Automatic sync

While `[sync].enabled`, peers converge on their own — no manual action. The engine keeps enrolled peers continuously discovered and runs lightweight sync rounds on three triggers: **at startup** (a device that just came online catches up immediately), **when an enrolled peer is discovered**, and on a **short interval** (~15s). Each round exchanges version manifests and transfers only the documents whose hashes differ, so a round with no changes is a cheap, silent no-op — the progress log and toasts fire only when something actually transfers. `[sync].discovery = false` disables continuous LAN discovery and LAN auto-dial (server-mediated auto-sync via `server_url` still runs); the kill switch (`[sync].enabled = false`) stops auto-sync with the rest of the engine. The manual "Sync now" button remains for an on-demand round. Convergence still needs a path: two LAN peers must be online together, or a server must be in the path to relay for a device that's currently offline. [sync-auto-sync]

A config that is `enabled` but can never reach a peer — peer mode with `discovery = false` and no `server_url` — would otherwise no-op every round in silence. The engine emits a one-time warning at service start (`tracing::warn!` + a progress-log line) so the trap is visible even when the Sync page is never opened; the Sync page renders its own equivalent from the engine snapshot. [sync-config-sanity-warning]

### Poke on commit

Sync is pull-based, so a committed edit on device A wouldn't reach B until B runs its own round (up to ~15s on the interval). To close that gap, **a commit on A pokes its enrolled peers** so they pull promptly. The poke carries no content — it's a content-free nudge over the authenticated channel (`SyncPoke`/`SyncPokeAck`, after the usual Hello) that sets the peer's `poked` flag; the peer's sync driver drains that flag the same way it drains the on-discovery trigger and fires an `auto_sync_round`, where the actual manifest/transfer exchange happens. The send side is debounced (~300ms) to coalesce a burst of saves into one poke round, and is a no-op when there are no enrolled peers or sync is off. This is the LAN baseline; an editor/headless role flag and keystroke-level streaming are deferred. [sync-poke-on-commit]

### On save = sync

Sync is **on save**: an edit reaches peers only once it's committed to `accepted` (Ctrl+S → `commit_working` → poke). A `working`-only (unsaved) edit stays local until a save folds it into `accepted`; the crash-recovery sidecar (`autosave.md`) preserves it across a crash, but only `accepted` ever crosses the wire. Live-syncing unsaved buffers (a concurrent-editing mode) is deliberately out of scope for now — see `ideas.md`.


## `[sync]` config section

Per-vault, in `vault/.hiker/config.toml`. Secrets are excluded — they are user-scope per `sync-secrets-user-scope`. [sync-config-section]

```toml
[sync]
enabled = false              # opt in per vault
transport = "libp2p"         # "libp2p" | "git" | "none"  [sync-transport-seam]
mode = "peer"                # libp2p: "peer" | "server" | "both"
server_url = ""              # libp2p: when using a relay / hub
discovery = true             # libp2p: allow continuous mDNS discovery
devices = []                 # libp2p: enrolled device fingerprints
device_name = ""             # THIS device's self-set human name (carried on the handshake) [sync-device-name]
device_names = {}            # learned fingerprint -> name map for enrolled peers [sync-device-name]
```

The git transport's keys (`remote`, `auto_commit`, …) live in `git.md`'s config section. Embedding sync rides alongside but separate — the content-addressed blob store of `op-log-embeddings-cache`, not the file transport.


## UI

The sync surfaces in the egui app; all of it degrades cleanly when `[sync]` is off.

- **Sync page** — a singleton tab (actions menu → "Sync"). [sync-ui-page] Shows:
  - Engine state (enabled, transport, mode, server URL); this device's fingerprint (copyable) and editable **device name** (`sync-device-name`).
  - Enrolled-device list — each peer's learned synced name, optional local-alias override, remove.
  - Content-key copy/import; a **held content-key change** banner when a peer's key differs and this device declined to switch its established key (`sync-content-key-confirm-on-change` — points at Copy/Import as the accept path).
  - Discovered-peer buckets with one-click Enroll (`sync-discovered-peers`, `sync-enroll-from-discovered`); config-sanity warnings; the conflicts section; last-sync result.
  - Recently-synced-items list (frames authored `sync:%`), each rendered as its real change, alongside a live progress log.
  - Actions: **Enable / Disable sync** (flips `[sync].enabled` via the config-commit path, same as Settings), enroll-by-fingerprint, "Sync now", "Discover (30s)", and a **"Connect to peer address"** field (dial an explicit multiaddr — an mDNS fallback; the peer must still be enrolled). When disabled, the page shows "Enable sync" rather than a dead end.
- **Attention surfacing** — sync captures failure/attention state proactively, not only when the page is open: `SyncReport.errored` per-doc/per-peer skips, blocked docs, a held content-key change, a surfaced last-error. Three surfaces: [sync-attention-badge]
  - **Errored list** on the Sync page — the per-doc/per-peer items the last round SKIPPED so the rest could sync — its own red section, distinct from the round-aborting `last_error` line and from Conflicts (forks the user resolves).
  - **Sync tab badge** `⚠ N` summing the items that need the user (blocked docs + held content-key change + present last-error); no badge when healthy.
  - **Toast** the first time a doc newly blocks, an errored item appears, or a content-key change is newly held; never re-fires in steady state. The standing state lives on the page + badge; the toast is the one-shot nudge. Subsumes the notification half of `sync-conflict-notify`.
- **Settings `[sync]` section** — `enabled` / `transport` / `mode` / `server_url` / `discovery` via the standard settings rows; `devices` is read-only there (enrollment is the Sync page's job). Vault scope; secrets never appear (user-scope per `sync-secrets-user-scope`). [sync-settings-section]
- **Activity provenance** — synced changes appear in the activity feed as their underlying change (Modified / Created / Renamed), not as a distinct "sync" category, tagged with their source device (`sync:<device>`) and isolable via a "Synced" filter pill. [sync-activity-provenance]
- **Enable/disable are both live** — sync is off by default; with `[sync].enabled = false` nothing is constructed (no keys, no swarm, no listener, no mDNS advertising). A per-frame reconcile builds + spawns the engine the moment the toggle goes on, and tears it down (closing the listener and mDNS) the moment it goes off — no vault reopen in either direction. [sync-disable-kill-switch]


## Out of scope

- **NAT traversal / cross-network P2P.** P2P is LAN-only; the server covers off-LAN sync. Hole-punching (`dcutr`) is banned, so there is no direct off-LAN peer path by design.
- **Embedding vector transport.** Specced as the content-addressed cache in `op-log-embeddings-cache`.
- **Cluster-tree sync.** Trees don't sync until they move to per-tree `.md` files (separate work); once they do, they ride this layer like any document.
- **Encryption at rest of the local vault.** Orthogonal; this doc covers in-transit plus server-at-rest only.
- **CRDT-level sync.** Replaced by file-level sync + the 3-way merge; there is no convergence engine on the wire.


## Deferred

- `sync-quic-transport-deferred` — QUIC transport (native muxing + encryption) once UDP reachability is acceptable, or as an opportunistic upgrade with TCP fallback.
- `sync-metadata-padding-deferred` — blob padding / cover traffic to blunt the server's count / size / timing metadata view.
- `sync-diff-on-the-wire-deferred` — push a diff vs the base instead of the whole file when the base is known to the peer; whole-file is the baseline.


## Forward refs

- `op-log.md` — the substrate: the `accepted` content, the `.ops` history + version hashes this layer's merge-base reads, external-edit reconciliation, and the "Sync substrate" seam.
- `git.md` — the integrated + manual git transports behind the same seam.
- `op-log-embeddings-cache` — embedding sync, a separate transport.
- `settings.md` — config conventions for the `[sync]` section.
- `diff.md` and `op-log-merge-conflict` — the conflict-hunk shape the Blocked resolution and the unified conflict surface reuse.
