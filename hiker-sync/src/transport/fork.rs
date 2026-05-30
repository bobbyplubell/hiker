//! Fork policy: turn an [`enroll::Classification`] into action and resolve a
//! user's decision. A fork must not be auto-merged (the reason
//! `op-log-merge-conflict` exists), so this module owns the keep-mine /
//! keep-theirs / keep-both branch table and the conflict-copy naming for both
//! the keep-both path and the concurrent-rename-collision case.
//!
//! Pure `impl SyncNode` continuation; no items of its own. The lineage verbs
//! it calls live in [`super::lineage`].

use libp2p::PeerId;

use hiker_core::oplog::shapes::Author;

use crate::enroll::Classification;
use crate::identity::{LocalDocId, Resolution, SyncStatus};
use crate::protocol::ManifestEntry;
use crate::Error;

use super::{SyncNode, SyncReport};

/// Build the sibling path for a conflict copy: `<stem>.conflict-<short>.<ext>`,
/// where `<short>` is a fresh 6-char alphanumeric token (the same
/// disambiguator shape trail-waypoint filenames use, per `docs/trails.md`).
/// The copy lands next to the original in the same directory so it's an
/// obvious neighbor in the vault.
///
/// Used by the keep-both fork-resolution path AND the
/// concurrent-rename collision case where the loser's new path collides with
/// another document at that path. [sync-blocked-state, sync-concurrent-rename-not-merged]
pub(super) fn conflict_copy_path(path: &str) -> String {
    // Split into dir / file, then stem / ext, preserving the directory prefix.
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let (stem, ext) = match file.rfind('.') {
        Some(i) if i > 0 => (&file[..i], &file[i..]),
        _ => (file, ""),
    };
    format!("{dir}{stem}.conflict-{}{ext}", random_alphanumeric_6())
}

/// 6-char random alphanumeric token used as the conflict-copy disambiguator.
/// Cryptographic randomness isn't required — collision is the only failure
/// mode and the caller's op-log `create_document` path catches a same-path
/// retry. Derived from the random tail of a fresh ULID (Crockford base32, so
/// uppercase letters + digits only — filesystem-safe alphanumeric on every
/// host fs hiker supports). Matches the trail-waypoint disambiguator shape per
/// `docs/trails.md`. [sync-concurrent-rename-not-merged]
fn random_alphanumeric_6() -> String {
    let s = ulid::Ulid::new().to_string();
    let n = s.len();
    s[n - 6..].to_string()
}

