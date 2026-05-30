//! Document lifecycle verbs on `OpLog` — create, tombstone, rename. Each
//! mutates `accepted` under one lock hold, writes the side-table row, persists
//! the Yrs delta, retains a history frame, and updates the on-disk `.md`
//! atomically. These are a second `impl OpLog` block kept here so `mod.rs`
//! stays within its file-length budget; they share the same private lock /
//! `ensure_loaded` / persistence machinery defined alongside `OpLog` in `mod.rs`.

use super::doc;
use super::error::Error;
use super::meta::{self, MetadataInsert, OpStatus};
use super::shapes::{Author, OpKind};
use super::{content_hash, now_ms, write_md_file, DocState, OpLog};

impl OpLog {
    /// Register a brand-new document (a `Create` op then a content `Replace`
    /// inserting `initial_text`). Mints a path→doc_id row, seeds and persists
    /// the Yrs Doc, writes the side-table rows, and atomically writes the
    /// initial `.md` (which equals `materialize(accepted)` by construction).
    /// Returns the new doc_id. Used by the bootstrap and create paths.
    ///
    /// status: op-log-document-shape
    /// status: op-log-disk-canonical
    pub fn create_document(
        &self,
        path: &str,
        kind: &str,
        initial_text: &str,
        author: &Author,
    ) -> Result<String, Error> {
        let doc_id = ulid::Ulid::new().to_string();
        let accepted = doc::seed_doc(kind, path, initial_text);
        let cid = accepted.client_id();
        let client_id = cid.get() as i64;
        let clock_hi = {
            use yrs::{ReadTxn, Transact};
            let txn = accepted.transact();
            txn.state_vector().get(&cid) as i64
        };
        let now = now_ms();
        // Creation is a SINGLE accepted op: the `Create` op carries the seed
        // text's full clock range (0..clock_hi) and owns the first history
        // frame, so `materialize_at(create_op)` reconstructs the note as of
        // creation. A separate content op would have no retained frame and show
        // in the version dropdown as an unloadable "version".
        let create_op_id = ulid::Ulid::new().to_string();
        let snapshot = doc::encode_full(&accepted);
        let materialized = doc::materialize(&accepted);
        let seed_op_id = create_op_id.clone();
        // Index, side-table rows, Yrs state, history frame, doc-cache insert,
        // and the `.md` all land under one lock so a concurrent writer can't
        // observe (or race) a half-registered document.
        self.locked(|inner| {
            super::store::save_yrs(&self.oplog_dir, &doc_id, &snapshot)?;
            meta::put_doc_id(&inner.index, path, &doc_id)?;
            // The `Create` op spans the seed text's clock range (0..clock_hi)
            // and is the note's first content version.
            meta::insert_metadata(
                &inner.meta,
                &MetadataInsert {
                    doc_id: &doc_id,
                    op_id: &create_op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: 0,
                    yrs_clock_hi: clock_hi,
                    author,
                    op_kind: &OpKind::Create,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            // The first history frame is a self-contained keyframe.
            super::store::append_op(
                &self.oplog_dir,
                &doc_id,
                &super::store::RetainedOp::keyframe(
                    seed_op_id,
                    &materialized.text,
                    materialized.tombstone,
                    now,
                )?,
            )?;
            // The base `.yrs` snapshot was just written, so the persisted state
            // vector is `accepted`'s current one — no `.yrslog` deltas yet. The
            // keyframe just written anchors the history delta chain.
            let persisted_sv = doc::state_vector(&accepted);
            inner.docs.insert(
                doc_id.clone(),
                DocState {
                    accepted,
                    working: None,
                    pending: Vec::new(),
                    persisted_sv,
                    last_retained_text: Some(materialized.text.clone()),
                    deltas_since_keyframe: 0,
                },
            );
            // The on-disk `.md` equals `materialize(accepted)` by construction.
            write_md_file(&self.oplog_dir, Some(path), &materialized)
        })?;
        Ok(doc_id)
    }

    /// Tombstone a document: set `meta.tombstone = true` directly on
    /// `accepted`, write a `Tombstone` side-table row, and persist the Yrs
    /// Doc. The on-disk `.md` is left in place (deletion of the file itself
    /// is the caller's concern; the op log only records the logical delete).
    ///
    /// status: op-log-op-shape
    /// status: op-log-atomic-write
    pub fn tombstone_document(&self, doc_id: &str, author: &Author) -> Result<(), Error> {
        let now = now_ms();
        // Mutate + persist under one lock so a concurrent writer can't
        // interleave between the in-memory tombstone and its disk persistence.
        // The path → doc_id mapping is kept so the history / activity feed can
        // still resolve a deleted note by path; the doc reads as tombstoned,
        // and the on-disk `.md` is left in place (file deletion is the caller's).
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (client_id, lo, hi, hash) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                doc::apply_tombstone(&state.accepted);
                let hi = doc::state_clock(&state.accepted, cid);
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                let materialized = doc::materialize(&state.accepted);
                Self::retain_frame(
                    &self.oplog_dir,
                    doc_id,
                    state,
                    op_id.clone(),
                    &materialized.text,
                    materialized.tombstone,
                    now,
                )?;
                (cid.get() as i64, lo, hi, content_hash(&materialized.text))
            };
            meta::insert_metadata(
                &inner.meta,
                &MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author,
                    op_kind: &OpKind::Tombstone,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            Ok(())
        })
    }

    /// Rename a document: update `meta.path` on `accepted`, repoint the
    /// `doc-index.db` mapping, write a `Rename { from }` side-table row, and
    /// persist the Yrs Doc. The caller is responsible for the filesystem
    /// rename of the `.md`; this records the logical rename.
    ///
    /// status: op-log-op-shape
    /// status: op-log-atomic-write
    pub fn rename_document(
        &self,
        doc_id: &str,
        new_path: &str,
        author: &Author,
    ) -> Result<(), Error> {
        let now = now_ms();
        // Mutate `accepted`, repoint the path index, and persist — all under
        // one lock so a concurrent writer can't interleave.
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (client_id, lo, hi, from, hash) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let from = doc::meta_string(&state.accepted, "path").unwrap_or_default();
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                doc::apply_rename(&state.accepted, new_path);
                let hi = doc::state_clock(&state.accepted, cid);
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                let materialized = doc::materialize(&state.accepted);
                Self::retain_frame(
                    &self.oplog_dir,
                    doc_id,
                    state,
                    op_id.clone(),
                    &materialized.text,
                    materialized.tombstone,
                    now,
                )?;
                (
                    cid.get() as i64,
                    lo,
                    hi,
                    from,
                    content_hash(&materialized.text),
                )
            };
            // Repoint the path index atomically (drops any stale row for this
            // doc), so a later note created at `from` mints its own doc.
            meta::repoint_doc(&inner.index, doc_id, new_path)?;
            meta::insert_metadata(
                &inner.meta,
                &MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author,
                    op_kind: &OpKind::Rename { from },
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            Ok(())
        })
    }
}
