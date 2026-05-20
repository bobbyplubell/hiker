use super::*;

impl Store {
    /// Look up the stable id for a path, or None if the note has never been
    /// indexed. Reads from `path_ids`; valid even after a rename has updated
    /// `notes.path` (the old path still resolves to the same id).
    pub fn id_for_path(&self, rel_path: &str) -> Result<Option<String>, StoreError> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM path_ids WHERE path = ?1",
                params![rel_path],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Fetch the note row for a given path, or None if not indexed.
    pub fn get_note_by_path(&self, rel_path: &str) -> Result<Option<NoteRow>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, path, content_hash, mtime, size, indexed_at, embedder_version,
                        skipped, skip_reason, last_accessed_at
                 FROM notes WHERE path = ?1",
                params![rel_path],
                map_note_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Vault-relative paths of every note the indexer flagged as
    /// `skipped` (unsupported extension, oversize, etc.). Used by the
    /// file-tree row renderer to badge skipped files without firing a
    /// per-row `get_note_by_path` query on every frame.
    pub fn list_skipped_paths(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM notes WHERE skipped = 1")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Total count of indexed notes.
    pub fn count_notes(&self) -> Result<u32, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))?;
        Ok(n.max(0) as u32)
    }

    /// Aggregate counts for the vault-home stats widget. Pulled from the
    /// notes / chunks tables in three cheap counts; queued / unsupported
    /// counts live elsewhere (queued on the indexer handle, unsupported is a
    /// vault-walk concern the home page skips for v1).
    ///
    /// status: vault-home-stats-widget
    pub fn vault_stats(&self) -> Result<VaultStats, StoreError> {
        let total_notes: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes",
            [],
            |row| row.get(0),
        )?;
        let total_chunks: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks",
            [],
            |row| row.get(0),
        )?;
        let skipped: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE skipped = 1",
            [],
            |row| row.get(0),
        )?;
        let indexed = (total_notes - skipped).max(0);
        Ok(VaultStats {
            total_notes: total_notes.max(0) as u32,
            total_chunks: total_chunks.max(0) as u32,
            indexed: indexed as u32,
            skipped: skipped.max(0) as u32,
        })
    }

    /// Top-N notes by filesystem mtime, descending. Skipped rows excluded —
    /// they're noise in a "recent activity" surface.
    ///
    /// status: vault-home-recent-modified
    pub fn recent_notes_by_mtime(
        &self,
        limit: usize,
    ) -> Result<Vec<RecentNote>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, mtime, last_accessed_at
             FROM notes
             WHERE skipped = 0
             ORDER BY mtime DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let path: String = row.get(0)?;
                let title = title_from_path(&path);
                Ok(RecentNote {
                    path,
                    title,
                    mtime: row.get(1)?,
                    last_accessed_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Top-N notes by user-open time, descending. Notes that have never been
    /// opened (`last_accessed_at IS NULL`) are excluded — the widget is "what
    /// have I been reading," empty when nothing's been opened yet.
    ///
    /// status: vault-home-recent-accessed
    pub fn recent_notes_by_access(
        &self,
        limit: usize,
    ) -> Result<Vec<RecentNote>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, mtime, last_accessed_at
             FROM notes
             WHERE skipped = 0 AND last_accessed_at IS NOT NULL
             ORDER BY last_accessed_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let path: String = row.get(0)?;
                let title = title_from_path(&path);
                Ok(RecentNote {
                    path,
                    title,
                    mtime: row.get(1)?,
                    last_accessed_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // status: note-properties-tab-content
    /// Read-only snapshot of everything the store knows about one note.
    /// Returns `Ok(None)` when the path has never been indexed (no
    /// `notes` row exists). Chunk count is computed separately — cheap
    /// per-note. Callers should also pull `changes.count_for_path` to
    /// fill the `change_count` field.
    pub fn note_properties(
        &self,
        rel_path: &str,
    ) -> Result<Option<NoteProperties>, StoreError> {
        let row = self.conn.query_row(
            "SELECT n.id, n.content_hash, n.mtime, n.size, n.indexed_at,
                    n.embedder_version, n.skipped, n.skip_reason,
                    n.last_accessed_at
             FROM notes n
             WHERE n.path = ?1",
            params![rel_path],
            |row| {
                let extension = std::path::Path::new(rel_path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_string());
                let note_id: Option<String> = row.get(0)?;
                let content_hash: Option<String> = row.get(1)?;
                let mtime: Option<i64> = row.get(2)?;
                let size: Option<i64> = row.get(3)?;
                let indexed_at: Option<i64> = row.get(4)?;
                let embedder_version: Option<String> = row.get(5)?;
                let skipped: Option<bool> = row.get(6)?;
                let skip_reason: Option<String> = row.get(7)?;
                let last_accessed_at: Option<i64> = row.get(8)?;
                Ok(NoteProperties {
                    path: rel_path.to_string(),
                    note_id,
                    path_ids_id: None,
                    mtime,
                    size,
                    content_hash,
                    extension,
                    indexed_at,
                    embedder_version,
                    skipped,
                    skip_reason,
                    chunk_count: None,
                    last_accessed_at,
                    change_count: None,
                })
            },
        );
        match row {
            Ok(props) => {
                // Pull the path_ids id in a separate query — cheap, and
                // keeps the main query simple.
                let path_ids_id: Option<String> = self.conn.query_row(
                    "SELECT id FROM path_ids WHERE path = ?1",
                    params![rel_path],
                    |row| row.get(0),
                ).optional()?;
                let chunk_count: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE note_id = ?1",
                    params![props.note_id],
                    |row| row.get(0),
                )?;
                let mut p = props;
                p.path_ids_id = path_ids_id;
                p.chunk_count = Some(chunk_count);
                Ok(Some(p))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Stamp `last_accessed_at` for the note at `rel_path`. No-op when the
    /// note isn't in the index yet (open-before-first-index case); the next
    /// successful ingest creates the row, and subsequent opens will record.
    /// Returns true when a row was actually updated.
    ///
    /// status: note-access-tracking
    pub fn touch_note_access(
        &mut self,
        rel_path: &str,
        ts: i64,
    ) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE notes SET last_accessed_at = ?1 WHERE path = ?2",
            params![ts, rel_path],
        )?;
        Ok(updated > 0)
    }

    /// All indexed paths. Used by the full-scan walker to detect notes whose
    /// files have vanished from disk.
    pub fn all_note_paths(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT path FROM notes")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record an attempted-but-refused ingest. Writes (or updates) a `notes`
    /// row with `skipped = 1` and the supplied reason; no chunks, no
    /// embeddings. Any prior chunks for this note are cleared so a file
    /// that was Indexed and is now Skipped doesn't keep stale hits.
    /// `content_hash` and `embedder_version` are stored empty — the row
    /// exists for state, not for retrieval.
    pub fn upsert_skipped(
        &mut self,
        rel_path: &str,
        reason: &str,
        mtime: i64,
        size: i64,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;

        // Reuse an existing id for this path if we have one, else mint a new
        // one — same rule as the indexed path so renames keep working.
        let id: String = match tx
            .query_row(
                "SELECT id FROM path_ids WHERE path = ?1",
                params![rel_path],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            Some(id) => id,
            None => new_id(),
        };
        let indexed_at = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tx.execute(
            "INSERT INTO notes (id, path, content_hash, mtime, size, indexed_at, embedder_version, skipped, skip_reason)
             VALUES (?1, ?2, '', ?3, ?4, ?5, '', 1, ?6)
             ON CONFLICT(id) DO UPDATE SET
               path = excluded.path,
               mtime = excluded.mtime,
               size = excluded.size,
               indexed_at = excluded.indexed_at,
               skipped = 1,
               skip_reason = excluded.skip_reason",
            params![id, rel_path, mtime, size, indexed_at, reason],
        )?;

        tx.execute(
            "INSERT INTO path_ids (path, id) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET id = excluded.id",
            params![rel_path, id],
        )?;

        // Clear any chunks/vecs left over from a previous successful ingest.
        let old_chunk_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_id = ?1")?;
            stmt.query_map(params![id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for cid in &old_chunk_ids {
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
        }
        tx.execute("DELETE FROM chunks WHERE note_id = ?1", params![id])?;

        tx.commit()?;
        Ok(())
    }

    /// Delete a note by id. Cascades through `chunks`; `chunk_vecs` cleaned
    /// up explicitly. `path_ids` for this id are removed too — a deleted
    /// note no longer has a stable path mapping.
    pub fn delete_note(&mut self, note_id: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let chunk_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_id = ?1")?;
            stmt.query_map(params![note_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for cid in &chunk_ids {
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
        }
        tx.execute("DELETE FROM notes WHERE id = ?1", params![note_id])?;
        tx.execute("DELETE FROM path_ids WHERE id = ?1", params![note_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Delete many notes by vault-relative path in a single transaction.
    /// Paths not present in the index are silently skipped — used by folder
    /// delete, where some `.md` files in the tree may simply have never been
    /// ingested. Returns the number of notes that were actually removed.
    pub fn delete_notes_by_paths(
        &mut self,
        rel_paths: &[String],
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        for rel in rel_paths {
            let id_opt: Option<String> = tx
                .query_row(
                    "SELECT id FROM path_ids WHERE path = ?1",
                    params![rel],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id) = id_opt else { continue };
            let chunk_ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_id = ?1")?;
                stmt.query_map(params![id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for cid in &chunk_ids {
                tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
            }
            tx.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
            tx.execute("DELETE FROM path_ids WHERE id = ?1", params![id])?;
            removed += 1;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Bulk rename: update `notes.path` for many indexed paths in a single
    /// transaction. Used by `vault::move_folder` to keep the index consistent
    /// with the on-disk folder rename — either every member's path updates or
    /// none do, so a mid-loop failure can't leave the index half-renamed.
    /// Skips entries that aren't in the index (non-md files, never-ingested
    /// `.md` files); they have nothing to update.
    pub fn rename_notes_by_paths(
        &mut self,
        renames: &[(String, String)],
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let mut updated = 0;
        for (old, new) in renames {
            let id_opt: Option<String> = tx
                .query_row(
                    "SELECT id FROM path_ids WHERE path = ?1",
                    params![old],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(id) = id_opt else { continue };
            tx.execute(
                "UPDATE notes SET path = ?1 WHERE id = ?2",
                params![new, id],
            )?;
            tx.execute("DELETE FROM path_ids WHERE id = ?1", params![id])?;
            tx.execute(
                "INSERT INTO path_ids (path, id) VALUES (?1, ?2)",
                params![new, id],
            )?;
            updated += 1;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Rename: update `notes.path` and add a new `path_ids` row for the new
    /// path. Old path_ids row is removed so search by old path returns None.
    /// Content unchanged — chunks stay valid.
    pub fn rename_note(&mut self, note_id: &str, new_path: &str) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        let updated = tx.execute(
            "UPDATE notes SET path = ?1 WHERE id = ?2",
            params![new_path, note_id],
        )?;
        if updated == 0 {
            return Err(StoreError::NotFound(note_id.to_string()));
        }
        tx.execute("DELETE FROM path_ids WHERE id = ?1", params![note_id])?;
        tx.execute(
            "INSERT INTO path_ids (path, id) VALUES (?1, ?2)",
            params![new_path, note_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Resolve a note id back to its current vault-relative path via the
    /// `path_ids` table. Returns the most recently inserted path for the
    /// id (renames write a fresh row + delete the old one, so there's
    /// at most one live row per id under normal operation).
    ///
    /// status: trail-reference-resolution
    pub fn path_for_id(&self, note_id: &str) -> Result<Option<String>, StoreError> {
        let path = self
            .conn
            .query_row(
                "SELECT path FROM path_ids WHERE id = ?1 LIMIT 1",
                params![note_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(path)
    }
}
