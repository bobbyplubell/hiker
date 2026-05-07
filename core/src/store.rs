//! Index store. SQLite + sqlite-vec, statically linked. See docs/index.md.
//!
//! All rusqlite usage is confined here; callers see only the API in this module
//! and the DTOs it returns. The writer connection is owned by whoever
//! constructs `Store` (in v1, the indexer task); read connections open fresh
//! per call via `Store::open_reader`.

use std::path::{Path, PathBuf};
use std::sync::Once;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::chunker::Chunk;
use crate::error::HikerError;

/// Bumped only when on-disk schema changes. Pre-real-use policy: a mismatch
/// is an error, not a migration trigger — delete `.hiker/index.db` and
/// re-index. See docs/index.md `store-version-fail-loud`.
///
/// v2 added `notes.skipped` + `notes.skip_reason` so the indexer can
/// distinguish "skipped on purpose" from "never seen" across launches.
pub const SCHEMA_VERSION: i32 = 2;

/// Embedding dimension for the v1 model (bge-small-en-v1.5). Pinned here so
/// the schema and the embedder agree.
pub const EMBED_DIM: usize = 384;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("schema version mismatch: db is v{found}, binary expects v{expected}")]
    VersionMismatch { found: i32, expected: i32 },
    #[error("embedding dimension mismatch: got {got}, expected {expected}")]
    EmbedDim { got: usize, expected: usize },
    #[error("note not found: {0}")]
    NotFound(String),
}

impl From<StoreError> for HikerError {
    fn from(e: StoreError) -> Self {
        HikerError::Io(e.to_string())
    }
}

/// Row-shaped DTO for a single note. Owned strings so it crosses module
/// boundaries cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    pub id: String,
    pub path: String,
    pub content_hash: String,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: String,
    /// True when the indexer attempted but refused this file (file too large,
    /// non-UTF-8, ...). The row exists so the UI can mark the file as
    /// Skipped across launches; chunks/vecs are not written for skipped rows.
    pub skipped: bool,
    /// Short, stable, human-readable reason — used directly in tooltips and
    /// the status bar (`"file too large"`, `"not UTF-8"`).
    pub skip_reason: Option<String>,
}

/// Stored chunk metadata (without the embedding — fetch via knn_chunks for
/// scored retrieval).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub id: String,
    pub note_id: String,
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: String,
    pub heading_path: Option<String>,
}

/// Public chunk-bounds DTO returned by `chunk_bounds_for` — the wire shape
/// for the chunk-boundary editor decoration. Omits `text` and `note_id` to
/// keep the payload small; the UI only needs offsets + heading_path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkBounds {
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub heading_path: Option<String>,
}

/// One hit from a KNN query. `score` is similarity (higher = closer); we
/// convert from sqlite-vec's distance (L2) so callers can rank uniformly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHit {
    pub chunk_id: String,
    pub note_id: String,
    pub note_path: String,
    pub heading_path: Option<String>,
    pub text: String,
    pub score: f32,
}

/// Note-level hit for the related-notes panel. Aggregates a note's chunk
/// hits into a single row — score is the max similarity across the note's
/// matching chunks, snippet/heading come from that highest-scoring chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedHit {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub score: f32,
    pub best_heading_path: Option<String>,
    pub snippet: String,
}

/// Bundle of everything needed to upsert a note in one transaction. Caller
/// (the indexer task) builds this after chunking + embedding.
pub struct NoteUpsert<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub content_hash: &'a str,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: &'a str,
    pub chunks: Vec<(Chunk, Vec<f32>)>,
}

/// Owned writer connection. Construct once per vault; live for the lifetime
/// of the indexer task.
pub struct Store {
    conn: Connection,
    db_path: PathBuf,
}

