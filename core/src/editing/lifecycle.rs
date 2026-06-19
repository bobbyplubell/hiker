//! Document lifecycle verbs on `LayeredDoc` — create, tombstone, rename, restore.
//! Each mutates the in-memory `accepted`/tombstone state under one lock hold
//! and updates the canonical `.md` on disk (the durable representation now that
//! the `.ops` history engine is gone — `op-log-disk-canonical`). Version
//! history is the plain-file snapshot tree (`core::snapshot`) `write_md_file`
//! maintains alongside each write. These are a second `impl LayeredDoc` block kept
//! here so `mod.rs` stays within its file-length budget; they share the same
//! private lock / `ensure_loaded` / persistence machinery defined alongside
//! `LayeredDoc` in `mod.rs`.

use super::doc::Materialized;
use super::error::Error;
use super::shapes::Author;
use super::store::remove_doc_files;
use super::{verify_md_matches, write_md_file, DocState, LayeredDoc, SeedDisk};

impl LayeredDoc {
    /// Register a brand-new document seeded with `initial_text`. Under
    /// path-as-identity the document id IS `path` — no ULID is minted
    /// (`op-log-path-identity`); seeds `accepted = initial_text` (tombstone
    /// false) and atomically writes the initial `.md` (which equals `accepted`
    /// by construction). Returns the new doc_id (= `path`). The `_kind` arg is
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
    /// insert the doc-cache entry, and reconcile the on-disk `.md` per `disk` —
    /// all under one lock hold.
    fn register_document(
        &self,
        path: &str,
        initial_text: &str,
        author: &Author,
        disk: SeedDisk,
    ) -> Result<String, Error> {
        let _ = author; // authorship rode the deleted history frame
        let doc_id = path.to_string();
        let materialized = Materialized { text: initial_text.to_string(), tombstone: false };
        // The disk reconcile and the doc-cache insert land under one lock so a
        // concurrent writer can't observe (or race) a half-registered document.
        self.locked(|inner| {
            // Reconcile the on-disk `.md` FIRST, BEFORE caching the doc state.
            // The on-disk `.md` equals `accepted` by construction. The create
            // path writes it (the file may not exist yet); the seed path's file
            // is already on disk with these exact bytes, so it verifies-and-
            // skips rather than rewriting — no mtime churn.
            //
            // Verifying before the cache insert matters: `VerifyExisting` returns
            // `Err(SeedMismatch)` when the on-disk bytes differ from
            // `initial_text`. If we inserted first, the divergent `DocState` would
            // stay cached on that error (the closure does no rollback), and a later
            // `is_loaded() == true` would short-circuit re-seeding — leaving
            // `accepted` permanently disagreeing with the canonical `.md`. By
            // reconciling first and only inserting on success, a failed verify
            // leaves NO cached state, so the doc re-seeds cleanly next time.
            match disk {
                SeedDisk::Write => {
                    write_md_file(&self.editing_dir, Some(path), &materialized, self.retention)?
                }
                SeedDisk::VerifyExisting => {
                    verify_md_matches(&self.editing_dir, Some(path), &materialized)?
                }
            }
            inner.docs.insert(
                doc_id.clone(),
                DocState {
                    accepted: materialized.text.clone(),
                    accepted_tombstone: false,
                    working: None,
                    pending: Vec::new(),
                },
            );
            Ok(())
        })?;
        Ok(doc_id)
    }

