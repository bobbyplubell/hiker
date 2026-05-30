//! Document and device identity types.
//!
//! Documents are identified across devices by their **vault-relative path**.
//! Each device keeps its own internal [`LocalDocId`] ULID for op-log
//! bookkeeping (the per-device `<doc-id>.yrs` filename / `op_metadata.doc_id`
//! key); the transport never exchanges those local ids. The wire speaks paths.
//! [sync-path-identity]
//!
//! A rename produces a new identity. Concurrent rename on two devices is
//! explicitly NOT a supported merge case — last arriving rename wins on path,
//! and a collision with another document at the new path surfaces as a
//! conflict via the conflict-copy path. [sync-concurrent-rename-not-merged]

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
    /// Why the doc is blocked (currently always `"fork"`).
    pub reason: String,
    /// The fingerprint of the peer device the fork was detected against — what
    /// the UI renders as "forked with <alias-or-fingerprint>".
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
    /// Our side is canonical: unblock and offer our lineage to the peer. Full
    /// convergence is bilateral — the other device must choose keep-theirs.
    KeepMine,
    /// The peer's side is canonical: adopt the peer's lineage and converge.
    KeepTheirs,
    /// Preserve the local version as a conflict copy in the vault, then adopt
    /// the peer's lineage at the original path. Both versions survive.
    KeepBoth,
}

#[cfg(test)]
mod tests {
    use super::*;

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