impl SyncNode {
    /// Act on the enrollment classification for a doc we already hold locally.
    /// Path is the cross-device identity — no separate logical id rides the wire,
    /// and the responder resolves every per-doc request via `doc_id_for_path`.
    /// [sync-path-identity]
    pub(super) async fn act_on_classification(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        entry: &ManifestEntry,
        class: Classification,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let path = entry.path.as_str();
        match class {
            Classification::Identical => {
                // Same content, but two independently-seeded vaults have
                // DISJOINT Yrs lineages (different client ids over the same
                // bytes). Marking bound now and letting a later round take the
                // steady-state delta path would be a correctness bug: our state
                // vector is meaningless to a disjoint-lineage peer, so its
                // `export_since` returns its ENTIRE doc and applying it inserts a
                // SECOND copy of the body alongside ours (the duplication bug).
                //
                // The cure is to establish a SHARED lineage before any delta.
                // Pick the canonical side deterministically by device fingerprint
                // so both sides agree without negotiating; the non-canonical side
                // adopts the canonical base (content-safe — the content is
                // identical) and only THEN marks bound. The canonical side does
                // nothing this round: the peer will classify us as
                // `FastForwardAdoptPeer`, adopt us, and bind itself on the now
                // shared lineage.
                let peer_fp = self
                    .enrolled
                    .fingerprint_of(&peer_id)
                    .map(|fp| fp.0)
                    .unwrap_or_else(|| peer_id.to_string());
                let canonical_is_us = self.fingerprint().0 < peer_fp;
                if canonical_is_us {
                    // We are canonical: do NOT mark bound, do NOT pull. The peer
                    // adopts us; once both share the lineage the next round runs
                    // the delta path. Clearing a stale fork record is still safe.
                    self.clear_blocked(path);
                } else {
                    // We are non-canonical: adopt the peer's base to establish a
                    // shared lineage (identical content, so nothing is lost),
                    // then mark bound. Only after this is the delta path safe.
                    self.adopt_from_peer(peer_id, local, path).await?;
                    self.mark_bound(path);
                    self.clear_blocked(path);
                    report.bound.push(path.to_string());
                    report.converged.push(path.to_string());
                }
            }
            Classification::FastForwardAdoptPeer => {
                // First contact and we are behind: there is no shared lineage to
                // merge a delta onto yet, so adopt the peer's canonical base and
                // re-express our (fast-forward: none) divergence on it. Once
                // bound, later rounds take the steady-state delta path above.
                // [sync-lineage-adoption]
                self.adopt_from_peer(peer_id, local, path).await?;
                self.mark_bound(path);
                self.clear_blocked(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
            }
            Classification::FastForwardPeerAdopts => {
                // The peer is behind: WE are canonical. Do NOT mark bound and do
                // NOT pull this round — being bound would make us eligible for
                // the steady-state delta path while our lineage is still disjoint
                // from the peer's, and a `DeltaRequest` across disjoint lineages
                // re-inserts the peer's whole body (the duplication bug). Instead
                // the behind peer classifies us as `FastForwardAdoptPeer` on its
                // own round, adopts our base (establishing a shared lineage), and
                // marks itself bound; we mark bound on a subsequent round when
                // the manifest entry is classified `Identical` against the now
                // shared lineage. [sync-lineage-adoption]
                self.clear_blocked(path);
            }
            Classification::Fork => {
                // Concurrent-rename collision (`sync-concurrent-rename-not-merged`):
                // a manifest entry classified Fork at a path where the peer's
                // doc and our doc have completely different histories AND our
                // local replica's accepted text is the empty string is the
                // signature of "we got here by adopting the peer's renamed-to
                // target while still holding another doc with its own content at
                // the same path." When the peer's doc has a `prior_paths` and
                // its current_hash does NOT match the empty hash, treat the
                // local doc as the loser of an LWW-on-path collision and write
                // it as a conflict-copy before adopting the peer's lineage at
                // the original path. The path the peer is renaming TO wins;
                // ours becomes `<stem>.conflict-<rand6>.<ext>`. The detection
                // is the safe choice the brief calls out: only the resolver
                // (this device) materializes a conflict-copy, so two peers each
                // colliding on the same target only produce one loser.
                if self.detect_rename_collision(local, entry)? {
                    self.write_conflict_copy(local, path)?;
                    self.adopt_theirs_from_peer(peer_id, local, path).await?;
                    self.mark_bound(path);
                    self.clear_blocked(path);
                    report.bound.push(path.to_string());
                    report.converged.push(path.to_string());
                    return Ok(());
                }
                // Otherwise: a content fork. If the user picked a resolution on
                // a prior round, act on it now instead of re-blocking; otherwise
                // block + record it for the UI. [sync-blocked-state]
                self.resolve_fork(peer_id, local, entry, report).await?;
            }
        }
        Ok(())
    }

    /// Handle a detected fork for a doc we hold locally: consume any pending
    /// resolution decision, or block + record it for the UI. With no decision
    /// set (the default) this blocks unchanged. Each resolution converges in a
    /// single round: keep-theirs / keep-both adopt the peer's lineage; keep-mine
    /// PUSHES our base for the peer to adopt (see the `KeepMine` arm), so all
    /// three resolve both sides on one click. [sync-blocked-state]
    pub(super) async fn resolve_fork(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        entry: &ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let path = entry.path.as_str();
        let decision = self.resolutions.lock().unwrap().get(path).copied();
        match decision {
            None => {
                // No decision: block the doc, stream nothing, and record it
                // persistently for the UI. [sync-blocked-state]
                self.status
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), SyncStatus::Blocked);
                let peer = self.peer_fingerprint(&peer_id);
                self.record_blocked(path, &peer);
                report.blocked.push((path.to_string(), "fork".to_string()));
            }
            Some(Resolution::KeepTheirs) => {
                // Adopt the peer's lineage, discarding our local divergence: the
                // user chose the peer's version. Pull + adopt-theirs; the
                // responder resolves the StateRequest by path. Fully convergent
                // to the peer's content on this device.
                self.adopt_theirs_from_peer(peer_id, local, path).await?;
                self.mark_bound(path);
                self.clear_blocked(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
            }
            Some(Resolution::KeepBoth) => {
                // Preserve the local version as a conflict copy alongside the
                // note (a normal indexed note via the op-log create path), THEN
                // keep-theirs: adopt the peer's lineage at the original path,
                // discarding the local branch there (it survives as the copy).
                self.write_conflict_copy(local, path)?;
                self.adopt_theirs_from_peer(peer_id, local, path).await?;
                self.mark_bound(path);
                self.clear_blocked(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
            }
            Some(Resolution::KeepMine) => {
                // Our version is canonical — and we converge BOTH sides in one
                // click by PUSHING our base so the peer adopts it. Our content is
                // unchanged; we send the peer our exact Yrs base
                // (`export_state`) for it to adopt (discarding its divergence —
                // that's what "keep mine" means).
                //
                // This is lineage-safe precisely BECAUSE the peer adopts OUR
                // actual base: after the push both sides are on our lineage →
                // shared → the steady-state delta path is now safe (no
                // cross-lineage interleave).
                //
                // The peer also clears any keep-mine it had queued when it
                // adopts, so whoever pushes first wins with no flapping (see
                // `PushAdopt` handler). [sync-blocked-state, sync-lineage-adoption]
                self.push_adopt_to_peer(peer_id, local, path).await?;
                self.mark_bound(path);
                self.clear_blocked(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
            }
        }
        Ok(())
    }

    /// Recognize a concurrent-rename collision: the peer's manifest entry has
    /// at least one `prior_paths` entry AND our local replica at the same path
    /// has its OWN distinct history (a separate doc that arrived at the same
    /// path by some other route — typically the local-side rename) AND there is
    /// no content-hash overlap (the `Fork` classification is already established
    /// by the caller). The combination is the LWW-on-path collision the spec
    /// calls out under `sync-concurrent-rename-not-merged`.
    ///
    /// `peer.prior_paths` being non-empty is the key signal: the peer is
    /// telling us "this doc used to live at one of these paths and has been
    /// renamed to where you see it now." If our local replica at the new path
    /// is some OTHER doc (its own current_hash is not in the peer's
    /// `prior_paths`-derived history either, and we have a non-empty current
    /// state), we are the loser of the LWW race. Returning `true` here makes
    /// `act_on_classification` write the local replica as a conflict-copy and
    /// adopt the peer's lineage at the original path.
    fn detect_rename_collision(
        &self,
        local: &LocalDocId,
        entry: &ManifestEntry,
    ) -> Result<bool, Error> {
        // A peer with no prior_paths isn't reporting a rename — fall back to the
        // normal content-fork flow so the user sees the fork modal.
        if entry.prior_paths.is_empty() {
            return Ok(false);
        }
        // A local doc with no body is a freshly-created shell at the target
        // path; nothing to preserve, normal adoption is fine.
        let ours_text = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Transport(format!("materialize: {e}")))?
            .text;
        if ours_text.is_empty() {
            return Ok(false);
        }
        Ok(true)
    }

    /// Write the local replica's current content to a sibling note in the vault
    /// as a fresh, indexed document — the keep-both conflict copy AND the
    /// concurrent-rename-collision landing pad. Routed through the op-log
    /// `create_document` path so it shows up like any other note (its own
    /// internal doc_id, indexed, materialized `.md`). Named
    /// `<stem>.conflict-<rand6>.<ext>` (matching the trail-waypoint
    /// disambiguator shape). [sync-blocked-state, sync-concurrent-rename-not-merged]
    pub(super) fn write_conflict_copy(&self, local: &LocalDocId, path: &str) -> Result<(), Error> {
        let text = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Transport(format!("materialize for conflict copy: {e}")))?
            .text;
        let copy_path = conflict_copy_path(path);
        self.oplog
            .create_document(&copy_path, "note", &text, &Author::User)
            .map_err(|e| Error::Transport(format!("create conflict copy: {e}")))?;
        Ok(())
    }
}
