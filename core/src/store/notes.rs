use rusqlite::{params, OptionalExtension};

use super::error::Error;
use super::dto::{
    title_from_path, NoteProperties, NoteRow, RecentNote, VaultStats,
};
use super::Store;

impl Store {
    /// True iff a note row exists for `rel_path`. Cheap path-only check —
    /// callers that just need "is this indexed" use this instead of
    /// `get_note_by_path` so we don't pay the row-decode cost.
    ///
    /// status: store-path-is-identity
    pub fn note_exists(&self, rel_path: &str) -> Result<bool, Error> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE path = ?1",
            params![rel_path],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Every indexed note whose basename (the filename, with `.md` /
    /// `.markdown` stripped) equals `name`. Backs the wikilink ambiguity
    /// resolver in `core::wikilink::resolve_path`: bare-name links collect
    /// every candidate and feed them to the resolver's policy.
    ///
    /// Returns vault-relative paths; ordering is unspecified (the resolver
    /// applies its own sort). Empty `Vec` when nothing matches.
    ///
    /// status: store-path-is-identity
    /// status: wikilink-ambiguous-resolution
    pub fn find_notes_by_basename(&self, name: &str) -> Result<Vec<String>, Error> {
        // Cheap shape: pull every indexed `.md` path, filter on the basename
        // in Rust. The vault is small enough at v1 scale (≤500k notes) that
        // a basename-only index isn't justified, and the resolver needs
        // exact-case equality on the basename stem regardless of host fs
        // casing.
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM notes WHERE skipped = 0")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for rel in rows {
            let base = rel.rsplit('/').next().unwrap_or(rel.as_str());
            let stem = base
                .strip_suffix(".md")
                .or_else(|| base.strip_suffix(".markdown"))
                .unwrap_or(base);
            if stem == name {
                out.push(rel);
            }
        }
        Ok(out)
    }

