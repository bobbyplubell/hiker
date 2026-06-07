//! The multi-device sync substrate verbs (`op-log-multi-device-sync`,
//! `op-log-sync-substrate`): whole-file TEXT export/import, the inbound
//! text-merge receive path, and lineage adoption at enrollment. These are a
//! second `impl OpLog` block kept here so `mod.rs` stays within its
//! file-length budget; they share the same private lock / `ensure_loaded` /
//! persistence machinery defined alongside `OpLog` in `mod.rs`.
//!
//! **Text on the wire (Option N).** The sync substrate ships whole-file TEXT +
//! a version hash. `accepted` and `working` are plain TEXT internally, and every
//! byte payload crossing this surface is the document's canonical `.md` text.
//! The receiver reconciles by a 3-way TEXT merge over the content-hash
//! merge-base (`sync-three-way-merge`), then lands the merged text through the
//! `commit_text_edit` path. The `_watermark`/`state_vector_bytes` args are
//! vestigial — text has no state-vector delta, so shipping the full current
//! text is correct (the receiver fast-forwards / merges by content hash).

use std::path::Path;

// Only the most heavily-used parent items are imported; the rest are reached
// via explicit `super::` (or `meta::` / `super::shapes::`) paths at their use
// sites so this file doesn't lean on a wide slice of its parent's namespace
// (per `check-splits` super-reach).
use super::doc;
use super::error::Error;
use super::meta;
use super::OpLog;
use crate::trash::Trash;

// `doc::kind_for` derives a document's kind from its path extension (the doc id
// IS the path under path-identity).

/// The verdict of [`OpLog::delete_vs_edit_verdict`]: whether a bound doc's peer
/// delta is a delete concurrent with an edit (which must BLOCK for the user to
/// pick Keep-deleted vs Keep-edit) or something the existing paths handle —
/// a clean fast-forward delete (auto-apply → trash), or not a delete-vs-edit at
/// all. `sync-conflict-delete-vs-edit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteVsEdit {
    /// One side tombstoned the doc while the other edited its text since the
    /// shared (live) base — neither causally after the other. Folding the delta
    /// would let the delete silently win (or the edit silently resurrect), so
    /// the doc BLOCKS for user resolution instead.
    Conflict,
    /// Not a delete-vs-edit conflict: either neither side is tombstoned, both
    /// are, the live side never edited past the shared base (a pure
    /// fast-forward delete of a version we already have — auto-applies → trash),
    /// or there is no reconstructable shared base. The caller falls through to
    /// the existing same-region / fast-forward / text-merge paths.
    NotApplicable,
}

/// The verdict of [`OpLog::same_region_verdict`]: whether a bound doc's peer
/// delta is a clean (auto-mergeable) change or a same-region conflict that must
/// block. `sync-conflict-detect-same-region`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameRegion {
    /// No content hash is shared between our history and the peer's recent
    /// window — there is no reconstructable common base. This is the fork
    /// signature, left to enrollment classification; the bound-doc gate does
    /// not silently merge it.
    NoSharedBase,
    /// A fast-forward or disjoint-region edit: the existing 3-way text merge
    /// applies the delta automatically with no block.
    CleanMerge,
    /// Both sides edited overlapping byte ranges since the common base —
    /// applying the delta would interleave a genuine conflict, so the doc
    /// blocks for user resolution instead.
    Conflict,
}

/// Longest run of consecutive ASCII digits in `s`. A healthy sync delta or merge
/// never LENGTHENS a numeric token — it carries the peer's real edits, whose
/// coordinates are ordinary-length. So a remote apply / adoption merge that
/// INCREASES this on a JSON `canvas` doc is the signature of a cross-lineage
/// positional interleave: two near-identical lineages splicing digits together
/// (`5828` -> `582828`). Drives the warn-level corruption probes only — it never
/// changes behavior. [sync-canvas-corruption-probe]
fn longest_digit_run(s: &str) -> usize {
    let mut max = 0usize;
    let mut cur = 0usize;
    for b in s.bytes() {
        if b.is_ascii_digit() {
            cur += 1;
            if cur > max {
                max = cur;
            }
        } else {
            cur = 0;
        }
    }
    max
}

