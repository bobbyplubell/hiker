//! Enrollment-time hash classification.
//!
//! A fork must not be auto-merged: a positional CRDT merge of two genuinely
//! divergent texts interleaves into nonsense. At binding there is no shared
//! lineage to trust, so divergence is classified from the **content-hash
//! history** before any adoption. Hashes are blake3 over `materialize(accepted)`;
//! a hash "in history" means content was once identical, not strict ancestry.
//! See `docs/sync.md` "Enrollment-time classification".
//! [sync-enrollment-hash-classification]

use std::collections::HashSet;

/// The outcome of comparing two replicas' current content hash against each
/// other's `.ops` content-hash history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Current hashes equal — identical content. Bind; canonical chosen by a
    /// deterministic rule; no reconcile.
    Identical,
    /// Our current hash is in the peer's history — we are behind. We adopt the
    /// peer's lineage (symmetric fast-forward).
    FastForwardAdoptPeer,
    /// The peer's current hash is in our history — the peer is a prior version
    /// of our lineage. The peer adopts; no prompt, no loss.
    FastForwardPeerAdopts,
    /// Neither current hash is in the other's history, or *both* are (a revert
    /// can recreate an old hash, making a both-directions match ambiguous). A
    /// true fork (or ambiguous): Blocked. [sync-blocked-state]
    Fork,
}

/// Classify two replicas of a document at first contact, per the table in
/// `docs/sync.md`:
///
/// | Condition | Result |
/// | --- | --- |
/// | current hashes equal | [`Classification::Identical`] |
/// | `theirs_current` ∈ `ours_history` | [`Classification::FastForwardPeerAdopts`] |
/// | `ours_current` ∈ `theirs_history` | [`Classification::FastForwardAdoptPeer`] |
/// | neither, **or both** | [`Classification::Fork`] |
///
/// Equality short-circuits first. A both-directions match is ambiguous (a
/// revert can recreate an old hash) and escalates to [`Classification::Fork`]
/// rather than guessing a direction.
pub fn classify(
    ours_current: &str,
    ours_history: &HashSet<String>,
    theirs_current: &str,
    theirs_history: &HashSet<String>,
) -> Classification {
    if ours_current == theirs_current {
        return Classification::Identical;
    }

    let peer_is_behind = ours_history.contains(theirs_current);
    let we_are_behind = theirs_history.contains(ours_current);

    match (peer_is_behind, we_are_behind) {
        // Exactly one direction matches: a clean fast-forward.
        (true, false) => Classification::FastForwardPeerAdopts,
        (false, true) => Classification::FastForwardAdoptPeer,
        // Neither matches (genuine fork) or both match (ambiguous revert).
        (false, false) | (true, true) => Classification::Fork,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn identical_current_binds() {
        // Equal current hashes -> Identical, even with disjoint histories.
        assert_eq!(
            classify("h", &set(&["a"]), "h", &set(&["b"])),
            Classification::Identical
        );
    }

    #[test]
    fn identical_takes_priority_over_history_matches() {
        // Equal currents short-circuit before any history check.
        assert_eq!(
            classify("h", &set(&["h"]), "h", &set(&["h"])),
            Classification::Identical
        );
    }

    #[test]
    fn peer_behind_us_peer_adopts() {
        // theirs_current is in our history => peer is a prior version of us.
        assert_eq!(
            classify("ours_now", &set(&["older", "theirs_now"]), "theirs_now", &set(&["theirs_now"])),
            Classification::FastForwardPeerAdopts
        );
    }

    #[test]
    fn we_are_behind_we_adopt() {
        // ours_current is in their history => we are behind, we adopt.
        assert_eq!(
            classify("ours_now", &set(&["ours_now"]), "theirs_now", &set(&["older", "ours_now"])),
            Classification::FastForwardAdoptPeer
        );
    }

    #[test]
    fn neither_in_history_is_fork() {
        assert_eq!(
            classify("ours_now", &set(&["ours_now"]), "theirs_now", &set(&["theirs_now"])),
            Classification::Fork
        );
    }

    #[test]
    fn empty_histories_distinct_currents_is_fork() {
        assert_eq!(
            classify("a", &set(&[]), "b", &set(&[])),
            Classification::Fork
        );
    }

    #[test]
    fn both_directions_match_is_ambiguous_fork() {
        // A revert recreated an old hash on each side: theirs_current ∈ ours
        // history AND ours_current ∈ theirs history. Ambiguous -> Fork.
        assert_eq!(
            classify(
                "ours_now",
                &set(&["ours_now_old", "theirs_now"]),
                "theirs_now",
                &set(&["theirs_now_old", "ours_now"])
            ),
            Classification::Fork
        );
    }
}
