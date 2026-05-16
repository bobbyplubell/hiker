use super::*;

impl Store {
    /// Ordered chunk bounds for the note at `rel_path`. Empty vec for an
    /// unindexed or empty note — never errors on absence, per spec.
    /// Slimmer than `get_note_chunks` (no chunk text), so the wire payload
    /// to the editor pane stays small even on long notes.
    ///
    /// status: tauri-cmd-chunks-for-path
    pub fn chunk_bounds_for(&self, rel_path: &str) -> Result<Vec<ChunkBounds>, StoreError> {
        let id = match self.id_for_path(rel_path)? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        let mut stmt = self.conn.prepare(
            "SELECT chunk_index, byte_start, byte_end, heading_path
             FROM chunks WHERE note_id = ?1 ORDER BY chunk_index",
        )?;
        let rows = stmt
            .query_map(params![id], |row| {
                Ok(ChunkBounds {
                    chunk_index: row.get::<_, i64>(0)? as u32,
                    byte_start: row.get::<_, i64>(1)? as u64,
                    byte_end: row.get::<_, i64>(2)? as u64,
                    // Filled in by `enrich_char_offsets` in the Tauri layer
                    // (it has access to the file contents). Default to 0
                    // so callers that don't enrich still get a valid DTO.
                    char_start: 0,
                    char_end: 0,
                    heading_path: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch the cached note-level embedding for a path, if any. NULL until
    /// the cluster pipeline first computes it; cleared whenever the note's
    /// chunks change.
    ///
    /// status: cluster-note-embeddings
    pub fn note_embedding_for_path(
        &self,
        rel_path: &str,
    ) -> Result<Option<Vec<f32>>, StoreError> {
        let blob: Option<Option<Vec<u8>>> = self
            .conn
            .query_row(
                "SELECT note_embedding FROM notes WHERE path = ?1",
                params![rel_path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.flatten().map(|b| blob_to_embedding(&b)))
    }

    /// Compute (or recompute) the note-level embedding for `rel_path` as a
    /// byte-length-weighted mean-pool of its chunk embeddings, and persist
    /// it on the `notes` row. Returns the freshly-computed embedding, or
    /// `None` when the note has no chunks (the column is left NULL).
    ///
    /// Lazy: callers (cluster pipeline) call this on first need and on
    /// chunk-change. The mean is over the indexer's `chunk_vecs` joined
    /// with `chunks.byte_end - chunks.byte_start` weights so the result
    /// reflects the chunker's heading-bounded structure.
    ///
    /// status: cluster-note-embeddings
    pub fn compute_and_store_note_embedding(
        &mut self,
        rel_path: &str,
    ) -> Result<Option<Vec<f32>>, StoreError> {
        let note_id = match self.id_for_path(rel_path)? {
            Some(id) => id,
            None => return Ok(None),
        };
        let weighted = self.collect_weighted_chunk_embeddings(&note_id)?;
        if weighted.is_empty() {
            // Empty notes get no embedding (see clustering.md "Level 0
            // input"). Leave the column NULL so the cluster pass excludes
            // the note rather than treating a zero-vector as a real point.
            return Ok(None);
        }
        let pooled = byte_weighted_mean_pool(&weighted, self.dim)?;
        let blob = embedding_to_blob(&pooled);
        self.conn.execute(
            "UPDATE notes SET note_embedding = ?1 WHERE id = ?2",
            params![blob, note_id],
        )?;
        Ok(Some(pooled))
    }

    /// Drop the cached `note_embedding` for `rel_path`. Called by the
    /// indexer whenever a note's chunks change so the next cluster pass
    /// recomputes from current data.
    ///
    /// status: cluster-note-embeddings
    pub fn clear_note_embedding(&mut self, rel_path: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE notes SET note_embedding = NULL WHERE path = ?1",
            params![rel_path],
        )?;
        Ok(())
    }

    /// Read each chunk's embedding plus byte-length weight, for note-level
    /// pooling. Private; callers go through `compute_and_store_note_embedding`.
    fn collect_weighted_chunk_embeddings(
        &self,
        note_id: &str,
    ) -> Result<Vec<(Vec<f32>, u64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.embedding, c.byte_end - c.byte_start
             FROM chunk_vecs v
             JOIN chunks c ON c.id = v.chunk_id
             WHERE c.note_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![note_id], |row| {
                let blob: Vec<u8> = row.get(0)?;
                let weight: i64 = row.get(1)?;
                Ok((blob_to_embedding(&blob), weight.max(0) as u64))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetch all chunks for a note, ordered by chunk_index.
    pub fn get_note_chunks(&self, note_id: &str) -> Result<Vec<ChunkRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, note_id, chunk_index, byte_start, byte_end, text, heading_path
             FROM chunks WHERE note_id = ?1 ORDER BY chunk_index",
        )?;
        let rows = stmt
            .query_map(params![note_id], map_chunk_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomic upsert: replace any existing chunks + vec rows for this note,
    /// then write the new ones. Single transaction.
    pub fn upsert_note(&mut self, upsert: NoteUpsert<'_>) -> Result<(), StoreError> {
        let expected = self.dim;
        for (_, emb) in &upsert.chunks {
            if emb.len() != expected {
                return Err(StoreError::EmbedDim {
                    got: emb.len(),
                    expected,
                });
            }
        }
        let tx = self.conn.transaction()?;

        // Upsert notes row. Successful upsert clears any prior Skipped flag
        // — the file made it through ingest, so it's no longer skipped.
        tx.execute(
            // Clears `note_embedding` on every upsert — chunks just
            // changed, so the cached pool is stale until the next cluster
            // pass recomputes it. status: cluster-note-embeddings
            "INSERT INTO notes (id, path, content_hash, mtime, size, indexed_at, embedder_version, skipped, skip_reason, note_embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, NULL)
             ON CONFLICT(id) DO UPDATE SET
               path = excluded.path,
               content_hash = excluded.content_hash,
               mtime = excluded.mtime,
               size = excluded.size,
               indexed_at = excluded.indexed_at,
               embedder_version = excluded.embedder_version,
               skipped = 0,
               skip_reason = NULL,
               note_embedding = NULL",
            params![
                upsert.id,
                upsert.path,
                upsert.content_hash,
                upsert.mtime,
                upsert.size,
                upsert.indexed_at,
                upsert.embedder_version,
            ],
        )?;

        // Path → id mapping (current path).
        tx.execute(
            "INSERT INTO path_ids (path, id) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET id = excluded.id",
            params![upsert.path, upsert.id],
        )?;

        // Replace chunks. Drop existing chunks (chunk_vecs cleaned up
        // explicitly — vec0 doesn't honor the FK cascade).
        let old_chunk_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE note_id = ?1")?;
            let ids = stmt
                .query_map(params![upsert.id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        for cid in &old_chunk_ids {
            tx.execute("DELETE FROM chunk_vecs WHERE chunk_id = ?1", params![cid])?;
        }
        tx.execute("DELETE FROM chunks WHERE note_id = ?1", params![upsert.id])?;

        // Insert new chunks + vecs.
        for (chunk, embedding) in &upsert.chunks {
            let chunk_id = format!("{}:{}", upsert.id, chunk.index);
            tx.execute(
                "INSERT INTO chunks (id, note_id, chunk_index, byte_start, byte_end, text, heading_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    chunk_id,
                    upsert.id,
                    chunk.index as i64,
                    chunk.byte_start as i64,
                    chunk.byte_end as i64,
                    chunk.text,
                    chunk.heading_path,
                ],
            )?;
            tx.execute(
                "INSERT INTO chunk_vecs (chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_to_blob(embedding)],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Reseat `chunk_vecs` to the dim reported by the loaded embedder.
    /// Idempotent: a matching on-disk dim is a no-op. A mismatch (or a
    /// missing table — sqlite returns no rows from `PRAGMA table_info` in
    /// that case) drops + recreates the vec0 table at the new dim and
    /// clears every per-note artifact that depends on the prior dim:
    ///
    /// - `notes.note_embedding` (packed f32 mean-pool — different-dim
    ///   bytes are garbage at the new dim, per `cluster-note-embeddings`).
    /// - `notes.embedder_version` (set to empty string) so the existing
    ///   `embedder-version-tag` per-note re-embed trigger picks every row
    ///   up on the next ingest pass.
    ///
    /// Called by the indexer task once the embedder has loaded, before
    /// any ingest runs. Schema-version bump is intentionally not required
    /// — the rebuild is observable through the dim mismatch itself, and
    /// the `notes` / `chunks` shapes don't change.
    ///
    /// status: store-rebuild-chunk-vecs-on-dim-change
    pub fn ensure_chunk_vecs_dim(&mut self, expected_dim: usize) -> Result<(), StoreError> {
        let current = read_chunk_vecs_dim(&self.conn)?;
        if current == Some(expected_dim) {
            self.dim = expected_dim;
            return Ok(());
        }
        tracing::info!(
            current = ?current,
            expected = expected_dim,
            "store: rebuilding chunk_vecs at new embedder dim",
        );
        let tx = self.conn.transaction()?;
        // vec0 doesn't support column-type changes; drop and recreate.
        // `DROP TABLE IF EXISTS` covers both "no such table yet" (fresh
        // db corner case) and the live mismatch path.
        tx.execute("DROP TABLE IF EXISTS chunk_vecs", [])?;
        tx.execute(
            &format!(
                "CREATE VIRTUAL TABLE chunk_vecs USING vec0(
                    chunk_id  TEXT PRIMARY KEY,
                    embedding float[{expected_dim}]
                )",
            ),
            [],
        )?;
        // Per-note caches that are dim-incompatible at the new dim. Pool
        // bytes get wiped; `embedder_version` gets blanked so the
        // existing per-note re-embed trigger (`embedder-version-tag`) on
        // the next ingest catches every row regardless of which model
        // wrote them.
        tx.execute("UPDATE notes SET note_embedding = NULL", [])?;
        tx.execute("UPDATE notes SET embedder_version = ''", [])?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('chunk_vecs_dim', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![expected_dim.to_string()],
        )?;
        tx.commit()?;
        self.dim = expected_dim;
        Ok(())
    }
}
