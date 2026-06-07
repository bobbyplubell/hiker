//! The pending-queue verbs (`op-log-pending-queue` / `op-log-reorg-batch`): a
//! second `impl OpLog` block holding the producer-staging and accept/reject
//! lifecycle for not-yet-accepted agent operations. Split out of `mod.rs` so
//! that file stays within its per-file line budget; these methods share the
//! same private lock / `ensure_loaded` / persistence machinery defined
//! alongside `OpLog` in `mod.rs` and reach it through `super::`.

use super::doc;
use super::overlay;
use super::shapes;
use super::store;
use super::store::{FrameMeta, FrameSpec};
use super::meta;
use super::{
    durable_metadata, now_ms, remove_old_md_file, write_md_file,
    EditSpec, Error, OpKind, PendingOp, ProducerCtx, StageOutcome,
};

impl super::OpLog {
    /// Stage a batch of producer edits as pending ops. Each edit is recorded
    /// as a text edit (its `old_str`/`new_str` and a typed `op_kind` resolved
    /// against the session's pending view) and queued in `<doc-id>.pending`. No
    /// side-table row is written yet — pending ops are not in `accepted` until
    /// they land there on accept.
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
            // Resolve edits against this session's *pending view* (accepted +
            // the session's own queued ops), not bare `accepted`, so a follow-up
            // edit can anchor on (or diff against) content the agent staged in a
            // prior, not-yet-accepted edit — the `op-log-agent-replica` contract
            // `get_note` already reads through. Built once before the loop:
            // within one call every edit resolves against the pre-call view,
            // matching the producer's own per-edit anchor validation.
            let accepted_text = state.accepted.clone();
            let session_text = overlay::fold_session_text(
                &accepted_text,
                &state.pending,
                ctx.session_id.as_deref(),
                |_| false,
            );
            // Session-id-matching pending op ids that were folded into
            // `session_text`. When the fallback path fires for an edit, the
            // produced edit implicitly depends on these — recorded on the op so
            // `accept_pending` can refuse a cross-op accept that would drift.
            // Computed once: the pending queue doesn't grow during this loop (we
            // push only after the loop assembles each op).
            let session_predecessors: Vec<String> = state
                .pending
                .iter()
                .filter(|op| op.session_id == ctx.session_id)
                .map(|op| op.op_id.clone())
                .collect();
            for edit in edits {
                // `used_fallback` flags edits whose anchor wasn't in bare
                // `accepted`, so the edit references positions the session's
                // prior pending ops establish.
                let (op_kind, used_fallback) = match &edit.old_str {
                    // Prefer resolving the anchor against `accepted` so an
                    // independent edit stays a standalone op (per-hunk
                    // accept/reject keeps working). Fall back to the session's
                    // pending view only when the anchor isn't in `accepted` —
                    // a follow-up edit anchored on the agent's own staged-but-
                    // unaccepted content.
                    Some(old_str) => {
                        let (base_text, start, used_fallback) =
                            match doc::resolve_anchor(&accepted_text, old_str) {
                                Ok(start) => (&accepted_text, start, false),
                                Err(_) => {
                                    let start = doc::resolve_anchor(&session_text, old_str)?;
                                    (&session_text, start, true)
                                }
                            };
                        let op_kind = if shapes::is_frontmatter_range(
                            base_text,
                            start,
                            start + old_str.len(),
                        ) {
                            OpKind::SetFrontmatter
                        } else {
                            OpKind::Replace {
                                anchor: Some(shapes::AnchorHint::from_old_str(old_str)),
                            }
                        };
                        (op_kind, used_fallback)
                    }
                    // A whole-document rewrite (`write_note` / `set_frontmatter`
                    // / `apply_tag`): `new_str` is the full new file. Diff it
                    // against the pending view so the op replaces the whole
                    // `text` — never appends after the existing frontmatter
                    // fence (which would duplicate the frontmatter). An
                    // unchanged rewrite produces no op. The diff is against
                    // `session_text` (pending view), so the op depends on the
                    // session's prior pending ops whenever any exist.
                    None => {
                        if session_text == edit.new_str {
                            continue;
                        }
                        let (start, removed, _) = crate::merge::text_delta(&session_text, &edit.new_str);
                        let op_kind = if shapes::is_frontmatter_range(
                            &session_text,
                            start,
                            start + removed,
                        ) {
                            OpKind::SetFrontmatter
                        } else {
                            OpKind::Replace { anchor: None }
                        };
                        (op_kind, !session_predecessors.is_empty())
                    }
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
                    op_kind,
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
            let base = state.accepted.clone();
            if base == new_text {
                return Ok(());
            }
            let (start, removed, _) = crate::merge::text_delta(&base, new_text);
            let op_kind = if shapes::is_frontmatter_range(&base, start, start + removed) {
                OpKind::SetFrontmatter
            } else {
                OpKind::Replace { anchor: None }
            };
            let op_id = ulid::Ulid::new().to_string();
            op_ids.push(op_id.clone());
            state.pending.push(PendingOp {
                op_id,
                op_kind,
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
                // The doc id IS the path (path-identity), so a pending rename's
                // `from` is the current doc_id.
                Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let from = doc_id.to_string();
                let state = inner.docs.get_mut(doc_id).expect("just loaded");
                state.pending.push(PendingOp {
                    op_id: op_id.clone(),
                    op_kind: OpKind::Rename { from },
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
    /// batch from its document's queue. None reach `accepted`, and rejection is
    /// transient editorial state — no durable audit row (`op-log-no-oplog-db`).
    /// Returns the op ids that were rejected.
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

    /// Accept a pending op: resolve its text edit into spans (or its rename)
    /// and apply it to `accepted`, write an `accepted` side-table row, drop it
    /// from the queue, append the `.ops` history frame and the materialized `.md`.
    ///
    /// A pending `Rename` op carries its prior path in `OpKind::Rename { from }`;
    /// applying it advances `meta.path` to the new location, so on
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
        // `accepted`, appending the `.ops` history frame, writing the metadata
        // row, repointing the path index, and the `.md` write/move — runs under one
        // lock hold, so a concurrent writer can't interleave between the
        // in-memory mutation and its disk persistence (no lost update).
        self.locked(|inner| {
            // Pre-flight: a pending `Rename` whose target is already taken by a
            // *different* document is a collision. Refuse before mutating, so
            // the failed op stays queued and a reorg batch's other moves still
            // apply (partial apply per `op-log-reorg-batch`).
            //
            // The collision target is the post-apply `meta.path` read off a
            // CLONE of `accepted` after `apply_rename` — the same single source
            // of truth (`metadata["new_path"]`) the real apply and the index
            // repoint use below, so the index, the `.md` location, and
            // `meta.path` can never disagree (bug-sync-accept-pending-trusts-
            // metadata-newpath). Mirrors the clone-first collision pre-check in
            // `apply_remote_update` (sync.rs).
            {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let op = state
                    .pending
                    .iter()
                    .find(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                // Cross-op dependency guard: the op's anchor was resolved
                // against `accepted + listed predecessors` (the fallback path
                // in `stage_pending`). Accepting now while any predecessor is
                // still queued would resolve the anchor against content only
                // the predecessor establishes — or fail to resolve and silently
                // no-op (the `bug-sync-per-hunk-accept-cross-op-deps` failure
                // mode). Refuse with a clean signal that names the blockers; the
                // caller accepts or rejects them first.
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
                    // Under path-identity the doc id IS the path, so a rename's
                    // target is `metadata["new_path"]` directly (no Yrs meta).
                    let new_path = op.metadata.get("new_path").and_then(|v| v.as_str());
                    if let Some(new_path) = new_path
                        && new_path != doc_id
                        && store::ops_path(&self.oplog_dir, new_path).exists()
                    {
                        return Err(Error::Anchor(format!(
                            "rename target already occupied: {new_path}"
                        )));
                    }
                }
            }
            // Remove the op from the queue, apply it to `accepted`, materialize.
            let (materialized, op, rel_path) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let idx = state
                    .pending
                    .iter()
                    .position(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                let op = state.pending.remove(idx);
                // The post-apply path: a rename relabels the doc to `new_path`
                // (path-identity); every other op keeps the current doc_id.
                let mut rel_path = doc_id.to_string();
                if matches!(op.op_kind, OpKind::Rename { .. }) {
                    // A rename moves the document's path; no text effect.
                    if let Some(np) = op.metadata.get("new_path").and_then(|v| v.as_str()) {
                        rel_path = np.to_string();
                    }
                } else {
                    // A text edit: resolve the op's spans against the accepted
                    // text and splice them in.
                    let spans = overlay::op_spans(&state.accepted, &op).unwrap_or_default();
                    state.accepted = overlay::apply_spans_str(&state.accepted, &spans);
                    // Replay the accepted op onto the user's uncommitted overlay
                    // too, so `working` stays equal to `accepted + the user's
                    // ops`. Without this the editable buffer (`accepted +
                    // working`) would drop the just-accepted content.
                    // Best-effort: a drifted op simply doesn't contribute (the
                    // disk `.md` is `accepted` regardless — `working` is never on
                    // disk).
                    if let Some(working) = state.working.clone()
                        && let Some(wspans) = overlay::op_spans(&working, &op)
                    {
                        state.working = Some(overlay::apply_spans_str(&working, &wspans));
                    }
                }
                let materialized = state.accepted();
                store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
                (materialized, op, rel_path)
            };
            // The `.ops` history frame is the durable persistence; it carries
            // the op's self-describing metadata, and `retain_frame` appends the
            // matching regenerable `op_history` row (keyed at the OLD doc_id —
            // a rename relabel repoints it to the new path below).
            let author_wire = op.author.as_wire();
            let durable = durable_metadata(&op.metadata);
            let rename_from = match &op.op_kind {
                OpKind::Rename { from } => Some(from.as_str()),
                _ => None,
            };
            {
                let (index, state) = inner.index_and_state(&self.oplog_dir, doc_id)?;
                Self::retain_frame(
                    &self.oplog_dir, index, doc_id, state,
                    &FrameSpec {
                        op_id: &op.op_id,
                        text: &materialized.text,
                        tombstone: materialized.tombstone,
                        timestamp_ms: now,
                        meta: &FrameMeta {
                            author: &author_wire,
                            op_kind: op.op_kind.as_str(),
                            rename_from,
                            surface: Some(&op.surface),
                            session_id: op.session_id.as_deref(),
                            batch_id: op.batch_id.as_deref(),
                            metadata: &durable,
                        },
                    },
                )?;
            }
            // A Rename relabels the document: the id IS the path, so relocate
            // the path-keyed per-doc files, move the in-memory cache entry, and
            // repoint the history rows from the old path to the new. The new id
            // is `rel_path` — the same value used for the `.md` write below — so
            // the substrate files and the on-disk `.md` can never desync.
            // status: op-log-observed-move
            if matches!(op.op_kind, OpKind::Rename { .. }) && rel_path.as_str() != doc_id {
                store::move_doc_files(&self.oplog_dir, doc_id, &rel_path)?;
                if let Some(state) = inner.docs.remove(doc_id) {
                    inner.docs.insert(rel_path.clone(), state);
                }
                meta::repoint_metadata(&inner.index, doc_id, &rel_path)?;
            }
            // Write the `.md` at the doc's (post-accept) path; a Rename also
            // removes the old file once the new one is written.
            write_md_file(&self.oplog_dir, Some(&rel_path), &materialized)?;
            if let OpKind::Rename { from } = &op.op_kind
                && rel_path.as_str() != from.as_str()
            {
                remove_old_md_file(&self.oplog_dir, from)?;
            }
            Ok(())
        })
    }

    /// Reject a pending op: drop it from the queue (and its `.pending`
    /// persistence). The op never enters `accepted` and leaves NO durable
    /// history row — a rejected pending edit is transient editorial state, not
    /// committed history (`op-log-no-oplog-db`). The regenerable `op_history`
    /// index rebuilds from `.ops` (accepted history only), so a rejected op —
    /// which has no frame — is correctly absent. The rejection is observable via
    /// the pending edit disappearing from the queue, not via a history row.
    ///
    /// status: op-log-status-states
    /// status: op-log-no-oplog-db
    pub fn reject_pending(&self, doc_id: &str, op_id: &str) -> Result<(), Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let idx = state
                .pending
                .iter()
                .position(|p| p.op_id == op_id)
                .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
            state.pending.remove(idx);
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
            Ok(())
        })
    }
}