    /// Fetch the note row for a given path, or None if not indexed.
    pub fn get_note_by_path(&self, rel_path: &str) -> Result<Option<NoteRow>, Error> {
        let row = self
            .conn
            .query_row(
                "SELECT path, content_hash, mtime, size, indexed_at, embedder_version,
                        skipped, skip_reason, last_accessed_at
                 FROM notes WHERE path = ?1",
                params![rel_path],
                |row| {
                    Ok(NoteRow {
                        path: row.get(0)?,
                        content_hash: row.get(1)?,
                        mtime: row.get(2)?,
                        size: row.get(3)?,
                        indexed_at: row.get(4)?,
                        embedder_version: row.get(5)?,
                        skipped: row.get::<_, i64>(6)? != 0,
                        skip_reason: row.get(7)?,
                        last_accessed_at: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Vault-relative paths of every note the indexer flagged as
    /// `skipped` (unsupported extension, oversize, etc.). Used by the
    /// file-tree row renderer to badge skipped files without firing a
    /// per-row `get_note_by_path` query on every frame.
    pub fn list_skipped_paths(&self) -> Result<Vec<String>, Error> {
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
    pub fn count_notes(&self) -> Result<u32, Error> {
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
    pub fn vault_stats(&self) -> Result<VaultStats, Error> {
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
    ) -> Result<Vec<RecentNote>, Error> {
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
    ) -> Result<Vec<RecentNote>, Error> {
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
    ) -> Result<Option<NoteProperties>, Error> {
        let row = self.conn.query_row(
            "SELECT n.path, n.content_hash, n.mtime, n.size, n.indexed_at,
                    n.embedder_version, n.skipped, n.skip_reason,
                    n.last_accessed_at
             FROM notes n
             WHERE n.path = ?1",
            params![rel_path],
            |row| {
                let extension = std::path::Path::new(rel_path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(std::string::ToString::to_string);
                // Under path-as-identity the note id IS its path
                // (`store-path-is-identity`); kept as `note_id` in the DTO for
                // the properties tab.
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
                let chunk_count: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM chunks WHERE note_path = ?1",
                    params![props.note_id],
                    |row| row.get(0),
                )?;
                let mut p = props;
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
    ) -> Result<bool, Error> {
        let updated = self.conn.execute(
            "UPDATE notes SET last_accessed_at = ?1 WHERE path = ?2",
            params![ts, rel_path],
        )?;
        Ok(updated > 0)
    }

    /// All indexed paths. Used by the full-scan walker to detect notes whose
    /// files have vanished from disk.
    pub fn all_note_paths(&self) -> Result<Vec<String>, Error> {
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
    ///
    /// Keyed purely by the vault path — the note's identity
    /// (`store-path-is-identity`); there is no separate id.
    pub fn upsert_skipped(
        &mut self,
        rel_path: &str,
        reason: &str,
        mtime: i64,
        size: i64,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let indexed_at = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tx.execute(
            "INSERT INTO notes (path, content_hash, mtime, size, indexed_at, embedder_version, skipped, skip_reason)
             VALUES (?1, '', ?2, ?3, ?4, '', 1, ?5)
             ON CONFLICT(path) DO UPDATE SET
               mtime = excluded.mtime,
               size = excluded.size,
               indexed_at = excluded.indexed_at,
               skipped = 1,
               skip_reason = excluded.skip_reason",
            params![rel_path, mtime, size, indexed_at, reason],
        )?;

        // Clear any chunks/vecs left over from a previous successful ingest.
        let old_chunk_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_path = ?1")?;
            stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for cid in &old_chunk_ids {
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
        }
        tx.execute("DELETE FROM chunks WHERE note_path = ?1", params![rel_path])?;
        // A skipped file isn't indexed — drop any metadata from a prior
        // successful ingest so structured queries don't surface it.
        tx.execute("DELETE FROM note_meta WHERE note_id = ?1", params![rel_path])?;

        tx.commit()?;
        Ok(())
    }

    /// Delete a note by its vault path (its identity). Cascades through
    /// `chunks`; `chunk_vecs` cleaned up explicitly (vec0 has no FK).
    /// Silent no-op when the path isn't indexed. Returns true when a row was
    /// actually removed.
    ///
    /// status: store-path-is-identity
    /// status: ingest-delete-cascade
    pub fn delete_note_by_path(&mut self, rel_path: &str) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let chunk_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_path = ?1")?;
            stmt.query_map(params![rel_path], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        for cid in &chunk_ids {
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
        }
        let removed = tx.execute("DELETE FROM notes WHERE path = ?1", params![rel_path])?;
        tx.execute("DELETE FROM note_meta WHERE note_id = ?1", params![rel_path])?;
        tx.commit()?;
        Ok(removed > 0)
    }

    /// Delete many notes by vault-relative path in a single transaction.
    /// Paths not present in the index are silently skipped — used by folder
    /// delete, where some `.md` files in the tree may simply have never been
    /// ingested. Returns the number of notes that were actually removed.
    pub fn delete_notes_by_paths(
        &mut self,
        rel_paths: &[String],
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        for rel in rel_paths {
            let chunk_ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_path = ?1")?;
                stmt.query_map(params![rel], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for cid in &chunk_ids {
                tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
            }
            let n = tx.execute("DELETE FROM notes WHERE path = ?1", params![rel])?;
            tx.execute("DELETE FROM note_meta WHERE note_id = ?1", params![rel])?;
            if n > 0 {
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Bulk rename: re-key many indexed notes from old path to new path in a
    /// single transaction. Used by `vault::move_folder` to keep the index
    /// consistent with the on-disk folder rename — either every member's path
    /// updates or none do, so a mid-loop failure can't leave the index
    /// half-renamed. Skips entries that aren't in the index (non-md files,
    /// never-ingested `.md` files); they have nothing to update.
    pub fn rename_notes_by_paths(
        &mut self,
        renames: &[(String, String)],
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut updated = 0;
        for (old, new) in renames {
            if Self::rekey_note_in_tx(&tx, old, new)? {
                updated += 1;
            }
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Rename a note by its old path. Re-keys `notes.path` (the identity) and
    /// the path-embedded chunk ids; the `note_path` FK cascade carries the
    /// chunk rows. Returns true when a row moved.
    ///
    /// status: store-path-is-identity
    pub fn rename_note_by_path(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let moved = Self::rekey_note_in_tx(&tx, old_path, new_path)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Re-key a single note from `old_path` to `new_path` within a transaction:
    /// update `notes.path` (the FK `ON UPDATE CASCADE` carries `chunks.note_path`),
    /// then re-key the path-embedded `chunks.id` / `chunk_vecs.chunk_id`
    /// (`"<note_path>:<idx>"`) and the derived `note_meta.note_id`. Content is
    /// unchanged, so the chunk text / embeddings stay valid — only the keys move.
    /// Returns true iff a `notes` row existed at `old_path`.
    ///
    /// status: store-path-is-identity
    fn rekey_note_in_tx(
        tx: &rusqlite::Transaction<'_>,
        old_path: &str,
        new_path: &str,
    ) -> Result<bool, Error> {
        if old_path == new_path {
            return Ok(tx.query_row(
                "SELECT 1 FROM notes WHERE path = ?1",
                params![old_path],
                |_| Ok(()),
            )
            .optional()?
            .is_some());
        }
        // Re-key the vec rows first (vec0 has no FK): read the chunk indices,
        // then move each `"<old>:<idx>"` vec to `"<new>:<idx>"`.
        let indices: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT chunk_index FROM chunks WHERE note_path = ?1")?;
            stmt.query_map(params![old_path], |row| row.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        // Update the note's identity. The FK `ON UPDATE CASCADE` carries
        // `chunks.note_path` to `new_path`.
        let moved = tx.execute(
            "UPDATE notes SET path = ?1 WHERE path = ?2",
            params![new_path, old_path],
        )?;
        if moved == 0 {
            return Ok(false);
        }
        for idx in indices {
            let old_cid = format!("{old_path}:{idx}");
            let new_cid = format!("{new_path}:{idx}");
            // vec0 rows don't UPDATE in place cleanly; move by delete+reinsert
            // of the same embedding under the new chunk id.
            tx.execute(
                "INSERT INTO chunk_vecs (chunk_id, embedding)
                 SELECT ?1, embedding FROM chunk_vecs WHERE chunk_id = ?2",
                params![new_cid, old_cid],
            )?;
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![old_cid])?;
            // `chunks.id` embeds the path; re-key it to match the vec join.
            tx.execute(
                "UPDATE chunks SET id = ?1 WHERE id = ?2",
                params![new_cid, old_cid],
            )?;
        }
        // The derived metadata index keys on the note path too.
        tx.execute(
            "UPDATE note_meta SET note_id = ?1 WHERE note_id = ?2",
            params![new_path, old_path],
        )?;
        Ok(true)
    }
}