impl Store {
    /// Open or create the index db at `<vault_root>/.hiker/index.db`. Runs
    /// idempotent schema setup on a matching version, fails loud on mismatch.
    pub fn open(vault_root: &Path) -> Result<Self, StoreError> {
        register_vec_extension();
        let db_path = vault_root.join(".hiker").join("index.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        configure_connection(&conn)?;
        ensure_schema(&conn)?;
        Ok(Self { conn, db_path })
    }

    /// Open a fresh read-only connection against the same db. Cheap (sub-ms on
    /// warm cache); intended for per-command read paths. Callers drop it on
    /// return.
    pub fn open_reader(&self) -> Result<Connection, StoreError> {
        register_vec_extension();
        let conn = Connection::open(&self.db_path)?;
        configure_connection(&conn)?;
        Ok(conn)
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

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
                        skipped, skip_reason
                 FROM notes WHERE path = ?1",
                params![rel_path],
                map_note_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Total count of indexed notes.
    pub fn count_notes(&self) -> Result<u32, StoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))?;
        Ok(n.max(0) as u32)
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
                    heading_path: row.get(3)?,
                })
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
        for (_, emb) in &upsert.chunks {
            if emb.len() != EMBED_DIM {
                return Err(StoreError::EmbedDim {
                    got: emb.len(),
                    expected: EMBED_DIM,
                });
            }
        }
        let tx = self.conn.transaction()?;

        // Upsert notes row. Successful upsert clears any prior Skipped flag
        // — the file made it through ingest, so it's no longer skipped.
        tx.execute(
            "INSERT INTO notes (id, path, content_hash, mtime, size, indexed_at, embedder_version, skipped, skip_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL)
             ON CONFLICT(id) DO UPDATE SET
               path = excluded.path,
               content_hash = excluded.content_hash,
               mtime = excluded.mtime,
               size = excluded.size,
               indexed_at = excluded.indexed_at,
               embedder_version = excluded.embedder_version,
               skipped = 0,
               skip_reason = NULL",
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
            let ids = stmt
                .query_map(params![id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
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
            let ids = stmt
                .query_map(params![note_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
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
                let ids = stmt
                    .query_map(params![id], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                ids
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

/// Standalone KNN against any connection (so read connections from
/// `open_reader` can reuse the implementation).
pub fn knn_chunks_on(
    conn: &Connection,
    query: &[f32],
    top_k: usize,
    exclude_note_id: Option<&str>,
) -> Result<Vec<ChunkHit>, StoreError> {
    if query.len() != EMBED_DIM {
        return Err(StoreError::EmbedDim {
            got: query.len(),
            expected: EMBED_DIM,
        });
    }
    // Pull a wider candidate set than top_k since we filter post-hoc; vec0
    // doesn't support arbitrary WHERE filters in the MATCH clause.
    let candidate_k = top_k.saturating_mul(4).max(top_k + 16);
    let mut stmt = conn.prepare(
        "SELECT v.chunk_id, c.note_id, n.path, c.heading_path, c.text, v.distance
         FROM chunk_vecs v
         JOIN chunks c ON c.id = v.chunk_id
         JOIN notes n ON n.id = c.note_id
         WHERE v.embedding MATCH ?1 AND k = ?2
         ORDER BY v.distance",
    )?;
    let blob = embedding_to_blob(query);
    let rows = stmt
        .query_map(params![blob, candidate_k as i64], |row| {
            let distance: f32 = row.get(5)?;
            Ok(ChunkHit {
                chunk_id: row.get(0)?,
                note_id: row.get(1)?,
                note_path: row.get(2)?,
                heading_path: row.get(3)?,
                text: row.get(4)?,
                // sqlite-vec returns L2 distance; convert to a similarity-ish
                // score so higher = closer. 1 / (1 + d) is bounded (0, 1].
                score: 1.0 / (1.0 + distance),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let filtered: Vec<ChunkHit> = match exclude_note_id {
        Some(id) => rows.into_iter().filter(|h| h.note_id != id).collect(),
        None => rows,
    };
    Ok(filtered.into_iter().take(top_k).collect())
}

/// Generate a fresh ulid as a string. Used by the indexer when assigning ids
/// to newly-seen notes.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Extract a display title from a vault-relative path. Filename without
/// extension; "Untitled" for an empty stem.
fn title_from_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let stem = last.strip_suffix(".md").unwrap_or(last);
    if stem.is_empty() {
        "Untitled".into()
    } else {
        stem.to_string()
    }
}

/// Trim and clip a chunk text into a short snippet for the panel. ~200 chars,
/// collapsing whitespace.
fn snippet_from(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= 200 {
        collapsed
    } else {
        // Char-boundary safe truncation.
        let cutoff = collapsed
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
        format!("{}…", &collapsed[..cutoff])
    }
}

fn embedding_to_blob(emb: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(emb.len() * 4);
    for f in emb {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn map_note_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        id: row.get(0)?,
        path: row.get(1)?,
        content_hash: row.get(2)?,
        mtime: row.get(3)?,
        size: row.get(4)?,
        indexed_at: row.get(5)?,
        embedder_version: row.get(6)?,
        skipped: row.get::<_, i64>(7)? != 0,
        skip_reason: row.get(8)?,
    })
}

fn map_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        note_id: row.get(1)?,
        chunk_index: row.get::<_, i64>(2)? as u32,
        byte_start: row.get::<_, i64>(3)? as u64,
        byte_end: row.get::<_, i64>(4)? as u64,
        text: row.get(5)?,
        heading_path: row.get(6)?,
    })
}

fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    // WAL mode lets readers run concurrently with the writer's transactions.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // FK enforcement is off by default in SQLite; we rely on the cascade.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Sensible durability: WAL with synchronous=NORMAL is safe and faster
    // than the FULL default.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Move/rename ops open a short-lived writer that may briefly contend
    // with the indexer's owned writer. busy_timeout makes the second writer
    // wait rather than fail loudly with SQLITE_BUSY.
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn ensure_schema(conn: &Connection) -> Result<(), StoreError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(StoreError::VersionMismatch {
            found: user_version,
            expected: SCHEMA_VERSION,
        });
    }

    conn.execute_batch(&format!(
        r#"
        CREATE TABLE IF NOT EXISTS notes (
            id               TEXT PRIMARY KEY,
            path             TEXT NOT NULL UNIQUE,
            content_hash     TEXT NOT NULL,
            mtime            INTEGER NOT NULL,
            size             INTEGER NOT NULL,
            indexed_at       INTEGER NOT NULL,
            embedder_version TEXT NOT NULL,
            skipped          INTEGER NOT NULL DEFAULT 0,
            skip_reason      TEXT
        );

        CREATE TABLE IF NOT EXISTS chunks (
            id           TEXT PRIMARY KEY,
            note_id      TEXT NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
            chunk_index  INTEGER NOT NULL,
            byte_start   INTEGER NOT NULL,
            byte_end     INTEGER NOT NULL,
            text         TEXT NOT NULL,
            heading_path TEXT,
            UNIQUE(note_id, chunk_index)
        );

        CREATE INDEX IF NOT EXISTS chunks_note_id ON chunks(note_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vecs USING vec0(
            chunk_id  TEXT PRIMARY KEY,
            embedding float[{dim}]
        );

        CREATE TABLE IF NOT EXISTS path_ids (
            path TEXT PRIMARY KEY,
            id   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS path_ids_id ON path_ids(id);
        "#,
        dim = EMBED_DIM,
    ))?;

    if user_version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = SCHEMA_VERSION,
            "store: created index db schema",
        );
    }
    Ok(())
}

/// Register sqlite-vec as an auto-extension. Idempotent: subsequent calls are
/// no-ops (guarded by `Once`). Affects every `Connection` opened after this
/// call in the process.
///
/// `sqlite-vec` declares its init symbol as `extern "C" fn()` (no parameters)
/// because the real C signature `(sqlite3*, char**, sqlite3_api_routines*)`
/// can't be expressed cleanly across the FFI boundary the crate uses. We
/// transmute to rusqlite's typed `RawAutoExtension` alias rather than to a
/// raw pointer so the destination signature is documented at the call site.
fn register_vec_extension() {
    use rusqlite::auto_extension::RawAutoExtension;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let init: RawAutoExtension =
            unsafe { std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ()) };
        if let Err(e) = unsafe { rusqlite::auto_extension::register_auto_extension(init) } {
            // Auto-extension registration is process-wide and only fails on
            // OOM or a pathologically broken sqlite build; if it does fail,
            // every subsequent Store::open will surface a clearer "vec_*
            // function not found" error, so log and continue.
            tracing::error!(
                error = %e,
                "sqlite-vec auto-extension register failed",
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn unit_vec(seed: f32) -> Vec<f32> {
        // Deterministic, distinct vectors. Each entry differs from the next by
        // a fixed offset; not unit-norm but fine for L2 KNN tests.
        (0..EMBED_DIM).map(|i| seed + i as f32 * 0.001).collect()
    }

    fn fresh_store() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn mk_chunk(idx: u32, text: &str) -> Chunk {
        Chunk {
            index: idx,
            byte_start: 0,
            byte_end: text.len(),
            text: text.to_string(),
            heading_path: None,
        }
    }

    #[test]
    fn open_creates_db_and_schema() {
        let (_dir, store) = fresh_store();
        assert!(store.db_path().exists());
        // Idempotent re-open works on the same path.
        let _again = Store::open(_dir.path()).unwrap();
    }

    #[test]
    fn version_mismatch_fails_loud() {
        let dir = tempdir().unwrap();
        let _ = Store::open(dir.path()).unwrap();
        // Corrupt the user_version to simulate a future db.
        let conn = Connection::open(dir.path().join(".hiker/index.db")).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        match Store::open(dir.path()) {
            Err(StoreError::VersionMismatch { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            Err(e) => panic!("expected VersionMismatch, got {e:?}"),
            Ok(_) => panic!("expected VersionMismatch, got Ok(Store)"),
        }
    }

    #[test]
    fn upsert_then_read() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "alpha.md",
                content_hash: "abc",
                mtime: 100,
                size: 42,
                indexed_at: 200,
                embedder_version: "test",
                chunks: vec![
                    (mk_chunk(0, "hello world"), unit_vec(0.0)),
                    (mk_chunk(1, "second chunk"), unit_vec(1.0)),
                ],
            })
            .unwrap();

        let note = store.get_note_by_path("alpha.md").unwrap().unwrap();
        assert_eq!(note.id, id);
        assert_eq!(note.size, 42);

        let chunks = store.get_note_chunks(&id).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_index, 0);
        assert_eq!(chunks[1].text, "second chunk");

        assert_eq!(store.id_for_path("alpha.md").unwrap().as_deref(), Some(id.as_str()));
    }

    #[test]
    fn upsert_replaces_chunks() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "a.md",
                content_hash: "v1",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "test",
                chunks: vec![
                    (mk_chunk(0, "old0"), unit_vec(0.0)),
                    (mk_chunk(1, "old1"), unit_vec(1.0)),
                    (mk_chunk(2, "old2"), unit_vec(2.0)),
                ],
            })
            .unwrap();

        // Re-upsert with fewer, different chunks. Old ones must vanish.
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "a.md",
                content_hash: "v2",
                mtime: 2,
                size: 2,
                indexed_at: 2,
                embedder_version: "test",
                chunks: vec![(mk_chunk(0, "new0"), unit_vec(10.0))],
            })
            .unwrap();

        let chunks = store.get_note_chunks(&id).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "new0");

        // Verify the vec table is also down to one row for this note.
        let conn = store.open_reader().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_vecs WHERE chunk_id LIKE ?1 || ':%'",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn delete_note_cascades() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "x.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "t"), unit_vec(0.5))],
            })
            .unwrap();

        store.delete_note(&id).unwrap();
        assert!(store.get_note_by_path("x.md").unwrap().is_none());
        assert!(store.get_note_chunks(&id).unwrap().is_empty());
        assert!(store.id_for_path("x.md").unwrap().is_none());

        let conn = store.open_reader().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM chunk_vecs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn rename_preserves_id_and_chunks() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "old.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "body"), unit_vec(0.0))],
            })
            .unwrap();

        store.rename_note(&id, "new.md").unwrap();

        assert!(store.get_note_by_path("old.md").unwrap().is_none());
        let note = store.get_note_by_path("new.md").unwrap().unwrap();
        assert_eq!(note.id, id);

        // Old path no longer maps to the id.
        assert!(store.id_for_path("old.md").unwrap().is_none());
        assert_eq!(store.id_for_path("new.md").unwrap().as_deref(), Some(id.as_str()));

        // Chunks survived.
        assert_eq!(store.get_note_chunks(&id).unwrap().len(), 1);
    }

    #[test]
    fn knn_finds_nearest_and_excludes_self() {
        let (_dir, mut store) = fresh_store();
        let id_a = new_id();
        let id_b = new_id();
        let id_c = new_id();

        // a's chunks are seeded near 0.0; b's near 0.0 too (so b is "close"
        // to a); c is far away at seed 100.0.
        store
            .upsert_note(NoteUpsert {
                id: &id_a,
                path: "a.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "a-chunk"), unit_vec(0.0))],
            })
            .unwrap();
        store
            .upsert_note(NoteUpsert {
                id: &id_b,
                path: "b.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "b-chunk"), unit_vec(0.001))],
            })
            .unwrap();
        store
            .upsert_note(NoteUpsert {
                id: &id_c,
                path: "c.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "c-chunk"), unit_vec(100.0))],
            })
            .unwrap();

        // Query near 0.0; expect a's chunk first, b's second, c's last.
        let hits = store.knn_chunks(&unit_vec(0.0), 3, None).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].note_id, id_a);
        assert_eq!(hits[1].note_id, id_b);
        assert_eq!(hits[2].note_id, id_c);
        // Scores monotonically decrease as distance grows.
        assert!(hits[0].score >= hits[1].score);
        assert!(hits[1].score > hits[2].score);

        // Excluding a should drop a's chunks; b ranks first among the rest.
        let hits = store.knn_chunks(&unit_vec(0.0), 3, Some(&id_a)).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].note_id, id_b);
        assert_eq!(hits[1].note_id, id_c);
    }

    #[test]
    fn related_notes_aggregates_by_note() {
        let (_dir, mut store) = fresh_store();
        let id_src = new_id();
        let id_near = new_id();
        let id_far = new_id();

        // Source has two chunks at seeds 0 and 1.
        store
            .upsert_note(NoteUpsert {
                id: &id_src,
                path: "src.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![
                    (mk_chunk(0, "src-c0"), unit_vec(0.0)),
                    (mk_chunk(1, "src-c1"), unit_vec(1.0)),
                ],
            })
            .unwrap();

        // "near" has one chunk close to source seed 0; one far away. The
        // aggregation should pick the closer one as its representative.
        store
            .upsert_note(NoteUpsert {
                id: &id_near,
                path: "near.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![
                    (mk_chunk(0, "near-good"), unit_vec(0.001)),
                    (mk_chunk(1, "near-bad"), unit_vec(50.0)),
                ],
            })
            .unwrap();

        store
            .upsert_note(NoteUpsert {
                id: &id_far,
                path: "far.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "far-only"), unit_vec(99.0))],
            })
            .unwrap();

        let hits = store.related_notes(&id_src, 5).unwrap();
        // Source note must not be present.
        assert!(!hits.iter().any(|h| h.note_id == id_src));
        // Near should outrank far.
        let near_pos = hits.iter().position(|h| h.note_id == id_near).unwrap();
        let far_pos = hits.iter().position(|h| h.note_id == id_far).unwrap();
        assert!(near_pos < far_pos);
        // The representative chunk for near should be the close one.
        let near_hit = &hits[near_pos];
        assert_eq!(near_hit.title, "near");
        assert!(near_hit.snippet.contains("near-good"));
    }

    #[test]
    fn related_notes_empty_when_source_unindexed() {
        let (_dir, store) = fresh_store();
        let hits = store.related_notes("nonexistent-id", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn embed_dim_mismatch_rejected() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        let bad = vec![0.0_f32; EMBED_DIM - 1];
        let res = store.upsert_note(NoteUpsert {
            id: &id,
            path: "x.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "t"), bad)],
        });
        assert!(matches!(res, Err(StoreError::EmbedDim { .. })));
    }

    #[test]
    fn knn_dim_mismatch_rejected() {
        let (_dir, store) = fresh_store();
        let res = store.knn_chunks(&[0.0; 10], 5, None);
        assert!(matches!(res, Err(StoreError::EmbedDim { .. })));
    }
}
