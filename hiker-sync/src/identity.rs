//! Document and device identity types.
//!
//! Documents are identified across devices by their **vault-relative path**.
//! Each device keeps its own internal [`LocalDocId`] ULID for op-log
//! bookkeeping (the per-device `<doc-id>.yrs` filename / `op_metadata.doc_id`
//! key); the transport never exchanges those local ids. The wire speaks paths.
//! [sync-path-identity]
//!
//! A rename produces a new identity. Concurrent rename on two devices is
//! explicitly NOT an auto-merge case: a collision with another document at the
//! new path BLOCKS both for user resolution (Keep mine / theirs / both) rather
//! than silently picking a winner. [sync-concurrent-rename-not-merged]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A device-local document id — the local ULID `doc_id` that names the
/// `<doc-id>.yrs` / `.pending` files and the `op_metadata.doc_id` key. Each
/// device mints its own; the transport resolves it from a vault path via
/// [`hiker_core::oplog::OpLog::doc_id_for_path`] and never exchanges it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalDocId(pub String);

/// A short, checksummed device public-key fingerprint (Syncthing-Device-ID
/// flavor). Swapped out of band during enrollment to authenticate the Noise
/// channel. Produced by [`crate::crypto::DeviceKeypair::fingerprint`].
/// [sync-key-swap-enrollment]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceFingerprint(pub String);

/// Per-document sync status. A true fork sets [`SyncStatus::Blocked`] until the
/// user resolves it; the rest of the vault keeps syncing. [sync-blocked-state]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Streaming both directions on the shared lineage for this path.
    Bound,
    /// A true fork; streaming halted for this document pending user resolution.
    Blocked,
}

/// A persistent record of a document the sync engine could not merge: a true
/// fork (two devices diverged with no common ancestor). Held on the node
/// keyed by vault path so the UI can list every blocked doc — not just the ones
/// touched by the most recent round — and offer resolution verbs. Cleared when
/// the doc later converges or the user resolves it. [sync-blocked-state]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDoc {
    /// The vault-relative path of the local replica — the stable key a
    /// resolution decision targets. [sync-path-identity]
    pub path: String,
    /// Why the doc is blocked: `"fork"`, `"same-region"`, `"delete-vs-edit"`,
    /// or `"rename-collision"`.
    pub reason: String,
    /// The fingerprint of the peer device the conflict was detected against —
    /// what the UI renders as "forked with <alias-or-fingerprint>".
    pub peer_fingerprint: DeviceFingerprint,
}

/// The user's decision for resolving a blocked (forked) document. Consumed by
/// the fork branch on the NEXT sync round so it acts instead of re-blocking;
/// reuses the `op-log-merge-conflict` / `drift-conflict-modal` keep-mine /
/// keep-theirs / keep-both shape. See `docs/sync.md` "Blocked documents".
/// [sync-blocked-state]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Our side wins. For a fork / same-region block: our lineage is canonical
    /// and we push it for the peer to adopt. For a rename collision: our doc
    /// keeps the contended path and the peer's doc yields to a `conflict-`
    /// sibling. [sync-concurrent-rename-not-merged]
    KeepMine,
    /// The peer's side wins. For a fork / same-region block: adopt the peer's
    /// lineage. For a rename collision: the peer's doc keeps the contended path
    /// and ours moves to a `conflict-` sibling. [sync-concurrent-rename-not-merged]
    KeepTheirs,
    /// Both versions survive. For a fork / same-region block: ours stays at the
    /// path, the peer's lands as a `conflict-` sibling. For a rename collision:
    /// both docs survive at distinct paths, the loser (deterministic by
    /// fingerprint) taking the `conflict-` suffix. [sync-concurrent-rename-not-merged]
    KeepBoth,
    /// Delete-vs-edit only: the delete wins — tombstone the doc (and trash the
    /// `.md`), converging the peer to deleted. [sync-conflict-delete-vs-edit]
    KeepDeleted,
    /// Delete-vs-edit only: resurrect — the doc stays/comes back live with the
    /// edit, converging the peer to the edited live doc.
    /// [sync-conflict-delete-vs-edit]
    KeepEdit,
}

