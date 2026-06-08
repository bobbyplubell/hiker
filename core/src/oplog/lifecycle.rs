//! Document lifecycle verbs on `OpLog` — create, tombstone, rename. Each
//! mutates `accepted` under one lock hold, appends a self-describing `.ops`
//! history frame (+ its regenerable `op_history` index row), and updates the
//! on-disk `.md` atomically. These are a second `impl OpLog` block kept here so
//! `mod.rs` stays within its file-length budget; they share the same private
//! lock / `ensure_loaded` / persistence machinery defined alongside `OpLog` in
//! `mod.rs`.

use super::doc::Materialized;
use super::error::Error;
use super::meta::{self, HistoryRow};
use super::shapes::{Author, OpKind};
use super::store::{FrameMeta, FrameSpec};
use super::{content_hash, now_ms, verify_md_matches, write_md_file, DocState, OpLog, SeedDisk};

impl OpLog {
    /// Register a brand-new document seeded with `initial_text`. Under
    /// path-as-identity the document id IS `path` — no ULID is minted
    /// (`op-log-path-identity`); seeds `accepted = initial_text` (tombstone
    /// false), appends the first `.ops` keyframe (+ its `op_history` index row), and
    /// atomically writes the initial `.md` (which equals `accepted` by
    /// construction). Returns the new doc_id (= `path`). The `_kind` arg is
    /// vestigial — `kind` is now derived from the path extension on demand
    /// (`doc::kind_for`), not stored. Used by the bootstrap and create paths.
    ///
    /// status: op-log-document-shape
    /// status: op-log-path-identity
    /// status: op-log-disk-canonical
    pub fn create_document(
        &self,
        path: &str,
        _kind: &str,
        initial_text: &str,
        author: &Author,
    ) -> Result<String, Error> {
        self.register_document(path, initial_text, author, SeedDisk::Write)
    }

    /// Register a document for a file that ALREADY exists on disk holding
    /// exactly `initial_text` — the bootstrap / first-open seed path. Identical
    /// to [`create_document`](Self::create_document) except it does **not**
    /// rewrite the `.md`: writing a file's own bytes back over itself gains
    /// nothing and churns its mtime (re-stamping the whole vault on first open).
    /// Instead it hashes the bytes it would have written and verifies they match
    /// the file on disk, erroring (`Error::SeedMismatch`) on any drift rather
    /// than silently overwriting.
    ///
    /// status: op-log-disk-canonical
    pub fn seed_document(
        &self,
        path: &str,
        _kind: &str,
        initial_text: &str,
        author: &Author,
    ) -> Result<String, Error> {
        self.register_document(path, initial_text, author, SeedDisk::VerifyExisting)
    }

