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

use super::doc;
use super::error::Error;
use super::meta::{self, MetadataInsert, OpStatus};
use super::shapes::{Author, OpKind};
use super::{content_hash, now_ms, write_md_file, EditInput, OpLog};

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
        let now = now_ms();
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (advanced, client_id, lo, hi, hash, rel_path, prev_path, materialized) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let cid = state.accepted.client_id();
                // Capture the clock watermark and full SV before applying, so we
                // can both record the op's clock range and (below) encode exactly
                // the ops the update introduced for the `working` mirror.
                let lo = doc::state_clock(&state.accepted, cid);
                let before_sv = doc::state_vector(&state.accepted);
                // The path BEFORE the merge: a remote delta can carry a peer-side
                // rename (a `meta.path` op on the shared lineage), so we compare
                // this against the post-merge path to detect a rename that must
                // repoint `doc-index.db` (the `.md` write alone leaves the path
                // index stale, so a later manifest match by the new path would
                // mint a SECOND doc — content duplication).
                let prev_path = doc::meta_string(&state.accepted, "path");
                doc::apply_update(&state.accepted, doc_id, update)?;
                let after_sv = doc::state_vector(&state.accepted);
                // A no-op (already-known ops) leaves the SV unchanged — nothing
                // to persist, so return early without touching disk.
                if after_sv == before_sv {
                    (false, 0, 0, 0, String::new(), None, None, doc::materialize(&state.accepted))
                } else {
                    let hi = doc::state_clock(&state.accepted, cid);
                    // Mirror the gained ops onto the user's uncommitted overlay
                    // (if any) so the editable buffer stays `accepted + working`.
                    // Best-effort: a drift simply doesn't contribute (the disk
                    // `.md` is `materialize(accepted)` regardless).
                    if let Some(working) = &state.working {
                        let delta = doc::encode_since(&state.accepted, &before_sv);
                        let _ = doc::apply_update(working, doc_id, &delta);
                    }
                    let rel_path = doc::meta_string(&state.accepted, "path");
                    let materialized = doc::materialize(&state.accepted);
                    // Persist the Yrs delta before the metadata row that
                    // references its clock range (op-log-atomic-write step 2/3).
                    Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                    Self::retain_frame(
                        &self.oplog_dir, doc_id, state, op_id.clone(),
                        &materialized.text, materialized.tombstone, now,
                    )?;
                    (
                        true,
                        cid.get() as i64,
                        lo,
                        hi,
                        content_hash(&materialized.text),
                        rel_path,
                        prev_path,
                        materialized,
                    )
                }
            };
            if !advanced {
                return Ok(false);
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
                &MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author: &Author::Sync(device_id.to_string()),
                    op_kind: &OpKind::Replace { anchor: None },
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: Some("sync"),
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            // Step 4: the `.md` is the projection of `accepted`.
            write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            Ok(true)
        })
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
            let local_text = doc::materialize(&state.accepted).text;
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
            Ok(doc::three_way_merge(&base_text, &local_text, &canonical_text))
        })?;
        // Hop 2: reconcile by committing the merged text. The whole-file commit
        // diffs it against the now-canonical `accepted` and lands the difference
        // as `user` ops (persisting `.yrs` delta, the metadata row, history
        // frame, and `.md` atomically). When the merge equals canonical (a pure
        // fast-forward or identical content) this is a no-op.
        self.commit_text_edit(doc_id, EditInput::FullText(&merged), &Author::User, None)?;
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
        let now = now_ms();
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            // Load the peer's base into a fresh Doc and make it canonical.
            let adopted = doc::load_doc(doc_id, canonical_state)?;
            let materialized = doc::materialize(&adopted);
            let rel_path = doc::meta_string(&adopted, "path");
            let cid = adopted.client_id();
            let hi = doc::state_clock(&adopted, cid);
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
                &MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: cid.get() as i64,
                    yrs_clock_lo: 0,
                    yrs_clock_hi: hi,
                    author: &Author::Sync(device_id.to_string()),
                    op_kind: &OpKind::Replace { anchor: None },
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface: Some("sync"),
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            Ok(())
        })
    }
}