impl OpLog {
    /// The doc's current canonical TEXT (`materialize(accepted).text` bytes) —
    /// the content a peer adopts at first contact or merges in steady state.
    /// The wire carries text, not a serialized base blob
    /// (`op-log-sync-substrate`). A peer adopting this seeds its
    /// own lineage from the same text; a peer merging it runs the 3-way text
    /// merge in [`apply_remote_update`].
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    pub fn export_state(&self, doc_id: &str) -> Result<Vec<u8>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(state.accepted().text.into_bytes())
        })
    }

    /// Decide CLEAN-MERGE vs SAME-REGION-CONFLICT for a peer's divergent text
    /// before applying its delta to a BOUND (shared-lineage) doc, via a 3-way
    /// span-overlap test (`sync-conflict-detect-same-region`).
    ///
    /// - **base** = the most recent content whose hash appears in BOTH our
    ///   `.ops` history AND the peer's `peer_hashes` (`recent_history_hashes`),
    ///   reconstructed via [`materialize_at`](Self::materialize_at) of the op
    ///   that carried that hash. No shared hash → [`SameRegion::NoSharedBase`]
    ///   (the fork path — left to enrollment classification, never silently
    ///   merged here).
    /// - **ours** = `materialize(accepted).text`.
    /// - **theirs** = the peer's current text, supplied by the caller (fetched
    ///   on demand only when this cheap test is reached — see the dialer's
    ///   hash-only pre-check that avoids a fetch on a pure fast-forward).
    ///
    /// If neither side diverged from the base (`ours == base` or
    /// `theirs == base`) it is a fast-forward → [`SameRegion::CleanMerge`].
    /// Otherwise the span-overlap predicate decides: overlapping divergent byte
    /// ranges → [`SameRegion::Conflict`] (BLOCK); disjoint ranges →
    /// [`SameRegion::CleanMerge`] (let the existing text merge auto-apply). The
    /// span logic is shared with [`crate::merge::three_way_merge`] — same overlap rule,
    /// detection-only so the merge callers are unchanged.
    ///
    /// status: sync-conflict-detect-same-region
    pub fn same_region_verdict(
        &self,
        doc_id: &str,
        theirs: &str,
        peer_hashes: &std::collections::HashSet<String>,
    ) -> Result<SameRegion, Error> {
        let base_op = self.locked(|inner| {
            meta::most_recent_shared_op_id(&inner.index, doc_id, peer_hashes)
        })?;
        let Some(base_op) = base_op else {
            return Ok(SameRegion::NoSharedBase);
        };
        let base = match self.materialize_at(doc_id, &base_op)? {
            Some(content) => content.text,
            // The shared hash's op has no retained frame (aged past retention) —
            // we can't reconstruct the base, so we can't run the overlap test.
            // Treat as no usable base rather than guessing.
            None => return Ok(SameRegion::NoSharedBase),
        };
        let ours = self.materialize_accepted(doc_id)?.text;
        // Cheap pre-check: a side that didn't move off the base contributes no
        // divergent spans, so it's a fast-forward, never a same-region overlap.
        if ours == base || theirs == base {
            return Ok(SameRegion::CleanMerge);
        }
        if crate::merge::spans_overlap(&base, &ours, theirs) {
            Ok(SameRegion::Conflict)
        } else {
            Ok(SameRegion::CleanMerge)
        }
    }

    /// Decide DELETE-VS-EDIT-CONFLICT vs not, before applying a peer's delta to
    /// a BOUND (shared-lineage) doc, by distinguishing a genuinely *concurrent*
    /// delete+edit from a sequential fast-forward delete
    /// (`sync-conflict-delete-vs-edit`).
    ///
    /// A delete-vs-edit conflict is a `Tombstone` op concurrent with a `Replace`
    /// on the same doc — neither causally after the other, both diverged from
    /// the shared base. Two directions, both block:
    /// - **peer tombstoned, we edited** — `theirs_tombstone` is set while our
    ///   `accepted` carries a text edit since the shared base (and isn't itself
    ///   tombstoned);
    /// - **we tombstoned, peer edited** — our `accepted` is tombstoned while the
    ///   peer's text diverged from the shared base (and the peer isn't
    ///   tombstoned).
    ///
    /// The shared base is reconstructed exactly as [`same_region_verdict`] does
    /// (`most_recent_shared_op_id` + [`materialize_at`](Self::materialize_at)),
    /// which is what tells a concurrent divergence from a *fast-forward delete*:
    /// when the live side's text equals the base text (it never edited past the
    /// version the deleter built on), the delete is a clean sequential
    /// fast-forward — [`DeleteVsEdit::NotApplicable`], so the caller's normal
    /// delta path auto-applies it (→ the Phase-3 trash move). Only a live side
    /// whose text *diverged* from the base is a genuine concurrent edit and
    /// blocks. A base that is itself tombstoned (the shared version was already
    /// deleted) is likewise not a live-vs-delete conflict.
    ///
    /// `theirs` is the peer's current text + `theirs_tombstone` its tombstone
    /// flag, supplied by the caller (fetched on demand, like the same-region
    /// fetch). Returns [`DeleteVsEdit::NotApplicable`] when neither side is
    /// tombstoned, both are, or there is no reconstructable shared base — the
    /// caller then falls through to the same-region / fast-forward / merge
    /// paths.
    ///
    /// status: sync-conflict-delete-vs-edit
    pub fn delete_vs_edit_verdict(
        &self,
        doc_id: &str,
        theirs: &str,
        theirs_tombstone: bool,
        peer_hashes: &std::collections::HashSet<String>,
    ) -> Result<DeleteVsEdit, Error> {
        let ours = self.materialize_accepted(doc_id)?;
        // A delete-vs-edit needs EXACTLY one tombstoned side. Neither tombstoned
        // is a plain edit (same-region's job); both tombstoned is a converged
        // delete (idempotent, no conflict).
        if ours.tombstone == theirs_tombstone {
            return Ok(DeleteVsEdit::NotApplicable);
        }
        let base_op = self.locked(|inner| {
            meta::most_recent_shared_op_id(&inner.index, doc_id, peer_hashes)
        })?;
        let Some(base_op) = base_op else {
            // No reconstructable common base → not a clean delete-vs-edit; leave
            // it to enrollment classification / the existing paths.
            return Ok(DeleteVsEdit::NotApplicable);
        };
        let Some(base) = self.materialize_at(doc_id, &base_op)? else {
            return Ok(DeleteVsEdit::NotApplicable);
        };
        // The conflict is decided on TEXT: the live side (whichever isn't
        // tombstoned) must have EDITED past the shared base text. A live side
        // still equal to the base text is a pure fast-forward delete (the
        // deleter just removed a version we already hold unchanged, or a bare
        // un-delete with no edit) → auto-apply, never block.
        //
        // The base FRAME's own tombstone flag is deliberately NOT consulted: the
        // shared `content_hash` can resolve to either side's tombstone op (a
        // tombstone keeps the text, so its hash equals the base text's), which
        // would make the reconstructed frame read tombstoned even though the
        // shared *content* was the live base text. Comparing text is what
        // correctly distinguishes a concurrent edit from a fast-forward in BOTH
        // directions (we-deleted+peer-edited and peer-deleted+we-edited).
        let live_text = if ours.tombstone { theirs } else { ours.text.as_str() };
        if live_text == base.text {
            return Ok(DeleteVsEdit::NotApplicable);
        }
        Ok(DeleteVsEdit::Conflict)
    }

    /// The doc's current canonical TEXT, same as [`export_state`]. Under the
    /// text substrate there is no state-vector delta to compute, so there is
    /// no smaller "ops since the watermark" payload: the receiver merges /
    /// fast-forwards by content hash, so shipping the full current text is
    /// correct (and idempotent — an identical resend is a no-op merge). The
    /// `_watermark` arg (the peer's vestigial [`state_vector_bytes`]) is kept in
    /// the signature to minimize transport churn but no longer carries a delta.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    pub fn export_since(&self, doc_id: &str, _watermark: &[u8]) -> Result<Vec<u8>, Error> {
        self.export_state(doc_id)
    }

    /// A VESTIGIAL watermark: under the text substrate the wire carries whole
    /// files reconciled by content hash, not state-vector deltas, so there
    /// is no per-client SV to ship. Returns the doc's current content-hash bytes
    /// (a stable per-content token) purely so the transport's
    /// `DeltaRequest { state_vector }` field has something to send; the responder
    /// ignores it ([`export_since`] returns the full text regardless). Kept as a
    /// `Vec<u8>` so the `hiker-sync` signatures don't churn.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    pub fn state_vector_bytes(&self, doc_id: &str) -> Result<Vec<u8>, Error> {
        let text = self.materialize_accepted(doc_id)?.text;
        Ok(super::content_hash(&text).into_bytes())
    }

    /// The inbound receive path: reconcile a remote device's whole-file TEXT
    /// into this doc as a 3-way TEXT merge (`op-log-sync-substrate`).
    /// `peer_bytes` is the peer's document text (UTF-8). `peer_tombstone` is the
    /// peer's delete flag (a deleted doc keeps its last-known text, so text alone
    /// can't carry it). `peer_hashes` is the peer's recent content-hash window,
    /// used to recover the merge-base the SAME way the conflict gates do.
    ///
    /// The transport has already GATED this call: a genuine same-region overlap
    /// or delete-vs-edit conflict BLOCKS before reaching here
    /// (`same_region_verdict` / `delete_vs_edit_verdict`), so what arrives is a
    /// clean fast-forward, a disjoint merge, or a fast-forward delete. The merge
    /// is therefore the proven, deterministic [`crate::merge::three_way_merge`]:
    ///
    /// - **base** = the most recent content whose hash is in BOTH our `.ops`
    ///   history and `peer_hashes` (`most_recent_shared_op_id` + `materialize_at`),
    ///   exactly as the gates reconstruct it. With no shared base (e.g. the
    ///   server path ships no hashes) we fall back to `base = ours`, which makes
    ///   the merge a pure fast-forward to the peer's text — the safe behavior the
    ///   gate's clean classification implies.
    /// - **ours** = `materialize(accepted).text`. **theirs** = `peer_bytes`.
    /// - the merged text lands as a `sync:<device>`-authored `user`-class commit
    ///   through [`commit_text_edit`](super::OpLog::commit_text_edit), which
    ///   reuses the whole persist/frame/metadata/`.md` path AND the `working`
    ///   overlay reconcile. A fast-forward (`ours == base`) yields `merged ==
    ///   theirs`; identical content is a no-op.
    ///
    /// A `peer_tombstone` that is a live → deleted transition tombstones the doc
    /// (authored `sync:<device>`) and moves the lingering `.md` to trash. A
    /// converged delete (both sides
    /// tombstoned) is a no-op. Renames are NOT seen here — text carries no path;
    /// the transport conveys a rename via the manifest path and applies it with
    /// an explicit `rename_document`.
    ///
    /// Returns `true` when content advanced, `false` on a no-op — the same
    /// contract as before.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    /// status: op-log-atomic-write
    pub fn apply_remote_update(
        &self,
        doc_id: &str,
        peer_bytes: &[u8],
        peer_tombstone: bool,
        device_id: &str,
        peer_hashes: &std::collections::HashSet<String>,
    ) -> Result<bool, Error> {
        let peer_text = String::from_utf8(peer_bytes.to_vec())
            .map_err(|e| Error::Anchor(format!("remote update is not UTF-8 text: {e}")))?;
        let ours = self.materialize_accepted(doc_id)?;

        // A converged delete: both sides tombstoned. Idempotent no-op (no
        // transition, nothing to trash again).
        if peer_tombstone && ours.tombstone {
            return Ok(false);
        }
        // A fast-forward delete: the peer deleted a version we hold and we did
        // not concurrently edit (the delete-vs-edit gate already classified a
        // concurrent edit as a conflict and blocked it). Tombstone our doc as a
        // `sync:<device>` op and move the lingering `.md` to trash — a
        // recoverable transition.
        if peer_tombstone && !ours.tombstone {
            return self.apply_sync_tombstone(doc_id, device_id);
        }

        // Live merge. Recover the content-hash merge-base exactly as the gates
        // do; with no shared base, fall back to `ours` (→ fast-forward to the
        // peer's text). A peer text equal to ours is a no-op.
        let base = self.merge_base_text(doc_id, peer_hashes)?.unwrap_or_else(|| ours.text.clone());
        let is_canvas = doc::kind_for(doc_id) == "canvas";
        let merged = crate::merge::three_way_merge(&base, &ours.text, &peer_text);
        // Canvas-corruption probe: a healthy text merge never produces a digit
        // run longer than either input held; if it does on a canvas doc, the
        // positional re-application spliced numeric tokens. Warn only — behavior
        // is unchanged. [sync-canvas-corruption-probe]
        if is_canvas {
            let merged_run = longest_digit_run(&merged);
            let input_run = longest_digit_run(&ours.text).max(longest_digit_run(&peer_text));
            if merged_run > input_run && merged_run >= 8 {
                tracing::warn!(
                    target: "hiker::sync",
                    doc_id,
                    peer = %device_id,
                    input_run,
                    merged_run,
                    "sync: a remote text merge LENGTHENED a digit run in a canvas doc \
                     ({input_run}->{merged_run}) — positional interleave (the canvas corruption)."
                );
            }
        }
        // Land the merged text through the shared commit path, authored
        // `sync:<device>`. This persists the `.ops` frame and the `.md`
        // atomically, mirrors onto the `working` overlay, and clears a tombstone if the
        // merged text resurrects a deleted doc. Returns false on an identical
        // no-op (e.g. a re-sent fast-forward we already hold).
        self.commit_text_edit(
            doc_id,
            super::EditInput::FullText(&merged),
            &super::shapes::Author::Sync(device_id.to_string()),
            Some("sync"),
        )
    }

    /// The content-hash merge-base text for `(doc_id, peer_hashes)`: the most
    /// recent content whose hash appears in BOTH our `.ops` history and the
    /// peer's recent window, reconstructed via [`materialize_at`](Self::materialize_at)
    /// — the same base [`same_region_verdict`](Self::same_region_verdict) uses.
    /// `None` when there is no reconstructable shared base (the fork / no-hashes
    /// case; the caller falls back to `ours`).
    fn merge_base_text(
        &self,
        doc_id: &str,
        peer_hashes: &std::collections::HashSet<String>,
    ) -> Result<Option<String>, Error> {
        let base_op = self.locked(|inner| {
            meta::most_recent_shared_op_id(&inner.index, doc_id, peer_hashes)
        })?;
        let Some(base_op) = base_op else {
            return Ok(None);
        };
        Ok(self.materialize_at(doc_id, &base_op)?.map(|c| c.text))
    }

    /// Tombstone `doc_id` as a `sync:<device>` op and move its lingering `.md`
    /// to trash — the inbound fast-forward-delete landing. Mirrors
    /// [`tombstone_document_to_trash`](Self::tombstone_document_to_trash) but
    /// authored by the syncing device. Returns `true` (the delete advanced
    /// state).
    fn apply_sync_tombstone(&self, doc_id: &str, device_id: &str) -> Result<bool, Error> {
        // Capture the recoverable artifact (last-known text + path) under the
        // lock while the doc is still live, then tombstone (its own lock), then
        // the fs trash move runs outside the lock — the same staged shape
        // `tombstone_document_to_trash` uses.
        let pending_trash = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let materialized = state.accepted();
            // The doc id IS the path (path-identity).
            Ok(Some(RemoteDeleteToTrash {
                rel: doc_id.to_string(),
                content: materialized.text,
                doc_id: doc_id.to_string(),
            }))
        })?;
        self.tombstone_document(doc_id, &super::shapes::Author::Sync(device_id.to_string()))?;
        if let Some(t) = pending_trash {
            t.move_to_trash(super::vault_root_of(&self.oplog_dir))?;
        }
        Ok(true)
    }

    /// Adopt a peer's canonical TEXT at first contact (`sync-lineage-adoption`),
    /// PRESERVING our local divergence. The peer ships text, so "adoption" is a
    /// 3-way TEXT merge over the shared pre-divergence seed — there is no base-blob
    /// swap, no lineage tower:
    ///
    /// 1. read our effective local text (the `working` overlay if present, else
    ///    `accepted`) as `ours` — uncommitted typing must flow into the merge;
    /// 2. recover the shared seed (our `.ops` history's first keyframe — the
    ///    content both devices started from at path-match) as `base`; with none,
    ///    fall back to `ours` (so the merge keeps the full local text);
    /// 3. `merged = three_way_merge(base, ours, theirs=canonical_text)` and
    ///    commit it through the whole-file path ([`commit_text_edit`]) as `user`
    ///    ops on the text `accepted`. Disjoint edits both survive; an
    ///    overlap resolves to the canonical (peer) content; a fast-forward /
    ///    identical content is a no-op.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    /// status: op-log-atomic-write
    pub fn adopt_lineage(&self, doc_id: &str, canonical_text: &[u8]) -> Result<(), Error> {
        let canonical_text = String::from_utf8(canonical_text.to_vec())
            .map_err(|e| Error::Anchor(format!("adopt_lineage canonical is not UTF-8 text: {e}")))?;
        // Capture our effective local text (working overlay if dirty, else
        // accepted) and the shared pre-divergence seed under one lock; the merge
        // + commit run on their own locks (non-reentrant discipline).
        let is_canvas = doc::kind_for(doc_id) == "canvas";
        let (local_text, base_text) = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let local_text = state.working_text().to_string();
            // The shared seed: our `.ops` chain's first retained keyframe. No
            // recoverable seed → treat local as its own base (keep all of ours).
            let base_text = super::store::load_ops(&self.oplog_dir, doc_id)?
                .first()
                .map(|frame| frame.decode(""))
                .transpose()?
                .unwrap_or_else(|| local_text.clone());
            Ok((local_text, base_text))
        })?;
        let merged = crate::merge::three_way_merge(&base_text, &local_text, &canonical_text);
        // Canvas-corruption probe: a healthy 3-way merge never grows a digit run
        // past either input's; on a canvas doc that signals a numeric splice.
        // Warn only. [sync-canvas-corruption-probe]
        if is_canvas {
            let merged_run = longest_digit_run(&merged);
            let input_run = longest_digit_run(&local_text).max(longest_digit_run(&canonical_text));
            if merged_run > input_run && merged_run >= 8 {
                tracing::warn!(
                    target: "hiker::sync",
                    doc_id,
                    input_run,
                    merged_run,
                    "sync: lineage-adoption 3-way merge LENGTHENED a digit run in a canvas doc \
                     ({input_run}->{merged_run}) — the divergence re-application spliced numeric \
                     tokens during fork/adoption reconcile."
                );
            }
        }
        // Commit the merged text as `user` ops; a fast-forward / identical
        // content is a no-op. This also reconciles `working` and rewrites `.md`.
        self.commit_text_edit(doc_id, super::EditInput::FullText(&merged), &super::shapes::Author::User, None)?;
        Ok(())
    }

    /// Tombstone a doc AND move its lingering `.md` to trash — the local
    /// keep-deleted resolution primitive. Wraps [`tombstone_document`] with the
    /// same recoverable trash move [`apply_remote_update`] does on a remote
    /// tombstone transition, so a user who resolves a delete-vs-edit conflict to
    /// "keep deleted" gets the file removed (recoverably) rather than left as a
    /// ghost. A no-op trash move when the doc was already tombstoned (the file
    /// is already gone) or the `.md` is already absent. The fs move runs after
    /// the oplog lock is released, mirroring `apply_remote_update`.
    ///
    /// status: sync-conflict-delete-vs-edit
    pub fn tombstone_document_to_trash(
        &self,
        doc_id: &str,
        author: &super::shapes::Author,
    ) -> Result<(), Error> {
        // Capture the recoverable artifact (last-known text + path) BEFORE the
        // tombstone, under the lock, while the doc is still live.
        let pending_trash = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let materialized = state.accepted();
            if materialized.tombstone {
                // Already deleted — nothing to trash.
                return Ok(None);
            }
            // The doc id IS the path (path-identity).
            Ok(Some(RemoteDeleteToTrash {
                rel: doc_id.to_string(),
                content: materialized.text,
                doc_id: doc_id.to_string(),
            }))
        })?;
        // The tombstone takes its own lock (non-reentrant), then the fs move
        // runs outside it.
        self.tombstone_document(doc_id, author)?;
        if let Some(t) = pending_trash {
            t.move_to_trash(super::vault_root_of(&self.oplog_dir))?;
        }
        Ok(())
    }

    /// Adopt a peer's canonical TEXT wholesale, DISCARDING this device's local
    /// divergence — the "keep theirs" fork-resolution primitive (also the
    /// keep-deleted / keep-edit converge over `PushAdopt`). Unlike
    /// [`adopt_lineage`] (which 3-way-merges local edits back on top), after this
    /// the doc materializes exactly the peer's chosen version. The peer ships
    /// text + a `tombstone` flag, so this is a plain whole-file
    /// commit authored `sync:<device>` — no base-blob swap.
    ///
    /// When `tombstone` is set, the peer's chosen version is a DELETE
    /// (keep-deleted): we tombstone the doc as a `sync:<device>` op and move the
    /// lingering `.md` to trash (a no-op when we were already deleted). When
    /// clear, we adopt the peer's live `canonical_text` (resurrecting a
    /// tombstoned doc if the peer's version is live — keep-edit). A no-op when we
    /// already hold the chosen state.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-sync-substrate
    /// status: op-log-atomic-write
    pub fn adopt_lineage_theirs(
        &self,
        doc_id: &str,
        canonical_text: &[u8],
        tombstone: bool,
        device_id: &str,
    ) -> Result<(), Error> {
        let canonical_text = String::from_utf8(canonical_text.to_vec()).map_err(|e| {
            Error::Anchor(format!("adopt_lineage_theirs canonical is not UTF-8 text: {e}"))
        })?;
        // A keep-deleted converge: adopt the peer's DELETE. First adopt the
        // peer's last-known text (so both sides' recoverable artifact matches —
        // the deleted doc converges on ONE last-known content, not two), then
        // tombstone our doc as a `sync:<device>` op and trash the lingering
        // `.md`. Idempotent: a no-op when we were already tombstoned.
        if tombstone {
            if self.materialize_accepted(doc_id)?.tombstone {
                return Ok(());
            }
            self.commit_text_edit(
                doc_id,
                super::EditInput::FullText(&canonical_text),
                &super::shapes::Author::Sync(device_id.to_string()),
                Some("sync"),
            )?;
            self.apply_sync_tombstone(doc_id, device_id)?;
            return Ok(());
        }
        // Commit the peer's live text wholesale, dropping any local divergence:
        // the `commit_text_edit` diff against our current `accepted` collapses
        // our branch into the peer's content and resurrects a tombstoned doc if
        // the peer's chosen version is live. A no-op when we already hold it.
        self.commit_text_edit(
            doc_id,
            super::EditInput::FullText(&canonical_text),
            &super::shapes::Author::Sync(device_id.to_string()),
            Some("sync"),
        )?;
        Ok(())
    }
}

