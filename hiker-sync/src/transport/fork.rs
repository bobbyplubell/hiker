//! Fork policy: turn an [`enroll::Classification`] into action and resolve a
//! user's decision. A fork must not be auto-merged (the reason
//! `op-log-merge-conflict` exists), so this module owns the keep-mine /
//! keep-theirs / keep-both branch table and the conflict-copy naming for both
//! the keep-both path and the concurrent-rename-collision case. A
//! concurrent-rename collision is likewise never auto-resolved: it BLOCKS for
//! the same Keep mine / theirs / both verbs. [sync-concurrent-rename-not-merged]
//!
//! Pure `impl SyncNode` continuation; no items of its own. The lineage verbs
//! it calls live in [`super::lineage`].

use std::collections::HashSet;

use libp2p::PeerId;

use hiker_core::oplog::shapes::Author;
use hiker_core::oplog::sync::SameRegion;

use crate::enroll::Classification;
use crate::identity::{LocalDocId, Resolution, SyncStatus};
use crate::protocol::{ManifestEntry, Message};
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
                // DISJOINT lineages (no shared history over the same
                // bytes). Marking bound now and letting a later round take the
                // steady-state delta path would be a correctness bug: our
                // watermark is meaningless to a disjoint-lineage peer, so its
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
                // the peer's manifest entry has a `prior_paths` (it renamed a
                // doc TO this path) and our local replica at the same path is a
                // DIFFERENT doc with its own content. The two lineages are
                // disjoint and both claim the path — this is a contended change,
                // so per the spec it is NOT auto-resolved: BLOCK both for user
                // resolution (Keep mine / Keep theirs / Keep both) rather than
                // silently picking a winner. If the user already queued a
                // decision on a prior round, `resolve_rename_collision` acts on
                // it now instead of re-blocking. [sync-concurrent-rename-not-merged]
                if self.detect_rename_collision(local, entry)? {
                    return self
                        .resolve_rename_collision(peer_id, local, entry, report)
                        .await;
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
                self.record_blocked(path, "fork", &peer);
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
                // unchanged; we send the peer our exact canonical text
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
            Some(Resolution::KeepDeleted) | Some(Resolution::KeepEdit) => {
                // Delete-vs-edit verbs don't apply to a content fork; leave it
                // blocked rather than guess a lineage outcome.
                return Err(Error::Apply(format!(
                    "delete-vs-edit resolution applied to a content fork at {path}"
                )));
            }
        }
        Ok(())
    }

    /// Handle a detected concurrent-rename collision: two devices renamed
    /// DIFFERENT documents onto the SAME path while disconnected. With no queued
    /// decision (the default) this BLOCKS the doc (reason `"rename-collision"`)
    /// and records it persistently for the UI — nothing is moved, copied, or
    /// adopted until the user resolves. Once the user picks, each choice
    /// converges BOTH devices in one round (the loser's doc lands at a
    /// `conflict-`suffixed path; the winner keeps the contended path):
    ///
    /// - **Keep mine** — OUR doc keeps the path. We materialize the peer's doc
    ///   as a `conflict-` sibling locally (so its content survives + streams to
    ///   the peer as a fresh doc), then PUSH our base so the peer adopts our doc
    ///   at the path (the peer's doc yields the path).
    /// - **Keep theirs** — the PEER's doc wins the path. We move our doc aside
    ///   to a `conflict-` sibling, then adopt the peer's lineage at the path.
    ///   (This is the old silent auto-behavior, now user-chosen.)
    /// - **Keep both** — both survive at distinct paths; the loser is picked
    ///   DETERMINISTICALLY by device fingerprint (`min(fingerprint)` keeps the
    ///   path) so both devices agree without negotiating. Dispatches to the
    ///   keep-mine mechanic when we win the path, keep-theirs when the peer does.
    ///
    /// [sync-concurrent-rename-not-merged, sync-conflict-block-and-resolve]
    // status: sync-concurrent-rename-not-merged
    pub(super) async fn resolve_rename_collision(
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
                // No decision. Before re-blocking, RE-EVALUATE: the collision may
                // have CONVERGED out-of-band — resolved on the other device so the
                // two formerly-disjoint lineages now share one at this path (our
                // local replica adopted the peer's, or vice-versa). In real
                // auto-sync BOTH devices are dialers and BOTH independently
                // blocked the collision; once one side resolves and the lineages
                // converge, this side's independent block is stale and must
                // auto-clear rather than re-block forever. The collision is gone
                // exactly when this path no longer classifies as a Fork against
                // the peer (a shared content hash now exists) OR the structural
                // collision predicate no longer holds. We clear ONLY when the
                // contention is genuinely gone — a still-disjoint collision
                // re-blocks. [sync-concurrent-rename-not-merged,
                // sync-conflict-block-and-resolve]
                let ours_current = self.current_hash(&local.0)?;
                let ours_history = self.history_set(&local.0)?;
                let theirs_history: HashSet<String> =
                    entry.recent_history_hashes.iter().cloned().collect();
                let class = crate::enroll::classify(
                    &ours_current,
                    &ours_history,
                    &entry.current_hash,
                    &theirs_history,
                );
                let still_colliding = matches!(class, Classification::Fork)
                    && self.detect_rename_collision(local, entry)?;
                if !still_colliding {
                    // The collision resolved out-of-band: the lineages converged
                    // at this path. Pull the (now clean) text so our content
                    // matches and drop the stale block.
                    self.apply_delta_from_peer(
                        peer_id,
                        local,
                        path,
                        &theirs_history,
                        entry.tombstone,
                    )
                    .await?;
                    self.clear_stale_block(path);
                    report.bound.push(path.to_string());
                    report.converged.push(path.to_string());
                    return Ok(());
                }
                // Still a genuine collision and no decision: BLOCK the doc, stream
                // nothing, hold everything in place (no move / copy / adopt), and
                // record it persistently for the UI + attention badge.
                // [sync-concurrent-rename-not-merged]
                self.status
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), SyncStatus::Blocked);
                let peer = self.peer_fingerprint(&peer_id);
                self.record_blocked(path, "rename-collision", &peer);
                report
                    .blocked
                    .push((path.to_string(), "rename-collision".to_string()));
                tracing::warn!(
                    path = %path,
                    "sync: rename collision — blocking for user resolution"
                );
            }
            Some(Resolution::KeepMine) => {
                self.rename_collision_keep_mine(peer_id, local, path, report)
                    .await?;
            }
            Some(Resolution::KeepTheirs) => {
                self.rename_collision_keep_theirs(peer_id, local, path, report)
                    .await?;
            }
            Some(Resolution::KeepBoth) => {
                // Deterministic winner so BOTH devices converge to the same
                // assignment: the smaller fingerprint keeps the contended path,
                // the larger takes the `conflict-` suffix. Routed to the same
                // keep-mine / keep-theirs mechanics.
                let peer_fp = self.peer_fingerprint(&peer_id).0;
                let we_keep_path = self.fingerprint().0 < peer_fp;
                if we_keep_path {
                    self.rename_collision_keep_mine(peer_id, local, path, report)
                        .await?;
                } else {
                    self.rename_collision_keep_theirs(peer_id, local, path, report)
                        .await?;
                }
            }
            Some(Resolution::KeepDeleted) | Some(Resolution::KeepEdit) => {
                // Delete-vs-edit verbs don't apply to a rename collision; leave
                // it blocked rather than guess an outcome.
                return Err(Error::Apply(format!(
                    "delete-vs-edit resolution applied to a rename collision at {path}"
                )));
            }
        }
        Ok(())
    }

    /// Keep-mine arm of a rename collision: OUR doc keeps the contended path.
    /// The peer's doc yields, so we preserve its content as a `conflict-` sibling
    /// (fetched over the open channel, written via the op-log create path so it
    /// streams to the peer as a fresh doc) and PUSH our base so the peer adopts
    /// our doc at the path. After this both sides share OUR lineage at the path
    /// and the peer holds the `conflict-` copy. [sync-concurrent-rename-not-merged]
    async fn rename_collision_keep_mine(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let (theirs, _) = self.fetch_peer_text(peer_id, path).await?;
        self.write_conflict_copy_text(path, &theirs)?;
        self.push_adopt_to_peer(peer_id, local, path).await?;
        self.mark_bound(path);
        self.clear_blocked(path);
        report.bound.push(path.to_string());
        report.converged.push(path.to_string());
        Ok(())
    }

    /// Keep-theirs arm of a rename collision: the PEER's doc wins the contended
    /// path. We move our doc aside to a `conflict-` sibling (its content survives
    /// as a fresh local note) and adopt the peer's lineage in place at the path.
    /// [sync-concurrent-rename-not-merged]
    async fn rename_collision_keep_theirs(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        self.write_conflict_copy(local, path)?;
        self.adopt_theirs_from_peer(peer_id, local, path).await?;
        self.mark_bound(path);
        self.clear_blocked(path);
        report.bound.push(path.to_string());
        report.converged.push(path.to_string());
        Ok(())
    }

    /// Steady-state delta apply for a BOUND doc, gated on a same-region
    /// conflict check. Both sides share a lineage, so a text merge of the peer's
    /// delta would silently interleave concurrent edits — desired for DISJOINT
    /// regions, garbling for the SAME region. Before applying, run the 3-way
    /// span-overlap verdict; on an overlap, BLOCK (hold the delta, mark
    /// `Blocked`/`same-region`) instead of folding it into `accepted`. Disjoint
    /// edits and fast-forwards still auto-merge via the normal delta path.
    ///
    /// A doc already same-region-blocked from a prior round consults the user's
    /// queued [`Resolution`] here and converges BOTH devices in one round:
    /// - **KeepTheirs** — the peer's text wins the overlap;
    /// - **KeepMine** — re-assert OUR text as a fresh op so it wins forward on
    ///   the shared lineage and the peer converges to it;
    /// - **KeepBoth** — the peer's text wins at the path, ours survives as a
    ///   `conflict-` sibling note.
    ///
    /// Each first folds the peer's ops into our state vector (so the delta isn't
    /// re-offered and re-blocked next round), then writes the decisive winner
    /// text. [sync-conflict-detect-same-region, sync-conflict-block-and-resolve]
    // status: sync-conflict-detect-same-region
    pub(super) async fn sync_bound_doc(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        entry: &ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let path = entry.path.as_str();
        // A queued resolution for an already-blocked doc acts now (converges +
        // unblocks) instead of re-running the detection. Copy the decision out
        // of the lock before any await so no MutexGuard is held across it.
        let decision = self.resolutions.lock().unwrap().get(path).copied();
        if let Some(decision) = decision {
            return self
                .resolve_same_region(peer_id, local, path, decision, report)
                .await;
        }

        // Local accepted state up front: text + tombstone. A tombstone keeps the
        // last-known text, so a delete-vs-edit can be hash-invisible — the cheap
        // hash pre-check below would call it a fast-forward. So the gate also
        // triggers on a tombstone on EITHER side.
        let ours = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Apply(format!("materialize ours: {e}")))?;
        let ours_current = blake3::hash(ours.text.as_bytes()).to_hex().to_string();
        let ours_history = self.history_set(&local.0)?;
        let theirs_history: HashSet<String> =
            entry.recent_history_hashes.iter().cloned().collect();
        let both_diverged = ours_current != entry.current_hash
            && !theirs_history.contains(&ours_current)
            && !ours_history.contains(&entry.current_hash);

        // A delete-vs-edit conflict is a `Tombstone` concurrent with a `Replace`
        // on the same doc; a tombstone is hash-invisible (it keeps the text), so
        // the `both_diverged` hash test alone can't see it. We therefore fetch
        // theirs (text + tombstone) whenever the doc could be a delete-vs-edit OR
        // a same-region overlap, and run the delete-vs-edit verdict FIRST. The
        // delete-vs-edit possibility is: we are tombstoned (we may have deleted
        // while they edited), OR our state differs from theirs in any way (they
        // may have tombstoned while we edited — including the case the cheap
        // fast-forward path would otherwise silently auto-apply). A pure
        // fast-forward delete (live side never edited past the shared base) is
        // classified `NotApplicable` by the verdict and falls through to
        // auto-apply → the Phase-3 trash move. [sync-conflict-delete-vs-edit]
        let maybe_delete_vs_edit = ours.tombstone || ours_current != entry.current_hash;
        if !both_diverged && !maybe_delete_vs_edit {
            // Clean fast-forward / disjoint history (incl. a fast-forward delete:
            // we sit at the base and the peer tombstoned) → normal text apply.
            // `entry.tombstone` carries the peer's delete so a fast-forward
            // delete auto-applies (→ trash) here. If this doc was Blocked on a
            // prior round and the conflict has since converged out-of-band, drop
            // the now-stale block so it stops surfacing.
            self.apply_delta_from_peer(peer_id, local, path, &theirs_history, entry.tombstone)
                .await?;
            self.clear_stale_block(path);
            report.bound.push(path.to_string());
            report.converged.push(path.to_string());
            return Ok(());
        }

        // Fetch theirs (text + tombstone) over the open authenticated channel —
        // the same `DocContentRequest` the View-diff uses, paid only on this
        // divergence path. [sync-fork-diff]
        let (theirs, theirs_tombstone) = self.fetch_peer_text(peer_id, path).await?;

        // Delete-vs-edit FIRST: a tombstone concurrent with an edit must block
        // for Keep-deleted / Keep-edit, not be auto-folded (which would let the
        // delete silently win or the edit silently resurrect).
        // [sync-conflict-delete-vs-edit]
        let dve = self
            .oplog
            .delete_vs_edit_verdict(&local.0, &theirs, theirs_tombstone, &theirs_history)
            .map_err(|e| Error::Apply(format!("delete_vs_edit_verdict: {e}")))?;
        if matches!(dve, hiker_core::oplog::sync::DeleteVsEdit::Conflict) {
            self.status
                .lock()
                .unwrap()
                .insert(path.to_string(), SyncStatus::Blocked);
            let peer = self.peer_fingerprint(&peer_id);
            self.record_blocked(path, "delete-vs-edit", &peer);
            report
                .blocked
                .push((path.to_string(), "delete-vs-edit".to_string()));
            tracing::warn!(
                path = %path,
                "sync: delete-vs-edit conflict — blocking for user resolution"
            );
            return Ok(());
        }

        // Not a delete-vs-edit. If the hashes didn't both diverge it's a clean
        // fast-forward (incl. a pure fast-forward delete) → normal text apply.
        if !both_diverged {
            self.apply_delta_from_peer(peer_id, local, path, &theirs_history, theirs_tombstone)
                .await?;
            self.clear_stale_block(path);
            report.bound.push(path.to_string());
            report.converged.push(path.to_string());
            return Ok(());
        }

        // Both sides moved off a shared base: run the same-region overlap
        // verdict on the already-fetched peer text.
        let verdict = self
            .oplog
            .same_region_verdict(&local.0, &theirs, &theirs_history)
            .map_err(|e| Error::Apply(format!("same_region_verdict: {e}")))?;
        match verdict {
            SameRegion::CleanMerge => {
                // A real shared base exists and the divergent regions are
                // disjoint — the 3-way text merge is exact, so apply the peer's
                // text. Clear any stale block: a same-region conflict resolved on
                // the other device converges here as a clean merge once the
                // lineages re-share a base, and this side's block drops. A
                // genuinely-converged doc reaches THIS arm (it shares a base
                // again), so the NoSharedBase→block change below never strands a
                // doc that truly converged.
                self.apply_delta_from_peer(peer_id, local, path, &theirs_history, theirs_tombstone)
                    .await?;
                self.clear_stale_block(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
            }
            SameRegion::NoSharedBase => {
                // BOTH sides diverged (the not-both-diverged fast-forward cases
                // returned earlier) AND no shared base is reconstructable — the
                // base aged out of the peer's bounded `recent_history_hashes`
                // window, or its op frame aged past retention. With no base,
                // `apply_remote_update` falls back to `base = ours`, and
                // `three_way_merge(ours, ours, theirs) == theirs` would SILENTLY
                // overwrite our divergent edits. That is data loss, and it
                // contradicts `sync.md` ("no common base → fork conflict, never
                // silently merged"). So BLOCK for user resolution instead.
                //
                // Reason is `same-region` so the existing dialer dispatch routes
                // a queued resolution to `sync_bound_doc` → `resolve_same_region`
                // (keep-mine / keep-theirs / keep-both): a no-base fork on a bound
                // lineage resolves exactly as a same-region conflict does, so this
                // block is RESOLVABLE through the established path rather than a
                // dead end. [sync-conflict-detect-same-region,
                // sync-conflict-block-and-resolve]
                self.status
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), SyncStatus::Blocked);
                let peer = self.peer_fingerprint(&peer_id);
                self.record_blocked(path, "same-region", &peer);
                report
                    .blocked
                    .push((path.to_string(), "same-region".to_string()));
                tracing::warn!(
                    path = %path,
                    "sync: no reconstructable base — blocking for user resolution"
                );
            }
            SameRegion::Conflict => {
                // Same-region overlap: do NOT fold the delta into `accepted`.
                // Mark blocked + record persistently for the UI; stream nothing
                // for this doc until the user resolves it. The rest of the round
                // is unaffected (per-doc resilience). [sync-conflict-block-and-resolve]
                self.status
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), SyncStatus::Blocked);
                let peer = self.peer_fingerprint(&peer_id);
                self.record_blocked(path, "same-region", &peer);
                report
                    .blocked
                    .push((path.to_string(), "same-region".to_string()));
                tracing::warn!(
                    path = %path,
                    "sync: same-region conflict — blocking for user resolution"
                );
            }
        }
        Ok(())
    }

    /// Act on a queued resolution for a same-region-blocked BOUND doc, converging
    /// both devices in one round. The peer's ops are first folded into our
    /// `accepted` (so the delta isn't re-offered next round and we don't
    /// re-block), then the decisive winner text is written as a fresh op on the
    /// shared lineage — which streams to the peer on its next pull, so the peer
    /// converges to the same outcome rather than re-conflicting.
    /// [sync-conflict-block-and-resolve]
    async fn resolve_same_region(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        path: &str,
        decision: Resolution,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        // Capture OUR text BEFORE folding the peer delta — the keep-mine winner
        // is the local content as it stands now, not the post-merge interleave.
        // Likewise capture ours for the keep-both conflict copy.
        let ours_before = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Apply(format!("resolve materialize ours: {e}")))?
            .text;
        match decision {
            Resolution::KeepMine => {
                // Our text wins forward. We make OUR version decisively canonical
                // by re-asserting it locally and PUSHING our base to the peer so
                // it adopts it (discarding its overlapping divergence) — exactly
                // the fork keep-mine converge. Pushing (rather than relying on
                // the peer to pull + re-detect) is what stops the peer from
                // independently re-blocking on the same overlap: it adopts our
                // base and clears its block in one shot. The peer's ops are NOT
                // folded into ours first — ours stays exactly `ours_before`, and
                // the push hands the peer our clean lineage.
                self.oplog
                    .apply_user_text(&local.0, &ours_before)
                    .map_err(|e| Error::Apply(format!("keep-mine apply_user_text: {e}")))?;
                self.push_adopt_to_peer(peer_id, local, path).await?;
            }
            Resolution::KeepTheirs => {
                // Their changes win the overlap. Re-assert the peer's CURRENT
                // text as the accepted content — the decisive winner. Under the
                // text substrate there is no delta to fold first (the wire
                // carries whole-file text), so the peer's text directly becomes
                // ours. The peer is already at this text, so when it later pulls
                // us the states match (a clean no-op), no re-block.
                let (theirs, _) = self.fetch_peer_text(peer_id, path).await?;
                self.oplog
                    .apply_user_text(&local.0, &theirs)
                    .map_err(|e| Error::Apply(format!("keep-theirs apply_user_text: {e}")))?;
            }
            Resolution::KeepBoth => {
                // Theirs wins at the path; ours survives as a `conflict-` sibling
                // note. Same shape as keep-theirs for the original path, plus the
                // copy (written from the pre-fold text we captured).
                self.write_conflict_copy_text(path, &ours_before)?;
                let (theirs, _) = self.fetch_peer_text(peer_id, path).await?;
                self.oplog
                    .apply_user_text(&local.0, &theirs)
                    .map_err(|e| Error::Apply(format!("keep-both apply_user_text: {e}")))?;
            }
            Resolution::KeepDeleted | Resolution::KeepEdit => {
                // Delete-vs-edit choices are routed to `resolve_delete_vs_edit`,
                // not here — a same-region block can't take a delete-vs-edit
                // verb. Surfacing it keeps the doc blocked rather than guessing.
                return Err(Error::Apply(format!(
                    "delete-vs-edit resolution applied to a same-region block at {path}"
                )));
            }
        }
        self.mark_bound(path);
        self.clear_blocked(path);
        report.bound.push(path.to_string());
        report.converged.push(path.to_string());
        Ok(())
    }

    /// Drive a delete-vs-edit-blocked doc: re-block it (no decision) or act on
    /// the user's queued Keep-deleted / Keep-edit choice, converging both
    /// devices in one round via the same PUSH-the-winner approach keep-mine uses
    /// (the peer adopts our decisive base wholesale and clears its own block, so
    /// it can't independently re-detect the same conflict next round).
    ///
    /// - **Keep deleted** — tombstone our doc (trashing our `.md` if we were the
    ///   live side), then push our tombstoned base; the peer adopts it and its
    ///   `.md` is trashed by the adopt path's transition handling.
    /// - **Keep edit** — resurrect/keep the live edited text: make our doc that
    ///   text (clearing our tombstone if we were the deleter), then push it; the
    ///   peer adopts the live edited doc.
    ///
    /// The decisive content is computed BEFORE any mutation: the live edited
    /// text is whichever side isn't tombstoned (ours if the peer deleted, the
    /// peer's if we deleted). [sync-conflict-delete-vs-edit]
    // status: sync-conflict-delete-vs-edit
    pub(super) async fn resolve_delete_vs_edit(
        &mut self,
        peer_id: PeerId,
        local: &LocalDocId,
        entry: &ManifestEntry,
        report: &mut SyncReport,
    ) -> Result<(), Error> {
        let path = entry.path.as_str();
        let decision = self.resolutions.lock().unwrap().get(path).copied();
        let Some(decision) = decision else {
            // No decision yet. Before re-blocking, RE-EVALUATE: the conflict may
            // have CONVERGED out-of-band — resolved on the other device so both
            // sides now agree (both tombstoned, or both live at the same text).
            // In real auto-sync BOTH devices are dialers, so BOTH independently
            // blocked; once one resolves and the content converges, this side's
            // independent block is stale and must auto-clear rather than re-block
            // forever (the user shouldn't have to also click here, and a
            // re-hydrated block after restart must not wedge). We only clear when
            // the contention is GENUINELY gone — a still-live conflict re-blocks.
            // [sync-conflict-delete-vs-edit, sync-conflict-block-and-resolve]
            let (theirs, theirs_tombstone) = self.fetch_peer_text(peer_id, path).await?;
            let theirs_history: HashSet<String> =
                entry.recent_history_hashes.iter().cloned().collect();
            let dve = self
                .oplog
                .delete_vs_edit_verdict(&local.0, &theirs, theirs_tombstone, &theirs_history)
                .map_err(|e| Error::Apply(format!("re-eval delete_vs_edit_verdict: {e}")))?;
            if matches!(dve, hiker_core::oplog::sync::DeleteVsEdit::NotApplicable) {
                // The delete-vs-edit contention is gone. Apply the peer's current
                // text/tombstone (a no-op when already converged) and drop the
                // now-stale block so it stops surfacing.
                self.apply_delta_from_peer(
                    peer_id,
                    local,
                    path,
                    &theirs_history,
                    theirs_tombstone,
                )
                .await?;
                self.clear_stale_block(path);
                report.bound.push(path.to_string());
                report.converged.push(path.to_string());
                return Ok(());
            }
            // Still a live conflict and no decision: keep it blocked + recorded
            // for the UI. The record already exists from the detecting round;
            // re-record so a restart that lost it re-surfaces. [sync-blocked-state]
            self.status
                .lock()
                .unwrap()
                .insert(path.to_string(), SyncStatus::Blocked);
            let peer = self.peer_fingerprint(&peer_id);
            self.record_blocked(path, "delete-vs-edit", &peer);
            report
                .blocked
                .push((path.to_string(), "delete-vs-edit".to_string()));
            return Ok(());
        };

        // Our accepted state + the peer's, captured before any mutation.
        let ours = self
            .oplog
            .materialize_accepted(&local.0)
            .map_err(|e| Error::Apply(format!("resolve dve materialize ours: {e}")))?;
        let (theirs, theirs_tombstone) = self.fetch_peer_text(peer_id, path).await?;

        match decision {
            Resolution::KeepDeleted => {
                // The delete wins. Make our doc tombstoned + trash our `.md` (a
                // no-op trash when we already were the deleter), then push the
                // tombstoned base so the peer converges to deleted and its `.md`
                // is trashed by the adopt path's transition handling.
                if !ours.tombstone {
                    self.oplog
                        .tombstone_document_to_trash(&local.0, &Author::User)
                        .map_err(|e| {
                            Error::Apply(format!("keep-deleted tombstone_to_trash: {e}"))
                        })?;
                }
                self.push_adopt_to_peer(peer_id, local, path).await?;
            }
            Resolution::KeepEdit => {
                // Resurrect with the edit. The live edited text is whichever side
                // isn't tombstoned: ours if the peer deleted, theirs if we did.
                let edited = if ours.tombstone { theirs } else { ours.text.clone() };
                // apply_user_text resurrects a tombstoned doc (clears the
                // tombstone) and lands the edited text as the accepted content.
                self.oplog
                    .apply_user_text(&local.0, &edited)
                    .map_err(|e| Error::Apply(format!("keep-edit apply_user_text: {e}")))?;
                // If we were the deleter, our `.md` was trashed at delete time;
                // resurrecting writes it back via the commit path's `.md` write.
                self.push_adopt_to_peer(peer_id, local, path).await?;
            }
            Resolution::KeepMine | Resolution::KeepTheirs | Resolution::KeepBoth => {
                // Lineage-direction verbs don't apply to a delete-vs-edit block;
                // surface rather than guess, leaving the doc blocked.
                let _ = theirs_tombstone;
                return Err(Error::Apply(format!(
                    "lineage resolution applied to a delete-vs-edit block at {path}"
                )));
            }
        }
        self.mark_bound(path);
        self.clear_blocked(path);
        report.bound.push(path.to_string());
        report.converged.push(path.to_string());
        Ok(())
    }

    /// Fetch the peer's CURRENT accepted text + tombstone flag for `path` over
    /// the already-open authenticated channel — the same `DocContentRequest` the
    /// View-diff probe uses, but issued mid-round on the connection the dialer
    /// already holds (so no second dial / Hello). The tombstone flag is what the
    /// delete-vs-edit detection needs (a deleted doc keeps its last-known text).
    /// [sync-fork-diff, sync-conflict-delete-vs-edit]
    async fn fetch_peer_text(&mut self, peer_id: PeerId, path: &str) -> Result<(String, bool), Error> {
        let req = Message::DocContentRequest {
            path: path.to_string(),
        };
        match self.request(peer_id, req).await? {
            Message::DocContentResponse { text, tombstone } => Ok((text, tombstone)),
            other => Err(Error::Transport(format!(
                "expected DocContentResponse, got {other:?}"
            ))),
        }
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
    /// state), the two disjoint lineages both claim the path. Returning `true`
    /// here makes `act_on_classification` route to `resolve_rename_collision`,
    /// which BLOCKS for user resolution rather than auto-deciding a winner.
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
        self.write_conflict_copy_text(path, &text)
    }

    /// Write `text` as a fresh `<stem>.conflict-<rand6>.<ext>` sibling note next
    /// to `path` — the keep-both copy when the content to preserve is already in
    /// hand (the same-region resolution captures OUR text before folding the
    /// peer delta, so it can't re-materialize it from the local doc). Routed
    /// through the op-log `create_document` path like any indexed note.
    /// [sync-blocked-state, sync-conflict-block-and-resolve]
    pub(super) fn write_conflict_copy_text(&self, path: &str, text: &str) -> Result<(), Error> {
        let copy_path = conflict_copy_path(path);
        self.oplog
            .create_document(&copy_path, "note", text, &Author::User)
            .map_err(|e| Error::Transport(format!("create conflict copy: {e}")))?;
        Ok(())
    }
}