    /// Tombstone a document: set `accepted_tombstone = true`. The on-disk `.md`
    /// is left in place (deletion of the file itself is the caller's concern;
    /// the layered model only records the logical delete in memory). The doc stays
    /// resolvable this session so the caller can still materialize its last
    /// known content (e.g. to route to trash).
    ///
    /// status: op-log-op-shape
    /// status: op-log-atomic-write
    pub fn tombstone_document(&self, doc_id: &str, author: &Author) -> Result<(), Error> {
        let _ = author;
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            state.accepted_tombstone = true;
            Ok(())
        })
    }

    /// Forget a document entirely — for a doc that should never have been
    /// tracked: a file now excluded by the composed ignore matcher
    /// (`.gitignore` / `.hikerignore` / config) that was seeded before the
    /// ignore rule existed. Removes the per-doc `.pending` file and drops the
    /// in-memory cache entry — under one lock hold.
    ///
    /// Unlike [`tombstone_document`](Self::tombstone_document) it routes nothing
    /// to trash: the on-disk file is left untouched and the path simply stops
    /// being a tracked layered document. Idempotent — forgetting an unknown /
    /// already-forgotten doc is `Ok`.
    ///
    /// status: op-log-doc-id-bootstrap
    pub fn forget_document(&self, doc_id: &str) -> Result<(), Error> {
        self.locked(|inner| {
            remove_doc_files(&self.editing_dir, doc_id)?;
            inner.docs.remove(doc_id);
            // Prune the path's plain-file snapshot history too. Without this a
            // later file created at the SAME vault path inherits this (now
            // forgotten) file's snapshots via `snapshot::list_snapshots`, so the
            // version dropdown / restore could roll the new, unrelated file back
            // to the old file's content. Snapshots are disposable cache, so a
            // failure to prune must not fail the forget — log and continue.
            if let Err(e) = crate::snapshot::remove_snapshots(self.vault_root(), doc_id) {
                tracing::warn!(
                    doc_id, error = %e,
                    "snapshot history removal failed on forget_document (non-fatal)",
                );
            }
            Ok(())
        })
    }

    /// Rename a document: under path-identity the doc id IS the path, so the
    /// text is unchanged — relocate the path-keyed `.pending` file to the new
    /// path and move the in-memory cache entry. The rename relabels the document
    /// (`op-log-observed-move`). The caller is responsible for the filesystem
    /// rename of the `.md`; this records the logical rename and moves the
    /// substrate file + the snapshot history dir.
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
        let _ = author;
        // A no-op rename (id already == new_path) has nothing to relocate.
        if doc_id == new_path {
            return Ok(());
        }
        self.locked(|inner| {
            // Ensure the doc is loaded so its in-memory state moves with it.
            Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            // Relocate the path-keyed `.pending` file (the doc id IS the path)
            // and move the in-memory cache entry to the new key, so subsequent
            // verbs resolve the doc at its new path.
            super::store::move_doc_files(&self.editing_dir, doc_id, new_path)?;
            if let Some(state) = inner.docs.remove(doc_id) {
                inner.docs.insert(new_path.to_string(), state);
            }
            // ADDITIVE: relocate the note's plain-file snapshot directory so its
            // whole-file history follows the rename (`plain-file-snapshots`).
            // Disposable cache — a failure must never fail the logical rename, so
            // log and continue.
            if let Err(e) =
                crate::snapshot::move_snapshots(self.vault_root(), doc_id, new_path)
            {
                tracing::warn!(
                    from = doc_id, to = new_path, error = %e,
                    "snapshot dir move failed (non-fatal; logical rename succeeded)",
                );
            }
            Ok(())
        })
    }

    /// Restore a tombstoned document at `path`. The inverse of
    /// [`tombstone_document`] for the trash round trip: relocate the doc's
    /// path-keyed `.pending` file to `path` (when it differs from the retained
    /// id), move the in-memory cache entry, and clear `accepted_tombstone`. The
    /// on-disk `.md` is restored by the caller (the trash fs-move); this records
    /// the logical half so the document comes back live.
    ///
    /// `path` is the location the file is restored to — usually its original
    /// path (= the retained id), but a restore-to-new-location relabels the doc
    /// via the file move (`op-log-observed-move`). A no-op (returns `Ok`) on a
    /// doc that is already live and already at `path`.
    ///
    /// status: vault-trash-restore
    /// status: op-log-atomic-write
    pub fn restore_document(
        &self,
        doc_id: &str,
        path: &str,
        author: &Author,
    ) -> Result<(), Error> {
        let _ = author;
        self.locked(|inner| {
            // Relocate the retained doc to `path` first (when restoring to a
            // new location), so a later resolve finds the doc at `path` even if
            // the tombstone-clear below is interrupted.
            let doc_id = if doc_id != path {
                super::store::move_doc_files(&self.editing_dir, doc_id, path)?;
                if let Some(state) = inner.docs.remove(doc_id) {
                    inner.docs.insert(path.to_string(), state);
                }
                if let Err(e) = crate::snapshot::move_snapshots(self.vault_root(), doc_id, path) {
                    tracing::warn!(
                        from = doc_id, to = path, error = %e,
                        "snapshot dir move failed on restore (non-fatal)",
                    );
                }
                path
            } else {
                doc_id
            };
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            // Already-live → nothing to resurrect.
            if !state.accepted_tombstone {
                return Ok(());
            }
            state.accepted_tombstone = false;
            Ok(())
        })
    }
}