/// A remote tombstone that landed on a path-bound doc whose local `.md` still
/// exists. Captured inside [`OpLog::apply_remote_update`]'s lock and acted on
/// after release, so the trash filesystem move stays out of the op-log critical
/// section. Mirrors the offline-delete reconcile
/// (`core::ops::op_writes::reconcile_one_doc`): the recoverable artifact is the
/// document's last known content (`materialize(accepted).text`), the entry
/// references the `doc_id` so restore rebinds `path → doc_id` and recovers
/// history, and the `.ops` history is retained keyed by `doc_id` rather than
/// purged.
struct RemoteDeleteToTrash {
    rel: String,
    content: String,
    doc_id: String,
}

impl RemoteDeleteToTrash {
    /// Move the lingering `.md` to trash. A no-op when the file is already gone
    /// — a re-applied tombstone whose file was trashed on a prior apply doesn't
    /// reach here (it carries no transition), and a manual prior removal of the
    /// file simply leaves nothing to move, so no duplicate trash entry is made.
    fn move_to_trash(&self, vault_root: &Path) -> Result<(), Error> {
        let trash = Trash::open(vault_root);
        let abs = vault_root.join(&self.rel);
        if !abs.exists() {
            tracing::debug!(
                path = %self.rel,
                "apply_remote_update: remote tombstone, local .md already absent — no trash move"
            );
            return Ok(());
        }
        // Capture the last-known content as the recoverable artifact, keyed by
        // doc_id (same shape as the offline-delete trash entry), then remove the
        // stale on-disk file so it can't be edited back into existence.
        let entry = trash
            .capture_content_in(&self.rel, &self.content, Some(self.doc_id.clone()))
            .map_err(|e| to_io_err(&e))?;
        trash.append(&entry).map_err(|e| to_io_err(&e))?;
        std::fs::remove_file(&abs)?;
        tracing::debug!(
            path = %self.rel,
            doc_id = %self.doc_id,
            "apply_remote_update: remote tombstone → local .md moved to trash, history retained"
        );
        Ok(())
    }
}

/// Map a `HikerError` from the trash machinery onto the op-log `Error`. Trash
/// failures are filesystem-shaped, so they fold into the `Io` variant (the
/// op-log error surface predates the trash dependency and has no `HikerError`
/// arm).
fn to_io_err(e: &crate::errors::HikerError) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}
