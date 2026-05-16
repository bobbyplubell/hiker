use super::*;

impl Store {
    /// Notes whose basename (filename minus extension) loosely matches the
    /// given prefix, ranked by match quality + recency. Backs the chat
    /// `@`-autocomplete popover (`chat-input-at-autocomplete-tauri-cmd`).
    ///
    /// Empty prefix → most-recently-accessed notes (NULLs last via
    /// `ORDER BY last_accessed_at IS NULL, last_accessed_at DESC`).
    /// Non-empty prefix → case-insensitive `LIKE %prefix%` substring on the
    /// basename, ordered by basename-prefix-match-quality (rank 0: starts
    /// with the prefix; rank 1: contains it elsewhere) then by recency.
    /// Skipped rows are excluded.
    ///
    /// status: chat-input-at-autocomplete-tauri-cmd
    pub fn at_autocomplete(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<AtSuggestion>, StoreError> {
        let limit = limit.max(1) as i64;
        // SQL pulls all non-skipped rows ordered by recency; Rust filters by
        // basename + ranks. The basename match is awkward in pure SQL (no
        // rsplit), and personal-vault scale (≤ tens of thousands of notes)
        // makes the in-memory pass cheap. If a vault ever pushes past that,
        // swap to FTS5 over basenames.
        let mut stmt = self.conn.prepare(
            "SELECT path, last_accessed_at
             FROM notes
             WHERE skipped = 0
             ORDER BY last_accessed_at IS NULL, last_accessed_at DESC, path ASC",
        )?;
        let all = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let last: Option<i64> = row.get(1)?;
                Ok((path, last))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let lower = prefix.to_lowercase();
        let mut scored: Vec<(u8, usize, String, Option<i64>)> = Vec::new();
        for (i, (path, last)) in all.into_iter().enumerate() {
            let basename = basename_of(&path);
            let stem = strip_indexable_extension(basename);
            let stem_lower = stem.to_lowercase();
            if !lower.is_empty() {
                let Some(pos) = stem_lower.find(&lower) else { continue };
                let rank: u8 = if pos == 0 { 0 } else { 1 };
                scored.push((rank, i, path, last));
            } else {
                scored.push((0, i, path, last));
            }
        }
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        scored.truncate(limit as usize);

        Ok(scored
            .into_iter()
            .map(|(_, _, path, last)| AtSuggestion::from_path(path, last))
            .collect())
    }

    /// Related-notes query. Aggregates per-chunk KNN results from all of the
    /// source note's chunks into note-level hits. See docs/index.md for the
    /// algorithm; in short: for each chunk in the source note, run KNN over
    /// `chunk_vecs` excluding the source note's chunks, then group by
    /// note_id and score each candidate as `max(similarity)` across its
    /// matching chunks. Returns top_k notes.
    pub fn related_notes(
        &self,
        source_note_id: &str,
        top_k: usize,
    ) -> Result<Vec<RelatedHit>, StoreError> {
        // Pull source note's chunk embeddings. We use `chunk_vecs` directly
        // so we don't have to round-trip embeddings through Rust types.
        let mut stmt = self.conn.prepare(
            "SELECT v.embedding FROM chunk_vecs v
             JOIN chunks c ON c.id = v.chunk_id
             WHERE c.note_id = ?1",
        )?;
        let source_embeddings: Vec<Vec<u8>> = stmt
            .query_map(params![source_note_id], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<_, _>>()?;
        if source_embeddings.is_empty() {
            return Ok(Vec::new());
        }

        // Per-chunk KNN. Pull a wide candidate set per source chunk; we
        // dedupe by note_id below.
        let per_chunk_k = (top_k * 4).max(20);
        let mut best_per_note: std::collections::HashMap<String, ChunkHit> =
            std::collections::HashMap::new();

        let mut knn_stmt = self.conn.prepare(
            "SELECT v.chunk_id, c.note_id, n.path, c.heading_path, c.text, v.distance
             FROM chunk_vecs v
             JOIN chunks c ON c.id = v.chunk_id
             JOIN notes n ON n.id = c.note_id
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance",
        )?;

        for blob in &source_embeddings {
            let rows = knn_stmt.query_map(params![blob, per_chunk_k as i64], |row| {
                let distance: f32 = row.get(5)?;
                Ok(ChunkHit {
                    chunk_id: row.get(0)?,
                    note_id: row.get(1)?,
                    note_path: row.get(2)?,
                    heading_path: row.get(3)?,
                    text: row.get(4)?,
                    score: 1.0 / (1.0 + distance),
                })
            })?;
            for hit in rows {
                let hit = hit?;
                if hit.note_id == source_note_id {
                    continue;
                }
                best_per_note
                    .entry(hit.note_id.clone())
                    .and_modify(|cur| {
                        if hit.score > cur.score {
                            *cur = hit.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }

        let mut hits: Vec<ChunkHit> = best_per_note.into_values().collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(top_k);

        Ok(hits
            .into_iter()
            .map(|h| {
                let title = title_from_path(&h.note_path);
                let snippet = snippet_from(&h.text);
                RelatedHit {
                    note_id: h.note_id,
                    path: h.note_path,
                    title,
                    score: h.score,
                    best_heading_path: h.heading_path,
                    snippet,
                }
            })
            .collect())
    }

    /// KNN search. Returns the top-k chunks by similarity to `query`, with
    /// chunks belonging to `exclude_note_id` (if Some) filtered out.
    pub fn knn_chunks(
        &self,
        query: &[f32],
        top_k: usize,
        exclude_note_id: Option<&str>,
    ) -> Result<Vec<ChunkHit>, StoreError> {
        knn_chunks_on(&self.conn, query, top_k, exclude_note_id)
    }
}
