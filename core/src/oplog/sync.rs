//! The multi-device sync substrate verbs (`op-log-multi-device-sync`): plain-
//! bytes lineage export/import, the inbound Yrs-update receive path, and
//! lineage adoption at enrollment. These are a second `impl OpLog` block kept
//! here so `mod.rs` stays within its file-length budget; they share the same
//! private lock / `ensure_loaded` / persistence machinery defined alongside
//! `OpLog` in `mod.rs`.
//!
//! **Boundary discipline.** Per the module contract, no `yrs` type crosses the
//! `OpLog` surface: every signature here takes/returns only `&str`, `Vec<u8>`,
//! and `bool`. The `StateVector` encode/decode lives in `doc.rs`; this module
//! only moves opaque `Vec<u8>` payloads (the same bytes the transport encrypts
//! and ships).

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
    /// the existing same-region / fast-forward / Yrs-merge paths.
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
    /// A fast-forward or disjoint-region edit: the existing Yrs merge applies
    /// the delta automatically with no block.
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
    /// The doc's full v2 state update — `encode_state_as_update_v2(&Default)`,
    /// the same bytes as the `.yrs` base. The transport ships this when a peer
    /// has no prior watermark (first contact, or as the canonical base another
    /// device adopts). Wraps [`doc::encode_full`]; returns plain bytes.
    ///
    /// status: op-log-multi-device-sync
    pub fn export_state(&self, doc_id: &str) -> Result<Vec<u8>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(doc::encode_full(&state.accepted))
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
    /// [`SameRegion::CleanMerge`] (let the existing Yrs merge auto-apply). The
    /// span logic is shared with [`doc::three_way_merge`] — same overlap rule,
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
            meta::most_recent_shared_op_id(&inner.meta, doc_id, peer_hashes)
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
        if doc::spans_overlap(&base, &ours, theirs) {
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
            meta::most_recent_shared_op_id(&inner.meta, doc_id, peer_hashes)
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

    /// "Ops since the peer's watermark" — `encode_state_as_update_v2(&peer_sv)`
    /// — the incremental payload the transport streams once both sides share a
    /// lineage. `peer_state_vector` is the peer's [`state_vector_bytes`] (v2);
    /// it's decoded inside `doc.rs` so no yrs `StateVector` crosses this surface.
    ///
    /// status: op-log-multi-device-sync
    pub fn export_since(
        &self,
        doc_id: &str,
        peer_state_vector: &[u8],
    ) -> Result<Vec<u8>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            doc::encode_since_sv_bytes(&state.accepted, doc_id, peer_state_vector)
        })
    }

    /// The doc's current state vector encoded as v2 bytes — the watermark this
    /// device ships so a peer can compute the delta to send back
    /// ([`export_since`]). Plain bytes; the yrs `StateVector` stays in `doc.rs`.
    ///
    /// status: op-log-multi-device-sync
    pub fn state_vector_bytes(&self, doc_id: &str) -> Result<Vec<u8>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(doc::state_vector_v2(&state.accepted))
        })
    }

    /// The inbound receive path: apply a remote device's v2 update to this
    /// doc's `accepted` Doc. The Yrs-update analog of [`apply_external_edit`]
    /// (which takes disk text) — here the transport hands over the peer's
    /// `update_v2` bytes directly, so the merge is Yrs's native one rather than
    /// a text diff. Follows the `op-log-atomic-write` persistence order under
    /// one lock hold:
    ///
    /// 1. apply the update to `accepted`;
    /// 2. mirror the gained ops onto the `working` overlay if present (capture
    ///    before-SV, `encode_since`, apply — the same technique `accept_pending`
    ///    and `commit_text_edit` use, so the user's uncommitted edits stay
    ///    layered on top of the newly-arrived state);
    /// 3. only if state advanced: append the `.yrslog` delta, retain a history
    ///    frame, insert a `sync:<device>`-authored `op_metadata` row with the
    ///    new `content_hash`, and rewrite the `.md`.
    ///
    /// Returns `true` when state advanced, `false` when the update carried only
    /// already-known ops (a no-op — Yrs `apply_update` is idempotent, so this
    /// is safe to call with overlapping/duplicate payloads). A no-op writes
    /// nothing.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-atomic-write
    pub fn apply_remote_update(
        &self,
        doc_id: &str,
        update: &[u8],
        device_id: &str,
    ) -> Result<bool, Error> {
        let now = super::now_ms();
        // The locked block returns the advance flag plus, on a remote
        // tombstone *transition* (was-live → now-deleted) of a doc whose `.md`
        // still exists, the bytes + path needed to move that file to trash.
        // The fs move runs AFTER the lock is released (it's independent of the
        // oplog mutex), mirroring how the offline-delete reconcile keeps the
        // trash capture out of the op-log critical section.
        let (advanced, pending_trash): (bool, Option<RemoteDeleteToTrash>) = self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            // Pre-flight collision check: apply the update to a CLONE of
            // `accepted` so we can see the post-merge path WITHOUT mutating
            // real state. If the rename would land on a path already owned by
            // another doc, refuse — mirrors `accept_pending`'s pre-check
            // (mod.rs ~line 650). Without this, `repoint_doc` would silently
            // steal the path mapping from the existing doc and `write_md_file`
            // would overwrite its `.md` on disk
            // (bug-sync-remote-rename-overwrites-collision).
            {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let prev_path = doc::meta_string(&state.accepted, "path");
                let preview = doc::clone_doc(&state.accepted);
                doc::apply_update(&preview, doc_id, update)?;
                let preview_path = doc::meta_string(&preview, "path");
                if let Some(new_path) = preview_path
                    && prev_path.as_deref() != Some(new_path.as_str())
                    && meta::doc_id_for_path(&inner.index, &new_path)?
                        .is_some_and(|other| other != doc_id)
                {
                    return Err(Error::Anchor(format!(
                        "remote rename target already occupied: {new_path}"
                    )));
                }
            }
            let (advanced, client_id, lo, hi, hash, rel_path, prev_path, prev_tombstone, materialized) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                // Capture the full SV before applying so we can both record the
                // peer's gained clock range (per-client SV diff — the local
                // cid's clock never advances on a peer-authored update, so the
                // recorded range must be the *peer's* cid + its actual advance)
                // and (below) encode exactly the ops the update introduced for
                // the `working` mirror.
                let before_sv = doc::state_vector(&state.accepted);
                // The tombstone flag BEFORE the merge: a remote delta can carry
                // a peer-side tombstone op on the shared lineage. Comparing this
                // against the post-merge flag detects the *transition* to deleted
                // (not a re-applied/idempotent tombstone), which is the only case
                // that must move the still-present local `.md` to trash.
                // status: op-log-external-edit-sync
                let prev_tombstone = doc::materialize(&state.accepted).tombstone;
                // The path BEFORE the merge: a remote delta can carry a peer-side
                // rename (a `meta.path` op on the shared lineage), so we compare
                // this against the post-merge path to detect a rename that must
                // repoint `doc-index.db` (the `.md` write alone leaves the path
                // index stale, so a later manifest match by the new path would
                // mint a SECOND doc — content duplication).
                let prev_path = doc::meta_string(&state.accepted, "path");
                // The accepted text BEFORE the merge — the common base for the
                // `working` overlay reconciliation below, and (gated on canvas
                // kind) the digit-run corruption probe.
                let before_text = doc::materialize(&state.accepted).text;
                // Canvas-corruption probe: for a `.canvas` doc (numeric-dense
                // JSON-as-Y.Text), snapshot the longest digit run BEFORE applying
                // the peer's update so we can warn if the apply lengthens it — the
                // cross-lineage interleave signature. Gated on kind so notes (which
                // legitimately hold long digit runs: timestamps, ids) never probe.
                // [sync-canvas-corruption-probe]
                let canvas_before_run = (doc::meta_string(&state.accepted, "kind").as_deref()
                    == Some("canvas"))
                .then(|| longest_digit_run(&before_text));
                doc::apply_update(&state.accepted, doc_id, update)?;
                let after_sv = doc::state_vector(&state.accepted);
                // A no-op (already-known ops) leaves the SV unchanged — nothing
                // to persist, so return early without touching disk.
                if after_sv == before_sv {
                    (false, 0, 0, 0, String::new(), None, None, prev_tombstone, doc::materialize(&state.accepted))
                } else {
                    // The dominant cid that advanced — fixes
                    // `bug-sync-clock-range-records-local-cid`.
                    let (client_id, lo, hi) = doc::dominant_advance(&before_sv, &after_sv)
                        .expect("SV changed but no client advanced");
                    // Reconcile the user's uncommitted overlay (`working`) so the
                    // editable buffer stays `accepted + local divergence`. We do a
                    // TEXT-level three-way merge, NOT a Yrs `apply_update` of the
                    // peer delta onto `working`: `working` holds locally-authored
                    // edits the accepted-level same-region gate can't see (they're
                    // uncommitted), so a peer edit that DUPLICATES one of them (same
                    // content, different client id) would be kept by Yrs ALONGSIDE
                    // it — doubling the buffer (the dirty-buffer-sync corruption).
                    // `three_way_merge` dedupes the identical twin and shifts a
                    // genuine disjoint edit, matching the on-disk `accepted`; a real
                    // same-region conflict drops to the peer's content there. The
                    // result lands as a localized diff so the reverse binding maps
                    // the cursor through it. Best-effort: a failed reconcile leaves
                    // `working` as-is (disk `.md` is `materialize(accepted)`).
                    if let Some(working) = &state.working {
                        let old_working = doc::materialize(working).text;
                        let new_accepted = doc::materialize(&state.accepted).text;
                        let merged = doc::three_way_merge(&before_text, &old_working, &new_accepted);
                        if merged != old_working {
                            let spans = doc::multi_span_delta(&old_working, &merged);
                            doc::apply_replaces(working, &spans);
                        }
                    }
                    let rel_path = doc::meta_string(&state.accepted, "path");
                    let materialized = doc::materialize(&state.accepted);
                    // Canvas-corruption probe: a healthy delta never lengthens a
                    // digit run; if this remote apply did on a canvas doc, the two
                    // replicas are almost certainly on DISJOINT Yrs lineages and
                    // the positional merge interleaved their near-identical numeric
                    // JSON. Warn (never error / never change behavior) so it's
                    // caught in the field. [sync-canvas-corruption-probe]
                    if let Some(before_run) = canvas_before_run {
                        let after_run = longest_digit_run(&materialized.text);
                        // A legit large coordinate can be ~6 digits; an 8+ digit
                        // run that the apply GREW is near-certainly interleave.
                        if after_run > before_run && after_run >= 8 {
                            tracing::warn!(
                                target: "hiker::sync",
                                doc_id,
                                peer = %device_id,
                                path = rel_path.as_deref().unwrap_or("?"),
                                before_run,
                                after_run,
                                bytes = materialized.text.len(),
                                "sync: a remote delta apply LENGTHENED a digit run in a canvas doc \
                                 ({before_run}->{after_run}) — cross-lineage interleave (the canvas \
                                 corruption). This doc and {device_id} are on disjoint Yrs lineages; \
                                 the bound-doc delta path should not have run."
                            );
                        }
                    }
                    // Persist the Yrs delta before the metadata row that
                    // references its clock range (op-log-atomic-write step 2/3).
                    Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                    Self::retain_frame(
                        &self.oplog_dir, doc_id, state, op_id.clone(),
                        &materialized.text, materialized.tombstone, now,
                    )?;
                    (
                        true,
                        client_id,
                        lo,
                        hi,
                        super::content_hash(&materialized.text),
                        rel_path,
                        prev_path,
                        prev_tombstone,
                        materialized,
                    )
                }
            };
            if !advanced {
                return Ok((false, None));
            }
            // If the merged update carried a peer-side rename, the doc's
            // `meta.path` moved. Repoint `doc-index.db` so `doc_id_for_path`
            // resolves the NEW path to THIS same doc — otherwise a later
            // manifest path-match would mint a second doc for the same content.
            // Lineage-safe: this only updates the path→doc_id mapping for the
            // doc that already owns the rename op on the shared lineage; it
            // never binds across lineages. The old path's row is dropped by
            // `repoint_doc` so a fresh note can later reuse it.
            if let Some(new_path) = &rel_path
                && prev_path.as_deref() != Some(new_path.as_str())
            {
                meta::repoint_doc(&inner.index, doc_id, new_path)?;
            }
            // A received update is one logical `Replace` authored by the peer
            // device (an opaque positional edit, so `anchor: None`). Its clock
            // range is the span gained on this doc's client id; the side stream
            // merges rows by range per `op-log-multi-device-sync`.
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author: &super::shapes::Author::Sync(device_id.to_string()),
                    op_kind: &super::shapes::OpKind::Replace { anchor: None },
                    status: meta::OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: Some("sync"),
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            // Step 4: the `.md` is the projection of `accepted`. For a
            // tombstoned doc this writes nothing (the projection of a deleted
            // doc is "no file") — the still-present local `.md` is removed by
            // the trash move below, NOT by `write_md_file`. For a non-tombstone
            // (incl. a remote resurrect: was-tombstoned → now-live) it writes
            // the file, so an un-delete is never stranded.
            super::write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            // A remote tombstone *transition* on a path-bound doc: capture the
            // bytes + path so the caller can move the lingering `.md` to trash
            // after the lock. Only the live → deleted transition qualifies — a
            // re-applied tombstone (already `prev_tombstone`) is a no-op here,
            // so an idempotent re-delete never double-trashes.
            // status: op-log-external-edit-sync
            let pending_trash = match (&rel_path, prev_tombstone, materialized.tombstone) {
                (Some(rel), false, true) => Some(RemoteDeleteToTrash {
                    rel: rel.clone(),
                    content: materialized.text.clone(),
                    doc_id: doc_id.to_string(),
                }),
                _ => None,
            };
            Ok((true, pending_trash))
        })?;
        // Outside the oplog lock: move the lingering `.md` to trash, referencing
        // the `doc_id` so a later restore rebinds `path → doc_id` and recovers
        // history — exactly like the offline-delete reconcile
        // (`op_writes::reconcile_one_doc`) and unlike a one-way projection that
        // would leave a stale ghost file that could be edited to resurrect the
        // doc. Idempotent: a transition only fires once (re-applied tombstones
        // carry `prev_tombstone = true`), and the move is a no-op when the file
        // is already gone (already trashed on a prior apply).
        if let Some(t) = pending_trash {
            t.move_to_trash(super::vault_root_of(&self.oplog_dir))?;
        }
        Ok(advanced)
    }

    /// Adopt a peer's canonical lineage at enrollment (`sync-lineage-adoption`).
    ///
    /// Two independently-seeded Yrs Docs can never CRDT-merge into the intended
    /// text: each lineage assigns its own client ids and clocks to the *same*
    /// bytes, so a positional merge interleaves the two copies character-by-
    /// character into nonsense rather than recognizing them as equal. So a
    /// newly-bound device does not apply the peer's update onto its own Doc — it
    /// *replaces* its lineage with the peer's canonical base, then re-expresses
    /// only its local divergence as fresh `user` ops on that shared lineage:
    ///
    /// 1. read this doc's current local materialized text;
    /// 2. swap `accepted` for a fresh Doc loaded from `canonical_state` (the
    ///    peer's full v2 base) and persist it as the new `.yrs` base;
    /// 3. three-way merge our local divergence onto the canonical text over the
    ///    common pre-divergence ancestor (our `.ops` history's first keyframe —
    ///    the seed both lineages shared at path-match), then commit the merged
    ///    text through the whole-file commit path ([`apply_user_text`] →
    ///    `commit_text_edit`) as `user` ops on the canonical lineage. Disjoint
    ///    edits both survive; an overlap resolves to the canonical content. The
    ///    adopting device's pre-binding op history collapses into that one
    ///    reconciliation. (A naive canonical→local diff can't preserve the
    ///    peer's divergence — it would revert it — so the three-way merge over
    ///    the shared seed is what keeps both sides.)
    ///
    /// The non-reentrant lock forces two hops: swap + persist the base under one
    /// `locked`, then let `commit_text_edit` take its own lock for the reconcile
    /// (the same multi-hop discipline [`commit_working`] uses).
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-atomic-write
    pub fn adopt_lineage(&self, doc_id: &str, canonical_state: &[u8]) -> Result<(), Error> {
        // Hop 1: capture our local text + the shared pre-divergence seed, swap
        // the lineage to the peer's canonical base, persist it, and compute the
        // merged text. A fresh keyframe re-anchors the `.ops` chain on the next
        // commit.
        let merged = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            // Mirror `materialize_working`: the user's effective local text is
            // the working overlay if present, else accepted. Reading only
            // accepted here would silently discard uncommitted typing — the
            // working edits must flow into the three-way merge as `ours`.
            let local_doc = state.working.as_ref().unwrap_or(&state.accepted);
            let local_text = doc::materialize(local_doc).text;
            // The common ancestor: our lineage's first retained history frame is
            // a self-contained keyframe of the seed both devices started from.
            // Falling back to the local text means "no recoverable seed" → treat
            // local as its own base, so the merge keeps the full local text.
            let base_text = super::store::load_ops(&self.oplog_dir, doc_id)?
                .first()
                .map(|frame| frame.decode(""))
                .transpose()?
                .unwrap_or_else(|| local_text.clone());
            // Load the peer's base into a fresh Doc and make it canonical.
            let adopted = doc::load_doc(doc_id, canonical_state)?;
            let canonical_text = doc::materialize(&adopted).text;
            let is_canvas = doc::meta_string(&adopted, "kind").as_deref() == Some("canvas");
            // Rewrite the `.yrs` base to the adopted lineage (atomic), and clear
            // the `.yrslog`: the old deltas belong to the abandoned lineage and
            // must not replay onto the new base.
            super::store::save_yrs(&self.oplog_dir, doc_id, &doc::encode_full(&adopted))?;
            super::store::clear_yrslog(&self.oplog_dir, doc_id)?;
            // Swap the in-memory state to the adopted lineage. `working` is
            // dropped: any uncommitted edits are part of `local_text` and fold
            // back in via the merge. `persisted_sv` matches the just-written
            // base; the next history frame is forced to a keyframe.
            state.accepted = adopted;
            state.working = None;
            state.persisted_sv = doc::state_vector(&state.accepted);
            state.last_retained_text = None;
            state.deltas_since_keyframe = 0;
            // Three-way merge: canonical (peer) is the new base, our divergence
            // re-applied on top over the shared seed.
            let merged = doc::three_way_merge(&base_text, &local_text, &canonical_text);
            // Canvas-corruption probe: the text-level 3-way merge should never
            // produce a digit run longer than either input held; if it does on a
            // canvas doc, the divergence re-application spliced numeric tokens.
            // Warn only — behavior is unchanged. [sync-canvas-corruption-probe]
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
            Ok(merged)
        })?;
        // Hop 2: reconcile by committing the merged text. The whole-file commit
        // diffs it against the now-canonical `accepted` and lands the difference
        // as `user` ops (persisting `.yrs` delta, the metadata row, history
        // frame, and `.md` atomically). When the merge equals canonical (a pure
        // fast-forward or identical content) this is a no-op.
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
            let materialized = doc::materialize(&state.accepted);
            if materialized.tombstone {
                // Already deleted — nothing to trash.
                return Ok(None);
            }
            let rel = doc::meta_string(&state.accepted, "path");
            Ok(rel.map(|rel| RemoteDeleteToTrash {
                rel,
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

    /// Adopt a peer's canonical lineage, DISCARDING this device's local
    /// divergence — the "keep theirs" fork-resolution primitive. Unlike
    /// [`adopt_lineage`] (which three-way-merges local edits back on top), this
    /// replaces both the lineage AND the content with the peer's: after it the
    /// doc materializes exactly the peer's `canonical_state`. Used when the user
    /// has explicitly chosen the peer's version over their own, so the local
    /// branch must not survive. The `.yrs` base is swapped atomically and the
    /// stale `.yrslog` cleared, same as [`adopt_lineage`]; no reconciliation
    /// commit is made because the adopted base already IS the desired content.
    ///
    /// status: op-log-multi-device-sync
    /// status: op-log-atomic-write
    pub fn adopt_lineage_theirs(
        &self,
        doc_id: &str,
        canonical_state: &[u8],
        device_id: &str,
    ) -> Result<(), Error> {
        let now = super::now_ms();
        let pending_trash: Option<RemoteDeleteToTrash> = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            // The tombstone flag BEFORE the swap: adopting a tombstoned base over
            // a previously-live doc is a delete transition that must move the
            // lingering local `.md` to trash, exactly like `apply_remote_update`.
            // This is the keep-deleted convergence path — the peer adopts our
            // tombstoned base via `PushAdopt` and its ghost `.md` is trashed.
            // status: sync-conflict-delete-vs-edit
            let prev_tombstone = doc::materialize(&state.accepted).tombstone;
            // Load the peer's base into a fresh Doc and make it canonical.
            let adopted = doc::load_doc(doc_id, canonical_state)?;
            let materialized = doc::materialize(&adopted);
            let rel_path = doc::meta_string(&adopted, "path");
            // The adopted lineage replaces local accepted wholesale, so the
            // gained range is *everything* in canonical: per-client SV diff
            // against an empty SV picks the dominant authoring cid (the peer
            // who built this lineage), avoiding the
            // `bug-sync-clock-range-records-local-cid` mistake of recording the
            // local cid against a zero-width range.
            let after_sv = doc::state_vector(&adopted);
            let (cid, lo, hi) = doc::dominant_advance(&yrs::StateVector::default(), &after_sv)
                .unwrap_or_else(|| (adopted.client_id().get() as i64, 0, 0));
            // Rewrite the `.yrs` base to the adopted lineage (atomic), clearing
            // the `.yrslog` so the abandoned lineage's deltas never replay.
            super::store::save_yrs(&self.oplog_dir, doc_id, &doc::encode_full(&adopted))?;
            super::store::clear_yrslog(&self.oplog_dir, doc_id)?;
            // Swap in the adopted lineage and drop any local divergence: the
            // user chose theirs, so the local branch is gone. Force the next
            // history frame to a keyframe of the adopted content.
            state.accepted = adopted;
            state.working = None;
            state.persisted_sv = doc::state_vector(&state.accepted);
            state.last_retained_text = None;
            state.deltas_since_keyframe = 0;
            // Retain a fresh keyframe + rewrite the `.md` so the projection and
            // history reflect the adopted content (the same persistence tail
            // `apply_remote_update` runs after it advances state).
            let op_id = ulid::Ulid::new().to_string();
            Self::retain_frame(
                &self.oplog_dir,
                doc_id,
                state,
                op_id.clone(),
                &materialized.text,
                materialized.tombstone,
                now,
            )?;
            // Record the adoption as one `sync:<device>`-authored op so the
            // resolved content shows in history with its provenance.
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: cid,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author: &super::shapes::Author::Sync(device_id.to_string()),
                    op_kind: &super::shapes::OpKind::Replace { anchor: None },
                    status: meta::OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&super::content_hash(&materialized.text)),
                    surface: Some("sync"),
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            super::write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            // A live → deleted transition on a path-bound doc: capture the
            // bytes + path so the lingering `.md` is trashed after the lock,
            // mirroring `apply_remote_update`. Only fires on the transition, so
            // adopting an already-tombstoned base over a tombstoned doc is a
            // no-op here (no double-trash).
            // status: sync-conflict-delete-vs-edit
            let pending_trash = match (&rel_path, prev_tombstone, materialized.tombstone) {
                (Some(rel), false, true) => Some(RemoteDeleteToTrash {
                    rel: rel.clone(),
                    content: materialized.text.clone(),
                    doc_id: doc_id.to_string(),
                }),
                _ => None,
            };
            Ok(pending_trash)
        })?;
        if let Some(t) = pending_trash {
            t.move_to_trash(super::vault_root_of(&self.oplog_dir))?;
        }
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
/// history, and the Yrs state/history is retained keyed by `doc_id` rather than
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
