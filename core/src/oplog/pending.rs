//! The pending-queue verbs (`op-log-pending-queue` / `op-log-reorg-batch`): a
//! second `impl OpLog` block holding the producer-staging and accept/reject
//! lifecycle for not-yet-accepted agent operations. Split out of `mod.rs` so
//! that file stays within its per-file line budget; these methods share the
//! same private lock / `ensure_loaded` / persistence machinery defined
//! alongside `OpLog` in `mod.rs` and reach it through `super::`.

use super::doc;
use super::store;
use super::meta;
use super::{
    content_hash, durable_metadata, now_ms, remove_old_md_file, write_md_file,
    EditSpec, Error, OpKind, OpStatus, PendingOp, ProducerCtx, StageOutcome,
};

impl super::OpLog {
    /// Stage a batch of producer edits as pending ops. Each edit is
    /// translated to a serialized Yrs update against a clone of `accepted`
    /// (the clone is discarded) and queued in `<doc-id>.pending`. No side-
    /// table row is written yet — pending ops have no Yrs clock range until
    /// they land in `accepted` on accept.
    ///
    /// status: op-log-pending-queue
    /// status: op-log-agent-replica
    pub fn stage_pending(
        &self,
        doc_id: &str,
        edits: &[EditSpec],
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::with_capacity(edits.len());
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            // Produce edits against this session's *pending view* (accepted +
            // the session's own queued ops), not bare `accepted`, so a follow-up
            // edit can anchor on (or diff against) content the agent staged in a
            // prior, not-yet-accepted edit — the `op-log-agent-replica` contract
            // `get_note` already reads through. Each op's update is a delta
            // against this view; `materialize_pending_view` applies the session's
            // ops in order, so they compose. Built once before the loop: within
            // one call every edit resolves against the pre-call view, matching
            // the producer's own per-edit anchor validation.
            let base_doc = doc::clone_doc(&state.accepted);
            for op in &state.pending {
                if op.session_id == ctx.session_id {
                    let _ = doc::apply_update(&base_doc, doc_id, &op.yrs_update);
                }
            }
            // Session-id-matching pending op ids that were folded into
            // `base_doc`. When the fallback path fires for an edit, the
            // produced update implicitly depends on these — recorded on the
            // op so `accept_pending` can refuse a cross-op accept that would
            // drift. Computed once: the pending queue doesn't grow during
            // this loop (we push only after the loop assembles each op).
            let session_predecessors: Vec<String> = state
                .pending
                .iter()
                .filter(|op| op.session_id == ctx.session_id)
                .map(|op| op.op_id.clone())
                .collect();
            for edit in edits {
                // `used_fallback` flags edits whose anchor wasn't in bare
                // `accepted`, so the encoded Yrs update references positions
                // the session's prior pending ops establish.
                let (produced, used_fallback) = match &edit.old_str {
                    // Prefer resolving the anchor against `accepted` so an
                    // independent edit stays a standalone op (per-hunk
                    // accept/reject keeps working). Fall back to the session's
                    // pending view only when the anchor isn't in `accepted` —
                    // a follow-up edit anchored on the agent's own staged-but-
                    // unaccepted content.
                    Some(old_str) => match doc::produce_replace(&state.accepted, old_str, &edit.new_str) {
                        Ok(produced) => (produced, false),
                        Err(_) => (doc::produce_replace(&base_doc, old_str, &edit.new_str)?, true),
                    },
                    // A whole-document rewrite (`write_note` / `set_frontmatter`
                    // / `apply_tag`): `new_str` is the full new file. Diff it
                    // against the pending view so the op replaces the whole
                    // `text` — never appends after the existing frontmatter
                    // fence (which would duplicate the frontmatter). An
                    // unchanged rewrite produces no op. The diff is against
                    // `base_doc` (pending view), so the op depends on the
                    // session's prior pending ops whenever any exist.
                    None => match doc::produce_content_replace(&base_doc, &edit.new_str) {
                        Some(produced) => (produced, !session_predecessors.is_empty()),
                        None => continue,
                    },
                };
                let op_id = ulid::Ulid::new().to_string();
                op_ids.push(op_id.clone());
                let depends_on = if used_fallback {
                    session_predecessors.clone()
                } else {
                    Vec::new()
                };
                state.pending.push(PendingOp {
                    op_id,
                    yrs_update: produced.yrs_update,
                    op_kind: produced.op_kind,
                    author: ctx.author.clone(),
                    session_id: ctx.session_id.clone(),
                    surface: ctx.surface.clone(),
                    batch_id: Some(batch_id.clone()),
                    created_at_ms: now,
                    metadata: serde_json::json!({
                        "new_str": edit.new_str,
                        "old_str": edit.old_str,
                    }),
                    depends_on,
                });
            }
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)
        })?;
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// Stage a single pending content edit from a *whole new document text*
    /// (the producer already computed the full new file). Diffs against
    /// `materialize(accepted)` and queues one pending op tagged per `ctx`,
    /// sharing a fresh `batch_id`. The op-kind is `SetFrontmatter` when the
    /// change lands inside the frontmatter fence (the cluster-editor tag /
    /// `apply_tag` shape), else `Replace`. A no-op (new text == current) stages
    /// nothing and returns an empty outcome.
    ///
    /// status: op-log-pending-queue
    pub fn stage_pending_content(
        &self,
        doc_id: &str,
        new_text: &str,
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::new();
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let Some(produced) = doc::produce_content_replace(&state.accepted, new_text) else {
                return Ok(());
            };
            let op_id = ulid::Ulid::new().to_string();
            op_ids.push(op_id.clone());
            state.pending.push(PendingOp {
                op_id,
                yrs_update: produced.yrs_update,
                op_kind: produced.op_kind,
                author: ctx.author.clone(),
                session_id: ctx.session_id.clone(),
                surface: ctx.surface.clone(),
                batch_id: Some(batch_id.clone()),
                created_at_ms: now,
                metadata: serde_json::json!({ "new_content": new_text }),
                depends_on: Vec::new(),
            });
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)
        })?;
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// Stage a batch of pending `Rename` ops sharing one cross-document
    /// `batch_id` — the multi-file reorganization seam (`op-log-reorg-batch`).
    /// Each `(doc_id, new_path)` produces one pending `Rename { from }` op on
    /// its document; nothing reaches disk until [`accept_batch`](Self::accept_batch).
    /// The batch is a review/display grouping, *not* a transaction: accept
    /// applies each rename independently and skips failures (partial apply).
    ///
    /// Returns the minted `batch_id` and the per-op ids (across documents).
    /// This is the only place a `batch_id` spans documents — note-edit batches
    /// (`stage_pending`) stay within one document.
    ///
    /// status: op-log-reorg-batch
    /// status: op-log-pending-queue
    pub fn stage_pending_renames(
        &self,
        renames: &[(String, String)],
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::with_capacity(renames.len());
        for (doc_id, new_path) in renames {
            let op_id = ulid::Ulid::new().to_string();
            self.locked(|inner| {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let produced = doc::produce_rename(&state.accepted, new_path);
                state.pending.push(PendingOp {
                    op_id: op_id.clone(),
                    yrs_update: produced.yrs_update,
                    op_kind: produced.op_kind,
                    author: ctx.author.clone(),
                    session_id: ctx.session_id.clone(),
                    surface: ctx.surface.clone(),
                    batch_id: Some(batch_id.clone()),
                    created_at_ms: now,
                    metadata: serde_json::json!({ "new_path": new_path }),
                    depends_on: Vec::new(),
                });
                store::save_pending(&self.oplog_dir, doc_id, &state.pending)
            })?;
            op_ids.push(op_id);
        }
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// The `(doc_id, op_id)` pairs of every pending op across the vault sharing
    /// `batch_id`. The handle the batch accept/reject verbs resolve a reorg
    /// batch through (a `batch_id` may span documents per `op-log-reorg-batch`).
    ///
    /// status: op-log-reorg-batch
    pub fn pending_ops_in_batch(&self, batch_id: &str) -> Result<Vec<(String, String)>, Error> {
        let all = self.all_pending_ops()?;
        Ok(all
            .into_iter()
            .filter(|(_, op)| op.batch_id.as_deref() == Some(batch_id))
            .map(|(doc_id, op)| (doc_id, op.op_id))
            .collect())
    }

    /// Accept an entire reorg batch by `batch_id`: apply each pending op in
    /// the batch independently, skipping any that fail (partial apply per
    /// `op-log-reorg-batch` — a target collision on one move does not block
    /// the rest). Returns the op ids that were successfully accepted.
    ///
    /// status: op-log-reorg-batch
    pub fn accept_batch(&self, batch_id: &str) -> Result<Vec<String>, Error> {
        let batch = self.pending_ops_in_batch(batch_id)?;
        let mut accepted = Vec::new();
        for (doc_id, op_id) in batch {
            match self.accept_pending(&doc_id, &op_id) {
                Ok(()) => accepted.push(op_id),
                Err(e) => {
                    tracing::warn!(
                        batch_id,
                        doc_id,
                        op_id,
                        error = %e,
                        "oplog: reorg batch op failed to apply; skipping (partial apply)"
                    );
                }
            }
        }
        Ok(accepted)
    }

    /// Reject an entire reorg batch by `batch_id`: drop each pending op in the
    /// batch from its document's queue (writing a `rejected` audit row). None
    /// reach `accepted`. Returns the op ids that were rejected.
    ///
    /// status: op-log-reorg-batch
    pub fn reject_batch(&self, batch_id: &str) -> Result<Vec<String>, Error> {
        let batch = self.pending_ops_in_batch(batch_id)?;
        let mut rejected = Vec::new();
        for (doc_id, op_id) in batch {
            self.reject_pending(&doc_id, &op_id)?;
            rejected.push(op_id);
        }
        Ok(rejected)
    }

    /// Accept a pending op: apply its serialized update to `accepted`, write
    /// an `accepted` side-table row, drop it from the queue, persist the
    /// Yrs Doc and the materialized `.md`.
    ///
    /// A pending `Rename` op carries its prior path in `OpKind::Rename { from }`;
    /// applying the update advances `meta.path` to the new location, so on
    /// accept the doc's `.md` is written at the new path, the old `.md` is
    /// removed, and `doc-index.db` is repointed (old path dropped, new path
    /// upserted) — the file moves on disk per `op-log-reorg-batch`.
    ///
    /// status: op-log-status-states
    /// status: op-log-atomic-write
    /// status: op-log-reorg-batch
    pub fn accept_pending(&self, doc_id: &str, op_id: &str) -> Result<(), Error> {
        let now = now_ms();
        // Everything — the rename collision pre-check, applying the op to
        // `accepted`, persisting `.yrs` / the metadata row / the history frame,
        // repointing the path index, and the `.md` write/move — runs under one
        // lock hold, so a concurrent writer can't interleave between the
        // in-memory mutation and its disk persistence (no lost update).
        self.locked(|inner| {
            // Pre-flight: a pending `Rename` whose target is already taken by a
            // *different* document is a collision. Refuse before mutating, so
            // the failed op stays queued and a reorg batch's other moves still
            // apply (partial apply per `op-log-reorg-batch`).
            //
            // The collision target is the path that results from APPLYING the
            // Yrs update to a CLONE of `accepted` — NOT the op's
            // `metadata["new_path"]` field. If those disagree (corrupted
            // `.pending`, producer bug), trusting the metadata would repoint
            // the index to one path while the `.md` is written at the
            // post-apply Yrs path, desyncing the index from disk
            // (bug-sync-accept-pending-trusts-metadata-newpath). Mirrors the
            // clone-first pattern in `apply_remote_update` (sync.rs ~line 103).
            {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let op = state
                    .pending
                    .iter()
                    .find(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                // Cross-op dependency guard: the op's Yrs update was produced
                // against `accepted + listed predecessors` (the fallback path
                // in `stage_pending`). Accepting now while any predecessor is
                // still queued would land an update keyed on positions only
                // the predecessor's apply establishes — silently corrupting
                // `accepted` (the `bug-sync-per-hunk-accept-cross-op-deps`
                // failure mode). Refuse with a clean signal that names the
                // blockers; the caller accepts or rejects them first.
                let blockers: Vec<String> = op
                    .depends_on
                    .iter()
                    .filter(|pred| state.pending.iter().any(|p| &p.op_id == *pred))
                    .cloned()
                    .collect();
                if !blockers.is_empty() {
                    return Err(Error::DependsOn {
                        op_id: op_id.to_string(),
                        predecessors: blockers,
                    });
                }
                if matches!(op.op_kind, OpKind::Rename { .. }) {
                    let prev_path = doc::meta_string(&state.accepted, "path");
                    let preview = doc::clone_doc(&state.accepted);
                    doc::apply_update(&preview, doc_id, &op.yrs_update)?;
                    let preview_path = doc::meta_string(&preview, "path");
                    if let Some(new_path) = preview_path
                        && prev_path.as_deref() != Some(new_path.as_str())
                        && meta::doc_id_for_path(&inner.index, &new_path)?
                            .is_some_and(|other| other != doc_id)
                    {
                        return Err(Error::Anchor(format!(
                            "rename target already occupied: {new_path}"
                        )));
                    }
                }
            }
            // Remove the op from the queue, apply it to `accepted`, materialize.
            let (materialized, client_id, lo, hi, op, rel_path) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let idx = state
                    .pending
                    .iter()
                    .position(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                let op = state.pending.remove(idx);
                // The pending op was authored under a per-session client_id
                // (the staged pending Doc's cid), NOT the local accepted cid —
                // so a per-client SV diff records the actual (client_id, lo,
                // hi) span the apply introduced. Fixes
                // `bug-sync-clock-range-records-local-cid`.
                let before_sv = doc::state_vector(&state.accepted);
                doc::apply_update(&state.accepted, doc_id, &op.yrs_update)?;
                // Replay the accepted op onto the user's uncommitted overlay
                // too, so `working` stays equal to `accepted + the user's ops`.
                // Without this the editable buffer (`materialize(accepted +
                // working)`) would drop the just-accepted content. Best-effort:
                // a drifted op simply doesn't contribute (the disk `.md` is
                // `materialize(accepted)` regardless — `working` is never on disk).
                if let Some(working) = &state.working {
                    let _ = doc::apply_update(working, doc_id, &op.yrs_update);
                }
                let after_sv = doc::state_vector(&state.accepted);
                // A no-advance accept (e.g. an idempotent reapply) records the
                // local cid with a zero-width range — semantically "nothing new
                // landed", consistent with the side-table's per-row contract.
                let (cid, lo, hi) = doc::dominant_advance(&before_sv, &after_sv)
                    .unwrap_or_else(|| (state.accepted.client_id().get() as i64, 0, 0));
                let materialized = doc::materialize(&state.accepted);
                let rel_path = doc::meta_string(&state.accepted, "path");
                store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                Self::retain_frame(
                    &self.oplog_dir, doc_id, state, op.op_id.clone(),
                    &materialized.text, materialized.tombstone, now,
                )?;
                (materialized, cid, lo, hi, op, rel_path)
            };
            // A Rename repoints the path index (atomically) so the `.md` move
            // and later path resolution agree. The repoint target is the
            // POST-APPLY Yrs `meta.path` (`rel_path`) — the same value used
            // for the `.md` write below — so the index and the on-disk file
            // can never desync, even if the op's `metadata["new_path"]` field
            // is stale or corrupted
            // (bug-sync-accept-pending-trusts-metadata-newpath).
            if let (OpKind::Rename { .. }, Some(new_path)) = (&op.op_kind, &rel_path) {
                meta::repoint_doc(&inner.index, doc_id, new_path)?;
            }
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op.op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author: &op.author,
                    op_kind: &op.op_kind,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface: Some(&op.surface),
                    session_id: op.session_id.as_deref(),
                    batch_id: op.batch_id.as_deref(),
                    metadata: &durable_metadata(&op.metadata),
                },
            )?;
            // Write the `.md` at the doc's (post-accept) path; a Rename also
            // removes the old file once the new one is written.
            write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            if let OpKind::Rename { from } = &op.op_kind
                && rel_path.as_deref() != Some(from.as_str())
            {
                remove_old_md_file(&self.oplog_dir, from)?;
            }
            Ok(())
        })
    }

    /// Reject a pending op: drop it from the queue and write a `rejected`
    /// audit row with the serialized update bytes stashed in the row's
    /// metadata. The op never enters `accepted`.
    ///
    /// status: op-log-status-states
    pub fn reject_pending(&self, doc_id: &str, op_id: &str) -> Result<(), Error> {
        let now = now_ms();
        let op = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let idx = state
                .pending
                .iter()
                .position(|p| p.op_id == op_id)
                .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
            let op = state.pending.remove(idx);
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
            Ok(op)
        })?;
        let mut metadata = op.metadata.clone();
        if let serde_json::Value::Object(map) = &mut metadata {
            map.insert(
                "rejected_update".to_string(),
                serde_json::Value::Array(
                    op.yrs_update
                        .iter()
                        .map(|b| serde_json::Value::from(*b))
                        .collect(),
                ),
            );
        }
        self.locked(|inner| {
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op.op_id,
                    // No Yrs range — the op never landed in `accepted`.
                    yrs_client_id: 0,
                    yrs_clock_lo: 0,
                    yrs_clock_hi: 0,
                    author: &op.author,
                    op_kind: &op.op_kind,
                    status: OpStatus::Rejected,
                    timestamp_ms: now,
                    // Rejected ops never land in `accepted`, so they have no
                    // materialized content to hash.
                    content_hash: None,
                    surface: Some(&op.surface),
                    session_id: op.session_id.as_deref(),
                    batch_id: op.batch_id.as_deref(),
                    metadata: &metadata,
                },
            )
        })
    }
}
