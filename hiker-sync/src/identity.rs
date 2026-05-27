//! Document and device identity types.
//!
//! A device never agrees on a shared `doc_id` string. Each keeps minting its
//! own local ULID `doc_id` (the [`LocalDocId`]); the transport binds each local
//! id to a shared [`LogicalId`] and maintains one shared Yrs lineage behind it.
//! Identity is the logical id, so a rename never re-opens the question. See
//! `docs/sync.md` "Identity". [sync-negotiated-doc-ids]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The shared, cross-device identity of a logical document. Every replica that
/// binds together shares one logical id and one Yrs lineage behind it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LogicalId(pub String);

/// A device-local document id — the local ULID `doc_id` that names the
/// `<doc-id>.yrs` / `.pending` files and the `op_metadata.doc_id` key. Each
/// device mints its own; binding maps it to a [`LogicalId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalDocId(pub String);

/// A short, checksummed device public-key fingerprint (Syncthing-Device-ID
/// flavor). Swapped out of band during enrollment to authenticate the Noise
/// channel. Produced by [`crate::crypto::DeviceKeypair::fingerprint`].
/// [sync-key-swap-enrollment]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceFingerprint(pub String);

/// One local-id → logical-id binding established at first contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub local_doc_id: LocalDocId,
    pub logical_id: LogicalId,
}

/// Per-vault map from each local document id to its shared logical id, plus
/// reverse lookup. Wraps a [`HashMap`] so it serializes as a plain object.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingTable {
    by_local: HashMap<LocalDocId, LogicalId>,
}

impl BindingTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or overwrite) a binding.
    pub fn insert(&mut self, local_doc_id: LocalDocId, logical_id: LogicalId) {
        self.by_local.insert(local_doc_id, logical_id);
    }

    /// Insert from a [`Binding`].
    pub fn bind(&mut self, binding: Binding) {
        self.insert(binding.local_doc_id, binding.logical_id);
    }

    /// Forward lookup: the logical id for a local document id, if bound.
    pub fn logical_for(&self, local: &LocalDocId) -> Option<&LogicalId> {
        self.by_local.get(local)
    }

    /// Reverse lookup: the local document id mapped to a given logical id, if
    /// any. Linear scan — the table is per-vault and small.
    pub fn local_for(&self, logical: &LogicalId) -> Option<&LocalDocId> {
        self.by_local
            .iter()
            .find_map(|(local, lid)| (lid == logical).then_some(local))
    }

    /// Whether a local document id is bound.
    pub fn is_bound(&self, local: &LocalDocId) -> bool {
        self.by_local.contains_key(local)
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.by_local.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.by_local.is_empty()
    }

    /// Iterate over `(local, logical)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&LocalDocId, &LogicalId)> {
        self.by_local.iter()
    }
}

/// Per-document sync status. A true fork sets [`SyncStatus::Blocked`] until the
/// user resolves it; the rest of the vault keeps syncing. [sync-blocked-state]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Bound to a logical lineage; updates stream both directions.
    Bound,
    /// Matched a peer path but not yet bound (awaiting classification / adopt).
    PendingBind,
    /// A true fork; streaming halted for this document pending user resolution.
    Blocked,
}

/// A persistent record of a document the sync engine could not merge: a true
/// fork (two devices diverged with no common ancestor). Held on the node
/// alongside the [`SyncStatus`] map so the UI can list every blocked doc — not
/// just the ones touched by the most recent round — and offer resolution verbs.
/// Cleared when the doc later converges or the user resolves it. [sync-blocked-state]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDoc {
    /// The shared logical id the doc bound (or would have bound) to. The stable
    /// key a resolution decision targets.
    pub logical_id: LogicalId,
    /// The vault-relative path of the local replica at the time of the fork.
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

    fn lid(s: &str) -> LogicalId {
        LogicalId(s.into())
    }
    fn local(s: &str) -> LocalDocId {
        LocalDocId(s.into())
    }

    #[test]
    fn forward_and_reverse_lookup() {
        let mut t = BindingTable::new();
        t.insert(local("01HLOCAL"), lid("LOGICAL-A"));
        assert_eq!(t.logical_for(&local("01HLOCAL")), Some(&lid("LOGICAL-A")));
        assert_eq!(t.local_for(&lid("LOGICAL-A")), Some(&local("01HLOCAL")));
        assert!(t.is_bound(&local("01HLOCAL")));
        assert!(!t.is_bound(&local("nope")));
        assert_eq!(t.logical_for(&local("nope")), None);
        assert_eq!(t.local_for(&lid("nope")), None);
    }

    #[test]
    fn bind_struct_inserts() {
        let mut t = BindingTable::new();
        t.bind(Binding {
            local_doc_id: local("L1"),
            logical_id: lid("G1"),
        });
        assert_eq!(t.len(), 1);
        assert!(!t.is_empty());
        assert_eq!(t.logical_for(&local("L1")), Some(&lid("G1")));
    }

    #[test]
    fn serde_round_trip() {
        let mut t = BindingTable::new();
        t.insert(local("L1"), lid("G1"));
        let json = serde_json::to_string(&t).unwrap();
        let back: BindingTable = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);

        let s = serde_json::to_string(&SyncStatus::PendingBind).unwrap();
        assert_eq!(s, "\"pending_bind\"");
    }
}