/// Durable per-vault store for the blocked-conflict set, so a held conflict
/// survives an app restart and re-surfaces rather than silently clearing — the
/// exact silent-resolution failure the conflict model exists to eliminate.
/// Without this the blocked set lives only in a `Mutex<HashMap>` on the node
/// and evaporates the moment the process exits. [sync-conflict-block-persistence]
///
/// Stored as a single JSON file at `<vault>/.hiker/sync/blocked.json` — vault-
/// scoped local state alongside the op-log, NOT user-scope (blocks are not
/// secrets, unlike the device/content keys, so the `sync-secrets-user-scope`
/// rule doesn't apply; this is recoverable, vault-relative conflict bookkeeping
/// that belongs next to the vault it describes). The whole `path -> BlockedDoc`
/// map is rewritten on every change; the set is small (one entry per blocked
/// doc), so a full rewrite is simpler and atomic enough than incremental edits.
///
/// A missing or unreadable file hydrates to an empty set (a fresh vault, or a
/// corrupt file we'd rather start clean from than wedge on) — the node logs the
/// corruption case via `tracing` so it isn't silent.
#[derive(Debug, Clone)]
pub struct BlockStore {
    path: PathBuf,
}

impl BlockStore {
    /// The block store for a vault, rooted at `<vault>/.hiker/sync/blocked.json`.
    pub fn for_vault(vault_root: &Path) -> Self {
        Self {
            path: vault_root
                .join(".hiker")
                .join("sync")
                .join("blocked.json"),
        }
    }

    /// Construct a store at an explicit file path — the test seam.
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Hydrate the persisted `path -> BlockedDoc` map. A missing file is an
    /// empty set; an unreadable / corrupt file logs a warning and also yields an
    /// empty set rather than wedging the node.
    pub fn load(&self) -> HashMap<String, BlockedDoc> {
        match std::fs::read(&self.path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<BlockedDoc>>(&bytes) {
                Ok(docs) => docs.into_iter().map(|d| (d.path.clone(), d)).collect(),
                Err(e) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %e,
                        "sync: blocked.json unreadable, starting with an empty blocked set"
                    );
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        }
    }

    /// Rewrite the persisted set from the current in-memory map. Best-effort:
    /// an I/O failure is logged (via the caller) but never panics the node — a
    /// failed persist means the block is still held in memory this session and
    /// will be re-recorded next round if the conflict persists.
    pub fn save(&self, blocked: &HashMap<String, BlockedDoc>) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut docs: Vec<&BlockedDoc> = blocked.values().collect();
        // Stable order on disk so the file doesn't churn on every rewrite.
        docs.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes =
            serde_json::to_vec_pretty(&docs).map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(&self.path, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_store_round_trips_across_reconstruct() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::for_vault(dir.path());
        assert!(store.load().is_empty(), "fresh vault has no blocks");

        let mut blocked = HashMap::new();
        blocked.insert(
            "notes/a.md".to_string(),
            BlockedDoc {
                path: "notes/a.md".into(),
                reason: "fork".into(),
                peer_fingerprint: DeviceFingerprint("DEV-PEER".into()),
            },
        );
        store.save(&blocked).unwrap();

        // A fresh store over the same vault dir (a "restart") re-hydrates it.
        let reopened = BlockStore::for_vault(dir.path()).load();
        assert_eq!(reopened, blocked, "block survives reconstruct");

        // Clearing the last block persists an empty set (no stale resurrection).
        store.save(&HashMap::new()).unwrap();
        assert!(
            BlockStore::for_vault(dir.path()).load().is_empty(),
            "cleared block does not resurrect on reload"
        );
    }

    #[test]
    fn block_store_corrupt_file_hydrates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::for_vault(dir.path());
        std::fs::create_dir_all(dir.path().join(".hiker").join("sync")).unwrap();
        std::fs::write(
            dir.path().join(".hiker").join("sync").join("blocked.json"),
            b"{ not json",
        )
        .unwrap();
        assert!(store.load().is_empty(), "corrupt file starts clean, not wedged");
    }

    #[test]
    fn serde_round_trip_status_and_resolution() {
        let s = serde_json::to_string(&SyncStatus::Blocked).unwrap();
        assert_eq!(s, "\"blocked\"");
        let back: SyncStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, SyncStatus::Blocked);

        let r = serde_json::to_string(&Resolution::KeepBoth).unwrap();
        assert_eq!(r, "\"keep_both\"");
    }

    #[test]
    fn blocked_doc_round_trip() {
        let b = BlockedDoc {
            path: "notes/a.md".into(),
            reason: "fork".into(),
            peer_fingerprint: DeviceFingerprint("DEV-X".into()),
        };
        let j = serde_json::to_string(&b).unwrap();
        let back: BlockedDoc = serde_json::from_str(&j).unwrap();
        assert_eq!(back, b);
    }
}