    /// Shared body of [`create_document`](Self::create_document) and
    /// [`seed_document`](Self::seed_document): seed `accepted = initial_text`,
    /// append the first `.ops` keyframe (+ its `op_history` index row), insert
    /// the doc-cache entry, and reconcile the on-disk `.md` per `disk` — all
    /// under one lock hold.
    fn register_document(
        &self,
        path: &str,
        initial_text: &str,
        author: &Author,
        disk: SeedDisk,
    ) -> Result<String, Error> {
        let doc_id = path.to_string();
        let now = now_ms();
        // Creation is a SINGLE accepted op: the `Create` op owns the first
        // history frame, so `materialize_at(create_op)` reconstructs the note as
        // of creation. A separate content op would have no retained frame and
        // show in the version dropdown as an unloadable "version".
        let create_op_id = ulid::Ulid::new().to_string();
        let materialized = Materialized { text: initial_text.to_string(), tombstone: false };
        let seed_op_id = create_op_id.clone();
        // The history keyframe, the index row, the doc-cache insert, and the `.md` all
        // land under one lock so a concurrent writer can't observe (or race) a
        // half-registered document.
        self.locked(|inner| {
            // The first history frame is a self-contained keyframe — and the
            // document's sole durable representation (there is no separate base
            // blob). It carries the self-describing `Create` metadata.
            let author_wire = author.as_wire();
            super::store::append_op(
                &self.oplog_dir,
                &doc_id,
                &super::store::RetainedOp::keyframe(&FrameSpec {
                    op_id: &seed_op_id,
                    text: &materialized.text,
                    tombstone: materialized.tombstone,
                    timestamp_ms: now,
                    meta: &FrameMeta {
                        author: &author_wire,
                        op_kind: OpKind::Create.as_str(),
                        rename_from: None,
                        surface: None,
                        session_id: None,
                        batch_id: None,
                        metadata: &serde_json::Value::Null,
                    },
                })?,
            )?;
            // The `Create` op is the note's first content version — append its
            // row to the regenerable `op_history` query-index.
            meta::insert_history(
                &inner.index,
                &HistoryRow {
                    doc_id: &doc_id,
                    op_id: &create_op_id,
                    author_wire: &author_wire,
                    op_kind: OpKind::Create.as_str(),
                    rename_from: None,
                    timestamp_ms: now,
                    content_hash: &content_hash(&materialized.text),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            inner.docs.insert(
                doc_id.clone(),
                DocState {
                    accepted: materialized.text.clone(),
                    accepted_tombstone: false,
                    working: None,
                    pending: Vec::new(),
                    // The keyframe just written anchors the history delta chain.
                    last_retained_text: Some(materialized.text.clone()),
                    deltas_since_keyframe: 0,
                },
            );
            // The on-disk `.md` equals `accepted` by construction. The create
            // path writes it (the file may not exist yet); the seed path's file
            // is already on disk with these exact bytes, so it verifies-and-
            // skips rather than rewriting — no mtime churn.
            match disk {
                SeedDisk::Write => write_md_file(&self.oplog_dir, Some(path), &materialized),
                SeedDisk::VerifyExisting => {
                    verify_md_matches(&self.oplog_dir, Some(path), &materialized)
                }
            }
        })?;
        Ok(doc_id)
    }

    /// Tombstone a document: set `accepted_tombstone = true` and append a
    /// `Tombstone` `.ops` frame (+ its `op_history` index row). The on-disk `.md` is left in place (deletion of the file itself
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
            let author_wire = author.as_wire();
            let (index, state) = inner.index_and_state(&self.oplog_dir, doc_id)?;
            state.accepted_tombstone = true;
            let materialized = state.accepted();
            // The `.ops` history frame is the durable persistence (a tombstone
            // always retains a keyframe); it carries the `Tombstone` metadata
            // and `retain_frame` appends the matching index row.
            Self::retain_frame(
                &self.oplog_dir,
                index,
                doc_id,
                state,
                &FrameSpec {
                    op_id: &op_id,
                    text: &materialized.text,
                    tombstone: materialized.tombstone,
                    timestamp_ms: now,
                    meta: &FrameMeta {
                        author: &author_wire,
                        op_kind: OpKind::Tombstone.as_str(),
                        rename_from: None,
                        surface: None,
                        session_id: None,
                        batch_id: None,
                        metadata: &serde_json::Value::Null,
                    },
                },
            )?;
            Ok(())
        })
    }

    /// Rename a document: under path-identity the doc id IS the path, so the
    /// text is unchanged — relocate the path-keyed per-document files (`.ops` /
    /// `.pending`) to the new path, repoint the `op_history` index rows from
    /// the old path to the new, append a `Rename { from }` `.ops` frame (+ its `op_history` index row). The rename relabels the document
    /// (`op-log-observed-move`). The caller is responsible for the filesystem
    /// rename of the `.md`; this records the logical rename and moves the
    /// substrate files.
    ///
    /// status: op-log-op-shape
    /// status: op-log-observed-move
    /// status: op-log-atomic-write
    pub fn rename_document(
        &self,
        doc_id: &str,
        new_path: &str,
        author: &Author,
    ) -> Result<(), Error> {
        // A no-op rename (id already == new_path) has nothing to relocate; the
        // file move below would also be a no-op, but short-circuit so we don't
        // mint a spurious rename frame.
        if doc_id == new_path {
            return Ok(());
        }
        let now = now_ms();
        // Mutate `accepted`, persist, then relocate the path-keyed files and
        // repoint the side table — all under one lock so a concurrent writer
        // can't interleave.
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let author_wire = author.as_wire();
            // The doc id IS the path (path-identity), so the rename's prior path
            // is the current doc_id; the text is unchanged — only the path-keyed
            // files relocate (below).
            let from = doc_id.to_string();
            {
                let (index, state) = inner.index_and_state(&self.oplog_dir, doc_id)?;
                let materialized = state.accepted();
                // The `.ops` history frame is the durable persistence. Retain
                // still keys by the OLD path here — the file relocation below
                // moves every per-doc file together — and the matching index
                // row (also at the OLD path) is repointed to the new path below.
                // The frame carries the `Rename { from }` metadata.
                Self::retain_frame(
                    &self.oplog_dir,
                    index,
                    doc_id,
                    state,
                    &FrameSpec {
                        op_id: &op_id,
                        text: &materialized.text,
                        tombstone: materialized.tombstone,
                        timestamp_ms: now,
                        meta: &FrameMeta {
                            author: &author_wire,
                            op_kind: OpKind::Rename { from: from.clone() }.as_str(),
                            rename_from: Some(&from),
                            surface: None,
                            session_id: None,
                            batch_id: None,
                            metadata: &serde_json::Value::Null,
                        },
                    },
                )?;
            }
            // Relocate the path-keyed files (the doc id IS the path) and move
            // the in-memory cache entry to the new key, so subsequent verbs
            // resolve the doc at its new path.
            super::store::move_doc_files(&self.oplog_dir, doc_id, new_path)?;
            if let Some(state) = inner.docs.remove(doc_id) {
                inner.docs.insert(new_path.to_string(), state);
            }
            // Repoint the history rows (including the rename row just appended)
            // from the old path key to the new.
            meta::repoint_metadata(&inner.index, doc_id, new_path)?;
            Ok(())
        })
    }

    /// Restore a tombstoned document at `path`, recovering its full history.
    /// The inverse of [`tombstone_document`] for the trash round trip: relocate
    /// the retained doc's path-keyed files to `path` (when it differs from the
    /// retained id), repoint the `op_history` index rows, clear
    /// `meta.tombstone` on `accepted`, append a `Create` `.ops` frame (+ its `op_history` index row) marking
    /// the resurrection. The on-disk `.md` is restored
    /// by the caller (the trash fs-move); this records the logical half so the
    /// document comes back with its prior history rather than as a fresh import.
    ///
    /// `path` is the location the file is restored to — usually its original
    /// path (= the retained id), but a restore-to-new-location relabels the doc
    /// via the file move (`op-log-observed-move`). A no-op (returns `Ok`) on a
    /// doc that is already live and already at `path`, so a redundant restore
    /// mints nothing.
    ///
    /// status: vault-trash-restore
    /// status: op-log-startup-disk-reconcile
    /// status: op-log-atomic-write
    pub fn restore_document(
        &self,
        doc_id: &str,
        path: &str,
        author: &Author,
    ) -> Result<(), Error> {
        let now = now_ms();
        self.locked(|inner| {
            // Relocate the retained doc to `path` first (when restoring to a
            // new location), so a later resolve finds the doc at `path` even if
            // the tombstone-clear below is interrupted. The move + repoint are
            // idempotent; a tombstoned-but-relocated doc is the same state we
            // recover from on a crashed restore.
            let doc_id = if doc_id != path {
                super::store::move_doc_files(&self.oplog_dir, doc_id, path)?;
                if let Some(state) = inner.docs.remove(doc_id) {
                    inner.docs.insert(path.to_string(), state);
                }
                meta::repoint_metadata(&inner.index, doc_id, path)?;
                path
            } else {
                doc_id
            };
            let op_id = ulid::Ulid::new().to_string();
            let author_wire = author.as_wire();
            let (index, state) = inner.index_and_state(&self.oplog_dir, doc_id)?;
            // Already-live + already-bound → nothing to resurrect.
            if !state.accepted_tombstone {
                return Ok(());
            }
            // Clear the tombstone; the path move (above) already relabelled the
            // doc to `path` under path-identity, so there's no separate path
            // field to set on the text.
            state.accepted_tombstone = false;
            let materialized = state.accepted();
            // The restore lands as a `Create`-kinded frame (the resurrection),
            // carrying its self-describing metadata; `retain_frame` appends the
            // matching index row.
            Self::retain_frame(
                &self.oplog_dir,
                index,
                doc_id,
                state,
                &FrameSpec {
                    op_id: &op_id,
                    text: &materialized.text,
                    tombstone: materialized.tombstone,
                    timestamp_ms: now,
                    meta: &FrameMeta {
                        author: &author_wire,
                        op_kind: OpKind::Create.as_str(),
                        rename_from: None,
                        surface: None,
                        session_id: None,
                        batch_id: None,
                        metadata: &serde_json::Value::Null,
                    },
                },
            )?;
            Ok(())
        })
    }
}
